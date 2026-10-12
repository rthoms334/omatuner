//! Movies (DESIGN §13.5): categories pane | poster grid (sort: recently added, A–Z, year,
//! rating; filter-as-you-type). Enter → MovieDetail. Owner: WP-12.
//!
//! [`VodBrowser`] is shared with the Series screen (identical apart from the item kind):
//! * Categories: All, Continue watching, Favourites, then the provider categories (renamed /
//!   reordered / hidden per overrides; locked ones show 󰌾 and need the PIN). Moving the
//!   cursor opens the category (locked ones only on Enter + PIN). Key context `[List]` /
//!   `[ListFiltering]` (typing filters the categories).
//!   Category shortcuts: Ctrl+L lock (PIN), F2 rename, Ctrl+H hide, Alt+↑/↓ reorder within
//!   its playlist, Shift+F10 menu.
//! * Grid: `[PosterGrid]` / `[PosterGridFiltering]` — typing filters the posters (launcher
//!   idiom), Enter opens details, Shift+Enter plays/resumes fullscreen, Ctrl+S opens the sort
//!   menu (the choice is saved as `interface.movie_sort` / `series_sort`), ← on the first
//!   column (also while filtering — the filter is kept — or on an empty grid) / Backspace
//!   returns to the categories (DESIGN §15.1 spatial panes), Tab switches panes. Item
//!   shortcuts: Ctrl+F / Ctrl+Shift+F favourites, Ctrl+W watched, Ctrl+L lock, Ctrl+H hide, F2
//!   rename, Ctrl+I details, Delete removes from Favourites / Continue watching, Alt+↑/↓ moves
//!   the title in Custom order (`item_override.user_order`, per title — DESIGN §8.3),
//!   switching a sorted grid to Custom order first (DESIGN §13.0); the cursor follows the
//!   title.
//! * Only the focused pane shows the cursor; the other shows its current row "selected".
//! * Wide: categories | grid. Medium (a tiled half): the categories are a drawer (← on the
//!   first column, Backspace, or the "{category} ▾" header button) so the grid keeps 4–5
//!   columns; Esc closes the drawer. Compact: one pane at a time (categories → grid; Esc or
//!   the header button in the grid go back to the categories). Below Wide one click on a
//!   category opens it (the drawer closes / the grid shows).
//! * Progress bars and Continue watching come from one background read of the unfinished
//!   progress rows, refreshed on `DataScope::Progress`.
//! * Several playlists with movies (series) (DESIGN §29): the scope bar on top (tabs at Wide,
//!   the `● TV 2 ▾` button otherwise; Alt+P picker, Ctrl+PgDn/PgUp) narrows the categories, All,
//!   Continue watching and Favourites to one playlist; the grid header leads with the
//!   breadcrumb "TV 2 › Action", and a 2 px stripe in the playlist's colour runs under the bar.
//!   In All, categories and cards carry the playlist's name in its colour.
//! * Copies of one title (DESIGN §29.14): in All (with Settings ▸ Interface ▸ "Same title in
//!   several playlists" on Combine) each stack is one **stack card** (`ui::stacks::collapse`):
//!   Enter opens its page (Copies section there), Shift+Enter plays the copy that plays first,
//!   Space or a click on its copy-count badge fans the copies out right after it (Space on any
//!   of them, or Esc, folds it), Ctrl+O "Play from…" picks a copy. A stack card's ★ / watched /
//!   progress / lock follow `stacks::stack_marks`; Ctrl+F adds the copy that would play or
//!   takes every copy out (Undo), Delete in Continue watching forgets every copy (Undo). A
//!   fanned-out copy's menu has "Not the same title" (Undo). Inside one playlist nothing
//!   stacks; a title that is also elsewhere shows "+1".

use std::collections::{HashMap, HashSet};

use eframe::egui;

use super::group_marks::{self, GroupMarks};
use super::home::kit::{self, Loader};
use crate::keymap::{Action, KeyContext};
use crate::model::{GroupId, ItemKey, ItemKind, SortMode, SourceId, VodKind, VodSummary};
use crate::msg::{AppEvent, DataScope};
use crate::store::stacks::StackId;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::scope::{self, Scope, ScopeFilter, ScopeKind};
use crate::ui::stacks as st;
use crate::ui::widgets::button::{ButtonStyle, LABEL_MAX_CHARS, button, icon_button, short_label};
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::poster::{self, Elsewhere, GridState, StackDecor, TileDecor};
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, scope_bar};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct MoviesScreen {
    browser: VodBrowser,
}

impl Screen for MoviesScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Movies
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        self.browser.key_contexts()
    }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let group = match route {
            Route::Movies { group } => *group,
            _ => None,
        };
        self.browser.enter(ctx, true, group);
    }
    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        self.browser.event(ctx, ev)
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.browser.action(ctx, action)
    }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        self.browser.overlay_result(ctx, result)
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.browser.ui(ui, ctx)
    }
    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        self.browser.hint_extras()
    }
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
/// Category context-menu "Open".
const MENU_OPEN_CAT: u8 = 22;
/// The categories menu's "Countries & languages…".
const MENU_COUNTRIES: u8 = 23;
/// The category menu's "Mark as explicit" / "Not explicit" (DESIGN §20.3).
const MENU_EXPLICIT: u8 = 24;
/// A fanned-out copy's "Not the same title".
const MENU_SPLIT: u8 = 25;
/// "Same title as…" (DESIGN §29.16).
const MENU_SAME_TITLE: u8 = 26;
/// "Play from…": copy `k` of the menu is `MENU_COPY + k`.
const MENU_COPY: u8 = 40;
/// Copies listed in "Play from…".
const MAX_COPIES: usize = 200;

/// Identity of the library snapshot (indices into it are only valid for that snapshot; two
/// rebuilds can share a `built_at` second).
fn lib_id(ctx: &UiCtx<'_>) -> u64 {
    ctx.lib.generation
}

fn sort_label(s: SortMode) -> &'static str {
    SORTS.iter().find(|(m, _)| *m == s).map(|(_, l)| *l).unwrap_or("Custom order")
}

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
    /// What `items` was computed from: (category, sort, library build, playlist scope,
    /// copies combined, stacks fanned out).
    items_key: Option<(Cat, SortMode, u64, ScopeFilter, bool, Vec<StackId>)>,
    /// `items` combines the copies of a title into stack cards (All, Combine).
    stacked: bool,
    /// Stacks fanned out in the grid (ids of the library `index_built`).
    open_stacks: Vec<StackId>,
    /// The cards of the fanned-out stacks (their other copies follow them).
    fan_heads: HashSet<u32>,
    /// Language / version of the copies shown fanned out ("EN · 4K"), by list index.
    copy_labels: HashMap<u32, String>,
    /// How recently each title was played (0 = latest), by list index: the copy a stack card
    /// shows progress of and that plays first.
    played_rank: HashMap<u32, usize>,
    /// The copies "Play from…" listed, and the library they index.
    play_from: (u64, Vec<u32>),
    /// A locked copy picked in "Play from…" waiting for the PIN.
    pending_copy: Option<u32>,
    /// The focused card heads a stack / has other copies (the hint line, last frame).
    hint: (bool, bool),
    /// The playlist scope the categories were listed for.
    cats_scope: Option<ScopeFilter>,
    progress: ProgressInfo,
    progress_loader: Option<Loader<ProgressInfo>>,
    /// Bumped whenever `progress` is replaced.
    progress_gen: u64,
    /// Watch progress by index into the kind's list (rebuilt with `progress` / `index`).
    progress_idx: HashMap<u32, f32>,
    /// (source, key) → index of the titles with progress or in the default favourites list:
    /// all this screen looks up by key (an index of every title cloned every key: ~30 MiB for
    /// 300k movies).
    index: HashMap<(SourceId, ItemKey), u32>,
    /// What `index` was resolved for: (VOD lists, `progress_gen`, favourites fingerprint). A
    /// zap's library copy shares the lists: nothing to redo.
    index_for: Option<(crate::store::library::VodIdentity, u64, u64)>,
    /// Keys looked for in the last scan of these lists that they don't hold (progress of a
    /// title the playlist dropped): a progress save that adds no new key rescans nothing.
    index_absent: HashSet<(SourceId, ItemKey)>,
    /// Full scans of the lists for `index` (tests).
    index_scans: u32,
    /// The library build the screen's indices belong to.
    index_built: u64,
    /// A locked category waiting for the PIN (cleared by any other open / item action).
    pending_cat: Option<Cat>,
    /// The sort just chosen, until the config edit lands at the end of the frame (the items
    /// computed in between must already use it).
    sort_pending: Option<SortMode>,
    /// Titles moved with Alt+↑/↓ that the library has not caught up with yet.
    moved: Option<Moved>,
    playing: kit::PlayingVod,
    /// Categories marked for a bulk hide (DESIGN §28.7).
    marks: GroupMarks,
    /// Short tag codes of the categories of playlists with several tags, per library build.
    tags: (u64, HashMap<GroupId, String>),
    /// The categories the filter shows whose names have the words typed: what "Hide these N
    /// categories" hides.
    matching: Vec<GroupId>,
    /// When a click on a category last opened it below Wide (egui time): the grid then takes
    /// its place under the pointer.
    cat_clicked_at: Option<f64>,
}

/// One playlist's titles in the custom order the user arranged with Alt+↑/↓ (the whole
/// playlist, not just the open category), shown over the library's order until a library
/// built after the writes has it. Repeated moves build on it, so a rebuild landing between
/// two key repeats neither shows nor stores an older order.
struct Moved {
    source: SourceId,
    order: Vec<ItemKey>,
    /// Position of each key in `order`.
    rank: HashMap<ItemKey, usize>,
    /// The `user_order` writes still running.
    writes: Vec<crate::store::Ticket<()>>,
    /// The library last compared with `order` (once the writes answered).
    checked: u64,
}

impl Moved {
    fn new(source: SourceId, order: Vec<ItemKey>) -> Self {
        let rank = order.iter().enumerate().map(|(i, k)| (k.clone(), i)).collect();
        Self { source, order, rank, writes: Vec::new(), checked: 0 }
    }

    /// Whether the library's order (`all` is in custom order) matches the user's.
    fn caught_up(&self, all: &[VodSummary]) -> bool {
        let lib: Vec<&ItemKey> = all.iter().filter(|v| v.source_id == self.source && self.rank.contains_key(&v.key)).map(|v| &v.key).collect();
        let present: std::collections::HashSet<&ItemKey> = lib.iter().copied().collect();
        lib.into_iter().eq(self.order.iter().filter(|k| present.contains(k)))
    }

    /// Put this playlist's titles in `items` (a custom-order list: each playlist's titles are
    /// contiguous) in the user's order, leaving the other playlists' slots alone.
    fn apply(&self, all: &[VodSummary], items: &mut [u32]) {
        let slots: Vec<usize> = (0..items.len()).filter(|i| all.get(items[*i] as usize).is_some_and(|v| v.source_id == self.source)).collect();
        let mut mine: Vec<u32> = slots.iter().map(|i| items[*i]).collect();
        mine.sort_by_key(|x| all.get(*x as usize).and_then(|v| self.rank.get(&v.key)).copied().unwrap_or(usize::MAX));
        for (slot, x) in slots.into_iter().zip(mine) {
            items[slot] = x;
        }
    }
}

