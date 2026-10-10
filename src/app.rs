//! `eframe::App` — the integration point. Owner: WP-10b. DESIGN §5, §12, §15.
//!
//! Frame anatomy:
//! * `raw_input_hook()` (before egui processes input): resolve every key event against the
//!   current context stack (`keymap::resolve`) and REMOVE resolved events from the input, so
//!   egui widgets never handle them a second time (Enter would also click the focused button,
//!   Tab would move egui focus). egui-winit reports Alt chords as `Key{alt}` followed by the
//!   `Text` of the bare key; when the Key resolves, that Text is dropped too (Alt+1 must not
//!   also type "1" into a filter). Also maps the mouse back button to `Back`.
//! * `logic()`: drain `AppEvent`s, refresh the player snapshot, render mpv into the FBO (GL
//!   current), idle inhibitor. NOTE: eframe calls `logic` only when the compositor sends frame
//!   callbacks — on Hyprland NOT for windows on hidden workspaces unless the window has the
//!   `render_unfocused` rule (`packaging/hypr/tuner.lua`). Nothing that must keep running
//!   while hidden lives here (mpv events, reconnects, MPRIS, reminders run on other threads;
//!   the player suspends video decoding when no frames are rendered).
//! * `ui()`: dispatch the resolved actions (overlay → screen → globals; `Ignored` overlay
//!   actions fall back to globals) → status bar / sidebar (or the Compact top glyph strip) /
//!   content → mini-player → overlays → toasts → apply queued navigation and config edits.
//! * `on_exit()`: remember the last screen, destroy the video surface (GL), then shut services
//!   down (mpv last).
//!
//! Esc ladder (DESIGN §15.1): the topmost overlay, then the screen get `Back` first (clear
//! filter, close side panel, leave OSD controls, hide banner); what nobody handled reaches
//! [`TunerApp::back`]: close a stray overlay → leave window fullscreen (player) → pop the
//! route stack (leaving the player keeps the mini-player playing) → a parent for a lone
//! sub-page (details → grid, Logs/EPG mapping → Settings, player → Live).
//!
//! Blocking work never runs here: log export goes to the blocking pool, the theme / fonts are
//! (re)loaded on workers, the only I/O is indexed SQLite reads on the UI reader and the
//! sub-millisecond `Config::save` of a Settings edit (DESIGN §3.1, `config.rs` contract).
//!
//! Only ONE native viewport is allowed (eframe makes the GL context current before `logic`
//! only for a single viewport; `VideoHost::render` relies on it).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Receiver;
use eframe::egui;
use eframe::glow;

use crate::config::Config;
use crate::fonts::LoadedFonts;
use crate::images::ImageCache;
use crate::ipc::IpcRequest;
use crate::jobs::{JobKind, JobOutcome};
use crate::keymap::{self, Action, KeyContext};
use crate::model::{ChannelId, Collection, ItemKey, ItemKind, ItemRef, MediaRef, PlayOrigin, SourceId, Timestamp, ZapList};
use crate::msg::{AppEvent, DataScope, Toast, ToastKind};
use crate::platform::{PickPurpose, PlatformEvent};
use crate::player::{PlayRequest, PlayerCommand, PlayerSnapshot, PlayerState};
use crate::search::SearchIndex;
use crate::services::Services;
use crate::store::library::{FavIndex, Library};
use crate::theme::{SpaceToken, Theme, ThemeEvent};
use crate::ui::data::{FrameBudget, NowNextCache};
use crate::ui::overlays::{self, Overlay, OverlayOutcome};
use crate::ui::video_host::{Placement, VideoHost};
use crate::ui::widgets::status_bar::{StatusClick, StatusInfo};
use crate::ui::widgets::toast::Toasts;
use crate::ui::{LayoutClass, NavRequest, OnboardingStart, OverlayRequest, ParentalSession, Route, Screen, ScreenId, UiCtx, UiParts};

/// An open overlay, the screen that opened it (receives its result) and which request built
/// it (Help / Jobs toggle closed when requested again).
struct OpenOverlay {
    overlay: Box<dyn Overlay>,
    opener: ScreenId,
    kind: std::mem::Discriminant<OverlayRequest>,
}

pub struct TunerApp {
    // DROP ORDER MATTERS: `video` (render context) is declared before `services` (mpv) so an
    // unwind that skips `on_exit` drops the surface first; `MpvGl` then leaks instead of
    // freeing without GL, and `Player` leaks mpv while the surface is alive (never UB).
    video: VideoHost,
    services: Option<Services>,
    events: Receiver<AppEvent>,
    theme: Arc<Theme>,
    db: rusqlite::Connection,
    lib: Arc<Library>,
    favs: FavIndex,
    budget: FrameBudget,
    now_next: NowNextCache,
    screens: Vec<Box<dyn Screen>>,
    routes: Vec<Route>,
    overlays: Vec<OpenOverlay>,
    toasts: Toasts,
    images: ImageCache,
    parental: ParentalSession,
    search_index: Option<SearchIndex>,
    snapshot: Arc<PlayerSnapshot>,
    nav: Vec<NavRequest>,
    pending_toasts: Vec<Toast>,
    config_edits: Vec<crate::ui::ConfigEdit>,
    /// Actions resolved in `raw_input_hook`, dispatched at the start of `ui`, with the context
    /// stack they were resolved against.
    pending_actions: Vec<(Action, Vec<KeyContext>)>,
    /// The last input event was an Alt chord that resolved: drop its `Text` twin if it comes
    /// next (possibly in the next batch of input).
    drop_alt_text: bool,
    idle: Option<crate::platform::idle::IdleInhibitor>,
    window_fullscreen: bool,
    /// Route whose `on_enter` ran last (a new route — even of the same screen — re-enters).
    entered: Option<Route>,
    layout: LayoutClass,
    /// Last successful EPG import of any enabled guide (status-bar freshness).
    epg_updated: Option<Timestamp>,
    /// A refresh failed with a network error; the next successful refresh clears it.
    offline: bool,
    #[cfg(test)]
    log: std::cell::RefCell<tests::Log>,
}

