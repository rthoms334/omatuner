//! The cast service and its session orchestrator (DESIGN §40 §5.6, §7.3, §8.3, §8.5, §9.4, §11.2)
//! against a scripted receiver (`receiver::FakeReceiver`) and relay (`session::FakeRelay`), a
//! scripted discovery and firewall detection, in a temp profile with a real store. Nothing
//! touches the LAN, logind or the firewall. Every wait is a bounded poll; the protocol-level fake
//! TV is `tests/cast_airplay.rs`'s.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use tuner::cast::discovery::{FakeDiscovery, Sighting};
use tuner::cast::firewall::{AllowOutcome, Detection, FirewallState, Rule, Verb};
use tuner::cast::receiver::{Backend, Call, FakeReceiver, FakeRemote, ReceiverEvent};
use tuner::cast::relay::{RelayConfig, RelayEvent};
use tuner::cast::service::{CastDeps, CastService};
use tuner::cast::session::{FakeRelay, FakeRelayRemote, Links, Relay, RelayCall, Timings};
use tuner::cast::{CastErrorKind, CastHandle, CastLevel, CastPhase, Credentials, Device, DeviceId, PairVariant, Pairing, PeerCredentials, Protocol};
use tuner::cast::{ReceiverKind, Usability};
use tuner::config::Config;
use tuner::model::{ChannelId, HttpHeaders, ItemKey, ItemKind, ItemRef, MediaRef, MovieId, Playable, PlayableKind, SourceId};
use tuner::paths::AppPaths;
use tuner::player::PlayerErrorKind;
use tuner::player::remote::{RemoteCmd, RemoteEvent, RemoteOutput};
use tuner::redact::Secret;
use tuner::store::Store;
use tuner::store::cast::StoredDevice;

const WAIT: Duration = Duration::from_secs(10);
const TV_ADDR: &str = "127.0.0.1:7000";

/// Hands out the receivers and relays a test queued, and answers detection as scripted.
struct TestLinks {
    receivers: Mutex<VecDeque<Backend>>,
    relays: Mutex<VecDeque<Relay>>,
    firewall: Mutex<FirewallState>,
    connects: AtomicUsize,
}

impl TestLinks {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            receivers: Mutex::default(),
            relays: Mutex::default(),
            firewall: Mutex::new(FirewallState::NoFirewall),
            connects: AtomicUsize::new(0),
        })
    }

    fn receiver(&self) -> FakeRemote {
        let (rx, remote) = FakeReceiver::new();
        self.receivers.lock().unwrap().push_back(Backend::Fake(rx));
        remote
    }

    fn relay(&self) -> FakeRelayRemote {
        let (relay, remote) = FakeRelay::new();
        self.relays.lock().unwrap().push_back(Relay::Fake(relay));
        remote
    }
}

impl Links for TestLinks {
    fn connect(&self, _device: &Device, _creds: &Credentials) -> BoxFuture<'static, Result<Backend, CastErrorKind>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let b = self.receivers.lock().unwrap().pop_front();
        Box::pin(async move { b.ok_or(CastErrorKind::Unreachable) })
    }

    fn relay(&self, _cfg: RelayConfig) -> BoxFuture<'static, Result<Relay, CastErrorKind>> {
        let r = self.relays.lock().unwrap().pop_front();
        Box::pin(async move { r.ok_or(CastErrorKind::Unsupported) })
    }

    fn detect(&self, port: u16, _toward: std::net::Ipv4Addr) -> BoxFuture<'static, Detection> {
        let state = *self.firewall.lock().unwrap();
        Box::pin(async move { Detection { state, ufw: true, pkexec: true, net: None, tagged: Vec::new(), port } })
    }

    fn firewall(&self, _verb: Verb, _rule: Rule, _cancel: CancellationToken) -> BoxFuture<'static, AllowOutcome> {
        Box::pin(async { AllowOutcome::Cancelled })
    }
}

fn tv_id() -> DeviceId {
    DeviceId::derive("airplay", "living-room-pi")
}

fn sighting(id: DeviceId, name: &str) -> Sighting {
    Sighting {
        id,
        key: format!("{name}._airplay._tcp.local."),
        name: name.into(),
        kind: ReceiverKind::AppleTv,
        protocol: Protocol::AirPlay,
        model: Some("AppleTV14,1".into()),
        os_version: Some("26.6 (23L773)".into()),
        source_version: Some("960.13.1".into()),
        pairing_required: true,
        usability: Usability::Usable,
        experimental: false,
        manual: false,
        addr: TV_ADDR.parse().unwrap(),
    }
}

