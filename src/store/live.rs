//! Live channel/group reads + failure cache. Owner: WP-02.
//! (Lists are served from the in-memory `library::LiveIndex`; these are point lookups.)

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::Result;
use crate::model::{Channel, ChannelId, Group, GroupId, ItemKey, ItemKind, ItemRef, SourceId, Timestamp};
use crate::store::sql::{Json, enum_parse, from_json_opt, kind_from_sql, or_not_found, u32_of};

const CHANNEL_COLS: &str = "c.id, c.source_id, c.key, c.provider_id, c.name, c.clean_name, c.number, c.logo, c.tvg_id, c.tvg_name, \
     c.tvg_shift_min, c.stream, c.headers, c.catchup, c.radio, c.unsupported, c.provider_order, c.added_at, o.rename, \
     COALESCE(o.hidden, 0), COALESCE(o.locked, 0), o.user_order, COALESCE(o.catchup_offset_s, 0), ce.epg_channel_id";

const CHANNEL_FROM: &str = "FROM channel c
     LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
     LEFT JOIN channel_epg ce ON ce.channel_id = c.id";

fn channel_from_row(r: &Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: r.get(0)?,
        source_id: r.get(1)?,
        key: ItemKey(r.get(2)?),
        provider_id: r.get(3)?,
        name: r.get(4)?,
        clean_name: r.get(5)?,
        rename: r.get(18)?,
        number: r.get(6)?,
        logo: r.get(7)?,
        group_ids: Vec::new(),
        tvg_id: r.get(8)?,
        tvg_name: r.get(9)?,
        tvg_shift_min: r.get(10)?,
        stream: r.get::<_, Json<_>>(11)?.0,
        headers: from_json_opt(r.get(12)?).unwrap_or_default(),
        catchup: from_json_opt(r.get(13)?),
        catchup_offset_s: r.get(22)?,
        radio: r.get(14)?,
        unsupported: r.get::<_, Option<String>>(15)?.and_then(|s| enum_parse(&s)),
        provider_order: r.get(16)?,
        user_order: r.get(21)?,
        hidden: r.get(19)?,
        locked: r.get(20)?,
        added_at: r.get(17)?,
        epg_channel_id: r.get(23)?,
    })
}

/// Group ids of a channel (primary first) and whether any of them is PIN-locked.
fn channel_groups(conn: &Connection, id: ChannelId) -> Result<(Vec<GroupId>, bool)> {
    let mut stmt = conn.prepare_cached(
        "SELECT cg.group_id, COALESCE(o.locked, 0) FROM channel_group cg JOIN grp g ON g.id = cg.group_id
         LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
         WHERE cg.channel_id = ?1 ORDER BY cg.is_primary DESC, g.provider_order, g.id",
    )?;
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, GroupId>(0)?, r.get::<_, bool>(1)?)))?;
    let mut ids = Vec::new();
    let mut locked = false;
    for row in rows {
        let (g, l) = row?;
        ids.push(g);
        locked |= l;
    }
    Ok((ids, locked))
}

/// Full channel row with overrides (rename/hidden/locked/order/catch-up offset) and resolved
/// `epg_channel_id` merged in.
///
/// `locked` is effective: the channel's own lock or a lock on any of its groups.
pub fn channel(conn: &Connection, id: ChannelId) -> Result<Channel> {
    let r = conn.prepare_cached(&format!("SELECT {CHANNEL_COLS} {CHANNEL_FROM} WHERE c.id = ?1"))?.query_row([id], channel_from_row);
    let mut ch = or_not_found(r, "channel", id)?;
    let (groups, group_locked) = channel_groups(conn, id)?;
    ch.group_ids = groups;
    ch.locked |= group_locked;
    Ok(ch)
}

/// Lookup by stable identity (CLI `--play`, MPRIS, reminders).
pub fn channel_by_ref(conn: &Connection, item: &ItemRef) -> Result<Option<Channel>> {
    if item.kind != ItemKind::Channel {
        return Ok(None);
    }
    let id: Option<ChannelId> = conn
        .prepare_cached("SELECT id FROM channel WHERE source_id = ?1 AND key = ?2")?
        .query_row(params![item.source, item.key.as_str()], |r| r.get(0))
        .optional()?;
    id.map(|id| channel(conn, id)).transpose()
}

pub(crate) const GROUP_COLS: &str = "g.id, g.source_id, g.kind, g.key, g.name, o.rename, g.provider_order, o.user_order, \
     COALESCE(o.hidden, 0), COALESCE(o.locked, 0), g.item_count, g.provider_id";

pub(crate) const GROUP_FROM: &str = "FROM grp g JOIN source s ON s.id = g.source_id
     LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key";

