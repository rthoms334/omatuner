//! Guide fixes storage (migration 0009, DESIGN §31): the logged batches that Undo reverts
//! (`guide_fix_log` / `guide_fix_change`), the suggestion cache (`epg_suggestion`), the
//! per-playlist rules' derived mapping rows, and the facts the issue list is classified from.
//! OWNER: G-core.
//!
//! * **Rule rows** ([`derive_rule_rows`], B7: after every rematch). "+1/+2 follow their base"
//!   gives a "+N" channel its base's guide channel with a time shift: a `sibling` row when it
//!   has no guide channel, or the shift on its automatic row when that row already points at
//!   the base's (a provider reusing the base's tvg-id). "Copies share a guide" gives a channel
//!   without one the guide channel of the same channel in another playlist (`copy` rows, the
//!   `store::copies` family key). Rules only ever fill gaps and never touch manual rows.
//! * **Batches** ([`apply`], [`undo`]). Every write is one logged batch (actor, a label without
//!   titles, n). Undo restores each field only while it still holds what the batch wrote, and
//!   names what it skipped. A change of a channel's effective time shift moves its pending
//!   reminders, Up next slots and Following rows in the same transaction (B8,
//!   `store::retime::channel_shift`). Callers emit `DataScope::Epg` afterwards and submit
//!   `JobKind::EvaluateFollows { reason: Mapping }` (B7).
//! * **Suggestions** are a derived cache (no fp, rebuilt on a stamp mismatch): the top 3 fuzzy
//!   candidates per unmapped channel the guide check looked at.

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};
use crate::model::{
    ChannelId, EpgChannelId, EpgMatchMethod, EpgSourceId, EpgSuggestion, FixActor, GuideFixLogId, GuideRules, GuideTarget, ItemKey, ItemKind,
    ItemRef, Proposal, SourceId, Timestamp,
};
use crate::sources::epg_match::{Resolved, RuleChannel, RuleIndex, Sibling};
use crate::store::sql::{self, atomic, enum_parse, enum_str, from_json_opt};

/// Logs kept: 90 days or 200 batches.
pub const LOG_KEEP_S: i64 = 90 * 86_400;
pub const LOG_KEEP_N: usize = 200;
/// Channel changes one batch from Claude may hold (50 on "yes", more opens the confirm card).
pub const MAX_BATCH: usize = 200;
/// The error text of a proposal made before the guides, playlists or rules changed.
pub const STALE: &str = "stale: the guide data changed since this was proposed";
/// Suggestions kept per channel.
pub const SUGGESTIONS: usize = 3;
/// `meta` key of the stamp the suggestion cache was built for.
const STAMP_KEY: &str = "guide_check:stamp";

/// The config the rules and the cache depend on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideOpts {
    /// `config.epg.rules_default`: the rules of playlists that don't set their own.
    pub rules_default: bool,
    /// `config.epg.fuzzy_threshold`.
    pub fuzzy_threshold: f64,
}

impl GuideOpts {
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self { rules_default: c.epg.rules_default, fuzzy_threshold: c.epg.fuzzy_threshold }
    }
}

impl Default for GuideOpts {
    fn default() -> Self {
        Self::from_config(&crate::config::Config::default())
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyReport {
    /// `None` when nothing changed (no batch is logged).
    pub log: Option<GuideFixLogId>,
    /// Channels changed (plus playlists whose rules changed).
    pub applied: usize,
    /// (channel key, reason) of changes left out (gone…).
    pub skipped: Vec<(String, String)>,
    /// Pending reminders that moved with a time shift.
    pub reminders_moved: usize,
    /// A mapping changed: the caller submits `EvaluateFollows { reason: Mapping }` (B7).
    pub mappings_changed: bool,
}

/// What [`undo`] did: changes made since are skipped and named.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UndoReport {
    pub reverted: usize,
    pub skipped: Vec<(String, String)>,
    pub reminders_moved: usize,
}

/// What [`derive_rule_rows`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeriveReport {
    /// `sibling` rows written ("+N" channels with no guide channel of their own).
    pub siblings: usize,
    /// Automatic rows given their base's time shift ("+N" channels reusing the base's tvg-id).
    pub shifted: usize,
    /// `copy` rows written.
    pub copies: usize,
    /// Pending reminders that moved because a rule changed a channel's shift.
    pub reminders_moved: usize,
}

// ------------------------------------------------------------------ rule channels

/// The `SELECT` of [`rule_channels`] over a resolution `{ce}` (`channel_id, epg_channel_id,
/// shift_min`).
fn rule_channels_sql(ce: &str) -> String {
    format!(
        "SELECT c.id, c.source_id, c.key, c.name, c.tvg_name, c.norm_name, COALESCE(c.family, c.norm_name), c.tvg_id,
                c.unsupported IS NULL AND COALESCE(o.hidden, 0) = 0 AND s.enabled = 1,
                ce.epg_channel_id, ec.epg_source_id, ec.xmltv_id, m.method, ce.shift_min
         FROM channel c
         JOIN source s ON s.id = c.source_id
         LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
         LEFT JOIN {ce} ce ON ce.channel_id = c.id
         LEFT JOIN epg_channel ec ON ec.id = ce.epg_channel_id
         LEFT JOIN channel_epg_map m ON m.source_id = c.source_id AND m.channel_key = c.key AND m.epg_source_id = ec.epg_source_id
         ORDER BY s.sort_index, s.id, c.provider_order, c.id"
    )
}

