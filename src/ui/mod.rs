//! UI framework: routes, screens, overlays, per-frame context. FROZEN contract (skeleton);
//! bodies per file owner. DESIGN §12–§13.
//!
//! * A [`Screen`] is a full-page view behind the sidebar (Home, Live, Guide, Movies, Series,
//!   details, Settings, Onboarding, Player, Logs, EPG mapping). Screens are kept alive in the
//!   app (state survives navigation) and receive a [`UiCtx`] every frame.
//! * Overlays (search, help, dialogs, menus, programme popover) stack above screens; the
//!   topmost overlay owns the keyboard (DESIGN §15.1 "layers").
//! * No screen reads raw keys: every key is a `keymap::Action` delivered through
//!   `on_action` (the app resolves and CONSUMES events before egui sees them).
//! * Screens never block: DB reads are indexed point/window queries on `ctx.db` (UI reader)
//!   inside the per-frame budget (`ctx.budget`, 2 ms) — now/next through `ctx.now_next`;
//!   anything heavier is a job.
//! * UI code never calls mpv: player control is `ctx.play(PlayerCommand)` (non-blocking),
//!   state is `ctx.player` (snapshot).

pub mod data;
pub mod epg_grid;
pub mod layout;
pub mod overlays;
pub mod parental;
pub mod screens;
#[doc(hidden)]
pub mod testing;
pub mod video_host;
pub mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::config::Config;
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, Collection, GroupId, ItemRef, MovieId, ProgrammeId, SeriesId, SourceId, Timestamp};
use crate::msg::{AppEvent, Toast};
use crate::player::PlayerSnapshot;
use crate::services::Services;
use crate::store::library::{FavIndex, Library};
use crate::theme::Theme;

pub use layout::LayoutClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScreenId {
    Home,
    Live,
    Guide,
    Movies,
    Series,
    MovieDetail,
    SeriesDetail,
    Player,
    Settings,
    Onboarding,
    EpgMapping,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SettingsPage {
    #[default]
    Playlists,
    Epg,
    Player,
    Interface,
    /// Favourites lists: create, rename, delete, reorder.
    Favourites,
    /// Pending reminders (Delete cancels).
    Reminders,
    Parental,
    Backup,
    /// Hidden channels/groups (unhide).
    Hidden,
    /// User data whose items vanished (reattach / forget).
    Orphans,
    About,
}

/// Where the onboarding card starts (the same card is reused by Settings ▸ Playlists ▸ Add,
/// drag & drop / `tuner file.m3u`, and restored sources that need credentials).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OnboardingStart {
    /// First run: Welcome (Add a playlist · Restore a backup).
    #[default]
    Welcome,
    /// Settings ▸ Playlists ▸ "+ Add": the Type step.
    AddSource,
    /// A local playlist file (CLI argument, drag & drop): Details step, name prefilled.
    PlaylistFile(PathBuf),
    /// A restored source without password: Details step for that source.
    NeedsCredentials(SourceId),
}

/// Navigation target with arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Home,
    Live {
        group: Option<GroupId>,
        channel: Option<ChannelId>,
    },
    Guide {
        collection: Option<Collection>,
        focus: Option<ChannelId>,
    },
    Movies {
        group: Option<GroupId>,
    },
    Series {
        group: Option<GroupId>,
    },
    MovieDetail(MovieId),
    SeriesDetail(SeriesId),
    /// Fullscreen player for whatever is playing.
    Player,
    Settings(SettingsPage),
    /// Sidebar and status bar are hidden while this route is current.
    Onboarding(OnboardingStart),
    EpgMapping {
        channel: Option<ChannelId>,
    },
    Logs,
}

impl Route {
    pub fn screen(&self) -> ScreenId {
        match self {
            Route::Home => ScreenId::Home,
            Route::Live { .. } => ScreenId::Live,
            Route::Guide { .. } => ScreenId::Guide,
            Route::Movies { .. } => ScreenId::Movies,
            Route::Series { .. } => ScreenId::Series,
            Route::MovieDetail(_) => ScreenId::MovieDetail,
            Route::SeriesDetail(_) => ScreenId::SeriesDetail,
            Route::Player => ScreenId::Player,
            Route::Settings(_) => ScreenId::Settings,
            Route::Onboarding(_) => ScreenId::Onboarding,
            Route::EpgMapping { .. } => ScreenId::EpgMapping,
            Route::Logs => ScreenId::Logs,
        }
    }
}

