//! One task per cast (DESIGN §40 §5.2, §8.5, §9.4): drives a `receiver::Backend` and a
//! `relay::RelaySession` from `player::remote::RemoteCmd`s, emits `RemoteEvent`s, escalates one
//! level (L2 → L3) once, runs the 10 s fetch watchdog (no playlist fetch after a 2xx insert →
//! `Firewall` when detection says Blocked/Unknown, else `ReceiverFailed`), and adds the item's
//! offset to every position. Also Test on TV. OWNER: CAST-6.
//!
//! ```text
//!  controller A ──RemoteCmd──▶ ┌ session task ─────────────────────────────────────────────┐
//!  service ──────────Ctl─────▶ │ connect: resolve (2 s mDNS refresh) → relay (LAN server)   │
//!                              │          → receiver (pair-verify, SETUP …) → sleep lock     │
//!                              │ Load:    cached hint → relay.load (L2/L3) → insert/replace │
//!                              │          → watchdogs (10 s fetch, 15 s stall)              │
//!  controller A ◀─RemoteEvent─ │ events:  receiver + relay → RemoteEvent (absolute times)   │
//!                              └────────────────────────────────────────────────────────────┘
//! ```
//! * **The session outlives zaps** (§7.3): every `Load` after the first is a replace on the same
//!   AirPlay session; a failed replace tears the receiver down and runs the whole start again
//!   (pair-verify … insert), which is the proven path. Re-pairing is never needed for a zap.
//! * **Long work is interruptible**: while the relay prepares a generation or the receiver
//!   connects, a `Close`, `Stop` or newer `Load` from the controller wins (rapid zaps coalesce);
//!   other commands wait their turn.
//! * **The lease**: the relay's `UpstreamClosed` releases picture A's lease in the controller, so
//!   a teardown forwards every relay event still queued and, if the upstream of a generation
//!   was opened but never reported closed, says so itself — before any `Lost`, after which the
//!   controller stops listening.
//! * **A reload under the same generation** (escalation, Convert, a VOD seek past what's
//!   produced, a live resume) closes the provider and opens it again, as an mpv seek does
//!   without an END_FILE: the old feed's `UpstreamClosed` and the new feed's `UpstreamOpened`
//!   are kept from the controller, which holds the lease across ([`Reopen`]). Within one
//!   generation the controller sees one `UpstreamClosed`, and it is final (`RemoteAdapter`
//!   reads a close before EOF as the upstream dropping).
//! * **Escalation** (§8.3) is one step per item: the TV fetched segments but never played
//!   within 15 s, the TV reported a failure, or the playlist guard tripped. L3 then replaces the
//!   item under the same generation; what worked is cached in `cast_hint` at the first picture.
//!   A surround item the TV doesn't play is first tried again in stereo (§8.4), which costs no
//!   escalation; a segment that outgrew the pinned target restarts the item where the TV is
//!   (§8.1.4), at the same level.
//! * **Positions** (§7.3): the receiver reports positions in its current playlist; the item's
//!   offset (a `-ss` restart or a playlist view's first segment) is added here, so the
//!   controller's snapshot, progress and `finished` stay true to the film. A full-length item
//!   (§8.5) is the whole film from 0 whatever its start (`EXT-X-START` places the TV): its offset
//!   is 0, and a seek, the TV's own scrub included, is the TV's position as it says it.
//!   A TV that ignores `EXT-X-START` (`StartIgnored`) gets a view that starts there instead. The receiver reports them on its own
//!   (AirPlay: `playbackInfo` every 10 s while VOD or catch-up plays, `Client::next_event`); the
//!   session asks once more before a stop, and before every reload of the item under the same
//!   generation ([`Session::here`]): a reload continues where the viewer is, not at the last 10 s
//!   report, and never at the cast's first start.
//! * **Test on TV** (§3.5, §8.5) runs the same machinery on the relay's lavfi test card, with no
//!   provider and no controller: it passes at the TV's first `playing`.
//! * **Field logs** (§7.6): one INFO line when the session ends and one when an item ends, each
//!   with why (the player closed it, another cast took over, shutdown; replaced by the next item,
//!   stopped, played to its end, failed, lost or stopped on the TV).

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::firewall::{self, AllowOutcome, Detection, FirewallEnv, FirewallState, Rule, Verb};
use super::inhibit::SleepInhibitor;
use super::receiver::{Backend, LoadReq, ReceiverEvent, ReceiverInfo};
use super::relay::{LoadSpec, Prepared, RelayConfig, RelayEvent, RelaySession};
use super::service::Shared;
use super::{CastErrorKind, CastHints, CastLevel, CastPhase, Credentials, Device, Protocol, ReceiverKind, airplay};
use crate::model::{Playable, PlayableKind};
use crate::player::PlayerErrorKind;
use crate::player::remote::{RemoteCmd, RemoteEvent};
use crate::redact::Secret;

/// The design's waits (DESIGN §40); tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// After a 2xx insert, the TV must ask for the playlist within this (§9.4 end-to-end check;
    /// the Apple TV fetches within seconds [NE B.2]).
    pub fetch_watchdog: Duration,
    /// Segments fetched but no `playing` after this: one-step escalation (§8.3).
    pub stall: Duration,
    /// The mDNS refresh before casting to a remembered TV (§6).
    pub refresh: Duration,
    /// The first-run pass's browse (§3.5).
    pub pass: Duration,
    /// A browse keeps running this long after the last `CastApi::browse` call (the picker and
    /// Settings ▸ Cast call it every frame while open).
    pub linger: Duration,
    /// Test on TV gives the TV this long to show the card (foxCast's and the controller's 30 s
    /// load timeout), then lets the 15 s card play out.
    pub test_load: Duration,
    pub test_card: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            fetch_watchdog: Duration::from_secs(10),
            stall: Duration::from_secs(15),
            refresh: Duration::from_secs(2),
            pass: Duration::from_secs(5),
            linger: Duration::from_secs(3),
            test_load: Duration::from_secs(30),
            test_card: Duration::from_secs(15),
        }
    }
}

/// Stopping the TV at shutdown (§5.6: fire and forget within 300 ms).
const FAST_STOP: Duration = Duration::from_millis(300);
/// Stopping the TV otherwise (setRate 0 + TEARDOWN take milliseconds on a LAN; a dead TV must
/// not hold up Play here).
const STOP: Duration = Duration::from_secs(2);
/// A relay that won't stop in this long is left to its `kill_on_drop` children.
const RELAY_STOP: Duration = Duration::from_secs(3);
/// The final `playbackInfo` before Play here / Stop.
const FINAL_POSITION: Duration = Duration::from_secs(1);
/// A live pause longer than the sliding window (20 × 4 s segments, "about 80 s", §11.3)
/// resumes by reloading at the live point.
const LIVE_WINDOW: Duration = Duration::from_secs(80);
/// Relay events forwarded at most per teardown drain.
const DRAIN_MAX: usize = 256;

// ------------------------------------------------------------------ links

/// What a session reaches out to: the receiver client, the relay and the firewall. Production
/// uses [`RealLinks`] ([`NullLinks`] off the desktop); `tests/cast_service.rs` scripts them with
/// `Backend::Fake` and [`Relay::Fake`].
pub trait Links: Send + Sync + 'static {
    /// Connect and verify (`Backend::connect`).
    fn connect(&self, device: &Device, creds: &Credentials) -> BoxFuture<'static, Result<Backend, CastErrorKind>>;
    /// Bind the relay's LAN server (`RelaySession::start`).
    fn relay(&self, cfg: RelayConfig) -> BoxFuture<'static, Result<Relay, CastErrorKind>>;
    /// Firewall detection toward `toward` for `port` (`firewall::detect`; no root).
    fn detect(&self, port: u16, toward: Ipv4Addr) -> BoxFuture<'static, Detection>;
    /// **Allow on firewall** / **Remove rule** through pkexec (`firewall::run`).
    fn firewall(&self, verb: Verb, rule: Rule, cancel: CancellationToken) -> BoxFuture<'static, AllowOutcome>;
}

