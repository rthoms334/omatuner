//! The cast service (DESIGN §40 §5.6): started by `Services::start_integrations` (a failure means
//! no casting, never a crash), restarted by `Services::apply_config` on `[cast]` changes (the
//! MPRIS pattern: `.take().stop()`, then start if enabled), stopped first at shutdown (a
//! fire-and-forget stop to the TV within 300 ms; children, a waiting `pkexec`, listeners, the
//! inhibitor and the session directories go; all inside the runtime's 2 s budget). It implements
//! [`super::CastApi`] (published with `AppShared::set_cast`), which also serves picture A's
//! `player::remote::CastConnector`. OWNER: CAST-6.
//!
//! Everything runs on the `tuner-rt` runtime; the API returns at once and the UI polls (devices,
//! pairing, firewall, the first-run pass, Test on TV), as `agent_connect` does:
//! * **Devices**: a [`Registry`] fed by the store's rows at start and by discovery, which runs
//!   only on demand ([`CastApi::browse`] from the open picker or Settings ▸ Cast, lingering a
//!   few seconds after the last call; the 5 s first-run pass; a 2 s refresh before casting to a
//!   remembered TV, cut short once it answers). Usable receivers are written to `cast_device`.
//! * **Pairing**: one task per [`PairJob`] holds `/pair-pin-start` and pair-setup on one
//!   connection while the user types (§7.2), moving down the HKP 3 → HKP 5 ladder when the TV
//!   refuses HKP 3, or on **Show a new code**. The identity is created by the first pairing.
//! * **Sessions**: one at a time ([`Session`], `session.rs`); a new cast or the end of Test on
//!   TV waits until the previous one let go of the TV and the media port.
//! * **Firewall**: detection cached ≤ 5 s (re-run on Check again, after Allow / Remove and at
//!   each session start); Allow / Remove run through pkexec with a cancel token that shutdown
//!   also cancels (killing pkexec closes polkit's dialog).
//!
//! Settings: every `[cast]` value is read live (a cast picks up a change at its next load,
//! §11.3), so the service restarts only when `cast.enabled` flips; a restart for any other
//! field would end the cast that's playing.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::airplay::PairingSession;
use super::discovery::{Discovery, MdnsDiscovery, NullDiscovery, Seen};
use super::firewall::{AllowOutcome, Detection, FirewallState, Verb};
use super::receiver::ReceiverInfo;
use super::registry::Registry;
use super::relay::RelayConfig;
use super::session::{Ctl, Links, NullLinks, RealLinks, Session, Timings};
use super::{
    CastApi, CastDiagnose, CastErrorKind, CastHandle, CastPhase, CastView, Credentials, Device, DeviceId, FirewallJob, Identity, PairError, PairJob,
    PairState, PairVariant, Pairing, PassOutcome, PeerCredentials, Pin, ReceiverKind, TestState, Usability,
};
use crate::config::{Config, SharedConfig};
use crate::error::Result;
use crate::paths::AppPaths;
use crate::player::remote::RemoteOutput;
use crate::player::{PlayerError, PlayerErrorKind};
use crate::store::{Store, Writer};

/// A firewall detection is reused this long (DESIGN §40 §9.4).
const FIREWALL_CACHE: Duration = Duration::from_secs(5);
/// A TV discovery saw this recently needs no refresh before a cast.
const SEEN_FRESH: Duration = Duration::from_secs(60);
/// How long stopping waits for the session to let go (inside the runtime's 2 s budget).
const STOP_WAIT: Duration = Duration::from_secs(1);
/// Detection's target when no TV is known: the mDNS group, so the route is the LAN's.
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// The trace directory keeps this many transcripts (§13), each at most
/// [`super::airplay::trace::MAX_FILE`] (10 MiB).
pub const TRACE_KEEP: usize = 5;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the service runs with.
pub struct CastDeps {
    pub store: Store,
    pub config: SharedConfig,
    pub paths: AppPaths,
    pub discovery: Arc<dyn Discovery>,
    pub links: Arc<dyn Links>,
    pub timings: Timings,
}

impl CastDeps {
    /// Production: mDNS and the real receiver, relay and firewall; off the desktop (tests,
    /// `TUNER_NO_DESKTOP`) nothing that touches the LAN, `systemctl` or `pkexec`.
    pub fn new(store: Store, config: SharedConfig, paths: AppPaths) -> Self {
        let (discovery, links): (Arc<dyn Discovery>, Arc<dyn Links>) = if crate::platform::off_desktop() {
            (Arc::new(NullDiscovery), Arc::new(NullLinks))
        } else {
            (Arc::new(MdnsDiscovery), Arc::new(RealLinks::default()))
        };
        Self { store, config, paths, discovery, links, timings: Timings::default() }
    }
}

