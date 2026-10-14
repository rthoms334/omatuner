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

#[derive(Default)]
pub struct WatchlistsScreen {
    snapshot: Arc<WatchlistsSnapshot>,
    selected: Option<WatchlistId>,
    cursor: usize,
    lists_focus: bool,
    drag: Option<WatchlistItemId>,
    drag_to: Option<usize>,
    copies: Vec<ItemRef>,
    menu_entry: Option<WatchlistItemId>,
    scroll: bool,
    opening: bool,
    fetch: crate::ui::copies::EpisodeFetch,
    pending_mark: Option<(EntryState, bool)>,
}
impl WatchlistsScreen {
    fn view(&self) -> Option<&ListView> {
        self.selected.and_then(|id| self.snapshot.list(id))
    }
    fn shown(&self) -> Vec<&EntryState> {
        let Some(v) = self.view() else { return vec![] };
        let mut entries: Vec<_> = v.entries.iter().filter(|e| v.show_watched || e.status != EntryStatus::Done).collect();
        if v.list.play_order == PlayOrder::Any {
            entries.sort_by_key(|e| e.done());
        }
        entries
    }
    fn entry(&self) -> Option<EntryState> {
        self.shown().get(self.cursor).copied().cloned()
    }
    fn select(&mut self, id: WatchlistId) {
        self.selected = Some(id);
        self.cursor = self.view().and_then(|v| v.next).and_then(|id| self.shown().iter().position(|e| e.entry.id == id)).unwrap_or(0);
        self.scroll = true;
    }
    fn sync(&mut self, ctx: &UiCtx<'_>) {
        self.snapshot = shared::snapshot(ctx);
        if self.opening && self.selected.is_some_and(|id| self.snapshot.list(id).is_some()) {
            self.select(self.selected.unwrap());
            self.opening = false;
        }
        if self.selected.is_none_or(|id| !self.opening && self.snapshot.list(id).is_none())
            && let Some(id) = self.snapshot.lists.first().map(|l| l.list.id)
        {
            self.select(id);
        }
        self.cursor = self.cursor.min(self.shown().len().saturating_sub(1));
    }
    fn new_list(&self, ctx: &mut UiCtx<'_>) {
        ctx.open(OverlayRequest::WatchlistPicker(crate::ui::overlays::watchlists::PickerSpec {
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
        self.cursor = to.min(self.shown().len().saturating_sub(1));
        self.scroll = true;
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
                    mi("Start again…", START_AGAIN, true),
                    mi("Duplicate…", DUPLICATE, true),
                    mi("Rename…", RENAME, !v.list.is_default),
                    mi("Delete…", DELETE_LIST, !v.list.is_default),
                    mi("New list…", NEW, true),
                ],
            )
        } else if let Some(e) = self.entry() {
            self.menu_entry = Some(e.entry.id);
            (
                shared::label(ctx, &e),
                vec![
                    mi("Play from here", PLAY, e.playable.is_some()),
                    mi("Play just this one", ONE, e.playable.is_some()),
                    mi("Add to Up next", QUEUE, e.playable.is_some()),
                    mi("Play next", QUEUE_NEXT, e.playable.is_some()),
                    mi(if e.done() { "Mark unwatched" } else { "Mark watched" }, WATCHED, e.entry.item.is_some()),
                    mi("Move earlier", MOVE_UP, e.entry.position > 0),
                    mi("Move later", MOVE_DOWN, e.entry.position + 1 < v.entries.len()),
                    mi(if e.entry.skipped { "Unskip" } else { "Skip in this list" }, SKIP, true),
                    mi("Make next", NEXT, true),
                    mi("Seasons…", SEASONS, e.entry.kind == ItemKind::Series),
                    mi("Use another copy…", COPY, e.entry.item.is_some()),
                    mi("Find in my playlists…", FIND, e.entry.item.is_none()),
                    mi("Remove", REMOVE, true),
                ],
            )
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
    fn mark(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(e) = self.entry() else { return };
        let on = e.status != EntryStatus::Done;
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
                self.pending_mark = Some((e, on));
                return;
            }
        }
        shared::mark_watched(ctx, e, on);
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
        let mut play = false;
        for v in &self.snapshot.lists {
            let next = v.next_entry().map(|e| format!("next {}", shared::label(ctx, e))).unwrap_or_else(|| "✓ Done".into());
            let title = if v.next.is_none() && v.total > 0 { format!("✓ {} (done)", v.list.name) } else { v.list.name.clone() };
            let label = format!("{title}   {}/{}\n{next}", v.done, v.total);
            let r = ui.selectable_label(self.selected == Some(v.list.id), label);
            if crate::ui::widgets::fresh_click(&r) {
                select = Some(v.list.id);
                play = true;
                self.lists_focus = false;
            }
            if r.secondary_clicked() {
                select = Some(v.list.id);
                self.lists_focus = true;
                menu = true;
            }
        }
        if let Some(id) = select {
            self.select(id);
            if play {
                shared::play(ctx, id, None, false);
            }
        }
        if menu {
            self.menu(ctx);
        }
        if button(ui, ctx.theme, Some(glyphs::ADD), "New list…", ButtonStyle::default()).clicked() {
            self.new_list(ctx);
        }
    }
    fn entries_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
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
        if watched > 0 && ui.button(format!("✓ {watched} watched · {}", if v.show_watched { "hide ▴" } else { "show ▾" })).clicked() {
            shared::write(ctx, "Watched entries updated", move |c| {
                store::set_show_watched(c, v.list.id, !v.show_watched)?;
                Ok(None)
            });
        }
        let entries: Vec<_> = self.shown().into_iter().cloned().collect();
        if entries.is_empty() {
            ui.label("Save films and series here with Ctrl+Shift+F.");
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
                if self.drag.is_some() && ui.input(|x| x.pointer.interact_pos()).is_some_and(|p| rect.contains(p)) {
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
        self.sync(ctx);
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
            Action::Back if self.drag.is_some() => {
                self.drag = None;
                self.drag_to = None;
            }
            Action::Parent | Action::Left => self.lists_focus = true,
            Action::Right => self.lists_focus = false,
            Action::FocusNext | Action::FocusPrev if ctx.layout != LayoutClass::Wide => {
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
            Action::Activate | Action::ActivateAlt => self.play(ctx, false),
            Action::WatchlistOne => self.play(ctx, true),
            Action::WatchlistNew => self.new_list(ctx),
            Action::WatchlistSkip => self.choose(ctx, SKIP),
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
                    return false;
                } else if let Some(e) = self.entry() {
                    let n = self.view().map_or(0, |v| v.entries.len());
                    self.move_entry(ctx, e.entry.id, (e.entry.position as isize + delta).clamp(0, n.saturating_sub(1) as isize) as usize);
                }
            }
            Action::Remove => self.remove(ctx),
            Action::Rename if self.lists_focus => self.choose(ctx, RENAME),
            Action::ContextMenu => self.menu(ctx),
            Action::PlayFrom => self.copies(ctx),
            Action::QueueAdd | Action::QueuePlayNext => {
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
            Action::MarkWatched => self.mark(ctx),
            _ => return false,
        }
        true
    }
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, r: OverlayResult) {
        match r {
            OverlayResult::Chosen(Action::Digit(COPY)) => self.copies(ctx),
            OverlayResult::Chosen(Action::Digit(WATCHED)) => self.mark(ctx),
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
            if !failed.is_empty() {
                ctx.toast(crate::msg::Toast::error("Some copies couldn't load their episodes"));
            }
            if let Some((e, on)) = self.pending_mark.take()
                && failed.is_empty()
            {
                shared::mark_watched(ctx, e, on);
            }
        }
        section::page_header(ui, ctx, "Watchlists", |_| {});
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
            egui::ComboBox::from_id_salt("watchlist-switcher").selected_text(format!("{selected} ▾")).show_ui(ui, |ui| self.lists(ui, ctx));
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
    }
}
