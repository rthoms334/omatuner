//! What the app shares with code that runs off the UI thread (B2; DESIGN §30.4): the MCP
//! handler, the queue engine, the Following job, the spoiler tracker and the MPRIS router.
//! Lifted from the MCP pass's `AgentShared` (same API: `publish_library`, `library`,
//! `parental`); owned by the integrator.
//!
//! These callers answer on the tokio runtime, never on the UI thread (which may not run while
//! the window sits on a hidden workspace). They read:
//! * the snapshot the app last published ([`AppShared::publish_library`], called by the app
//!   whenever `TunerApp::lib` is replaced). The previous snapshot comes back to the caller so the
//!   app drops it on the memory maintenance thread, never on the UI thread;
//! * the parental state, computed at request time from the published unlock instant and the
//!   live config ([`AppShared::parental`]) — so a session that relocked by its timer while the
//!   UI wasn't running counts as locked, and explicit content in a snapshot built while unlocked
//!   is concealed by the reader itself (`ParentalView::hides_explicit`);
//! * the spoiler guard ([`AppShared::guard`], identity until S1), the pictures as the UI last
//!   published them ([`AppShared::tiles`], B1), the main picture's snapshots
//!   ([`AppShared::main_player`]), Up next's snapshot ([`AppShared::queue`], empty until
//!   Q-core) and the window ([`AppShared::window`]: where an agent's play opens).
//!
//! Never hold a snapshot across an await: take it, use it, drop it (the memory field fixes).
//! Headless `tuner --refresh` has no published library: it uses `Library::load` and treats the
//! session as locked.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use tokio::sync::watch;

use crate::config::ParentalConfig;
use crate::platform::notify::ReminderWaker;
use crate::playback::TilesState;
use crate::player::PlayerSnapshot;
use crate::queue::QueueSnapshot;
use crate::reminders::ui::WindowNow;
use crate::store::library::{ExplicitGate, Library};
use crate::ui::ParentalSession;

#[derive(Debug)]
pub struct AppShared {
    lib: RwLock<Arc<Library>>,
    /// The published snapshot's generation, for services that follow the library (Up next).
    lib_generation: watch::Sender<u64>,
    unlocked_at: Mutex<Option<Instant>>,
    /// The reminder scheduler's waker (`Services::start_reminders`): a switch the handler
    /// wrote is picked up at once, UI frame or not.
    reminders: Mutex<Option<ReminderWaker>>,
    /// Whether explicit content shows now (the jobs' gate; the app writes it every frame).
    explicit: ExplicitGate,
    /// The spoiler guard (B4).
    guard: RwLock<crate::guard::Guard>,
    /// The pictures (B1), published by the UI.
    tiles: watch::Sender<TilesState>,
    /// The main picture's snapshots (`Tiles::main_watch`); `None` before the player starts.
    main_player: Mutex<Option<watch::Receiver<Arc<PlayerSnapshot>>>>,
    /// Up next's snapshots (the engine publishes; empty until Q-core).
    queue: watch::Sender<Arc<QueueSnapshot>>,
    /// Up next's engine (`Services::start_integrations`; `None` while it is off).
    queue_handle: Mutex<Option<crate::queue::QueueHandle>>,
    /// The sleep timer (`Services::start_integrations`): the agent's `sleep_timer`, the menu.
    sleep: Mutex<Option<crate::reminders::sleep::SleepHandle>>,
    /// Picture B's snapshots while it exists (the spoiler tracker, multi-view).
    tile_b: Mutex<Option<watch::Receiver<Arc<PlayerSnapshot>>>>,
    /// Tuner's window as the app last saw it (where an agent's play opens, DESIGN §30.13);
    /// `None` before the app's first frame.
    window: Mutex<Option<WindowNow>>,
}

impl Default for AppShared {
    fn default() -> Self {
        Self::new(ExplicitGate::default())
    }
}

/// The parental state at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentalView {
    /// No PIN, or the PIN was entered and the session hasn't relocked.
    pub unlocked: bool,
    /// Explicit content is hidden right now (`ParentalConfig::hides_explicit`).
    pub hides_explicit: bool,
    /// A PIN is set (locked items need it).
    pub pin_active: bool,
}

