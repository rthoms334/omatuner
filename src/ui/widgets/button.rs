//! Buttons (Omarchy Button.qml): borderless at rest, padding 10×6, icon/text gap 8, selected =
//! accent + bold; reserves border width so hover never shifts layout. Owner: WP-10a.

use eframe::egui;

use super::paint;
use crate::theme::{SpaceToken, Theme, TypeToken};

#[derive(Debug, Clone, Copy, Default)]
pub struct ButtonStyle {
    pub bordered: bool,
    pub selected: bool,
    /// Destructive: text in `urgent`.
    pub danger: bool,
    /// Primary call-to-action: accent text, bordered.
    pub primary: bool,
    /// Keyboard focus ring.
    pub focused: bool,
}

/// Text colour for a button style (disabled > danger > selected/primary > fg).
fn text_color(theme: &Theme, style: ButtonStyle, enabled: bool) -> egui::Color32 {
    let t = &theme.tokens;
    if !enabled {
        t.text_disabled
    } else if style.danger {
        t.urgent
    } else if style.selected || style.primary {
        t.accent
    } else {
        t.fg
    }
}

/// Fill and inset border (width, colour) of a button in a state. Priority (Omarchy): pressed >
/// focus > hover > selected > idle. A primary button keeps its accent border when focused or
/// hovered and gains the hover fill and a 2 px border, so focus never looks weaker than rest.
fn frame_style(theme: &Theme, style: ButtonStyle, enabled: bool, pressed: bool, hot: bool) -> (egui::Color32, Option<(f32, egui::Color32)>) {
    let t = &theme.tokens;
    let fill = if pressed {
        t.fill_pressed
    } else if hot {
        t.fill_hover
    } else if style.selected {
        t.fill_selected
    } else {
        egui::Color32::TRANSPARENT
    };
    let border = if style.primary && enabled {
        Some((if hot || pressed { 2.0 } else { 1.0 }, t.accent))
    } else if hot {
        Some((1.0, t.border_hover))
    } else if style.bordered || style.primary {
        Some((1.0, t.border_field))
    } else {
        None
    };
    (fill, border)
}

/// Paint the state fill + border of a button-like rect; the border is always inset, so it
/// never shifts the layout.
pub(super) fn paint_frame(painter: &egui::Painter, rect: egui::Rect, theme: &Theme, resp: &egui::Response, style: ButtonStyle) {
    let enabled = resp.enabled();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let hot = enabled && (style.focused || resp.hovered() || resp.has_focus());
    let (fill, border) = frame_style(theme, style, enabled, pressed, hot);
    painter.rect_filled(rect, theme.tokens.rounding, fill);
    if let Some((w, c)) = border {
        painter.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(w, c), egui::StrokeKind::Inside);
    }
}

pub fn button(ui: &mut egui::Ui, theme: &Theme, icon: Option<&str>, label: &str, style: ButtonStyle) -> egui::Response {
    let enabled = ui.is_enabled();
    let color = text_color(theme, style, enabled);
    let text_font = if style.selected { paint::bold(ui.ctx(), theme, TypeToken::Body) } else { paint::font(theme, TypeToken::Body) };
    let pad = egui::vec2(theme.space(SpaceToken::ControlPaddingX), theme.space(SpaceToken::ControlPaddingY));
    let gap = theme.space(SpaceToken::ControlGap);
    let painter = ui.painter().clone();
    let icon_g = icon.map(|i| paint::galley(&painter, i, paint::font(theme, TypeToken::Icon), color, f32::INFINITY));
    let label_g = (!label.is_empty()).then(|| paint::galley(&painter, label, text_font, color, f32::INFINITY));
    let icon_w = icon_g.as_ref().map_or(0.0, |g| g.size().x);
    let label_w = label_g.as_ref().map_or(0.0, |g| g.size().x);
    let content_w = icon_w + label_w + if icon_g.is_some() && label_g.is_some() { gap } else { 0.0 };
    let content_h = [icon_g.as_ref(), label_g.as_ref()].into_iter().flatten().map(|g| g.size().y).fold(0.0, f32::max);
    // + 2 px: the border width is always reserved (hover never shifts the layout)
    let size = egui::vec2(content_w + 2.0 * pad.x + 2.0, (content_h + 2.0 * pad.y + 2.0).max(theme.space(SpaceToken::ControlHeight)));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        paint_frame(&painter, rect, theme, &resp, style);
        let mut x = rect.center().x - content_w / 2.0;
        if let Some(g) = icon_g {
            painter.galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, color);
            x += icon_w + gap;
        }
        if let Some(g) = label_g {
            painter.galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, color);
        }
    }
    resp
}

/// Icon-only button with tooltip (400 ms delay, body-small).
pub fn icon_button(ui: &mut egui::Ui, theme: &Theme, icon: &str, tooltip: &str, style: ButtonStyle) -> egui::Response {
    let enabled = ui.is_enabled();
    let color = text_color(theme, style, enabled);
    let side = theme.space(SpaceToken::ControlHeight);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tooltip));
    if ui.is_rect_visible(rect) {
        paint_frame(ui.painter(), rect, theme, &resp, style);
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, paint::font(theme, TypeToken::Icon), color);
    }
    if tooltip.is_empty() { resp } else { resp.on_hover_text(egui::RichText::new(tooltip).size(theme.font(TypeToken::BodySmall))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A focused primary button must read at least as strongly as an idle one (it used to
    /// drop the accent border): the accent border is kept, and wider.
    #[test]
    fn focused_primary_is_stronger_than_idle() {
        let th = Theme::fallback();
        let primary = ButtonStyle { primary: true, ..Default::default() };
        let (_, idle_border) = frame_style(&th, primary, true, false, false);
        let (_, focus_border) = frame_style(&th, ButtonStyle { focused: true, ..primary }, true, false, true);
        let (w, c) = focus_border.expect("focused primary keeps a border");
        assert_eq!(c, th.tokens.accent, "focus keeps the accent border");
        assert!(w > idle_border.map_or(0.0, |b| b.0), "focus border is wider");
    }
}
