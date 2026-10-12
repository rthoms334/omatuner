//! The ffmpeg supervisor (DESIGN §40 §8.1.3, §8.4): `tokio::process` with `kill_on_drop` and
//! `PR_SET_PDEATHSIG` (spawned from a runtime worker, never the blocking pool), environment
//! cleared but `LC_ALL=C`, argv with only the loopback URL and the output directory, stderr
//! redacted at debug, `-progress pipe:1` speed → `CpuTooSlow`, encoder check once per run.
//! OWNER: CAST-4.
//!
//! `PR_SET_PDEATHSIG` follows the spawning *thread*: a tokio worker lives as long as the runtime,
//! but a blocking-pool thread exits when idle and would take the child with it. So children are
//! spawned from `async` code on a worker (§5.6), and the pre-exec hook also catches the race where
//! Tuner died between `fork` and `prctl`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{OnceCell, watch};
use tokio_util::sync::CancellationToken;

use super::super::CastErrorKind;
use super::plan::{Audio, Plan, Segments, Video};
use crate::redact::redact;

/// `hls_time` (s): VioletRelay's 4 s segments, which work on its tvOS 26.5 hardware (§8.4).
pub const SEGMENT_S: f64 = 4.0;
/// Live window: 20 segments (≥ 6, Apple rule 8.11; 80 s of pause).
const LIST_SIZE: u32 = 20;
/// Segments kept on disk behind the window before deletion.
const DELETE_THRESHOLD: u32 = 4;
/// Converting slower than this for [`SLOW_FOR`] is `CpuTooSlow` (§8.1.3).
pub const SLOW_SPEED: f64 = 0.98;
pub const SLOW_FOR: Duration = Duration::from_secs(20);
/// stderr lines logged per child (then one summary line).
const STDERR_LINES: usize = 64;
/// The test card (§8.5): 15 s of `testsrc2` and a 440 Hz tone.
const TEST_CARD_S: u32 = 15;

/// `hls_time` override for the loopback tests (`-hls_time 1`, DESIGN §40 §14.2), in ms; 0 = the
/// default [`SEGMENT_S`].
static SEGMENT_MS_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// Tests only: segment length in seconds for every later pipeline in this process.
#[doc(hidden)]
pub fn set_segment_seconds_for_tests(s: f64) {
    SEGMENT_MS_OVERRIDE.store((s * 1000.0).round() as u64, Ordering::Relaxed);
}

/// The configured segment length (4 s, or the tests' override).
pub fn segment_s() -> f64 {
    match SEGMENT_MS_OVERRIDE.load(Ordering::Relaxed) {
        0 => SEGMENT_S,
        ms => ms as f64 / 1000.0,
    }
}

/// What this machine's ffmpeg can do (checked once per run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// `libx264` and `aac` are there: L3 (and Test on TV) can run.
    pub l3: bool,
    /// The `drawtext` filter (the test card's "Tuner" label).
    pub drawtext: bool,
}

static CAPS: OnceCell<Result<Caps, CastErrorKind>> = OnceCell::const_new();

/// [`Caps`], probed on first use: `Err(NeedsFfmpeg)` when either CLI is missing.
pub async fn caps() -> Result<Caps, CastErrorKind> {
    CAPS.get_or_init(probe_caps).await.clone()
}

async fn probe_caps() -> Result<Caps, CastErrorKind> {
    let (Some(ffmpeg), Some(ffprobe)) = (which("ffmpeg"), which("ffprobe")) else {
        tracing::warn!("cast: ffmpeg/ffprobe not found on PATH: casting needs them");
        return Err(CastErrorKind::NeedsFfmpeg);
    };
    let list = |what: &'static str| {
        let ffmpeg = ffmpeg.clone();
        async move {
            let mut cmd = command(&ffmpeg);
            cmd.args(["-hide_banner", what]).stdout(Stdio::piped()).stderr(Stdio::null());
            match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
                Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
                _ => None,
            }
        }
    };
    let Some(encoders) = list("-encoders").await else {
        tracing::warn!("cast: ffmpeg doesn't run");
        return Err(CastErrorKind::NeedsFfmpeg);
    };
    let has = |text: &str, name: &str| text.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
    let l3 = has(&encoders, "libx264") && has(&encoders, "aac");
    let drawtext = list("-filters").await.is_some_and(|f| has(&f, "drawtext"));
    if !l3 {
        tracing::warn!("cast: ffmpeg has no libx264/aac encoder: converting is off");
    }
    Ok(Caps { ffmpeg, ffprobe, l3, drawtext })
}

