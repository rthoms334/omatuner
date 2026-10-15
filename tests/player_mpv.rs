//! Headless libmpv player tests (DESIGN §23 "Player (headless)", WP-07 acceptance): the real
//! `Player` (mpv `vo=null`, `ao=null`) with a fake `MediaResolver` and a recording
//! `PlayerStore`, playing ffmpeg-generated media from a local fault-injecting HTTP server.
//! Every test SKIPS with a note when ffmpeg is unavailable (`support::media::ensure_media`).

mod support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use support::media::{Faults, KeepAlive, SessionLimit, TestServer, ensure_media};
use tuner::config::{Config, SubtitleAuto};
use tuner::model::{
    ChannelId, Collection, EpisodeId, HistoryEntry, HttpHeaders, ItemKey, ItemKind, ItemRef, MediaRef, MovieId, PlayOrigin, Playable, PlayableKind,
    SourceId, TileId, WatchProgress, ZapList,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::player::{
    MediaResolver, PlayRequest, Player, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerEvent, PlayerHandle, PlayerSnapshot,
    PlayerState, PlayerStore,
};
use tuner::redact::Secret;

// ---------------------------------------------------------------------------------------------
// fakes

/// `record_failure` calls: `None` = cleared after a successful play.
type FailureLog = Arc<Mutex<Vec<(ItemRef, Option<PlayerErrorKind>)>>>;

#[derive(Clone, Default)]
struct Recorded {
    progress: Arc<Mutex<Vec<WatchProgress>>>,
    history: Arc<Mutex<Vec<HistoryEntry>>>,
    failures: FailureLog,
    video: Arc<Mutex<Vec<(ItemRef, i64, i64)>>>,
}

struct RecordingStore(Recorded);

impl PlayerStore for RecordingStore {
    fn save_progress(&mut self, p: WatchProgress) {
        self.0.progress.lock().unwrap().push(p);
    }
    fn push_history(&mut self, e: HistoryEntry) {
        self.0.history.lock().unwrap().push(e);
    }
    fn record_failure(&mut self, item: ItemRef, kind: Option<PlayerErrorKind>) {
        self.0.failures.lock().unwrap().push((item, kind));
    }
    fn record_video(&mut self, item: ItemRef, width: i64, height: i64) {
        self.0.video.lock().unwrap().push((item, width, height));
    }
}

/// Resolves from a fixed table; unknown media → `Resolve` error.
struct TableResolver(HashMap<MediaRef, Playable>);

impl MediaResolver for TableResolver {
    fn resolve(&mut self, media: &MediaRef, _resume: bool) -> Result<Playable, PlayerError> {
        self.0.get(media).cloned().ok_or(PlayerError { kind: PlayerErrorKind::Resolve, message: "unknown item".into() })
    }
    fn skip_on_zap(&mut self, _ch: ChannelId) -> bool {
        false
    }
    fn is_locked(&mut self, _ch: ChannelId) -> bool {
        false
    }
}

fn item(key: &str, kind: ItemKind) -> ItemRef {
    ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) }
}

fn playable(media: MediaRef, kind: PlayableKind, url: String) -> Playable {
    let key = format!("{media:?}");
    let item_kind = match kind {
        PlayableKind::Movie => ItemKind::Movie,
        PlayableKind::Episode { .. } => ItemKind::Episode,
        _ => ItemKind::Channel,
    };
    Playable {
        media,
        kind,
        item: Some(item(&key, item_kind)),
        source_id: Some(SourceId(1)),
        title: "Test stream".into(),
        subtitle: None,
        logo: None,
        number: None,
        url: Secret::new(url),
        headers: HttpHeaders::default(),
        start_at: None,
        duration_hint: None,
    }
}

fn live(ch: i64, url: String) -> Playable {
    playable(MediaRef::Channel(ChannelId(ch)), PlayableKind::Live, url)
}
fn movie(id: i64, url: String) -> Playable {
    playable(MediaRef::Movie(MovieId(id)), PlayableKind::Movie, url)
}

// ---------------------------------------------------------------------------------------------
// harness

/// A sliding HLS playlist without ENDLIST. All seeks use its existing demuxer; no load or
/// provider byte-range request is needed. Pausing must keep fetching new live segments.
#[test]
fn live_rewind_cache_seek_pause_clamp_and_channel_change() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    live_rewind_behaviour(srv.url("live/index.m3u8"), srv.url("live/index.m3u8"));
}

/// Same real decoder/cache checks through paced MPEG-TS FIFOs, usable when bind(2) is
/// prohibited by the sandbox. Each producer is continuous, non-seekable and runs in real time.
#[test]
fn live_rewind_local_pipe_seek_pause_clamp_and_channel_change() {
    let Some((media, _serial)) = setup() else { return };
    let first = UnseekableStream::start(&media);
    let second = UnseekableStream::start(&media);
    live_rewind_behaviour(first.url(), second.url());
}

fn live_rewind_behaviour(first: String, second: String) {
    let cache = tempfile::tempdir().unwrap();
    let mut h = Harness::start_in(headless_config(), vec![live(1, first), live(2, second)], Some(cache.path().into()));
    h.play(MediaRef::Channel(ChannelId(1)), true);
    h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && s.video_size.is_some());
    h.player.mark_rendered_for_tests();
    let s = h.wait_snapshot(Duration::from_secs(25), |s| s.state == PlayerState::Playing && s.live_buffer.is_some_and(|r| r.end - r.start > 12.0));
    let generation = s.load_generation;
    assert!(!s.video_suspended, "a hidden live picture must keep caching video");
    h.handle.send(PlayerCommand::SetPause(true));
    let paused = h.wait_snapshot(LONG, |s| s.paused);
    let before = paused.position.unwrap();
    let edge = paused.live_buffer.unwrap().end;
    h.wait_snapshot(Duration::from_secs(8), |s| s.live_buffer.is_some_and(|r| r.end > edge + 2.0));
    assert!((h.snapshot().position.unwrap() - before).abs() < 0.5, "pause preserves the point while buffering");

    h.handle.send(PlayerCommand::SeekBy(-10.0));
    let back =
        h.wait_snapshot(LONG, |s| s.position.is_some_and(|p| p < before - 8.5) && s.live_buffer.is_some_and(|r| r.behind(s.position.unwrap())));
    assert_eq!(back.load_generation, generation);

    // First jump to live, then back and immediately forward: +10 returns to the edge.
    h.handle.send(PlayerCommand::JumpToLive);
    h.wait_snapshot(LONG, |s| !s.paused && s.live_buffer.is_some_and(|r| !r.behind(s.position.unwrap_or(0.0))));
    h.handle.send(PlayerCommand::SetPause(true));
    h.wait_snapshot(LONG, |s| s.paused);
    let at_live = h.snapshot().position.unwrap();
    h.handle.send(PlayerCommand::SeekBy(-10.0));
    h.wait_snapshot(LONG, |s| s.position.is_some_and(|p| p < at_live - 8.5));
    h.handle.send(PlayerCommand::SeekBy(10.0));
    h.wait_snapshot(LONG, |s| s.live_buffer.is_some_and(|r| !r.behind(s.position.unwrap_or(0.0))));

    let mark = h.mark();
    h.handle.send(PlayerCommand::SeekBy(-3600.0));
    assert!(h.wait_event_from(mark, LONG, |e| matches!(e, PlayerEvent::LiveSeekBoundary { oldest: true })).is_some());
    h.wait_snapshot(LONG, |s| s.live_buffer.is_some_and(|r| (s.position.unwrap_or(-100.0) - r.start).abs() < 1.0));
    assert_eq!(h.snapshot().load_generation, generation, "cached seeks never reload");

    let root = cache.path().join("live-rewind");
    let dir = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    // mpv unlinks its file immediately; a stale file also must not survive a new channel.
    std::fs::write(dir.join("old-buffer"), b"stale").unwrap();
    h.play(MediaRef::Channel(ChannelId(2)), true);
    let new = h.wait_snapshot(LONG, |s| s.load_generation > generation && s.state == PlayerState::Playing && s.live_buffer.is_some());
    assert!(!dir.join("old-buffer").exists());
    assert!(new.live_buffer.unwrap().end - new.live_buffer.unwrap().start < 8.0, "new channel has no old history");
    drop(h);
    assert!(!dir.exists(), "exit removes the private buffer directory");
}

struct Harness {
    player: Player,
    handle: PlayerHandle,
    events: Receiver<AppEvent>,
    seen: Vec<PlayerEvent>,
    rec: Recorded,
}

fn headless_config() -> Config {
    let mut cfg = Config::default();
    cfg.player.mpv_options.insert("vo".into(), "null".into());
    cfg.player.mpv_options.insert("ao".into(), "null".into());
    cfg.player.network_timeout_s = 5;
    cfg
}

impl Harness {
    fn start(cfg: Config, table: Vec<Playable>) -> Self {
        Self::start_in(cfg, table, None)
    }

    fn start_in(cfg: Config, table: Vec<Playable>, cache_dir: Option<PathBuf>) -> Self {
        let (bus, events) = EventBus::new(None);
        let rec = Recorded::default();
        let deps = PlayerDeps {
            cache_dir,
            bus,
            store: Box::new(RecordingStore(rec.clone())),
            resolver: Box::new(TableResolver(table.into_iter().map(|p| (p.media.clone(), p)).collect())),
            cast: None,
        };
        let player = Player::start(&cfg, deps).expect("headless mpv");
        let handle = player.handle();
        Self { player, handle, events, seen: Vec::new(), rec }
    }

    fn play(&self, media: MediaRef, immediate: bool) {
        self.handle.send(PlayerCommand::Play(PlayRequest { media, zap: None, origin: PlayOrigin::Fullscreen, resume: true, immediate }));
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            if let AppEvent::Player(ev) = ev {
                self.seen.push(ev);
            }
        }
    }

    /// Position in the event history; take it BEFORE triggering what you then wait for.
    fn mark(&mut self) -> usize {
        self.drain();
        self.seen.len()
    }

    /// Wait until an event matching `pred` is at index ≥ `from` of the history.
    fn wait_event_from(&mut self, from: usize, timeout: Duration, pred: impl Fn(&PlayerEvent) -> bool) -> Option<PlayerEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = self.seen.iter().skip(from).find(|e| pred(e)) {
                return Some(ev.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if let Ok(AppEvent::Player(ev)) = self.events.recv_timeout(left.min(Duration::from_millis(100))) {
                self.seen.push(ev);
            }
        }
    }

    /// Wait until an event matching `pred` arrives (the whole history counts).
    fn wait_event(&mut self, timeout: Duration, pred: impl Fn(&PlayerEvent) -> bool) -> Option<PlayerEvent> {
        self.wait_event_from(0, timeout, pred)
    }

    /// Wait for a `StateChanged` matching `pred` at index ≥ `from`.
    fn wait_state_from(&mut self, from: usize, timeout: Duration, pred: impl Fn(&PlayerState) -> bool) -> PlayerState {
        match self.wait_event_from(from, timeout, |e| matches!(e, PlayerEvent::StateChanged(s) if pred(s))) {
            Some(PlayerEvent::StateChanged(s)) => s,
            _ => panic!("state not reached within {timeout:?}; states seen: {:?}", self.states()),
        }
    }

    /// Wait for a `StateChanged` matching `pred` (seen earlier or arriving within `timeout`).
    fn wait_state(&mut self, timeout: Duration, pred: impl Fn(&PlayerState) -> bool) -> PlayerState {
        self.wait_state_from(0, timeout, pred)
    }

    fn states(&self) -> Vec<PlayerState> {
        self.seen.iter().filter_map(|e| if let PlayerEvent::StateChanged(s) = e { Some(s.clone()) } else { None }).collect()
    }

    fn snapshot(&self) -> Arc<PlayerSnapshot> {
        self.handle.snapshot()
    }

    /// Poll the snapshot until `pred` holds.
    fn wait_snapshot(&self, timeout: Duration, pred: impl Fn(&PlayerSnapshot) -> bool) -> Arc<PlayerSnapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.snapshot();
            if pred(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "snapshot condition not met within {timeout:?}: {:?}", s.state);
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.player.shutdown();
    }
}

