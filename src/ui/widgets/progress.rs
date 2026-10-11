//! Progress bars: programme progress (thin, fg@0.18 track, fg fill), job progress, poster
//! watch progress (accent). Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

/// Period of the indeterminate bar's sweep.
const BUSY_PERIOD_S: f64 = 1.0;
/// Width of the moving block, as a fraction of the track.
const BUSY_BLOCK: f32 = 0.3;

pub fn progress_bar(ui: &mut egui::Ui, theme: &Theme, fraction: f32, height: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height.max(1.0)), egui::Sense::hover());
    let f = clamp_fraction(fraction);
    resp.widget_info(|| egui::WidgetInfo::slider(true, f as f64, "progress"));
    if ui.is_rect_visible(rect) {
        paint_progress(ui.painter(), rect, theme, f, false);
    }
    resp
}

/// Paint-only variant into an existing rect.
pub fn paint_progress(painter: &egui::Painter, rect: egui::Rect, theme: &Theme, fraction: f32, accent: bool) {
    let t = &theme.tokens;
    painter.rect_filled(rect, t.rounding, t.fill_selected);
    let f = clamp_fraction(fraction);
    if f > 0.0 {
        let fill = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + rect.width() * f, rect.bottom()));
        painter.rect_filled(fill, t.rounding, if accent { t.accent } else { t.fg });
    }
}

/// Indeterminate (unknown total) — a moving block, 1 s period; requests repaint while visible.
pub fn busy_bar(ui: &mut egui::Ui, theme: &Theme, height: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height.max(1.0)), egui::Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ProgressIndicator, true, "busy"));
    if ui.is_rect_visible(rect) {
        let t = &theme.tokens;
        let painter = ui.painter();
        painter.rect_filled(rect, t.rounding, t.fill_selected);
        let phase = busy_phase(ui.input(|i| i.time));
        let block = rect.width() * BUSY_BLOCK;
        // the block enters from the left edge and leaves on the right (clipped to the track)
        let x = rect.left() - block + (rect.width() + block) * phase;
        let r = egui::Rect::from_min_max(egui::pos2(x.max(rect.left()), rect.top()), egui::pos2((x + block).min(rect.right()), rect.bottom()));
        if r.width() > 0.0 {
            painter.rect_filled(r, t.rounding, t.fg);
        }
        // ~30 fps is plenty for a status indicator (keeps an idle window cheap)
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    resp
}

fn clamp_fraction(f: f32) -> f32 {
    if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 }
}

/// 0..1 position of the busy block at time `t` (seconds).
fn busy_phase(t: f64) -> f32 {
    ((t / BUSY_PERIOD_S).fract()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_are_sanitized() {
        assert_eq!(clamp_fraction(f32::NAN), 0.0);
        assert_eq!(clamp_fraction(-1.0), 0.0);
        assert_eq!(clamp_fraction(2.0), 1.0);
        assert_eq!(clamp_fraction(0.25), 0.25);
    }

    #[test]
    fn busy_phase_wraps_every_period() {
        assert!((busy_phase(0.25) - 0.25).abs() < 1e-6);
        assert!((busy_phase(3.25) - 0.25).abs() < 1e-6);
    }
}
