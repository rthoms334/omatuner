//! Spoiler-guard storage: `spoiler_rule` + `spoiler_seen` (migration 0012, DESIGN §34). OWNER:
//! S1.
//!
//! `spoiler_seen` is keyed by `(textmatch::event_key, first_start)` (B3), the same key as
//! Following's ledger, so "Mark seen" on a followed match lifts the guard and Following's
//! Upcoming shows "seen". Only guarded subjects are ever recorded (no general watch log).
//!
//! Rules hold no `source_id`: they survive refreshes, re-keys and the move between playlists
//! with no reattach work. Topic terms are stored folded (`textmatch::fold`). Following rules
//! with `spoiler_safe = 1` are guard topics by data (B4): [`follow_topics`] reads them, nothing
//! is copied.
//!
//! The snapshot builder's reads ([`event_candidates`], [`episode_progress`], [`series_rows`],
//! [`next_episode_after`]) live here too, so `spoiler::snapshot` holds no SQL.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::sql::{Json, atomic, enum_parse, enum_str, or_not_found, to_json};
use crate::error::{Error, Result};
use crate::model::{
    EpgChannelId, ItemRef, ProgrammeId, SEEN_SPAN_S, SeenHow, SeenRow, SpoilerMode, SpoilerRule, SpoilerRuleId, SpoilerRuleKind, Timestamp,
};

/// Seen rows are pruned this long after their window closes.
pub const SEEN_KEEP_S: i64 = 30 * 86_400;
/// At most this many rules (Settings and the MCP tool refuse more).
pub const MAX_RULES: usize = 200;
/// Per rule: at most this many terms (aliases included), each at most [`MAX_TERM_CHARS`].
pub const MAX_TERMS: usize = 20;
pub const MAX_TERM_CHARS: usize = 60;
pub const MAX_LABEL_CHARS: usize = 80;

const RULE_COLS: &str = "id, kind, label, terms, series_norm, categories, mode, window_h, until_at, sport_descr, shield, enabled, origin, created_at";

fn rule_from_row(r: &Row<'_>) -> rusqlite::Result<SpoilerRule> {
    let bad = |what: &str, v: String| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, format!("bad {what} {v:?}").into());
    let kind: String = r.get(1)?;
    let mode: String = r.get(6)?;
    let origin: String = r.get(12)?;
    Ok(SpoilerRule {
        id: r.get(0)?,
        kind: enum_parse(&kind).ok_or_else(|| bad("spoiler rule kind", kind))?,
        label: r.get(2)?,
        // a broken JSON column is a skipped rule term list, never a failed read (DESIGN §34 §5)
        terms: r.get::<_, Json<Vec<String>>>(3).map(|j| j.0).unwrap_or_default(),
        series_norm: r.get(4)?,
        categories: r.get::<_, Json<Vec<String>>>(5).map(|j| j.0).unwrap_or_default(),
        mode: enum_parse(&mode).ok_or_else(|| bad("spoiler mode", mode))?,
        window_h: r.get(7)?,
        until_at: r.get(8)?,
        sport_descr: r.get(9)?,
        shield: r.get(10)?,
        enabled: r.get(11)?,
        by_agent: origin == "agent",
        created_at: r.get(13)?,
    })
}

