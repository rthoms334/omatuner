//! The relay and media pipeline (DESIGN §40 §8, §9.1–9.3): Tuner fetches the stream itself (the
//! TV never gets a provider URL), ffmpeg makes HLS from a loopback URL, and a LAN server hands the
//! TV token URLs. The signatures here are frozen (CAST-0); CAST-4 owns the bodies and the
//! modules.
//!
//! ```text
//!  provider ──(reqwest, Playable.headers, A's lease)──▶ upstream proxy 127.0.0.1:P /u/<tok>
//!                                     ffprobe (once) ──┤
//!                                     ffmpeg ──────────┴──▶ session dir (tmpfs live / cache VOD)
//!  TV ◀── LAN server <lan-ip>:47800 /c/<tok>/… (token, peer check, uniform 404) ◀── playlist guard
//! ```
//! A session serves one receiver: [`RelaySession::start`] binds the LAN server, each
//! [`RelaySession::load`] is one generation (stop-before-start, a new token, a new pipeline),
//! and [`RelaySession::stop`] kills the children, closes the listener and wipes the directory.
//! * **One target duration per item** (§8.1.4): the first load pins it from its first segments,
//!   and every later load of the same item in the session (a reconnect, an escalation, the
//!   restart a longer segment asks for) declares at least as much.
//! * **Surround** (§8.4): multichannel sound on the Apple TV makes three media playlists (`v/`,
//!   `a/`, `s/`) and a multivariant playlist the relay writes ([`renditions`]); when ffmpeg can't
//!   make them, the load runs again in stereo.
//! * **VOD and catch-up stay about a minute ahead** of the TV (`-readrate 1` after a 60 s burst),
//!   and production pauses when it runs [`LEAD_MAX`] ahead of what the TV fetched (a paused TV):
//!   no film is pulled from the provider faster than it is watched.

pub mod ffmpeg;
pub mod plan;
pub mod playlist;
pub mod probe;
pub mod renditions;
pub mod server;
pub mod token;
pub mod upstream;

use std::net::Ipv4Addr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use self::plan::{Segments, Surround, Video};
use self::playlist::{Codec, Guard};
use self::server::Rendition;
use self::token::Token;
use super::receiver::LoadReq;
use super::{CastErrorKind, CastHints, CastLevel, ReceiverKind};
use crate::config::{CastConvert, CastInterlaced};
use crate::model::{MediaRef, Playable};
use crate::redact::Secret;

/// Everything a session needs (from `[cast]`, the paths and the network toward the TV).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    /// This computer's address toward the TV (the UDP `connect()` trick): the only bind.
    pub bind: Ipv4Addr,
    /// The receiver's address: the only peer served (everyone else gets the same 404).
    pub peer: Ipv4Addr,
    /// `cast.media_port`.
    pub port: u16,
    /// `cast.min_segments`: segments ready before `load` returns.
    pub min_segments: u32,
    pub convert: CastConvert,
    pub interlaced: CastInterlaced,
    /// `AppPaths::cast_live_dir()` (tmpfs) and `cast_vod_dir()` (disk, capped at 8 GiB).
    pub live_dir: PathBuf,
    pub vod_dir: PathBuf,
    /// The User-Agent for providers whose playlist sets none.
    pub user_agent: String,
}

/// One load generation.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadSpec {
    /// The controller's load generation (echoed in every [`RelayEvent`]).
    pub generation: u64,
    pub playable: Arc<Playable>,
    pub hints: CastHints,
    /// The receiver's profile (what it plays without converting).
    pub receiver: ReceiverKind,
    /// Force a level: the user's "Convert this channel", a cached hint, or the session's one-step
    /// escalation (L2 → L3).
    pub force: Option<CastLevel>,
    /// Stereo only: the TV didn't play this item's surround renditions (§8.4).
    pub stereo: bool,
}

/// A generation ready for the TV: its URL (a bearer token: `Secret`), whether it is a live
/// window, where it starts in the item, and the level chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub load: LoadReq,
    pub level: CastLevel,
    /// Live: how far behind the live point the TV will run (hold-back + ready segments), for
    /// the casting card's "about 15 s behind"; `None` for EVENT playlists.
    pub behind_live_s: Option<u16>,
    /// The TV gets a multivariant playlist with a surround rendition (§8.4).
    pub surround: bool,
}

/// What the relay reports. The `Upstream*`, `Http` and `ProviderText` events become the same
/// `state::Input`s mpv produces today (DESIGN §40 §11.2), so the reconnect policy works as is.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    UpstreamOpened {
        generation: u64,
    },
    /// The upstream connection closed (the controller releases the lease and records when).
    UpstreamClosed {
        generation: u64,
        at: Instant,
    },
    /// A real HTTP status from the provider.
    Http {
        generation: u64,
        status: u16,
    },
    /// The provider's text where the stream should be (redacted).
    ProviderText {
        generation: u64,
        text: String,
    },
    /// The upstream ended (EOF).
    UpstreamEof {
        generation: u64,
    },
    /// ffprobe's view of the input (the info panel while casting; the hints cached in
    /// `cast_hint`).
    Probed {
        generation: u64,
        info: Box<crate::player::tracks::StreamInfo>,
    },
    /// The TV fetched the playlist (the 10 s firewall watchdog's evidence).
    PlaylistFetched {
        generation: u64,
    },
    /// The TV fetched the segment `from_s..upto_s` of the current playlist (every fetch of a
    /// segment, not only new ones: the position estimate's bound and, when the run of fetches
    /// breaks, its new base; the `actionAtItemEnd` re-arm).
    SegmentFetched {
        generation: u64,
        from_s: f64,
        upto_s: f64,
    },
    /// The playlist guard tripped twice: mark the item irregular (→ L3 next time).
    Irregular {
        generation: u64,
    },
    /// A segment outgrew the pinned target duration and is held back (§8.1.4): the session
    /// restarts the item where the TV is; the relay pins the longer target it needs.
    TargetOverflow {
        generation: u64,
    },
    /// Converting runs below 0.98× for 20 s.
    CpuTooSlow {
        generation: u64,
    },
    /// The pipeline failed (ffmpeg missing, unsupported input, …).
    Failed {
        generation: u64,
        kind: CastErrorKind,
    },
}

