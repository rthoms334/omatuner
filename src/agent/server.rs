//! The running Tuner's side of IPC v2 (DESIGN §30.4–§30.6). Owner: MCP-app track.
//!
//! `ipc.rs` hands every v2 request line to an [`AgentService`]; production uses
//! [`AgentHandler`]. The handler runs on the tokio runtime (blocking store reads on the blocking
//! pool, a small pool of lazily opened readers), never on the UI thread:
//! * queries read the snapshot the app published (`AppShared::library`), a store reader (EPG,
//!   progress, favourites, details) and the parental state computed now (`AppShared::parental`)
//!   — `agent::query`. A snapshot built while unlocked loses its explicit content here when the
//!   session relocked since (`Library::hiding_explicit`, cached per snapshot);
//! * `play` validates the target (`agent::act::plan_play`: exists, visible, catch-up window…),
//!   resolves where it shows (`agent::act::agent_show`: by default the full window when it's
//!   on screen, else the pop-out, DESIGN §30.13) and sends
//!   `AppEvent::Agent(AgentCommand::Play)`, with `hypr::focus_self()` only when it shows in
//!   the window: the app applies the PIN rules (prompt on screen), zap lists and
//!   resume rules exactly as for a key press, and moves the window;
//! * `play {device}` casts instead (DESIGN §40 §12): the TV is resolved and checked
//!   (`agent::act::cast_gate`: paired, seen, no firewall block, no multi-view), the window
//!   stays where it is (no placement, no raise except for the PIN prompt), and the reply is
//!   immediate (`casting_started`; connecting can take longer than a request may): `status`
//!   says how it went;
//! * `control` / `status` go straight to the `PlayerHandle` (work while the window is hidden);
//! * `schedule_switch` / `cancel_switch` write reminder rows (`Reminder::SWITCH_LEAD`) through
//!   the store writer and wake the reminder scheduler, which fires them (`PlatformEvent::Switch*`).
//!
//! Guardrails (§30.6): visibility in `query`; limits and the reply check in `agent::guard`;
//! actions are logged at INFO by method and ref (never a title), queries at DEBUG without their
//! text.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use rusqlite::Connection;
use serde::Serialize;

use super::act::{self, PlayStep};
use super::guard::{self, Limits};
use super::proto::{Call, CancelOut, Control, ControlAction, ErrorCode, Reply, Request};
use super::query::{self, Fail, Out, Q};
use crate::config::SharedConfig;
use crate::jobs::JobManager;
use crate::model::{CastErrorKind, OpenIn};
use crate::msg::{AppEvent, DataScope, EventBus};
use crate::player::tiles::TileRegistry;
use crate::player::{PlayerCommand, PlayerHandle};
use crate::reminders::ui::{Placement, WindowNow};
use crate::shared::{AppShared, ParentalView};
use crate::store::Store;
use crate::store::library::Library;

/// Answers v2 requests (the IPC server holds one; tests install fakes).
pub trait AgentService: Send + Sync + 'static {
    /// The reply to `req` (always a reply: failures are `Reply::err`). Must finish well within
    /// the IPC connection limit (5 s); the handler's own budget is [`HANDLER_BUDGET`].
    fn handle(&self, req: Request) -> BoxFuture<'static, Reply>;
}

/// Time a request may take inside the handler (the IPC connection allows 5 s in all).
pub const HANDLER_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// How long `details` / `play` wait for a title's lazily fetched details.
const DETAILS_WAIT: Duration = Duration::from_secs(3);
/// Store readers kept for the handler.
const READERS: usize = 3;
/// How long `control` waits for the player to show the change.
const CONTROL_WAIT: Duration = Duration::from_millis(500);
/// How long `control stop_casting` / `play_here` wait: the TV is told to stop first (the
/// controller waits up to 1.5 s for the relay to close).
const CAST_STOP_WAIT: Duration = Duration::from_secs(2);
/// How long `play {device}` lets discovery look for a remembered TV Tuner hasn't seen yet.
const DISCOVER_WAIT: Duration = Duration::from_secs(2);
/// How long `play {device}` waits for a firewall detection toward the TV (files and three
/// `systemctl is-active`: tens of ms); none by then blocks nothing (DESIGN §40 §12).
const FIREWALL_WAIT: Duration = Duration::from_secs(1);

/// The production [`AgentService`].
#[derive(Clone)]
pub struct AgentHandler {
    inner: Arc<Inner>,
}

