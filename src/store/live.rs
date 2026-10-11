//! Live channel/group reads + failure cache. Owner: WP-02.
//! (Lists are served from the in-memory `library::LiveIndex`; these are point lookups.)

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::Result;
use crate::model::{Channel, ChannelId, Group, GroupId, ItemKey, ItemKind, ItemRef, LabelTrust, Quality, SourceId, Timestamp};
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
     COALESCE(o.hidden, 0), COALESCE(o.locked, 0), g.item_count, g.provider_id, COALESCE(g.raw_name, g.name), COALESCE(o.shown, 0), g.explicit, o.explicit";

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
        user_hidden: r.get(8)?,
        locked: r.get(9)?,
        item_count: u32_of(r.get(10)?),
        provider_id: r.get(11)?,
        // the provider's raw name; groups imported before P6 fall back to the display name
        tag: crate::sources::category_tags::detect_tag(&r.get::<_, String>(12)?),
        // the user's "show anyway"; `library::load` applies the source's rule (DESIGN §28.4)
        filter: if r.get(13)? { crate::model::GroupFilter::Exempt } else { crate::model::GroupFilter::Kept },
        explicit: crate::model::Explicit::of(r.get(14)?, r.get(15)?),
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

/// Is a channel / movie / series row explicit content (DESIGN §20.3), by the library's rule
/// (`library::Inherited::explicit`): one of its categories is (`grp.explicit`, the user's word
/// first), or the provider flags the item and none of its categories is marked "Not
/// explicit". `table` / `link` / `col`: the item table, its group link table and column.
fn explicit_row(conn: &Connection, table: &str, link: &str, col: &str, id: i64) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM {link} l JOIN grp g ON g.id = l.group_id
                          LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
                        WHERE l.{col} = x.id AND COALESCE(o.explicit, g.explicit > 0) = 1)
             OR (x.explicit = 1 AND NOT EXISTS (SELECT 1 FROM {link} l JOIN grp g ON g.id = l.group_id
                          JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
                        WHERE l.{col} = x.id AND o.explicit = 0))
         FROM {table} x WHERE x.id = ?1"
    );
    Ok(conn.prepare_cached(&sql)?.query_row([id], |r| r.get(0)).optional()?.unwrap_or(false))
}

/// A channel is explicit content (DESIGN §20.3; see `explicit_row`). Unknown ids aren't.
pub fn channel_explicit(conn: &Connection, id: ChannelId) -> Result<bool> {
    explicit_row(conn, "channel", "channel_group", "channel_id", id.0)
}

/// A channel, movie, series or episode (its series) is explicit content (DESIGN §20.3). Rows
/// that no longer exist aren't.
pub fn item_explicit(conn: &Connection, item: &ItemRef) -> Result<bool> {
    let id_of = |table: &str| -> Result<Option<i64>> {
        Ok(conn
            .prepare_cached(&format!("SELECT id FROM {table} WHERE source_id = ?1 AND key = ?2"))?
            .query_row(params![item.source, item.key.as_str()], |r| r.get(0))
            .optional()?)
    };
    let (table, link, col, id) = match item.kind {
        ItemKind::Channel => ("channel", "channel_group", "channel_id", id_of("channel")?),
        ItemKind::Movie => ("movie", "movie_group", "movie_id", id_of("movie")?),
        ItemKind::Series => ("series", "series_group", "series_id", id_of("series")?),
        ItemKind::Episode => {
            let series: Option<i64> = conn
                .prepare_cached("SELECT series_id FROM episode WHERE source_id = ?1 AND key = ?2")?
                .query_row(params![item.source, item.key.as_str()], |r| r.get(0))
                .optional()?;
            ("series", "series_group", "series_id", series)
        }
    };
    match id {
        Some(id) => explicit_row(conn, table, link, col, id),
        None => Ok(false),
    }
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

/// Remember the picture size mpv decoded for a live channel (`channel_video`; its class wins
/// over the quality word of the name, DESIGN §6), with the provider name and `url_key` it was
/// measured under: it applies only while the channel at that key still has them
/// (`store::import` moves it with the channel). Also the trust evidence of the source's
/// quality words (`quality_check`: what the name claimed, what played; DESIGN §13.0.1) — not
/// for radio (a cover picture is no video). Not a channel, or gone → nothing.
pub fn record_video(conn: &Connection, item: &ItemRef, width: i64, height: i64, now: Timestamp) -> Result<()> {
    if item.kind != ItemKind::Channel {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO channel_video(source_id, item_key, name, url_key, width, height, at)
         SELECT c.source_id, c.key, c.name, c.url_key, ?3, ?4, ?5 FROM channel c WHERE c.source_id = ?1 AND c.key = ?2
         ON CONFLICT(source_id, item_key) DO UPDATE SET name = excluded.name, url_key = excluded.url_key,
           width = excluded.width, height = excluded.height, at = excluded.at",
    )?
    .execute(params![item.source, item.key.as_str(), width, height, now])?;
    if let Some(class) = Quality::of_video(width, height) {
        conn.prepare_cached(
            "INSERT INTO quality_check(source_id, name, claimed, measured, at)
             SELECT c.source_id, c.name, c.quality, ?3, ?4 FROM channel c WHERE c.source_id = ?1 AND c.key = ?2 AND c.radio = 0
             ON CONFLICT(source_id, name) DO UPDATE SET claimed = excluded.claimed, measured = excluded.measured, at = excluded.at",
        )?
        .execute(params![item.source, item.key.as_str(), class.code(), now])?;
    }
    Ok(())
}

/// Per source, how far its quality words held up when its channels played (`quality_check`;
/// [`LabelTrust`]). Sources with no measurement are absent.
pub fn label_trust(conn: &Connection) -> Result<HashMap<SourceId, LabelTrust>> {
    let mut stmt = conn.prepare_cached("SELECT source_id, claimed, measured, count(*) FROM quality_check GROUP BY source_id, claimed, measured")?;
    let mut out: HashMap<SourceId, LabelTrust> = HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let claimed = r.get::<_, Option<i64>>(1)?.and_then(Quality::from_code);
        let Some(measured) = Quality::from_code(r.get(2)?) else { continue };
        let t = out.entry(r.get(0)?).or_default();
        for _ in 0..r.get::<_, i64>(3)? {
            t.add(claimed, measured);
        }
    }
    Ok(out)
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
