//! Desktop notifications (notify-rust over zbus) + the reminder scheduler. Owners: WP-08, the
//! remind track (MCP pass 2). DESIGN §21.3, §30.8.
//!
//! Omarchy renders freedesktop notifications in its shell (accent border). Reminders:
//! * The scheduler task loads the pending reminders at start and whenever the app calls
//!   [`ReminderScheduler::refresh`] (on `DataScope::Reminders`; the MCP handler and `tuner
//!   --reminder <id>` wake it through [`ReminderWaker`]), sleeps until the next fire time, then
//!   sends a notification "‹programme› starts in 2 min on ‹channel›" with actions `watch`
//!   ("Watch") and `default`; marks it `Fired`. Missed reminders (app was closed) older than
//!   their stop time are marked `Dismissed` silently; ones still airing fire immediately with
//!   "started N min ago". Titles pass the spoiler guard (`Guard::outside_title`).
//! * Action "watch" (or a click on the notification body) → `PlatformEvent::ReminderWatch`
//!   (UI resolves the channel by ItemRef, plays it and calls `hypr::focus_self`).
//! * No notification server → the same text as an in-app toast.
//!
//! Timing uses the wall clock and never sleeps longer than a minute, so suspend/resume and
//! clock changes are picked up. A reminder is about `start + offset_s`; a notification goes out
//! its own `lead_s` before (else `reminders.lead_min`), see `crate::reminders`.
//!
//! Automatic switches (DESIGN §30.8, `Reminder::switches`) run whatever `reminders.enabled`
//! says (with reminders off only notification reminders are skipped). At fire time
//! `reminders::fire_policy` decides (B1/B6): a switch goes ahead (a Cancel countdown
//! `SWITCH_WARN_S` before, then the switch), becomes a notification with Watch (a Following
//! switch would interrupt a film or episode, or its channel needs the PIN), or does nothing
//! (the channel already plays; a news flash in quiet hours). It is decided again at the switch:
//! the user may have started a film during the countdown. A switch missed while Tuner was
//! closed still happens up to [`LATE_SWITCH_S`] late, saying how late (`reminders::Late`);
//! older ones are dismissed silently. The window is raised only for "Open in: Full window":
//! the pop-out (default) and "Where I am" never take focus.
//!
//! Events: until `reminders::ENABLED` the scheduler sends the pass-1 pair
//! `PlatformEvent::{SwitchSoon, SwitchNow}` (and raises the window); from then on
//! `AppEvent::Reminder(ReminderEvent::{SwitchSoon, SwitchNow, NotSwitched})`, which carry the
//! route and where to open.
//!
//! While Tuner is closed, transient systemd timers fire reminders (`platform::systemd`,
//! `reminders::arming`): the scheduler keeps them in step with the pending reminders on every
//! reload when "Fire while Tuner is closed" is on.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use notify_rust::{Hint, Notification, NotificationResponse, Urgency};

use crate::config::SharedConfig;
use crate::error::{Error, Result};
use crate::model::{MediaRef, OpenIn, PlayOrigin, Reminder, ReminderId, ReminderState, Timestamp};
use crate::msg::{AppEvent, DataScope, EventBus, Toast, ToastKind};
use crate::platform::PlatformEvent;
use crate::platform::systemd::{ArmSync, Armer};
use crate::reminders::{FireAs, FireCtx, Late, NotSwitched, NotifyWhy, ReminderEvent};
use crate::shared::AppShared;
use crate::store::Store;

/// How long `notify` waits for the notification server.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest scheduler sleep (wall-clock re-check, see module docs).
const MAX_SLEEP_S: i64 = 60;
/// How long a shown reminder keeps listening for its "Watch" action.
const ACTION_WAIT: Duration = Duration::from_secs(3 * 3600);
/// How late an automatic switch still happens (Tuner was closed at the start, or asleep).
pub const LATE_SWITCH_S: i64 = 900;

/// Fire-and-forget notification (errors logged). Summary/body must be redacted already.
/// Returns false when no notification server answered — callers fall back to an in-app
/// toast (DESIGN §13.12 "Notifications unavailable").
///
/// Blocks for one D-Bus round trip (at most 5 s): call it from a worker, not the UI thread.
/// Works inside or outside a tokio runtime (the call runs on its own helper thread).
pub fn notify(summary: &str, body: &str, urgency_critical: bool) -> bool {
    notify_at(None, summary, body, urgency_critical)
}

/// [`notify`] against `org.freedesktop.Notifications` (`bus = None`) or notify-rust's test bus
/// namespace `de.hoodie.Notification.<bus>`.
fn notify_at(bus: Option<String>, summary: &str, body: &str, urgency_critical: bool) -> bool {
    let n = build(summary, body, urgency_critical, &[]);
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("notify".into()).spawn(move || {
        let shown = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(async { show(&n, bus.as_deref()).await.map(drop) }),
            Err(e) => Err(Error::other(e)),
        };
        if let Err(e) = &shown {
            tracing::debug!("notification not shown: {e}");
        }
        let _ = tx.send(shown.is_ok());
    });
    spawned.is_ok() && rx.recv_timeout(NOTIFY_TIMEOUT).unwrap_or(false)
}

/// A reminder note with Watch from a process without a window (`reminders::closed`): true when
/// the user picked Watch. Blocks until then, until the note is dismissed, or for at most the
/// action wait (3 h); false at once without a notification server. Call it outside a runtime.
pub fn show_and_wait_watch(summary: &str, body: &str) -> bool {
    show_and_wait_watch_at(None, summary, body, ACTION_WAIT)
}

fn show_and_wait_watch_at(bus: Option<String>, summary: &str, body: &str, wait: Duration) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return false };
    let note = Note { summary: summary.to_owned(), body: body.to_owned() };
    rt.block_on(async move {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let notifier = DesktopNotifier { bus };
        if !notifier
            .show_reminder(
                note,
                Box::new(move || {
                    let _ = tx.send(());
                }),
            )
            .await
        {
            return false;
        }
        matches!(tokio::time::timeout(wait, rx).await, Ok(Ok(())))
    })
}

fn build(summary: &str, body: &str, urgency_critical: bool, actions: &[(&str, &str)]) -> Notification {
    let mut n = Notification::new();
    n.appname(crate::APP_NAME)
        .summary(&crate::redact::redact(summary))
        .body(&crate::redact::redact(body))
        .icon(crate::APP_ID)
        .hint(Hint::DesktopEntry(crate::APP_ID.into()))
        .urgency(if urgency_critical { Urgency::Critical } else { Urgency::Normal });
    for (id, label) in actions {
        n.action(id, label);
    }
    n
}

async fn show(n: &Notification, bus: Option<&str>) -> Result<notify_rust::NotificationHandle> {
    // Tests run real services (jobs, the scheduler) and must never reach the desktop's
    // notification server: only the fake test buses answer them (callers fall back to a toast).
    if bus.is_none() && crate::platform::off_desktop() {
        return Err(Error::other("no desktop notifications off the desktop (tests)"));
    }
    match bus {
        None => n.show_async().await,
        Some(b) => n.show_async_at_bus(b).await,
    }
    .map_err(Error::other)
}

pub struct ReminderScheduler {
    task: Option<tokio::task::JoinHandle<()>>,
    wake: tokio::sync::mpsc::UnboundedSender<()>,
    /// Its settings (read live by the pass-2 scheduler).
    backend: Arc<dyn ReminderBackend>,
}

/// Asks a running scheduler to reload its reminders (the agent handler's switches, which
/// must not wait for a UI frame; `tuner --reminder <id>` handed to a running Tuner). Harmless
/// once the scheduler is gone.
#[derive(Debug, Clone)]
pub struct ReminderWaker(tokio::sync::mpsc::UnboundedSender<()>);

impl ReminderWaker {
    pub fn wake(&self) {
        let _ = self.0.send(());
    }
}

/// What the pass-2 scheduler runs with ([`ReminderScheduler::start_with`]).
pub struct SchedulerDeps {
    pub store: Store,
    pub bus: EventBus,
    /// The pictures, the main player, Up next, the guard, the parental session, the explicit
    /// gate (B2).
    pub shared: Arc<AppShared>,
    /// The live config (`[reminders]`, `follow.interrupt_vod`), read on every reload.
    pub config: SharedConfig,
    /// Transient systemd timers (`SystemdArmer::for_profile`); `None` never arms.
    pub armer: Option<Arc<dyn Armer>>,
}