/// Every rule, oldest first.
pub fn list_rules(conn: &Connection) -> Result<Vec<SpoilerRule>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {RULE_COLS} FROM spoiler_rule ORDER BY id"))?;
    let rows = stmt.query_map([], rule_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn rule(conn: &Connection, id: SpoilerRuleId) -> Result<SpoilerRule> {
    or_not_found(conn.query_row(&format!("SELECT {RULE_COLS} FROM spoiler_rule WHERE id = ?1"), [id], rule_from_row), "spoiler rule", id)
}

/// Fold and tidy terms: folded (`textmatch::fold`), trimmed, empty and duplicate ones dropped.
pub fn fold_terms(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(terms.len());
    let mut buf = String::new();
    for t in terms {
        crate::textmatch::fold::fold(t, &mut buf);
        let t = buf.split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// A rule's fields are usable (labels, term counts and lengths, a series key, a blackout end).
fn validate(r: &SpoilerRule) -> Result<()> {
    let bad = |m: &str| Err(Error::Other(m.into()));
    if r.label.trim().is_empty() || r.label.chars().count() > MAX_LABEL_CHARS {
        return bad("a spoiler rule needs a name of at most 80 characters");
    }
    if r.terms.len() > MAX_TERMS || r.terms.iter().any(|t| t.chars().count() > MAX_TERM_CHARS) {
        return bad("too many or too long words for one spoiler rule");
    }
    match r.kind {
        SpoilerRuleKind::Topic | SpoilerRuleKind::Blackout if r.terms.is_empty() => bad("a spoiler topic needs at least one word"),
        SpoilerRuleKind::Series if r.series_norm.as_deref().is_none_or(str::is_empty) => bad("a series rule needs a series"),
        SpoilerRuleKind::Blackout if r.until_at.is_none() => bad("“I'll watch it later” needs an end time"),
        _ => Ok(()),
    }
}

/// Add a rule (`id` and `created_at` of `r` are ignored; terms are folded here). A series rule
/// replaces the series' existing row (one per series: an exception or an addition).
pub fn add_rule(conn: &Connection, r: &SpoilerRule, now: Timestamp) -> Result<SpoilerRuleId> {
    let r = SpoilerRule { terms: fold_terms(&r.terms), ..r.clone() };
    validate(&r)?;
    atomic(conn, |c| {
        let n: i64 = c.query_row("SELECT count(*) FROM spoiler_rule", [], |x| x.get(0))?;
        let replaces = r.kind == SpoilerRuleKind::Series
            && c.query_row("SELECT 1 FROM spoiler_rule WHERE kind = 'series' AND series_norm = ?1", [&r.series_norm], |_| Ok(()))
                .optional()?
                .is_some();
        if n as usize >= MAX_RULES && !replaces {
            return Err(Error::Other(format!("at most {MAX_RULES} spoiler rules")));
        }
        if replaces {
            c.execute("DELETE FROM spoiler_rule WHERE kind = 'series' AND series_norm = ?1", [&r.series_norm])?;
        }
        c.prepare_cached(
            "INSERT INTO spoiler_rule(kind, label, terms, series_norm, categories, mode, window_h, until_at, sport_descr, shield,
                                      enabled, origin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?
        .execute(params![
            enum_str(&r.kind),
            r.label.trim(),
            to_json(&r.terms)?,
            r.series_norm,
            to_json(&r.categories)?,
            enum_str(&r.mode),
            r.window_h,
            r.until_at,
            r.sport_descr,
            r.shield,
            r.enabled,
            if r.by_agent { "agent" } else { "user" },
            now
        ])?;
        Ok(SpoilerRuleId(c.last_insert_rowid()))
    })
}

/// Save an edited rule (everything but `kind`, `origin` and `created_at`).
pub fn update_rule(conn: &Connection, r: &SpoilerRule) -> Result<()> {
    let r = SpoilerRule { terms: fold_terms(&r.terms), ..r.clone() };
    validate(&r)?;
    let n = conn
        .prepare_cached(
            "UPDATE spoiler_rule SET label = ?2, terms = ?3, series_norm = ?4, categories = ?5, mode = ?6, window_h = ?7,
                    until_at = ?8, sport_descr = ?9, shield = ?10, enabled = ?11
             WHERE id = ?1",
        )?
        .execute(params![
            r.id,
            r.label.trim(),
            to_json(&r.terms)?,
            r.series_norm,
            to_json(&r.categories)?,
            enum_str(&r.mode),
            r.window_h,
            r.until_at,
            r.sport_descr,
            r.shield,
            r.enabled
        ])?;
    if n == 0 {
        return Err(super::sql::not_found("spoiler rule", r.id));
    }
    Ok(())
}

/// Turn a rule on or off (a series row off = "don't guard this series").
pub fn set_enabled(conn: &Connection, id: SpoilerRuleId, enabled: bool) -> Result<()> {
    if conn.execute("UPDATE spoiler_rule SET enabled = ?2 WHERE id = ?1", params![id, enabled])? == 0 {
        return Err(super::sql::not_found("spoiler rule", id));
    }
    Ok(())
}

/// Delete a rule; `false` when it didn't exist.
pub fn remove_rule(conn: &Connection, id: SpoilerRuleId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM spoiler_rule WHERE id = ?1", [id])? > 0)
}

/// The series' own row: `guarded = false` is the user's "don't guard" exception (it wins over
/// the automatic part-way rule), `true` an explicit addition.
pub fn set_series_guard(conn: &Connection, series_norm: &str, label: &str, guarded: bool, by_agent: bool, now: Timestamp) -> Result<SpoilerRuleId> {
    add_rule(
        conn,
        &SpoilerRule {
            id: SpoilerRuleId(0),
            kind: SpoilerRuleKind::Series,
            label: label.into(),
            terms: Vec::new(),
            series_norm: Some(series_norm.into()),
            categories: Vec::new(),
            mode: SpoilerMode::UntilSeen,
            window_h: 0,
            until_at: None,
            sport_descr: false,
            shield: false,
            enabled: guarded,
            by_agent,
            created_at: now,
        },
        now,
    )
}

// ------------------------------------------------------------------ seen events

const SEEN_COLS: &str = "event_key, first_start, title, series_norm, season, episode, seen_at, how";

fn seen_from_row(r: &Row<'_>) -> rusqlite::Result<SeenRow> {
    let how: String = r.get(7)?;
    Ok(SeenRow {
        event_key: r.get(0)?,
        first_start: r.get(1)?,
        title: r.get(2)?,
        series_norm: r.get(3)?,
        season: r.get(4)?,
        episode: r.get(5)?,
        seen_at: r.get(6)?,
        how: enum_parse(&how).unwrap_or(SeenHow::Marked),
    })
}

/// The seen row covering an airing of `event_key` starting at `start` (same key, start within
/// [`SEEN_SPAN_S`] of the row's first airing).
pub fn seen_covering(conn: &Connection, event_key: &str, start: Timestamp) -> Result<Option<SeenRow>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {SEEN_COLS} FROM spoiler_seen WHERE event_key = ?1 AND first_start BETWEEN ?2 - ?3 AND ?2 + ?3
             ORDER BY first_start LIMIT 1"
        ))?
        .query_row(params![event_key, start, SEEN_SPAN_S], seen_from_row)
        .optional()?)
}

