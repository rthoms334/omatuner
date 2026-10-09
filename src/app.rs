//! `eframe::App` — the integration point. Owner: WP-10b (every call site wired in the
//! skeleton). DESIGN §5, §12, §15.
//!
//! Frame anatomy:
//! * `raw_input_hook()` (before egui processes input): resolve every key event against the
//!   current context stack (`keymap::resolve`) and REMOVE resolved events from the input, so
//!   egui widgets never handle them a second time (Enter would also click the focused button,
//!   Tab would move egui focus). Also maps the mouse back button to `Back`.
//! * `logic()`: drain `AppEvent`s, refresh the player snapshot, render mpv into the FBO (GL
//!   current), idle inhibitor. NOTE: eframe calls `logic` only when the compositor sends frame
//!   callbacks — on Hyprland NOT for windows on hidden workspaces unless the window has the
//!   `render_unfocused` rule (`packaging/hypr/tuner.lua`). Nothing that must keep running
//!   while hidden lives here (mpv events, reconnects, MPRIS, reminders run on other threads;
//!   the player suspends video decoding when no frames are rendered).
//! * `ui()`: dispatch the resolved actions (overlay → screen → globals; `Ignored` overlay
//!   actions fall back to globals) → status bar / sidebar / content → mini-player → overlays →
//!   toasts → apply queued navigation and config edits.
//! * `on_exit()`: destroy the video surface (GL), then shut services down (mpv last).
//!
//! Only ONE native viewport is allowed (eframe makes the GL context current before `logic`
//! only for a single viewport; `VideoHost::render` relies on it).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Receiver;
use eframe::egui;
use eframe::glow;

