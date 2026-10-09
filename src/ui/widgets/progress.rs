//! Progress bars: programme progress (thin, fg@0.18 track, fg fill), job progress, poster
//! watch progress (accent). Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

pub fn progress_bar(ui: &mut egui::Ui, theme: &Theme, fraction: f32, height: f32) -> egui::Response { todo!("WP-10a") }

/// Paint-only variant into an existing rect.
pub fn paint_progress(painter: &egui::Painter, rect: egui::Rect, theme: &Theme, fraction: f32, accent: bool) { todo!("WP-10a") }

/// Indeterminate (unknown total) — a moving block, 1 s period; requests repaint.
pub fn busy_bar(ui: &mut egui::Ui, theme: &Theme, height: f32) -> egui::Response { todo!("WP-10a") }