/// Record a seen event; `false` (nothing written) when a row already covers it.
pub fn mark_seen(conn: &Connection, row: &SeenRow) -> Result<bool> {
    atomic(conn, |c| {
        if seen_covering(c, &row.event_key, row.first_start)?.is_some() {
            return Ok(false);
        }
        c.prepare_cached(&format!("INSERT INTO spoiler_seen({SEEN_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"))?.execute(params![
            row.event_key,
            row.first_start,
            row.title,
            row.series_norm,
            row.season,
            row.episode,
            row.seen_at,
            enum_str(&row.how)
        ])?;
        Ok(true)
    })
}

/// Undo a "seen" (the toast's Undo, Settings ▸ Seen recently): the row covering that airing
/// goes; `false` when there was none.
pub fn unmark_seen(conn: &Connection, event_key: &str, start: Timestamp) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM spoiler_seen WHERE event_key = ?1 AND first_start BETWEEN ?2 - ?3 AND ?2 + ?3",
        params![event_key, start, SEEN_SPAN_S],
    )? > 0)
}

/// Seen rows, newest first (`limit` rows; Settings ▸ "Seen recently").
pub fn seen_recent(conn: &Connection, limit: usize) -> Result<Vec<SeenRow>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {SEEN_COLS} FROM spoiler_seen ORDER BY seen_at DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], seen_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Seen rows whose first airing started at or after `since` (the snapshot builder).
pub fn seen_since(conn: &Connection, since: Timestamp) -> Result<Vec<SeenRow>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {SEEN_COLS} FROM spoiler_seen WHERE first_start >= ?1"))?;
    let rows = stmt.query_map([since], seen_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Seen series airings (rows with S/E), any age: they lift series boundaries like progress.
pub fn seen_episodes(conn: &Connection) -> Result<Vec<SeenRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SEEN_COLS} FROM spoiler_seen WHERE series_norm IS NOT NULL AND season IS NOT NULL AND episode IS NOT NULL"
    ))?;
    let rows = stmt.query_map([], seen_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Prune seen rows (`jobs::maintenance::prune_features`): a row goes [`SEEN_KEEP_S`] after the
/// longest window any rule could still give its event (rules' `window_h`, at least 48 h, plus
/// the repeat span). Series rows (S/E) stay: they are the series' seen state.
pub fn prune(conn: &Connection, now: Timestamp) -> Result<usize> {
    let max_h: i64 = conn.query_row("SELECT max(48, COALESCE(max(window_h), 0)) FROM spoiler_rule WHERE kind != 'series'", [], |r| r.get(0))?;
    let before = now - SEEN_KEEP_S - SEEN_SPAN_S - max_h * 3600;
    Ok(conn.execute("DELETE FROM spoiler_seen WHERE first_start < ?1 AND series_norm IS NULL", [before])?)
}

// ------------------------------------------------------------------ backup

#[derive(Debug, Serialize, Deserialize)]
struct Backup {
    rules: Vec<SpoilerRule>,
    seen: Vec<SeenRow>,
}

/// Rules and seen rows for a backup (`None` when there are none).
pub fn export(conn: &Connection) -> Result<Option<serde_json::Value>> {
    let rules = list_rules(conn)?;
    let seen = seen_recent(conn, usize::MAX >> 1)?;
    if rules.is_empty() && seen.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_value(Backup { rules, seen }).map_err(Error::other)?))
}

