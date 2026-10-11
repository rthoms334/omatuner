//! The playlist scope bar (DESIGN §29): which playlist Movies, Series, Live TV and the guide
//! show. Drawn only with two or more eligible playlists (`Scope::multi`).
//!
//! * Tabs (Wide): `All playlists 214 380 · ● TV 151 022 · ● TV 2 98 117`, the selected tab
//!   underlined (2 px, the playlist's colour; accent for All) with fg text, the others
//!   `text_secondary`. Left click picks a tab, right click opens its menu (edit, move).
//! * Button (Medium / Compact, or when the tabs don't fit): `● TV 2 ▾`, opening the Alt+P
//!   picker.
//! * Stripe: with one playlist picked, a 2 px line in its colour across the content area
//!   ("All" has none) — the screen paints it under the bar.
//!
//! The dot is a non-text mark in the raw palette colour; names are fg / `text_secondary`, so no
//! colour has to carry text.

use eframe::egui::{self, Align2, Rect};

use super::{button, glyphs, paint};
use crate::model::SourceId;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::scope::Scope;

/// What a click on the bar asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarClick {
    None,
    /// A tab: show this playlist (`None` = All).
    Choose(Option<SourceId>),
    /// Right click on a playlist's tab: its menu.
    Menu(SourceId),
    /// The compact button: the picker.
    Picker,
}

/// Height of the bar (one control row).
pub fn height(theme: &Theme) -> f32 {
    theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Xxs)
}

struct Tab {
    choice: Option<SourceId>,
    dot: Option<egui::Color32>,
    name: String,
    count: String,
    selected: bool,
}

/// The bar across `ui`'s width. `counts[i]` = items of `scope.choices()[i]`; `hint` is drawn
/// at the right when there is room (tabs only). `tabs`: the layout has room for tabs.
pub fn scope_bar(ui: &mut egui::Ui, theme: &Theme, scope: &Scope<'_>, counts: &[usize], tabs: bool, hint: &str) -> BarClick {
    let t = &theme.tokens;
    let current = scope.selected.map(|s| s.id);
    let list: Vec<Tab> = scope
        .choices()
        .iter()
        .enumerate()
        .map(|(i, c)| Tab {
            choice: c.map(|s| s.id),
            dot: c.map(|s| theme.source_color(s.color)),
            name: c.map_or(crate::ui::scope::ALL_PLAYLISTS.to_owned(), |s| s.name.clone()),
            count: super::rows::grouped(counts.get(i).copied().unwrap_or(0)),
            selected: c.map(|s| s.id) == current,
        })
        .collect();
    let h = height(theme);
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let pad = theme.space(SpaceToken::ControlPaddingX);
    let gap = theme.space(SpaceToken::Md);
    let name_font = paint::font(theme, TypeToken::Body);
    let count_font = paint::font(theme, TypeToken::BodySmall);
    let p = ui.painter().clone();
    // measure: tab widths (dot · name · count)
    let widths: Vec<f32> = list
        .iter()
        .map(|tab| {
            let dot = if tab.dot.is_some() { paint::galley(&p, glyphs::DOT, name_font.clone(), t.fg, f32::INFINITY).size().x + gap } else { 0.0 };
            let name = paint::galley(&p, tab.name.clone(), name_font.clone(), t.fg, f32::INFINITY).size().x;
            let count = paint::galley(&p, tab.count.clone(), count_font.clone(), t.fg, f32::INFINITY).size().x;
            dot + name + gap + count + 2.0 * pad
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * list.len().saturating_sub(1) as f32;
    if !tabs || total > row.width() {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)));
        return if scope_button(&mut child, theme, scope, row.width()).clicked() { BarClick::Picker } else { BarClick::None };
    }
    let mut click = BarClick::None;
    let mut x = row.left();
    for (i, (tab, w)) in list.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_size(egui::pos2(x, row.top()), egui::vec2(*w, h));
        x += w + gap;
        let resp = ui.interact(rect, ui.id().with(("scope-tab", i)), egui::Sense::click());
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, tab.selected, &tab.name));
        if resp.clicked() {
            click = BarClick::Choose(tab.choice);
        }
        if resp.secondary_clicked()
            && let Some(id) = tab.choice
        {
            click = BarClick::Menu(id);
        }
        if resp.hovered() {
            p.rect_filled(rect, t.rounding, t.fill_hover);
        }
        let text = if tab.selected || resp.hovered() { t.fg } else { t.text_secondary };
        let y = rect.center().y;
        let mut tx = rect.left() + pad;
        if let Some(c) = tab.dot {
            tx = paint::text(&p, egui::pos2(tx, y), Align2::LEFT_CENTER, glyphs::DOT, name_font.clone(), c, f32::INFINITY).right() + gap;
        }
        tx = paint::text(&p, egui::pos2(tx, y), Align2::LEFT_CENTER, tab.name.clone(), name_font.clone(), text, f32::INFINITY).right() + gap;
        paint::text(&p, egui::pos2(tx, y), Align2::LEFT_CENTER, tab.count.clone(), count_font.clone(), t.text_secondary, f32::INFINITY);
        if tab.selected {
            let line = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 2.0), rect.right_bottom());
            p.rect_filled(line, 1.0, tab.dot.unwrap_or(t.accent));
        }
    }
    // the key, when there is room for it
    if !hint.is_empty() {
        let g = paint::galley(&p, hint, count_font.clone(), t.text_secondary, f32::INFINITY);
        if x + g.size().x + gap <= row.right() {
            p.galley(egui::pos2(row.right() - g.size().x, row.center().y - g.size().y / 2.0), g, t.text_secondary);
        }
    }
    click
}