/// One cast session's relay.
pub struct RelaySession {
    cfg: RelayConfig,
    /// `cast session <4 hex>`: how logs name the session (§9.2), never an address or URL.
    label: Arc<str>,
    /// The session's directory name under the live and VOD roots.
    sid: String,
    tx: mpsc::UnboundedSender<RelayEvent>,
    rx: mpsc::UnboundedReceiver<RelayEvent>,
    lan: Option<server::LanServer>,
    proxy: Option<upstream::Proxy>,
    current: Option<Pipeline>,
    /// What a load (or an ending) has started and not handed over yet.
    pending: Pending,
    /// Pipelines started (each gets its own directory; a view shares its pipeline's).
    started: u64,
    /// The item's pinned target duration in this session (module docs).
    pin: Option<(MediaRef, u32)>,
}

/// What a load has opened or started before it becomes [`RelaySession::current`]: its feed (the
/// provider connection), its pipeline (ffmpeg's monitor and the token that kills ffmpeg) and its
/// directories. The session's `play` is dropped mid-load by a zap or a stop; whatever that load
/// had started stays here, and the next load, `stop` or `Drop` ends it (§10: a one-connection
/// account must never see the abandoned connection beside the new one). `end_current` moves the
/// pipeline it ends here first, for the same reason.
#[derive(Default)]
struct Pending {
    feed: Option<Arc<upstream::Feed>>,
    pipeline: Option<(CancellationToken, JoinHandle<()>)>,
    dirs: Vec<PathBuf>,
}

/// The generation now producing.
struct Pipeline {
    generation: Arc<AtomicU64>,
    live: bool,
    level: CastLevel,
    /// Seconds into the item where ffmpeg started (`-ss`).
    offset_s: f64,
    /// The item (the pinned target's key).
    media: Option<MediaRef>,
    out: Arc<server::Output>,
    feed: Option<Arc<upstream::Feed>>,
    /// Ends the monitor quietly (it kills ffmpeg first).
    stop: CancellationToken,
    monitor: JoinHandle<()>,
}

/// The monitor's view of a pipeline: loading → ready, or loading → ended (load decides).
const LOADING: u8 = 0;
const READY: u8 = 1;
const ENDED: u8 = 2;

/// No first segments this long after the start: the load budget of §5.5 (foxCast: 30 s).
const LOAD_BUDGET: Duration = Duration::from_secs(30);
/// ffmpeg must have asked the proxy for its input by then, else the attempt restarts (§8.1.1).
const ATTACH_WITHIN: Duration = Duration::from_secs(10);
/// How often the readiness wait looks at ffmpeg's playlist.
const READY_POLL: Duration = Duration::from_millis(200);
/// The VOD directory's cap (§8.4) and how often it is checked.
const VOD_CAP: u64 = 8 << 30;
const VOD_CAP_POLL: Duration = Duration::from_secs(5);
/// VOD and catch-up production pauses this far ahead of the end of what the TV fetched (a TV
/// paused for minutes), and resumes below [`LEAD_RESUME`].
const LEAD_MAX: f64 = 180.0;
const LEAD_RESUME: f64 = 120.0;
/// Copied VOD pins its target on this much of the first burst (a film's GOPs vary: the first
/// two segments alone said 10 s, and 12 s came 20 s later), waiting at most [`PIN_WAIT`] beyond
/// the first ready segments.
const PIN_WINDOW_S: f64 = 20.0;
const PIN_WAIT: Duration = Duration::from_secs(3);

/// How a launch failed (before the TV was given anything).
enum Fail {
    /// The provider failed: its events (`Http` / `ProviderText`, then `UpstreamClosed`) tell.
    Upstream,
    /// ffmpeg exited without producing (the re-probe path).
    Exited,
    /// The surround renditions didn't come (the stereo retry).
    Surround,
    Kind(CastErrorKind),
}

/// What [`RelaySession::launch`] needs besides the argv.
struct Launch {
    argv: Vec<String>,
    live: bool,
    level: CastLevel,
    offset_s: f64,
    dir: PathBuf,
    fmp4: bool,
    codec: Option<Codec>,
    hls_time: f64,
    gop: Option<f64>,
    /// Copied video: the target gets headroom (`playlist::pin`).
    copy: bool,
    /// The item and its pinned target so far.
    media: Option<MediaRef>,
    floor: Option<u32>,
    /// A surround output: what its multivariant playlist says beyond what was produced.
    surround: Option<SurroundSpec>,
    feed: Option<Arc<upstream::Feed>>,
    generation: Arc<AtomicU64>,
    watch_speed: bool,
}

/// The multivariant playlist's inputs from the plan and the probe ([`renditions::Master`]).
struct SurroundSpec {
    codecs: &'static str,
    channels: String,
    language: String,
    fps: Option<f64>,
    range: &'static str,
}

/// The `Err` of [`RelaySession::load`] when the *provider* failed: the events are already queued
/// (see `load`), and `Unreachable` is the kind whose reconnect (one retry) is harmless if a
/// caller does turn it into a failure.
const UPSTREAM_FAILED: CastErrorKind = CastErrorKind::Unreachable;

