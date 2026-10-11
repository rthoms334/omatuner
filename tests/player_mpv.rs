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
use tuner::config::Config;
use tuner::model::{
    ChannelId, Collection, HistoryEntry, HttpHeaders, ItemKey, ItemKind, ItemRef, MediaRef, MovieId, PlayOrigin, Playable, PlayableKind, SourceId,
    WatchProgress, ZapList,
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
        let (bus, events) = EventBus::new(None);
        let rec = Recorded::default();
        let deps = PlayerDeps {
            bus,
            store: Box::new(RecordingStore(rec.clone())),
            resolver: Box::new(TableResolver(table.into_iter().map(|p| (p.media.clone(), p)).collect())),
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

// ---------------------------------------------------------------------------------------------
// tests

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