struct Rig {
    service: CastService,
    cast: CastHandle,
    links: Arc<TestLinks>,
    discovery: Arc<FakeDiscovery>,
    store: Store,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A service with "Living Room" paired (stored, and announced by discovery) and one playlist.
fn rig(name: &str, timings: Timings) -> Rig {
    tuner::platform::keep_off_desktop();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_temp(name, None).unwrap();
    store
        .writer()
        .blocking(|c| {
            c.execute("INSERT INTO source(name, kind, url, created_at) VALUES ('S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0)", [])?;
            tuner::store::cast::ensure_identity(c, 1)?;
            let mut tv = StoredDevice::seen(tv_id(), Protocol::AirPlay, "Living Room");
            tv.model = Some("AppleTV14,1".into());
            tv.last_addr = Some(TV_ADDR.parse().unwrap());
            tuner::store::cast::upsert_device(c, &tv)?;
            let peer = PeerCredentials { peer_id: "tv-pairing-id".into(), peer_ltpk: [9; 32], variant: PairVariant::Hkp3 };
            tuner::store::cast::set_paired(c, tv_id(), &peer, 2)
        })
        .unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut cfg = Config::default();
    cfg.cast.keep_awake = false;
    let links = TestLinks::new();
    let discovery = Arc::new(FakeDiscovery::default());
    discovery.with(|s| s.sightings.push(sighting(tv_id(), "Living Room")));
    let deps = CastDeps {
        store: store.clone(),
        config: Arc::new(RwLock::new(Arc::new(cfg))),
        paths: AppPaths::rooted(dir.path()),
        discovery: discovery.clone(),
        links: links.clone(),
        timings,
    };
    let service = CastService::start(rt.handle(), deps).unwrap();
    let cast = service.handle();
    until("the paired TV is listed", || cast.devices().iter().any(|d| d.id == tv_id() && matches!(d.pairing, Pairing::Paired(_))));
    Rig { service, cast, links, discovery, store, _rt: rt, _dir: dir }
}

/// Timings that never fire on their own during a test.
fn calm() -> Timings {
    Timings { fetch_watchdog: Duration::from_secs(600), stall: Duration::from_secs(600), ..Timings::default() }
}

fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Events up to and including the first that matches.
fn wait_event(rx: &Receiver<RemoteEvent>, what: &str, mut hit: impl FnMut(&RemoteEvent) -> bool) -> Vec<RemoteEvent> {
    let end = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ev) => {
                let done = hit(&ev);
                seen.push(ev);
                if done {
                    return seen;
                }
            }
            Err(_) => panic!("timed out waiting for {what}; got {seen:?}"),
        }
    }
}

fn item(kind: ItemKind, key: &str) -> ItemRef {
    ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) }
}

fn playable(media: MediaRef, kind: PlayableKind, item: ItemRef, start_at: Option<f64>, duration_hint: Option<f64>) -> Arc<Playable> {
    Arc::new(Playable {
        media,
        kind,
        item: Some(item),
        source_id: Some(SourceId(1)),
        title: "Test".into(),
        subtitle: None,
        logo: None,
        number: None,
        url: Secret::new("http://provider.example/live/user/secret/1.ts".into()),
        headers: HttpHeaders::default(),
        start_at,
        duration_hint,
    })
}

fn channel(key: &str) -> Arc<Playable> {
    playable(MediaRef::Channel(ChannelId(1)), PlayableKind::Live, item(ItemKind::Channel, key), None, None)
}

fn film(start_at: Option<f64>) -> Arc<Playable> {
    playable(MediaRef::Movie(MovieId(1)), PlayableKind::Movie, item(ItemKind::Movie, "m:1"), start_at, Some(7200.0))
}

fn load(out: &RemoteOutput, generation: u64, playable: Arc<Playable>) {
    out.cmd.send(RemoteCmd::Load { generation, playable, hints: Default::default() }).unwrap();
}

fn calls(rx: &FakeRemote) -> Vec<Call> {
    rx.with(|s| s.calls.clone())
}

