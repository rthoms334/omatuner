//! Row renderers (Omarchy menu rows: 50 px, 58 with detail; compact density 36 px).
//! Owner: WP-10a.

use eframe::egui;

use super::RowState;
use crate::config::Density;
use crate::images::ImageCache;
use crate::model::{ChannelSummary, Episode, Group, NowNext, ProgrammeSlim, Timestamp};
use crate::store::library::SourceMeta;
use crate::theme::Theme;

/// Row height for a density (+ detail line).
pub fn row_height(theme: &Theme, density: Density, with_detail: bool) -> f32 { todo!("WP-10a") }

/// Icon + label (+ detail at body-small @0.52, + chevron @0.36).
pub fn menu_row(ui: &mut egui::Ui, theme: &Theme, icon: Option<&str>, label: &str, detail: Option<&str>, chevron: bool, state: RowState) -> egui::Response {
    todo!("WP-10a")
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelRowOpts<'a> {
    pub density: Density,
    pub show_number: bool,
    pub show_logo: bool,
    /// Show a source badge (multiple sources / search results).
    pub source: Option<&'a SourceMeta>,
    pub favourite: bool,
    /// Dim + warning glyph (recent failures).
    pub failed: bool,
}

/// `[logo] 101  BBC One                       ▕████░░▏ 20:00–21:00 News at Eight · next: …`
/// Medium/compact layouts drop the progress text then the next programme.
pub fn channel_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    ch: &ChannelSummary,
    now_next: Option<&NowNext>,
    now: Timestamp,
    opts: ChannelRowOpts<'_>,
    state: RowState,
) -> egui::Response {
    todo!("WP-10a")
}

/// Group/category row: name, count, lock glyph.
pub fn group_row(ui: &mut egui::Ui, theme: &Theme, g: &Group, state: RowState) -> egui::Response { todo!("WP-10a") }

/// `20:00  News at Eight` (+ catch-up glyph when available, + live progress).
pub fn programme_row(ui: &mut egui::Ui, theme: &Theme, p: &ProgrammeSlim, now: Timestamp, catchup: bool, state: RowState) -> egui::Response {
    todo!("WP-10a")
}

/// `E05  Title · 42 min` with watched check / progress bar.
pub fn episode_row(ui: &mut egui::Ui, theme: &Theme, e: &Episode, progress: Option<f32>, watched: bool, state: RowState) -> egui::Response {
    todo!("WP-10a")
}
