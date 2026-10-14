//! The Watchlists… picker and one-time favourites move card (DESIGN §42 §2.3, §4.1).
use super::dialogs::{card_area, scrim_guarded};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, WatchlistItemId};
use crate::store::watchlist as store;
use crate::ui::screens::home::kit;
use crate::ui::watchlists as shared;
use crate::ui::widgets::{
    button::{ButtonStyle, button},
    card::CardKind,
    glyphs,
};
use crate::ui::{Route, UiCtx};
use crate::watchlist::{Insert, PlayOrder};
use eframe::egui;

#[derive(Debug, Clone, PartialEq)]
pub struct PickerSpec {
    pub items: Vec<ItemRef>,
    pub title: String,
    /// Seasons… edits this entry instead of adding another range.
    pub edit: Option<WatchlistItemId>,
    /// New list… / Save as a watchlist… begins with the name field.
    pub create: bool,
}

pub struct Picker {
    spec: PickerSpec,
    cursor: usize,
    name: Option<String>,
    whole: bool,
    from: u32,
    to: u32,
    place: usize,
    position: usize,
    submit: bool,
    last_used: Option<String>,
    loaded_last: bool,
    loaded_edit: bool,
    focus: usize,
}
impl Picker {
    pub fn new(spec: PickerSpec) -> Self {
        let name = spec.create.then(String::new);
        Self {
            spec,
            cursor: 0,
            name,
            whole: true,
            from: 1,
            to: 1,
            place: 0,
            position: 1,
            submit: false,
            last_used: None,
            loaded_last: false,
            loaded_edit: false,
            focus: 0,
        }
    }
    fn init(&mut self, ctx: &UiCtx<'_>) {
        if !self.loaded_edit {
            self.loaded_edit = true;
            if let Some(e) = self.spec.edit.and_then(|id| store::entry(ctx.db, id).ok().flatten()) {
                self.whole = e.season_from.is_none();
                self.from = e.season_from.unwrap_or(1);
                self.to = e.season_to.unwrap_or(self.from);
            }
        }
    }
    fn fields(&self) -> Vec<usize> {
        let mut f = vec![0];
        if self.series() || self.spec.edit.is_some() {
            f.push(1);
            if !self.whole {
                f.extend([2, 3]);
            }
        }
        if self.spec.edit.is_none() {
            f.push(4);
            if self.place == 2 {
                f.push(5);
            }
        }
        f
    }
    fn change_field(&mut self, forward: bool) {
        match self.focus {
            1 => self.whole = !self.whole,
            2 => {
                self.from = if forward { self.from.saturating_add(1) } else { self.from.saturating_sub(1) };
                self.to = self.to.max(self.from);
            }
            3 => self.to = if forward { self.to.saturating_add(1) } else { self.to.saturating_sub(1).max(self.from) },
            4 => self.place = if forward { (self.place + 1) % 3 } else { (self.place + 2) % 3 },
            5 => self.position = if forward { (self.position + 1).min(300) } else { self.position.saturating_sub(1).max(1) },
            _ => {}
        }
    }
    fn series(&self) -> bool {
        self.spec.items.len() == 1 && self.spec.items[0].kind == ItemKind::Series
    }
    fn choose(&mut self, ctx: &mut UiCtx<'_>) {
        let snap = shared::snapshot(ctx);
        if self.cursor >= snap.lists.len() {
            self.name = Some(String::new());
            return;
        }
        let list = &snap.lists[self.cursor];
        let list_id = list.list.id;
        let name = list.list.name.clone();
        let refs = self.spec.items.clone();
        let now = ctx.now;
        let lib = ctx.lib.clone();
        let range = (!self.whole && self.series()).then_some((self.from, self.to.max(self.from)));
        let (a, b) = range.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        if refs.len() == 1 {
            let ids = store::matching_entries(ctx.db, ctx.lib, list_id, &refs[0], a, b).unwrap_or_default();
            if !ids.is_empty() {
                shared::write(ctx, format!("Removed from {name}"), move |c| store::remove(c, list_id, &ids).map(Some));
                return;
            }
        }
        let at = self.insertion();
        shared::write(ctx, format!("Saved to {name}"), move |c| {
            crate::store::sql::atomic(c, |c| {
                let mut entries = refs.iter().map(|r| store::new_entry(c, r)).collect::<crate::error::Result<Vec<_>>>()?;
                if let Some((a, b)) = range {
                    entries[0].season_from = Some(a);
                    entries[0].season_to = Some(b);
                }
                let added = store::add(c, &lib, list_id, &entries, at, now)?;
                if let Some((index, reason)) = added.refused.first() {
                    if reason == "duplicate" {
                        let ids = store::matching_entries(c, &lib, list_id, &refs[*index], a, b)?;
                        let pos = ids.first().and_then(|id| store::entry(c, *id).ok().flatten()).map_or(1, |e| e.position + 1);
                        return Err(crate::error::Error::other(format!("Already in {name} ({})", ordinal(pos))));
                    }
                    return Err(crate::error::Error::other(if reason == "overlapping_seasons" {
                        "Those seasons are already in this list".to_owned()
                    } else {
                        reason.clone()
                    }));
                }
                c.execute("INSERT OR REPLACE INTO meta(key,value) VALUES('watchlist:last_used',?1)", [list_id.0.to_string()])?;
                Ok(None)
            })
        });
    }
    fn insertion(&self) -> Insert {
        match self.place {
            1 => Insert::Next,
            2 => Insert::Index(self.position.saturating_sub(1)),
            _ => Insert::End,
        }
    }
    fn create(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let Some(name) = self.name.as_ref().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) else { return OverlayOutcome::Keep };
        let refs = self.spec.items.clone();
        let lib = ctx.lib.clone();
        let now = ctx.now;
        let range = (!self.whole && self.series()).then_some((self.from, self.to.max(self.from)));
        shared::write(ctx, format!("Created {name}"), move |c| {
            crate::store::sql::atomic(c, |c| {
                let list = store::create(c, &name, None, PlayOrder::Ordered, "app", now)?;
                let mut entries = refs.iter().map(|r| store::new_entry(c, r)).collect::<crate::error::Result<Vec<_>>>()?;
                if let Some((a, b)) = range {
                    entries[0].season_from = Some(a);
                    entries[0].season_to = Some(b);
                }
                for chunk in entries.chunks(50) {
                    let result = store::add(c, &lib, list, chunk, Insert::End, now)?;
                    if let Some((_, why)) = result.refused.iter().find(|(_, why)| why != "duplicate") {
                        return Err(crate::error::Error::other(match why.as_str() {
                            "overlapping_seasons" => "Those seasons are already in this list",
                            "full" => "A watchlist can hold up to 300 entries",
                            "invalid_seasons" => "Choose a valid season range",
                            other => other,
                        }));
                    }
                }
                Ok(None)
            })
        });
        OverlayOutcome::Close
    }
    fn edit(&self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let Some(id) = self.spec.edit else { return OverlayOutcome::Keep };
        let lib = ctx.lib.clone();
        let (a, b) = if self.whole { (None, None) } else { (Some(self.from), Some(self.to.max(self.from))) };
        shared::write(ctx, "Seasons updated", move |c| {
            store::set_seasons(c, &lib, id, a, b)?;
            Ok(None)
        });
        OverlayOutcome::Close
    }
}
impl Overlay for Picker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.name.is_some() && self.focus == 0 { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }
    fn is_dirty(&self) -> bool {
        self.name.as_ref().is_some_and(|n| !n.is_empty())
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, a: Action) -> OverlayOutcome {
        self.init(ctx);
        match a {
            Action::Back => return OverlayOutcome::Close,
            Action::FocusNext | Action::FocusPrev => {
                let fields = self.fields();
                let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
                self.focus = fields[if a == Action::FocusNext { (at + 1) % fields.len() } else { (at + fields.len() - 1) % fields.len() }];
            }
            Action::Left | Action::Right | Action::Up | Action::Down | Action::Activate if self.focus != 0 => {
                self.change_field(matches!(a, Action::Right | Action::Down | Action::Activate))
            }
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(shared::snapshot(ctx).lists.len()),
            Action::Activate if self.name.is_some() => self.submit = true,
            Action::Activate if self.spec.edit.is_some() => return self.edit(ctx),
            Action::Activate => self.choose(ctx),
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.init(ctx);
        if scrim_guarded(egui_ctx, ctx.theme, "watchlist-picker", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        let snap = shared::snapshot(ctx);
        let members = self.spec.items.first().map(|r| shared::memberships(ctx, r)).unwrap_or_default();
        if !self.loaded_last {
            self.loaded_last = true;
            self.last_used = ctx.db.query_row("SELECT value FROM meta WHERE key='watchlist:last_used'", [], |r| r.get::<_, String>(0)).ok();
        }
        let last = self.last_used.clone();
        let mut choose = None;
        let mut close = false;
        let mut save = false;
        let theme = ctx.theme;
        card_area(egui_ctx, theme, "watchlist-picker", CardKind::Popup, 500.0 * theme.rem(), None, |ui| {
            ui.heading(if self.spec.edit.is_some() { "Seasons" } else { "Add to a watchlist" });
            if !self.spec.title.is_empty() {
                ui.label(&self.spec.title);
            }
            if self.spec.edit.is_none() {
                if let Some(name) = &mut self.name {
                    ui.label("List name");
                    let field = ui.text_edit_singleline(name);
                    if self.focus == 0 {
                        field.request_focus();
                    } else {
                        field.surrender_focus();
                    }
                } else {
                    egui::ScrollArea::vertical().max_height(320.0 * theme.rem()).show(ui, |ui| {
                        for (i, l) in snap.lists.iter().enumerate() {
                            let tick = members.iter().any(|(m, _, _)| m.id == l.list.id);
                            let label = format!(
                                "{}{}{}",
                                l.list.name,
                                if tick { "  ✓" } else { "" },
                                if last.as_deref() == Some(&l.list.id.0.to_string()) { "  (last used)" } else { "" }
                            );
                            let r = ui.selectable_label(i == self.cursor, label);
                            if crate::ui::widgets::fresh_click(&r) {
                                choose = Some(i);
                            }
                        }
                        if ui.selectable_label(self.cursor == snap.lists.len(), "New list…").clicked() {
                            self.name = Some(String::new());
                        }
                    });
                }
            }
            ui.separator();
            if self.series() || self.spec.edit.is_some() {
                ui.horizontal(|ui| {
                    ui.label(if self.focus == 1 { "▸ Series:" } else { "Series:" });
                    ui.radio_value(&mut self.whole, true, "Whole series");
                    ui.radio_value(&mut self.whole, false, "Seasons");
                });
                if !self.whole {
                    ui.horizontal(|ui| {
                        ui.label(if self.focus == 2 { "▸ From" } else { "From" });
                        ui.add(egui::DragValue::new(&mut self.from).range(0..=999));
                        ui.label(if self.focus == 3 { "▸ to" } else { "to" });
                        ui.add(egui::DragValue::new(&mut self.to).range(self.from..=999));
                    });
                }
            }
            if self.spec.edit.is_none() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(if self.focus == 4 { "▸ Place:" } else { "Place:" });
                    ui.radio_value(&mut self.place, 0, "At the end");
                    ui.radio_value(&mut self.place, 1, "After the next");
                    ui.radio_value(&mut self.place, 2, "Choose position…");
                });
                if self.place == 2 {
                    ui.add(egui::DragValue::new(&mut self.position).range(1..=300));
                }
            }
            ui.horizontal(|ui| {
                if self.name.is_some() || self.spec.edit.is_some() {
                    save = button(ui, theme, Some(glyphs::CHECK), "Save", ButtonStyle::default()).clicked();
                }
                close = button(ui, theme, None, "Done", ButtonStyle::default()).clicked();
            });
        });
        if let Some(i) = choose {
            self.cursor = i;
            self.choose(ctx);
        }
        if std::mem::take(&mut self.submit) || save {
            return if self.spec.edit.is_some() { self.edit(ctx) } else { self.create(ctx) };
        }
        if close { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}

