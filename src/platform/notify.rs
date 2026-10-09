//! Desktop notifications (notify-rust over zbus) + the reminder scheduler. Owner: WP-08.
//! DESIGN §21.3.
//!
//! Omarchy renders freedesktop notifications in its shell (accent border). Reminders:
//! * The scheduler task loads `store::user::pending_reminders(now)` at start and whenever the
//!   app calls [`ReminderScheduler::refresh`] (the app does so on `DataScope::Reminders`), sleeps until the next `start - lead_s`, then sends a notification
//!   "‹programme› starts in 2 min on ‹channel›" with actions `watch` ("Watch") and `default`;
//!   marks it `Fired`. Missed reminders (app was closed) older than their stop time are marked
//!   `Dismissed` silently; ones still airing fire immediately with "started N min ago".
//! * Action "watch" → `PlatformEvent::ReminderWatch` (UI resolves the channel by ItemRef, plays
//!   it and calls `hypr::focus_self`).
//!
//! Reminders only fire while Tuner is running (v1 limitation, documented in Settings).

use crate::error::Result;
use crate::msg::EventBus;
use crate::store::Store;

/// Fire-and-forget notification (errors logged). Summary/body must be redacted already.
/// Returns false when no notification server answered — callers fall back to an in-app
/// toast (DESIGN §13.12 "Notifications unavailable").
pub fn notify(summary: &str, body: &str, urgency_critical: bool) -> bool { todo!("WP-08") }

pub struct ReminderScheduler {
    task: Option<tokio::task::JoinHandle<()>>,
    wake: tokio::sync::mpsc::UnboundedSender<()>,
}

impl ReminderScheduler {
    /// SKELETON SAFE DEFAULT: `Err` (logged; reminders don't fire).
    pub fn start(rt: &tokio::runtime::Handle, store: Store, bus: EventBus, lead_default_s: i64) -> Result<Self> {
        Err(crate::error::Error::other("reminders not implemented yet (WP-08)"))
    }

    /// Reload pending reminders (after add/remove).
    pub fn refresh(&self) { let _ = self.wake.send(()); }
}
