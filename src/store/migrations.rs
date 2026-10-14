//! Schema migrations keyed by `PRAGMA user_version`. Implemented in the skeleton.
//!
//! FROZEN (skeleton). Contract: migrations are append-only (WP-02 appends entries after the
//! `skeleton-v1` tag, with architect sign-off). Each entry runs inside one transaction and bumps
//! `user_version` to its index + 1. Never edit a shipped migration; add `000N_*.sql`. A
//! migration whose new columns need data SQL can't compute (it needs Rust parsing) has a
//! [`backfill`] step, run in the same transaction right after its script.

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::model::Quality;
use crate::sources::smart_names::{clean_channel_name, clean_vod_name, group_display};

/// Ordered migration scripts.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    ("0002_category_filter", include_str!("migrations/0002_category_filter.sql")),
    ("0003_quality", include_str!("migrations/0003_quality.sql")),
    ("0004_name_rules", include_str!("migrations/0004_name_rules.sql")),
    ("0005_explicit", include_str!("migrations/0005_explicit.sql")),
    ("0006_title_stacks", include_str!("migrations/0006_title_stacks.sql")),
    ("0007_title_join", include_str!("migrations/0007_title_join.sql")),
    // the big-features batch (numbers fixed by merge order, each idempotent; owners in each
    // file's header and in the step functions below)
    ("0008_reminder_modes", include_str!("migrations/0008_reminder_modes.sql")),
    ("0009_guide_fixes", include_str!("migrations/0009_guide_fixes.sql")),
    ("0010_play_queue", include_str!("migrations/0010_play_queue.sql")),
    ("0011_follow", include_str!("migrations/0011_follow.sql")),
    ("0012_spoiler_guard", include_str!("migrations/0012_spoiler_guard.sql")),
    ("0013_agent_names", include_str!("migrations/0013_agent_names.sql")),
    // casting (DESIGN §40; skeleton CAST-0, architect sign-off)
    ("0014_cast", include_str!("migrations/0014_cast.sql")),
    ("0015_watchlists", include_str!("migrations/0015_watchlists.sql")),
];

/// Current schema version after all migrations.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// `meta` key holding the Tuner version that last opened the database for writing
/// ([`record_app_version`]). Not a schema change: `meta` exists since v1, so every build from
/// this one on writes it and every build reads it when it refuses a newer schema (DESIGN
/// §21.7). Keep writing it in future builds — it is what an older build quotes to the user.
pub const APP_VERSION_KEY: &str = "app_version";

/// Apply pending migrations. Refuses to open a DB newer than this binary
/// ([`Error::NewerSchema`](crate::error::Error::NewerSchema), naming the version that last
/// opened it when it recorded one).
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > latest_version() {
        return Err(crate::error::Error::NewerSchema { found: current, supported: latest_version(), opened_by: recorded_app_version(conn) });
    }
    reconcile_renumbered(conn, current)?;
    cast_hint_failures(conn, current)?;
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        tracing::info!("applying migration {name}");
        if *name == "0015_watchlists" {
            crate::store::backup::before_watchlists(conn)?;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        backfill(&tx, name)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// Note this build's version as the one that last opened the database
/// ([`APP_VERSION_KEY`]); written only when it changed, so a normal start costs one read.
pub fn record_app_version(conn: &Connection) -> Result<()> {
    let me = env!("CARGO_PKG_VERSION");
    if recorded_app_version(conn).as_deref() != Some(me) {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![APP_VERSION_KEY, me],
        )?;
    }
    Ok(())
}

/// The recorded [`APP_VERSION_KEY`], if any and if it looks like a version (it ends up in a
/// desktop notification: a damaged or odd value is dropped, never quoted). Best effort: a
/// database too new or too damaged to read it gives `None`.
fn recorded_app_version(conn: &Connection) -> Option<String> {
    use rusqlite::OptionalExtension;
    let v: String = conn.query_row("SELECT value FROM meta WHERE key = ?1", [APP_VERSION_KEY], |r| r.get(0)).optional().ok()??;
    crate::platform::startup::version_like(&v).then_some(v)
}

/// `user_version` is positional, so a database from a pre-merge build of the quality-badges
/// branch is at v2 with *its* `0002_quality` applied, not `0002_category_filter`. Such a
/// database (v2+ without `grp.raw_name`) gets the category-filter columns here; the
/// renumbered `0003_quality` then re-runs harmlessly (it is idempotent).
///
/// Likewise a development database of the stacks branch is at v5 with its `0005_title_stacks`
/// (now 0006, re-run harmlessly) instead of `0005_explicit`: the explicit columns are added and
/// backfilled here.
fn reconcile_renumbered(conn: &mut Connection, current: i64) -> Result<()> {
    if current >= 5 && !has_column(conn, "grp", "explicit")? {
        tracing::info!("applying migration 0005_explicit (database had another branch's 0005)");
        let tx = conn.transaction()?;
        backfill_explicit(&tx)?;
        tx.commit()?;
    }
    if current < 2 || has_column(conn, "grp", "raw_name")? {
        return Ok(());
    }
    tracing::info!("applying migration 0002_category_filter (database had another branch's 0002)");
    let tx = conn.transaction()?;
    add_column_if_missing(&tx, "grp", "raw_name", "TEXT")?;
    add_column_if_missing(&tx, "group_override", "shown", "INTEGER NOT NULL DEFAULT 0")?;
    tx.commit()?;
    Ok(())
}

