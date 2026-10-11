//! The guard snapshot: matcher, rules, embargo windows per topic, series boundaries, VOD hits
//! (DESIGN §34 §4). Built on the tokio blocking pool (debounced 1 s) on `DataScope::{Epg, Live,
//! Vod, Progress, Spoiler, Follow}`, a config change and an hourly tick. OWNER: S1.

use crate::model::Timestamp;

/// An embargo window of one event (binary-searched by time; no rebuild as time passes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub open: Timestamp,
    pub close: Timestamp,
    pub event_key: String,
}

/// The built snapshot (≤ 300 KB).
#[derive(Debug, Clone, Default)]
pub struct GuardSnapshot {
    pub generation: u64,
    pub on: bool,
    pub built_at: Timestamp,
}

/// Build a snapshot from the rules, Following's spoiler-safe rules (virtual topics, B4), the
/// guide and merged series progress. Placeholder until S1: an empty, "off" snapshot.
pub fn build(
    _conn: &rusqlite::Connection,
    _cfg: &crate::config::Config,
    _lib: &crate::store::library::Library,
    now: Timestamp,
) -> crate::error::Result<GuardSnapshot> {
    Ok(GuardSnapshot { built_at: now, ..Default::default() })
}
