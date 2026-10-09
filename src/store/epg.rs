//! EPG storage: generation-based bulk import, grid/now-next queries, FTS search, mappings.
//! Owner: WP-02.
//!
//! Import protocol (DESIGN §9.3):
//! ```text
//! let imp = begin_epg_import(conn, id)?;            // generation = active_gen + 1, offset captured
//! write_epg_channels(conn, &imp, &batch)?;          // upsert (source, xmltv_id) ON CONFLICT DO
//!                                                    //   UPDATE (row ids stable), display names
//! write_programmes(conn, &imp, &batch)?;            // ~5 000 rows / tx: programmes + their FTS
//!                                                    //   rows in the SAME tx; unknown channel
//!                                                    //   ids auto-created
//! let s = finish_epg_import(conn, &imp, now)?;      // SMALL tx: flip active_gen, counts,
//!                                                    //   max_stop, resolve mappings
//! while sweep_old_generations(conn, id, 20_000)? > 0 {}   // FTS rows by rowid, then programmes,
//!                                                    //   then epg channels unseen in active gen
//! ```
//! Readers always filter `programme.gen = epg_source.active_gen`, so the flip is atomic for
//! the UI and an aborted import ([`abort_epg_import`]) leaves the old guide intact.
//!
//! FTS contract (DESIGN §6.3): `programme_fts` is contentless with `contentless_delete=1`,
//! keyed by `programme.id` (AUTOINCREMENT, never reused). Every programme delete — sweep, prune,
//! abort, EPG-source/source removal — deletes the FTS rows by rowid FIRST, in the same
//! transaction, in chunks. `programme.epg_channel_id` has no ON DELETE CASCADE, so forgetting
//! this fails loudly. No transaction may exceed one batch (~20 ms) so user writes stay snappy.
//!
//! Times: every read returns EPG time (source offset applied, channel tvg-shift NOT applied);
//! callers apply `ProgrammeSlim::shifted` once. Exception: [`now_next`] takes the shifts.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    ChannelId, EpgChannel, EpgChannelId, EpgMapping, EpgSourceId, EpisodeNum, ItemRef, NowNext, Programme, ProgrammeHit,
    ProgrammeId, ProgrammeSlim, SourceId, Timestamp,
};