/// 0014 gained `cast_hint.l2_failed_at` / `l2_failed_logic` before its release (DESIGN §40
/// §8.3, `store::cast` module docs): a test build's database already at v14 (or a later cast
/// branch's) has the table without them and gets them here, NULL. Its rows' L2 failures were
/// judged by the guard that called long-GOP films irregular, and a NULL failure never forces
/// L3, so that verdict is dropped without touching the rows (their codecs stay useful).
fn cast_hint_failures(conn: &mut Connection, current: i64) -> Result<()> {
    if current < 14 || has_column(conn, "cast_hint", "l2_failed_at")? {
        return Ok(());
    }
    tracing::info!("applying 0014_cast's L2 failure columns (database from a test build)");
    let tx = conn.transaction()?;
    add_column_if_missing(&tx, "cast_hint", "l2_failed_at", "INTEGER")?;
    add_column_if_missing(&tx, "cast_hint", "l2_failed_logic", "INTEGER")?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT count(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2", [table, col], |r| r.get(0))?)
}

pub(crate) fn add_column_if_missing(conn: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    if !has_column(conn, table, col)? {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
    }
    Ok(())
}

/// The Rust part of migration `name`, if it has one.
fn backfill(conn: &Connection, name: &str) -> Result<()> {
    match name {
        "0003_quality" => backfill_channel_quality(conn),
        "0004_name_rules" => {
            seed_quality_checks(conn)?;
            reclean_names(conn)
        }
        "0005_explicit" => backfill_explicit(conn),
        "0006_title_stacks" => {
            add_title_columns(conn)?;
            crate::store::titles::refresh_all(conn).map(|_| ())
        }
        "0007_title_join" => rebuild_title_link(conn),
        "0008_reminder_modes" => reminder_modes(conn),
        "0009_guide_fixes" => guide_fix_columns(conn),
        "0011_follow" => follow_columns(conn),
        "0013_agent_names" => agent_names(conn),
        "0015_watchlists" => {
            add_column_if_missing(conn, "queue_item", "watchlist_item_id", "INTEGER")?;
            crate::store::watchlist::refresh_episode_matches(conn, None)?;
            crate::store::watchlist::move_favourites(conn).map(|_| ())
        }
        _ => Ok(()),
    }
}

/// 0008 (owner: remind track): the reminder's mode, start offset and where it opens; rows made
/// with the old switch marker (`lead_s = Reminder::SWITCH_LEAD`) become `mode = 'switch'`. From
/// here on both say the same (`store::user` keeps `lead_s = SWITCH_LEAD` ⟺ `mode = 'switch'`),
/// so readers of either agree.
fn reminder_modes(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "reminder", "mode", "TEXT NOT NULL DEFAULT 'notify'")?;
    add_column_if_missing(conn, "reminder", "offset_s", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "reminder", "open_in", "TEXT")?;
    conn.execute("UPDATE reminder SET mode = 'switch' WHERE lead_s = ?1 AND mode <> 'switch'", [crate::model::Reminder::SWITCH_LEAD])?;
    Ok(())
}

/// 0009 (owner: G-core): the Guide fixes columns (see the script's header).
fn guide_fix_columns(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "item_override", "shift_min", "INTEGER")?;
    add_column_if_missing(conn, "item_override", "guide_ok", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "channel_epg_map", "shift_min", "INTEGER")?;
    add_column_if_missing(conn, "channel_epg_map", "epg_name", "TEXT")?;
    add_column_if_missing(conn, "channel_epg_map", "actor", "TEXT")?;
    add_column_if_missing(conn, "channel_epg", "shift_min", "INTEGER")
}

/// 0011 (owner: F-AB): the Following rule that made a reminder.
fn follow_columns(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "reminder", "rule_id", "INTEGER REFERENCES follow_rule(id) ON DELETE SET NULL")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS reminder_rule ON reminder(rule_id) WHERE rule_id IS NOT NULL")?;
    Ok(())
}

/// 0013: the name of the agent that made a rule, a seen mark or a guide-fix batch (DESIGN
/// §30.12); NULL = unknown ("your agent"). Agent rows from before it were Claude's: only the
/// Claude Code plugin could make them. Run once (`user_version`); a renumbered re-run would only
/// name unknown agents' rows Claude, which is harmless.
fn agent_names(conn: &Connection) -> Result<()> {
    for t in ["follow_rule", "spoiler_rule", "spoiler_seen", "guide_fix_log"] {
        add_column_if_missing(conn, t, "agent", "TEXT")?;
    }
    conn.execute_batch(
        "UPDATE follow_rule   SET agent = 'Claude' WHERE origin = 'agent'  AND agent IS NULL;
         UPDATE spoiler_rule  SET agent = 'Claude' WHERE origin = 'agent'  AND agent IS NULL;
         UPDATE spoiler_seen  SET agent = 'Claude' WHERE how    = 'agent'  AND agent IS NULL;
         UPDATE guide_fix_log SET agent = 'Claude' WHERE actor  = 'claude' AND agent IS NULL;",
    )?;
    Ok(())
}

/// 0007: `title_link` rebuilt to accept `join` rows with a `join_group` (a CHECK constraint
/// only changes with a new table). Its rows are copied, `join_group` too when a re-run finds
/// the column already there.
fn rebuild_title_link(conn: &Connection) -> Result<()> {
    let group = if has_column(conn, "title_link", "join_group")? { "join_group" } else { "NULL" };
    conn.execute_batch(&format!(
        "CREATE TABLE title_link_new (
           source_id  INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
           kind       TEXT    NOT NULL CHECK (kind IN ('movie', 'series')),
           item_key   TEXT    NOT NULL,
           action     TEXT    NOT NULL CHECK (action IN ('split', 'pin', 'join')),
           at         INTEGER NOT NULL,
           fp         TEXT    NOT NULL DEFAULT '{{}}',
           orphaned_since INTEGER,
           join_group INTEGER,
           PRIMARY KEY (source_id, kind, item_key, action)
         ) WITHOUT ROWID;
         INSERT INTO title_link_new(source_id, kind, item_key, action, at, fp, orphaned_since, join_group)
           SELECT source_id, kind, item_key, action, at, fp, orphaned_since, {group} FROM title_link;
         DROP TABLE title_link;
         ALTER TABLE title_link_new RENAME TO title_link;"
    ))?;
    Ok(())
}