/// The handler's state, shared by the tool modules (`agent::{queue, follow, …}`).
pub(super) struct Inner {
    pub(super) shared: Arc<AppShared>,
    pub(super) store: Store,
    pub(super) config: SharedConfig,
    /// Picture A (single view: the only one).
    pub(super) player: PlayerHandle,
    pub(super) bus: EventBus,
    pub(super) jobs: JobManager,
    /// Every picture's handle (multi-view `control tile`); `None`: picture A only.
    pub(super) tiles: Option<TileRegistry>,
    /// `guide_fix propose`'s proposals until applied or expired.
    pub(super) proposals: super::guide::Proposals,
    /// When each named agent last asked (Settings ▸ AI agents' "last used"; DESIGN §39).
    usage: super::connect::usage::Usage,
    limits: Limits,
    readers: Mutex<Vec<Connection>>,
    /// The published snapshot without its explicit content (generation → copy), for a
    /// session that relocked after the snapshot was built.
    concealed: Mutex<Option<(u64, Arc<Library>)>>,
}

impl AgentHandler {
    pub fn new(shared: Arc<AppShared>, store: Store, config: SharedConfig, player: PlayerHandle, bus: EventBus, jobs: JobManager) -> Self {
        Self::with_limits(shared, store, config, player, bus, jobs, Limits::default())
    }

    /// [`AgentHandler::new`] with other request limits (tests).
    pub fn with_limits(
        shared: Arc<AppShared>,
        store: Store,
        config: SharedConfig,
        player: PlayerHandle,
        bus: EventBus,
        jobs: JobManager,
        limits: Limits,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                shared,
                store,
                config,
                player,
                bus,
                jobs,
                tiles: None,
                proposals: super::guide::Proposals::default(),
                usage: Default::default(),
                limits,
                readers: Mutex::new(Vec::new()),
                concealed: Mutex::new(None),
            }),
        }
    }

    fn configure(mut self, f: impl FnOnce(&mut Inner)) -> Self {
        f(Arc::get_mut(&mut self.inner).expect("configure the handler before cloning it"));
        self
    }

    /// Every picture (multi-view: `control tile`, `status.multiview`); without it the handler
    /// reaches picture A only.
    pub fn with_tiles(self, tiles: TileRegistry) -> Self {
        self.configure(|i| i.tiles = Some(tiles))
    }
}

impl AgentService for AgentHandler {
    fn handle(&self, req: Request) -> BoxFuture<'static, Reply> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (id, method, action) = (req.id, req.call.name(), req.call.is_action());
            let _slot = match inner.limits.admit(action) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("agent: {method}: {:?}", e.code);
                    return Reply::err(id, e.code, e.message);
                }
            };
            let started = Instant::now();
            let (policy, reveal) = req.call.guard_opts();
            // who asked, for "added by Codex" (display only: never a permission)
            let who = req.agent_name();
            if let Some(name) = who.stored() {
                inner.usage.note(&inner.store, name, crate::model::now());
            }
            let reply = match tokio::time::timeout(HANDLER_BUDGET, inner.clone().dispatch(req.call, who)).await {
                // every result passes the spoiler guard's one choke point (B4)
                Ok(Ok(mut v)) => {
                    guard::guard_reply(&mut v, &inner.shared.guard(), policy, &reveal);
                    Reply::ok(id, v)
                }
                Ok(Err(f)) => Reply::err(id, f.code, f.message),
                Err(_) => {
                    tracing::warn!("agent: {method} took longer than {} s", HANDLER_BUDGET.as_secs());
                    Reply::err(id, ErrorCode::Internal, "Tuner took too long to answer; try a narrower request")
                }
            };
            let reply = guard::sanitize(reply, method);
            let outcome = reply.error.as_ref().map_or("ok", |e| code_name(e.code));
            tracing::debug!("agent: {method} → {outcome} in {} ms", started.elapsed().as_millis());
            reply
        })
    }
}

fn code_name(c: ErrorCode) -> &'static str {
    match c {
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::NotStarted => "not_started",
        ErrorCode::Loading => "loading",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Busy => "busy",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Internal => "internal",
    }
}

/// A failed write, for the model (the store's message is logged, not sent: it may name rows).
fn write_fail(e: crate::error::Error) -> Fail {
    match e {
        crate::error::Error::Other(m) if m.len() < 160 && !m.contains("://") => Fail::new(ErrorCode::Unavailable, m),
        e => Fail::from(e),
    }
}

pub(super) fn to_value<T: Serialize>(v: &T) -> Out<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| Fail::new(ErrorCode::Internal, format!("encoding the answer: {e}")))
}

