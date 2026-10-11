//! Sliders (PanelSlider: track fg@0.18, fill fg, knob grows ×1.15 on hover) and the player
//! seek bar. Owner: WP-10a.
//!
//! Keyboard: neither widget reads keys; the screen maps `Left`/`Right` (Panel) or the OSD
//! scrub actions to value changes and passes `focused` for the focus ring / visible knob.
//!
//! Mouse: click sets, drag scrubs, the pointer is a pointing hand. [`slider_in`] (the core of
//! [`slider`], also the OSD volume) can take the wheel: one step per 40 px notch while hovered
//! (the player's convention), consuming the scroll so nothing under it scrolls too.

use eframe::egui::{self, Color32, Rect};

use super::paint;
use crate::theme::{SpaceToken, Theme, Tokens, TypeToken, with_alpha};

/// Knob scale while hovered or dragged (Omarchy PanelSlider).
const KNOB_HOVER_SCALE: f32 = 1.15;

/// Wheel distance of one step (one notch of a line-based wheel).
const WHEEL_NOTCH: f32 = 40.0;

/// Track thickness and knob size for a control of height `ch`.
fn metrics(ch: f32) -> (f32, f32) {
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

/// Colours of a slider: the theme's (Settings) or the fixed OSD ones (over video).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SliderLook {
    pub track: Color32,
    pub fill: Color32,
    pub knob: Color32,
    /// Tick marks on the track ([`SliderSpec::marks`]).
    pub mark: Color32,
}

impl SliderLook {
    pub fn theme(t: &Tokens) -> Self {
        Self { track: t.fill_selected, fill: t.fg, knob: t.fg, mark: t.text_secondary }
    }
    /// Over video (FIXED DECISION 6): never theme colours.
    pub fn osd(t: &Tokens) -> Self {
        Self { track: with_alpha(t.osd_text, 0.25), fill: t.osd_accent, knob: t.osd_accent, mark: with_alpha(t.osd_text, 0.6) }
    }
}

/// What a [`slider_in`] edits and how.
pub struct SliderSpec<'a> {
    pub range: std::ops::RangeInclusive<f32>,
    /// Snapping step from the range start (0 = continuous).
    pub step: f32,
    /// Values marked with a tick on the track (e.g. 100 % on a 0–130 % volume).
    pub marks: &'a [f32],
    /// The wheel changes the value by this much per notch while hovered (`None` = ignored).
    pub wheel_step: Option<f32>,
    /// Accessible name (what kittest and screen readers find it by).
    pub label: &'a str,
    pub look: SliderLook,
}

pub fn slider(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, focused: bool) -> egui::Response {
    let ch = theme.space(SpaceToken::ControlHeight);
    let (_, knob) = metrics(ch);
    let width = ui.available_width().max(knob * 4.0);
    let (id, rect) = ui.allocate_space(egui::vec2(width, ch));
    let spec = SliderSpec { range, step, marks: &[], wheel_step: None, label: "", look: SliderLook::theme(&theme.tokens) };
    slider_in(ui, rect, id, theme, value, focused, &spec)
}

/// A slider in `rect` (its height is the control height the track and knob are sized from).
/// Click sets the value, drag scrubs, the wheel steps it ([`SliderSpec::wheel_step`]); the
/// response is `changed()` when `value` changed.
pub fn slider_in(ui: &egui::Ui, rect: Rect, id: egui::Id, theme: &Theme, value: &mut f32, focused: bool, spec: &SliderSpec<'_>) -> egui::Response {
    let t = &theme.tokens;
    let (track_h, knob) = metrics(rect.height());
    let mut resp = ui.interact(rect, id, egui::Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::PointingHand);
    let range = &spec.range;
    let (lo, hi) = (*range.start(), *range.end());
    // the knob's centre travels between these x positions
    let (x0, x1) = (rect.left() + knob / 2.0, rect.right() - knob / 2.0);
    let mut v = *value;
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.clicked() || resp.dragged())
    {
        v = snap(lo + fraction_at(pos.x, x0, x1) * (hi - lo), range, spec.step);
    }
    if let Some(step) = spec.wheel_step.filter(|_| resp.hovered()) {
        let notches = wheel_notches(ui, id);
        if notches != 0.0 {
            v = snap(v + notches * step, range, spec.step);
        }
    }
    if v != *value {
        *value = v;
        resp.mark_changed();
    }
    let frac = if hi != lo { ((*value - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 0.0 };
    resp.widget_info(|| egui::WidgetInfo::slider(ui.is_enabled(), *value as f64, spec.label));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let look = &spec.look;
        let track = Rect::from_center_size(rect.center(), egui::vec2(x1 - x0, track_h));
        p.rect_filled(track, t.rounding, look.track);
        let kx = x0 + frac * (x1 - x0);
        p.rect_filled(Rect::from_min_max(track.min, egui::pos2(kx, track.bottom())), t.rounding, look.fill);
        if hi != lo {
            for m in spec.marks.iter().filter(|m| range.contains(*m)) {
                let mx = x0 + (m - lo) / (hi - lo) * (x1 - x0);
                p.rect_filled(Rect::from_center_size(egui::pos2(mx, rect.center().y), egui::vec2(2.0, track_h * 2.0)), 0.0, look.mark);
            }
        }
        let hot = resp.hovered() || resp.dragged();
        let k = knob * if hot { KNOB_HOVER_SCALE } else { 1.0 };
        p.rect_filled(Rect::from_center_size(egui::pos2(kx, rect.center().y), egui::vec2(k, k)), t.rounding, look.knob);
        if focused {
            paint::focus_ring(p, rect, theme);
        }
    }
    resp
}

