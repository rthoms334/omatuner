//! Badges: source (glyph in source colour + name), quality (HD/FHD/4K/HDR), LIVE (urgent),
//! CATCH-UP, NEW, counts. Text at caption size, 1 px border in the badge colour @0.6.
//! Owner: WP-10a.

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect};

use super::{glyphs, paint};
use crate::model::{QualityBadge, SourceKind};
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
pub(crate) fn source_glyph(src: &SourceMeta) -> &str {
    kind_glyph(&src.icon, src.kind)
}

/// A playlist's glyph from its stored icon (empty = the glyph of its kind).
pub(crate) fn kind_glyph(icon: &str, kind: SourceKind) -> &str {
    if !icon.is_empty() {
        return icon;
    }
    match kind {
        SourceKind::Xtream => glyphs::SOURCE_XTREAM,
        SourceKind::M3uUrl => glyphs::SOURCE_URL,
        SourceKind::M3uFile => glyphs::SOURCE_FILE,
    }
}

/// Contrast a source colour needs to carry text or a glyph on its background (WCAG 1.4.11
/// non-text / large text).
const SOURCE_MIN_CONTRAST: f32 = 3.0;

/// A palette colour used as text on `bg`: itself when it reads (≥ 3:1), else `fallback`.
pub fn legible_on(color: Color32, bg: Color32, fallback: Color32) -> Color32 {
    if crate::theme::tokens::contrast_ratio(color, bg) >= SOURCE_MIN_CONTRAST { color } else { fallback }
}

/// A palette colour used as text on the window background (fg where it wouldn't read).
pub fn legible(theme: &Theme, color: Color32) -> Color32 {
    legible_on(color, theme.tokens.bg, theme.tokens.fg)
}

/// "The Matrix · Provider A": an item's accessible label with the playlist it comes from.
pub fn label_with_source(name: &str, src: Option<&SourceMeta>) -> String {
    match src {
        Some(s) => format!("{name} · {}", s.name),
        None => name.to_owned(),
    }
}

/// The poster corner chip (DESIGN §13.0): the source glyph on bg @ 0.85, like the lock and
/// watched chips. The glyph takes the source colour where it reads on the chip; otherwise it
/// is fg and the colour moves to a 1.5 px edge, so the playlist never depends on a colour that
/// vanishes in this theme.
pub(crate) fn paint_source_chip(p: &egui::Painter, theme: &Theme, rect: Rect, src: &SourceMeta) {
    let t = &theme.tokens;
    let color = theme.source_color(src.color);
    let glyph_color = legible(theme, color);
    p.rect_filled(rect, t.rounding, with_alpha(t.bg, 0.85));
    if glyph_color != color {
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.5, color), egui::StrokeKind::Inside);
    }
    p.text(rect.center(), Align2::CENTER_CENTER, source_glyph(src), paint::font(theme, TypeToken::Icon), glyph_color);
}

/// The compact source mark of a row or tile: the glyph in the source colour on `bg`, anchored at
/// `pos`; where the colour doesn't read the glyph is `fallback` with a 2 px underline in the
/// source colour. Returns the painted rect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_source_mark(
    p: &egui::Painter,
    theme: &Theme,
    pos: Pos2,
    anchor: Align2,
    src: &SourceMeta,
    font: FontId,
    bg: Color32,
    fallback: Color32,
) -> Rect {
    let color = theme.source_color(src.color);
    let glyph_color = legible_on(color, bg, fallback);
    let r = paint::text(p, pos, anchor, source_glyph(src), font, glyph_color, f32::INFINITY);
    if glyph_color != color {
        let bar = Rect::from_min_max(egui::pos2(r.left(), r.bottom() - 2.0), r.right_bottom());
        p.rect_filled(bar, 1.0, color);
    }
    r
}