impl VodBrowser {
    fn kind_label(&self) -> &'static str {
        if self.movies { "movies" } else { "series" }
    }

    fn scope_kind(&self) -> ScopeKind {
        if self.movies { ScopeKind::Movies } else { ScopeKind::Series }
    }

    /// The playlist scope this frame (DESIGN §29).
    fn scope<'l>(&self, ctx: &UiCtx<'l>) -> Scope<'l> {
        Scope::current(ctx, self.scope_kind())
    }

    pub(super) fn key_contexts(&self) -> Vec<KeyContext> {
        match self.pane {
            Pane::Categories => vec![self.cats.key_context()],
            Pane::Grid => vec![self.grid.key_context()],
        }
    }

    pub(super) fn enter(&mut self, ctx: &mut UiCtx<'_>, movies: bool, group: Option<GroupId>) {
        self.movies = movies;
        self.marks.set_noun(group_marks::Noun::Category);
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

    fn all<'l>(&self, ctx: &'l UiCtx<'_>) -> &'l [VodSummary] {
        if self.movies { &ctx.lib.vod.movies } else { &ctx.lib.vod.series }
    }

    /// All categories of the kind (hidden ones included).
    fn all_groups<'l>(&self, ctx: &'l UiCtx<'_>) -> &'l [crate::model::Group] {
        if self.movies { &ctx.lib.vod.movie_groups } else { &ctx.lib.vod.series_groups }
    }

    /// The categories shown in the pane (those of the playlists in scope).
    fn groups<'l>(&self, ctx: &'l UiCtx<'_>) -> Vec<&'l crate::model::Group> {
        let hide_empty = ctx.config.interface.hide_empty_groups;
        let scope = self.scope(ctx).filter();
        self.all_groups(ctx).iter().filter(|g| !g.hidden && !(hide_empty && g.item_count == 0) && scope.admits(g.source_id)).collect()
    }

    fn sort(&self, ctx: &UiCtx<'_>) -> SortMode {
        self.sort_pending.unwrap_or_else(|| self.config_sort(ctx))
    }

    fn config_sort(&self, ctx: &UiCtx<'_>) -> SortMode {
        if self.movies { ctx.config.interface.movie_sort } else { ctx.config.interface.series_sort }
    }

    /// Choose the grid's sort: saved in the config (applied at the end of the frame), used
    /// at once.
    fn set_sort(&mut self, ctx: &mut UiCtx<'_>, mode: SortMode) {
        let movies = self.movies;
        ctx.edit_config(move |c| if movies { c.interface.movie_sort = mode } else { c.interface.series_sort = mode });
        self.sort_pending = Some(mode);
        self.items_key = None;
    }

    /// Settle the Alt+↑/↓ moves: drop the user's order once a library has it, or when a
    /// write failed (the library's order is then the stored one).
    fn settle_moves(&mut self, ctx: &mut UiCtx<'_>) {
        let (all, lib) = (self.all(ctx), lib_id(ctx));
        let Some(m) = &mut self.moved else { return };
        let mut failed = false;
        m.writes.retain_mut(|t| match t.try_take() {
            None => true,
            Some(r) => {
                failed |= r.is_err();
                false
            }
        });
        let done = !failed && m.writes.is_empty() && m.checked != lib && {
            m.checked = lib;
            m.caught_up(all)
        };
        if failed || done {
            self.moved = None;
        }
        if failed {
            self.items_key = None;
            ctx.toast(crate::msg::Toast::error("Couldn't save the new order"));
        }
    }

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
        if self.tags.0 != lib_id(ctx) {
            let tagged = crate::ui::overlays::categories::multi_tag_sources(ctx.lib);
            let codes = self
                .all_groups(ctx)
                .iter()
                .filter(|g| tagged.contains(&g.source_id))
                .filter_map(|g| g.tag.as_ref().map(|t| (g.id, crate::sources::category_tags::tag_short(t))))
                .collect();
            self.tags = (lib_id(ctx), codes);
        }
        let groups = self.groups(ctx);
        let mut all = vec![Cat::All, Cat::Continue, Cat::Favourites];
        all.extend(groups.iter().map(|g| Cat::Group(g.id)));
        let mut names: Vec<String> = all[..3].iter().map(|c| self.virtual_name(*c).to_owned()).collect();
        // the tag code filters too ("uk" finds the UK categories smart names renamed), and in
        // All the playlist's name ("tv 2" finds TV 2's categories, like Live TV's groups)
        // (bulk marking matches the category's own words only, not the playlist's name)
        let sources = self.scope(ctx).marks(ctx);
        let words: Vec<String> = groups
            .iter()
            .map(|g| match self.tags.1.get(&g.id) {
                Some(code) => format!("{} {code}", g.display_name()),
                None => g.display_name().to_owned(),
            })
            .collect();
        names.extend(groups.iter().zip(&words).map(|(g, w)| match sources.of(g.source_id) {
            Some(src) => format!("{w} {} {}", src.name, src.code),
            None => w.clone(),
        }));
        let shown = self.cats.filter.apply(names.iter().map(String::as_str));
        let q = self.cats.filter.query();
        self.matching = shown
            .iter()
            .map(|&i| i as usize)
            .filter(|&i| i >= 3 && group_marks::word_match(q, &words[i - 3]))
            .filter_map(|i| match all[i] {
                Cat::Group(id) => Some(id),
                _ => None,
            })
            .collect();
        self.cat_rows = shown.into_iter().map(|i| all[i as usize]).collect();
        self.cats.set_len(self.cat_rows.len());
        let listed: std::collections::HashSet<GroupId> = groups.iter().map(|g| g.id).collect();
        self.marks.retain(|id| listed.contains(&id));
    }

    /// Progress by list index (posters look it up per visible card, without cloning keys).
    fn rebuild_progress_idx(&mut self) {
        self.progress_idx = self.progress.fraction.iter().filter_map(|(k, f)| self.index.get(k).map(|i| (*i, *f))).collect();
        self.played_rank = self.progress.order.iter().enumerate().filter_map(|(r, k)| self.index.get(k).map(|i| (*i, r))).collect();
    }

    /// Resolve the keys of the titles with progress and of the favourites to list indices,
    /// when the library, the progress or the favourites changed (one pass over the list; only
    /// the matches' keys are cloned).
    fn refresh_index(&mut self, ctx: &UiCtx<'_>) {
        let kind = if self.movies { ItemKind::Movie } else { ItemKind::Series };
        let favs: Vec<&crate::model::ItemRef> =
            ctx.favs.default_list().map(|l| ctx.favs.items(l).iter().filter(|r| r.kind == kind).collect()).unwrap_or_default();
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            favs.hash(&mut h);
            h.finish()
        };
        let want = (ctx.lib.vod_identity(), self.progress_gen, fingerprint);
        if self.index_for.as_ref() == Some(&want) {
            return;
        }
        let same_lists = self.index_for.as_ref().is_some_and(|f| f.0 == want.0);
        self.index_for = Some(want);
        let wanted: HashSet<(SourceId, &ItemKey)> =
            self.progress.order.iter().map(|(s, k)| (*s, k)).chain(favs.iter().map(|r| (r.source, &r.key))).collect();
        // the same lists and no key it hasn't looked for (a progress save of a known title, a
        // favourite taken out): the stale extra entries are harmless, lookups go by these keys
        let known = |k: &(SourceId, ItemKey)| self.index.contains_key(k) || self.index_absent.contains(k);
        if same_lists && wanted.iter().all(|(s, k)| known(&(*s, (*k).clone()))) {
            self.rebuild_progress_idx();
            return;
        }
        let index: HashMap<(SourceId, ItemKey), u32> = if wanted.is_empty() {
            HashMap::new()
        } else {
            self.index_scans += 1;
            let all = self.all(ctx);
            all.iter()
                .enumerate()
                .filter(|(_, v)| wanted.contains(&(v.source_id, &v.key)))
                .map(|(i, v)| ((v.source_id, v.key.clone()), i as u32))
                .collect()
        };
        self.index_absent = wanted.into_iter().filter(|(s, k)| !index.contains_key(&(*s, (*k).clone()))).map(|(s, k)| (s, k.clone())).collect();
        self.index = index;
        self.rebuild_progress_idx();
    }

    /// Recompute the open category's items when its inputs changed.
    fn refresh_items(&mut self, ctx: &UiCtx<'_>) {
        if self.sort_pending == Some(self.config_sort(ctx)) {
            self.sort_pending = None;
        }
        if self.index_built != lib_id(ctx) {
            self.index_built = lib_id(ctx);
            self.items_key = None;
            // stack ids belong to their snapshot
            self.open_stacks.clear();
        }
        self.refresh_index(ctx);
        // copies stack in All only (DESIGN §29.14)
        let stacked = self.scope(ctx).selected.is_none() && st::combine(ctx.config);
        let key = (self.open, self.sort(ctx), lib_id(ctx), self.scope(ctx).filter(), stacked, self.open_stacks.clone());
        // favourites change without a library rebuild: recompute that category every frame
        if self.items_key.as_ref() == Some(&key) && self.open != Cat::Favourites {
            return;
        }
        let (sort, scope) = (key.1, key.3.clone());
        self.items_key = Some(key);
        self.stacked = stacked;
        let movies = self.movies;
        let mut items: Vec<u32> = match self.open {
            Cat::All => ctx.lib.vod.all_sorted(movies, sort),
            Cat::Group(g) => ctx.lib.vod.group_items(g, sort),
            Cat::Continue => self.progress.order.iter().filter_map(|k| self.index.get(k).copied()).collect(),
            Cat::Favourites => {
                let kind = if movies { ItemKind::Movie } else { ItemKind::Series };
                let list = ctx.favs.default_list();
                list.map(|l| {
                    ctx.favs.items(l).iter().filter(|r| r.kind == kind).filter_map(|r| self.index.get(&(r.source, r.key.clone())).copied()).collect()
                })
                .unwrap_or_default()
            }
        };
        // the virtual categories show the playlists in scope (a category is one playlist's);
        // your own lists show every playlist in All, even one switched out of it
        let scope = if matches!(self.open, Cat::Continue | Cat::Favourites) { scope.for_user_list() } else { scope };
        if scope != ScopeFilter::Everything && !matches!(self.open, Cat::Group(_)) {
            let all = self.all(ctx);
            items.retain(|i| all.get(*i as usize).is_some_and(|v| scope.admits(v.source_id)));
        }
        if let Some(m) = &self.moved
            && sort == SortMode::Provider
            && matches!(self.open, Cat::All | Cat::Group(_))
        {
            m.apply(self.all(ctx), &mut items);
        }
        if stacked {
            let (stacks, all) = (ctx.lib.vod.stacks(movies), self.all(ctx));
            let cards = st::collapse(stacks, &items, all);
            self.fan_heads = cards.iter().copied().filter(|i| stacks.stack_of(*i).is_some_and(|s| self.open_stacks.contains(&s))).collect();
            // a stack folded away (gone from the list) stays closed
            self.open_stacks.retain(|s| self.fan_heads.iter().any(|h| stacks.stack_of(*h) == Some(*s)));
            items = st::fan_out(stacks, &cards, &self.open_stacks);
            self.copy_labels = self
                .open_stacks
                .iter()
                .flat_map(|s| stacks.members(*s).iter().copied())
                .filter_map(|m| st::copy_facts_label(stacks, m).map(|l| (m, l)))
                .collect();
        } else {
            self.fan_heads.clear();
            self.copy_labels.clear();
        }
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
        self.pending_cat = None;
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

    /// The list index of the card under the cursor.
    fn current_idx(&self) -> Option<u32> {
        self.shown.get(self.grid.cursor).copied()
    }

    /// The stack a card stands for: a stack card of All (folded, or the head of its fan).
    fn card_stack(&self, ctx: &UiCtx<'_>, idx: u32) -> Option<StackId> {
        if !self.stacked {
            return None;
        }
        let s = ctx.lib.vod.stacks(self.movies).stack_of(idx)?;
        (!self.open_stacks.contains(&s) || self.fan_heads.contains(&idx)).then_some(s)
    }

    /// The copies of the title under the cursor (several only while copies combine).
    fn current_copies(&self, ctx: &UiCtx<'_>) -> Vec<u32> {
        self.current_idx().map(|i| st::copies_of(ctx.lib, ctx.config, self.movies, i)).unwrap_or_default()
    }

    /// The copies of title `idx` with their state and play order (`store::copies::plan`: a
    /// few indexed rows per copy, on a key press).
    fn plan_of(&self, ctx: &UiCtx<'_>, idx: u32) -> crate::store::copies::CopyPlan {
        crate::ui::copies::plan(ctx, self.movies, idx).unwrap_or_else(|e| {
            tracing::warn!("copies: {e}");
            Default::default()
        })
    }

    /// Play one copy fullscreen (the resume prompt when a copy has progress, the episode a
    /// series' copy is at), the next copy as its fallback.
    fn play_copy(&mut self, ctx: &mut UiCtx<'_>, copy: u32) {
        let Some(v) = self.all(ctx).get(copy as usize).cloned() else { return };
        if v.locked && !kit::unlocked(ctx) {
            self.pending_copy = Some(copy);
            kit::ask_pin(ctx, Action::PlayFrom, "This copy is locked");
            return;
        }
        crate::ui::copies::play_title(ctx, self.movies, copy, Some(copy), true);
    }

    /// Space / a click on the badge: fan the stack of card `at` out, or fold it (the cursor
    /// stays on the stack's card).
    fn toggle_fan(&mut self, ctx: &mut UiCtx<'_>, at: usize) {
        let Some(idx) = self.shown.get(at).copied() else { return };
        let Some(s) = self.stacked.then(|| ctx.lib.vod.stacks(self.movies).stack_of(idx)).flatten() else { return };
        let head = if let Some(p) = self.open_stacks.iter().position(|x| *x == s) {
            self.open_stacks.remove(p);
            self.shown.iter().copied().find(|i| self.fan_heads.contains(i) && ctx.lib.vod.stacks(self.movies).stack_of(*i) == Some(s))
        } else {
            self.open_stacks.push(s);
            Some(idx)
        };
        self.refresh_items(ctx);
        if let Some(r) = head.and_then(|h| self.shown.iter().position(|x| *x == h)) {
            self.grid.select(r);
        }
    }

    /// Esc: fold every fanned-out stack (the cursor stays on its card or copy's stack card).
    fn fold_all(&mut self, ctx: &mut UiCtx<'_>) -> bool {
        if self.open_stacks.is_empty() {
            return false;
        }
        let stacks = ctx.lib.vod.stacks(self.movies);
        let cur = self.current_idx();
        let head = cur.and_then(|c| match stacks.stack_of(c) {
            Some(s) if self.open_stacks.contains(&s) => {
                self.shown.iter().copied().find(|i| self.fan_heads.contains(i) && stacks.stack_of(*i) == Some(s))
            }
            _ => Some(c),
        });
        self.open_stacks.clear();
        self.refresh_items(ctx);
        if let Some(r) = head.and_then(|h| self.shown.iter().position(|x| *x == h)) {
            self.grid.select(r);
        }
        true
    }

    /// Ctrl+O: "Play from…" — the title's copies with their playlist, language and version, the
    /// one that plays first marked.
    fn play_from_menu(&mut self, ctx: &mut UiCtx<'_>) {
        let copies = self.current_copies(ctx);
        if copies.len() < 2 {
            ctx.toast(crate::msg::Toast::info("This title has one copy"));
            return;
        }
        let first = self.current_idx().and_then(|i| self.plan_of(ctx, i).first());
        let all = self.all(ctx);
        let items = copies
            .iter()
            .take(MAX_COPIES)
            .enumerate()
            .map(|(k, &c)| {
                let locked = all.get(c as usize).is_some_and(|v| v.locked) && !kit::unlocked(ctx);
                MenuItem {
                    icon: if locked { glyphs::LOCK } else { glyphs::PLAY },
                    label: st::copy_label(ctx.lib, self.movies, c),
                    action: Action::Digit(MENU_COPY + k as u8),
                    enabled: true,
                    shortcut: (Some(c) == first).then(|| "plays first".to_owned()),
                }
            })
            .collect();
        self.play_from = (lib_id(ctx), copies);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Play from".into(), items }));
    }

    /// The provider category under the categories cursor (virtual ones have none).
    fn current_group<'l>(&self, ctx: &'l UiCtx<'_>) -> Option<&'l crate::model::Group> {
        match self.cat_rows.get(self.cats.cursor) {
            Some(Cat::Group(g)) => self.all_groups(ctx).iter().find(|x| x.id == *g),
            _ => None,
        }
    }

    fn open_item(&mut self, ctx: &mut UiCtx<'_>, v: &VodSummary, play: bool) {
        self.pending_cat = None;
        let idx = self.current_idx().filter(|i| self.all(ctx).get(*i as usize).is_some_and(|x| x.kind == v.kind));
        let asked = if play { Action::ActivateAlt } else { Action::Activate };
        // a stack card (DESIGN §29.12): Shift+Enter plays the copy that plays first, with its
        // fallback; Enter opens the card's page — its PIN when that copy is locked
        if let Some(i) = idx.filter(|i| self.card_stack(ctx, *i).is_some()) {
            if play {
                if !crate::ui::copies::play_title(ctx, self.movies, i, None, true) {
                    kit::ask_pin(ctx, asked, "Every copy of this title is locked");
                }
            } else if v.locked && !kit::unlocked(ctx) {
                kit::ask_pin(ctx, asked, "This copy is locked");
            } else {
                match v.kind {
                    VodKind::Movie(id) => ctx.push(Route::MovieDetail(id)),
                    VodKind::Series(id) => ctx.push(Route::SeriesDetail(id)),
                }
            }
            return;
        }
        if v.locked && !kit::unlocked(ctx) {
            kit::ask_pin(ctx, asked, "This title is locked");
            return;
        }
        // a copy shown on its own (fanned out, one playlist's list): its page and Play use it
        let copies = idx.map(|i| st::copies_of(ctx.lib, ctx.config, self.movies, i).len()).unwrap_or(0);
        match (v.kind, play) {
            (_, false) if copies > 1 => st::open_copy(ctx, v.kind),
            (VodKind::Movie(id), false) => ctx.push(Route::MovieDetail(id)),
            (VodKind::Series(id), false) => ctx.push(Route::SeriesDetail(id)),
            (_, true) if copies > 1 && self.stacked => {
                if let Some(i) = idx {
                    crate::ui::copies::play_title(ctx, self.movies, i, Some(i), true);
                }
            }
            (VodKind::Movie(id), true) => super::movie_detail::play_movie(ctx, id),
            (VodKind::Series(id), true) => play_series(ctx, id, &v.name),
        }
    }

    fn sort_menu(&self, ctx: &mut UiCtx<'_>) {
        let current = self.sort(ctx);
        let items = SORTS
            .iter()
            .enumerate()
            .map(|(i, (m, l))| MenuItem {
                icon: if *m == current { glyphs::CHECK } else { "" },
                label: (*l).into(),
                action: Action::Digit(i as u8),
                enabled: true,
                shortcut: None,
            })
            .collect();
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Sort by".into(), items }));
    }

    fn item_menu(&self, ctx: &mut UiCtx<'_>, v: &VodSummary) {
        let fav = ctx.favs.is_fav(&super::home::vod_ref(v));
        let mi = |icon: &'static str, label: &str, action: Action, shortcut: &str| MenuItem {
            icon,
            label: label.into(),
            action,
            enabled: true,
            shortcut: Some(shortcut.into()),
        };
        let mut items =
            vec![mi(glyphs::INFO, "Details", Action::Digit(MENU_OPEN), "Enter"), mi(glyphs::PLAY, "Play", Action::Digit(MENU_PLAY), "Shift+Enter")];
        // copies of the title (DESIGN §29.14): pick one, fan them out, or take one apart
        let idx = self.current_idx().filter(|i| self.all(ctx).get(*i as usize).is_some_and(|x| x.kind == v.kind));
        let copies = idx.map(|i| st::copies_of(ctx.lib, ctx.config, self.movies, i)).unwrap_or_default();
        let stack = idx.and_then(|i| self.card_stack(ctx, i));
        if copies.len() > 1 {
            items.push(mi(glyphs::PLAY, "Play from…", Action::PlayFrom, "Ctrl+O"));
        }
        if let Some(s) = stack {
            let open = self.open_stacks.contains(&s);
            items.push(mi(glyphs::FILMSTRIP, if open { "Fold the copies" } else { "Show each copy" }, Action::FanOut, "Space"));
        } else if self.stacked && copies.len() > 1 {
            items.push(MenuItem { shortcut: None, ..mi(glyphs::CLOSE, "Not the same title", Action::Digit(MENU_SPLIT), "") });
        }
        // combine it with a title the matcher kept apart (copies combine in All; with two
        // playlists of this kind or more)
        let kind = if self.movies { ItemKind::Movie } else { ItemKind::Series };
        if st::combine(ctx.config) && ctx.lib.eligible(kind).len() > 1 {
            items.push(MenuItem { shortcut: None, ..mi(glyphs::PLAYLIST, "Same title as…", Action::Digit(MENU_SAME_TITLE), "") });
        }
        let fav =
            fav || (stack.is_some() && copies.iter().any(|c| self.all(ctx).get(*c as usize).is_some_and(|x| ctx.favs.is_fav(&st::item_ref(x)))));
        items.extend([
            mi(glyphs::FAVOURITE, if fav { "Remove from Favourites" } else { "Add to Favourites" }, Action::ToggleFavourite, "Ctrl+F"),
            mi(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, "Ctrl+Shift+F"),
        ]);
        if matches!(v.kind, VodKind::Movie(_)) {
            items.push(mi(glyphs::CHECK, "Mark watched", Action::MarkWatched, "Ctrl+W"));
        }
        match self.open {
            Cat::Favourites => items.push(mi(glyphs::CLOSE, "Remove from Favourites", Action::Remove, "Delete")),
            Cat::Continue => items.push(mi(glyphs::CLOSE, "Remove from Continue watching", Action::Remove, "Delete")),
            _ => {}
        }
        items.extend([
            mi(glyphs::TAG, "Rename…", Action::Rename, "F2"),
            mi(glyphs::LOCK, "Lock / unlock", Action::ToggleLock, "Ctrl+L"),
            mi(glyphs::HIDDEN, "Hide", Action::ToggleHidden, "Ctrl+H"),
            mi(glyphs::SORT, "Sort & view options…", Action::SortMenu, "Ctrl+S"),
        ]);
        crate::ui::item_menu::extend(&mut items, &crate::ui::item_menu::Target::vod(v.kind), ctx, KeyContext::List);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: v.name.to_string(), items }));
    }

    /// Item actions shared by the grid keys and the context menu.
    fn item_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.pending_cat = None;
        // the batch features' keys and menu choices on the card (Ctrl+N…)
        if let Some(v) = self.current(ctx)
            && crate::ui::item_menu::dispatch(ctx, action, &crate::ui::item_menu::Target::vod(v.kind))
        {
            return true;
        }
        let item_actions = [
            Action::ToggleFavourite,
            Action::AddToList,
            Action::ToggleLock,
            Action::ToggleHidden,
            Action::Rename,
            Action::ShowDetails,
            Action::MarkWatched,
            Action::Remove,
            Action::MoveUp,
            Action::MoveDown,
            Action::OpenInGuide,
            Action::AssignEpg,
        ];
        if !item_actions.contains(&action) {
            return false;
        }
        if matches!(action, Action::MoveUp | Action::MoveDown) {
            self.reorder(ctx, action == Action::MoveUp);
            return true;
        }
        let Some(v) = self.current(ctx) else { return true };
        let item = super::home::vod_ref(&v);
        // a stack card: every action covers the title's copies (DESIGN §29.12, §29.15)
        if let Some(i) = self.current_idx().filter(|i| self.card_stack(ctx, *i).is_some()) {
            let plan = self.plan_of(ctx, i);
            match action {
                // Delete in Favourites takes every copy out, like Ctrl+F on a favourite stack
                Action::ToggleFavourite | Action::Remove if action == Action::ToggleFavourite || self.open == Cat::Favourites => {
                    crate::ui::copies::toggle_favourite(ctx, &plan)
                }
                Action::MarkWatched if self.movies => crate::ui::copies::mark_watched(ctx, &plan, &v.name, true),
                Action::MarkWatched => ctx.toast(crate::msg::Toast::info("Open the series to mark its episodes watched")),
                Action::Remove if self.open == Cat::Continue && self.movies => crate::ui::copies::mark_watched(ctx, &plan, &v.name, false),
                Action::Remove if self.open == Cat::Continue => crate::ui::copies::forget_series(ctx, &plan),
                Action::ToggleLock => crate::ui::copies::toggle_lock(ctx, &plan, self.movies),
                Action::ToggleHidden => crate::ui::copies::hide(ctx, &plan, &v.name),
                Action::Rename => crate::ui::copies::rename_dialog(ctx, &plan, &v.name),
                Action::AddToList => kit::list_picker(ctx, &item, &v.name),
                Action::ShowDetails => self.open_item(ctx, &v, false),
                _ => kit::not_here(ctx),
            }
            return true;
        }
        match action {
            Action::ToggleFavourite => kit::toggle_favourite(ctx, &item),
            Action::AddToList => kit::list_picker(ctx, &item, &v.name),
            Action::ToggleLock => {
                let groups = self.all_groups(ctx);
                let by_group = kit::any_group_locked(groups, &v.group_ids);
                kit::toggle_lock(ctx, &item, v.locked, by_group);
            }
            Action::ToggleHidden => kit::hide_item(ctx, &item, &v.name),
            Action::Rename => kit::rename_item(ctx, &item, &v.name),
            Action::ShowDetails => self.open_item(ctx, &v, false),
            Action::MarkWatched => match v.kind {
                VodKind::Movie(_) => kit::mark_watched(ctx, &item, &v.name, true),
                VodKind::Series(_) => ctx.toast(crate::msg::Toast::info("Open the series to mark its episodes watched")),
            },
            Action::Remove => match self.open {
                Cat::Favourites => kit::toggle_favourite(ctx, &item),
                Cat::Continue => self.forget_progress(ctx, &v),
                _ => kit::not_here(ctx),
            },
            _ => kit::not_here(ctx),
        }
        true
    }

    /// Delete in "Continue watching": forget the progress (for a series, of its episodes).
    fn forget_progress(&mut self, ctx: &mut UiCtx<'_>, v: &VodSummary) {
        match v.kind {
            VodKind::Movie(_) => kit::mark_watched(ctx, &super::home::vod_ref(v), &v.name, false),
            VodKind::Series(id) => {
                let now = ctx.now;
                let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| {
                    let items: Vec<(crate::model::ItemRef, String)> = crate::store::vod::episodes(c, id, None)?
                        .into_iter()
                        .map(|e| (crate::model::ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }, e.title))
                        .collect();
                    crate::store::user::mark_watched(c, &items, false, now)
                });
                ctx.toast(crate::msg::Toast::info("Removed from Continue watching"));
            }
        }
    }

    /// Alt+↑/↓ on a poster: move the title one place in Custom order. A sorted grid switches
    /// to Custom order first (DESIGN §13.0) and the move happens in that order — the one the
    /// user is about to see. The title swaps places with its neighbour in the playlist's whole
    /// order (in a category the two are adjacent there, with only other categories' titles
    /// between them, which keep their places), stored as `user_order` of the playlist's
    /// titles up to the moved pair (positions; the titles further down keep theirs), shown at
    /// once and confirmed by the library rebuild. The cursor follows the title.
    fn reorder(&mut self, ctx: &mut UiCtx<'_>, up: bool) {
        let Some(cur) = self.current(ctx) else { return };
        if !matches!(self.open, Cat::All | Cat::Group(_)) {
            ctx.toast(crate::msg::Toast::info("This list keeps its own order"));
            return;
        }
        if self.grid.filter.is_active() {
            ctx.toast(crate::msg::Toast::info("Clear the filter to reorder"));
            return;
        }
        if self.sort(ctx) != SortMode::Provider {
            self.set_sort(ctx, SortMode::Provider);
            ctx.toast(crate::msg::Toast::info("Switched to custom order"));
            self.refresh_items(ctx);
        }
        let all = self.all(ctx);
        let at = |x: &u32| all.get(*x as usize);
        let Some(i) = self.items.iter().position(|x| at(x).is_some_and(|v| v.kind == cur.kind)) else { return };
        let j = if up { i.checked_sub(1) } else { (i + 1 < self.items.len()).then_some(i + 1) };
        // titles stay with their playlist: the first / last of it doesn't move
        let Some(other) = j.and_then(|j| at(&self.items[j])).filter(|v| v.source_id == cur.source_id) else {
            self.select_title(ctx, &cur);
            return;
        };
        let mut m = match self.moved.take() {
            Some(m) if m.source == cur.source_id => m,
            _ => Moved::new(cur.source_id, all.iter().filter(|v| v.source_id == cur.source_id).map(|v| v.key.clone()).collect()),
        };
        let (Some(&gi), Some(&gj)) = (m.rank.get(&cur.key), m.rank.get(&other.key)) else { return };
        m.order.swap(gi, gj);
        m.rank.insert(cur.key.clone(), gj);
        m.rank.insert(other.key.clone(), gi);
        let keys = m.order[..=gi.max(gj)].to_vec();
        let (src, kind) = (cur.source_id, if self.movies { ItemKind::Movie } else { ItemKind::Series });
        m.writes
            .push(ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::reorder_items(c, src, kind, &keys)));
        self.moved = Some(m);
        self.items_key = None;
        self.refresh_items(ctx);
        self.select_title(ctx, &cur);
    }

    fn select_title(&mut self, ctx: &UiCtx<'_>, v: &VodSummary) {
        let all = self.all(ctx);
        if let Some(r) = self.shown.iter().position(|x| all.get(*x as usize).is_some_and(|x| x.kind == v.kind)) {
            self.grid.select(r);
        }
    }

    /// Category shortcuts (lock, rename, hide, reorder, context menu) in the categories pane.
    fn category_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if !matches!(
            action,
            Action::ToggleLock
                | Action::Rename
                | Action::ToggleHidden
                | Action::MoveUp
                | Action::MoveDown
                | Action::ContextMenu
                | Action::ToggleMark
                | Action::HideShown
        ) {
            return false;
        }
        self.pending_cat = None;
        // bulk hide: the marked categories, the ones the filter shows (DESIGN §28.7)
        if action == Action::ToggleHidden && self.marks.hide_marked(ctx) {
            return true;
        }
        if action == Action::HideShown {
            let by_id: HashMap<GroupId, &crate::model::Group> = self.all_groups(ctx).iter().map(|g| (g.id, g)).collect();
            let shown: Vec<crate::model::Group> = self.matching.iter().filter_map(|id| by_id.get(id).map(|g| (*g).clone())).collect();
            self.marks.hide_shown(ctx, self.cats.filter.query(), &shown.iter().collect::<Vec<_>>());
            return true;
        }
        let Some(g) = self.current_group(ctx).cloned() else {
            ctx.toast(crate::msg::Toast::info("Built-in categories can't be changed"));
            return true;
        };
        match action {
            Action::ToggleLock => kit::toggle_group_lock(ctx, &g),
            Action::Rename => kit::rename_group(ctx, &g),
            Action::ToggleHidden => {
                group_marks::hide(ctx, group_marks::Noun::Category, vec![crate::model::GroupRef::from(&g)], Some(g.display_name()))
            }
            Action::ToggleMark => {
                self.marks.toggle(&g);
                self.action(ctx, Action::Down);
            }
            Action::MoveUp | Action::MoveDown => {
                let delta = if action == Action::MoveUp { -1 } else { 1 };
                let visible = self.groups(ctx);
                if kit::reorder_group(&*ctx, &visible, g.id, delta) {
                    // the cursor follows the category (the library rebuild reorders the rows)
                    let next = (self.cats.cursor as isize + delta).max(0) as usize;
                    self.cats.select(next);
                }
            }
            _ => {
                let mi = |icon: &'static str, label: &str, action: Action, shortcut: &str| MenuItem {
                    icon,
                    label: label.into(),
                    action,
                    enabled: true,
                    shortcut: Some(shortcut.into()),
                };
                let mut items = vec![
                    mi(glyphs::CHEVRON_RIGHT, "Open", Action::Digit(MENU_OPEN_CAT), "Enter"),
                    mi(glyphs::TAG, "Rename…", Action::Rename, "F2"),
                    mi(
                        if g.locked { glyphs::UNLOCK } else { glyphs::LOCK },
                        if g.locked { "Unlock" } else { "Lock with PIN" },
                        Action::ToggleLock,
                        "Ctrl+L",
                    ),
                    mi(glyphs::HIDDEN, if self.marks.is_empty() { "Hide" } else { "Hide the marked categories" }, Action::ToggleHidden, "Ctrl+H"),
                    mi(glyphs::CHECK, if self.marks.is_marked(g.id) { "Unmark" } else { "Mark for hiding" }, Action::ToggleMark, "Space"),
                ];
                if self.cats.filter.is_active() {
                    let n = self.matching.len();
                    items.push(mi(glyphs::HIDDEN, &format!("Hide these {n} categories"), Action::HideShown, "Ctrl+Shift+H"));
                }
                // the category filter of the category's playlist (DESIGN §28.6)
                if self.tags.1.contains_key(&g.id) {
                    items.push(MenuItem { shortcut: None, ..mi(glyphs::EARTH, "Countries & languages…", Action::Digit(MENU_COUNTRIES), "") });
                }
                items.push(super::live::explicit_menu_item(&g, MENU_EXPLICIT));
                items.extend([mi(glyphs::REORDER, "Move up", Action::MoveUp, "Alt+↑"), mi(glyphs::REORDER, "Move down", Action::MoveDown, "Alt+↓")]);
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: g.display_name().to_owned(), items }));
            }
        }
        true
    }

    /// Esc: the drawer / pane steps back before the screen is left (Medium, Compact).
    fn back(&mut self, ctx: &UiCtx<'_>) -> bool {
        match (ctx.layout, self.pane) {
            (LayoutClass::Medium, Pane::Categories) => {
                self.pane = Pane::Grid;
                true
            }
            (LayoutClass::Compact, Pane::Grid) => {
                self.pane = Pane::Categories;
                true
            }
            _ => false,
        }
    }

    pub(super) fn action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if scope::on_action(ctx, self.scope_kind(), action) {
            return true;
        }
        if action == Action::SortMenu {
            self.sort_menu(ctx);
            return true;
        }
        if matches!(action, Action::FocusNext | Action::FocusPrev) {
            self.pane = if self.pane == Pane::Grid { Pane::Categories } else { Pane::Grid };
            return true;
        }
        if action == Action::Back {
            // Esc ladder: the marks go first, then fanned-out stacks
            return self.marks.clear() || (self.pane == Pane::Grid && self.fold_all(ctx)) || self.back(ctx);
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
                ListOutcome::ContextMenu(_) => self.category_action(ctx, Action::ContextMenu),
                ListOutcome::Parent => true,
                ListOutcome::None => {
                    matches!(
                        action,
                        Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_)
                    ) || self.category_action(ctx, action)
                        || {
                            // the other item shortcuts mean nothing for a category
                            let item_shortcut = matches!(
                                action,
                                Action::ToggleFavourite
                                    | Action::AddToList
                                    | Action::ShowDetails
                                    | Action::MarkWatched
                                    | Action::Remove
                                    | Action::OpenInGuide
                                    | Action::AssignEpg
                            );
                            if item_shortcut {
                                kit::not_here(ctx);
                            }
                            item_shortcut
                        }
                }
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
                    Action::Up
                    | Action::Down
                    | Action::Left
                    | Action::Right
                    | Action::PageUp
                    | Action::PageDown
                    | Action::Home
                    | Action::End
                    | Action::FilterInput(_) => true,
                    Action::FanOut => {
                        self.toggle_fan(ctx, self.grid.cursor);
                        true
                    }
                    Action::PlayFrom => {
                        self.play_from_menu(ctx);
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
                self.set_sort(ctx, SORTS[d as usize].0);
            }
            OverlayResult::Chosen(Action::Digit(MENU_OPEN)) | OverlayResult::Chosen(Action::Digit(MENU_PLAY)) => {
                let play = result == OverlayResult::Chosen(Action::Digit(MENU_PLAY));
                if let Some(v) = self.current(ctx) {
                    self.open_item(ctx, &v, play);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_OPEN_CAT)) => {
                if let Some(c) = self.cat_rows.get(self.cats.cursor).copied() {
                    self.open_cat(ctx, c, true);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_COUNTRIES)) => {
                if let Some(g) = self.current_group(ctx) {
                    ctx.open(OverlayRequest::Categories(g.source_id));
                }
            }
            OverlayResult::Chosen(Action::Digit(d)) if d >= MENU_COPY => {
                // a copy of "Play from…" (of the library the menu listed)
                let (lib, copies) = &self.play_from;
                if let Some(c) = copies.get(usize::from(d - MENU_COPY)).copied().filter(|_| *lib == lib_id(ctx)) {
                    self.play_copy(ctx, c);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_SPLIT)) => {
                if let Some(v) = self.current(ctx) {
                    crate::ui::copies::split(ctx, &st::item_ref(&v), &v.name);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_SAME_TITLE)) => {
                if let Some(v) = self.current(ctx) {
                    crate::ui::copies::same_title_picker(ctx, st::item_ref(&v), &v.name);
                }
            }
            OverlayResult::Chosen(Action::FanOut) => self.toggle_fan(ctx, self.grid.cursor),
            OverlayResult::Chosen(Action::PlayFrom) => self.play_from_menu(ctx),
            OverlayResult::Unlocked(Action::PlayFrom) => {
                if let Some(c) = self.pending_copy.take() {
                    self.play_copy(ctx, c);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_EXPLICIT)) => {
                if let Some(g) = self.current_group(ctx).cloned() {
                    super::live::toggle_group_explicit(ctx, &g);
                }
            }
            OverlayResult::Chosen(Action::SortMenu) => self.sort_menu(ctx),
            OverlayResult::Confirmed(token) => {
                if self.marks.confirmed(ctx, token) {
                    self.cats.filter.clear();
                    self.refilter_cats(ctx);
                }
            }
            OverlayResult::Cancelled(token) => self.marks.cancelled(token),
            OverlayResult::Chosen(a) => {
                if self.pane == Pane::Categories {
                    self.category_action(ctx, a);
                } else {
                    self.item_action(ctx, a);
                }
            }
            OverlayResult::Unlocked(a) => {
                // a category asked with `Activate`; anything else retries what the pane asked for
                match self.pending_cat.take() {
                    Some(c) if a == Action::Activate => self.open_cat(ctx, c, true),
                    _ if self.pane == Pane::Categories && a == Action::ToggleLock => {
                        self.category_action(ctx, a);
                    }
                    _ => {
                        if let Some(v) = self.current(ctx) {
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
                }
            }
        }
    }

    /// The status bar's extra hints: Space and Ctrl+O on a stack card, Ctrl+O on a copy.
    pub(super) fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        match self.hint {
            (true, _) => vec![(Action::FanOut, "copies"), (Action::PlayFrom, "play from")],
            (false, true) => vec![(Action::PlayFrom, "play from")],
            _ => Vec::new(),
        }
    }

    pub(super) fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        // what the focused card offers (one lookup per frame)
        let idx = self.current_idx().filter(|_| self.pane == Pane::Grid);
        let kind = idx.and_then(|i| self.all(ctx).get(i as usize)).map(|v| v.kind);
        self.hint =
            (idx.is_some_and(|i| self.card_stack(ctx, i).is_some()), kind.is_some_and(|k| st::stack_of_kind(ctx.lib, ctx.config, k).is_some()));
        match kit::poll(&mut self.progress_loader) {
            Some(Ok(p)) => {
                self.progress = p;
                // `refresh_items` resolves its keys (below, before any card is drawn)
                self.progress_gen += 1;
                if self.open == Cat::Continue {
                    self.items_key = None;
                }
            }
            Some(Err(e)) => tracing::warn!("{} progress: {e}", self.kind_label()),
            None => {}
        }
        let theme = ctx.theme;
        // no titles and no categories: nothing imported (hidden titles keep their categories)
        if ctx.lib.built_at != 0 && self.all(ctx).is_empty() && self.all_groups(ctx).iter().all(|g| g.item_count == 0) {
            let title = format!("Your playlists have no {}", self.kind_label());
            empty_state::empty_state(
                ui,
                theme,
                if self.movies { glyphs::MOVIES } else { glyphs::SERIES },
                &title,
                Some("Xtream logins usually include movies and series; M3U playlists rarely do."),
                None,
            );
            return;
        }
        let scope = self.scope(ctx);
        let filter = scope.filter();
        if self.cats_scope.as_ref() != Some(&filter) {
            // another playlist: its categories, from the top of the grid (a filter typed for
            // the old one would keep its category open under the new name)
            if self.cats_scope.is_some() {
                self.cats.filter.clear();
                self.grid.filter.clear();
                self.grid.select(0);
                self.cats.select(0);
            }
            self.cats_scope = Some(filter);
            self.refilter_cats(ctx);
        } else if self.cat_rows.is_empty() || self.cat_rows.len() != self.cats.len || lib_id(ctx) != self.index_built {
            self.refilter_cats(ctx);
        }
        self.settle_moves(ctx);
        self.refresh_items(ctx);
        if !self.cat_rows.contains(&self.open) && !self.cats.filter.is_active() {
            self.open = Cat::All;
            self.items_key = None;
            self.refresh_items(ctx);
        }
        // Wide: categories | grid. Medium: the categories are a drawer over the left edge (the
        // grid gets the width: 4–5 columns at a tiled half). Compact: one pane at a time.
        let layout = ctx.layout;
        let rem = theme.rem();
        if scope.multi() {
            self.scope_bar(ui, ctx, &scope);
        }
        let full = ui.available_rect_before_wrap();
        let (show_cats, show_grid) = match layout {
            LayoutClass::Wide => (true, true),
            LayoutClass::Medium => (self.pane == Pane::Categories, true),
            LayoutClass::Compact => (self.pane == Pane::Categories, self.pane == Pane::Grid),
        };
        let cats_w = if layout == LayoutClass::Compact {
            full.width()
        } else {
            (if layout == LayoutClass::Wide { 240.0 } else { 200.0 } * rem).round().min(full.width())
        };
        let gap = theme.space(SpaceToken::PanelPadding);
        let mut cats_rect = full;
        cats_rect.set_width(cats_w);
        let grid_rect = if show_cats && layout != LayoutClass::Compact {
            egui::Rect::from_min_max(egui::pos2(cats_rect.right() + gap, full.top()), full.max)
        } else {
            full
        };

        if show_cats {
            let mut child = ui.new_child(
                egui::UiBuilder::new().id_salt(("vod-cats", self.movies)).max_rect(cats_rect).layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.cats_pane(&mut child, ctx);
            if layout != LayoutClass::Compact {
                crate::ui::widgets::section::pane_divider(ui.painter(), cats_rect.right() + gap / 2.0, full.y_range(), theme);
            }
        }
        if show_grid {
            let mut child = ui.new_child(
                egui::UiBuilder::new().id_salt(("vod-grid", self.movies)).max_rect(grid_rect).layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.grid_pane(&mut child, ctx);
        }
        ui.allocate_rect(full, egui::Sense::hover());
    }

    /// The scope bar + the stripe of a picked playlist (DESIGN §29).
    fn scope_bar(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, scope: &Scope<'_>) {
        let theme = ctx.theme;
        let counts: Vec<usize> = scope.choices().iter().map(|c| scope.count(ctx.lib, c.map(|s| s.id))).collect();
        let hint = super::live::shortcut(KeyContext::Global, Action::PlaylistPicker).map(|k| format!("{k} playlist")).unwrap_or_default();
        let click = scope_bar::scope_bar(ui, theme, scope, &counts, ctx.layout == LayoutClass::Wide, &hint);
        let y = ui.cursor().top();
        scope_bar::paint_stripe(ui.painter(), theme, ui.max_rect().x_range(), y, scope);
        ui.add_space(theme.space(SpaceToken::Md));
        match click {
            scope_bar::BarClick::Choose(c) => scope::choose(ctx, self.scope_kind(), c, false),
            scope_bar::BarClick::Menu(id) => ctx.open(OverlayRequest::PlaylistMenu(id, self.scope_kind())),
            scope_bar::BarClick::Picker => ctx.open(OverlayRequest::PlaylistPicker(self.scope_kind())),
            scope_bar::BarClick::None => {}
        }
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
        let row_h = rows::row_height(theme, ctx.config.interface.density, false);
        let focused = self.pane == Pane::Categories;
        let open = self.open;
        let rows_snapshot = self.cat_rows.clone();
        // one map per frame; rows are drawn (and named) only when visible
        let groups: HashMap<GroupId, &crate::model::Group> = self.groups(ctx).into_iter().map(|g| (g.id, g)).collect();
        let virtual_names = [self.virtual_name(Cat::All), self.virtual_name(Cat::Continue), self.virtual_name(Cat::Favourites)];
        let scope = self.scope(ctx);
        let total = scope.count(ctx.lib, scope.selected.map(|s| s.id));
        // in All with several playlists, each category names its playlist (DESIGN §29)
        let sources = scope.marks(ctx);
        let footer = self.marks.footer(self.cats.filter.query(), self.matching.len());
        let hide_n = self.marks.hide_count(self.matching.len());
        let (marks, tags, movies, cats) = (&self.marks, &self.tags.1, self.movies, &mut self.cats);
        let mut clicked = None;
        let (outcome, footer_click) = group_marks::with_footer(ui, theme, footer.as_deref(), Some(hide_n), |ui| {
            virtual_list(ui, theme, ("vod-cats", movies), cats, row_h, |ui, rc| {
                let index = rc.index;
                let cat = rows_snapshot[rc.index];
                let state = RowState::from_flags(focused && rc.state.is_cursor(), cat == open);
                let group = match cat {
                    Cat::Group(g) => groups.get(&g).copied(),
                    _ => None,
                };
                let r = match group {
                    Some(g) => rows::group_row_decor(
                        ui,
                        theme,
                        g,
                        state,
                        rows::GroupDecor {
                            marked: marks.is_marked(g.id),
                            // no badge where the name shows the code already ("|UK| Sports")
                            tag: tags.get(&g.id).map(String::as_str).filter(|c| !group_marks::has_words(g.display_name(), c)),
                            source: sources.of(g.source_id),
                        },
                    ),
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
                };
                if r.clicked() {
                    clicked = Some(index);
                }
                r
            })
        });
        // the footer's Hide N / Clear: its keys, on this pane
        if let Some(hide) = footer_click {
            self.pane = Pane::Categories;
            let a = self.marks.footer_action(hide);
            self.action(ctx, a);
            return;
        }
        // a tiled half or narrower: a click on a category opens it and shows its titles (the
        // drawer closes, Compact turns to the grid); in Wide both panes show anyway
        if ctx.layout != LayoutClass::Wide
            && let Some(c) = clicked.and_then(|i| self.cat_rows.get(i).copied())
        {
            self.cat_clicked_at = Some(ui.input(|i| i.time));
            self.open_cat(ctx, c, true);
            return;
        }
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
            ListOutcome::ContextMenu(_) => {
                self.pane = Pane::Categories;
                self.category_action(ctx, Action::ContextMenu);
            }
            _ => {}
        }
    }

    fn grid_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let scope = self.scope(ctx);
        // the breadcrumb: which playlist, then the category (DESIGN §29)
        let mut name = self.cat_name(ctx, self.open);
        if scope.multi() {
            // in All a category is still one playlist's: name it ("All playlists › Action · TV 2")
            if let Cat::Group(g) = self.open
                && let Some(src) = self.groups(ctx).iter().find(|x| x.id == g).and_then(|x| scope.marks(ctx).of(x.source_id))
            {
                name = format!("{name} · {}", src.name);
            }
            name = format!("{} › {name}", scope.label());
        }
        let sort = self.sort(ctx);
        // a tiled half (drawer) or Compact (one pane): the way to the categories
        let breadcrumb = ctx.layout != LayoutClass::Wide;
        let mut sort_clicked = false;
        let mut drawer_clicked = false;
        let mut header_clicked = false;
        // the sort button is laid out first (right to left) so the header gets what is left
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = format!("Sort: {} ▾", sort_label(sort));
                let style = ButtonStyle::default();
                sort_clicked = if ui.available_width() < 520.0 * theme.rem() {
                    icon_button(ui, theme, glyphs::SORT, &label, style)
                } else {
                    button(ui, theme, Some(glyphs::SORT), &label, style)
                }
                .clicked();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if breadcrumb {
                        // the categories drawer / pane (also ← on the first column / Backspace),
                        // named like Live's "{group} ▾"; a long name is shortened on screen
                        let full = format!("{name} ▾");
                        let shown = format!("{} ▾", short_label(&name, LABEL_MAX_CHARS));
                        let r = button(ui, theme, None, &shown, ButtonStyle { selected: self.pane == Pane::Categories, ..Default::default() });
                        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &full));
                        drawer_clicked = r.on_hover_text("Categories").clicked();
                    }
                    // the sort key, like the hint line of a list would say it (Home's rails share
                    // that hint line and have no sort, so it lives here)
                    let sort_key =
                        super::live::shortcut(KeyContext::PosterGrid, Action::SortMenu).map(|k| format!(" · {k} sort")).unwrap_or_default();
                    // the button already names the category
                    let placeholder = if breadcrumb { format!("Type to filter{sort_key}") } else { format!("{name} — type to filter{sort_key}") };
                    header_clicked = search_header(ui, theme, self.grid.filter.query(), &placeholder, None).clicked();
                });
            });
        });
        if sort_clicked {
            self.sort_menu(ctx);
        }
        if drawer_clicked {
            self.pane = if self.pane == Pane::Categories { Pane::Grid } else { Pane::Categories };
        } else if header_clicked {
            self.pane = Pane::Grid;
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if self.cat_locked(ctx, self.open) {
            if empty_state::empty_state(ui, theme, glyphs::LOCK, "This category is locked", Some("Enter the PIN to open it."), Some("Unlock"))
                .is_some_and(|r| r.clicked())
            {
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
                (
                    glyphs::HIDDEN,
                    "Everything here is hidden".to_owned(),
                    Some("Hidden titles are listed in Settings ▸ Hidden items."),
                    Some("Show hidden"),
                )
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
        let progress = &self.progress_idx;
        let movies = self.movies;
        let all: &[VodSummary] = if movies { &ctx.lib.vod.movies } else { &ctx.lib.vod.series };
        // only the focused pane shows the cursor (DESIGN §13.0); the other shows "selected"
        let focused = self.pane == Pane::Grid;
        let sources = scope.marks(ctx);
        // stacks (DESIGN §29.14): the cards of All, "+1" inside one playlist
        let stacks = ctx.lib.vod.stacks(movies);
        let unlocked = kit::unlocked(ctx);
        let (stacked, single) = (self.stacked, scope.selected.is_some());
        let named = ctx.config.interface.playlist_on_cards != crate::config::PlaylistOnCards::Never;
        let (lib, config) = (ctx.lib, ctx.config);
        let (open, heads, labels, rank) = (&self.open_stacks, &self.fan_heads, &self.copy_labels, &self.played_rank);
        let outcome = poster::poster_grid_with(ui, theme, ctx.images, &mut self.grid, all, &self.shown, focused, |i, v| {
            let mut d = TileDecor {
                progress: progress.get(&i).copied(),
                playing: playing == Some(v.kind),
                source: sources.of(v.source_id),
                ..Default::default()
            };
            match stacks.stack_of(i) {
                Some(s) if stacked && (!open.contains(&s) || heads.contains(&i)) => {
                    let members = stacks.members(s);
                    let others: Vec<SourceId> = st::playlists_of(stacks, members, all).into_iter().filter(|p| *p != v.source_id).collect();
                    let marks = crate::store::stacks::stack_marks(
                        members,
                        |_| false,
                        |_| false,
                        |m| rank.get(&m).map(|r| -(*r as i64)),
                        |m| all.get(m as usize).is_some_and(|x| x.locked),
                    );
                    d.stack = Some(StackDecor {
                        count: members.len(),
                        open: open.contains(&s),
                        fan: true,
                        next: [0, 1].map(|k| others.get(k).and_then(|p| sources.of(*p))),
                        others: others.len(),
                    });
                    // the progress of the copy played last
                    d.progress = marks.resume.and_then(|m| progress.get(&m)).copied();
                    d.dimmed = marks.locked && !unlocked;
                    // fanned out: the head card is one of the copies too — say which
                    if open.contains(&s) {
                        d.copy = labels.get(&i).map(String::as_str);
                    }
                }
                Some(_) if stacked => {
                    d.copy = labels.get(&i).map(String::as_str);
                    d.dimmed = v.locked && !unlocked;
                }
                Some(_) if single => {
                    let also = st::elsewhere(config, stacks, i, all);
                    if !also.is_empty() {
                        d.elsewhere = Some(Elsewhere { count: also.len(), first: lib.source(also[0]).filter(|_| named) });
                    }
                }
                _ => {}
            }
            d
        });
        if let Some(at) = self.grid.take_badge_click() {
            self.pane = Pane::Grid;
            self.toggle_fan(ctx, at);
            return;
        }
        // the second click of the double-click that opened the category lands on a poster
        // that just appeared under the pointer: it doesn't open that title
        let outcome = match outcome {
            ListOutcome::Moved | ListOutcome::Activated(_) if crate::ui::widgets::second_click_after(ui, self.cat_clicked_at) => ListOutcome::None,
            o => o,
        };
        match outcome {
            ListOutcome::Moved => self.pane = Pane::Grid,
            ListOutcome::Activated(_) => {
                self.pane = Pane::Grid;
                if let Some(v) = self.current(ctx) {
                    self.open_item(ctx, &v, false);
                }
            }
            ListOutcome::ContextMenu(_) => {
                self.pane = Pane::Grid;
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
            let title = egui::Rect::from_min_size(
                egui::pos2(min.x, poster.bottom() + theme.space(SpaceToken::Md)),
                egui::vec2(card_w * 0.7, theme.font(TypeToken::Body)),
            );
            ui.painter().rect_filled(title, theme.tokens.rounding, theme.tokens.fill_field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, has_label_containing, rig, seed, until};
    use egui_kittest::kittest::Queryable;

    /// Several playlists with movies (DESIGN §29): the bar offers All + each playlist with
    /// movies (a free-channel preset is no tab) with counts; in All the playlist's name finds
    /// its categories and an open category's breadcrumb names it; Ctrl+PgDn shows one playlist —
    /// a toast says so, the choice is saved for Movies only — and its categories, All and
    /// Favourites are that playlist's, the breadcrumb names it and cards drop their marks;
    /// a click on a tab picks it, a right click opens its menu; Alt+P opens the picker; a
    /// saved playlist that has no movies reads as All.
    #[test]
    fn the_scope_bar_narrows_movies_to_one_playlist() {
        let (mut t, one) = rig();
        let (a, b, free) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let free = testkit::source(c, "Free")?;
            testkit::movies(c, a, 3)?;
            testkit::movies(c, b, 2)?;
            testkit::channels(c, free, 2)?;
            let list = crate::store::user::default_fav_list(c)?;
            let fav = |s: SourceId, i: usize| crate::model::ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) };
            crate::store::user::set_favourite(c, list, &fav(a, 0), true, 1)?;
            crate::store::user::set_favourite(c, list, &fav(b, 0), true, 2)?;
            Ok((a, b, free))
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        assert!(has_label(&h, "All playlists") && has_label(&h, "Alpha") && has_label(&h, "Bravo"), "tabs");
        assert!(!has_label(&h, "Free"), "no movies, no tab");
        let painted = testkit::painted(&h);
        assert!(["5", "3", "2"].iter().all(|n| painted.iter().any(|p| p == n)), "counts: {painted:?}");
        assert_eq!(h.state().s.browser.shown.len(), 5);
        assert!(has_label(&h, "Movie 0 · Alpha") && has_label(&h, "Movie 0 · Bravo"), "All: each card names its playlist");
        assert!(has_label(&h, "Movies · Alpha") && has_label(&h, "Movies · Bravo"), "two “Movies” categories told apart");
        // typing the playlist's name finds its categories; the breadcrumb names it
        h.state_mut().act(Action::Left);
        for c in "bravo".chars() {
            h.state_mut().act(Action::FilterInput(c));
        }
        h.run_steps(1);
        assert_eq!(h.state().s.browser.cat_rows.len(), 1, "Bravo's “Movies”");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(h.state().s.browser.shown.len(), 2);
        assert!(has_label_containing(&h, "All playlists › Movies · Bravo"), "breadcrumb in All");

        // Ctrl+PgDn: the next playlist, with a toast; saved for Movies only (the filter typed
        // for All goes: Bravo's category never stays open under "Alpha ›")
        assert!(h.state_mut().act(Action::NextPlaylist));
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some("Showing Alpha only"));
        h.state_mut().apply_config();
        let scopes = h.state().t.config.interface.playlist_scope;
        assert_eq!((scopes.movies, scopes.series), (Some(a), None), "per screen");
        h.run_steps(2);
        assert_eq!(h.state().s.browser.open, Cat::All);
        assert!(!h.state().s.browser.cats.filter.is_active());
        assert_eq!(h.state().s.browser.shown.len(), 3, "Alpha's movies");
        assert!(has_label(&h, "Movie 0") && !has_label_containing(&h, "· Alpha"), "no marks inside one playlist");
        assert!(has_label_containing(&h, "Alpha › All movies"), "breadcrumb");
        assert_eq!(h.state().s.browser.cat_rows.iter().filter(|c| matches!(c, Cat::Group(_))).count(), 1, "Alpha's categories");
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down); // Favourites
        h.run_steps(1);
        assert_eq!(h.state().s.browser.shown.len(), 1, "Alpha's favourite only");

        // mouse: a tab picks its playlist; a right click opens its menu
        h.get_by_label("Bravo").click();
        h.run_steps(1);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.playlist_scope.movies, Some(b));
        h.get_by_label("Alpha").click_secondary();
        h.run_steps(1);
        assert!(h.state_mut().nav().contains(&NavRequest::Open(OverlayRequest::PlaylistMenu(a, ScopeKind::Movies))));
        // Alt+P: the picker
        h.state_mut().act(Action::PlaylistPicker);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::PlaylistPicker(ScopeKind::Movies))]);

        // a saved playlist without movies: All
        h.state_mut().t.config_edits.push(Box::new(move |c| c.interface.playlist_scope.movies = Some(free)));
        h.state_mut().apply_config();
        h.run_steps(2);
        assert_eq!(h.state().s.browser.shown.len(), 2, "Favourites of every playlist");
    }

    /// Two playlists sharing "Dune" (DESIGN §29.14): Alpha has it in English and French, Bravo
    /// in English; Heat is Alpha's only, Solo Bravo's. Returns (Alpha, Bravo).
    fn dune(t: &mut crate::ui::testing::TestUi) -> (SourceId, SourceId) {
        seed(t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("FR ★ Dune - 2021", "Dune (FR)"), ("EN ★ Heat - 1995", "Heat")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune"), ("EN - Solo (2018)", "Solo")])?;
            Ok((a, b))
        })
    }

    fn named(s: SourceId, i: usize) -> crate::model::ItemRef {
        crate::model::ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) }
    }

    fn shown_names(h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>) -> Vec<String> {
        let app = h.state();
        app.s.browser.shown.iter().map(|i| app.t.lib.vod.movies[*i as usize].name.to_string()).collect()
    }

    /// All playlists: the three copies of Dune are one stack card (its top, the first English
    /// copy, named with both playlists and its count) where its newest copy sorts; Space fans the
    /// copies out after it (English before French, each with its language) and Esc folds them;
    /// the badge does the same with the mouse; Ctrl+O lists every copy with the one that plays
    /// first marked and plays the one picked; "Show each copy" lists every copy; inside one
    /// playlist nothing stacks and a title also in Bravo says "+1".
    #[test]
    fn stacks_in_all_fan_out_fold_and_play_from() {
        let (mut t, one) = rig();
        let (a, _) = dune(&mut t);
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        assert_eq!(shown_names(&h), ["Heat", "Dune", "Solo"], "one card per title; Dune where its newest copy sorts");
        assert!(has_label(&h, "Dune · Alpha + Bravo · 3 copies"), "the stack card names both playlists and its count");
        let all = h.state_mut().t.with_ctx(|ctx| crate::ui::scope::Scope::current(ctx, crate::ui::scope::ScopeKind::Movies).count(ctx.lib, None));
        assert_eq!(all, 3, "the scope bar's All counts cards, not the five copies");
        // Space on the stack card: the copies follow it; Esc folds them. The hint line says so.
        h.state_mut().act(Action::Right);
        h.run_steps(1);
        let line = crate::keymap::hint_line_with(&h.state().s.key_contexts(), &h.state().s.hint_extras());
        assert!(line.contains("Space copies · Ctrl+O play from"), "{line}");
        assert!(h.state_mut().act(Action::FanOut));
        h.run_steps(1);
        assert_eq!(shown_names(&h), ["Heat", "Dune", "Dune", "Dune (FR)", "Solo"]);
        assert!(has_label(&h, "Dune · Bravo · EN") && has_label(&h, "Dune (FR) · Alpha · FR"), "each copy names its language");
        assert!(has_label(&h, "Dune · Alpha · 3 copies · EN"), "the head card too, with the stack's count");
        h.state_mut().act(Action::Right); // onto a fanned-out copy: Space folds its stack too
        h.state_mut().act(Action::FanOut);
        assert_eq!(shown_names(&h).len(), 3);
        assert_eq!(h.state().s.browser.grid.cursor, 1, "the cursor stays on the stack");
        h.state_mut().act(Action::FanOut);
        assert!(h.state_mut().act(Action::Back), "Esc folds first");
        assert_eq!(shown_names(&h).len(), 3);
        // the mouse: the badge fans out and folds
        h.run_steps(1);
        h.get_by_label("3 copies — show each").click();
        h.run_steps(2);
        assert_eq!(shown_names(&h).len(), 5);
        h.get_by_label("3 copies — fold them").click();
        h.run_steps(2);
        assert_eq!(shown_names(&h).len(), 3);
        // Ctrl+O: every copy, the first English copy plays first; picking Bravo's plays it
        h.state_mut().nav();
        h.state_mut().s.browser.grid.select(1);
        h.state_mut().act(Action::PlayFrom);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::ContextMenu(m))] = nav.as_slice() else { panic!("{nav:?}") };
        let labels: Vec<(&str, Option<&str>)> = m.items.iter().map(|i| (i.label.as_str(), i.shortcut.as_deref())).collect();
        assert_eq!(labels, [("Alpha · EN", Some("plays first")), ("Bravo · EN", None), ("Alpha · FR", None)]);
        let bravo = m.items[1].action;
        h.state_mut().result(OverlayResult::Chosen(bravo));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        // "Show each copy": every copy is a card
        h.state_mut().t.config_edits.push(Box::new(|c| c.interface.same_title = crate::config::SameTitle::ShowEach));
        h.state_mut().apply_config();
        h.run_steps(1);
        assert_eq!(shown_names(&h).len(), 5);
        h.state_mut().t.config_edits.push(Box::new(|c| c.interface.same_title = crate::config::SameTitle::Combine));
        h.state_mut().apply_config();
        // one playlist: Alpha's three movies, nothing stacked; its Dunes are also in Bravo
        h.state_mut().act(Action::NextPlaylist);
        h.state_mut().apply_config();
        h.run_steps(2);
        assert_eq!(h.state().t.config.interface.playlist_scope.movies, Some(a));
        assert_eq!(shown_names(&h), ["Heat", "Dune (FR)", "Dune"]);
        assert!(has_label(&h, "Dune · +1") && has_label(&h, "Dune (FR) · +1") && has_label(&h, "Heat"), "+1 = also in Bravo");
        h.state_mut().s.browser.grid.select(2);
        h.state_mut().act(Action::FanOut);
        assert_eq!(shown_names(&h).len(), 3, "nothing to fan out inside one playlist");
    }

    /// Locked copies stay in their stack, drawn disabled while the session is locked: the top
    /// card is the first unlocked copy; a stack whose every copy is locked shows locked and asks
    /// for the PIN. Hidden copies and copies the visibility hook rejects (explicit content) are
    /// not in the stack at all.
    #[test]
    fn stacks_keep_locked_copies_and_drop_hidden_and_rejected_ones() {
        let (mut t, one) = rig();
        let (a, b) = dune(&mut t);
        with_pin(&mut t);
        let set = |t: &mut crate::ui::testing::TestUi, item: crate::model::ItemRef, ov: crate::model::ItemOverride| {
            seed(t, move |c| crate::store::user::set_item_override(c, &item, &ov))
        };
        let locked = crate::model::ItemOverride { locked: true, ..Default::default() };
        set(&mut t, named(a, 0), locked.clone());
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        assert!(has_label(&h, "Dune · Bravo + Alpha · 3 copies"), "Alpha's English copy is locked: Bravo's is the card");
        h.state_mut().s.browser.grid.select(1);
        h.state_mut().act(Action::FanOut);
        h.run_steps(1);
        assert!(has_label(&h, "Dune · Alpha · EN · locked"), "the locked copy stays, drawn disabled");
        h.state_mut().act(Action::Back);
        // every copy locked: the card is locked and Enter asks for the PIN
        set(&mut h.state_mut().t, named(a, 1), locked.clone());
        set(&mut h.state_mut().t, named(b, 0), locked);
        h.run_steps(2);
        assert!(has_label_containing(&h, "· 3 copies · locked"));
        h.state_mut().s.browser.grid.select(1);
        h.state_mut().nav();
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]));
        // a hidden copy leaves the stack (and the snapshot)
        let hidden = crate::model::ItemOverride { hidden: true, locked: true, ..Default::default() };
        set(&mut h.state_mut().t, named(a, 1), hidden);
        h.run_steps(2);
        assert!(has_label_containing(&h, "Dune · Alpha + Bravo · 2 copies · locked"), "the French copy left the stack");
        // explicit content hidden by parental control drops a copy the same way (DESIGN §20.3)
        let bravo_dune = named(b, 0);
        seed(&mut h.state_mut().t, move |c| {
            c.execute(
                "UPDATE movie SET explicit = 1 WHERE source_id = ?1 AND key = ?2",
                rusqlite::params![bravo_dune.source, bravo_dune.key.as_str()],
            )?;
            Ok(())
        });
        let reader = h.state().t.services().store.reader().unwrap();
        h.state_mut().t.set_library(crate::store::library::load_visible(&reader, crate::store::library::Visibility::NO_EXPLICIT).unwrap());
        h.run_steps(2);
        assert_eq!(shown_names(&h), ["Heat", "Solo", "Dune"], "Bravo's copy is explicit: Alpha's English copy stands alone");
        assert!(!has_label_containing(&h, "copies"));
    }

    /// A narrow window (790 pt) with long playlist names: the stack card chips the playlists'
    /// codes ("AP", "BSE"), never their names, and its count badge is still there to click.
    #[test]
    fn a_narrow_stack_card_chips_its_playlists_and_keeps_its_badge() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let a = testkit::source(c, "Alphaville Premium")?;
            let b = testkit::source(c, "Bravo Sports Extra")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("FR ★ Dune - 2021", "Dune (FR)")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(790.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        let painted = testkit::painted(&h);
        assert!(painted.iter().any(|p| p == "AP") && painted.iter().any(|p| p == "BSE"), "{painted:?}");
        // the names stay on the playlist tabs only, the card's meta line is the year
        assert_eq!(painted.iter().filter(|p| p.contains("Alphaville") || p.contains("Bravo Sports")).count(), 2, "{painted:?}");
        assert!(painted.iter().any(|p| p == "2021"), "{painted:?}");
        assert!(has_label(&h, "3 copies — show each"), "the badge");
    }

    /// Ctrl+H, Ctrl+L and F2 on a stack card cover every copy (DESIGN §29.15): Hide takes the
    /// title out of All, the toast naming its playlists, and its Undo brings every copy back;
    /// Lock locks them all; F2 renames every copy.
    #[test]
    fn hide_lock_and_rename_on_a_stack_card_cover_every_copy() {
        let (mut t, one) = rig();
        let (a, b) = dune(&mut t);
        with_pin(&mut t);
        let p = t.config.parental.clone();
        assert!(t.parental.unlock("1234", &p));
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        h.state_mut().act(Action::Right); // Dune
        let ov = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>, r: crate::model::ItemRef| {
            crate::store::user::item_override(&h.state().t.services().store.reader().unwrap(), &r).unwrap().unwrap_or_default()
        };
        let copies = [named(a, 0), named(a, 1), named(b, 0)];
        h.state_mut().act(Action::ToggleHidden);
        until(&mut h, "every copy hidden", |h| copies.iter().all(|r| ov(h, r.clone()).hidden));
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Hid Dune in Alpha and Bravo — Settings ▸ Hidden items shows it again");
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        assert_eq!(shown_names(&h), ["Heat", "Solo"], "the title is gone, not just its top copy");
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "every copy back", |h| copies.iter().all(|r| !ov(h, r.clone()).hidden));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        h.state_mut().s.browser.grid.select(1);
        h.state_mut().act(Action::ToggleLock);
        until(&mut h, "every copy locked", |h| copies.iter().all(|r| ov(h, r.clone()).locked));
        // F2: the rename dialog for every copy (`dialogs` tests what it writes)
        h.state_mut().nav();
        h.state_mut().act(Action::Rename);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::Rename(spec))] = nav.as_slice() else { panic!("{nav:?}") };
        let crate::ui::overlays::RenameTarget::Copies { items } = &spec.target else { panic!("{spec:?}") };
        assert_eq!((spec.title.as_str(), items.len()), ("Rename every copy", 3));
    }

    /// Only titles with progress or in Favourites are looked up by key (indexing every title
    /// cloned every key: ~30 MiB for 300k movies); a favourite added since the library was
    /// built, and progress that arrives later, are resolved too.
    #[test]
    fn only_titles_with_progress_or_favourites_are_indexed_by_key() {
        let (mut t, one) = rig();
        let (a, b) = dune(&mut t);
        seed(&mut t, move |c| {
            let p = crate::model::WatchProgress {
                item: named(b, 1),
                position_s: 60.0,
                duration_s: Some(600.0),
                updated_at: 5,
                finished: false,
                title: "Solo".into(),
                fingerprint: Default::default(),
            };
            crate::store::user::save_progress(c, &p)
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        let keys = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>| {
            let mut k: Vec<String> = h.state().s.browser.index.keys().map(|(s, k)| format!("{}:{}", s.0, k.0)).collect();
            k.sort();
            k
        };
        until(&mut h, "the progress loaded", |h| !h.state().s.browser.progress_idx.is_empty());
        assert_eq!(keys(&h), [format!("{}:m3u:named0001", b.0)], "the title with progress only");
        let solo = h.state().t.lib.vod.movies.iter().position(|v| v.name.as_ref() == "Solo").unwrap() as u32;
        assert_eq!(h.state().s.browser.progress_idx.get(&solo), Some(&0.1));
        // a favourite toggled without a library rebuild
        let app = h.state_mut();
        let list = app.t.favs.default_list().unwrap();
        app.t.favs.apply_toggle(list, &named(a, 2), true);
        h.run_steps(1);
        assert_eq!(keys(&h), [format!("{}:m3u:named0002", a.0), format!("{}:m3u:named0001", b.0)]);

        // a zap copies the library with another recent list and shares the VOD lists: nothing
        // is rescanned (review: every zap scanned every title)
        let (scans, resolved) = (h.state().s.browser.index_scans, h.state().s.browser.index_for.clone());
        let app = h.state_mut();
        app.t.lib = std::sync::Arc::new(app.t.lib.with_recent_channels(Vec::new()));
        h.run_steps(2);
        assert_eq!(h.state().s.browser.index_for, resolved);
        assert_eq!(h.state().s.browser.index_scans, scans, "a zap rescans nothing");
        // progress reloaded (saved every 10 s while a title plays) with no new key: remapped,
        // not rescanned
        let generation = h.state().s.browser.progress_gen;
        let app = h.state_mut();
        let s = &mut app.s;
        app.t.with_ctx(|ctx| s.on_event(ctx, &AppEvent::Data(DataScope::Progress)));
        until(&mut h, "the progress reloaded", |h| h.state().s.browser.progress_gen > generation);
        h.run_steps(1);
        assert_eq!(h.state().s.browser.index_scans, scans, "a progress save rescans nothing");
        assert_eq!(h.state().s.browser.progress_idx.get(&solo), Some(&0.1));
        // a rebuild has new lists: scanned again
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        assert_eq!(h.state().s.browser.index_scans, scans + 1);
    }

    /// Favourites and Continue watching of a stack (DESIGN §29.12): Favourites shows one card
    /// for the title; Ctrl+F on it takes every favourite copy out with an Undo that puts them
    /// back, and on a stack without favourites adds the copy that plays first; Delete in
    /// Continue watching forgets every copy's progress, and Undo restores the rows.
    #[test]
    fn stack_favourites_and_continue_watching_cover_every_copy_with_undo() {
        let (mut t, one) = rig();
        let (a, b) = dune(&mut t);
        seed(&mut t, move |c| {
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &named(a, 1), true, 1)?;
            crate::store::user::set_favourite(c, list, &named(b, 0), true, 2)?;
            for (i, item) in [named(a, 0), named(b, 0)].into_iter().enumerate() {
                let p = crate::model::WatchProgress {
                    item,
                    position_s: 600.0,
                    duration_s: Some(6000.0),
                    updated_at: 10 + i as i64,
                    finished: false,
                    title: "Dune".into(),
                    fingerprint: Default::default(),
                };
                crate::store::user::save_progress(c, &p)?;
            }
            Ok(())
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down); // Favourites
        h.run_steps(1);
        assert_eq!(shown_names(&h), ["Dune"], "two favourite copies, one card");
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::ToggleFavourite);
        let fav = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>, r: crate::model::ItemRef| h.state().t.favs.is_fav(&r);
        assert!(!fav(&h, named(a, 1)) && !fav(&h, named(b, 0)), "every copy left Favourites");
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Removed from Favourites in Alpha and Bravo");
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "Undo puts them back", |h| {
            let favs = crate::store::library::load_favs(&h.state().t.services().store.reader().unwrap()).unwrap();
            favs.is_fav(&named(a, 1)) && favs.is_fav(&named(b, 0))
        });
        let favs = crate::store::library::load_favs(&h.state().t.services().store.reader().unwrap()).unwrap();
        h.state_mut().t.favs = favs;
        // (the frames `until` stepped while the Undo was being written showed an empty
        // Favourites list: wait for the card to be back before acting on it)
        until(&mut h, "the restored favourites listed", |h| shown_names(h) == ["Dune"]);
        // Ctrl+F again: out again (no Undo taken); then on the stack in All, with no favourite
        // copy, Ctrl+F adds the copy that plays first — Bravo's, the one watched last
        h.state_mut().act(Action::ToggleFavourite);
        assert!(!fav(&h, named(a, 1)) && !fav(&h, named(b, 0)), "out again");
        until(&mut h, "the removal written", |h| {
            let favs = crate::store::library::load_favs(&h.state().t.services().store.reader().unwrap()).unwrap();
            !favs.is_fav(&named(a, 1)) && !favs.is_fav(&named(b, 0))
        });
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Home);
        h.run_steps(1);
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Right); // All movies: Dune
        h.state_mut().act(Action::ToggleFavourite);
        assert!(fav(&h, named(b, 0)) && !fav(&h, named(a, 0)) && !fav(&h, named(a, 1)), "the copy that would play");
        // Continue watching: one card (the copy played last); Delete forgets both copies
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Down);
        h.run_steps(1);
        until(&mut h, "Continue watching", |h| shown_names(h) == ["Dune"]);
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Remove);
        let progress = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>, r: crate::model::ItemRef| {
            crate::store::user::progress(&h.state().t.services().store.reader().unwrap(), &r).unwrap()
        };
        until(&mut h, "both copies forgotten", |h| progress(h, named(a, 0)).is_none() && progress(h, named(b, 0)).is_none());
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Removed from Continue watching");
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "the rows restored", |h| {
            progress(h, named(a, 0)).is_some_and(|p| p.position_s == 600.0) && progress(h, named(b, 0)).is_some_and(|p| p.updated_at == 11)
        });
    }

    /// The Alt+P picker: digits jump (0 = All, 1… = the playlists in order) while nothing is
    /// typed, typing filters by name and Enter picks; the choice is saved for its screen.
    #[test]
    fn the_playlist_picker_jumps_by_digit_and_filters_by_name() {
        use crate::ui::overlays::playlists::PlaylistPickerOverlay;
        use crate::ui::screens::home::testkit::overlay_harness;
        let (mut t, one) = rig();
        let (_, b) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            testkit::series(c, a, 1, 1, 1)?;
            testkit::series(c, b, 1, 1, 1)?;
            Ok((a, b))
        });
        let saved = |h: &mut egui_kittest::Harness<'static, testkit::OvApp<PlaylistPickerOverlay>>| {
            let mut c = (*h.state().t.config).clone();
            for e in h.state_mut().t.config_edits.drain(..) {
                e(&mut c);
            }
            c.interface.playlist_scope
        };
        let mut h = overlay_harness(t, one, PlaylistPickerOverlay::new(ScopeKind::Series), egui::vec2(1000.0, 700.0));
        assert!(has_label(&h, "All playlists") && has_label(&h, "Alpha") && has_label(&h, "Bravo"));
        // typing filters ("b" leaves Bravo); a digit types while filtering
        assert_eq!(h.state_mut().act(Action::FilterInput('b')), "Keep");
        assert_eq!(h.state_mut().act(Action::FilterInput('2')), "Keep", "filtering: digits type");
        h.state_mut().act(Action::FilterBackspace);
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(saved(&mut h).series, Some(b));
        // digits jump while nothing is typed: 0 = All, 1… = the playlists in order
        h.state_mut().o = PlaylistPickerOverlay::new(ScopeKind::Series);
        h.run_steps(1);
        assert_eq!(h.state_mut().act(Action::FilterInput('9')), "Keep", "no ninth playlist");
        assert_eq!(h.state_mut().act(Action::FilterInput('2')), "Close");
        assert_eq!(saved(&mut h).series, Some(b), "2 = the second playlist");
        h.state_mut().o = PlaylistPickerOverlay::new(ScopeKind::Series);
        assert_eq!(h.state_mut().act(Action::FilterInput('0')), "Close");
        assert_eq!(saved(&mut h).series, None, "0 = All playlists");
    }

    /// Custom order works for titles: Alt+↑ in a sorted grid switches to Custom order and
    /// moves the title there (also in the frames before the config edit and the library
    /// rebuild land), the order is stored per title and survives the library rebuild, and the
    /// cursor follows the title. A move inside a category swaps the pair in the playlist's
    /// whole order without reshuffling "All".
    #[test]
    fn titles_reorder_in_custom_order() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 5)
        });
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        let shown = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>| {
            let app = h.state();
            let all = &app.t.lib.vod.movies;
            app.s.browser.shown.iter().map(|i| all[*i as usize].name.to_string()).collect::<Vec<_>>()
        };
        let cursor = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>| shown(h)[h.state().s.browser.grid.cursor].clone();
        assert_eq!(cursor(&h), "Movie 4", "recently added first");
        // Alt+↑: custom order (provider order 0..4), Movie 4 moves before Movie 3
        // (the app runs the frame before it applies the config edit)
        h.state_mut().act(Action::MoveUp);
        h.run_steps(1);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.movie_sort, SortMode::Provider);
        assert_eq!(shown(&h), ["Movie 0", "Movie 1", "Movie 2", "Movie 4", "Movie 3"], "shown at once");
        assert_eq!(cursor(&h), "Movie 4");
        h.run_steps(1);
        assert_eq!(shown(&h), ["Movie 0", "Movie 1", "Movie 2", "Movie 4", "Movie 3"], "before the library has it");
        assert_eq!(cursor(&h), "Movie 4");
        let order = |h: &egui_kittest::Harness<'_, testkit::App<MoviesScreen>>, key: &str| {
            crate::store::user::item_overrides(&h.state().t.services().store.reader().unwrap(), None)
                .unwrap()
                .into_iter()
                .find(|r| r.item.key.0 == key)
                .and_then(|r| r.ov.user_order)
        };
        until(&mut h, "the stored order", |h| order(h, "m3u:movie0000000004") == Some(3) && order(h, "m3u:movie0000000003") == Some(4));
        // a new library (the app rebuilds it on Overrides): the order holds, the cursor too
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        assert_eq!(shown(&h), ["Movie 0", "Movie 1", "Movie 2", "Movie 4", "Movie 3"], "after the rebuild");
        assert_eq!(cursor(&h), "Movie 4");
        // Alt+↑ again, now in custom order: no toast, one more place
        h.state_mut().t.toasts.clear();
        h.state_mut().act(Action::MoveUp);
        assert_eq!(shown(&h), ["Movie 0", "Movie 1", "Movie 4", "Movie 2", "Movie 3"]);
        assert!(h.state().t.toasts.is_empty(), "{:?}", h.state().t.toasts);
        until(&mut h, "the second move", |h| order(h, "m3u:movie0000000004") == Some(2));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        assert_eq!(shown(&h), ["Movie 0", "Movie 1", "Movie 4", "Movie 2", "Movie 3"]);
        assert_eq!(cursor(&h), "Movie 4");
        // a category with Movies 0, 4, 3: Movie 3 up swaps it with Movie 4 everywhere, and
        // Movies 1 and 2 keep their places in "All"
        let picks = seed(&mut h.state_mut().t, |c| {
            c.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) SELECT id,'movie','movie:picks','Picks',1,3,1 FROM source", [])?;
            let g = c.last_insert_rowid();
            c.execute("INSERT INTO movie_group(group_id, movie_id, position) SELECT ?1, id, 0 FROM movie WHERE key IN ('m3u:movie0000000000', 'm3u:movie0000000003', 'm3u:movie0000000004')", [g])?;
            Ok(GroupId(g))
        });
        h.state_mut().enter(Route::Movies { group: Some(picks) });
        h.run_steps(2);
        assert_eq!(shown(&h), ["Movie 0", "Movie 4", "Movie 3"]);
        h.state_mut().s.browser.grid.select(2);
        h.state_mut().act(Action::MoveUp);
        assert_eq!(shown(&h), ["Movie 0", "Movie 3", "Movie 4"]);
        assert_eq!(cursor(&h), "Movie 3");
        until(&mut h, "the category move", |h| order(h, "m3u:movie0000000003") == Some(2));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        let all: Vec<String> = h.state().t.lib.vod.movies.iter().map(|v| v.name.to_string()).collect();
        assert_eq!(all, ["Movie 0", "Movie 1", "Movie 3", "Movie 2", "Movie 4"]);
        assert_eq!(shown(&h), ["Movie 0", "Movie 3", "Movie 4"]);
    }

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
        assert!(has_label(&h, "Sort: Recently added ▾"));
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
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.title == "Sort by" && m.items.len() == 6),
            "{nav:?}"
        );
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

    /// Seed: 6 movies in "Movies" plus a second category "Drama" (movies 0 and 1).
    fn two_categories(t: &mut crate::ui::testing::TestUi) -> SourceId {
        seed(t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::movies(c, s, 6)?;
            c.execute(
                "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'movie','movie:drama','Drama',1,2,1)",
                [s],
            )?;
            let g = c.last_insert_rowid();
            for (i, id) in ids[..2].iter().enumerate() {
                c.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, ?3)", rusqlite::params![g, id.0, i as i64])?;
            }
            Ok(s)
        })
    }

    fn group_ov(t: &crate::ui::testing::TestUi, key: &str) -> Option<crate::model::GroupOverride> {
        crate::store::user::group_overrides(&t.services().store.reader().unwrap()).unwrap().into_iter().find(|r| r.key.0 == key).map(|r| r.ov)
    }

    fn with_pin(t: &mut crate::ui::testing::TestUi) {
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
        };
        t.config = std::sync::Arc::new(cfg);
    }

    /// The category shortcuts listed in Help (lock, rename, hide, reorder, menu), and the
    /// grid's Alt+↑/↓ switching a sorted grid to Custom order (DESIGN §13.0).
    #[test]
    fn category_and_grid_shortcuts() {
        let (mut t, one) = rig();
        two_categories(&mut t);
        with_pin(&mut t);
        let p = t.config.parental.clone();
        assert!(t.parental.unlock("1234", &p));
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        // Tab to the categories; ↓↓↓ to "Movies" (after All, Continue watching, Favourites)
        h.state_mut().act(Action::FocusNext);
        for _ in 0..3 {
            h.state_mut().act(Action::Down);
        }
        h.state_mut().act(Action::ToggleLock);
        until(&mut h, "the category lock", |h| group_ov(&h.state().t, "movie:movies").is_some_and(|o| o.locked));
        h.state_mut().act(Action::MoveDown);
        until(&mut h, "the category order", |h| {
            let t = &h.state().t;
            group_ov(t, "movie:movies").and_then(|o| o.user_order) == Some(1) && group_ov(t, "movie:drama").and_then(|o| o.user_order) == Some(0)
        });
        assert!(group_ov(&h.state().t, "movie:movies").is_some_and(|o| o.locked), "reordering keeps the lock");
        h.state_mut().act(Action::Rename);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Rename(r))] if matches!(r.target, crate::ui::overlays::RenameTarget::Group { .. })),
            "{nav:?}"
        );
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.action == Action::ToggleHidden)),
            "{nav:?}"
        );
        // a built-in category can't be locked
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::ToggleLock);
        assert!(h.state_mut().nav().is_empty());
        assert!(h.state().t.toasts.iter().any(|t| t.text.contains("Built-in categories")), "{:?}", h.state().t.toasts);
        // the grid: Alt+↓ on a sorted grid switches to Custom order (with a toast)
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::MoveDown);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.movie_sort, SortMode::Provider);
        assert!(h.state().t.toasts.iter().any(|t| t.text == "Switched to custom order"));
        h.run_steps(1);
        // Ctrl+I opens the page of the title under the cursor; Ctrl+H hides it
        h.run_steps(1);
        h.state_mut().act(Action::ShowDetails);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Push(Route::MovieDetail(_))]));
        h.state_mut().act(Action::ToggleHidden);
        until(&mut h, "a hidden title", |h| {
            crate::store::user::item_overrides(&h.state().t.services().store.reader().unwrap(), None)
                .unwrap()
                .iter()
                .any(|r| r.item.kind == ItemKind::Movie && r.ov.hidden)
        });
    }

    /// DESIGN §28.7 in the categories pane: Space marks (and moves down), Ctrl+H hides every
    /// marked category; Esc clears the marks first.
    #[test]
    fn categories_bulk_hide() {
        let (mut t, one) = rig();
        two_categories(&mut t);
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        h.state_mut().act(Action::FocusNext);
        for _ in 0..3 {
            h.state_mut().act(Action::Down);
        }
        // Movies, then Drama
        h.state_mut().act(Action::ToggleMark);
        h.state_mut().act(Action::ToggleMark);
        h.run_steps(1);
        assert!(has_label(&h, "2 marked · Ctrl+H hides · Esc clears"));
        assert!(h.state_mut().act(Action::Back), "Esc clears the marks");
        h.run_steps(1);
        assert!(!has_label(&h, "2 marked · Ctrl+H hides · Esc clears"));
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::ToggleMark);
        h.state_mut().act(Action::ToggleMark);
        h.state_mut().act(Action::ToggleHidden);
        until(&mut h, "both hidden", |h| ["movie:movies", "movie:drama"].iter().all(|k| group_ov(&h.state().t, k).is_some_and(|o| o.hidden)));
    }

    /// DESIGN §12.1 density: 4–5 columns at a tiled half (the categories in a drawer), 7+ at
    /// full screen; the sort button never covers the filter header; only the focused pane
    /// shows the poster cursor.
    #[test]
    fn grid_density_header_layout_and_one_cursor() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 40)
        });
        // content width = window − sidebar − 2 × panel padding (the harness has no chrome)
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(706.0, 900.0));
        for (window, content, scale, cols) in [(790.0, 706.0, 1.0, 4..=5), (1600.0, 1380.0, 1.0, 7..=9), (1600.0, 1380.0, 4.0 / 3.0, 4..=6)] {
            let base = crate::theme::Theme::fallback();
            let theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.set_size(egui::vec2(content, 900.0));
            let app = h.state_mut();
            app.t.layout = LayoutClass::from_width(window, theme.rem());
            app.t.theme = theme;
            app.s = MoviesScreen::default();
            app.enter(Route::Movies { group: None });
            h.run_steps(3);
            let n = h.state().s.browser.grid.columns;
            assert!(cols.contains(&n), "{window} pt × {scale}: {n} columns");
            let header = h.query_all_by_label_contains("to filter").next().expect("the filter header").rect();
            let sort = h.query_all_by_label_contains("Sort: ").next().expect("the sort button").rect();
            assert!(!header.intersects(sort.shrink(1.0)), "{window} pt × {scale}: header {header:?} under the sort button {sort:?}");
            assert!(sort.right() <= content + 1.0);
        }
        // one cursor: with the categories focused the poster cursor (2 px accent) is gone
        let accent_outlines = |h: &egui_kittest::Harness<'static, testkit::App<MoviesScreen>>| {
            let accent = h.state().t.theme.tokens.accent;
            fn walk(s: &egui::Shape, accent: egui::Color32, n: &mut usize) {
                match s {
                    egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, accent, n)),
                    egui::Shape::Rect(r) if r.stroke.width == 2.0 && r.stroke.color == accent => *n += 1,
                    _ => {}
                }
            }
            let mut n = 0;
            h.output().shapes.iter().for_each(|c| walk(&c.shape, accent, &mut n));
            n
        };
        h.run_steps(1);
        assert_eq!(accent_outlines(&h), 1, "the grid has the keyboard: one cursor");
        h.state_mut().act(Action::FocusNext);
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::List]);
        assert_eq!(accent_outlines(&h), 0, "the categories have it: the poster shows 'selected' only");
    }

    /// Spatial panes (DESIGN §15.1) at Medium: ← inside a row moves between posters; on the
    /// first column it opens the categories drawer — also while the grid filters (the filter is
    /// kept) and on an empty grid.
    #[test]
    fn left_on_the_first_column_opens_the_categories_while_filtering_and_when_empty() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 30).map(|_| ())
        });
        t.layout = LayoutClass::Medium;
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(706.0, 800.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        h.state_mut().act(Action::FilterInput('2'));
        h.run_steps(1);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGridFiltering]);
        let n = h.state().s.browser.shown.len();
        assert!(n > 1, "{n}");
        h.state_mut().act(Action::Right);
        assert!(h.state_mut().act(Action::Left));
        assert_eq!(h.state().s.browser.grid.cursor, 0, "inside the row: the poster on the left");
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGridFiltering]);
        assert!(!has_label(&h, "All movies"), "the drawer is closed");
        assert!(h.state_mut().act(Action::Left));
        h.run_steps(1);
        assert!(has_label(&h, "All movies"), "first column: the drawer opens");
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::List]);
        assert_eq!((h.state().s.browser.grid.filter.query(), h.state().s.browser.shown.len()), ("2", n), "the filter is kept");
        // Esc closes the drawer: the same filtered grid
        assert!(h.state_mut().act(Action::Back));
        h.run_steps(1);
        assert!(!has_label(&h, "All movies"));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGridFiltering]);
        // nothing matches: ← still leads to the categories
        for c in "zzz".chars() {
            h.state_mut().act(Action::FilterInput(c));
        }
        h.run_steps(1);
        assert!(h.state().s.browser.shown.is_empty());
        assert!(h.state_mut().act(Action::Left));
        h.run_steps(1);
        assert!(has_label(&h, "All movies"), "an empty grid: the drawer opens");
    }

    /// Medium: the categories are a drawer (← opens it, Esc closes it); Compact: Esc in the
    /// grid goes back to the categories. A cancelled category PIN doesn't hijack a later
    /// title PIN.
    #[test]
    fn drawer_compact_back_and_a_stale_category_pin() {
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::movies(c, s, 4)?;
            crate::store::user::set_group_override(
                c,
                s,
                ItemKind::Movie,
                &crate::model::GroupKey("movie:movies".into()),
                &crate::model::GroupOverride { locked: true, ..Default::default() },
            )?;
            let item = crate::model::ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000003".into()) };
            crate::store::user::set_item_override(c, &item, &crate::model::ItemOverride { locked: true, ..Default::default() })?;
            Ok(ids)
        });
        with_pin(&mut t);
        t.layout = LayoutClass::Medium;
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(706.0, 800.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        assert!(!has_label(&h, "All movies"), "Medium: the drawer starts closed");
        assert!(h.state_mut().act(Action::Parent));
        h.run_steps(1);
        assert!(has_label(&h, "All movies"), "Backspace opens the drawer");
        // the locked category asks for the PIN; the prompt is cancelled (Esc: no result)
        for _ in 0..3 {
            h.state_mut().act(Action::Down);
        }
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]));
        assert!(h.state_mut().act(Action::Back), "Esc closes the drawer first");
        h.run_steps(1);
        assert!(!has_label(&h, "All movies"));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid]);
        // later (All movies), a locked title (Movie 3, newest first) asks for the PIN: the
        // answer opens it
        for a in [Action::Parent, Action::Home, Action::Activate, Action::Home, Action::Activate] {
            h.state_mut().act(a);
            h.run_steps(1);
        }
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]));
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        app.result(OverlayResult::Unlocked(Action::Activate));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(ids[3]))], "the title, not the stale category");
        // Compact: Esc in the grid steps back to the categories, then leaves
        h.state_mut().t.layout = LayoutClass::Compact;
        h.run_steps(1);
        assert!(h.state_mut().act(Action::Back));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::List]);
        assert!(!h.state_mut().act(Action::Back), "the app's Back leaves the screen");
    }

    /// Below Wide, the "{category} ▾" button leads to the categories and one click on a category
    /// opens it with its titles: Compact turns to the grid, the Medium drawer closes.
    #[test]
    fn categories_open_with_one_click_below_wide() {
        let (mut t, one) = rig();
        two_categories(&mut t);
        t.layout = LayoutClass::Compact;
        let mut h = harness(t, one, MoviesScreen::default(), egui::vec2(420.0, 800.0));
        h.state_mut().enter(Route::Movies { group: None });
        h.run_steps(2);
        h.get_by_label("All movies ▾").click();
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::List], "the categories pane");
        h.get_by_label("Drama").click();
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid], "one click: the grid");
        assert!(has_label(&h, "Movie 1") && !has_label(&h, "Movie 4"), "of Drama");
        // a double-click (the old way to open one): its second click lands on a poster that
        // just appeared under the pointer, and opens nothing
        h.get_by_label("Drama ▾").click();
        // (long enough for that click not to make the next two a triple-click)
        h.run_steps(40);
        let row = h.get_by_label("Drama").rect();
        let at = egui::pos2(row.left() + row.width() / 4.0, row.center().y);
        h.state_mut().nav();
        h.hover_at(at);
        for _ in 0..2 {
            for pressed in [true, false] {
                h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
            }
            // frames between the clicks (the grid shows under the pointer), within the
            // double-click time
            h.run_steps(2);
        }
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid], "the grid");
        assert!(h.state_mut().nav().is_empty(), "no title opened");
        h.state_mut().t.layout = LayoutClass::Medium;
        h.run_steps(2);
        h.get_by_label("Drama ▾").click();
        h.run_steps(2);
        assert!(has_label(&h, "All movies"), "the drawer");
        h.get_by_label("All movies").click();
        h.run_steps(2);
        assert!(!has_label(&h, "All movies") && has_label(&h, "Movie 4"), "the drawer closed on All movies");
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
