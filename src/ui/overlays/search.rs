//! Global search overlay (`/`, Ctrl+K): launcher card, header is the query, sections
//! All · Live · Movies · Series · Guide (Tab cycles), results grouped by kind with source
//! badges (colour + glyph), match highlighting, live now/next on channel hits. Enter plays /
//! opens; Shift+Enter plays fullscreen / opens details. Owner: WP-12. DESIGN §13.7.
//!
//! * The fuzzy index is the app-owned `ctx.search_index` (kept across opens, reset by the app
//!   when the library changes); it is (re)built lazily here, also when the parental session
//!   opens or closes (locked items are excluded while locked, DESIGN §20.3).
//! * Guide results come from `search::EpgSearch` (FTS on a runtime task, debounced 150 ms,
//!   older searches never overwrite newer ones). The overlay only sends queries of 2+
//!   characters (a one-letter prefix scans most of the FTS index) and hides results that no
//!   longer match the text in the field while the newer search is still running, so a stale
//!   list never shows under a newer query.
//! * Enter: channels play FULLSCREEN, zapping through their primary group (DESIGN §13.0);
//!   movies/series open their page; guide hits watch (airing), catch up (past, archive) or set
//!   a reminder (future). Shift+Enter plays movies/series directly. Esc clears the query,
//!   then closes.
//! * Item shortcuts (`SEARCH_ITEM_ACTIONS`, listed in Help for `[Search]`) act on the highlighted
//!   hit: Ctrl+F favourite, Ctrl+Shift+F lists, Ctrl+G guide, Ctrl+I details / edit
//!   information, Ctrl+E assign EPG, Ctrl+W watched, F2 rename, Ctrl+H hide, Ctrl+L lock.
//!   The menu key (and a right-click on a hit) opens them as a menu, "Play" first; the menu
//!   answers this overlay (the app routes a menu's result to the overlay that opened it).
//!   Overlays opened from here (rename, lists, edit information) write by themselves; PIN
//!   prompts would answer the screen BELOW the search, so the overlay never opens them (a
//!   locked hit — only reachable when the session relocks while the card is open — toasts
//!   instead).
//! * Rows are rebuilt only when the query, section, lock state or library change, or when
//!   the fuzzy matcher / the guide search report new results — not every frame.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Align2, Rect, RichText};

use super::dialogs::{card_area, scrim, with_close};
use super::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, MediaRef, PlayOrigin, ProgrammeHit, ProgrammeSlim, VodKind};
use crate::search::{EpgSearch, SearchHit, SearchIndex, SearchSection, SearchTarget};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::screens::home::{kit, vod_ref};
use crate::ui::widgets::badge::{InlineBadge, job_with_quality};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::list::paint_row_state;
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, glyphs, section};
use crate::ui::{Route, UiCtx};

/// Hits per kind in "All", and in a single section.
const ALL_LIMIT: usize = 90;
const SECTION_LIMIT: usize = 200;
/// Guide hits shown in "All".
const ALL_GUIDE: usize = 12;
/// Shortest query sent to the EPG search.
const EPG_MIN_CHARS: usize = 2;
/// Launcher card width (pt × rem).
const CARD_W: f32 = 720.0;

thread_local! {
    /// `include_locked` of the last index build (the index lives in the app across opens).
    static BUILT_UNLOCKED: Cell<Option<bool>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, PartialEq)]
enum Row {
    Header(&'static str),
    Hit(SearchHit),
    Guide(ProgrammeHit),
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Header(_))
    }
}

pub struct SearchOverlay {
    query: String,
    section: SearchSection,
    /// Index into the selectable rows.
    cursor: usize,
    epg: Option<EpgSearch>,
    /// The query the EPG search was last given.
    epg_query: String,
    /// Rows the user sees (actions resolve against them).
    rows: Vec<Row>,
    /// (query, section, session unlocked, library build) the rows were built for.
    rows_key: Option<(String, SearchSection, bool, crate::model::Timestamp)>,
    /// Set by the guide search when new results landed (rows must be rebuilt).
    epg_dirty: Arc<AtomicBool>,
    /// Card top edge, frozen once typing starts (the card grows downward only).
    top: Option<f32>,
    last_top: Option<f32>,
    scroll_to_cursor: bool,
}

impl SearchOverlay {
    pub fn new(initial: String) -> Self {
        Self {
            query: initial,
            section: SearchSection::All,
            cursor: 0,
            epg: None,
            epg_query: String::new(),
            rows: Vec::new(),
            rows_key: None,
            epg_dirty: Arc::default(),
            top: None,
            last_top: None,
            scroll_to_cursor: false,
        }
    }

    fn selectable(&self) -> Vec<usize> {
        self.rows.iter().enumerate().filter(|(_, r)| r.selectable()).map(|(i, _)| i).collect()
    }

    fn current(&self) -> Option<Row> {
        self.selectable().get(self.cursor).map(|i| self.rows[*i].clone())
    }

    fn edit(&mut self, f: impl FnOnce(&mut String)) -> OverlayOutcome {
        f(&mut self.query);
        self.cursor = 0;
        self.scroll_to_cursor = true;
        if !self.query.is_empty() && self.top.is_none() {
            self.top = self.last_top;
        }
        OverlayOutcome::Keep
    }

    fn move_cursor(&mut self, delta: isize) -> OverlayOutcome {
        let n = self.selectable().len();
        if n > 0 {
            self.cursor = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
            self.scroll_to_cursor = true;
        }
        OverlayOutcome::Keep
    }

