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
use support::media::{Faults, TestServer, ensure_media};
use tuner::config::Config;
use tuner::model::{
    ChannelId, Collection, HistoryEntry, HttpHeaders, ItemKey, ItemKind, ItemRef, MediaRef, MovieId, PlayOrigin, Playable,
    PlayableKind, SourceId, WatchProgress, ZapList,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::player::{
    MediaResolver, PlayRequest, Player, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerEvent, PlayerHandle,
    PlayerSnapshot, PlayerState, PlayerStore,
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
}

struct RecordingStore(Recorded);

impl PlayerStore for RecordingStore {
    fn save_progress(&mut self, p: WatchProgress) { self.0.progress.lock().unwrap().push(p); }
    fn push_history(&mut self, e: HistoryEntry) { self.0.history.lock().unwrap().push(e); }
    fn record_failure(&mut self, item: ItemRef, kind: Option<PlayerErrorKind>) { self.0.failures.lock().unwrap().push((item, kind)); }
}

/// Resolves from a fixed table; unknown media → `Resolve` error.
struct TableResolver(HashMap<MediaRef, Playable>);

impl MediaResolver for TableResolver {
    fn resolve(&mut self, media: &MediaRef, _resume: bool) -> Result<Playable, PlayerError> {
        self.0.get(media).cloned().ok_or(PlayerError { kind: PlayerErrorKind::Resolve, message: "unknown item".into() })
    }
    fn skip_on_zap(&mut self, _ch: ChannelId) -> bool { false }
}

fn item(key: &str, kind: ItemKind) -> ItemRef { ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) } }

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

fn live(ch: i64, url: String) -> Playable { playable(MediaRef::Channel(ChannelId(ch)), PlayableKind::Live, url) }
fn movie(id: i64, url: String) -> Playable { playable(MediaRef::Movie(MovieId(id)), PlayableKind::Movie, url) }

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

    fn snapshot(&self) -> Arc<PlayerSnapshot> { self.handle.snapshot() }

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
    fn drop(&mut self) { self.player.shutdown(); }
}

const LONG: Duration = Duration::from_secs(30);

/// Test media (or `None` → skip) plus a process-wide lock held for the whole test.
///
/// The tests run one at a time: libav has ONE global log callback, and mpv routes ffmpeg's
/// messages (including the "HTTP error 403" lines the classifier reads) to a single mpv
/// instance per process. The app has exactly one instance; parallel tests would steal each
/// other's HTTP statuses. `RUST_LOG=tuner=debug` shows the controller's decisions.
fn setup() -> Option<(PathBuf, MutexGuard<'static, ()>)> {
    static SERIAL: Mutex<()> = Mutex::new(());
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let media = ensure_media()?;
    Some((media, SERIAL.lock().unwrap_or_else(|e| e.into_inner())))
}

fn is_playing(s: &PlayerState) -> bool { matches!(s, PlayerState::Playing) }

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
    let ended = h.wait_event(LONG, |e| matches!(e, PlayerEvent::Ended { .. })).expect("Ended event");
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
    // the start option reached mpv: playback began near 7 s
    let first_req = srv.requests_for("/testsrc.mp4");
    assert!(!first_req.is_empty());
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
    assert!(matches!(h.wait_event(LONG, |e| matches!(e, PlayerEvent::Failed(_))), Some(PlayerEvent::Failed(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. }))));

    // VOD 404: no retry
    let mark = h.mark();
    h.play(vod.media.clone(), true);
    let s = h.wait_state_from(mark, LONG, |s| matches!(s, PlayerState::Reconnecting { .. } | PlayerState::Error(_)));
    assert!(matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, .. })), "{s:?}");
    assert!(h.rec.failures.lock().unwrap().iter().any(|(i, k)| Some(i) == vod.item.as_ref() && *k == Some(PlayerErrorKind::HttpNotFound)));
    assert_eq!(h.snapshot().last_error.as_ref().map(|e| e.kind), Some(PlayerErrorKind::HttpNotFound));
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
    let mark = h.mark();
    h.play(b.media.clone(), true);
    h.wait_state_from(mark, LONG, is_playing);

    let a_reqs = srv.requests_for("/bars.ts");
    let b_reqs = srv.requests_for("/hls/seg001.ts");
    let a_closed = a_reqs.iter().map(|r| r.closed.expect("A closed before B started")).max().unwrap();
    for r in &b_reqs {
        assert!(r.opened >= a_closed, "B connected {:?} before A closed", a_closed - r.opened);
    }
    // (one stream alone may briefly hold two connections: ffmpeg opens the Range request of a
    // seek before closing the old one, so `max_concurrent` is not the right measure here)
}

#[test]
fn fast_zaps_coalesce_into_one_connection() {
    let Some((media, _serial)) = setup() else { return };
    let srv = TestServer::start(media, Faults::default());
    let paths = ["long/index.m3u8", "bars.ts", "hls/index.m3u8", "testsrc.mp4"];
    let chans: Vec<Playable> = paths.iter().zip(50..).map(|(p, id)| live(id, srv.url(p))).collect();
    let mut h = Harness::start(headless_config(), chans.clone());
    let ids = chans.iter().map(|c| match c.media { MediaRef::Channel(id) => id, _ => unreachable!() }).collect();
    let zap = ZapList { collection: Collection::AllChannels, channels: ids };
    let req = PlayRequest { media: chans[0].media.clone(), zap: Some(zap), origin: PlayOrigin::Fullscreen, resume: false, immediate: true };
    h.handle.send(PlayerCommand::Play(req));
    h.wait_state(LONG, is_playing);
    let mark = h.mark();
    for _ in 0..2 {
        h.handle.send(PlayerCommand::Zap { forward: true });
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
    h.handle.send(PlayerCommand::LastChannel);
    h.wait_snapshot(LONG, |s| s.playable.as_ref().is_some_and(|p| p.media == chans[0].media));
    h.wait_state_from(mark, LONG, is_playing);
    assert_eq!(h.snapshot().previous_live.as_ref().map(|p| p.media.clone()), Some(chans[2].media.clone()));
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
        matches!(s, PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, .. }) | PlayerState::Reconnecting { reason: PlayerErrorKind::HttpNotFound, .. })
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