/// The real receiver, relay and firewall.
#[derive(Debug, Default)]
pub struct RealLinks {
    pub env: FirewallEnv,
}

impl Links for RealLinks {
    fn connect(&self, device: &Device, creds: &Credentials) -> BoxFuture<'static, Result<Backend, CastErrorKind>> {
        let (device, creds) = (device.clone(), creds.clone());
        Box::pin(async move { Backend::connect(&device, &creds).await })
    }

    fn relay(&self, cfg: RelayConfig) -> BoxFuture<'static, Result<Relay, CastErrorKind>> {
        Box::pin(async move { RelaySession::start(cfg).await.map(|r| Relay::Session(Box::new(r))) })
    }

    fn detect(&self, port: u16, toward: Ipv4Addr) -> BoxFuture<'static, Detection> {
        let env = self.env.clone();
        Box::pin(async move { firewall::detect(&env, port, toward).await })
    }

    fn firewall(&self, verb: Verb, rule: Rule, cancel: CancellationToken) -> BoxFuture<'static, AllowOutcome> {
        let env = self.env.clone();
        Box::pin(async move { firewall::run(&env, verb, &rule, cancel).await })
    }
}

/// Off the desktop: no TV, no LAN server, no `systemctl`, no `pkexec` (DESIGN §40 §14.9).
#[derive(Debug, Default)]
pub struct NullLinks;

impl Links for NullLinks {
    fn connect(&self, _device: &Device, _creds: &Credentials) -> BoxFuture<'static, Result<Backend, CastErrorKind>> {
        Box::pin(async { Err(CastErrorKind::Unreachable) })
    }

    fn relay(&self, _cfg: RelayConfig) -> BoxFuture<'static, Result<Relay, CastErrorKind>> {
        Box::pin(async { Err(CastErrorKind::Unsupported) })
    }

    fn detect(&self, port: u16, toward: Ipv4Addr) -> BoxFuture<'static, Detection> {
        let unknown = FirewallState::Unknown { why: firewall::UnknownWhy::Unreadable };
        Box::pin(async move { Detection { state: unknown, toward, ufw: false, pkexec: false, net: None, tagged: Vec::new(), port } })
    }

    fn firewall(&self, _verb: Verb, _rule: Rule, _cancel: CancellationToken) -> BoxFuture<'static, AllowOutcome> {
        Box::pin(async { AllowOutcome::NotAuthorized })
    }
}

/// This computer's address toward `peer` (the UDP `connect()` trick: the kernel picks the
/// route, no packet is sent; [NE] confirmed it matches the Apple TV's echoed `senderAddress`).
/// The relay binds only here, never `0.0.0.0`.
pub fn local_toward(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((peer, airplay::CONTROL_PORT)).ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(_) => None,
    }
}

// ------------------------------------------------------------------ relay dispatch

/// The relay a session drives: enum dispatch like `receiver::Backend` (no `dyn` async trait).
/// The real session is boxed (as `airplay::Client` is) so the enum stays small.
pub enum Relay {
    Session(Box<RelaySession>),
    /// Scripted (session tests).
    #[doc(hidden)]
    Fake(FakeRelay),
}

impl Relay {
    pub async fn load(&mut self, spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        match self {
            Relay::Session(r) => r.load(spec).await,
            Relay::Fake(f) => f.load(spec),
        }
    }

    pub async fn view(&mut self, generation: u64, at_s: f64) -> Result<Option<Prepared>, CastErrorKind> {
        match self {
            Relay::Session(r) => r.view(generation, at_s).await,
            Relay::Fake(f) => f.view(generation, at_s),
        }
    }

    /// How far a catch-up seek can go (`RelaySession::produced_s`).
    pub async fn produced_s(&mut self) -> Option<f64> {
        match self {
            Relay::Session(r) => r.produced_s().await,
            Relay::Fake(f) => Some(f.state.lock().unwrap_or_else(|e| e.into_inner()).produced_s),
        }
    }

    pub async fn test_card(&mut self, generation: u64) -> Result<Prepared, CastErrorKind> {
        match self {
            Relay::Session(r) => r.test_card(generation).await,
            Relay::Fake(f) => f.test_card(generation),
        }
    }

    /// Cancel-safe.
    pub async fn next_event(&mut self) -> RelayEvent {
        match self {
            Relay::Session(r) => r.next_event().await,
            Relay::Fake(f) => match f.events.recv().await {
                Some(ev) => ev,
                None => std::future::pending().await,
            },
        }
    }

    pub async fn stop(&mut self, fast: bool) {
        match self {
            Relay::Session(r) => r.stop(fast).await,
            Relay::Fake(f) => f.state.lock().unwrap_or_else(|e| e.into_inner()).calls.push(RelayCall::Stop { fast }),
        }
    }
}

/// What a [`FakeRelay`] was asked, in order.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq)]
pub enum RelayCall {
    Load { generation: u64, force: Option<CastLevel>, start_at: Option<f64>, hints: CastHints, stereo: bool },
    View { generation: u64, at_s: f64 },
    TestCard { generation: u64 },
    Stop { fast: bool },
}

/// The fake relay's script and record.
#[doc(hidden)]
#[derive(Debug)]
pub struct FakeRelayState {
    pub calls: Vec<RelayCall>,
    /// The next loads fail with these, in order.
    pub fail_next: Vec<CastErrorKind>,
    /// Views answer for targets up to this many seconds into the item (what's "produced").
    pub produced_s: f64,
    /// Segment length (a view starts at the segment holding its target).
    pub segment_s: f64,
    /// Loads that may make surround renditions say they did (`Prepared::surround`).
    pub surround: bool,
}

impl Default for FakeRelayState {
    fn default() -> Self {
        Self { calls: Vec::new(), fail_next: Vec::new(), produced_s: 0.0, segment_s: 4.0, surround: false }
    }
}

/// A scripted relay: answers loads with a token URL, records calls, emits what the test pushes.
#[doc(hidden)]
pub struct FakeRelay {
    state: Arc<Mutex<FakeRelayState>>,
    events: mpsc::UnboundedReceiver<RelayEvent>,
}

/// The test's side of a [`FakeRelay`].
#[doc(hidden)]
#[derive(Clone)]
pub struct FakeRelayRemote {
    state: Arc<Mutex<FakeRelayState>>,
    events: mpsc::UnboundedSender<RelayEvent>,
}

impl FakeRelay {
    pub fn new() -> (Self, FakeRelayRemote) {
        let state = Arc::new(Mutex::new(FakeRelayState::default()));
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { state: state.clone(), events: rx }, FakeRelayRemote { state, events: tx })
    }

    fn record(&self, c: RelayCall) -> Result<(), CastErrorKind> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.calls.push(c);
        if s.fail_next.is_empty() { Ok(()) } else { Err(s.fail_next.remove(0)) }
    }

    fn prepared(&self, live: bool, offset_s: f64, level: CastLevel, surround: bool) -> Prepared {
        let n = self.state.lock().unwrap_or_else(|e| e.into_inner()).calls.len();
        Prepared {
            load: LoadReq { url: Secret::new(format!("http://192.0.2.1:47800/c/fake{n}/index.m3u8")), live, offset_s, fmp4: false },
            level,
            behind_live_s: live.then_some(15),
            surround,
            as_is: false,
        }
    }

    fn load(&self, spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        let live = spec.playable.kind.is_live();
        let start_at = spec.playable.start_at;
        self.record(RelayCall::Load { generation: spec.generation, force: spec.force, start_at, hints: spec.hints, stereo: spec.stereo })?;
        let offset = if live { 0.0 } else { start_at.unwrap_or(0.0) };
        let surround = !spec.stereo && self.state.lock().unwrap_or_else(|e| e.into_inner()).surround;
        // the fake's format never needs converting: a forced L3 is all that converts it
        let level = spec.force.unwrap_or(CastLevel::Repackaged);
        Ok(Prepared { as_is: level == CastLevel::Converted, ..self.prepared(live, offset, level, surround) })
    }

    fn view(&self, generation: u64, at_s: f64) -> Result<Option<Prepared>, CastErrorKind> {
        self.record(RelayCall::View { generation, at_s })?;
        let (produced, seg) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.produced_s, s.segment_s)
        };
        Ok((at_s <= produced).then(|| self.prepared(false, (at_s / seg).floor() * seg, CastLevel::Repackaged, false)))
    }

    fn test_card(&self, generation: u64) -> Result<Prepared, CastErrorKind> {
        self.record(RelayCall::TestCard { generation })?;
        Ok(self.prepared(false, 0.0, CastLevel::Converted, false))
    }
}