const LONG: Duration = Duration::from_secs(30);

/// Debug messages of the player controller, in order (see [`controller_log`]).
static CONTROLLER_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The INFO-and-above part of [`CONTROLLER_LOG`] (what a user's log file holds).
static INFO_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Records the `message` of every event it is given into [`CONTROLLER_LOG`].
struct CaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut m = Message(String::new());
        event.record(&mut m);
        if *event.metadata().level() <= tracing::Level::INFO {
            INFO_LOG.lock().unwrap_or_else(|e| e.into_inner()).push(m.0.clone());
        }
        CONTROLLER_LOG.lock().unwrap_or_else(|e| e.into_inner()).push(m.0);
    }
}

/// The controller's debug lines since the last call (the decisions mpv itself can't show:
/// stop-before-start, `loadfile` entry ids, END_FILEs as processed).
fn controller_log() -> Vec<String> {
    std::mem::take(&mut *CONTROLLER_LOG.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Test media (or `None` → skip) plus a process-wide lock held for the whole test.
///
/// The tests run one at a time: libav has ONE global log callback, and mpv routes ffmpeg's
/// messages (including the "HTTP error 403" lines the classifier reads) to a single mpv
/// instance per process. The app has exactly one instance; parallel tests would steal each
/// other's HTTP statuses. `RUST_LOG=tuner=debug` shows the controller's decisions.
fn setup() -> Option<(PathBuf, MutexGuard<'static, ()>)> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, filter::Targets};
    static SERIAL: Mutex<()> = Mutex::new(());
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_test_writer().with_filter(EnvFilter::from_default_env()))
        .with(CaptureLayer.with_filter(Targets::new().with_target("tuner::player::controller", tracing::Level::DEBUG)))
        .try_init();
    let media = ensure_media()?;
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    controller_log(); // earlier tests' lines
    INFO_LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
    Some((media, guard))
}

/// The entry id of the first `loadfile` in `log` at or after index `from` (and its index).
fn loadfile_entry(log: &[String], from: usize) -> Option<(usize, i64)> {
    log.iter().enumerate().skip(from).find_map(|(i, l)| {
        let id = l.strip_prefix("loadfile: entry Some(")?.strip_suffix(')')?.parse().ok()?;
        Some((i, id))
    })
}

/// Index of the controller's END_FILE line of `entry` in `log`.
fn end_file_at(log: &[String], entry: i64) -> Option<usize> {
    let prefix = format!("END_FILE entry {entry}:");
    log.iter().position(|l| l.starts_with(&prefix))
}

/// The stop-before-start sequence after a switch to a same-source stream: the old entry is
/// stopped, its END_FILE processed, and only THEN the new `loadfile` is issued (a plain
/// `loadfile replace` issues the new load first and sees the old END_FILE afterwards).
fn assert_stopped_before_start(log: &[String]) {
    let (stop_at, old) = log
        .iter()
        .enumerate()
        .find_map(|(i, l)| Some((i, l.strip_prefix("stop-before-start: stopping entry ")?.split(' ').next()?.parse::<i64>().ok()?)))
        .unwrap_or_else(|| panic!("no stop-before-start: {log:#?}"));
    let ended = end_file_at(log, old).unwrap_or_else(|| panic!("END_FILE of entry {old} never processed: {log:#?}"));
    let (loaded, new) = loadfile_entry(log, stop_at).unwrap_or_else(|| panic!("no loadfile after the stop: {log:#?}"));
    assert!(stop_at < ended && ended < loaded, "stop {stop_at}, END_FILE {ended}, loadfile {loaded}: {log:#?}");
    assert_ne!(old, new);
    assert!(!log.iter().any(|l| l.contains("did not report END_FILE in time")), "waited for END_FILE, not the deadline: {log:#?}");
}

fn is_playing(s: &PlayerState) -> bool {
    matches!(s, PlayerState::Playing)
}

/// Pause so ordinary playback cannot masquerade as a successful forward skip.
fn pause_vod(h: &Harness) -> f64 {
    h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && s.position.is_some());
    h.handle.send(PlayerCommand::SetPause(true));
    let s = h.wait_snapshot(LONG, |s| s.paused && s.state == PlayerState::Paused);
    eprintln!("VOD paused: position={:?}, seekable={}", s.position, s.seekable);
    s.position.unwrap()
}

fn assert_skip(h: &Harness, from: f64, delta: f64) -> f64 {
    let want = (from + delta).max(0.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    h.handle.send(PlayerCommand::SeekBy(delta));
    loop {
        let s = h.snapshot();
        // MPEG-TS can land on a nearby keyframe (the fixture's GOPs are irregular).
        if s.position.is_some_and(|p| (p - want).abs() < 2.0) {
            assert!(s.seekable, "successful seek must be reflected in the snapshot");
            return s.position.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "SeekBy({delta}) from {from}: wanted {want}, got {:?}, seekable={}, state={:?}",
            s.position,
            s.seekable,
            s.state
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_small_skips(h: &Harness) {
    let from = pause_vod(h);
    let forward = assert_skip(h, from, 10.0);
    assert_skip(h, forward, -10.0);
    h.handle.send(PlayerCommand::SeekTo(4.0));
    h.wait_snapshot(Duration::from_secs(5), |s| s.position.is_some_and(|p| (p - 4.0).abs() < 0.6));
}

#[test]
fn vod_skip_plain_range_sources() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    skips_plain_files(|path| srv.url(path));
    assert!(srv.requests().iter().any(|r| r.status == 206 && r.headers.iter().any(|(k, _)| k == "range")), "mpv used HTTP Range");
}

#[test]
fn vod_skip_local_formats() {
    let Some((media, _serial)) = setup() else { return };
    skips_plain_files(|path| media.join(path).to_string_lossy().into_owned());
}

fn skips_plain_files(url: impl Fn(&str) -> String) {
    // Indexed files, index-free files and finite HLS: all the provider VOD shapes.
    for path in ["vod.mp4", "vod.mkv", "vod.ts", "vod_nocues.mkv", "long/index.m3u8"] {
        eprintln!("plain VOD: {path}");
        let m = movie(1, url(path));
        let h = Harness::start(headless_config(), vec![m.clone()]);
        h.play(m.media, true);
        assert_small_skips(&h);
    }
}

#[test]
fn vod_skip_second_file_and_after_live() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    skips_between_files(|path| srv.url(path), None);
}

fn skips_between_files(url: impl Fn(&str) -> String, limit: Option<u32>) {
    let first = movie(1, url("vod.mp4"));
    let second = playable(MediaRef::Episode(EpisodeId(2)), PlayableKind::Episode { season: 1, episode: 2 }, url("vod.mkv"));
    // A seekable transport classified as live must still have live controls.
    let ch = live(3, url("vod.ts"));
    let h = Harness::start(headless_config(), vec![first.clone(), second.clone(), ch.clone()]);
    h.handle.send(PlayerCommand::SetConnectionLimit { source: SourceId(1), max: limit });
    for p in [first, second.clone(), ch, second] {
        let generation = h.snapshot().load_generation;
        h.play(p.media.clone(), true);
        h.wait_snapshot(LONG, |s| s.load_generation > generation && s.state == PlayerState::Playing && s.position.is_some());
        if p.kind.is_live() {
            h.wait_snapshot(LONG, |s| s.seekable && s.live_buffer.is_some());
        } else {
            assert_small_skips(&h);
        }
    }
}

#[test]
fn vod_skip_after_video_suspend_resume() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    skips_after_video_suspend(srv.url("vod.mkv"));
}

#[test]
fn vod_skip_local_after_video_suspend_resume() {
    let Some((media, _serial)) = setup() else { return };
    skips_after_video_suspend(media.join("vod.mkv").to_string_lossy().into_owned());
}

fn skips_after_video_suspend(url: String) {
    let m = movie(1, url);
    let h = Harness::start(headless_config(), vec![m.clone()]);
    h.play(m.media, true);
    h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && s.video_size.is_some());
    h.player.mark_rendered_for_tests();
    h.wait_snapshot(LONG, |s| s.video_suspended);
    h.player.mark_rendered_for_tests();
    h.handle.send(PlayerCommand::ResumeVideo);
    h.wait_snapshot(LONG, |s| !s.video_suspended && s.video_size.is_some());
    assert_small_skips(&h);
}

/// Also runs without sockets: property notifications and live/VOD transitions belong to
/// mpv, independently of the HTTP transport covered above.
#[test]
fn vod_skip_local_files_and_after_live() {
    let Some((media, _serial)) = setup() else { return };
    skips_between_files(|path| media.join(path).to_string_lossy().into_owned(), None);
}

#[test]
fn vod_skip_local_replace_after_live() {
    let Some((media, _serial)) = setup() else { return };
    // Two connections allow loadfile to replace directly, without stop-before-start.
    skips_between_files(|path| media.join(path).to_string_lossy().into_owned(), Some(2));
}

#[test]
fn vod_skip_unseekable_reports_why() {
    let Some((media, _serial)) = setup() else { return };
    let stream = UnseekableStream::start(&media);
    let m = movie(1, stream.url());
    let mut cfg = headless_config();
    cfg.player.mpv_options.insert("cache".into(), "no".into());
    let mut h = Harness::start(cfg, vec![m.clone()]);
    h.play(m.media, true);
    let before = pause_vod(&h);
    assert!(!h.snapshot().seekable);
    for cmd in [PlayerCommand::SeekBy(10.0), PlayerCommand::SeekBy(-10.0), PlayerCommand::SeekTo(10.0)] {
        let mark = h.mark();
        h.handle.send(cmd);
        assert!(
            h.wait_event_from(mark, Duration::from_secs(3), |e| matches!(e, PlayerEvent::SeekUnavailable)).is_some(),
            "a refused seek must explain why"
        );
        assert!((h.snapshot().position.unwrap() - before).abs() < 0.1);
    }
}

/// A pipe cannot provide byte ranges. Disable the demuxer cache in its tests too, so the
/// refusal is a real mpv limitation, independent of HTTP and its sandbox restrictions.
struct UnseekableStream {
    dir: tempfile::TempDir,
    writer: std::process::Child,
}

impl UnseekableStream {
    fn start(media: &std::path::Path) -> Self {
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("stream.ts");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let writer = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-re", "-stream_loop", "-1", "-i"])
            .arg(media.join("vod.ts"))
            .args(["-c", "copy", "-f", "mpegts"])
            .arg(fifo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { dir, writer }
    }

    fn url(&self) -> String {
        self.dir.path().join("stream.ts").to_string_lossy().into_owned()
    }
}

impl Drop for UnseekableStream {
    fn drop(&mut self) {
        let _ = self.writer.kill();
        let _ = self.writer.wait();
    }
}

/// Real screen input → UiCtx routing → controller → mpv. The resolver reads actual movie
/// and episode rows, so the screen's PlayerVod context isn't supplied by the test.
#[test]
fn vod_skip_osd_buttons_and_arrow_keys() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    skips_from_screen(srv.url("vod.mkv"), true);
}

#[test]
fn vod_skip_local_osd_buttons_and_arrow_keys() {
    let Some((media, _serial)) = setup() else { return };
    skips_from_screen(media.join("vod.mkv").to_string_lossy().into_owned(), true);
}

#[test]
fn vod_skip_unseekable_osd_and_keys_flash() {
    let Some((media, _serial)) = setup() else { return };
    let stream = UnseekableStream::start(&media);
    skips_from_screen(stream.url(), false);
}

