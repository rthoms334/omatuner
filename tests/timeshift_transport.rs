//! Real-mpv transport checks. The original remux experiments remain for comparison;
//! `*_provider_hls` and `rolling_hls_http_mpv` exercise the production HLS recorder.
//! `downloaded_hls_*` are the filesystem-only Downloads finishing gates (DESIGN §43 §8.4).
//! See docs/design/timeshift-design.md and docs/design/downloads-design.md.
//!
//! Run outside the sandbox:
//! cargo test --test timeshift_transport zap_gate_http -- --ignored --nocapture

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use libmpv2::Mpv;
use libmpv2::events::Event;
use support::media::{Faults, TestServer, ensure_media};

const TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const WINDOW_SEGMENTS: usize = 10;
const SEGMENT_SECONDS: f64 = 2.0;
// 0.5 s user limit plus 0.1 s scheduling allowance. Report the unrounded delta too.
const ZAP_LIMIT: f64 = 0.6;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ffmpeg() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    // Only generated fixtures / loopback addresses are accepted by this test harness.
    // Discard child diagnostics: production must never log provider argv or stderr raw.
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

struct Input(TestServer);

impl Input {
    fn url(&self) -> String {
        self.0.url("live/index.m3u8")
    }
}

struct Recorder {
    child: ChildGuard,
    dir: tempfile::TempDir,
}

impl Recorder {
    fn start(input: &Input) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = ffmpeg();
        // No ffprobe or multi-segment ready wait. Give this candidate a fast-start chance.
        cmd.args(["-analyzeduration", "500000", "-probesize", "524288"]);
        cmd.args(["-http_multiple", "0", "-http_persistent", "0", "-live_start_index", "-3"]);
        let child = cmd
            .arg("-i")
            .arg(input.url())
            .args(["-map", "0:v:0", "-map", "0:a:0", "-c", "copy", "-f", "hls", "-hls_time"])
            .arg(SEGMENT_SECONDS.to_string())
            .arg("-hls_list_size")
            .arg(WINDOW_SEGMENTS.to_string())
            .args(["-hls_delete_threshold", "1", "-hls_flags", "delete_segments+independent_segments+temp_file+omit_endlist"])
            .arg("-hls_segment_filename")
            .arg(dir.path().join("%06d.ts"))
            .arg(dir.path().join("index.m3u8"))
            .spawn()
            .unwrap();
        Self { child: ChildGuard(child), dir }
    }

    fn playlist(&self) -> PathBuf {
        self.dir.path().join("index.m3u8")
    }

    fn wait_ready(&mut self, started: Instant) {
        while !self.playlist().exists() {
            if let Some(status) = self.child.0.try_wait().unwrap() {
                panic!("fixture segmenter exited before publishing a playlist ({status})");
            }
            assert!(started.elapsed() < TIMEOUT, "no complete segment within {TIMEOUT:?}");
            thread::sleep(POLL);
        }
    }
}

fn mpv() -> Mpv {
    // Install before Buffer starts its worker so RUST_LOG also covers local HTTP requests.
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
    Mpv::with_initializer(|init| {
        for (name, value) in [
            ("config", "no"),
            ("terminal", "no"),
            ("vo", "null"),
            ("ao", "null"),
            ("hwdec", "no"),
            ("idle", "yes"),
            ("keep-open", "no"),
            ("cache", "yes"),
            ("cache-on-disk", "no"),
            ("cache-secs", "1"),
            ("demuxer-readahead-secs", "1"),
            // A rewind must actually reach the disk recording, not mpv's cached packets.
            ("demuxer-max-back-bytes", "0"),
            ("cache-pause-initial", "no"),
            ("demuxer-lavf-o", "http_persistent=0,http_multiple=0"),
            ("network-timeout", "5"),
        ] {
            init.set_option(name, value)?;
        }
        Ok(())
    })
    .unwrap()
}

