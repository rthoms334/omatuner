//! Channel logo / monogram (fit into a square, aspect kept, transparent bg; monogram = first
//! letters on fg@0.04 with fg@0.52 text when missing/failed). Owner: WP-10a.

use eframe::egui;

use crate::images::ImageCache;
use crate::theme::Theme;

pub fn logo(ui: &mut egui::Ui, theme: &Theme, images: &mut ImageCache, url: Option<&str>, name: &str, size: f32) -> egui::Response {
    todo!("WP-10a")
}

/// Monogram text for a name: "BBC One" → "BO", "CNN" → "CN", digits kept ("ESPN 2" → "E2").
pub fn monogram(name: &str) -> String { todo!("WP-10a") }