#[derive(Debug, Clone)]
pub struct EpgImport {
    pub epg_source: EpgSourceId,
    pub generation: i64,
    /// Source offset in seconds, added to every programme time at write.
    pub offset_s: i64,
    /// xmltv_id → epg_channel.id, filled as channels are written.
    pub channel_ids: HashMap<String, EpgChannelId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportEpgChannel {
    pub xmltv_id: String,
    /// (display name, `identity::normalize_name(display name)`) — the norm is computed by the
    /// importer (WP-06), so the store never calls into `sources::`.
    pub display_names: Vec<(String, String)>,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportProgramme {
    pub xmltv_id: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub icon: Option<String>,
    pub episode: Option<EpisodeNum>,
    pub rating: Option<String>,
    pub star_rating: Option<String>,
    pub catchup_id: Option<String>,
    pub is_new: bool,
    pub is_live: bool,
    pub previously_shown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpgImportSummary {
    pub channels: u32,
    pub programmes: u32,
    pub max_stop: Option<Timestamp>,
    pub mapped_channels: u32,
}

/// Matching input row for one playlist channel (see `sources::epg_match`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMatchRow {
    pub channel: ItemRef,
    pub tvg_id: Option<String>,
    pub name: String,
    pub norm_name: String,
}

// ------------------------------------------------------------------ import

pub fn begin_epg_import(conn: &Connection, id: EpgSourceId) -> Result<EpgImport> { todo!("WP-02") }

/// Upsert channels (`ON CONFLICT(epg_source_id, xmltv_id) DO UPDATE` — never REPLACE: that
/// would cascade the live guide away) + replace their display names; sets `seen_gen`.
pub fn write_epg_channels(conn: &mut Connection, imp: &mut EpgImport, batch: &[ImportEpgChannel]) -> Result<()> {
    todo!("WP-02")
}

/// Insert one batch of programmes for `imp.generation` in one transaction, together with their
/// `programme_fts` rows. `INSERT … ON CONFLICT(epg_channel_id, gen, start) DO NOTHING`:
/// duplicate (channel, start) inside a generation → FIRST wins (only inserted rows get FTS
/// rows). Returns rows inserted.
pub fn write_programmes(conn: &mut Connection, imp: &mut EpgImport, batch: &[ImportProgramme]) -> Result<usize> {
    todo!("WP-02")
}

/// One SHORT transaction: set `active_gen`, counts, `max_stop`, `last_refresh`, then
/// [`resolve_mappings`] (which only considers epg channels with `seen_gen = active_gen`).
/// FTS rows already exist (written per batch); unseen channels are removed by
/// [`sweep_old_generations`] once their programmes are gone.
pub fn finish_epg_import(conn: &mut Connection, imp: &EpgImport, now: Timestamp) -> Result<EpgImportSummary> {
    todo!("WP-02")
}

/// Delete everything written for `imp.generation` (cancel/failure): FTS rows by rowid, then
/// programmes, chunked (one tx per chunk).
pub fn abort_epg_import(conn: &mut Connection, imp: &EpgImport) -> Result<()> { todo!("WP-02") }

/// Delete up to `chunk` programme rows of inactive generations of `id` (FTS rows by rowid
/// first, same tx). When none remain, deletes epg channels of `id` not seen in the active
/// generation (their `channel_epg` rows cascade). Returns rows deleted; call until 0.
pub fn sweep_old_generations(conn: &mut Connection, id: EpgSourceId, chunk: usize) -> Result<usize> {
    todo!("WP-02")
}

/// Delete programmes with `stop < before` (keeps catch-up windows: caller passes
/// now − max(keep_past_days, max catch-up days)). FTS rows first. At most `chunk` rows per call
/// (one tx); returns rows deleted — call until 0.
pub fn prune(conn: &mut Connection, before: Timestamp, chunk: usize) -> Result<usize> { todo!("WP-02") }

/// Which programmes [`delete_programmes_of`] removes.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgrammeScope {
    /// Before deleting an EPG source.
    EpgSource(EpgSourceId),
    /// Before deleting individual epg channels.
    EpgChannels(Vec<EpgChannelId>),
    /// Before deleting a playlist source (its provider EPG sources).
    PlaylistSource(SourceId),
}

/// Delete up to `chunk` programmes (FTS rows first) in `scope`. Call until 0, then delete the
/// parent rows. Required before any delete of `epg_channel`, `epg_source` or `source`.
pub fn delete_programmes_of(conn: &mut Connection, scope: &ProgrammeScope, chunk: usize) -> Result<usize> {
    todo!("WP-02")
}

/// Maintenance safety net: `DELETE FROM programme_fts WHERE rowid NOT IN (SELECT id FROM
/// programme)`. Returns rows removed (should be 0).
pub fn fts_cleanup(conn: &Connection) -> Result<usize> { todo!("WP-02") }

// ------------------------------------------------------------------ queries (UI thread safe)

/// Programmes of `ch` (active generation) overlapping `[from, to)` (EPG time), ordered by
/// start. Uses `programme_grid`: `start >= from - 86400 AND start < to AND stop > from`.
/// Must stay < 1 ms for a 6 h window.
pub fn window(conn: &Connection, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>> {
    todo!("WP-02")
}

/// Now/next for each (epg channel, tvg-shift minutes) at `now`: queries at `now − shift·60`
/// and returns programmes ALREADY SHIFTED (two "+1" channels sharing a guide channel get
/// different answers). One prepared statement executed per key.
pub fn now_next(conn: &Connection, chans: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
    todo!("WP-02")
}

/// Next `limit` programmes starting at or after `from` (EPG time; Live preview "Upcoming").
pub fn upcoming(conn: &Connection, ch: EpgChannelId, from: Timestamp, limit: usize) -> Result<Vec<ProgrammeSlim>> {
    todo!("WP-02")
}

/// Full programme (details popover).
pub fn programme(conn: &Connection, id: ProgrammeId) -> Result<Programme> { todo!("WP-02") }

/// FTS search over title/sub_title of active generations (`JOIN programme p ON p.id = f.rowid
/// … AND p.gen = es.active_gen`). `query` is user text; the function
/// builds a safe FTS5 expression (each token quoted, last token prefix `*`). Results: airing now
/// first, then upcoming by start, then past (catch-up) by recency; each joined to the playlist
/// channels mapped to its epg channel via `channel_epg`.
pub fn search(conn: &Connection, query: &str, now: Timestamp, limit: usize) -> Result<Vec<ProgrammeHit>> {
    todo!("WP-02")
}

/// The epg channel resolved for a playlist channel (from `channel_epg`).
pub fn epg_channel_for(conn: &Connection, ch: ChannelId) -> Result<Option<EpgChannelId>> { todo!("WP-02") }

pub fn epg_channel(conn: &Connection, id: EpgChannelId) -> Result<EpgChannel> { todo!("WP-02") }

// ------------------------------------------------------------------ mappings

/// All epg channels of a source with display names (input to the matcher).
pub fn epg_channels(conn: &Connection, id: EpgSourceId) -> Result<Vec<EpgChannel>> { todo!("WP-02") }

/// Matching input for every channel of a playlist source.
pub fn channels_for_matching(conn: &Connection, source: SourceId) -> Result<Vec<ChannelMatchRow>> { todo!("WP-02") }

/// Replace automatic mappings for (source, epg_source); manual rows are preserved.
pub fn replace_auto_mappings(
    conn: &mut Connection,
    source: SourceId,
    epg_source: EpgSourceId,
    maps: &[EpgMapping],
) -> Result<()> {
    todo!("WP-02")
}

/// Manual assignment from the EPG mapping screen (method = Manual; wins over everything). The
/// channel's fingerprint is computed inside (`user::fingerprint_of`) so manual mappings reattach
/// like other user data.
pub fn set_manual_mapping(conn: &Connection, channel: &ItemRef, epg_source: EpgSourceId, xmltv_id: &str) -> Result<()> {
    todo!("WP-02")
}

/// Remove the manual mapping for a channel (automatic ones re-apply on next resolve).
pub fn clear_manual_mapping(conn: &Connection, channel: &ItemRef) -> Result<()> { todo!("WP-02") }

pub fn mappings_for_channel(conn: &Connection, channel: &ItemRef) -> Result<Vec<EpgMapping>> { todo!("WP-02") }

/// Rebuild `channel_epg`: for each channel, the mapping with (Manual first, then lowest
/// epg_source.priority, then best method) whose epg channel exists in an enabled source with
/// `epg_channel.seen_gen = epg_source.active_gen`.
/// Pure SQL; < 200 ms for 100k channels.
pub fn resolve_mappings(conn: &Connection) -> Result<()> { todo!("WP-02") }