fn first_frame(mpv: &Mpv, started: Instant) -> f64 {
    // PLAYBACK_RESTART follows the decoder/VO start, unlike FILE_LOADED. Also require a
    // decoded picture type: an audio-only start must not pass the gate.
    // A replaced reader can still enqueue a restart after load_buffer drained its events.
    // Only accept a restart after the new file has loaded.
    let mut loaded = false;
    let mut restarted = false;
    loop {
        match mpv.wait_event(0.01) {
            Some(Ok(Event::FileLoaded)) => loaded = true,
            Some(Ok(Event::PlaybackRestart)) if loaded => restarted = true,
            _ => {}
        }
        if restarted && mpv.get_property::<String>("video-frame-info/picture-type").is_ok_and(|v| matches!(v.as_str(), "I" | "P" | "B")) {
            return started.elapsed().as_secs_f64();
        }
        assert!(started.elapsed() < TIMEOUT, "no output video frame within {TIMEOUT:?}; {}", playback_state(mpv));
    }
}

fn measure(input: Input, buffered: bool) -> f64 {
    let mpv = mpv();
    let started = Instant::now();
    let mut recorder = buffered.then(|| Recorder::start(&input));
    let mut output_server = None;
    let url = if let Some(recorder) = &mut recorder {
        recorder.wait_ready(started);
        let server = TestServer::start(recorder.dir.path().to_owned(), Faults::default());
        let url = server.url("index.m3u8");
        output_server = Some(server);
        url
    } else {
        input.url()
    };
    mpv.command("loadfile", &[&url, "replace"]).unwrap();
    let elapsed = first_frame(&mpv, started);
    // Reader before writer before input; no background producers left by a failed gate.
    drop(mpv);
    drop(output_server);
    drop(recorder);
    drop(input);
    elapsed
}

fn gate(label: &str, make_input: impl Fn() -> Input) {
    let mut deltas = Vec::new();
    for sample in 0..3 {
        // Alternate order to reduce warm filesystem / decoder bias. Each input starts with
        // the same initial live playlist, rather than comparing different live GOP phases.
        let (direct, buffered) = if sample % 2 == 0 {
            (measure(make_input(), false), measure(make_input(), true))
        } else {
            let buffered = measure(make_input(), true);
            (measure(make_input(), false), buffered)
        };
        let delta = buffered - direct;
        eprintln!("{label} sample {}: direct={direct:.3}s buffered={buffered:.3}s added={delta:.3}s", sample + 1);
        deltas.push(delta);
    }
    assert!(deltas.iter().all(|d| *d <= ZAP_LIMIT), "STOP: closed-segment HLS exceeds 0.5 s + 0.1 s scheduling margin: {deltas:?}");
}

#[test]
#[ignore = "transport experiment: requires localhost sockets, run outside the sandbox"]
fn zap_gate_http() {
    let media = ensure_media().expect("ffmpeg fixtures required for the transport gate");
    gate("HTTP TestServer live HLS", || Input(TestServer::start(media.clone(), Faults::default())));
}

fn hls_playable(url: String) -> tuner::model::Playable {
    use tuner::model::*;
    Playable {
        from_download: false,
        media: MediaRef::Channel(ChannelId(1)),
        kind: PlayableKind::Live,
        source_id: Some(SourceId(1)),
        item: None,
        title: "fixture".into(),
        subtitle: None,
        logo: None,
        number: None,
        url: tuner::redact::Secret::new(url),
        headers: HttpHeaders::default(),
        start_at: None,
        duration_hint: None,
    }
}

fn load_buffer(mpv: &Mpv, buffer: &tuner::player::hls::Buffer, target: Option<f64>) {
    while mpv.wait_event(0.0).is_some() {}
    let (url, offset) = buffer.view(target);
    let index = if target.is_some() { 0 } else { -1 };
    // Deliberately disable backward packet caching; these seeks must use recorded files.
    let opts = format!(
        "{},cache-secs=1,demuxer-readahead-secs=1,demuxer-max-back-bytes=0,demuxer-lavf-o=live_start_index={index}{}",
        buffer.options(),
        target.map_or(String::new(), |_| format!(",start={offset:.6}"))
    );
    mpv.command("loadfile", &[&url, "replace", "-1", &opts]).unwrap();
}

