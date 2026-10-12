//! The mini-card of one copy of a title in a detail page's Copies section (DESIGN §29.14):
//! the playlist (glyph in its colour + name), the copy's language and version ("EN · 4K HDR"),
//! a status at the right ("plays first", "always plays first", "locked", "failed 2 h ago"),
//! and a progress bar along the bottom while it is partly watched.
//!
//! The copy that Play uses is drawn selected (fill + accent border), the keyboard focus adds
//! the 2 px accent outline; a locked copy is drawn disabled (dimmed) while the session is
//! locked. A click picks the copy, a right-click opens its menu — the page decides.

use eframe::egui::{self, Align2, Rect};

use super::{badge, glyphs, paint};
use crate::store::library::SourceMeta;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

/// Mini-card width (pt × rem).
pub const CARD_W: f32 = 230.0;

/// How a status reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    /// "plays first".
    Accent,
    /// "failed 2 h ago".
    Warning,
    /// "locked".
    Muted,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CopyCard<'a> {
    pub source: Option<&'a SourceMeta>,
    /// Language and version ("EN · 4K HDR").
    pub facts: Option<&'a str>,
    pub status: Option<(&'a str, StatusTone)>,
    /// Watch progress 0..1 (shown while partly watched).
    pub progress: Option<f32>,
    /// Locked while the session is: drawn disabled.
    pub dimmed: bool,
    /// The copy Play uses.
    pub chosen: bool,
    /// The keyboard focus.
    pub focused: bool,
}

/// Height of a mini-card.
pub fn card_height(theme: &Theme) -> f32 {
    (2.0 * theme.font(TypeToken::Body) * 1.3 + 2.0 * theme.space(SpaceToken::Md) + 4.0 * theme.rem()).ceil()
}

/// Draw one mini-card of `width`.
pub fn copy_card(ui: &mut egui::Ui, theme: &Theme, width: f32, c: CopyCard<'_>) -> egui::Response {
    let t = &theme.tokens;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, card_height(theme)), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = [c.source.map(|s| s.name.as_str()), c.facts, c.status.map(|s| s.0)].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, c.chosen, &label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let fill = if c.chosen {
        t.fill_selected
    } else if resp.hovered() {
        t.fill_hover
    } else {
        t.fill_field
    };
    p.rect_filled(rect, t.rounding, fill);
    let border = if c.focused {
        egui::Stroke::new(2.0, t.accent)
    } else if c.chosen {
        egui::Stroke::new(1.0, t.accent)
    } else {
        egui::Stroke::new(1.0, t.border_hover)
    };
    p.rect_stroke(rect, t.rounding, border, egui::StrokeKind::Inside);
    let pad = theme.space(SpaceToken::Md);
    let inner = rect.shrink(pad);
    let font = paint::font(theme, TypeToken::Body);
    let small = paint::font(theme, TypeToken::BodySmall);
    let ink = if c.dimmed { t.text_disabled } else { t.fg };
    // line 1: the playlist, the status at the right
    let mut right = inner.right();
    if let Some((text, tone)) = c.status {
        let color = match tone {
            StatusTone::Accent if !c.dimmed => t.accent,
            StatusTone::Warning => t.warning,
            _ => t.text_secondary,
        };
        let text = if tone == StatusTone::Muted && c.dimmed { format!("{} {text}", glyphs::LOCK) } else { text.to_owned() };
        right = paint::text(p, egui::pos2(inner.right(), inner.top()), Align2::RIGHT_TOP, text, small.clone(), color, inner.width() * 0.5).left()
            - theme.space(SpaceToken::Sm);
    }
    let mut left = inner.left();
    if let Some(src) = c.source {
        let bg = if c.chosen { t.bg } else { t.card_bg };
        // the code chip, then the name (the card is about which copy: the full name stays)
        let y = inner.top() + theme.font(TypeToken::Body) * 0.65;
        let mark = badge::paint_code_chip(p, theme, egui::pos2(left, y), Align2::LEFT_CENTER, src, bg, ink);
        left = mark.right() + theme.space(SpaceToken::Sm);
        paint::text(p, egui::pos2(left, inner.top()), Align2::LEFT_TOP, src.name.as_str(), font, ink, (right - left).max(0.0));
    }
    // line 2: language and version
    if let Some(f) = c.facts {
        let y = inner.top() + theme.font(TypeToken::Body) * 1.3;
        paint::text(p, egui::pos2(inner.left(), y), Align2::LEFT_TOP, f, small, t.text_secondary, inner.width());
    }
    if let Some(f) = c.progress.filter(|f| *f > 0.0 && *f < 1.0) {
        let h = (3.0 * theme.rem()).round().max(2.0);
        let bar = Rect::from_min_max(egui::pos2(rect.left() + 1.0, rect.bottom() - h - 1.0), rect.right_bottom() - egui::vec2(1.0, 1.0));
        super::progress::paint_progress(p, bar, theme, f, true);
    }
    if c.dimmed {
        p.rect_filled(rect, t.rounding, with_alpha(t.bg, 0.35));
    }
    resp
}