impl RelaySession {
    /// Bind the LAN server (`cfg.bind:cfg.port`) for `cfg.peer`.
    pub async fn start(cfg: RelayConfig) -> Result<Self, CastErrorKind> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut id = [0u8; 2];
        let _ = aws_lc_rs::rand::SecureRandom::fill(&aws_lc_rs::rand::SystemRandom::new(), &mut id);
        let sid = format!("{:02x}{:02x}", id[0], id[1]);
        let label: Arc<str> = format!("cast session {sid}").into();
        let lan = server::LanServer::bind(cfg.bind, cfg.port, cfg.peer, tx.clone(), label.clone()).await.map_err(|e| {
            // no address at WARN (§9.3): the port says enough
            tracing::warn!("{label}: can't listen on port {} toward the TV: {e}", cfg.port);
            CastErrorKind::Unreachable
        })?;
        let proxy = upstream::Proxy::start().await.map_err(|e| {
            tracing::warn!("{label}: can't start the loopback proxy: {e}");
            CastErrorKind::Unreachable
        })?;
        tracing::debug!("{label}: listening on port {}", lan.addr().port());
        Ok(Self { cfg, label, sid, tx, rx, lan: Some(lan), proxy: Some(proxy), current: None, pending: Pending::default(), started: 0, pin: None })
    }

    pub fn config(&self) -> &RelayConfig {
        &self.cfg
    }

    /// Stop the previous generation (waiting ≤ 1.5 s for its upstream to close), mint a token,
    /// plan, probe if needed, start ffmpeg, and return once `min_segments` are ready (or ENDLIST).
    ///
    /// `Err` kinds: `NeedsFfmpeg` (the CLI or, when converting is needed, an encoder is
    /// missing), `Unsupported` (DRM, a format the receiver can't take with converting off, or
    /// input ffmpeg can't read), `CpuTooSlow` (converting didn't produce the first segments in
    /// time). A *provider* failure (an error status, its prose, a dead connection) is not a cast
    /// failure (§3.4 "Upstream errors: unchanged"): its events are already queued (`Http` or
    /// `ProviderText`, then `UpstreamClosed`), so the controller's reconnect policy acts on them as
    /// it does for mpv, and `load` returns `Err(Unreachable)` with nothing more to add.
    ///
    /// Cancel-safe: a load dropped mid-way leaves what it started in [`Pending`], which the next
    /// load or `stop` ends.
    pub async fn load(&mut self, spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        self.end_current(false).await;
        let r = self.prepare(spec).await;
        if r.is_err() {
            // whatever the failed attempt left (its own paths end most of it)
            self.end_pending(false).await;
        }
        r
    }

    async fn prepare(&mut self, spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        let caps = ffmpeg::caps().await?;
        let p = spec.playable.clone();
        let live = p.kind.is_live();
        let offset_s = if live { 0.0 } else { p.start_at.filter(|s| *s > 0.0).unwrap_or(0.0) };
        let generation = Arc::new(AtomicU64::new(spec.generation));
        let ctx = upstream::FeedCtx { generation: generation.clone(), tx: self.tx.clone(), label: self.label.clone() };
        let pick = upstream::Pick { profile: plan::profile(spec.receiver), height: spec.hints.height };
        let proxy = self.proxy.as_ref().ok_or(CastErrorKind::Unreachable)?;
        let feed = proxy.feed(ctx, &p.url, &p.headers, &self.cfg.user_agent);
        self.pending.feed = Some(feed.clone());
        feed.open(pick).await;
        if feed.failed() {
            self.drop_feed(&feed).await;
            return Err(UPSTREAM_FAILED);
        }
        let loopback = feed.loopback_url().expose().clone();

        let mut probed = None;
        let mut stereo = spec.stereo;
        let floor = self.pin.as_ref().filter(|(m, _)| *m == p.media).map(|(_, t)| *t);
        if !plan::hints_suffice(&spec.hints, spec.receiver) {
            match self.probe(&caps, &feed, &loopback, spec.generation).await {
                Ok(pr) => probed = Some(pr),
                Err(kind) => {
                    self.drop_feed(&feed).await;
                    return Err(kind);
                }
            }
        }
        loop {
            let mut facts = probed.as_ref().map_or_else(|| plan::Facts::from_hints(&spec.hints), |pr| pr.facts(&spec.hints));
            facts.encrypted |= feed.encrypted();
            let opts = plan::Options { convert: self.cfg.convert, interlaced: self.cfg.interlaced, force: spec.force, can_convert: caps.l3, stereo };
            let plan = match plan::plan(spec.receiver, &facts, opts) {
                Ok(plan) => plan,
                Err(kind) => {
                    tracing::info!("{}: {:?} can't go to the TV: {kind:?}", self.label, facts.vcodec);
                    self.drop_feed(&feed).await;
                    return Err(kind);
                }
            };
            let gop = probed.as_ref().and_then(|pr| pr.max_gop_s);
            // copy mode cuts on the source's keyframes: never shorter than its GOP (§8.4);
            // converting forces 2 s GOPs
            let hls_time = match plan.video {
                Video::Copy => ffmpeg::segment_s().max(gop.unwrap_or(0.0)),
                _ => ffmpeg::segment_s(),
            };
            let codec = match (plan.video, facts.vcodec.as_deref()) {
                (Video::Encode { .. }, _) | (Video::Copy, Some("h264")) => Some(Codec::H264),
                (Video::Copy, Some("hevc")) => Some(Codec::Hevc),
                _ => None,
            };
            let dir = self.new_dir(live)?;
            let input = ffmpeg::Input::Proxy { url: &loopback, hls: feed.is_hls(), seek_s: offset_s, readrate: !live };
            let out = ffmpeg::Output { dir: &dir, live, hls_time, fps: facts.fps, audio_index: facts.audio_index };
            let argv = ffmpeg::argv(&input, &plan, &out);
            tracing::info!(
                "{}: {} {} {}{}{}",
                self.label,
                level_name(plan.level),
                facts.vcodec.as_deref().unwrap_or("no video"),
                facts.sound(),
                match plan.surround {
                    Some(Surround::Copy { .. }) => " + surround (copied)",
                    Some(Surround::Ac3) => " + surround (as AC-3 5.1)",
                    None => "",
                },
                if live { " (live)" } else { "" }
            );
            let surround = plan.surround.map(|s| {
                let (codec, profile) = (facts.acodec.as_deref().unwrap_or_default(), facts.aprofile.as_deref());
                let (codecs, channels) = match s {
                    Surround::Copy { .. } => {
                        (renditions::audio_codecs(codec, profile), renditions::audio_channels(codec, profile, facts.channels.unwrap_or(6)))
                    }
                    Surround::Ac3 => ("ac-3", "6".to_owned()),
                };
                SurroundSpec {
                    codecs,
                    channels,
                    language: renditions::language(facts.language.as_deref()),
                    fps: facts.fps,
                    range: renditions::video_range(facts.transfer.as_deref()),
                }
            });
            let launch = Launch {
                argv,
                live,
                level: plan.level,
                offset_s,
                dir: dir.clone(),
                fmp4: plan.segments == Segments::Fmp4,
                codec,
                hls_time,
                gop,
                copy: plan.video == Video::Copy,
                media: Some(p.media.clone()),
                floor,
                surround,
                feed: Some(feed.clone()),
                generation: generation.clone(),
                watch_speed: live && plan.level == CastLevel::Converted,
            };
            match self.launch(&caps, launch).await {
                Ok(prepared) => return Ok(prepared),
                // surround never costs the cast: the same plan in stereo (§8.4)
                Err(Fail::Surround) if !feed.failed() => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    tracing::info!("{}: ffmpeg didn't make the surround renditions: stereo", self.label);
                    stereo = true;
                }
                // a failure with the hints alone re-probes once (§8.1.2)
                Err(Fail::Exited) if probed.is_none() && !feed.failed() => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    tracing::info!("{}: ffmpeg failed with the cached hints: probing", self.label);
                    match self.probe(&caps, &feed, &loopback, spec.generation).await {
                        Ok(pr) => probed = Some(pr),
                        Err(kind) => {
                            self.drop_feed(&feed).await;
                            return Err(kind);
                        }
                    }
                }
                Err(fail) => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    self.drop_feed(&feed).await;
                    return Err(match fail {
                        Fail::Upstream => UPSTREAM_FAILED,
                        Fail::Exited | Fail::Surround => CastErrorKind::Unsupported,
                        Fail::Kind(k) => k,
                    });
                }
            }
        }
    }

    /// ffprobe on the head buffer: `Probed` for the info panel; a provider failure is
    /// [`UPSTREAM_FAILED`], an unreadable input `Unsupported`.
    async fn probe(&self, caps: &ffmpeg::Caps, feed: &upstream::Feed, url: &str, generation: u64) -> Result<probe::Probe, CastErrorKind> {
        match probe::probe(&caps.ffprobe, url, feed.is_hls()).await {
            Ok(pr) => {
                let _ = self.tx.send(RelayEvent::Probed { generation, info: Box::new(pr.stream_info()) });
                Ok(pr)
            }
            Err(_) if feed.failed() => Err(UPSTREAM_FAILED),
            Err(probe::ProbeError::Spawn) => Err(CastErrorKind::NeedsFfmpeg),
            Err(probe::ProbeError::Failed) => {
                tracing::info!("{}: ffprobe can't read the stream", self.label);
                Err(CastErrorKind::Unsupported)
            }
        }
    }

    /// End a feed that never reached a pipeline (its `UpstreamClosed` releases the lease).
    async fn drop_feed(&mut self, feed: &Arc<upstream::Feed>) {
        if let Some(proxy) = &self.proxy {
            proxy.forget(feed);
        }
        feed.close(false).await;
        self.pending.feed = None;
    }

    /// A fresh 0700 directory for one pipeline: tmpfs for a live window, the cache for EVENT
    /// playlists (a film doesn't fit in tmpfs).
    fn new_dir(&mut self, live: bool) -> Result<PathBuf, CastErrorKind> {
        self.started += 1;
        let root = if live { &self.cfg.live_dir } else { &self.cfg.vod_dir };
        let dir = root.join(&self.sid).join(format!("p{}", self.started));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| {
            tracing::warn!("{}: can't create the HLS directory: {e}", self.label);
            CastErrorKind::Unsupported
        })?;
        self.pending.dirs.push(dir.clone());
        Ok(dir)
    }

    /// Start ffmpeg and wait for the first segments (§8.4 `min_segments`, VioletRelay's
    /// `wait_until_ready(segments=2)`; copied VOD waits for [`PIN_WINDOW_S`] of them, a surround
    /// output for a segment of each audio rendition too), pin the target duration, then hand the
    /// TV a token. One restart when ffmpeg never asks the proxy for its input.
    async fn launch(&mut self, caps: &ffmpeg::Caps, l: Launch) -> Result<Prepared, Fail> {
        let min = self.cfg.min_segments.max(1) as usize;
        let renditions = || match &l.surround {
            None => vec![Rendition::new(None, l.dir.clone(), l.codec, if l.fmp4 { "video/mp4" } else { "video/mp2t" })],
            Some(_) => vec![
                Rendition::new(Some(renditions::VIDEO), l.dir.join(renditions::VIDEO), l.codec, "video/mp4"),
                Rendition::new(Some(renditions::STEREO), l.dir.join(renditions::STEREO), None, "audio/mp4"),
                Rendition::new(Some(renditions::SURROUND), l.dir.join(renditions::SURROUND), None, "audio/mp4"),
            ],
        };
        // what ffmpeg exiting before anything was ready means
        let exited = |l: &Launch| {
            if l.feed.as_ref().is_some_and(|f| f.failed()) {
                Fail::Upstream
            } else if l.surround.is_some() {
                Fail::Surround
            } else {
                Fail::Exited
            }
        };
        'attempt: for attempt in 0..2 {
            if let Some(f) = &l.feed {
                f.arm_final_reader();
            }
            let tx = self.tx.clone();
            let generation = l.generation.clone();
            let label = self.label.clone();
            let progress = ffmpeg::Progress {
                watch_speed: l.watch_speed,
                on_slow: Box::new(move || {
                    tracing::warn!("{label}: converting runs slower than real time");
                    let _ = tx.send(RelayEvent::CpuTooSlow { generation: generation.load(Ordering::Relaxed) });
                }),
            };
            let child = match ffmpeg::spawn(&caps.ffmpeg, &l.argv, self.label.clone(), progress) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("{}: ffmpeg didn't start: {e}", self.label);
                    return Err(Fail::Kind(CastErrorKind::NeedsFfmpeg));
                }
            };
            let out = Arc::new(server::Output::new(l.dir.clone(), l.fmp4, l.generation.clone(), renditions()));
            let phase = Arc::new(AtomicU8::new(LOADING));
            let stop = CancellationToken::new();
            let (exit_tx, mut exit_rx) = watch::channel(None);
            self.pending.pipeline = Some((
                stop.clone(),
                tokio::spawn(monitor(
                    child,
                    Watch {
                        feed: l.feed.clone(),
                        phase: phase.clone(),
                        stop: stop.clone(),
                        exit: exit_tx,
                        tx: self.tx.clone(),
                        generation: l.generation.clone(),
                        live: l.live,
                        out: out.clone(),
                        label: self.label.clone(),
                    },
                )),
            ));
            let started = Instant::now();
            let mut enough_since: Option<Instant> = None;
            let mut tick = tokio::time::interval(READY_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let ready = loop {
                tokio::select! {
                    _ = exit_rx.changed() => {
                        let exit = *exit_rx.borrow();
                        // a short stream that ended: what's there is all there will be
                        if exit == Some(ffmpeg::Exit::Done) && let Some(pl) = ready_playlists(&out, 1).await {
                            break pl;
                        }
                        self.quit_pending().await;
                        return Err(exited(&l));
                    }
                    _ = tick.tick() => {
                        if let Some(pl) = ready_playlists(&out, min).await {
                            let since = *enough_since.get_or_insert_with(Instant::now);
                            let produced: f64 = pl[0].segments.iter().map(|s| s.dur).sum();
                            // copied VOD pins on more of its first burst (its GOPs vary)
                            if l.live || !l.copy || pl[0].endlist || produced >= PIN_WINDOW_S || since.elapsed() >= PIN_WAIT {
                                break pl;
                            }
                        }
                        if l.feed.as_ref().is_some_and(|f| !f.final_attached()) && started.elapsed() >= ATTACH_WITHIN {
                            tracing::info!("{}: ffmpeg didn't open its input within {ATTACH_WITHIN:?}", self.label);
                            self.quit_pending().await;
                            if attempt == 0 {
                                continue 'attempt;
                            }
                            return Err(Fail::Exited);
                        }
                        if started.elapsed() >= LOAD_BUDGET {
                            self.quit_pending().await;
                            return Err(if l.feed.as_ref().is_some_and(|f| f.failed()) {
                                Fail::Upstream
                            } else if l.level == CastLevel::Converted {
                                Fail::Kind(CastErrorKind::CpuTooSlow)
                            } else if l.surround.is_some() {
                                Fail::Surround
                            } else {
                                // copy mode without two segments in 30 s: the provider stalls
                                Fail::Upstream
                            });
                        }
                    }
                }
            };
            // ready: pin the target duration before the TV reads anything (§8.1.4), at least
            // what the item had in this session; one for every rendition (Apple rule 8.2)
            let target = playlist::pin(l.hls_time, l.gop, &ready[0], l.copy).max(l.floor.unwrap_or(0));
            if let Some(media) = &l.media {
                self.pin = Some((media.clone(), target));
            }
            for r in &out.renditions {
                *r.guard.lock().unwrap_or_else(|e| e.into_inner()) = Guard::new(target, r.codec);
                // the first segments' IDR check
                r.refresh(out.fmp4).await;
            }
            if let Some(spec) = &l.surround {
                match master(&out, &ready, spec).await {
                    Some(text) => *out.master.lock().unwrap_or_else(|e| e.into_inner()) = Some(text),
                    None => {
                        tracing::info!("{}: the video rendition's init segment can't be read", self.label);
                        self.quit_pending().await;
                        return Err(Fail::Surround);
                    }
                }
            }
            let ended = phase.compare_exchange(LOADING, READY, Ordering::AcqRel, Ordering::Acquire).is_err();
            if ended {
                // ffmpeg ended before the TV got anything: a short stream is what it is
                if *exit_rx.borrow() != Some(ffmpeg::Exit::Done) {
                    self.quit_pending().await;
                    return Err(exited(&l));
                }
            }
            let token = Token::mint();
            let lan = self.lan.as_ref().ok_or(Fail::Kind(CastErrorKind::Unreachable))?;
            let url = format!("http://{}/c/{}/index.m3u8", lan.addr(), token.expose());
            lan.grant_only(token, out.clone(), None);
            let Some((stop, monitor)) = self.pending.pipeline.take() else { return Err(Fail::Exited) };
            // handed over: the pipeline, its feed and its directory are `current`'s now
            self.pending = Pending::default();
            self.current = Some(Pipeline {
                generation: l.generation.clone(),
                live: l.live,
                level: l.level,
                offset_s: l.offset_s,
                media: l.media.clone(),
                out,
                feed: l.feed.clone(),
                stop,
                monitor,
            });
            let behind_live_s = l.live.then(|| (3.0 * f64::from(target) + l.hls_time).round().min(f64::from(u16::MAX)) as u16);
            tracing::debug!("{}: ready, target duration {target} s", self.label);
            if ended && let Some(feed) = &l.feed {
                // the monitor left this to us: the input's end, then the close
                let _ = self.tx.send(RelayEvent::UpstreamEof { generation: l.generation.load(Ordering::Relaxed) });
                feed.close(false).await;
            }
            let load = LoadReq { url: Secret::new(url), live: l.live, offset_s: l.offset_s };
            return Ok(Prepared { load, level: l.level, behind_live_s, surround: l.surround.is_some() });
        }
        Err(Fail::Exited)
    }

    /// A playlist view starting at the segment that holds `at_s` (seconds into the item) under a
    /// new token, when it is inside what's produced; `Ok(None)` when it isn't (the caller loads
    /// again with `start_at`, or clamps catch-up).
    pub async fn view(&mut self, generation: u64, at_s: f64) -> Result<Option<Prepared>, CastErrorKind> {
        let Some(p) = &self.current else { return Ok(None) };
        if p.live {
            return Ok(None);
        }
        let rel = at_s - p.offset_s;
        if rel < 0.0 || !rel.is_finite() {
            return Ok(None);
        }
        // what ffmpeg has written by now, not only what the TV fetched; each audio rendition
        // starts at its segment holding the video's first (they share the output's timeline)
        let mut firsts = Vec::with_capacity(p.out.renditions.len());
        let mut start = 0.0;
        for (i, r) in p.out.renditions.iter().enumerate() {
            r.refresh(p.out.fmp4).await;
            let at = if i == 0 { rel } else { start };
            let Some((seq, s)) = r.guard.lock().unwrap_or_else(|e| e.into_inner()).segment_at(at) else { return Ok(None) };
            if tokio::fs::metadata(r.dir.join(server::File::Segment(seq).name(p.out.fmp4))).await.is_err() {
                return Ok(None); // deleted by the VOD cap: restart there instead
            }
            if i == 0 {
                start = s;
            }
            firsts.push(seq);
        }
        p.generation.store(generation, Ordering::Relaxed);
        let lan = self.lan.as_ref().ok_or(CastErrorKind::Unreachable)?;
        let token = Token::mint();
        let url = format!("http://{}/c/{}/index.m3u8", lan.addr(), token.expose());
        tracing::debug!("{}: view from segment {} ({start:.1} s)", self.label, firsts[0]);
        lan.grant_only(token, p.out.clone(), Some(firsts));
        let load = LoadReq { url: Secret::new(url), live: false, offset_s: p.offset_s + start };
        Ok(Some(Prepared { load, level: p.level, behind_live_s: None, surround: p.out.renditions.len() > 1 }))
    }

    /// Seconds into the item where the last segment ffmpeg has written starts: how far a
    /// catch-up seek can go (§8.5: beyond, it clamps here). `None` for live, or before the
    /// first segment.
    pub async fn produced_s(&mut self) -> Option<f64> {
        let p = self.current.as_ref().filter(|p| !p.live)?;
        // what ffmpeg has written by now, not only what the TV fetched (as `view`)
        let main = p.out.main();
        main.refresh(p.out.fmp4).await;
        let guard = main.guard.lock().unwrap_or_else(|e| e.into_inner());
        Some(p.offset_s + guard.start_of(guard.last_seq()?)?)
    }

    /// Test on TV: the lavfi test card (15 s, "Tuner" label) through L3 into the same server; no
    /// provider connection.
    pub async fn test_card(&mut self, generation: u64) -> Result<Prepared, CastErrorKind> {
        self.end_current(false).await;
        let caps = ffmpeg::caps().await?;
        if !caps.l3 {
            return Err(CastErrorKind::NeedsFfmpeg);
        }
        let plan = plan::Plan {
            level: CastLevel::Converted,
            video: Video::Encode { deinterlace: false },
            audio: plan::Audio::Aac,
            segments: Segments::Ts,
            surround: None,
        };
        let dir = self.new_dir(true)?;
        let hls_time = ffmpeg::segment_s();
        let argv = ffmpeg::argv(
            &ffmpeg::Input::TestCard { label: caps.drawtext },
            &plan,
            &ffmpeg::Output { dir: &dir, live: false, hls_time, fps: Some(25.0), audio_index: None },
        );
        let launch = Launch {
            argv,
            live: false,
            level: CastLevel::Converted,
            offset_s: 0.0,
            dir: dir.clone(),
            fmp4: false,
            codec: Some(Codec::H264),
            hls_time,
            gop: Some(2.0),
            copy: false,
            media: None,
            floor: None,
            surround: None,
            feed: None,
            generation: Arc::new(AtomicU64::new(generation)),
            watch_speed: false,
        };
        match self.launch(&caps, launch).await {
            Ok(p) => Ok(p),
            Err(fail) => {
                self.end_pending(false).await;
                Err(match fail {
                    Fail::Kind(k) => k,
                    Fail::Upstream | Fail::Exited | Fail::Surround => CastErrorKind::NeedsFfmpeg,
                })
            }
        }
    }

    /// The next event (cancel-safe: use it in `select!`).
    pub async fn next_event(&mut self) -> RelayEvent {
        match self.rx.recv().await {
            Some(ev) => ev,
            // unreachable: the session holds a sender
            None => std::future::pending().await,
        }
    }

    /// Kill the children, close the listener, revoke the tokens, wipe the session directory.
    /// `fast`: shutdown's path (no waiting for the upstream close).
    pub async fn stop(&mut self, fast: bool) {
        self.end_current(fast).await;
        if let Some(lan) = self.lan.take() {
            lan.shutdown();
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.shutdown();
        }
        self.wipe();
    }

    /// Stop-before-start (§8.5): the monitor kills ffmpeg, the feed closes (≤ 1.5 s for the
    /// upstream connection unless `fast`) and reports `UpstreamClosed`, the token goes, and so
    /// does the pipeline's directory. What a dropped load or ending left goes first.
    async fn end_current(&mut self, fast: bool) {
        self.end_pending(fast).await;
        let Some(p) = self.current.take() else { return };
        if let Some(lan) = &self.lan {
            lan.revoke_all();
        }
        // a segment outgrew the target: the item's next load pins what it needs (§8.1.4)
        let wanted = p.out.main().guard.lock().unwrap_or_else(|e| e.into_inner()).overflow();
        if let (Some(media), Some(wanted)) = (&p.media, wanted)
            && let Some((_, t)) = self.pin.as_mut().filter(|(m, _)| m == media)
        {
            tracing::debug!("{}: the item's target duration goes from {t} s to {wanted} s", self.label);
            *t = (*t).max(wanted);
        }
        // through the pending slot: an ending dropped half-way is finished by the next one
        self.pending = Pending { feed: p.feed, pipeline: Some((p.stop, p.monitor)), dirs: vec![p.out.dir.clone()] };
        self.end_pending(fast).await;
    }

    /// End what [`Pending`] holds: ffmpeg is told to die and the feed's requests are cancelled at
    /// once (nothing outlives a drop of this future), then the monitor is awaited (≤ 2 s unless
    /// `fast`), the feed closes (its `UpstreamClosed`, once) and the directories go.
    async fn end_pending(&mut self, fast: bool) {
        if let Some((stop, _)) = &self.pending.pipeline {
            stop.cancel();
        }
        if let Some(feed) = &self.pending.feed {
            if let Some(proxy) = &self.proxy {
                proxy.forget(feed);
            }
            feed.cancel_all();
        }
        if let Some((_, monitor)) = &mut self.pending.pipeline
            && !fast
        {
            let _ = tokio::time::timeout(Duration::from_secs(2), monitor).await;
        }
        self.pending.pipeline = None;
        if let Some(feed) = self.pending.feed.clone() {
            feed.close(fast).await;
        }
        self.pending.feed = None;
        for dir in std::mem::take(&mut self.pending.dirs) {
            if fast {
                let _ = std::fs::remove_dir_all(&dir);
            } else {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
        }
    }

    /// Stop the pipeline `launch` is waiting on and wait for its monitor (ffmpeg killed); its
    /// feed and directory stay for the caller.
    async fn quit_pending(&mut self) {
        if let Some((stop, monitor)) = &mut self.pending.pipeline {
            stop.cancel();
            let _ = monitor.await;
        }
        self.pending.pipeline = None;
    }

    /// Remove the session's directories (both roots).
    fn wipe(&self) {
        for root in [&self.cfg.live_dir, &self.cfg.vod_dir] {
            let _ = std::fs::remove_dir_all(root.join(&self.sid));
        }
    }
}