impl TunerApp {
    /// Called inside eframe's creation callback (GL context current). `fonts` were resolved
    /// and read on a helper thread (DESIGN §5.1).
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mut services: Services,
        events: Receiver<AppEvent>,
        theme: Arc<Theme>,
        fonts: LoadedFonts,
        initial: IpcRequest,
    ) -> Self {
        fonts.install(&cc.egui_ctx);
        crate::theme::egui_style::apply(&cc.egui_ctx, &theme);
        let surface = match services.player.render_core().map(|core| crate::player::VideoSurface::new(core, cc)) {
            Some(Ok(s)) => Some(s),
            Some(Err(e)) => {
                tracing::error!("video surface: {e}");
                None
            }
            None => None,
        };
        let db = services.store.reader().expect("open UI database reader");
        let images = ImageCache::new(&services.paths, services.http.clone(), services.rt.handle().clone(), cc.egui_ctx.clone(), 192 << 20);
        services.start_integrations();
        let idle = {
            use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
            match (cc.display_handle(), cc.window_handle()) {
                (Ok(d), Ok(w)) => crate::platform::idle::IdleInhibitor::new(d.as_raw(), w.as_raw())
                    .map_err(|e| tracing::info!("idle inhibitor: {e}"))
                    .ok(),
                _ => None,
            }
        };
        let start = start_route(&services.config(), &initial, &db);
        let snapshot = services.player.handle().snapshot();
        let mut app = Self {
            video: VideoHost::new(surface),
            services: Some(services),
            events,
            theme,
            db,
            lib: Arc::new(Library::default()),
            favs: FavIndex::default(),
            budget: FrameBudget::default(),
            now_next: NowNextCache::default(),
            screens: crate::ui::screens::all(),
            routes: vec![start],
            overlays: Vec::new(),
            toasts: Toasts::default(),
            images,
            parental: ParentalSession::default(),
            search_index: None,
            snapshot,
            nav: Vec::new(),
            pending_toasts: Vec::new(),
            config_edits: Vec::new(),
            pending_actions: Vec::new(),
            drop_alt_text: false,
            idle,
            window_fullscreen: false,
            entered: None,
            layout: LayoutClass::Wide,
            epg_updated: None,
            offline: false,
            #[cfg(test)]
            log: Default::default(),
        };
        app.refresh_epg_updated();
        app.submit_job(JobKind::RebuildLibrary);
        app.handle_ipc(&initial);
        app
    }

    fn services(&self) -> &Services { self.services.as_ref().expect("services alive") }

    fn current_route(&self) -> &Route { self.routes.last().expect("route stack never empty") }

    fn screen_index(&self, id: ScreenId) -> usize {
        self.screens.iter().position(|s| s.id() == id).expect("every ScreenId has a screen")
    }

    /// The key context stack right now (topmost overlay, else the current screen); `Global`
    /// is appended by `keymap::resolve`.
    fn key_stack(&self) -> Vec<KeyContext> {
        match self.overlays.last() {
            Some(o) => o.overlay.key_contexts(),
            None => self.screens[self.screen_index(self.current_route().screen())].key_contexts(),
        }
    }

    /// Every player command of the app goes through here (tests record them).
    fn send_player(&self, cmd: PlayerCommand) {
        #[cfg(test)]
        self.log.borrow_mut().commands.push(cmd.clone());
        self.services().player.handle().send(cmd);
    }

    /// Every job the app submits goes through here (tests record them).
    fn submit_job(&self, kind: JobKind) {
        #[cfg(test)]
        self.log.borrow_mut().jobs.push(kind.clone());
        self.services().jobs.submit(kind);
    }

    fn toast(&mut self, t: Toast) {
        #[cfg(test)]
        self.log.borrow_mut().toasts.push(t.clone());
        self.toasts.push(t);
    }

    /// Run `f` with a fresh `UiCtx` and a screen/overlay borrowed alongside it.
    fn with_ctx<R>(&mut self, f: impl FnOnce(&mut UiCtx<'_>, &mut Vec<Box<dyn Screen>>, &mut Vec<OpenOverlay>) -> R) -> R {
        let services = self.services.as_ref().expect("services alive");
        let cfg = services.config();
        let mini_rect = self.video.mini_rect();
        let mut ctx = UiCtx::new(UiParts {
            theme: &self.theme,
            services,
            lib: &self.lib,
            favs: &mut self.favs,
            db: &self.db,
            budget: &mut self.budget,
            now_next: &mut self.now_next,
            config: &cfg,
            player: &self.snapshot,
            images: &mut self.images,
            video: &mut self.video,
            parental: &mut self.parental,
            search_index: &mut self.search_index,
            layout: self.layout,
            mini_rect,
            now: crate::model::now(),
            nav: &mut self.nav,
            toasts: &mut self.pending_toasts,
            config_edits: &mut self.config_edits,
        });
        f(&mut ctx, &mut self.screens, &mut self.overlays)
    }

    fn handle_event(&mut self, ev: AppEvent, egui_ctx: &egui::Context) {
        match &ev {
            AppEvent::Theme(ThemeEvent::Changed(t)) => {
                self.theme = t.clone();
                crate::theme::egui_style::apply(egui_ctx, &self.theme);
            }
            AppEvent::Theme(ThemeEvent::FontsChanged(f)) => f.install(egui_ctx),
            AppEvent::Job(crate::jobs::JobEvent::Finished { kind, outcome, .. }) => self.on_job_finished(kind, outcome),
            AppEvent::Job(crate::jobs::JobEvent::Updated(_)) => {} // the status bar polls `jobs.active()`
            AppEvent::Data(scope) => self.on_data_changed(*scope),
            AppEvent::Toast(t) => self.toast(t.clone()),
            // The IPC server already raised the window (`hypr::focus_self`).
            AppEvent::Ipc(req) => self.handle_ipc(req),
            AppEvent::Platform(p) => match p {
                PlatformEvent::Quit => egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                PlatformEvent::Raise => crate::platform::hypr::focus_self(),
                // The scheduler raised the window when the user picked "Watch".
                PlatformEvent::ReminderWatch { channel, .. } => match self.channel_for(channel) {
                    Some(id) => self.play_channel(id, PlayOrigin::Reminder),
                    None => self.toast(Toast::info("That channel is no longer in your playlists")),
                },
                PlatformEvent::FilePicked { purpose: PickPurpose::ExportLog, path: Some(path) } => self.export_log(path.clone()),
                // Other pickers belong to the screen that opened them (`on_event` below).
                PlatformEvent::FilePicked { .. } | PlatformEvent::ReminderFired(_) => {}
            },
            AppEvent::Player(_) => {}
        }
        self.with_ctx(|ctx, screens, _| {
            for s in screens.iter_mut() {
                s.on_event(ctx, &ev);
            }
        });
    }

    fn on_job_finished(&mut self, kind: &JobKind, outcome: &Result<JobOutcome, String>) {
        match outcome {
            Ok(JobOutcome::Library(lib)) => {
                self.lib = lib.clone();
                self.favs = lib.favs.clone();
                self.search_index = None;
                self.refresh_epg_updated();
            }
            Ok(JobOutcome::SourceRefreshed { source, account, .. }) => {
                self.offline = false;
                if let Some(a) = account {
                    self.send_player(PlayerCommand::SetConnectionLimit { source: *source, max: a.max_connections });
                }
            }
            Ok(JobOutcome::EpgRefreshed { .. }) => self.offline = false,
            Ok(_) => {}
            Err(msg) => {
                // Probe/add/backup failures are shown by the screen that started them; library
                // builds, details and maintenance only log (WP-06 logs every failure).
                let what = match kind {
                    JobKind::RefreshSource { source, .. } => self.source_name(*source),
                    JobKind::RefreshEpg { .. } => "the guide".to_owned(),
                    JobKind::RefreshStale => "your playlists".to_owned(),
                    _ => return,
                };
                if msg == &crate::error::Error::Cancelled.to_string() {
                    return;
                }
                // `Error::Network` displays as "network error: …" (frozen error.rs).
                self.offline = msg.contains("network error");
                self.toast(Toast::error(format!("Couldn't refresh {what}: {}", crate::redact::redact(msg))));
            }
        }
    }

    fn source_name(&self, id: SourceId) -> String {
        match self.lib.source(id) {
            Some(s) => s.name.clone(),
            None => crate::store::sources::get_source(&self.db, id).map(|s| s.name).unwrap_or_else(|_| "a playlist".to_owned()),
        }
    }

    /// DataScope → reaction (DESIGN §5.4).
    fn on_data_changed(&mut self, scope: DataScope) {
        match scope {
            DataScope::Favourites => match crate::store::library::load_favs(&self.db) {
                Ok(f) => self.favs = f,
                Err(e) => tracing::warn!("favourites: {e}"),
            },
            DataScope::Epg => {
                self.now_next.invalidate();
                self.refresh_epg_updated();
                self.submit_job(JobKind::RebuildLibrary);
            }
            DataScope::Reminders => {
                if let Some(r) = &self.services().integrations.reminders {
                    r.refresh();
                }
            }
            DataScope::Sources | DataScope::All => {
                self.refresh_epg_updated();
                self.submit_job(JobKind::RebuildLibrary);
            }
            DataScope::Live | DataScope::Vod | DataScope::Overrides | DataScope::History => {
                self.submit_job(JobKind::RebuildLibrary);
            }
            DataScope::VodDetails | DataScope::Progress => {}
        }
    }

    /// Newest `last_refresh` of the enabled EPG sources (one aggregate over a tiny table).
    fn refresh_epg_updated(&mut self) {
        self.epg_updated = self
            .db
            .query_row("SELECT max(last_refresh) FROM epg_source WHERE enabled = 1", [], |r| r.get::<_, Option<Timestamp>>(0))
            .unwrap_or_else(|e| {
                tracing::debug!("guide freshness: {e}");
                None
            });
    }

    /// CLI / IPC requests (DESIGN §21.4): view switch, `--play <source-id>:<item-key>` or
    /// `--play <url|file>`, positional files/URLs (`.m3u`/`.m3u8` files open the onboarding
    /// card at Details; other files/URLs play fullscreen). `main` made file paths absolute in
    /// the invoking process (its working directory may differ from ours).
    fn handle_ipc(&mut self, req: &IpcRequest) {
        match req.view {
            Some(crate::cli::ViewArg::Search) => self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() })),
            Some(v) => self.nav.push(NavRequest::Switch(route_for_view(v))),
            None => {}
        }
        let mut play: Option<&str> = req.play.as_deref();
        for input in &req.inputs {
            if is_playlist_file(input) {
                self.nav.push(NavRequest::Switch(Route::Onboarding(OnboardingStart::PlaylistFile(PathBuf::from(input)))));
            } else if play.is_none() {
                play = Some(input);
            }
        }
        let Some(target) = play else { return };
        if let Some(item) = parse_channel_ref(target) {
            match self.channel_for(&item) {
                Some(id) => self.play_channel(id, PlayOrigin::Cli),
                None => self.toast(Toast::error(format!("No channel {target} in your playlists"))),
            }
        } else {
            self.send_player(PlayerCommand::Play(PlayRequest {
                media: MediaRef::Url(target.to_owned()),
                zap: None,
                origin: PlayOrigin::Cli,
                resume: true,
                immediate: true,
            }));
            self.nav.push(NavRequest::Push(Route::Player));
        }
    }

    /// A channel by stable reference: the library snapshot, else one indexed DB lookup (the
    /// library may still be loading at startup).
    fn channel_for(&self, item: &ItemRef) -> Option<ChannelId> {
        self.lib.live.by_ref(item).or_else(|| match crate::store::live::channel_by_ref(&self.db, item) {
            Ok(c) => c.map(|c| c.id),
            Err(e) => {
                tracing::warn!("channel lookup: {e}");
                None
            }
        })
    }

    /// Play a channel fullscreen, zapping through its primary group (DESIGN §13.0). Locked
    /// channels need the parental PIN first (the app never bypasses it).
    fn play_channel(&mut self, id: ChannelId, origin: PlayOrigin) {
        let cfg = self.services().config();
        let (locked, group) = self.lib.live.get(id).map_or((false, None), |c| (c.locked, c.group_ids.first().copied()));
        if locked && !self.parental.is_unlocked(&cfg.parental) {
            self.toast(Toast::info("This channel is locked — unlock it with your PIN first"));
            return;
        }
        let zap = group.map(|g| {
            let collection = Collection::Group(g);
            let channels = self.lib.live.collection(&collection, &self.favs, &self.lib.recent_channels, cfg.interface.live_sort);
            ZapList { collection, channels: channels.into() }
        });
        self.send_player(PlayerCommand::Play(PlayRequest { media: MediaRef::Channel(id), zap, origin, resume: false, immediate: true }));
        self.nav.push(NavRequest::Push(Route::Player));
    }

    /// Logs ▸ Export…: `LogBuffer::export` is blocking file I/O → blocking pool; the outcome
    /// comes back as a toast.
    fn export_log(&self, path: PathBuf) {
        let services = self.services();
        let (logs, bus) = (services.logs.clone(), services.bus.clone());
        services.rt.spawn_blocking(move || {
            let toast = match logs.export(&path) {
                Ok(()) => Toast { kind: ToastKind::Success, text: format!("Log saved to {}", path.display()), action: None, ttl_ms: 4000 },
                Err(e) => Toast::error(format!("Couldn't save the log: {e}")),
            };
            bus.toast(toast);
        });
    }

    fn global_action(&mut self, action: Action, stack: &[KeyContext], egui_ctx: &egui::Context) {
        match action {
            Action::OpenSearch => self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() })),
            Action::ToggleHelp => self.open_help(stack.to_vec()),
            Action::ShowJobs => self.nav.push(NavRequest::Open(OverlayRequest::Jobs)),
            Action::GoHome => self.nav.push(NavRequest::Switch(Route::Home)),
            Action::GoLive => self.nav.push(NavRequest::Switch(Route::Live { group: None, channel: None })),
            Action::GoGuide => self.nav.push(NavRequest::Switch(Route::Guide { collection: None, focus: None })),
            Action::GoMovies | Action::GoSeries if self.lib.built_at != 0 && !self.lib.has_vod() && !self.lib.has_series() => {
                self.toast(Toast::info("Your playlists have no movies or series"));
            }
            Action::GoMovies => self.nav.push(NavRequest::Switch(Route::Movies { group: None })),
            Action::GoSeries => self.nav.push(NavRequest::Switch(Route::Series { group: None })),
            Action::GoSettings => self.nav.push(NavRequest::Switch(Route::Settings(Default::default()))),
            Action::Quit => egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::ToggleWindowFullscreen => self.set_window_fullscreen(!self.window_fullscreen, egui_ctx),
            Action::OpenPlayer if self.snapshot.state != PlayerState::Idle => self.nav.push(NavRequest::Push(Route::Player)),
            Action::GlobalPlayPause => self.send_player(PlayerCommand::TogglePause),
            Action::StopPlayback => self.send_player(PlayerCommand::Stop),
            Action::RefreshSource => self.submit_job(JobKind::RefreshStale),
            Action::Back => self.back(egui_ctx),
            _ => {}
        }
    }

    fn set_window_fullscreen(&mut self, on: bool, egui_ctx: &egui::Context) {
        self.window_fullscreen = on;
        egui_ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// The app's rungs of the Esc ladder (see module docs).
    fn back(&mut self, egui_ctx: &egui::Context) {
        if !self.overlays.is_empty() {
            // an overlay that doesn't handle Back itself still closes on Esc
            self.overlays.pop();
        } else if self.window_fullscreen && self.current_route().screen() == ScreenId::Player {
            self.set_window_fullscreen(false, egui_ctx);
        } else {
            self.nav.push(NavRequest::Back);
        }
    }

    /// `?` / F1: the help overlay with the contexts the user is in first (DESIGN §13.11);
    /// pressed again (or with Help on top) it closes.
    fn open_help(&mut self, contexts: Vec<KeyContext>) {
        let kind = std::mem::discriminant(&OverlayRequest::Help);
        if self.overlays.last().is_some_and(|o| o.kind == kind) {
            self.overlays.pop();
            return;
        }
        let opener = self.current_route().screen();
        self.overlays.push(OpenOverlay { overlay: Box::new(overlays::help::HelpOverlay::with_contexts(contexts)), opener, kind });
    }

    fn open_overlay(&mut self, req: OverlayRequest) {
        match req {
            OverlayRequest::Help => self.open_help(self.key_stack()),
            OverlayRequest::Jobs if self.overlays.last().is_some_and(|o| o.kind == std::mem::discriminant(&req)) => {
                self.overlays.pop();
            }
            req => {
                let kind = std::mem::discriminant(&req);
                let opener = self.current_route().screen();
                let overlay = self.with_ctx(|ctx, _, _| overlays::build(req, ctx));
                self.overlays.push(OpenOverlay { overlay, opener, kind });
            }
        }
    }

    /// Dispatch the actions resolved in `raw_input_hook`: topmost overlay → screen → globals.
    fn dispatch_actions(&mut self, egui_ctx: &egui::Context) {
        for (action, stack) in std::mem::take(&mut self.pending_actions) {
            let handled = if let Some(top) = self.overlays.len().checked_sub(1) {
                let outcome = self.with_ctx(|ctx, _, ovs| ovs[top].overlay.on_action(ctx, action));
                let ignored = matches!(outcome, OverlayOutcome::Ignored);
                self.apply_overlay_outcome(outcome);
                !ignored
            } else {
                let idx = self.screen_index(self.current_route().screen());
                self.with_ctx(|ctx, screens, _| screens[idx].on_action(ctx, action))
            };
            if !handled {
                self.global_action(action, &stack, egui_ctx);
            }
        }
    }

    fn apply_overlay_outcome(&mut self, outcome: OverlayOutcome) {
        match outcome {
            OverlayOutcome::Close => {
                self.overlays.pop();
            }
            OverlayOutcome::CloseWith(result) => {
                if let Some(o) = self.overlays.pop() {
                    let idx = self.screen_index(o.opener);
                    self.with_ctx(|ctx, screens, _| screens[idx].on_overlay_result(ctx, result));
                }
            }
            OverlayOutcome::Keep | OverlayOutcome::Ignored => {}
        }
    }

    fn apply_nav(&mut self) {
        let reqs: Vec<NavRequest> = std::mem::take(&mut self.nav);
        for r in reqs {
            match r {
                NavRequest::Push(route) => {
                    if self.current_route() != &route {
                        self.routes.push(route);
                    }
                }
                NavRequest::Switch(route) => {
                    // a tab switch leaves whatever the old screen had open
                    self.overlays.clear();
                    self.routes.clear();
                    self.routes.push(route);
                }
                NavRequest::Back => {
                    if self.routes.len() > 1 {
                        self.routes.pop();
                    } else if let Some(parent) = parent_route(self.current_route()) {
                        self.routes[0] = parent;
                    }
                }
                NavRequest::Open(req) => self.open_overlay(req),
                NavRequest::CloseOverlay => {
                    self.overlays.pop();
                }
            }
        }
        if self.entered.as_ref() != Some(self.current_route()) {
            let route = self.current_route().clone();
            self.entered = Some(route.clone());
            let idx = self.screen_index(route.screen());
            self.with_ctx(|ctx, screens, _| screens[idx].on_enter(ctx, &route));
        }
    }

    /// Apply queued Settings edits: save, swap the shared config, re-apply to the player, the
    /// integrations and the theme scale (no restart needed for any v1 setting).
    fn apply_config_edits(&mut self, egui_ctx: &egui::Context) {
        if self.config_edits.is_empty() {
            return;
        }
        let edits = std::mem::take(&mut self.config_edits);
        let services = self.services.as_mut().expect("services alive");
        let old = services.config();
        let mut cfg: Config = (*old).clone();
        for e in edits {
            e(&mut cfg);
        }
        if cfg == *old {
            return;
        }
        let saved = cfg.save(&services.paths.config_file());
        let cfg = Arc::new(cfg);
        *services.config.write().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
        services.apply_config(&old, &cfg);
        if let Err(e) = saved {
            self.toast(Toast::error(format!("Couldn't save settings: {e}")));
        }
        self.send_player(PlayerCommand::ApplyConfig(Box::new((*cfg).clone())));
        let follow = cfg.integration.follow_omarchy_theme;
        if follow != old.integration.follow_omarchy_theme && !follow {
            // the watcher is gone; the built-in palette applies right away (turning it back on
            // reloads the Omarchy theme on a worker → `ThemeEvent::Changed`)
            self.theme = Arc::new(scaled(Theme::fallback(), cfg.interface.ui_scale));
            crate::theme::egui_style::apply(egui_ctx, &self.theme);
        } else if cfg.interface.ui_scale != old.interface.ui_scale {
            self.theme = Arc::new(scaled((*self.theme).clone(), cfg.interface.ui_scale));
            crate::theme::egui_style::apply(egui_ctx, &self.theme);
        }
    }

    fn chrome_hidden(&self) -> bool { matches!(self.current_route().screen(), ScreenId::Player | ScreenId::Onboarding) }

    /// `StartScreen::Last`: store the main screen the user leaves on (written at exit only).
    fn remember_last_screen(&self) {
        let Some(services) = &self.services else { return };
        let cfg = services.config();
        if cfg.general.start_screen != crate::config::StartScreen::Last {
            return;
        }
        let last = self.routes.iter().rev().find_map(|r| view_name(r.screen())).map(str::to_owned);
        if last.is_none() || last == cfg.interface.last_screen {
            return;
        }
        let mut cfg = (*cfg).clone();
        cfg.interface.last_screen = last;
        if let Err(e) = cfg.save(&services.paths.config_file()) {
            tracing::warn!("remembering the last screen: {e}");
        }
    }

    /// Sidebar (vertical) or, below 480·rem in Compact, the top glyph strip (horizontal).
    fn sidebar_panel(&mut self, ui: &mut egui::Ui, theme: &Theme, current: ScreenId, side_w: f32) {
        let mut search_clicked = false;
        let has_vod = self.lib.has_vod() || self.lib.has_series() || self.lib.built_at == 0;
        let frame = egui::Frame::NONE.fill(theme.palette.dark_background);
        let clicked = if side_w > 0.0 {
            egui::Panel::left("sidebar")
                .exact_size(side_w)
                .resizable(false)
                .show_separator_line(false)
                .frame(frame)
                .show(ui, |ui| crate::ui::widgets::sidebar::sidebar(ui, theme, current, side_w > 100.0, has_vod, &mut search_clicked))
                .inner
        } else {
            let strip = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Md);
            egui::Panel::top("glyph-strip")
                .exact_size(strip)
                .resizable(false)
                .show_separator_line(false)
                .frame(frame.inner_margin(egui::Margin::symmetric(theme.space(SpaceToken::Md) as i8, 0)))
                .show(ui, |ui| ui.horizontal(|ui| crate::ui::widgets::sidebar::sidebar(ui, theme, current, false, has_vod, &mut search_clicked)).inner)
                .inner
        };
        if let Some(id) = clicked {
            self.nav.push(NavRequest::Switch(route_for_screen(id)));
        }
        if search_clicked {
            self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() }));
        }
    }
}