fn position(mpv: &Mpv, buffer: &tuner::player::hls::Buffer) -> f64 {
    mpv.get_property::<f64>("time-pos").unwrap() + buffer.snapshot().origin
}

fn playback_state(mpv: &Mpv) -> String {
    format!(
        "position={:?}, picture={:?}, paused={:?}, buffering={:?}, seeking={:?}, cache={:?}, eof={:?}",
        mpv.get_property::<f64>("time-pos").ok(),
        mpv.get_property::<String>("video-frame-info/picture-type").ok(),
        mpv.get_property::<bool>("pause").ok(),
        mpv.get_property::<bool>("paused-for-cache").ok(),
        mpv.get_property::<bool>("seeking").ok(),
        mpv.get_property::<f64>("demuxer-cache-duration").ok(),
        mpv.get_property::<bool>("eof-reached").ok(),
    )
}

/// DESIGN §43 §3, §8.4: three minutes of encrypted provider bytes, a rotating key and an
/// optional timestamp reset. The durable index is the same input the keeping store writes.
fn downloaded_hls_fixture(discontinuity: bool) -> tempfile::TempDir {
    use aws_lc_rs::cipher::{AES_128, PaddedBlockEncryptingKey, UnboundCipherKey};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let media = support::media::ensure_rolling_media();
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: &[u8]| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dir.path().join(name)).unwrap();
        file.write_all(bytes).unwrap();
    };
    // Synthetic fixture keys, rotated at the midpoint. Never provider data.
    let keys = [[0x31; 16], [0x72; 16]];
    write("0.key", &keys[0]);
    write("1.key", &keys[1]);
    let mut segments = Vec::new();
    for seq in 0..90 {
        let source = if discontinuity { (seq + 45) % 90 } else { seq };
        let mut bytes = fs::read(media.join(format!("rolling/seg{source:03}.ts"))).unwrap();
        let key_id = usize::from(seq >= 45);
        let key = PaddedBlockEncryptingKey::cbc_pkcs7(UnboundCipherKey::new(&AES_128, &keys[key_id]).unwrap()).unwrap();
        let context = key.encrypt(&mut bytes).unwrap();
        let iv: &[u8] = (&context).try_into().unwrap();
        let iv = iv.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        write(&format!("{seq}.ts"), &bytes);
        segments.push(tuner::player::hls::finish::Segment {
            id: seq,
            extension: "ts".into(),
            start: seq as f64 * 2.0,
            end: (seq + 1) as f64 * 2.0,
            discontinuity: u64::from(discontinuity && seq >= 45),
            key: Some((key_id as u64, format!("0x{iv}"))),
            map: None,
        });
    }
    tuner::player::hls::finish::Recording { tracks: vec![segments] }.save(dir.path()).unwrap();
    dir
}

fn finish_downloaded_hls(discontinuity: bool) -> (tempfile::TempDir, PathBuf) {
    let source = downloaded_hls_fixture(discontinuity);
    let result = tempfile::tempdir().unwrap();
    let output = result.path().join("1.mkv");
    let input = source.path().to_owned();
    let path = output.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(rt.spawn(async move {
        // A killed finish must leave everything needed to retry; restarting uses the durable
        // index and replaces any incomplete intermediate files (DESIGN §43 §8.4).
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop = cancel.clone();
        assert!(matches!(
            tuner::player::hls::finish::finish(&input, &path, &cancel, move |fraction| {
                if fraction > 0.0 {
                    stop.cancel();
                }
            })
            .await,
            Err(tuner::player::hls::finish::FinishError::Cancelled)
        ));
        assert!(input.join("index.json").exists());
        assert!(input.join("0.mkv").exists(), "interrupt a real finish after remuxing has started");
        let cancel = tokio_util::sync::CancellationToken::new();
        tuner::player::hls::finish::finish(&input, &path, &cancel, |_| {}).await.unwrap();
        assert!(!input.exists(), "verified output replaces the kept folder");
    }))
    .unwrap();
    (result, output)
}