impl ReminderScheduler {
    /// Start the pass-1 scheduler on `rt` (no fire context: every switch goes ahead; nothing is
    /// armed). Kept for `Services::start_reminders` until it moves to
    /// [`ReminderScheduler::start_with`]. `notify`: notification reminders are on
    /// (`reminders.enabled`); automatic switches run either way. `explicit`: while it hides
    /// explicit content, reminders on explicit channels stay silent (they are announced once it
    /// shows again, if still airing; DESIGN §20.3).
    pub fn start(
        rt: &tokio::runtime::Handle,
        store: Store,
        bus: EventBus,
        lead_default_s: i64,
        notify: bool,
        explicit: crate::store::library::ExplicitGate,
    ) -> Result<Self> {
        let settings = Settings { lead_default_s, notify, open_in: OpenIn::FullWindow, when_closed: false, rich: false };
        let backend = Arc::new(StoreBackend { store, reader: Mutex::new(None), explicit, live: None, fixed: settings });
        Ok(Self::spawn(rt, backend, Arc::new(DesktopNotifier { bus: None }), bus, None))
    }

    /// Start the pass-2 scheduler: fire context from `deps.shared`, settings read live, systemd
    /// arming. Never fails; storage errors are logged and retried on the next refresh / minute.
    pub fn start_with(rt: &tokio::runtime::Handle, deps: SchedulerDeps) -> Self {
        let fixed = settings_of(&crate::config::current(&deps.config));
        let backend = Arc::new(StoreBackend {
            store: deps.store,
            reader: Mutex::new(None),
            explicit: deps.shared.explicit().clone(),
            live: Some(Live { shared: deps.shared, config: deps.config }),
            fixed,
        });
        let arm = deps.armer.map(|a| Arc::new(Mutex::new(ArmSync::new(a))));
        Self::spawn(rt, backend, Arc::new(DesktopNotifier { bus: None }), deps.bus, arm)
    }

    fn spawn(
        rt: &tokio::runtime::Handle,
        backend: Arc<dyn ReminderBackend>,
        notifier: Arc<dyn Notifier>,
        bus: EventBus,
        arm: Option<Arc<Mutex<ArmSync>>>,
    ) -> Self {
        let (wake, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = rt.spawn(run(backend.clone(), notifier, bus, arm, rx));
        Self { task: Some(task), wake, backend }
    }

    /// Reload pending reminders (after add/remove).
    pub fn refresh(&self) {
        let _ = self.wake.send(());
    }

    /// A handle that does what [`ReminderScheduler::refresh`] does, from anywhere.
    pub fn waker(&self) -> ReminderWaker {
        ReminderWaker(self.wake.clone())
    }

    /// Notification reminders are on (automatic switches run either way).
    pub fn notifies(&self) -> bool {
        self.backend.settings().notify
    }
}

/// What the scheduler runs with (re-read from the live config on every reload).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Settings {
    /// `reminders.lead_min` in seconds: the lead of rows without their own.
    lead_default_s: i64,
    /// Notification reminders are on (switches run regardless).
    notify: bool,
    /// Where scheduled playback opens (`reminders.open_in`; a reminder's own choice wins).
    open_in: OpenIn,
    /// Arm systemd timers (`reminders.when_closed`, pass 2 enabled).
    when_closed: bool,
    /// Pass-2 events (`reminders::ENABLED`).
    rich: bool,
}

fn settings_of(cfg: &crate::config::Config) -> Settings {
    let r = &cfg.reminders;
    Settings {
        lead_default_s: i64::from(r.lead_min) * 60,
        notify: r.enabled,
        open_in: r.open_in,
        when_closed: r.when_closed && crate::reminders::ENABLED,
        rich: crate::reminders::ENABLED,
    }
}

impl Drop for ReminderScheduler {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// What the scheduler needs to know at fire time.
#[derive(Debug, Clone)]
struct FireFacts {
    ctx: FireCtx,
    /// The channel has catch-up (a late switch may start from the beginning).
    catchup: bool,
}

/// Reminder persistence and the app's state as the scheduler sees them (the store and
/// `AppShared` in production, fakes in tests).
trait ReminderBackend: Send + Sync + 'static {
    /// Every reminder still `Pending` (blocking; called on the blocking pool).
    fn pending(&self) -> Result<Vec<Reminder>>;
    /// Persist a state change (fire-and-forget, must not block).
    fn set_state(&self, id: ReminderId, state: ReminderState);
    /// The settings now.
    fn settings(&self) -> Settings;
    /// Fire-time facts for `r` (blocking: one indexed query).
    fn fire_facts(&self, r: &Reminder, now: Timestamp) -> FireFacts;
}

/// The live parts of a pass-2 scheduler.
struct Live {
    shared: Arc<AppShared>,
    config: SharedConfig,
}

struct StoreBackend {
    store: Store,
    reader: Mutex<Option<rusqlite::Connection>>,
    explicit: crate::store::library::ExplicitGate,
    /// `None`: the pass-1 scheduler (no fire context).
    live: Option<Live>,
    /// The settings when there is no live config.
    fixed: Settings,
}

impl StoreBackend {
    fn with_reader<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(self.store.reader()?);
        }
        let conn = guard.as_ref().ok_or_else(|| Error::other("no database reader"))?;
        f(conn)
    }
}

/// What a picture plays, for the router (`None`: nothing, or it ended / failed).
fn kind_of(p: &crate::player::PlayerSnapshot) -> Option<crate::playback::Kind> {
    use crate::model::PlayableKind as K;
    use crate::playback::Kind;
    let on = p.state.is_active() || p.state == crate::player::PlayerState::Paused;
    p.playable.as_ref().filter(|_| on).map(|pl| match pl.kind {
        K::Live => Kind::Live,
        K::Catchup { .. } => Kind::Catchup,
        K::Movie | K::Episode { .. } | K::Url => Kind::Vod,
    })
}

impl ReminderBackend for StoreBackend {
    fn pending(&self) -> Result<Vec<Reminder>> {
        self.with_reader(|conn| {
            // A panic in the repository must not kill the scheduler.
            let mut pending = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::user::pending_reminders(conn, Timestamp::MIN)))
                .unwrap_or_else(|_| Err(Error::other("reminder storage is not available")))?;
            // explicit channels' reminders wait, untouched, while parental control hides them: a
            // notification would name the programme and the channel (a channel that can't be
            // read counts as explicit)
            if !self.explicit.visibility().explicit {
                pending.retain(|r| !crate::store::live::item_explicit(conn, &r.channel).unwrap_or(true));
            }
            // Following off: its reminders wait, untouched
            if self.live.as_ref().is_some_and(|l| !crate::config::current(&l.config).follow.enabled) {
                pending.retain(|r| r.rule_id.is_none());
            }
            Ok(pending)
        })
    }

    fn set_state(&self, id: ReminderId, state: ReminderState) {
        let _ = self.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::set_reminder_state(c, id, state));
    }

    fn settings(&self) -> Settings {
        self.live.as_ref().map_or(self.fixed, |l| settings_of(&crate::config::current(&l.config)))
    }

    fn fire_facts(&self, r: &Reminder, now: Timestamp) -> FireFacts {
        let Some(live) = &self.live else { return FireFacts { ctx: FireCtx::idle(now), catchup: false } };
        let cfg = crate::config::current(&live.config);
        let shared = &live.shared;
        let main = shared.main_player().map(|rx| rx.borrow().clone());
        let lib = shared.library();
        let channel = lib.live.by_ref(&r.channel);
        let main_kind = main.as_deref().and_then(kind_of);
        // the live channels on either picture (multi-view's silent one too: playing it twice
        // would take a second connection)
        let on_screen: Vec<crate::model::ChannelId> = [crate::model::TileId::A, crate::model::TileId::B]
            .into_iter()
            .filter_map(|t| shared.tile_player(t).map(|rx| rx.borrow().clone()))
            .filter(|p| kind_of(p).is_some())
            .filter_map(|p| match p.playable.as_ref().map(|pl| &pl.media) {
                Some(MediaRef::Channel(c)) => Some(*c),
                _ => None,
            })
            .collect();
        let target_playing = channel.is_some_and(|c| on_screen.contains(&c));
        let queue = shared.queue();
        let queue_running = queue.current.is_some() && !queue.interrupted && main.as_ref().is_some_and(|p| p.origin == Some(PlayOrigin::Queue));
        let unlocked = shared.parental(&cfg.parental).unlocked;
        // a channel that can't be read counts as locked
        let target_locked = !unlocked && self.with_reader(|c| crate::store::user::channel_locked(c, &r.channel)).unwrap_or(true);
        let tiles = shared.tiles();
        // a news flash's own checks (quiet hours, another app fullscreen, a copy already on)
        let flash = r.rule_id.and_then(|_| {
            self.with_reader(|c| crate::store::follow::flash_of(c, r.id)).unwrap_or_else(|e| {
                tracing::debug!("news flash {}: {e}", r.id);
                None
            })
        });
        let flash_skip = flash.and_then(|f| {
            let playing = on_screen
                .iter()
                .filter_map(|c| lib.live.get(*c))
                .map(|c| crate::model::ItemRef { source: c.source_id, kind: crate::model::ItemKind::Channel, key: c.key.clone() })
                .collect();
            let ctx = crate::follow::flash::FlashCtx {
                minute_of_day: crate::reminders::local_minute_of_day(now),
                other_fullscreen: cfg.follow.news_flash.skip_when_fullscreen && crate::reminders::other_app_fullscreen(),
                main_kind,
                multiview: tiles.on,
                playing,
            };
            crate::reminders::flash_skip(&f, &cfg.follow.news_flash, &ctx, r)
        });
        let ctx = FireCtx {
            route: crate::playback::RouteState {
                tiles,
                main_kind,
                queue_running,
                follow_interrupt_vod: cfg.follow.interrupt_vod,
                casting: main.as_ref().is_some_and(|p| p.cast.is_some()),
            },
            target_locked,
            target_playing,
            flash_skip,
            guard: shared.guard(),
            now,
        };
        FireFacts { ctx, catchup: channel.and_then(|c| lib.live.get(c)).is_some_and(|c| c.has_catchup) }
    }
}

