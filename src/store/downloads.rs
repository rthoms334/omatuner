//! Private downloads and recording jobs (DESIGN §43 §3–§4). State changes go through the
//! single writer; consumers read a bounded snapshot, never the database on every frame.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::DownloadsConfig;
use crate::downloads::{Job, Kind, State};
use crate::error::{Error, Result};
use crate::model::{DownloadId, ItemKey, ItemRef, SourceId, Timestamp};

const COLS: &str = "id,position,kind,source_id,item_key,any_copy,prog_start,prog_stop,pad_before_s,pad_after_s,catchup_id,title,subtitle,state,reason,store_name,bytes_done,bytes_total,etag,last_modified,attempts,added_at,added_by,done_at,watched_at,fp,orphaned_since";

fn enumeration<T: serde::de::DeserializeOwned>(r: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<T> {
    let value: String = r.get(column)?;
    serde_json::from_value(serde_json::Value::String(value))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e)))
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        position: r.get(1)?,
        kind: enumeration(r, 2)?,
        source_id: r.get(3)?,
        item_key: r.get::<_, Option<String>>(4)?.map(ItemKey),
        any_copy: r.get(5)?,
        prog_start: r.get(6)?,
        prog_stop: r.get(7)?,
        pad_before_s: r.get(8)?,
        pad_after_s: r.get(9)?,
        catchup_id: r.get(10)?,
        title: r.get(11)?,
        subtitle: r.get(12)?,
        state: enumeration(r, 13)?,
        reason: r.get(14)?,
        store_name: r.get(15)?,
        bytes_done: r.get::<_, i64>(16)?.max(0) as u64,
        bytes_total: r.get::<_, Option<i64>>(17)?.map(|n| n.max(0) as u64),
        etag: r.get(18)?,
        last_modified: r.get(19)?,
        attempts: r.get(20)?,
        added_at: r.get(21)?,
        added_by: r.get(22)?,
        done_at: r.get(23)?,
        watched_at: r.get(24)?,
        fp: serde_json::from_str(&r.get::<_, String>(25)?).unwrap_or_default(),
        orphaned_since: r.get(26)?,
    })
}

pub fn jobs(c: &Connection) -> Result<Vec<Job>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM download ORDER BY position,id"))?.query_map([], row)?.collect::<rusqlite::Result<_>>()?)
}

pub fn job(c: &Connection, id: DownloadId) -> Result<Option<Job>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM download WHERE id=?1"))?.query_row([id], row).optional()?)
}

pub fn same_target(a: &Job, b: &Job) -> bool {
    if a.kind != b.kind || a.prog_start != b.prog_start {
        return false;
    }
    if a.source_id == b.source_id && a.item_key == b.item_key {
        return true;
    }
    a.any_copy
        && matches!(a.kind, Kind::Movie | Kind::Episode)
        && b.item().is_some_and(|item| {
            a.fp.get("download_copies")
                .and_then(|v| serde_json::from_value::<Vec<ItemRef>>(v.clone()).ok())
                .is_some_and(|copies| copies.contains(&item))
        })
}
pub fn duplicate(c: &Connection, job: &Job) -> Result<bool> {
    Ok(jobs(c)?.iter().any(|other| !matches!(other.state, State::Cancelled | State::Failed | State::Missed) && same_target(job, other)))
}

/// A high-water mark prevents Undo, a delayed worker, or an old timer from naming a newer
/// job after the largest row was deleted. Add and sequence update are one transaction.
pub fn add(c: &Connection, job: &Job) -> Result<DownloadId> {
    super::sql::atomic(c, |c| {
        if duplicate(c, job)? {
            return Err(Error::other("duplicate"));
        }
        let id = DownloadId(c.query_row("SELECT max(COALESCE((SELECT max(id) FROM download),0),COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='downloads:sequence'),0))+1", [], |r| r.get(0))?);
        let mut job = job.clone();
        job.id = id;
        job.position = c.query_row("SELECT COALESCE(max(position),-1)+1 FROM download", [], |r| r.get(0))?;
        put(c, &job)?;
        c.execute(
            "INSERT INTO meta(key,value) VALUES ('downloads:sequence',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [id.0.to_string()],
        )?;
        Ok(id)
    })
}