impl Drop for RelaySession {
    /// Dropped without `stop` (a panic, the runtime going down): kill and wipe what can be
    /// reached synchronously; `kill_on_drop` and `PR_SET_PDEATHSIG` cover the children.
    fn drop(&mut self) {
        if let Some(p) = self.current.take() {
            p.stop.cancel();
            p.monitor.abort();
            if let Some(feed) = &p.feed {
                feed.cancel_all();
            }
        }
        if let Some((stop, monitor)) = self.pending.pipeline.take() {
            stop.cancel();
            monitor.abort();
        }
        if let Some(feed) = self.pending.feed.take() {
            feed.cancel_all();
        }
        if let Some(lan) = self.lan.take() {
            lan.shutdown();
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.shutdown();
        }
        self.wipe();
    }
}

/// ffmpeg's playlist, when it has written one.
async fn read_playlist(dir: &Path) -> Option<playlist::MediaPlaylist> {
    tokio::fs::read_to_string(dir.join("index.m3u8")).await.ok().map(|t| playlist::parse(&t))
}

/// Every rendition's playlist once the TV can start: `min` segments of the main one (or its
/// end), and one of each audio rendition (or its end).
async fn ready_playlists(out: &server::Output, min: usize) -> Option<Vec<playlist::MediaPlaylist>> {
    let mut all = Vec::with_capacity(out.renditions.len());
    for (i, r) in out.renditions.iter().enumerate() {
        let pl = read_playlist(&r.dir).await?;
        let need = if i == 0 { min } else { 1 };
        if pl.segments.len() < need && !pl.endlist || pl.segments.is_empty() {
            return None;
        }
        all.push(pl);
    }
    Some(all)
}