fn rule_channels_with(conn: &Connection, sql: &str) -> Result<Vec<RuleChannel>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([], |r| {
        let epg: Option<EpgChannelId> = r.get(9)?;
        let resolved = match epg {
            Some(epg_channel) => Some(Resolved {
                epg_channel,
                epg_source: r.get(10)?,
                xmltv_id: r.get(11)?,
                method: r.get::<_, Option<String>>(12)?.and_then(|m| enum_parse(&m)).unwrap_or(EpgMatchMethod::Fuzzy),
                shift_min: r.get(13)?,
            }),
            None => None,
        };
        Ok(RuleChannel {
            id: r.get(0)?,
            source: r.get(1)?,
            key: ItemKey(r.get(2)?),
            name: r.get(3)?,
            tvg_name: r.get::<_, Option<String>>(4)?.filter(|s| !s.trim().is_empty()),
            norm: r.get(5)?,
            family: r.get(6)?,
            tvg_id: r.get::<_, Option<String>>(7)?.filter(|s| !s.trim().is_empty()),
            usable: r.get(8)?,
            resolved,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every playlist channel with its resolved guide channel (`channel_epg`), in playlist order
/// then provider order (the input of the issue list).
pub fn rule_channels(conn: &Connection) -> Result<Vec<RuleChannel>> {
    rule_channels_with(conn, &rule_channels_sql("channel_epg"))
}

/// [`rule_channels`] as the matcher and the user resolve them, without any rule row or rule
/// shift (the input of the derivation; read-only, so it runs on a reader).
pub fn base_rule_channels(conn: &Connection) -> Result<Vec<RuleChannel>> {
    let base = crate::store::epg::resolve_select("AND m.method NOT IN ('sibling', 'copy')");
    let mut chans = rule_channels_with(conn, &rule_channels_sql(&format!("({base})")))?;
    for r in chans.iter_mut().filter_map(|c| c.resolved.as_mut()) {
        r.shift_min = None;
    }
    Ok(chans)
}

/// Every playlist's rules as stored (`SourceSettings::guide`).
pub fn guide_rules(conn: &Connection) -> Result<HashMap<SourceId, GuideRules>> {
    let mut stmt = conn.prepare_cached("SELECT id, settings FROM source")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, settings) = row?;
        let s: crate::model::SourceSettings = from_json_opt(Some(settings)).unwrap_or_default();
        out.insert(id, s.guide);
    }
    Ok(out)
}

/// Whether any display name of guide channel `id` carries a "+N" marker (the guide lists the
/// "+1" channel itself).
fn guide_marked(conn: &Connection, id: EpgChannelId) -> Result<bool> {
    let mut stmt = conn.prepare_cached("SELECT name FROM epg_display_name WHERE epg_channel_id = ?1")?;
    let names = stmt.query_map([id], |r| r.get::<_, String>(0))?;
    for n in names {
        if crate::textmatch::timeshift::marker(&n?).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Channels whose effective shift comes from a rule row now: `(source, key) → (rule shift,
/// provider shift)` (channels with a shift of their own are left out: rules don't move them).
fn rule_shifts(conn: &Connection) -> Result<HashMap<(SourceId, String), (i32, i32)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.source_id, c.key, ce.shift_min, c.tvg_shift_min FROM channel_epg ce
         JOIN channel c ON c.id = ce.channel_id
         LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
         WHERE ce.shift_min IS NOT NULL AND o.shift_min IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One derived write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleWrite {
    /// A `sibling` or `copy` row.
    Row { source: SourceId, key: String, epg_source: EpgSourceId, xmltv_id: String, method: EpgMatchMethod, shift_min: Option<i32> },
    /// The base's time shift on the channel's own automatic row (a "+1" reusing its base's
    /// tvg-id).
    Shift { source: SourceId, key: String, epg_source: EpgSourceId, shift_min: i32 },
}

/// What the rules derive from the mappings as matched now ([`plan_rule_rows`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RulePlan {
    pub writes: Vec<RuleWrite>,
}

/// Run both rules over the matcher's and the user's mappings (read-only: a reader connection,
/// so the writer is held only by [`write_rule_rows`]).
pub fn plan_rule_rows(conn: &Connection, opts: &GuideOpts) -> Result<RulePlan> {
    let rules = guide_rules(conn)?;
    let on = |s: SourceId, which: fn(&GuideRules, bool) -> bool| which(&rules.get(&s).copied().unwrap_or_default(), opts.rules_default);
    let mut chans = base_rule_channels(conn)?;
    let mut plan = RulePlan::default();

    // "+1/+2 follow their base"
    let mut follows: Vec<(usize, usize, i32, bool)> = Vec::new();
    {
        let idx = RuleIndex::new(&chans);
        let mut marked_cache: HashMap<EpgChannelId, bool> = HashMap::new();
        let mut err = None;
        let mut marked = |id: EpgChannelId| -> bool {
            *marked_cache.entry(id).or_insert_with(|| {
                guide_marked(conn, id).unwrap_or_else(|e| {
                    err = Some(e);
                    true
                })
            })
        };
        for (i, ch) in chans.iter().enumerate() {
            if on(ch.source, GuideRules::siblings_on)
                && let Sibling::Follows { base, minutes, already } = idx.sibling(i, &mut marked)
            {
                follows.push((i, base, minutes, already));
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
    }
    for (i, base, minutes, already) in follows {
        let Some(target) = chans[base].resolved.clone() else { continue };
        let ch = &chans[i];
        let (source, key) = (ch.source, ch.key.as_str().to_owned());
        let method = if already {
            let own = ch.resolved.as_ref().map(|r| r.method);
            if own == Some(EpgMatchMethod::Manual) {
                continue;
            }
            plan.writes.push(RuleWrite::Shift { source, key, epg_source: target.epg_source, shift_min: minutes });
            own.unwrap_or(EpgMatchMethod::Sibling)
        } else {
            plan.writes.push(RuleWrite::Row {
                source,
                key,
                epg_source: target.epg_source,
                xmltv_id: target.xmltv_id.clone(),
                method: EpgMatchMethod::Sibling,
                shift_min: Some(minutes),
            });
            EpgMatchMethod::Sibling
        };
        chans[i].resolved = Some(Resolved { method, shift_min: Some(minutes), ..target });
    }

    // "Copies share a guide" (after the siblings, so a "+1" copy lends its shifted guide)
    let idx = RuleIndex::new(&chans);
    for (i, ch) in chans.iter().enumerate() {
        if ch.resolved.is_some() || !on(ch.source, GuideRules::copies_on) {
            continue;
        }
        if let Some(r) = idx.copy_of(i).and_then(|j| chans[j].resolved.as_ref()) {
            plan.writes.push(RuleWrite::Row {
                source: ch.source,
                key: ch.key.as_str().to_owned(),
                epg_source: r.epg_source,
                xmltv_id: r.xmltv_id.clone(),
                method: EpgMatchMethod::Copy,
                shift_min: r.shift_min,
            });
        }
    }
    Ok(plan)
}

/// Replace every rule row and rule shift with `plan`, then resolve; pending reminders (Up next
/// slots, Following rows) of channels whose shown times moved move with them (B8). One
/// transaction, about 0.1 s at 79k channels.
pub fn write_rule_rows(conn: &Connection, plan: &RulePlan) -> Result<DeriveReport> {
    atomic(conn, |c| {
        let before = rule_shifts(c)?;
        c.execute("DELETE FROM channel_epg_map WHERE method IN ('sibling', 'copy')", [])?;
        c.execute("UPDATE channel_epg_map SET shift_min = NULL WHERE shift_min IS NOT NULL AND method != 'manual'", [])?;
        let mut report = DeriveReport::default();
        {
            let mut shift = c.prepare_cached(
                "UPDATE channel_epg_map SET shift_min = ?4 WHERE source_id = ?1 AND channel_key = ?2 AND epg_source_id = ?3 AND method != 'manual'",
            )?;
            // a manual row (made since the plan was read) wins
            let mut ins = c.prepare_cached(
                "INSERT INTO channel_epg_map(source_id, channel_key, epg_source_id, xmltv_id, method, confidence, shift_min)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1.0, ?6)
                 ON CONFLICT(source_id, channel_key, epg_source_id) DO UPDATE SET
                   xmltv_id = excluded.xmltv_id, method = excluded.method, confidence = 1.0, shift_min = excluded.shift_min,
                   epg_name = NULL, actor = NULL
                 WHERE channel_epg_map.method != 'manual'",
            )?;
            for w in &plan.writes {
                match w {
                    RuleWrite::Shift { source, key, epg_source, shift_min } => {
                        report.shifted += shift.execute(params![source, key, epg_source, shift_min])?;
                    }
                    RuleWrite::Row { source, key, epg_source, xmltv_id, method, shift_min } => {
                        let n = ins.execute(params![source, key, epg_source, xmltv_id, enum_str(method), shift_min])?;
                        match method {
                            EpgMatchMethod::Copy => report.copies += n,
                            _ => report.siblings += n,
                        }
                    }
                }
            }
        }
        crate::store::epg::resolve_mappings(c)?;

        // B8: the shown times of these channels moved; their reminders, slots and rows move too
        let after = rule_shifts(c)?;
        let keys: HashSet<&(SourceId, String)> = before.keys().chain(after.keys()).collect();
        for k in keys {
            let tvg = before.get(k).or(after.get(k)).map_or(0, |v| v.1);
            let was = before.get(k).map_or(tvg, |v| v.0);
            let now = after.get(k).map_or(tvg, |v| v.0);
            if was != now {
                report.reminders_moved += crate::store::retime::channel_shift(c, k.0, &k.1, i64::from(now - was) * 60)?.reminders;
            }
        }
        if report.siblings + report.shifted + report.copies > 0 {
            tracing::info!(
                target: "tuner::guide",
                "guide rules: {} +N channels follow their base, {} copies share a guide",
                report.siblings + report.shifted,
                report.copies
            );
        }
        Ok(report)
    })
}

/// Re-derive every rule row ("+1/+2 follow their base", "Copies share a guide") from the
/// mappings as matched now, then resolve: [`plan_rule_rows`] + [`write_rule_rows`] on one
/// connection, in one transaction (a rule change in [`apply`] / [`undo`]). The jobs (B7: after
/// `rematch_all` and a guide import's flip) plan on a reader and write on the writer.
pub fn derive_rule_rows(conn: &Connection, opts: &GuideOpts) -> Result<DeriveReport> {
    atomic(conn, |c| write_rule_rows(c, &plan_rule_rows(c, opts)?))
}

// ------------------------------------------------------------------ stamp + suggestion cache

/// What the guide check's results depend on: the enabled guides' active generations, the
/// playlists' catalogue generations, the fuzzy threshold and the rules. A proposal bound to an
/// older stamp is [`STALE`].
pub fn stamp(conn: &Connection, opts: &GuideOpts) -> Result<[u8; 16]> {
    let mut h = blake3::Hasher::new();
    for (id, generation) in crate::guide_fix::catalog::generations(conn)? {
        h.update(&id.0.to_le_bytes());
        h.update(&generation.to_le_bytes());
    }
    h.update(b"|");
    let mut stmt = conn.prepare_cached("SELECT id, generation, settings FROM source ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (id, generation, settings) = row?;
        let s: crate::model::SourceSettings = from_json_opt(Some(settings)).unwrap_or_default();
        h.update(&id.to_le_bytes());
        h.update(&generation.to_le_bytes());
        h.update(&[u8::from(s.guide.siblings_on(opts.rules_default)), u8::from(s.guide.copies_on(opts.rules_default))]);
    }
    h.update(&opts.fuzzy_threshold.to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    Ok(out)
}

/// The stamp the suggestion cache was last built for.
pub fn cached_stamp(conn: &Connection) -> Result<Option<[u8; 16]>> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [STAMP_KEY], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| {
        let bytes: Vec<u8> = (0..s.len()).step_by(2).filter_map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
        bytes.try_into().ok()
    }))
}

pub fn set_cached_stamp(conn: &Connection, stamp: &[u8; 16]) -> Result<()> {
    let hex: String = stamp.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute("INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![STAMP_KEY, hex])?;
    Ok(())
}

/// Forget every cached suggestion (the stamp changed).
pub fn clear_suggestions(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM epg_suggestion", [])?)
}

/// The channels with cached suggestions: `(source, channel key)`.
pub fn suggested_keys(conn: &Connection) -> Result<HashSet<(SourceId, String)>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT source_id, channel_key FROM epg_suggestion")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Replace the cached suggestions of these channels (≤ [`SUGGESTIONS`] each, best first).
pub fn write_suggestions(conn: &Connection, rows: &[(ItemRef, Vec<EpgSuggestion>)]) -> Result<()> {
    atomic(conn, |c| {
        let mut del = c.prepare_cached("DELETE FROM epg_suggestion WHERE source_id = ?1 AND channel_key = ?2")?;
        let mut ins = c.prepare_cached(
            "INSERT INTO epg_suggestion(source_id, channel_key, rank, epg_source_id, xmltv_id, display_name, score) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (ch, sugg) in rows {
            del.execute(params![ch.source, ch.key.as_str()])?;
            for (rank, s) in sugg.iter().take(SUGGESTIONS).enumerate() {
                ins.execute(params![ch.source, ch.key.as_str(), rank as i64, s.epg_source_id, s.xmltv_id, s.display_name, f64::from(s.score)])?;
            }
        }
        Ok(())
    })
}

/// Every cached suggestion whose guide channel is still in its guide: `(source, key) → best
/// first`.
pub fn suggestions(conn: &Connection) -> Result<HashMap<(SourceId, String), Vec<EpgSuggestion>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.source_id, s.channel_key, ec.id, s.epg_source_id, s.xmltv_id, s.display_name, s.score
         FROM epg_suggestion s
         JOIN epg_source es ON es.id = s.epg_source_id AND es.enabled = 1
         JOIN epg_channel ec ON ec.epg_source_id = s.epg_source_id AND ec.xmltv_id = s.xmltv_id AND ec.seen_gen >= es.active_gen
         ORDER BY s.source_id, s.channel_key, s.rank",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?),
            EpgSuggestion {
                epg_channel_id: r.get(2)?,
                epg_source_id: r.get(3)?,
                xmltv_id: r.get(4)?,
                display_name: r.get(5)?,
                score: r.get::<_, f64>(6)? as f32,
            },
        ))
    })?;
    let mut out: HashMap<(SourceId, String), Vec<EpgSuggestion>> = HashMap::new();
    for row in rows {
        let (k, s) = row?;
        out.entry(k).or_default().push(s);
    }
    Ok(out)
}