impl eframe::App for TunerApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let stack = self.key_stack();
        let mut resolved: Vec<(Action, Vec<KeyContext>)> = Vec::new();
        let mut drop_text = self.drop_alt_text;
        raw.events.retain(|ev| {
            // the Text twin of a resolved Alt chord (only if it comes right after it)
            if std::mem::take(&mut drop_text) && matches!(ev, egui::Event::Text(_)) {
                return false;
            }
            // mouse back button = Back (DESIGN §15.1)
            if let egui::Event::PointerButton { button: egui::PointerButton::Extra1, pressed, .. } = ev {
                if *pressed {
                    resolved.push((Action::Back, stack.clone()));
                }
                return false;
            }
            match keymap::resolve(&stack, ev) {
                Some(a) => {
                    drop_text = matches!(ev, egui::Event::Key { modifiers, .. } if modifiers.alt);
                    resolved.push((a, stack.clone()));
                    false // consumed: egui never sees it
                }
                None => true,
            }
        });
        self.drop_alt_text = drop_text;
        self.pending_actions.extend(resolved);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        while let Ok(ev) = self.events.try_recv() {
            self.handle_event(ev, ctx);
        }
        self.snapshot = self.services().player.handle().snapshot();
        let busy = matches!(self.snapshot.state, PlayerState::Loading { .. } | PlayerState::Zapping { .. });
        self.services().jobs.context().player_busy.store(busy, Ordering::Relaxed);
        if self.snapshot.video_suspended {
            self.send_player(PlayerCommand::ResumeVideo);
        }
        if let Some(gl) = frame.gl().cloned() {
            let snap = self.snapshot.clone();
            self.video.render(&gl, frame, &snap);
        }
        let mode = self.services().config().integration.idle_inhibit;
        let active = self.snapshot.state.is_active() && !self.snapshot.paused;
        let fs = self.window_fullscreen;
        if let Some(i) = &mut self.idle {
            i.set(mode, active, fs);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let egui_ctx = ui.ctx().clone();
        let width = egui_ctx.content_rect().width();
        self.layout = LayoutClass::from_width(width, self.theme.rem());
        self.budget.begin_frame();
        self.now_next.begin_frame();
        self.video.begin_frame();
        self.images.pump();
        self.dispatch_actions(&egui_ctx);
        self.apply_nav();

        let theme = self.theme.clone();
        let current = self.current_route().screen();
        let chrome = !self.chrome_hidden();
        let cfg = self.services().config();
        let side_w = crate::ui::layout::sidebar_width(width, theme.rem(), cfg.interface.sidebar);

        let mut status_top = egui_ctx.content_rect().bottom();
        if chrome {
            let jobs = self.services().jobs.active();
            let hint = keymap::hint_line(&self.key_stack());
            let mini_hidden = self.layout == LayoutClass::Compact || !cfg.interface.mini_player;
            let now_playing = (mini_hidden && self.snapshot.state != PlayerState::Idle)
                .then(|| self.snapshot.playable.as_ref().map(|p| p.title.clone()))
                .flatten();
            let info = StatusInfo {
                jobs: &jobs,
                epg_updated: self.epg_updated,
                offline: self.offline,
                now: crate::model::now(),
                hint: &hint,
                now_playing: now_playing.as_deref(),
            };
            let bar = egui::Panel::bottom("status-bar")
                .exact_size((26.0 * theme.rem()).round())
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.fill(theme.palette.dark_background).inner_margin(egui::Margin::symmetric(10, 0)))
                .show(ui, |ui| crate::ui::widgets::status_bar::status_bar(ui, &theme, &info));
            status_top = bar.response.rect.top();
            match bar.inner {
                StatusClick::Jobs => self.nav.push(NavRequest::Open(OverlayRequest::Jobs)),
                StatusClick::OpenPlayer => self.nav.push(NavRequest::Push(Route::Player)),
                StatusClick::StopPlayback => self.send_player(PlayerCommand::Stop),
                StatusClick::None => {}
            }
            self.sidebar_panel(ui, &theme, current, side_w);
        }

        let idx = self.screen_index(current);
        let fullscreen_player = current == ScreenId::Player;
        let pad = if fullscreen_player { 0.0 } else { theme.space(SpaceToken::PanelPadding) };
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(if fullscreen_player { egui::Color32::BLACK } else { theme.tokens.bg }).inner_margin(pad))
            .show(ui, |ui| {
                self.with_ctx(|ctx, screens, _| screens[idx].ui(ui, ctx));
            });

        // corner mini-player: something plays, no screen placed the video, the screen allows it
        let playing = !matches!(self.snapshot.state, PlayerState::Idle);
        if playing
            && cfg.interface.mini_player
            && self.layout != LayoutClass::Compact
            && self.screens[idx].allows_mini_player()
            && self.video.placement() == Placement::None
        {
            let snap = self.snapshot.clone();
            match self.video.mini_player(&egui_ctx, &theme, &snap) {
                Some(true) => self.nav.push(NavRequest::Push(Route::Player)),
                Some(false) => self.send_player(PlayerCommand::Stop),
                None => {}
            }
        }

        if let Some(i) = self.overlays.len().checked_sub(1) {
            let outcome = self.with_ctx(|ctx, _, ovs| ovs[i].overlay.ui(&egui_ctx, ctx));
            self.apply_overlay_outcome(outcome);
        }
        for t in std::mem::take(&mut self.pending_toasts) {
            self.toast(t);
        }
        if let Some(action) = self.toasts.show(&egui_ctx, &theme, self.video.mini_rect(), status_top) {
            let stack = self.key_stack();
            self.pending_actions.push((action, stack));
        }
        self.apply_nav();
        self.apply_config_edits(&egui_ctx);
        if self.budget.deferred() || self.now_next.pending() || !self.pending_actions.is_empty() {
            egui_ctx.request_repaint();
        }
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        self.remember_last_screen();
        // SHUTDOWN ORDER: render context (needs GL) → FBO → services (player/mpv, then writer).
        self.video.destroy(gl);
        if let Some(s) = self.services.take() {
            s.shutdown();
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = self.theme.tokens.bg;
        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
    }
}