/// What a fired reminder shows.
#[derive(Debug, Clone, PartialEq)]
struct Note {
    summary: String,
    body: String,
}

impl Note {
    /// One-line form for the in-app toast fallback.
    fn toast_text(&self) -> String {
        format!("{} — {}", self.summary, self.body)
    }
}

/// Shows reminder notifications. `on_watch` runs (at most once) when the user picks "Watch".
trait Notifier: Send + Sync + 'static {
    /// Resolves to false when no notification server answered.
    fn show_reminder(&self, note: Note, on_watch: Box<dyn FnOnce() + Send>) -> BoxFuture<'static, bool>;
}

struct DesktopNotifier {
    /// `None` = the session's notification server; `Some` = notify-rust test bus (tests).
    bus: Option<String>,
}

impl Notifier for DesktopNotifier {
    fn show_reminder(&self, note: Note, on_watch: Box<dyn FnOnce() + Send>) -> BoxFuture<'static, bool> {
        let bus = self.bus.clone();
        Box::pin(async move { show_with_action(bus, &note.summary, &note.body, ("watch", "Watch"), on_watch).await })
    }
}

/// A notification with one action (`(id, label)`, also on a click on its body): `on_action`
/// runs at most once when the user picks it, within [`ACTION_WAIT`]. Resolves to false when no
/// notification server answered (in [`NOTIFY_TIMEOUT`]): the caller falls back to a toast.
/// Must run inside a tokio runtime (the wait for the action is spawned on it).
async fn show_with_action(bus: Option<String>, summary: &str, body: &str, action: (&str, &str), on_action: Box<dyn FnOnce() + Send>) -> bool {
    let (id, label) = action;
    let n = build(summary, body, false, &[("default", label), (id, label)]);
    // zbus calls have no timeout of their own: a wedged notification server must not stall
    // the caller (it falls back to a toast).
    let handle = match tokio::time::timeout(NOTIFY_TIMEOUT, show(&n, bus.as_deref())).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::info!("notification not shown: {e}");
            return false;
        }
        Err(_) => {
            tracing::info!("notification timed out");
            return false;
        }
    };
    let id = id.to_owned();
    tokio::spawn(async move {
        let wait = handle.wait_for_action_async(|response| match response {
            NotificationResponse::Default => on_action(),
            NotificationResponse::Action(a) if *a == id || a == "default" => on_action(),
            _ => {}
        });
        let _ = tokio::time::timeout(ACTION_WAIT, wait).await;
    });
    true
}

/// Following's summary of new matches with **Open** (and a click on its body): `on_open` runs
/// when the user picks it. False when no notification server answered (the caller toasts
/// instead). Must run inside a tokio runtime.
pub async fn notify_with_open(summary: &str, body: &str, on_open: Box<dyn FnOnce() + Send>) -> bool {
    show_with_action(None, summary, body, ("open", "Open"), on_open).await
}

/// A cast alert while Tuner's window isn't on a visible workspace (DESIGN §40 §3.8): the
/// alert's fixed sentence (`summary`, `body`: no device or provider text beyond the TV's
/// sanitized name) with **Show** (and a click on its body), which raises Tuner (raised right
/// away: the UI may not run while the window sits on another workspace) and sends
/// `PlatformEvent::ShowPlayer` — the player, where the casting card has the actions (Allow on
/// firewall, Play here…). False when no notification server answered (the caller toasts
/// instead). Must run inside a tokio runtime.
pub async fn notify_cast(summary: &str, body: &str, bus: EventBus) -> bool {
    let on_show: Box<dyn FnOnce() + Send> = Box::new(move || {
        bus.send(AppEvent::Platform(PlatformEvent::ShowPlayer));
        crate::platform::hypr::focus_self();
    });
    show_with_action(None, summary, body, ("show", "Show"), on_show).await
}

#[derive(Debug, Clone, PartialEq)]
enum Decision {
    /// Ended while nobody was looking: mark `Dismissed`, no notification.
    Dismiss(ReminderId),
    Fire(Reminder),
    /// An automatic switch is `SWITCH_WARN_S` away: the app's Cancel toast (the row stays
    /// pending). With the time it switches at: its target, or — late (Tuner closed or asleep at
    /// its countdown) — `SWITCH_WARN_S` from now, so even a late switch can be cancelled.
    Warn(Reminder, Timestamp),
    /// An automatic switch's time: switch; the row becomes `Fired`.
    Switch(Reminder),
}

#[derive(Debug, Default, PartialEq)]
struct Plan {
    now: Vec<Decision>,
    /// Earliest future fire time.
    next_at: Option<Timestamp>,
}

impl Plan {
    fn wake_at(&mut self, t: Timestamp) {
        self.next_at = Some(self.next_at.map_or(t, |n| n.min(t)));
    }
}

/// Pure scheduling step: what to do at `now` and when to look again. `warned`: switches whose
/// countdown went out (or was decided), with the time they switch at.
fn plan(pending: &[Reminder], now: Timestamp, settings: Settings, handled: &HashSet<ReminderId>, warned: &HashMap<ReminderId, Timestamp>) -> Plan {
    use crate::reminders::{effective_stop, notify_at, switch_at};
    let mut p = Plan::default();
    for r in pending.iter().filter(|r| r.state == ReminderState::Pending && !handled.contains(&r.id)) {
        if r.switches() {
            let at = switch_at(r);
            let warn_at = at - Reminder::SWITCH_WARN_S;
            let due = warned.get(&r.id).copied();
            if effective_stop(r) <= now || (due.is_none() && now > at + LATE_SWITCH_S) {
                p.now.push(Decision::Dismiss(r.id));
            } else if let Some(due) = due {
                if now >= due {
                    p.now.push(Decision::Switch(r.clone()));
                } else {
                    p.wake_at(due);
                }
            } else if now >= warn_at {
                // the countdown always runs its full time: a late switch is never sprung
                let due = at.max(now + Reminder::SWITCH_WARN_S);
                p.now.push(Decision::Warn(r.clone(), due));
                p.wake_at(due);
            } else {
                p.wake_at(warn_at);
            }
            continue;
        }
        if !settings.notify {
            continue;
        }
        let fire_at = notify_at(r, settings.lead_default_s);
        if effective_stop(r) <= now {
            p.now.push(Decision::Dismiss(r.id));
        } else if fire_at <= now {
            p.now.push(Decision::Fire(r.clone()));
        } else {
            p.wake_at(fire_at);
        }
    }
    p
}

/// "‹programme›" / "Starts in 2 min on ‹channel›" (or "Starting now", "Started 5 min ago").
fn note_for(r: &Reminder, now: Timestamp, guard: &crate::guard::Guard, why: Option<NotifyWhy>) -> Note {
    let (summary, body) = crate::reminders::note_text(r, now, guard, why);
    Note { summary, body }
}

/// The scheduler's loop state and side effects.
struct Engine {
    backend: Arc<dyn ReminderBackend>,
    notifier: Arc<dyn Notifier>,
    bus: EventBus,
    /// Fired/dismissed here but maybe not yet written: never act on them twice.
    handled: HashSet<ReminderId>,
    /// Switches whose countdown went out (or was decided at its time), with the time they
    /// switch at.
    warned: HashMap<ReminderId, Timestamp>,
}

impl Engine {
    async fn facts(&self, r: &Reminder, now: Timestamp) -> FireFacts {
        let (b, r2) = (self.backend.clone(), r.clone());
        tokio::task::spawn_blocking(move || b.fire_facts(&r2, now)).await.unwrap_or(FireFacts { ctx: FireCtx::idle(now), catchup: false })
    }

