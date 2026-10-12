//! Before / after now-next of proposed changes (batched `store::epg::now_next`, < 20 ms for
//! 200 pairs). Read-only: works on a reader connection. OWNER: G-core.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::model::{EpgChannelId, FixChange, NowNext, Timestamp};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreviewRow {
    pub before: NowNext,
    pub after: NowNext,
    pub warnings: Vec<String>,
}

/// One channel's guide channel and effective shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct State {
    epg: Option<EpgChannelId>,
    shift: i32,
}

/// The phrase when the new guide channel has nothing now or later.
pub const WARN_EMPTY: &str = "the guide has nothing for it now or later";
/// The phrase for a change to a channel that is gone.
pub const WARN_GONE: &str = "no longer in the playlist";

/// Preview `changes` at `now`: one row per change, in order (the confirm card, the detail
/// pane, Claude's `guide_fix propose`). Times are shown times (shifts applied).
pub fn preview(conn: &Connection, changes: &[FixChange], now: Timestamp) -> Result<Vec<PreviewRow>> {
    let mut states: Vec<Option<(State, State)>> = Vec::with_capacity(changes.len());
    for ch in changes {
        states.push(match &ch.channel {
            Some(r) => before_after(conn, r, ch)?,
            None => None,
        });
    }
    let pairs: Vec<(EpgChannelId, i32)> = states.iter().flatten().flat_map(|(b, a)| [b, a]).filter_map(|s| s.epg.map(|e| (e, s.shift))).collect();
    let nn = crate::store::epg::now_next(conn, &pairs, now)?;
    let get = |s: &State| s.epg.and_then(|e| nn.get(&(e, s.shift)).cloned()).unwrap_or_default();
    Ok(states
        .iter()
        .map(|st| match st {
            None => PreviewRow { warnings: vec![WARN_GONE.into()], ..Default::default() },
            Some((b, a)) => {
                let after = get(a);
                let mut warnings = Vec::new();
                if a.epg.is_some() && after.now.is_none() && after.next.is_none() {
                    warnings.push(WARN_EMPTY.into());
                }
                PreviewRow { before: get(b), after, warnings }
            }
        })
        .collect())
}

/// `(guide channel, own shift, rule shift, provider shift)` of a channel.
type Shifts = (Option<EpgChannelId>, Option<i32>, Option<i32>, i32);

/// The channel's state now and after `change` (`None`: no such channel).
fn before_after(conn: &Connection, r: &crate::model::ItemRef, change: &FixChange) -> Result<Option<(State, State)>> {
    let row: Option<Shifts> = conn
        .prepare_cached(
            "SELECT ce.epg_channel_id, o.shift_min, ce.shift_min, c.tvg_shift_min FROM channel c
             LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
             LEFT JOIN channel_epg ce ON ce.channel_id = c.id
             WHERE c.source_id = ?1 AND c.key = ?2",
        )?
        .query_row(params![r.source, r.key.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .optional()?;
    let Some((epg, own, rule, provider)) = row else { return Ok(None) };
    let before = State { epg, shift: own.or(rule).unwrap_or(provider) };
    // the guide channel (and its rule shift) after the mapping change
    let (epg_after, rule_after) = match &change.map {
        None => (epg, rule),
        Some(Some(t)) => (guide_channel(conn, t.epg_source, &t.xmltv_id)?, None),
        Some(None) => match crate::store::epg::resolve_one(conn, r, "AND m.method != 'manual'")? {
            Some((e, s)) => (Some(e), s),
            None => (None, None),
        },
    };
    let own_after = match change.shift_min {
        Some(v) => v,
        None => own,
    };
    Ok(Some((before, State { epg: epg_after, shift: own_after.or(rule_after).unwrap_or(provider) })))
}

/// The guide channel `(guide, xmltv id)` in the guide's active generation.
fn guide_channel(conn: &Connection, epg_source: crate::model::EpgSourceId, xmltv_id: &str) -> Result<Option<EpgChannelId>> {
    Ok(conn
        .prepare_cached(
            "SELECT ec.id FROM epg_channel ec JOIN epg_source es ON es.id = ec.epg_source_id
             WHERE ec.epg_source_id = ?1 AND ec.xmltv_id = ?2 AND ec.seen_gen >= es.active_gen",
        )?
        .query_row(params![epg_source, xmltv_id], |r| r.get(0))
        .optional()?)
}