/// The same theme at another `interface.ui_scale`.
fn scaled(t: Theme, ui_scale: f32) -> Theme { Theme::from_parts(t.name, t.palette, t.shell, t.tokens.rounding, ui_scale) }

fn route_for_screen(id: ScreenId) -> Route {
    match id {
        ScreenId::Home => Route::Home,
        ScreenId::Live => Route::Live { group: None, channel: None },
        ScreenId::Guide => Route::Guide { collection: None, focus: None },
        ScreenId::Movies => Route::Movies { group: None },
        ScreenId::Series => Route::Series { group: None },
        ScreenId::Settings => Route::Settings(Default::default()),
        ScreenId::Player => Route::Player,
        ScreenId::Onboarding => Route::Onboarding(OnboardingStart::Welcome),
        ScreenId::Logs => Route::Logs,
        ScreenId::EpgMapping => Route::EpgMapping { channel: None },
        ScreenId::MovieDetail | ScreenId::SeriesDetail => Route::Home,
    }
}

/// Where Back goes from a sub-page that is alone on the route stack (opened by CLI/IPC, a
/// reminder, or a tab switch replaced what was below it). Tabs have no parent.
fn parent_route(route: &Route) -> Option<Route> {
    match route.screen() {
        ScreenId::Player => Some(Route::Live { group: None, channel: None }),
        ScreenId::MovieDetail => Some(Route::Movies { group: None }),
        ScreenId::SeriesDetail => Some(Route::Series { group: None }),
        ScreenId::Logs | ScreenId::EpgMapping => Some(Route::Settings(Default::default())),
        _ => None,
    }
}

fn route_for_view(v: crate::cli::ViewArg) -> Route {
    use crate::cli::ViewArg as V;
    match v {
        V::Home => Route::Home,
        V::Live => Route::Live { group: None, channel: None },
        V::Guide => Route::Guide { collection: None, focus: None },
        V::Movies => Route::Movies { group: None },
        V::Series => Route::Series { group: None },
        V::Search => Route::Home, // + the search overlay (see `handle_ipc`)
        V::Settings => Route::Settings(Default::default()),
    }
}

/// `interface.last_screen` value of a main screen (the `--view` names); sub-pages count as
/// their tab, the player and onboarding as nothing.
fn view_name(screen: ScreenId) -> Option<&'static str> {
    Some(match screen {
        ScreenId::Home => "home",
        ScreenId::Live => "live",
        ScreenId::Guide => "guide",
        ScreenId::Movies | ScreenId::MovieDetail => "movies",
        ScreenId::Series | ScreenId::SeriesDetail => "series",
        ScreenId::Settings | ScreenId::Logs | ScreenId::EpgMapping => "settings",
        ScreenId::Player | ScreenId::Onboarding => return None,
    })
}

fn route_for_view_name(name: &str) -> Option<Route> {
    use clap::ValueEnum;
    crate::cli::ViewArg::from_str(name, true).ok().filter(|v| *v != crate::cli::ViewArg::Search).map(route_for_view)
}

/// `.m3u` / `.m3u8` local file (a playlist to add, not a stream to play).
fn is_playlist_file(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    !lower.contains("://") && (lower.ends_with(".m3u") || lower.ends_with(".m3u8"))
}

/// `--play <source-id>:<item-key>` for a channel (`2:xt:live:1234`, `1:m3u:0123abcd…`);
/// anything else is a URL or a file.
fn parse_channel_ref(s: &str) -> Option<ItemRef> {
    let (source, key) = s.split_once(':')?;
    let source = source.parse::<i64>().ok()?;
    let is_channel_key = key.strip_prefix("xt:live:").is_some_and(|id| !id.is_empty()) || key.strip_prefix("m3u:").is_some_and(|h| !h.is_empty());
    is_channel_key.then(|| ItemRef { source: SourceId(source), kind: ItemKind::Channel, key: ItemKey(key.to_owned()) })
}