    fn cycle_section(&mut self, forward: bool) -> OverlayOutcome {
        let all = SearchSection::ALL;
        let i = all.iter().position(|s| *s == self.section).unwrap_or(0);
        self.section = all[if forward { (i + 1) % all.len() } else { (i + all.len() - 1) % all.len() }];
        self.cursor = 0;
        self.scroll_to_cursor = true;
        OverlayOutcome::Keep
    }

    /// Enter / Shift+Enter / click on a row.
    fn activate(&mut self, ctx: &mut UiCtx<'_>, row: Row, alt: bool) -> OverlayOutcome {
        match row {
            Row::Header(_) => OverlayOutcome::Keep,
            Row::Hit(h) => match h.item.target {
                SearchTarget::Channel(id) => {
                    if !kit::play_channel(ctx, id, None, PlayOrigin::Search) {
                        locked_toast(ctx);
                        return OverlayOutcome::Keep;
                    }
                    OverlayOutcome::Close
                }
                SearchTarget::Movie(id) if alt => {
                    crate::ui::screens::movie_detail::play_movie(ctx, id);
                    OverlayOutcome::Close
                }
                SearchTarget::Movie(id) => {
                    ctx.push(Route::MovieDetail(id));
                    OverlayOutcome::Close
                }
                SearchTarget::Series(id) if alt => {
                    crate::ui::screens::movies::play_series(ctx, id, &h.item.name);
                    OverlayOutcome::Close
                }
                SearchTarget::Series(id) => {
                    ctx.push(Route::SeriesDetail(id));
                    OverlayOutcome::Close
                }
            },
            Row::Guide(g) => {
                let Some((ch, p)) = guide_channel(ctx, &g) else { return OverlayOutcome::Keep };
                let now = ctx.now;
                if p.contains(now) {
                    if kit::play_channel(ctx, ch, None, PlayOrigin::Search) {
                        return OverlayOutcome::Close;
                    }
                    locked_toast(ctx);
                    OverlayOutcome::Keep
                } else if p.stop <= now {
                    if catchup_available(ctx, ch, &p) {
                        if kit::channel_locked(ctx, ch) {
                            locked_toast(ctx);
                            return OverlayOutcome::Keep;
                        }
                        kit::play_media(
                            ctx,
                            MediaRef::Catchup { channel: ch, start: p.start, stop: p.stop, catchup_id: None },
                            false,
                            PlayOrigin::Search,
                        );
                        OverlayOutcome::Close
                    } else {
                        ctx.toast(crate::msg::Toast::info("This programme has ended and the channel has no catch-up for it"));
                        OverlayOutcome::Keep
                    }
                } else {
                    kit::set_reminder(ctx, ch, &p.title, p.start, p.stop);
                    OverlayOutcome::Keep
                }
            }
        }
    }