impl FakeRelayRemote {
    /// Send an event as the relay would.
    pub fn emit(&self, ev: RelayEvent) {
        let _ = self.events.send(ev);
    }

    /// Change the script / read the record.
    pub fn with<T>(&self, f: impl FnOnce(&mut FakeRelayState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

// ------------------------------------------------------------------ the session

/// From the service to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ctl {
    /// **Convert this channel**: L3 for what plays now (a replace, same generation).
    Convert,
    /// **Send as is**: forget the item's remembered conversion on this TV and, when that is
    /// all that converts it, load it again without (a replace, same generation).
    AsIs,
    /// Test on TV: play the relay's test card.
    TestCard,
    /// The service ends the session (another cast took over, or shutdown).
    Close { fast: bool },
}

/// Where session events go: picture A's controller (crossbeam), or the Test on TV task.
pub(crate) type Emit = Arc<dyn Fn(RemoteEvent) + Send + Sync>;

/// What a load plays.
#[derive(Clone)]
enum Source {
    Item { playable: Arc<Playable>, hints: CastHints },
    TestCard,
}

/// One generation to start.
#[derive(Clone)]
struct Job {
    generation: u64,
    source: Source,
    /// A level to force on top of the cached hint (escalation, Convert this channel).
    force: Option<CastLevel>,
    /// The one escalation step is used up.
    escalated: bool,
    /// A seek's restart (the card says "Seeking…", not "Starting…").
    seeking: bool,
    /// Stereo only (the TV didn't play the surround renditions).
    stereo: bool,
}

impl Job {
    fn item(generation: u64, playable: Arc<Playable>, hints: CastHints) -> Self {
        Self { generation, source: Source::Item { playable, hints }, force: None, escalated: false, seeking: false, stereo: false }
    }

    /// `c`'s item again from `at` (see [`Current::again`]), at its level and settings.
    fn again(c: &Current, at: Option<f64>) -> Self {
        Self { generation: c.generation, source: c.again(at), force: Some(c.level), escalated: c.escalated, seeking: false, stereo: c.stereo }
    }
}

/// The generation the receiver is on.
struct Current {
    generation: u64,
    source: Source,
    level: CastLevel,
    escalated: bool,
    /// The TV got surround renditions (§8.4); `stereo`: this generation was asked for none.
    surround: bool,
    stereo: bool,
    /// L3 only because it was forced (`relay::Prepared::as_is`): **Send as is** applies.
    as_is: bool,
    /// Seconds into the item where the TV's playlist starts.
    offset_s: f64,
    live: bool,
    duration: Option<f64>,
    /// The first picture came (the controller saw `Playing`).
    playing: bool,
    paused: Option<Instant>,
    buffering: bool,
    seeking: bool,
    fetched_playlist: bool,
    fetched_segments: bool,
    /// The provider's last HTTP status for this generation.
    http: Option<u16>,
    /// What the probe found (cached in `cast_hint` at the first picture).
    probed: CastHints,
    /// Absolute, last reported.
    position: Option<f64>,
    watchdog: Option<Instant>,
    stall: Option<Instant>,
}

impl Current {
    fn playable(&self) -> Option<&Arc<Playable>> {
        match &self.source {
            Source::Item { playable, .. } => Some(playable),
            Source::TestCard => None,
        }
    }

    /// The item to load again (escalation, Convert, a target overflow, a live resume, a VOD
    /// seek): from `at` seconds for VOD and catch-up (`-ss`; [`Session::here`] for "where the TV
    /// is"), at the live point for live.
    fn again(&self, at: Option<f64>) -> Source {
        match &self.source {
            Source::Item { playable, hints } => {
                let mut p = (**playable).clone();
                if !self.live {
                    p.start_at = at.or(self.position).or(p.start_at);
                }
                Source::Item { playable: Arc::new(p), hints: hints.clone() }
            }
            Source::TestCard => Source::TestCard,
        }
    }

    fn can_escalate(&self) -> bool {
        !self.escalated && self.level == CastLevel::Repackaged && matches!(self.source, Source::Item { .. })
    }

    fn deadline(&self) -> Option<Instant> {
        [self.watchdog, self.stall].into_iter().flatten().min()
    }
}

/// How starting a generation went wrong.
enum Trouble {
    /// The session is gone (connect, verify): `Lost`.
    Lost(CastErrorKind),
    /// This item didn't load (the session lives on): `Failed`.
    Failed(PlayerErrorKind),
}

/// One cast session (see the module docs).
pub(crate) struct Session {
    shared: Arc<Shared>,
    device: Device,
    creds: Credentials,
    emit: Emit,
    backend: Option<Backend>,
    /// Something is in the receiver's play queue (the next item is a replace).
    loaded: bool,
    relay: Option<Relay>,
    inhibitor: Option<SleepInhibitor>,
    cur: Option<Current>,
    phase: CastPhase,
    /// A generation whose upstream opened and hasn't been reported closed.
    upstream: Option<u64>,
    /// A reload under the generation whose upstream is open.
    reopen: Option<Reopen>,
}

/// Reloads under the same generation (see the module docs). Each one ends a feed and opens
/// the next (the relay stops the old before it opens the new, §8.5), so each keeps one close and
/// one open from the controller; a close kept is held until its successor opens. A close with
/// nothing left to keep (the new feed failed before it opened) is reported and ends this, as
/// does [`Session::end_reopen`] when a reload fails or is superseded. Counted, because the
/// relay's events may still be queued when the next reload starts (a second seek).
#[derive(Debug, Clone, Copy)]
struct Reopen {
    generation: u64,
    closes: u32,
    opens: u32,
    /// The last close kept whose successor hasn't opened.
    held: Option<std::time::Instant>,
}

impl Session {
    pub(crate) fn new(shared: Arc<Shared>, device: Device, creds: Credentials, emit: Emit) -> Self {
        Self {
            shared,
            device,
            creds,
            emit,
            backend: None,
            loaded: false,
            relay: None,
            inhibitor: None,
            cur: None,
            phase: CastPhase::Connecting,
            upstream: None,
            reopen: None,
        }
    }

    /// The session task. `after`: the session (or Test on TV) this one replaces, which must have
    /// let go of the TV and the media port first.
    pub(crate) async fn run(
        mut self,
        mut cmds: UnboundedReceiver<RemoteCmd>,
        mut ctl: UnboundedReceiver<Ctl>,
        after: Option<tokio::task::JoinHandle<()>>,
    ) {
        let stop = self.shared.stop.clone();
        let mut deferred = VecDeque::new();
        if let Some(prev) = after {
            let _ = prev.await;
        }
        // connect at once, so the TV is ready when the controller's first Load arrives (which
        // waits its turn: it needs this connection)
        match interruptible(&mut cmds, &mut deferred, &stop, false, self.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(kind)) => self.lost(kind).await,
            Err(cmd) => deferred.push_front(cmd),
        }
        loop {
            let deadline = self.cur.as_ref().and_then(Current::deadline);
            let next = match deferred.pop_front() {
                Some(cmd) => Next::Cmd(Some(cmd)),
                None => tokio::select! {
                    biased;
                    _ = stop.cancelled() => Next::Cmd(Some(RemoteCmd::Close { fast: true })),
                    c = cmds.recv() => Next::Cmd(c),
                    Some(c) = ctl.recv() => Next::Ctl(c),
                    ev = relay_event(&mut self.relay) => Next::Relay(ev),
                    ev = receiver_event(&mut self.backend) => Next::Receiver(ev),
                    () = sleep_until(deadline) => Next::Timer,
                },
            };
            let job = match next {
                Next::Cmd(None) => {
                    // the controller dropped its output without a Close
                    self.close(false, "the player let go").await;
                    break;
                }
                Next::Cmd(Some(RemoteCmd::Close { fast })) | Next::Ctl(Ctl::Close { fast }) => {
                    if self.phase != CastPhase::Lost {
                        self.shared.clear_error();
                    }
                    let why = match next {
                        _ if stop.is_cancelled() => "shutdown",
                        Next::Ctl(_) => "another cast took over",
                        _ => "the player closed it",
                    };
                    self.close(fast, why).await;
                    break;
                }
                Next::Cmd(Some(cmd)) => self.command(cmd, &mut deferred).await,
                Next::Ctl(Ctl::Convert) => self.convert().await,
                Next::Ctl(Ctl::AsIs) => self.as_is().await,
                Next::Ctl(Ctl::TestCard) => {
                    Some(Job { generation: 0, source: Source::TestCard, force: None, escalated: true, seeking: false, stereo: false })
                }
                Next::Relay(ev) => self.relay_event(ev).await,
                Next::Receiver(ev) => self.receiver_event(ev).await,
                Next::Timer => self.timer().await,
            };
            if let Some(job) = job {
                self.start(job, &mut cmds, &mut deferred, &stop).await;
            }
        }
    }

    fn send(&self, ev: RemoteEvent) {
        (self.emit)(ev);
    }

    fn set_phase(&mut self, phase: CastPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.send(RemoteEvent::Phase(phase));
        }
    }

