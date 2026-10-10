//! Helpers shared by the repository modules. Owner: WP-02 (moved out of the frozen
//! `store/mod.rs` so WP-02 can grow it freely).
//!
//! Write rules (DESIGN §6.3/§7.2): upserts are `INSERT … ON CONFLICT(…) DO UPDATE`; never
//! `INSERT OR REPLACE` on a table referenced by a foreign key.

use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::model::{Fingerprint, ItemKind};

pub fn fp_to_json(fp: &Fingerprint) -> String { serde_json::to_string(fp).unwrap_or_else(|_| "{}".into()) }
pub fn fp_from_json(s: &str) -> Fingerprint { serde_json::from_str(s).unwrap_or_default() }
pub fn kind_str(k: ItemKind) -> &'static str { k.as_str() }

/// `ItemKind` from a TEXT column (unknown values are a data error, not a panic).
pub(crate) fn kind_from_sql(s: &str) -> rusqlite::Result<ItemKind> {
    ItemKind::parse(s).ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, format!("bad item kind {s:?}").into()))
}

/// Fingerprint JSON for a write, or `None` when the item no longer exists (the default
/// fingerprint): SQL then keeps the stored value via `COALESCE(?, fp)`.
pub(crate) fn fp_opt(fp: &Fingerprint) -> Option<String> { (*fp != Fingerprint::default()).then(|| fp_to_json(fp)) }

/// Serialize a unit-like serde enum (kebab/lowercase rename) to its bare string.
pub(crate) fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Parse a unit-like serde enum from its bare string.
pub(crate) fn enum_parse<T: DeserializeOwned>(s: &str) -> Option<T> { serde_json::from_value(serde_json::Value::String(s.to_owned())).ok() }

/// JSON text of a value (`None` → SQL NULL).
pub(crate) fn to_json<T: Serialize>(v: &T) -> Result<String> { serde_json::to_string(v).map_err(Error::other) }

/// Parse an optional JSON column; malformed JSON is treated as absent (logged).
pub(crate) fn from_json_opt<T: DeserializeOwned>(s: Option<String>) -> Option<T> {
    let s = s?;
    match serde_json::from_str(&s) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("ignoring malformed JSON column: {e}");
            None
        }
    }
}

/// A JSON column read as a typed value (`FromSql` adaptor for `row.get::<_, Json<T>>`).
pub(crate) struct Json<T>(pub T);

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = v.as_str()?;
        serde_json::from_str(s).map(Json).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

/// Run `f` atomically on a shared connection, whether or not a transaction is already open
/// (SAVEPOINTs nest; `Connection::transaction` needs `&mut` and cannot nest). Rolled back on
/// error.
pub(crate) fn atomic<R>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
    static N: AtomicU64 = AtomicU64::new(0);
    let name = format!("tuner_sp{}", N.fetch_add(1, Ordering::Relaxed));
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f(conn) {
        Ok(r) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(r)
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

/// JSON array text of ids, for `… IN (SELECT value FROM json_each(?))` (one bound parameter
/// regardless of the list length).
pub(crate) fn json_ids(ids: impl IntoIterator<Item = i64>) -> String {
    let mut s = String::from("[");
    for (i, id) in ids.into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&id.to_string());
    }
    s.push(']');
    s
}

/// The `<kind>:` prefix stripped from a group key (`Fingerprint.norm_group`).
pub(crate) fn group_norm(key: &str) -> String { key.split_once(':').map(|(_, n)| n).unwrap_or(key).to_owned() }

/// `Error::NotFound` with a uniform message.
pub(crate) fn not_found(what: &str, id: impl std::fmt::Display) -> Error { Error::NotFound(format!("{what} {id}")) }

/// Map `QueryReturnedNoRows` to `Error::NotFound`.
pub(crate) fn or_not_found<T>(r: rusqlite::Result<T>, what: &str, id: impl std::fmt::Display) -> Result<T> {
    match r {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(not_found(what, id)),
        Err(e) => Err(e.into()),
    }
}

/// Saturating u32 from a SQLite integer (counts never go negative).
pub(crate) fn u32_of(v: i64) -> u32 { v.clamp(0, u32::MAX as i64) as u32 }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpgMatchMethod, SourceColor};

    #[test]
    fn enum_round_trip() {
        assert_eq!(enum_str(&EpgMatchMethod::TvgIdExact), "tvg-id-exact");
        assert_eq!(enum_parse::<EpgMatchMethod>("tvg-id-exact"), Some(EpgMatchMethod::TvgIdExact));
        assert_eq!(enum_str(&SourceColor::Magenta), "magenta");
        assert_eq!(enum_parse::<SourceColor>("nope"), None);
    }

    #[test]
    fn savepoints_nest_and_roll_back() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t(x)").unwrap();
        let r: Result<()> = atomic(&c, |c| {
            c.execute("INSERT INTO t VALUES (1)", [])?;
            atomic(c, |c| {
                c.execute("INSERT INTO t VALUES (2)", [])?;
                Ok(())
            })?;
            Err(Error::other("boom"))
        });
        assert!(r.is_err());
        let n: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        assert!(c.is_autocommit());
        assert_eq!(json_ids([1, 2, 3]), "[1,2,3]");
        assert_eq!(group_norm("channel:uk news"), "uk news");
    }
}
