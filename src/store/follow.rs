//! Following storage: `follow_rule` + the `follow_match` ledger (migration 0011, DESIGN §33).
//! OWNER: F-AB.
//!
//! Hooks other owners call (signatures fixed by the base):
//! * [`on_reminder_removed`] from `store::user::remove_reminder` (the only delete path, B6), in
//!   the same transaction: a removed rule reminder's ledger row becomes `skipped`.
//! * [`on_reminder_edited`] when the user edits a rule reminder's lead or mode: the reminder
//!   detaches (`rule_id = NULL`, ledger row `adopted`).
//! * [`retime`] from `store::retime::channel_shift` (B8).
//! * [`flash_of`] from the reminder scheduler at fire time: is this reminder a news flash?
//!
//! The evaluation (`jobs::follow`) reads with [`rules`], [`ledger`], [`scan`] and friends on
//! the blocking pool and writes with [`apply_plan`] in one writer transaction.

use std::collections::{HashMap, HashSet};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::{Error, Result};
use crate::follow::flash::FlashSpec;
use crate::follow::matcher::{Matcher, Row as GuideRow, Scratch};
use crate::follow::plan::{Hit, LedgerRow, MatchState, Plan, ReminderOp, Upcoming};
use crate::follow::{FollowKind, FollowMode, FollowRule, FollowSpec};
use crate::model::{FollowRuleId, ItemKey, ItemKind, ItemRef, OpenIn, Reminder, ReminderId, ReminderMode, RemoveWhy, SourceId, Timestamp};
use crate::store::sql::{atomic, enum_parse, enum_str};

/// Ledger rows of show rules are kept this long (new-only remembers episodes).
pub const SHOW_KEEP_S: i64 = 2 * 365 * 86_400;
/// Other ledger rows are kept this long after their stop.
pub const KEEP_S: i64 = 30 * 86_400;

// ------------------------------------------------------------------ rules

const RULE_COLS: &str = "id, name, kind, spec, mode, lead_s, offset_s, spoiler_safe, max_per_week, priority, enabled, origin, created_at, updated_at";

