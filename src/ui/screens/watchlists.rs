//! Watchlists: named play orders and their derived place (DESIGN §42 §2.1–§2.4).
//! The list and entry panes share the cached snapshot with Home. Reorder by keyboard and by
//! grip both call the same store operation; drag cancellation never writes.
use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, WatchlistId, WatchlistItemId};
use crate::msg::AppEvent;
use crate::store::watchlist as store;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult, RenameSpec, RenameTarget};
use crate::ui::watchlists as shared;
use crate::ui::widgets::{
    button::{ButtonStyle, button},
    glyphs, section,
};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};
use crate::watchlist::{EntryState, EntryStatus, ListView, PlayOrder, WatchlistsSnapshot};
use eframe::egui;
use std::sync::Arc;

const DELETE: u64 = 0x574c_4445;
const RESTART: u64 = 0x574c_5253;
const PLAY: u8 = 200;
const ONE: u8 = 201;
const SKIP: u8 = 202;
const NEXT: u8 = 203;
const SEASONS: u8 = 204;
const COPY: u8 = 205;
const FIND: u8 = 206;
const REMOVE: u8 = 207;
const RENAME: u8 = 208;
const DELETE_LIST: u8 = 209;
const START_AGAIN: u8 = 210;
const DUPLICATE: u8 = 211;
const NEW: u8 = 212;
const WATCHED: u8 = 213;
const QUEUE: u8 = 214;
const QUEUE_NEXT: u8 = 215;
const MOVE_UP: u8 = 216;
const MOVE_DOWN: u8 = 217;
const SHOW_WATCHED: u8 = 218;
const ALL_WATCHED: u8 = 219;
const DETAILS: u8 = 220;

