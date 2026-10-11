//! Section header (caption, bold, darker(fg,1.4)), separators (fg@0.12), pane divider
//! (fg@0.28), group divider (fg@0.2). Implemented in the skeleton.

use eframe::egui::{self, RichText};

use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::{NavRequest, UiCtx};

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

/// A page's header row: `‹ {back}` when the app's Back leads somewhere (`ctx.back`; a click goes
/// back like Esc), then `title` (none when empty), then `trailing` controls at the right end.
pub fn page_header(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, title: &str, trailing: impl FnOnce(&mut egui::Ui)) {
    let theme = ctx.theme;
    let mut back = false;
    let target = ctx.back;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
        // the trailing controls first (right to left), the back button and title get the rest
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            trailing(ui);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if let Some(b) = target {
                    back = super::button::back_button(ui, theme, b.label).clicked();
                }
                if !title.is_empty() {
                    ui.add(egui::Label::new(RichText::new(title).size(theme.font(TypeToken::Heading)).color(theme.tokens.fg)).truncate());
                }
            });
        });
    });
    if back {
        ctx.navigate(NavRequest::Back);
    }
}