    fn fail(&mut self, generation: u64, kind: PlayerErrorKind) {
        tracing::info!("cast session {}: the item failed ({kind:?})", self.device.id.short());
        if let PlayerErrorKind::Cast(k) = kind {
            self.shared.note_error(k);
        }
        if let Some(c) = self.cur.as_mut().filter(|c| c.generation == generation) {
            (c.watchdog, c.stall) = (None, None);
        }
        self.send(RemoteEvent::Failed { generation, kind });
    }

    /// The session is gone: release everything (forwarding the upstream's close first), then
    /// say so. It stays alive for the controller's `Close` or a new `Load` (which connects
    /// again).
    async fn lost(&mut self, kind: CastErrorKind) {
        match kind {
            CastErrorKind::StoppedOnTv => tracing::info!("cast session {}: ended (stopped on the TV)", self.device.id.short()),
            kind => tracing::info!("cast session {}: lost ({})", self.device.id.short(), kind.reason()),
        }
        self.shared.note_error(kind);
        self.teardown(true).await;
        self.set_phase(CastPhase::Lost);
        self.send(RemoteEvent::Lost(kind));
    }

    // ---- connecting

    /// Make sure the relay listens and the receiver is verified (and the sleep lock is held).
    async fn connect(&mut self) -> Result<(), CastErrorKind> {
        if self.backend.is_some() && self.relay.is_some() {
            return Ok(());
        }
        self.set_phase(CastPhase::Connecting);
        let addr = self.shared.resolve(self.device.id).await.ok_or(CastErrorKind::Unreachable)?;
        // IPv4 only in Phase 1
        let IpAddr::V4(peer) = addr.ip() else { return Err(CastErrorKind::Unreachable) };
        self.device = self.device.clone().at(addr);
        if self.device.protocol == Protocol::Dlna {
            self.device.dlna = self.shared.dlna_control(self.device.id, peer).await;
        }
        if self.relay.is_none() {
            let bind = local_toward(peer).ok_or(CastErrorKind::Unreachable)?;
            // detection re-runs at each session start (§9.4); the watchdog asks again
            self.shared.refresh_firewall(Some(peer));
            let relay = self.shared.links.relay(self.shared.relay_config(bind, peer, self.device.protocol)).await?;
            self.relay = Some(relay);
        }
        if self.backend.is_none() {
            let backend = self.shared.links.connect(&self.device, &self.creds).await?;
            let info = self.shared.learned(self.device.id, &backend.info());
            log_receiver(&self.device, &info);
            self.backend = Some(backend);
            self.loaded = false;
        }
        if self.inhibitor.is_none() && self.shared.config().cast.keep_awake {
            self.inhibitor = SleepInhibitor::take(&self.device.name).await;
        }
        Ok(())
    }

    /// Stop the TV and the relay; the controller hears every upstream close.
    async fn teardown(&mut self, fast: bool) {
        if let Some(mut b) = self.backend.take() {
            let _ = tokio::time::timeout(if fast { FAST_STOP } else { STOP }, b.stop()).await;
        }
        if let Some(mut r) = self.relay.take() {
            let _ = tokio::time::timeout(RELAY_STOP, r.stop(fast)).await;
            for _ in 0..DRAIN_MAX {
                let Some(ev) = r.next_event().now_or_never() else { break };
                self.forward_relay(ev);
            }
        }
        self.reopen = None;
        if let Some(generation) = self.upstream.take() {
            self.send(RemoteEvent::UpstreamClosed { generation, at: std::time::Instant::now() });
        }
        self.inhibitor = None;
        self.loaded = false;
        self.cur = None;
    }

    async fn close(&mut self, fast: bool, why: &str) {
        tracing::info!("cast session {}: ended ({why})", self.device.id.short());
        if !fast {
            self.final_position().await;
        }
        self.teardown(fast).await;
    }

    /// Where the TV is in the item now (absolute; `None` for live): asked now (`playbackInfo`,
    /// else the receiver's estimate, which errs behind) once the item played, else where its
    /// playlist starts (it never played: it starts there again). Reloads under the same
    /// generation start here.
    async fn here(&mut self) -> Option<f64> {
        let c = self.cur.as_ref().filter(|c| !c.live)?;
        let (offset, playing, last) = (c.offset_s, c.playing, c.position);
        if playing
            && let Some(b) = self.backend.as_mut()
            && let Ok(Some(rel)) = tokio::time::timeout(FINAL_POSITION, b.position()).await
        {
            return Some(rel + offset);
        }
        Some(if playing { last.unwrap_or(offset) } else { offset })
    }

    /// One last `playbackInfo` before Play here / Stop, so the computer resumes where the TV was.
    async fn final_position(&mut self) {
        let (Some(b), Some(c)) = (self.backend.as_mut(), self.cur.as_ref()) else { return };
        if c.live || !c.playing {
            return;
        }
        if let Ok(Some(pos)) = tokio::time::timeout(FINAL_POSITION, b.position()).await {
            let (generation, pos, dur) = (c.generation, pos + c.offset_s, c.duration);
            self.send(RemoteEvent::Position { generation, pos, dur });
        }
    }

    // ---- loading

    /// Start `job`; a newer `Load` from the controller supersedes it (zaps coalesce), `Close`
    /// and `Stop` end it.
    async fn start(&mut self, mut job: Job, cmds: &mut UnboundedReceiver<RemoteCmd>, deferred: &mut VecDeque<RemoteCmd>, stop: &CancellationToken) {
        loop {
            match interruptible(cmds, deferred, stop, true, self.play(&job)).await {
                Ok(()) => return,
                Err(RemoteCmd::Load { generation, playable, hints }) => {
                    // seeks that waited for the superseded item don't apply to the new one
                    deferred.retain(|c| !matches!(c, RemoteCmd::SeekTo(_)));
                    job = Job::item(generation, playable, hints);
                }
                Err(other) => {
                    deferred.push_front(other);
                    return;
                }
            }
        }
    }

