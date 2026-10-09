//! Live channel/group reads + failure cache. Owner: WP-02.
//! (Lists are served from the in-memory `library::LiveIndex`; these are point lookups.)

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{Channel, ChannelId, Group, GroupId, ItemKind, ItemRef, SourceId, Timestamp};

/// Full channel row with overrides (rename/hidden/locked/order/catch-up offset) and resolved
/// `epg_channel_id` merged in.
pub fn channel(conn: &Connection, id: ChannelId) -> Result<Channel> { todo!("WP-02") }

/// Lookup by stable identity (CLI `--play`, MPRIS, reminders).
pub fn channel_by_ref(conn: &Connection, item: &ItemRef) -> Result<Option<Channel>> { todo!("WP-02") }

/// Groups of `kind` (optionally one source) with group overrides merged, in display order
/// (user_order, then provider_order). Hidden groups included (callers filter).
pub fn groups(conn: &Connection, kind: ItemKind, source: Option<SourceId>) -> Result<Vec<Group>> { todo!("WP-02") }

pub fn group(conn: &Connection, id: GroupId) -> Result<Group> { todo!("WP-02") }

/// Why a stream failed (persisted per channel; DESIGN §11.6).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFailure {
    pub reason: String,
    pub at: Timestamp,
    pub count: u32,
}

/// Record a playback failure of a channel (increments count).
pub fn record_failure(conn: &Connection, item: &ItemRef, reason: &str, now: Timestamp) -> Result<()> { todo!("WP-02") }

/// Clear on successful playback.
pub fn clear_failure(conn: &Connection, item: &ItemRef) -> Result<()> { todo!("WP-02") }

/// Should zapping skip this channel? True when it has ≥ 2 failures newer than `since` or is
/// DRM-flagged. Point query (controller thread, per zap step).
pub fn should_skip(conn: &Connection, ch: ChannelId, since: Timestamp) -> Result<bool> { todo!("WP-02") }

/// Failures newer than `since` (UI dims these rows; zapping skips them after 2 failures).
pub fn failures(conn: &Connection, since: Timestamp) -> Result<HashMap<ItemRef, StreamFailure>> { todo!("WP-02") }