/// The running service (`Integrations::cast`).
pub struct CastService {
    shared: Arc<Shared>,
    stopped: bool,
}

/// The fast stop for `Services::exit_cleanup` (a signal's direct exit), cloneable.
#[derive(Clone)]
pub struct CastStopper(Arc<Shared>);

impl CastStopper {
    /// Stop the TV (≤ 300 ms), kill the children and pkexec, close the listener, wipe the
    /// session directories.
    pub async fn stop_fast(self) {
        let handles = self.0.shut();
        let _ = tokio::time::timeout(STOP_WAIT, async {
            for h in handles {
                let _ = h.await;
            }
        })
        .await;
        sweep(&self.0.paths);
    }
}

impl CastService {
    /// Create the casting directories (sweeping what a crash left) and start the service on
    /// `rt`. The remembered TVs and the identity load in the background.
    pub fn start(rt: &Handle, deps: CastDeps) -> Result<Self> {
        deps.paths.ensure_cast_dirs()?;
        sweep(&deps.paths);
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            rt: rt.clone(),
            links: deps.links,
            discovery: deps.discovery,
            store: deps.store,
            config: deps.config,
            paths: deps.paths,
            timings: deps.timings,
            stop: CancellationToken::new(),
            registry: Mutex::default(),
            found: Notify::new(),
            identity: Mutex::default(),
            browse: Mutex::default(),
            seen_tx,
            firewall: Mutex::default(),
            pairs: Mutex::default(),
            next_id: AtomicU64::new(1),
            add: Mutex::default(),
            pass: Mutex::default(),
            test: Mutex::default(),
            live: Mutex::default(),
            last: Mutex::default(),
        });
        rt.spawn(pump(shared.clone(), seen_rx));
        let this = shared.clone();
        rt.spawn(async move {
            let loaded = this.writer().run(|c| Ok((crate::store::cast::identity(c)?, crate::store::cast::devices(c)?))).await;
            match loaded {
                Ok((identity, rows)) => {
                    if let Some(id) = identity {
                        lock(&this.identity).get_or_insert(id);
                    }
                    lock(&this.registry).load(rows);
                }
                Err(e) => tracing::warn!("cast: remembered TVs: {e}"),
            }
        });
        Ok(Self { shared, stopped: false })
    }

    /// The facade the app and the agent use.
    pub fn handle(&self) -> CastHandle {
        CastHandle::new(Arc::new(Api(self.shared.clone())))
    }

    /// For `Services::exit_cleanup`.
    pub fn stopper(&self) -> CastStopper {
        CastStopper(self.shared.clone())
    }

    /// Stop everything (see the module docs); idempotent.
    pub fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        let handles = self.shared.shut();
        // off the runtime (the UI thread at shutdown or on a Settings change) the TV gets its
        // stop before the runtime goes; inside it, the tasks finish on their own
        if !handles.is_empty() && Handle::try_current().is_err() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.shared.rt.spawn(async move {
                for h in handles {
                    let _ = h.await;
                }
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(STOP_WAIT);
        }
        sweep(&self.shared.paths);
    }
}

