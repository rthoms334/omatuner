//! Global search overlay (`/`, Ctrl+K): launcher card, header is the query, sections
//! All · Live · Movies · Series · Guide (Tab cycles), results grouped by kind with source
//! badges (colour + glyph), match highlighting, live now/next on channel hits. Enter plays /
//! opens; Shift+Enter plays fullscreen / opens details. Owner: WP-12. DESIGN §13.7.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::search::{EpgSearch, SearchIndex, SearchSection};
use crate::ui::UiCtx;

pub struct SearchOverlay {
    query: String,
    section: SearchSection,
    cursor: usize,
    index: Option<SearchIndex>,
    epg: Option<EpgSearch>,
}

impl SearchOverlay {
    pub fn new(initial: String) -> Self { Self { query: initial, section: SearchSection::All, cursor: 0, index: None, epg: None } }
}

impl Overlay for SearchOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Search] }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome { super::placeholder_action(action) }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome { super::placeholder_ui(egui_ctx, ctx, "Search", "WP-12") }
}