/// A surround output's multivariant playlist from what was produced ([`renditions`]): the video
/// codec and size from its init segment, the bandwidths measured on the ready segments.
async fn master(out: &server::Output, ready: &[playlist::MediaPlaylist], spec: &SurroundSpec) -> Option<String> {
    let [v, a, s] = &out.renditions[..] else { return None };
    let init = tokio::fs::read(v.dir.join(server::File::Init(Some(0)).name(true))).await.ok()?;
    let video = renditions::video_init(&init)?;
    let (vr, ar, sr) = (renditions::measure(&v.dir, &ready[0]), renditions::measure(&a.dir, &ready[1]), renditions::measure(&s.dir, &ready[2]));
    Some(renditions::render(&renditions::Master {
        video,
        fps: spec.fps,
        range: spec.range,
        video_rate: vr,
        stereo_rate: ar,
        surround_rate: sr,
        surround_codecs: spec.codecs.to_owned(),
        surround_channels: spec.channels.clone(),
        language: spec.language.clone(),
    }))
}

fn level_name(l: CastLevel) -> &'static str {
    match l {
        CastLevel::Original => "L1 original",
        CastLevel::Repackaged => "L2 repackaged",
        CastLevel::Converted => "L3 converted",
    }
}

/// What the monitor of one pipeline holds.
struct Watch {
    feed: Option<Arc<upstream::Feed>>,
    phase: Arc<AtomicU8>,
    stop: CancellationToken,
    exit: watch::Sender<Option<ffmpeg::Exit>>,
    tx: mpsc::UnboundedSender<RelayEvent>,
    generation: Arc<AtomicU64>,
    live: bool,
    out: Arc<server::Output>,
    label: Arc<str>,
}