/// Restore rules and seen rows (rules hold no item refs, so `_remap` is unused). Rules already
/// present (same kind, label and terms; a series by its key) and covered seen rows are skipped.
/// Returns the rows added.
pub fn import(conn: &Connection, v: &serde_json::Value, _remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    let b: Backup = serde_json::from_value(v.clone()).map_err(|e| Error::BadPayload(format!("spoiler guard backup: {e}")))?;
    let have = list_rules(conn)?;
    let mut added = 0;
    for r in &b.rules {
        let dup = have.iter().any(|h| {
            h.kind == r.kind
                && match r.kind {
                    SpoilerRuleKind::Series => h.series_norm == r.series_norm,
                    _ => h.label == r.label && h.terms == fold_terms(&r.terms),
                }
        });
        if !dup {
            match add_rule(conn, r, r.created_at) {
                Ok(_) => added += 1,
                Err(e) => tracing::warn!("spoiler guard backup: rule skipped: {e}"),
            }
        }
    }
    for s in &b.seen {
        added += usize::from(mark_seen(conn, s)?);
    }
    Ok(added)
}

// ------------------------------------------------------------------ Following's topics (B4)

/// A spoiler-safe Following rule as a guard topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowTopic {
    pub term: crate::guard::GuardTerm,
    /// The spec's `without` words, folded ("women", "fan tv", "highlights"): an airing that
    /// carries one isn't the followed event.
    pub without: Vec<String>,
    /// A Team rule: only fixtures ("A v B") are its events.
    pub fixtures_only: bool,
}