/// The match columns of movies and series and `series.tmdb_id` (0006; only the missing ones,
/// so a renumbered re-run is harmless).
fn add_title_columns(conn: &Connection) -> Result<()> {
    for table in ["movie", "series"] {
        for (col, decl) in
            [("match_key", "TEXT"), ("match_hash", "INTEGER"), ("match_lang", "TEXT"), ("match_tags", "INTEGER"), ("match_origin", "TEXT")]
        {
            add_column_if_missing(conn, table, col, decl)?;
        }
    }
    add_column_if_missing(conn, "series", "tmdb_id", "INTEGER")?;
    // a database that ran this migration before it gained the user-data columns
    add_column_if_missing(conn, "title_link", "fp", "TEXT NOT NULL DEFAULT '{}'")?;
    add_column_if_missing(conn, "title_link", "orphaned_since", "INTEGER")
}

/// The explicit-content columns (DESIGN §20.3; added here when missing, so the migration is safe
/// to re-run) and `grp.explicit` from the stored category names: the provider's raw name, else
/// the display name (groups imported before P6). A provider flag (code 2) is never lowered.
fn backfill_explicit(conn: &Connection) -> Result<()> {
    for (table, col, decl) in [
        ("grp", "explicit", "INTEGER NOT NULL DEFAULT 0"),
        ("channel", "explicit", "INTEGER NOT NULL DEFAULT 0"),
        ("movie", "explicit", "INTEGER NOT NULL DEFAULT 0"),
        ("series", "explicit", "INTEGER NOT NULL DEFAULT 0"),
        ("group_override", "explicit", "INTEGER"),
    ] {
        add_column_if_missing(conn, table, col, decl)?;
    }
    let mut read = conn.prepare("SELECT id, COALESCE(raw_name, name) FROM grp WHERE explicit = 0")?;
    let mut write = conn.prepare("UPDATE grp SET explicit = ?2 WHERE id = ?1")?;
    let mut rows = read.query([])?;
    while let Some(r) = rows.next()? {
        if crate::sources::explicit::is_explicit_name(&r.get::<_, String>(1)?) {
            write.execute(params![r.get::<_, i64>(0)?, crate::sources::explicit::GROUP_BY_NAME])?;
        }
    }
    Ok(())
}

/// `quality_check` from the picture sizes measured before 0004 (`channel_video`): the claim is
/// the quality word of the name it was measured under; radio is no evidence.
fn seed_quality_checks(conn: &Connection) -> Result<()> {
    let mut read = conn.prepare(
        "SELECT v.source_id, v.name, v.width, v.height, v.at FROM channel_video v
         WHERE NOT EXISTS (SELECT 1 FROM channel c WHERE c.source_id = v.source_id AND c.key = v.item_key AND c.radio = 1)",
    )?;
    let mut write = conn.prepare("INSERT OR IGNORE INTO quality_check(source_id, name, claimed, measured, at) VALUES (?1, ?2, ?3, ?4, ?5)")?;
    let mut rows = read.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        let Some(measured) = Quality::of_video(r.get(2)?, r.get(3)?) else { continue };
        let claimed = clean_channel_name(&name).quality;
        write.execute(params![r.get::<_, i64>(0)?, name, claimed.map(Quality::code), measured.code(), r.get::<_, i64>(4)?])?;
    }
    Ok(())
}

/// Display names from the stored provider names with the current smart-name rules (names
/// are cleaned at import, so without this an existing database would show the old cleanup
/// until each playlist's next refresh): `channel.clean_name`, `movie` / `series.clean_name`
/// and `grp.name` (from `grp.raw_name`; groups imported before it was stored wait for the
/// refresh) for the sources with smart names on, and every channel's `quality` / `family`.
/// A source's smart-names setting is its own, else inferred from its rows (the app-wide
/// default lives in the config file): on when any stored display name differs from the
/// provider's. Only changed rows are written. Safe to re-run.
fn reclean_names(conn: &Connection) -> Result<()> {
    let sources: Vec<(i64, Option<bool>)> = conn
        .prepare("SELECT id, json_extract(settings, '$.smart_names') FROM source")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, setting) in sources {
        let smart = match setting {
            Some(on) => on,
            None => conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM channel WHERE source_id = ?1 AND clean_name <> name)
                     OR EXISTS (SELECT 1 FROM movie WHERE source_id = ?1 AND clean_name <> name)
                     OR EXISTS (SELECT 1 FROM series WHERE source_id = ?1 AND clean_name <> name)
                     OR EXISTS (SELECT 1 FROM grp WHERE source_id = ?1 AND raw_name IS NOT NULL AND name <> raw_name)",
                [id],
                |r| r.get(0),
            )?,
        };
        reclean_channels(conn, id, smart)?;
        if smart {
            for table in ["movie", "series"] {
                reclean_vod(conn, table, id)?;
            }
            let mut read = conn.prepare("SELECT id, raw_name, name FROM grp WHERE source_id = ?1 AND raw_name IS NOT NULL")?;
            let mut write = conn.prepare("UPDATE grp SET name = ?2 WHERE id = ?1")?;
            let mut rows = read.query([id])?;
            while let Some(r) = rows.next()? {
                let name = group_display(&r.get::<_, String>(1)?, true);
                if name != r.get::<_, String>(2)? {
                    write.execute(params![r.get::<_, i64>(0)?, name])?;
                }
            }
            crate::store::import::distinguish_group_names(conn, crate::model::SourceId(id), None)?;
        }
    }
    Ok(())
}