pub struct Moved {
    names: Vec<String>,
}
impl Moved {
    pub fn new(names: Vec<String>) -> Self {
        Self { names }
    }
}
impl Overlay for Moved {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, a: Action) -> OverlayOutcome {
        if matches!(a, Action::Back | Action::Activate) { OverlayOutcome::Close } else { OverlayOutcome::Ignored }
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let mut open = false;
        let mut done = false;
        card_area(egui_ctx, ctx.theme, "watchlists-moved", CardKind::Popup, 540.0 * ctx.theme.rem(), None, |ui| {
            ui.heading("Your saved films and series have moved");
            let visible = shared::snapshot(ctx);
            let names: Vec<_> = self.names.iter().filter(|name| visible.lists.iter().any(|l| &l.list.name == *name)).cloned().collect();
            ui.label(if names.is_empty() {
                "Films and series you'd saved are now in Watchlists. Favourites are for channels.".to_owned()
            } else {
                format!("Films and series you'd saved are now in Watchlists: {}. Favourites are for channels.", names.join(", "))
            });
            ui.horizontal(|ui| {
                open = button(ui, ctx.theme, Some(glyphs::FAV_LIST), "Open Watchlists", ButtonStyle::default()).clicked();
                done = button(ui, ctx.theme, None, "OK", ButtonStyle::default()).clicked();
            });
        });
        if open {
            ctx.push(Route::Watchlists { list: None });
        }
        if open || done { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}

/// A missing row can be attached by the viewer without changing its place or season range.
pub struct Find {
    entry: WatchlistItemId,
    kind: ItemKind,
    query: String,
    loaded: String,
    rows: Vec<(ItemRef, String)>,
    pending: Option<kit::Loader<Vec<(ItemRef, String)>>>,
    cursor: usize,
    results_focus: bool,
}
impl Find {
    pub fn new(entry: WatchlistItemId, kind: ItemKind, title: String) -> Self {
        Self { entry, kind, query: title, loaded: String::new(), rows: vec![], pending: None, cursor: 0, results_focus: false }
    }
    fn attach(&self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if let Some((item, _)) = self.rows.get(self.cursor) {
            let item = item.clone();
            let id = self.entry;
            shared::write(ctx, "Found in your playlists", move |c| {
                store::attach(c, id, &item)?;
                Ok(None)
            });
            OverlayOutcome::Close
        } else {
            OverlayOutcome::Keep
        }
    }
}
impl Overlay for Find {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.results_focus { vec![KeyContext::Modal] } else { vec![KeyContext::TextInput, KeyContext::Modal] }
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, a: Action) -> OverlayOutcome {
        match a {
            Action::Back => OverlayOutcome::Close,
            Action::FocusNext | Action::FocusPrev => {
                self.results_focus = !self.results_focus;
                OverlayOutcome::Keep
            }
            Action::Activate => self.attach(ctx),
            Action::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                OverlayOutcome::Keep
            }
            Action::Down => {
                self.cursor = (self.cursor + 1).min(self.rows.len().saturating_sub(1));
                OverlayOutcome::Keep
            }
            _ => OverlayOutcome::Ignored,
        }
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if scrim_guarded(egui_ctx, ctx.theme, "watchlist-find", false) {
            return OverlayOutcome::Close;
        }
        if self.query != self.loaded {
            self.loaded = self.query.clone();
            let query = self.query.clone();
            let lib =
                if ctx.parental.hides_explicit(&ctx.config.parental) { std::sync::Arc::new(ctx.lib.hiding_explicit()) } else { ctx.lib.clone() };
            let kind = self.kind;
            self.pending = Some(kit::Loader::start(ctx, move |c| {
                let (table, name) = match kind {
                    ItemKind::Movie => ("movie", "name"),
                    ItemKind::Series => ("series", "name"),
                    ItemKind::Episode => ("episode", "title"),
                    _ => return Ok(vec![]),
                };
                let sql = if kind == ItemKind::Episode {
                    "SELECT e.source_id,e.key,s.name || ' · S' || e.season || 'E' || e.episode FROM episode e JOIN series s ON s.id=e.series_id WHERE e.title LIKE ?1 LIMIT 100".to_owned()
                } else {
                    format!("SELECT source_id,key,{name} FROM {table} WHERE {name} LIKE ?1 LIMIT 100")
                };
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map([format!("%{}%", query.trim())], |r| {
                    Ok((ItemRef { source: r.get(0)?, kind, key: crate::model::ItemKey(r.get(1)?) }, r.get::<_, String>(2)?))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter(|(r, _)| !lib.conceals(r)).collect())
            }));
        }
        if let Some(Ok(rows)) = kit::poll(&mut self.pending) {
            self.rows = rows;
            self.cursor = 0;
        }
        let mut picked = None;
        card_area(egui_ctx, ctx.theme, "watchlist-find", CardKind::Popup, 540.0 * ctx.theme.rem(), None, |ui| {
            ui.heading("Find in my playlists");
            let input = ui.text_edit_singleline(&mut self.query);
            if self.results_focus {
                input.surrender_focus();
            } else {
                input.request_focus();
            }
            egui::ScrollArea::vertical().max_height(350.0).show(ui, |ui| {
                for (i, (item, name)) in self.rows.iter().enumerate() {
                    let source = ctx.lib.source(item.source).map(|s| s.name.as_str()).unwrap_or("");
                    let r = ui.selectable_label(self.cursor == i, format!("{name} · {source}"));
                    if crate::ui::widgets::fresh_click(&r) {
                        picked = Some(i);
                    }
                }
            });
        });
        if let Some(i) = picked {
            self.cursor = i;
            return self.attach(ctx);
        }
        OverlayOutcome::Keep
    }
}

