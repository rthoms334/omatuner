//! Sliders (PanelSlider: track fg@0.18, fill fg, knob grows ×1.15 on hover) and the player
//! seek bar. Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

pub fn slider(ui: &mut egui::Ui, theme: &Theme, value: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, focused: bool) -> egui::Response {
    todo!("WP-10a")
}

/// Seek bar over video (OSD colours, not theme): played, buffered (`cache_end`), hover time
/// tooltip. Returns a requested absolute position when clicked/dragged/scrubbed.
pub fn seek_bar(ui: &mut egui::Ui, theme: &Theme, position: f64, duration: f64, cache_end: Option<f64>, focused: bool) -> Option<f64> {
    todo!("WP-10a")
}
