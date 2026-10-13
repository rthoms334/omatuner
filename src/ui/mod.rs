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

pub mod casting;
pub mod click_guard;
pub mod copies;
pub mod data;
pub mod epg_grid;
pub mod item_menu;
pub mod layout;
pub mod multiview;
pub mod overlays;
pub mod parental;
pub mod popout;
pub mod scope;
pub mod screens;
pub mod stacks;
#[doc(hidden)]
pub mod testing;
pub mod video_host;
pub mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::config::Config;
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, ChannelSummary, Collection, GroupId, ItemRef, MovieId, ProgrammeId, SeriesId, SourceId, Timestamp};
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
    /// Up next (Alt+6, DESIGN §32).
    Queue,
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
    /// Following rules and what they matched (DESIGN §33; listed once `follow::ENABLED`).
    Following,
    /// The spoiler guard (DESIGN §34; listed once `spoiler::ENABLED`).
    SpoilerGuard,
    /// Connect AI agents to Tuner (DESIGN §39).
    Agents,
    /// Casting to TVs (DESIGN §40; listed once `cast::ENABLED`).
    Cast,
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
    /// Settings ▸ Playlists ▸ "Add free channels": the free-channel presets (DESIGN §27.8).
    FreeChannels,
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
    /// Settings ▸ Playlists with one playlist's editor open (a scope tab's "Edit playlist…").
    EditPlaylist(SourceId),
    /// Sidebar and status bar are hidden while this route is current.
    Onboarding(OnboardingStart),
    EpgMapping {
        channel: Option<ChannelId>,
    },
    Logs,
    /// Up next (Alt+6, DESIGN §32).
    Queue,
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
            Route::Settings(_) | Route::EditPlaylist(_) => ScreenId::Settings,
            Route::Onboarding(_) => ScreenId::Onboarding,
            Route::EpgMapping { .. } => ScreenId::EpgMapping,
            Route::Logs => ScreenId::Logs,
            Route::Queue => ScreenId::Queue,
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
    /// "Choose what to show": a playlist's category filter (DESIGN §28.6).
    Categories(SourceId),
    /// Alt+P: which playlist a screen shows (DESIGN §29). The overlay applies the choice.
    PlaylistPicker(scope::ScopeKind),
    /// A scope tab's right-click menu: edit the playlist, move it up / down (DESIGN §29).
    PlaylistMenu(SourceId, scope::ScopeKind),
    /// "Same title as…": combine a movie or series with another title (DESIGN §29.16). The
    /// overlay writes the join.
    SameTitle(overlays::same_title::SameTitleSpec),
    /// "Delete playlist…" with the offer to move favourites and progress to the same titles
    /// in other playlists (DESIGN §29.16). The overlay deletes; `token` comes back in
    /// `Confirmed` / `Cancelled`.
    DeletePlaylist {
        source: SourceId,
        token: u64,
    },
    /// Following's sheet (`R`, "Follow…"; DESIGN §33, F-C).
    FollowSheet(overlays::follow_sheet::FollowSheetSpec),
    /// The spoiler guard menu (Ctrl+Shift+B; DESIGN §34, S-ui).
    SpoilerMenu,
    /// Add / edit a spoiler topic or blackout (DESIGN §34, S-ui).
    SpoilerRule(overlays::spoiler_rule::SpoilerRuleSpec),
    /// How many episodes of a series to add to Up next (DESIGN §32, Q-ui).
    QueueSeries {
        series: SeriesId,
        play_next: bool,
    },
    /// The sleep timer menu (Ctrl+Shift+T; B6, rui).
    SleepTimer,
    /// A programme's reminder sheet: notify or switch, lead, start offset, where it opens
    /// (B6, rui).
    ReminderSheet(overlays::reminders::ReminderSheetSpec),
    /// Choose the channel for the other picture (Ctrl+M in the player; DESIGN §35, MV-2).
    WatchAlongside,
    /// The first-run card: connect the AI agents found on this computer (DESIGN §39). Opened
    /// by the app, once per Tuner version.
    AgentConnect(crate::agent::connect::Offer),
    /// "Cast to…" (`C`, the OSD Cast control, the player menu; DESIGN §40 §3.1): the right-edge
    /// device picker over the player (CAST-8).
    CastPicker,
    /// PIN pairing with a TV on Tuner's own screen (DESIGN §40 §3.2; CAST-8). Opened from the
    /// picker and the casting card (`then_cast`: what plays moves to the TV once paired), and
    /// from Settings ▸ Cast and the setup wizard (pairing only).
    CastPair {
        device: crate::model::DeviceId,
        then_cast: bool,
    },
    /// The firewall card: Allow on firewall, "Waiting for your password…", the outcomes, Show
    /// command (DESIGN §40 §3.4, §9.4; CAST-9).
    CastFirewall,
    /// The first-run card and setup wizard: pair → network → Test on TV (DESIGN §40 §3.5;
    /// CAST-9). Opened by the app, once per Tuner version.
    CastOffer(crate::cast::SetupOffer),
    /// "Cast to…" on an item with no single TV ready (DESIGN §40 §3.1): the picker for that
    /// item, which casts it itself (`ui::casting`).
    CastItem(casting::ItemCast),
    /// The Play on chooser (DESIGN §40 §3.9; the status chip, Alt+T, Settings ▸ Cast): this
    /// computer or a TV.
    PlayOn,
    /// [`OverlayRequest::CastPair`] with what follows the pairing: the item "Cast to…" was for,
    /// or the Play on switch.
    CastPairThen {
        device: crate::model::DeviceId,
        then: casting::AfterPair,
    },
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
    /// Pop the window out to the video-only player on every workspace, or back in (the
    /// player's double-click; DESIGN §21.6).
    TogglePopOut,
    /// Multi-view (DESIGN §35): applied by the app after the frame (`app/multiview.rs`).
    MultiView(multiview::MvRequest),
}