/// Whole wheel notches scrolled over the widget `id` this frame (up = positive), the remainder
/// kept for the next frame (smooth scrolling spreads one notch over several frames). Consumes
/// the scroll so nothing under the widget scrolls or reacts to it as well.
pub fn wheel_notches(ui: &egui::Ui, id: egui::Id) -> f32 {
    let key = id.with("wheel");
    let delta = ui.input(|i| i.smooth_scroll_delta.y);
    let mut acc = ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(0.0) + delta;
    // smoothing splits a notch into parts whose sum can miss it by a rounding error
    let notches = (acc / WHEEL_NOTCH + 1e-3 * acc.signum()).trunc();
    acc -= notches * WHEEL_NOTCH;
    ui.data_mut(|d| d.insert_temp(key, acc));
    if delta != 0.0 {
        ui.ctx().input_mut(|i| i.smooth_scroll_delta.y = 0.0);
    }
    notches
}

/// Seek bar over video (OSD colours, not theme): played, buffered (`cache_end`), hover time
/// tooltip. Returns a requested absolute position when clicked/dragged/scrubbed.
///
/// Unknown or zero `duration` (live streams) draws an empty track and never seeks.
pub fn seek_bar(ui: &mut egui::Ui, theme: &Theme, position: f64, duration: f64, cache_end: Option<f64>, focused: bool) -> Option<f64> {
    let t = &theme.tokens;
    let (track_h, knob) = metrics(theme.space(SpaceToken::ControlHeight));
    let seekable = duration.is_finite() && duration > 0.0;
    let sense = if seekable { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(knob * 4.0), theme.space(SpaceToken::ControlHeight)), sense);
    let resp = if seekable { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
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

    fn wheel(points: f32) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, points),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// The OSD volume's slider: a click sets a snapped value, the wheel over it steps by
    /// `wheel_step` per 40 px notch (half a notch waits for the other half), clamped to the
    /// range; the 100 % mark changes nothing about the value.
    #[test]
    fn slider_in_clicks_and_wheel_notches() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        struct S {
            theme: Theme,
            v: f32,
            changes: usize,
        }
        let mut h = Harness::builder().with_size(egui::vec2(300.0, 60.0)).with_step_dt(0.02).build_ui_state(
            |ui, s: &mut S| {
                let rect = Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(260.0, 32.0));
                let spec = SliderSpec {
                    range: 0.0..=130.0,
                    step: 1.0,
                    marks: &[100.0],
                    wheel_step: Some(5.0),
                    label: "Volume",
                    look: SliderLook::osd(&s.theme.tokens),
                };
                if slider_in(ui, rect, egui::Id::new("vol"), &s.theme, &mut s.v, false, &spec).changed() {
                    s.changes += 1;
                }
            },
            S { theme: Theme::fallback(), v: 80.0, changes: 0 },
        );
        h.run_steps(2);
        let r = h.get_by_label("Volume").rect();
        let c = egui::pos2(r.center().x, r.center().y);
        h.hover_at(c);
        h.drag_at(c);
        h.drop_at(c);
        h.run_steps(2);
        assert_eq!(h.state().v, 65.0, "the middle of 0–130");
        h.hover_at(c);
        h.event(wheel(40.0));
        h.run_steps(30);
        assert_eq!(h.state().v, 70.0, "one notch up");
        h.hover_at(c);
        h.event(wheel(-20.0));
        h.run_steps(30);
        assert_eq!(h.state().v, 70.0, "half a notch: nothing yet");
        h.hover_at(c);
        h.event(wheel(-20.0));
        h.run_steps(30);
        assert_eq!(h.state().v, 65.0, "the other half: one step down");
        h.state_mut().v = 128.0;
        h.hover_at(c);
        h.event(wheel(40.0));
        h.run_steps(30);
        assert_eq!(h.state().v, 130.0, "clamped to the range");
        assert_eq!(h.state().changes, 4);
    }
}