/// One pipeline's supervisor: ffmpeg's exit, the feed failing (ffmpeg is killed: the
/// generation is over), the VOD cap. Before the TV has the pipeline (`LOADING`), `load` decides
/// what an exit means; after (`READY`), the monitor reports it: `UpstreamEof` for a clean end,
/// then the feed's `UpstreamClosed` either way (the `Http` of a provider failure is already out).
async fn monitor(mut child: ffmpeg::Child, w: Watch) {
    let mut cap = tokio::time::interval(VOD_CAP_POLL);
    cap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut suspended = false;
    let failed = async {
        match &w.feed {
            Some(f) => f.wait_failed().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(failed);
    let exit = loop {
        tokio::select! {
            biased;
            () = w.stop.cancelled() => {
                child.kill().await;
                let _ = w.exit.send(Some(ffmpeg::Exit::Killed));
                return;
            }
            e = child.wait() => break e,
            () = &mut failed => {
                child.kill().await;
                break ffmpeg::Exit::Killed;
            }
            _ = cap.tick(), if !w.live => {
                suspended = vod_cap(&w.out, &child, suspended, &w.label).await;
            }
        }
    };
    let _ = w.exit.send(Some(exit));
    if w.phase.compare_exchange(LOADING, ENDED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        return;
    }
    let Some(feed) = &w.feed else { return };
    match exit {
        ffmpeg::Exit::Done if !feed.failed() => {
            tracing::debug!("{}: the input ended", w.label);
            let _ = w.tx.send(RelayEvent::UpstreamEof { generation: w.generation.load(Ordering::Relaxed) });
        }
        ffmpeg::Exit::Failed if !feed.failed() => tracing::warn!("{}: ffmpeg stopped with an error", w.label),
        _ => {}
    }
    feed.close(false).await;
}

/// The VOD directory's 8 GiB cap (§8.4): delete segments the TV has already fetched (oldest
/// first, keeping two behind it, in every rendition); if that isn't enough, pause ffmpeg until it
/// is. And the lead cap: pause it while production runs more than [`LEAD_MAX`] ahead of what the
/// TV fetched (a paused TV), until it is back under [`LEAD_RESUME`]. Returns whether ffmpeg is
/// paused.
async fn vod_cap(out: &server::Output, child: &ffmpeg::Child, suspended: bool, label: &str) -> bool {
    let mut files = Vec::new();
    let mut total = 0u64;
    for (i, r) in out.renditions.iter().enumerate() {
        let Ok(mut rd) = tokio::fs::read_dir(&r.dir).await else { continue };
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(meta) = e.metadata().await else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            if let Some(server::File::Segment(n)) = e.file_name().to_str().and_then(server::served_file) {
                files.push((i, n, e.path(), meta.len()));
            }
        }
    }
    if total > VOD_CAP {
        files.sort_by_key(|f| f.1);
        for (i, n, path, len) in files {
            if total <= VOD_CAP {
                break;
            }
            if n >= out.renditions[i].fetched.load(Ordering::Relaxed).saturating_sub(2) {
                continue;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total -= len;
            }
        }
    }
    let full = total > VOD_CAP;
    let lead = lead_s(out).await;
    let ahead = lead.is_some_and(|s| s > if suspended { LEAD_RESUME } else { LEAD_MAX });
    let over = full || ahead;
    if over != suspended {
        match (over, full) {
            (true, true) => tracing::info!("{label}: pausing production at the {} GiB cap", VOD_CAP >> 30),
            (true, false) => tracing::info!("{label}: pausing production {:.0} s ahead of the TV", lead.unwrap_or_default()),
            (false, _) => tracing::info!("{label}: resuming production"),
        }
        child.suspend(over);
    }
    over
}

/// How far production runs ahead of the end of what the TV fetched (seconds; `None` before the
/// TV fetched a segment).
async fn lead_s(out: &server::Output) -> Option<f64> {
    let main = out.main();
    let fetched = main.fetched.load(Ordering::Relaxed).checked_sub(1)?;
    main.refresh(out.fmp4).await;
    let g = main.guard.lock().unwrap_or_else(|e| e.into_inner());
    let (_, seen) = g.span(fetched, 0)?;
    let (_, made) = g.span(g.last_seq()?, 0)?;
    Some(made - seen)
}

/// `ffmpeg` and `ffprobe` are runnable and have `libx264` and `aac` (checked once per run).
/// `Err(NeedsFfmpeg)` when the CLI is missing; `Ok(false)` when L3 can't run (an encoder is
/// missing: the UI says converting is off).
pub async fn check_ffmpeg() -> Result<bool, CastErrorKind> {
    ffmpeg::caps().await.map(|c| c.l3)
}
