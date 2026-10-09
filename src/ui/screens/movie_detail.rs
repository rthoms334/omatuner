//! Movie detail (DESIGN §13.6): backdrop (dimmed), poster, title, year · duration · rating ·
//! genre, plot, cast/director, buttons Play/Resume · Start over · Favourite · Mark watched.
//! Loads details lazily (`JobKind::LoadMovieDetails`) with a skeleton state. Key context
//! `[Detail]`. When a movie ends it is marked watched and the player returns here. Owner: WP-12.

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::ui::widgets::glyphs;
use crate::ui::{Route, Screen, ScreenId, UiCtx};

#[derive(Default)]
pub struct MovieDetailScreen {
    // WP-12: screen state (private; free to reshape)
}

impl Screen for MovieDetailScreen {
    fn id(&self) -> ScreenId { ScreenId::MovieDetail }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Detail] }
    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {}
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { false }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { super::placeholder(ui, ctx, glyphs::MOVIES, "Movie", "WP-12") }
}