impl AppShared {
    /// Shared state around the jobs' explicit gate.
    pub fn new(explicit: ExplicitGate) -> Self {
        Self {
            lib: RwLock::default(),
            lib_generation: watch::Sender::new(0),
            unlocked_at: Mutex::default(),
            reminders: Mutex::default(),
            explicit,
            guard: RwLock::default(),
            tiles: watch::Sender::new(TilesState::default()),
            main_player: Mutex::default(),
            queue: watch::Sender::new(Arc::default()),
            queue_handle: Mutex::default(),
            sleep: Mutex::default(),
            tile_b: Mutex::default(),
            window: Mutex::default(),
        }
    }

    /// Publish the app's snapshot; returns the one it replaces (the caller releases it off the
    /// UI thread), `None` when `lib` is already the published one.
    #[must_use = "drop the old snapshot through `TunerApp::release`"]
    pub fn publish_library(&self, lib: &Arc<Library>) -> Option<Arc<Library>> {
        // every frame: the usual "unchanged" answer takes only the read lock
        if Arc::ptr_eq(&self.lib.read().unwrap_or_else(|e| e.into_inner()), lib) {
            return None;
        }
        let mut cur = self.lib.write().unwrap_or_else(|e| e.into_inner());
        if Arc::ptr_eq(&cur, lib) {
            return None;
        }
        let old = std::mem::replace(&mut *cur, lib.clone());
        drop(cur);
        self.lib_generation.send_replace(lib.generation);
        Some(old)
    }

    /// Wakes when a new snapshot is published (its generation).
    pub fn subscribe_library(&self) -> watch::Receiver<u64> {
        self.lib_generation.subscribe()
    }