impl Drop for CastService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Empty the session directories (live on tmpfs, VOD on disk) and keep the newest
/// [`TRACE_KEEP`] transcripts. Never follows a symlink.
fn sweep(paths: &AppPaths) {
    for dir in [paths.cast_live_dir(), paths.cast_vod_dir()] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let r = match e.file_type() {
                Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
                _ => std::fs::remove_file(&path),
            };
            if let Err(err) = r {
                tracing::debug!("cast: sweeping a session directory: {err}");
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(paths.cast_trace_dir()) else { return };
    let mut traces: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| (e.metadata().and_then(|m| m.modified()).ok(), e.path()))
        .collect();
    traces.sort_by_key(|t| std::cmp::Reverse(t.0));
    for (_, path) in traces.into_iter().skip(TRACE_KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

// ------------------------------------------------------------------ shared state

#[derive(Default)]
struct BrowseState {
    token: Option<CancellationToken>,
    until: Option<Instant>,
}

#[derive(Default)]
struct FirewallCache {
    det: Option<Detection>,
    at: Option<Instant>,
    refreshing: bool,
    job: FirewallJob,
    cancel: Option<CancellationToken>,
}

/// A pairing in progress.
struct PairSlot {
    state: PairState,
    ops: UnboundedSender<PairOp>,
}

/// From the PIN overlay to its pairing task.
#[derive(Debug)]
enum PairOp {
    /// Used once, then dropped (zeroed).
    Submit(Pin),
    NewCode,
    Cancel,
}

#[derive(Default)]
struct TestSlot {
    state: TestState,
    task: Option<(JoinHandle<()>, CancellationToken)>,
}

/// The session casting now.
struct Live {
    id: u64,
    ctl: UnboundedSender<Ctl>,
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Last {
    /// The device last cast to.
    device: Option<DeviceId>,
    /// What its session's `/info` said.
    info: Option<(DeviceId, ReceiverInfo)>,
    error: Option<CastErrorKind>,
}

/// The service's state, shared by the API, the sessions and the background tasks.
pub(crate) struct Shared {
    rt: Handle,
    pub(crate) links: Arc<dyn Links>,
    discovery: Arc<dyn Discovery>,
    store: Store,
    config: SharedConfig,
    paths: AppPaths,
    pub(crate) timings: Timings,
    /// Cancelled when the service stops: sessions close fast, browses, pairings, tests and a
    /// waiting pkexec end.
    pub(crate) stop: CancellationToken,
    registry: Mutex<Registry>,
    /// Discovery found something (a refresh before a cast waits on it).
    found: Notify,
    identity: Mutex<Option<Identity>>,
    browse: Mutex<BrowseState>,
    seen_tx: UnboundedSender<Seen>,
    firewall: Mutex<FirewallCache>,
    pairs: Mutex<HashMap<u64, PairSlot>>,
    next_id: AtomicU64,
    add: Mutex<Option<std::result::Result<DeviceId, CastErrorKind>>>,
    pass: Mutex<Option<PassOutcome>>,
    test: Mutex<TestSlot>,
    live: Mutex<Option<Live>>,
    last: Mutex<Last>,
}

/// Forward discovery's reports into the registry until the service stops.
async fn pump(shared: Arc<Shared>, mut rx: UnboundedReceiver<Seen>) {
    loop {
        let seen = tokio::select! {
            _ = shared.stop.cancelled() => return,
            s = rx.recv() => s,
        };
        match seen {
            Some(s) => shared.seen(s),
            None => return,
        }
    }
}

impl Shared {
    pub(crate) fn config(&self) -> Arc<Config> {
        crate::config::current(&self.config)
    }

    pub(crate) fn writer(&self) -> &Writer {
        self.store.writer()
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// A sighting into the registry; a new or changed usable receiver is written to the store.
    fn seen(&self, seen: Seen) {
        match seen {
            Seen::Found(s) => {
                let (id, usable) = (s.id, s.usability == Usability::Usable);
                let row = {
                    let mut reg = lock(&self.registry);
                    let changed = reg.found(s, Instant::now());
                    if changed && usable { reg.stored(id, crate::model::now()) } else { None }
                };
                if let Some(row) = row {
                    let _ = self.writer().submit(move |c| crate::store::cast::upsert_device(c, &row));
                }
                self.found.notify_waiters();
            }
            Seen::Gone(key) => lock(&self.registry).gone(&key),
        }
    }

    /// Browse for at least `for_` more (starting a browse when none runs).
    fn keep_browsing(self: &Arc<Self>, for_: Duration) {
        if self.stop.is_cancelled() {
            return;
        }
        let until = Instant::now() + for_;
        let token = {
            let mut b = lock(&self.browse);
            b.until = Some(b.until.map_or(until, |u| u.max(until)));
            if b.token.is_some() {
                return;
            }
            let t = self.stop.child_token();
            b.token = Some(t.clone());
            t
        };
        self.discovery.browse(&self.rt, self.seen_tx.clone(), token.clone());
        let this = self.clone();
        self.rt.spawn(async move {
            loop {
                let Some(until) = lock(&this.browse).until else { break };
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep_until(until.into()) => {}
                }
                if lock(&this.browse).until.is_none_or(|u| Instant::now() >= u) {
                    break;
                }
            }
            token.cancel();
            let mut b = lock(&this.browse);
            if b.token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                (b.token, b.until) = (None, None);
            }
        });
    }

    /// Where `id` is now (DESIGN §40 §6: addresses are re-resolved every session): a recent
    /// sighting, else a short mDNS refresh that ends as soon as the TV answers, else the stored
    /// address (a manual device isn't announced: its address is the one typed).
    pub(crate) async fn resolve(self: &Arc<Self>, id: DeviceId) -> Option<SocketAddr> {
        {
            let reg = lock(&self.registry);
            let recent = Instant::now().checked_sub(SEEN_FRESH).is_some_and(|t| reg.seen_since(id, t));
            if reg.manual(id) || recent {
                return reg.addr(id);
            }
        }
        let start = Instant::now();
        self.keep_browsing(self.timings.refresh);
        let deadline = tokio::time::Instant::from(start + self.timings.refresh);
        loop {
            let notified = self.found.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock(&self.registry).seen_since(id, start) {
                break;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => break,
                _ = self.stop.cancelled() => break,
            }
        }
        lock(&self.registry).addr(id)
    }

    /// The relay's settings for a session toward `peer` (read live: a change applies at the
    /// next session).
    pub(crate) fn relay_config(&self, bind: Ipv4Addr, peer: Ipv4Addr) -> RelayConfig {
        let cfg = self.config();
        RelayConfig {
            bind,
            peer,
            port: cfg.cast.media_port,
            min_segments: cfg.cast.min_segments,
            convert: cfg.cast.convert,
            interlaced: cfg.cast.interlaced,
            live_dir: self.paths.cast_live_dir(),
            vod_dir: self.paths.cast_vod_dir(),
            user_agent: cfg.player.user_agent.clone(),
        }
    }

    /// A session's `/info` (Settings shows the TV's current version; `diagnose`): the info with
    /// the OS version joined with what discovery knew ("27.0" + "(23L773)").
    pub(crate) fn learned(&self, id: DeviceId, info: &ReceiverInfo) -> ReceiverInfo {
        let mut info = info.clone();
        let row = {
            let mut reg = lock(&self.registry);
            reg.learned(id, &info);
            if let Some(d) = reg.get(id) {
                info.os_version = d.os_version.clone();
            }
            reg.get(id).filter(|d| d.usability == Usability::Usable).and_then(|_| reg.stored(id, crate::model::now()))
        };
        if let Some(row) = row {
            let _ = self.writer().submit(move |c| crate::store::cast::upsert_device(c, &row));
        }
        lock(&self.last).info = Some((id, info.clone()));
        info
    }

    /// The last cast failure (the agent's `status.casting.error`; kept after casting ended).
    pub(crate) fn note_error(&self, kind: CastErrorKind) {
        lock(&self.last).error = Some(kind);
    }

    /// The failure is over: the TV shows a picture again, or the user ended a cast that wasn't
    /// lost (Stop casting, Play here). A cast lost or stopped on the TV keeps its reason for
    /// the agent while nothing plays.
    pub(crate) fn clear_error(&self) {
        lock(&self.last).error = None;
    }

    /// The detection target: the TV last cast to, else the default TV, else any TV on the
    /// network, else the mDNS group (the LAN's route either way).
    fn toward(&self) -> Ipv4Addr {
        let v4 = |a: Option<SocketAddr>| match a?.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        };
        let last = lock(&self.last).device;
        let default = self.config().cast.default_device.as_deref().and_then(|s| s.parse::<DeviceId>().ok());
        let reg = lock(&self.registry);
        [last, default]
            .into_iter()
            .flatten()
            .find_map(|id| v4(reg.addr(id)))
            .or_else(|| reg.list().iter().filter(|d| d.available).find_map(|d| v4(d.addr())))
            .unwrap_or(MDNS_GROUP)
    }

    /// Detect in the background unless a detection already runs.
    pub(crate) fn refresh_firewall(self: &Arc<Self>, toward: Option<Ipv4Addr>) {
        {
            let mut fw = lock(&self.firewall);
            if fw.refreshing || self.stop.is_cancelled() {
                return;
            }
            fw.refreshing = true;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            this.detect_now(toward).await;
        });
    }

    /// Detect now (and cache it).
    pub(crate) async fn detect_now(&self, toward: Option<Ipv4Addr>) -> Detection {
        let toward = toward.unwrap_or_else(|| self.toward());
        let det = self.links.detect(self.config().cast.media_port, toward).await;
        let mut fw = lock(&self.firewall);
        (fw.det, fw.at, fw.refreshing) = (Some(det.clone()), Some(Instant::now()), false);
        det
    }

    /// This install's identity (created by the first pairing).
    async fn ensure_identity(&self) -> Result<Identity> {
        if let Some(id) = lock(&self.identity).clone() {
            return Ok(id);
        }
        let now = crate::model::now();
        let id = self.writer().run(move |c| crate::store::cast::ensure_identity(c, now)).await?;
        Ok(lock(&self.identity).get_or_insert(id).clone())
    }

    fn set_pair(&self, job: u64, state: PairState) {
        if let Some(slot) = lock(&self.pairs).get_mut(&job) {
            slot.state = state;
        }
    }

    /// Pairing finished: store it (the device row first, in case discovery's write is still
    /// queued) and list the TV as paired.
    async fn paired(&self, id: DeviceId, peer: PeerCredentials) -> Result<()> {
        let row = lock(&self.registry).stored(id, crate::model::now());
        let stored = peer.clone();
        let now = crate::model::now();
        self.writer()
            .run(move |c| {
                if let Some(row) = &row {
                    crate::store::cast::upsert_device(c, row)?;
                }
                crate::store::cast::set_paired(c, id, &stored, now)
            })
            .await?;
        lock(&self.registry).paired(id, peer);
        Ok(())
    }

    /// The TV and the media port, free for a new session or Test on TV: closes the session
    /// casting now and cancels a running test; the returned task ends when both let go.
    fn take_tv(&self) -> Option<JoinHandle<()>> {
        let mut handles = Vec::new();
        if let Some(live) = lock(&self.live).take() {
            let _ = live.ctl.send(Ctl::Close { fast: false });
            handles.push(live.task);
        }
        if let Some((task, cancel)) = lock(&self.test).task.take() {
            cancel.cancel();
            handles.push(task);
        }
        (!handles.is_empty()).then(|| {
            self.rt.spawn(async move {
                for h in handles {
                    let _ = h.await;
                }
            })
        })
    }

    /// Stopping: cancel everything and hand back the tasks that still talk to a TV.
    fn shut(&self) -> Vec<JoinHandle<()>> {
        self.stop.cancel();
        let mut handles: Vec<JoinHandle<()>> = lock(&self.live).take().map(|l| l.task).into_iter().collect();
        handles.extend(lock(&self.test).task.take().map(|(t, _)| t));
        handles
    }

    /// A device's credentials for a session, or why it can't cast.
    fn credentials(&self, id: DeviceId) -> std::result::Result<(Device, Credentials), CastErrorKind> {
        let (device, peer) = {
            let reg = lock(&self.registry);
            (reg.get(id).cloned(), reg.peer(id))
        };
        let device = device.ok_or(CastErrorKind::Unreachable)?;
        if !device.selectable() {
            return Err(CastErrorKind::Unsupported);
        }
        let peer = peer.ok_or(CastErrorKind::PairingRequired)?;
        let identity = lock(&self.identity).clone().ok_or(CastErrorKind::PairingRequired)?;
        Ok((device, Credentials { identity, peer }))
    }
}