impl Inner {
    async fn dispatch(self: Arc<Self>, call: Call, who: crate::model::AgentName) -> Out<serde_json::Value> {
        match call {
            Call::GuideSearch(p) => self.query(move |q| query::guide_search(q, &p)).await,
            Call::WhatsOn(p) => self.query(move |q| query::whats_on(q, &p)).await,
            Call::Search(p) => self.query(move |q| query::search(q, &p)).await,
            Call::Categories(p) => self.query(move |q| query::categories(q, &p)).await,
            Call::Browse(p) => self.query(move |q| query::browse(q, &p)).await,
            Call::ContinueWatching(p) => self.query(move |q| query::continue_watching(q, &p)).await,
            Call::Favourites(p) => self.query(move |q| query::favourites(q, &p)).await,
            Call::Details(p) => self.details(p).await,
            Call::Status(_) => self.status().await,
            Call::CastDevices(_) => self.cast_devices(),
            Call::Play(p) => self.play(p).await.inspect_err(|_| self.undo_empty_pop_out()),
            Call::ScheduleSwitch(p) => self.schedule_switch(p).await,
            Call::CancelSwitch(p) => {
                let id = self.run(true, move |q| act::check_cancel(q, &p)).await?;
                self.store
                    .writer()
                    .submit_notify(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id, crate::model::RemoveWhy::Agent))
                    .recv()
                    .await
                    .map_err(Fail::from)?;
                self.shared.wake_reminders();
                tracing::info!("agent: cancel_switch {} → cancelled", id.0);
                to_value(&CancelOut { cancelled: true })
            }
            Call::Control(c) => self.control(c).await,
            // the batch's tools (`handle` refused them already when their feature is off)
            Call::Queue(p) => {
                let starts = matches!(p.action, super::params::QueueAction::Play | super::params::QueueAction::Skip);
                super::queue::handle(&self, p).await.inspect_err(|_| {
                    if starts {
                        self.undo_empty_pop_out();
                    }
                })
            }
            Call::Downloads(p) => super::downloads::handle(&self, p).await,
            Call::Watchlist(p) => {
                let starts = p.action == super::params::WatchlistAction::Play;
                super::watchlist::handle(&self, p, &who).await.inspect_err(|_| {
                    if starts {
                        self.undo_empty_pop_out();
                    }
                })
            }
            Call::Follow(p) => super::follow::handle(&self, p, &who).await,
            Call::SpoilerGuard(p) => super::spoiler::handle(&self, p, &who).await,
            Call::GuideFix(p) => super::guide::handle(&self, p, &who).await,
            Call::Multiview(p) => super::multiview::handle(&self, p).await,
            Call::Reminders(p) => super::reminders::handle(&self, p).await,
            Call::SleepTimer(p) => super::reminders::sleep_timer(&self, p).await,
            Call::MatchTitles(p) => self.query(move |q| super::library::match_titles(q, &p)).await,
            Call::History(p) => self.query(move |q| super::library::history(q, &p)).await,
            Call::Episodes(p) => super::library::episodes(&self, p).await,
            Call::FavouritesEdit(p) => super::library::favourites_edit(&self, p).await,
            Call::ParentalStatus(_) => self.query(super::library::parental_status).await,
            Call::Lock(p) => super::library::lock(&self, p).await,
            Call::Diagnose(p) => super::health::diagnose(&self, p).await,
            Call::Sources(_) => self.query(super::health::sources).await,
        }
    }

    /// The live config.
    pub(super) fn cfg(&self) -> Arc<crate::config::Config> {
        crate::config::current(&self.config)
    }

    /// Tuner's window as the app last published it; before its first frame, only whether a
    /// pop-out is possible (Hyprland).
    pub(super) fn window(&self) -> WindowNow {
        self.shared
            .window()
            .unwrap_or_else(|| WindowNow { can_pop: crate::platform::hypr::is_hyprland() && !crate::platform::off_desktop(), ..Default::default() })
    }

    /// Where an agent's play shows ([`act::agent_show`] with the user's "Agents play in"), and
    /// the window it was decided on. When the answer depends on whether the user sees Tuner's
    /// window (unasked, not popped out, without the keyboard: the agent's terminal has it),
    /// Hyprland is asked ([`crate::platform::popout::self_shown`]); when it can't tell — also
    /// for a Tuner just started for this play, not mapped yet: it opens on the terminal's
    /// workspace — the window counts as seen (nothing moves uninvited).
    pub(super) async fn show(&self, asked: Option<OpenIn>, pin: bool) -> (act::AgentShow, WindowNow) {
        let (mut w, setting) = (self.window(), self.cfg().agents.play_in);
        let unasked = asked.is_none() && !pin && setting != OpenIn::FullWindow;
        if unasked && w.can_pop && !w.popped_out && !w.visible {
            let shown = tokio::task::spawn_blocking(crate::platform::popout::self_shown).await.ok().flatten();
            w.visible = shown.unwrap_or(true);
        }
        (act::agent_show(asked, setting, w, pin), w)
    }

    /// An agent's play failed: a pop-out with nothing in it (Tuner started for that play,
    /// `--agent-popout`; popping out counts) goes back into the window, unraised, so no empty
    /// player stays pinned on every workspace (DESIGN §30.13).
    pub(super) fn undo_empty_pop_out(&self) {
        let idle = matches!(self.player.snapshot().state, crate::player::PlayerState::Idle | crate::player::PlayerState::Ended);
        if idle && self.shared.window().is_some_and(|w| w.popped_out) {
            tracing::debug!("agent: the play failed; an empty pop-out goes back in");
            self.bus.send(AppEvent::Agent(super::AgentCommand::PopIn));
        }
    }

    /// Show a play where `show` says: raise the window when it shows there. `opened_in` for the
    /// result.
    pub(super) fn raise_for(show: act::AgentShow, w: WindowNow) -> super::proto::OpenedIn {
        if show.raise {
            crate::platform::hypr::focus_self();
        }
        act::opened_in(show, w)
    }

    /// One write through the store writer (notifying `scope`).
    pub(super) async fn write<T: Send + 'static>(
        &self,
        scope: DataScope,
        f: impl FnOnce(&mut Connection) -> crate::error::Result<T> + Send + 'static,
    ) -> Out<T> {
        self.store.writer().submit_notify(scope, f).recv().await.map_err(write_fail)
    }

    /// What the pictures, Up next and the sleep timer say now (`status`).
    pub(super) fn status_more(&self) -> act::StatusMore {
        let tiles = self.shared.tiles();
        let b = self.shared.tile_player(crate::model::TileId::B).map(|rx| rx.borrow().clone());
        act::StatusMore {
            queue: self.shared.queue(),
            tiles,
            tile_b: b,
            sleep: self.shared.sleep().and_then(|h| h.status()),
            casting: None,
            play_on: None,
        }
    }

    /// What the cast service remembers of the last cast, for `status.casting` (DESIGN §40 §12).
    pub(super) fn cast_last(&self) -> act::CastLast {
        let cast = self.shared.cast();
        let Some(error) = cast.last_error() else { return act::CastLast::default() };
        let device = cast.diagnose().device.and_then(|id| cast.devices().into_iter().find(|d| d.id == id)).map(|d| (d.id, d.name, d.kind));
        act::CastLast { error: Some(error), device }
    }

    /// `cast_devices`: the registry as it is (it never starts a scan).
    fn cast_devices(&self) -> Out<serde_json::Value> {
        let casting = self.player.snapshot().cast.as_ref().map(|v| v.device);
        to_value(&act::cast_devices(&self.shared.cast().devices(), casting))
    }

    /// The TV `play {device}` asks for, once a cast to it can start, and whether it still has to
    /// be paired (an outcome, not an error: agents never pair). A remembered TV Tuner hasn't
    /// seen since it started gets one look by discovery (bounded) before it counts as
    /// unreachable. The firewall is judged last, by a fresh detection made toward this TV's own
    /// address (bounded too): never by one made toward another TV or before any was known.
    async fn cast_target(&self, asked: &str) -> Out<(crate::cast::Device, bool)> {
        let cast = self.shared.cast();
        let mut device = act::resolve_device(&cast.devices(), asked)?;
        let multiview = self.shared.tiles().on;
        let mut gate = act::cast_gate(&device, multiview, None);
        if gate == act::CastGate::Refused(CastErrorKind::Unreachable.reason()) {
            cast.browse();
            let deadline = Instant::now() + DISCOVER_WAIT;
            while Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(seen) = cast.devices().into_iter().find(|d| d.id == device.id && d.available) {
                    gate = act::cast_gate(&seen, multiview, None);
                    device = seen;
                    break;
                }
            }
        }
        if gate == act::CastGate::Go {
            let deadline = Instant::now() + FIREWALL_WAIT;
            let mut det = cast.firewall_for(device.id);
            while det.is_none() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
                det = cast.firewall_for(device.id);
            }
            gate = act::cast_gate(&device, multiview, det.as_ref());
        }
        match gate {
            act::CastGate::Go => Ok((device, false)),
            act::CastGate::PairingRequired => Ok((device, true)),
            act::CastGate::Refused(reason) => Err(act::refusal(reason)),
        }
    }

    /// The TV a play without `device` lands on (DESIGN §40 §3.8, §3.9): the one picture A casts
    /// to, else the Play on switch's (single view), with its name.
    pub(super) fn lands_on(&self, snap: &crate::player::PlayerSnapshot) -> Option<(crate::model::DeviceId, String)> {
        if let Some(v) = &snap.cast {
            return Some((v.device, v.name.clone()));
        }
        let id = snap.play_on.filter(|_| !self.window().multiview)?;
        let name = self.shared.cast().devices().into_iter().find(|d| d.id == id).map_or_else(|| "your TV".to_owned(), |d| d.name);
        Some((id, name))
    }

    async fn status(self: &Arc<Self>) -> Out<serde_json::Value> {
        let snap = self.player.snapshot();
        let mut more = self.status_more();
        more.casting = act::casting_out(&snap, &self.cast_last());
        // where a play without `device` goes: not with two pictures, as `lands_on`
        more.play_on = act::play_on_out(&snap, &self.shared.cast().devices()).filter(|_| !self.window().multiview);
        let v = self.run(false, move |q| act::status_with(q, &snap, &more)).await?;
        let mut out = to_value(&v)?;
        if let Some(handle) = self.shared.downloads() {
            let snap = handle.snapshot();
            let (view, _) = self.view();
            let jobs: Vec<_> = snap.jobs.iter().filter(|j| !view.hides_explicit || !j.explicit()).collect();
            out["downloads"] = serde_json::json!({"running":jobs.iter().filter(|j| j.state.active()).count(),"waiting":jobs.iter().filter(|j| j.state == crate::downloads::State::Waiting).count(),"used_gb":snap.used_bytes as f64 / 1e9});
        }
        Ok(out)
    }

    /// The parental state and the snapshot as it may be shown now.
    pub(super) fn view(&self) -> (ParentalView, Arc<crate::config::Config>) {
        let cfg = crate::config::current(&self.config);
        (self.shared.parental(&cfg.parental), cfg)
    }

    /// The published snapshot, without explicit content when the session hides it now
    /// (blocking: a copy of the indexes; call on the blocking pool).
    fn library(&self, view: ParentalView) -> Arc<Library> {
        let lib = self.shared.library();
        let mut cached = self.concealed.lock().unwrap_or_else(|e| e.into_inner());
        if !(view.hides_explicit && lib.shows_explicit()) {
            // a copy made for an older snapshot (or before an unlock) isn't needed any more
            if cached.as_ref().is_some_and(|(generation, _)| *generation != lib.generation || !view.hides_explicit) {
                drop(cached.take());
            }
            return lib;
        }
        if let Some((generation, hidden)) = cached.as_ref()
            && *generation == lib.generation
        {
            return hidden.clone();
        }
        let hidden = Arc::new(lib.hiding_explicit());
        *cached = Some((lib.generation, hidden.clone()));
        hidden
    }

    /// Run `f` with a reader (pooled) on the blocking pool.
    fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Out<T>) -> Out<T> {
        let conn = self.readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let conn = match conn {
            Some(c) => c,
            None => self.store.reader().map_err(Fail::from)?,
        };
        let out = f(&conn);
        let mut pool = self.readers.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < READERS {
            pool.push(conn);
        }
        out
    }

    /// `f` over a [`Q`] on the blocking pool. `needs_library`: `loading` until the app published
    /// its first snapshot.
    pub(super) async fn run<T: Send + 'static>(self: &Arc<Self>, needs_library: bool, f: impl FnOnce(&Q) -> Out<T> + Send + 'static) -> Out<T> {
        let me = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            let (view, cfg) = me.view();
            if needs_library && me.shared.library().built_at == 0 {
                return Err(Fail::new(ErrorCode::Loading, "Tuner is still loading your playlists; try again in a few seconds"));
            }
            let lib = me.library(view);
            me.with_reader(|conn| f(&Q::new(&lib, conn, view, crate::model::now(), &cfg)))
        });
        task.await.unwrap_or_else(|e| {
            tracing::warn!("agent: request failed: {e}");
            Err(Fail::new(ErrorCode::Internal, "Tuner couldn't answer that"))
        })
    }

    pub(super) async fn query<T: Serialize + Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Q) -> Out<T> + Send + 'static,
    ) -> Out<serde_json::Value> {
        let v = self.run(true, f).await?;
        to_value(&v)
    }

    async fn details(self: &Arc<Self>, p: super::proto::Details) -> Out<serde_json::Value> {
        let again = p.clone();
        let (mut out, job) = self.run(true, move |q| query::details(q, &p)).await?;
        if let Some(job) = job {
            self.jobs.submit(job);
            let deadline = Instant::now() + DETAILS_WAIT;
            while out.details_pending && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let p = again.clone();
                out = self.run(true, move |q| query::details(q, &p)).await?.0;
            }
        }
        to_value(&out)
    }

    async fn play(self: &Arc<Self>, p: super::proto::Play) -> Out<serde_json::Value> {
        let asked = act::parse_open_in(p.open_in.as_deref())?;
        // a cast: the TV first (a TV that can't take it needs no look at the item)
        let target = match p.device.as_deref() {
            Some(d) => Some(self.cast_target(d).await?),
            None => None,
        };
        let again = p.clone();
        let mut step = self.run(true, move |q| act::plan_play(q, &p)).await?;
        if let PlayStep::NeedsDetails(job) = step {
            self.jobs.submit(job);
            let deadline = Instant::now() + DETAILS_WAIT;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let p = again.clone();
                step = self.run(true, move |q| act::plan_play(q, &p)).await?;
                if !matches!(step, PlayStep::NeedsDetails(_)) {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(Fail::new(ErrorCode::Unavailable, "Tuner is still loading this series' episodes; try play again in a few seconds"));
                }
            }
        }
        let PlayStep::Ready { mut command, mut out, log_ref } = step else {
            return Err(Fail::new(ErrorCode::Internal, "Tuner couldn't decide what to play"));
        };
        if let Some((device, true)) = &target {
            return to_value(&act::pairing_required(&out, device));
        }
        if let Some(handle) = self.shared.downloads() {
            let wanted = command.clone();
            let resolved = self
                .run(true, move |q| {
                    let super::AgentCommand::Play(play) = wanted else { return Ok(None) };
                    let media = match play.target {
                        super::PlayTarget::Media(m) => m,
                        super::PlayTarget::Series(id) => {
                            let copies =
                                q.lib.vod.index_of(crate::model::VodKind::Series(id)).map(|i| q.series_copies(i)).unwrap_or_else(|| vec![id]);
                            let Some((_, ep)) = crate::store::copies::next_episode(q.conn, &copies)? else { return Ok(None) };
                            crate::model::MediaRef::Episode(ep.id)
                        }
                    };
                    let p = crate::sources::resolve::resolve_with_view(q.conn, q.cfg, &media, true, q.now, q.lib, q.view)?;
                    Ok(p.source_id.map(|source| (source, media)))
                })
                .await?;
            if let Some((source, media)) = resolved {
                let budget = handle.budget();
                if !budget.admits(crate::model::TileId::A, source)
                    && !budget.has_reader(&media)
                    && let Some(id) = budget.download_holder(source)
                {
                    let snapshot = handle.snapshot();
                    let job = snapshot.jobs.iter().find(|j| j.id == id);
                    if again.pause_downloads != Some(true) {
                        let mut busy = job.map(super::downloads::label).unwrap_or_else(|| serde_json::json!({}));
                        busy["id"] = id.0.into();
                        busy["what"] = if job.is_some_and(|j| j.kind.recording()) { "recording" } else { "download" }.into();
                        busy["progress"] = job.and_then(|j| j.progress()).into();
                        return Ok(serde_json::json!({"busy":busy}));
                    }
                    if out.outcome == super::proto::PlayOutcome::PinRequired {
                        if let super::AgentCommand::Play(play) = &mut command {
                            play.pause_after_pin = Some(source);
                        }
                    } else {
                        let previous = budget.claim_of(crate::model::TileId::A);
                        budget.claim(crate::model::TileId::A, Some(source));
                        let result = handle.request(crate::downloads::DownloadCommand::PauseForPlay(source)).await;
                        if result.error.is_some() {
                            budget.claim(crate::model::TileId::A, previous);
                            return Err(Fail::new(ErrorCode::Busy, "The account has not closed yet"));
                        }
                    }
                }
            }
        }
        // the PIN is typed in Tuner's window: the prompt brings it forward
        let pin = out.outcome == super::proto::PlayOutcome::PinRequired;
        if let Some((device, unpaired)) = &target {
            if *unpaired {
                tracing::info!("agent: play {log_ref} → pairing_required (cast device {})", device.id.short());
                return to_value(&act::pairing_required(&out, device));
            }
            act::for_device(&mut command, &mut out, device);
        }
        // without `device` while casting, the play lands on that TV too; and with Tuner's Play on
        // switch on a TV (DESIGN §40 §3.9), on the switch's TV (single view: two pictures are
        // this computer's)
        let casting = if target.is_none() { self.lands_on(&self.player.snapshot()) } else { None };
        // a cast never places the window (DESIGN §40 §3.8): only the PIN prompt raises it
        let (show, w) = match (target.is_some() || casting.is_some(), pin) {
            (true, false) => (act::AgentShow::IN_PLACE, self.window()),
            (true, true) => (act::AgentShow::window(true), self.window()),
            (false, _) => self.show(asked, pin).await,
        };
        if let super::AgentCommand::Play(play) = &mut command {
            play.show = show;
        }
        self.bus.send(AppEvent::Agent(command));
        let opened = Self::raise_for(show, w);
        out.opened_in.get_or_insert(opened);
        if let Some((device, name)) = &casting {
            act::on_the_tv(&mut out, name);
            tracing::info!("agent: play {log_ref} → {:?} on cast device {}", out.outcome, device.short());
            return to_value(&out);
        }
        if let Some((device, _)) = &target {
            tracing::info!("agent: play {log_ref} → {:?} on cast device {}", out.outcome, device.id.short());
            return to_value(&out);
        }
        if !pin {
            match show.place {
                Placement::PopOut => out.message.push_str(" It's in the pop-out."),
                Placement::FullWindow if asked == Some(OpenIn::PopOut) => {
                    out.message.push_str(" It's in Tuner's window (the pop-out needs Hyprland).")
                }
                Placement::FullWindow if show.fullscreen => out.message.push_str(" It's full screen in Tuner's window."),
                Placement::FullWindow => out.message.push_str(" It's in Tuner's window."),
                Placement::InPlace => {}
            }
        }
        tracing::info!("agent: play {log_ref} → {:?} in {opened:?}", out.outcome);
        to_value(&out)
    }

    async fn schedule_switch(self: &Arc<Self>, p: super::proto::ScheduleSwitch) -> Out<serde_json::Value> {
        let plan = self.run(true, move |q| act::plan_switch(q, &p)).await?;
        let reminder = plan.reminder.clone();
        let late = crate::model::now() - crate::platform::notify::LATE_SWITCH_S;
        let written = self
            .store
            .writer()
            .submit_notify(DataScope::Reminders, move |c| {
                let pending = crate::store::user::pending_switches(c)?;
                if let Some(same) = pending.iter().find(|x| x.channel == reminder.channel && x.start == reminder.start) {
                    return Ok(Some(same.id));
                }
                // rows that will still fire (hidden ones count too; the message names no number)
                if pending.iter().filter(|x| x.start >= late).count() >= act::MAX_SWITCHES {
                    return Ok(None);
                }
                // a notification reminder of the same programme becomes the switch
                let id = crate::store::user::add_reminder(c, &reminder)?;
                crate::store::user::set_reminder_switch(c, id)?;
                Ok(Some(id))
            })
            .recv()
            .await
            .map_err(Fail::from)?;
        let Some(id) = written else {
            return Err(Fail::new(ErrorCode::Unavailable, "Too many switches are scheduled; cancel one first"));
        };
        self.shared.wake_reminders();
        tracing::info!("agent: schedule_switch {} → reminder {}", plan.log_ref, id.0);
        let mut out = plan.out;
        out.id = id.0;
        to_value(&out)
    }

    /// The pictures a `control` reaches: the one named, else every picture for pause / resume /
    /// stop and the one with the sound for volume and mute.
    fn control_targets(&self, c: &Control) -> Out<Vec<PlayerHandle>> {
        let tiles = self.shared.tiles();
        let get = |t: crate::model::TileId| match &self.tiles {
            Some(reg) => reg.get(t),
            None => (t == crate::model::TileId::A).then(|| self.player.clone()),
        };
        if let Some(t) = c.tile {
            return get(t.id()).map(|h| vec![h]).ok_or_else(|| Fail::new(ErrorCode::Unavailable, "That picture isn't open (multi-view is off)"));
        }
        Ok(match c.action {
            ControlAction::Volume | ControlAction::Mute | ControlAction::Unmute => {
                get(tiles.audible).or_else(|| get(crate::model::TileId::A)).into_iter().collect()
            }
            // the focused picture (A while multi-view is off)
            ControlAction::PreviousChannel | ControlAction::Popout | ControlAction::Window => {
                get(tiles.focus).or_else(|| get(crate::model::TileId::A)).into_iter().collect()
            }
            // casting is picture A's (multi-view can't be on while it casts)
            ControlAction::StopCasting | ControlAction::PlayHere => get(crate::model::TileId::A).into_iter().collect(),
            _ => match &self.tiles {
                Some(reg) => reg.all(),
                None => vec![self.player.clone()],
            },
        })
    }

    async fn control(self: &Arc<Self>, c: Control) -> Out<serde_json::Value> {
        if c.value.is_some() && c.action != ControlAction::Volume {
            return Err(Fail::bad("value is only for volume"));
        }
        let targets = self.control_targets(&c)?;
        let Some(first) = targets.first().cloned() else { return Err(Fail::new(ErrorCode::Unavailable, "Nothing is playing")) };
        let snap = first.snapshot();
        let idle = |s: &crate::player::PlayerSnapshot| matches!(s.state, crate::player::PlayerState::Idle | crate::player::PlayerState::Ended);
        let view = self.view().0;
        let tv_stopped = snap.cast.is_none() && idle(&snap) && self.cast_last().error == Some(crate::cast::CastErrorKind::StoppedOnTv);
        if let Some(refused) = act::cast_control_guard(c.action, snap.cast.as_ref(), tv_stopped) {
            return Err(refused);
        }
        let cmd = match c.action {
            ControlAction::Pause | ControlAction::Resume | ControlAction::TogglePause | ControlAction::Popout | ControlAction::Window
                if idle(&snap) =>
            {
                return Err(Fail::new(ErrorCode::Unavailable, "Nothing is playing"));
            }
            ControlAction::Pause => Some(PlayerCommand::SetPause(true)),
            ControlAction::Resume => Some(PlayerCommand::SetPause(false)),
            ControlAction::TogglePause => Some(PlayerCommand::TogglePause),
            ControlAction::Stop => Some(PlayerCommand::Stop),
            ControlAction::Volume => {
                let v = c.value.ok_or_else(|| Fail::bad("volume needs value (0–130)"))?;
                if !(0.0..=130.0).contains(&v) {
                    return Err(Fail::bad("volume is 0–130"));
                }
                Some(PlayerCommand::SetVolume(v))
            }
            ControlAction::Mute => (!snap.muted).then_some(PlayerCommand::ToggleMute),
            ControlAction::Unmute => snap.muted.then_some(PlayerCommand::ToggleMute),
            // the player keeps the PIN rule: a locked channel isn't recalled while locked
            ControlAction::PreviousChannel => {
                if snap.previous_live.is_none() {
                    return Err(Fail::new(ErrorCode::Unavailable, "There is no previous channel to go back to"));
                }
                Some(PlayerCommand::LastChannel { unlocked: view.unlocked })
            }
            ControlAction::StopCasting => Some(PlayerCommand::StopCasting { play_here: false }),
            ControlAction::PlayHere => Some(PlayerCommand::StopCasting { play_here: true }),
            ControlAction::Popout => {
                self.bus.send(AppEvent::Agent(super::AgentCommand::PopOut));
                tracing::info!("agent: control Popout");
                None
            }
            // like `play` with `open_in: full_window`, without playing it again
            ControlAction::Window => {
                let show = act::AgentShow { fullscreen: true, ..act::AgentShow::window(true) };
                self.bus.send(AppEvent::Agent(super::AgentCommand::Show(show)));
                crate::platform::hypr::focus_self();
                tracing::info!("agent: control Window");
                None
            }
        };
        if let Some(cmd) = cmd {
            let was_paused = snap.paused;
            let was_gen = snap.load_generation;
            // the change this asked for, as the (first) player shows it
            let done = |s: &crate::player::PlayerSnapshot| match c.action {
                ControlAction::Pause => s.paused,
                ControlAction::Resume => !s.paused,
                ControlAction::TogglePause => s.paused != was_paused,
                ControlAction::Stop => idle(s),
                ControlAction::Volume => c.value.is_some_and(|v| (s.volume - v).abs() < 0.5),
                ControlAction::Mute => s.muted,
                ControlAction::Unmute => !s.muted,
                ControlAction::PreviousChannel => s.load_generation != was_gen,
                ControlAction::Popout | ControlAction::Window => true,
                // after the TV stopped it: the last item playing here again
                ControlAction::PlayHere if tv_stopped => s.load_generation != was_gen,
                ControlAction::StopCasting | ControlAction::PlayHere => s.cast.is_none(),
            };
            let stops = matches!(c.action, ControlAction::StopCasting | ControlAction::PlayHere) && !tv_stopped;
            let wait = if stops { CAST_STOP_WAIT } else { CONTROL_WAIT };
            let mut rx = first.subscribe();
            rx.borrow_and_update();
            for t in &targets {
                // a toggle per picture would flip them apart: pause / resume them all alike
                let cmd = match (&cmd, targets.len() > 1) {
                    (PlayerCommand::TogglePause, true) => PlayerCommand::SetPause(!was_paused),
                    (cmd, _) => cmd.clone(),
                };
                t.send(cmd);
            }
            let _ = tokio::time::timeout(wait, async {
                while !done(&rx.borrow_and_update()) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            tracing::info!("agent: control {:?}", c.action);
        }
        self.status().await
    }
}