    /// Prepare `job` on the relay and put it on the TV (insert, or replace on the same session).
    async fn play(&mut self, job: &Job) {
        let (force, hints) = match &job.source {
            Source::Item { playable, hints } => self.hinted(playable, hints).await,
            Source::TestCard => (None, CastHints::default()),
        };
        let force = job.force.or(force);
        if let Err(kind) = self.connect().await {
            self.lost(kind).await;
            return;
        }
        if self.reopen.is_some_and(|r| r.generation != job.generation) {
            self.end_reopen();
        }
        if self.cur.as_ref().is_some_and(|c| c.generation != job.generation) {
            tracing::info!("cast session {}: the item ended (replaced by the next one)", self.device.id.short());
        }
        let same = self.cur.as_ref().filter(|c| c.generation == job.generation);
        let (playing, position) = (same.is_some_and(|c| c.playing), same.and_then(|c| c.position));
        if same.is_some() {
            let r = self.reopen.get_or_insert(Reopen { generation: job.generation, closes: 0, opens: 0, held: None });
            (r.closes, r.opens) = (r.closes + 1, r.opens + 1);
        }
        let (live, duration) = match &job.source {
            Source::Item { playable, .. } => (playable.kind.is_live(), playable.duration_hint),
            Source::TestCard => (false, None),
        };
        self.cur = Some(Current {
            generation: job.generation,
            source: job.source.clone(),
            level: force.unwrap_or(CastLevel::Repackaged),
            escalated: job.escalated || force == Some(CastLevel::Converted),
            surround: false,
            stereo: job.stereo,
            as_is: false,
            offset_s: 0.0,
            live,
            duration,
            playing,
            paused: None,
            buffering: false,
            seeking: job.seeking,
            fetched_playlist: false,
            fetched_segments: false,
            http: None,
            probed: CastHints::default(),
            position,
            watchdog: None,
            stall: None,
        });
        self.set_phase(if job.seeking { CastPhase::Seeking } else { CastPhase::Starting });
        let relay = self.relay.as_mut().expect("connected");
        let prepared = match &job.source {
            Source::Item { playable, .. } => {
                let spec = LoadSpec {
                    generation: job.generation,
                    playable: playable.clone(),
                    hints,
                    receiver: self.device.kind,
                    model: self.device.model.clone(),
                    force,
                    stereo: job.stereo,
                };
                relay.load(spec).await
            }
            Source::TestCard => relay.test_card(job.generation).await,
        };
        let prepared = match prepared {
            Ok(p) => p,
            Err(kind) => {
                // the provider's status, if the relay saw one, is the real reason (§11.2)
                self.drain_relay();
                self.end_reopen();
                let http = self.cur.as_ref().and_then(|c| c.http).filter(|s| *s >= 400);
                let failure = match http {
                    Some(status) => crate::player::state::classify(-13, Some(status)),
                    None => PlayerErrorKind::Cast(kind),
                };
                self.fail(job.generation, failure);
                return;
            }
        };
        if let Some(c) = self.cur.as_mut() {
            c.level = prepared.level;
            c.offset_s = prepared.load.offset_s;
            c.surround = prepared.surround;
            c.as_is = prepared.as_is;
        }
        self.send(RemoteEvent::Level(prepared.level));
        self.send(RemoteEvent::AsIs(prepared.as_is));
        self.send(RemoteEvent::BehindLive(prepared.behind_live_s));
        match self.insert(&prepared.load).await {
            Ok(()) => {}
            Err(Trouble::Lost(kind)) => return self.lost(kind).await,
            Err(Trouble::Failed(kind)) => return self.fail(job.generation, kind),
        }
        let t = self.shared.timings;
        if let Some(c) = self.cur.as_mut().filter(|c| !c.playing) {
            if !c.fetched_playlist {
                c.watchdog = Some(Instant::now() + t.fetch_watchdog);
            }
            c.stall = Some(Instant::now() + t.stall);
        }
    }

    /// The cached hint for the item on this TV (§8.2): the level to force, and the codecs mpv
    /// didn't know.
    async fn hinted(&self, playable: &Playable, hints: &CastHints) -> (Option<CastLevel>, CastHints) {
        let mut hints = hints.clone();
        let Some(item) = playable.item.clone() else { return (None, hints) };
        let device = self.device.id;
        let now = crate::model::now();
        match self.shared.writer().run(move |c| crate::store::cast::hint(c, &item, device)).await {
            Ok(Some(h)) => {
                h.fill(&mut hints, now);
                let force = h.force(now);
                if force.is_some() {
                    let why = match (h.forced, h.irregular) {
                        (true, _) => "Convert this channel",
                        (false, true) => "its playlist was irregular here before",
                        (false, false) => "the TV failed it as is before",
                    };
                    tracing::info!("cast session {}: converting ({why})", self.device.id.short());
                }
                (force, hints)
            }
            Ok(None) => (None, hints),
            Err(e) => {
                tracing::debug!("cast hint: {e}");
                (None, hints)
            }
        }
    }

    /// Put `load` on the TV: the first item is an insert; later ones replace on the same
    /// session, and a failed replace (or a receiver that went away meanwhile) runs the whole
    /// session again (§7.3). What the TV queued about the item being replaced is dropped first:
    /// the old item's position or `playing` must not be read as the new one's.
    async fn insert(&mut self, load: &LoadReq) -> Result<(), Trouble> {
        if self.loaded && self.discard_stale() {
            tracing::info!("cast session {}: the TV went away meanwhile; starting the session again", self.device.id.short());
            self.drop_backend().await;
        }
        if self.loaded
            && let Some(b) = self.backend.as_mut()
        {
            match b.replace(load).await {
                Ok(()) => return Ok(()),
                Err(kind) => {
                    tracing::info!("cast session {}: replace failed ({}); starting the session again", self.device.id.short(), kind.reason());
                    self.drop_backend().await;
                }
            }
        }
        if self.backend.is_none() {
            self.connect().await.map_err(Trouble::Lost)?;
        }
        let b = self.backend.as_mut().expect("connected");
        b.load(load).await.map_err(|k| Trouble::Failed(PlayerErrorKind::Cast(k)))?;
        self.loaded = true;
        Ok(())
    }

    /// Drop the receiver's queued events; true when one said the session is gone.
    fn discard_stale(&mut self) -> bool {
        let Some(b) = self.backend.as_mut() else { return false };
        let mut gone = false;
        for _ in 0..DRAIN_MAX {
            match b.next_event().now_or_never() {
                Some(ReceiverEvent::Lost(_) | ReceiverEvent::StoppedOnTv) => gone = true,
                Some(_) => {}
                None => break,
            }
        }
        gone
    }

    /// TEARDOWN and close the receiver's connection (a restart follows).
    async fn drop_backend(&mut self) {
        if let Some(mut b) = self.backend.take() {
            let _ = tokio::time::timeout(STOP, b.stop()).await;
        }
        self.loaded = false;
    }

    // ---- commands

    async fn command(&mut self, cmd: RemoteCmd, deferred: &mut VecDeque<RemoteCmd>) -> Option<Job> {
        match cmd {
            // a newer Load waiting behind this one supersedes it (zaps during the connect coalesce)
            RemoteCmd::Load { .. } if deferred.iter().any(|c| matches!(c, RemoteCmd::Load { .. })) => None,
            RemoteCmd::Load { generation, playable, hints } => Some(Job::item(generation, playable, hints)),
            RemoteCmd::SetPaused(paused) => self.set_paused(paused).await,
            RemoteCmd::SeekTo(at) => self.seek(at).await,
            RemoteCmd::SetVolume(v) => {
                // Tuner's 0–130 → the receiver's 0–1 (AirPlay has no volume path: nothing)
                if let Some(b) = self.backend.as_mut() {
                    let _ = b.set_volume(v.clamp(0.0, 100.0) / 100.0).await;
                }
                None
            }
            RemoteCmd::Stop => {
                if self.cur.is_some() {
                    tracing::info!("cast session {}: the item ended (stopped)", self.device.id.short());
                }
                self.final_position().await;
                self.teardown(false).await;
                None
            }
            RemoteCmd::Close { .. } => None,
        }
    }