/// One pairing: holds the connection while the user types; a wrong PIN keeps it (the TV shows
/// a new code), anything else drops it until **Show a new code**.
///
/// The HKP 3 → HKP 5 ladder (DESIGN §40 §7.2) runs inside [`PairingSession`] on the pairing
/// connection: a refused pin-start climbs in `start`, and a 470 at M2–M4 under HKP 3 makes
/// `finish` ask for a new code with HKP 5 and answer `WrongPin` ("The TV is showing a new
/// one"); `variant()` then says which variant shows the code. A wrong code is `EnterCode` again
/// (the TV waits for its new code on the same connection), never `Failed`. A `Refused` is final.
async fn pair_task(shared: Arc<Shared>, job: u64, device: DeviceId, mut ops: UnboundedReceiver<PairOp>) {
    let set = |s: PairState| shared.set_pair(job, s);
    let identity = match shared.ensure_identity().await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("cast pairing: identity: {e}");
            return set(PairState::Failed(PairError::Failed));
        }
    };
    let Some(addr) = shared.resolve(device).await else { return set(PairState::Failed(PairError::Unreachable)) };
    let mut variant = PairVariant::Hkp3;
    let mut session = match PairingSession::start(addr, &identity, variant).await {
        Ok(s) => {
            variant = s.variant();
            set(PairState::EnterCode { variant });
            Some(s)
        }
        Err(e) => {
            set(PairState::Failed(e));
            None
        }
    };
    loop {
        let op = tokio::select! {
            _ = shared.stop.cancelled() => return,
            op = ops.recv() => op,
        };
        match op {
            None | Some(PairOp::Cancel) => return,
            Some(PairOp::NewCode) => {
                session = None; // closes the pairing connection
                set(PairState::Starting);
                let next = variant.next();
                match PairingSession::start(addr, &identity, next).await {
                    Ok(s) => {
                        variant = s.variant();
                        set(PairState::EnterCode { variant });
                        session = Some(s);
                    }
                    Err(e) => set(PairState::Failed(e)),
                }
            }
            Some(PairOp::Submit(pin)) => {
                let Some(s) = session.as_mut() else {
                    set(PairState::Failed(PairError::Expired));
                    continue;
                };
                set(PairState::Checking);
                let result = s.finish(&pin).await;
                drop(pin);
                match result {
                    Ok(peer) => {
                        // the pairing connection closes when the task ends, right after
                        match shared.paired(device, peer).await {
                            Ok(()) => {
                                tracing::info!("cast: paired device {} ({})", device.short(), variant.as_str());
                                set(PairState::Paired);
                            }
                            Err(e) => {
                                tracing::warn!("cast pairing: storing it: {e}");
                                set(PairState::Failed(PairError::Failed));
                            }
                        }
                        return;
                    }
                    // the TV shows a new code on the same connection (maybe with HKP 5 now) and
                    // waits for it: asked for again, as at the start (the overlay, which saw this
                    // code go from `Checking` back to `EnterCode`, says it didn't match)
                    Err(PairError::WrongPin) => {
                        variant = s.variant();
                        set(PairState::EnterCode { variant });
                    }
                    Err(e) => {
                        session = None; // closes the pairing connection
                        set(PairState::Failed(e));
                    }
                }
            }
        }
    }
}