/// The URL the relay handed the session for its `n`th load (what the TV must be given).
fn is_relay_url(c: &Call) -> bool {
    let url = match c {
        Call::Load(r) | Call::Replace(r) => r.url.expose().clone(),
        _ => return false,
    };
    url.starts_with("http://192.0.2.1:47800/c/") && !url.contains("provider.example")
}

/// The first load is an insert of the relay's token URL, a zap replaces on the same session,
/// positions come back absolute (a resumed film's offset added, its known duration kept),
/// what played is cached for the item and TV, and Close stops the TV and the relay and still
/// reports the upstream closed (the controller's lease).
#[test]
fn a_cast_loads_then_zaps_by_replace_with_absolute_positions() {
    let r = rig("tuner-cast-service-load", calm());
    let tv = r.links.receiver();
    let relay = r.links.relay();
    let out = r.cast.open(tv_id()).unwrap();
    assert_eq!((out.view.phase, out.view.volume, out.view.name.as_str()), (CastPhase::Connecting, false, "Living Room"));

    load(&out, 1, channel("ch:news"));
    let evs = wait_event(&out.events, "BehindLive", |e| matches!(e, RemoteEvent::BehindLive(_)));
    assert!(evs.contains(&RemoteEvent::Level(CastLevel::Repackaged)) && evs.contains(&RemoteEvent::BehindLive(Some(15))), "{evs:?}");
    until("the insert", || calls(&tv).len() == 1);
    assert!(matches!(&calls(&tv)[0], Call::Load(req) if req.live && req.offset_s == 0.0) && is_relay_url(&calls(&tv)[0]));
    relay.emit(RelayEvent::UpstreamOpened { generation: 1 });
    relay.emit(RelayEvent::PlaylistFetched { generation: 1 });
    tv.emit(ReceiverEvent::Playing);
    let evs = wait_event(&out.events, "the first picture", |e| matches!(e, RemoteEvent::Playing { generation: 1 }));
    assert!(evs.contains(&RemoteEvent::UpstreamOpened { generation: 1 }) && evs.contains(&RemoteEvent::Phase(CastPhase::Playing)), "{evs:?}");
    let ch = item(ItemKind::Channel, "ch:news");
    until("the hint is cached", || {
        let ch = ch.clone();
        let hint = r.store.writer().blocking(move |c| tuner::store::cast::hint(c, &ch, tv_id())).unwrap();
        hint.is_some_and(|h| h.level == Some(CastLevel::Repackaged))
    });

    load(&out, 2, film(Some(4200.0)));
    until("the replace", || calls(&tv).len() == 2);
    assert!(matches!(&calls(&tv)[1], Call::Replace(req) if !req.live && req.offset_s == 4200.0) && is_relay_url(&calls(&tv)[1]));
    assert_eq!(r.links.connects.load(Ordering::SeqCst), 1, "the session outlives zaps");
    tv.emit(ReceiverEvent::Playing);
    tv.emit(ReceiverEvent::Position { pos: 10.0, dur: Some(3000.0) });
    let evs = wait_event(&out.events, "a position", |e| matches!(e, RemoteEvent::Position { .. }));
    assert_eq!(evs.last(), Some(&RemoteEvent::Position { generation: 2, pos: 4210.0, dur: Some(7200.0) }));

    out.cmd.send(RemoteCmd::Close { fast: false }).unwrap();
    let evs = wait_event(&out.events, "the upstream's close", |e| matches!(e, RemoteEvent::UpstreamClosed { .. }));
    assert!(matches!(evs.last(), Some(RemoteEvent::UpstreamClosed { generation: 1, .. })));
    until("the TV and the relay stop", || {
        calls(&tv).last() == Some(&Call::Stop) && relay.with(|s| s.calls.contains(&RelayCall::Stop { fast: false }))
    });
}

/// A replace the TV refuses runs the whole session again (pair-verify … insert, §7.3) on a new
/// connection, and the item is inserted there; the controller sees no `Lost`.
#[test]
fn a_failed_replace_runs_the_whole_session_again() {
    let r = rig("tuner-cast-service-restart", calm());
    let (first, second) = (r.links.receiver(), r.links.receiver());
    let _relay = r.links.relay();
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 1, channel("ch:a"));
    until("the insert", || calls(&first).len() == 1);
    first.with(|s| s.fail_next.push(CastErrorKind::Unreachable));
    load(&out, 2, channel("ch:b"));
    until("the new session's insert", || calls(&second).len() == 1);
    assert!(matches!(calls(&first)[1], Call::Replace(_)) && calls(&first).last() == Some(&Call::Stop));
    assert!(matches!(&calls(&second)[0], Call::Load(_)), "an insert on the new connection");
    assert_eq!(r.links.connects.load(Ordering::SeqCst), 2);
    second.emit(ReceiverEvent::Playing);
    let evs = wait_event(&out.events, "the zap's picture", |e| matches!(e, RemoteEvent::Playing { generation: 2 }));
    assert!(!evs.iter().any(|e| matches!(e, RemoteEvent::Lost(_))), "{evs:?}");
}