    async fn set_paused(&mut self, paused: bool) -> Option<Job> {
        let c = self.cur.as_ref()?;
        // a live pause longer than the window resumes at the live point (§11.3)
        if !paused && c.live && c.paused.is_some_and(|t| t.elapsed() >= LIVE_WINDOW) {
            return Some(Job::again(c, None));
        }
        let b = self.backend.as_mut()?;
        match b.set_paused(paused).await {
            Ok(()) => {
                if let Some(c) = self.cur.as_mut() {
                    c.paused = paused.then(Instant::now);
                }
                self.send(RemoteEvent::Paused(paused));
                self.set_phase(if paused { CastPhase::Paused } else { CastPhase::Playing });
            }
            Err(kind) => tracing::debug!("cast session {}: setRate: {}", self.device.id.short(), kind.reason()),
        }
        None
    }

    /// A seek (§11.3): inside what's produced, a replace with a playlist view; beyond it, VOD
    /// restarts at the target (`-ss`, a new token) and catch-up clamps to the produced edge
    /// (§8.5: it can seek only as far as it has loaded; the controller's banner says so).
    async fn seek(&mut self, at: f64) -> Option<Job> {
        let c = self.cur.as_ref()?;
        if c.live || c.playable().is_none() {
            return None;
        }
        let (generation, catchup) = (c.generation, matches!(c.playable()?.kind, PlayableKind::Catchup { .. }));
        self.set_phase(CastPhase::Seeking);
        let relay = self.relay.as_mut()?;
        let mut target = at;
        let mut view = relay.view(generation, at).await;
        if catchup
            && matches!(view, Ok(None))
            && let Some(edge) = relay.produced_s().await.filter(|edge| at > *edge)
        {
            tracing::debug!("cast session {}: catch-up seek clamped to what is loaded", self.device.id.short());
            target = edge;
            view = relay.view(generation, edge).await;
            if matches!(view, Ok(Some(_))) {
                self.send(RemoteEvent::SeekClamped { generation });
            }
        }
        match view {
            Ok(Some(view)) => {
                if let Some(c) = self.cur.as_mut() {
                    c.seeking = true;
                }
                match self.insert(&view.load).await {
                    Ok(()) => {
                        if let Some(c) = self.cur.as_mut() {
                            c.offset_s = view.load.offset_s;
                            c.position = Some(target);
                        }
                    }
                    Err(Trouble::Lost(kind)) => self.lost(kind).await,
                    Err(Trouble::Failed(kind)) => self.fail(generation, kind),
                }
                None
            }
            Ok(None) if catchup => {
                let back = if self.cur.as_ref().is_some_and(|c| c.paused.is_some()) { CastPhase::Paused } else { CastPhase::Playing };
                self.set_phase(back);
                None
            }
            Ok(None) => {
                let c = self.cur.as_ref()?;
                let force = (c.level == CastLevel::Converted).then_some(CastLevel::Converted);
                Some(Job { force, seeking: true, ..Job::again(c, Some(at)) })
            }
            Err(kind) => {
                self.fail(generation, PlayerErrorKind::Cast(kind));
                None
            }
        }
    }

    /// **Convert this channel**: remembered for the item on this TV (`cast_hint.forced`), and
    /// L3 now unless it already converts (a replace, same generation, where the TV is).
    async fn convert(&mut self) -> Option<Job> {
        let c = self.cur.as_ref()?;
        let item = c.playable()?.item.clone();
        if let Some(item) = item {
            let (device, now) = (self.device.id, crate::model::now());
            let _ = self.shared.writer().submit(move |conn| crate::store::cast::force_convert(conn, &item, device, now));
        }
        self.cur.as_ref().filter(|c| c.level != CastLevel::Converted)?;
        let at = self.here().await;
        let c = self.cur.as_ref()?;
        Some(Job { force: Some(CastLevel::Converted), escalated: true, ..Job::again(c, at) })
    }

    /// One step L2 → L3 (§8.3), remembered for the item and TV at once as an L2 failure
    /// (`store::cast::l2_failed`, under today's `L2_FAILURE_LOGIC`; `irregular`: the playlist
    /// guard's): L2's 15 s and L3's own start (a new relay load, two real-time live segments,
    /// the replace) can outlast the controller's 30 s load timeout, and its retry (a new
    /// generation) then starts converted instead of on L2 again. The first L2 picture of the item
    /// on this TV clears it again (`remember`), as does **Send as is**. It continues where the
    /// TV is.
    async fn escalate(&mut self, why: &str, irregular: bool) -> Option<Job> {
        self.cur.as_ref().filter(|c| c.can_escalate())?;
        let at = self.here().await;
        let c = self.cur.as_mut()?;
        c.escalated = true;
        let job = Job { force: Some(CastLevel::Converted), escalated: true, ..Job::again(c, at) };
        let cache = match &c.source {
            Source::Item { playable, hints } => playable.item.clone().map(|item| (item, hints.clone())),
            Source::TestCard => None,
        };
        tracing::info!("cast session {}: converting ({why})", self.device.id.short());
        if let Some((item, seen)) = cache {
            let (device, now) = (self.device.id, crate::model::now());
            let _ = self.shared.writer().submit(move |conn| crate::store::cast::l2_failed(conn, &item, device, irregular, &seen, now));
        }
        Some(job)
    }

    /// **Send as is** (§3.3, §8.3): forget why the item converts on this TV (Convert this
    /// channel, a remembered L2 failure) and, when the TV is fed L3 only for that reason
    /// ([`Current::as_is`]), load it again where the TV is as the plan alone decides, with the
    /// escalation step back: a real failure escalates (and is remembered) again, an L2
    /// picture confirms the item plays as is.
    async fn as_is(&mut self) -> Option<Job> {
        let c = self.cur.as_ref()?;
        let item = c.playable()?.item.clone();
        if let Some(item) = item {
            let device = self.device.id;
            // the writer runs in order: the reload's `hinted` reads the row after this
            let _ = self.shared.writer().submit(move |conn| crate::store::cast::clear_convert(conn, &item, device));
        }
        self.cur.as_ref().filter(|c| c.as_is)?;
        tracing::info!("cast session {}: sending as is (the conversion is forgotten)", self.device.id.short());
        let at = self.here().await;
        let c = self.cur.as_ref()?;
        Some(Job { force: None, escalated: false, ..Job::again(c, at) })
    }

    /// The TV didn't play a surround item (§8.4): the same item, level and place in stereo, once;
    /// no escalation is spent. `None` when it wasn't surround (or already stereo).
    async fn stereo_again(&mut self, why: &str) -> Option<Job> {
        self.cur.as_ref().filter(|c| c.surround && !c.stereo)?;
        tracing::info!("cast session {}: {why} with surround: again in stereo", self.device.id.short());
        let at = self.here().await;
        let c = self.cur.as_ref()?;
        Some(Job { stereo: true, ..Job::again(c, at) })
    }

    // ---- events

