//! Poster cards and the 2-D virtualized poster grid (Movies / Series / Home rails).
//! Owner: WP-12 (VOD screens are its main user).
//!
//! Card: 2:3 poster (Poster image kind), title below (2 lines max, body), year + rating
//! (body-small @0.52), progress bar along the bottom edge (accent) when partially watched,
//! lock glyph when locked. Cursor = 2 px accent outline (posters have no row background).

use eframe::egui;

use super::list::ListOutcome;
use crate::images::ImageCache;
use crate::keymap::Action;
use crate::model::VodSummary;
use crate::theme::Theme;

#[derive(Default)]
pub struct GridState {
    pub cursor: usize,
    pub len: usize,
    pub columns: usize,
    pub filter: crate::search::ListFilter,
    scroll_to_cursor: bool,
}

impl GridState {
    pub fn set_len(&mut self, len: usize) { todo!("WP-12") }
    pub fn handle(&mut self, action: Action) -> ListOutcome { todo!("WP-12") }
}

pub fn poster_card(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    item: &VodSummary,
    width: f32,
    cursor: bool,
    progress: Option<f32>,
) -> egui::Response {
    todo!("WP-12")
}

/// Grid of `items` (indices into `all`), column count from available width (min card 132 pt
/// Medium, 150 pt Wide), rows virtualized with `show_rows`.
pub fn poster_grid(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    state: &mut GridState,
    all: &[VodSummary],
    items: &[u32],
) -> ListOutcome {
    todo!("WP-12")
}

/// Horizontal rail (Home sections): ←/→ move, Enter opens.
pub fn poster_rail(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    id_salt: impl std::hash::Hash,
    all: &[VodSummary],
    cursor: Option<usize>,
) -> Option<usize> {
    todo!("WP-12")
}