/// **Allow on firewall** (or **Remove rule**): pkexec's outcome, then a fresh detection.
async fn firewall_task(shared: Arc<Shared>, remove: bool, cancel: CancellationToken) {
    let det = shared.detect_now(None).await;
    let port = shared.config().cast.media_port;
    let set = |job: FirewallJob| lock(&shared.firewall).job = job;
    let (verb, outcome) = if remove {
        let verb = Verb::Remove;
        set(FirewallJob::Waiting { verb });
        let mut outcome = AllowOutcome::Added;
        // every tagged rule, from its parsed tuple (works after a network change); never by
        // rule number
        for rule in det.tagged.clone() {
            outcome = shared.links.firewall(verb, rule, cancel.clone()).await;
            if !matches!(outcome, AllowOutcome::Added | AllowOutcome::AddedUnverified) {
                break;
            }
        }
        (verb, outcome)
    } else {
        let verb = Verb::Allow { first: matches!(det.state, FirewallState::Blocked { shadowed: true }) };
        set(FirewallJob::Waiting { verb });
        let outcome = match &det.net {
            Some(net) => {
                let rule = net.rule(port);
                let outcome = shared.links.firewall(verb, rule.clone(), cancel.clone()).await;
                if matches!(outcome, AllowOutcome::Added | AllowOutcome::AddedUnverified) {
                    // the port changed: Tuner's rule for the old port on this network moves (one
                    // add and one remove under the same polkit keep)
                    let stale = det.tagged.iter().filter(|r| r.iface == rule.iface && r.net == rule.net && r.prefix == rule.prefix && r.port != port);
                    for old in stale {
                        let o = shared.links.firewall(Verb::Remove, old.clone(), cancel.clone()).await;
                        if !matches!(o, AllowOutcome::Added | AllowOutcome::AddedUnverified) {
                            tracing::info!("cast firewall: the rule for the old port stays ({o:?})");
                        }
                    }
                }
                outcome
            }
            None => AllowOutcome::Failed { message: "Tuner couldn't tell which network the TV is on.".into() },
        };
        (verb, outcome)
    };
    {
        let mut fw = lock(&shared.firewall);
        fw.job = FirewallJob::Done { verb, outcome };
        fw.cancel = None;
    }
    shared.detect_now(None).await;
}