    fn forward_relay(&mut self, ev: RelayEvent) {
        match ev {
            RelayEvent::UpstreamOpened { generation } => {
                self.upstream = Some(generation);
                if let Some(r) = self.reopen.as_mut().filter(|r| r.generation == generation && r.opens > 0) {
                    // a reload's feed: the controller's upstream (and lease) carry over
                    (r.opens, r.held) = (r.opens - 1, None);
                    if r.opens == 0 && r.closes == 0 {
                        self.reopen = None;
                    }
                    return;
                }
                self.send(RemoteEvent::UpstreamOpened { generation });
            }
            RelayEvent::UpstreamClosed { generation, at } => {
                if let Some(r) = self.reopen.as_mut().filter(|r| r.generation == generation && r.closes > 0) {
                    (r.closes, r.held) = (r.closes - 1, Some(at));
                    return;
                }
                if self.reopen.is_some_and(|r| r.generation == generation) {
                    // a reload's feed closed before it opened (a provider failure): final
                    self.reopen = None;
                }
                if self.upstream == Some(generation) {
                    self.upstream = None;
                }
                self.send(RemoteEvent::UpstreamClosed { generation, at });
            }
            RelayEvent::Http { generation, status } => {
                if let Some(c) = self.cur.as_mut().filter(|c| c.generation == generation) {
                    c.http = Some(status);
                }
                self.send(RemoteEvent::Http { generation, status });
            }
            RelayEvent::ProviderText { generation, text } => self.send(RemoteEvent::ProviderText { generation, text }),
            RelayEvent::UpstreamEof { generation } => self.send(RemoteEvent::UpstreamEof { generation }),
            RelayEvent::Probed { generation, info } => {
                if let Some(c) = self.cur.as_mut().filter(|c| c.generation == generation) {
                    c.probed.vcodec.clone_from(&info.video_codec);
                    c.probed.acodec.clone_from(&info.audio_codec);
                    c.probed.height = info.height.and_then(|h| u32::try_from(h).ok());
                }
                self.send(RemoteEvent::StreamInfo { generation, info });
            }
            RelayEvent::PlaylistFetched { generation } => {
                if let Some(c) = self.cur.as_mut().filter(|c| c.generation == generation) {
                    c.fetched_playlist = true;
                    c.watchdog = None;
                }
            }
            RelayEvent::SegmentFetched { generation, .. } => {
                if let Some(c) = self.cur.as_mut().filter(|c| c.generation == generation) {
                    (c.fetched_playlist, c.fetched_segments, c.watchdog) = (true, true, None);
                }
            }
            RelayEvent::Irregular { .. }
            | RelayEvent::StreamEnded { .. }
            | RelayEvent::TargetOverflow { .. }
            | RelayEvent::StartIgnored { .. }
            | RelayEvent::Unindexed { .. }
            | RelayEvent::CpuTooSlow { .. }
            | RelayEvent::Failed { .. } => {}
        }
    }

    /// A reload is over without its feed opening (it failed, or a newer load superseded it):
    /// the held close is reported now. A close not seen yet is reported when it comes.
    fn end_reopen(&mut self) {
        let Some(r) = self.reopen.take() else { return };
        if let Some(at) = r.held {
            if self.upstream == Some(r.generation) {
                self.upstream = None;
            }
            self.send(RemoteEvent::UpstreamClosed { generation: r.generation, at });
        }
    }

    /// Forward what the relay already queued (after a failed load: its `Http` is the reason).
    fn drain_relay(&mut self) {
        for _ in 0..DRAIN_MAX {
            let Some(ev) = self.relay.as_mut().and_then(|r| r.next_event().now_or_never()) else { break };
            self.forward_relay(ev);
        }
    }

    async fn relay_event(&mut self, ev: RelayEvent) -> Option<Job> {
        match ev {
            RelayEvent::SegmentFetched { generation, from_s, upto_s } => {
                self.forward_relay(RelayEvent::SegmentFetched { generation, from_s, upto_s });
                if self.cur.as_ref().is_some_and(|c| c.generation == generation)
                    && let Some(b) = self.backend.as_mut()
                {
                    b.fetched(from_s, upto_s).await;
                }
                None
            }
            RelayEvent::StreamEnded { generation } => {
                // a DLNA renderer has all of the item: its next stop is the end, not the remote
                if self.cur.as_ref().is_some_and(|c| c.generation == generation)
                    && let Some(b) = self.backend.as_mut()
                {
                    b.stream_ended();
                }
                None
            }
            RelayEvent::Irregular { generation } => {
                // only real failures since da47cd9: missing IDRs, or a segment past any target
                // (a longer GOP is `TargetOverflow`); remembered by `escalate`, which only an
                // L2 generation takes (an L3 one has its keyframes forced)
                self.cur.as_ref().filter(|c| c.generation == generation)?;
                self.escalate("irregular playlist", true).await
            }
            RelayEvent::TargetOverflow { generation } => {
                // a longer GOP than the pinned target covers: the same level, where the TV is
                self.cur.as_ref().filter(|c| c.generation == generation)?;
                tracing::info!("cast session {}: a segment outgrew the target duration: restarting with a longer one", self.device.id.short());
                let at = self.here().await;
                Some(Job::again(self.cur.as_ref()?, at))
            }
            RelayEvent::StartIgnored { generation, at_s } => {
                // the TV played a full-length item from its beginning, not from where its playlist
                // said: the same place again, as a view that starts there (§8.5)
                self.cur.as_ref().filter(|c| c.generation == generation)?;
                tracing::info!(
                    "cast session {}: the TV doesn't start where the playlist says: a playlist from there instead",
                    self.device.id.short()
                );
                self.seek(at_s).await
            }
            RelayEvent::Unindexed { generation } => {
                // the item's keyframe index didn't hold: a growing playlist, where the TV is
                self.cur.as_ref().filter(|c| c.generation == generation)?;
                tracing::info!("cast session {}: the film's keyframe index didn't hold: restarting it as a growing playlist", self.device.id.short());
                let at = self.here().await;
                Some(Job::again(self.cur.as_ref()?, at))
            }
            RelayEvent::CpuTooSlow { generation } => {
                self.fail(generation, PlayerErrorKind::Cast(CastErrorKind::CpuTooSlow));
                None
            }
            RelayEvent::Failed { generation, kind } => {
                self.fail(generation, PlayerErrorKind::Cast(kind));
                None
            }
            ev => {
                self.forward_relay(ev);
                None
            }
        }
    }

    async fn receiver_event(&mut self, ev: ReceiverEvent) -> Option<Job> {
        match ev {
            ReceiverEvent::StoppedOnTv => {
                self.lost(CastErrorKind::StoppedOnTv).await;
                return None;
            }
            ReceiverEvent::Lost(kind) => {
                // the TV gave up before the first picture: that's the TV failing the format
                if kind == CastErrorKind::ReceiverFailed && self.cur.as_ref().is_some_and(|c| !c.playing) {
                    if self.cur.as_ref().is_some_and(|c| c.surround && !c.stereo) {
                        self.drop_backend().await;
                        return self.stereo_again("the TV couldn't play it").await;
                    }
                    if self.cur.as_ref().is_some_and(Current::can_escalate) {
                        self.drop_backend().await;
                        return self.escalate("the TV couldn't play it", false).await;
                    }
                }
                self.lost(kind).await;
                return None;
            }
            _ => {}
        }
        let c = self.cur.as_mut()?;
        let generation = c.generation;
        // decided under the borrow of `cur`, sent after it (phases deduplicated in order)
        let mut out = Vec::new();
        let mut first = false;
        let mut finished = false;
        match ev {
            ReceiverEvent::Loading => {
                if !c.playing && !c.seeking {
                    out.push(RemoteEvent::Phase(CastPhase::Starting));
                } else if c.playing && !c.seeking {
                    c.buffering = true;
                    out.extend([RemoteEvent::Buffering(true), RemoteEvent::Phase(CastPhase::Buffering)]);
                }
            }
            ReceiverEvent::Playing => {
                first = !c.playing;
                if c.paused.take().is_some() {
                    out.push(RemoteEvent::Paused(false));
                }
                if std::mem::take(&mut c.buffering) {
                    out.push(RemoteEvent::Buffering(false));
                }
                (c.playing, c.seeking, c.watchdog, c.stall) = (true, false, None, None);
                out.extend([RemoteEvent::Phase(CastPhase::Playing), RemoteEvent::Playing { generation }]);
            }
            ReceiverEvent::Paused => {
                c.paused.get_or_insert_with(Instant::now);
                out.extend([RemoteEvent::Paused(true), RemoteEvent::Phase(CastPhase::Paused)]);
            }
            ReceiverEvent::Buffering(on) => {
                c.buffering = on;
                let phase = match (on, c.paused.is_some(), c.playing) {
                    (true, _, _) => CastPhase::Buffering,
                    (false, true, _) => CastPhase::Paused,
                    (false, false, true) => CastPhase::Playing,
                    (false, false, false) => CastPhase::Starting,
                };
                out.extend([RemoteEvent::Buffering(on), RemoteEvent::Phase(phase)]);
            }
            ReceiverEvent::Position { pos, dur } => {
                let pos = pos + c.offset_s;
                c.position = Some(pos);
                let dur = c.duration.or(dur.map(|d| d + c.offset_s));
                out.push(RemoteEvent::Position { generation, pos, dur });
            }
            ReceiverEvent::Ended => {
                tracing::info!("cast session {}: the item played to its end", self.device.id.short());
                finished = !c.live;
                out.push(RemoteEvent::Ended { generation });
            }
            ReceiverEvent::StoppedOnTv | ReceiverEvent::Lost(_) => {}
        }
        for ev in out {
            match ev {
                RemoteEvent::Phase(p) => self.set_phase(p),
                ev => self.send(ev),
            }
        }
        if first {
            self.shared.clear_error();
            self.remember();
        }
        if finished {
            // the film is over and nothing may follow: the computer may sleep again (`connect`
            // takes the lock back for the next load, Up next's or Cast again's)
            self.inhibitor = None;
        }
        None
    }

