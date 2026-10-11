//! Small painting helpers shared by the widgets of this module (not part of the widget API).

use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontFamily, FontId, Pos2, Rect};

use super::RowState;
use crate::theme::egui_style::{FAMILY_BOLD, FAMILY_MEDIUM};
use crate::theme::{Theme, TypeToken};

/// Where the app records each frame whether the parental session is locked.
fn session_locked_id() -> egui::Id {
    egui::Id::new("tuner.parental.session-locked")
}

/// Record that the parental session is locked (`true`) or open this frame. The app calls this
/// every frame (DESIGN §20.3) so locked rows and posters paint dimmed until the PIN is entered.
pub fn set_session_locked(ctx: &egui::Context, locked: bool) {
    ctx.data_mut(|d| d.insert_temp(session_locked_id(), locked));
}

/// The parental session is locked this frame ([`set_session_locked`]; open when never set).
pub fn session_locked(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(session_locked_id())).unwrap_or(false)
}

/// Regular weight at a type token.
pub(super) fn font(theme: &Theme, t: TypeToken) -> FontId {
    FontId::proportional(theme.font(t))
}

/// Medium weight (menu row labels, sidebar).
pub(super) fn medium(ctx: &egui::Context, theme: &Theme, t: TypeToken) -> FontId {
    weighted(ctx, theme, t, FAMILY_MEDIUM)
}

/// Bold weight (selected buttons, section headers).
pub(super) fn bold(ctx: &egui::Context, theme: &Theme, t: TypeToken) -> FontId {
    weighted(ctx, theme, t, FAMILY_BOLD)
}

/// A named weight registered by `fonts::install`, or the regular face while it isn't bound
/// (egui panics on unknown families; headless harnesses and the first frames have none).
fn weighted(ctx: &egui::Context, theme: &Theme, t: TypeToken, name: &str) -> FontId {
    let family = FontFamily::Name(name.into());
    let bound = ctx.fonts(|f| f.definitions().families.contains_key(&family));
    FontId::new(theme.font(t), if bound { family } else { FontFamily::Proportional })
}

/// Text colour of a row in `state`: accent under the cursor, fg otherwise.
pub(super) fn row_text(theme: &Theme, state: RowState) -> Color32 {
    if state.is_cursor() { theme.tokens.list_cursor_text } else { theme.tokens.fg }
}

/// Lay out one line, elided with "…" at `max_width` (no elision when `max_width` is infinite).
pub(super) fn galley(painter: &egui::Painter, text: impl Into<String>, font: FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.into(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    painter.layout_job(job)
}

/// Paint one elided line anchored at `pos`; returns the painted rect.
pub(super) fn text(
    painter: &egui::Painter,
    pos: Pos2,
    anchor: Align2,
    text: impl Into<String>,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Rect {
    let g = galley(painter, text, font, color, max_width);
    let rect = anchor.anchor_size(pos, g.size());
    painter.galley(rect.min, g, color);
    rect
}

/// 1 px keyboard focus ring (fg @ 0.25, Omarchy focus = hover by design), inside `rect`.
pub(super) fn focus_ring(painter: &egui::Painter, rect: Rect, theme: &Theme) {
    painter.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, theme.tokens.border_hover), egui::StrokeKind::Inside);
}

/// An `egui::Margin` from point values (egui margins are whole points in an `i8`).
pub(super) fn margin(x: f32, y: f32) -> egui::Margin {
    let p = |v: f32| v.round().clamp(0.0, i8::MAX as f32) as i8;
    egui::Margin::symmetric(p(x), p(y))
}

/// Height for a row widget: its natural height, unless the enclosing `Ui` is a fixed row slot
/// that is shorter (e.g. a compact-density `virtual_list` slot), so rows never overflow it.
pub(super) fn fit_height(ui: &egui::Ui, natural: f32) -> f32 {
    let slot = ui.max_rect().height();
    if slot.is_finite() && slot >= 16.0 && slot < natural { slot } else { natural }
}

/// Media duration "1:02:13" / "41:20" (negative and NaN clamp to 0).
pub(super) fn clock(seconds: f64) -> String {
    let s = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(2480.4), "41:20");
        assert_eq!(clock(3733.0), "1:02:13");
        assert_eq!(clock(-5.0), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
    }

    #[test]
    fn margin_rounds_and_clamps() {
        assert_eq!(margin(10.4, 7.6), egui::Margin::symmetric(10, 8));
        assert_eq!(margin(500.0, -3.0), egui::Margin::symmetric(127, 0));
    }
}
