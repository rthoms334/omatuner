//! Movie / series / episode reads. Owner: WP-02.

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{Episode, EpisodeId, ItemRef, Movie, MovieId, Season, Series, SeriesId, VodSummary};

pub fn movie(conn: &Connection, id: MovieId) -> Result<Movie> { todo!("WP-02") }

pub fn series(conn: &Connection, id: SeriesId) -> Result<Series> { todo!("WP-02") }

/// Seasons ordered by number (synthesized from episodes when the provider sent none).
pub fn seasons(conn: &Connection, id: SeriesId) -> Result<Vec<Season>> { todo!("WP-02") }

/// Episodes of a series, optionally one season, ordered (season, episode).
pub fn episodes(conn: &Connection, id: SeriesId, season: Option<u32>) -> Result<Vec<Episode>> { todo!("WP-02") }

pub fn episode(conn: &Connection, id: EpisodeId) -> Result<Episode> { todo!("WP-02") }

/// The episode to open the series page at (UHF users: "open at the current episode"):
/// the most recently progressed unfinished episode, else the first episode after the last
/// finished one, else S1E1.
pub fn next_episode(conn: &Connection, id: SeriesId) -> Result<Option<Episode>> { todo!("WP-02") }

/// Most recently added (Home "Recently added").
pub fn recently_added(conn: &Connection, movies: bool, limit: usize) -> Result<Vec<VodSummary>> { todo!("WP-02") }

/// Resolve stable refs (Continue Watching rows) to current ids.
pub fn resolve_movie(conn: &Connection, item: &ItemRef) -> Result<Option<MovieId>> { todo!("WP-02") }
pub fn resolve_episode(conn: &Connection, item: &ItemRef) -> Result<Option<EpisodeId>> { todo!("WP-02") }
