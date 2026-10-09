//! Playlist channel ↔ EPG channel matching. Owner: WP-05a. DESIGN §9.4.
//!
//! Automatic steps, first hit wins (only steps 1–4 are applied automatically; step 5 only
//! *suggests*):
//! 1. `tvg-id` == `xmltv_id` (exact), then case-insensitive.
//! 2. Xtream `epg_channel_id` against the same source's provider guide (same as 1, but tagged
//!    `XtreamEpgId`).
//! 3. iptv-org ids: strip `@Feed` (`CNN.us@SD` → `CNN.us`) on either side and retry.
//! 4. `normalize_name(tvg-name or name)` equals a normalized `display-name` — only if exactly one
//!    epg channel in that source has that norm (ambiguity → no match) and
//!    `config.epg.auto_name_match` is on.
//! 5. Fuzzy (Jaro-Winkler on norms, ≥ `config.epg.fuzzy_threshold`) → [`suggest`] for the
//!    "Assign EPG" picker; the user's choice is stored as `Manual`.
//!
//! Manual mappings are never touched by automatic matching.

use std::collections::HashMap;

use crate::model::{EpgChannel, EpgChannelId, EpgMapping, EpgSourceId, EpgSuggestion};
use crate::store::epg::ChannelMatchRow;

/// Lookup structures for one EPG source.
#[derive(Debug, Default)]
pub struct EpgCandidateIndex {
    pub epg_source: Option<EpgSourceId>,
    by_id: HashMap<String, (EpgChannelId, String)>,
    by_id_lower: HashMap<String, Vec<(EpgChannelId, String)>>,
    by_id_feedless: HashMap<String, Vec<(EpgChannelId, String)>>,
    by_norm: HashMap<String, Vec<(EpgChannelId, String)>>,
    /// (norm, epg channel id, xmltv id, display name) for fuzzy scans.
    norms: Vec<(String, EpgChannelId, String, String)>,
}

/// Build the index for `channels` of `epg_source`.
pub fn build_index(epg_source: EpgSourceId, channels: &[EpgChannel]) -> EpgCandidateIndex { todo!("WP-05a") }

/// Automatic match for one channel. `provider_guide` = this EPG source is the provider guide of
/// the channel's own playlist source (enables step 2 tagging).
pub fn match_channel(idx: &EpgCandidateIndex, row: &ChannelMatchRow, provider_guide: bool, name_match: bool) -> Option<EpgMapping> {
    todo!("WP-05a")
}

/// Match all rows (parallel-safe; pure). Returns only matched rows.
pub fn match_all(idx: &EpgCandidateIndex, rows: &[ChannelMatchRow], provider_guide: bool, name_match: bool) -> Vec<EpgMapping> {
    todo!("WP-05a")
}

/// Top `limit` fuzzy candidates for `name` with score ≥ `threshold`, best first.
pub fn suggest(idx: &EpgCandidateIndex, name: &str, limit: usize, threshold: f64) -> Vec<EpgSuggestion> { todo!("WP-05a") }

/// `CNN.us@SD` → `CNN.us`.
pub fn strip_feed(id: &str) -> &str { id.split_once('@').map(|(a, _)| a).unwrap_or(id) }
