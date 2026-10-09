//! Buttons (Omarchy Button.qml): borderless at rest, padding 10×6, icon/text gap 8, selected =
//! accent + bold; reserves border width so hover never shifts layout. Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, Default)]
pub struct ButtonStyle {
    pub bordered: bool,
    pub selected: bool,
    /// Destructive: text in `urgent`.
    pub danger: bool,
    /// Primary call-to-action: accent text, bordered.
    pub primary: bool,
    /// Keyboard focus ring.
    pub focused: bool,
}

pub fn button(ui: &mut egui::Ui, theme: &Theme, icon: Option<&str>, label: &str, style: ButtonStyle) -> egui::Response { todo!("WP-10a") }

/// Icon-only button with tooltip (400 ms delay, body-small).
pub fn icon_button(ui: &mut egui::Ui, theme: &Theme, icon: &str, tooltip: &str, style: ButtonStyle) -> egui::Response { todo!("WP-10a") }