    /// The durable reference, effective lock and lock-through-a-category of a hit.
    fn hit_item(ctx: &UiCtx<'_>, h: &SearchHit) -> Option<(ItemRef, bool, bool)> {
        match h.item.target {
            SearchTarget::Channel(id) => ctx.lib.live.get(id).map(|c| {
                let by_group = kit::any_group_locked(&ctx.lib.live.groups, &c.group_ids);
                (ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }, c.locked, by_group)
            }),
            SearchTarget::Movie(id) => {
                ctx.lib.vod.find(VodKind::Movie(id)).map(|v| (vod_ref(v), v.locked, kit::any_group_locked(&ctx.lib.vod.movie_groups, &v.group_ids)))
            }
            SearchTarget::Series(id) => {
                ctx.lib.vod.find(VodKind::Series(id)).map(|v| (vod_ref(v), v.locked, kit::any_group_locked(&ctx.lib.vod.series_groups, &v.group_ids)))
            }
        }
    }

    /// An item shortcut on the highlighted row (see the module docs).
    fn item_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let row = self.current();
        // the guide section: Ctrl+G shows the programme's channel in the guide
        if let (Some(Row::Guide(g)), Action::OpenInGuide) = (&row, action) {
            if let Some((ch, _)) = guide_channel(ctx, g) {
                ctx.push(Route::Guide { collection: None, focus: Some(ch) });
                return OverlayOutcome::Close;
            }
            return OverlayOutcome::Keep;
        }
        if action == Action::ContextMenu {
            if let Some(row) = row {
                Self::menu(ctx, &row);
            }
            return OverlayOutcome::Keep;
        }
        let Some(Row::Hit(h)) = row else {
            kit::not_here(ctx);
            return OverlayOutcome::Keep;
        };
        let Some((item, locked, by_group)) = Self::hit_item(ctx, &h) else { return OverlayOutcome::Keep };
        let name = h.item.name.to_string();
        match (action, h.item.target) {
            (Action::ToggleFavourite, _) => kit::toggle_favourite(ctx, &item),
            (Action::AddToList, _) => kit::list_picker(ctx, &item, &name),
            (Action::Rename, _) => kit::rename_item(ctx, &item, &name),
            (Action::ToggleHidden, _) => kit::hide_item(ctx, &item, &name),
            // the PIN prompt would answer the screen below
            (Action::ToggleLock, _) if !kit::unlocked(ctx) => {
                ctx.toast(crate::msg::Toast::info("Locks change with the PIN — press Ctrl+L in a list"))
            }
            (Action::ToggleLock, _) => kit::toggle_lock(ctx, &item, locked, by_group),
            (Action::ShowDetails, SearchTarget::Channel(_)) => ctx.open(crate::ui::OverlayRequest::EditItem(item)),
            (Action::ShowDetails, SearchTarget::Movie(id)) => {
                ctx.push(Route::MovieDetail(id));
                return OverlayOutcome::Close;
            }
            (Action::ShowDetails, SearchTarget::Series(id)) => {
                ctx.push(Route::SeriesDetail(id));
                return OverlayOutcome::Close;
            }
            (Action::OpenInGuide, SearchTarget::Channel(id)) => {
                ctx.push(Route::Guide { collection: None, focus: Some(id) });
                return OverlayOutcome::Close;
            }
            (Action::AssignEpg, SearchTarget::Channel(id)) => {
                ctx.push(Route::EpgMapping { channel: Some(id) });
                return OverlayOutcome::Close;
            }
            (Action::MarkWatched, SearchTarget::Movie(_)) => kit::mark_watched(ctx, &item, &name, true),
            _ => kit::not_here(ctx),
        }
        OverlayOutcome::Keep
    }

    /// The highlighted hit's actions as a menu (its result comes back to this overlay).
    fn menu(ctx: &mut UiCtx<'_>, row: &Row) {
        let mi = |icon: &'static str, label: &str, action: Action, shortcut: &str| MenuItem {
            icon,
            label: label.into(),
            action,
            enabled: true,
            shortcut: Some(shortcut.into()),
        };
        let (title, items) = match row {
            Row::Header(_) => return,
            Row::Guide(g) => {
                let (play, play_label) = match guide_channel(ctx, g) {
                    Some((_, p)) if p.contains(ctx.now) => (glyphs::PLAY, "Watch"),
                    Some((_, p)) if p.stop <= ctx.now => (glyphs::CATCHUP, "Catch up"),
                    _ => (glyphs::REMINDER, "Remind me"),
                };
                (
                    g.programme.title.to_string(),
                    vec![mi(play, play_label, Action::Activate, "Enter"), mi(glyphs::GUIDE, "Show in TV guide", Action::OpenInGuide, "Ctrl+G")],
                )
            }
            Row::Hit(h) => {
                let fav = Self::hit_item(ctx, h).is_some_and(|(item, ..)| ctx.favs.is_fav(&item));
                let fav = mi(glyphs::FAVOURITE, if fav { "Remove from Favourites" } else { "Add to Favourites" }, Action::ToggleFavourite, "Ctrl+F");
                let lists = mi(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, "Ctrl+Shift+F");
                let mut items = match h.item.target {
                    SearchTarget::Channel(_) => vec![
                        mi(glyphs::PLAY, "Play now", Action::Activate, "Enter"),
                        fav,
                        lists,
                        mi(glyphs::GUIDE, "Show in TV guide", Action::OpenInGuide, "Ctrl+G"),
                        mi(glyphs::INFO, "Edit information…", Action::ShowDetails, "Ctrl+I"),
                        mi(glyphs::GUIDE, "Assign EPG channel…", Action::AssignEpg, "Ctrl+E"),
                    ],
                    SearchTarget::Movie(_) => vec![
                        mi(glyphs::PLAY, "Play now", Action::ActivateAlt, "Shift+Enter"),
                        mi(glyphs::INFO, "Open the movie's page", Action::Activate, "Enter"),
                        fav,
                        lists,
                        mi(glyphs::CHECK, "Mark watched", Action::MarkWatched, "Ctrl+W"),
                    ],
                    SearchTarget::Series(_) => vec![
                        mi(glyphs::PLAY, "Play now", Action::ActivateAlt, "Shift+Enter"),
                        mi(glyphs::INFO, "Open the series' page", Action::Activate, "Enter"),
                        fav,
                        lists,
                    ],
                };
                items.extend([
                    mi(glyphs::TAG, "Rename…", Action::Rename, "F2"),
                    mi(glyphs::LOCK, "Lock / unlock", Action::ToggleLock, "Ctrl+L"),
                    mi(glyphs::HIDDEN, "Hide", Action::ToggleHidden, "Ctrl+H"),
                ]);
                (h.item.name.to_string(), items)
            }
        };
        ctx.open(crate::ui::OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    /// Build or refresh the shared index (lazily, also after a lock change).
    fn ensure_index(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
        let unlocked = kit::unlocked(ctx);
        if ctx.search_index.is_some() && BUILT_UNLOCKED.get() == Some(unlocked) {
            return;
        }
        let repaint = egui_ctx.clone();
        let mut idx = ctx.search_index.take().unwrap_or_else(|| SearchIndex::new(Arc::new(move || repaint.request_repaint())));
        idx.rebuild(ctx.lib, unlocked);
        BUILT_UNLOCKED.set(Some(unlocked));
        *ctx.search_index = Some(idx);
    }

    /// Send the query to the EPG search when it changed (2+ characters, else clear).
    fn update_epg(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
        let wants = matches!(self.section, SearchSection::All | SearchSection::Guide);
        if !wants {
            return;
        }
        let q = self.query.trim();
        let q = if q.chars().count() >= EPG_MIN_CHARS { q.to_owned() } else { String::new() };
        if q == self.epg_query && self.epg.is_some() {
            return;
        }
        let epg = self.epg.get_or_insert_with(|| EpgSearch::new(ctx.services.rt.handle().clone(), ctx.services.store.clone()));
        let (repaint, dirty) = (egui_ctx.clone(), self.epg_dirty.clone());
        epg.set_query(
            &q,
            ctx.now,
            Arc::new(move || {
                dirty.store(true, Ordering::Release);
                repaint.request_repaint();
            }),
        );
        self.epg_query = q;
    }

    /// Guide hits for the current query (stale ones hidden, locked channels excluded).
    fn guide_hits(&self, ctx: &UiCtx<'_>) -> Vec<ProgrammeHit> {
        let Some(epg) = &self.epg else { return Vec::new() };
        if self.epg_query.is_empty() {
            return Vec::new();
        }
        let unlocked = kit::unlocked(ctx);
        epg.results()
            .into_iter()
            .filter(|h| hit_matches(&self.query, &h.programme.title, h.programme.sub_title.as_deref()))
            .filter(|h| {
                h.channel_ids
                    .iter()
                    .any(|c| ctx.lib.live.get(*c).is_some() && !ctx.lib.live.hidden.contains(c) && (unlocked || !kit::channel_locked(ctx, *c)))
            })
            .collect()
    }

    fn build_rows(&mut self, ctx: &UiCtx<'_>) {
        let mut rows = Vec::new();
        if self.query.trim().is_empty() {
            self.rows = rows;
            return;
        }
        let hits = ctx
            .search_index
            .as_ref()
            .map(|i| i.results(self.section, if self.section == SearchSection::All { ALL_LIMIT } else { SECTION_LIMIT }))
            .unwrap_or_default();
        let mut last: Option<&'static str> = None;
        for h in hits {
            let header = match h.item.target {
                SearchTarget::Channel(_) => "Live",
                SearchTarget::Movie(_) => "Movies",
                SearchTarget::Series(_) => "Series",
            };
            if self.section == SearchSection::All && last != Some(header) {
                rows.push(Row::Header(header));
                last = Some(header);
            }
            rows.push(Row::Hit(h));
        }
        if matches!(self.section, SearchSection::All | SearchSection::Guide) {
            let guide = self.guide_hits(ctx);
            let take = if self.section == SearchSection::All { ALL_GUIDE } else { usize::MAX };
            if !guide.is_empty() && self.section == SearchSection::All {
                rows.push(Row::Header("Guide"));
            }
            rows.extend(guide.into_iter().take(take).map(Row::Guide));
        }
        self.rows = rows;
        let n = self.selectable().len();
        self.cursor = self.cursor.min(n.saturating_sub(1));
    }
}