    /// Show `r`'s note with Watch (a toast without a notification server).
    async fn show_note(&self, r: &Reminder, now: Timestamp, guard: &crate::guard::Guard, why: Option<NotifyWhy>) {
        let note = note_for(r, now, guard, why);
        let watch_bus = self.bus.clone();
        let (id, channel) = (r.id, r.channel.clone());
        // Raise right away as well (DESIGN §21.3): the UI may not be running while the window
        // sits on another workspace, so the event alone could wait. A click is the user's.
        let on_watch: Box<dyn FnOnce() + Send> = Box::new(move || {
            watch_bus.send(AppEvent::Platform(PlatformEvent::ReminderWatch { reminder: id, channel }));
            crate::platform::hypr::focus_self();
        });
        if !self.notifier.show_reminder(note.clone(), on_watch).await {
            self.bus.toast(Toast { kind: ToastKind::Info, text: note.toast_text(), action: None, ttl_ms: 10_000 });
        }
    }

    /// A switch that won't switch: noted (or nothing), marked fired, the app told.
    async fn not_switched(&mut self, r: &Reminder, now: Timestamp, s: Settings, guard: &crate::guard::Guard, how: FireAs) {
        self.handled.insert(r.id);
        self.backend.set_state(r.id, ReminderState::Fired);
        let why = match how {
            FireAs::Notify(why) => {
                tracing::info!("reminder {}: not switching ({why:?}); notifying", r.id);
                self.show_note(r, now, guard, Some(why)).await;
                NotSwitched::Notified(why)
            }
            FireAs::Skip(why) => {
                tracing::info!("reminder {}: not switching ({why:?})", r.id);
                NotSwitched::Skipped(why)
            }
            FireAs::Switch(_) => return,
        };
        if s.rich {
            self.bus.send(AppEvent::Reminder(ReminderEvent::NotSwitched { reminder: r.id, why }));
        }
    }

    async fn act(&mut self, d: Decision, now: Timestamp, s: Settings) {
        match d {
            Decision::Dismiss(id) => {
                self.handled.insert(id);
                self.backend.set_state(id, ReminderState::Dismissed);
                tracing::info!("reminder {id} missed (programme ended); dismissed");
            }
            Decision::Fire(r) => {
                self.handled.insert(r.id);
                self.backend.set_state(r.id, ReminderState::Fired);
                self.bus.send(AppEvent::Platform(PlatformEvent::ReminderFired(r.id)));
                let guard = self.facts(&r, now).await.ctx.guard;
                self.show_note(&r, now, &guard, None).await;
            }
            Decision::Warn(r, at) => {
                // decided again at the switch either way (the user may zap away from the channel,
                // or start a film, during the countdown): only the countdown depends on this
                self.warned.insert(r.id, at);
                let facts = self.facts(&r, now).await;
                match crate::reminders::fire_policy(&r, &facts.ctx) {
                    FireAs::Switch(route) => {
                        tracing::info!("reminder {}: switching in {} s", r.id, (at - now).max(0));
                        let event = if s.rich {
                            let title = r.programme_title.trim();
                            AppEvent::Reminder(ReminderEvent::SwitchSoon {
                                reminder: r.id,
                                channel: r.channel.clone(),
                                channel_name: r.channel_name.clone(),
                                title: if title.is_empty() { String::new() } else { facts.ctx.guard.outside_title(title, now).into_owned() },
                                at,
                                open_in: crate::reminders::open_in(&r, s.open_in),
                                route,
                            })
                        } else {
                            AppEvent::Platform(PlatformEvent::SwitchSoon { reminder: r.id, channel: r.channel.clone(), start: at })
                        };
                        self.bus.send(event);
                    }
                    how => tracing::info!("reminder {}: no countdown ({how:?}); decided at its time", r.id),
                }
            }
            Decision::Switch(r) => {
                let facts = self.facts(&r, now).await;
                match crate::reminders::fire_policy(&r, &facts.ctx) {
                    FireAs::Switch(route) => {
                        self.handled.insert(r.id);
                        self.backend.set_state(r.id, ReminderState::Fired);
                        let open_in = crate::reminders::open_in(&r, s.open_in);
                        tracing::info!("reminder {}: switching now ({open_in:?})", r.id);
                        // raise first: the UI may not run while the window sits on another
                        // workspace. Only for the full window: the pop-out and "where I am"
                        // never take focus (the pop-out shows on every workspace). Never while
                        // casting: the switch retunes the TV (DESIGN §40 §3.8)
                        if (!s.rich || open_in == OpenIn::FullWindow) && !facts.ctx.route.casting {
                            crate::platform::hypr::focus_self();
                        }
                        let event = if s.rich {
                            let at = crate::reminders::switch_at(&r);
                            let late = (now - at >= crate::reminders::LATE_NOTE_S).then(|| Late {
                                by_s: now - at,
                                from: at,
                                stop: crate::reminders::effective_stop(&r),
                                catchup: facts.catchup,
                            });
                            AppEvent::Reminder(ReminderEvent::SwitchNow {
                                reminder: r.id,
                                channel: r.channel.clone(),
                                open_in,
                                route,
                                rule: r.rule_id,
                                late,
                            })
                        } else {
                            AppEvent::Platform(PlatformEvent::SwitchNow { reminder: r.id, channel: r.channel.clone() })
                        };
                        self.bus.send(event);
                    }
                    how => self.not_switched(&r, now, s, &facts.ctx.guard, how).await,
                }
            }
        }
    }
}

/// Keep the systemd timers in step with `pending` (only when the plan changed).
async fn sync_units(arm: &Arc<Mutex<ArmSync>>, pending: &[Reminder], s: Settings, last: &mut Option<Vec<(ReminderId, Timestamp)>>) {
    let want = if s.when_closed { crate::reminders::arming::plan(pending, crate::model::now(), s.lead_default_s) } else { Vec::new() };
    if last.as_ref() == Some(&want) {
        return;
    }
    let arm = arm.clone();
    let units = want.clone();
    let done = tokio::task::spawn_blocking(move || {
        let mut a = arm.lock().unwrap_or_else(|e| e.into_inner());
        if s.when_closed { a.sync(&units) } else { a.clear() }
    })
    .await;
    if done.is_ok() {
        *last = Some(want);
    }
}