fn skips_from_screen(url: String, seekable: bool) {
    use eframe::egui;
    use egui_kittest::kittest::Queryable;
    use tuner::keymap::{self, Action, KeyContext};
    use tuner::ui::screens::player::PlayerScreen;
    use tuner::ui::testing::TestUi;
    use tuner::ui::{Route, Screen};

    let mut cfg = headless_config();
    if !seekable {
        cfg.player.mpv_options.insert("cache".into(), "no".into());
    }
    let mut t = TestUi::with_config(cfg).expect("headless UI");
    assert_eq!(t.config.player.skip_small_s, 10);
    let stream = serde_json::json!({ "t": "url", "url": url }).to_string();
    let (movie, episode) = t
        .services()
        .store
        .writer()
        .blocking(move |c| {
            let source = support::seed::seed_source(c, "VOD")?;
            let movies = support::seed::seed_movies(c, source, 1)?;
            support::seed::seed_series_with_episodes(c, source, 1, 1, 1)?;
            c.execute("UPDATE movie SET stream=?1", [&stream])?;
            c.execute("UPDATE episode SET stream=?1", [&stream])?;
            let episode = c.query_row("SELECT id FROM episode", [], |r| r.get::<_, EpisodeId>(0))?;
            Ok((movies[0], episode))
        })
        .unwrap();
    let handle = t.services().tiles.focused();
    for media in [MediaRef::Movie(movie), MediaRef::Episode(episode)] {
        if !seekable && matches!(media, MediaRef::Episode(_)) {
            break; // the pipe has a single reader
        }
        let generation = handle.snapshot().load_generation;
        handle.send(PlayerCommand::Play(PlayRequest { media, zap: None, origin: PlayOrigin::Fullscreen, resume: false, immediate: true }));
        let deadline = Instant::now() + LONG;
        loop {
            let s = handle.snapshot();
            if s.load_generation > generation && s.state == PlayerState::Playing && s.position.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "VOD didn't play: {:?}", s.state);
            std::thread::sleep(Duration::from_millis(50));
        }
        handle.send(PlayerCommand::SetPause(true));
        while !handle.snapshot().paused {
            assert!(Instant::now() < deadline, "VOD didn't pause");
            std::thread::sleep(Duration::from_millis(50));
        }
        t.refresh_snapshot();
        let mut screen = PlayerScreen::default();
        t.with_ctx(|ctx| screen.on_enter(ctx, &Route::Player));
        let mut ui = egui_kittest::Harness::builder().with_size(egui::vec2(1600.0, 900.0)).build_ui_state(
            |ui, (t, screen): &mut (TestUi, PlayerScreen)| {
                t.video.begin_frame();
                t.refresh_snapshot();
                while let Ok(ev) = t.events.try_recv() {
                    t.with_ctx(|ctx| screen.on_event(ctx, &ev));
                }
                t.with_ctx(|ctx| screen.ui(ui, ctx));
            },
            (t, screen),
        );
        ui.run_steps(3);
        assert_eq!(ui.state().1.key_contexts(), vec![KeyContext::PlayerVod]);
        assert_eq!(handle.snapshot().seekable, seekable);
        // Repeated skips must add up, rather than seek to fixed timestamps or send a
        // live zap. Both paths exercise +10 and -10 SeekBy commands.
        for button in [true, false] {
            if button && !seekable {
                assert!(ui.query_by_label("Back").is_none() && ui.query_by_label("Forward").is_none());
                continue;
            }
            for forward in [true, true, false, false] {
                let before = handle.snapshot().position.unwrap();
                if button {
                    ui.get_by_label(if forward { "Forward" } else { "Back" }).click();
                } else {
                    let key = if forward { egui::Key::ArrowRight } else { egui::Key::ArrowLeft };
                    let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
                    let action = keymap::resolve(&ui.state().1.key_contexts(), &event).expect("arrow binding");
                    assert_eq!(action, if forward { Action::SeekForward } else { Action::SeekBack });
                    let (t, screen) = ui.state_mut();
                    assert!(t.with_ctx(|ctx| screen.on_action(ctx, action)));
                }
                let want = (before + if forward { 10.0 } else { -10.0 }).max(0.0);
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    ui.run_steps(1);
                    let s = handle.snapshot();
                    if !seekable {
                        fn contains_text(shape: &egui::Shape, text: &str) -> bool {
                            match shape {
                                egui::Shape::Text(t) => t.galley.job.text.contains(text),
                                egui::Shape::Vec(v) => v.iter().any(|s| contains_text(s, text)),
                                _ => false,
                            }
                        }
                        assert!((s.position.unwrap() - before).abs() < 0.1);
                        if ui.output().shapes.iter().any(|s| contains_text(&s.shape, "Can't skip in this stream")) {
                            break;
                        }
                    } else if s.position.is_some_and(|p| (p - want).abs() < 0.6) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "button={button}, forward={forward}: wanted {want}, got {:?}, seekable={}",
                        s.position,
                        s.seekable
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        if seekable {
            let before = handle.snapshot().position.unwrap();
            for _ in 0..3 {
                ui.get_by_label("Forward").click();
                ui.run_steps(2);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                ui.run_steps(1);
                let moved = handle.snapshot().position.is_some_and(|p| (p - before - 30.0).abs() < 0.6);
                let ripple = ui.output().shapes.iter().any(|s| {
                    matches!(&s.shape,
                    egui::Shape::Text(t) if t.galley.job.text == "⟳ 30")
                });
                if moved && ripple {
                    break;
                }
                assert!(Instant::now() < deadline, "three clicks must seek 30 seconds and paint the total");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // The next item uses the same mpv and services.
        (t, _) = ui.into_state();
    }
}

// ---------------------------------------------------------------------------------------------
// tests

/// Exercise the production option builder against real track selection, not just option
/// strings. A matching English slang used to enable ordinary tracks in Forced mode too.
#[test]
fn subtitle_auto_selection_respects_track_flags() {
    use libmpv2::events::Event;
    use tuner::player::{mpv, tracks::parse_track_list};

    let Some((dir, _serial)) = setup() else { return };
    for mode in [SubtitleAuto::Off, SubtitleAuto::Forced, SubtitleAuto::Always] {
        for (flag, forced, default) in [("none", false, false), ("forced", true, false), ("default", false, true)] {
            let mut cfg = headless_config();
            cfg.player.sub_auto = mode;
            // Hold the short file open at its first frame while inspecting the selection.
            cfg.player.mpv_options.insert("pause".into(), "yes".into());
            let mpv = mpv::create(&cfg, TileId::A).expect("headless mpv");
            let path = dir.join(format!("subtitles_{flag}.mkv"));
            mpv.command("loadfile", &[path.to_str().unwrap()]).unwrap();
            let deadline = Instant::now() + LONG;
            loop {
                assert!(Instant::now() < deadline, "{mode:?}/{flag}: file did not load");
                if let Some(event) = mpv.wait_event(0.1) {
                    match event.expect("mpv event") {
                        Event::FileLoaded => break,
                        Event::EndFile(reason) => panic!("{mode:?}/{flag}: file ended before loading: {reason:?}"),
                        _ => {}
                    }
                }
            }
            let tracks = parse_track_list(&mpv.get_property::<String>("track-list").unwrap());
            assert_eq!(tracks.subs.len(), 1, "{flag}: one subtitle track");
            let sub = &tracks.subs[0];
            assert_eq!(sub.lang.as_deref(), Some("eng"), "{flag}: English matches the configured sub_langs");
            assert_eq!((sub.forced, sub.default), (forced, default), "{flag}: fixture dispositions");
            let selected = mode == SubtitleAuto::Always || (mode == SubtitleAuto::Forced && forced);
            let expected_sid = if selected { sub.id.to_string() } else { "no".into() };
            assert_eq!(mpv.get_property::<String>("sid").unwrap(), expected_sid, "{mode:?}/{flag}");
            assert_eq!(sub.selected, selected, "{mode:?}/{flag}: track-list agrees with sid");

            if mode == SubtitleAuto::Off {
                // Off still allows the commands behind `s` and the `S` track picker.
                mpv.command("cycle", &["sub"]).unwrap();
                assert_eq!(mpv.get_property::<i64>("sid").unwrap(), sub.id);
                mpv.set_property("sid", "no").unwrap();
                mpv.set_property("sid", sub.id).unwrap();
                assert_eq!(mpv.get_property::<i64>("sid").unwrap(), sub.id);
            }
        }
    }
}

#[test]
fn vod_eof_ends_saves_progress_and_history() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let mut m = movie(1, srv.url("testsrc.mp4"));
    m.start_at = Some(7.0); // 10 s file: reach the end quickly
    let mut h = Harness::start(headless_config(), vec![m.clone()]);
    h.play(m.media.clone(), true);
    h.wait_state(LONG, is_playing);
    let playing = Instant::now();
    // the start option reached mpv: playback begins near 7 s, not at 0
    let first = h.wait_snapshot(LONG, |s| s.position.is_some()).position.unwrap();
    assert!(first >= 6.5, "playback started at {first} s");
    let ended = h.wait_event(LONG, |e| matches!(e, PlayerEvent::Ended { .. })).expect("Ended event");
    // ~3 s left to play (playback never runs faster than real time: from 0 it'd take ≥ 9 s)
    assert!(playing.elapsed() < Duration::from_secs(7), "EOF after {:?}", playing.elapsed());
    assert_eq!(ended, PlayerEvent::Ended { item: m.item.clone(), finished: true });
    h.wait_state(LONG, |s| matches!(s, PlayerState::Ended));
    assert!(!h.states().iter().any(|s| matches!(s, PlayerState::Reconnecting { .. } | PlayerState::Error(_))), "{:?}", h.states());

    let progress = h.rec.progress.lock().unwrap().clone();
    let last = progress.last().expect("final progress save at EOF");
    assert_eq!(Some(&last.item), m.item.as_ref());
    assert!(last.position_s > 9.0, "{last:?}");
    assert!(last.finished);
    assert!((last.duration_s.unwrap() - 10.0).abs() < 0.5);
    assert_eq!(h.rec.history.lock().unwrap().len(), 1, "history pushed once");
    assert!(h.rec.failures.lock().unwrap().iter().all(|(_, k)| k.is_none()), "a good play only clears failures");
}

#[test]
fn progress_saved_every_10s_and_on_stop() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let m = movie(2, srv.url("long/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![m.clone()]);
    h.play(m.media.clone(), true);
    h.wait_state(LONG, is_playing);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(25);
    while h.rec.progress.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no periodic progress save");
        std::thread::sleep(Duration::from_millis(100));
    }
    let periodic = h.rec.progress.lock().unwrap()[0].clone();
    assert!(started.elapsed() >= Duration::from_secs(8), "periodic save came too early: {:?}", started.elapsed());
    assert!(periodic.position_s >= 5.0, "{periodic:?}");
    assert!(!periodic.finished);
    assert_eq!(Some(&periodic.item), m.item.as_ref());

    h.handle.send(PlayerCommand::Stop);
    h.wait_state(LONG, |s| matches!(s, PlayerState::Idle));
    let all = h.rec.progress.lock().unwrap().clone();
    assert!(all.len() >= 2, "final save on stop: {all:?}");
    let last = all.last().unwrap();
    assert!(last.position_s >= periodic.position_s, "{all:?}");
    assert!((last.duration_s.unwrap() - 60.0).abs() < 1.0, "{last:?}");
}

#[test]
fn live_hls_server_killed_reconnects_and_recovers() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let ch = live(10, srv.url("live/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    std::thread::sleep(Duration::from_secs(2));

    let mark = h.mark();
    srv.kill();
    let s = h.wait_state_from(mark, LONG, |s| matches!(s, PlayerState::Reconnecting { .. }));
    let PlayerState::Reconnecting { reason, .. } = s else { unreachable!() };
    assert!(
        matches!(reason, PlayerErrorKind::StreamEnded | PlayerErrorKind::Network | PlayerErrorKind::Stalled | PlayerErrorKind::Timeout),
        "{reason:?}"
    );
    // a couple of failed attempts while the server is down, then it comes back
    std::thread::sleep(Duration::from_secs(2));
    assert!(!matches!(h.snapshot().state, PlayerState::Error(_)), "gave up too early: {:?}", h.states());
    let mark = h.mark();
    srv.revive();
    h.wait_state_from(mark, Duration::from_secs(45), is_playing);
    assert!(h.wait_snapshot(LONG, |s| s.position.is_some()).position.is_some());
    assert!(!h.rec.failures.lock().unwrap().iter().any(|(_, k)| k.is_some()), "recovered streams are not recorded as failures");
}

#[test]
fn http_403_and_404_are_classified_from_the_fault_server() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    srv.set_status("/forbidden.ts", 403);
    srv.set_status("/gone.mp4", 404);
    let ch = live(20, srv.url("forbidden.ts"));
    let vod = movie(21, srv.url("gone.mp4"));
    let mut h = Harness::start(headless_config(), vec![ch.clone(), vod.clone()]);

    // live 403: one retry after 3 s (connection-limit race), then Error
    h.play(ch.media.clone(), true);
    let s = h.wait_state(LONG, |s| matches!(s, PlayerState::Reconnecting { .. } | PlayerState::Error(_)));
    assert!(matches!(s, PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, attempt: 1, max: 1, .. }), "{s:?}");
    let s = h.wait_state(LONG, |s| matches!(s, PlayerState::Error(_)));
    assert!(matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. })), "{s:?}");
    assert_eq!(srv.requests_for("/forbidden.ts").len(), 2, "initial attempt + one retry");
    assert!(h.rec.failures.lock().unwrap().iter().any(|(i, k)| Some(i) == ch.item.as_ref() && *k == Some(PlayerErrorKind::HttpForbidden)));
    assert!(matches!(
        h.wait_event(LONG, |e| matches!(e, PlayerEvent::Failed(_))),
        Some(PlayerEvent::Failed(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. }))
    ));

    // VOD 404: no retry
    let mark = h.mark();
    h.play(vod.media.clone(), true);
    let s = h.wait_state_from(mark, LONG, |s| matches!(s, PlayerState::Reconnecting { .. } | PlayerState::Error(_)));
    assert!(matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, .. })), "{s:?}");
    assert!(h.rec.failures.lock().unwrap().iter().any(|(i, k)| Some(i) == vod.item.as_ref() && *k == Some(PlayerErrorKind::HttpNotFound)));
    // the snapshot is published after the event went out (the controller logs in between)
    let snap = h.wait_snapshot(LONG, |s| matches!(s.state, PlayerState::Error(_)));
    assert_eq!(snap.last_error.as_ref().map(|e| e.kind), Some(PlayerErrorKind::HttpNotFound));
}

