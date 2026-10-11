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
//!   sends `AppEvent::Agent(AgentCommand::Play)` + `hypr::focus_self()`: the app applies the PIN
//!   rules (prompt on screen), zap lists and resume rules exactly as for a key press;
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
use crate::msg::{AppEvent, DataScope, EventBus};
use crate::player::{PlayerCommand, PlayerHandle};
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

/// The production [`AgentService`].
#[derive(Clone)]
pub struct AgentHandler {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<AppShared>,
    store: Store,
    config: SharedConfig,
    player: PlayerHandle,
    bus: EventBus,
    jobs: JobManager,
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
            inner: Arc::new(Inner { shared, store, config, player, bus, jobs, limits, readers: Mutex::new(Vec::new()), concealed: Mutex::new(None) }),
        }
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
            let reply = match tokio::time::timeout(HANDLER_BUDGET, inner.clone().dispatch(req.call)).await {
                // every result passes the spoiler guard's one choke point (B4)
                Ok(Ok(mut v)) => {
                    guard::guard_reply(&mut v, &inner.shared.guard(), crate::guard::Policy::Normal);
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

fn to_value<T: Serialize>(v: &T) -> Out<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| Fail::new(ErrorCode::Internal, format!("encoding the answer: {e}")))
}

impl Inner {
    async fn dispatch(self: Arc<Self>, call: Call) -> Out<serde_json::Value> {
        match call {
            Call::GuideSearch(p) => self.query(move |q| query::guide_search(q, &p)).await,
            Call::WhatsOn(p) => self.query(move |q| query::whats_on(q, &p)).await,
            Call::Search(p) => self.query(move |q| query::search(q, &p)).await,
            Call::Categories(p) => self.query(move |q| query::categories(q, &p)).await,
            Call::Browse(p) => self.query(move |q| query::browse(q, &p)).await,
            Call::ContinueWatching(p) => self.query(move |q| query::continue_watching(q, &p)).await,
            Call::Favourites(p) => self.query(move |q| query::favourites(q, &p)).await,
            Call::Details(p) => self.details(p).await,
            Call::Status(_) => {
                let snap = self.player.snapshot();
                let v = self.run(false, move |q| act::status(q, &snap)).await?;
                to_value(&v)
            }
            Call::Play(p) => self.play(p).await,
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
            // the batch's tools: listed by `tools::tools()` only once their feature is on; until
            // then a client that calls one anyway hears so (owner: agent track)
            call @ (Call::Queue(_)
            | Call::Follow(_)
            | Call::SpoilerGuard(_)
            | Call::GuideFix(_)
            | Call::Multiview(_)
            | Call::Reminders(_)
            | Call::SleepTimer(_)
            | Call::MatchTitles(_)
            | Call::History(_)
            | Call::Episodes(_)
            | Call::FavouritesEdit(_)
            | Call::ParentalStatus(_)
            | Call::Lock(_)
            | Call::Diagnose(_)
            | Call::Sources(_)) => Err(Fail::new(ErrorCode::Unsupported, format!("{} isn't available in this Tuner yet", call.name()))),
        }
    }

    /// The parental state and the snapshot as it may be shown now.
    fn view(&self) -> (ParentalView, Arc<crate::config::Config>) {
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
    async fn run<T: Send + 'static>(self: &Arc<Self>, needs_library: bool, f: impl FnOnce(&Q) -> Out<T> + Send + 'static) -> Out<T> {
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

    async fn query<T: Serialize + Send + 'static>(self: &Arc<Self>, f: impl FnOnce(&Q) -> Out<T> + Send + 'static) -> Out<serde_json::Value> {
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
        let PlayStep::Ready { command, out, log_ref } = step else {
            return Err(Fail::new(ErrorCode::Internal, "Tuner couldn't decide what to play"));
        };
        self.bus.send(AppEvent::Agent(command));
        crate::platform::hypr::focus_self();
        tracing::info!("agent: play {log_ref} → {:?}", out.outcome);
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

    async fn control(self: &Arc<Self>, c: Control) -> Out<serde_json::Value> {
        let snap = self.player.snapshot();
        let idle = matches!(snap.state, crate::player::PlayerState::Idle | crate::player::PlayerState::Ended);
        let cmd = match c.action {
            ControlAction::Pause | ControlAction::Resume | ControlAction::TogglePause if idle => {
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
        };
        if c.value.is_some() && c.action != ControlAction::Volume {
            return Err(Fail::bad("value is only for volume"));
        }
        if let Some(cmd) = cmd {
            let was_paused = snap.paused;
            // the change this asked for, as the player shows it
            let done = |s: &crate::player::PlayerSnapshot| match c.action {
                ControlAction::Pause => s.paused,
                ControlAction::Resume => !s.paused,
                ControlAction::TogglePause => s.paused != was_paused,
                ControlAction::Stop => matches!(s.state, crate::player::PlayerState::Idle | crate::player::PlayerState::Ended),
                ControlAction::Volume => c.value.is_some_and(|v| (s.volume - v).abs() < 0.5),
                ControlAction::Mute => s.muted,
                ControlAction::Unmute => !s.muted,
            };
            let mut rx = self.player.subscribe();
            rx.borrow_and_update();
            self.player.send(cmd);
            let _ = tokio::time::timeout(CONTROL_WAIT, async {
                while !done(&rx.borrow_and_update()) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            tracing::info!("agent: control {:?}", c.action);
        }
        let snap = self.player.snapshot();
        let v = self.run(false, move |q| act::status(q, &snap)).await?;
        to_value(&v)
    }
}