    /// The first picture: cache what worked for this item on this TV (§8.3).
    fn remember(&self) {
        let Some(c) = self.cur.as_ref() else { return };
        let Source::Item { playable, hints } = &c.source else { return };
        let Some(item) = playable.item.clone() else { return };
        let mut seen = hints.clone();
        if seen.vcodec.is_none() {
            seen.vcodec.clone_from(&c.probed.vcodec);
        }
        if seen.acodec.is_none() {
            seen.acodec.clone_from(&c.probed.acodec);
        }
        let (device, level, now) = (self.device.id, c.level, crate::model::now());
        let _ = self.shared.writer().submit(move |conn| crate::store::cast::save_hint(conn, &item, device, level, &seen, now));
    }

    async fn timer(&mut self) -> Option<Job> {
        let now = Instant::now();
        let c = self.cur.as_mut()?;
        let generation = c.generation;
        if c.watchdog.is_some_and(|t| t <= now) {
            c.watchdog = None;
            // the TV was told to play and never asked: the firewall, unless detection says
            // the port is open (then it's the TV) — §9.4
            let toward = self.device.addr().and_then(|a| match a.ip() {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            });
            let state = self.shared.detect_now(toward).await.state;
            let kind = match state {
                FirewallState::Blocked { .. } | FirewallState::Unknown { .. } => CastErrorKind::Firewall,
                FirewallState::Allowed { .. } | FirewallState::NoFirewall => CastErrorKind::ReceiverFailed,
            };
            tracing::info!("cast session {}: the TV never fetched the playlist ({})", self.device.id.short(), kind.reason());
            self.fail(generation, PlayerErrorKind::Cast(kind));
            return None;
        }
        if c.stall.is_some_and(|t| t <= now) {
            c.stall = None;
            if !c.playing && c.fetched_segments {
                if c.surround && !c.stereo {
                    return self.stereo_again("fetched but not playing").await;
                }
                if c.can_escalate() {
                    return self.escalate("fetched but not playing", false).await;
                }
                self.fail(generation, PlayerErrorKind::Cast(CastErrorKind::ReceiverFailed));
            }
            return None;
        }
        None
    }
}

/// What woke the session.
enum Next {
    Cmd(Option<RemoteCmd>),
    Ctl(Ctl),
    Relay(RelayEvent),
    Receiver(ReceiverEvent),
    Timer,
}

/// Run `work` unless the controller supersedes it first: `Close`, `Stop` or (with `loads`) a
/// newer `Load` is returned (the work is dropped); other commands wait in `deferred`.
async fn interruptible<T>(
    cmds: &mut UnboundedReceiver<RemoteCmd>,
    deferred: &mut VecDeque<RemoteCmd>,
    stop: &CancellationToken,
    loads: bool,
    work: impl Future<Output = T>,
) -> Result<T, RemoteCmd> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Err(RemoteCmd::Close { fast: true }),
            c = cmds.recv() => match c {
                None => return Err(RemoteCmd::Close { fast: false }),
                Some(c @ (RemoteCmd::Close { .. } | RemoteCmd::Stop)) => return Err(c),
                Some(c @ RemoteCmd::Load { .. }) if loads => return Err(c),
                Some(c) => deferred.push_back(c),
            },
            r = &mut work => return Ok(r),
        }
    }
}

async fn relay_event(r: &mut Option<Relay>) -> RelayEvent {
    match r {
        Some(r) => r.next_event().await,
        None => std::future::pending().await,
    }
}

async fn receiver_event(b: &mut Option<Backend>) -> ReceiverEvent {
    match b {
        Some(b) => b.next_event().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// The one INFO line per session, without addresses (DESIGN §40 §7.6):
/// `cast: AppleTV14,1 tvOS 26.6 (23L773) AirPlay 960.13.1 pairing=hkp3 timing=ptp`, a Mac's with
/// `pairing=transient`; `cast: Chromecast Google Cast`, `cast: QE55Q60R DLNA`.
fn log_receiver(device: &Device, info: &ReceiverInfo) {
    let or = |s: &Option<String>| s.clone().unwrap_or_else(|| "?".into());
    let model = or(&info.model.clone().or_else(|| device.model.clone()));
    match device.protocol {
        Protocol::GoogleCast => tracing::info!("cast: {model} Google Cast"),
        Protocol::Dlna => tracing::info!("cast: {model} DLNA"),
        Protocol::AirPlay => {
            let os = match device.kind {
                ReceiverKind::AppleTv => "tvOS",
                ReceiverKind::Mac => "macOS",
                _ => "OS",
            };
            let pairing = match (info.pairing, device.kind) {
                (Some(p), _) => p.as_str(),
                (None, ReceiverKind::Mac) => "transient",
                (None, _) => "?",
            };
            tracing::info!("cast: {model} {os} {} AirPlay {} pairing={pairing} timing=ptp", or(&info.os_version), or(&info.source_version));
        }
    }
}

/// Test on TV (§3.5, §8.5): the relay's test card through a session of its own; the verdict is
/// the first `playing` (or why not). The card then plays out (or 15 s pass) and the session
/// closes.
pub(crate) async fn test_on_tv(session: Session, after: Option<tokio::task::JoinHandle<()>>, stop: CancellationToken) -> super::TestState {
    use super::TestState;
    let timings = session.shared.timings;
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let (ev_tx, mut events) = mpsc::unbounded_channel();
    let session = Session { emit: Arc::new(move |ev| drop(ev_tx.send(ev))), ..session };
    let task = tokio::spawn(session.run(cmd_rx, ctl_rx, after));
    let _ = ctl_tx.send(Ctl::TestCard);
    let verdict = async {
        while let Some(ev) = events.recv().await {
            match ev {
                RemoteEvent::Playing { .. } => return TestState::Passed,
                RemoteEvent::Failed { kind: PlayerErrorKind::Cast(k), .. } | RemoteEvent::Lost(k) => return TestState::Failed(k),
                RemoteEvent::Failed { .. } => return TestState::Failed(CastErrorKind::ReceiverFailed),
                _ => {}
            }
        }
        TestState::Failed(CastErrorKind::Unreachable)
    };
    let state = tokio::select! {
        _ = stop.cancelled() => TestState::Idle,
        r = tokio::time::timeout(timings.test_load, verdict) => r.unwrap_or(TestState::Failed(CastErrorKind::ReceiverFailed)),
    };
    if state == TestState::Passed {
        let ended = async {
            while let Some(ev) = events.recv().await {
                if matches!(ev, RemoteEvent::Ended { .. } | RemoteEvent::Lost(_)) {
                    return;
                }
            }
        };
        tokio::select! {
            _ = stop.cancelled() => {}
            _ = tokio::time::timeout(timings.test_card, ended) => {}
        }
    }
    let _ = ctl_tx.send(Ctl::Close { fast: stop.is_cancelled() });
    let _ = task.await;
    state
}