fn downloaded_hls_plays_to_end(discontinuity: bool) {
    let (_dir, output) = finish_downloaded_hls(discontinuity);
    let mpv = mpv();
    // Three minutes of media, decoded at 10x so the transport gate takes about 18 seconds.
    mpv.set_property("speed", 10.0).unwrap();
    mpv.command("loadfile", &[output.to_str().unwrap(), "replace"]).unwrap();
    let started = Instant::now();
    first_frame(&mpv, started);
    let duration = mpv.get_property::<f64>("duration").unwrap();
    assert!((duration - 180.0).abs() <= 2.0, "three-minute recording, got {duration}");
    let mut last = mpv.get_property::<f64>("time-pos").unwrap();
    loop {
        let event = mpv.wait_event(0.01);
        if let Ok(position) = mpv.get_property::<f64>("time-pos") {
            assert!((0.0..=duration + 0.5).contains(&position), "downloaded HLS timeline outside the recording: {position:.3}/{duration:.3} s");
            assert!(position + 0.5 >= last, "downloaded HLS timeline went backwards: {last:.3} -> {position:.3}; {}", playback_state(&mpv));
            last = position;
        }
        if matches!(event, Some(Ok(Event::EndFile(_)))) {
            assert!(last >= 179.0, "recording must play through the final segment, stopped at {last:.3}");
            eprintln!("downloaded HLS: decoded {last:.3}/180 s in {:.2} s", started.elapsed().as_secs_f64());
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(35), "recording stalled: {}", playback_state(&mpv));
    }
}

#[test]
fn downloaded_hls_continuous_timestamps_play_to_end() {
    downloaded_hls_plays_to_end(false);
}

#[test]
fn downloaded_hls_discontinuity_plays_to_end() {
    downloaded_hls_plays_to_end(true);
}

/// Resume beyond the timestamp reset, outside the packet cache (DESIGN §43 §8.4).
#[test]
fn downloaded_hls_finished_discontinuity_seeks() {
    for discontinuity in [false, true] {
        let (_dir, output) = finish_downloaded_hls(discontinuity);
        let mpv = mpv();
        mpv.command("loadfile", &[output.to_str().unwrap(), "replace", "-1", "start=120"]).unwrap();
        let started = Instant::now();
        let mut restarted = false;
        loop {
            match mpv.wait_event(0.01) {
                Some(Ok(Event::EndFile(_))) => panic!("downloaded HLS ended before decoding at 120 s (discontinuity={discontinuity})"),
                Some(Ok(Event::PlaybackRestart)) => restarted = true,
                _ => {}
            }
            if restarted
                && mpv.get_property::<String>("video-frame-info/picture-type").is_ok_and(|p| matches!(p.as_str(), "I" | "P" | "B"))
                && let Ok(position) = mpv.get_property::<f64>("time-pos")
            {
                assert!((position - 120.0).abs() < 0.5, "resume target 120 s, got {position:.3} (discontinuity={discontinuity})");
                eprintln!("downloaded HLS: resumed at {position:.3} s (discontinuity={discontinuity})");
                break;
            }
            assert!(started.elapsed() < TIMEOUT, "resume stalled (discontinuity={discontinuity}): {}", playback_state(&mpv));
        }
    }
}