/// Glyph in the source colour + name; `compact` shows the glyph only (name as tooltip). The
/// border keeps the source colour; the text falls back to fg where the colour wouldn't read.
pub fn source_badge(ui: &mut egui::Ui, theme: &Theme, src: &SourceMeta, compact: bool) -> egui::Response {
    let color = theme.source_color(src.color);
    let text = if compact { source_glyph(src).to_owned() } else { format!("{} {}", source_glyph(src), src.name) };
    let font = paint::font(theme, TypeToken::Caption);
    let pad = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xxs));
    let g = paint::galley(ui.painter(), text, font, legible(theme, color), f32::INFINITY);
    let (rect, resp) = ui.allocate_exact_size(g.size() + 2.0 * pad + egui::vec2(2.0, 2.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(color, 0.6)), egui::StrokeKind::Inside);
        p.galley(rect.center() - g.size() / 2.0, g, legible(theme, color));
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &src.name));
    if compact { resp.on_hover_text(egui::RichText::new(&src.name).size(theme.font(TypeToken::BodySmall))) } else { resp }
}

/// A clickable playlist chip: the source glyph + `text` ("TV 2 (FHD)") with the source colour
/// as its edge, like [`source_badge`] — the Live preview's "Also on" alternatives (DESIGN
/// §29.16). `playing` (the alternative now playing) adds ▶ and a fill; `locked` adds the lock
/// glyph. The accessible name is `label`.
pub fn source_chip_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    src: &SourceMeta,
    text: &str,
    locked: bool,
    playing: bool,
    label: &str,
) -> egui::Response {
    let t = &theme.tokens;
    let color = theme.source_color(src.color);
    let lock = if locked { format!(" {}", glyphs::LOCK) } else { String::new() };
    // the one playing says so with ▶ and a fill (it isn't a keyboard cursor: no focus ring)
    let play = if playing { format!("{} ", glyphs::PLAY) } else { String::new() };
    let shown = format!("{play}{} {text}{lock}", source_glyph(src));
    let font = paint::font(theme, TypeToken::BodySmall);
    let pad = egui::vec2(theme.space(SpaceToken::Md), theme.space(SpaceToken::Xs));
    let g = paint::galley(ui.painter(), shown, font, legible(theme, color), f32::INFINITY);
    let (rect, resp) = ui.allocate_exact_size(g.size() + 2.0 * pad + egui::vec2(2.0, 2.0), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() || playing {
            p.rect_filled(rect, t.rounding, t.fill_hover);
        }
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, with_alpha(color, 0.8)), egui::StrokeKind::Inside);
        p.galley(rect.center() - g.size() / 2.0, g, legible(theme, color));
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    resp
}

pub fn live_badge(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    badge(ui, theme, "LIVE", theme.tokens.urgent)
}

/// A channel's quality badge in a `Ui` flow (preview, guide side panel): the [`InlineBadge`]
/// the lists paint, so "FHD" looks the same everywhere; the tooltip says where it comes from
/// (measured, or the provider's word — "not verified").
pub fn quality_badge(ui: &mut egui::Ui, theme: &Theme, q: QualityBadge, color: Color32) -> egui::Response {
    let b = InlineBadge::quality(ui.painter(), theme, q, color, 1.0);
    let (rect, resp) = ui.allocate_exact_size(b.size(), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        b.paint(ui.painter(), theme, rect.min, Align2::LEFT_TOP);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, q.label()));
    resp.on_hover_text(egui::RichText::new(q.explain()).size(theme.font(TypeToken::BodySmall)))
}

/// The painter-level [`badge`] (rows, grid headers, OSD lists): `text` in `font`, 1 px border
/// in its colour at 0.6 of the text's opacity (lighter than the text, also for a dimmed
/// colour), the same padding. Laid out up front so a row can measure it first.
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

    /// A channel's quality badge: monospace caption (`scale`d for the OSD); `4K?` for a
    /// doubtful claim.
    pub fn quality(p: &egui::Painter, theme: &Theme, q: QualityBadge, color: Color32, scale: f32) -> Self {
        Self::new(p, theme, q.label(), FontId::monospace((theme.font(TypeToken::Caption) * scale).round()), color)
    }

    pub fn size(&self) -> egui::Vec2 {
        self.galley.size() + 2.0 * self.pad
    }

    /// Paint at `pos` with `anchor`; returns the badge rect.
    pub fn paint(self, p: &egui::Painter, theme: &Theme, pos: Pos2, anchor: Align2) -> Rect {
        let rect = anchor.anchor_size(pos, self.size());
        let border = with_alpha(self.color, 0.6 * f32::from(self.color.a()) / 255.0);
        p.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);
        p.galley(rect.center() - self.galley.size() / 2.0, self.galley, self.color);
        rect
    }
}

