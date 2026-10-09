//! Empty / error / loading states (Clipboard.qml): glyph at display-large in accent @0.8,
//! message at title in fg @0.7, optional detail and action button. Implemented (basic) in the
//! skeleton; WP-10a owns polish.

use eframe::egui::{self, RichText};

use crate::theme::{Theme, TypeToken, with_alpha};

/// Centred in the available rect. Returns the action button response if `action` was given.
pub fn empty_state(ui: &mut egui::Ui, theme: &Theme, glyph: &str, title: &str, detail: Option<&str>, action: Option<&str>) -> Option<egui::Response> {
    let t = &theme.tokens;
    let mut resp = None;
    ui.vertical_centered(|ui| {
        let h = ui.available_height();
        ui.add_space((h * 0.33).max(0.0));
        ui.label(RichText::new(glyph).size(theme.font(TypeToken::DisplayLarge)).color(with_alpha(t.accent, 0.8)));
        ui.add_space(theme.space(crate::theme::SpaceToken::Md));
        ui.label(RichText::new(title).size(theme.font(TypeToken::Title)).color(with_alpha(t.fg, 0.7)));
        if let Some(d) = detail {
            ui.label(RichText::new(d).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        }
        if let Some(a) = action {
            ui.add_space(theme.space(crate::theme::SpaceToken::Lg));
            resp = Some(ui.button(RichText::new(a).color(t.accent)));
        }
    });
    resp
}
