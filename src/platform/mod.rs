//! Desktop integration: MPRIS, idle inhibit, notifications/reminders, file picker, Hyprland
//! helpers. Owner: WP-08. Every service degrades gracefully (logs a warning and becomes a
//! no-op) when its D-Bus name / Wayland protocol / binary is unavailable.

pub mod filepicker;
pub mod hypr;
pub mod idle;
pub mod mpris;
pub mod notify;

use crate::model::{ItemRef, ReminderId};

/// Events from platform services to the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformEvent {
    /// MPRIS `Raise` — bring the window forward (best effort, `hypr::focus_self`).
    Raise,
    /// MPRIS `Quit`.
    Quit,
    /// A reminder notification fired (the UI may show a toast too).
    ReminderFired(ReminderId),
    /// User clicked "Watch" on a reminder notification → the app resolves the channel by its
    /// stable ref (`LiveIndex::by_ref`), plays it and raises the window.
    ReminderWatch { reminder: ReminderId, channel: ItemRef },
    /// File picker result (None = cancelled). `purpose` echoes the request.
    FilePicked { purpose: PickPurpose, path: Option<std::path::PathBuf> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickPurpose {
    OpenPlaylist,
    OpenEpg,
    ExportBackup,
    ImportBackup,
    ExportLog,
}
