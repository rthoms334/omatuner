//! Sliders (PanelSlider: track fg@0.18, fill fg, knob grows ×1.15 on hover) and the player
//! seek bar. Owner: WP-10a.
//!
//! Keyboard: neither widget reads keys; the screen maps `Left`/`Right` (Panel) or the OSD
//! scrub actions to value changes and passes `focused` for the focus ring / visible knob.

use eframe::egui::{self, Rect};

use super::paint;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

/// Knob scale while hovered or dragged (Omarchy PanelSlider).
const KNOB_HOVER_SCALE: f32 = 1.15;

/// Track thickness and knob size for the theme's control height.
fn metrics(theme: &Theme) -> (f32, f32) {
    let ch = theme.space(SpaceToken::ControlHeight);
    ((0.11 * ch).max(4.0).round(), (0.38 * ch).max(14.0).round())
}

/// Snap `v` to `step` from the range start and clamp it into the range.
fn snap(v: f32, range: &std::ops::RangeInclusive<f32>, step: f32) -> f32 {
    let (lo, hi) = (*range.start(), *range.end());
    let v = if step > 0.0 { lo + ((v - lo) / step).round() * step } else { v };
    v.clamp(lo.min(hi), hi.max(lo))
}

/// 0..1 position of `x` along a track whose usable span is `[left, right]`.
fn fraction_at(x: f32, left: f32, right: f32) -> f32 {
    if right > left { ((x - left) / (right - left)).clamp(0.0, 1.0) } else { 0.0 }
}

pub fn slider(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, focused: bool) -> egui::Response {
    let t = &theme.tokens;
    let (track_h, knob) = metrics(theme);
    let width = ui.available_width().max(knob * 4.0);
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(width, theme.space(SpaceToken::ControlHeight)), egui::Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    // the knob's centre travels between these x positions
    let (x0, x1) = (rect.left() + knob / 2.0, rect.right() - knob / 2.0);
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.clicked() || resp.dragged())
    {
        let v = snap(lo + fraction_at(pos.x, x0, x1) * (hi - lo), &range, step);
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    let frac = if hi != lo { ((*value - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 0.0 };
    resp.widget_info(|| egui::WidgetInfo::slider(ui.is_enabled(), *value as f64, ""));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let track = Rect::from_center_size(rect.center(), egui::vec2(x1 - x0, track_h));
        p.rect_filled(track, t.rounding, t.fill_selected);
        let kx = x0 + frac * (x1 - x0);
        p.rect_filled(Rect::from_min_max(track.min, egui::pos2(kx, track.bottom())), t.rounding, t.fg);
        let hot = resp.hovered() || resp.dragged();
        let k = knob * if hot { KNOB_HOVER_SCALE } else { 1.0 };
        p.rect_filled(Rect::from_center_size(egui::pos2(kx, rect.center().y), egui::vec2(k, k)), t.rounding, t.fg);
        if focused {
            paint::focus_ring(p, rect, theme);
        }
    }
    resp
}

/// Seek bar over video (OSD colours, not theme): played, buffered (`cache_end`), hover time
/// tooltip. Returns a requested absolute position when clicked/dragged/scrubbed.
///
/// Unknown or zero `duration` (live streams) draws an empty track and never seeks.
pub fn seek_bar(ui: &mut egui::Ui, theme: &Theme, position: f64, duration: f64, cache_end: Option<f64>, focused: bool) -> Option<f64> {
    let t = &theme.tokens;
    let (track_h, knob) = metrics(theme);
    let seekable = duration.is_finite() && duration > 0.0;
    let sense = if seekable { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(knob * 4.0), theme.space(SpaceToken::ControlHeight)), sense);
    resp.widget_info(|| egui::WidgetInfo::slider(seekable, if position.is_finite() { position } else { 0.0 }, "Seek"));
    let (x0, x1) = (rect.left() + knob / 2.0, rect.right() - knob / 2.0);
    let at = |x: f32| fraction_at(x, x0, x1) as f64 * duration;
    // where the pointer holds the knob; a seek is requested only when that changes (a click, a
    // drag step, the release), not on every frame the knob is held still
    let held = match resp.interact_pointer_pos() {
        Some(pos) if seekable && (resp.clicked() || resp.dragged() || resp.drag_stopped()) => Some(at(pos.x)),
        _ => None,
    };
    let request = held.filter(|_| resp.clicked() || resp.drag_stopped() || resp.drag_delta().x != 0.0);
    if !ui.is_rect_visible(rect) {
        return request;
    }
    let p = ui.painter();
    let track = Rect::from_center_size(rect.center(), egui::vec2(x1 - x0, track_h));
    p.rect_filled(track, t.rounding, with_alpha(t.osd_text, 0.25));
    if !seekable {
        return None;
    }
    let x_of = |s: f64| x0 + ((s / duration).clamp(0.0, 1.0) as f32) * (x1 - x0);
    // while dragging, show the pointer's position (mpv's answer arrives a frame later)
    let shown = held.unwrap_or(if position.is_finite() { position } else { 0.0 });
    let played_x = x_of(shown);
    if let Some(end) = cache_end.filter(|e| e.is_finite() && *e > shown) {
        p.rect_filled(
            Rect::from_min_max(egui::pos2(played_x, track.top()), egui::pos2(x_of(end), track.bottom())),
            t.rounding,
            with_alpha(t.osd_text, 0.5),
        );
    }
    p.rect_filled(Rect::from_min_max(track.min, egui::pos2(played_x, track.bottom())), t.rounding, t.osd_accent);
    let hot = resp.hovered() || resp.dragged();
    if hot || focused {
        let k = knob * if hot { KNOB_HOVER_SCALE } else { 1.0 };
        p.rect_filled(Rect::from_center_size(egui::pos2(played_x, rect.center().y), egui::vec2(k, k)), t.rounding, t.osd_accent);
    }
    if focused {
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, with_alpha(t.osd_text, 0.5)), egui::StrokeKind::Inside);
    }
    // hover time above the pointer, on the OSD scrim (a layer above the clip rect)
    if let Some(pos) = resp.hover_pos() {
        let label = paint::clock(at(pos.x));
        let layer = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, resp.id.with("seek-time")));
        let g = paint::galley(&layer, label, paint::font(theme, TypeToken::BodySmall), t.osd_text, f32::INFINITY);
        let pad = egui::vec2(theme.space(SpaceToken::Md), theme.space(SpaceToken::Xxs));
        let r = Rect::from_center_size(egui::pos2(pos.x, rect.top() - g.size().y / 2.0 - pad.y * 2.0), g.size() + 2.0 * pad);
        layer.rect_filled(r, t.rounding, t.osd_scrim);
        layer.galley(r.min + pad, g, t.osd_text);
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping_and_fractions() {
        assert_eq!(snap(0.53, &(0.0..=1.0), 0.1), 0.5);
        assert_eq!(snap(57.0, &(0.0..=100.0), 5.0), 55.0);
        assert_eq!(snap(130.0, &(0.0..=100.0), 5.0), 100.0);
        assert_eq!(snap(-3.0, &(0.0..=100.0), 0.0), 0.0);
        assert_eq!(snap(1.75, &(1.0..=2.0), 0.5), 2.0);
        assert_eq!(fraction_at(150.0, 100.0, 200.0), 0.5);
        assert_eq!(fraction_at(50.0, 100.0, 200.0), 0.0);
        assert_eq!(fraction_at(250.0, 100.0, 200.0), 1.0);
        assert_eq!(fraction_at(5.0, 10.0, 10.0), 0.0);
    }
}
