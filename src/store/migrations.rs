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
];

/// Current schema version after all migrations.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Apply pending migrations. Refuses to open a DB newer than this binary.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > latest_version() {
        return Err(crate::error::Error::Config(format!(
            "database schema v{current} is newer than this build (v{}); refusing to open",
            latest_version()
        )));
    }
    reconcile_renumbered(conn, current)?;
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        tracing::info!("applying migration {name}");
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        backfill(&tx, name)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// `user_version` is positional, so a database from a pre-merge build of the quality-badges
/// branch is at v2 with *its* `0002_quality` applied, not `0002_category_filter`. Such a
/// database (v2+ without `grp.raw_name`) gets the category-filter columns here; the
/// renumbered `0003_quality` then re-runs harmlessly (it is idempotent).
fn reconcile_renumbered(conn: &mut Connection, current: i64) -> Result<()> {
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

fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT count(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2", [table, col], |r| r.get(0))?)
}

fn add_column_if_missing(conn: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
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
        _ => Ok(()),
    }
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

    #[test]
    fn fresh_db_migrates() {
        let c = fresh();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, latest_version());
        let names: Vec<&str> = MIGRATIONS.iter().map(|m| m.0).collect();
        assert_eq!(names, ["0001_init", "0002_category_filter", "0003_quality", "0004_name_rules", "0005_explicit"]);
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
        ] {
            assert!(has_column(&c, t, col).unwrap(), "{t}.{col}");
        }
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

    #[test]
    fn user_data_tables_carry_fingerprints() {
        let c = fresh();
        for t in ["fav_item", "item_override", "watch_progress", "reminder", "channel_epg_map"] {
            for col in ["fp", "orphaned_since"] {
                let n: i64 = c.query_row(&format!("SELECT count(*) FROM pragma_table_info('{t}') WHERE name='{col}'"), [], |r| r.get(0)).unwrap();
                assert_eq!(n, 1, "{t}.{col}");
            }
        }
    }
}