fn ordinal(n: usize) -> String {
    let suffix = if (11..=13).contains(&(n % 100)) {
        "th"
    } else {
        match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::home::testkit::{self, has_label, overlay_harness, rig, seed};
    #[test]
    fn series_picker_offers_ranges_and_keyboard_places() {
        let (mut t, one) = rig();
        let item = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            let id = testkit::series(c, src, 1, 3, 2)?[0];
            let s = crate::store::vod::series(c, id)?;
            Ok(ItemRef { source: src, kind: ItemKind::Series, key: s.key })
        });
        t.with_ctx(|ctx| crate::ui::screens::home::kit::list_picker(ctx, &item, "Series 0"));
        let crate::ui::NavRequest::Open(crate::ui::OverlayRequest::WatchlistPicker(spec)) = t.nav.pop().unwrap() else {
            panic!("series opens Watchlists")
        };
        let mut h = overlay_harness(t, one, Picker::new(spec), egui::vec2(1000.0, 800.0));
        assert!(has_label(&h, "Whole series") && has_label(&h, "Seasons"));
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Right);
        assert!(!h.state().o.whole, "Tab reaches the range control");
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Right);
        assert_eq!(h.state().o.from, 2);
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Right);
        assert_eq!(h.state().o.to, 3);
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Right);
        assert_eq!(h.state().o.place, 1, "After the next is keyboard-accessible");
    }
}
