//! Movies (DESIGN §13.5): categories pane | poster grid (sort: recently added, A–Z, year,
//! rating; filter-as-you-type). Enter → MovieDetail. Owner: WP-12.
//!
//! [`VodBrowser`] is shared with the Series screen (identical apart from the item kind):
//! * Categories: All, Continue watching, Favourites, then the provider categories (renamed /
//!   reordered / hidden per overrides; locked ones show 󰌾 and need the PIN). Moving the
//!   cursor opens the category (locked ones only on Enter + PIN). Key context `[List]` /
//!   `[ListFiltering]` (typing filters the categories).
//! * Grid: `[PosterGrid]` / `[PosterGridFiltering]` — typing filters the posters (launcher
//!   idiom), Enter opens details, Shift+Enter plays/resumes fullscreen, Ctrl+S opens the sort
//!   menu (the choice is saved as `interface.movie_sort` / `series_sort`), ← on the first
//!   column / Backspace returns to the categories, Tab switches panes.
//! * Compact: one pane at a time (categories → grid).
//! * Progress bars and Continue watching come from one background read of the unfinished
//!   progress rows, refreshed on `DataScope::Progress`.

use std::collections::HashMap;

use eframe::egui;

use super::home::kit::{self, Loader};
use crate::keymap::{Action, KeyContext};
use crate::model::{GroupId, ItemKey, ItemKind, SortMode, SourceId, Timestamp, VodKind, VodSummary};
use crate::msg::{AppEvent, DataScope};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::poster::{self, GridState, TileDecor};
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, empty_state, glyphs, rows};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct MoviesScreen {
    browser: VodBrowser,
}

impl Screen for MoviesScreen {
    fn id(&self) -> ScreenId { ScreenId::Movies }
    fn key_contexts(&self) -> Vec<KeyContext> { self.browser.key_contexts() }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let group = match route {
            Route::Movies { group } => *group,
            _ => None,
        };
        self.browser.enter(ctx, true, group);
    }
    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) { self.browser.event(ctx, ev) }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { self.browser.action(ctx, action) }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) { self.browser.overlay_result(ctx, result) }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { self.browser.ui(ui, ctx) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Cat {
    #[default]
    All,
    Continue,
    Favourites,
    Group(GroupId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    Categories,
    #[default]
    Grid,
}

/// Sort-menu choices (echoed back as `Action::Digit(i)`).
const SORTS: [(SortMode, &str); 6] = [
    (SortMode::RecentlyAdded, "Recently added"),
    (SortMode::NameAsc, "A–Z"),
    (SortMode::NameDesc, "Z–A"),
    (SortMode::Year, "Year"),
    (SortMode::Rating, "Rating"),
    (SortMode::Provider, "Custom order"),
];
/// Grid context-menu choices (Digit codes above the sort codes).
const MENU_OPEN: u8 = 20;
const MENU_PLAY: u8 = 21;

fn sort_label(s: SortMode) -> &'static str { SORTS.iter().find(|(m, _)| *m == s).map(|(_, l)| *l).unwrap_or("Custom order") }

/// Unfinished progress per item key (fraction), plus the "Continue watching" order.
#[derive(Debug, Default, Clone)]
struct ProgressInfo {
    /// (source, key) of the movie/series → progress fraction.
    fraction: HashMap<(SourceId, ItemKey), f32>,
    /// Most recent first.
    order: Vec<(SourceId, ItemKey)>,
}

fn load_progress(c: &rusqlite::Connection, movies: bool) -> crate::error::Result<ProgressInfo> {
    use crate::store::{user, vod};
    let mut info = ProgressInfo::default();
    for p in user::continue_watching(c, 1000)? {
        let frac = p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d).clamp(0.0, 1.0) as f32).unwrap_or(0.0);
        let key = match (p.item.kind, movies) {
            (ItemKind::Movie, true) => (p.item.source, p.item.key.clone()),
            (ItemKind::Episode, false) => {
                let Some(id) = vod::resolve_episode(c, &p.item)? else { continue };
                let s = vod::series(c, vod::episode(c, id)?.series_id)?;
                (s.source_id, s.key)
            }
            _ => continue,
        };
        if !info.fraction.contains_key(&key) {
            info.order.push(key.clone());
            info.fraction.insert(key, frac);
        }
    }
    Ok(info)
}