async fn run(
    backend: Arc<dyn ReminderBackend>,
    notifier: Arc<dyn Notifier>,
    bus: EventBus,
    arm: Option<Arc<Mutex<ArmSync>>>,
    mut wake: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    let mut engine = Engine { backend, notifier, bus, handled: HashSet::new(), warned: HashMap::new() };
    // Last load error, so a persistent one is logged once rather than every minute.
    let mut last_error: Option<String> = None;
    let mut last_units = None;
    loop {
        let b = engine.backend.clone();
        let loaded = match tokio::task::spawn_blocking(move || b.pending()).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(e) => Err(format!("loading failed: {e}")),
        };
        let pending = match loaded {
            Ok(v) => {
                last_error = None;
                v
            }
            Err(e) => {
                if last_error.as_deref() != Some(e.as_str()) {
                    tracing::warn!("reminders: {e}");
                    last_error = Some(e);
                }
                Vec::new()
            }
        };
        let settings = engine.backend.settings();
        if let Some(arm) = &arm
            && last_error.is_none()
        {
            sync_units(arm, &pending, settings, &mut last_units).await;
        }
        engine.handled.retain(|id| pending.iter().any(|r| r.id == *id));
        engine.warned.retain(|id, _| pending.iter().any(|r| r.id == *id));
        let now = crate::model::now();
        let p = plan(&pending, now, settings, &engine.handled, &engine.warned);
        for d in p.now {
            engine.act(d, now, settings).await;
        }
        let wait = p.next_at.map_or(MAX_SLEEP_S, |t| (t - crate::model::now()).clamp(0, MAX_SLEEP_S));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            w = wake.recv() => {
                if w.is_none() {
                    return;
                }
                while wake.try_recv().is_ok() {} // coalesce bursts of refreshes
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use futures_util::StreamExt;
    use mpris_server::zbus;

    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};

    const ON: Settings = Settings { lead_default_s: 120, notify: true, open_in: OpenIn::PopOut, when_closed: false, rich: false };
    const OFF: Settings = Settings { notify: false, ..ON };
    /// Pass 2: rich events, arming.
    const RICH: Settings = Settings { rich: true, when_closed: true, ..ON };

    fn reminder(id: i64, start: Timestamp, stop: Timestamp, lead_s: i64) -> Reminder {
        Reminder {
            id: ReminderId(id),
            channel: ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("xt:live:{id}")) },
            channel_name: "BBC One".into(),
            programme_title: format!("Programme {id}"),
            start,
            stop,
            lead_s,
            state: ReminderState::Pending,
            mode: Default::default(),
            offset_s: 0,
            open_in: None,
            rule_id: None,
        }
    }

    #[test]
    fn plan_fires_waits_and_dismisses() {
        let now = 1_000_000;
        let pending = vec![
            reminder(1, now + 120, now + 1800, 120),  // fire time == now
            reminder(2, now + 600, now + 1800, 120),  // in 480 s
            reminder(3, now - 3600, now - 60, 120),   // ended → dismissed
            reminder(4, now - 300, now + 600, 120),   // still airing → fires late
            reminder(5, now + 300, now + 900, 0),     // no own lead → default 240 → in 60 s
            reminder(6, now - 7200, now - 7200, 120), // no stop: assumed 1 h → ended
        ];
        let p = plan(&pending, now, Settings { lead_default_s: 240, ..ON }, &HashSet::new(), &HashMap::new());
        assert_eq!(
            p.now,
            vec![
                Decision::Fire(pending[0].clone()),
                Decision::Dismiss(ReminderId(3)),
                Decision::Fire(pending[3].clone()),
                Decision::Dismiss(ReminderId(6)),
            ]
        );
        assert_eq!(p.next_at, Some(now + 60));
    }

    #[test]
    fn plan_skips_handled_and_non_pending() {
        let now = 50_000;
        let mut fired = reminder(2, now, now + 60, 0);
        fired.state = ReminderState::Fired;
        let pending = vec![reminder(1, now, now + 60, 0), fired];
        let handled: HashSet<_> = [ReminderId(1)].into();
        assert_eq!(plan(&pending, now, ON, &handled, &HashMap::new()), Plan::default());
    }

    /// DESIGN §30.8: a switch warns `SWITCH_WARN_S` before its start (once), switches at the
    /// start, still switches up to `LATE_SWITCH_S` late — after a full countdown of its own, so
    /// it can be cancelled too — and is dismissed after; it runs with notification reminders
    /// off, which only skips the notification rows.
    #[test]
    fn plan_warns_then_switches_whatever_notifications_say() {
        let now = 1_000_000;
        let sw = |id, start| reminder(id, start, start + 1800, Reminder::SWITCH_LEAD);
        let pending = vec![
            sw(1, now + 60),                       // warns at now + 50
            sw(2, now + 5),                        // warns late (5 s left): a full countdown
            sw(3, now - 300),                      // 5 min late: a countdown from now
            sw(4, now - LATE_SWITCH_S - 1),        // too late: dismissed
            reminder(5, now + 60, now + 900, 120), // a notification due now
        ];
        let off = OFF;
        let late_at = now + Reminder::SWITCH_WARN_S;
        let p = plan(&pending, now, off, &HashSet::new(), &HashMap::new());
        assert_eq!(
            p.now,
            vec![Decision::Warn(pending[1].clone(), late_at), Decision::Warn(pending[2].clone(), late_at), Decision::Dismiss(ReminderId(4))]
        );
        assert_eq!(p.next_at, Some(late_at));
        // on time: the countdown ends at the start
        assert_eq!(plan(&pending[..1], now + 50, off, &HashSet::new(), &HashMap::new()).now, vec![Decision::Warn(pending[0].clone(), now + 60)]);
        // warned already: wait for its time, no second toast; then switch (the late one too)
        let warned: HashMap<_, _> = [(ReminderId(2), late_at), (ReminderId(3), late_at)].into();
        let p = plan(&pending[1..3], now, off, &HashSet::new(), &warned);
        assert_eq!((p.now, p.next_at), (vec![], Some(late_at)));
        let p = plan(&pending[1..3], late_at, off, &HashSet::new(), &warned);
        assert_eq!(p.now, vec![Decision::Switch(pending[1].clone()), Decision::Switch(pending[2].clone())]);
        // a countdown that started just inside the late limit still switches after it
        let edge = sw(6, now - LATE_SWITCH_S + 5);
        let warned: HashMap<_, _> = [(ReminderId(6), now + Reminder::SWITCH_WARN_S)].into();
        let p = plan(std::slice::from_ref(&edge), now + Reminder::SWITCH_WARN_S, off, &HashSet::new(), &warned);
        assert_eq!(p.now, vec![Decision::Switch(edge)]);
        assert_eq!(plan(&pending[..1], now, off, &HashSet::new(), &HashMap::new()).next_at, Some(now + 50));
        // notifications on: the notification row fires as before
        let p = plan(&pending[4..], now, ON, &HashSet::new(), &HashMap::new());
        assert_eq!(p.now, vec![Decision::Fire(pending[4].clone())]);
    }

    #[test]
    fn note_texts() {
        let note_for = |r: &Reminder, now| super::note_for(r, now, &crate::guard::Guard::default(), None);
        let r = reminder(1, 10_000, 12_000, 120);
        assert_eq!(note_for(&r, 10_000 - 120), Note { summary: "Programme 1".into(), body: "Starts in 2 min on BBC One".into() });
        assert_eq!(note_for(&r, 10_000 - 61).body, "Starts in 2 min on BBC One");
        assert_eq!(note_for(&r, 10_000 - 10).body, "Starting now on BBC One");
        assert_eq!(note_for(&r, 10_000 + 59).body, "Starting now on BBC One");
        assert_eq!(note_for(&r, 10_000 + 300).body, "Started 5 min ago on BBC One");
        let mut untitled = r.clone();
        untitled.programme_title = "  ".into();
        assert_eq!(note_for(&untitled, 0).summary, "Reminder");
        assert_eq!(note_for(&r, 0).toast_text(), "Programme 1 — Starts in 167 min on BBC One");
    }

    // ---------------------------------------------------------------- scheduler with fakes

    struct FakeBackend {
        rows: Mutex<Vec<Reminder>>,
        states: Mutex<Vec<(ReminderId, ReminderState)>>,
        settings: Settings,
        /// What plays now (fire-time facts).
        ctx: Mutex<FireCtx>,
    }

    impl FakeBackend {
        fn new(settings: Settings) -> Arc<Self> {
            Arc::new(Self { rows: Mutex::default(), states: Mutex::default(), settings, ctx: Mutex::new(FireCtx::idle(0)) })
        }
    }

    impl ReminderBackend for FakeBackend {
        fn pending(&self) -> Result<Vec<Reminder>> {
            let states = self.states.lock().unwrap().clone();
            let rows = self.rows.lock().unwrap();
            Ok(rows.iter().filter(|r| !states.iter().any(|(id, _)| *id == r.id)).cloned().collect())
        }
        fn set_state(&self, id: ReminderId, state: ReminderState) {
            self.states.lock().unwrap().push((id, state));
        }
        fn settings(&self) -> Settings {
            self.settings
        }
        fn fire_facts(&self, _r: &Reminder, now: Timestamp) -> FireFacts {
            FireFacts { ctx: FireCtx { now, ..self.ctx.lock().unwrap().clone() }, catchup: true }
        }
    }

    /// Records notes; clicks "Watch" immediately when `click` is set; `available` = server up.
    struct FakeNotifier {
        shown: Mutex<Vec<Note>>,
        click: bool,
        available: bool,
    }

    impl Notifier for FakeNotifier {
        fn show_reminder(&self, note: Note, on_watch: Box<dyn FnOnce() + Send>) -> BoxFuture<'static, bool> {
            self.shown.lock().unwrap().push(note);
            if self.click && self.available {
                on_watch();
            }
            let ok = self.available;
            Box::pin(async move { ok })
        }
    }

    fn recv_until(rx: &crossbeam_channel::Receiver<AppEvent>, deadline: Duration, mut pred: impl FnMut(&AppEvent) -> bool) -> Vec<AppEvent> {
        let end = Instant::now() + deadline;
        let mut seen = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) => {
                    let done = pred(&ev);
                    seen.push(ev);
                    if done {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    #[test]
    fn scheduler_fires_due_dismisses_missed_and_reports_watch() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(ON);
        *backend.rows.lock().unwrap() = vec![
            reminder(1, now + 60, now + 3600, 120),   // due now
            reminder(2, now - 7200, now - 3600, 120), // missed
            reminder(3, now + 2, now + 3600, 1),      // due in ~1 s
            reminder(4, now + 7200, now + 9000, 120), // later
        ];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: true, available: true });
        let (bus, rx) = EventBus::new(None);
        let sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier.clone(), bus, None);
        let mut watched = Vec::new();
        let events = recv_until(&rx, Duration::from_secs(10), |ev| {
            if let AppEvent::Platform(PlatformEvent::ReminderWatch { reminder, channel }) = ev {
                watched.push((*reminder, channel.key.0.clone()));
            }
            watched.len() == 2
        });
        assert_eq!(watched, vec![(ReminderId(1), "xt:live:1".to_owned()), (ReminderId(3), "xt:live:3".to_owned())], "{events:?}");
        assert!(events.iter().any(|e| matches!(e, AppEvent::Platform(PlatformEvent::ReminderFired(ReminderId(1))))));
        let states = backend.states.lock().unwrap().clone();
        assert!(states.contains(&(ReminderId(1), ReminderState::Fired)));
        assert!(states.contains(&(ReminderId(2), ReminderState::Dismissed)));
        assert!(states.contains(&(ReminderId(3), ReminderState::Fired)));
        assert!(!states.iter().any(|(id, _)| *id == ReminderId(4)));
        let shown = notifier.shown.lock().unwrap().clone();
        assert_eq!(shown.len(), 2, "missed reminders are silent: {shown:?}");
        assert_eq!(shown[0].body, "Starts in 1 min on BBC One");
        // refresh picks up a reminder added later
        backend.rows.lock().unwrap().push(reminder(5, now - 60, now + 600, 120));
        sched.refresh();
        let ev = recv_until(&rx, Duration::from_secs(5), |ev| matches!(ev, AppEvent::Platform(PlatformEvent::ReminderWatch { .. })));
        assert!(ev.iter().any(|e| matches!(e, AppEvent::Platform(PlatformEvent::ReminderWatch { reminder: ReminderId(5), .. }))), "{ev:?}");
        assert_eq!(notifier.shown.lock().unwrap().last().unwrap().body, "Started 1 min ago on BBC One");
        drop(sched);
    }

    /// A switch with notification reminders off: the Cancel-toast event, then the switch at
    /// the start; the row ends `Fired`; nothing is notified.
    #[test]
    fn scheduler_warns_then_switches() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(OFF);
        *backend.rows.lock().unwrap() = vec![reminder(8, now + 2, now + 1800, Reminder::SWITCH_LEAD)];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: true });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier.clone(), bus, None);
        let events = recv_until(&rx, Duration::from_secs(20), |ev| matches!(ev, AppEvent::Platform(PlatformEvent::SwitchNow { .. })));
        let seen: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AppEvent::Platform(PlatformEvent::SwitchSoon { reminder: ReminderId(8), .. }) => Some("soon"),
                AppEvent::Platform(PlatformEvent::SwitchNow { reminder: ReminderId(8), channel }) => {
                    assert_eq!(channel.key.0, "xt:live:8");
                    Some("now")
                }
                _ => None,
            })
            .collect();
        assert_eq!(seen, ["soon", "now"], "{events:?}");
        assert_eq!(backend.states.lock().unwrap().clone(), [(ReminderId(8), ReminderState::Fired)]);
        assert!(notifier.shown.lock().unwrap().is_empty());
    }

    #[test]
    fn scheduler_falls_back_to_a_toast_without_a_notification_server() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(ON);
        *backend.rows.lock().unwrap() = vec![reminder(7, now + 100, now + 900, 120)];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: false });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend, notifier, bus, None);
        let events = recv_until(&rx, Duration::from_secs(5), |ev| matches!(ev, AppEvent::Toast(_)));
        let toast = events.iter().find_map(|e| if let AppEvent::Toast(t) = e { Some(t.text.clone()) } else { None });
        assert_eq!(toast.as_deref(), Some("Programme 7 — Starts in 2 min on BBC One"));
    }

    #[test]
    fn store_backend_survives_unimplemented_storage() {
        // Until WP-02 lands the repository panics; afterwards it returns rows. Either way the
        // scheduler must get a Result, never a panic.
        let store = Store::open_temp("tuner-reminders-test", None).unwrap();
        let backend = StoreBackend { store, reader: Mutex::new(None), explicit: Default::default(), live: None, fixed: ON };
        let _ = backend.pending();
    }

    /// DESIGN §20.3: while parental control hides explicit content, a reminder on an explicit
    /// channel is not announced (its notification would name the programme and the channel);
    /// it stays pending and is announced once explicit content shows again.
    #[test]
    fn reminders_on_explicit_channels_wait_while_hidden() {
        let store = Store::open_temp("tuner-reminders-explicit", None).unwrap();
        let now = crate::model::now();
        store
            .writer()
            .blocking(move |c| {
                c.execute("INSERT INTO source(id, name, kind, url, created_at, generation) VALUES (1, 'S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1)", [])?;
                for (id, key, group, explicit) in [(1, "m3u:news", "channel:news", 0), (2, "m3u:late", "channel:xxx", 1)] {
                    c.execute(
                        "INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                         VALUES (?1, 1, ?2, 'C', 'C', 'c', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/1.ts\"}', 0, 1)",
                        rusqlite::params![id, key],
                    )?;
                    c.execute(
                        "INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen, explicit) VALUES (?1, 1, 'channel', ?2, ?2, 0, 1, 1, ?3)",
                        rusqlite::params![id, group, explicit],
                    )?;
                    c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?1, 0, 1)", [id])?;
                    c.execute(
                        "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state)
                         VALUES (1, ?1, 'C', 'T', ?2, ?3, 120, 'pending')",
                        rusqlite::params![key, now + 60, now + 1800],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let gate = crate::store::library::ExplicitGate::default();
        let backend = StoreBackend { store, reader: Mutex::new(None), explicit: gate.clone(), live: None, fixed: ON };
        let keys = |b: &StoreBackend| b.pending().unwrap().into_iter().map(|r| r.channel.key.0).collect::<Vec<_>>();
        assert_eq!(keys(&backend), ["m3u:news"], "hidden by default");
        gate.set(crate::store::library::Visibility::ALL);
        assert_eq!(keys(&backend).len(), 2);
    }

    /// DESIGN §24.2 WP-08 "[e2e after WP-02] reminder fires and Watch resolves by ItemRef":
    /// raw-SQL rows in a real store → scheduler with the real `StoreBackend` → `ReminderWatch`
    /// carries the channel's ItemRef, which the app's `LiveIndex::by_ref` resolves; the row
    /// ends up `fired`.
    #[test]
    fn reminder_e2e_with_the_store() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let store = Store::open_temp("tuner-reminder-e2e", None).unwrap();
        let now = crate::model::now();
        let (channel_id, source) = store
            .writer()
            .blocking(move |c| {
                c.execute("INSERT INTO source(name, kind, url, created_at, generation) VALUES ('S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1)", [])?;
                let source = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, stream, provider_order, added_at, seen_gen)
                     VALUES (?1, 'm3u:remind00001', 'News 24', 'News 24', 'news 24', 1, '{\"t\":\"url\",\"url\":\"http://127.0.0.1/1.ts\"}', 0, 0, 1)",
                    [source],
                )?;
                let channel = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:all','All',0,1,1)",
                    [source],
                )?;
                c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, 0, 1)", [c.last_insert_rowid(), channel])?;
                c.execute(
                    "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state)
                     VALUES (?1, 'm3u:remind00001', 'News 24', 'Headlines', ?2, ?3, 120, 'pending')",
                    rusqlite::params![source, now + 60, now + 1800],
                )?;
                Ok((crate::model::ChannelId(channel), SourceId(source)))
            })
            .unwrap();
        let backend = Arc::new(StoreBackend { store: store.clone(), reader: Mutex::new(None), explicit: Default::default(), live: None, fixed: ON });
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: true, available: true });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend, notifier.clone(), bus, None);
        let events = recv_until(&rx, Duration::from_secs(10), |ev| matches!(ev, AppEvent::Platform(PlatformEvent::ReminderWatch { .. })));
        let channel = events
            .iter()
            .find_map(|e| if let AppEvent::Platform(PlatformEvent::ReminderWatch { channel, .. }) = e { Some(channel.clone()) } else { None })
            .expect("ReminderWatch");
        assert_eq!(channel, ItemRef { source, kind: ItemKind::Channel, key: ItemKey("m3u:remind00001".into()) });
        assert_eq!(notifier.shown.lock().unwrap()[0], Note { summary: "Headlines".into(), body: "Starts in 1 min on News 24".into() });
        let lib = crate::store::library::load(&store.reader().unwrap()).unwrap();
        assert_eq!(lib.live.by_ref(&channel), Some(channel_id));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state: String = store.reader().unwrap().query_row("SELECT state FROM reminder", [], |r| r.get(0)).unwrap();
            if state == "fired" {
                break;
            }
            assert!(Instant::now() < deadline, "reminder state stayed {state}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---------------------------------------------------------------- pass 2 (modes, policy, arming)

    fn switch(id: i64, start: Timestamp) -> Reminder {
        Reminder { mode: crate::model::ReminderMode::Switch, ..reminder(id, start, start + 1800, 0) }
    }

    fn reminder_events(events: &[AppEvent]) -> Vec<ReminderEvent> {
        events.iter().filter_map(|e| if let AppEvent::Reminder(r) = e { Some(r.clone()) } else { None }).collect()
    }

    /// The offset moves both kinds: a note goes out `lead` before start + offset, a switch
    /// switches at start + offset.
    #[test]
    fn plan_honours_the_start_offset() {
        let now = 1_000_000;
        let note = Reminder { offset_s: 600, ..reminder(1, now + 60, now + 3600, 120) }; // at now + 540
        let sw = Reminder { offset_s: 300, ..switch(2, now - 290) }; // at now + 10
        let p = plan(&[note, sw.clone()], now, ON, &HashSet::new(), &HashMap::new());
        assert_eq!(p.now, vec![Decision::Warn(sw, now + 10)]);
        assert_eq!(p.next_at, Some(now + 10));
    }

    /// User decision (2026-10-11): a Following switch while a film plays becomes a note with
    /// Watch — no countdown, no switch; the row is fired and the app told why.
    #[test]
    fn a_rule_switch_during_a_film_notifies_instead() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(RICH);
        *backend.rows.lock().unwrap() = vec![Reminder { rule_id: Some(crate::model::FollowRuleId(4)), ..switch(3, now + 2) }];
        backend.ctx.lock().unwrap().route.main_kind = Some(crate::playback::Kind::Vod);
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: true });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier.clone(), bus, None);
        let events = recv_until(&rx, Duration::from_secs(20), |e| matches!(e, AppEvent::Reminder(ReminderEvent::NotSwitched { .. })));
        let seen = reminder_events(&events);
        assert_eq!(
            seen,
            [ReminderEvent::NotSwitched { reminder: ReminderId(3), why: NotSwitched::Notified(NotifyWhy::WouldInterrupt) }],
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, AppEvent::Platform(PlatformEvent::SwitchSoon { .. } | PlatformEvent::SwitchNow { .. }))));
        assert_eq!(notifier.shown.lock().unwrap()[0].summary, "Programme 3");
        assert_eq!(backend.states.lock().unwrap().clone(), [(ReminderId(3), ReminderState::Fired)]);
    }

    /// Pass 2: an own switch counts down and switches carrying the route and where it opens
    /// (the pop-out by default, the reminder's own choice first); one that is late says by how
    /// much, so the app can start it from the beginning on catch-up.
    #[test]
    fn switches_carry_where_they_open_and_lateness() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(RICH);
        let late = Reminder { open_in: Some(OpenIn::WhereIAm), ..switch(6, now - 300) };
        *backend.rows.lock().unwrap() = vec![switch(5, now + 2), late];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: true });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier.clone(), bus, None);
        let mut now_count = 0;
        // the late one counts down from now too: it switches `SWITCH_WARN_S` later
        let events = recv_until(&rx, Duration::from_secs(20), |e| {
            now_count += usize::from(matches!(e, AppEvent::Reminder(ReminderEvent::SwitchNow { .. })));
            now_count == 2
        });
        let main = crate::playback::Route::Tile { tile: crate::model::TileId::A, take_sound: false, interrupts_queue: false };
        let seen = reminder_events(&events);
        let soon = seen.iter().find(|e| matches!(e, ReminderEvent::SwitchSoon { reminder: ReminderId(5), .. })).expect("countdown");
        assert!(matches!(soon, ReminderEvent::SwitchSoon { open_in: OpenIn::PopOut, route, title, .. } if *route == main && title == "Programme 5"));
        assert!(seen.iter().any(|e| matches!(e, ReminderEvent::SwitchSoon { reminder: ReminderId(6), .. })), "a late switch can be cancelled too");
        let switched: Vec<_> = seen
            .iter()
            .filter_map(|e| {
                if let ReminderEvent::SwitchNow { reminder, open_in, route, late, .. } = e {
                    Some((reminder.0, *open_in, *route, *late))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(switched.len(), 2, "{seen:?}");
        let (_, open, route, late) = switched.iter().find(|s| s.0 == 6).copied().unwrap();
        assert_eq!((open, route), (OpenIn::WhereIAm, main));
        let late = late.expect("late");
        assert!((300..330).contains(&late.by_s) && late.from == now - 300 && late.catchup, "{late:?}");
        assert_eq!(switched.iter().find(|s| s.0 == 5).unwrap().3, None);
        assert!(notifier.shown.lock().unwrap().is_empty());
    }

    /// "Fire while Tuner is closed": the units follow the pending reminders on every reload —
    /// armed for what is coming, gone once a reminder is cancelled.
    #[test]
    fn scheduler_keeps_the_units_in_step() {
        use crate::platform::systemd::tests::FakeArmer;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = FakeBackend::new(RICH);
        *backend.rows.lock().unwrap() = vec![reminder(1, now + 3600, now + 7200, 120), switch(2, now + 7200)];
        let armer = Arc::new(FakeArmer::default());
        let arm = Arc::new(Mutex::new(ArmSync::new(armer.clone())));
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: true });
        let (bus, _rx) = EventBus::new(None);
        let sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier, bus, Some(arm));
        let units = |want: usize| {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let u = armer.units.lock().unwrap().clone();
                if u.len() == want || Instant::now() > end {
                    return u;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let u = units(2);
        assert_eq!(u.get(&ReminderId(1)), Some(&(now + 3600 - 120)));
        assert_eq!(u.get(&ReminderId(2)), Some(&(now + 7200 - Reminder::SWITCH_WARN_S - crate::reminders::arming::LAUNCH_EARLY_S)));
        backend.rows.lock().unwrap().retain(|r| r.id != ReminderId(1));
        sched.refresh();
        assert_eq!(units(1).keys().copied().collect::<Vec<_>>(), [ReminderId(2)]);
    }

    /// The live fire context: the session's lock and the channel's lock decide "needs your
    /// PIN"; `follow.interrupt_vod` and the published pictures reach the router.
    #[test]
    fn fire_facts_read_the_session_and_the_store() {
        let store = Store::open_temp("tuner-reminder-facts", None).unwrap();
        store
            .writer()
            .blocking(|c| {
                c.execute(
                    "INSERT INTO source(id, name, kind, url, created_at, generation) VALUES (1, 'S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1)",
                    [],
                )?;
                c.execute("INSERT INTO item_override(source_id, kind, item_key, locked, fp) VALUES (1, 'channel', 'xt:live:7', 1, '{}')", [])?;
                Ok(())
            })
            .unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.parental.enabled = true;
        cfg.parental.pin_hash = Some("x".into());
        cfg.follow.interrupt_vod = true;
        let config: SharedConfig = Arc::new(std::sync::RwLock::new(Arc::new(cfg)));
        let shared = Arc::new(AppShared::default());
        shared.publish_tiles(crate::playback::TilesState { on: true, ..Default::default() });
        let backend = StoreBackend {
            store: store.clone(),
            reader: Mutex::new(None),
            explicit: Default::default(),
            live: Some(Live { shared: shared.clone(), config: config.clone() }),
            fixed: ON,
        };
        let facts = backend.fire_facts(&reminder(7, 100, 200, 0), 100);
        assert!(facts.ctx.target_locked && facts.ctx.route.follow_interrupt_vod && facts.ctx.route.tiles.on);
        assert_eq!(facts.ctx.flash_skip, None, "not a news flash");
        assert!(!backend.fire_facts(&reminder(8, 100, 200, 0), 100).ctx.target_locked, "not locked");
        shared.set_unlocked_at(Some(std::time::Instant::now()));
        assert!(!backend.fire_facts(&reminder(7, 100, 200, 0), 100).ctx.target_locked, "unlocked session");

        // a news flash (its copy on the other playlist) and what plays on the two pictures
        let news = |key: &str| ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(key.into()) };
        let alternates = serde_json::to_string(&[news("m3u:news2")]).unwrap();
        store
            .writer()
            .blocking(move |c| {
                for (id, key) in [(10, "m3u:news"), (11, "m3u:news2")] {
                    c.execute(
                        "INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                         VALUES (?1, 1, ?2, 'News', 'News', 'news', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/1.ts\"}', 0, 1)",
                        rusqlite::params![id, key],
                    )?;
                    c.execute(
                        "INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1, 1, 'channel', ?2, 'G', 0, 1, 1)",
                        rusqlite::params![id, format!("channel:{key}")],
                    )?;
                    c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?1, 0, 1)", [id])?;
                }
                c.execute(
                    "INSERT INTO follow_rule(id, name, kind, spec, mode, priority, created_at, updated_at) VALUES (5, 'News at One', 'news', '{}', 'flash', 0, 0, 0)",
                    [],
                )?;
                c.execute(
                    "INSERT INTO reminder(id, source_id, channel_key, channel_name, title, start, stop, lead_s, state, mode, rule_id)
                     VALUES (20, 1, 'm3u:news', 'News', 'News at One', 100, 200, ?1, 'pending', 'switch', 5)",
                    [Reminder::SWITCH_LEAD],
                )?;
                c.execute(
                    "INSERT INTO follow_match(rule_id, event_key, start, stop, title, source_id, channel_key, alternates, state, reminder_id, seen_at)
                     VALUES (5, 'news', 100, 200, 'News at One', 1, 'm3u:news', ?1, 'scheduled', 20, 0)",
                    [alternates],
                )?;
                Ok(())
            })
            .unwrap();
        let _ = shared.publish_library(&Arc::new(crate::store::library::load(&store.reader().unwrap()).unwrap()));
        let flash = Reminder { id: ReminderId(20), rule_id: Some(crate::model::FollowRuleId(5)), ..switch(9, 100) };
        let flash = Reminder { channel: news("m3u:news"), ..flash };
        let set_cfg = |f: &dyn Fn(&mut crate::config::Config)| {
            let mut c = (*crate::config::current(&config)).clone();
            f(&mut c);
            *config.write().unwrap() = Arc::new(c);
        };
        set_cfg(&|c| {
            c.follow.news_flash.quiet_from_min = Some(0);
            c.follow.news_flash.quiet_to_min = Some(24 * 60);
            c.follow.news_flash.skip_when_fullscreen = false;
        });
        let facts = backend.fire_facts(&flash, 100);
        assert_eq!(facts.ctx.flash_skip, Some(crate::follow::flash::FlashSkip::QuietHours));
        assert!(matches!(crate::reminders::fire_policy(&flash, &facts.ctx), FireAs::Skip(crate::reminders::SkipWhy::Quiet(_))));
        set_cfg(&|c| c.follow.news_flash.quiet_from_min = None);
        assert_eq!(backend.fire_facts(&flash, 100).ctx.flash_skip, None, "pops up");
        // picture B plays the bulletin's copy, then the bulletin itself
        let on_b = |id: i64| {
            let playable = crate::model::Playable {
                media: MediaRef::Channel(crate::model::ChannelId(id)),
                kind: crate::model::PlayableKind::Live,
                item: None,
                source_id: Some(SourceId(1)),
                title: "News".into(),
                subtitle: None,
                logo: None,
                number: None,
                url: crate::redact::Secret::new("http://127.0.0.1/1.ts".into()),
                headers: Default::default(),
                start_at: None,
                duration_hint: None,
            };
            let snap = crate::player::PlayerSnapshot {
                state: crate::player::PlayerState::Playing,
                playable: Some(Arc::new(playable)),
                ..Default::default()
            };
            let (_tx, rx) = tokio::sync::watch::channel(Arc::new(snap));
            shared.set_tile_b(Some(rx));
        };
        on_b(11);
        let facts = backend.fire_facts(&flash, 100);
        assert_eq!((facts.ctx.flash_skip, facts.ctx.target_playing), (Some(crate::follow::flash::FlashSkip::AlreadyWatching), false));
        on_b(10);
        let facts = backend.fire_facts(&flash, 100);
        assert!(facts.ctx.target_playing, "the silent picture counts");
        assert_eq!(crate::reminders::fire_policy(&flash, &facts.ctx), FireAs::Skip(crate::reminders::SkipWhy::AlreadyPlaying));
    }

    // ---------------------------------------------------------------- D-Bus (fake server)

    fn session_bus_available() -> bool {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
    }

    /// Minimal notification server on `de.hoodie.Notification.<suffix>` that records Notify
    /// calls and keeps emitting `ActionInvoked(id, action)` for them (the client subscribes
    /// after the call returns, so a single early signal could be missed).
    async fn fake_server(suffix: &str, action: Option<&'static str>) -> (zbus::Connection, Arc<Mutex<Vec<(String, String, Vec<String>)>>>) {
        let name = format!("de.hoodie.Notification.{suffix}");
        let conn = zbus::conn::Builder::session().unwrap().name(name).unwrap().build().await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (c, rec) = (conn.clone(), calls.clone());
        // subscribed before the caller can send: zbus drops a message no stream is active for, so
        // a stream made inside the task lost the Notify whenever the task started late (a loaded
        // run: no reply, `show` timed out, "assertion failed: shown")
        let mut stream = zbus::MessageStream::from(&c);
        tokio::spawn(async move {
            let mut next_id = 4_000_000u32;
            while let Some(Ok(msg)) = stream.next().await {
                let h = msg.header();
                if h.message_type() != zbus::message::Type::MethodCall || h.member().map(|m| m.as_str()) != Some("Notify") {
                    continue;
                }
                type Args = (String, u32, String, String, String, Vec<String>, HashMap<String, zbus::zvariant::OwnedValue>, i32);
                let Ok((_app, _replaces, _icon, summary, body, actions, _hints, _timeout)) = msg.body().deserialize::<Args>() else {
                    continue;
                };
                rec.lock().unwrap().push((summary, body, actions));
                next_id += 1;
                let id = next_id;
                c.reply(&h, &(id,)).await.unwrap();
                if let Some(action) = action {
                    let c2 = c.clone();
                    tokio::spawn(async move {
                        for _ in 0..50 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let _ = c2
                                .emit_signal(
                                    None::<&str>,
                                    "/org/freedesktop/Notifications",
                                    "org.freedesktop.Notifications",
                                    "ActionInvoked",
                                    &(id, action),
                                )
                                .await;
                        }
                    });
                }
            }
        });
        (conn, calls)
    }

    #[test]
    fn notify_reports_whether_a_server_answered() {
        if !session_bus_available() {
            eprintln!("skipped: no session bus");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let suffix = format!("tuner_notify_{}", std::process::id());
        let (_conn, calls) = rt.block_on(fake_server(&suffix, None));
        assert!(notify_at(Some(suffix), "Hello", "http://h.tv/live/u1/p1/1.ts", true));
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls, vec![("Hello".to_owned(), "http://h.tv/live/***/***/1.ts".to_owned(), vec![])]);
        // nobody owns this name → false, quickly
        let t = Instant::now();
        assert!(!notify_at(Some(format!("tuner_nobody_{}", std::process::id())), "x", "y", false));
        assert!(t.elapsed() < NOTIFY_TIMEOUT);
    }

    #[test]
    fn desktop_notifier_delivers_the_watch_action() {
        if !session_bus_available() {
            eprintln!("skipped: no session bus");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let suffix = format!("tuner_reminder_{}", std::process::id());
        let (_conn, calls) = rt.block_on(fake_server(&suffix, Some("watch")));
        let notifier = DesktopNotifier { bus: Some(suffix) };
        let (tx, rx) = std::sync::mpsc::channel();
        let note = Note { summary: "News".into(), body: "Starts in 2 min on BBC One".into() };
        let shown = rt.block_on(notifier.show_reminder(note, Box::new(move || tx.send(()).unwrap())));
        assert!(shown);
        rx.recv_timeout(Duration::from_secs(10)).expect("watch action delivered");
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "News");
        assert_eq!(calls[0].2, vec!["default", "Watch", "watch", "Watch"]);
    }

    /// Following's summary carries Open (and a click on its body): picking it runs the callback
    /// (the job sends `FollowEvent::Open`); a Watch-style action of another note doesn't.
    #[test]
    fn the_summary_notification_delivers_open() {
        if !session_bus_available() {
            eprintln!("skipped: no session bus");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let suffix = format!("tuner_summary_{}", std::process::id());
        let (_conn, calls) = rt.block_on(fake_server(&suffix, Some("open")));
        let (tx, rx) = std::sync::mpsc::channel();
        let shown = rt.block_on(show_with_action(
            Some(suffix.clone()),
            "Following: 2 new",
            "Arsenal · Sat 12:30",
            ("open", "Open"),
            Box::new(move || tx.send(()).unwrap()),
        ));
        assert!(shown);
        rx.recv_timeout(Duration::from_secs(10)).expect("open action delivered");
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "Following: 2 new");
        assert_eq!(calls[0].2, vec!["default", "Open", "open", "Open"]);
    }

    /// Unit tests never reach the desktop's notification server (they run real jobs): the
    /// session target answers "not shown" at once, so callers toast instead.
    #[test]
    fn unit_tests_never_notify_the_desktop() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert!(!notify("Following: 1 new", "", false));
        assert!(!rt.block_on(notify_with_open("Following: 1 new", "", Box::new(|| {}))));
    }

    #[test]
    fn a_wedged_notification_server_does_not_stall_reminders() {
        if !session_bus_available() {
            eprintln!("skipped: no session bus");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let suffix = format!("tuner_wedged_{}", std::process::id());
        // Owns the name and never answers a call (no object server on this connection).
        let _conn = rt.block_on(async {
            zbus::conn::Builder::session().unwrap().name(format!("de.hoodie.Notification.{suffix}")).unwrap().build().await.unwrap()
        });
        let notifier = DesktopNotifier { bus: Some(suffix) };
        let note = Note { summary: "News".into(), body: "Starts in 2 min".into() };
        let t = Instant::now();
        assert!(!rt.block_on(notifier.show_reminder(note, Box::new(|| {}))));
        let took = t.elapsed();
        assert!(took >= NOTIFY_TIMEOUT && took < NOTIFY_TIMEOUT * 2, "gave up after {took:?}");
    }
}