fn decoded_at(mpv: &Mpv, buffer: &tuner::player::hls::Buffer, target: f64, advance: f64) {
    let start = Instant::now();
    let first = first_frame(mpv, start);
    let actual = position(mpv, buffer);
    let mut picture = mpv.get_property::<String>("video-frame-info/picture-type").unwrap();
    let mut picture_changes = 0;
    let origin = buffer.snapshot().origin;
    assert!((actual - target).abs() < 0.6, "decoded at {actual}, expected {target}, origin={origin}; {}", playback_state(mpv));
    eprintln!("seek target={target:.3}, decoded={actual:.3}, first frame={first:.3}s; {}", playback_state(mpv));
    // Require forward progress as well as a decoded first frame after every seek.
    let began = Instant::now();
    loop {
        let _ = mpv.wait_event(0.01);
        if let Ok(next) = mpv.get_property::<String>("video-frame-info/picture-type")
            && next != picture
        {
            picture_changes += 1;
            picture = next;
        }
        // These fixtures have I/P frames every two seconds. Across old segments, require
        // output-picture changes as well as the clock, so audio alone cannot pass.
        if position(mpv, buffer) > actual + advance && (advance < 2.0 || picture_changes >= 2) {
            eprintln!("seek target={target:.3}: video advanced {advance:.2}s in {:.3}s", began.elapsed().as_secs_f64());
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs_f64(advance + 5.0),
            "video stalled after seek to {target}, started at {actual}, origin={origin}; {}",
            playback_state(mpv)
        );
    }
}

#[test]
#[ignore = "production HLS gate: real localhost HTTP and mpv; run outside the sandbox"]
fn zap_gate_http_provider_hls() {
    use tuner::model::TileId;
    use tuner::player::hls::Buffer;
    let media = support::media::ensure_rolling_media();
    let mut deltas = Vec::new();
    for sample in 0..3 {
        let measure = |buffered: bool| {
            let server = TestServer::start(media.clone(), Faults::default());
            let p = hls_playable(server.url("rolling/index.m3u8"));
            let cache = tempfile::tempdir().unwrap();
            let mpv = mpv();
            let start = Instant::now();
            let buffer = buffered.then(|| Buffer::start(&p, cache.path(), TileId::A, "fixture", 20.0).unwrap());
            if let Some(b) = &buffer {
                load_buffer(&mpv, b, None);
            } else {
                mpv.command("loadfile", &[p.url.expose(), "replace"]).unwrap();
            }
            let elapsed = first_frame(&mpv, start);
            drop(mpv);
            drop(buffer);
            if buffered {
                assert!(server.max_concurrent() <= 1, "provider socket overlap: {:?}", server.overlaps());
            }
            elapsed
        };
        let (direct, buffered) = if sample % 2 == 0 {
            (measure(false), measure(true))
        } else {
            let buffered = measure(true);
            (measure(false), buffered)
        };
        eprintln!("provider HLS sample {sample}: direct={direct:.4}s buffered={buffered:.4}s added={:.4}s", buffered - direct);
        deltas.push(buffered - direct);
    }
    deltas.sort_by(f64::total_cmp);
    assert!(deltas[1] <= 0.15, "provider HLS median added latency exceeds 0.15 s: {deltas:?}");
}

