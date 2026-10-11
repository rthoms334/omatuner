//! Schema migrations keyed by `PRAGMA user_version`. Implemented in the skeleton.
//!
//! FROZEN (skeleton). Contract: migrations are append-only (WP-02 appends entries after the
//! `skeleton-v1` tag, with architect sign-off). Each entry runs inside one transaction and bumps
//! `user_version` to its index + 1. Never edit a shipped migration; add `000N_*.sql`. A
//! migration whose new columns need data SQL can't compute (it needs Rust parsing) has a
//! [`backfill`] step, run in the same transaction right after its script.

use rusqlite::{Connection, params};

use crate::error::Result;

/// Ordered migration scripts.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    ("0002_category_filter", include_str!("migrations/0002_category_filter.sql")),
    ("0003_quality", include_str!("migrations/0003_quality.sql")),
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
    if name == "0003_quality" {
        backfill_channel_quality(conn)?;
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
        if let Some(q) = crate::store::import::NameQuality::of(&crate::sources::smart_names::clean_channel_name(&name)) {
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
        assert_eq!(names, ["0001_init", "0002_category_filter", "0003_quality"]);
        for (t, col) in [("grp", "raw_name"), ("group_override", "shown"), ("channel", "quality"), ("channel", "family"), ("channel_video", "width")]
        {
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
