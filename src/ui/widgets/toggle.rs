//! ToggleSwitch (square when rounding = 0; checked = selected fill + accent knob). Owner: WP-10a.
//!
//! Neither widget reads keys: in a Settings pane the screen maps `Activate` (Enter/Space) to
//! `*on = !*on` and `Left`/`Right` (h/l) to the previous/next segment; `focused` draws the ring.

use eframe::egui::{self, Align2, Rect};

use super::button::{ButtonStyle, button};
use super::paint;
use crate::theme::tokens::darker;
use crate::theme::{SpaceToken, Theme, TypeToken};

/// Knob travel animation (Omarchy: 110 ms).
const KNOB_ANIM_S: f32 = 0.11;

/// Track (width, height) and knob side for the theme's control height.
fn metrics(theme: &Theme) -> (egui::Vec2, f32) {
    let h = (0.55 * theme.space(SpaceToken::ControlHeight)).max(22.0).round();
    (egui::vec2((h * 1.9).round(), h), (h * 0.72).round())
}

/// Label on the left, switch on the right (the row takes the available width); an empty label
/// gives just the switch. Click anywhere on the row toggles.
pub fn toggle(ui: &mut egui::Ui, theme: &Theme, on: &mut bool, label: &str, focused: bool) -> egui::Response {
    let t = &theme.tokens;
    let (track, knob) = metrics(theme);
    let pad = theme.space(SpaceToken::ControlPaddingX);
    let label_g = (!label.is_empty()).then(|| paint::galley(ui.painter(), label, paint::font(theme, TypeToken::Body), t.fg, f32::INFINITY));
    let width = match &label_g {
        Some(g) => ui.available_width().max(g.size().x + 3.0 * pad + track.x),
        None => track.x,
    };
    let height = theme.space(SpaceToken::ControlHeight).max(track.y);
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    if label_g.is_some() && (focused || resp.hovered()) {
        p.rect_filled(rect, t.rounding, t.fill_hover);
    }
    if focused {
        paint::focus_ring(p, rect, theme);
    }
    let right_pad = if label_g.is_some() { pad } else { 0.0 };
    let track_rect = Rect::from_min_size(egui::pos2(rect.right() - right_pad - track.x, rect.center().y - track.y / 2.0), track);
    p.rect_filled(track_rect, t.rounding, if *on { t.fill_selected } else { t.fill_field });
    let pos = ui.ctx().animate_bool_with_time(resp.id, *on, KNOB_ANIM_S);
    let inset = (track.y - knob) / 2.0;
    let kx = track_rect.left() + inset + pos * (track.x - knob - 2.0 * inset);
    let knob_rect = Rect::from_min_size(egui::pos2(kx, track_rect.top() + inset), egui::vec2(knob, knob));
    p.rect_filled(knob_rect, t.rounding, if *on { t.accent } else { darker(t.fg, 1.25) });
    if let Some(g) = label_g {
        let r = Align2::LEFT_CENTER.anchor_size(egui::pos2(rect.left() + pad, rect.center().y), g.size());
        p.galley(r.min, g, t.fg);
    }
    resp
}

/// Segmented choice (sort mode, live format, sidebar mode): `h/l` or ←/→ change.
pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, theme: &Theme, value: &mut T, options: &[(T, &str)], focused: bool) -> egui::Response {
    let inner = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xxs);
        let mut changed = false;
        for (v, label) in options {
            let selected = *value == *v;
            if button(ui, theme, None, label, ButtonStyle { selected, ..Default::default() }).clicked() && !selected {
                *value = *v;
                changed = true;
            }
        }
        changed
    });
    let mut resp = inner.response;
    if inner.inner {
        resp.mark_changed();
    }
    if focused {
        paint::focus_ring(ui.painter(), resp.rect, theme);
    }
    resp
}