#[test]
fn per_file_user_agent_referrer_and_headers_with_commas_reach_the_server() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let mut ch = live(30, srv.url("bars.ts"));
    ch.headers = HttpHeaders {
        user_agent: Some("TunerTest/1.0 (X11, Linux), like VLC".into()),
        referrer: Some("http://ref.example/a,b?x=1".into()),
        origin: Some("http://origin.example".into()),
        extra: vec![("X-Token".into(), "a,b,c".into()), ("Cookie".into(), "s=1; t=2".into())],
    };
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    let reqs = srv.requests_for("/bars.ts");
    let r = reqs.first().expect("request");
    assert_eq!(r.header("user-agent"), Some("TunerTest/1.0 (X11, Linux), like VLC"));
    assert_eq!(r.header("referer"), Some("http://ref.example/a,b?x=1"));
    assert_eq!(r.header("origin"), Some("http://origin.example"));
    assert_eq!(r.header("x-token"), Some("a,b,c"));
    assert_eq!(r.header("cookie"), Some("s=1; t=2"));
}

#[test]
fn default_user_agent_applies_without_per_file_headers() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let ch = live(31, srv.url("bars.ts"));
    let mut cfg = headless_config();
    cfg.player.user_agent = "DefaultUA/2.0".into();
    let mut h = Harness::start(cfg, vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    assert_eq!(srv.requests_for("/bars.ts")[0].header("user-agent"), Some("DefaultUA/2.0"));
}

#[test]
fn stop_before_start_waits_for_end_file_with_a_limit_of_one() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    // slow streams keep their connections open while playing
    srv.set_throttle(Some(48 * 1024));
    let a = live(40, srv.url("bars.ts"));
    let b = live(41, srv.url("hls/seg001.ts"));
    let mut h = Harness::start(headless_config(), vec![a.clone(), b.clone()]);
    h.handle.send(PlayerCommand::SetConnectionLimit { source: SourceId(1), max: Some(1) });
    h.play(a.media.clone(), true);
    h.wait_state(LONG, is_playing);
    assert!(srv.active() >= 1, "A is still streaming when we switch");
    controller_log();
    let mark = h.mark();
    h.play(b.media.clone(), true);
    h.wait_state_from(mark, LONG, is_playing);
    // the controller's own ordering (the server-side check below alone can't tell: against a
    // local server mpv usually closes A before opening B even on a plain replace)
    assert_stopped_before_start(&controller_log());

    let a_reqs = srv.requests_for("/bars.ts");
    let b_reqs = srv.requests_for("/hls/seg001.ts");
    let a_closed = a_reqs.iter().map(|r| r.closed.expect("A closed before B started")).max().unwrap();
    for r in &b_reqs {
        assert!(r.opened >= a_closed, "B connected {:?} before A closed", a_closed - r.opened);
    }
    // (one stream alone may briefly hold two connections: ffmpeg opens the Range request of a
    // seek before closing the old one, so `max_concurrent` is not the right measure here)
}

/// Review fix: a playlist URL (`.m3u` listing the real stream) ends its entry with a redirect;
/// the lease must follow the expanded entry so the next same-source switch still stops first.
#[test]
fn lease_follows_a_playlist_redirect() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    srv.set_throttle(Some(48 * 1024));
    let lists = tempfile::tempdir().expect("tempdir");
    std::fs::write(lists.path().join("ch.m3u"), format!("#EXTM3U\n#EXTINF:-1,Bars\n{}\n", srv.url("bars.ts"))).unwrap();
    let list_srv = TestServer::start(lists.path().to_path_buf(), Faults::default());
    let a = live(42, list_srv.url("ch.m3u"));
    let b = live(43, srv.url("hls/seg001.ts"));
    let mut h = Harness::start(headless_config(), vec![a.clone(), b.clone()]);
    h.handle.send(PlayerCommand::SetConnectionLimit { source: SourceId(1), max: Some(1) });
    h.play(a.media.clone(), true);
    h.wait_state(LONG, is_playing);
    assert!(!srv.requests_for("/bars.ts").is_empty(), "the playlist expanded into the stream");
    let log = controller_log();
    assert!(log.iter().any(|l| l.starts_with("END_FILE") && l.contains("Redirect")), "{log:#?}");
    let mark = h.mark();
    h.play(b.media.clone(), true);
    h.wait_state_from(mark, LONG, is_playing);
    assert_stopped_before_start(&controller_log());
}

#[test]
fn fast_zaps_coalesce_into_one_connection() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let paths = ["long/index.m3u8", "bars.ts", "hls/index.m3u8", "testsrc.mp4"];
    let chans: Vec<Playable> = paths.iter().zip(50..).map(|(p, id)| live(id, srv.url(p))).collect();
    let mut h = Harness::start(headless_config(), chans.clone());
    let ids = chans
        .iter()
        .map(|c| match c.media {
            MediaRef::Channel(id) => id,
            _ => unreachable!(),
        })
        .collect();
    let zap = ZapList { collection: Collection::AllChannels, channels: ids };
    let req = PlayRequest { media: chans[0].media.clone(), zap: Some(zap), origin: PlayOrigin::Fullscreen, resume: false, immediate: true };
    h.handle.send(PlayerCommand::Play(req));
    h.wait_state(LONG, is_playing);
    let mark = h.mark();
    for _ in 0..2 {
        h.handle.send(PlayerCommand::Zap { forward: true, unlocked: true });
    }
    let s = h.wait_snapshot(LONG, |s| s.playable.as_ref().is_some_and(|p| p.media == chans[2].media));
    assert!(matches!(s.state, PlayerState::Zapping { .. }), "zaps wait out the delay: {:?}", s.state);
    assert_eq!(s.origin, Some(PlayOrigin::Fullscreen), "zaps keep the origin");
    h.wait_state_from(mark, LONG, is_playing);
    assert!(srv.requests_for("/bars.ts").is_empty(), "the zapped-over channel never connected");
    assert!(!srv.requests_for("/hls/index.m3u8").is_empty());
    // Backspace recalls the channel that really played before, not the one zapped over
    assert_eq!(h.snapshot().previous_live.as_ref().map(|p| p.media.clone()), Some(chans[0].media.clone()));
    let mark = h.mark();
    h.handle.send(PlayerCommand::LastChannel { unlocked: true });
    h.wait_snapshot(LONG, |s| s.playable.as_ref().is_some_and(|p| p.media == chans[0].media));
    h.wait_state_from(mark, LONG, is_playing);
    assert_eq!(h.snapshot().previous_live.as_ref().map(|p| p.media.clone()), Some(chans[2].media.clone()));
    // the measured size (quality badge) goes to the channels that played, never to the one
    // zapped over or to the next channel while the old picture is still up
    h.wait_snapshot(LONG, |s| s.video_size.is_some());
    std::thread::sleep(Duration::from_millis(300));
    let video = h.rec.video.lock().unwrap().clone();
    let sizes = |c: &Playable| video.iter().filter(|(i, ..)| Some(i) == c.item.as_ref()).map(|(_, w, h)| (*w, *h)).collect::<Vec<_>>();
    let first = sizes(&chans[0]);
    assert!(!first.is_empty() && first.iter().all(|s| *s == (320, 180)), "{video:?}");
    assert!(sizes(&chans[1]).is_empty(), "{video:?}");
    assert_eq!(sizes(&chans[2]), [(640, 360)], "{video:?}");
}

#[test]
fn resolve_failure_keeps_the_current_stream_playing() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let ch = live(60, srv.url("hls/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    let mark = h.mark();
    h.play(MediaRef::Channel(ChannelId(999)), true);
    let failed = h.wait_event_from(mark, LONG, |e| matches!(e, PlayerEvent::Failed(_))).expect("Failed event");
    assert!(matches!(failed, PlayerEvent::Failed(PlayerError { kind: PlayerErrorKind::Resolve, .. })));
    std::thread::sleep(Duration::from_millis(500));
    let s = h.snapshot();
    assert!(matches!(s.state, PlayerState::Playing | PlayerState::Buffering { .. }), "{:?}", s.state);
    assert_eq!(s.playable.as_ref().map(|p| p.media.clone()), Some(ch.media.clone()));
    assert!(h.rec.failures.lock().unwrap().iter().all(|(_, k)| k.is_none()), "nothing recorded for a resolve failure");
}

#[test]
fn segment_404s_while_playing_are_not_channel_offline() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults { segment_404_every: Some(3), ..Default::default() });
    let ch = live(70, srv.url("long/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    // segments 3, 6, 9… answer 404 while we play
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        h.drain();
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(srv.requests().iter().any(|r| r.status == 404), "faults were injected");
    let bad = h.states().into_iter().find(|s| {
        matches!(
            s,
            PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, .. })
                | PlayerState::Reconnecting { reason: PlayerErrorKind::HttpNotFound, .. }
        )
    });
    assert_eq!(bad, None, "states: {:?}", h.states());
}

#[test]
fn connection_cut_mid_file_is_resumed_by_mpv() {
    let Some((media, _serial)) = setup() else { return };
    // MPEG-TS is read front to back: the only way past the cut is to reconnect (stream-lavf-o
    // reconnect*, DESIGN §11.4 — TCP drops inside a file are mpv's job, not the policy's)
    let srv = TestServer::start(media, Faults { drop_after_bytes: Some(60_000), ..Default::default() });
    let m = movie(80, srv.url("bars.ts"));
    let mut h = Harness::start(headless_config(), vec![m.clone()]);
    h.play(m.media.clone(), true);
    h.wait_state(LONG, |s| matches!(s, PlayerState::Ended | PlayerState::Error(_)));
    assert!(h.states().contains(&PlayerState::Ended), "{:?}", h.states());
    assert!(!h.states().iter().any(|s| matches!(s, PlayerState::Reconnecting { .. })), "{:?}", h.states());
    let reqs = srv.requests_for("/bars.ts");
    assert!(reqs.len() >= 2, "the cut connection was reopened: {reqs:?}");
    let resumed_at = reqs[1].header("range").and_then(|r| r.strip_prefix("bytes=")?.split('-').next()?.parse::<u64>().ok());
    assert!(resumed_at.is_some_and(|b| b > 0), "reconnect resumes with a Range request: {:?}", reqs[1].headers);
}