/// Shift+Enter on a series: play the episode its page would open at (resume prompt when it
/// has progress); the page itself when nothing is known (one small indexed read in budget).
pub(crate) fn play_series(ctx: &mut UiCtx<'_>, id: crate::model::SeriesId, name: &str) {
    let db = ctx.db;
    match ctx.budget.run(|| crate::store::vod::next_episode(db, id)).and_then(|r| r.ok()).flatten() {
        Some(e) => {
            let item = crate::model::ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() };
            let title = format!("{name} — S{:02}E{:02}", e.season, e.episode);
            super::movie_detail::resume_or_play(ctx, crate::model::MediaRef::Episode(e.id), &item, title, crate::model::PlayOrigin::Fullscreen);
        }
        None => ctx.push(Route::SeriesDetail(id)),
    }
}

/// The shared Movies / Series browser.
#[derive(Default)]
pub(super) struct VodBrowser {
    movies: bool,
    pane: Pane,
    cats: ListState,
    /// Displayed categories (after the categories filter).
    cat_rows: Vec<Cat>,
    open: Cat,
    grid: GridState,
    /// Items of the open category (indices into the kind's list), sorted.
    items: Vec<u32>,
    /// `items` after the grid filter.
    shown: Vec<u32>,
    /// What `items` was computed from: (category, sort, library build).
    items_key: Option<(Cat, SortMode, Timestamp)>,
    progress: ProgressInfo,
    progress_loader: Option<Loader<ProgressInfo>>,
    /// (source, key) → index, per library build.
    index: HashMap<(SourceId, ItemKey), u32>,
    index_built: Timestamp,
    /// A locked category waiting for the PIN.
    pending_cat: Option<Cat>,
    playing: kit::PlayingVod,
}

impl VodBrowser {
    fn kind_label(&self) -> &'static str { if self.movies { "movies" } else { "series" } }

    pub(super) fn key_contexts(&self) -> Vec<KeyContext> {
        match self.pane {
            Pane::Categories => vec![self.cats.key_context()],
            Pane::Grid => vec![self.grid.key_context()],
        }
    }

    pub(super) fn enter(&mut self, ctx: &mut UiCtx<'_>, movies: bool, group: Option<GroupId>) {
        self.movies = movies;
        if let Some(g) = group {
            self.open = Cat::Group(g);
            self.pane = Pane::Grid;
            self.items_key = None;
        }
        self.progress_loader = Some(Loader::start(ctx, move |c| load_progress(c, movies)));
    }

