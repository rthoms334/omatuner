//! Empty / error / loading states (Clipboard.qml): glyph at display-large in accent @0.8,
//! message at title in fg @0.7, optional detail and action button. Owner: WP-10a.

use eframe::egui::{self, RichText};

use super::button::{ButtonStyle, button};
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

/// Centred in the available rect. Returns the action button response if `action` was given.
pub fn empty_state(ui: &mut egui::Ui, theme: &Theme, glyph: &str, title: &str, detail: Option<&str>, action: Option<&str>) -> Option<egui::Response> {
    let t = &theme.tokens;
    let mut resp = None;
    ui.vertical_centered(|ui| {
        let h = ui.available_height();
        // a third of the way down; never pushes the content out of short panes
        let content = theme.font(TypeToken::DisplayLarge) + theme.font(TypeToken::Title) * 3.0 + theme.space(SpaceToken::ControlHeight);
        ui.add_space(((h - content) * 0.33).max(0.0));
        ui.label(RichText::new(glyph).size(theme.font(TypeToken::DisplayLarge)).color(with_alpha(t.accent, 0.8)));
        ui.add_space(theme.space(SpaceToken::Md));
        ui.add(egui::Label::new(RichText::new(title).size(theme.font(TypeToken::Title)).color(with_alpha(t.fg, 0.7))).wrap());
        if let Some(d) = detail {
            ui.add_space(theme.space(SpaceToken::Xs));
            ui.add(egui::Label::new(RichText::new(d).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).wrap());
        }
        if let Some(a) = action {
            ui.add_space(theme.space(SpaceToken::Lg));
            resp = Some(button(ui, theme, None, a, ButtonStyle { primary: true, ..Default::default() }));
        }
    });
    resp
}