#[test]
fn apply_config_changes_the_zap_delay() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let ch = live(90, srv.url("hls/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    let mut cfg = headless_config();
    cfg.player.zap_delay_ms = 1500;
    cfg.player.cache_secs = 4;
    h.handle.send(PlayerCommand::ApplyConfig(Box::new(cfg)));
    let sent = Instant::now();
    h.play(ch.media.clone(), false);
    h.wait_state(LONG, |s| matches!(s, PlayerState::Zapping { .. }));
    h.wait_state(LONG, |s| matches!(s, PlayerState::Loading { .. }));
    assert!(sent.elapsed() >= Duration::from_millis(1400), "zap delay applied: {:?}", sent.elapsed());
    h.wait_state(LONG, is_playing);
}

// ---------------------------------------------------------------------------------------------
// field report 2026-10-11: live streams that dropped out and eventually stopped

/// `StateChanged`s at index ≥ `from` of the history.
fn states_from(h: &mut Harness, from: usize) -> Vec<PlayerState> {
    h.drain();
    h.seen[from..].iter().filter_map(|e| if let PlayerEvent::StateChanged(s) = e { Some(s.clone()) } else { None }).collect()
}

/// The provider's restreamer error that its live playlist address answers during an outage.
const PROVIDER_TEXT: &str = "Cannot read /home/nxt/storage/streams/1_.m3u8";

/// Requests for the "address" mpv makes of the provider's text (`…/live/Cannot read …`).
fn bogus_requests(srv: &TestServer) -> usize {
    srv.requests().iter().filter(|r| r.path.contains("Cannot")).count()
}

/// Keep-alive: providers close idle keep-alive connections between segment fetches ("Error
/// reading HTTP response: End of file" every 10 s in the field log) or leave them half-dead
/// (a request that is never answered: a network-timeout stall, segments skipped). HLS opens a
/// fresh connection for every request by default (`demuxer-lavf-o=http_persistent=0`), so
/// neither mode buffers or reconnects, and no request ever rides a reused connection.
#[test]
fn hls_keepalive_idle_close_and_abandoned_connections_do_not_stall() {
    let Some((media, _serial)) = setup() else { return };
    for keep in [KeepAlive::IdleClose(Duration::from_millis(500)), KeepAlive::Abandon] {
        let srv = TestServer::start(media.clone(), Faults::default());
        srv.set_keep_alive(keep);
        let ch = live(100, srv.url("live/index.m3u8"));
        let mut h = Harness::start(headless_config(), vec![ch.clone()]);
        h.play(ch.media.clone(), true);
        h.wait_state(LONG, is_playing);
        let first = h.wait_snapshot(LONG, |s| s.position.is_some()).position.unwrap();
        let mark = h.mark();
        std::thread::sleep(Duration::from_secs(12));
        let states = states_from(&mut h, mark);
        assert!(states.is_empty(), "{keep:?}: {states:?}");
        let pos = h.snapshot().position.unwrap_or(0.0);
        assert!(pos - first >= 10.0, "{keep:?}: played {:.1} s in 12 s", pos - first);
        let reused: Vec<_> = srv.requests().into_iter().filter(|r| r.nth > 1).map(|r| r.path).collect();
        assert!(reused.is_empty(), "{keep:?}: requests on reused connections: {reused:?}");
    }
}

/// A provider outage while a live channel plays: its playlist address answers the
/// restreamer's text instead of a playlist and the segments are gone, for longer than any
/// first-play budget. The channel that played keeps reconnecting ("attempt n", no maximum),
/// recognises the provider's message (and doesn't chase the "address" made of it more than
/// once per attempt), never reaches Error, isn't recorded as failing, and plays again when the
/// provider is back.
#[test]
fn a_played_channel_keeps_reconnecting_through_a_provider_outage_and_recovers() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let ch = live(101, srv.url("live/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    h.wait_state(LONG, is_playing);
    std::thread::sleep(Duration::from_secs(2));

    let mark = h.mark();
    srv.set_body("/live/index.m3u8", Some(PROVIDER_TEXT));
    srv.set_status("/live/seg*", 404);
    // the first-play budget of a provider message is 3 retries (~10 s): wait well past it
    h.wait_state_from(mark, LONG, |s| matches!(s, PlayerState::Reconnecting { reason: PlayerErrorKind::ProviderError, max: 0, .. }));
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        assert!(!matches!(h.snapshot().state, PlayerState::Error(_)), "gave up: {:?}", states_from(&mut h, mark));
        std::thread::sleep(Duration::from_millis(200));
    }
    let attempts = states_from(&mut h, mark).iter().filter(|s| matches!(s, PlayerState::Reconnecting { .. })).count();
    assert!(attempts >= 4, "kept trying past the first-play budget: {:?}", states_from(&mut h, mark));
    assert!(bogus_requests(&srv) <= attempts, "{} requests of the text as an address in {attempts} attempts", bogus_requests(&srv));

    let back = h.mark();
    srv.set_body("/live/index.m3u8", None);
    srv.set_status("/live/seg*", 0);
    h.wait_state_from(back, Duration::from_secs(45), is_playing);
    let states = states_from(&mut h, mark);
    assert!(!states.iter().any(|s| matches!(s, PlayerState::Error(_))), "{states:?}");
    assert!(states.iter().all(|s| !matches!(s, PlayerState::Reconnecting { max, .. } if *max != 0)), "no attempt limit: {states:?}");
    assert!(h.rec.failures.lock().unwrap().iter().all(|(_, k)| k.is_none()), "a channel that played is never recorded as failing");
    // the log tells the story (INFO transitions, the provider's words, the recovery), without
    // stream addresses
    // (the controller logs right after it announced the state: give it a moment)
    let mut log = controller_log();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !log.iter().any(|l| l.starts_with("player: recovered after ")) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        log.extend(controller_log());
    }
    let key = &ch.item.as_ref().unwrap().key.0;
    assert!(log.iter().any(|l| l.contains(&format!("source 1 {key} Playing → Reconnecting(attempt 1, "))), "{log:#?}");
    // titles stay out of INFO lines (explicit titles must never reach the log file)
    let info = std::mem::take(&mut *INFO_LOG.lock().unwrap_or_else(|e| e.into_inner()));
    assert!(!info.iter().any(|l| l.contains("Test stream")), "{info:#?}");
    assert!(info.iter().any(|l| l.starts_with("player: recovered after ")), "{info:#?}");
    assert!(log.iter().any(|l| l.starts_with("player: the provider answered with a message") && l.contains(PROVIDER_TEXT)), "{log:#?}");
    assert!(log.iter().any(|l| l.starts_with("player: recovered after ")), "{log:#?}");
    assert!(!log.iter().any(|l| l.contains("http://")), "no stream addresses: {log:#?}");
}

/// First play while the provider answers with its message: three retries, then Error with the
/// friendly headline's kind and the provider's own words, recorded as a provider error.
#[test]
fn a_provider_message_on_first_play_ends_in_provider_error_with_its_text() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    srv.set_body("/live/index.m3u8", Some(PROVIDER_TEXT));
    let ch = live(102, srv.url("live/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![ch.clone()]);
    h.play(ch.media.clone(), true);
    let s = h.wait_state(LONG, |s| matches!(s, PlayerState::Error(_)));
    let PlayerState::Error(err) = s else { unreachable!() };
    assert_eq!(err.kind, PlayerErrorKind::ProviderError, "{err:?}");
    assert!(err.message.contains(PROVIDER_TEXT), "{err:?}");
    let reconnects: Vec<_> = h.states().into_iter().filter(|s| matches!(s, PlayerState::Reconnecting { .. })).collect();
    assert_eq!(reconnects.len(), 3, "{reconnects:?}");
    let fetches = srv.requests_for("/live/index.m3u8").len();
    assert!((4..=8).contains(&fetches), "one fetch (or mpv's probe pair) per attempt: {fetches}");
    assert!(bogus_requests(&srv) <= 4, "{} requests of the text as an address", bogus_requests(&srv));
    assert!(h.rec.failures.lock().unwrap().iter().any(|(i, k)| Some(i) == ch.item.as_ref() && *k == Some(PlayerErrorKind::ProviderError)));
}

/// The zap race window on a one-connection account: the provider keeps counting the stream
/// we zapped away from for a while (here 8 s), so the new channel is refused (403) at first:
/// retried every 3 s, it plays once the old session expires — no Error, nothing recorded.
/// A channel the provider refuses for good still reaches Error once the window (45 s after the
/// close) ends, and is recorded.
#[test]
fn a_zap_inside_the_connection_limit_race_window_recovers_and_a_refused_one_errors() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    srv.set_session_limit(Some(SessionLimit { max: 1, linger: Duration::from_secs(8) }));
    let a = live(103, srv.url("live/index.m3u8"));
    let b = live(104, srv.url("long/index.m3u8"));
    let mut h = Harness::start(headless_config(), vec![a.clone(), b.clone()]);
    h.play(a.media.clone(), true);
    h.wait_state(LONG, is_playing);
    std::thread::sleep(Duration::from_secs(2));

    let mark = h.mark();
    h.play(b.media.clone(), true);
    h.wait_state_from(mark, LONG, |s| matches!(s, PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, max: 0, .. }));
    h.wait_state_from(mark, LONG, is_playing);
    let states = states_from(&mut h, mark);
    assert!(!states.iter().any(|s| matches!(s, PlayerState::Error(_))), "{states:?}");
    assert!(srv.requests_for("/long/index.m3u8").iter().filter(|r| r.status == 403).count() >= 2, "refused while the old session lingered");
    assert!(h.rec.failures.lock().unwrap().iter().all(|(_, k)| k.is_none()), "{:?}", h.rec.failures.lock().unwrap());

    // back to A, which the provider now refuses for good
    srv.set_status("/live/index.m3u8", 403);
    std::thread::sleep(Duration::from_secs(2));
    let mark = h.mark();
    let zapped = Instant::now();
    h.play(a.media.clone(), true);
    let s = h.wait_state_from(mark, Duration::from_secs(75), |s| matches!(s, PlayerState::Error(_)));
    assert!(matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. })), "{s:?}");
    assert!(zapped.elapsed() >= Duration::from_secs(44), "gave up inside the window: {:?}", zapped.elapsed());
    let tries = srv.requests_for("/live/index.m3u8").iter().filter(|r| r.status == 403).count();
    assert!(tries >= 10, "retried every 3 s through the window: {tries}");
    assert!(h.rec.failures.lock().unwrap().iter().any(|(i, k)| Some(i) == a.item.as_ref() && *k == Some(PlayerErrorKind::HttpForbidden)));
}

// ---------------------------------------------------------------------------------------------
// the fault-injecting test server itself (no mpv)