#[test]
#[ignore = "60+ second production rolling recorder, real mpv and HTTP; run outside the sandbox"]
fn rolling_hls_http_mpv() {
    use tuner::model::TileId;
    use tuner::player::hls::Buffer;
    let media = support::media::ensure_rolling_media();
    let server = TestServer::start(media.clone(), Faults::default());
    let p = hls_playable(server.url("rolling/index.m3u8"));
    let cache = tempfile::tempdir().unwrap();
    let mpv = mpv();
    let buffer = Buffer::start(&p, cache.path(), TileId::A, "fixture", 20.0).unwrap();
    let dir = buffer.directory();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    load_buffer(&mpv, &buffer, None);
    first_frame(&mpv, Instant::now());
    let initial = buffer.snapshot().range.unwrap();
    let max_segment = fs::read_dir(media.join("rolling"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|e| e == "ts"))
        .map(|e| e.metadata().unwrap().len())
        .max()
        .unwrap();
    let began = Instant::now();
    let mut outage = false;
    while began.elapsed() < Duration::from_secs(64) {
        if !outage && began.elapsed() >= Duration::from_secs(28) {
            let before = buffer.snapshot();
            server.set_status("/rolling/index.m3u8", 503);
            thread::sleep(Duration::from_millis(1400));
            server.set_status("/rolling/index.m3u8", 0);
            buffer.retry(); // Stand in for the controller's scheduled retry.
            assert_eq!(buffer.snapshot().epoch, before.epoch, "transient reconnect retains the buffer");
            outage = true;
        }
        let s = buffer.snapshot();
        assert!(!s.memory, "test needs at least 3 GB free");
        assert!(s.files <= 11 && s.bytes <= max_segment * 11, "20 s plus at most one segment");
        assert!(fs::read_dir(&dir).unwrap().count() <= 11);
        thread::sleep(Duration::from_millis(100));
    }
    let s = buffer.snapshot();
    assert!(s.range.unwrap().start > initial.start + 40.0, "oldest point rolls forward");
    assert!(position(&mpv, &buffer) > 55.0, "playback survived reconnect and kept advancing");
    let back = position(&mpv, &buffer) - 15.0;
    assert!(back >= s.range.unwrap().start);
    load_buffer(&mpv, &buffer, Some(back));
    decoded_at(&mpv, &buffer, back, 3.0);
    // Back to live; then the actual -10/+10 controls' arithmetic.
    let edge = buffer.snapshot().range.unwrap().end;
    load_buffer(&mpv, &buffer, Some(edge));
    decoded_at(&mpv, &buffer, edge, 0.2);
    let back = buffer.snapshot().range.unwrap().end - 10.0;
    load_buffer(&mpv, &buffer, Some(back));
    decoded_at(&mpv, &buffer, back, 3.0);
    let forward = (position(&mpv, &buffer) + 10.0).min(buffer.snapshot().range.unwrap().end);
    load_buffer(&mpv, &buffer, Some(forward));
    decoded_at(&mpv, &buffer, forward, 0.2);
    assert!(!buffer.snapshot().range.unwrap().behind(position(&mpv, &buffer)));
    // A long pause cannot stop recording or preserve files beyond the cap.
    mpv.set_property("pause", true).unwrap();
    let paused = position(&mpv, &buffer);
    thread::sleep(Duration::from_secs(24));
    let oldest = buffer.snapshot().range.unwrap().start;
    assert!(oldest > paused);
    load_buffer(&mpv, &buffer, Some(oldest));
    first_frame(&mpv, Instant::now());
    assert!(mpv.get_property::<bool>("pause").unwrap());
    assert!((position(&mpv, &buffer) - oldest).abs() < 0.6);
    mpv.set_property("pause", false).unwrap();
    assert!(server.max_concurrent() <= 1, "provider socket overlap: {:?}", server.overlaps());
    drop(mpv);
    drop(buffer); // channel change: old reader is joined before another can open
    assert!(!dir.exists(), "zap removes the old folder");
    let next = Buffer::start(&p, cache.path(), TileId::A, "fixture", 20.0).unwrap();
    assert!(next.snapshot().range.is_none(), "next channel starts empty");
    drop(next);
}

