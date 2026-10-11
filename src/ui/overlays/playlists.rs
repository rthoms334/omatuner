//! Choosing the playlist a screen shows (DESIGN §29): the Alt+P picker and a scope tab's
//! right-click menu. Both act themselves (config edit, store write, navigation) and close: the
//! screen below reads the result from the config next frame.
//!
//! Picker: a launcher card — "All playlists" then each eligible playlist with its colour and
//! count; ↑/↓ + Enter, a click, or a digit (0 = All, 1–9 = the playlists in order, while the
//! filter is empty); typing filters by name.

use eframe::egui::{self, Align2, RichText};

use super::dialogs::{ContextMenuOverlay, card_area, scrim, with_close};
use super::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::keymap::{Action, KeyContext};
use crate::model::SourceId;
use crate::msg::{DataScope, Toast};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::scope::{self, ALL_PLAYLISTS, Scope, ScopeKind};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state};
use crate::ui::widgets::rows::grouped;
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, glyphs};
use crate::ui::{Route, UiCtx};

/// Alt+P: which playlist a screen of `kind` shows.
pub struct PlaylistPickerOverlay {
    kind: ScopeKind,
    list: ListState,
    /// Shown choices (indices into `Scope::choices`) for the current filter.
    rows: Vec<usize>,
    /// Choice labels at open (the filter haystack).
    names: Vec<String>,
    started: bool,
}

impl PlaylistPickerOverlay {
    pub fn new(kind: ScopeKind) -> Self {
        Self { kind, list: ListState::default(), rows: Vec::new(), names: Vec::new(), started: false }
    }

    /// First use: the choices of this frame's library, the cursor on the current one.
    fn start(&mut self, ctx: &UiCtx<'_>) {
        if self.started {
            return;
        }
        self.started = true;
        let scope = Scope::current(ctx, self.kind);
        let choices = scope.choices();
        self.names = choices.iter().map(|c| c.map_or(ALL_PLAYLISTS.to_owned(), |s| s.name.clone())).collect();
        self.refilter();
        let at = choices.iter().position(|c| c.map(|s| s.id) == scope.selected.map(|s| s.id)).unwrap_or(0);
        self.list.select(at);
    }

    fn refilter(&mut self) {
        self.rows = self.list.filter.apply(self.names.iter().map(String::as_str)).into_iter().map(|i| i as usize).collect();
        self.list.set_len(self.rows.len());
    }

    /// Choice `i` (index into `Scope::choices`).
    fn choose(&self, ctx: &mut UiCtx<'_>, i: usize) -> OverlayOutcome {
        let scope = Scope::current(ctx, self.kind);
        let Some(choice) = scope.choices().get(i).copied() else { return OverlayOutcome::Keep };
        scope::choose(ctx, self.kind, choice.map(|s| s.id), false);
        OverlayOutcome::Close
    }
}