/// Does a guide hit still match `query` (FTS semantics: every token a word, the last one a
/// prefix; case and diacritics folded)? Hides results of an older query while the newer
/// search runs.
pub(crate) fn hit_matches(query: &str, title: &str, sub_title: Option<&str>) -> bool {
    let fold = |s: &str| deunicode::deunicode(s).to_lowercase();
    let tokens: Vec<String> = fold(query).split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_owned).collect();
    if tokens.is_empty() {
        return false;
    }
    let hay = fold(&format!("{title} {}", sub_title.unwrap_or("")));
    let words: Vec<&str> = hay.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let last = tokens.len() - 1;
    tokens.iter().enumerate().all(|(i, t)| if i == last { words.iter().any(|w| w.starts_with(t.as_str())) } else { words.iter().any(|w| w == t) })
}

/// The playable channel of a guide hit and the programme as shown on it (tvg-shift applied).
fn guide_channel(ctx: &UiCtx<'_>, g: &ProgrammeHit) -> Option<(crate::model::ChannelId, ProgrammeSlim)> {
    let unlocked = kit::unlocked(ctx);
    let id = g
        .channel_ids
        .iter()
        .copied()
        .find(|c| ctx.lib.live.get(*c).is_some() && !ctx.lib.live.hidden.contains(c) && (unlocked || !kit::channel_locked(ctx, *c)))?;
    let shift = ctx.lib.live.get(id).map_or(0, |c| c.tvg_shift_min);
    Some((id, g.programme.shifted(shift)))
}

/// The channel archives this past programme (within its catch-up window).
fn catchup_available(ctx: &UiCtx<'_>, ch: crate::model::ChannelId, p: &ProgrammeSlim) -> bool {
    ctx.lib.live.get(ch).is_some_and(|c| c.has_catchup && p.start >= ctx.now - i64::from(c.catchup_days.max(1)) * 86_400)
}