// ------------------------------------------------------------------ the API

/// [`CastApi`] over the shared state (what `CastHandle` derefs to).
struct Api(Arc<Shared>);

fn cast_error(kind: CastErrorKind) -> PlayerError {
    let message = match kind {
        CastErrorKind::PairingRequired => "Pair this TV first (Settings ▸ Cast).",
        CastErrorKind::Unsupported => "Tuner can't cast to this device.",
        _ => "That TV isn't available.",
    };
    PlayerError { kind: PlayerErrorKind::Cast(kind), message: message.into() }
}

impl CastApi for Api {
    fn devices(&self) -> Vec<Device> {
        lock(&self.0.registry).list()
    }

    fn browse(&self) {
        self.0.keep_browsing(self.0.timings.linger);
    }

    fn searching(&self) -> bool {
        lock(&self.0.browse).token.is_some()
    }

    fn add_by_address(&self, ip: Ipv4Addr) {
        let shared = self.0.clone();
        let probe = shared.discovery.probe(ip);
        self.0.rt.spawn(async move {
            let outcome = probe.await.map(|s| {
                let id = s.id;
                shared.seen(Seen::Found(s));
                id
            });
            *lock(&shared.add) = Some(outcome);
        });
    }

    fn take_add_outcome(&self) -> Option<std::result::Result<DeviceId, CastErrorKind>> {
        lock(&self.0.add).take()
    }