/// Overlays (modal layers). Owners in `overlays/mod.rs`.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayRequest {
    /// Global search (WP-12).
    Search {
        initial: String,
    },
    Help,
    Jobs,
    /// Also used for Ctrl+S "Sort & view options…" menus.
    ContextMenu(overlays::ContextMenuSpec),
    Confirm(overlays::ConfirmSpec),
    /// Ask for the parental PIN, then run `then`.
    Pin {
        then: Box<Action>,
        reason: String,
    },
    Rename(overlays::RenameSpec),
    /// Pick favourites lists (Ctrl+Shift+F), incl. "New list…".
    ListPicker(overlays::ListPickerSpec),
    /// Guide programme popover (WP-11). `zap`: the list Watch zaps through (the opener's
    /// collection, DESIGN §13.0); `None` = the channel's primary group.
    Programme {
        programme: ProgrammeId,
        channel: ChannelId,
        zap: Option<crate::model::ZapList>,
    },
    /// VOD "Resume from 41:20 / Start over" (WP-12).
    Resume(overlays::ResumeSpec),
    /// Audio / subtitle track menu (WP-11).
    Tracks {
        audio: bool,
    },
    /// Ctrl+I "Edit information": rename, per-channel catch-up offset, EPG assignment, lock,
    /// hide (WP-11).
    EditItem(ItemRef),
}

/// Requests a screen makes during a frame; the app applies them after the frame.
#[derive(Debug, Clone, PartialEq)]
pub enum NavRequest {
    Push(Route),
    /// Replace the current route (tab switches from the sidebar).
    Switch(Route),
    Back,
    Open(OverlayRequest),
    CloseOverlay,
}

/// A queued Settings change (applied by the app after the frame).
pub type ConfigEdit = Box<dyn FnOnce(&mut Config)>;

/// Per-viewer UI session state that isn't config (parental unlock etc.).
pub use parental::ParentalSession;

/// Everything a screen needs for one frame.
pub struct UiCtx<'a> {
    pub theme: &'a Theme,
    pub services: &'a Services,
    /// Current catalogue snapshot (swapped atomically after imports). Do NOT read
    /// `lib.favs`; use [`UiCtx::favs`].
    pub lib: &'a Arc<Library>,
    /// Favourites, owned by the app: patch optimistically with `apply_toggle` right after
    /// submitting `store::user::set_favourite` (DESIGN §5.4).
    pub favs: &'a mut FavIndex,
    /// UI-thread read-only DB connection (cheap indexed queries only, within `budget`).
    pub db: &'a rusqlite::Connection,
    /// Per-frame UI-thread DB time budget (2 ms).
    pub budget: &'a mut data::FrameBudget,
    /// Now/next cache keyed by (epg channel, tvg-shift).
    pub now_next: &'a mut data::NowNextCache,
    /// Read-only view of the config; mutate via [`UiCtx::edit_config`].
    pub config: &'a Config,
    pub player: &'a Arc<PlayerSnapshot>,
    pub images: &'a mut crate::images::ImageCache,
    pub video: &'a mut video_host::VideoHost,
    pub parental: &'a mut ParentalSession,
    /// Global search index, owned by the app so it survives overlay close/open; reset to
    /// `None` by the app whenever the Library snapshot changes (rebuilt lazily by WP-12).
    pub search_index: &'a mut Option<crate::search::SearchIndex>,
    pub layout: LayoutClass,
    /// Mini-player rect of the previous frame (screens keep focus/scroll targets clear of it,
    /// e.g. the guide's bottom-right cells).
    pub mini_rect: Option<egui::Rect>,
    pub now: Timestamp,
    nav: &'a mut Vec<NavRequest>,
    toasts: &'a mut Vec<Toast>,
    config_edits: &'a mut Vec<ConfigEdit>,
}

