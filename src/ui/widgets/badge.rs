//! Badges: source (glyph in source colour + name), quality (HD/FHD/4K/HDR), LIVE (urgent),
//! CATCH-UP, NEW, counts. Text at caption size, 1 px border in the badge colour @0.6.
//! Owner: WP-10a.

use eframe::egui;

use crate::store::library::SourceMeta;
use crate::theme::Theme;

pub fn badge(ui: &mut egui::Ui, theme: &Theme, text: &str, color: egui::Color32) -> egui::Response { todo!("WP-10a") }

pub fn source_badge(ui: &mut egui::Ui, theme: &Theme, src: &SourceMeta, compact: bool) -> egui::Response { todo!("WP-10a") }

pub fn live_badge(ui: &mut egui::Ui, theme: &Theme) -> egui::Response { todo!("WP-10a") }
