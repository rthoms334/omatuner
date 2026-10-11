//! Text input (padding 10×7, focus > hover > normal fills, placeholder darker(fg,1.6), selection
//! fg@0.35) and the launcher "header is the search field" idiom. Owner: WP-10a.

use eframe::egui::{self, Align2};

use super::paint;
use crate::theme::tokens::darker;
use crate::theme::{SpaceToken, Theme, TypeToken};

/// Blink period of the launcher caret (on for half of it).
const CARET_PERIOD_S: f64 = 1.0;

/// Standard field. `password` masks with ●. Returns the egui response (`.changed()`).
///
/// Keys reach the field only while the screen reports `KeyContext::TextInput` (then the app
/// leaves unbound keys to egui); Enter/Esc/Tab are keymap actions, never handled here.
pub fn text_field(ui: &mut egui::Ui, theme: &Theme, text: &mut String, placeholder: &str, password: bool) -> egui::Response {
    let t = &theme.tokens;
    // placeholder: Qt darker(fg, 1.6) on dark themes; on light themes that would be darker than
    // the text itself, so use fg @ 0.58 there
    let hint_color = if theme.is_light() { t.text_placeholder } else { darker(t.fg, 1.6) };
    let bg = ui.painter().add(egui::Shape::Noop);
    let resp = ui
        .scope(|ui| {
            let v = ui.visuals_mut();
            v.selection.bg_fill = t.fill_text_selection;
            v.selection.stroke.color = t.fg;
            v.text_cursor.stroke.color = t.fg;
            let edit = egui::TextEdit::singleline(text)
                .font(paint::font(theme, TypeToken::Body))
                .text_color(t.fg)
                .hint_text(egui::RichText::new(placeholder).color(hint_color))
                .password(password)
                .frame(egui::Frame::NONE)
                .margin(paint::margin(theme.space(SpaceToken::ControlPaddingX), theme.space(SpaceToken::InputPaddingY)))
                .desired_width(ui.available_width());
            ui.add(edit)
        })
        .inner;
    let hot = resp.has_focus() || resp.hovered();
    let (fill, border) = if hot { (t.fill_hover, t.border_hover) } else { (t.fill_field, t.border_field) };
    ui.painter().set(bg, egui::epaint::RectShape::new(resp.rect, t.rounding, fill, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside));
    resp
}

/// Launcher header: shows `query` at heading size, or `placeholder…` at opacity 0.58 when
/// empty, with a blinking accent caret while typing. Not an egui TextEdit — keys arrive as
/// `Action::FilterInput` through the keymap.
pub fn search_header(ui: &mut egui::Ui, theme: &Theme, query: &str, placeholder: &str, icon: Option<&str>) -> egui::Response {
    let t = &theme.tokens;
    let font = paint::font(theme, TypeToken::Heading);
    let h = (34.0 * theme.rem()).max(theme.font(TypeToken::Heading) + 2.0 * theme.space(SpaceToken::ControlPaddingY)).round();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    let shown = if query.is_empty() { placeholder_text(placeholder) } else { query.to_owned() };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, &shown));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let mut x = rect.left();
    if let Some(icon) = icon {
        let r = paint::text(
            p,
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            icon,
            paint::font(theme, TypeToken::IconLarge),
            t.text_placeholder,
            f32::INFINITY,
        );
        x = r.right() + theme.space(SpaceToken::ControlGap);
    }
    let caret_w = 2.0;
    let max_w = (rect.right() - x - caret_w - theme.space(SpaceToken::Xxs)).max(0.0);
    if query.is_empty() {
        paint::text(p, egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, shown, font, t.text_placeholder, max_w);
    } else {
        // long queries keep their end (what is being typed) visible
        let g = paint::galley(p, query, font.clone(), t.fg, f32::INFINITY);
        let g = if g.size().x > max_w {
            paint::galley(p, format!("…{}", tail_fitting(p, &g, query, &font, max_w)), font, t.fg, f32::INFINITY)
        } else {
            g
        };
        let r = Align2::LEFT_CENTER.anchor_size(egui::pos2(x, rect.center().y), g.size());
        let text_right = r.right();
        p.galley(r.min, g, t.fg);
        let time = ui.input(|i| i.time);
        if (time / CARET_PERIOD_S).fract() < 0.5 {
            let caret = egui::Rect::from_center_size(
                egui::pos2(text_right + theme.space(SpaceToken::Xxs) + caret_w / 2.0, rect.center().y),
                egui::vec2(caret_w, theme.font(TypeToken::Heading)),
            );
            p.rect_filled(caret, 0.0, t.accent);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(CARET_PERIOD_S / 2.0));
    }
    resp
}

/// "Search channels" → "Search channels…" (unless it already ends with an ellipsis).
fn placeholder_text(p: &str) -> String {
    if p.ends_with('…') || p.is_empty() { p.to_owned() } else { format!("{p}…") }
}

/// The longest suffix of `text` (laid out as `g`) that fits in `max_w` after a leading "…".
fn tail_fitting<'t>(painter: &egui::Painter, g: &egui::Galley, text: &'t str, font: &egui::FontId, max_w: f32) -> &'t str {
    let ell = paint::galley(painter, "…", font.clone(), egui::Color32::WHITE, f32::INFINITY).size().x;
    let budget = max_w - ell;
    let Some(row) = g.rows.first() else { return text };
    let total = g.size().x;
    let start = row.glyphs.iter().position(|gl| total - gl.pos.x <= budget).unwrap_or(row.glyphs.len());
    text.char_indices().nth(start).map_or("", |(b, _)| &text[b..])
}

#[cfg(test)]
mod tests {
    use super::placeholder_text;

    #[test]
    fn placeholders_get_one_ellipsis() {
        assert_eq!(placeholder_text("Search channels"), "Search channels…");
        assert_eq!(placeholder_text("Menu…"), "Menu…");
        assert_eq!(placeholder_text(""), "");
    }
}