/// `name` on this process's `PATH` (children run with a cleared environment, so the program is
/// resolved here, once).
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// A child command as every cast child runs: environment cleared but `LC_ALL=C`, stdin closed,
/// killed on drop, and `PR_SET_PDEATHSIG = SIGKILL` so it dies with Tuner even on SIGKILL.
pub fn command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env_clear().env("LC_ALL", "C").stdin(Stdio::null()).kill_on_drop(true);
    let parent = std::process::id();
    // SAFETY: the hook runs between fork and exec and only calls the async-signal-safe `prctl`
    // and `getppid`.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Tuner died before the prctl: the signal will never come
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
    cmd
}

/// What ffmpeg reads.
#[derive(Debug, Clone, PartialEq)]
pub enum Input<'a> {
    /// The loopback proxy's URL for this generation.
    Proxy {
        url: &'a str,
        /// The proxy serves one HLS media playlist: one segment request at a time
        /// (`-http_multiple 0`), so a one-connection account never sees two.
        hls: bool,
        /// `-ss` (VOD resume or a seek beyond what's produced).
        seek_s: f64,
        /// EVENT playlists (VOD, catch-up): `-readrate 4 -readrate_initial_burst 30`, ahead of
        /// the TV without draining the provider at line rate.
        readrate: bool,
    },
    /// Test on TV (§8.5): `testsrc2` + `sine`, 15 s.
    TestCard { label: bool },
}

/// Where and how ffmpeg writes HLS.
#[derive(Debug, Clone, PartialEq)]
pub struct Output<'a> {
    pub dir: &'a Path,
    /// A sliding window (else an EVENT playlist that ends with ENDLIST).
    pub live: bool,
    pub hls_time: f64,
    /// L3 keyframe interval: 2 × fps (§8.3); 25 fps when unknown.
    pub fps: Option<f64>,
    /// `-map 0:a:<n>` (else the first audio stream, if any).
    pub audio_index: Option<usize>,
}

