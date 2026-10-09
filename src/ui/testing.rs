//! Headless UI test harness (skeleton, frozen). Builds real [`Services`] in a private temp
//! profile with mpv `vo=null`, `ao=null` (no GPU, no audio device, no Wayland) and owns every
//! part a [`UiCtx`] borrows, so screen/overlay/widget tests (and `egui_kittest` harnesses
//! built by the test itself) can call `on_action` / `ui` exactly like the app does.
//!
//! ```ignore
//! let mut t = tuner::ui::testing::TestUi::new().unwrap();
//! let mut screen = tuner::ui::screens::live::LiveScreen::default();
//! t.with_ctx(|ctx| screen.on_action(ctx, Action::Down));
//! ```
//! Seed data with raw SQL through `t.services().store.writer()` (see `tests/support/seed.rs`),
//! then `t.set_library(...)` with an index built via the `from_parts` constructors.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use super::data::{FrameBudget, NowNextCache};
use super::video_host::VideoHost;
use super::{LayoutClass, NavRequest, ParentalSession, UiCtx, UiParts};
use crate::config::Config;
use crate::error::Result;
use crate::msg::{AppEvent, EventBus, Toast};
use crate::player::PlayerSnapshot;
use crate::services::Services;
use crate::store::library::{FavIndex, Library};
use crate::theme::Theme;

pub struct TestUi {
    services: Option<Services>,
    pub events: crossbeam_channel::Receiver<AppEvent>,
    pub egui: egui::Context,
    pub theme: Theme,
    pub lib: Arc<Library>,
    pub favs: FavIndex,
    pub db: rusqlite::Connection,
    pub budget: FrameBudget,
    pub now_next: NowNextCache,
    pub config: Arc<Config>,
    pub snapshot: Arc<PlayerSnapshot>,
    pub images: crate::images::ImageCache,
    pub video: VideoHost,
    pub parental: ParentalSession,
    pub search_index: Option<crate::search::SearchIndex>,
    pub layout: LayoutClass,
    pub now: crate::model::Timestamp,
    pub nav: Vec<NavRequest>,
    pub toasts: Vec<Toast>,
    pub config_edits: Vec<crate::ui::ConfigEdit>,
    dir: PathBuf,
}

impl TestUi {
    /// Fresh profile with default config (+ headless mpv options).
    pub fn new() -> Result<Self> { Self::with_config(Config::default()) }

    pub fn with_config(mut config: Config) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("tuner-testui-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let paths = crate::paths::AppPaths::rooted(&dir);
        config.player.mpv_options.insert("vo".into(), "null".into());
        config.player.mpv_options.insert("ao".into(), "null".into());
        config.integration.mpris = false;
        let (bus, events) = EventBus::new(None);
        let services = Services::start(paths, config, crate::logging::LogBuffer::new(1000), bus)?;
        let db = services.store.reader()?;
        let egui = egui::Context::default();
        let images = crate::images::ImageCache::new(&services.paths, services.http.clone(), services.rt.handle().clone(), egui.clone(), 16 << 20);
        let config = services.config();
        let snapshot = services.player.handle().snapshot();
        Ok(Self {
            services: Some(services),
            events,
            egui,
            theme: Theme::fallback(),
            lib: Arc::new(Library::default()),
            favs: FavIndex::default(),
            db,
            budget: FrameBudget::default(),
            now_next: NowNextCache::default(),
            config,
            snapshot,
            images,
            video: VideoHost::new(None),
            parental: ParentalSession::default(),
            search_index: None,
            layout: LayoutClass::Wide,
            now: crate::model::now(),
            nav: Vec::new(),
            toasts: Vec::new(),
            config_edits: Vec::new(),
            dir,
        })
    }

    pub fn services(&self) -> &Services { self.services.as_ref().expect("services alive") }

    /// Replace the library snapshot (and the app-owned favourites, like the app does on swap).
    pub fn set_library(&mut self, lib: Library) {
        self.favs = lib.favs.clone();
        self.lib = Arc::new(lib);
    }

    /// Refresh `snapshot` from the player (after sending commands).
    pub fn refresh_snapshot(&mut self) { self.snapshot = self.services().player.handle().snapshot(); }

    /// Run `f` with a `UiCtx` over the owned parts.
    pub fn with_ctx<R>(&mut self, f: impl FnOnce(&mut UiCtx<'_>) -> R) -> R {
        self.budget.begin_frame();
        self.now_next.begin_frame();
        let services = self.services.as_ref().expect("services alive");
        let mut ctx = UiCtx::new(UiParts {
            theme: &self.theme,
            services,
            lib: &self.lib,
            favs: &mut self.favs,
            db: &self.db,
            budget: &mut self.budget,
            now_next: &mut self.now_next,
            config: &self.config,
            player: &self.snapshot,
            images: &mut self.images,
            video: &mut self.video,
            parental: &mut self.parental,
            search_index: &mut self.search_index,
            layout: self.layout,
            mini_rect: None,
            now: self.now,
            nav: &mut self.nav,
            toasts: &mut self.toasts,
            config_edits: &mut self.config_edits,
        });
        f(&mut ctx)
    }
}

impl Drop for TestUi {
    fn drop(&mut self) {
        if let Some(s) = self.services.take() {
            s.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