fn rule_from_row(r: &Row<'_>) -> rusqlite::Result<FollowRule> {
    let spec: String = r.get(3)?;
    Ok(FollowRule {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: enum_parse(&r.get::<_, String>(2)?).unwrap_or_default(),
        spec: serde_json::from_str(&spec).unwrap_or_else(|e| {
            tracing::warn!(target: "tuner::follow", "a rule's spec didn't parse ({e}); it matches nothing");
            FollowSpec::default()
        }),
        mode: enum_parse(&r.get::<_, String>(4)?).unwrap_or_default(),
        lead_s: r.get(5)?,
        offset_s: r.get(6)?,
        spoiler_safe: r.get(7)?,
        max_per_week: r.get::<_, i64>(8)?.clamp(0, i64::from(u32::MAX)) as u32,
        priority: r.get(9)?,
        enabled: r.get(10)?,
        by_agent: r.get::<_, String>(11)? == "agent",
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

/// Every rule in list order (priority; lower wins a clash).
pub fn rules(conn: &Connection) -> Result<Vec<FollowRule>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {RULE_COLS} FROM follow_rule ORDER BY priority, id"))?;
    let rows = stmt.query_map([], rule_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn rule(conn: &Connection, id: FollowRuleId) -> Result<Option<FollowRule>> {
    Ok(conn.query_row(&format!("SELECT {RULE_COLS} FROM follow_rule WHERE id = ?1"), [id], rule_from_row).optional()?)
}

/// Save `r`: a new rule (`id` 0; last in the list unless it has a priority) or an edit. At most
/// `max_rules` rules (`follow.max_rules`). Returns its id. The caller submits
/// `EvaluateFollows { reason: Rules }`.
pub fn save_rule(conn: &Connection, r: &FollowRule, max_rules: u32, now: Timestamp) -> Result<FollowRuleId> {
    if let Some(why) = r.spec.problem() {
        return Err(Error::Other(why.into()));
    }
    let name = r.name.trim();
    if name.is_empty() {
        return Err(Error::Other("a rule needs a name".into()));
    }
    let spec = serde_json::to_string(&r.spec).map_err(Error::other)?;
    let origin = if r.by_agent { "agent" } else { "user" };
    if r.id.0 == 0 {
        let n: i64 = conn.query_row("SELECT count(*) FROM follow_rule", [], |x| x.get(0))?;
        if n >= i64::from(max_rules) {
            return Err(Error::Other(format!("you follow {n} things already (the limit is {max_rules})")));
        }
        let priority = if r.priority == i64::MAX {
            conn.query_row("SELECT COALESCE(max(priority), 0) + 1 FROM follow_rule", [], |x| x.get(0))?
        } else {
            r.priority
        };
        conn.execute(
            "INSERT INTO follow_rule(name, kind, spec, mode, lead_s, offset_s, spoiler_safe, max_per_week, priority, enabled, origin, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![name, enum_str(&r.kind), spec, enum_str(&r.mode), r.lead_s, r.offset_s, r.spoiler_safe, r.max_per_week, priority, r.enabled, origin, now],
        )?;
        return Ok(FollowRuleId(conn.last_insert_rowid()));
    }
    atomic(conn, |c| {
        let Some(was) = rule(c, r.id)? else { return Err(crate::store::sql::not_found("follow rule", r.id)) };
        c.execute(
            "UPDATE follow_rule SET name = ?2, kind = ?3, spec = ?4, mode = ?5, lead_s = ?6, offset_s = ?7, spoiler_safe = ?8, max_per_week = ?9,
               enabled = ?10, updated_at = ?11 WHERE id = ?1",
            params![r.id, name, enum_str(&r.kind), spec, enum_str(&r.mode), r.lead_s, r.offset_s, r.spoiler_safe, r.max_per_week, r.enabled, now],
        )?;
        // what its reminders carry changed: they go, and the evaluation the caller submits makes
        // them again with the new settings (ledger rows stay `scheduled`, their reminder NULL)
        let carried = |x: &FollowRule, name: &str| {
            (x.mode, x.lead_s, x.offset_s, x.spoiler_safe, x.spec.flash, if x.spoiler_safe { Some(name.to_owned()) } else { None })
        };
        if carried(&was, was.name.trim()) != carried(r, name) {
            let owned: Vec<ReminderId> = {
                let mut stmt = c.prepare_cached("SELECT id FROM reminder WHERE rule_id = ?1 AND state = 'pending'")?;
                stmt.query_map([r.id], |x| x.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            for id in owned {
                crate::store::user::remove_reminder(c, id, RemoveWhy::Rule)?;
            }
        }
        Ok(r.id)
    })
}

/// Delete a rule and its ledger; its pending reminders are cancelled unless `keep_reminders`
/// (they then become your own). Returns how many reminders were cancelled.
pub fn delete_rule(conn: &Connection, id: FollowRuleId, keep_reminders: bool) -> Result<usize> {
    atomic(conn, |c| {
        let mut cancelled = 0;
        if keep_reminders {
            c.execute("UPDATE reminder SET rule_id = NULL WHERE rule_id = ?1", [id])?;
        } else {
            let owned: Vec<ReminderId> = {
                let mut stmt = c.prepare_cached("SELECT id FROM reminder WHERE rule_id = ?1 AND state = 'pending'")?;
                stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            // one another rule adopted passes to that rule
            for r in owned {
                if release(c, r)? {
                    cancelled += 1;
                }
            }
            c.execute("UPDATE reminder SET rule_id = NULL WHERE rule_id = ?1", [id])?;
        }
        c.execute("DELETE FROM follow_match WHERE rule_id = ?1", [id])?;
        c.execute("DELETE FROM follow_rule WHERE id = ?1", [id])?;
        Ok(cancelled)
    })
}

/// Pause or resume a rule (a paused rule's reminders go at the next evaluation).
pub fn set_rule_enabled(conn: &Connection, id: FollowRuleId, on: bool, now: Timestamp) -> Result<()> {
    if conn.execute("UPDATE follow_rule SET enabled = ?2, updated_at = ?3 WHERE id = ?1", params![id, on, now])? == 0 {
        return Err(crate::store::sql::not_found("follow rule", id));
    }
    Ok(())
}

/// The list order (first wins a clash); rules not named keep their place after them.
pub fn reorder_rules(conn: &Connection, order: &[FollowRuleId], now: Timestamp) -> Result<()> {
    atomic(conn, |c| {
        let mut all: Vec<FollowRuleId> = rules(c)?.into_iter().map(|r| r.id).collect();
        all.retain(|id| !order.contains(id));
        for (i, id) in order.iter().chain(&all).enumerate() {
            c.execute("UPDATE follow_rule SET priority = ?2, updated_at = ?3 WHERE id = ?1", params![id, i as i64 + 1, now])?;
        }
        Ok(())
    })
}

/// Rules Claude added in the last `window_s` (MCP rate limit: 10 an hour) and in all.
pub fn agent_rule_counts(conn: &Connection, since: Timestamp) -> Result<(usize, usize)> {
    Ok(conn.query_row("SELECT count(*) FILTER (WHERE created_at >= ?1), count(*) FROM follow_rule WHERE origin = 'agent'", [since], |r| {
        Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
    })?)
}

// ------------------------------------------------------------------ the ledger

const MATCH_COLS: &str = "id, rule_id, event_key, start, stop, title, sub_title, source_id, channel_key, alternates, state, note, reminder_id";

fn channel_ref(source: Option<SourceId>, key: Option<String>) -> Option<ItemRef> {
    Some(ItemRef { source: source?, kind: ItemKind::Channel, key: ItemKey(key?) })
}

fn ledger_from_row(r: &Row<'_>) -> rusqlite::Result<LedgerRow> {
    let alternates: String = r.get(9)?;
    Ok(LedgerRow {
        id: r.get(0)?,
        rule: r.get(1)?,
        event_key: r.get(2)?,
        start: r.get(3)?,
        stop: r.get(4)?,
        title: r.get(5)?,
        sub_title: r.get(6)?,
        channel: channel_ref(r.get(7)?, r.get(8)?),
        alternates: serde_json::from_str(&alternates).unwrap_or_default(),
        state: MatchState::parse(&r.get::<_, String>(10)?).unwrap_or(MatchState::Gone),
        note: r.get(11)?,
        reminder: r.get(12)?,
        covered: false,
    })
}

/// Every ledger row (pruned to 30 days, two years for shows); `covered` says whether the guide
/// still covers a row's start.
pub fn ledger(conn: &Connection, covered: &dyn Fn(&LedgerRow) -> bool) -> Result<Vec<LedgerRow>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {MATCH_COLS} FROM follow_match ORDER BY start, id"))?;
    let rows = stmt.query_map([], ledger_from_row)?;
    let mut out: Vec<LedgerRow> = rows.collect::<rusqlite::Result<_>>()?;
    for r in &mut out {
        r.covered = covered(r);
    }
    Ok(out)
}

/// The Upcoming list: matches still to come (or on now), soonest first, at most `limit`.
/// Spoiler-safe rules' rows carry the rule name as their title — no fixture, no episode title.
pub fn upcoming(conn: &Connection, now: Timestamp, limit: usize) -> Result<Vec<Upcoming>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.rule_id, m.event_key, m.start, m.stop, m.title, m.sub_title, m.source_id, m.channel_key, m.alternates, m.state, m.note,
                m.reminder_id, r.mode, r.spoiler_safe, r.name, COALESCE(c.name, '')
         FROM follow_match m JOIN follow_rule r ON r.id = m.rule_id
         LEFT JOIN channel c ON c.source_id = m.source_id AND c.key = m.channel_key
         WHERE m.stop > ?1 AND m.state IN ('scheduled', 'adopted', 'listed', 'over-cap', 'skipped')
         ORDER BY m.start, r.priority LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![now, limit as i64], |r| {
        let row = ledger_from_row(r)?;
        let mode: FollowMode = enum_parse(&r.get::<_, String>(13)?).unwrap_or_default();
        let safe: bool = r.get(14)?;
        let name: String = r.get(15)?;
        let downgraded = row.note.as_deref().is_some_and(|n| n == "locked" || n.starts_with("clash:"));
        Ok(Upcoming {
            rule: row.rule,
            event_key: row.event_key,
            start: row.start,
            stop: row.stop,
            title: if safe { name } else { row.title },
            sub_title: if safe { None } else { row.sub_title },
            channel: row.channel,
            channel_name: r.get(16)?,
            alternates: row.alternates,
            state: row.state,
            mode: if downgraded { FollowMode::Notify } else { mode },
            note: row.note,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What [`apply_plan`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    /// Reminders made (adoptions not counted).
    pub reminders_set: usize,
    pub adopted: usize,
    pub moved: usize,
    pub gone: usize,
    /// Reminders cancelled (gone, moved, paused, downgraded).
    pub cancelled: usize,
}

/// Write `plan` (one transaction): ledger rows, and the reminders the rules own — made through
/// `store::user::add_reminder` with `rule_id`, cancelled through `remove_reminder(…, Rule)`. A
/// reminder you already had on that channel at that time is adopted, never touched.
pub fn apply_plan(conn: &Connection, plan: &Plan, rules: &[FollowRule], now: Timestamp) -> Result<Applied> {
    let by_id: HashMap<FollowRuleId, &FollowRule> = rules.iter().map(|r| (r.id, r)).collect();
    atomic(conn, |c| {
        let mut done = Applied::default();
        // cancelled reminders another rule had adopted pass to that rule once every row is
        // written (a rule paused or over its limit; never a moved or gone airing's)
        let mut released: Vec<ReminderId> = Vec::new();
        for step in &plan.steps {
            let mut to = step.to.clone();
            // None: leave the row's reminder as it is
            let mut reminder: Option<Option<ReminderId>> = None;
            match step.reminder {
                ReminderOp::Keep => {}
                ReminderOp::Cancel(id) => {
                    if owned(c, id)? {
                        released.push(id);
                    }
                    reminder = Some(None);
                }
                ReminderOp::Replace(id) => {
                    cancel(c, id, &mut done)?;
                    reminder = Some(None);
                }
                ReminderOp::Create => {}
            }
            if matches!(step.reminder, ReminderOp::Create | ReminderOp::Replace(_)) {
                match by_id.get(&to.rule) {
                    Some(rule) => {
                        let (id, adopted) = make_reminder(c, rule, &to)?;
                        if adopted {
                            to.state = MatchState::Adopted;
                            done.adopted += 1;
                        } else if id.is_some() {
                            done.reminders_set += 1;
                        }
                        reminder = Some(id);
                    }
                    None => reminder = Some(None),
                }
            }
            if step.moved_from.is_some() {
                done.moved += 1;
            }
            if to.state == MatchState::Gone {
                done.gone += 1;
            }
            write_row(c, step.row, &to, reminder, now)?;
        }
        for id in released {
            if release(c, id)? {
                done.cancelled += 1;
            }
        }
        Ok(done)
    })
}

/// A reminder a rule still owns (an edited one detached itself).
fn owned(c: &Connection, id: ReminderId) -> Result<bool> {
    Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM reminder WHERE id = ?1 AND rule_id IS NOT NULL)", [id], |r| r.get(0))?)
}

fn cancel(c: &Connection, id: ReminderId, done: &mut Applied) -> Result<()> {
    if owned(c, id)? {
        crate::store::user::remove_reminder(c, id, RemoveWhy::Rule)?;
        done.cancelled += 1;
    }
    Ok(())
}

/// Its rule lets go of reminder `id`: an enabled rule that adopted it (its row still `adopted`
/// with that reminder) takes it over, else it is cancelled. Returns whether it was cancelled.
fn release(c: &Connection, id: ReminderId) -> Result<bool> {
    let heir: Option<(i64, FollowRuleId)> = c
        .query_row(
            "SELECT m.id, m.rule_id FROM follow_match m
               JOIN follow_rule r ON r.id = m.rule_id AND r.enabled = 1
               JOIN reminder x ON x.id = m.reminder_id
             WHERE m.reminder_id = ?1 AND m.state = 'adopted' AND x.state = 'pending' AND m.rule_id IS NOT x.rule_id
             ORDER BY r.priority, r.id LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((row, rule)) = heir else {
        crate::store::user::remove_reminder(c, id, RemoveWhy::Rule)?;
        return Ok(true);
    };
    c.execute("UPDATE reminder SET rule_id = ?2 WHERE id = ?1", params![id, rule])?;
    c.execute("UPDATE follow_match SET state = 'scheduled' WHERE id = ?1", [row])?;
    c.execute("UPDATE follow_match SET reminder_id = NULL WHERE reminder_id = ?1 AND rule_id <> ?2", params![id, rule])?;
    Ok(false)
}

/// Make `to`'s reminder for `rule`, or adopt the one already at that channel and time.
/// Returns (its id, adopted).
fn make_reminder(c: &Connection, rule: &FollowRule, to: &Upcoming) -> Result<(Option<ReminderId>, bool)> {
    let Some(ch) = &to.channel else { return Ok((None, false)) };
    let existing: Option<ReminderId> = c
        .query_row(
            "SELECT id FROM reminder WHERE source_id = ?1 AND channel_key = ?2 AND start = ?3",
            params![ch.source, ch.key.as_str(), to.start],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((Some(id), true));
    }
    let switches = matches!(to.mode, FollowMode::Switch | FollowMode::Flash);
    let mut r = Reminder::notify(
        ch.clone(),
        to.channel_name.clone(),
        rule.safe_title(&to.title).to_owned(),
        to.start,
        to.stop,
        if switches { Reminder::SWITCH_LEAD } else { rule.lead_s.max(0) },
    );
    r.mode = if switches { ReminderMode::Switch } else { ReminderMode::Notify };
    r.offset_s = rule.offset_s;
    r.open_in = (to.mode == FollowMode::Flash).then_some(OpenIn::PopOut);
    r.rule_id = Some(rule.id);
    Ok((Some(crate::store::user::add_reminder(c, &r)?), false))
}

fn write_row(c: &Connection, row: Option<i64>, to: &Upcoming, reminder: Option<Option<ReminderId>>, now: Timestamp) -> Result<()> {
    let alternates = serde_json::to_string(&to.alternates).map_err(Error::other)?;
    let (source, key) = match &to.channel {
        Some(ch) => (Some(ch.source), Some(ch.key.as_str())),
        None => (None, None),
    };
    match row {
        Some(id) => {
            // UPDATE OR REPLACE: a stale row of the same (rule, key, start) gives way
            c.execute(
                "UPDATE OR REPLACE follow_match SET event_key = ?2, start = ?3, stop = ?4, title = ?5, sub_title = ?6, source_id = ?7,
                   channel_key = ?8, alternates = ?9, state = ?10, note = ?11,
                   reminder_id = CASE WHEN ?12 THEN ?13 ELSE reminder_id END, seen_at = ?14,
                   announced = CASE WHEN state IN ('gone', 'over-cap', 'skipped') AND ?10 IN ('scheduled', 'listed') THEN 0 ELSE announced END
                 WHERE id = ?1",
                params![
                    id,
                    to.event_key,
                    to.start,
                    to.stop,
                    to.title,
                    to.sub_title,
                    source,
                    key,
                    alternates,
                    to.state.as_str(),
                    to.note,
                    reminder.is_some(),
                    reminder.flatten(),
                    now
                ],
            )?;
        }
        None => {
            c.execute(
                "INSERT INTO follow_match(rule_id, event_key, start, stop, title, sub_title, source_id, channel_key, alternates, state, note, reminder_id, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(rule_id, event_key, start) DO UPDATE SET stop = excluded.stop, title = excluded.title, sub_title = excluded.sub_title,
                   source_id = excluded.source_id, channel_key = excluded.channel_key, alternates = excluded.alternates,
                   state = CASE WHEN follow_match.state = 'skipped' THEN 'skipped' ELSE excluded.state END,
                   note = excluded.note, reminder_id = COALESCE(excluded.reminder_id, follow_match.reminder_id), seen_at = excluded.seen_at",
                params![to.rule, to.event_key, to.start, to.stop, to.title, to.sub_title, source, key, alternates, to.state.as_str(), to.note, reminder.flatten(), now],
            )?;
        }
    }
    Ok(())
}

/// A new match for the summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Unannounced {
    pub id: i64,
    pub rule: FollowRuleId,
    pub start: Timestamp,
    /// The programme title, or the rule name for a spoiler-safe rule.
    pub title: String,
    pub channel: String,
}

/// New matches not announced yet (the summary), soonest first. News flashes aren't announced:
/// they pop up by themselves.
pub fn unannounced(conn: &Connection, now: Timestamp) -> Result<Vec<Unannounced>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.rule_id, m.start, CASE WHEN r.spoiler_safe THEN r.name ELSE m.title END, COALESCE(c.name, '')
         FROM follow_match m JOIN follow_rule r ON r.id = m.rule_id
         LEFT JOIN channel c ON c.source_id = m.source_id AND c.key = m.channel_key
         WHERE m.announced = 0 AND m.start > ?1 AND m.state IN ('scheduled', 'adopted', 'listed') AND r.mode <> 'flash'
         ORDER BY m.start, r.priority",
    )?;
    let rows =
        stmt.query_map([now], |r| Ok(Unannounced { id: r.get(0)?, rule: r.get(1)?, start: r.get(2)?, title: r.get(3)?, channel: r.get(4)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn mark_announced(conn: &Connection, ids: &[i64]) -> Result<()> {
    conn.execute(
        "UPDATE follow_match SET announced = 1 WHERE id IN (SELECT value FROM json_each(?1))",
        [crate::store::sql::json_ids(ids.iter().copied())],
    )?;
    Ok(())
}

// ------------------------------------------------------------------ the scan's inputs

/// The latest programme stop of each guide channel's guide (`epg_source.max_stop`): a ledger
/// row is "covered" while its start is before it.
pub fn coverage(conn: &Connection) -> Result<HashMap<i64, Timestamp>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ec.id, s.max_stop FROM epg_channel ec JOIN epg_source s ON s.id = ec.epg_source_id WHERE s.enabled = 1 AND s.max_stop IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Channels that failed to play since `since` (ranked after the others, B5).
pub fn trouble(conn: &Connection, since: Timestamp) -> Result<Vec<ItemRef>> {
    let mut stmt = conn.prepare_cached("SELECT source_id, item_key FROM stream_failure WHERE at >= ?1")?;
    let rows = stmt.query_map([since], |r| Ok(ItemRef { source: r.get(0)?, kind: ItemKind::Channel, key: ItemKey(r.get(1)?) }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Pending switches that aren't a rule's (yours, Claude's): (start, stop). They win clashes.
pub fn manual_switches(conn: &Connection, now: Timestamp) -> Result<Vec<(Timestamp, Timestamp)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT start + offset_s, stop FROM reminder WHERE state = 'pending' AND rule_id IS NULL AND (mode = 'switch' OR lead_s = ?1) AND stop > ?2",
    )?;
    let rows = stmt.query_map(params![Reminder::SWITCH_LEAD, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What the guide and the rules look like (the evaluation's early exit): the active guide
/// generations, the rules' last change and the channel ↔ guide mappings (a guide fix or a
/// re-match changes which channels show what).
pub fn inputs_hash(conn: &Connection) -> Result<String> {
    let (gens, rules, maps): (String, String, String) = conn.query_row(
        "SELECT (SELECT COALESCE(group_concat(id || ':' || active_gen || ':' || COALESCE(max_stop, 0), ','), '') FROM epg_source WHERE enabled = 1),
                (SELECT count(*) || ':' || COALESCE(max(updated_at), 0) FROM follow_rule),
                (SELECT count(*) || ':' || total(channel_id * 1000003 + epg_channel_id) FROM channel_epg)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(format!("{gens}|{rules}|{maps}"))
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    Ok(())
}

/// Scan counters (INFO logs carry counts only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub rows: u64,
    pub hits: u64,
}

/// Programmes on `followable` guide channels that end after `from` (and start before `until`)
/// through `matcher`: every hit — left-outs too when `keep_left_out` (previews) — lands in
/// `hits`. Text is read borrowed and folded into reusable buffers; the channel is checked
/// before any text is read. `cancelled` is asked every 8 192 rows.
pub fn scan(
    conn: &Connection,
    followable: &EpgSet,
    matcher: &Matcher,
    (from, until): (Timestamp, Option<Timestamp>),
    keep_left_out: bool,
    cancelled: &dyn Fn() -> bool,
    hits: &mut Vec<Hit>,
) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    if matcher.is_empty() || followable.is_empty() {
        return Ok(stats);
    }
    let mut stmt = conn.prepare(
        "SELECT p.epg_channel_id, p.start, p.stop, p.flags, p.title, p.sub_title, p.categories, p.descr, p.season, p.episode
         FROM programme p JOIN epg_source s ON s.id = p.epg_source_id AND p.gen = s.active_gen AND s.enabled = 1
         WHERE p.stop > ?1 AND p.start < ?2",
    )?;
    let mut rows = stmt.query(params![from, until.unwrap_or(i64::MAX)])?;
    let (mut scratch, mut verdicts) = (Scratch::default(), Vec::new());
    while let Some(r) = rows.next()? {
        stats.rows += 1;
        if stats.rows.is_multiple_of(8192) && cancelled() {
            return Err(Error::Cancelled);
        }
        let epg: i64 = r.get(0)?;
        if !followable.contains(epg) {
            continue;
        }
        let text = |i: usize| -> rusqlite::Result<Option<&str>> {
            match r.get_ref(i)? {
                ValueRef::Text(t) => Ok(std::str::from_utf8(t).ok()),
                _ => Ok(None),
            }
        };
        let row = GuideRow {
            title: text(4)?.unwrap_or(""),
            sub_title: text(5)?,
            categories: text(6)?,
            description: text(7)?,
            flags: r.get::<_, i64>(3)? as u32,
            season: r.get(8)?,
            episode: r.get(9)?,
        };
        matcher.evaluate(&row, &mut scratch, &mut verdicts);
        for (rule, verdict) in verdicts.drain(..) {
            if verdict.is_err() && !keep_left_out {
                continue;
            }
            stats.hits += 1;
            hits.push(Hit {
                rule,
                epg_channel: epg,
                start: r.get(1)?,
                stop: r.get(2)?,
                title: row.title.to_owned(),
                sub_title: row.sub_title.map(str::to_owned),
                categories: row.categories.map(str::to_owned),
                season: row.season,
                episode: row.episode,
                verdict,
            });
        }
    }
    Ok(stats)
}

/// A set of guide channel ids (a bitset: the scan asks it for every programme).
#[derive(Debug, Default, Clone)]
pub struct EpgSet(Vec<u64>);

impl EpgSet {
    pub fn new(ids: impl IntoIterator<Item = i64>) -> Self {
        let mut bits: Vec<u64> = Vec::new();
        for id in ids.into_iter().filter(|i| *i >= 0) {
            let (w, b) = ((id / 64) as usize, id % 64);
            if bits.len() <= w {
                bits.resize(w + 1, 0);
            }
            bits[w] |= 1 << b;
        }
        Self(bits)
    }

    pub fn contains(&self, id: i64) -> bool {
        id >= 0 && self.0.get((id / 64) as usize).is_some_and(|w| w & (1 << (id % 64)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }
}

// ------------------------------------------------------------------ hooks

/// A reminder is being removed (B6). `why == User | Agent` on a rule's reminder → its ledger
/// row becomes `skipped` (never re-created). Must not fail when the ledger is empty.
pub fn on_reminder_removed(conn: &Connection, id: ReminderId, why: RemoveWhy) -> Result<()> {
    if matches!(why, RemoveWhy::User | RemoveWhy::Agent) {
        conn.execute(
            "UPDATE follow_match SET state = 'skipped', reminder_id = NULL WHERE reminder_id = ?1 AND state IN ('scheduled', 'adopted')",
            [id],
        )?;
    }
    Ok(())
}

/// The user edited a rule reminder's lead or mode: detach it from its rule (B6 point 5).
pub fn on_reminder_edited(conn: &Connection, id: ReminderId) -> Result<()> {
    conn.execute("UPDATE reminder SET rule_id = NULL WHERE id = ?1", [id])?;
    conn.execute("UPDATE follow_match SET state = 'adopted' WHERE reminder_id = ?1 AND state = 'scheduled'", [id])?;
    Ok(())
}

/// Move ledger rows of one channel by `delta_s` (B8). Returns how many moved.
pub fn retime(conn: &Connection, source: SourceId, channel_key: &str, delta_s: i64) -> Result<usize> {
    if delta_s == 0 {
        return Ok(0);
    }
    Ok(conn.execute(
        "UPDATE OR IGNORE follow_match SET start = start + ?3, stop = stop + ?3
         WHERE source_id = ?1 AND channel_key = ?2 AND state NOT IN ('aired', 'gone')",
        params![source, channel_key, delta_s],
    )?)
}

/// Mark past rows `aired` and prune (`jobs::maintenance::prune_features`): rows of show and
/// new-only rules are kept two years (new-only remembers episodes), others 30 days after their
/// stop. Returns how many rows were removed.
pub fn prune(conn: &Connection, now: Timestamp) -> Result<usize> {
    conn.execute("UPDATE follow_match SET state = 'aired' WHERE stop <= ?1 AND state IN ('scheduled', 'adopted', 'listed', 'over-cap')", [now])?;
    Ok(conn.execute(
        "DELETE FROM follow_match WHERE id IN (
           SELECT m.id FROM follow_match m LEFT JOIN follow_rule r ON r.id = m.rule_id
           WHERE r.id IS NULL
              OR m.stop < ?1 - CASE WHEN r.kind = 'show' OR json_extract(r.spec, '$.airings') = 'new-only' THEN ?2 ELSE ?3 END)",
        params![now, SHOW_KEEP_S, KEEP_S],
    )?)
}

/// A news flash reminder at fire time (the scheduler asks before it switches): its rule's
/// flash settings and the other channels showing the bulletin, best first (on a one-connection
/// account the flash takes another playlist's copy, `follow::flash::pick`).
#[derive(Debug, Clone, PartialEq)]
pub struct FlashFire {
    pub rule: FollowRuleId,
    pub name: String,
    pub spec: FlashSpec,
    pub alternates: Vec<ItemRef>,
}

pub fn flash_of(conn: &Connection, reminder: ReminderId) -> Result<Option<FlashFire>> {
    let row: Option<(FollowRuleId, String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT r.id, r.name, r.mode, r.spec, (SELECT m.alternates FROM follow_match m WHERE m.reminder_id = ?1 LIMIT 1)
             FROM reminder x JOIN follow_rule r ON r.id = x.rule_id WHERE x.id = ?1",
            [reminder],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((rule, name, mode, spec, alternates)) = row else { return Ok(None) };
    if enum_parse::<FollowMode>(&mode) != Some(FollowMode::Flash) {
        return Ok(None);
    }
    let spec: FollowSpec = serde_json::from_str(&spec).unwrap_or_default();
    Ok(Some(FlashFire {
        rule,
        name,
        spec: spec.flash.unwrap_or_default(),
        alternates: alternates.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default(),
    }))
}

// ------------------------------------------------------------------ backup

/// One rule in a backup (no id, no ledger).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct BackupRule {
    name: String,
    kind: FollowKind,
    spec: FollowSpec,
    mode: FollowMode,
    lead_s: i64,
    offset_s: i64,
    spoiler_safe: bool,
    max_per_week: u32,
    enabled: bool,
    by_agent: bool,
}

/// Rules for a backup (not the ledger).
pub fn export(conn: &Connection) -> Result<Option<serde_json::Value>> {
    let rules = rules(conn)?;
    if rules.is_empty() {
        return Ok(None);
    }
    let out: Vec<BackupRule> = rules
        .into_iter()
        .map(|r| BackupRule {
            name: r.name,
            kind: r.kind,
            spec: r.spec,
            mode: r.mode,
            lead_s: r.lead_s,
            offset_s: r.offset_s,
            spoiler_safe: r.spoiler_safe,
            max_per_week: r.max_per_week,
            enabled: r.enabled,
            by_agent: r.by_agent,
        })
        .collect();
    Ok(Some(serde_json::to_value(out).map_err(Error::other)?))
}

/// Restore rules (merged by name + spec; channel refs remapped, a rule whose channel list
/// lost every channel is skipped rather than widened to any channel).
pub fn import(conn: &Connection, v: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    let rules: Vec<BackupRule> = serde_json::from_value(v.clone()).map_err(Error::other)?;
    let existing: HashSet<(String, String)> =
        self::rules(conn)?.into_iter().map(|r| (r.name, serde_json::to_string(&r.spec).unwrap_or_default())).collect();
    let mut n = 0;
    let now = crate::model::now();
    for mut b in rules {
        if let crate::follow::ChannelScope::Channels(refs) = &b.spec.channels {
            let mapped: Vec<ItemRef> = refs.iter().filter_map(remap).collect();
            if mapped.is_empty() {
                continue;
            }
            b.spec.channels = crate::follow::ChannelScope::Channels(mapped);
        }
        b.spec.prefer.pin = b.spec.prefer.pin.as_ref().and_then(remap);
        let key = (b.name.clone(), serde_json::to_string(&b.spec).unwrap_or_default());
        if existing.contains(&key) {
            continue;
        }
        let rule = FollowRule {
            id: FollowRuleId(0),
            name: b.name,
            kind: b.kind,
            spec: b.spec,
            mode: b.mode,
            lead_s: b.lead_s,
            offset_s: b.offset_s,
            spoiler_safe: b.spoiler_safe,
            max_per_week: b.max_per_week,
            priority: i64::MAX,
            enabled: b.enabled,
            by_agent: b.by_agent,
            created_at: now,
            updated_at: now,
        };
        if save_rule(conn, &rule, u32::MAX, now).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        crate::store::migrations::migrate(&mut c).unwrap();
        c
    }

    fn seed_channel(c: &Connection) -> ItemRef {
        c.execute("INSERT INTO source(name, kind, url, created_at, generation) VALUES ('TV', 'm3u-url', 'http://x/p.m3u', 0, 1)", []).unwrap();
        let src = SourceId(c.last_insert_rowid());
        c.execute(
            "INSERT INTO channel(source_id, key, name, clean_name, norm_name, stream, url_key, provider_order, seen_gen)
             VALUES (?1, 'k1', 'Sky Sports', 'Sky Sports', 'skysports', '{}', 'u1', 0, 1)",
            [src],
        )
        .unwrap();
        ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey("k1".into()) }
    }

    fn up(rule: FollowRuleId, ch: &ItemRef, start: Timestamp) -> Upcoming {
        Upcoming {
            rule,
            event_key: format!("arsenal|chelsea@{start}"),
            start,
            stop: start + 7200,
            title: "Arsenal v Chelsea".into(),
            sub_title: None,
            channel: Some(ch.clone()),
            channel_name: "Sky Sports".into(),
            alternates: Vec::new(),
            state: MatchState::Scheduled,
            mode: FollowMode::Notify,
            note: None,
        }
    }

    /// The hooks other tracks call: removing a rule reminder yourself skips its match for
    /// good (a rule's own cancel doesn't); editing one detaches it; deleting a rule cancels
    /// its reminders but never one you had already; a spoiler-safe rule's reminder carries
    /// only the rule name.
    #[test]
    fn reminder_hooks_and_rule_lifecycle() {
        let c = db();
        let ch = seed_channel(&c);
        let mut r = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &Default::default(), 0);
        r.spoiler_safe = true;
        r.id = save_rule(&c, &r, 100, 0).unwrap();
        let start = 2_000_000_000;
        // your own reminder at the second match's time is adopted
        let mine = crate::store::user::add_reminder(
            &c,
            &Reminder::notify(ch.clone(), "Sky Sports".into(), "Mine".into(), start + 86_400, start + 90_000, 300),
        )
        .unwrap();
        let plan = Plan {
            steps: [start, start + 86_400, start + 2 * 86_400]
                .iter()
                .map(|&s| crate::follow::plan::Step { row: None, to: up(r.id, &ch, s), reminder: ReminderOp::Create, moved_from: None })
                .collect(),
            left_out: Vec::new(),
        };
        let done = apply_plan(&c, &plan, std::slice::from_ref(&r), 0).unwrap();
        assert_eq!((done.reminders_set, done.adopted), (2, 1));
        let rows = ledger(&c, &|_| true).unwrap();
        assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), [MatchState::Scheduled, MatchState::Adopted, MatchState::Scheduled]);
        let first = crate::store::user::reminder(&c, rows[0].reminder.unwrap()).unwrap().unwrap();
        assert_eq!((first.programme_title.as_str(), first.rule_id), ("Arsenal", Some(r.id)), "spoiler-safe: the rule name only");

        crate::store::user::remove_reminder(&c, rows[0].reminder.unwrap(), RemoveWhy::User).unwrap();
        on_reminder_edited(&c, rows[2].reminder.unwrap()).unwrap();
        let rows = ledger(&c, &|_| true).unwrap();
        assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), [MatchState::Skipped, MatchState::Adopted, MatchState::Adopted]);
        assert_eq!(crate::store::user::reminder(&c, rows[2].reminder.unwrap()).unwrap().unwrap().rule_id, None, "edited: yours now");

        assert_eq!(delete_rule(&c, r.id, false).unwrap(), 0, "nothing it still owns");
        assert!(crate::store::user::reminder(&c, mine).unwrap().is_some(), "your reminder survives the rule");
        assert!(ledger(&c, &|_| true).unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_shows_longer() {
        let c = db();
        let ch = seed_channel(&c);
        let team = save_rule(&c, &FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &Default::default(), 0), 100, 0).unwrap();
        let show =
            save_rule(&c, &FollowRule::draft("Taskmaster", FollowKind::Show, vec!["Taskmaster".into()], &Default::default(), 0), 100, 0).unwrap();
        let now = 2_000_000_000;
        let old = now - 60 * 86_400;
        let plan = Plan {
            steps: [(team, old), (show, old), (team, now + 3600)]
                .iter()
                .map(|&(rule, s)| crate::follow::plan::Step { row: None, to: up(rule, &ch, s), reminder: ReminderOp::Keep, moved_from: None })
                .collect(),
            left_out: Vec::new(),
        };
        apply_plan(&c, &plan, &[], now).unwrap();
        assert_eq!(prune(&c, now).unwrap(), 1);
        let rows = ledger(&c, &|_| true).unwrap();
        assert_eq!(rows.iter().map(|r| (r.rule, r.state)).collect::<Vec<_>>(), [(show, MatchState::Aired), (team, MatchState::Scheduled)]);
    }

    /// A backup restore merges rules by name and spec (a second restore adds nothing) and maps
    /// channel lists to the restored channels, dropping a rule whose channels are all gone.
    #[test]
    fn backup_round_trip_merges() {
        let c = db();
        let ch = seed_channel(&c);
        let mut pinned = FollowRule::draft("On Sky", FollowKind::Event, vec!["Formula 1".into()], &Default::default(), 0);
        pinned.spec.channels = crate::follow::ChannelScope::Channels(vec![ch.clone()]);
        save_rule(&c, &pinned, 100, 0).unwrap();
        save_rule(&c, &FollowRule::draft("Taskmaster", FollowKind::Show, vec!["Taskmaster".into()], &Default::default(), 0), 100, 0).unwrap();
        let v = export(&c).unwrap().unwrap();
        let fresh = db();
        let moved = ItemRef { key: ItemKey("k9".into()), ..ch.clone() };
        assert_eq!(import(&fresh, &v, &|r| (r == &ch).then(|| moved.clone())).unwrap(), 2);
        assert_eq!(import(&fresh, &v, &|r| (r == &ch).then(|| moved.clone())).unwrap(), 0, "merged by name and spec");
        assert_eq!(rules(&fresh).unwrap()[0].spec.channels, crate::follow::ChannelScope::Channels(vec![moved]));
        assert_eq!(import(&db(), &v, &|_| None).unwrap(), 1, "a channel list with no channel left isn't widened to any channel");
    }

    #[test]
    fn epg_set() {
        let s = EpgSet::new([0, 63, 64, 9_300]);
        assert!(s.contains(0) && s.contains(63) && s.contains(64) && s.contains(9_300));
        assert!(!s.contains(1) && !s.contains(9_301) && !s.contains(-1) && !s.contains(1_000_000));
        assert!(EpgSet::new([]).is_empty());
    }
}