impl Overlay for PlaylistPickerOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.key_context(), KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.start(ctx);
        // digits jump while nothing is typed: 0 = All, 1–9 = the playlists
        if let Action::FilterInput(c) = action
            && !self.list.filter.is_active()
            && let Some(d) = c.to_digit(10)
        {
            return if (d as usize) < self.names.len() { self.choose(ctx, d as usize) } else { OverlayOutcome::Keep };
        }
        match self.list.handle(action) {
            ListOutcome::Moved | ListOutcome::ContextMenu(_) => OverlayOutcome::Keep,
            ListOutcome::FilterChanged => {
                self.refilter();
                OverlayOutcome::Keep
            }
            ListOutcome::Activated(r) | ListOutcome::ActivatedAlt(r) => match self.rows.get(r) {
                Some(&i) => self.choose(ctx, i),
                None => OverlayOutcome::Keep,
            },
            ListOutcome::Parent => OverlayOutcome::Close,
            ListOutcome::None => match action {
                Action::Back | Action::PlaylistPicker => OverlayOutcome::Close,
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => {
                    OverlayOutcome::Keep
                }
                Action::Activate | Action::ActivateAlt | Action::Right => OverlayOutcome::Keep,
                _ => OverlayOutcome::Ignored,
            },
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.start(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "playlist-picker") {
            return OverlayOutcome::Close;
        }
        let scope = Scope::current(ctx, self.kind);
        let choices = scope.choices();
        let counts: Vec<usize> = choices.iter().map(|c| scope.count(ctx.lib, c.map(|s| s.id))).collect();
        let current = scope.selected.map(|s| s.id);
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg);
        let mut chosen = None;
        let mut close = false;
        card_area(egui_ctx, theme, "playlist-picker", CardKind::Menu, 380.0 * theme.rem(), None, |ui| {
            close = with_close(ui, theme, |ui| {
                search_header(ui, theme, self.list.filter.query(), "Show which playlist? — 0–9 jump", None);
            });
            ui.add_space(theme.space(SpaceToken::Md));
            if self.rows.is_empty() {
                let q = self.list.filter.query();
                ui.label(RichText::new(format!("No matches for “{q}”")).size(theme.font(TypeToken::Title)).color(t.text_secondary));
                return;
            }
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xs);
            for (row, &i) in self.rows.iter().enumerate() {
                let Some(choice) = choices.get(i).copied() else { continue };
                let name = self.names.get(i).map_or("", String::as_str);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, choice.map(|s| s.id) == current, name));
                let cursor = row == self.list.cursor;
                if pointer_moved && resp.hovered() && !cursor {
                    self.list.select(row);
                }
                if resp.clicked() {
                    chosen = Some(i);
                }
                let p = ui.painter();
                paint_row_state(p, rect, theme, if cursor { RowState::Cursor } else { RowState::Idle });
                let inset = theme.space(SpaceToken::Lg);
                let icon_col = (28.0 * theme.rem()).round();
                let text = if cursor { t.list_cursor_text } else { t.fg };
                let (glyph, color) = match choice {
                    Some(s) => (glyphs::DOT, theme.source_color(s.color)),
                    None => (glyphs::PLAYLIST, text),
                };
                let y = rect.center().y;
                p.text(
                    egui::pos2(rect.left() + inset + icon_col / 2.0, y),
                    Align2::CENTER_CENTER,
                    glyph,
                    egui::FontId::proportional(theme.font(TypeToken::Icon)),
                    color,
                );
                // right: digit key, then the count
                let small = egui::FontId::proportional(theme.font(TypeToken::BodySmall));
                let mut right = rect.right() - inset;
                // the jump digit, only while digits jump (typing filters otherwise)
                if i < 10 && !self.list.filter.is_active() {
                    right = p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, i.to_string(), small.clone(), t.text_secondary).left()
                        - theme.space(SpaceToken::Lg);
                }
                right =
                    p.text(egui::pos2(right, y), Align2::RIGHT_CENTER, grouped(counts.get(i).copied().unwrap_or(0)), small, t.text_secondary).left()
                        - theme.space(SpaceToken::Lg);
                if choice.map(|s| s.id) == current {
                    right = p
                        .text(
                            egui::pos2(right, y),
                            Align2::RIGHT_CENTER,
                            glyphs::CHECK,
                            egui::FontId::proportional(theme.font(TypeToken::Icon)),
                            t.accent,
                        )
                        .left()
                        - theme.space(SpaceToken::Md);
                }
                let left = rect.left() + inset + icon_col + theme.space(SpaceToken::Md);
                let mut job =
                    egui::text::LayoutJob::simple_singleline(name.to_owned(), egui::FontId::proportional(theme.font(TypeToken::Title)), text);
                job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(0.0));
                let g = p.layout_job(job);
                p.galley(egui::pos2(left, y - g.size().y / 2.0), g, text);
            }
        });
        if close {
            return OverlayOutcome::Close;
        }
        chosen.map_or(OverlayOutcome::Keep, |i| self.choose(ctx, i))
    }
}

/// The playlist order with `source` moved past its neighbouring tab of `kind` (up: just
/// before the previous eligible playlist; down: just after the next one); `None` at the end.
fn moved_order(lib: &crate::store::library::Library, kind: ScopeKind, source: SourceId, up: bool) -> Option<Vec<SourceId>> {
    let tabs: Vec<SourceId> = lib.eligible(kind.item_kind()).iter().map(|s| s.id).collect();
    let at = tabs.iter().position(|s| *s == source)?;
    let neighbour = if up { tabs.get(at.checked_sub(1)?) } else { tabs.get(at + 1) }.copied()?;
    let mut order: Vec<SourceId> = lib.sources.iter().map(|s| s.id).filter(|s| *s != source).collect();
    let j = order.iter().position(|s| *s == neighbour)?;
    order.insert(if up { j } else { j + 1 }, source);
    Some(order)
}