/// What the app's Back leads to from the current route: the route below it, or the parent of a
/// lone sub-page (named for the back button: "‹ Settings").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackTarget {
    pub label: &'static str,
}

impl BackTarget {
    /// The back button's label for going back to `screen`.
    pub fn to(screen: ScreenId) -> Self {
        let label = match screen {
            ScreenId::Home => "Home",
            ScreenId::Live => "Live TV",
            ScreenId::Guide => "Guide",
            ScreenId::Movies => "Movies",
            ScreenId::Series => "Series",
            ScreenId::MovieDetail => "Movie",
            ScreenId::SeriesDetail => "Series",
            ScreenId::Player => "Player",
            ScreenId::Settings => "Settings",
            ScreenId::Onboarding => "Setup",
            ScreenId::EpgMapping => "Guide fixes",
            ScreenId::Logs => "Logs",
            ScreenId::Queue => "Up next",
        };
        Self { label }
    }
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
    /// Quality badges measured this session (read through [`UiCtx::quality`]).
    pub measured: &'a data::MeasuredQuality,
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
    /// The window is popped out: the player draws its video-only layout (DESIGN §12.3).
    pub video_only: bool,
    /// Where the app's Back (Esc) goes from the current route, for the on-screen back button
    /// ([`widgets::section::page_header`]); `None` on a tab root. Set by the app each frame.
    pub back: Option<BackTarget>,
    /// Multi-view this frame (both pictures, focus, sound); set by the app each frame, off
    /// otherwise (screen tests). `player` is the focused picture's snapshot.
    pub multiview: multiview::MvFrame,
    pub now: Timestamp,
    nav: &'a mut Vec<NavRequest>,
    /// Multi-view requests made through `&self` (`play`); the app moves them to `nav`.
    mv_out: std::cell::RefCell<Vec<multiview::MvRequest>>,
    toasts: &'a mut Vec<Toast>,
    config_edits: &'a mut Vec<ConfigEdit>,
    /// "Cast to…" in progress ([`UiCtx::cast_with`]): where this action's plays go, and whether
    /// one went.
    cast: std::cell::RefCell<Option<(casting::CastDest, bool)>>,
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
    pub measured: &'a data::MeasuredQuality,
    pub images: &'a mut crate::images::ImageCache,
    pub video: &'a mut video_host::VideoHost,
    pub parental: &'a mut ParentalSession,
    pub search_index: &'a mut Option<crate::search::SearchIndex>,
    pub layout: LayoutClass,
    pub mini_rect: Option<egui::Rect>,
    pub video_only: bool,
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
            measured: p.measured,
            images: p.images,
            video: p.video,
            parental: p.parental,
            search_index: p.search_index,
            layout: p.layout,
            mini_rect: p.mini_rect,
            video_only: p.video_only,
            back: None,
            multiview: Default::default(),
            now: p.now,
            nav: p.nav,
            mv_out: Default::default(),
            toasts: p.toasts,
            config_edits: p.config_edits,
            cast: Default::default(),
        }
    }

    pub fn navigate(&mut self, r: NavRequest) {
        self.nav.push(r);
    }
    pub fn push(&mut self, route: Route) {
        // a cast from where things are chosen stays there: the mini player shows the casting
        // card (DESIGN §40 §3.1)
        if route == Route::Player && self.cast.borrow().is_some() {
            return;
        }
        self.nav.push(NavRequest::Push(route));
    }
    pub fn open(&mut self, o: OverlayRequest) {
        // the Resume prompt of a film "Cast to…" casts: its answer plays on the TV too
        let o = match (o, self.cast.borrow().as_ref()) {
            (OverlayRequest::Resume(mut spec), Some((dest, _))) => {
                spec.cast = Some(dest.clone());
                OverlayRequest::Resume(spec)
            }
            (o, _) => o,
        };
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
    /// Send a player command (non-blocking; never call mpv from the UI thread). Single view: to
    /// the picture on screen. Multi-view (DESIGN §35): volume and mute to the picture with the
    /// sound, a channel or catch-up through the copy planner for the focused picture, a film or
    /// episode ends multi-view ([`multiview::route_command`]); the rest to the focused picture.
    /// Inside [`UiCtx::cast_with`] a play goes to that TV instead (DESIGN §40 §3.1).
    pub fn play(&self, cmd: crate::player::PlayerCommand) {
        use crate::player::PlayerCommand as C;
        // "Cast to…" (`cast_with`): the play goes to the TV instead
        let cmd = match (self.cast.borrow_mut().as_mut(), cmd) {
            (Some((dest, sent)), C::Play(req)) => {
                *sent = true;
                C::CastTo { device: dest.device, play: Some(req) }
            }
            (Some((dest, sent)), C::PlayCopy(copy)) => {
                *sent = true;
                C::CastCopy { device: dest.device, copy }
            }
            (_, cmd) => cmd,
        };
        match multiview::route_command(&self.multiview.view, cmd) {
            multiview::Routed::Focused(cmd) => self.services.tiles.focused().send(cmd),
            multiview::Routed::Audible(cmd) => self.services.tiles.audible_handle().send(cmd),
            multiview::Routed::Request(r) => self.mv_out.borrow_mut().push(r),
        }
    }
    /// "Cast to…" (DESIGN §40 §3.1): run `f` — a play exactly as it plays here, its PIN, copy
    /// and Resume rules included — with its plays sent to `dest`'s TV: `Play` becomes `CastTo`,
    /// `PlayCopy` `CastCopy`, a Resume prompt it opens casts its answer, and the Player screen
    /// isn't opened (the mini player shows the casting card). A play that went out is toasted
    /// ("Casting Dune to Living Room").
    pub fn cast_with<R>(&mut self, dest: casting::CastDest, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.cast.replace(Some((dest, false)));
        let r = f(self);
        if let Some((dest, true)) = self.cast.replace(outer) {
            self.toast(Toast::info(format!("Casting {} to {}", dest.title, dest.tv)));
        }
        r
    }
    /// Send a command to one picture, whatever has the focus (multi-view chrome).
    pub fn play_in(&self, tile: crate::model::TileId, cmd: crate::player::PlayerCommand) {
        if let Some(p) = self.services.tiles.player(tile) {
            p.handle().send(cmd);
        }
    }
    /// Ask multi-view for something (focus, sound, layout, a picture's channel…); the app
    /// applies it after the frame.
    pub fn mv_request(&self, r: multiview::MvRequest) {
        self.mv_out.borrow_mut().push(r);
    }
    /// The requests made this frame (the app turns them into `NavRequest::MultiView`).
    pub fn take_mv_requests(&mut self) -> Vec<multiview::MvRequest> {
        std::mem::take(self.mv_out.get_mut())
    }
    /// The snapshot of the picture with the sound (the volume control's).
    pub fn audible_player(&self) -> Arc<PlayerSnapshot> {
        let mv = &self.multiview;
        match mv.snap(mv.view.audible) {
            Some(s) if mv.on() => s.clone(),
            _ => self.player.clone(),
        }
    }
    /// The quality badge of a channel (measured this session, else the library's: measured
    /// earlier, else from the name), or `None` while Settings ▸ Interface ▸ Quality badges
    /// is off.
    pub fn quality(&self, ch: &ChannelSummary) -> Option<crate::model::QualityBadge> {
        self.quality_badges().of(ch)
    }
    /// [`UiCtx::quality`] as a `Copy` value (for painters that can't borrow the context).
    pub fn quality_badges(&self) -> data::QualityBadges<'a> {
        data::QualityBadges(self.config.interface.quality_badges.then_some(self.measured))
    }

    /// The playlist to show on an item of `kind` from `source` in a view without a playlist
    /// scope (Home, Search, detail pages, the player), or `None` while Settings ▸ Interface ▸
    /// Playlist on cards doesn't show it (DESIGN §13.0, §29). Scoped screens use
    /// [`scope::Scope::marks`].
    pub fn source_of(&self, kind: crate::model::ItemKind, source: SourceId) -> Option<&'a crate::store::library::SourceMeta> {
        self.source_marks(kind).of(source)
    }
    /// [`UiCtx::source_of`] as a `Copy` value (for closures and painters that can't borrow the
    /// context).
    pub fn source_marks(&self, kind: crate::model::ItemKind) -> data::SourceMarks<'a> {
        data::SourceMarks::for_view(self.config.interface.playlist_on_cards, &self.lib.sources, kind, true)
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

    /// Hints the focused element adds to the status bar's (a stack card: "Space copies ·
    /// Ctrl+O play from"), as of the last frame.
    fn hint_extras(&self) -> Vec<(crate::keymap::Action, &'static str)> {
        Vec::new()
    }

    /// The focused item the spoiler guard's Ctrl+B acts on, for the status bar's "Ctrl+B show
    /// spoilers" (DESIGN §34.5); `None`: no such item (the player says it in its banner).
    fn reveal_target(&self, _ctx: &UiCtx<'_>) -> Option<crate::ui::item_menu::Target> {
        None
    }

    /// Whether the corner mini-player may show over this screen while something plays.
    /// False for Onboarding and the Player itself.
    fn allows_mini_player(&self) -> bool {
        true
    }
}
