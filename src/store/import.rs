//! Catalogue (live/VOD/series) import sink: batched upserts with mark-and-sweep generations and
//! user-data reattach. Owner: WP-02. Driven by `jobs::import_source` (WP-06).
//!
//! Protocol (DESIGN §9.2):
//! ```text
//! let imp = begin_catalogue(conn, source)?;                // generation = source.generation + 1
//! upsert_groups(conn, &imp, &groups)?;                     // small, one tx
//! for batch in channels.chunks(2000) { write_channels(conn, &imp, batch)?; }   // one tx each
//! ... movies / series likewise ...
//! let summary = finish_catalogue(conn, &imp, &completed_kinds, now)?;   // one tx: sweep +
//!                                     // reattach + counts + generation bump + resolve EPG
//! ```
//! Rows are upserted on `(source_id, key)` so unchanged items keep their row id. A kind that did
//! not complete (network error mid-way) is NOT swept, so a failed refresh never empties a list.
//! If the job is cancelled, nothing is swept and `source.generation` is not bumped; rows already
//! upserted simply carry the newer data.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    CatchupSpec, EpgMatchMethod, GroupKey, HttpHeaders, ItemKey, ItemKind, ReattachOutcome, SourceId, StreamLocator,
    Timestamp, Unsupported,
};

/// An in-progress catalogue import.
#[derive(Debug, Clone)]
pub struct CatalogueImport {
    pub source: SourceId,
    pub generation: i64,
    pub started_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportGroup {
    pub kind: ItemKind,
    pub key: GroupKey,
    pub name: String,
    pub provider_id: Option<String>,
    pub provider_order: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportChannel {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub number: Option<u32>,
    pub logo: Option<String>,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    pub tvg_shift_min: i32,
    pub stream: StreamLocator,
    pub url_key: Option<String>,
    pub headers: Option<HttpHeaders>,
    pub catchup: Option<CatchupSpec>,
    pub radio: bool,
    pub unsupported: Option<Unsupported>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    /// Group keys, primary first (must have been passed to `upsert_groups`).
    pub groups: Vec<GroupKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportMovie {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub year: Option<u16>,
    pub poster: Option<String>,
    pub rating: Option<f32>,
    pub stream: StreamLocator,
    pub url_key: Option<String>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    pub groups: Vec<GroupKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSeries {
    pub key: ItemKey,
    pub provider_id: Option<String>,
    pub name: String,
    pub clean_name: String,
    pub norm_name: String,
    pub year: Option<u16>,
    pub cover: Option<String>,
    pub backdrop: Option<String>,
    pub rating: Option<f32>,
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub release_date: Option<String>,
    pub last_modified: Option<Timestamp>,
    pub provider_order: i32,
    pub added_at: Option<Timestamp>,
    pub groups: Vec<GroupKey>,
}

/// Lazy movie details (`get_vod_info`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MovieDetails {
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub cast: Option<String>,
    pub director: Option<String>,
    pub duration_s: Option<u32>,
    pub release_date: Option<String>,
    pub tmdb_id: Option<String>,
    pub trailer: Option<String>,
    pub backdrop: Option<String>,
    pub rating: Option<f32>,
    pub poster: Option<String>,
    /// Container extension from `movie_data` (overrides the list's when present).
    pub container_ext: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSeason {
    pub number: u32,
    pub name: Option<String>,
    pub cover: Option<String>,
    pub air_date: Option<String>,
    pub overview: Option<String>,
    pub episode_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportEpisode {
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

/// Lazy series details (`get_series_info`). Seasons may be empty while episodes exist; the
/// store synthesizes seasons from episodes in that case.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeriesDetails {
    pub seasons: Vec<ImportSeason>,
    pub episodes: Vec<ImportEpisode>,
    pub plot: Option<String>,
    pub backdrop: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KindCounts {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImportSummary {
    pub channels: KindCounts,
    pub movies: KindCounts,
    pub series: KindCounts,
    pub groups: KindCounts,
    pub reattached: u32,
    pub orphaned: u32,
}

/// Start an import: computes the next generation; nothing is written yet.
pub fn begin_catalogue(conn: &Connection, source: SourceId, now: Timestamp) -> Result<CatalogueImport> {
    todo!("WP-02")
}

/// Upsert all groups of the import (marks `seen_gen`). One transaction.
pub fn upsert_groups(conn: &mut Connection, imp: &CatalogueImport, groups: &[ImportGroup]) -> Result<()> {
    todo!("WP-02")
}

/// Upsert one batch of channels in ONE transaction (prepared/cached statements), replacing
/// their `channel_group` rows. Returns (added, updated).
pub fn write_channels(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportChannel]) -> Result<(u32, u32)> {
    todo!("WP-02")
}

pub fn write_movies(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportMovie]) -> Result<(u32, u32)> {
    todo!("WP-02")
}

pub fn write_series(conn: &mut Connection, imp: &CatalogueImport, batch: &[ImportSeries]) -> Result<(u32, u32)> {
    todo!("WP-02")
}

/// Finish (one transaction): for each kind in `completed`, delete rows with `seen_gen < gen`
/// (and groups likewise); recompute group `item_count`; run [`reattach_user_data`]; update
/// source counts and `generation = gen`; call `store::epg::resolve_mappings` for the source.
pub fn finish_catalogue(
    conn: &mut Connection,
    imp: &CatalogueImport,
    completed: &[ItemKind],
    now: Timestamp,
) -> Result<ImportSummary> {
    todo!("WP-02")
}

/// Re-attach favourites / overrides / progress / reminders / manual EPG mappings of `source`
/// (every table with `fp` + `orphaned_since`) whose item key no longer exists (DESIGN §8.3).
/// Candidate lookups use indexes (`channel_provider`, `channel_tvg`, `channel_norm`,
/// `channel_urlkey`, movie equivalents) or HashMaps built once per call. Steps, first unique
/// hit wins:
/// 1. same `provider_id` (Xtream renumbering keeps ids rarely, but cheap to check)
/// 2. same `tvg_id` AND same `norm_name`
/// 3. same `norm_name` AND same `norm_group`
/// 4. same `url_key`
/// 5. same `norm_name` alone, if unique in the source
///
/// Ambiguous or no hit → `orphaned_since = now` (kept, listed in Settings ▸ Orphans).
/// A previously orphaned row that matches again is re-attached and un-orphaned.
pub fn reattach_user_data(conn: &Connection, source: SourceId, now: Timestamp) -> Result<Vec<(ItemKey, ReattachOutcome)>> {
    todo!("WP-02")
}

/// Store lazily fetched movie details.
pub fn store_movie_details(conn: &Connection, movie: crate::model::MovieId, d: &MovieDetails, now: Timestamp) -> Result<()> {
    todo!("WP-02")
}

/// Replace seasons + episodes of a series (upsert episodes by key, delete missing).
pub fn store_series_details(
    conn: &mut Connection,
    series: crate::model::SeriesId,
    d: &SeriesDetails,
    now: Timestamp,
) -> Result<()> {
    todo!("WP-02")
}