/// Plain HTTP/1.1 GET with std sockets: (status, headers lower-cased, body bytes read before
/// the connection ended, whether it ended cleanly).
fn http_get(base: &str, path: &str, extra: &str) -> (u16, Vec<(String, String)>, Vec<u8>, bool) {
    use std::io::{Read, Write};
    let addr = base.strip_prefix("http://").expect("http base");
    let mut s = std::net::TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n").unwrap();
    let mut buf = Vec::new();
    let clean = s.read_to_end(&mut buf).is_ok();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    (status, headers, buf[split + 4..].to_vec(), clean)
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn serve_returns_a_loopback_url_with_injected_faults() {
    let Some(media) = ensure_media() else { return };
    let base = support::media::serve(media.clone(), Faults { drop_after_bytes: Some(1000), segment_404_every: Some(2) }).await;
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");
    let full = std::fs::read(media.join("testsrc.mp4")).unwrap();
    let base2 = base.clone();
    let (first, second, segs) = tokio::task::spawn_blocking(move || {
        let first = http_get(&base2, "/testsrc.mp4", "");
        let second = http_get(&base2, "/testsrc.mp4", "");
        let segs: Vec<u16> = (0..4).map(|i| http_get(&base2, &format!("/hls/seg{i:03}.ts"), "").0).collect();
        (first, second, segs)
    })
    .await
    .unwrap();
    // first response cut after 1000 body bytes, the reconnect is served in full
    assert_eq!(first.0, 200);
    assert_eq!(first.2.len(), 1000);
    assert_eq!(second.2, full);
    assert_eq!(segs, [200, 404, 200, 404], "every 2nd segment is a 404");
}

#[test]
fn test_server_ranges_statuses_kill_revive_and_live_playlist() {
    let Some(media) = ensure_media() else { return };
    let srv = TestServer::start(media.clone(), Faults::default());
    let full = std::fs::read(media.join("bars.ts")).unwrap();

    let (status, h, body, clean) = http_get(&srv.base, "/bars.ts", "Range: bytes=100-199\r\nX-Probe: a,b\r\n");
    assert_eq!((status, clean), (206, true));
    assert_eq!(body, full[100..200]);
    assert_eq!(header(&h, "content-range"), Some(format!("bytes 100-199/{}", full.len()).as_str()));
    let r = &srv.requests_for("/bars.ts")[0];
    assert_eq!(r.header("X-Probe"), Some("a,b"), "request headers are recorded verbatim");
    assert_eq!(r.status, 206);
    // `closed` is stamped once the server sees our hang-up (asynchronously)
    let deadline = Instant::now() + Duration::from_secs(5);
    while srv.requests_for("/bars.ts")[0].closed.is_none() {
        assert!(Instant::now() < deadline, "close time never recorded");
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(http_get(&srv.base, "/missing.ts", "").0, 404);
    assert_eq!(http_get(&srv.base, "/../Cargo.toml", "").0, 404, "no path traversal");
    srv.set_status("/bars.ts", 403);
    assert_eq!(http_get(&srv.base, "/bars.ts", "").0, 403);
    srv.set_status("/bars.ts", 0);
    assert_eq!(http_get(&srv.base, "/bars.ts", "").0, 200);

    // live playlist: sliding window over the long/ segments, never ENDLIST
    let (status, h, body, _) = http_get(&srv.base, "/live/index.m3u8", "");
    assert_eq!(status, 200);
    assert_eq!(header(&h, "content-type"), Some("application/vnd.apple.mpegurl"));
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:") && !text.contains("ENDLIST"), "{text}");
    assert_eq!(text.matches("#EXTINF").count(), 4, "{text}");
    let seg = text.lines().find(|l| l.ends_with(".ts")).unwrap();
    assert_eq!(http_get(&srv.base, &format!("/live/{seg}"), "").0, 200);

    // kill: connection refused; revive: same port serves again
    srv.kill();
    let addr = srv.base.strip_prefix("http://").unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(&addr).is_ok() {
        assert!(Instant::now() < deadline, "listener still accepting after kill");
        std::thread::sleep(Duration::from_millis(20));
    }
    srv.revive();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline, "listener not back after revive");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(http_get(&srv.base, "/bars.ts", "").0, 200);
    assert!(srv.max_concurrent() >= 1);
}

/// The field-report modes of the test server: keep-alive (two answers on one connection, then
/// a silent close when idle, or an abandoned second request), a text body where a playlist
/// should be, `prefix*` statuses, and a connection limit whose sessions linger.
#[test]
fn test_server_keep_alive_text_bodies_and_lingering_sessions() {
    use std::io::{Read, Write};
    let Some(media) = ensure_media() else { return };
    let srv = TestServer::start(media, Faults::default());
    let addr = srv.base.strip_prefix("http://").unwrap().to_owned();
    // one response off a persistent connection: (status, body)
    let read_one = |s: &mut std::net::TcpStream| -> Option<(u16, String)> {
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).ok()? == 0 {
                return None;
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let status = head.split_whitespace().nth(1)?.parse().ok()?;
        let len: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: "))?.trim().parse().ok()?;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).ok()?;
        Some((status, String::from_utf8_lossy(&body).into_owned()))
    };
    // a write to a connection the server closed may fail: the read below tells
    let get = |s: &mut std::net::TcpStream, path: &str| {
        let _ = write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    };

    srv.set_keep_alive(KeepAlive::IdleClose(Duration::from_millis(300)));
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    get(&mut s, "/live/index.m3u8");
    assert_eq!(read_one(&mut s).map(|r| r.0), Some(200));
    get(&mut s, "/live/index.m3u8");
    assert_eq!(read_one(&mut s).map(|r| r.0), Some(200), "the same connection answers again");
    std::thread::sleep(Duration::from_millis(600));
    get(&mut s, "/live/index.m3u8");
    assert_eq!(read_one(&mut s), None, "closed while idle");
    let reqs = srv.requests_for("/live/index.m3u8");
    assert_eq!(reqs.iter().map(|r| (r.conn, r.nth)).collect::<Vec<_>>(), [(1, 1), (1, 2)]);

    srv.set_keep_alive(KeepAlive::Abandon);
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    get(&mut s, "/bars.ts");
    assert_eq!(read_one(&mut s).map(|r| r.0), Some(200));
    get(&mut s, "/bars.ts");
    assert_eq!(read_one(&mut s), None, "the reused request is never answered");
    drop(s);
    srv.set_keep_alive(KeepAlive::Off);

    srv.set_body("/live/index.m3u8", Some("Cannot read /home/nxt/storage/streams/1_.m3u8"));
    srv.set_status("/live/seg*", 404);
    let (status, h, body, _) = http_get(&srv.base, "/live/index.m3u8", "");
    assert_eq!((status, String::from_utf8(body).unwrap()), (200, "Cannot read /home/nxt/storage/streams/1_.m3u8".to_owned()));
    assert_eq!(header(&h, "content-type"), Some("application/vnd.apple.mpegurl"));
    assert_eq!(http_get(&srv.base, "/live/seg001.ts", "").0, 404);
    srv.set_body("/live/index.m3u8", None);
    srv.set_status("/live/seg*", 0);
    assert_eq!(http_get(&srv.base, "/live/seg001.ts", "").0, 200);
    assert!(http_get(&srv.base, "/live/index.m3u8", "").2.starts_with(b"#EXTM3U"));

    // one stream at a time; a closed stream counts for `linger`
    srv.set_session_limit(Some(SessionLimit { max: 1, linger: Duration::from_millis(800) }));
    std::thread::sleep(Duration::from_millis(900)); // the requests above
    assert_eq!(http_get(&srv.base, "/live/index.m3u8", "").0, 200);
    assert_eq!(http_get(&srv.base, "/live/seg002.ts", "").0, 200, "the same stream");
    assert_eq!(http_get(&srv.base, "/long/index.m3u8", "").0, 403, "another stream while /live lingers");
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(http_get(&srv.base, "/long/index.m3u8", "").0, 200, "after the linger");
}

// ---------------------------------------------------------------------------------------------
// casting (cast-design §14.5): picture A's remote output against a scripted session

mod casting {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;
    use tuner::model::{CastErrorKind, CastHints, CastPhase, CastView, DeviceId, ReceiverKind, TileId};
    use tuner::player::remote::{CastConnector, RemoteCmd, RemoteEvent, RemoteOutput};

    /// A session's far ends: what the controller sends, and the events the test answers with
    /// (the test plays the cast service, its relay and the TV).
    struct FakeTv {
        cmds: tokio::sync::mpsc::UnboundedReceiver<RemoteCmd>,
        events: crossbeam_channel::Sender<RemoteEvent>,
    }

    impl FakeTv {
        fn next(&mut self) -> RemoteCmd {
            let deadline = Instant::now() + LONG;
            loop {
                match self.cmds.try_recv() {
                    Ok(c) => return c,
                    Err(TryRecvError::Empty) => assert!(Instant::now() < deadline, "no command for the TV within {LONG:?}"),
                    Err(TryRecvError::Disconnected) => panic!("the controller dropped the session"),
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn load(&mut self) -> (u64, Arc<Playable>, CastHints) {
            match self.next() {
                RemoteCmd::Load { generation, playable, hints } => (generation, playable, hints),
                other => panic!("expected a load, got {other:?}"),
            }
        }

        fn send(&self, ev: RemoteEvent) {
            self.events.send(ev).expect("the controller listens");
        }

        /// The relay's upstream opens and the TV shows its first picture.
        fn start(&self, generation: u64) {
            self.send(RemoteEvent::UpstreamOpened { generation });
            self.send(RemoteEvent::Phase(CastPhase::Playing));
            self.send(RemoteEvent::Playing { generation });
        }
    }

    /// Every `open` hands the new session's far ends to the test.
    struct FakeConnector {
        opened: crossbeam_channel::Sender<FakeTv>,
        can_seek: bool,
    }

    impl CastConnector for FakeConnector {
        fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError> {
            let (cmd, cmds) = tokio::sync::mpsc::unbounded_channel();
            let (events, rx) = crossbeam_channel::unbounded();
            self.opened.send(FakeTv { cmds, events }).expect("the test listens");
            let view = CastView {
                device,
                name: "Living Room".into(),
                kind: ReceiverKind::AppleTv,
                phase: CastPhase::Connecting,
                level: None,
                as_is: false,
                behind_live_s: None,
                can_seek: self.can_seek,
                volume: false,
            };
            Ok(RemoteOutput { cmd, events: rx, view })
        }
    }

    fn device() -> DeviceId {
        DeviceId::derive("airplay", "living-room")
    }

    fn start_casting(table: Vec<Playable>) -> (Harness, Receiver<FakeTv>) {
        let (opened, sessions) = crossbeam_channel::unbounded();
        (start_with(table, Arc::new(FakeConnector { opened, can_seek: true })), sessions)
    }

    fn start_with(table: Vec<Playable>, connector: Arc<dyn CastConnector>) -> Harness {
        // the test answers `Close` itself (polling): leaving the TV waits for its upstream close
        tuner::player::controller::set_cast_close_wait_for_tests(LONG);
        let (bus, events) = EventBus::new(None);
        let rec = Recorded::default();
        let deps = PlayerDeps {
            cache_dir: None,
            bus,
            store: Box::new(RecordingStore(rec.clone())),
            resolver: Box::new(TableResolver(table.into_iter().map(|p| (p.media.clone(), p)).collect())),
            cast: Some(connector),
        };
        let player = Player::start(&headless_config(), deps).expect("headless mpv");
        let handle = player.handle();
        Harness { player, handle, events, seen: Vec::new(), rec }
    }

    /// A TV whose session can't open (forgotten, casting switched off meanwhile).
    struct NoTv;

    impl CastConnector for NoTv {
        fn open(&self, _device: DeviceId) -> Result<RemoteOutput, PlayerError> {
            Err(PlayerError { kind: PlayerErrorKind::Cast(CastErrorKind::PairingRequired), message: String::new() })
        }
    }

    fn cast(h: &Harness, media: Option<MediaRef>) {
        let play = media.map(|media| PlayRequest { media, zap: None, origin: PlayOrigin::Fullscreen, resume: false, immediate: true });
        h.handle.send(PlayerCommand::CastTo { device: device(), play });
    }

    /// Streams picture A holds open on source 1 (0, 1, or 2 for "more"): probes the shared
    /// budget at limits 1 and 2 and gives the probe's lease back. Only while nothing loads.
    fn a_leases(p: &Player) -> u32 {
        let b = p.budget();
        let full = |limit| {
            b.set_limit(SourceId(1), Some(limit));
            match b.acquire(TileId::A, SourceId(1)) {
                Ok(l) => {
                    b.release(TileId::A, l, None);
                    false
                }
                Err(_) => true,
            }
        };
        let n = if !full(1) {
            0
        } else if !full(2) {
            1
        } else {
            2
        };
        b.set_limit(SourceId(1), Some(1));
        n
    }

    fn casting_and_playing(s: &PlayerSnapshot) -> bool {
        s.state == PlayerState::Playing && s.cast.as_ref().is_some_and(|c| c.phase == CastPhase::Playing)
    }

    #[test]
    fn vod_skip_unseekable_tv_reports_why() {
        let Some((_media, _serial)) = setup() else { return };
        let m = movie(1, "http://127.0.0.1:9/never-fetched.mp4".into());
        let (opened, sessions) = crossbeam_channel::unbounded();
        let mut h = start_with(vec![m.clone()], Arc::new(FakeConnector { opened, can_seek: false }));
        cast(&h, Some(m.media));
        let mut tv = sessions.recv_timeout(LONG).unwrap();
        let (generation, ..) = tv.load();
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        assert!(!h.snapshot().seekable);
        for cmd in [PlayerCommand::SeekBy(10.0), PlayerCommand::SeekTo(10.0)] {
            let mark = h.mark();
            h.handle.send(cmd);
            assert!(h.wait_event_from(mark, Duration::from_secs(3), |e| matches!(e, PlayerEvent::SeekUnavailable)).is_some());
            assert!(matches!(tv.cmds.try_recv(), Err(TryRecvError::Empty)), "no seek sent to a TV that cannot seek");
        }
    }

    #[test]
    fn vod_skip_after_play_here() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        skips_after_play_here(srv.url("vod.mkv"));
    }

    #[test]
    fn vod_skip_local_after_play_here() {
        let Some((media, _serial)) = setup() else { return };
        skips_after_play_here(media.join("vod.mkv").to_string_lossy().into_owned());
    }

    fn skips_after_play_here(url: String) {
        let m = movie(1, url);
        let (h, sessions) = start_casting(vec![m.clone()]);
        h.play(m.media, true);
        assert_small_skips(&h);
        cast(&h, None);
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        tv.send(RemoteEvent::Position { generation, pos: 5.0, dur: Some(30.0) });
        h.wait_snapshot(LONG, |s| casting_and_playing(s) && s.position == Some(5.0));
        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        assert!(matches!(tv.next(), RemoteCmd::Close { .. }));
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        h.wait_snapshot(LONG, |s| s.cast.is_none() && s.state == PlayerState::Playing && s.position.is_some());
        assert_small_skips(&h);
    }

    #[test]
    fn lip_sync_reapplies_when_playing_here() {
        let ch = live(1, "av://lavfi:testsrc2=size=160x90:rate=25".into());
        let (h, sessions) = start_casting(vec![ch.clone()]);
        let mut cfg = headless_config();
        cfg.player.mpv_options.insert("audio-device".into(), "null/speakers".into());
        cfg.player.lip_sync.insert("null/speakers".into(), tuner::config::LipSync { offset_ms: -150, label: "Speakers".into() });
        h.handle.send(PlayerCommand::ApplyConfig(Box::new(cfg.clone())));
        h.play(ch.media.clone(), true);
        h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && s.lip_sync_ms == -150);

        cast(&h, None);
        let mut tv = sessions.recv_timeout(LONG).unwrap();
        let (generation, _, _) = tv.load();
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        cfg.player.lip_sync.get_mut("null/speakers").unwrap().offset_ms = -350;
        h.handle.send(PlayerCommand::ApplyConfig(Box::new(cfg)));
        // A snapshot-visible command after ApplyConfig proves the edit has been processed.
        h.handle.send(PlayerCommand::SetPlayOn(Some(device())));
        h.wait_snapshot(LONG, |s| s.play_on == Some(device()));

        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        assert!(matches!(tv.next(), RemoteCmd::Close { .. }));
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        h.wait_snapshot(LONG, |s| s.cast.is_none() && s.state == PlayerState::Playing && s.lip_sync_ms == -350);
    }

    /// `CastTo` with what plays: mpv's hints travel with the first load, mpv's stream closes
    /// before the TV's load goes out (one-connection account), the relay's upstream is A's
    /// only lease, and a zap is a new load with a new generation.
    #[test]
    fn casting_moves_the_channel_after_the_local_stream_closed_and_zaps_reload() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        srv.set_throttle(Some(48 * 1024));
        let ch = live(70, srv.url("bars.ts"));
        let other = live(71, srv.url("hls/seg001.ts"));
        let (mut h, sessions) = start_casting(vec![ch.clone(), other.clone()]);
        h.handle.send(PlayerCommand::SetConnectionLimit { source: SourceId(1), max: Some(1) });
        h.play(ch.media.clone(), true);
        h.wait_state(LONG, is_playing);
        controller_log();

        cast(&h, None);
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, playable, hints) = tv.load();
        assert_eq!((&playable.media, playable.start_at), (&ch.media, None), "live comes in at the live point");
        let read = (hints.vcodec.as_deref(), hints.acodec.as_deref(), hints.channels, hints.height, hints.interlaced);
        assert_eq!(read, (Some("h264"), Some("aac"), Some(1), Some(720), Some(false)), "mpv's view of bars.ts");
        let log = controller_log();
        let (waited, entry) = log
            .iter()
            .enumerate()
            .find_map(|(i, l)| Some((i, l.strip_prefix("cast: waiting for the local entry ")?.split('\'').next()?.parse::<i64>().ok()?)))
            .unwrap_or_else(|| panic!("the TV's load didn't wait for mpv: {log:#?}"));
        let ended = end_file_at(&log, entry).unwrap_or_else(|| panic!("END_FILE of {entry} never processed: {log:#?}"));
        let loaded = log.iter().position(|l| *l == format!("cast load: generation {generation}")).expect("the load");
        assert!(waited < ended && ended < loaded, "wait {waited}, END_FILE {ended}, load {loaded}: {log:#?}");

        tv.start(generation);
        let s = h.wait_snapshot(LONG, casting_and_playing);
        assert_eq!(s.cast.as_ref().map(|c| c.device), Some(device()));
        assert!(s.video_size.is_none() && s.hwdec.is_none() && s.tracks == Default::default(), "mpv-only fields stay empty");
        assert_eq!(a_leases(&h.player), 1, "the relay's upstream is A's only lease");

        let mark = h.mark();
        h.play(other.media.clone(), true);
        let (zapped, p, hints) = tv.load();
        assert!(zapped > generation);
        assert_eq!(p.media, other.media);
        assert_eq!(hints, CastHints::default(), "mpv's hints were for the item that moved");
        // the old generation's late close doesn't touch the new lease
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        tv.start(zapped);
        h.wait_state_from(mark, LONG, is_playing);
        assert_eq!(a_leases(&h.player), 1);
    }

    /// A TV that was buffering when the channel changed doesn't carry its cache pause into the
    /// next one: the new item's first picture is Playing (the session reports no
    /// `Buffering(false)` for it), not a Buffering that the stall clock would reload every 20 s.
    #[test]
    fn a_zap_while_the_tv_buffers_plays_the_next_channel() {
        let Some((_media, _serial)) = setup() else { return };
        let ch = live(72, "http://127.0.0.1:9/never-fetched.ts".into());
        let other = live(73, "http://127.0.0.1:9/never-fetched-either.ts".into());
        let (mut h, sessions) = start_casting(vec![ch.clone(), other.clone()]);
        cast(&h, Some(ch.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        tv.send(RemoteEvent::Buffering(true));
        h.wait_state(LONG, |s| matches!(s, PlayerState::Buffering { .. }));

        let mark = h.mark();
        h.play(other.media.clone(), true);
        let (zapped, ..) = tv.load();
        tv.start(zapped);
        h.wait_state_from(mark, LONG, |s| *s == PlayerState::Playing);
    }

    /// Three quick skips while casting add up: each counts from the last one's target, not from
    /// the TV's last position report (every 10 s).
    #[test]
    fn quick_skips_on_the_tv_add_up() {
        let Some((_media, _serial)) = setup() else { return };
        let m = movie(81, "http://127.0.0.1:9/never-fetched.mp4".into());
        let (h, sessions) = start_casting(vec![m.clone()]);
        cast(&h, Some(m.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        tv.send(RemoteEvent::Paused(true));
        tv.send(RemoteEvent::Position { generation, pos: 100.0, dur: Some(3600.0) });
        h.wait_snapshot(LONG, |s| s.paused && s.position == Some(100.0) && s.seekable);
        for want in [110.0, 120.0, 130.0] {
            h.handle.send(PlayerCommand::SeekBy(10.0));
            assert_eq!(tv.next(), RemoteCmd::SeekTo(want));
        }
        h.handle.send(PlayerCommand::SeekBy(600.0));
        h.handle.send(PlayerCommand::SeekBy(-10.0));
        assert_eq!((tv.next(), tv.next()), (RemoteCmd::SeekTo(730.0), RemoteCmd::SeekTo(720.0)), "a small skip keeps the big one");
    }

    /// The provider breaks a film's stream while the TV still plays what it buffered: the
    /// reconnect resumes where the TV is by then (its last report run on while it played, 2 s back
    /// as ever), not at that report, which can be 10 s old, nor at the cast's first start
    /// (2026-10-13: "it reset back to the start").
    #[test]
    fn a_reconnect_resumes_where_the_tv_is_now() {
        let Some((_media, _serial)) = setup() else { return };
        let m = movie(82, "http://127.0.0.1:9/never-fetched.mp4".into());
        let (h, sessions) = start_casting(vec![m.clone()]);
        cast(&h, Some(m.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        tv.send(RemoteEvent::Position { generation, pos: 100.0, dur: Some(3600.0) });
        h.wait_snapshot(LONG, |s| casting_and_playing(s) && s.position == Some(100.0));
        let reported = Instant::now();
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        let (again, p, _) = tv.load();
        let ran = reported.elapsed().as_secs_f64();
        assert!(again > generation && p.media == m.media);
        let at = p.start_at.expect("a resume point");
        assert!(at > 98.0 && at <= 98.0 + ran, "resumed at {at}: the report (100 s) run on for up to {ran:.3} s, 2 s back");
    }

    /// A film on the TV: transport goes to the session, positions are the progress, and the
    /// TV's `itemPlayedToEnd` is the film's `Ended` (Up next's cue).
    #[test]
    fn a_film_on_the_tv_saves_its_progress_and_ends() {
        let Some((_media, _serial)) = setup() else { return };
        let m = movie(80, "http://127.0.0.1:9/never-fetched.mp4".into());
        let (mut h, sessions) = start_casting(vec![m.clone()]);
        cast(&h, Some(m.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        tv.send(RemoteEvent::Position { generation, pos: 30.0, dur: Some(60.0) });
        h.wait_snapshot(LONG, |s| casting_and_playing(s) && s.position == Some(30.0) && s.duration == Some(60.0) && s.seekable);
        h.handle.send(PlayerCommand::SetAudioTrack(Some(2)));
        h.handle.send(PlayerCommand::SeekTo(10.0));
        assert_eq!(tv.next(), RemoteCmd::SeekTo(10.0), "a track choice is a no-op on the TV");
        h.handle.send(PlayerCommand::TogglePause);
        assert_eq!(tv.next(), RemoteCmd::SetPaused(true));
        tv.send(RemoteEvent::Paused(true));
        h.wait_snapshot(LONG, |s| s.state == PlayerState::Paused && s.paused);
        tv.send(RemoteEvent::Playing { generation });
        h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && !s.paused);

        tv.send(RemoteEvent::Position { generation, pos: 59.5, dur: Some(60.0) });
        tv.send(RemoteEvent::Ended { generation });
        let ended = h.wait_event(LONG, |e| matches!(e, PlayerEvent::Ended { .. })).expect("Ended");
        assert_eq!(ended, PlayerEvent::Ended { item: m.item.clone(), finished: true });
        h.wait_state(LONG, |s| matches!(s, PlayerState::Ended));
        let last = h.rec.progress.lock().unwrap().last().cloned().expect("the final save");
        assert_eq!((last.position_s, last.duration_s, last.finished), (59.5, Some(60.0), true));
        assert_eq!(h.rec.history.lock().unwrap().len(), 1);
        assert!(h.snapshot().cast.is_some(), "the TV stays for Up next");
    }

    /// The sleep timer's (and Ctrl+Shift+S's) Stop ends casting: the session closes, the lease
    /// goes with the relay's upstream, the player is Idle here.
    #[test]
    fn a_stop_turns_the_tv_off_and_leaves_the_player_idle() {
        let Some((_media, _serial)) = setup() else { return };
        let ch = live(85, "http://127.0.0.1:9/never-fetched.ts".into());
        let (mut h, sessions) = start_casting(vec![ch.clone()]);
        h.handle.send(PlayerCommand::SetConnectionLimit { source: SourceId(1), max: Some(1) });
        cast(&h, Some(ch.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        let mark = h.mark();
        h.handle.send(PlayerCommand::Stop);
        assert_eq!(tv.next(), RemoteCmd::Close { fast: false });
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        h.wait_state_from(mark, LONG, |s| *s == PlayerState::Idle);
        assert!(h.wait_snapshot(LONG, |s| s.cast.is_none()).last_error.is_none());
        assert_eq!(a_leases(&h.player), 0);
    }

    /// A lost session is an error with the casting card's Play here (the picture stays on
    /// the TV until the user picks); Cast again opens a new session; the TV's own stop ends
    /// casting quietly (Idle, `CastEnded` for the alert).
    #[test]
    fn a_lost_tv_errors_with_play_here_and_a_stop_on_the_tv_goes_idle() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let ch = live(90, srv.url("long/index.m3u8"));
        let (mut h, sessions) = start_casting(vec![ch.clone()]);
        cast(&h, Some(ch.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        tv.send(RemoteEvent::Lost(CastErrorKind::ReceiverFailed));
        let s = h.wait_state(LONG, |s| matches!(s, PlayerState::Error(_)));
        assert!(matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::Cast(CastErrorKind::ReceiverFailed), .. })), "{s:?}");
        let snap = h.wait_snapshot(LONG, |s| matches!(s.state, PlayerState::Error(_)));
        assert_eq!(snap.cast.as_ref().map(|c| c.phase), Some(CastPhase::Lost), "still casting: Play here is on the card");
        assert!(h.rec.failures.lock().unwrap().iter().all(|(_, k)| k.is_none()), "the TV's failure isn't the channel's");

        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        h.wait_snapshot(LONG, |s| s.cast.is_none() && s.state == PlayerState::Playing && s.position.is_some());

        cast(&h, None);
        let mut tv = sessions.recv_timeout(LONG).expect("cast again: a new session");
        let (generation, playable, _) = tv.load();
        assert_eq!(playable.media, ch.media);
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);
        let mark = h.mark();
        tv.send(RemoteEvent::Lost(CastErrorKind::StoppedOnTv));
        h.wait_state_from(mark, LONG, |s| *s == PlayerState::Idle);
        let ended = h.wait_event_from(mark, LONG, |e| matches!(e, PlayerEvent::CastEnded { .. })).expect("CastEnded");
        assert_eq!(ended, PlayerEvent::CastEnded { device: device(), name: "Living Room".into(), kind: CastErrorKind::StoppedOnTv });
        assert!(!states_from(&mut h, mark).iter().any(|s| matches!(s, PlayerState::Error(_))), "no error state");
        assert!(h.wait_snapshot(LONG, |s| s.cast.is_none()).last_error.is_none());
    }

    /// The Play on switch (DESIGN §40 §3.9): with it on a TV every play goes there — nothing
    /// plays here first, the TV's first load is the play — except the Live preview's, which plays
    /// here (moving the cursor never casts). Play here plays here and leaves the switch on the
    /// TV: the next play goes there again.
    #[test]
    fn play_on_casts_every_play_but_the_preview_and_play_here_keeps_it() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let ch = live(74, srv.url("long/index.m3u8"));
        let other = live(75, "http://127.0.0.1:9/never-fetched.ts".into());
        let (mut h, sessions) = start_casting(vec![ch.clone(), other.clone()]);
        h.handle.send(PlayerCommand::SetPlayOn(Some(device())));
        h.wait_snapshot(LONG, |s| s.play_on == Some(device()));

        let preview = PlayRequest { media: ch.media.clone(), zap: None, origin: PlayOrigin::LivePreview, resume: false, immediate: true };
        h.handle.send(PlayerCommand::Play(preview));
        h.wait_state(LONG, is_playing);
        assert!(sessions.try_recv().is_err(), "the preview opened no session");
        assert!(!srv.requests().is_empty() && h.snapshot().cast.is_none(), "the preview plays here");

        h.play(other.media.clone(), true);
        let mut tv = sessions.recv_timeout(LONG).expect("the play went to the TV");
        let (generation, playable, _) = tv.load();
        assert_eq!(playable.media, other.media);
        tv.start(generation);
        h.wait_snapshot(LONG, casting_and_playing);

        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        assert_eq!(tv.next(), RemoteCmd::Close { fast: false });
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        let s = h.wait_snapshot(LONG, |s| s.cast.is_none() && s.playable.as_ref().is_some_and(|p| p.media == other.media));
        assert_eq!(s.play_on, Some(device()), "Play here leaves the switch alone");
        h.play(ch.media.clone(), true);
        let mut tv = sessions.recv_timeout(LONG).expect("the next play: on the TV again");
        assert_eq!(tv.load().1.media, ch.media);
    }

    /// "Cast to…" a film with copies (and an agent's `play {device}` of one) is `CastCopy`: the
    /// copy plays on the TV as `PlayCopy` plays here — one that can't be resolved falls back to
    /// the next copy, at the start point asked for (the accepted limit of 0.3.0 lost both).
    #[test]
    fn a_copy_cast_falls_back_and_keeps_its_start() {
        let Some((_media, _serial)) = setup() else { return };
        let second = movie(84, "http://127.0.0.1:9/never-fetched.mp4".into());
        let (mut h, sessions) = start_casting(vec![second.clone()]);
        // copy 83 isn't in the table: it can't be resolved
        let req = PlayRequest { media: MediaRef::Movie(MovieId(83)), zap: None, origin: PlayOrigin::Fullscreen, resume: true, immediate: true };
        let copy = tuner::player::CopyPlay { req, fallback: Some(second.media.clone()), elsewhere: None, start_at: Some(2460.0) };
        h.handle.send(PlayerCommand::CastCopy { device: device(), copy });
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (_, p, _) = tv.load();
        assert_eq!((&p.media, p.start_at), (&second.media, Some(2460.0)));
        assert!(h.wait_event(LONG, |e| matches!(e, PlayerEvent::FellBack { .. })).is_some(), "the toast names the copy");
    }

    /// A play the TV never got — its session couldn't open — is a cast error with nothing
    /// played here; the alert's Play here plays it here.
    #[test]
    fn a_cast_that_never_opened_plays_here_on_play_here() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let ch = live(76, srv.url("long/index.m3u8"));
        let mut h = start_with(vec![ch.clone()], Arc::new(NoTv));
        h.handle.send(PlayerCommand::SetPlayOn(Some(device())));
        h.play(ch.media.clone(), true);
        let failed = h.wait_event(LONG, |e| matches!(e, PlayerEvent::Failed(_)));
        assert!(matches!(failed, Some(PlayerEvent::Failed(PlayerError { kind: PlayerErrorKind::Cast(CastErrorKind::PairingRequired), .. }))));
        assert!(srv.requests().is_empty() && h.snapshot().state == PlayerState::Idle, "nothing plays here by itself");
        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        h.wait_state(LONG, is_playing);
        assert!(h.snapshot().playable.as_ref().is_some_and(|p| p.media == ch.media));
    }

    /// The Play on switch chosen while picture A plays here (in multi-view, then it ended; or
    /// a move to the TV that failed): previous channel, the next channel and numeric tuning
    /// go to the TV as a `Play` does, not here (DESIGN §40 §3.9).
    #[test]
    fn play_on_sends_zaps_recall_and_numeric_tuning_of_a_local_picture_to_the_tv() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let a = live(77, srv.url("long/index.m3u8"));
        let b = live(78, srv.url("long/index.m3u8"));
        let (mut h, sessions) = start_casting(vec![a.clone(), b.clone()]);
        let zap = ZapList { collection: Collection::AllChannels, channels: vec![ChannelId(77), ChannelId(78)].into() };
        h.handle.send(PlayerCommand::Play(PlayRequest {
            media: a.media.clone(),
            zap: Some(zap),
            origin: PlayOrigin::Fullscreen,
            resume: false,
            immediate: true,
        }));
        h.wait_state(LONG, is_playing);
        let mark = h.mark();
        h.play(b.media.clone(), true);
        h.wait_state_from(mark, LONG, is_playing);
        h.handle.send(PlayerCommand::SetPlayOn(Some(device())));
        let s = h.wait_snapshot(LONG, |s| s.play_on == Some(device()));
        assert!(s.cast.is_none() && s.previous_live.as_ref().is_some_and(|p| p.media == a.media), "b plays here, a before it");

        // each lands on a TV; Play here brings the picture back here for the next one
        let on_tv = |h: &mut Harness, cmd: PlayerCommand, want: &Playable| {
            h.handle.send(cmd);
            let mut tv = sessions.recv_timeout(LONG).expect("a session: the play went to the TV");
            let (generation, playable, _) = tv.load();
            assert_eq!(playable.media, want.media);
            tv.start(generation);
            h.wait_snapshot(LONG, casting_and_playing);
            h.handle.send(PlayerCommand::StopCasting { play_here: true });
            assert_eq!(tv.next(), RemoteCmd::Close { fast: false });
            tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
            let s = h.wait_snapshot(LONG, |s| s.cast.is_none() && s.state == PlayerState::Playing);
            assert_eq!(s.playable.as_ref().map(|p| p.media.clone()), Some(want.media.clone()), "Play here: the same item here");
        };
        on_tv(&mut h, PlayerCommand::LastChannel { unlocked: true }, &a);
        on_tv(&mut h, PlayerCommand::Zap { forward: true, unlocked: true }, &b);
        on_tv(&mut h, PlayerCommand::ZapTo { id: ChannelId(77), unlocked: true }, &a);
    }

    /// A play the Play on switch sent to a TV whose session can't open, while an older item
    /// still plays here: the failure's Play here replaces the older item with it.
    #[test]
    fn play_here_after_a_cast_that_never_opened_replaces_what_still_plays_here() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let a = live(79, srv.url("long/index.m3u8"));
        let b = live(80, srv.url("long/index.m3u8"));
        let mut h = start_with(vec![a.clone(), b.clone()], Arc::new(NoTv));
        h.play(a.media.clone(), true);
        h.wait_state(LONG, is_playing);
        h.handle.send(PlayerCommand::SetPlayOn(Some(device())));
        let mark = h.mark();
        h.play(b.media.clone(), true);
        assert!(h.wait_event_from(mark, LONG, |e| matches!(e, PlayerEvent::Failed(_))).is_some(), "the cast failed");
        let s = h.snapshot();
        assert_eq!((s.state.clone(), s.playable.as_ref().map(|p| p.media.clone())), (PlayerState::Playing, Some(a.media.clone())), "a plays on");
        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        let s = h.wait_snapshot(LONG, |s| s.state == PlayerState::Playing && s.playable.as_ref().is_some_and(|p| p.media == b.media));
        assert!(s.cast.is_none(), "here, not on a TV");
    }

    /// Play here: the session closes (its last position report still counts), and the film
    /// carries on here from where the TV was.
    #[test]
    fn play_here_carries_on_from_the_tvs_position() {
        let Some((media, _serial)) = setup() else { return };
        let srv = TestServer::start(media, Faults::default());
        let m = movie(95, srv.url("long/index.m3u8"));
        let (h, sessions) = start_casting(vec![m.clone()]);
        cast(&h, Some(m.media.clone()));
        let mut tv = sessions.recv_timeout(LONG).expect("a session");
        let (generation, ..) = tv.load();
        tv.start(generation);
        tv.send(RemoteEvent::Position { generation, pos: 30.0, dur: Some(60.0) });
        h.wait_snapshot(LONG, |s| casting_and_playing(s) && s.position == Some(30.0));
        assert!(srv.requests().is_empty(), "the TV's relay fetches, not mpv");

        h.handle.send(PlayerCommand::StopCasting { play_here: true });
        assert_eq!(tv.next(), RemoteCmd::Close { fast: false });
        tv.send(RemoteEvent::Position { generation, pos: 31.0, dur: Some(60.0) });
        tv.send(RemoteEvent::UpstreamClosed { generation, at: Instant::now() });
        let s = h.wait_snapshot(LONG, |s| s.cast.is_none() && s.state == PlayerState::Playing && s.position.is_some());
        assert!(s.position.unwrap() >= 30.0, "resumed at {:?}", s.position);
        let saved = h.rec.progress.lock().unwrap().clone();
        assert!(saved.iter().any(|p| p.position_s == 31.0 && !p.finished), "the TV's last position was saved: {saved:?}");
    }
}
