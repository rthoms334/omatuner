//! Desktop notifications (notify-rust over zbus) + the reminder scheduler. Owner: WP-08.
//! DESIGN §21.3.
//!
//! Omarchy renders freedesktop notifications in its shell (accent border). Reminders:
//! * The scheduler task loads `store::user::pending_reminders` at start and whenever the
//!   app calls [`ReminderScheduler::refresh`] (the app does so on `DataScope::Reminders`), sleeps until the next `start - lead_s`, then sends a notification
//!   "‹programme› starts in 2 min on ‹channel›" with actions `watch` ("Watch") and `default`;
//!   marks it `Fired`. Missed reminders (app was closed) older than their stop time are marked
//!   `Dismissed` silently; ones still airing fire immediately with "started N min ago".
//! * Action "watch" (or a click on the notification body) → `PlatformEvent::ReminderWatch`
//!   (UI resolves the channel by ItemRef, plays it and calls `hypr::focus_self`).
//! * No notification server → the same text as an in-app toast.
//!
//! Timing uses the wall clock and never sleeps longer than a minute, so suspend/resume and
//! clock changes are picked up. Each reminder's own `lead_s` wins; `lead_default_s` (config
//! `reminders.lead_min`) applies to rows without one.
//!
//! Reminders only fire while Tuner is running (v1 limitation, documented in Settings).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use notify_rust::{Hint, Notification, NotificationResponse, Urgency};

use crate::error::{Error, Result};
use crate::model::{Reminder, ReminderId, ReminderState, Timestamp};
use crate::msg::{AppEvent, DataScope, EventBus, Toast, ToastKind};
use crate::platform::PlatformEvent;
use crate::store::Store;

/// How long `notify` waits for the notification server.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest scheduler sleep (wall-clock re-check, see module docs).
const MAX_SLEEP_S: i64 = 60;
/// How long a shown reminder keeps listening for its "Watch" action.
const ACTION_WAIT: Duration = Duration::from_secs(3 * 3600);
/// Programmes without a usable stop time are assumed to last this long.
const DEFAULT_DURATION_S: i64 = 3600;

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
    match bus {
        None => n.show_async().await,
        Some(b) => n.show_async_at_bus(b).await,
    }
    .map_err(Error::other)
}

pub struct ReminderScheduler {
    task: Option<tokio::task::JoinHandle<()>>,
    wake: tokio::sync::mpsc::UnboundedSender<()>,
}

impl ReminderScheduler {
    /// Start the scheduler task on `rt` (store reads run on the blocking pool). Never fails
    /// today; storage errors are logged and retried on the next refresh / minute tick.
    /// `explicit`: while it hides explicit content, reminders on explicit channels stay
    /// silent (they are announced once it shows again, if still airing; DESIGN §20.3).
    pub fn start(
        rt: &tokio::runtime::Handle,
        store: Store,
        bus: EventBus,
        lead_default_s: i64,
        explicit: crate::store::library::ExplicitGate,
    ) -> Result<Self> {
        let backend = Arc::new(StoreBackend { store, reader: Mutex::new(None), explicit });
        Ok(Self::spawn(rt, backend, Arc::new(DesktopNotifier { bus: None }), bus, lead_default_s))
    }