    pub(super) fn event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        if let AppEvent::Data(DataScope::Progress | DataScope::All) = ev {
            let movies = self.movies;
            self.progress_loader = Some(Loader::start(ctx, move |c| load_progress(c, movies)));
            self.items_key = None;
        }
    }

    fn all<'l>(&self, ctx: &'l UiCtx<'_>) -> &'l [VodSummary] { if self.movies { &ctx.lib.vod.movies } else { &ctx.lib.vod.series } }

    fn groups<'l>(&self, ctx: &'l UiCtx<'_>) -> Vec<&'l crate::model::Group> {
        let gs = if self.movies { &ctx.lib.vod.movie_groups } else { &ctx.lib.vod.series_groups };
        let hide_empty = ctx.config.interface.hide_empty_groups;
        gs.iter().filter(|g| !g.hidden && !(hide_empty && g.item_count == 0)).collect()
    }

    fn sort(&self, ctx: &UiCtx<'_>) -> SortMode { if self.movies { ctx.config.interface.movie_sort } else { ctx.config.interface.series_sort } }

    fn cat_name(&self, ctx: &UiCtx<'_>, c: Cat) -> String {
        match c {
            Cat::Group(g) => self.groups(ctx).iter().find(|x| x.id == g).map(|x| x.display_name().to_owned()).unwrap_or_else(|| "Category".into()),
            c => self.virtual_name(c).to_owned(),
        }
    }

    /// Names of the virtual categories.
    fn virtual_name(&self, c: Cat) -> &'static str {
        match c {
            Cat::All if self.movies => "All movies",
            Cat::All => "All series",
            Cat::Continue => "Continue watching",
            _ => "Favourites",
        }
    }

    fn cat_locked(&self, ctx: &UiCtx<'_>, c: Cat) -> bool {
        match c {
            Cat::Group(g) => self.groups(ctx).iter().any(|x| x.id == g && x.locked) && !kit::unlocked(ctx),
            _ => false,
        }
    }

    fn refilter_cats(&mut self, ctx: &UiCtx<'_>) {
        let groups = self.groups(ctx);
        let mut all = vec![Cat::All, Cat::Continue, Cat::Favourites];
        all.extend(groups.iter().map(|g| Cat::Group(g.id)));
        let mut names: Vec<String> = all[..3].iter().map(|c| self.virtual_name(*c).to_owned()).collect();
        names.extend(groups.iter().map(|g| g.display_name().to_owned()));
        self.cat_rows = self.cats.filter.apply(names.iter().map(String::as_str)).into_iter().map(|i| all[i as usize]).collect();
        self.cats.set_len(self.cat_rows.len());
    }

    /// Recompute the open category's items when its inputs changed.
    fn refresh_items(&mut self, ctx: &UiCtx<'_>) {
        if self.index_built != ctx.lib.built_at {
            self.index_built = ctx.lib.built_at;
            self.index = self.all(ctx).iter().enumerate().map(|(i, v)| ((v.source_id, v.key.clone()), i as u32)).collect();
            self.items_key = None;
        }
        let key = (self.open, self.sort(ctx), ctx.lib.built_at);
        // favourites change without a library rebuild: recompute that category every frame
        if self.items_key == Some(key) && self.open != Cat::Favourites {
            return;
        }
        self.items_key = Some(key);
        let movies = self.movies;
        let items: Vec<u32> = match self.open {
            Cat::All => ctx.lib.vod.all_sorted(movies, key.1),
            Cat::Group(g) => ctx.lib.vod.group_items(g, key.1),
            Cat::Continue => self.progress.order.iter().filter_map(|k| self.index.get(k).copied()).collect(),
            Cat::Favourites => {
                let kind = if movies { ItemKind::Movie } else { ItemKind::Series };
                let list = ctx.favs.default_list();
                list.map(|l| ctx.favs.items(l).iter().filter(|r| r.kind == kind).filter_map(|r| self.index.get(&(r.source, r.key.clone())).copied()).collect()).unwrap_or_default()
            }
        };
        if items != self.items {
            self.items = items;
            self.apply_filter(ctx);
        }
    }

    fn apply_filter(&mut self, ctx: &UiCtx<'_>) {
        let all = self.all(ctx);
        let names: Vec<&str> = self.items.iter().map(|i| all.get(*i as usize).map(|v| &*v.name).unwrap_or("")).collect();
        self.shown = self.grid.filter.apply(names.into_iter()).into_iter().map(|i| self.items[i as usize]).collect();
        self.grid.set_len(self.shown.len());
    }

    fn open_cat(&mut self, ctx: &mut UiCtx<'_>, c: Cat, focus_grid: bool) {
        if self.cat_locked(ctx, c) {
            self.pending_cat = Some(c);
            kit::ask_pin(ctx, Action::Activate, "This category is locked");
            return;
        }
        if self.open != c {
            self.open = c;
            self.grid.filter.clear();
            self.grid.select(0);
            self.items_key = None;
            // actions later in this frame act on the new category's items
            self.refresh_items(ctx);
        }
        if focus_grid {
            self.pane = Pane::Grid;
        }
    }

    fn current(&self, ctx: &UiCtx<'_>) -> Option<VodSummary> {
        self.shown.get(self.grid.cursor).and_then(|i| self.all(ctx).get(*i as usize)).cloned()
    }

    fn open_item(&mut self, ctx: &mut UiCtx<'_>, v: &VodSummary, play: bool) {
        if v.locked && !kit::unlocked(ctx) {
            kit::ask_pin(ctx, if play { Action::ActivateAlt } else { Action::Activate }, "This title is locked");
            return;
        }
        match (v.kind, play) {
            (VodKind::Movie(id), false) => ctx.push(Route::MovieDetail(id)),
            (VodKind::Movie(id), true) => super::movie_detail::play_movie(ctx, id),
            (VodKind::Series(id), false) => ctx.push(Route::SeriesDetail(id)),
            (VodKind::Series(id), true) => play_series(ctx, id, &v.name),
        }
    }

    fn sort_menu(&self, ctx: &mut UiCtx<'_>) {
        let current = self.sort(ctx);
        let items = SORTS
            .iter()
            .enumerate()
            .map(|(i, (m, l))| MenuItem { icon: if *m == current { glyphs::CHECK } else { "" }, label: (*l).into(), action: Action::Digit(i as u8), enabled: true, shortcut: None })
            .collect();
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Sort by".into(), items }));
    }

    fn item_menu(&self, ctx: &mut UiCtx<'_>, v: &VodSummary) {
        let fav = ctx.favs.is_fav(&super::home::vod_ref(v));
        let items = vec![
            MenuItem { icon: glyphs::INFO, label: "Details".into(), action: Action::Digit(MENU_OPEN), enabled: true, shortcut: Some("Enter".into()) },
            MenuItem { icon: glyphs::PLAY, label: "Play".into(), action: Action::Digit(MENU_PLAY), enabled: true, shortcut: Some("Shift+Enter".into()) },
            MenuItem {
                icon: glyphs::FAVOURITE,
                label: if fav { "Remove from Favourites".into() } else { "Add to Favourites".into() },
                action: Action::ToggleFavourite,
                enabled: true,
                shortcut: Some("Ctrl+F".into()),
            },
            MenuItem { icon: glyphs::FAV_LIST, label: "Favourites lists…".into(), action: Action::AddToList, enabled: true, shortcut: Some("Ctrl+Shift+F".into()) },
            MenuItem { icon: glyphs::LOCK, label: "Lock / unlock".into(), action: Action::ToggleLock, enabled: true, shortcut: Some("Ctrl+L".into()) },
            MenuItem { icon: glyphs::SORT, label: "Sort & view options…".into(), action: Action::SortMenu, enabled: true, shortcut: Some("Ctrl+S".into()) },
        ];
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: v.name.to_string(), items }));
    }

    /// Item actions shared by the grid keys and the context menu.
    fn item_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let Some(v) = self.current(ctx) else { return matches!(action, Action::ToggleFavourite | Action::AddToList | Action::ToggleLock) };
        let item = super::home::vod_ref(&v);
        match action {
            Action::ToggleFavourite => kit::toggle_favourite(ctx, &item),
            Action::AddToList => ctx.open(OverlayRequest::ListPicker(crate::ui::overlays::ListPickerSpec { item, title: v.name.to_string() })),
            Action::ToggleLock => kit::toggle_lock(ctx, &item, v.locked),
            _ => return false,
        }
        true
    }

    pub(super) fn action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if action == Action::SortMenu {
            self.sort_menu(ctx);
            return true;
        }
        if matches!(action, Action::FocusNext | Action::FocusPrev) {
            self.pane = if self.pane == Pane::Grid { Pane::Categories } else { Pane::Grid };
            return true;
        }
        match self.pane {
            Pane::Categories => match self.cats.handle(action) {
                ListOutcome::Moved => {
                    if let Some(c) = self.cat_rows.get(self.cats.cursor).copied()
                        && !self.cat_locked(ctx, c)
                    {
                        self.open_cat(ctx, c, false);
                    }
                    true
                }
                ListOutcome::FilterChanged => {
                    self.refilter_cats(ctx);
                    true
                }
                ListOutcome::Activated(i) | ListOutcome::ActivatedAlt(i) => {
                    if let Some(c) = self.cat_rows.get(i).copied() {
                        self.open_cat(ctx, c, true);
                    }
                    true
                }
                ListOutcome::Parent | ListOutcome::ContextMenu(_) => true,
                ListOutcome::None => matches!(action, Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_)),
            },
            Pane::Grid => match self.grid.handle(action) {
                ListOutcome::Moved => true,
                ListOutcome::FilterChanged => {
                    self.apply_filter(ctx);
                    true
                }
                ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                    if let Some(v) = self.current(ctx) {
                        self.open_item(ctx, &v, action == Action::ActivateAlt);
                    }
                    true
                }
                ListOutcome::ContextMenu(_) => {
                    if let Some(v) = self.current(ctx) {
                        self.item_menu(ctx, &v);
                    }
                    true
                }
                ListOutcome::Parent => {
                    self.pane = Pane::Categories;
                    true
                }
                ListOutcome::None => match action {
                    Action::Up | Action::Down | Action::Left | Action::Right | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => true,
                    Action::MarkWatched => {
                        if let Some(v) = self.current(ctx).filter(|v| matches!(v.kind, VodKind::Movie(_))) {
                            let item = super::home::vod_ref(&v);
                            let (now, title) = (ctx.now, v.name.to_string());
                            let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| crate::store::user::mark_watched(c, &[(item, title)], true, now));
                            ctx.toast(crate::msg::Toast::info("Marked as watched"));
                        }
                        true
                    }
                    a => self.item_action(ctx, a),
                },
            },
        }
    }

    pub(super) fn overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Chosen(Action::Digit(d)) if (d as usize) < SORTS.len() => {
                let mode = SORTS[d as usize].0;
                let movies = self.movies;
                ctx.edit_config(move |c| if movies { c.interface.movie_sort = mode } else { c.interface.series_sort = mode });
                self.items_key = None;
            }
            OverlayResult::Chosen(Action::Digit(MENU_OPEN)) | OverlayResult::Chosen(Action::Digit(MENU_PLAY)) => {
                let play = result == OverlayResult::Chosen(Action::Digit(MENU_PLAY));
                if let Some(v) = self.current(ctx) {
                    self.open_item(ctx, &v, play);
                }
            }
            OverlayResult::Chosen(Action::SortMenu) => self.sort_menu(ctx),
            OverlayResult::Chosen(a) => {
                self.item_action(ctx, a);
            }
            OverlayResult::Unlocked(a) => {
                if let Some(c) = self.pending_cat.take() {
                    self.open_cat(ctx, c, true);
                } else if let Some(v) = self.current(ctx) {
                    match a {
                        Action::Activate => self.open_item(ctx, &v, false),
                        Action::ActivateAlt => self.open_item(ctx, &v, true),
                        Action::ToggleLock => {
                            self.item_action(ctx, Action::ToggleLock);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.progress_loader) {
            Some(Ok(p)) => {
                self.progress = p;
                if self.open == Cat::Continue {
                    self.items_key = None;
                }
            }
            Some(Err(e)) => tracing::warn!("{} progress: {e}", self.kind_label()),
            None => {}
        }
        let theme = ctx.theme;
        // no titles and no categories: nothing imported (hidden titles keep their categories)
        let groups = if self.movies { &ctx.lib.vod.movie_groups } else { &ctx.lib.vod.series_groups };
        if ctx.lib.built_at != 0 && self.all(ctx).is_empty() && groups.iter().all(|g| g.item_count == 0) {
            let title = format!("Your playlists have no {}", self.kind_label());
            empty_state::empty_state(ui, theme, if self.movies { glyphs::MOVIES } else { glyphs::SERIES }, &title, Some("Xtream logins usually include movies and series; M3U playlists rarely do."), None);
            return;
        }
        if self.cat_rows.is_empty() || self.cat_rows.len() != self.cats.len || ctx.lib.built_at != self.index_built {
            self.refilter_cats(ctx);
        }
        self.refresh_items(ctx);
        if !self.cat_rows.contains(&self.open) && !self.cats.filter.is_active() {
            self.open = Cat::All;
            self.items_key = None;
            self.refresh_items(ctx);
        }
        let compact = ctx.layout == LayoutClass::Compact;
        let rem = theme.rem();
        let cats_w = if compact { ui.available_width() } else { (if ctx.layout == LayoutClass::Wide { 240.0 } else { 200.0 } * rem).round() };
        let full = ui.available_rect_before_wrap();
        let show_cats = !compact || self.pane == Pane::Categories;
        let show_grid = !compact || self.pane == Pane::Grid;
        let gap = theme.space(SpaceToken::PanelPadding);
        let mut cats_rect = full;
        cats_rect.set_width(cats_w.min(full.width()));
        let grid_rect = if compact { full } else { egui::Rect::from_min_max(egui::pos2(cats_rect.right() + gap, full.top()), full.max) };

        if show_cats {
            let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("vod-cats", self.movies)).max_rect(cats_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            self.cats_pane(&mut child, ctx);
            if !compact {
                crate::ui::widgets::section::pane_divider(ui.painter(), cats_rect.right() + gap / 2.0, full.y_range(), theme);
            }
        }
        if show_grid {
            let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("vod-grid", self.movies)).max_rect(grid_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            self.grid_pane(&mut child, ctx);
        }
        ui.allocate_rect(full, egui::Sense::hover());
    }

    fn cats_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let h = search_header(ui, theme, self.cats.filter.query(), "Categories", Some(glyphs::SEARCH));
        if h.clicked() {
            self.pane = Pane::Categories;
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if self.cat_rows.is_empty() {
            let q = self.cats.filter.query().to_owned();
            empty_state::empty_state(ui, theme, glyphs::SEARCH, &format!("No matches for “{q}”"), None, None);
            return;
        }
        let row_h = rows::row_height(theme, crate::config::Density::Comfortable, false);
        let focused = self.pane == Pane::Categories;
        let open = self.open;
        let rows_snapshot = self.cat_rows.clone();
        // one map per frame; rows are drawn (and named) only when visible
        let groups: HashMap<GroupId, &crate::model::Group> = self.groups(ctx).into_iter().map(|g| (g.id, g)).collect();
        let virtual_names = [self.virtual_name(Cat::All), self.virtual_name(Cat::Continue), self.virtual_name(Cat::Favourites)];
        let total = self.all(ctx).len();
        let outcome = virtual_list(ui, theme, ("vod-cats", self.movies), &mut self.cats, row_h, |ui, rc| {
            let cat = rows_snapshot[rc.index];
            let state = RowState::from_flags(focused && rc.state.is_cursor(), cat == open);
            let group = match cat {
                Cat::Group(g) => groups.get(&g).copied(),
                _ => None,
            };
            match group {
                Some(g) => rows::group_row(ui, theme, g, state),
                None => {
                    let name = match cat {
                        Cat::All => virtual_names[0],
                        Cat::Continue => virtual_names[1],
                        _ => virtual_names[2],
                    };
                    let (icon, detail) = match cat {
                        Cat::All => (glyphs::FILMSTRIP, Some(total.to_string())),
                        Cat::Continue => (glyphs::HISTORY, None),
                        _ => (glyphs::FAVOURITE, None),
                    };
                    let r = rows::menu_row(ui, theme, Some(icon), name, None, false, state);
                    if let Some(d) = detail {
                        ui.painter().text(
                            egui::pos2(r.rect.right() - theme.space(SpaceToken::Lg), r.rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                            d,
                            egui::FontId::proportional(theme.font(TypeToken::Body)),
                            theme.tokens.text_secondary,
                        );
                    }
                    if cat == Cat::Favourites {
                        // provider categories follow (DESIGN §13.5 separator)
                        ui.painter().hline(r.rect.x_range(), r.rect.bottom() - 0.5, egui::Stroke::new(1.0, theme.tokens.group_divider));
                    }
                    r
                }
            }
        });
        match outcome {
            ListOutcome::Moved => {
                self.pane = Pane::Categories;
                if let Some(c) = self.cat_rows.get(self.cats.cursor).copied()
                    && !self.cat_locked(ctx, c)
                {
                    self.open_cat(ctx, c, false);
                }
            }
            ListOutcome::Activated(i) => {
                if let Some(c) = self.cat_rows.get(i).copied() {
                    self.open_cat(ctx, c, true);
                }
            }
            _ => {}
        }
    }

    fn grid_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let name = self.cat_name(ctx, self.open);
        let sort = self.sort(ctx);
        let mut sort_clicked = false;
        ui.horizontal(|ui| {
            let w = ui.available_width();
            let sort_w = (230.0 * theme.rem()).min(w * 0.45);
            ui.allocate_ui(egui::vec2(w - sort_w, ui.available_height()), |ui| {
                let h = search_header(ui, theme, self.grid.filter.query(), &format!("{name} (type to filter)"), None);
                if h.clicked() {
                    self.pane = Pane::Grid;
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = format!("Sort: {} ▾ (Ctrl+S)", sort_label(sort));
                sort_clicked = button(ui, theme, Some(glyphs::SORT), &label, ButtonStyle::default()).clicked();
            });
        });
        if sort_clicked {
            self.sort_menu(ctx);
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if self.cat_locked(ctx, self.open) {
            if empty_state::empty_state(ui, theme, glyphs::LOCK, "This category is locked", Some("Enter the PIN to open it."), Some("Unlock")).is_some_and(|r| r.clicked()) {
                let c = self.open;
                self.open_cat(ctx, c, true);
            }
            return;
        }
        if ctx.lib.built_at == 0 {
            loading_grid(ui, theme);
            return;
        }
        if self.shown.is_empty() {
            let q = self.grid.filter.query().to_owned();
            // a provider category whose every title is hidden (DESIGN §13.12)
            let all_hidden = q.is_empty() && matches!(self.open, Cat::Group(g) if self.groups(ctx).iter().any(|x| x.id == g && x.item_count > 0));
            let (glyph, title, detail, action) = if !q.is_empty() {
                (glyphs::SEARCH, format!("No matches for “{q}”"), None, None)
            } else if all_hidden {
                (glyphs::HIDDEN, "Everything here is hidden".to_owned(), Some("Hidden titles are listed in Settings ▸ Hidden items."), Some("Show hidden"))
            } else {
                match self.open {
                    Cat::Continue => (glyphs::HISTORY, "Nothing to continue".to_owned(), Some("Titles you start watching show up here."), None),
                    Cat::Favourites => (glyphs::FAVOURITE, "No favourites yet".to_owned(), Some("Press Ctrl+F on any title to add it here"), None),
                    _ => (glyphs::FILMSTRIP, "This category is empty".to_owned(), None, None),
                }
            };
            if empty_state::empty_state(ui, theme, glyph, &title, detail, action).is_some_and(|r| r.clicked()) {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Hidden));
            }
            return;
        }
        let playing = self.playing.get(ctx);
        let progress = &self.progress.fraction;
        let movies = self.movies;
        let all: &[VodSummary] = if movies { &ctx.lib.vod.movies } else { &ctx.lib.vod.series };
        let outcome = poster::poster_grid_with(ui, theme, ctx.images, &mut self.grid, all, &self.shown, |v| TileDecor {
            progress: progress.get(&(v.source_id, v.key.clone())).copied(),
            playing: playing == Some(v.kind),
            watched: false,
        });
        match outcome {
            ListOutcome::Moved => self.pane = Pane::Grid,
            ListOutcome::Activated(_) => {
                self.pane = Pane::Grid;
                if let Some(v) = self.current(ctx) {
                    self.open_item(ctx, &v, false);
                }
            }
            ListOutcome::ContextMenu(_) => {
                if let Some(v) = self.current(ctx) {
                    self.item_menu(ctx, &v);
                }
            }
            _ => {}
        }
    }
}

/// Skeleton posters while the first library loads (fg @ 0.04 blocks, no spinner; DESIGN §13.12).
fn loading_grid(ui: &mut egui::Ui, theme: &crate::theme::Theme) {
    let (cols, card_w, gap) = poster::grid_metrics(theme, ui.available_width());
    let card_h = poster::card_height(theme, card_w);
    let top = ui.cursor().top();
    let left = ui.cursor().left();
    let rows = ((ui.available_height() + gap) / (card_h + gap)).ceil().clamp(1.0, 4.0) as usize;
    for r in 0..rows {
        for c in 0..cols {
            let min = egui::pos2(left + c as f32 * (card_w + gap), top + r as f32 * (card_h + gap));
            let poster = egui::Rect::from_min_size(min, egui::vec2(card_w, card_w * 1.5));
            ui.painter().rect_filled(poster, theme.tokens.rounding, theme.tokens.fill_field);
            let title = egui::Rect::from_min_size(egui::pos2(min.x, poster.bottom() + theme.space(SpaceToken::Md)), egui::vec2(card_w * 0.7, theme.font(TypeToken::Body)));
            ui.painter().rect_filled(title, theme.tokens.rounding, theme.tokens.fill_field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::NavRequest;
    use egui_kittest::kittest::Queryable;
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed};

    #[test]
    fn grid_navigation_type_to_filter_sort_menu_and_categories() {
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 30)
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid]);
        assert!(has_label(&h, "Movie 29"), "recently added first");
        assert!(has_label(&h, "Sort: Recently added ▾ (Ctrl+S)"));
        let cols = h.state().s.browser.grid.columns;
        assert!((6..=9).contains(&cols), "wide grid columns: {cols}");
        // ↓ moves a row, → one card; Enter opens the page
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(ids[29 - cols - 1]))]);
        // typing filters (launcher idiom)
        h.state_mut().act(Action::FilterInput('2'));
        h.state_mut().act(Action::FilterInput('7'));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGridFiltering]);
        h.run_steps(1);
        assert_eq!(h.state().s.browser.shown.first().copied(), Some(27));
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(ids[27]))]);
        h.state_mut().act(Action::FilterClear);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid]);
        // Ctrl+S: the sort menu; A–Z is saved as the movie sort
        h.state_mut().act(Action::SortMenu);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.title == "Sort by" && m.items.len() == 6), "{nav:?}");
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(1)));
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.movie_sort, SortMode::NameAsc);
        h.run_steps(1);
        assert_eq!(h.state().s.browser.shown.first().copied(), Some(0), "A–Z: Movie 0 first");
        // ← on the first column: the categories pane (typing filters categories there)
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Left);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::List]);
        h.state_mut().act(Action::Down); // Continue watching opens as the cursor moves
        h.run_steps(1);
        assert!(has_label(&h, "Nothing to continue"));
        h.state_mut().act(Action::Down); // Favourites
        h.run_steps(1);
        assert!(has_label(&h, "No favourites yet"));
        h.state_mut().act(Action::Down); // the provider category
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid], "Enter moves to the grid");
        // Ctrl+F on a poster puts it in Favourites
        h.state_mut().act(Action::ToggleFavourite);
        h.state_mut().act(Action::Parent);
        h.state_mut().act(Action::Up);
        h.run_steps(1);
        assert_eq!(h.state().s.browser.shown.len(), 1, "the favourite shows in Favourites");
        // Shift+Enter plays (no progress: straight to the player)
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::ActivateAlt);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
    }

    #[test]
    fn series_grid_plays_the_next_episode_and_no_vod_says_so() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::series(c, s, 3, 2, 3)
        });
        let mut h = harness(t, one, crate::ui::screens::series::SeriesScreen::default(), egui::vec2(900.0, 800.0));
        h.state_mut().enter(Route::Series { group: None });
        h.run_steps(2);
        assert!(has_label(&h, "Series 2"));
        assert!(has_label(&h, "All series"));
        h.state_mut().act(Action::ActivateAlt);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)], "Shift+Enter plays the episode the page would open at");
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Push(Route::SeriesDetail(_))]));
        // a library without VOD
        let app = h.state_mut();
        app.t.set_library(crate::store::library::Library { built_at: 1, ..Default::default() });
        h.run_steps(2);
        assert!(has_label(&h, "Your playlists have no series"));
    }

    /// DESIGN §13.12: skeleton posters while the first library loads; a category whose every
    /// title is hidden says so and links to Settings ▸ Hidden items.
    #[test]
    fn loading_skeleton_and_a_fully_hidden_category() {
        let (mut t, one) = rig();
        let group = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 3)?;
            for i in 0..3 {
                let item = crate::model::ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) };
                crate::store::user::set_item_override(c, &item, &crate::model::ItemOverride { hidden: true, ..Default::default() })?;
            }
            Ok(GroupId(c.query_row("SELECT id FROM grp WHERE key = 'movie:movies'", [], |r| r.get(0))?))
        });
        testkit::reload_library(&mut t);
        let lib = t.lib.clone();
        t.set_library(crate::store::library::Library::default());
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Movies { group: Some(group) });
        h.run_steps(2);
        assert!(!has_label(&h, "This category is empty"), "loading: skeletons, no empty state");
        let app = h.state_mut();
        app.t.lib = lib;
        app.enter(Route::Movies { group: Some(group) });
        h.run_steps(2);
        assert!(has_label(&h, "Everything here is hidden"));
        h.get_by_label("Show hidden").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Settings(crate::ui::SettingsPage::Hidden))]);
    }
}
