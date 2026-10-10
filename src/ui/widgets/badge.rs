//! Badges: source (glyph in source colour + name), quality (HD/FHD/4K/HDR), LIVE (urgent),
//! CATCH-UP, NEW, counts. Text at caption size, 1 px border in the badge colour @0.6.
//! Owner: WP-10a.

use eframe::egui;

use super::{glyphs, paint};
use crate::model::SourceKind;
use crate::store::library::SourceMeta;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

pub fn badge(ui: &mut egui::Ui, theme: &Theme, text: &str, color: egui::Color32) -> egui::Response {
    let font = paint::font(theme, TypeToken::Caption);
    let pad = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xxs));
    let g = paint::galley(ui.painter(), text, font, color, f32::INFINITY);
    let (rect, resp) = ui.allocate_exact_size(g.size() + 2.0 * pad + egui::vec2(2.0, 2.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(color, 0.6)), egui::StrokeKind::Inside);
        p.galley(rect.center() - g.size() / 2.0, g, color);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    resp
}

/// The source's glyph (its user-chosen icon, else one for its kind).
pub(super) fn source_glyph(src: &SourceMeta) -> &str {
    if !src.icon.is_empty() {
        return &src.icon;
    }
    match src.kind {
        SourceKind::Xtream => glyphs::SOURCE_XTREAM,
        SourceKind::M3uUrl => glyphs::SOURCE_URL,
        SourceKind::M3uFile => glyphs::SOURCE_FILE,
    }
}

/// Glyph in the source colour + name; `compact` shows the glyph only (name as tooltip).
pub fn source_badge(ui: &mut egui::Ui, theme: &Theme, src: &SourceMeta, compact: bool) -> egui::Response {
    let color = theme.source_color(src.color);
    let text = if compact { source_glyph(src).to_owned() } else { format!("{} {}", source_glyph(src), src.name) };
    let resp = badge(ui, theme, &text, color);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &src.name));
    if compact { resp.on_hover_text(egui::RichText::new(&src.name).size(theme.font(TypeToken::BodySmall))) } else { resp }
}

pub fn live_badge(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    badge(ui, theme, "LIVE", theme.tokens.urgent)
}