/// A channel name followed by its quality badge, left-aligned at `left_center` within `max_w`
/// (DESIGN §13.0). See [`job_with_quality`].
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
    job_with_quality(p, theme, left_center, egui::text::LayoutJob::simple_singleline(name.to_owned(), font, color), badge, max_w)
}

/// A laid-out name (`job`: one line, e.g. with the matched characters of a search hit)
/// followed by its quality badge. The badge tells the duplicates of a channel apart, so it
/// stays and the name elides beside it; only when that would leave the name less than
/// `max(2 × badge, 6 em)` is the badge dropped for the name. Returns the painted extent.
pub fn job_with_quality(
    p: &egui::Painter,
    theme: &Theme,
    left_center: Pos2,
    mut job: egui::text::LayoutJob,
    badge: Option<InlineBadge>,
    max_w: f32,
) -> Rect {
    let (em, color) = job.sections.first().map(|s| (s.format.font_id.size, s.format.color)).unwrap_or((0.0, Color32::PLACEHOLDER));
    let gap = theme.space(SpaceToken::Md);
    let full = p.layout_job(job.clone());
    let name_w = |b: &InlineBadge| max_w - gap - b.size().x;
    let badge = badge.filter(|b| full.size().x <= name_w(b) || name_w(b) >= (2.0 * b.size().x).max(6.0 * em));
    let room = badge.as_ref().map_or(max_w, name_w);
    let name_g = if full.size().x <= room {
        full
    } else {
        job.wrap = egui::text::TextWrapping::truncate_at_width(room.max(0.0));
        p.layout_job(job)
    };
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

    /// The badge stays and the name elides beside it, down to a readable floor; below that
    /// the name gets the room — never past `max_w`.
    #[test]
    fn quality_badge_stays_while_the_name_elides() {
        let theme = Theme::fallback();
        let font = FontId::proportional(theme.font(TypeToken::Body));
        // (painted width, badge painted?) of `name` within `max_w`
        let draw = |name: &str, max_w: f32| {
            let ctx = egui::Context::default();
            let mut w = 0.0;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let p = ui.painter();
                let badge = InlineBadge::quality(p, &theme, crate::model::QualityBadge::named(crate::model::Quality::Hd, false), Color32::GRAY, 1.0);
                w = name_with_quality(p, &theme, Pos2::new(0.0, 50.0), name, font.clone(), Color32::WHITE, Some(badge), max_w).width();
            });
            out.textures_delta.clear();
            (w, out.shapes.iter().any(|s| matches!(s.shape, egui::Shape::Rect(_))))
        };
        let long = "Sky Sports Main Event";
        // name + gap + badge, and gap + badge
        let ((full_w, _), (badge_w, _)) = (draw(long, f32::INFINITY), draw("", f32::INFINITY));
        let floor = 6.0 * font.size + badge_w;
        assert!(full_w > floor + 8.0, "the long name needs eliding at the floor");
        assert_eq!(draw(long, full_w), (full_w, true), "room: name and badge");
        let (w, badge) = draw(long, floor + 4.0);
        assert!(badge && w <= floor + 4.5, "the name elides, the badge stays: {w}");
        let (w, badge) = draw(long, floor - 4.0);
        assert!(!badge && w <= floor - 3.5, "below the floor the name gets the room: {w}");
        assert!(draw("BBC", floor - 4.0).1, "a short name that fits keeps its badge");
    }
}
