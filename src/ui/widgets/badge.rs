//! Badges: source (glyph in source colour + name), quality (HD/FHD/4K/HDR), LIVE (urgent),
//! CATCH-UP, NEW, counts. Text at caption size, 1 px border in the badge colour @0.6.
//! Owner: WP-10a.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect};

use super::{glyphs, paint};
use crate::model::{Quality, SourceKind};
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

/// The painter-level [`badge`] (rows, grid headers, OSD lists): `text` in `font`, 1 px border
/// in its colour @ 0.6, the same padding. Laid out up front so a row can measure it first.
pub struct InlineBadge {
    galley: Arc<egui::Galley>,
    pad: egui::Vec2,
    color: Color32,
}

impl InlineBadge {
    pub fn new(p: &egui::Painter, theme: &Theme, text: &str, font: FontId, color: Color32) -> Self {
        let scale = font.size / theme.font(TypeToken::Caption).max(1.0);
        let pad = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xxs)) * scale + egui::vec2(1.0, 1.0);
        Self { galley: paint::galley(p, text, font, color, f32::INFINITY), pad, color }
    }

    /// A channel's quality badge: monospace caption (`scale`d for the OSD).
    pub fn quality(p: &egui::Painter, theme: &Theme, q: Quality, color: Color32, scale: f32) -> Self {
        Self::new(p, theme, q.label(), FontId::monospace((theme.font(TypeToken::Caption) * scale).round()), color)
    }

    pub fn size(&self) -> egui::Vec2 {
        self.galley.size() + 2.0 * self.pad
    }

    /// Paint at `pos` with `anchor`; returns the badge rect.
    pub fn paint(self, p: &egui::Painter, theme: &Theme, pos: Pos2, anchor: Align2) -> Rect {
        let rect = anchor.anchor_size(pos, self.size());
        p.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(self.color, 0.6)), egui::StrokeKind::Inside);
        p.galley(rect.center() - self.galley.size() / 2.0, self.galley, self.color);
        rect
    }
}

/// A channel name followed by its quality badge, left-aligned at `left_center` within `max_w`
/// (DESIGN §13.0). The badge only shows while the whole name fits beside it: it never
/// truncates the name — when space runs out the badge goes first, then the name elides.
/// Returns the painted extent.
#[allow(clippy::too_many_arguments)]
pub fn name_with_quality(
    p: &egui::Painter,
    theme: &Theme,
    left_center: Pos2,
    name: &str,
    font: FontId,
    color: Color32,
    badge: Option<InlineBadge>,
    max_w: f32,
) -> Rect {
    let full = paint::galley(p, name, font.clone(), color, f32::INFINITY);
    let gap = theme.space(SpaceToken::Md);
    let badge = badge.filter(|b| full.size().x + gap + b.size().x <= max_w);
    let name_g = if badge.is_some() { full } else { paint::galley(p, name, font, color, max_w) };
    let name_rect = Align2::LEFT_CENTER.anchor_size(left_center, name_g.size());
    p.galley(name_rect.min, name_g, color);
    match badge {
        Some(b) => name_rect.union(b.paint(p, theme, egui::pos2(name_rect.right() + gap, left_center.y), Align2::LEFT_CENTER)),
        None => name_rect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge goes before the name is cut: shown while the whole name fits beside it, then
    /// the full name alone, then the name elided — never past `max_w`.
    #[test]
    fn quality_badge_gives_way_before_the_name_elides() {
        let theme = Theme::fallback();
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let p = ui.painter();
            let font = FontId::proportional(theme.font(TypeToken::Heading));
            let name_w = paint::galley(p, "BBC One", font.clone(), Color32::WHITE, f32::INFINITY).size().x;
            let draw = |max_w: f32| {
                let badge = InlineBadge::quality(p, &theme, Quality::Hd, Color32::GRAY, 1.0);
                name_with_quality(p, &theme, Pos2::ZERO, "BBC One", font.clone(), Color32::WHITE, Some(badge), max_w).width()
            };
            assert!(draw(name_w * 3.0) > name_w + theme.space(SpaceToken::Md), "room: name and badge");
            assert_eq!(draw(name_w + 4.0), name_w, "no room for the badge: the whole name, no badge");
            assert!(draw(name_w / 2.0) <= name_w / 2.0 + 0.5, "then the name elides");
        });
        out.textures_delta.clear();
    }
}