    fn spawn(
        rt: &tokio::runtime::Handle,
        backend: Arc<dyn ReminderBackend>,
        notifier: Arc<dyn Notifier>,
        bus: EventBus,
        lead_default_s: i64,
    ) -> Self {
        let (wake, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = rt.spawn(run(backend, notifier, bus, lead_default_s, rx));
        Self { task: Some(task), wake }
    }

    /// Reload pending reminders (after add/remove).
    pub fn refresh(&self) {
        let _ = self.wake.send(());
    }
}

impl Drop for ReminderScheduler {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Reminder persistence seen by the scheduler (the store in production, fakes in tests).
trait ReminderBackend: Send + Sync + 'static {
    /// Every reminder still `Pending` (blocking; called on the blocking pool).
    fn pending(&self) -> Result<Vec<Reminder>>;
    /// Persist a state change (fire-and-forget, must not block).
    fn set_state(&self, id: ReminderId, state: ReminderState);
}

struct StoreBackend {
    store: Store,
    reader: Mutex<Option<rusqlite::Connection>>,
    explicit: crate::store::library::ExplicitGate,
}

impl ReminderBackend for StoreBackend {
    fn pending(&self) -> Result<Vec<Reminder>> {
        let mut guard = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(self.store.reader()?);
        }
        let conn = guard.as_ref().ok_or_else(|| Error::other("no database reader"))?;
        // A panic in the repository (e.g. not implemented yet) must not kill the scheduler.
        let mut pending = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::user::pending_reminders(conn, Timestamp::MIN)))
            .unwrap_or_else(|_| Err(Error::other("reminder storage is not available")))?;
        // explicit channels' reminders wait, untouched, while parental control hides them: a
        // notification would name the programme and the channel (a channel that can't be
        // read counts as explicit)
        if !self.explicit.visibility().explicit {
            pending.retain(|r| !crate::store::live::item_explicit(conn, &r.channel).unwrap_or(true));
        }
        Ok(pending)
    }

    fn set_state(&self, id: ReminderId, state: ReminderState) {
        let _ = self.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::set_reminder_state(c, id, state));
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
        Box::pin(async move {
            let n = build(&note.summary, &note.body, false, &[("default", "Watch"), ("watch", "Watch")]);
            // zbus calls have no timeout of their own: a wedged notification server must not
            // stall the scheduler (the caller falls back to a toast).
            let handle = match tokio::time::timeout(NOTIFY_TIMEOUT, show(&n, bus.as_deref())).await {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => {
                    tracing::info!("reminder notification not shown: {e}");
                    return false;
                }
                Err(_) => {
                    tracing::info!("reminder notification timed out");
                    return false;
                }
            };
            tokio::spawn(async move {
                let wait = handle.wait_for_action_async(|response| match response {
                    NotificationResponse::Default => on_watch(),
                    NotificationResponse::Action(a) if a == "watch" || a == "default" => on_watch(),
                    _ => {}
                });
                let _ = tokio::time::timeout(ACTION_WAIT, wait).await;
            });
            true
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Decision {
    /// Ended while nobody was looking: mark `Dismissed`, no notification.
    Dismiss(ReminderId),
    Fire(Reminder),
}

#[derive(Debug, Default, PartialEq)]
struct Plan {
    now: Vec<Decision>,
    /// Earliest future fire time.
    next_at: Option<Timestamp>,
}

/// Programme end used for the "missed" test (rows without a sane stop get a default length).
fn effective_stop(r: &Reminder) -> Timestamp {
    if r.stop > r.start { r.stop } else { r.start + DEFAULT_DURATION_S }
}

/// Pure scheduling step: what to do at `now` and when to look again.
fn plan(pending: &[Reminder], now: Timestamp, lead_default_s: i64, handled: &HashSet<ReminderId>) -> Plan {
    let mut p = Plan::default();
    for r in pending.iter().filter(|r| r.state == ReminderState::Pending && !handled.contains(&r.id)) {
        let lead = if r.lead_s > 0 { r.lead_s } else { lead_default_s.max(0) };
        let fire_at = r.start - lead;
        if effective_stop(r) <= now {
            p.now.push(Decision::Dismiss(r.id));
        } else if fire_at <= now {
            p.now.push(Decision::Fire(r.clone()));
        } else {
            p.next_at = Some(p.next_at.map_or(fire_at, |t| t.min(fire_at)));
        }
    }
    p
}

/// "‹programme›" / "Starts in 2 min on ‹channel›" (or "Starting now", "Started 5 min ago").
fn note_for(r: &Reminder, now: Timestamp) -> Note {
    let until = r.start - now;
    let when = if until >= 30 {
        format!("Starts in {} min", (until + 59) / 60)
    } else if until > -60 {
        "Starting now".to_owned()
    } else {
        format!("Started {} min ago", -until / 60)
    };
    let title = if r.programme_title.trim().is_empty() { "Reminder" } else { r.programme_title.trim() };
    Note { summary: title.to_owned(), body: format!("{when} on {}", r.channel_name.trim()) }
}

async fn run(
    backend: Arc<dyn ReminderBackend>,
    notifier: Arc<dyn Notifier>,
    bus: EventBus,
    lead_default_s: i64,
    mut wake: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    // Fired/dismissed here but maybe not yet written: never act on them twice.
    let mut handled: HashSet<ReminderId> = HashSet::new();
    // Last load error, so a persistent one is logged once rather than every minute.
    let mut last_error: Option<String> = None;
    loop {
        let b = backend.clone();
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
        handled.retain(|id| pending.iter().any(|r| r.id == *id));
        let now = crate::model::now();
        let p = plan(&pending, now, lead_default_s, &handled);
        for d in p.now {
            match d {
                Decision::Dismiss(id) => {
                    handled.insert(id);
                    backend.set_state(id, ReminderState::Dismissed);
                    tracing::info!("reminder {id} missed (programme ended); dismissed");
                }
                Decision::Fire(r) => {
                    handled.insert(r.id);
                    backend.set_state(r.id, ReminderState::Fired);
                    bus.send(AppEvent::Platform(PlatformEvent::ReminderFired(r.id)));
                    let note = note_for(&r, now);
                    let watch_bus = bus.clone();
                    let (id, channel) = (r.id, r.channel.clone());
                    // Raise right away as well (DESIGN §21.3): the UI may not be running while
                    // the window sits on another workspace, so the event alone could wait.
                    let on_watch: Box<dyn FnOnce() + Send> = Box::new(move || {
                        watch_bus.send(AppEvent::Platform(PlatformEvent::ReminderWatch { reminder: id, channel }));
                        crate::platform::hypr::focus_self();
                    });
                    if !notifier.show_reminder(note.clone(), on_watch).await {
                        bus.toast(Toast { kind: ToastKind::Info, text: note.toast_text(), action: None, ttl_ms: 10_000 });
                    }
                }
            }
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
        let p = plan(&pending, now, 240, &HashSet::new());
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
        assert_eq!(plan(&pending, now, 120, &handled), Plan::default());
    }

    #[test]
    fn note_texts() {
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

    #[derive(Default)]
    struct FakeBackend {
        rows: Mutex<Vec<Reminder>>,
        states: Mutex<Vec<(ReminderId, ReminderState)>>,
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
        let backend = Arc::new(FakeBackend::default());
        *backend.rows.lock().unwrap() = vec![
            reminder(1, now + 60, now + 3600, 120),   // due now
            reminder(2, now - 7200, now - 3600, 120), // missed
            reminder(3, now + 2, now + 3600, 1),      // due in ~1 s
            reminder(4, now + 7200, now + 9000, 120), // later
        ];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: true, available: true });
        let (bus, rx) = EventBus::new(None);
        let sched = ReminderScheduler::spawn(rt.handle(), backend.clone(), notifier.clone(), bus, 120);
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

    #[test]
    fn scheduler_falls_back_to_a_toast_without_a_notification_server() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let now = crate::model::now();
        let backend = Arc::new(FakeBackend::default());
        *backend.rows.lock().unwrap() = vec![reminder(7, now + 100, now + 900, 120)];
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: false, available: false });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend, notifier, bus, 120);
        let events = recv_until(&rx, Duration::from_secs(5), |ev| matches!(ev, AppEvent::Toast(_)));
        let toast = events.iter().find_map(|e| if let AppEvent::Toast(t) = e { Some(t.text.clone()) } else { None });
        assert_eq!(toast.as_deref(), Some("Programme 7 — Starts in 2 min on BBC One"));
    }

    #[test]
    fn store_backend_survives_unimplemented_storage() {
        // Until WP-02 lands the repository panics; afterwards it returns rows. Either way the
        // scheduler must get a Result, never a panic.
        let store = Store::open_temp("tuner-reminders-test", None).unwrap();
        let backend = StoreBackend { store, reader: Mutex::new(None), explicit: Default::default() };
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
        let backend = StoreBackend { store, reader: Mutex::new(None), explicit: gate.clone() };
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
        let backend = Arc::new(StoreBackend { store: store.clone(), reader: Mutex::new(None), explicit: Default::default() });
        let notifier = Arc::new(FakeNotifier { shown: Mutex::new(Vec::new()), click: true, available: true });
        let (bus, rx) = EventBus::new(None);
        let _sched = ReminderScheduler::spawn(rt.handle(), backend, notifier.clone(), bus, 120);
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
        tokio::spawn(async move {
            let mut stream = zbus::MessageStream::from(&c);
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
