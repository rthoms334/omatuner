//! Messages into the UI thread. FROZEN (skeleton).
//!
//! Every background component (jobs, player controller, theme watcher, platform services,
//! store writer, IPC) holds an [`EventBus`] clone. `send` enqueues on an unbounded crossbeam
//! channel and calls `egui::Context::request_repaint()`; the app drains the channel at the top
//! of `App::logic` (which eframe also runs while the window is hidden). Background components
//! never block on the UI and never assume an event is processed promptly.

use crossbeam_channel::{Receiver, Sender};

use crate::ipc::IpcRequest;
use crate::jobs::JobEvent;
use crate::platform::PlatformEvent;
use crate::player::PlayerEvent;
use crate::theme::ThemeEvent;

/// What changed in the database, so the UI can refresh only affected views/indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataScope {
    Sources,
    /// Channels/groups of a source were (re)imported → rebuild LiveIndex.
    Live,
    /// Movies/series lists changed → rebuild VodIndex.
    Vod,
    /// Lazy details (vod info / series episodes) were stored.
    VodDetails,
    /// A new EPG generation became active, or mappings changed → flush EPG caches.
    Epg,
    Favourites,
    /// Watchlist rows, order, mode or membership changed (DESIGN §42).
    Watchlists,
    Overrides,
    Progress,
    History,
    Reminders,
    /// Everything (backup restore).
    All,
    /// Following rules or their ledger changed (DESIGN §33).
    Follow,
    /// Spoiler-guard rules or seen events changed (DESIGN §34): rebuild the guard snapshot.
    Spoiler,
    /// The guide check's results (suggestions, probes) changed (DESIGN §31): light, no library
    /// rebuild.
    GuideIssues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

/// Transient notice in the bottom-right toast stack (in-app; not a desktop notification).
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub kind: ToastKind,
    /// Redacted text.
    pub text: String,
    /// Optional action label + action fired when the user presses it (e.g. "Undo").
    pub action: Option<(String, crate::keymap::Action)>,
    /// A second action, beside the first (a cast the TV stopped: "Play here · Cast again").
    pub also: Option<(String, crate::keymap::Action)>,
    pub ttl_ms: u32,
}

impl Toast {
    pub fn info(text: impl Into<String>) -> Self {
        Self { kind: ToastKind::Info, text: text.into(), action: None, also: None, ttl_ms: 3000 }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self { kind: ToastKind::Error, text: text.into(), action: None, also: None, ttl_ms: 6000 }
    }
    /// An M3U playlist now signs in with its Xtream login (DESIGN §27.3); `name` when it could
    /// be any of several playlists.
    pub fn upgraded(name: Option<&str>) -> Self {
        const TEXT: &str = "upgraded to Xtream login — movies & series now available";
        match name {
            Some(n) => Self::info(format!("{n}: {TEXT}")),
            None => Self::info("Upgraded to Xtream login — movies & series now available"),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // rare messages; boxing buys nothing
pub enum AppEvent {
    DownloadBusy(crate::ui::downloads::Busy),
    Job(JobEvent),
    Player(PlayerEvent),
    Theme(ThemeEvent),
    Data(DataScope),
    Platform(PlatformEvent),
    Ipc(IpcRequest),
    /// An MCP client asked for something only the UI can do (DESIGN §30.4).
    Agent(crate::agent::AgentCommand),
    Toast(Toast),
    /// Following's preview service answered (DESIGN §33; F-C draws it).
    Follow(crate::follow::FollowEvent),
    /// The reminder engine: sleep timer, scheduled playback, news flash (B6; remind / rui).
    Reminder(crate::reminders::ReminderEvent),
    /// Multi-view's second picture (DESIGN §35; MV-1 / MV-2).
    Tile(crate::player::tiles::TileEvent),
}

/// Cloneable sender half. `Send + Sync`. The egui context is shared by all clones and may be
/// attached after creation (the bus exists before the window does).
#[derive(Clone)]
pub struct EventBus {
    tx: Sender<AppEvent>,
    egui: std::sync::Arc<std::sync::OnceLock<eframe::egui::Context>>,
}

impl EventBus {
    /// Create the bus. `egui` is `None` in tests/headless (no repaint requests).
    pub fn new(egui: Option<eframe::egui::Context>) -> (Self, Receiver<AppEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cell = std::sync::OnceLock::new();
        if let Some(c) = egui {
            let _ = cell.set(c);
        }
        (Self { tx, egui: std::sync::Arc::new(cell) }, rx)
    }

    /// Attach the egui context once the window exists; affects every clone.
    pub fn attach_egui(&self, egui: eframe::egui::Context) {
        let _ = self.egui.set(egui);
    }

    pub fn send(&self, ev: AppEvent) {
        let _ = self.tx.send(ev);
        self.request_repaint();
    }

    pub fn toast(&self, t: Toast) {
        self.send(AppEvent::Toast(t))
    }

    /// Ask for a frame without sending an event (continuous state changed, e.g. time-pos).
    pub fn request_repaint(&self) {
        if let Some(c) = self.egui.get() {
            c.request_repaint();
        }
    }

    pub fn data_changed(&self, scope: DataScope) {
        self.send(AppEvent::Data(scope))
    }
}
