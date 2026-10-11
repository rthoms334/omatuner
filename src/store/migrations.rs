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
pub const MIGRATIONS: &[(&str, &str)] =
    &[("0001_init", include_str!("migrations/0001_init.sql")), ("0002_quality", include_str!("migrations/0002_quality.sql"))];

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

/// The Rust part of migration `name`, if it has one.
fn backfill(conn: &Connection, name: &str) -> Result<()> {
    if name == "0002_quality" {
        backfill_channel_quality(conn)?;
    }
    Ok(())
}

/// `channel.quality` of the channels imported before 0002, from their stored provider name
/// (the same parse the import does; ~0.1 s per 100k channels, once).
fn backfill_channel_quality(conn: &Connection) -> Result<()> {
    let mut read = conn.prepare("SELECT id, name FROM channel")?;
    let mut write = conn.prepare("UPDATE channel SET quality = ?2 WHERE id = ?1")?;
    let mut rows = read.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        if let Some(q) = crate::sources::smart_names::clean_channel_name(&name).quality {
            write.execute(params![r.get::<_, i64>(0)?, q.code()])?;
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

    /// A database from before 0002 gets the quality word of every stored channel name.
    #[test]
    fn quality_migration_backfills_from_the_provider_name() {
        let mut c = Connection::open_in_memory().unwrap();
        let (name, sql) = MIGRATIONS[0];
        let tx = c.transaction().unwrap();
        tx.execute_batch(sql).unwrap();
        tx.pragma_update(None, "user_version", 1).unwrap();
        tx.commit().unwrap();
        assert_eq!(name, "0001_init");
        c.execute_batch(
            "INSERT INTO source(id,name,kind,created_at) VALUES (1,'s','m3u-url',0);
             INSERT INTO channel(source_id,key,name,clean_name,norm_name,stream,provider_order,seen_gen) VALUES
               (1,'a','UK: BBC One FHD','BBC One','bbcone','{}',0,1),
               (1,'b','BBC One ᴴᴰ','BBC One','bbcone','{}',1,1),
               (1,'c','BBC One','BBC One','bbcone','{}',2,1);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        let q = |k: &str| c.query_row("SELECT quality FROM channel WHERE key = ?1", [k], |r| r.get::<_, Option<i64>>(0)).unwrap();
        assert_eq!(q("a"), Some(3));
        assert_eq!(q("b"), Some(2));
        assert_eq!(q("c"), None);
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
