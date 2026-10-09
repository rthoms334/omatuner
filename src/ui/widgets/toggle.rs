//! ToggleSwitch (square when rounding = 0; checked = selected fill + accent knob). Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

pub fn toggle(ui: &mut egui::Ui, theme: &Theme, on: &mut bool, label: &str, focused: bool) -> egui::Response { todo!("WP-10a") }

/// Segmented choice (sort mode, live format, sidebar mode): `h/l` or ←/→ change.
pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, theme: &Theme, value: &mut T, options: &[(T, &str)], focused: bool) -> egui::Response {
    todo!("WP-10a")
}