    /// The last published snapshot (`Library::default()` — `built_at` 0 — before the first).
    pub fn library(&self) -> Arc<Library> {
        self.lib.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The app's parental session changed (`ParentalSession::unlocked_at`).
    pub fn set_unlocked_at(&self, at: Option<Instant>) {
        *self.unlocked_at.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// The reminder scheduler (re)started (`None`: it isn't running).
    pub fn set_reminder_waker(&self, waker: Option<ReminderWaker>) {
        *self.reminders.lock().unwrap_or_else(|e| e.into_inner()) = waker;
    }

    /// Ask the reminder scheduler to re-read its reminders (a switch was added or removed).
    pub fn wake_reminders(&self) {
        if let Some(w) = self.reminders.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            w.wake();
        }
    }

    /// Whether explicit content shows now.
    pub fn explicit(&self) -> &ExplicitGate {
        &self.explicit
    }

    /// The spoiler guard (cheap clone).
    pub fn guard(&self) -> crate::guard::Guard {
        self.guard.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A new guard snapshot (S1's builder).
    pub fn set_guard(&self, g: crate::guard::Guard) {
        *self.guard.write().unwrap_or_else(|e| e.into_inner()) = g;
    }

    /// The pictures as the UI last published them.
    pub fn tiles(&self) -> TilesState {
        *self.tiles.borrow()
    }

    /// The UI publishes the pictures (focus, sound, what each plays); unchanged → no wake-up.
    pub fn publish_tiles(&self, t: TilesState) {
        self.tiles.send_if_modified(|cur| std::mem::replace(cur, t) != t);
    }

    /// Changes of the pictures (MPRIS router, reminder routing while hidden).
    pub fn subscribe_tiles(&self) -> watch::Receiver<TilesState> {
        self.tiles.subscribe()
    }

    /// The main picture's player started (`Services::start`).
    pub fn set_main_player(&self, rx: watch::Receiver<Arc<PlayerSnapshot>>) {
        *self.main_player.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    }

    /// The main picture's snapshots (the queue engine, the spoiler tracker).
    pub fn main_player(&self) -> Option<watch::Receiver<Arc<PlayerSnapshot>>> {
        self.main_player.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A picture's snapshots: A is the main picture, B only while it exists.
    pub fn tile_player(&self, tile: crate::model::TileId) -> Option<watch::Receiver<Arc<PlayerSnapshot>>> {
        if tile == crate::model::TileId::A { self.main_player() } else { self.tile_b.lock().unwrap_or_else(|e| e.into_inner()).clone() }
    }

    /// Picture B started (`Services::adopt_tile_b`) or went away (`None`).
    pub fn set_tile_b(&self, rx: Option<watch::Receiver<Arc<PlayerSnapshot>>>) {
        *self.tile_b.lock().unwrap_or_else(|e| e.into_inner()) = rx;
    }

    /// Up next's engine, for the agent and MPRIS (`None` while Up next is off).
    pub fn queue_handle(&self) -> Option<crate::queue::QueueHandle> {
        self.queue_handle.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The queue engine started.
    pub fn set_queue_handle(&self, h: Option<crate::queue::QueueHandle>) {
        *self.queue_handle.lock().unwrap_or_else(|e| e.into_inner()) = h;
    }

    /// The sleep timer (`None` before the services start it).
    pub fn sleep(&self) -> Option<crate::reminders::sleep::SleepHandle> {
        self.sleep.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The sleep timer's service started.
    pub fn set_sleep(&self, h: Option<crate::reminders::sleep::SleepHandle>) {
        *self.sleep.lock().unwrap_or_else(|e| e.into_inner()) = h;
    }

    /// The app publishes its window every frame (`TunerApp::window_now`).
    pub fn set_window(&self, w: WindowNow) {
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = Some(w);
    }

    /// Tuner's window as last published (`None` before the app's first frame).
    pub fn window(&self) -> Option<WindowNow> {
        *self.window.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Up next's latest snapshot.
    pub fn queue(&self) -> Arc<QueueSnapshot> {
        self.queue.borrow().clone()
    }

    /// Changes of Up next (UI panel, MPRIS, Home, the agent).
    pub fn subscribe_queue(&self) -> watch::Receiver<Arc<QueueSnapshot>> {
        self.queue.subscribe()
    }

    /// The queue engine publishes.
    pub fn publish_queue(&self, q: Arc<QueueSnapshot>) {
        self.queue.send_replace(q);
    }

    /// The parental state now, under `cfg` (the live config).
    pub fn parental(&self, cfg: &ParentalConfig) -> ParentalView {
        let at = *self.unlocked_at.lock().unwrap_or_else(|e| e.into_inner());
        let session = ParentalSession::with_unlocked_at(at);
        ParentalView { unlocked: session.is_unlocked(cfg), hides_explicit: session.hides_explicit(cfg), pin_active: cfg.pin_active() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_hands_back_the_old_snapshot_once() {
        let shared = AppShared::default();
        assert_eq!(shared.library().built_at, 0);
        let a = Arc::new(Library { built_at: 1, ..Default::default() });
        let old = shared.publish_library(&a).expect("replaced the default");
        assert_eq!(old.built_at, 0);
        assert!(shared.publish_library(&a).is_none(), "same snapshot");
        assert_eq!(shared.library().built_at, 1);
    }

    #[test]
    fn the_parental_view_follows_the_unlock_and_the_relock_timer() {
        let salt = crate::ui::parental::new_salt();
        let cfg = ParentalConfig {
            enabled: true,
            pin_hash: Some(crate::ui::parental::hash_pin("1234", &salt)),
            pin_salt: Some(salt),
            relock_min: 1,
            ..Default::default()
        };
        let shared = AppShared::default();
        assert_eq!(shared.parental(&cfg), ParentalView { unlocked: false, hides_explicit: true, pin_active: true });
        shared.set_unlocked_at(Some(Instant::now()));
        assert!(shared.parental(&cfg).unlocked && !shared.parental(&cfg).hides_explicit);
        // unlocked two minutes ago with a one-minute relock: locked again, UI running or not
        shared.set_unlocked_at(Instant::now().checked_sub(std::time::Duration::from_secs(120)));
        assert!(!shared.parental(&cfg).unlocked);
        let open = ParentalConfig::default();
        assert_eq!(shared.parental(&open), ParentalView { unlocked: true, hides_explicit: true, pin_active: false });
    }
}