impl Overlay for SearchOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Search]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::FilterInput(c) => {
                if self.query.is_empty() && c.is_whitespace() {
                    return OverlayOutcome::Keep;
                }
                self.edit(|q| crate::search::push_typed(q, c))
            }
            Action::FilterBackspace => self.edit(|q| {
                q.pop();
            }),
            Action::FilterDeleteWord => self.edit(crate::search::delete_last_word),
            Action::FilterClear => self.edit(String::clear),
            Action::Back if !self.query.is_empty() => self.edit(String::clear),
            Action::Back => OverlayOutcome::Close,
            Action::Up => self.move_cursor(-1),
            Action::Down => self.move_cursor(1),
            Action::PageUp => self.move_cursor(-10),
            Action::PageDown => self.move_cursor(10),
            Action::FocusNext => self.cycle_section(true),
            Action::FocusPrev => self.cycle_section(false),
            Action::Activate | Action::ActivateAlt => match self.current() {
                Some(row) => self.activate(ctx, row, action == Action::ActivateAlt),
                None => OverlayOutcome::Keep,
            },
            Action::ToggleFavourite
            | Action::AddToList
            | Action::ToggleHidden
            | Action::Rename
            | Action::ToggleLock
            | Action::OpenInGuide
            | Action::ShowDetails
            | Action::AssignEpg
            | Action::MarkWatched
            | Action::ContextMenu => self.item_action(ctx, action),
            _ => OverlayOutcome::Ignored,
        }
    }

    /// A choice from the hit's menu: what its shortcut does.
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) -> OverlayOutcome {
        match result {
            OverlayResult::Chosen(a) if a != Action::ContextMenu => self.on_action(ctx, a),
            _ => OverlayOutcome::Keep,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.ensure_index(egui_ctx, ctx);
        let mut matched = false;
        if let Some(idx) = ctx.search_index.as_mut() {
            idx.set_query(&self.query);
            matched = idx.tick(10);
        }
        self.update_epg(egui_ctx, ctx);
        let key = (self.query.clone(), self.section, kit::unlocked(ctx), ctx.lib.built_at);
        let epg_new = self.epg_dirty.swap(false, Ordering::AcqRel);
        if matched || epg_new || self.rows_key.as_ref() != Some(&key) {
            self.build_rows(ctx);
            self.rows_key = Some(key);
        }
        let theme = ctx.theme;
        if scrim(egui_ctx, theme, "search") {
            return OverlayOutcome::Close;
        }
        let t = &theme.tokens;
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let row_h = (44.0 * theme.rem()).round();
        let selectable = self.selectable();
        let cursor_row = selectable.get(self.cursor).copied();
        let mut clicked: Option<usize> = None;
        let mut menu: Option<usize> = None;
        let mut close = false;
        let mut hovered: Option<usize> = None;
        let mut section_clicked = None;
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let rows = std::mem::take(&mut self.rows);
        let r = card_area(egui_ctx, theme, "search", CardKind::Menu, CARD_W * theme.rem(), self.top, |ui| {
            close = with_close(ui, theme, |ui| {
                search_header(ui, theme, &self.query, "Search channels, movies, series and the guide", Some(glyphs::SEARCH));
            });
            ui.add_space(theme.space(SpaceToken::Md));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xxs);
                for s in SearchSection::ALL {
                    let label = match s {
                        SearchSection::All => "All",
                        SearchSection::Live => "Live",
                        SearchSection::Movies => "Movies",
                        SearchSection::Series => "Series",
                        SearchSection::Guide => "Guide",
                    };
                    if button(ui, theme, None, label, ButtonStyle { selected: s == self.section, ..Default::default() }).clicked() {
                        section_clicked = Some(s);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Tab cycles").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                });
            });
            ui.add_space(theme.space(SpaceToken::Md));
            if self.query.trim().is_empty() {
                ui.label(
                    RichText::new("Type to search channels, movies, series and the guide").size(theme.font(TypeToken::Body)).color(t.text_secondary),
                );
                return;
            }
            if rows.is_empty() {
                ui.label(
                    RichText::new("No matches — Ctrl+K searches everything, Tab switches sections")
                        .size(theme.font(TypeToken::Title))
                        .color(t.text_secondary),
                );
                return;
            }
            egui::ScrollArea::vertical().max_height(egui_ctx.content_rect().height() * 0.62).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xxs);
                for (i, row) in rows.iter().enumerate() {
                    if let Row::Header(h) = row {
                        ui.add_space(theme.space(SpaceToken::Sm));
                        section::section_header(ui, theme, h);
                        continue;
                    }
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                    let is_cursor = cursor_row == Some(i);
                    if is_cursor && scroll {
                        ui.scroll_to_rect(rect, None);
                    }
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        clicked = Some(i);
                    } else if resp.secondary_clicked() {
                        menu = Some(i);
                    } else if pointer_moved && resp.hovered() && !is_cursor {
                        hovered = Some(i);
                    }
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    paint_row_state(ui.painter(), rect, theme, if is_cursor { RowState::Cursor } else { RowState::Idle });
                    match row {
                        Row::Hit(h) => {
                            let label = crate::ui::widgets::badge::label_with_source(
                                &h.item.name,
                                ctx.source_of(h.item.target.item_kind(), h.item.source_id),
                            );
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                            paint_hit(ui, ctx, rect, h, is_cursor);
                        }
                        Row::Guide(g) => {
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &*g.programme.title));
                            paint_guide(ui, ctx, rect, g, is_cursor);
                        }
                        Row::Header(_) => {}
                    }
                }
            });
        });
        self.rows = rows;
        self.last_top = Some(r.response.rect.top());
        if let Some(s) = section_clicked {
            self.section = s;
            self.cursor = 0;
        }
        let sel = self.selectable();
        if let Some(i) = hovered.and_then(|i| sel.iter().position(|x| *x == i)) {
            self.cursor = i;
        }
        if close {
            return OverlayOutcome::Close;
        }
        if let Some(i) = clicked.and_then(|i| sel.iter().position(|x| *x == i)) {
            self.cursor = i;
            if let Some(row) = self.current() {
                return self.activate(ctx, row, false);
            }
        }
        if let Some(i) = menu.and_then(|i| sel.iter().position(|x| *x == i)) {
            self.cursor = i;
            return self.item_action(ctx, Action::ContextMenu);
        }
        if ctx.now_next.pending() {
            egui_ctx.request_repaint();
        }
        OverlayOutcome::Keep
    }
}

