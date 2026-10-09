//! Screens (one file = one owner). Wireframes and behaviour: DESIGN §13. In the skeleton each
//! screen draws a placeholder and handles no keys; the owning WP replaces the bodies.

pub mod epg_mapping;
pub mod guide;
pub mod home;
pub mod live;
pub mod logs;
pub mod movie_detail;
pub mod movies;
pub mod onboarding;
pub mod player;
pub mod series;
pub mod series_detail;
pub mod settings;

use eframe::egui;

use super::{Screen, ScreenId, UiCtx};

/// Instantiate every screen (the app keeps them alive for the session).
pub fn all() -> Vec<Box<dyn Screen>> {
    vec![
        Box::new(home::HomeScreen::default()),
        Box::new(live::LiveScreen::default()),
        Box::new(guide::GuideScreen::default()),
        Box::new(movies::MoviesScreen::default()),
        Box::new(series::SeriesScreen::default()),
        Box::new(movie_detail::MovieDetailScreen::default()),
        Box::new(series_detail::SeriesDetailScreen::default()),
        Box::new(player::PlayerScreen::default()),
        Box::new(settings::SettingsScreen::default()),
        Box::new(onboarding::OnboardingScreen::default()),
        Box::new(epg_mapping::EpgMappingScreen::default()),
        Box::new(logs::LogsScreen::default()),
    ]
}

/// Skeleton placeholder body (removed by each WP).
pub(crate) fn placeholder(ui: &mut egui::Ui, ctx: &UiCtx<'_>, glyph: &str, title: &str, owner: &str) {
    super::widgets::empty_state::empty_state(ui, ctx.theme, glyph, title, Some(&format!("Not implemented yet — {owner}")), None);
}