/// Following rules with `spoiler_safe = 1` as guard topics ("until seen"; Sport scope for Team
/// and Event rules): their name and the spec's `terms`, folded, with what keeps them narrow
/// (`without`, fixtures only for Team rules). A broken spec is skipped with a WARN (by id,
/// never the text).
pub fn follow_topics(conn: &Connection) -> Result<Vec<FollowTopic>> {
    #[derive(Deserialize)]
    struct Terms {
        #[serde(default)]
        terms: Vec<String>,
        #[serde(default)]
        without: Vec<String>,
    }
    let mut stmt =
        conn.prepare_cached("SELECT id, name, kind, spec FROM follow_rule WHERE spoiler_safe = 1 AND enabled = 1 ORDER BY priority, id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, kind, spec) = row?;
        match serde_json::from_str::<Terms>(&spec) {
            Ok(t) => {
                let terms = fold_terms(&t.terms);
                if !terms.is_empty() {
                    out.push(FollowTopic {
                        term: crate::guard::GuardTerm { label: name, terms, sport_only: matches!(kind.as_str(), "team" | "event") },
                        without: fold_terms(&t.without),
                        fixtures_only: kind == "team",
                    });
                }
            }
            Err(e) => tracing::warn!("follow rule {id}: unreadable spec, not a spoiler topic: {e}"),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ snapshot builder reads

/// A guide programme that may belong to a guarded topic.
#[derive(Debug, Clone, PartialEq)]
pub struct EventCandidate {
    pub id: ProgrammeId,
    pub epg_channel_id: EpgChannelId,
    /// EPG time (no channel shift).
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

/// Rows a single candidate query returns at most (a topic as broad as "news" can't flood the
/// builder).
pub const MAX_CANDIDATES: usize = 4000;

/// FTS5 expression for folded terms: each term one phrase, OR-ed; `None` without a token.
pub fn fts_terms(terms: &[String]) -> Option<String> {
    let phrases: Vec<String> = terms
        .iter()
        .map(|t| t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .map(|p| format!("\"{p}\""))
        .collect();
    (!phrases.is_empty()).then(|| phrases.join(" OR "))
}

/// Programmes of enabled guides (active generation) mapped to a playlist channel, overlapping
/// `[from, to)` (EPG time), whose title (`title_only`) or title / sub-title mention one of
/// `terms` (an FTS pre-filter; callers re-check whole tokens). Newest first: at the
/// [`MAX_CANDIDATES`] cap the oldest airings go, never the match on now.
pub fn event_candidates(conn: &Connection, terms: &[String], title_only: bool, from: Timestamp, to: Timestamp) -> Result<Vec<EventCandidate>> {
    let Some(expr) = fts_terms(terms) else { return Ok(Vec::new()) };
    let expr = if title_only { format!("{{title}} : ({expr})") } else { expr };
    let mut stmt = conn.prepare_cached(
        "SELECT p.id, p.epg_channel_id, p.start, p.stop, p.title, p.sub_title, p.descr, p.categories, p.season, p.episode
         FROM programme_fts f
         JOIN programme p ON p.id = f.rowid
         JOIN epg_channel ec ON ec.id = p.epg_channel_id
         JOIN epg_source es ON es.id = ec.epg_source_id
         WHERE programme_fts MATCH ?1 AND p.gen = es.active_gen AND es.enabled = 1
           AND p.start < ?3 AND p.stop > ?2
           AND EXISTS (SELECT 1 FROM channel_epg ce WHERE ce.epg_channel_id = p.epg_channel_id)
         ORDER BY p.start DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![expr, from, to, MAX_CANDIDATES as i64], |r| {
        Ok(EventCandidate {
            id: r.get(0)?,
            epg_channel_id: r.get(1)?,
            start: r.get(2)?,
            stop: r.get(3)?,
            title: r.get(4)?,
            sub_title: r.get(5)?,
            description: r.get(6)?,
            categories: r
                .get::<_, Option<String>>(7)?
                .map(|c| c.split('\x1f').filter(|s| !s.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default(),
            season: r.get(8)?,
            episode: r.get(9)?,
        })
    })?;
    let out: Vec<EventCandidate> = rows.collect::<rusqlite::Result<_>>()?;
    if out.len() == MAX_CANDIDATES {
        tracing::debug!("spoiler guard: a topic hit the {MAX_CANDIDATES}-programme cap; its oldest airings were left out");
    }
    Ok(out)
}

/// One episode's progress, merged across playlists by the series key in its fingerprint
/// (`<series norm>:s<S>e<E>`, which survives orphaning and copies).
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeProgress {
    pub series_norm: String,
    pub season: i32,
    pub episode: i32,
    /// Finished, or watched to 90 %.
    pub done: bool,
    /// Started (position > 0).
    pub started: bool,
    pub updated_at: Timestamp,
}

/// Every episode progress row, parsed (rows without a series key are skipped).
pub fn episode_progress(conn: &Connection) -> Result<Vec<EpisodeProgress>> {
    let mut stmt = conn.prepare_cached(
        "SELECT json_extract(fp, '$.norm_name'), finished, position_s, duration_s, updated_at FROM watch_progress WHERE kind = 'episode'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, bool>(1)?, r.get::<_, f64>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Timestamp>(4)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (norm, finished, pos, dur, at) = row?;
        let Some((series, s, e)) = norm.as_deref().and_then(parse_episode_norm) else { continue };
        let near_end = dur.is_some_and(|d| d > 0.0 && pos / d >= 0.9);
        out.push(EpisodeProgress {
            series_norm: series.into(),
            season: s,
            episode: e,
            done: finished || near_end,
            started: pos > 0.0,
            updated_at: at,
        });
    }
    Ok(out)
}

/// `"severance:s2e4"` → `("severance", 2, 4)`.
pub fn parse_episode_norm(s: &str) -> Option<(&str, i32, i32)> {
    let (series, se) = s.rsplit_once(":s")?;
    let (season, episode) = se.split_once('e')?;
    Some((series, season.parse().ok()?, episode.parse().ok()?))
}

/// A catalogue series row (for labels, aliases and the explicit check).
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesRow {
    pub item: ItemRef,
    pub name: String,
    pub norm_name: String,
}

/// The catalogue's series rows with this key, in every playlist.
pub fn series_rows(conn: &Connection, series_norm: &str) -> Result<Vec<SeriesRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT source_id, key, name, norm_name FROM series WHERE source_id IN (SELECT id FROM source) AND norm_name = ?1 ORDER BY source_id, id",
    )?;
    let rows = stmt.query_map([series_norm], |r| {
        Ok(SeriesRow {
            item: ItemRef { source: r.get(0)?, kind: crate::model::ItemKind::Series, key: crate::model::ItemKey(r.get(1)?) },
            name: r.get(2)?,
            norm_name: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The catalogue's first episode of the series after `(season, episode)`, any playlist.
pub fn next_episode_after(conn: &Connection, series_norm: &str, after: (i32, i32)) -> Result<Option<(i32, i32)>> {
    Ok(conn
        .prepare_cached(
            "SELECT e.season, e.episode FROM episode e JOIN series s ON s.id = e.series_id
             WHERE s.source_id IN (SELECT id FROM source) AND s.norm_name = ?1
               AND (e.season > ?2 OR (e.season = ?2 AND e.episode > ?3))
             ORDER BY e.season, e.episode LIMIT 1",
        )?
        .query_row(params![series_norm, after.0, after.1], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_keys_parse() {
        assert_eq!(parse_episode_norm("severance:s2e4"), Some(("severance", 2, 4)));
        assert_eq!(parse_episode_norm("9-1-1:s10e12"), Some(("9-1-1", 10, 12)));
        assert_eq!(parse_episode_norm("severance"), None);
    }

    #[test]
    fn a_term_without_a_token_makes_no_query() {
        assert_eq!(fts_terms(&["\"".into()]), None);
    }
}