/// `[logo] BBC One HD  [󰐹 My IPTV]   News at Eight · 20:00   ▕██░▏` (name highlighted).
fn paint_hit(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, h: &SearchHit, cursor: bool) {
    let theme = ctx.theme;
    let t = &theme.tokens;
    let pad = theme.space(SpaceToken::Lg);
    // Settings ▸ Interface ▸ Channel logos (posters of movies / series always show)
    let logos = ctx.config.interface.show_logos || !matches!(h.item.target, SearchTarget::Channel(_));
    let mut left = rect.left() + pad;
    if logos {
        let icon = rect.height() - 2.0 * theme.space(SpaceToken::Md);
        let logo_rect = Rect::from_min_size(egui::pos2(left, rect.center().y - icon / 2.0), egui::vec2(icon, icon));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(logo_rect));
        crate::ui::widgets::logo::logo(&mut child, theme, ctx.images, h.item.logo.as_deref(), &h.item.name, icon);
        left = logo_rect.right() + pad;
    }
    let p = ui.painter().clone();
    let mut right = rect.right() - pad;
    let y = rect.center().y;
    let small = egui::FontId::proportional(theme.font(TypeToken::BodySmall));

    // right side: progress (channels), the programme, the source badge
    if let SearchTarget::Channel(id) = h.item.target {
        if ctx.player.playable.as_ref().is_some_and(|p| p.media == MediaRef::Channel(id)) {
            let r = p.text(egui::pos2(left, y), Align2::LEFT_CENTER, glyphs::PLAY, egui::FontId::proportional(theme.font(TypeToken::Icon)), t.accent);
            left = r.right() + theme.space(SpaceToken::Sm);
        }
        if let Some(c) = ctx.lib.live.get(id).cloned()
            && let Some(e) = c.epg_channel_id
        {
            let mut reader = ctx.epg_reader();
            let now = ctx.now;
            if let Some(cur) = ctx.now_next.get((e, c.tvg_shift_min), now, &mut reader, ctx.budget).and_then(|n| n.now.clone()) {
                let bar_w = (40.0 * theme.rem()).round();
                let bar = Rect::from_center_size(egui::pos2(right - bar_w / 2.0, y), egui::vec2(bar_w, (3.0 * theme.rem()).round().max(2.0)));
                crate::ui::widgets::progress::paint_progress(&p, bar, theme, cur.progress(now), false);
                right -= bar_w + pad;
                let text = format!("{} · {}", cur.title, kit::hhmm(cur.start, ctx.config.interface.clock_24h));
                let g = elided(&p, &text, small.clone(), t.text_secondary, (rect.width() * 0.32).max(60.0));
                let r = Align2::RIGHT_CENTER.anchor_size(egui::pos2(right, y), g.size());
                p.galley(r.min, g, t.text_secondary);
                right = r.left() - pad;
            }
        }
    }
    // the playlist, with several (Settings ▸ Interface ▸ Playlist on cards, DESIGN §13.0)
    if let Some(src) = ctx.source_of(h.item.target.item_kind(), h.item.source_id) {
        let glyph = crate::ui::widgets::badge::source_glyph(src);
        let text = format!("{glyph} {}", src.name);
        let color = theme.source_color(src.color);
        // the border keeps the source colour; the text falls back to fg where it wouldn't read
        let text_color = kit::legible(theme, color);
        let g = elided(&p, &text, egui::FontId::proportional(theme.font(TypeToken::Caption)), text_color, 140.0 * theme.rem());
        let size = g.size() + egui::vec2(2.0 * theme.space(SpaceToken::Sm), 2.0 * theme.space(SpaceToken::Xxs));
        let r = Align2::RIGHT_CENTER.anchor_size(egui::pos2(right, y), size);
        p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, crate::theme::with_alpha(color, 0.6)), egui::StrokeKind::Inside);
        p.galley(r.center() - g.size() / 2.0, g, text_color);
        right = r.left() - pad;
    }
    // name with the matched characters in accent
    let base = if cursor { t.list_cursor_text } else { t.fg };
    let font = egui::FontId::proportional(theme.font(TypeToken::Title));
    let mut job = egui::text::LayoutJob::default();
    for (i, ch) in h.item.name.chars().enumerate() {
        let color = if h.indices.contains(&(i as u32)) { t.accent } else { base };
        job.append(&ch.to_string(), 0.0, egui::TextFormat::simple(font.clone(), color));
    }
    if h.item.locked {
        job.append(&format!("  {}", glyphs::LOCK), 0.0, egui::TextFormat::simple(font.clone(), t.text_secondary));
    }
    // the quality badge after the name (DESIGN §13.0)
    let quality = match h.item.target {
        SearchTarget::Channel(id) => ctx.lib.live.get(id).and_then(|c| ctx.quality(c)),
        _ => None,
    };
    let badge = quality.map(|q| InlineBadge::quality(&p, theme, q, t.text_secondary, 1.0));
    job_with_quality(&p, theme, egui::pos2(left, y), job, badge, (right - left).max(0.0));
}