fn reclean_channels(conn: &Connection, source: i64, smart: bool) -> Result<()> {
    let mut read = conn.prepare("SELECT id, name, clean_name, quality, family FROM channel WHERE source_id = ?1")?;
    let mut write = conn.prepare("UPDATE channel SET clean_name = ?2, quality = ?3, family = ?4 WHERE id = ?1")?;
    let mut rows = read.query([source])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        let cleaned = clean_channel_name(&name);
        let q = crate::store::import::NameQuality::of(&cleaned);
        let (quality, family) = (q.as_ref().map(|q| i64::from(q.class.code())), q.map(|q| q.family));
        let clean = if smart { cleaned.name } else { r.get(2)? };
        if (&clean, quality, &family) != (&r.get::<_, String>(2)?, r.get(3)?, &r.get::<_, Option<String>>(4)?) {
            write.execute(params![r.get::<_, i64>(0)?, clean, quality, family])?;
        }
    }
    Ok(())
}

fn reclean_vod(conn: &Connection, table: &str, source: i64) -> Result<()> {
    let mut read = conn.prepare(&format!("SELECT id, name, clean_name FROM {table} WHERE source_id = ?1"))?;
    let mut write = conn.prepare(&format!("UPDATE {table} SET clean_name = ?2 WHERE id = ?1"))?;
    let mut rows = read.query([source])?;
    while let Some(r) = rows.next()? {
        let clean = clean_vod_name(&r.get::<_, String>(1)?).0;
        if clean != r.get::<_, String>(2)? {
            write.execute(params![r.get::<_, i64>(0)?, clean])?;
        }
    }
    Ok(())
}