/// `● TV 2 ▾` / `All playlists ▾`: a bordered button that opens the picker (Medium and Compact
/// layouts, the guide header). A long name elides so the button stays within `max_w`.
pub fn scope_button(ui: &mut egui::Ui, theme: &Theme, scope: &Scope<'_>, max_w: f32) -> egui::Response {
    let t = &theme.tokens;
    let font = paint::font(theme, TypeToken::Body);
    let pad = egui::vec2(theme.space(SpaceToken::ControlPaddingX), theme.space(SpaceToken::ControlPaddingY));
    let gap = theme.space(SpaceToken::ControlGap);
    let p = ui.painter().clone();
    let label = format!("{} ▾", scope.label());
    let dot = scope.selected.map(|s| paint::galley(&p, glyphs::DOT, font.clone(), theme.source_color(s.color), f32::INFINITY));
    let dot_w = dot.as_ref().map_or(0.0, |d| d.size().x + gap);
    let arrow = paint::galley(&p, " ▾", font.clone(), t.fg, f32::INFINITY);
    let name_w = (max_w - dot_w - 2.0 * pad.x - 2.0 - arrow.size().x).max(arrow.size().x);
    let name = paint::galley(&p, scope.label(), font, t.fg, name_w);
    let text_w = name.size().x + arrow.size().x;
    let size = egui::vec2(dot_w + text_w + 2.0 * pad.x + 2.0, (name.size().y + 2.0 * pad.y + 2.0).max(theme.space(SpaceToken::ControlHeight)));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if ui.is_rect_visible(rect) {
        button::paint_frame(&p, rect, theme, &resp, button::ButtonStyle { bordered: true, ..Default::default() });
        let mut x = rect.left() + pad.x + 1.0;
        let y = rect.center().y;
        if let Some(d) = dot {
            let w = d.size().x;
            p.galley(egui::pos2(x, y - d.size().y / 2.0), d, t.fg);
            x += w + gap;
        }
        let w = name.size().x;
        p.galley(egui::pos2(x, y - name.size().y / 2.0), name, t.fg);
        p.galley(egui::pos2(x + w, y - arrow.size().y / 2.0), arrow, t.fg);
    }
    resp.on_hover_text(egui::RichText::new("Choose the playlist shown (Alt+P)").size(theme.font(TypeToken::BodySmall)))
}

/// The 2 px stripe in the picked playlist's colour along `y` (nothing for All).
pub fn paint_stripe(p: &egui::Painter, theme: &Theme, x: egui::Rangef, y: f32, scope: &Scope<'_>) {
    if let Some(s) = scope.selected {
        p.rect_filled(Rect::from_x_y_ranges(x, egui::Rangef::new(y, y + 2.0)), 0.0, theme.source_color(s.color));
    }
}