/// `Thu 21:00  Bbc Proms · BBC Four     󰂚 / ⟲ / LIVE`.
fn paint_guide(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, g: &ProgrammeHit, cursor: bool) {
    let theme = ctx.theme;
    let t = &theme.tokens;
    let p = ui.painter();
    let pad = theme.space(SpaceToken::Lg);
    let y = rect.center().y;
    let shown = guide_channel(ctx, g);
    let (prog, channel_name, catchup) = match &shown {
        Some((id, prog)) => (prog.clone(), ctx.lib.live.get(*id).map(|c| c.name.to_string()).unwrap_or_default(), catchup_available(ctx, *id, prog)),
        None => (g.programme.clone(), String::new(), false),
    };
    let now = ctx.now;
    let mut right = rect.right() - pad;
    let (marker, color) = if prog.contains(now) {
        ("LIVE", t.urgent)
    } else if prog.stop <= now {
        (if catchup { glyphs::CATCHUP } else { "" }, t.text_secondary)
    } else {
        (glyphs::REMINDER, t.text_secondary)
    };
    if !marker.is_empty() {
        let r = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, marker, egui::FontId::proportional(theme.font(TypeToken::Icon)), color);
        right = r.left() - pad;
    }
    let time_font = egui::FontId::proportional(theme.font(TypeToken::Body));
    let time = kit::day_time(prog.start, ctx.config.interface.clock_24h);
    let tg = elided(p, &time, time_font, t.text_secondary, f32::INFINITY);
    let tw = tg.size().x;
    p.galley(egui::pos2(rect.left() + pad, y - tg.size().y / 2.0), tg, t.text_secondary);
    let left = rect.left() + pad + tw.max(90.0 * theme.rem()) + pad;
    let title = if channel_name.is_empty() { prog.title.to_string() } else { format!("{} · {channel_name}", prog.title) };
    let past = prog.stop <= now;
    let c = if cursor {
        t.list_cursor_text
    } else if past {
        t.text_secondary
    } else {
        t.fg
    };
    let g = elided(p, &title, egui::FontId::proportional(theme.font(TypeToken::Title)), c, (right - left).max(0.0));
    p.galley(egui::pos2(left, y - g.size().y / 2.0), g, c);
}

/// A locked hit (the session relocked while the card was open): a PIN prompt from here would
/// answer the screen below, so the user unlocks from a list instead.
fn locked_toast(ctx: &mut UiCtx<'_>) {
    ctx.toast(crate::msg::Toast::info("This is locked — open it from its list to enter the PIN"));
}