const MENU_EDIT: u8 = 0;
const MENU_UP: u8 = 1;
const MENU_DOWN: u8 = 2;

/// A scope tab's right-click menu: "Edit playlist…", "Move up", "Move down" (the playlist
/// order of Settings ▸ Playlists, which is also the tab order). A move passes the neighbouring
/// *tab* of the screen (`kind`), not a playlist without a tab there.
pub struct PlaylistMenuOverlay {
    source: SourceId,
    kind: ScopeKind,
    menu: ContextMenuOverlay,
}

impl PlaylistMenuOverlay {
    pub fn new(source: SourceId, kind: ScopeKind, ctx: &UiCtx<'_>) -> Self {
        let name = ctx.lib.source(source).map_or_else(|| "Playlist".to_owned(), |s| s.name.clone());
        let item = |icon: &'static str, label: &str, code: u8| MenuItem {
            icon,
            label: label.into(),
            action: Action::Digit(code),
            enabled: true,
            shortcut: None,
        };
        let spec = ContextMenuSpec {
            title: name,
            items: vec![
                item(glyphs::SETTINGS, "Edit playlist…", MENU_EDIT),
                item(glyphs::REORDER, "Move up", MENU_UP),
                item(glyphs::REORDER, "Move down", MENU_DOWN),
            ],
        };
        Self { source, kind, menu: ContextMenuOverlay::new(spec) }
    }

    fn act(&self, ctx: &mut UiCtx<'_>, outcome: OverlayOutcome) -> OverlayOutcome {
        let OverlayOutcome::CloseWith(OverlayResult::Chosen(Action::Digit(code))) = outcome else { return outcome };
        match code {
            MENU_EDIT => ctx.push(Route::EditPlaylist(self.source)),
            _ => match moved_order(ctx.lib, self.kind, self.source, code == MENU_UP) {
                Some(order) => {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::reorder_sources(c, &order));
                }
                None => ctx.toast(Toast::info(if code == MENU_UP { "Already the first playlist" } else { "Already the last playlist" })),
            },
        }
        OverlayOutcome::Close
    }
}

impl Overlay for PlaylistMenuOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        self.menu.key_contexts()
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let out = self.menu.on_action(ctx, action);
        self.act(ctx, out)
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let out = self.menu.ui(egui_ctx, ctx);
        self.act(ctx, out)
    }
    /// A tab's right-click: the menu opens at the pointer like every other right-click menu.
    fn anchor_at(&mut self, pos: egui::Pos2) {
        self.menu.anchor_at(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceColor, SourceKind};
    use crate::store::library::{Library, SourceMeta};

    /// "Move up / down" on a tab passes the neighbouring *tab*: with TV, iptv-org (channels
    /// only), TV 2, moving TV 2 up on Movies puts it before TV — not merely before iptv-org,
    /// which has no Movies tab and would leave the tabs unchanged.
    #[test]
    fn a_tab_moves_past_its_neighbouring_tab() {
        let meta = |id: i64, vod: bool| SourceMeta {
            id: SourceId(id),
            name: format!("P{id}"),
            icon: String::new(),
            color: SourceColor::Accent,
            kind: SourceKind::Xtream,
            enabled: true,
            last_refresh: None,
            has_vod: vod,
            has_series: false,
            has_live: true,
            in_all: true,
            category_filter: None,
        };
        let lib = Library { sources: vec![meta(1, true), meta(2, false), meta(3, true)], ..Default::default() };
        let ids = |v: Option<Vec<SourceId>>| v.map(|v| v.into_iter().map(|s| s.0).collect::<Vec<_>>());
        assert_eq!(ids(moved_order(&lib, ScopeKind::Movies, SourceId(3), true)), Some(vec![3, 1, 2]));
        assert_eq!(ids(moved_order(&lib, ScopeKind::Movies, SourceId(1), false)), Some(vec![2, 3, 1]));
        assert_eq!(ids(moved_order(&lib, ScopeKind::Movies, SourceId(1), true)), None, "already the first tab");
        // Live TV has three tabs: the plain neighbour
        assert_eq!(ids(moved_order(&lib, ScopeKind::Live, SourceId(3), true)), Some(vec![1, 3, 2]));
    }
}