#[test]
#[ignore = "recorded playback without new publications: real localhost HTTP and mpv; run outside the sandbox"]
fn recorded_hls_http_mpv() {
    use tuner::model::TileId;
    use tuner::player::hls::Buffer;
    let media = support::media::ensure_rolling_media();
    let server = TestServer::start(media, Faults::default());
    let playlist = |count| {
        let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n".to_owned();
        for seq in 0..count {
            text.push_str(&format!("#EXTINF:2.0,\nseg{seq:03}.ts\n"));
        }
        text
    };
    server.set_body("/rolling/index.m3u8", Some(&playlist(1)));
    let p = hls_playable(server.url("rolling/index.m3u8"));
    let cache = tempfile::tempdir().unwrap();
    let mpv = mpv();
    let buffer = Buffer::start(&p, cache.path(), TileId::A, "fixture", 30.0).unwrap();
    load_buffer(&mpv, &buffer, None);
    first_frame(&mpv, Instant::now());
    // Publish once, then freeze this unfinished live playlist. A seek into the recording
    // must cross several segment boundaries without relying on any new provider bytes.
    server.set_body("/rolling/index.m3u8", Some(&playlist(12)));
    let began = Instant::now();
    while buffer.snapshot().range.is_none_or(|r| r.end < 22.0) {
        assert!(began.elapsed() < TIMEOUT, "recording did not fill");
        thread::sleep(POLL);
    }
    let frozen = buffer.snapshot();
    assert!(!frozen.memory, "test needs at least 3 GB free");
    let segment_requests = || server.requests().iter().filter(|r| r.path.ends_with(".ts")).count();
    let requests = segment_requests();
    // Includes a seek near a segment boundary; packet back-cache is disabled in load_buffer.
    for target in [5.3, 3.85] {
        load_buffer(&mpv, &buffer, Some(target));
        decoded_at(&mpv, &buffer, target, 6.0);
        assert_eq!(buffer.snapshot().range, frozen.range, "no new publication may rescue the reader");
        assert_eq!(segment_requests(), requests, "the seek must use the recording");
    }
    drop(mpv);
    drop(buffer);
    assert!(server.max_concurrent() <= 1);
}

#[test]
#[ignore = "master, AES-128, headers and expired segment URLs over real HTTP; run outside the sandbox"]
fn provider_hls_master_keys_headers_and_expiring_urls() {
    use tuner::model::TileId;
    use tuner::player::hls::Buffer;
    let fixture = support::media::ensure_rolling_media();
    let provider = tempfile::tempdir().unwrap();
    let key = [7_u8; 16];
    fs::write(provider.path().join("key.key"), key).unwrap();
    for n in 0..6 {
        let status = Command::new("openssl")
            .args(["enc", "-aes-128-cbc", "-K", "07070707070707070707070707070707", "-iv"])
            .arg(format!("{:032x}", 100 + n))
            .arg("-in")
            .arg(fixture.join(format!("rolling/seg{n:03}.ts")))
            .arg("-out")
            .arg(provider.path().join(format!("{n}.ts")))
            .status()
            .unwrap();
        assert!(status.success());
    }
    fs::copy(provider.path().join("4.ts"), provider.path().join("fresh.ts")).unwrap();
    let server = TestServer::start(provider.path().into(), Faults::default());
    server.set_body(
        "/master.m3u8",
        Some("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nunselected.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nselected.m3u8\n"),
    );
    let playlist = |start: u64, fresh: bool| {
        let mut p = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.key?token=private\"\n",
            start + 100
        );
        for n in start..start + 3 {
            let uri = if fresh && n == 4 { "fresh.ts".into() } else { format!("{n}.ts") };
            p.push_str(&format!("#EXTINF:2,private title\n{uri}?token=private\n"));
        }
        p
    };
    server.set_body("/selected.m3u8", Some(&playlist(1, false)));
    let mut p = hls_playable(server.url("master.m3u8"));
    p.headers.user_agent = Some("Tuner-fixture".into());
    p.headers.referrer = Some("https://fixture.test/".into());
    p.headers.origin = Some("https://fixture.test".into());
    p.headers.extra.push(("Cookie".into(), "session=private".into()));
    let cache = tempfile::tempdir().unwrap();
    let buffer = Buffer::start(&p, cache.path(), TileId::A, "default", 20.0).unwrap();
    let mpv = mpv();
    load_buffer(&mpv, &buffer, None);
    first_frame(&mpv, Instant::now());
    let before = buffer.snapshot();
    server.set_status("/4.ts", 403);
    server.set_body("/selected.m3u8", Some(&playlist(2, false)));
    let start = Instant::now();
    while !server.requests_for("/4.ts").iter().any(|r| r.status == 403) {
        assert!(start.elapsed() < TIMEOUT);
        thread::sleep(POLL);
    }
    server.set_body("/selected.m3u8", Some(&playlist(2, true)));
    buffer.retry(); // The production controller authorises retries through its backoff.
    while buffer.snapshot().range.unwrap().end <= before.range.unwrap().end {
        assert!(start.elapsed() < TIMEOUT, "must refetch the playlist to obtain fresh segment credentials");
        thread::sleep(POLL);
    }
    assert_eq!(buffer.snapshot().epoch, before.epoch);
    let target = buffer.snapshot().range.unwrap().end - 2.0;
    load_buffer(&mpv, &buffer, Some(target));
    decoded_at(&mpv, &buffer, target, 0.2);
    assert!(server.requests_for("/unselected.m3u8").is_empty());
    assert!(!server.requests_for("/fresh.ts").is_empty());
    for r in server.requests() {
        assert_eq!(r.header("user-agent"), Some("Tuner-fixture"));
        assert_eq!(r.header("referer"), Some("https://fixture.test/"));
        assert_eq!(r.header("origin"), Some("https://fixture.test"));
        assert_eq!(r.header("cookie"), Some("session=private"));
    }
    assert!(server.max_concurrent() <= 1, "one provider socket, keys included");
    for entry in fs::read_dir(buffer.directory()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(name.split('.').next().unwrap().bytes().all(|b| b.is_ascii_digit()));
    }
    drop(mpv);
    drop(buffer);
}