/// First route: CLI view → no sources ⇒ onboarding → configured start screen. `Auto` = Live
/// until the user has favourites or history, then Home. Two tiny indexed queries (< 1 ms).
fn start_route(cfg: &Config, initial: &IpcRequest, db: &rusqlite::Connection) -> Route {
    if let Some(v) = initial.view {
        return route_for_view(v);
    }
    let exists = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n != 0).unwrap_or(false);
    if !exists("SELECT EXISTS(SELECT 1 FROM source)") {
        return Route::Onboarding(OnboardingStart::Welcome);
    }
    use crate::config::StartScreen as S;
    match cfg.general.start_screen {
        S::Home => Route::Home,
        S::Live => Route::Live { group: None, channel: None },
        S::Guide => Route::Guide { collection: None, focus: None },
        S::Last => cfg.interface.last_screen.as_deref().and_then(route_for_view_name).unwrap_or(Route::Home),
        S::Auto => {
            if exists("SELECT EXISTS(SELECT 1 FROM fav_item) OR EXISTS(SELECT 1 FROM history)") {
                Route::Home
            } else {
                Route::Live { group: None, channel: None }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The real `TunerApp` (real services in a temp profile, headless mpv, no window) driven
    //! frame by frame exactly like eframe does: `raw_input_hook` → `run_ui { logic; ui }`.
    //! Screens that need observing are swapped for a recording [`ProbeScreen`].

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use eframe::App as _;
    use egui::{Event, Key, Modifiers};

    use super::*;
    use crate::jobs::JobEvent;
    use crate::model::JobId;
    use crate::paths::AppPaths;
    use crate::ui::overlays::OverlayResult;

    /// What the app sent out (player commands, jobs, toasts).
    #[derive(Default)]
    pub(super) struct Log {
        pub commands: Vec<PlayerCommand>,
        pub jobs: Vec<JobKind>,
        pub toasts: Vec<Toast>,
    }

    /// One app (mpv + runtime + store) at a time keeps the test binary's memory flat.
    static ONE_APP: Mutex<()> = Mutex::new(());

    struct Rig {
        app: TunerApp,
        ctx: egui::Context,
        frame: eframe::Frame,
        size: egui::Vec2,
        dir: tempfile::TempDir,
        _one: std::sync::MutexGuard<'static, ()>,
    }

    /// Headless config: null video/audio outputs, no session-bus or theme-dir integrations.
    fn test_config() -> Config {
        let mut c = Config::default();
        c.player.mpv_options.insert("vo".into(), "null".into());
        c.player.mpv_options.insert("ao".into(), "null".into());
        c.integration.mpris = false;
        c.integration.follow_omarchy_theme = false;
        c.reminders.enabled = false;
        c
    }

    fn rooted(dir: &std::path::Path) -> AppPaths {
        let mut paths = AppPaths::rooted(dir);
        paths.home = dir.join("home"); // never read the user's Omarchy theme / fonts
        paths
    }

    impl Rig {
        /// Live TV start route, default test config, empty profile.
        fn new() -> Self { Self::with(test_config(), live(), 1.0, |_| Ok(())) }

        fn with(cfg: Config, initial: IpcRequest, ui_scale: f32, seed: impl FnOnce(&mut rusqlite::Connection) -> crate::error::Result<()> + Send + 'static) -> Self {
            let one = ONE_APP.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let paths = rooted(dir.path());
            paths.ensure().unwrap();
            let store = crate::store::Store::open(&paths.db_file(), None).unwrap();
            store.writer().blocking(seed).unwrap();
            assert!(store.close(Duration::from_secs(5)));
            let (bus, events) = crate::msg::EventBus::new(None);
            let services = Services::start(paths, cfg, crate::logging::LogBuffer::new(100), bus).unwrap();
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let cc = eframe::CreationContext::_new_kittest(ctx.clone());
            let theme = Arc::new(scaled(Theme::fallback(), ui_scale));
            let app = TunerApp::new(&cc, services, events, theme, LoadedFonts::default(), initial);
            let mut rig = Self { app, ctx, frame: eframe::Frame::_new_kittest(), size: egui::vec2(1280.0, 800.0), dir, _one: one };
            rig.frame_with(vec![]);
            rig
        }

        /// One eframe frame with these input events.
        fn frame_with(&mut self, events: Vec<Event>) -> egui::FullOutput { self.frame_raw(events, true) }

        fn frame_raw(&mut self, events: Vec<Event>, hook: bool) -> egui::FullOutput {
            let mut raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.size)), events, ..Default::default() };
            let Rig { app, ctx, frame, .. } = self;
            if hook {
                app.raw_input_hook(ctx, &mut raw);
            }
            let mut out = ctx.run_ui(raw, |ui| {
                app.logic(ui.ctx(), frame);
                app.ui(ui, frame);
            });
            out.textures_delta.clear(); // no renderer here
            out
        }

        fn press(&mut self, key: Key, modifiers: Modifiers) -> egui::FullOutput { self.frame_with(key_events(key, modifiers)) }

        /// Run frames until `done` or 10 s (background work finishing).
        fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
                self.frame_with(vec![]);
            }
        }

        fn route(&self) -> &Route { self.app.current_route() }

        fn log(&self) -> std::cell::Ref<'_, Log> { self.app.log.borrow() }

        fn probe(&mut self, id: ScreenId, contexts: Vec<KeyContext>) -> Rc<RefCell<Probe>> {
            let st = Rc::new(RefCell::new(Probe { contexts, ..Default::default() }));
            let idx = self.app.screen_index(id);
            self.app.screens[idx] = Box::new(ProbeScreen { id, st: st.clone() });
            self.app.entered = None; // re-enter the (new) current screen next frame
            st
        }

        fn event(&mut self, ev: AppEvent) {
            let ctx = self.ctx.clone();
            self.app.handle_event(ev, &ctx);
        }

        fn writer(&self) -> crate::store::Writer { self.app.services().store.writer().clone() }

        fn overlay_kinds(&self) -> Vec<std::mem::Discriminant<OverlayRequest>> { self.app.overlays.iter().map(|o| o.kind).collect() }
    }

    impl Drop for Rig {
        fn drop(&mut self) { self.app.on_exit(None); }
    }

    fn live() -> IpcRequest { IpcRequest { view: Some(crate::cli::ViewArg::Live), ..Default::default() } }

    fn kind(req: OverlayRequest) -> std::mem::Discriminant<OverlayRequest> { std::mem::discriminant(&req) }

    fn key_events(key: Key, modifiers: Modifiers) -> Vec<Event> {
        let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
        vec![ev(true), ev(false)]
    }

    const NONE: Modifiers = Modifiers::NONE;
    const ALT: Modifiers = Modifiers { alt: true, ctrl: false, shift: false, mac_cmd: false, command: false };
    const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };

    /// Records what the app delivers to a screen; draws one focusable button.
    #[derive(Default)]
    struct Probe {
        contexts: Vec<KeyContext>,
        actions: Vec<Action>,
        entered: Vec<Route>,
        results: Vec<OverlayResult>,
        events: usize,
        clicks: u32,
        handle_back: bool,
        focus_requested: bool,
    }

    struct ProbeScreen {
        id: ScreenId,
        st: Rc<RefCell<Probe>>,
    }

    impl Screen for ProbeScreen {
        fn id(&self) -> ScreenId { self.id }
        fn key_contexts(&self) -> Vec<KeyContext> { self.st.borrow().contexts.clone() }
        fn on_enter(&mut self, _ctx: &mut UiCtx<'_>, route: &Route) { self.st.borrow_mut().entered.push(route.clone()) }
        fn on_event(&mut self, _ctx: &mut UiCtx<'_>, _ev: &AppEvent) { self.st.borrow_mut().events += 1 }
        fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> bool {
            let mut st = self.st.borrow_mut();
            st.actions.push(action);
            match action {
                Action::Back => st.handle_back,
                Action::Activate | Action::Down | Action::FilterInput(_) => true,
                _ => false,
            }
        }
        fn ui(&mut self, ui: &mut egui::Ui, _ctx: &mut UiCtx<'_>) {
            let r = ui.button("Probe button");
            let mut st = self.st.borrow_mut();
            if !st.focus_requested {
                st.focus_requested = true;
                r.request_focus();
            }
            if r.clicked() {
                st.clicks += 1;
            }
        }
        fn on_overlay_result(&mut self, _ctx: &mut UiCtx<'_>, result: OverlayResult) { self.st.borrow_mut().results.push(result) }
    }

    /// One source, an "All" group and channels `m3u:ch<i>` (numbers 1…n).
    fn seed_channels(c: &mut rusqlite::Connection, n: usize, account: Option<&str>) -> crate::error::Result<()> {
        use rusqlite::params;
        c.execute("INSERT INTO source(name, kind, url, created_at, generation, account) VALUES ('My IPTV', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1, ?1)", [account])?;
        let src = c.last_insert_rowid();
        c.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:all','All',0,?2,1)", params![src, n as i64])?;
        let grp = c.last_insert_rowid();
        for i in 0..n {
            let url = format!("http://127.0.0.1/live/{i}.ts");
            c.execute(
                "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, stream, url_key, provider_order, seen_gen)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
                params![src, format!("m3u:ch{i}"), format!("Channel {i}"), format!("channel{i}"), (i + 1) as i64, format!(r#"{{"t":"url","url":"{url}"}}"#), url, i as i64],
            )?;
            let id = c.last_insert_rowid();
            c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, 1)", params![grp, id, i as i64])?;
        }
        Ok(())
    }

    fn channel_ref(i: usize) -> ItemRef { ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:ch{i}")) } }

    /// Load the real library from the profile and deliver it like the RebuildLibrary job.
    fn deliver_library(rig: &mut Rig) {
        let lib = crate::store::library::load(&rig.app.db).unwrap();
        rig.event(AppEvent::Job(JobEvent::Finished { id: JobId(9), kind: JobKind::RebuildLibrary, outcome: Ok(JobOutcome::Library(Arc::new(lib))) }));
    }

    fn played(log: &Log) -> Vec<(MediaRef, PlayOrigin)> {
        log.commands
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::Play(r) => Some((r.media.clone(), r.origin)),
                _ => None,
            })
            .collect()
    }

    type Node = (String, egui::accesskit::Rect, egui::accesskit::Role);

    /// Text of every accessibility node with its bounds and role (labels carry their text as
    /// the value, buttons as the label).
    fn texts(out: &egui::FullOutput) -> Vec<Node> {
        let update = out.platform_output.accesskit_update.as_ref().expect("accesskit enabled");
        update.nodes.iter().filter_map(|(_, n)| Some((n.label().or(n.value())?.to_owned(), n.bounds()?, n.role()))).collect()
    }

    fn find<'a>(texts: &'a [Node], text: &str) -> Option<&'a egui::accesskit::Rect> {
        texts.iter().find(|(t, _, _)| t == text).map(|(_, r, _)| r)
    }

    /// A button by its label (the sidebar entries; screens may show the same words as text).
    fn button<'a>(texts: &'a [Node], text: &str) -> Option<&'a egui::accesskit::Rect> {
        texts.iter().find(|(t, _, role)| t == text && *role == egui::accesskit::Role::Button).map(|(_, r, _)| r)
    }

    // ---- key consumption -----------------------------------------------------------------

    #[test]
    fn enter_is_consumed_and_never_also_clicks_the_focused_button() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        rig.frame_with(vec![]);
        rig.frame_with(vec![]);
        rig.press(Key::Enter, NONE);
        assert_eq!(probe.borrow().actions, [Action::Activate]);
        assert_eq!(probe.borrow().clicks, 0, "egui must not see the resolved Enter");
        // control: the same key without the hook does click the focused button
        rig.frame_raw(key_events(Key::Enter, NONE), false);
        assert_eq!(probe.borrow().clicks, 1);
    }

    #[test]
    fn unresolved_keys_reach_egui_and_printables_filter_lists() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        rig.frame_with(vec![Event::Text("x".into())]);
        assert_eq!(probe.borrow().actions, [Action::FilterInput('x')]);
        // F24 is bound nowhere: it stays in egui's input
        let mut raw = egui::RawInput { events: key_events(Key::F24, NONE), ..Default::default() };
        rig.app.raw_input_hook(&rig.ctx.clone(), &mut raw);
        assert_eq!(raw.events.len(), 2);
        assert!(rig.app.pending_actions.is_empty());
    }

    #[test]
    fn a_resolved_alt_chord_drops_its_text_twin() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        // Alt+3 = Key{Num3, alt} immediately followed by Text("3") (egui-winit)
        let mut evs = key_events(Key::Num3, ALT);
        evs.insert(1, Event::Text("3".into()));
        rig.frame_with(evs);
        assert_eq!(rig.route(), &Route::Guide { collection: None, focus: None });
        assert_eq!(probe.borrow().actions, [Action::GoGuide], "no FilterInput('3')");

        // the twin may arrive with the next batch of input
        rig.app.routes = vec![Route::Live { group: None, channel: None }];
        probe.borrow_mut().actions.clear();
        rig.frame_with(vec![Event::Key { key: Key::Num2, physical_key: None, pressed: true, repeat: false, modifiers: ALT }]);
        rig.frame_with(vec![Event::Text("2".into())]);
        assert_eq!(probe.borrow().actions, [Action::GoLive]);

        // only the event right after the chord is dropped
        rig.frame_with(vec![Event::Text("y".into())]);
        assert_eq!(probe.borrow().actions, [Action::GoLive, Action::FilterInput('y')]);

        // an Alt chord nobody binds keeps its text (AltGr-style input)
        let mut evs = key_events(Key::Z, ALT);
        evs.insert(1, Event::Text("z".into()));
        rig.frame_with(evs);
        assert_eq!(probe.borrow().actions.last(), Some(&Action::FilterInput('z')));
    }

    #[test]
    fn mouse_back_button_is_back() {
        let mut rig = Rig::new();
        rig.app.routes.push(Route::Logs);
        let click = |pressed| Event::PointerButton { pos: egui::pos2(5.0, 5.0), button: egui::PointerButton::Extra1, pressed, modifiers: NONE };
        rig.frame_with(vec![click(true), click(false)]);
        assert_eq!(rig.route(), &Route::Live { group: None, channel: None });
    }

    // ---- dispatch layers -------------------------------------------------------------------

    #[test]
    fn overlay_ignored_actions_fall_back_to_globals() {
        let mut rig = Rig::new();
        rig.press(Key::J, CTRL);
        assert_eq!(rig.overlay_kinds(), [kind(OverlayRequest::Jobs)]);
        // the jobs panel doesn't bind Alt+3 → globals switch tabs (and the tab switch closes it)
        rig.press(Key::Num3, ALT);
        assert_eq!(rig.route(), &Route::Guide { collection: None, focus: None });
        assert!(rig.app.overlays.is_empty());
        // Ctrl+Q still quits with an overlay open
        rig.press(Key::J, CTRL);
        let out = rig.press(Key::Q, CTRL);
        assert!(out.viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::Close)));
    }

    #[test]
    fn screens_get_actions_before_globals() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        rig.frame_with(vec![]);
        assert_eq!(probe.borrow().entered, [Route::Live { group: None, channel: None }]);
        rig.press(Key::ArrowDown, NONE);
        rig.press(Key::F5, NONE); // screen declines → global RefreshSource
        assert_eq!(probe.borrow().actions, [Action::Down, Action::RefreshSource]);
        assert_eq!(rig.log().jobs.last(), Some(&JobKind::RefreshStale));
    }

    #[test]
    fn help_opens_with_the_current_contexts_first_and_toggles() {
        let mut rig = Rig::new();
        rig.size = egui::vec2(600.0, 2400.0); // one column, tall enough to lay out the top
        rig.probe(ScreenId::Live, vec![KeyContext::ListFiltering]);
        rig.frame_with(vec![]);
        rig.press(Key::F1, NONE);
        assert_eq!(rig.overlay_kinds(), [kind(OverlayRequest::Help)]);
        let out = rig.frame_with(vec![]);
        let t = texts(&out);
        let filtering = find(&t, "LISTS WHILE FILTERING").expect("ListFiltering section");
        let lists = find(&t, "LISTS").expect("List section");
        assert!(filtering.y0 < lists.y0, "the current context comes first");
        // F1 again closes it (the overlay handles its own toggle)
        rig.press(Key::F1, NONE);
        assert!(rig.app.overlays.is_empty());
        // `?`-style requests from screens open the same overlay with the current stack
        rig.app.nav.push(NavRequest::Open(OverlayRequest::Help));
        rig.frame_with(vec![]);
        assert_eq!(rig.overlay_kinds(), [kind(OverlayRequest::Help)]);
    }

    #[test]
    fn help_from_an_overlay_lists_that_overlays_contexts() {
        let mut rig = Rig::new();
        rig.press(Key::J, CTRL); // jobs panel: [List, Modal]
        let stack = rig.app.key_stack();
        rig.press(Key::F1, NONE); // Ignored by the panel → global help over it
        assert_eq!(rig.overlay_kinds(), [kind(OverlayRequest::Jobs), kind(OverlayRequest::Help)]);
        assert_eq!(stack[0], KeyContext::List);
        rig.press(Key::Escape, NONE);
        rig.press(Key::Escape, NONE);
        assert!(rig.app.overlays.is_empty());
        assert_eq!(rig.route(), &Route::Live { group: None, channel: None });
    }

    #[test]
    fn overlay_results_go_to_the_screen_that_opened_it() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        let spec = crate::ui::overlays::ConfirmSpec { title: "Delete?".into(), body: String::new(), confirm_label: "Delete".into(), destructive: false, token: 7 };
        rig.app.nav.push(NavRequest::Open(OverlayRequest::Confirm(spec)));
        rig.frame_with(vec![]);
        assert_eq!(rig.overlay_kinds().len(), 1);
        rig.press(Key::Escape, NONE);
        assert!(rig.app.overlays.is_empty());
        assert_eq!(probe.borrow().results, [OverlayResult::Cancelled(7)]);
    }

    // ---- Esc ladder ------------------------------------------------------------------------

    #[test]
    fn esc_ladder() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Live, vec![KeyContext::List]);
        // 1. the screen first (clear filter, close side panel…)
        probe.borrow_mut().handle_back = true;
        rig.press(Key::Escape, NONE);
        assert_eq!(probe.borrow().actions, [Action::Back]);
        probe.borrow_mut().handle_back = false;
        // 2. an overlay closes before anything below it
        rig.press(Key::J, CTRL);
        rig.press(Key::Escape, NONE);
        assert!(rig.app.overlays.is_empty());
        assert_eq!(probe.borrow().actions, [Action::Back, Action::ShowJobs], "the overlay took that Esc");
        // 3. player in window fullscreen: leave fullscreen, then the player
        rig.app.nav.push(NavRequest::Push(Route::Player));
        rig.frame_with(vec![]);
        let out = rig.press(Key::F11, NONE);
        assert!(rig.app.window_fullscreen);
        assert!(out.viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::Fullscreen(true))));
        let out = rig.press(Key::Escape, NONE);
        assert!(!rig.app.window_fullscreen);
        assert!(out.viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::Fullscreen(false))));
        assert_eq!(rig.route(), &Route::Player);
        rig.press(Key::Escape, NONE);
        assert_eq!(rig.route(), &Route::Live { group: None, channel: None });
        assert_eq!(probe.borrow().entered.len(), 2, "returning from the player re-enters Live");
        // 4. a tab root stays put
        rig.press(Key::Escape, NONE);
        assert_eq!(rig.app.routes, [Route::Live { group: None, channel: None }]);
        // 5. a lone sub-page goes to its parent
        rig.app.routes = vec![Route::Logs];
        rig.press(Key::Escape, NONE);
        assert_eq!(rig.app.routes, [Route::Settings(Default::default())]);
        rig.app.routes = vec![Route::Player];
        rig.press(Key::Escape, NONE);
        assert_eq!(rig.app.routes, [Route::Live { group: None, channel: None }]);
        // Alt+← is Back too
        rig.app.routes.push(Route::MovieDetail(crate::model::MovieId(1)));
        rig.press(Key::ArrowLeft, ALT);
        assert_eq!(rig.app.routes, [Route::Live { group: None, channel: None }]);
    }

    // ---- data scopes, jobs, events -----------------------------------------------------------

    #[test]
    fn data_scope_reactions() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| seed_channels(c, 3, None));
        rig.app.log.borrow_mut().jobs.clear();
        // rebuild scopes
        for scope in [DataScope::Live, DataScope::Vod, DataScope::Overrides, DataScope::History, DataScope::Sources, DataScope::All] {
            rig.event(AppEvent::Data(scope));
        }
        assert_eq!(rig.log().jobs.len(), 6);
        assert!(rig.log().jobs.iter().all(|j| *j == JobKind::RebuildLibrary));
        // screens react themselves
        rig.event(AppEvent::Data(DataScope::VodDetails));
        rig.event(AppEvent::Data(DataScope::Progress));
        assert_eq!(rig.log().jobs.len(), 6);
        // EPG: fresh guide time + rebuild
        rig.writer()
            .blocking(|c| Ok(c.execute("INSERT INTO epg_source(name, kind, url, last_refresh) VALUES ('G', 'url', 'http://127.0.0.1/e.xml', 1234567)", [])?))
            .unwrap();
        assert_eq!(rig.app.epg_updated, None);
        rig.event(AppEvent::Data(DataScope::Epg));
        assert_eq!(rig.app.epg_updated, Some(1_234_567));
        assert_eq!(rig.log().jobs.len(), 7);
        // favourites reload from the DB into the app-owned index
        let item = channel_ref(1);
        assert!(!rig.app.favs.is_fav(&item));
        let it = item.clone();
        rig.writer()
            .blocking(move |c| {
                let list = crate::store::user::default_fav_list(c)?;
                crate::store::user::set_favourite(c, list, &it, true, 1)
            })
            .unwrap();
        rig.event(AppEvent::Data(DataScope::Favourites));
        assert!(rig.app.favs.is_fav(&item));
        assert_eq!(rig.log().jobs.len(), 7, "favourites never rebuild the library");
    }

    #[test]
    fn library_swap_copies_favourites_and_resets_search() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| {
            seed_channels(c, 2, None)?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(0), true, 1)
        });
        rig.app.search_index = Some(SearchIndex::new(Arc::new(|| {})));
        deliver_library(&mut rig);
        assert_eq!(rig.app.lib.live.channels.len(), 2);
        assert!(rig.app.favs.is_fav(&channel_ref(0)));
        assert!(rig.app.search_index.is_none());
    }

    #[test]
    fn connection_limits_follow_refreshes() {
        let mut rig = Rig::new();
        let account = crate::model::AccountInfo { max_connections: Some(2), ..Default::default() };
        let refreshed = |account| {
            AppEvent::Job(JobEvent::Finished {
                id: JobId(3),
                kind: JobKind::RefreshSource { source: SourceId(4), force: false },
                outcome: Ok(JobOutcome::SourceRefreshed { source: SourceId(4), summary: Default::default(), not_modified: false, account }),
            })
        };
        rig.event(refreshed(None));
        rig.event(refreshed(Some(account)));
        let limits: Vec<_> = rig.log().commands.iter().filter(|c| matches!(c, PlayerCommand::SetConnectionLimit { .. })).cloned().collect();
        assert_eq!(limits, [PlayerCommand::SetConnectionLimit { source: SourceId(4), max: Some(2) }]);
    }

    #[test]
    fn failed_refreshes_toast_and_flag_offline_until_the_next_success() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| seed_channels(c, 1, None));
        let failed = |kind, msg: &str| AppEvent::Job(JobEvent::Finished { id: JobId(5), kind, outcome: Err(msg.to_owned()) });
        rig.event(failed(JobKind::RebuildLibrary, "boom"));
        rig.event(failed(JobKind::RefreshSource { source: SourceId(1), force: false }, "cancelled"));
        assert!(rig.log().toasts.is_empty(), "library builds and cancellations stay quiet");
        rig.event(failed(JobKind::RefreshSource { source: SourceId(1), force: false }, "network error: dns failure"));
        assert!(rig.app.offline);
        assert_eq!(rig.log().toasts.last().unwrap().text, "Couldn't refresh My IPTV: network error: dns failure");
        rig.event(failed(JobKind::RefreshEpg { epg_source: crate::model::EpgSourceId(1), force: false }, "HTTP 404 from http://h/e.xml"));
        assert!(!rig.app.offline, "an HTTP error means the network works");
        assert_eq!(rig.log().toasts.last().unwrap().kind, ToastKind::Error);
        rig.event(failed(JobKind::RefreshStale, "network error: timeout"));
        assert!(rig.app.offline);
        rig.event(AppEvent::Job(JobEvent::Finished {
            id: JobId(6),
            kind: JobKind::RefreshSource { source: SourceId(1), force: false },
            outcome: Ok(JobOutcome::SourceRefreshed { source: SourceId(1), summary: Default::default(), not_modified: true, account: None }),
        }));
        assert!(!rig.app.offline);
    }

    #[test]
    fn job_events_reach_every_screen() {
        let mut rig = Rig::new();
        let probe = rig.probe(ScreenId::Settings, vec![KeyContext::Panel]);
        rig.event(AppEvent::Data(DataScope::Progress));
        rig.event(AppEvent::Toast(Toast::info("hello")));
        assert_eq!(probe.borrow().events, 2, "screens get events while not current");
        assert_eq!(rig.log().toasts.last().unwrap().text, "hello");
    }

    #[test]
    fn reminders_refresh_on_their_data_scope() {
        let mut cfg = test_config();
        cfg.reminders.enabled = true;
        let now = crate::model::now();
        let insert = move |c: &mut rusqlite::Connection, key: &str| -> crate::error::Result<()> {
            // ended an hour ago: the scheduler dismisses it silently (no notification)
            c.execute(
                "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop) VALUES (1, ?1, 'Channel', 'Show', ?2, ?3)",
                rusqlite::params![key, now - 7200, now - 3600],
            )?;
            Ok(())
        };
        let mut rig = Rig::with(cfg, live(), 1.0, move |c| {
            seed_channels(c, 1, None)?;
            insert(c, "m3u:ch0")
        });
        let state = |rig: &Rig, key: &str| -> String {
            rig.app.db.query_row("SELECT state FROM reminder WHERE channel_key = ?1", [key], |r| r.get(0)).unwrap()
        };
        // the scheduler's first load dismissed the first one
        rig.until("the startup load", |r| state(r, "m3u:ch0") != "pending");
        rig.writer().blocking(move |c| insert(c, "m3u:ch1")).unwrap();
        let asked = Instant::now();
        rig.event(AppEvent::Data(DataScope::Reminders));
        rig.until("the refreshed load", |r| state(r, "m3u:ch1") != "pending");
        assert!(asked.elapsed() < Duration::from_secs(10), "picked up by the refresh, not the minute tick");
    }

    #[test]
    fn log_export_runs_off_the_ui_thread_and_toasts() {
        let mut rig = Rig::new();
        rig.app.services().logs.push(crate::logging::LogLine { at: 0, level: tracing::Level::INFO, target: "tuner".into(), message: "exported line".into() });
        let path = rig.dir.path().join("log.txt");
        rig.event(AppEvent::Platform(PlatformEvent::FilePicked { purpose: PickPurpose::ExportLog, path: Some(path.clone()) }));
        rig.until("the export toast", |r| r.log().toasts.iter().any(|t| t.kind == ToastKind::Success));
        assert!(std::fs::read_to_string(&path).unwrap().contains("exported line"));
        // other pickers are left to their screens; a cancelled pick does nothing
        rig.event(AppEvent::Platform(PlatformEvent::FilePicked { purpose: PickPurpose::ExportLog, path: None }));
        rig.event(AppEvent::Platform(PlatformEvent::FilePicked { purpose: PickPurpose::OpenPlaylist, path: Some(path) }));
        assert_eq!(rig.log().toasts.len(), 1);
    }

    // ---- CLI / IPC ------------------------------------------------------------------------

    #[test]
    fn cli_and_ipc_requests() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| seed_channels(c, 3, None));
        let ipc = |rig: &mut Rig, view, play: Option<&str>, inputs: &[&str]| {
            let req = IpcRequest { view, play: play.map(str::to_owned), inputs: inputs.iter().map(|s| s.to_string()).collect() };
            rig.event(AppEvent::Ipc(req));
            rig.frame_with(vec![]);
        };
        // view switch
        ipc(&mut rig, Some(crate::cli::ViewArg::Guide), None, &[]);
        assert_eq!(rig.route(), &Route::Guide { collection: None, focus: None });
        ipc(&mut rig, Some(crate::cli::ViewArg::Search), None, &[]);
        assert_eq!(rig.overlay_kinds(), [kind(OverlayRequest::Search { initial: String::new() })]);
        rig.app.overlays.clear();
        // a playlist file opens the onboarding card at Details
        ipc(&mut rig, None, None, &["/music/My List.M3U8"]);
        assert_eq!(rig.route(), &Route::Onboarding(OnboardingStart::PlaylistFile("/music/My List.M3U8".into())));
        // a URL plays fullscreen (an explicit --play wins over positional media)
        ipc(&mut rig, None, Some("http://127.0.0.1:9/a.ts"), &["http://127.0.0.1:9/b.ts", "/tmp/c.m3u"]);
        assert_eq!(played(&rig.log()), [(MediaRef::Url("http://127.0.0.1:9/a.ts".into()), PlayOrigin::Cli)]);
        assert_eq!(rig.route(), &Route::Player);
        assert_eq!(rig.app.routes[0], Route::Onboarding(OnboardingStart::PlaylistFile("/tmp/c.m3u".into())));
        // a channel reference resolves through the DB before the library has loaded…
        rig.app.routes = vec![Route::Home];
        ipc(&mut rig, None, Some("1:m3u:ch2"), &[]);
        let id = crate::store::live::channel_by_ref(&rig.app.db, &channel_ref(2)).unwrap().unwrap().id;
        assert_eq!(played(&rig.log()).last(), Some(&(MediaRef::Channel(id), PlayOrigin::Cli)));
        assert_eq!(rig.app.routes, [Route::Home, Route::Player]);
        // …and zaps through its primary group once it has
        deliver_library(&mut rig);
        ipc(&mut rig, None, Some("1:m3u:ch0"), &[]);
        let zap = rig.log().commands.iter().rev().find_map(|c| match c {
            PlayerCommand::Play(r) => r.zap.clone(),
            _ => None,
        });
        assert_eq!(zap.map(|z| z.channels.len()), Some(3));
        // an unknown reference only explains itself
        let before = played(&rig.log()).len();
        ipc(&mut rig, None, Some("1:m3u:nope"), &[]);
        assert_eq!(played(&rig.log()).len(), before);
        assert!(rig.log().toasts.last().unwrap().text.contains("1:m3u:nope"));
    }

    #[test]
    fn initial_cli_request_applies_at_startup() {
        let req = IpcRequest { view: Some(crate::cli::ViewArg::Settings), play: Some("http://127.0.0.1:9/x.ts".into()), inputs: vec![] };
        let rig = Rig::with(test_config(), req, 1.0, |_| Ok(()));
        assert_eq!(rig.app.routes, [Route::Settings(Default::default()), Route::Player]);
        assert_eq!(played(&rig.log()), [(MediaRef::Url("http://127.0.0.1:9/x.ts".into()), PlayOrigin::Cli)]);
        assert_eq!(rig.log().jobs.first(), Some(&JobKind::RebuildLibrary), "the library loads asynchronously at startup");
    }

    #[test]
    fn reminder_watch_plays_the_channel() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| seed_channels(c, 2, None));
        deliver_library(&mut rig);
        rig.event(AppEvent::Platform(PlatformEvent::ReminderWatch { reminder: crate::model::ReminderId(1), channel: channel_ref(1) }));
        rig.frame_with(vec![]);
        let id = rig.app.lib.live.by_ref(&channel_ref(1)).unwrap();
        assert_eq!(played(&rig.log()), [(MediaRef::Channel(id), PlayOrigin::Reminder)]);
        assert_eq!(rig.route(), &Route::Player);
        rig.event(AppEvent::Platform(PlatformEvent::ReminderWatch { reminder: crate::model::ReminderId(2), channel: channel_ref(9) }));
        assert_eq!(played(&rig.log()).len(), 1);
        assert_eq!(rig.log().toasts.len(), 1);
    }

    #[test]
    fn channel_references_parse_strictly() {
        let r = parse_channel_ref("2:xt:live:1234").unwrap();
        assert_eq!((r.source, r.kind, r.key.as_str()), (SourceId(2), ItemKind::Channel, "xt:live:1234"));
        assert_eq!(parse_channel_ref("10:m3u:0123abcd#2").unwrap().key.as_str(), "m3u:0123abcd#2");
        for s in ["http://h/x.ts", "/tmp/a.ts", "2:xt:vod:5", "x:m3u:ab", "2:xt:live:", "2:m3u:", "2", "file.ts"] {
            assert_eq!(parse_channel_ref(s), None, "{s}");
        }
        assert!(is_playlist_file("/a/b.m3u") && is_playlist_file("B.M3U8"));
        assert!(!is_playlist_file("http://h/list.m3u8") && !is_playlist_file("/a/b.ts"));
    }

    // ---- globals ----------------------------------------------------------------------------

    #[test]
    fn tab_globals_and_vod_absence() {
        let mut rig = Rig::with(test_config(), live(), 1.0, |c| seed_channels(c, 1, None));
        rig.press(Key::Num1, ALT);
        assert_eq!(rig.route(), &Route::Home);
        rig.press(Key::Num0, ALT);
        assert_eq!(rig.route(), &Route::Settings(Default::default()));
        // before the library loaded, Movies opens (skeleton rows)
        rig.press(Key::Num4, ALT);
        assert_eq!(rig.route(), &Route::Movies { group: None });
        // a loaded library without VOD explains instead
        deliver_library(&mut rig);
        rig.press(Key::Num5, ALT);
        assert_eq!(rig.route(), &Route::Movies { group: None });
        assert_eq!(rig.log().toasts.last().unwrap().text, "Your playlists have no movies or series");
        // the player only opens when something plays; stop / play-pause go to the player
        rig.press(Key::P, CTRL);
        assert_eq!(rig.route(), &Route::Movies { group: None });
        rig.press(Key::S, Modifiers { shift: true, ..CTRL });
        rig.press(Key::P, Modifiers { shift: true, ..CTRL });
        let cmds = &rig.log().commands;
        assert!(cmds.contains(&PlayerCommand::Stop) && cmds.contains(&PlayerCommand::TogglePause));
    }

    // ---- settings ----------------------------------------------------------------------------

    #[test]
    fn config_edits_are_saved_and_applied() {
        let mut rig = Rig::new();
        let file = rig.app.services().paths.config_file();
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.player.cache_secs = 33));
        rig.frame_with(vec![]);
        assert_eq!(rig.app.services().config().player.cache_secs, 33);
        assert_eq!(Config::load(&file).unwrap().player.cache_secs, 33);
        let applied: Vec<u32> = rig
            .log()
            .commands
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::ApplyConfig(cfg) => Some(cfg.player.cache_secs),
                _ => None,
            })
            .collect();
        assert_eq!(applied, [33]);
        // an edit that changes nothing sends nothing
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.player.cache_secs = 33));
        rig.frame_with(vec![]);
        assert_eq!(rig.log().commands.iter().filter(|c| matches!(c, PlayerCommand::ApplyConfig(_))).count(), 1);
        // UI scale re-derives the theme
        let rem = rig.app.theme.rem();
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.interface.ui_scale = 1.5));
        rig.frame_with(vec![]);
        assert!((rig.app.theme.rem() - rem * 1.5).abs() < 1e-4);
    }

    #[test]
    fn integration_settings_apply_live() {
        let mut rig = Rig::new();
        assert!(rig.app.services().integrations.reminders.is_none());
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.reminders.enabled = true));
        rig.frame_with(vec![]);
        assert!(rig.app.services().integrations.reminders.is_some());
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.reminders.enabled = false));
        rig.frame_with(vec![]);
        assert!(rig.app.services().integrations.reminders.is_none());
        // follow the Omarchy theme (fake home: nothing there → fallback palette, loaded on a
        // worker), then stop following
        rig.app.config_edits.push(Box::new(|c: &mut Config| c.integration.follow_omarchy_theme = true));
        rig.frame_with(vec![]);
        assert!(rig.app.services().integrations.theme_watch.is_some());
        let before = Arc::as_ptr(&rig.app.theme);
        rig.until("the reloaded theme", |r| Arc::as_ptr(&r.app.theme) != before);
        rig.app.config_edits.push(Box::new(|c: &mut Config| {
            c.integration.follow_omarchy_theme = false;
            c.interface.ui_scale = 1.25;
        }));
        rig.frame_with(vec![]);
        assert!(rig.app.services().integrations.theme_watch.is_none());
        assert_eq!(rig.app.theme.name, "fallback");
        assert!((rig.app.theme.rem() - 1.25).abs() < 1e-4);
    }

    #[test]
    fn start_screen_last_is_remembered_at_exit() {
        let mut cfg = test_config();
        cfg.general.start_screen = crate::config::StartScreen::Last;
        let mut rig = Rig::with(cfg, live(), 1.0, |c| seed_channels(c, 1, None));
        rig.app.routes = vec![Route::Guide { collection: None, focus: None }, Route::Player];
        let file = rig.app.services().paths.config_file();
        rig.app.remember_last_screen();
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.interface.last_screen.as_deref(), Some("guide"));
        assert_eq!(start_route(&saved, &IpcRequest::default(), &rig.app.db), Route::Guide { collection: None, focus: None });
    }

    #[test]
    fn start_routes() {
        let store = crate::store::Store::open_temp("tuner-app-start", None).unwrap();
        let db = store.reader().unwrap();
        let cfg = Config::default();
        let none = IpcRequest::default();
        assert_eq!(start_route(&cfg, &none, &db), Route::Onboarding(OnboardingStart::Welcome));
        assert_eq!(start_route(&cfg, &live(), &db), Route::Live { group: None, channel: None }, "--view wins");
        store.writer().blocking(|c| seed_channels(c, 1, None)).unwrap();
        assert_eq!(start_route(&cfg, &none, &db), Route::Live { group: None, channel: None }, "Auto without history");
        store
            .writer()
            .blocking(|c| Ok(c.execute("INSERT INTO history(source_id, kind, item_key, title, at) VALUES (1, 'channel', 'm3u:ch0', 'C', 1)", [])?))
            .unwrap();
        assert_eq!(start_route(&cfg, &none, &db), Route::Home, "Auto with history");
        let mut c = cfg.clone();
        c.general.start_screen = crate::config::StartScreen::Guide;
        assert_eq!(start_route(&c, &none, &db), Route::Guide { collection: None, focus: None });
        c.general.start_screen = crate::config::StartScreen::Last;
        assert_eq!(start_route(&c, &none, &db), Route::Home, "nothing remembered yet");
        c.interface.last_screen = Some("series".into());
        assert_eq!(start_route(&c, &none, &db), Route::Series { group: None });
        c.interface.last_screen = Some("search".into());
        assert_eq!(start_route(&c, &none, &db), Route::Home, "search is not a screen");
        assert!(store.close(Duration::from_secs(5)));
    }

    // ---- layout -----------------------------------------------------------------------------

    /// DESIGN §12.1 at a tiled half (790 pt) and full screen (1600 pt), Omarchy text size 12
    /// and 16 (rem 1 and 4/3); below 480·rem the sidebar becomes the top glyph strip.
    #[test]
    fn usable_at_790_and_1600_pt_with_text_size_12_and_16() {
        // (width, rem, class, sidebar width; None = top glyph strip). 1600 pt at rem 4/3 sits
        // exactly on the Wide breakpoint (1200·rem), so either side of it is accepted there.
        let cases: [(f32, f32, &[LayoutClass], Option<f32>); 5] = [
            (790.0, 1.0, &[LayoutClass::Medium], Some(48.0)),
            (790.0, 4.0 / 3.0, &[LayoutClass::Compact], Some(64.0)),
            (1600.0, 1.0, &[LayoutClass::Wide], Some(184.0)),
            (1600.0, 4.0 / 3.0, &[LayoutClass::Medium, LayoutClass::Wide], None),
            (420.0, 1.0, &[LayoutClass::Compact], None),
        ];
        for (w, scale, classes, sidebar) in cases {
            let mut rig = Rig::with(test_config(), live(), scale, |_| Ok(()));
            rig.size = egui::vec2(w, 800.0);
            rig.frame_with(vec![]);
            let out = rig.frame_with(vec![]);
            assert!(classes.contains(&rig.app.layout), "{w} pt @ {scale}: {:?}", rig.app.layout);
            let sidebar = sidebar.or_else(|| {
                let s = crate::ui::layout::sidebar_width(w, rig.app.theme.rem(), Default::default());
                (s > 0.0).then_some(s)
            });
            let t = texts(&out);
            let home = *button(&t, "Home").unwrap_or_else(|| panic!("sidebar entry at {w} pt @ {scale}: {t:?}"));
            let live_tv = *button(&t, "Live TV").expect("Live TV entry");
            let settings = *button(&t, "Settings").expect("Settings entry");
            match sidebar {
                Some(width) => {
                    assert!(((home.x1 - home.x0) as f32 - width).abs() <= 1.0, "sidebar width at {w} pt @ {scale}: {home:?}");
                    assert!(live_tv.y0 > home.y0, "vertical sidebar");
                }
                None => {
                    assert_eq!(live_tv.y0, home.y0, "glyph strip is one row");
                    assert!(live_tv.x0 > home.x0 && settings.x1 as f32 <= w, "the strip fits the window");
                    assert!(home.y0 < 10.0, "strip at the top");
                }
            }
            // everything stays inside the window and the status bar (hint line) shows
            assert!(settings.y1 < 800.0);
            assert!(t.iter().any(|(s, r, _)| s.contains("help") && r.y0 > 700.0), "status-bar hint line at {w} pt @ {scale}");
        }
    }

    #[test]
    fn chrome_hides_in_the_player_and_onboarding() {
        let mut rig = Rig::new();
        let out = rig.frame_with(vec![]);
        assert!(button(&texts(&out), "Live TV").is_some());
        for route in [Route::Player, Route::Onboarding(OnboardingStart::Welcome)] {
            rig.app.routes = vec![route];
            rig.frame_with(vec![]);
            let out = rig.frame_with(vec![]);
            assert!(button(&texts(&out), "Live TV").is_none());
        }
    }

    /// Navigation to every screen (WP-11/12 bodies included) and every overlay never panics.
    #[test]
    fn every_route_and_overlay_draws() {
        let mut rig = Rig::new();
        let routes = [
            Route::Home,
            Route::Live { group: None, channel: None },
            Route::Guide { collection: None, focus: None },
            Route::Movies { group: None },
            Route::Series { group: None },
            Route::MovieDetail(crate::model::MovieId(1)),
            Route::SeriesDetail(crate::model::SeriesId(1)),
            Route::Player,
            Route::Settings(Default::default()),
            Route::Onboarding(OnboardingStart::AddSource),
            Route::EpgMapping { channel: None },
            Route::Logs,
        ];
        for r in routes {
            rig.app.nav.push(NavRequest::Push(r.clone()));
            rig.frame_with(vec![]);
            rig.press(Key::ArrowDown, NONE);
            rig.frame_with(vec![]);
            assert_eq!(rig.route(), &r);
        }
        let overlays = [
            OverlayRequest::Search { initial: "bbc".into() },
            OverlayRequest::Jobs,
            OverlayRequest::Help,
            OverlayRequest::Tracks { audio: false },
            OverlayRequest::Resume(crate::ui::overlays::ResumeSpec { media: MediaRef::Url("x".into()), title: "t".into(), position_s: 10.0, duration_s: None }),
        ];
        for o in overlays {
            rig.app.nav.push(NavRequest::Open(o));
            rig.frame_with(vec![]);
            rig.press(Key::ArrowDown, NONE);
            rig.press(Key::Escape, NONE);
            rig.frame_with(vec![]);
        }
    }
}