/// The 10 s check (§9.4): the TV was told to play and never asked for the playlist. With the
/// firewall blocking that's `Firewall`; with the port allowed it's the TV (`ReceiverFailed`).
#[test]
fn a_tv_that_never_fetches_is_the_firewall_only_when_detection_blocks() {
    let watchdog = Timings { fetch_watchdog: Duration::from_millis(200), stall: Duration::from_secs(600), ..Timings::default() };
    let r = rig("tuner-cast-service-firewall", watchdog);
    let _tv = r.links.receiver();
    let _relay = r.links.relay();
    *r.links.firewall.lock().unwrap() = FirewallState::Blocked { shadowed: false };
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 1, channel("ch:a"));
    let evs = wait_event(&out.events, "the watchdog", |e| matches!(e, RemoteEvent::Failed { .. }));
    assert_eq!(evs.last(), Some(&RemoteEvent::Failed { generation: 1, kind: PlayerErrorKind::Cast(CastErrorKind::Firewall) }));
    assert_eq!(r.cast.last_error(), Some(CastErrorKind::Firewall));

    *r.links.firewall.lock().unwrap() = FirewallState::Allowed { ours: true };
    load(&out, 2, channel("ch:b"));
    let evs = wait_event(&out.events, "the watchdog", |e| matches!(e, RemoteEvent::Failed { .. }));
    assert_eq!(evs.last(), Some(&RemoteEvent::Failed { generation: 2, kind: PlayerErrorKind::Cast(CastErrorKind::ReceiverFailed) }));
}

/// Stopped from the TV's remote: the relay stops and the upstream's close reaches the controller
/// before `Lost(StoppedOnTv)` (after which it stops listening); the agent can still read why.
#[test]
fn stopped_on_the_tv_releases_the_upstream_before_lost() {
    let r = rig("tuner-cast-service-stopped", calm());
    let tv = r.links.receiver();
    let relay = r.links.relay();
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 1, channel("ch:a"));
    until("the insert", || calls(&tv).len() == 1);
    relay.emit(RelayEvent::UpstreamOpened { generation: 1 });
    relay.emit(RelayEvent::PlaylistFetched { generation: 1 });
    tv.emit(ReceiverEvent::Playing);
    wait_event(&out.events, "the picture", |e| matches!(e, RemoteEvent::Playing { .. }));
    tv.emit(ReceiverEvent::StoppedOnTv);
    let evs = wait_event(&out.events, "Lost", |e| matches!(e, RemoteEvent::Lost(_)));
    let closed = evs.iter().position(|e| matches!(e, RemoteEvent::UpstreamClosed { generation: 1, .. }));
    assert!(closed.is_some_and(|i| i < evs.len() - 1), "{evs:?}");
    assert_eq!(evs.last(), Some(&RemoteEvent::Lost(CastErrorKind::StoppedOnTv)));
    assert!(relay.with(|s| s.calls.iter().any(|c| matches!(c, RelayCall::Stop { .. }))));
    assert_eq!(r.cast.last_error(), Some(CastErrorKind::StoppedOnTv));
}

