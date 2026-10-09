//! Movies, series, seasons, episodes. FROZEN (skeleton).

use std::sync::Arc;

use super::{EpisodeId, GroupId, ItemKey, MovieId, SeriesId, SourceId, StreamLocator, Timestamp};

#[derive(Debug, Clone, PartialEq)]
pub struct Movie {
    pub id: MovieId,
    pub source_id: SourceId,
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub rename: Option<String>,
    pub year: Option<u16>,
    pub group_ids: Vec<GroupId>,
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    /// 0..10
    pub rating: Option<f32>,
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub duration_s: Option<u32>,
    pub release_date: Option<String>,
    pub tmdb_id: Option<String>,
    pub trailer: Option<String>,
    pub stream: StreamLocator,
    pub added_at: Option<Timestamp>,
    pub provider_order: i32,
    pub hidden: bool,
    pub locked: bool,
    /// `get_vod_info` fetched (details are lazy).
    pub details_loaded_at: Option<Timestamp>,
}

impl Movie {
    pub fn display_name(&self) -> &str { self.rename.as_deref().unwrap_or(&self.clean_name) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub id: SeriesId,
    pub source_id: SourceId,
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub rename: Option<String>,
    pub year: Option<u16>,
    pub group_ids: Vec<GroupId>,
    pub cover: Option<String>,
    pub backdrop: Option<String>,
    pub rating: Option<f32>,
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub release_date: Option<String>,
    pub last_modified: Option<Timestamp>,
    pub added_at: Option<Timestamp>,
    pub provider_order: i32,
    pub hidden: bool,
    pub locked: bool,
    /// `get_series_info` fetched (seasons/episodes are lazy).
    pub details_loaded_at: Option<Timestamp>,
}

impl Series {
    pub fn display_name(&self) -> &str { self.rename.as_deref().unwrap_or(&self.clean_name) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Season {
    pub series_id: SeriesId,
    pub number: u32,
    pub name: Option<String>,
    pub cover: Option<String>,
    pub air_date: Option<String>,
    pub overview: Option<String>,
    pub episode_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub id: EpisodeId,
    pub series_id: SeriesId,
    pub source_id: SourceId,
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub plot: Option<String>,
    pub duration_s: Option<u32>,
    pub still: Option<String>,
    pub air_date: Option<String>,
    pub rating: Option<f32>,
    pub stream: StreamLocator,
}

/// Poster-grid row kept in memory (movies and series share the grid widget).
#[derive(Debug, Clone, PartialEq)]
pub struct VodSummary {
    pub kind: VodKind,
    pub source_id: SourceId,
    pub key: ItemKey,
    pub name: Arc<str>,
    pub year: Option<u16>,
    pub poster: Option<Arc<str>>,
    pub rating: Option<f32>,
    pub group_ids: Vec<GroupId>,
    pub added_at: Option<Timestamp>,
    pub provider_order: i32,
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VodKind {
    Movie(MovieId),
    Series(SeriesId),
}