#[derive(Default)]
pub struct WatchlistsScreen {
    snapshot: Arc<WatchlistsSnapshot>,
    selected: Option<WatchlistId>,
    /// Pane navigation can select a different list without changing the opening route.
    route_list: Option<WatchlistId>,
    cursor: usize,
    lists_focus: bool,
    drag: Option<WatchlistItemId>,
    drag_to: Option<usize>,
    list_drag: Option<WatchlistId>,
    list_drag_to: Option<WatchlistId>,
    copies: Vec<ItemRef>,
    menu_entry: Option<WatchlistItemId>,
    scroll: bool,
    opening: bool,
    fetch: crate::ui::copies::EpisodeFetch,
    pending_mark: Option<(EntryState, bool, bool)>,
    pending_focus: Option<(WatchlistItemId, usize)>,
    return_to: Option<(Option<WatchlistId>, WatchlistId, WatchlistItemId)>,
    /// Keep the returned entry visible even if it was completed on its details page.
    revealed_entry: Option<WatchlistItemId>,
}
impl WatchlistsScreen {
    fn view(&self) -> Option<&ListView> {
        self.selected.and_then(|id| self.snapshot.list(id))
    }
    fn shown(&self) -> Vec<&EntryState> {
        let Some(v) = self.view() else { return vec![] };
        let mut entries: Vec<_> =
            v.entries.iter().filter(|e| v.show_watched || e.status != EntryStatus::Done || Some(e.entry.id) == self.revealed_entry).collect();
        if v.list.play_order == PlayOrder::Any {
            entries.sort_by_key(|e| e.done());
        }
        entries
    }
    fn entry(&self) -> Option<EntryState> {
        self.shown().get(self.cursor).copied().cloned()
    }
    fn select(&mut self, id: WatchlistId) {
        self.pending_focus = None;
        self.revealed_entry = None;
        self.selected = Some(id);
        self.cursor = self.view().and_then(|v| v.next).and_then(|id| self.shown().iter().position(|e| e.entry.id == id)).unwrap_or(0);
        self.scroll = true;
    }
    fn sync(&mut self, ctx: &UiCtx<'_>) {
        let focused = self.shown().get(self.cursor).map(|e| e.entry.id);
        self.snapshot = shared::snapshot(ctx);
        if let Some(index) = focused.and_then(|id| self.shown().iter().position(|e| e.entry.id == id)) {
            self.cursor = index;
        }
        if self.opening && self.selected.is_some_and(|id| self.snapshot.list(id).is_some()) {
            self.select(self.selected.unwrap());
            self.opening = false;
        }
        if self.selected.is_none_or(|id| !self.opening && self.snapshot.list(id).is_none())
            && let Some(id) = self.snapshot.lists.first().map(|l| l.list.id)
        {
            self.select(id);
        }
        if let Some((id, position)) = self.pending_focus
            && let Some(index) = self.shown().iter().position(|e| e.entry.id == id && e.entry.position == position)
        {
            self.cursor = index;
            self.pending_focus = None;
            self.scroll = true;
        }
        self.cursor = self.cursor.min(self.shown().len().saturating_sub(1));
    }
    fn new_list(&self, ctx: &mut UiCtx<'_>) {
        ctx.open(OverlayRequest::WatchlistPicker(crate::ui::overlays::watchlists::PickerSpec {
            omitted: (0, 0),
            items: vec![],
            title: String::new(),
            edit: None,
            create: true,
        }));
    }
    fn play(&self, ctx: &mut UiCtx<'_>, just_one: bool) {
        if let Some(id) = self.selected {
            let from = (!self.lists_focus).then(|| self.entry().map(|e| e.entry.id)).flatten();
            shared::play(ctx, id, from, just_one);
        }
    }
    fn move_entry(&mut self, ctx: &UiCtx<'_>, id: WatchlistItemId, to: usize) {
        let Some(list) = self.selected else { return };
        shared::write(ctx, "Moved", move |c| {
            store::move_entries(c, list, &[id], to)?;
            Ok(None)
        });
        self.pending_focus = Some((id, to));
        self.scroll = true;
    }
    fn list_move_target(&self, down: bool) -> Option<WatchlistId> {
        let v = self.view().filter(|v| !v.list.is_default)?;
        let group: Vec<_> = self.snapshot.lists.iter().filter(|l| !l.list.is_default && l.finished() == v.finished()).collect();
        let at = group.iter().position(|l| l.list.id == v.list.id)?;
        let to = if down { at + 1 } else { at.checked_sub(1)? };
        group.get(to).map(|l| l.list.id)
    }
    fn move_list(&mut self, ctx: &UiCtx<'_>, id: WatchlistId, to: WatchlistId) {
        let Some(v) = self.snapshot.list(id).filter(|v| !v.list.is_default) else { return };
        let mut ids: Vec<_> = self.snapshot.lists.iter().filter(|l| !l.list.is_default && l.finished() == v.finished()).map(|l| l.list.id).collect();
        let Some(from) = ids.iter().position(|i| *i == id) else { return };
        let Some(to) = ids.iter().position(|i| *i == to) else { return };
        if from == to {
            return;
        }
        ids.remove(from);
        ids.insert(to, id);
        shared::write(ctx, "Moved watchlist", move |c| {
            store::reorder_lists(c, &ids)?;
            Ok(None)
        });
    }
    fn remove(&self, ctx: &mut UiCtx<'_>) {
        if self.lists_focus {
            self.choose(ctx, DELETE_LIST);
            return;
        }
        if let Some(e) = self.entry() {
            shared::write(ctx, "Removed from watchlist", move |c| store::remove(c, e.entry.list_id, &[e.entry.id]).map(Some));
        }
    }
    fn menu(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(v) = self.view().cloned() else {
            self.new_list(ctx);
            return;
        };
        let mi = |label: &str, code, enabled| MenuItem { icon: "", label: label.into(), action: Action::Digit(code), enabled, shortcut: None };
        let (title, items) = if self.lists_focus {
            (
                v.list.name.clone(),
                vec![
                    mi("Play", PLAY, true),
                    mi(if v.show_watched { "Hide watched" } else { "Show watched" }, SHOW_WATCHED, true),
                    mi("Start again…", START_AGAIN, true),
                    mi("Duplicate…", DUPLICATE, true),
                    mi("Move earlier", MOVE_UP, self.list_move_target(false).is_some()),
                    mi("Move later", MOVE_DOWN, self.list_move_target(true).is_some()),
                    mi("Rename…", RENAME, !v.list.is_default),
                    mi("Delete…", DELETE_LIST, !v.list.is_default),
                    mi("New list…", NEW, true),
                ],
            )
        } else if let Some(e) = self.entry() {
            self.menu_entry = Some(e.entry.id);
            let series = e.entry.kind == ItemKind::Series;
            let current = series && e.status != EntryStatus::Done;
            let watched = if e.status == EntryStatus::Done || (!series && e.done()) {
                "Mark unwatched".into()
            } else if series {
                shared::episode_number(&e).map(|n| format!("Mark {n} watched")).unwrap_or_else(|| "Mark current episode watched".into())
            } else {
                "Mark watched".into()
            };
            let mut items = vec![
                mi("Play from here", PLAY, e.playable.is_some()),
                mi("Play just this one", ONE, e.playable.is_some()),
                mi("Add to Up next", QUEUE, e.playable.is_some()),
                mi("Play next", QUEUE_NEXT, e.playable.is_some()),
                MenuItem {
                    shortcut: Some("Ctrl+W".into()),
                    ..mi(&watched, WATCHED, e.entry.item.is_some() && (!current || e.episode.is_some() || e.playable.is_some()))
                },
            ];
            if series {
                items.push(mi("Mark all watched", ALL_WATCHED, e.entry.item.is_some()));
            }
            items.push(MenuItem {
                shortcut: Some("Ctrl+I".into()),
                ..mi(if e.entry.kind == ItemKind::Movie { "Details…" } else { "Episodes…" }, DETAILS, e.entry.item.is_some())
            });
            items.extend([
                mi("Move earlier", MOVE_UP, e.entry.position > 0),
                mi("Move later", MOVE_DOWN, e.entry.position + 1 < v.entries.len()),
                mi(if e.entry.skipped { "Unskip" } else { "Skip in this list" }, SKIP, true),
                mi("Make next", NEXT, true),
                mi("Seasons…", SEASONS, e.entry.kind == ItemKind::Series),
                mi("Use another copy…", COPY, e.entry.item.is_some()),
                mi("Find in my playlists…", FIND, e.entry.item.is_none()),
                mi("Remove", REMOVE, true),
            ]);
            (shared::label(ctx, &e), items)
        } else {
            return;
        };
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }
    fn choose(&self, ctx: &mut UiCtx<'_>, code: u8) {
        let Some(v) = self.view() else { return };
        let id = v.list.id;
        match code {
            PLAY => self.play(ctx, false),
            ONE => self.play(ctx, true),
            NEW => self.new_list(ctx),
            SHOW_WATCHED => {
                let show = !v.show_watched;
                shared::write(ctx, "Watched entries updated", move |c| {
                    store::set_show_watched(c, id, show)?;
                    Ok(None)
                });
            }
            RENAME if !v.list.is_default => ctx.open(OverlayRequest::Rename(RenameSpec {
                title: "Rename watchlist".into(),
                initial: v.list.name.clone(),
                target: RenameTarget::Watchlist(id),
            })),
            DUPLICATE => ctx.open(OverlayRequest::Rename(RenameSpec {
                title: "Duplicate watchlist".into(),
                initial: format!("{} (copy)", v.list.name),
                target: RenameTarget::DuplicateWatchlist(id),
            })),
            DELETE_LIST if !v.list.is_default => ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                title: format!("Delete {}?", v.list.name),
                body: "The list and its entries will be removed. Your watch history stays.".into(),
                confirm_label: "Delete".into(),
                destructive: true,
                token: DELETE,
            })),
            START_AGAIN => ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                title: format!("Start {} again?", v.list.name),
                body: "Everything in this list will count as unwatched. Your watch history stays.".into(),
                confirm_label: "Start again".into(),
                destructive: false,
                token: RESTART,
            })),
            REMOVE => {
                if let Some(e) = self.entry() {
                    shared::write(ctx, "Removed from watchlist", move |c| store::remove(c, id, &[e.entry.id]).map(Some));
                }
            }
            SKIP => {
                if let Some(e) = self.entry() {
                    shared::write(ctx, if e.entry.skipped { "Unskipped" } else { "Skipped in this list" }, move |c| {
                        store::skip(c, id, &[e.entry.id], !e.entry.skipped)?;
                        Ok(None)
                    });
                }
            }
            NEXT => {
                if let Some(e) = self.entry() {
                    shared::write(ctx, "Made next", move |c| {
                        store::make_next(c, id, e.entry.id)?;
                        Ok(None)
                    });
                }
            }
            SEASONS => {
                if let Some(e) = self.entry() {
                    ctx.open(OverlayRequest::WatchlistPicker(crate::ui::overlays::watchlists::PickerSpec {
                        omitted: (0, 0),
                        items: e.entry.item.into_iter().collect(),
                        title: e.entry.title,
                        edit: Some(e.entry.id),
                        create: false,
                    }));
                }
            }
            FIND => {
                if let Some(e) = self.entry() {
                    ctx.open(OverlayRequest::WatchlistFind { entry: e.entry.id, kind: e.entry.kind, title: e.entry.title });
                }
            }
            _ => {}
        }
    }
    fn details(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(e) = self.entry() else { return };
        let Some(item) = e.playable.as_ref().or(e.entry.item.as_ref()) else { return };
        let route = match item.kind {
            ItemKind::Movie => crate::store::vod::resolve_movie(ctx.db, item).ok().flatten().map(Route::MovieDetail),
            ItemKind::Episode => crate::store::vod::resolve_episode(ctx.db, item)
                .ok()
                .flatten()
                .and_then(|id| crate::store::vod::episode(ctx.db, id).ok())
                .map(|ep| Route::SeriesDetailAt { series: ep.series_id, season: ep.season, episode: ep.episode }),
            ItemKind::Series => resolve_series(ctx.db, item).ok().flatten().map(|series| {
                let (season, episode) = e.episode.unwrap_or((e.entry.season_from.unwrap_or(1), 1));
                Route::SeriesDetailAt { series, season, episode }
            }),
            _ => None,
        };
        if let Some(route) = route {
            self.return_to = Some((self.route_list, e.entry.list_id, e.entry.id));
            ctx.push(route);
        }
    }
    fn mark(&mut self, ctx: &mut UiCtx<'_>, all: bool) {
        let Some(e) = self.entry() else { return };
        let on = all || e.status != EntryStatus::Done;
        if matches!(e.entry.kind, ItemKind::Series | ItemKind::Episode)
            && let Some(item) = &e.entry.item
        {
            let series = if item.kind == ItemKind::Series {
                Some(item.clone())
            } else {
                crate::store::vod::resolve_episode(ctx.db, item)
                    .ok()
                    .flatten()
                    .and_then(|id| crate::store::vod::episode(ctx.db, id).ok())
                    .and_then(|ep| crate::store::vod::series(ctx.db, ep.series_id).ok())
                    .map(|s| ItemRef { source: s.source_id, kind: ItemKind::Series, key: s.key })
            };
            let ids = series
                .and_then(|r| store::title_copies(ctx.db, ctx.lib, &r).ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|r| resolve_series(ctx.db, r).ok().flatten())
                .collect::<Vec<_>>();
            let unloaded = crate::store::copies::unloaded_series(ctx.db, &ids).unwrap_or_default();
            if !unloaded.is_empty() {
                self.fetch.start(ctx, &unloaded);
                self.pending_mark = Some((e, on, all));
                ctx.toast(crate::msg::Toast::info("Loading episodes…"));
                return;
            }
        }
        Self::finish_mark(ctx, e, on, all);
    }
    fn finish_mark(ctx: &UiCtx<'_>, e: EntryState, on: bool, all: bool) {
        if e.entry.kind == ItemKind::Series && on && !all {
            shared::mark_current_watched(ctx, e);
        } else {
            shared::mark_watched(ctx, e, on);
        }
    }
    fn copies(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(e) = self.entry() else { return };
        let Some(item) = e.entry.item.as_ref() else { return };
        let idx = match item.kind {
            ItemKind::Movie => {
                crate::store::vod::resolve_movie(ctx.db, item).ok().flatten().and_then(|id| ctx.lib.vod.index_of(crate::model::VodKind::Movie(id)))
            }
            ItemKind::Series => resolve_series(ctx.db, item).ok().flatten().and_then(|id| ctx.lib.vod.index_of(crate::model::VodKind::Series(id))),
            ItemKind::Episode => crate::store::vod::resolve_episode(ctx.db, item)
                .ok()
                .flatten()
                .and_then(|id| crate::store::vod::episode(ctx.db, id).ok())
                .and_then(|ep| ctx.lib.vod.index_of(crate::model::VodKind::Series(ep.series_id))),
            _ => None,
        };
        let Some(plan) = idx.and_then(|i| crate::ui::copies::plan(ctx, item.kind == ItemKind::Movie, i).ok()) else { return };
        self.copies = plan.copies.iter().map(|c| c.item_ref.clone()).collect();
        if item.kind == ItemKind::Episode
            && let Some((s, n)) = e.episode
        {
            self.copies = self
                .copies
                .iter()
                .filter_map(|r| resolve_series(ctx.db, r).ok().flatten())
                .filter_map(|id| crate::store::copies::same_episode(ctx.db, id, s, n).ok().flatten())
                .filter_map(|id| crate::store::vod::episode(ctx.db, id).ok())
                .map(|ep| ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key })
                .collect();
        }
        self.menu_entry = Some(e.entry.id);
        let items = self
            .copies
            .iter()
            .enumerate()
            .take(100)
            .map(|(i, r)| MenuItem {
                icon: glyphs::PLAYLIST,
                label: ctx.lib.source(r.source).map(|s| s.name.clone()).unwrap_or_default(),
                action: Action::Digit(i as u8),
                enabled: true,
                shortcut: None,
            })
            .collect();
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Use another copy".into(), items }));
    }
    fn lists(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let mut select = None;
        let mut menu = false;
        self.list_drag_to = None;
        if self.list_drag.is_some()
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        {
            let clip = ui.clip_rect();
            let edge = 32.0 * ctx.theme.rem();
            let dy = if p.y < clip.top() + edge {
                6.0
            } else if p.y > clip.bottom() - edge {
                -6.0
            } else {
                0.0
            };
            if dy != 0.0 {
                ui.scroll_with_delta(egui::vec2(0.0, dy));
                ui.ctx().request_repaint();
            }
        }
        for v in &self.snapshot.lists {
            let next = v
                .next_entry()
                .map(|e| format!("next {}", shared::label(ctx, e)))
                .unwrap_or_else(|| if v.finished() { "✓ Done" } else { "No playable entries" }.into());
            let title = if v.finished() { format!("✓ {} (done)", v.list.name) } else { v.list.name.clone() };
            let label = format!("{title}   {}/{}\n{next}", v.done, v.total);
            let row = ui.horizontal(|ui| {
                let (grip, drag) = ui.allocate_exact_size(
                    egui::vec2(24.0 * ctx.theme.rem(), 38.0 * ctx.theme.rem()),
                    if v.list.is_default { egui::Sense::hover() } else { egui::Sense::drag() },
                );
                if !v.list.is_default {
                    let drag = drag.on_hover_cursor(egui::CursorIcon::Grab);
                    drag.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Drag watchlist {}", v.list.name)));
                    ui.painter().text(
                        grip.center(),
                        egui::Align2::CENTER_CENTER,
                        glyphs::REORDER,
                        egui::FontId::proportional(ctx.theme.font(TypeToken::Body)),
                        ctx.theme.tokens.text_secondary,
                    );
                    if drag.drag_started() {
                        self.list_drag = Some(v.list.id);
                        select = Some(v.list.id);
                        self.lists_focus = true;
                    }
                }
                ui.selectable_label(self.selected == Some(v.list.id), label)
            });
            let r = row.inner;
            if crate::ui::widgets::fresh_click(&r) {
                select = Some(v.list.id);
                self.lists_focus = false;
            }
            if r.secondary_clicked() {
                select = Some(v.list.id);
                self.lists_focus = true;
                menu = true;
            }
            if !v.list.is_default
                && self.list_drag.and_then(|id| self.snapshot.list(id)).is_some_and(|drag| drag.finished() == v.finished())
                && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| row.response.rect.intersect(ui.clip_rect()).contains(p))
            {
                self.list_drag_to = Some(v.list.id);
                ui.painter().hline(row.response.rect.x_range(), row.response.rect.top(), egui::Stroke::new(2.0, ctx.theme.tokens.accent));
            }
        }
        if ui.input(|i| i.pointer.any_released())
            && let Some(id) = self.list_drag.take()
            && let Some(to) = self.list_drag_to.take()
        {
            self.move_list(ctx, id, to);
        }
        if let Some(id) = select {
            self.select(id);
        }
        if menu {
            self.menu(ctx);
        }
        if button(ui, ctx.theme, Some(glyphs::ADD), "New list…", ButtonStyle::default()).clicked() {
            self.new_list(ctx);
        }
    }
    fn entries_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.drag_to = None;
        let Some(v) = self.view().cloned() else { return };
        let theme = ctx.theme;
        let mut order = false;
        ui.horizontal_wrapped(|ui| {
            ui.heading(&v.list.name);
            if let Some(note) = &v.list.note {
                ui.label(note);
            }
            ui.label(format!("{} of {} · {} left", v.done, v.total, crate::queue::ui::duration(v.remaining_s)));
            order = ui.button(if v.list.play_order == PlayOrder::Any { "Any order ▾" } else { "In order ▾" }).clicked();
            if ui.button("Play").clicked() {
                shared::play(ctx, v.list.id, None, false);
            }
            if ui.button("⋯").clicked() {
                self.lists_focus = true;
                self.menu(ctx);
            }
        });
        if order {
            self.on_action(ctx, Action::WatchlistOrder);
        }
        let watched = v.entries.iter().filter(|e| e.status == EntryStatus::Done).count();
        if watched > 0
            && ui
                .button(format!(
                    "✓ {}{watched} watched · {}",
                    if v.finished() { "All " } else { "" },
                    if v.show_watched { "Hide watched" } else { "Show watched" }
                ))
                .clicked()
        {
            shared::write(ctx, "Watched entries updated", move |c| {
                store::set_show_watched(c, v.list.id, !v.show_watched)?;
                Ok(None)
            });
        }
        let entries: Vec<_> = self.shown().into_iter().cloned().collect();
        if entries.is_empty() {
            if !v.finished() {
                ui.label(if v.list.is_default { "Save films and series here with Ctrl+F." } else { "Save films and series here with Ctrl+Shift+F." });
            }
            return;
        }
        let height = (54.0 * theme.rem()).round();
        let mut play = None;
        let mut menu = false;
        let mut dropped = None;
        egui::ScrollArea::vertical().id_salt("watchlist-entries").show_rows(ui, height, entries.len(), |ui, range| {
            if self.drag.is_some()
                && let Some(p) = ui.input(|i| i.pointer.interact_pos())
            {
                let clip = ui.clip_rect();
                let edge = 32.0 * theme.rem();
                let dy = if p.y < clip.top() + edge {
                    6.0
                } else if p.y > clip.bottom() - edge {
                    -6.0
                } else {
                    0.0
                };
                if dy != 0.0 {
                    ui.scroll_with_delta(egui::vec2(0.0, dy));
                    ui.ctx().request_repaint();
                }
            }
            for i in range {
                let e = &entries[i];
                let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, shared::label(ctx, e)));
                let selected = self.cursor == i && !self.lists_focus;
                let t = &theme.tokens;
                if selected || r.hovered() {
                    ui.painter().rect_filled(rect, t.rounding, t.list_cursor_bg);
                }
                let dim = e.done() || e.status == EntryStatus::Missing || e.locked;
                let color = if dim { t.text_secondary } else { t.fg };
                let grip = egui::Rect::from_min_size(rect.min, egui::vec2(28.0 * theme.rem(), height));
                let drag = ui.interact(grip, r.id.with("grip"), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
                drag.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Drag {}", shared::label(ctx, e))));
                ui.painter().text(
                    grip.center(),
                    egui::Align2::CENTER_CENTER,
                    glyphs::REORDER,
                    egui::FontId::proportional(theme.font(TypeToken::Body)),
                    t.text_secondary,
                );
                if drag.drag_started() {
                    self.drag = Some(e.entry.id);
                    self.cursor = i;
                    self.lists_focus = false;
                }
                if self.drag.is_some() && ui.input(|x| x.pointer.interact_pos()).is_some_and(|p| rect.intersect(ui.clip_rect()).contains(p)) {
                    self.drag_to = Some(e.entry.position);
                    ui.painter().hline(rect.x_range(), rect.top(), egui::Stroke::new(2.0, t.accent));
                }
                let number =
                    if v.list.play_order == PlayOrder::Ordered { format!("{}  ", v.place(e.entry.id).map_or(i + 1, |x| x.0)) } else { String::new() };
                let mark = if e.entry.skipped {
                    "↷ "
                } else if e.status == EntryStatus::Missing {
                    "⊘ "
                } else if e.locked {
                    "󰌾 "
                } else if e.done() {
                    "✓ "
                } else {
                    ""
                };
                let title = format!("{number}{mark}{}", shared::label(ctx, e));
                let x = grip.right() + 4.0;
                crate::ui::video_host::osd_text(
                    ui.painter(),
                    egui::pos2(x, rect.top() + height * 0.3),
                    egui::Align2::LEFT_CENTER,
                    &title,
                    egui::FontId::proportional(theme.font(TypeToken::Body)),
                    color,
                    rect.right() - x - 8.0,
                );
                let mut detail = if e.status == EntryStatus::Missing {
                    "Not in your playlists".into()
                } else if let Some((s, n)) = e.episode {
                    format!("next S{s}E{n} · {} of {} watched", e.done_episodes, e.total_episodes)
                } else if let Some(p) = &e.progress {
                    format!("{} watched", kit::clock(p.position_s))
                } else {
                    String::new()
                };
                if ctx.layout != LayoutClass::Compact {
                    if let Some(copy) = &e.copy_name {
                        detail.push_str(&format!(" · {copy}"));
                    }
                    if e.duration_s.is_some() {
                        detail.push_str(&format!(" · {}", crate::queue::ui::duration(e.duration_s)));
                    }
                }
                crate::ui::video_host::osd_text(
                    ui.painter(),
                    egui::pos2(x, rect.top() + height * 0.72),
                    egui::Align2::LEFT_CENTER,
                    &detail,
                    egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                    t.text_secondary,
                    rect.right() - x - 8.0,
                );
                if let Some(p) = &e.progress
                    && let Some(d) = p.duration_s.filter(|d| *d > 0.0)
                {
                    let bar = egui::Rect::from_min_size(
                        egui::pos2(x, rect.bottom() - 3.0),
                        egui::vec2((rect.right() - x) * (p.position_s / d).clamp(0.0, 1.0) as f32, 2.0),
                    );
                    ui.painter().rect_filled(bar, 0.0, t.accent);
                }
                if crate::ui::widgets::fresh_click(&r) && !grip.contains(ui.input(|x| x.pointer.interact_pos()).unwrap_or_default()) {
                    self.cursor = i;
                    self.lists_focus = false;
                    play = Some(e.entry.id);
                }
                if r.secondary_clicked() {
                    self.cursor = i;
                    self.lists_focus = false;
                    menu = true;
                }
                if self.scroll && self.cursor == i {
                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    self.scroll = false;
                }
            }
            if ui.input(|x| x.pointer.any_released())
                && let Some(id) = self.drag.take()
            {
                dropped = self.drag_to.take().map(|to| (id, to));
            }
        });
        if let Some((id, to)) = dropped {
            self.move_entry(ctx, id, to);
        }
        if let Some(id) = play {
            shared::play(ctx, v.list.id, Some(id), false);
        }
        if menu {
            self.menu(ctx);
        }
    }
}
impl Screen for WatchlistsScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Watchlists
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Watchlists, KeyContext::List]
    }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let Route::Watchlists { list: target } = route else { return };
        self.route_list = *target;
        self.sync(ctx);
        if let Some((route_list, list, entry)) = self.return_to.take()
            && route_list == *target
        {
            self.selected = Some(list);
            self.revealed_entry = Some(entry);
            self.cursor = self.shown().iter().position(|e| e.entry.id == entry).unwrap_or(0);
            self.lists_focus = false;
            self.opening = false;
            self.scroll = true;
            return;
        }
        if let Route::Watchlists { list: Some(id) } = route {
            self.selected = Some(*id);
        }
        if let Some(id) = self.selected {
            self.select(id);
        }
        self.lists_focus = false;
        self.opening = true;
    }
    fn on_event(&mut self, _ctx: &mut UiCtx<'_>, event: &AppEvent) {
        if matches!(event, AppEvent::Data(_)) {
            shared::invalidate();
        }
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, a: Action) -> bool {
        self.sync(ctx);
        match a {
            Action::Back if self.drag.is_some() || self.list_drag.is_some() => {
                self.list_drag = None;
                self.list_drag_to = None;
                self.drag = None;
                self.drag_to = None;
            }
            Action::Parent | Action::Left => self.lists_focus = true,
            Action::Right => self.lists_focus = false,
            Action::FocusNext | Action::FocusPrev if ctx.layout != LayoutClass::Wide => {
                self.lists_focus = false;
                let n = self.snapshot.lists.len();
                if n > 0 {
                    let i = self.selected.and_then(|id| self.snapshot.lists.iter().position(|l| l.list.id == id)).unwrap_or(0);
                    let j = if a == Action::FocusNext { (i + 1) % n } else { (i + n - 1) % n };
                    self.select(self.snapshot.lists[j].list.id);
                }
            }
            Action::FocusNext | Action::FocusPrev => self.lists_focus = !self.lists_focus,
            Action::Up | Action::Down => {
                let delta = if a == Action::Up { -1 } else { 1 };
                if self.lists_focus {
                    let n = self.snapshot.lists.len();
                    if n > 0 {
                        let i = self.selected.and_then(|id| self.snapshot.lists.iter().position(|l| l.list.id == id)).unwrap_or(0);
                        let j = (i as isize + delta).clamp(0, n as isize - 1) as usize;
                        self.select(self.snapshot.lists[j].list.id);
                    }
                } else {
                    self.cursor = (self.cursor as isize + delta).clamp(0, self.shown().len().saturating_sub(1) as isize) as usize;
                    self.scroll = true;
                }
            }
            Action::Activate | Action::ActivateAlt if self.lists_focus => self.lists_focus = false,
            Action::Activate | Action::ActivateAlt => self.play(ctx, false),
            Action::WatchlistOne if !self.lists_focus => self.play(ctx, true),
            Action::WatchlistNew => self.new_list(ctx),
            Action::WatchlistSkip if !self.lists_focus => self.choose(ctx, SKIP),
            Action::WatchlistOrder => {
                if let Some(v) = self.view() {
                    let id = v.list.id;
                    let order = if v.list.play_order == PlayOrder::Any { PlayOrder::Ordered } else { PlayOrder::Any };
                    shared::write(ctx, "Play order updated", move |c| {
                        store::set_order(c, id, order)?;
                        Ok(None)
                    });
                }
            }
            Action::MoveUp | Action::MoveDown => {
                let delta = if a == Action::MoveUp { -1 } else { 1 };
                if self.lists_focus {
                    if let Some((id, to)) = self.selected.zip(self.list_move_target(a == Action::MoveDown)) {
                        self.move_list(ctx, id, to);
                    }
                } else if let Some(e) = self.entry() {
                    let n = self.view().map_or(0, |v| v.entries.len());
                    self.move_entry(ctx, e.entry.id, (e.entry.position as isize + delta).clamp(0, n.saturating_sub(1) as isize) as usize);
                }
            }
            Action::Remove => self.remove(ctx),
            Action::Rename if self.lists_focus => self.choose(ctx, RENAME),
            Action::ContextMenu => self.menu(ctx),
            Action::PlayFrom if !self.lists_focus => self.copies(ctx),
            Action::QueueAdd | Action::QueuePlayNext if !self.lists_focus => {
                if let Some(e) = self.entry() {
                    crate::queue::ui::send(
                        ctx,
                        crate::queue::QueueCommand::AddListEntry {
                            list: e.entry.list_id,
                            entry: e.entry.id,
                            at: if a == Action::QueuePlayNext { crate::queue::Insert::Next } else { crate::queue::Insert::End },
                        },
                    );
                }
            }
            Action::MarkWatched if !self.lists_focus => self.mark(ctx, false),
            Action::ShowDetails if !self.lists_focus => self.details(ctx),
            _ => return false,
        }
        true
    }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, r: OverlayResult) {
        match r {
            OverlayResult::Chosen(Action::Digit(COPY)) => self.copies(ctx),
            OverlayResult::Chosen(Action::Digit(WATCHED)) => self.mark(ctx, false),
            OverlayResult::Chosen(Action::Digit(ALL_WATCHED)) => self.mark(ctx, true),
            OverlayResult::Chosen(Action::Digit(DETAILS)) => self.details(ctx),
            OverlayResult::Chosen(Action::Digit(code @ (QUEUE | QUEUE_NEXT | MOVE_UP | MOVE_DOWN))) => {
                self.on_action(
                    ctx,
                    match code {
                        QUEUE => Action::QueueAdd,
                        QUEUE_NEXT => Action::QueuePlayNext,
                        MOVE_UP => Action::MoveUp,
                        _ => Action::MoveDown,
                    },
                );
            }
            OverlayResult::Chosen(Action::Digit(c)) if c < 100 => {
                if let Some((id, item)) = self.menu_entry.zip(self.copies.get(c as usize).cloned()) {
                    shared::write(ctx, "Copy selected", move |db| {
                        store::pin(db, id, &item)?;
                        Ok(None)
                    });
                }
            }
            OverlayResult::Chosen(Action::Digit(c)) => self.choose(ctx, c),
            OverlayResult::Confirmed(DELETE) => {
                if let Some(id) = self.selected {
                    shared::write(ctx, "Watchlist deleted", move |c| store::delete(c, id).map(Some));
                    crate::queue::ui::send(ctx, crate::queue::QueueCommand::RefreshLists);
                }
            }
            OverlayResult::Confirmed(RESTART) => {
                if let Some(id) = self.selected {
                    let now = ctx.now;
                    shared::write(ctx, "Started again", move |c| store::restart(c, id, now).map(Some));
                }
            }
            _ => {}
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.sync(ctx);
        if let Some(failed) = self.fetch.poll(ctx, ui.ctx()) {
            if failed.is_empty() {
                shared::invalidate();
                self.sync(ctx);
            } else {
                ctx.toast(crate::msg::Toast::error("Some copies couldn't load their episodes"));
                self.pending_mark = None;
            }
        }
        if !self.fetch.busy()
            && !shared::loading()
            && let Some((e, on, all)) = self.pending_mark.take()
        {
            // Keep a known episode fixed while other copies load. If no copy had episodes,
            // use this entry's current episode from the refreshed view, even after moving.
            let e = if e.entry.kind == ItemKind::Series && on && !all && e.episode.is_none() {
                self.snapshot.list(e.entry.list_id).and_then(|v| v.entries.iter().find(|current| current.entry.id == e.entry.id)).cloned()
            } else {
                Some(e)
            };
            if let Some(e) = e {
                Self::finish_mark(ctx, e, on, all);
            }
        }
        section::page_header(ui, ctx, "Watchlists", |_| {});
        if shared::loading() && self.snapshot.lists.is_empty() {
            ui.spinner();
            ui.label("Loading watchlists…");
            return;
        }
        if self.fetch.busy() {
            ui.label("Loading episodes to mark them watched…");
        }
        if ctx.layout == LayoutClass::Wide {
            let rect = ui.available_rect_before_wrap();
            let width = (260.0 * ctx.theme.rem()).min(rect.width() * 0.32);
            let gap = ctx.theme.space(SpaceToken::Lg);
            let left = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + width, rect.bottom()));
            ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.lists(ui, ctx));
            });
            let right = egui::Rect::from_min_max(egui::pos2(left.right() + gap, rect.top()), rect.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| self.entries_ui(ui, ctx));
            ui.advance_cursor_after_rect(rect);
        } else {
            let selected = self.view().map(|v| v.list.name.clone()).unwrap_or_else(|| "Watchlists".into());
            let mut selected_id = None;
            let switcher = egui::ComboBox::from_id_salt("watchlist-switcher").selected_text(selected).show_ui(ui, |ui| {
                for list in &self.snapshot.lists {
                    if ui.selectable_label(self.selected == Some(list.list.id), &list.list.name).clicked() {
                        selected_id = Some(list.list.id);
                    }
                }
            });
            if self.lists_focus {
                switcher.response.request_focus();
            }
            if let Some(id) = selected_id {
                self.select(id);
                self.lists_focus = false;
            }
            self.entries_ui(ui, ctx);
        }
    }
    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        vec![(Action::QueueAdd, "add to Up next"), (Action::MoveUp, "move"), (Action::WatchlistSkip, "skip"), (Action::Remove, "remove")]
    }
}
use crate::ui::screens::home::kit;