    fn forget(&self, device: DeviceId) {
        lock(&self.0.registry).forget(device);
        let _ = self.0.writer().submit(move |c| crate::store::cast::forget(c, device));
    }

    fn pair_start(&self, device: DeviceId) -> PairJob {
        let job = self.0.next_id();
        let (tx, rx) = mpsc::unbounded_channel();
        lock(&self.0.pairs).insert(job, PairSlot { state: PairState::Starting, ops: tx });
        self.0.rt.spawn(pair_task(self.0.clone(), job, device, rx));
        PairJob(job)
    }

    fn pair_state(&self, job: PairJob) -> PairState {
        lock(&self.0.pairs).get(&job.0).map_or(PairState::Gone, |s| s.state)
    }

    /// `Checking` from the moment the code is sent (not when the task picks it up): the overlay
    /// never reads the `EnterCode` from before it as the TV asking again.
    fn pair_submit(&self, job: PairJob, pin: Pin) {
        if let Some(slot) = lock(&self.0.pairs).get_mut(&job.0) {
            slot.state = PairState::Checking;
            let _ = slot.ops.send(PairOp::Submit(pin));
        }
    }

    fn pair_new_code(&self, job: PairJob) {
        if let Some(slot) = lock(&self.0.pairs).get(&job.0) {
            let _ = slot.ops.send(PairOp::NewCode);
        }
    }

    fn pair_cancel(&self, job: PairJob) {
        if let Some(slot) = lock(&self.0.pairs).remove(&job.0) {
            let _ = slot.ops.send(PairOp::Cancel);
        }
    }

    fn firewall(&self) -> Option<Detection> {
        let (det, stale) = {
            let fw = lock(&self.0.firewall);
            (fw.det.clone(), fw.at.is_none_or(|t| t.elapsed() > FIREWALL_CACHE))
        };
        if stale {
            self.0.refresh_firewall(None);
        }
        det
    }

    fn firewall_check(&self) {
        self.0.refresh_firewall(None);
    }

    fn firewall_allow(&self) {
        self.firewall_job_start(false);
    }

    fn firewall_remove(&self) {
        self.firewall_job_start(true);
    }

    fn firewall_cancel(&self) {
        if let Some(c) = lock(&self.0.firewall).cancel.take() {
            c.cancel();
        }
    }

    fn firewall_job(&self) -> FirewallJob {
        lock(&self.0.firewall).job.clone()
    }

    fn start_pass(&self) {
        let shared = self.0.clone();
        shared.keep_browsing(shared.timings.pass);
        self.0.rt.spawn(async move {
            tokio::select! {
                _ = shared.stop.cancelled() => return,
                _ = tokio::time::sleep(shared.timings.pass) => {}
            }
            let offer = lock(&shared.registry).offer();
            *lock(&shared.pass) = Some(PassOutcome { offer });
        });
    }

    fn take_pass_outcome(&self) -> Option<PassOutcome> {
        lock(&self.0.pass).take()
    }