use crate::config::Config;
use crate::fonts::LoadedFonts;
use crate::images::ImageCache;
use crate::ipc::IpcRequest;
use crate::keymap::{self, Action, KeyContext};
use crate::model::MediaRef;
use crate::msg::{AppEvent, DataScope, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerSnapshot, PlayerState};
use crate::search::SearchIndex;
use crate::services::Services;
use crate::store::library::{FavIndex, Library};
use crate::theme::{Theme, ThemeEvent};
use crate::ui::data::{FrameBudget, NowNextCache};
use crate::ui::overlays::{self, Overlay, OverlayOutcome};
use crate::ui::video_host::{Placement, VideoHost};
use crate::ui::widgets::status_bar::{StatusClick, StatusInfo};
use crate::ui::widgets::toast::Toasts;
use crate::ui::{LayoutClass, NavRequest, OnboardingStart, OverlayRequest, ParentalSession, Route, Screen, ScreenId, UiCtx, UiParts};

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
    overlays: Vec<(Box<dyn Overlay>, ScreenId)>,
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
    idle: Option<crate::platform::idle::IdleInhibitor>,
    window_fullscreen: bool,
    entered: Option<ScreenId>,
    layout: LayoutClass,
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
            idle,
            window_fullscreen: false,
            entered: None,
            layout: LayoutClass::Wide,
        };
        app.services().jobs.submit(crate::jobs::JobKind::RebuildLibrary);
        app.handle_ipc(&initial);
        app
    }

    fn services(&self) -> &Services { self.services.as_ref().expect("services alive") }

    fn current_route(&self) -> &Route { self.routes.last().expect("route stack never empty") }

    fn screen_index(&self, id: ScreenId) -> usize {
        self.screens.iter().position(|s| s.id() == id).expect("every ScreenId has a screen")
    }

    /// The key context stack right now (topmost overlay, else the current screen).
    fn key_stack(&self) -> Vec<KeyContext> {
        match self.overlays.last() {
            Some((o, _)) => o.key_contexts(),
            None => self.screens[self.screen_index(self.current_route().screen())].key_contexts(),
        }
    }

    /// Run `f` with a fresh `UiCtx` and a screen/overlay borrowed alongside it.
    fn with_ctx<R>(&mut self, f: impl FnOnce(&mut UiCtx<'_>, &mut Vec<Box<dyn Screen>>, &mut Vec<(Box<dyn Overlay>, ScreenId)>) -> R) -> R {
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
            AppEvent::Job(crate::jobs::JobEvent::Finished { outcome: Ok(outcome), .. }) => match outcome {
                crate::jobs::JobOutcome::Library(lib) => {
                    self.lib = lib.clone();
                    self.favs = lib.favs.clone();
                    self.search_index = None;
                }
                crate::jobs::JobOutcome::SourceRefreshed { source, account: Some(a), .. } => {
                    self.services().player.handle().send(PlayerCommand::SetConnectionLimit { source: *source, max: a.max_connections });
                }
                _ => {}
            },
            AppEvent::Data(scope) => self.on_data_changed(*scope),
            AppEvent::Toast(t) => self.toasts.push(t.clone()),
            AppEvent::Ipc(req) => {
                let req = req.clone();
                self.handle_ipc(&req);
                crate::platform::hypr::focus_self();
            }
            AppEvent::Platform(p) => match p {
                crate::platform::PlatformEvent::Quit => egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                crate::platform::PlatformEvent::Raise => crate::platform::hypr::focus_self(),
                crate::platform::PlatformEvent::ReminderWatch { channel, .. } => {
                    if let Some(id) = self.lib.live.by_ref(channel) {
                        self.play_now(MediaRef::Channel(id), crate::model::PlayOrigin::Reminder);
                        self.nav.push(NavRequest::Push(Route::Player));
                    }
                    crate::platform::hypr::focus_self();
                }
                _ => {}
            },
            _ => {}
        }
        self.with_ctx(|ctx, screens, _| {
            for s in screens.iter_mut() {
                s.on_event(ctx, &ev);
            }
        });
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
                self.services().jobs.submit(crate::jobs::JobKind::RebuildLibrary);
            }
            DataScope::Reminders => {
                if let Some(r) = &self.services().integrations.reminders {
                    r.refresh();
                }
            }
            DataScope::Live | DataScope::Vod | DataScope::Overrides | DataScope::History | DataScope::Sources | DataScope::All => {
                self.services().jobs.submit(crate::jobs::JobKind::RebuildLibrary);
            }
            DataScope::VodDetails | DataScope::Progress => {}
        }
    }

    /// CLI / IPC requests: view switch, `--play <url|file>`, positional files/URLs
    /// (`.m3u`/`.m3u8` files open the onboarding card; other files/URLs play).
    fn handle_ipc(&mut self, req: &IpcRequest) {
        match req.view {
            Some(crate::cli::ViewArg::Search) => self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() })),
            Some(v) => self.nav.push(NavRequest::Switch(route_for_view(v))),
            None => {}
        }
        let mut play: Option<String> = req.play.clone();
        for input in &req.inputs {
            let lower = input.to_ascii_lowercase();
            let is_url = lower.contains("://");
            if !is_url && (lower.ends_with(".m3u") || lower.ends_with(".m3u8")) {
                let path = std::path::PathBuf::from(input);
                self.nav.push(NavRequest::Switch(Route::Onboarding(OnboardingStart::PlaylistFile(path))));
            } else if play.is_none() {
                play = Some(input.clone());
            }
        }
        if let Some(target) = play {
            let target = if target.contains("://") {
                target
            } else {
                std::fs::canonicalize(&target).map(|p| p.to_string_lossy().into_owned()).unwrap_or(target)
            };
            self.play_now(MediaRef::Url(target), crate::model::PlayOrigin::Cli);
            self.nav.push(NavRequest::Push(Route::Player));
        }
    }

    fn play_now(&self, media: MediaRef, origin: crate::model::PlayOrigin) {
        self.services().player.handle().send(PlayerCommand::Play(PlayRequest { media, zap: None, origin, resume: true, immediate: true }));
    }

    fn global_action(&mut self, action: Action, egui_ctx: &egui::Context) {
        let player = self.services().player.handle();
        match action {
            Action::OpenSearch => self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() })),
            Action::ToggleHelp => self.nav.push(NavRequest::Open(OverlayRequest::Help)),
            Action::ShowJobs => self.nav.push(NavRequest::Open(OverlayRequest::Jobs)),
            Action::GoHome => self.nav.push(NavRequest::Switch(Route::Home)),
            Action::GoLive => self.nav.push(NavRequest::Switch(Route::Live { group: None, channel: None })),
            Action::GoGuide => self.nav.push(NavRequest::Switch(Route::Guide { collection: None, focus: None })),
            Action::GoMovies | Action::GoSeries if self.lib.built_at != 0 && !self.lib.has_vod() && !self.lib.has_series() => {
                self.toasts.push(Toast::info("Your playlists have no movies or series"));
            }
            Action::GoMovies => self.nav.push(NavRequest::Switch(Route::Movies { group: None })),
            Action::GoSeries => self.nav.push(NavRequest::Switch(Route::Series { group: None })),
            Action::GoSettings => self.nav.push(NavRequest::Switch(Route::Settings(Default::default()))),
            Action::Quit => egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::ToggleWindowFullscreen => {
                self.window_fullscreen = !self.window_fullscreen;
                egui_ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.window_fullscreen));
            }
            Action::OpenPlayer => self.nav.push(NavRequest::Push(Route::Player)),
            Action::GlobalPlayPause => player.send(PlayerCommand::TogglePause),
            Action::StopPlayback => player.send(PlayerCommand::Stop),
            Action::RefreshSource => {
                self.services().jobs.submit(crate::jobs::JobKind::RefreshStale);
            }
            Action::Back => {
                if self.window_fullscreen && self.current_route().screen() == ScreenId::Player {
                    self.window_fullscreen = false;
                    egui_ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                } else {
                    self.nav.push(NavRequest::Back);
                }
            }
            _ => {}
        }
    }

    /// Dispatch the actions resolved in `raw_input_hook`.
    fn dispatch_actions(&mut self, egui_ctx: &egui::Context) {
        for (action, stack) in std::mem::take(&mut self.pending_actions) {
            if action == Action::ToggleHelp && self.overlays.is_empty() {
                self.overlays.push((Box::new(overlays::help::HelpOverlay::with_contexts(stack)), self.current_route().screen()));
                continue;
            }
            let handled = if !self.overlays.is_empty() {
                let outcome = self.with_ctx(|ctx, _, ovs| ovs.last_mut().map(|(o, _)| o.on_action(ctx, action)));
                let ignored = matches!(outcome, Some(OverlayOutcome::Ignored));
                self.apply_overlay_outcome(outcome);
                !ignored
            } else {
                let idx = self.screen_index(self.current_route().screen());
                self.with_ctx(|ctx, screens, _| screens[idx].on_action(ctx, action))
            };
            if !handled {
                self.global_action(action, egui_ctx);
            }
        }
    }

    fn apply_overlay_outcome(&mut self, outcome: Option<OverlayOutcome>) {
        match outcome {
            Some(OverlayOutcome::Close) => {
                self.overlays.pop();
            }
            Some(OverlayOutcome::CloseWith(result)) => {
                if let Some((_, opener)) = self.overlays.pop() {
                    let idx = self.screen_index(opener);
                    self.with_ctx(|ctx, screens, _| screens[idx].on_overlay_result(ctx, result));
                }
            }
            _ => {}
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
                    self.routes.clear();
                    self.routes.push(route);
                }
                NavRequest::Back => {
                    if self.routes.len() > 1 {
                        self.routes.pop();
                    }
                }
                NavRequest::Open(req) => {
                    let opener = self.current_route().screen();
                    let ov = self.with_ctx(|ctx, _, _| overlays::build(req, ctx));
                    self.overlays.push((ov, opener));
                }
                NavRequest::CloseOverlay => {
                    self.overlays.pop();
                }
            }
        }
        let current = self.current_route().screen();
        if self.entered != Some(current) {
            self.entered = Some(current);
            let route = self.current_route().clone();
            let idx = self.screen_index(current);
            self.with_ctx(|ctx, screens, _| screens[idx].on_enter(ctx, &route));
        }
    }

    /// Apply queued Settings edits: save, swap the shared config, re-apply to the player and
    /// the theme scale (no restart needed for any v1 setting).
    fn apply_config_edits(&mut self) {
        if self.config_edits.is_empty() {
            return;
        }
        let edits = std::mem::take(&mut self.config_edits);
        let services = self.services.as_ref().expect("services alive");
        let mut cfg: Config = (*services.config()).clone();
        let old_scale = cfg.interface.ui_scale;
        for e in edits {
            e(&mut cfg);
        }
        if let Err(e) = cfg.save(&services.paths.config_file()) {
            self.pending_toasts.push(Toast::error(format!("Couldn't save settings: {e}")));
        }
        let cfg = Arc::new(cfg);
        *services.config.write().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
        services.player.handle().send(PlayerCommand::ApplyConfig(Box::new((*cfg).clone())));
        if cfg.interface.ui_scale != old_scale {
            let t = &self.theme;
            self.theme = Arc::new(Theme::from_parts(t.name.clone(), t.palette.clone(), t.shell.clone(), t.tokens.rounding, cfg.interface.ui_scale));
        }
    }

    fn chrome_hidden(&self) -> bool { matches!(self.current_route().screen(), ScreenId::Player | ScreenId::Onboarding) }
}