/// ENDLIST belongs to the archive recorder; live-only fallback must not hand it to mpv.
#[test]
fn archive_endlist_is_saved_and_finished_into_a_playable_file() {
    use tuner::player::hls::{Buffer, RecordingLimits, finish};
    let media = support::media::ensure_rolling_media();
    let server = TestServer::start(media, Faults::default());
    let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n".to_owned();
    for seq in 0..6 {
        playlist.push_str(&format!("#EXTINF:2.0,\nseg{seq:03}.ts\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    server.set_body("/rolling/archive.m3u8", Some(&playlist));
    let p = hls_playable(server.url("rolling/archive.m3u8"));
    let root = tempfile::tempdir().unwrap();
    let kept = root.path().join("1");
    let resources = tuner::downloads::limits::Resources::default();
    resources.configure(u64::MAX, 0.000001);
    let buffer = Buffer::start_keep(
        &p,
        &kept,
        "fixture",
        RecordingLimits { reserve_bytes: 0, max_bytes: u64::MAX, limits: Some(resources.job(tuner::model::DownloadId(1)).recording()) },
        true,
    )
    .unwrap();
    let started = Instant::now();
    loop {
        let state = buffer.snapshot();
        assert!(!state.failed, "archive failed: {:?}", state.failure);
        if state.ended {
            break;
        }
        assert!(started.elapsed() < TIMEOUT, "archive was throttled or never ended");
        thread::sleep(POLL);
    }
    drop(buffer);
    let index = finish::Recording::load(&kept).unwrap();
    assert_eq!(index.tracks[0].len(), 6);
    let output = root.path().join("1.mkv");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(finish::finish(&kept, &output, &tokio_util::sync::CancellationToken::new(), |_| {})).unwrap();
    let mpv = mpv();
    mpv.command("loadfile", &[output.to_str().unwrap(), "replace"]).unwrap();
    first_frame(&mpv, Instant::now());
    assert!((mpv.get_property::<f64>("duration").unwrap() - 12.0).abs() < 2.0);
    mpv.command("seek", &["8", "absolute+exact"]).unwrap();
    let started = Instant::now();
    while mpv.get_property::<f64>("time-pos").unwrap_or(0.0) < 7.5 {
        assert!(started.elapsed() < TIMEOUT);
        thread::sleep(POLL);
    }
}