/// Display order of groups: source order, then user order (reordered groups first), then
/// provider order.
pub(crate) const GROUP_ORDER: &str = "s.sort_index, s.id, (o.user_order IS NULL), o.user_order, g.provider_order, g.id";

pub(crate) fn group_from_row(r: &Row<'_>) -> rusqlite::Result<Group> {
    Ok(Group {
        id: r.get(0)?,
        source_id: r.get(1)?,
        kind: kind_from_sql(&r.get::<_, String>(2)?)?,
        key: crate::model::GroupKey(r.get(3)?),
        name: r.get(4)?,
        rename: r.get(5)?,
        provider_order: r.get(6)?,
        user_order: r.get(7)?,
        hidden: r.get(8)?,
        locked: r.get(9)?,
        item_count: u32_of(r.get(10)?),
        provider_id: r.get(11)?,
        // P6-core: detected from `g.raw_name` (fallback `g.name`); `filter` from `o.shown` and
        // the source's rule in `library::load` (DESIGN §28.4)
        tag: None,
        filter: crate::model::GroupFilter::Kept,
    })
}

/// Groups of `kind` (optionally one source) with group overrides merged, in display order
/// (user_order, then provider_order). Hidden groups included (callers filter).
pub fn groups(conn: &Connection, kind: ItemKind, source: Option<SourceId>) -> Result<Vec<Group>> {
    let mut stmt = conn
        .prepare_cached(&format!("SELECT {GROUP_COLS} {GROUP_FROM} WHERE g.kind = ?1 AND (?2 IS NULL OR g.source_id = ?2) ORDER BY {GROUP_ORDER}"))?;
    let rows = stmt.query_map(params![kind.as_str(), source], group_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn group(conn: &Connection, id: GroupId) -> Result<Group> {
    let r = conn.prepare_cached(&format!("SELECT {GROUP_COLS} {GROUP_FROM} WHERE g.id = ?1"))?.query_row([id], group_from_row);
    or_not_found(r, "group", id)
}

/// Why a stream failed (persisted per channel; DESIGN §11.6).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFailure {
    pub reason: String,
    pub at: Timestamp,
    pub count: u32,
}

/// Record a playback failure of a channel (increments count).
pub fn record_failure(conn: &Connection, item: &ItemRef, reason: &str, now: Timestamp) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO stream_failure(source_id, item_key, reason, at, count) VALUES (?1, ?2, ?3, ?4, 1)
         ON CONFLICT(source_id, item_key) DO UPDATE SET reason = excluded.reason, at = excluded.at, count = stream_failure.count + 1",
    )?
    .execute(params![item.source, item.key.as_str(), reason, now])?;
    Ok(())
}

/// Clear on successful playback.
pub fn clear_failure(conn: &Connection, item: &ItemRef) -> Result<()> {
    conn.prepare_cached("DELETE FROM stream_failure WHERE source_id = ?1 AND item_key = ?2")?.execute(params![item.source, item.key.as_str()])?;
    Ok(())
}

/// Should zapping skip this channel? True when it has ≥ 2 failures newer than `since` or is
/// DRM-flagged. Point query (controller thread, per zap step).
///
/// The cache keeps the latest failure time and a running count (reset by a successful play),
/// so "≥ 2 failures newer than `since`" reads as "count ≥ 2 and the last one after `since`".
pub fn should_skip(conn: &Connection, ch: ChannelId, since: Timestamp) -> Result<bool> {
    let row: Option<(bool, Option<i64>, Option<Timestamp>)> = conn
        .prepare_cached(
            "SELECT c.unsupported IS NOT NULL, f.count, f.at FROM channel c
             LEFT JOIN stream_failure f ON f.source_id = c.source_id AND f.item_key = c.key
             WHERE c.id = ?1",
        )?
        .query_row([ch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    Ok(match row {
        Some((drm, count, at)) => drm || (count.unwrap_or(0) >= 2 && at.is_some_and(|t| t >= since)),
        None => false,
    })
}

/// Failures newer than `since` (UI dims these rows; zapping skips them after 2 failures).
pub fn failures(conn: &Connection, since: Timestamp) -> Result<HashMap<ItemRef, StreamFailure>> {
    let mut stmt = conn.prepare_cached("SELECT source_id, item_key, reason, at, count FROM stream_failure WHERE at >= ?1")?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            ItemRef { source: r.get(0)?, kind: ItemKind::Channel, key: ItemKey(r.get(1)?) },
            StreamFailure { reason: r.get(2)?, at: r.get(3)?, count: u32_of(r.get(4)?) },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