/// The playlist guard tripping escalates L2 → L3 once (a replace under the same generation) and
/// marks the item irregular; the next cast of it starts at L3 straight away (§8.3).
#[test]
fn an_irregular_playlist_escalates_once_and_the_next_cast_starts_converted() {
    let r = rig("tuner-cast-service-escalate", calm());
    let tv = r.links.receiver();
    let relay = r.links.relay();
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 1, channel("ch:sport"));
    until("the insert", || calls(&tv).len() == 1);
    relay.emit(RelayEvent::Irregular { generation: 1 });
    let evs = wait_event(&out.events, "L3", |e| *e == RemoteEvent::Level(CastLevel::Converted));
    assert!(!evs.iter().any(|e| matches!(e, RemoteEvent::Failed { .. })), "{evs:?}");
    until("the replace", || calls(&tv).len() == 2);
    assert!(matches!(calls(&tv)[1], Call::Replace(_)));
    let loads: Vec<_> = relay.with(|s| {
        s.calls.iter().filter_map(|c| if let RelayCall::Load { generation, force, .. } = c { Some((*generation, *force)) } else { None }).collect()
    });
    assert_eq!(loads, [(1, None), (1, Some(CastLevel::Converted))]);
    relay.emit(RelayEvent::Irregular { generation: 1 });
    out.cmd.send(RemoteCmd::Close { fast: false }).unwrap();
    until("the relay stops", || relay.with(|s| s.calls.iter().any(|c| matches!(c, RelayCall::Stop { .. }))));
    assert_eq!(relay.with(|s| s.calls.iter().filter(|c| matches!(c, RelayCall::Load { .. })).count()), 2, "one step only");

    let _tv = r.links.receiver();
    let relay = r.links.relay();
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 7, channel("ch:sport"));
    until("the load", || relay.with(|s| !s.calls.is_empty()));
    assert!(matches!(relay.with(|s| s.calls[0].clone()), RelayCall::Load { generation: 7, force: Some(CastLevel::Converted), .. }));
}

/// A seek inside what's produced replaces with a playlist view (its first segment is the new
/// offset); beyond it, the film restarts there (`-ss`) and the card says "Seeking…". The
/// restart's provider close and reopen stay inside generation 1: the controller hears only the
/// final close (one per generation, which it would read as the upstream dropping).
#[test]
fn a_seek_uses_a_view_inside_what_is_produced_and_a_restart_beyond() {
    let r = rig("tuner-cast-service-seek", calm());
    let tv = r.links.receiver();
    let relay = r.links.relay();
    relay.with(|s| s.produced_s = 600.0);
    let out = r.cast.open(tv_id()).unwrap();
    load(&out, 1, film(None));
    until("the insert", || calls(&tv).len() == 1);
    relay.emit(RelayEvent::UpstreamOpened { generation: 1 });
    relay.emit(RelayEvent::PlaylistFetched { generation: 1 });
    tv.emit(ReceiverEvent::Playing);
    wait_event(&out.events, "the picture", |e| matches!(e, RemoteEvent::Playing { .. }));

    out.cmd.send(RemoteCmd::SeekTo(301.0)).unwrap();
    until("the view's replace", || calls(&tv).len() == 2);
    assert!(matches!(&calls(&tv)[1], Call::Replace(req) if req.offset_s == 300.0));
    tv.emit(ReceiverEvent::Position { pos: 2.0, dur: None });
    let evs = wait_event(&out.events, "a position", |e| matches!(e, RemoteEvent::Position { .. }));
    assert!(evs.contains(&RemoteEvent::Phase(CastPhase::Seeking)));
    assert_eq!(evs.last(), Some(&RemoteEvent::Position { generation: 1, pos: 302.0, dur: Some(7200.0) }));

    out.cmd.send(RemoteCmd::SeekTo(5000.0)).unwrap();
    until("the restart's replace", || calls(&tv).len() == 3);
    assert!(matches!(&calls(&tv)[2], Call::Replace(req) if req.offset_s == 5000.0));
    assert!(relay.with(|s| s.calls.iter().any(|c| matches!(c, RelayCall::Load { generation: 1, start_at: Some(t), .. } if *t == 5000.0))));

    let old_feed = std::time::Instant::now();
    relay.emit(RelayEvent::UpstreamClosed { generation: 1, at: old_feed });
    relay.emit(RelayEvent::UpstreamOpened { generation: 1 });
    out.cmd.send(RemoteCmd::Close { fast: false }).unwrap();
    let evs = wait_event(&out.events, "the final close", |e| matches!(e, RemoteEvent::UpstreamClosed { .. }));
    assert!(!evs.iter().any(|e| matches!(e, RemoteEvent::UpstreamOpened { .. })), "{evs:?}");
    assert!(matches!(evs.last(), Some(RemoteEvent::UpstreamClosed { generation: 1, at }) if *at != old_feed), "{evs:?}");
}

