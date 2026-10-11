//! Schema migrations keyed by `PRAGMA user_version`. Implemented in the skeleton.
//!
//! FROZEN (skeleton). Contract: migrations are append-only (WP-02 appends entries after the
//! `skeleton-v1` tag, with architect sign-off). Each entry runs inside one transaction and bumps
//! `user_version` to its index + 1. Never edit a shipped migration; add `000N_*.sql`.

use rusqlite::Connection;

use crate::error::Result;

/// Ordered migration scripts.
pub const MIGRATIONS: &[(&str, &str)] = &[("0001_init", include_str!("migrations/0001_init.sql"))];

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
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
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
