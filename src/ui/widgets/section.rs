//! Section header (caption, bold, darker(fg,1.4)), separators (fg@0.12), pane divider
//! (fg@0.28), group divider (fg@0.2). Implemented in the skeleton.

use eframe::egui::{self, RichText};

use crate::theme::{Theme, TypeToken};

pub fn section_header(ui: &mut egui::Ui, theme: &Theme, text: &str) -> egui::Response {
    ui.add_space((theme.font(TypeToken::Caption) * 0.15).ceil());
    ui.label(RichText::new(text.to_uppercase()).size(theme.font(TypeToken::Caption)).strong().color(theme.tokens.text_section))
}

pub fn separator(ui: &mut egui::Ui, theme: &Theme) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 0.0, theme.tokens.separator);
}

/// Vertical 1 px divider between panes, full height of `rect`.
pub fn pane_divider(painter: &egui::Painter, x: f32, y_range: egui::Rangef, theme: &Theme) {
    painter.vline(x, y_range, egui::Stroke::new(1.0, theme.tokens.pane_divider));
}