/// Catch-up can seek only as far as it has loaded (§8.5): a seek beyond the produced edge is a
/// view at the edge (no new upstream), and the controller is told for its banner.
#[test]
fn a_catch_up_seek_past_what_is_loaded_clamps_to_the_edge() {
    let r = rig("tuner-cast-service-clamp", calm());
    let tv = r.links.receiver();
    let relay = r.links.relay();
    relay.with(|s| s.produced_s = 600.0);
    let out = r.cast.open(tv_id()).unwrap();
    let kind = PlayableKind::Catchup { start: 1_000, stop: 4_600 };
    let media = MediaRef::Catchup { channel: ChannelId(1), start: 1_000, stop: 4_600, catchup_id: None };
    load(&out, 1, playable(media, kind, item(ItemKind::Channel, "ch:a"), None, Some(3600.0)));
    until("the insert", || calls(&tv).len() == 1);
    relay.emit(RelayEvent::PlaylistFetched { generation: 1 });
    tv.emit(ReceiverEvent::Playing);
    wait_event(&out.events, "the picture", |e| matches!(e, RemoteEvent::Playing { .. }));

    out.cmd.send(RemoteCmd::SeekTo(2000.0)).unwrap();
    wait_event(&out.events, "the clamp", |e| *e == RemoteEvent::SeekClamped { generation: 1 });
    until("the view's replace", || calls(&tv).len() == 2);
    assert!(matches!(&calls(&tv)[1], Call::Replace(req) if req.offset_s == 600.0));
    let loads = relay.with(|s| s.calls.iter().filter(|c| matches!(c, RelayCall::Load { .. })).count());
    assert_eq!(loads, 1, "no restart: catch-up keeps its one upstream");
}

/// Discovery lists a new TV as "Pair first" (and the store remembers it); casting to it is
/// refused until it is paired.
#[test]
fn casting_needs_a_paired_tv() {
    let r = rig("tuner-cast-service-unpaired", calm());
    let den = DeviceId::derive("airplay", "den-pi");
    let mut s = sighting(den, "Den");
    s.key = "Den._airplay._tcp.local.".into();
    s.addr = "127.0.0.2:7000".parse::<SocketAddr>().unwrap();
    r.discovery.with(|d| d.sightings = vec![s]);
    r.cast.browse();
    until("the new TV", || r.cast.devices().iter().any(|d| d.id == den && d.pairing == Pairing::Required && d.available));
    until("it is remembered", || r.store.writer().blocking(move |c| tuner::store::cast::device(c, den)).unwrap().is_some());
    let err = r.cast.open(den).unwrap_err();
    assert_eq!(err.kind, PlayerErrorKind::Cast(CastErrorKind::PairingRequired));
    assert!(!err.message.contains("Den"), "copy never carries device text");
    drop(r.service);
}

/// Pairings and hints are per machine (§7.5): a backup never carries a `cast_*` row.
#[test]
fn cast_rows_never_reach_a_backup() {
    let store = Store::open_temp("tuner-cast-service-backup", None).unwrap();
    let id = tv_id();
    let pairing_id = store
        .writer()
        .blocking(move |c| {
            c.execute("INSERT INTO source(name, kind, url, created_at) VALUES ('S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0)", [])?;
            let identity = tuner::store::cast::ensure_identity(c, 1)?;
            assert_eq!(tuner::store::cast::ensure_identity(c, 2)?.pairing_id, identity.pairing_id, "once per install");
            tuner::store::cast::upsert_device(c, &StoredDevice::seen(id, Protocol::AirPlay, "Living Room"))?;
            let peer = PeerCredentials { peer_id: "tv-pairing-id".into(), peer_ltpk: [9; 32], variant: PairVariant::Hkp5 };
            tuner::store::cast::set_paired(c, id, &peer, 2)?;
            tuner::store::cast::force_convert(c, &item(ItemKind::Channel, "ch:a"), id, 3)?;
            Ok(identity.pairing_id)
        })
        .unwrap();
    let opts = tuner::store::backup::BackupOptions { include_credentials: true, include_history: true };
    let backup = store.writer().blocking(move |c| tuner::store::backup::export(c, opts, None, 4)).unwrap();
    let json = serde_json::to_string(&backup).unwrap();
    for needle in [id.to_string(), pairing_id, "Living Room".into(), "tv-pairing-id".into(), "cast_".into()] {
        assert!(!json.contains(&needle), "{needle} leaked into the backup");
    }
}