fn elided(p: &egui::Painter, text: &str, font: egui::FontId, color: egui::Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(0.0));
    p.layout_job(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{Collection, ItemKey, ItemOverride};
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, has_label, overlay_harness, rig, seed, until_ov};

    #[test]
    fn sections_highlights_actions_and_esc() {
        let (mut t, one) = rig();
        let now = t.now;
        let (chans, movies) = seed(&mut t, move |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ch = testkit::channels(c, s, 30)?;
            testkit::epg(c, &ch[..5], now - 3600, 4, true)?;
            let mv = testkit::movies(c, s, 5)?;
            testkit::series(c, s, 2, 1, 2)?;
            Ok((ch, mv))
        });
        let mut h = overlay_harness(t, one, SearchOverlay::new("Channel 1".into()), egui::vec2(1400.0, 900.0));
        until_ov(&mut h, "channel hits", |h| has_label(h, "Channel 1"));
        assert_eq!(h.state().o.key_contexts(), vec![KeyContext::Search]);
        assert!(has_label(&h, "LIVE"), "results grouped by kind");
        assert!(has_label(&h, "Tab cycles"));
        // no source badge with a single playlist (Playlist on cards: Auto, DESIGN §13.0); the
        // now-playing programme of a mapped channel
        let texts = testkit::painted(&h);
        assert!(!texts.iter().any(|t| t.contains("My IPTV")), "one playlist, no badge: {texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("Show ")), "now playing on the mapped channel: {texts:?}");
        // Enter on the first hit plays it FULLSCREEN, zapping through its primary group
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        let group = h.state().t.lib.live.get(chans[1]).unwrap().group_ids[0];
        until_ov(&mut h, "the zap list", |h| {
            let app = h.state_mut();
            app.t.refresh_snapshot();
            app.t.snapshot.zap.as_ref().is_some_and(|z| z.collection == Collection::Group(group))
        });

        // movies: typing replaces the query; Enter opens the page, Shift+Enter plays
        let app = h.state_mut();
        assert_eq!(app.act(Action::FilterClear), "Keep");
        for c in "movie 3".chars() {
            app.act(Action::FilterInput(c));
        }
        until_ov(&mut h, "movie hits", |h| has_label(h, "Movie 3"));
        assert!(has_label(&h, "MOVIES"));
        // Tab: Live, Movies — the movie is the first row of the Movies section
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::FocusNext);
        h.run_steps(1);
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(movies[3]))]);

        // the guide section (FTS, debounced, 2+ characters)
        let app = h.state_mut();
        app.act(Action::FilterClear);
        for c in "show 5".chars() {
            app.act(Action::FilterInput(c));
        }
        app.act(Action::FocusNext);
        app.act(Action::FocusNext);
        assert_eq!(h.state().o.section, SearchSection::Guide, "Movies → Series → Guide");
        until_ov(&mut h, "guide hits", |h| testkit::has_label_containing(h, "Show 5 on"));

        // no matches
        let app = h.state_mut();
        app.act(Action::FilterClear);
        app.act(Action::FocusNext);
        for c in "zzqqxx".chars() {
            app.act(Action::FilterInput(c));
        }
        h.run_steps(3);
        assert!(has_label(&h, "No matches — Ctrl+K searches everything, Tab switches sections"));
        // Esc clears, then closes
        assert_eq!(h.state_mut().act(Action::Back), "Keep");
        assert_eq!(h.state().o.query, "");
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        assert_eq!(h.state_mut().act(Action::Quit), "Ignored", "globals keep working");
    }

    #[test]
    fn locked_items_are_hidden_until_the_pin_is_entered() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 5)?;
            let item = ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 2)) };
            crate::store::user::set_item_override(c, &item, &ItemOverride { locked: true, ..Default::default() })
        });
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
        };
        t.config = Arc::new(cfg);
        let mut h = overlay_harness(t, one, SearchOverlay::new("Channel".into()), egui::vec2(1000.0, 900.0));
        until_ov(&mut h, "results", |h| has_label(h, "Channel 1"));
        assert!(!has_label(&h, "Channel 2"), "locked while the session is locked");
        let app = h.state_mut();
        let cfg = app.t.config.clone();
        assert!(app.t.parental.unlock("1234", &cfg.parental));
        until_ov(&mut h, "the unlocked channel", |h| has_label(h, "Channel 2"));
    }

    /// The item shortcuts Help lists for `[Search]` act on the highlighted hit; a hit that is
    /// locked by a relock while the card is open toasts instead of opening a PIN prompt
    /// (whose answer would go to the screen below).
    #[test]
    fn item_shortcuts_and_a_relocked_hit() {
        let (mut t, one) = rig();
        let movies = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 5)?;
            let item = ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 2)) };
            crate::store::user::set_item_override(c, &item, &ItemOverride { locked: true, ..Default::default() })?;
            testkit::movies(c, s, 3)
        });
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
        };
        t.config = Arc::new(cfg);
        let p = t.config.parental.clone();
        assert!(t.parental.unlock("1234", &p));
        let mut h = overlay_harness(t, one, SearchOverlay::new("Channel 2".into()), egui::vec2(1000.0, 900.0));
        until_ov(&mut h, "the locked channel (session open)", |h| has_label(h, "Channel 2"));
        // Ctrl+G: the channel in the guide
        assert_eq!(h.state_mut().act(Action::OpenInGuide), "Close");
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Push(Route::Guide { focus: Some(_), .. })]));
        // the menu key: the hit's actions as a menu, "Play now" first (the app routes its answer
        // back to this overlay); a right-click on the hit opens the same menu
        assert_eq!(h.state_mut().act(Action::ContextMenu), "Keep");
        let keyed = h.state_mut().nav();
        let [NavRequest::Open(crate::ui::OverlayRequest::ContextMenu(spec))] = keyed.as_slice() else { panic!("{keyed:?}") };
        assert_eq!(spec.items[0].label, "Play now");
        egui_kittest::kittest::Queryable::get_by_role_and_label(&h, egui::accesskit::Role::Button, "Channel 2").click_secondary();
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), keyed, "right-click = the menu key");
        // its answer is the item's shortcut
        let app = h.state_mut();
        let (t, o) = (&mut app.t, &mut app.o);
        assert_eq!(testkit::outcome(&t.with_ctx(|ctx| o.on_overlay_result(ctx, OverlayResult::Chosen(Action::OpenInGuide)))), "Close");
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Push(Route::Guide { focus: Some(_), .. })]));
        // the session relocks with the card open: Enter toasts, no PIN prompt
        h.state_mut().t.parental.lock();
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        assert!(h.state_mut().nav().is_empty(), "no PIN overlay from inside the search");
        assert!(h.state().t.toasts.iter().any(|t| t.text.contains("locked")), "{:?}", h.state().t.toasts);
        // movies: Ctrl+I opens the page; Ctrl+Shift+F the lists picker; Ctrl+W marks it watched
        let app = h.state_mut();
        app.act(Action::FilterClear);
        for c in "movie 1".chars() {
            app.act(Action::FilterInput(c));
        }
        until_ov(&mut h, "the movie", |h| has_label(h, "Movie 1"));
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::FocusNext);
        h.run_steps(1);
        assert_eq!(h.state_mut().act(Action::AddToList), "Keep");
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(crate::ui::OverlayRequest::ListPicker(_))]));
        assert_eq!(h.state_mut().act(Action::ShowDetails), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(movies[1]))]);
    }

    #[test]
    fn stale_guide_hits_are_hidden() {
        assert!(hit_matches("bbc pro", "BBC Proms", None));
        assert!(hit_matches("proms", "The BBC Proms", Some("Last Night")));
        assert!(hit_matches("last nig", "The BBC Proms", Some("Last Night")));
        assert!(!hit_matches("bbc pro", "BBC News", None), "older 'bbc' results under 'bbc pro'");
        assert!(!hit_matches("bb", "Abba Gold", None), "prefix of a word, not a substring");
        assert!(hit_matches("cafe", "Café Society", None), "diacritics folded");
        assert!(!hit_matches("", "anything", None));
        assert!(!hit_matches("tom jer", "Tomorrow's Jerusalem", None), "non-final tokens are whole words");
    }

    #[test]
    fn editing_and_sections() {
        let mut o = SearchOverlay::new(String::new());
        o.rows = vec![
            Row::Header("Live"),
            Row::Guide(ProgrammeHit {
                programme: ProgrammeSlim {
                    id: crate::model::ProgrammeId(1),
                    start: 0,
                    stop: 1,
                    title: "X".into(),
                    sub_title: None,
                    flags: Default::default(),
                },
                epg_channel_id: crate::model::EpgChannelId(1),
                channel_ids: vec![],
            }),
        ];
        assert_eq!(o.selectable(), vec![1]);
        o.cursor = 5;
        let _ = o.move_cursor(1);
        assert_eq!(o.cursor, 0, "clamped to the selectable rows");
        let _ = o.cycle_section(true);
        assert_eq!(o.section, SearchSection::Live);
        let _ = o.cycle_section(false);
        let _ = o.cycle_section(false);
        assert_eq!(o.section, SearchSection::Guide, "wraps backwards");
        let _ = o.edit(|q| q.push_str("bbc"));
        assert_eq!(o.query, "bbc");
    }
}