/// `channel.quality` / `channel.family` (added here when missing, so the migration is safe to
/// re-run), filled for the channels imported before 0003 from their stored provider name with
/// the import's parse (`import_source::name_quality`; ~0.1 s per 100k channels, once).
fn backfill_channel_quality(conn: &Connection) -> Result<()> {
    for (col, ty) in [("quality", "INTEGER"), ("family", "TEXT")] {
        add_column_if_missing(conn, "channel", col, ty)?;
    }
    let mut read = conn.prepare("SELECT id, name FROM channel WHERE quality IS NULL")?;
    let mut write = conn.prepare("UPDATE channel SET quality = ?2, family = ?3 WHERE id = ?1")?;
    let mut rows = read.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        if let Some(q) = crate::store::import::NameQuality::of(&clean_channel_name(&name)) {
            write.execute(params![r.get::<_, i64>(0)?, q.class.code(), q.family])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        migrate(&mut c).unwrap();
        c
    }

    /// Incident 0.2.6: a database from a newer dev build (v14) was refused with a bare
    /// "config error". The refusal stays, now typed, and names the version that last opened it
    /// when that one recorded it (an implausible value is never quoted).
    #[test]
    fn a_newer_schema_is_refused_naming_the_build_that_opened_it() {
        let mut c = fresh();
        record_app_version(&c).unwrap();
        assert_eq!(recorded_app_version(&c).as_deref(), Some(env!("CARGO_PKG_VERSION")));
        let newer = latest_version() + 1;
        c.pragma_update(None, "user_version", newer).unwrap();
        c.execute("UPDATE meta SET value = '9.1.0' WHERE key = ?1", [APP_VERSION_KEY]).unwrap();
        match migrate(&mut c) {
            Err(crate::error::Error::NewerSchema { found, supported, opened_by }) => {
                assert_eq!((found, supported, opened_by.as_deref()), (newer, latest_version(), Some("9.1.0")));
            }
            other => panic!("{other:?}"),
        }
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, newer, "nothing was touched");
        // a build from before the record, or a value that isn't a version
        c.execute("UPDATE meta SET value = 'http://h.tv/live/u/p/1.ts' WHERE key = ?1", [APP_VERSION_KEY]).unwrap();
        let err = migrate(&mut c).unwrap_err();
        assert!(matches!(err, crate::error::Error::NewerSchema { opened_by: None, .. }), "{err:?}");
        assert_eq!(err.to_string(), format!("database schema v{newer} is newer than this build (v{}); refusing to open", latest_version()));
    }

    #[test]
    fn fresh_db_migrates() {
        let c = fresh();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, latest_version());
        let names: Vec<&str> = MIGRATIONS.iter().map(|m| m.0).collect();
        assert_eq!(
            names,
            [
                "0001_init",
                "0002_category_filter",
                "0003_quality",
                "0004_name_rules",
                "0005_explicit",
                "0006_title_stacks",
                "0007_title_join",
                "0008_reminder_modes",
                "0009_guide_fixes",
                "0010_play_queue",
                "0011_follow",
                "0012_spoiler_guard",
                "0013_agent_names",
                "0014_cast",
                "0015_watchlists"
            ]
        );
        for (t, col) in [
            ("grp", "raw_name"),
            ("group_override", "shown"),
            ("channel", "quality"),
            ("channel", "family"),
            ("channel_video", "width"),
            ("quality_check", "claimed"),
            ("grp", "explicit"),
            ("channel", "explicit"),
            ("movie", "explicit"),
            ("series", "explicit"),
            ("group_override", "explicit"),
            ("reminder", "mode"),
            ("reminder", "offset_s"),
            ("reminder", "open_in"),
            ("reminder", "rule_id"),
            ("item_override", "shift_min"),
            ("item_override", "guide_ok"),
            ("channel_epg_map", "shift_min"),
            ("channel_epg", "shift_min"),
            ("queue_item", "any_copy"),
            ("queue_item", "watchlist_item_id"),
            ("watchlist", "since"),
            ("watchlist_item", "season_from"),
            ("follow_match", "event_key"),
            ("spoiler_seen", "event_key"),
            ("guide_fix_log", "actor"),
            ("follow_rule", "agent"),
            ("spoiler_rule", "agent"),
            ("spoiler_seen", "agent"),
            ("guide_fix_log", "agent"),
        ] {
            assert!(has_column(&c, t, col).unwrap(), "{t}.{col}");
        }
    }

    /// The batch's migrations (0008–0013) are safe to re-run, the way a renumbered branch
    /// database re-runs them: every script and Rust step again on a migrated database changes
    /// nothing and fails nothing; an old switch row (`lead_s = -1`) reads as mode 'switch'.
    #[test]
    fn batch_migrations_are_idempotent() {
        let mut c = fresh();
        c.execute_batch(
            "INSERT INTO source(name, kind, url, created_at) VALUES ('S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0);
             INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s) VALUES (1, 'k', 'C', 'T', 10, 20, -1);
             UPDATE reminder SET mode = 'notify';",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        for (name, sql) in MIGRATIONS.iter().skip(7) {
            tx.execute_batch(sql).unwrap_or_else(|e| panic!("{name}: {e}"));
            backfill(&tx, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        tx.commit().unwrap();
        let mode: String = c.query_row("SELECT mode FROM reminder", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "switch");
    }

    /// The FTS contract of DESIGN §6.3: contentless FTS keyed by programme.id, inserted with the
    /// batch, deleted by rowid before the programme rows, joined back for display.
    #[test]
    fn fts_contentless_delete_by_rowid_and_join_back() {
        let c = fresh();
        c.execute_batch(
            "INSERT INTO epg_source(id,name,kind,active_gen) VALUES (1,'t','url',1);
             INSERT INTO epg_channel(id,epg_source_id,xmltv_id,seen_gen) VALUES (1,1,'a',1);
             INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title) VALUES (1,1,1,0,60,'Tom & Jerry');
             INSERT INTO programme_fts(rowid,title,sub_title) SELECT id,title,sub_title FROM programme WHERE gen=1;",
        )
        .unwrap();
        let q = "SELECT p.id FROM programme_fts f JOIN programme p ON p.id=f.rowid
                 JOIN epg_channel ec ON ec.id=p.epg_channel_id JOIN epg_source es ON es.id=ec.epg_source_id
                 WHERE programme_fts MATCH ?1 AND p.gen = es.active_gen";
        let id: i64 = c.query_row(q, ["\"jer\"*"], |r| r.get(0)).unwrap();
        // duplicate (channel, gen, start) → first wins
        let n = c
            .execute("INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title) VALUES (1,1,1,0,60,'Dup') ON CONFLICT(epg_channel_id,gen,start) DO NOTHING", [])
            .unwrap();
        assert_eq!(n, 0);
        // delete: FTS rows by rowid first, then programmes
        c.execute_batch(
            "DELETE FROM programme_fts WHERE rowid IN (SELECT id FROM programme WHERE gen=1);
             DELETE FROM programme WHERE gen=1;",
        )
        .unwrap();
        let hits: i64 = c.query_row("SELECT count(*) FROM programme_fts WHERE programme_fts MATCH '\"jer\"*'", [], |r| r.get(0)).unwrap();
        assert_eq!(hits, 0);
        // AUTOINCREMENT: ids are never reused
        c.execute("INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title) VALUES (1,1,2,0,60,'Other')", []).unwrap();
        let new_id: i64 = c.query_row("SELECT max(id) FROM programme", [], |r| r.get(0)).unwrap();
        assert!(new_id > id);
        // orphan sweep used by maintenance
        c.execute_batch("DELETE FROM programme_fts WHERE rowid NOT IN (SELECT id FROM programme);").unwrap();
    }

    /// Programme deletes are explicit: removing an EPG source that still has programmes fails.
    #[test]
    fn programmes_block_cascading_epg_deletes() {
        let c = fresh();
        c.execute_batch(
            "INSERT INTO epg_source(id,name,kind) VALUES (1,'t','url');
             INSERT INTO epg_channel(id,epg_source_id,xmltv_id,seen_gen) VALUES (1,1,'a',1);
             INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title) VALUES (1,1,1,0,60,'x');",
        )
        .unwrap();
        assert!(c.execute("DELETE FROM epg_source WHERE id=1", []).is_err());
        c.execute_batch(
            "DELETE FROM programme_fts WHERE rowid IN (SELECT id FROM programme WHERE epg_source_id=1);
             DELETE FROM programme WHERE epg_source_id=1; DELETE FROM epg_source WHERE id=1;",
        )
        .unwrap();
    }

    /// Runs migrations `0..n` only, like an older build.
    fn migrated_to(n: usize) -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        for (i, (_, sql)) in MIGRATIONS[..n].iter().enumerate() {
            let tx = c.transaction().unwrap();
            tx.execute_batch(sql).unwrap();
            backfill(&tx, MIGRATIONS[i].0).unwrap();
            tx.pragma_update(None, "user_version", (i + 1) as i64).unwrap();
            tx.commit().unwrap();
        }
        c
    }

    /// A database from before the quality migration gets the quality word (and family) of
    /// every stored channel name; the migration is safe to run again.
    #[test]
    fn quality_migration_backfills_from_the_provider_name() {
        let mut c = migrated_to(2);
        assert_eq!(MIGRATIONS[2].0, "0003_quality");
        c.execute_batch(
            "INSERT INTO source(id,name,kind,created_at) VALUES (1,'s','m3u-url',0);
             INSERT INTO channel(source_id,key,name,clean_name,norm_name,stream,provider_order,seen_gen) VALUES
               (1,'a','UK: BBC One FHD','BBC One','bbcone','{}',0,1),
               (1,'b','BBC One ᴴᴰ','BBC One','bbcone','{}',1,1),
               (1,'c','BBC One','BBC One','bbcone','{}',2,1);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        let q = |c: &Connection, k: &str| {
            c.query_row("SELECT quality, family FROM channel WHERE key = ?1", [k], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap()
        };
        assert_eq!(q(&c, "a"), (Some(3), Some("bbcone".into())));
        assert_eq!(q(&c, "b"), (Some(2), Some("bbcone".into())));
        assert_eq!(q(&c, "c"), (None, None));
        // safe to re-run
        c.pragma_update(None, "user_version", 2).unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(q(&c, "a"), (Some(3), Some("bbcone".into())));
    }

    /// 0013 on a 0.2.1 profile: agent rows made before names were kept are Claude's (only the
    /// Claude Code plugin could make them); the user's rows stay unnamed.
    #[test]
    fn agent_names_migration_names_old_rows_claude() {
        let mut c = migrated_to(12);
        assert_eq!(MIGRATIONS[12].0, "0013_agent_names");
        c.execute_batch(
            "INSERT INTO follow_rule(name, kind, spec, priority, origin, created_at, updated_at) VALUES
               ('Mine', 'team', '{}', 1, 'user', 0, 0), ('Theirs', 'team', '{}', 2, 'agent', 0, 0);
             INSERT INTO spoiler_rule(kind, label, origin, created_at) VALUES ('topic', 'Mine', 'user', 0), ('topic', 'Theirs', 'agent', 0);
             INSERT INTO spoiler_seen(event_key, first_start, title, seen_at, how) VALUES ('mine', 0, 'Mine', 0, 'live'), ('theirs', 0, 'Theirs', 0, 'agent');
             INSERT INTO guide_fix_log(at, actor, label, n) VALUES (0, 'you', 'Mine', 1), (0, 'claude', 'Theirs', 1);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        for (t, label) in [("follow_rule", "name"), ("spoiler_rule", "label"), ("spoiler_seen", "title"), ("guide_fix_log", "label")] {
            let named: Vec<(String, Option<String>)> = c
                .prepare(&format!("SELECT {label}, agent FROM {t} ORDER BY {label}"))
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(named, [("Mine".to_owned(), None), ("Theirs".to_owned(), Some("Claude".to_owned()))], "{t}");
        }
    }

    /// A database from a pre-merge build of the quality branch: v2 with that branch's
    /// `0002_quality` (now 0003) applied and no category-filter columns. It ends with every
    /// column, at the latest version.
    #[test]
    fn renumbered_quality_0002_database_gets_the_category_filter_columns() {
        let mut c = Connection::open_in_memory().unwrap();
        for (i, name) in ["0001_init", "0003_quality"].into_iter().enumerate() {
            let tx = c.transaction().unwrap();
            tx.execute_batch(MIGRATIONS.iter().find(|m| m.0 == name).unwrap().1).unwrap();
            backfill(&tx, name).unwrap();
            tx.pragma_update(None, "user_version", (i + 1) as i64).unwrap();
            tx.commit().unwrap();
        }
        assert!(!has_column(&c, "grp", "raw_name").unwrap());
        migrate(&mut c).unwrap();
        for (t, col) in [("grp", "raw_name"), ("group_override", "shown"), ("channel", "quality"), ("channel", "family")] {
            assert!(has_column(&c, t, col).unwrap(), "{t}.{col}");
        }
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, latest_version());
        migrate(&mut c).unwrap();
    }

    /// 0004: an existing database shows the current smart-name rules without a refresh (only
    /// for sources with smart names on, inferred when unset; quality words for all), and the
    /// sizes measured before it become trust evidence. Safe to run again.
    #[test]
    fn name_rules_migration_recleans_names_and_seeds_trust() {
        let mut c = migrated_to(3);
        assert_eq!(MIGRATIONS[3].0, "0004_name_rules");
        c.execute_batch(
            r#"INSERT INTO source(id,name,kind,created_at) VALUES (1,'s','m3u-url',0), (2,'raw','m3u-url',0);
               UPDATE source SET settings = '{"smart_names":false}' WHERE id = 2;
               INSERT INTO channel(source_id,key,name,clean_name,norm_name,stream,provider_order,seen_gen) VALUES
                 (1,'a','UK: ITV 4 4K ◉','ITV 4 ◉','itv4','{}',0,1),
                 (1,'b','US ★ CNN HD','US ★ CNN','uscnn','{}',1,1),
                 (2,'c','UK ★ BBC One HD','UK ★ BBC One HD','ukbbcone','{}',0,1);
               INSERT INTO grp(source_id,kind,key,name,raw_name,provider_order,seen_gen) VALUES
                 (1,'channel','channel:ukgeneral','UK ❖ General','UK ❖ General',0,1),
                 (1,'channel','channel:old','Old',NULL,1,1),
                 (1,'channel','channel:generalhevc','GENERAL ʰᵉᵛᶜ','GENERAL ʰᵉᵛᶜ',2,1);
               INSERT INTO movie(source_id,key,name,clean_name,norm_name,stream,provider_order,seen_gen) VALUES
                 (1,'m','EN ★ Dune 4K (2021)','EN ★ Dune','dune','{}',0,1);
               INSERT INTO channel_video(source_id,item_key,name,width,height,at) VALUES (1,'a','UK: ITV 4 4K ◉',1280,720,5);"#,
        )
        .unwrap();
        let check = |c: &Connection| {
            let ch = |k: &str| {
                c.query_row("SELECT clean_name, quality, family FROM channel WHERE key = ?1", [k], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?))
                })
                .unwrap()
            };
            assert_eq!(ch("a"), ("ITV 4".into(), Some(4), Some("itv4".into())));
            assert_eq!(ch("b"), ("CNN".into(), Some(2), Some("cnn".into())));
            assert_eq!(ch("c"), ("UK ★ BBC One HD".into(), Some(2), Some("bbcone".into())), "smart names off: raw name kept");
            let grp = |k: &str| c.query_row("SELECT name FROM grp WHERE key = ?1", [k], |r| r.get::<_, String>(0)).unwrap();
            // two categories that clean to the same name keep what tells them apart
            assert_eq!(
                (grp("channel:ukgeneral"), grp("channel:generalhevc"), grp("channel:old")),
                ("General · UK".into(), "General · HEVC".into(), "Old".into())
            );
            assert_eq!(c.query_row("SELECT clean_name FROM movie", [], |r| r.get::<_, String>(0)).unwrap(), "Dune");
            let qc = c.query_row("SELECT source_id, name, claimed, measured, at FROM quality_check", [], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
            });
            assert_eq!(qc.unwrap(), (1, "UK: ITV 4 4K ◉".into(), Some(4), 2, 5));
        };
        migrate(&mut c).unwrap();
        check(&c);
        c.pragma_update(None, "user_version", 3).unwrap();
        migrate(&mut c).unwrap();
        check(&c);
    }

    /// A database from before the stacks keys every stored movie and series from its provider
    /// name (no refresh needed), adds `series.tmdb_id` and `title_link`; running it again
    /// (renumbered after another branch's 0005) changes nothing.
    #[test]
    fn title_stacks_migration_keys_existing_titles() {
        let mut c = migrated_to(5);
        assert_eq!(MIGRATIONS[5].0, "0006_title_stacks");
        c.execute_batch(
            r#"INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','xtream',0), (2,'TV 2','xtream',0);
               INSERT INTO movie(source_id,key,name,clean_name,norm_name,year,stream,provider_order,seen_gen) VALUES
                 (1,'a','EN ★ Dune - 2021 [4k]','Dune','dune',2021,'{}',0,1),
                 (2,'b','AR-SUBS - Dune (2021)','Dune','dune',NULL,'{}',0,1),
                 (2,'c','SE - Gold','Gold','gold',NULL,'{}',1,1);
               INSERT INTO series(source_id,key,name,clean_name,norm_name,year,provider_order,seen_gen) VALUES
                 (2,'s','EN - Lewis (2007) (GB)','Lewis','lewis',NULL,0,1);"#,
        )
        .unwrap();
        let check = |c: &Connection| {
            let row = |t: &str, k: &str| {
                c.query_row(&format!("SELECT match_key, match_hash, match_lang, match_tags, match_origin FROM {t} WHERE key = ?1"), [k], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .unwrap()
            };
            let (a, b, gold, lewis) = (row("movie", "a"), row("movie", "b"), row("movie", "c"), row("series", "s"));
            assert_eq!((a.0.as_deref(), a.2.as_deref(), a.3), (Some("dune"), Some("EN"), i64::from(Quality::Uhd.code())));
            assert_eq!(a.1, b.1, "one stack across languages, the year from the name");
            assert_eq!(b.2.as_deref(), Some("AR-SUBS"));
            assert_eq!(gold.1, None, "no year: no key stacking");
            assert_eq!((lewis.0.as_deref(), lewis.4.as_deref()), (Some("lewis"), Some("GB")));
            let n: i64 = c.query_row("SELECT count(*) FROM pragma_table_info('series') WHERE name = 'tmdb_id'", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 1);
            let stamps: i64 = c.query_row("SELECT count(*) FROM meta WHERE key LIKE 'vod_match:%'", [], |r| r.get(0)).unwrap();
            assert_eq!(stamps, 2);
        };
        migrate(&mut c).unwrap();
        check(&c);
        c.execute("INSERT INTO title_link(source_id,kind,item_key,action,at) VALUES (1,'movie','a','split',5)", []).unwrap();
        c.pragma_update(None, "user_version", 5).unwrap();
        migrate(&mut c).unwrap();
        check(&c);
        let kept: i64 = c.query_row("SELECT count(*) FROM title_link", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1, "a re-run keeps the user's corrections");
        // a development database of the stacks branch, where this migration was 0005: the
        // explicit-content migration it skipped is applied on open
        let mut d = migrated_to(4);
        let tx = d.transaction().unwrap();
        tx.execute_batch(MIGRATIONS[5].1).unwrap();
        backfill(&tx, MIGRATIONS[5].0).unwrap();
        tx.pragma_update(None, "user_version", 5).unwrap();
        tx.commit().unwrap();
        assert!(!has_column(&d, "grp", "explicit").unwrap());
        migrate(&mut d).unwrap();
        assert!(has_column(&d, "grp", "explicit").unwrap() && has_column(&d, "movie", "explicit").unwrap());
    }

    /// 0007: `title_link` accepts "join" rows with a `join_group` and keeps the splits and pins
    /// it had (with their fingerprints); running it again changes nothing.
    #[test]
    fn title_join_migration_keeps_corrections_and_accepts_joins() {
        let mut c = migrated_to(6);
        assert_eq!(MIGRATIONS[6].0, "0007_title_join");
        c.execute_batch(
            r#"INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','xtream',0);
               INSERT INTO title_link(source_id,kind,item_key,action,at,fp) VALUES (1,'movie','a','split',5,'{"n":"x"}'), (1,'series','s','pin',6,'{}');"#,
        )
        .unwrap();
        assert!(
            c.execute("INSERT INTO title_link(source_id,kind,item_key,action,at) VALUES (1,'movie','b','join',7)", []).is_err(),
            "v6 has no joins"
        );
        let rows = |c: &Connection| -> Vec<(String, String, i64, String, Option<i64>)> {
            c.prepare("SELECT item_key, action, at, fp, join_group FROM title_link ORDER BY item_key")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        migrate(&mut c).unwrap();
        let kept = vec![("a".to_owned(), "split".to_owned(), 5, r#"{"n":"x"}"#.to_owned(), None), ("s".into(), "pin".into(), 6, "{}".into(), None)];
        assert_eq!(rows(&c), kept);
        c.execute("INSERT INTO title_link(source_id,kind,item_key,action,at,join_group) VALUES (1,'movie','b','join',7,3)", []).unwrap();
        c.pragma_update(None, "user_version", 6).unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(rows(&c).len(), 3, "a re-run keeps every row");
        assert_eq!(rows(&c)[1].4, Some(3), "and the join's group");
        c.execute_batch("PRAGMA foreign_keys=ON; DELETE FROM source WHERE id = 1").unwrap();
        assert!(rows(&c).is_empty(), "still cascades with its playlist");
    }

    /// 0005: categories whose stored names read as adult content are explicit without a
    /// refresh (raw name first, display name for groups imported before P6); innocent ones
    /// aren't. Safe to run again.
    #[test]
    fn explicit_migration_backfills_from_stored_names() {
        let mut c = migrated_to(4);
        assert_eq!(MIGRATIONS[4].0, "0005_explicit");
        c.execute_batch(
            "INSERT INTO source(id,name,kind,created_at) VALUES (1,'s','m3u-url',0);
             INSERT INTO grp(source_id,kind,key,name,raw_name,provider_order,seen_gen) VALUES
               (1,'channel','channel:xxx','Movies','|XXX| Movies',0,1),
               (1,'channel','channel:adults','For Adults',NULL,1,1),
               (1,'movie','movie:swim','Adult Swim','US | Adult Swim',2,1),
               (1,'series','series:news','News','UK: News',3,1);
             INSERT INTO group_override(source_id,kind,group_key,hidden) VALUES (1,'channel','channel:xxx',1);",
        )
        .unwrap();
        let check = |c: &Connection| {
            let codes: Vec<(String, i64)> = c
                .prepare("SELECT key, explicit FROM grp ORDER BY provider_order")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(codes, [("channel:xxx".into(), 1), ("channel:adults".into(), 1), ("movie:swim".into(), 0), ("series:news".into(), 0)]);
            let ov: Option<i64> = c.query_row("SELECT explicit FROM group_override", [], |r| r.get(0)).unwrap();
            assert_eq!(ov, None, "no user override until the user says so");
        };
        migrate(&mut c).unwrap();
        check(&c);
        c.execute("UPDATE grp SET explicit = 2 WHERE key = 'series:news'", []).unwrap();
        c.pragma_update(None, "user_version", 4).unwrap();
        migrate(&mut c).unwrap();
        assert_eq!(
            c.query_row("SELECT explicit FROM grp WHERE key = 'series:news'", [], |r| r.get::<_, i64>(0)).unwrap(),
            2,
            "a provider flag stays"
        );
    }

    /// DESIGN §42 §8.11: the v14 move preserves list order and orphan fingerprints, leaves
    /// channel-only lists alone, and runs once. Restoring an old backup uses this same step.
    #[test]
    fn watchlists_migration_moves_vod_favourites_once() {
        let mut c = migrated_to(14);
        c.execute_batch("INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','m3u-url',0);
          INSERT INTO movie(source_id,key,name,clean_name,norm_name,year,stream,provider_order,seen_gen) VALUES
            (1,'empire','Empire','Empire','empire',1980,'{}',0,1),(1,'hope','A New Hope','A New Hope','anewhope',1977,'{}',1,1);
          INSERT INTO fav_list(id,name,icon,sort_index,is_default) VALUES (20,'Trending','',1,0),(21,'Channels','',2,0),(22,'My list','',3,0);
          INSERT INTO fav_item(list_id,source_id,kind,item_key,position,added_at,fp,orphaned_since) VALUES
            (1,1,'movie','empire',0,5,'{}',NULL),(1,1,'channel','bbc',1,5,'{}',NULL),(1,1,'movie','hope',2,6,'{}',NULL),
            (20,1,'movie','orphan',0,5,'{\"norm_name\":\"missingfilm\"}',9),(21,1,'channel','itv',0,5,'{}',NULL),(22,1,'movie','hope',0,5,'{}',NULL);") .unwrap();
        migrate(&mut c).unwrap();
        let mine = crate::store::watchlist::default_list(&c).unwrap();
        let rows = crate::store::watchlist::entries(&c, mine).unwrap();
        assert_eq!(rows.iter().map(|e| (e.title.as_str(), e.position)).collect::<Vec<_>>(), [("Empire", 0), ("A New Hope", 1)]);
        let named = crate::store::watchlist::lists(&c).unwrap();
        assert!(named.iter().any(|l| l.name == "My list (favourites)"));
        let trending = named.iter().find(|l| l.name == "Trending").unwrap();
        let orphan = &crate::store::watchlist::entries(&c, trending.id).unwrap()[0];
        assert_eq!(orphan.orphaned_since, Some(9));
        assert_eq!(orphan.fingerprint.norm_name, "missingfilm");
        assert_eq!(c.query_row("SELECT count(*) FROM fav_item WHERE kind<>'channel'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(c.query_row("SELECT count(*) FROM fav_list WHERE id IN (20,22)", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(c.query_row("SELECT count(*) FROM fav_list WHERE id=21", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let item = crate::model::ItemRef {
            source: crate::model::SourceId(1),
            kind: crate::model::ItemKind::Movie,
            key: crate::model::ItemKey("empire".into()),
        };
        assert!(crate::store::user::set_favourite(&c, crate::model::FavListId(1), &item, true, 10).is_err());
        migrate(&mut c).unwrap();
        assert_eq!(crate::store::watchlist::lists(&c).unwrap().len(), 3);
    }

    #[test]
    fn user_data_tables_carry_fingerprints() {
        let c = fresh();
        for t in ["fav_item", "item_override", "watch_progress", "reminder", "channel_epg_map", "title_link"] {
            for col in ["fp", "orphaned_since"] {
                let n: i64 = c.query_row(&format!("SELECT count(*) FROM pragma_table_info('{t}') WHERE name='{col}'"), [], |r| r.get(0)).unwrap();
                assert_eq!(n, 1, "{t}.{col}");
            }
        }
    }
}