/// The app-owned parts a [`UiCtx`] borrows (keeps `UiCtx::new` readable).
pub struct UiParts<'a> {
    pub theme: &'a Theme,
    pub services: &'a Services,
    pub lib: &'a Arc<Library>,
    pub favs: &'a mut FavIndex,
    pub db: &'a rusqlite::Connection,
    pub budget: &'a mut data::FrameBudget,
    pub now_next: &'a mut data::NowNextCache,
    pub config: &'a Config,
    pub player: &'a Arc<PlayerSnapshot>,
    pub images: &'a mut crate::images::ImageCache,
    pub video: &'a mut video_host::VideoHost,
    pub parental: &'a mut ParentalSession,
    pub search_index: &'a mut Option<crate::search::SearchIndex>,
    pub layout: LayoutClass,
    pub mini_rect: Option<egui::Rect>,
    pub now: Timestamp,
    pub nav: &'a mut Vec<NavRequest>,
    pub toasts: &'a mut Vec<Toast>,
    pub config_edits: &'a mut Vec<ConfigEdit>,
}

impl<'a> UiCtx<'a> {
    /// Constructed by the app each frame (and by `ui::testing::TestUi`).
    pub fn new(p: UiParts<'a>) -> Self {
        Self {
            theme: p.theme,
            services: p.services,
            lib: p.lib,
            favs: p.favs,
            db: p.db,
            budget: p.budget,
            now_next: p.now_next,
            config: p.config,
            player: p.player,
            images: p.images,
            video: p.video,
            parental: p.parental,
            search_index: p.search_index,
            layout: p.layout,
            mini_rect: p.mini_rect,
            now: p.now,
            nav: p.nav,
            toasts: p.toasts,
            config_edits: p.config_edits,
        }
    }

    pub fn navigate(&mut self, r: NavRequest) {
        self.nav.push(r);
    }
    pub fn push(&mut self, route: Route) {
        self.nav.push(NavRequest::Push(route));
    }
    pub fn open(&mut self, o: OverlayRequest) {
        self.nav.push(NavRequest::Open(o));
    }
    pub fn toast(&mut self, t: Toast) {
        self.toasts.push(t);
    }
    /// Queue a config change; the app applies it, saves `config.toml`, sends
    /// `PlayerCommand::ApplyConfig` and re-applies dependent state (theme scale) after the frame.
    pub fn edit_config(&mut self, f: impl FnOnce(&mut Config) + 'static) {
        self.config_edits.push(Box::new(f));
    }
    /// Send a player command (non-blocking; never call mpv from the UI thread).
    pub fn play(&self, cmd: crate::player::PlayerCommand) {
        self.services.player.handle().send(cmd);
    }
    /// An `EpgReader` over the UI connection (guide grid, now/next cache).
    pub fn epg_reader(&self) -> data::DbEpg<'a> {
        data::DbEpg(self.db)
    }
}

/// A full-page view. Implementations live in `ui/screens/*` (one file per screen, one owner).
pub trait Screen {
    fn id(&self) -> ScreenId;

    /// Keyboard contexts for the focused element, innermost first (DESIGN §15.1). `Global` is
    /// appended by the app. Must reflect the CURRENT focus (filter active, text field focused,
    /// side panel open, numeric entry, OSD controls focus mode...).
    fn key_contexts(&self) -> Vec<KeyContext>;

    /// Called when the screen becomes current (with its route arguments).
    fn on_enter(&mut self, _ctx: &mut UiCtx<'_>, _route: &Route) {}

    /// Called for every app event while the screen exists (current or not).
    fn on_event(&mut self, _ctx: &mut UiCtx<'_>, _ev: &AppEvent) {}

    /// A resolved key action. Return true if handled (otherwise the app handles globals).
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool;

    /// Draw. `ui` covers the content area (right of the sidebar, above the status bar).
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>);

    /// Result of an overlay this screen opened (confirm token, chosen menu action...).
    fn on_overlay_result(&mut self, _ctx: &mut UiCtx<'_>, _result: overlays::OverlayResult) {}

    /// Whether the corner mini-player may show over this screen while something plays.
    /// False for Onboarding and the Player itself.
    fn allows_mini_player(&self) -> bool {
        true
    }
}