/// The full argv after the program (pure: tested). Holds only `input`'s loopback URL and paths
/// under `out.dir`.
pub fn argv(input: &Input<'_>, plan: &Plan, out: &Output<'_>) -> Vec<String> {
    let mut a: Vec<String> = Vec::with_capacity(64);
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| (*s).to_owned()));
    push(&["-nostdin", "-hide_banner", "-nostats", "-loglevel", "warning", "-progress", "pipe:1"]);
    match input {
        Input::Proxy { url, hls, seek_s, readrate } => {
            // the proxy owns reconnects (§8.1.1)
            push(&["-reconnect", "0"]);
            if *hls {
                push(&["-http_multiple", "0"]);
            }
            if *readrate {
                push(&["-readrate", "4", "-readrate_initial_burst", "30"]);
            }
            if *seek_s > 0.0 {
                push(&["-ss", &format!("{seek_s:.3}")]);
            }
            push(&["-i", url]);
        }
        Input::TestCard { .. } => {
            push(&["-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25", "-f", "lavfi", "-i", "sine=f=440"]);
            push(&["-t", &TEST_CARD_S.to_string()]);
        }
    }
    if plan.video != Video::None {
        push(&["-map", "0:v:0"]);
    }
    match (plan.audio, input) {
        (Audio::None, _) => {}
        (_, Input::TestCard { .. }) => push(&["-map", "1:a:0"]),
        (_, Input::Proxy { .. }) => match out.audio_index {
            Some(n) => push(&["-map", &format!("0:a:{n}")]),
            None => push(&["-map", "0:a:0?"]),
        },
    }
    match plan.video {
        Video::None => push(&["-vn"]),
        Video::Copy => {
            push(&["-c:v", "copy"]);
            if plan.segments == Segments::Fmp4 {
                push(&["-tag:v", "hvc1"]);
            }
        }
        Video::Encode { deinterlace } => {
            let gop = (2.0 * out.fps.filter(|f| *f > 0.0).unwrap_or(25.0)).round().max(1.0) as u32;
            push(&["-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level", "4.1", "-crf", "21"]);
            push(&["-maxrate", "8M", "-bufsize", "16M", "-g", &gop.to_string(), "-sc_threshold", "0"]);
            let mut vf = Vec::new();
            if deinterlace {
                vf.push("yadif=deint=interlaced".to_owned());
            }
            vf.push("scale=-2:'min(1080,ih)'".to_owned());
            // High profile is 8-bit 4:2:0 (a 10-bit HEVC source would make x264 pick High 10)
            vf.push("format=yuv420p".to_owned());
            if matches!(input, Input::TestCard { label: true }) {
                vf.push("drawtext=text=Tuner:fontsize=120:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=24:x=(w-tw)/2:y=(h-th)/2".to_owned());
            }
            push(&["-vf", &vf.join(",")]);
        }
    }
    match plan.audio {
        Audio::None => push(&["-an"]),
        Audio::Copy => push(&["-c:a", "copy"]),
        Audio::Aac => push(&["-c:a", "aac", "-ac", "2", "-b:a", "160k"]),
    }
    // subtitles and teletext are dropped in Phase 1
    push(&["-sn", "-dn", "-f", "hls", "-hls_time", &fmt_secs(out.hls_time)]);
    if out.live {
        push(&["-hls_list_size", &LIST_SIZE.to_string(), "-hls_delete_threshold", &DELETE_THRESHOLD.to_string()]);
        push(&["-hls_flags", "delete_segments+independent_segments+program_date_time+temp_file+omit_endlist"]);
    } else {
        push(&["-hls_playlist_type", "event", "-hls_flags", "independent_segments+program_date_time+temp_file"]);
    }
    let ext = plan.segments.ext();
    match plan.segments {
        Segments::Ts => push(&["-hls_segment_type", "mpegts"]),
        Segments::Fmp4 => push(&["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4"]),
    }
    let seg = out.dir.join(format!("s%d.{ext}"));
    let index = out.dir.join("index.m3u8");
    push(&["-hls_segment_filename", &seg.to_string_lossy(), &index.to_string_lossy()]);
    a
}

fn fmt_secs(s: f64) -> String {
    let s = format!("{s:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exit status 0 (the input ended and everything is written).
    Done,
    /// A non-zero status or a signal from elsewhere.
    Failed,
    /// Stopped by [`Child::kill`].
    Killed,
}

/// A running, supervised ffmpeg.
#[derive(Debug)]
pub struct Child {
    kill: CancellationToken,
    exit: watch::Receiver<Option<Exit>>,
    pid: Option<u32>,
    started: Instant,
}

/// Progress callbacks (from the supervisor task).
pub struct Progress {
    /// Watch `speed=` against [`SLOW_SPEED`] (a live conversion).
    pub watch_speed: bool,
    /// Called once when converting stays too slow for [`SLOW_FOR`].
    pub on_slow: Box<dyn FnOnce() + Send>,
}

/// Start ffmpeg with `argv` (see [`command`]); `label` names the session in logs.
pub fn spawn(program: &Path, argv: &[String], label: Arc<str>, progress: Progress) -> std::io::Result<Child> {
    let mut cmd = command(program);
    cmd.args(argv).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let (tx, rx) = watch::channel(None);
    let kill = CancellationToken::new();
    if let Some(err) = child.stderr.take() {
        let label = label.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            let mut n = 0usize;
            while let Ok(Some(line)) = lines.next_line().await {
                n += 1;
                if n <= STDERR_LINES {
                    tracing::debug!("{label}: ffmpeg: {}", redact(line.trim_end()));
                }
            }
            if n > STDERR_LINES {
                tracing::debug!("{label}: ffmpeg: {} more stderr lines not logged", n - STDERR_LINES);
            }
        });
    }
    if let Some(out) = child.stdout.take() {
        let Progress { watch_speed, on_slow } = progress;
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            let mut slow = SpeedWatch::default();
            let mut on_slow = Some(on_slow);
            while let Ok(Some(line)) = lines.next_line().await {
                if !watch_speed {
                    continue;
                }
                if let Some(speed) = line.strip_prefix("speed=").and_then(parse_speed)
                    && slow.feed(speed, Instant::now())
                    && let Some(f) = on_slow.take()
                {
                    f();
                }
            }
        });
    }
    let stop = kill.clone();
    tokio::spawn(async move {
        let exit = tokio::select! {
            status = child.wait() => match status {
                Ok(s) if s.success() => Exit::Done,
                Ok(s) => {
                    tracing::debug!("{label}: ffmpeg exited: {s}");
                    Exit::Failed
                }
                Err(e) => {
                    tracing::debug!("{label}: ffmpeg wait: {e}");
                    Exit::Failed
                }
            },
            () = stop.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Exit::Killed
            }
        };
        let _ = tx.send(Some(exit));
    });
    Ok(Child { kill, exit: rx, pid, started: Instant::now() })
}

