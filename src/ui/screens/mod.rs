//! Screens (one file = one owner). Wireframes and behaviour: DESIGN §13.

pub(crate) mod copies;
pub mod epg_mapping;
pub(crate) mod group_marks;
pub mod guide;
pub mod home;
pub mod live;
pub mod logs;
pub mod movie_detail;
pub mod movies;
pub mod onboarding;
pub mod player;
pub mod queue;
pub mod series;
pub mod series_detail;
pub mod settings;
pub mod watchlists;

use super::Screen;

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
        Box::new(queue::QueueScreen::default()),
        Box::new(watchlists::WatchlistsScreen::default()),
    ]
}