    /// Not while casting (the TV and the port are the cast's): the state then stays as it was.
    fn test_on_tv(&self, device: DeviceId) {
        let shared = &self.0;
        if lock(&shared.live).is_some() {
            tracing::info!("cast: Test on TV skipped while casting");
            return;
        }
        let mut test = lock(&shared.test);
        if test.task.as_ref().is_some_and(|(t, _)| !t.is_finished()) {
            return;
        }
        let (device, creds) = match shared.credentials(device) {
            Ok(dc) => dc,
            Err(kind) => {
                test.state = TestState::Failed(kind);
                return;
            }
        };
        test.state = TestState::Running;
        let cancel = shared.stop.child_token();
        let short = device.id.short();
        tracing::info!("cast: Test on TV on device {short}");
        let session = Session::new(shared.clone(), device, creds, Arc::new(|_| {}));
        let this = shared.clone();
        let stop = cancel.clone();
        let task = shared.rt.spawn(async move {
            let state = super::session::test_on_tv(session, None, stop).await;
            match state {
                TestState::Failed(kind) => tracing::info!("cast: Test on TV on device {short}: failed ({})", kind.reason()),
                state => tracing::info!("cast: Test on TV on device {short}: {state:?}"),
            }
            lock(&this.test).state = state;
        });
        test.task = Some((task, cancel));
    }

    fn test_state(&self) -> TestState {
        lock(&self.0.test).state
    }

    fn convert_current(&self) {
        if let Some(live) = lock(&self.0.live).as_ref() {
            let _ = live.ctl.send(Ctl::Convert);
        }
    }

    fn last_error(&self) -> Option<CastErrorKind> {
        lock(&self.0.last).error
    }

    fn diagnose(&self) -> CastDiagnose {
        let (device, info, last_error) = {
            let last = lock(&self.0.last);
            (last.device, last.info.clone(), last.error)
        };
        let info = info.filter(|(id, _)| Some(*id) == device).map(|(_, i)| i).unwrap_or_default();
        let reg = lock(&self.0.registry);
        let d = device.and_then(|id| reg.get(id));
        let pairing = match d.map(|d| d.pairing) {
            Some(Pairing::Paired(v)) => Some(v),
            _ => info.pairing,
        };
        CastDiagnose {
            device,
            model: info.model.or_else(|| d.and_then(|d| d.model.clone())),
            os_version: info.os_version.or_else(|| d.and_then(|d| d.os_version.clone())),
            source_version: info.source_version.or_else(|| device.and_then(|id| reg.source_version(id))),
            pairing,
            timing: "ptp-declared",
            firewall: lock(&self.0.firewall).det.as_ref().map(|d| d.state),
            last_error,
        }
    }

    fn open(&self, id: DeviceId) -> std::result::Result<RemoteOutput, PlayerError> {
        let shared = &self.0;
        if shared.stop.is_cancelled() {
            return Err(cast_error(CastErrorKind::Unreachable));
        }
        let (device, creds) = shared.credentials(id).map_err(cast_error)?;
        // one cast at a time: the previous session (or a test) lets go first
        let after = shared.take_tv();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let view = CastView {
            device: id,
            name: device.name.clone(),
            kind: device.kind,
            phase: CastPhase::Connecting,
            level: None,
            behind_live_s: None,
            can_seek: true,
            // AirPlay has no verified volume path ("Use the TV remote for volume")
            volume: !matches!(device.kind, ReceiverKind::AppleTv | ReceiverKind::AirPlayTv),
        };
        tracing::info!("cast session {}: starting", id.short());
        let session = Session::new(shared.clone(), device, creds, Arc::new(move |ev| drop(ev_tx.send(ev))));
        let sid = shared.next_id();
        let this = shared.clone();
        let task = shared.rt.spawn(async move {
            session.run(cmd_rx, ctl_rx, after).await;
            let mut live = lock(&this.live);
            if live.as_ref().is_some_and(|l| l.id == sid) {
                *live = None;
            }
        });
        *lock(&shared.live) = Some(Live { id: sid, ctl: ctl_tx, task });
        let now = crate::model::now();
        lock(&shared.registry).used(id, now);
        let _ = shared.writer().submit(move |c| crate::store::cast::mark_used(c, id, now));
        {
            let mut last = lock(&shared.last);
            (last.device, last.error) = (Some(id), None);
        }
        Ok(RemoteOutput { cmd: cmd_tx, events: ev_rx, view })
    }
}

impl Api {
    fn firewall_job_start(&self, remove: bool) {
        let shared = &self.0;
        let cancel = {
            let mut fw = lock(&shared.firewall);
            if matches!(fw.job, FirewallJob::Waiting { .. }) || shared.stop.is_cancelled() {
                return;
            }
            // until detection says whether a shadowing rule needs `prepend`
            fw.job = FirewallJob::Waiting { verb: if remove { Verb::Remove } else { Verb::Allow { first: false } } };
            let c = shared.stop.child_token();
            fw.cancel = Some(c.clone());
            c
        };
        shared.rt.spawn(firewall_task(shared.clone(), remove, cancel));
    }
}