impl eframe::App for TunerApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let stack = self.key_stack();
        let mut resolved: Vec<(Action, Vec<KeyContext>)> = Vec::new();
        raw.events.retain(|ev| {
            // mouse back button = Back (DESIGN §15.1)
            if let egui::Event::PointerButton { button: egui::PointerButton::Extra1, pressed, .. } = ev {
                if *pressed {
                    resolved.push((Action::Back, stack.clone()));
                }
                return false;
            }
            match keymap::resolve(&stack, ev) {
                Some(a) => {
                    resolved.push((a, stack.clone()));
                    false // consumed: egui never sees it
                }
                None => true,
            }
        });
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
            self.services().player.handle().send(PlayerCommand::ResumeVideo);
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
                epg_updated: None,
                offline: false,
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
                StatusClick::StopPlayback => self.services().player.handle().send(PlayerCommand::Stop),
                StatusClick::None => {}
            }
        }

        if chrome && side_w > 0.0 {
            let mut search_clicked = false;
            let has_vod = self.lib.has_vod() || self.lib.has_series() || self.lib.built_at == 0;
            let clicked = egui::Panel::left("sidebar")
                .exact_size(side_w)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.fill(theme.palette.dark_background))
                .show(ui, |ui| crate::ui::widgets::sidebar::sidebar(ui, &theme, current, side_w > 100.0, has_vod, &mut search_clicked))
                .inner;
            if let Some(id) = clicked {
                self.nav.push(NavRequest::Switch(route_for_screen(id)));
            }
            if search_clicked {
                self.nav.push(NavRequest::Open(OverlayRequest::Search { initial: String::new() }));
            }
        }

        let idx = self.screen_index(current);
        let fullscreen_player = current == ScreenId::Player;
        let pad = if fullscreen_player { 0.0 } else { theme.space(crate::theme::SpaceToken::PanelPadding) };
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
                Some(false) => self.services().player.handle().send(PlayerCommand::Stop),
                None => {}
            }
        }

        if let Some(i) = self.overlays.len().checked_sub(1) {
            let outcome = self.with_ctx(|ctx, _, ovs| ovs[i].0.ui(&egui_ctx, ctx));
            self.apply_overlay_outcome(Some(outcome));
        }
        for t in std::mem::take(&mut self.pending_toasts) {
            self.toasts.push(t);
        }
        if let Some(action) = self.toasts.show(&egui_ctx, &theme, self.video.mini_rect(), status_top) {
            let stack = self.key_stack();
            self.pending_actions.push((action, stack));
        }
        self.apply_nav();
        self.apply_config_edits();
        if self.budget.deferred() || self.now_next.pending() {
            egui_ctx.request_repaint();
        }
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
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
        S::Home | S::Last => Route::Home,
        S::Live => Route::Live { group: None, channel: None },
        S::Guide => Route::Guide { collection: None, focus: None },
        S::Auto => {
            if exists("SELECT EXISTS(SELECT 1 FROM fav_item) OR EXISTS(SELECT 1 FROM history)") {
                Route::Home
            } else {
                Route::Live { group: None, channel: None }
            }
        }
    }
}