// ------------------------------------------------------------------ facts for the issue list

/// A channel's own Guide fixes data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelExtra {
    /// The user's time shift (`item_override.shift_min`).
    pub shift_min: Option<i32>,
    pub catchup_offset_s: Option<i64>,
    /// Dismissed issue kinds (`IssueKind::bit`).
    pub guide_ok: i64,
    pub manual: Option<ManualRow>,
}

/// A manual mapping row.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualRow {
    pub epg_source: EpgSourceId,
    pub xmltv_id: String,
    /// The guide channel's name when it was chosen.
    pub epg_name: Option<String>,
    /// Its guide channel is in the guide's active generation.
    pub live: bool,
}

/// What the issue list is classified from (`guide_fix::issues::classify`): read on a reader
/// connection, off the UI thread (about 0.3 s at 79k channels).
#[derive(Debug, Clone, Default)]
pub struct GuideFacts {
    pub chans: Vec<RuleChannel>,
    pub index: HashMap<ChannelId, u32>,
    pub extras: HashMap<ChannelId, ChannelExtra>,
    pub suggestions: HashMap<ChannelId, Vec<EpgSuggestion>>,
    /// Resolved guide channels with a programme now or later (the probes).
    pub airing: HashSet<EpgChannelId>,
    /// `(name, norm)` of each resolved guide channel's display names.
    pub guide_names: HashMap<EpgChannelId, Vec<(String, String)>>,
    pub rules: HashMap<SourceId, GuideRules>,
    pub opts: GuideOpts,
    /// The stamp now, and the one the suggestion cache was built for (`scan: fresh | stale`).
    pub stamp: [u8; 16],
    pub cached_stamp: Option<[u8; 16]>,
}