impl Child {
    /// Kill it and wait until it's gone.
    pub async fn kill(&mut self) {
        self.kill.cancel();
        let _ = self.exit.wait_for(Option::is_some).await;
    }

    /// Kill it without waiting (shutdown's fast path; `kill_on_drop` covers the rest).
    pub fn kill_now(&self) {
        self.kill.cancel();
    }

    /// How it ended, once it has.
    pub fn exit(&self) -> Option<Exit> {
        *self.exit.borrow()
    }

    /// Wait for the end (cancel-safe).
    pub async fn wait(&mut self) -> Exit {
        match self.exit.wait_for(Option::is_some).await {
            Ok(e) => e.unwrap_or(Exit::Failed),
            Err(_) => Exit::Failed,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    /// Pause (`SIGSTOP`) or resume (`SIGCONT`) production: the VOD directory's 8 GiB cap.
    pub fn suspend(&self, on: bool) {
        if let Some(pid) = self.pid.and_then(|p| libc::pid_t::try_from(p).ok())
            && self.exit().is_none()
        {
            // SAFETY: plain kill(2) on our own child's pid, which is not reaped while `exit` is
            // `None`.
            unsafe {
                libc::kill(pid, if on { libc::SIGSTOP } else { libc::SIGCONT });
            }
        }
    }
}

/// `-progress` `speed=` values: `" 0.97x"`, `"1.02x"`, `"N/A"`.
pub fn parse_speed(v: &str) -> Option<f64> {
    v.trim().strip_suffix('x')?.trim().parse::<f64>().ok().filter(|s| s.is_finite())
}

/// "Below 0.98× for 20 s" (§8.1.3): pure, fed with every progress block.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpeedWatch {
    below_since: Option<Instant>,
    fired: bool,
}

impl SpeedWatch {
    /// `true` exactly once: when the speed has stayed below [`SLOW_SPEED`] for [`SLOW_FOR`].
    pub fn feed(&mut self, speed: f64, now: Instant) -> bool {
        if speed >= SLOW_SPEED {
            self.below_since = None;
            return false;
        }
        let since = *self.below_since.get_or_insert(now);
        if !self.fired && now.duration_since(since) >= SLOW_FOR {
            self.fired = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(level: crate::model::CastLevel, video: Video, audio: Audio, segments: Segments) -> Plan {
        Plan { level, video, audio, segments }
    }

    #[test]
    fn live_l2_argv_follows_the_design_and_holds_no_upstream() {
        let dir = Path::new("/run/user/1000/tuner/cast/3fa1/g7");
        let input = Input::Proxy { url: "http://127.0.0.1:41234/u/tok", hls: true, seek_s: 0.0, readrate: false };
        let p = plan(crate::model::CastLevel::Repackaged, Video::Copy, Audio::Aac, Segments::Ts);
        let a = argv(&input, &p, &Output { dir, live: true, hls_time: 4.0, fps: None, audio_index: Some(1) }).join(" ");
        for want in [
            "-reconnect 0 -http_multiple 0 -i http://127.0.0.1:41234/u/tok",
            "-map 0:v:0 -map 0:a:1 -c:v copy -c:a aac -ac 2 -b:a 160k -sn -dn",
            "-f hls -hls_time 4 -hls_list_size 20 -hls_delete_threshold 4",
            "-hls_flags delete_segments+independent_segments+program_date_time+temp_file+omit_endlist",
            "-hls_segment_type mpegts -hls_segment_filename /run/user/1000/tuner/cast/3fa1/g7/s%d.ts /run/user/1000/tuner/cast/3fa1/g7/index.m3u8",
        ] {
            assert!(a.contains(want), "{want:?} missing from {a}");
        }
        assert_eq!(a.matches("://").count(), 1, "only the loopback URL: {a}");
        assert!(!a.contains("-readrate") && !a.contains("-ss "));
    }

    #[test]
    fn vod_hevc_and_l3_argv() {
        let dir = Path::new("/c/g1");
        let vod = Input::Proxy { url: "http://127.0.0.1:1/u/t", hls: false, seek_s: 4210.5, readrate: true };
        let hevc = plan(crate::model::CastLevel::Repackaged, Video::Copy, Audio::Copy, Segments::Fmp4);
        let a = argv(&vod, &hevc, &Output { dir, live: false, hls_time: 6.0, fps: None, audio_index: None }).join(" ");
        assert!(a.contains("-readrate 4 -readrate_initial_burst 30 -ss 4210.500 -i http://127.0.0.1:1/u/t"), "{a}");
        assert!(a.contains("-map 0:v:0 -map 0:a:0? -c:v copy -tag:v hvc1 -c:a copy"), "{a}");
        assert!(a.contains("-hls_playlist_type event -hls_flags independent_segments+program_date_time+temp_file"), "{a}");
        assert!(a.contains("-hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 -hls_segment_filename /c/g1/s%d.m4s"), "{a}");
        assert!(!a.contains("delete_segments") && !a.contains("-http_multiple"));

        let l3 = plan(crate::model::CastLevel::Converted, Video::Encode { deinterlace: true }, Audio::Aac, Segments::Ts);
        let live = Input::Proxy { url: "http://127.0.0.1:1/u/t", hls: false, seek_s: 0.0, readrate: false };
        let a = argv(&live, &l3, &Output { dir, live: true, hls_time: 4.0, fps: Some(25.0), audio_index: Some(0) }).join(" ");
        assert!(
            a.contains(
                "-c:v libx264 -preset veryfast -profile:v high -level 4.1 -crf 21 -maxrate 8M -bufsize 16M -g 50 -sc_threshold 0 \
                 -vf yadif=deint=interlaced,scale=-2:'min(1080,ih)',format=yuv420p"
            ),
            "{a}"
        );

        let card = argv(
            &Input::TestCard { label: true },
            &plan(crate::model::CastLevel::Converted, Video::Encode { deinterlace: false }, Audio::Aac, Segments::Ts),
            &Output { dir, live: false, hls_time: 4.0, fps: Some(25.0), audio_index: None },
        )
        .join(" ");
        assert!(card.contains("-f lavfi -i testsrc2=s=1280x720:r=25 -f lavfi -i sine=f=440 -t 15 -map 0:v:0 -map 1:a:0"), "{card}");
        assert!(card.contains("drawtext=text=Tuner") && !card.contains("://"), "{card}");
    }

    #[test]
    fn slow_conversion_fires_once_after_twenty_seconds_below() {
        let t = Instant::now();
        let s = |n: u64| t + Duration::from_secs(n);
        let mut w = SpeedWatch::default();
        assert!(!w.feed(0.9, s(0)));
        assert!(!w.feed(1.01, s(10)), "recovered: the clock restarts");
        assert!(!w.feed(0.95, s(11)));
        assert!(!w.feed(0.97, s(30)));
        assert!(w.feed(0.97, s(31)));
        assert!(!w.feed(0.5, s(60)), "once");
        assert_eq!(parse_speed(" 0.97x"), Some(0.97));
        assert_eq!(parse_speed("N/A"), None);
    }
}