pub fn put(c: &Connection, j: &Job) -> Result<()> {
    c.execute(&format!("INSERT INTO download({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)
        ON CONFLICT(id) DO UPDATE SET position=excluded.position,kind=excluded.kind,source_id=excluded.source_id,item_key=excluded.item_key,any_copy=excluded.any_copy,
        prog_start=excluded.prog_start,prog_stop=excluded.prog_stop,pad_before_s=excluded.pad_before_s,pad_after_s=excluded.pad_after_s,catchup_id=excluded.catchup_id,
        title=excluded.title,subtitle=excluded.subtitle,state=excluded.state,reason=excluded.reason,store_name=excluded.store_name,bytes_done=excluded.bytes_done,
        bytes_total=excluded.bytes_total,etag=excluded.etag,last_modified=excluded.last_modified,attempts=excluded.attempts,done_at=excluded.done_at,watched_at=excluded.watched_at,fp=excluded.fp,orphaned_since=excluded.orphaned_since"),
        params![j.id,j.position,j.kind.as_str(),j.source_id,j.item_key.as_ref().map(ItemKey::as_str),j.any_copy,j.prog_start,j.prog_stop,
            j.pad_before_s,j.pad_after_s,j.catchup_id,j.title,j.subtitle,j.state.as_str(),j.reason,j.store_name,j.bytes_done.min(i64::MAX as u64) as i64,j.bytes_total.map(|n| n.min(i64::MAX as u64) as i64),
            j.etag,j.last_modified,j.attempts,j.added_at,j.added_by,j.done_at,j.watched_at,j.fp.to_string(),j.orphaned_since])?;
    Ok(())
}

pub fn set_state(c: &Connection, id: DownloadId, state: State, reason: Option<&str>) -> Result<()> {
    c.execute("UPDATE download SET state=?2,reason=?3 WHERE id=?1", params![id, state.as_str(), reason])?;
    Ok(())
}

pub fn remove(c: &Connection, id: DownloadId) -> Result<()> {
    c.execute("DELETE FROM download WHERE id=?1", [id])?;
    Ok(())
}

pub fn move_to(c: &Connection, id: DownloadId, to: usize) -> Result<()> {
    super::sql::atomic(c, |c| {
        let mut rows = jobs(c)?;
        let Some(from) = rows.iter().position(|j| j.id == id) else { return Ok(()) };
        let row = rows.remove(from);
        rows.insert(to.min(rows.len()), row);
        for (i, row) in rows.iter().enumerate() {
            c.execute("UPDATE download SET position=?2 WHERE id=?1", params![row.id, i as i64])?;
        }
        Ok(())
    })
}

pub fn root(c: &Connection, cfg: &DownloadsConfig) -> Result<PathBuf> {
    if !cfg.dir.is_empty() {
        return Ok(PathBuf::from(&cfg.dir));
    }
    let db: String = c.query_row("SELECT file FROM pragma_database_list WHERE name='main'", [], |r| r.get(0))?;
    Ok(Path::new(&db).parent().ok_or_else(|| Error::other("Downloads folder unavailable"))?.join("downloads"))
}

/// Crash recovery never promotes an incomplete file. A kept HLS index is retried through
/// finishing; no provider request is needed to complete it (DESIGN §43 §4, §8.11).
pub fn reconcile(c: &Connection, root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(root, e))?;
    c.execute("UPDATE download SET fp=json_remove(fp,'$.download_finish_failed') WHERE state='finishing'", [])?;
    let rows = jobs(c)?;
    for j in &rows {
        let path = j.path(root);
        let finished = root.join(format!("{}.mkv", j.id));
        let receipt = finished.with_extension("mkv.ok");
        if matches!(j.state, State::Running | State::Finishing) && finished.is_file() && receipt.is_file() {
            let mut recovered = j.clone();
            recovered.store_name = Some(format!("{}.mkv", j.id));
            recovered.bytes_done = std::fs::metadata(&finished).map_err(|e| Error::io(&finished, e))?.len();
            recovered.bytes_total = Some(recovered.bytes_done);
            recovered.state = if recovered.fp.get("download_capture_failed").and_then(|v| v.as_bool()) == Some(true) {
                State::Failed
            } else if recovered.fp.get("download_partial").and_then(|v| v.as_bool()) == Some(true) {
                State::Partial
            } else {
                State::Done
            };
            recovered.done_at = Some(crate::model::now());
            if let Ok(seconds) = std::fs::read_to_string(&receipt).unwrap_or_default().parse::<f64>() {
                recovered.fp["download_duration_s"] = seconds.into();
            }
            put(c, &recovered)?;
            let _ = std::fs::remove_dir_all(root.join(j.id.to_string()));
            let _ = std::fs::remove_file(receipt);
            continue;
        }
        if j.state == State::Running {
            let kept = root.join(j.id.to_string()).join("index.json").is_file();
            if kept && j.kind.recording() {
                let mut j = j.clone();
                j.state = State::Finishing;
                j.store_name = Some(format!("{}/", j.id));
                put(c, &j)?;
            } else {
                set_state(c, j.id, State::Waiting, None)?;
            }
        } else if j.state.playable() && !path.is_some_and(|p| p.is_file()) {
            set_state(c, j.id, State::Failed, Some("file missing"))?;
        }
    }
    for entry in std::fs::read_dir(root).map_err(|e| Error::io(root, e))? {
        let entry = entry.map_err(|e| Error::io(root, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".part") && entry.file_type().is_ok_and(|t| t.is_file()) {
            let id = name.split('.').next().and_then(|s| s.parse::<i64>().ok()).map(DownloadId);
            if let Some(job) = rows.iter().find(|j| Some(j.id) == id) {
                let bytes = entry.metadata().map_err(|e| Error::io(entry.path(), e))?.len();
                c.execute("UPDATE download SET bytes_done=?1 WHERE id=?2", params![bytes.min(i64::MAX as u64) as i64, job.id])?;
            } else {
                std::fs::remove_file(entry.path()).map_err(|e| Error::io(entry.path(), e))?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepRule {
    pub id: i64,
    pub source_id: SourceId,
    pub series_key: ItemKey,
    pub keep: u32,
    pub added_at: Timestamp,
    pub fp: serde_json::Value,
    pub orphaned_since: Option<Timestamp>,
}

pub fn rules(c: &Connection) -> Result<Vec<KeepRule>> {
    Ok(c.prepare_cached("SELECT id,source_id,series_key,keep,added_at,fp,orphaned_since FROM download_rule ORDER BY id")?
        .query_map([], |r| {
            Ok(KeepRule {
                id: r.get(0)?,
                source_id: r.get(1)?,
                series_key: ItemKey(r.get(2)?),
                keep: r.get(3)?,
                added_at: r.get(4)?,
                fp: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                orphaned_since: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn keep(c: &Connection, item: &ItemRef, count: u32, now: Timestamp) -> Result<i64> {
    if !(1..=10).contains(&count) {
        return Err(Error::other("Keep between 1 and 10 episodes"));
    }
    let mut fp = super::user::fingerprint_of(c, item)?;
    fp.url_key = None;
    let fp = serde_json::to_string(&fp).map_err(Error::other)?;
    super::sql::atomic(c, |c| {
        if let Some(id) = c
            .query_row("SELECT id FROM download_rule WHERE source_id=?1 AND series_key=?2", params![item.source, item.key.as_str()], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        {
            c.execute("UPDATE download_rule SET keep=?2 WHERE id=?1", params![id, count])?;
            return Ok(id);
        }
        c.execute(
            "INSERT INTO download_rule(source_id,series_key,keep,added_at,fp) VALUES (?1,?2,?3,?4,?5)",
            params![item.source, item.key.as_str(), count, now, fp],
        )?;
        Ok(c.last_insert_rowid())
    })
}

pub fn finished(c: &Connection, item: &ItemRef) -> Result<Option<Job>> {
    Ok(c.prepare_cached(&format!(
        "SELECT {COLS} FROM download WHERE source_id=?1 AND item_key=?2 AND kind=?3 AND state='done' ORDER BY done_at DESC LIMIT 1"
    ))?
    .query_row(params![item.source, item.key.as_str(), item.kind.as_str()], row)
    .optional()?)
}

pub fn total_bytes(c: &Connection) -> Result<u64> {
    Ok(c.query_row("SELECT COALESCE(sum(bytes_done),0) FROM download", [], |r| r.get::<_, i64>(0))?.max(0) as u64)
}

/// The estimate used for admission and the add confirmation: 2 GB per hour unless a
/// trustworthy response length is already known (DESIGN §43 §2.3).
pub fn estimate(j: &Job) -> u64 {
    j.bytes_total.unwrap_or_else(|| {
        let duration = j
            .stop()
            .zip(j.start())
            .map(|(end, start)| (end - start).max(0) as f64)
            .or_else(|| j.fp.get("download_duration_s").and_then(serde_json::Value::as_f64))
            .unwrap_or(3600.0);
        (duration * (2_000_000_000.0 / 3600.0)).ceil() as u64
    })
}

pub fn is_recording(j: &Job) -> bool {
    j.kind == Kind::Live
}

/// Backups carry identities and rules, never media or transport validators.
pub fn export(c: &Connection) -> Result<Option<serde_json::Value>> {
    if !crate::store::migrations::has_column(c, "download", "id")? {
        return Ok(None);
    }
    let mut rows = jobs(c)?;
    let rules = rules(c)?;
    if rows.is_empty() && rules.is_empty() {
        return Ok(None);
    }
    for j in &mut rows {
        j.etag = None;
        j.last_modified = None;
        if let Some(fp) = j.fp.as_object_mut() {
            fp.remove("download_validators");
        }
    }
    Ok(Some(serde_json::json!({"v":1,"rows":rows,"rules":rules,"media_included":false})))
}

pub fn import(c: &Connection, value: &serde_json::Value, remap: &dyn Fn(&ItemRef) -> Option<ItemRef>) -> Result<usize> {
    let rows: Vec<Job> = serde_json::from_value(value.get("rows").cloned().unwrap_or_else(|| serde_json::json!([]))).map_err(Error::other)?;
    let rules: Vec<KeepRule> = serde_json::from_value(value.get("rules").cloned().unwrap_or_else(|| serde_json::json!([]))).map_err(Error::other)?;
    let mut added = 0;
    for mut j in rows {
        let item = j.item().as_ref().and_then(remap);
        j.source_id = item.as_ref().map(|i| i.source);
        j.orphaned_since = j.source_id.is_none().then_some(crate::model::now());
        j.item_key = item.map(|i| i.key).or(j.item_key);
        j.store_name = None;
        j.bytes_done = 0;
        j.bytes_total = None;
        j.etag = None;
        j.last_modified = None;
        if j.state == State::Scheduled && j.source_id.is_some() {
            j.reason = None;
        } else {
            j.state = State::Failed;
            j.reason = Some("file missing".into());
        }
        if let Some(fp) = j.fp.as_object_mut() {
            fp.remove("download_follow_rule");
            fp.remove("download_keep_rule");
            fp.remove("download_validators");
        }
        if let Some(copies) = j.fp.get("download_copies").and_then(|v| serde_json::from_value::<Vec<ItemRef>>(v.clone()).ok()) {
            j.fp["download_copies"] = serde_json::to_value(copies.iter().filter_map(remap).collect::<Vec<_>>()).map_err(Error::other)?;
        }
        if !jobs(c)?.iter().any(|other| same_target(&j, other)) && add(c, &j).is_ok() {
            added += 1;
        }
    }
    for rule in rules {
        let item = ItemRef { source: rule.source_id, kind: crate::model::ItemKind::Series, key: rule.series_key };
        if let Some(item) = remap(&item) {
            keep(c, &item, rule.keep, rule.added_at)?;
            added += 1;
        }
    }
    Ok(added)
}

/// Recorded programmes and removed-playlist files keep progress under their numeric job id.
pub fn save_progress(c: &Connection, id: DownloadId, position: f64, duration: Option<f64>, finished: bool) -> Result<()> {
    c.execute("UPDATE download SET fp=json_set(fp,'$.download_position_s',?2,'$.download_duration_s',COALESCE(?3,json_extract(fp,'$.download_duration_s'))), watched_at=CASE WHEN ?4 THEN COALESCE(watched_at,?5) ELSE NULL END WHERE id=?1",
        params![id,position,duration,finished,crate::model::now()])?;
    Ok(())
}