impl GuideFacts {
    pub fn channel(&self, id: ChannelId) -> Option<&RuleChannel> {
        self.index.get(&id).map(|&i| &self.chans[i as usize])
    }

    /// The suggestion cache matches the guides as they are now.
    pub fn fresh(&self) -> bool {
        self.cached_stamp == Some(self.stamp)
    }
}

/// Load the facts at `now`.
pub fn facts(conn: &Connection, opts: &GuideOpts, now: Timestamp) -> Result<GuideFacts> {
    let chans = rule_channels(conn)?;
    let index: HashMap<ChannelId, u32> = chans.iter().enumerate().map(|(i, c)| (c.id, i as u32)).collect();
    let by_key: HashMap<(SourceId, &str), ChannelId> = chans.iter().map(|c| ((c.source, c.key.as_str()), c.id)).collect();

    let mut extras: HashMap<ChannelId, ChannelExtra> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT source_id, item_key, shift_min, catchup_offset_s, guide_ok FROM item_override
             WHERE kind = 'channel' AND (shift_min IS NOT NULL OR catchup_offset_s IS NOT NULL OR guide_ok != 0)",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, SourceId>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i32>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, i64>(4)?))
        })?;
        for row in rows {
            let (s, k, shift_min, catchup_offset_s, guide_ok) = row?;
            if let Some(id) = by_key.get(&(s, k.as_str())) {
                let e = extras.entry(*id).or_default();
                (e.shift_min, e.catchup_offset_s, e.guide_ok) = (shift_min, catchup_offset_s, guide_ok);
            }
        }
        let mut stmt = conn.prepare_cached(
            "SELECT m.source_id, m.channel_key, m.epg_source_id, m.xmltv_id, m.epg_name,
                    EXISTS (SELECT 1 FROM epg_channel ec JOIN epg_source es ON es.id = ec.epg_source_id
                            WHERE ec.epg_source_id = m.epg_source_id AND ec.xmltv_id = m.xmltv_id AND ec.seen_gen >= es.active_gen)
             FROM channel_epg_map m WHERE m.method = 'manual' AND m.orphaned_since IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, SourceId>(0)?,
                r.get::<_, String>(1)?,
                ManualRow { epg_source: r.get(2)?, xmltv_id: r.get(3)?, epg_name: r.get(4)?, live: r.get(5)? },
            ))
        })?;
        for row in rows {
            let (s, k, m) = row?;
            if let Some(id) = by_key.get(&(s, k.as_str())) {
                extras.entry(*id).or_default().manual = Some(m);
            }
        }
    }

    let suggestions: HashMap<ChannelId, Vec<EpgSuggestion>> =
        suggestions(conn)?.into_iter().filter_map(|((s, k), v)| by_key.get(&(s, k.as_str())).map(|id| (*id, v))).collect();

    let guides: HashSet<EpgChannelId> = chans.iter().filter_map(|c| c.resolved.as_ref().map(|r| r.epg_channel)).collect();
    let mut airing = HashSet::new();
    {
        let mut probe = conn.prepare_cached(
            "SELECT 1 FROM programme p WHERE p.epg_channel_id = ?1
               AND p.gen = (SELECT es.active_gen FROM epg_channel ec JOIN epg_source es ON es.id = ec.epg_source_id WHERE ec.id = ?1)
               AND p.start >= ?2 - 86400 AND p.stop > ?2 LIMIT 1",
        )?;
        for &g in &guides {
            if probe.exists(params![g, now])? {
                airing.insert(g);
            }
        }
    }
    let mut guide_names: HashMap<EpgChannelId, Vec<(String, String)>> = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT dn.epg_channel_id, dn.name, dn.norm FROM (SELECT DISTINCT epg_channel_id FROM channel_epg) ce
             JOIN epg_display_name dn ON dn.epg_channel_id = ce.epg_channel_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, EpgChannelId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for row in rows {
            let (id, name, norm) = row?;
            guide_names.entry(id).or_default().push((name, norm));
        }
    }
    Ok(GuideFacts {
        chans,
        index,
        extras,
        suggestions,
        airing,
        guide_names,
        rules: guide_rules(conn)?,
        opts: *opts,
        stamp: stamp(conn, opts)?,
        cached_stamp: cached_stamp(conn)?,
    })
}