fn resolve_series(c: &rusqlite::Connection, item: &ItemRef) -> crate::error::Result<Option<crate::model::SeriesId>> {
    use rusqlite::OptionalExtension;
    Ok(c.query_row("SELECT id FROM series WHERE source_id=?1 AND key=?2", rusqlite::params![item.source, item.key.as_str()], |r| r.get(0))
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, SourceId};
    use crate::ui::screens::home::testkit::{self, harness, rig, seed, until};
    use egui_kittest::kittest::Queryable;

    fn movie(source: SourceId, n: usize) -> ItemRef {
        ItemRef { source, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{n:010}")) }
    }

    type H = egui_kittest::Harness<'static, testkit::App<WatchlistsScreen>>;

    fn unloaded_series() -> H {
        let (mut t, one) = rig();
        let list = seed(&mut t, |c| {
            for name in ["TV", "TV 2"] {
                let src = testkit::source(c, name)?;
                testkit::series(c, src, 1, 2, 4)?;
                c.execute("UPDATE series SET year=2020, details_at=NULL WHERE source_id=?1", [src])?;
                c.execute("UPDATE source SET kind='xtream' WHERE id=?1", [src])?;
                crate::store::titles::refresh_source(c, src)?;
            }
            testkit::movies(c, SourceId(1), 1)?;
            let list = store::create(c, "Unloaded episodes", None, PlayOrder::Ordered, "app", 1)?;
            let lib = crate::store::library::load(c)?;
            let series = ItemRef { source: SourceId(1), kind: ItemKind::Series, key: ItemKey("m3u:series000000000".into()) };
            let mut range = store::new_entry(c, &series)?;
            range.season_from = Some(2);
            range.season_to = Some(2);
            store::add(c, &lib, list, &[range, store::new_entry(c, &movie(SourceId(1), 0))?], crate::watchlist::Insert::End, 1)?;
            crate::store::user::save_progress(
                c,
                &crate::model::WatchProgress {
                    item: ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey("m3u:ep0-2-3".into()) },
                    position_s: 90.0,
                    duration_s: Some(2700.0),
                    updated_at: 2,
                    finished: false,
                    title: "Episode 3".into(),
                    fingerprint: Default::default(),
                },
            )?;
            // The fake load restores these rows when the test chooses; no provider needed.
            c.execute_batch("CREATE TEMP TABLE unloaded_episodes AS SELECT * FROM episode; DELETE FROM episode;")?;
            Ok(list)
        });
        let mut h = harness(t, one, WatchlistsScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Watchlists { list: Some(list) });
        until(&mut h, "unloaded series entry", |h| h.state().s.entry().is_some_and(|e| e.entry.kind == ItemKind::Series));
        assert!(h.state().s.entry().unwrap().episode.is_none());
        h
    }

    #[test]
    fn mark_current_loads_episodes_and_marks_the_refreshed_entry_after_moving_away() {
        let mut h = unloaded_series();
        h.state_mut().act(Action::MarkWatched);
        assert!(h.state().s.fetch.busy());
        assert!(h.state().t.toasts.iter().any(|t| t.text == "Loading episodes…"));
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().s.entry().unwrap().entry.kind, ItemKind::Movie);
        seed(&mut h.state_mut().t, |c| {
            c.execute_batch("INSERT INTO episode SELECT * FROM unloaded_episodes; UPDATE series SET details_at=1;")?;
            Ok(())
        });
        until(&mut h, "numbered watched toast", |h| h.state().t.toasts.iter().any(|t| t.text == "Marked S02E03 watched"));
        let db = &h.state().t.db;
        for source in [1, 2] {
            for season in 1..=2 {
                for episode in 1..=4 {
                    let item = ItemRef { source: SourceId(source), kind: ItemKind::Episode, key: ItemKey(format!("m3u:ep0-{season}-{episode}")) };
                    let watched = crate::store::user::progress(db, &item).unwrap().is_some_and(|p| p.finished);
                    assert_eq!(watched, (season, episode) == (2, 3), "copy {source}, S{season}E{episode}");
                }
            }
        }
        assert!(crate::store::user::progress(db, &movie(SourceId(1), 0)).unwrap().is_none());
        assert_eq!(h.state().s.entry().unwrap().entry.kind, ItemKind::Movie);
        let toast = h.state_mut().undo_toast().unwrap();
        assert_eq!(toast.text, "Marked S02E03 watched");
        assert_eq!(toast.action.unwrap().0, "Undo");
    }

    #[test]
    fn unloaded_current_episode_menu_is_enabled_and_a_failed_copy_marks_nothing() {
        let mut h = unloaded_series();
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let Some(crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(menu))) = nav.last() else { panic!("{nav:?}") };
        let mark = menu.items.iter().find(|m| m.label == "Mark current episode watched").unwrap();
        assert!(mark.enabled);
        h.state_mut().result(OverlayResult::Chosen(mark.action));
        assert!(h.state().s.fetch.busy());
        assert!(h.state().t.toasts.iter().any(|t| t.text == "Loading episodes…"));
        seed(&mut h.state_mut().t, |c| {
            // One copy loads, the other's job ends without episodes.
            c.execute_batch(
                "INSERT INTO episode SELECT * FROM unloaded_episodes WHERE source_id=1; UPDATE series SET details_at=1 WHERE source_id=1;",
            )?;
            Ok(())
        });
        until(&mut h, "load failure toast", |h| h.state().t.toasts.iter().any(|t| t.text == "Some copies couldn't load their episodes"));
        assert!(h.state().s.pending_mark.is_none());
        assert!(!h.state().s.fetch.busy());
        h.state().t.services().store.writer().blocking(|_| Ok(())).unwrap();
        assert_eq!(h.state().t.db.query_row("SELECT count(*) FROM watch_progress WHERE finished=1", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert!(!h.state().t.toasts.iter().any(|t| t.text.starts_with("Marked ")));
    }

    #[test]
    fn ctrl_f_saves_films_to_my_list_and_channels_to_favourites() {
        let (mut t, one) = rig();
        let (src, films, channels) = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            Ok((src, testkit::movies(c, src, 1)?, testkit::channels(c, src, 1)?))
        });
        let mut h = harness(t, one, crate::ui::screens::movie_detail::MovieDetailScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::MovieDetail(films[0]));
        h.state_mut().act(Action::ToggleFavourite);
        until(&mut h, "film saved", |h| {
            h.state().t.db.query_row("SELECT count(*) FROM watchlist_item WHERE kind='movie'", [], |r| r.get::<_, i64>(0)).unwrap() == 1
        });
        let mut live = crate::ui::screens::live::LiveScreen::default();
        h.state_mut().t.with_ctx(|ctx| {
            live.on_enter(ctx, &Route::Live { group: None, channel: Some(channels[0]) });
            live.on_action(ctx, Action::ToggleFavourite);
        });
        until(&mut h, "channel saved", |h| {
            h.state().t.db.query_row("SELECT count(*) FROM fav_item WHERE kind='channel'", [], |r| r.get::<_, i64>(0)).unwrap() == 1
        });
        assert_eq!(h.state().t.db.query_row("SELECT count(*) FROM fav_item WHERE kind!='channel'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert!(h.state().t.favs.is_fav(&movie(src, 0)));
    }

    #[test]
    fn lists_reorder_by_keyboard_and_grip_with_pinned_default_and_done_group() {
        let (mut t, one) = rig();
        let (mine, a, b, done) = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            testkit::movies(c, src, 3)?;
            let lib = crate::store::library::load(c)?;
            let mine = store::default_list(c)?;
            let mut lists = Vec::new();
            for (i, name) in ["First", "Second", "Finished"].iter().enumerate() {
                let id = store::create(c, name, None, PlayOrder::Ordered, "app", 1)?;
                store::add(c, &lib, id, &[store::new_entry(c, &movie(src, i))?], crate::watchlist::Insert::End, 1)?;
                lists.push(id);
            }
            crate::store::user::mark_watched(c, &[(movie(src, 2), "Movie 2".into())], true, 2)?;
            store::played(c, lists[1], 10)?;
            Ok((mine, lists[0], lists[1], lists[2]))
        });
        let mut h = harness(t, one, WatchlistsScreen::default(), egui::vec2(1400.0, 800.0));
        h.state_mut().enter(Route::Watchlists { list: Some(a) });
        let order =
            |h: &egui_kittest::Harness<'_, testkit::App<WatchlistsScreen>>| h.state().s.snapshot.lists.iter().map(|l| l.list.id).collect::<Vec<_>>();
        until(&mut h, "manual order", |h| order(h) == [mine, a, b, done]);
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::MoveDown);
        until(&mut h, "keyboard list order", |h| order(h) == [mine, b, a, done]);
        assert_eq!(h.state().s.selected, Some(a));
        assert!(h.state().s.list_move_target(true).is_none(), "unfinished lists cannot move below the done group");
        h.state_mut().act(Action::MoveUp);
        until(&mut h, "restored list order", |h| order(h) == [mine, a, b, done]);
        assert!(h.state().s.list_move_target(false).is_none(), "My list stays first");
        h.run_steps(2);
        let from = h.get_by_label("Drag watchlist First").rect().center();
        let to = h.get_by_label("Drag watchlist Second").rect().center();
        h.hover_at(from);
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(to);
        h.run_steps(2);
        h.drop_at(to);
        h.run_steps(2);
        until(&mut h, "grip list order", |h| order(h) == [mine, b, a, done]);
        assert_eq!(h.state().s.selected, Some(a));
        assert!(h.state().s.lists_focus);
        // Escape during a second drag leaves the persisted order alone.
        let from = h.get_by_label("Drag watchlist First").rect().center();
        let to = h.get_by_label("Drag watchlist Second").rect().center();
        h.hover_at(from);
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(to);
        h.run_steps(2);
        h.state_mut().act(Action::Back);
        h.drop_at(to);
        h.run_steps(2);
        assert_eq!(order(&h), [mine, b, a, done]);
        assert_eq!(store::lists(&h.state().t.db).unwrap().iter().map(|l| l.id).collect::<Vec<_>>(), [mine, b, a, done]);
        h.state_mut().s.select(mine);
        h.state_mut().act(Action::MoveDown);
        assert!(h.state().s.list_move_target(true).is_none());
    }

    #[test]
    fn entries_open_on_next_and_keyboard_and_drag_store_the_same_order() {
        let (mut t, one) = rig();
        let list = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            testkit::movies(c, src, 3)?;
            let lib = crate::store::library::load(c)?;
            let list = store::create(c, "Saga", None, PlayOrder::Ordered, "app", 1)?;
            let rows = (0..3).map(|i| store::new_entry(c, &movie(src, i))).collect::<crate::error::Result<Vec<_>>>()?;
            store::add(c, &lib, list, &rows, crate::watchlist::Insert::End, 1)?;
            crate::store::user::mark_watched(c, &[(movie(src, 0), "Movie 0".into())], true, 2)?;
            Ok(list)
        });
        let mut h = harness(t, one, WatchlistsScreen::default(), egui::vec2(1400.0, 800.0));
        h.state_mut().enter(Route::Watchlists { list: Some(list) });
        until(&mut h, "next entry", |h| h.state().s.entry().is_some_and(|e| e.entry.title == "Movie 1"));
        assert_eq!(h.state().s.entry().unwrap().status, EntryStatus::Next);
        h.state_mut().act(Action::MoveDown);
        let order = |h: &egui_kittest::Harness<'_, testkit::App<WatchlistsScreen>>| {
            store::entries(&h.state().t.db, list).unwrap().into_iter().map(|e| e.title).collect::<Vec<_>>()
        };
        until(&mut h, "keyboard order", |h| order(h) == ["Movie 0", "Movie 2", "Movie 1"]);
        until(&mut h, "updated rows", |h| h.state().s.shown().first().is_some_and(|e| e.entry.title == "Movie 2"));
        assert_eq!(h.state().s.entry().unwrap().entry.title, "Movie 1", "focus follows the moved id while watched rows are folded");
        // Move back, then perform the same move by its drag-only handle.
        let id = h.state().s.view().unwrap().entries.iter().find(|e| e.entry.title == "Movie 1").unwrap().entry.id;
        h.state_mut().t.services().store.writer().blocking(move |c| store::move_entries(c, list, &[id], 1)).unwrap();
        shared::invalidate();
        until(&mut h, "restored rows", |h| h.state().s.shown().first().is_some_and(|e| e.entry.title == "Movie 1"));
        h.run_steps(2);
        let label = |h: &egui_kittest::Harness<'_, testkit::App<WatchlistsScreen>>, title: &str| {
            h.state().s.shown().iter().find(|e| e.entry.title == title).map(|e| format!("{} ({})", e.entry.title, e.entry.year.unwrap())).unwrap()
        };
        let from = h.get_by_label(&format!("Drag {}", label(&h, "Movie 1"))).rect().center();
        let row = h.get_by_label(&label(&h, "Movie 2")).rect();
        h.hover_at(from);
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(egui::pos2(from.x, row.center().y));
        h.run_steps(2);
        h.drop_at(egui::pos2(from.x, row.center().y));
        h.run_steps(2);
        until(&mut h, "drag order", |h| order(h) == ["Movie 0", "Movie 2", "Movie 1"]);
        until(&mut h, "dragged rows refreshed", |h| h.state().s.shown().first().is_some_and(|e| e.entry.title == "Movie 2"));
        let from = h.get_by_label(&format!("Drag {}", label(&h, "Movie 1"))).rect().center();
        let to = h.get_by_label(&label(&h, "Movie 2")).rect().center();
        h.hover_at(from);
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(to);
        h.run_steps(2);
        let outside = egui::pos2(1390.0, 790.0);
        h.hover_at(outside);
        h.run_steps(2);
        h.drop_at(outside);
        h.run_steps(2);
        h.state().t.services().store.writer().blocking(|_| Ok(())).unwrap();
        assert_eq!(order(&h), ["Movie 0", "Movie 2", "Movie 1"], "dropping outside rows cancels the move");
    }
    #[test]
    fn watchlists_pane_navigation_and_hidden_cursor_keys_work_at_each_width() {
        for width in [1400.0, 1000.0, 600.0] {
            let (mut t, one) = rig();
            let list = seed(&mut t, |c| {
                let src = testkit::source(c, "TV")?;
                testkit::movies(c, src, 1)?;
                let lib = crate::store::library::load(c)?;
                let list = store::create(c, "Saga", None, PlayOrder::Ordered, "app", 1)?;
                store::add(c, &lib, list, &[store::new_entry(c, &movie(src, 0))?], crate::watchlist::Insert::End, 1)?;
                Ok(list)
            });
            let mut h = harness(t, one, WatchlistsScreen::default(), egui::vec2(width, 800.0));
            h.state_mut().enter(Route::Watchlists { list: Some(list) });
            until(&mut h, "list", |h| h.state().s.entry().is_some());
            h.state_mut().act(Action::Left);
            assert!(h.state().s.lists_focus);
            for action in [Action::MarkWatched, Action::WatchlistSkip, Action::QueueAdd, Action::PlayFrom, Action::WatchlistOne] {
                h.state_mut().act(action);
            }
            h.state().t.services().store.writer().blocking(|_| Ok(())).unwrap();
            assert!(!store::entries(&h.state().t.db, list).unwrap()[0].skipped);
            assert_eq!(h.state().t.db.query_row("SELECT count(*) FROM watch_progress", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            h.state_mut().act(Action::Activate);
            assert!(!h.state().s.lists_focus);
            h.state_mut().act(Action::Left);
            h.state_mut().act(Action::Right);
            assert!(!h.state().s.lists_focus);
            h.state_mut().act(Action::Left);
            h.state_mut().act(Action::FocusNext);
            assert!(!h.state().s.lists_focus);
        }
    }
}
