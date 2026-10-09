//! Text input (padding 10×7, focus > hover > normal fills, placeholder darker(fg,1.6), selection
//! fg@0.35) and the launcher "header is the search field" idiom. Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

/// Standard field. `password` masks with ●. Returns the egui response (`.changed()`).
#[allow(clippy::ptr_arg)] // TextEdit needs a String buffer (stub body)
pub fn text_field(ui: &mut egui::Ui, theme: &Theme, text: &mut String, placeholder: &str, password: bool) -> egui::Response {
    todo!("WP-10a")
}

/// Launcher header: shows `query` at heading size, or `placeholder…` at opacity 0.58 when
/// empty, with a blinking accent caret while typing. Not an egui TextEdit — keys arrive as
/// `Action::FilterInput` through the keymap.
pub fn search_header(ui: &mut egui::Ui, theme: &Theme, query: &str, placeholder: &str, icon: Option<&str>) -> egui::Response {
    todo!("WP-10a")
}