// ------------------------------------------------------------------ batches

/// One logged batch ("Recent changes").
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub id: GuideFixLogId,
    pub at: Timestamp,
    pub actor: FixActor,
    pub label: String,
    pub n: usize,
    pub undone_at: Option<Timestamp>,
}

/// One logged field change (JSON before / after).
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedChange {
    pub source: SourceId,
    /// Empty for a playlist rule change.
    pub channel_key: String,
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The newest batches first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<LogEntry>> {
    let mut stmt = conn.prepare_cached("SELECT id, at, actor, label, n, undone_at FROM guide_fix_log ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(LogEntry {
            id: r.get(0)?,
            at: r.get(1)?,
            actor: enum_parse(&r.get::<_, String>(2)?).unwrap_or_default(),
            label: r.get(3)?,
            n: r.get::<_, i64>(4)?.max(0) as usize,
            undone_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The batch Ctrl+Z undoes: the newest one not undone yet.
pub fn last_undoable(conn: &Connection) -> Result<Option<GuideFixLogId>> {
    Ok(conn.query_row("SELECT id FROM guide_fix_log WHERE undone_at IS NULL ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).optional()?)
}

/// The changes of a batch.
pub fn changes_of(conn: &Connection, log: GuideFixLogId) -> Result<Vec<LoggedChange>> {
    let mut stmt = conn.prepare_cached(
        "SELECT source_id, channel_key, field, before, after FROM guide_fix_change WHERE log_id = ?1 ORDER BY source_id, channel_key, field",
    )?;
    let rows = stmt.query_map([log], |r| {
        Ok(LoggedChange { source: r.get(0)?, channel_key: r.get(1)?, field: r.get(2)?, before: r.get(3)?, after: r.get(4)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The fields a batch writes, as logged.
const F_MAP: &str = "map";
const F_SHIFT: &str = "shift";
const F_CATCHUP: &str = "catchup";
const F_DISMISS: &str = "dismiss";
const F_RULE: &str = "rule";

fn json<T: Serialize>(v: &Option<T>) -> Result<Option<String>> {
    v.as_ref().map(sql::to_json).transpose()
}

fn unjson<T: DeserializeOwned>(s: &Option<String>) -> Option<T> {
    from_json_opt(s.clone())
}

fn channel_exists(conn: &Connection, ch: &ItemRef) -> Result<bool> {
    Ok(conn.prepare_cached("SELECT 1 FROM channel WHERE source_id = ?1 AND key = ?2")?.exists(params![ch.source, ch.key.as_str()])?)
}

/// The channel's effective time shift (B8): its own, else its rule row's, else the provider's.
fn effective_shift(conn: &Connection, ch: &ItemRef) -> Result<i32> {
    Ok(conn
        .prepare_cached(
            "SELECT COALESCE(o.shift_min, ce.shift_min, c.tvg_shift_min) FROM channel c
             LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
             LEFT JOIN channel_epg ce ON ce.channel_id = c.id
             WHERE c.source_id = ?1 AND c.key = ?2",
        )?
        .query_row(params![ch.source, ch.key.as_str()], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

/// The channel's manual mapping as a target.
pub fn manual_target(conn: &Connection, ch: &ItemRef) -> Result<Option<GuideTarget>> {
    Ok(conn
        .prepare_cached("SELECT epg_source_id, xmltv_id, COALESCE(epg_name, '') FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual'")?
        .query_row(params![ch.source, ch.key.as_str()], |r| Ok(GuideTarget { epg_source: r.get(0)?, xmltv_id: r.get(1)?, name: r.get(2)? }))
        .optional()?)
}

/// One `item_override` column of a channel (`None`: no row or NULL).
fn override_col<T: rusqlite::types::FromSql>(conn: &Connection, ch: &ItemRef, col: &str) -> Result<Option<T>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {col} FROM item_override WHERE source_id = ?1 AND kind = 'channel' AND item_key = ?2"))?
        .query_row(params![ch.source, ch.key.as_str()], |r| r.get::<_, Option<T>>(0))
        .optional()?
        .flatten())
}

/// Write one `item_override` column (the row is created with the channel's fingerprint so it
/// reattaches like other user data; a row left all-default is removed).
fn set_override_col(conn: &Connection, ch: &ItemRef, col: &str, value: Value) -> Result<()> {
    let fp = crate::store::user::fingerprint_of(conn, ch)?;
    conn.prepare_cached(&format!(
        "INSERT INTO item_override(source_id, kind, item_key, {col}, fp) VALUES (?1, 'channel', ?2, ?3, COALESCE(?4, '{{}}'))
         ON CONFLICT(source_id, kind, item_key) DO UPDATE SET {col} = excluded.{col}, fp = COALESCE(?4, item_override.fp)"
    ))?
    .execute(params![ch.source, ch.key.as_str(), value, sql::fp_opt(&fp)])?;
    conn.prepare_cached(
        "DELETE FROM item_override WHERE source_id = ?1 AND kind = 'channel' AND item_key = ?2
           AND rename IS NULL AND hidden = 0 AND locked = 0 AND user_order IS NULL AND catchup_offset_s IS NULL
           AND shift_min IS NULL AND guide_ok = 0",
    )?
    .execute(params![ch.source, ch.key.as_str()])?;
    Ok(())
}

fn set_map(conn: &Connection, ch: &ItemRef, to: Option<&GuideTarget>, actor: FixActor) -> Result<()> {
    match to {
        Some(t) => crate::store::epg::set_manual_mapping_as(
            conn,
            ch,
            t.epg_source,
            &t.xmltv_id,
            Some(&t.name).filter(|n| !n.is_empty()).map(|s| s.as_str()),
            actor,
        ),
        None => crate::store::epg::clear_manual_mapping(conn, ch),
    }
}

fn same_target(a: &Option<GuideTarget>, b: &Option<GuideTarget>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.epg_source == b.epg_source && a.xmltv_id == b.xmltv_id,
        (None, None) => true,
        _ => false,
    }
}

/// Log one field change of a batch (a second change of the same field keeps the first
/// `before`).
fn log_change(
    conn: &Connection,
    log: GuideFixLogId,
    source: SourceId,
    key: &str,
    field: &str,
    before: Option<String>,
    after: Option<String>,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO guide_fix_change(log_id, source_id, channel_key, field, before, after) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(log_id, source_id, channel_key, field) DO UPDATE SET after = excluded.after",
    )?
    .execute(params![log, source, key, field, before, after])?;
    Ok(())
}

/// Apply a proposal as one logged batch. A proposal carrying a stamp (`Proposal::stamp` ≠ 0)
/// must match [`stamp`] now, else it is [`STALE`]; Claude's batches hold at most
/// [`MAX_BATCH`] channel changes. Changes to channels that no longer exist are skipped and
/// named. Rule changes re-derive the rule rows in the same transaction.
pub fn apply(conn: &Connection, actor: FixActor, label: &str, p: &Proposal, opts: &GuideOpts, now: Timestamp) -> Result<ApplyReport> {
    if p.stamp != [0; 16] && p.stamp != stamp(conn, opts)? {
        return Err(Error::Other(STALE.into()));
    }
    if actor == FixActor::Claude && p.changes.len() > MAX_BATCH {
        return Err(Error::Other(format!("too many changes in one batch ({} > {MAX_BATCH})", p.changes.len())));
    }
    atomic(conn, |c| {
        c.execute("INSERT INTO guide_fix_log(at, actor, label, n) VALUES (?1, ?2, ?3, 0)", params![now, enum_str(&actor), label])?;
        let log = GuideFixLogId(c.last_insert_rowid());
        let mut report = ApplyReport::default();
        for ch in &p.changes {
            let Some(r) = &ch.channel else { continue };
            if r.kind != ItemKind::Channel || !channel_exists(c, r)? {
                report.skipped.push((r.key.as_str().to_owned(), "no longer in the playlist".into()));
                continue;
            }
            let key = r.key.as_str();
            let shift0 = effective_shift(c, r)?;
            let mut changed = false;
            if let Some(to) = &ch.map {
                let before = manual_target(c, r)?;
                if !same_target(&before, to) {
                    set_map(c, r, to.as_ref(), actor)?;
                    log_change(c, log, r.source, key, F_MAP, json(&before)?, json(to)?)?;
                    changed = true;
                    report.mappings_changed = true;
                }
            }
            if let Some(to) = ch.shift_min {
                let before: Option<i32> = override_col(c, r, "shift_min")?;
                if before != to {
                    set_override_col(c, r, "shift_min", to.map_or(Value::Null, |v| Value::Integer(v.into())))?;
                    log_change(c, log, r.source, key, F_SHIFT, json(&before)?, json(&to)?)?;
                    changed = true;
                }
            }
            if let Some(to) = ch.catchup_offset_s {
                let before: Option<i64> = override_col(c, r, "catchup_offset_s")?;
                if before != to {
                    set_override_col(c, r, "catchup_offset_s", to.map_or(Value::Null, Value::Integer))?;
                    log_change(c, log, r.source, key, F_CATCHUP, json(&before)?, json(&to)?)?;
                    changed = true;
                }
            }
            if let Some(kind) = ch.dismiss {
                let before: i64 = override_col(c, r, "guide_ok")?.unwrap_or(0);
                let after = before | kind.bit();
                if before != after {
                    set_override_col(c, r, "guide_ok", Value::Integer(after))?;
                    log_change(c, log, r.source, key, F_DISMISS, Some(before.to_string()), Some(after.to_string()))?;
                    changed = true;
                }
            }
            let shift1 = effective_shift(c, r)?;
            if shift1 != shift0 {
                report.reminders_moved += crate::store::retime::channel_shift(c, r.source, key, i64::from(shift1 - shift0) * 60)?.reminders;
            }
            report.applied += usize::from(changed);
        }
        let mut rules_changed = false;
        for (source, rules) in &p.rules {
            let before = guide_rules(c)?.get(source).copied();
            let Some(before) = before else {
                report.skipped.push((String::new(), "playlist removed".into()));
                continue;
            };
            if before != *rules {
                crate::store::sources::set_guide_rules(c, *source, rules)?;
                log_change(c, log, *source, "", F_RULE, json(&Some(before))?, json(&Some(*rules))?)?;
                rules_changed = true;
                report.applied += 1;
            }
        }
        if rules_changed {
            report.reminders_moved += derive_rule_rows(c, opts)?.reminders_moved;
            report.mappings_changed = true;
        }
        if report.applied == 0 {
            c.execute("DELETE FROM guide_fix_log WHERE id = ?1", [log])?;
            return Ok(report);
        }
        c.execute("UPDATE guide_fix_log SET n = ?2 WHERE id = ?1", params![log, report.applied as i64])?;
        report.log = Some(log);
        tracing::info!(target: "tuner::guide", "guide fix: {} changes ({}) #{}", report.applied, enum_str(&actor), log.0);
        Ok(report)
    })
}

/// Revert a logged batch: each field goes back to its `before` only while it still holds what
/// the batch wrote (anything changed since is skipped and named, as are channels gone from
/// their playlist). Shifts move reminders back; rule changes re-derive.
pub fn undo(conn: &Connection, log: GuideFixLogId, opts: &GuideOpts, now: Timestamp) -> Result<UndoReport> {
    atomic(conn, |c| {
        let undone: Option<Option<Timestamp>> = c.query_row("SELECT undone_at FROM guide_fix_log WHERE id = ?1", [log], |r| r.get(0)).optional()?;
        match undone {
            None => return Err(sql::not_found("guide fix", log)),
            Some(Some(_)) => return Err(Error::Other("that guide fix was already undone".into())),
            Some(None) => {}
        }
        let actor: FixActor =
            enum_parse(&c.query_row("SELECT actor FROM guide_fix_log WHERE id = ?1", [log], |r| r.get::<_, String>(0))?).unwrap_or_default();
        let changes = changes_of(c, log)?;
        let mut report = UndoReport::default();
        let mut by_channel: Vec<(SourceId, String, Vec<&LoggedChange>)> = Vec::new();
        let mut rules_changed = false;
        for ch in &changes {
            if ch.field == F_RULE {
                let now_rules = guide_rules(c)?.get(&ch.source).copied();
                let after: Option<GuideRules> = unjson(&ch.after);
                match (now_rules, unjson::<GuideRules>(&ch.before)) {
                    (Some(cur), Some(before)) if Some(cur) == after => {
                        crate::store::sources::set_guide_rules(c, ch.source, &before)?;
                        rules_changed = true;
                        report.reverted += 1;
                    }
                    (None, _) => report.skipped.push((String::new(), "playlist removed".into())),
                    _ => report.skipped.push((String::new(), "rules changed since".into())),
                }
                continue;
            }
            match by_channel.iter_mut().find(|(s, k, _)| *s == ch.source && *k == ch.channel_key) {
                Some((_, _, v)) => v.push(ch),
                None => by_channel.push((ch.source, ch.channel_key.clone(), vec![ch])),
            }
        }
        for (source, key, fields) in by_channel {
            let r = ItemRef { source, kind: ItemKind::Channel, key: ItemKey(key.clone()) };
            if !channel_exists(c, &r)? {
                report.skipped.push((key, "no longer in the playlist".into()));
                continue;
            }
            let shift0 = effective_shift(c, &r)?;
            let mut reverted = false;
            for f in fields {
                let ok = match f.field.as_str() {
                    F_MAP => {
                        let cur = manual_target(c, &r)?;
                        let (before, after): (Option<GuideTarget>, Option<GuideTarget>) = (unjson(&f.before), unjson(&f.after));
                        same_target(&cur, &after) && {
                            set_map(c, &r, before.as_ref(), actor)?;
                            true
                        }
                    }
                    F_SHIFT => {
                        let cur: Option<i32> = override_col(c, &r, "shift_min")?;
                        cur == unjson(&f.after) && {
                            let before: Option<i32> = unjson(&f.before);
                            set_override_col(c, &r, "shift_min", before.map_or(Value::Null, |v| Value::Integer(v.into())))?;
                            true
                        }
                    }
                    F_CATCHUP => {
                        let cur: Option<i64> = override_col(c, &r, "catchup_offset_s")?;
                        cur == unjson(&f.after) && {
                            let before: Option<i64> = unjson(&f.before);
                            set_override_col(c, &r, "catchup_offset_s", before.map_or(Value::Null, Value::Integer))?;
                            true
                        }
                    }
                    F_DISMISS => {
                        let cur: i64 = override_col(c, &r, "guide_ok")?.unwrap_or(0);
                        Some(cur) == unjson(&f.after) && {
                            set_override_col(c, &r, "guide_ok", Value::Integer(unjson(&f.before).unwrap_or(0)))?;
                            true
                        }
                    }
                    _ => false,
                };
                if ok {
                    reverted = true;
                } else {
                    report.skipped.push((key.clone(), "changed since".into()));
                }
            }
            let shift1 = effective_shift(c, &r)?;
            if shift1 != shift0 {
                report.reminders_moved += crate::store::retime::channel_shift(c, source, &key, i64::from(shift1 - shift0) * 60)?.reminders;
            }
            report.reverted += usize::from(reverted);
        }
        if rules_changed {
            report.reminders_moved += derive_rule_rows(c, opts)?.reminders_moved;
        }
        c.execute("UPDATE guide_fix_log SET undone_at = ?2 WHERE id = ?1", params![log, now])?;
        tracing::info!(target: "tuner::guide", "guide fix #{} undone: {} reverted, {} skipped", log.0, report.reverted, report.skipped.len());
        Ok(report)
    })
}

/// Prune logs older than [`LOG_KEEP_S`] or beyond the newest [`LOG_KEEP_N`], and suggestions
/// of channels that are gone (`jobs::maintenance::prune_features`).
pub fn prune(conn: &Connection, now: Timestamp) -> Result<usize> {
    let logs = conn.execute(
        "DELETE FROM guide_fix_log WHERE at < ?1 OR id NOT IN (SELECT id FROM guide_fix_log ORDER BY id DESC LIMIT ?2)",
        params![now - LOG_KEEP_S, LOG_KEEP_N as i64],
    )?;
    conn.execute("DELETE FROM guide_fix_change WHERE log_id NOT IN (SELECT id FROM guide_fix_log)", [])?;
    let sugg = conn.execute(
        "DELETE FROM epg_suggestion WHERE NOT EXISTS (SELECT 1 FROM channel c WHERE c.source_id = epg_suggestion.source_id AND c.key = epg_suggestion.channel_key)",
        [],
    )?;
    Ok(logs + sugg)
}
