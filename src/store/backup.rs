//! Export / import of all user state as one JSON document (UHF backup; DESIGN §19).
//! Owner: WP-02.
//!
//! Format: `tuner-backup.json`, `{"format":"tuner-backup","version":1,...}`. Catalogue rows
//! are NOT exported (they come back on refresh); user data carries fingerprints so it
//! re-attaches against the freshly imported catalogue. Credentials are included only when the
//! user ticks "Include passwords" (default off); the file is written 0600 either way.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{
    EpgSourceId, EpgSourceKind, FavListId, FavouriteEntry, FavouriteList, GroupOverrideRow, HistoryEntry, ItemOverrideRow, ItemRef, Reminder,
    SourceColor, SourceId, SourceKind, SourceSettings, Timestamp, WatchProgress,
};
use crate::store::sql::{self, enum_str};
use crate::store::user;

pub const FORMAT: &str = "tuner-backup";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackupOptions {
    pub include_credentials: bool,
    pub include_history: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupSource {
    /// Original id; used only to re-map references inside this file.
    pub id: i64,
    pub name: String,
    pub kind: SourceKind,
    pub icon: String,
    pub color: SourceColor,
    pub url: Option<String>,
    pub path: Option<String>,
    pub xt_server: Option<String>,
    pub xt_username: Option<String>,
    pub xt_password: Option<String>,
    pub settings: SourceSettings,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupEpgSource {
    pub name: String,
    pub kind: String,
    pub url: Option<String>,
    pub path: Option<String>,
    pub priority: i32,
    pub offset_min: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupManualMapping {
    pub source_id: i64,
    pub channel_key: String,
    /// Matched by name on import (EPG source ids differ between installs).
    pub epg_source_name: String,
    pub xmltv_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    pub format: String,
    pub version: u32,
    pub created_at: Timestamp,
    pub app_version: String,
    pub sources: Vec<BackupSource>,
    pub epg_sources: Vec<BackupEpgSource>,
    pub fav_lists: Vec<FavouriteList>,
    pub fav_items: Vec<FavouriteEntry>,
    pub item_overrides: Vec<ItemOverrideRow>,
    pub group_overrides: Vec<GroupOverrideRow>,
    pub progress: Vec<WatchProgress>,
    pub history: Vec<HistoryEntry>,
    pub reminders: Vec<Reminder>,
    pub manual_mappings: Vec<BackupManualMapping>,
    /// "Not the same title" splits, "Always play from" pins (DESIGN §29.11; absent in backups
    /// from before P2) and "Same title as…" joins (§29.16, P3).
    #[serde(default)]
    pub title_links: Vec<BackupTitleLink>,
    /// Raw `config.toml` text.
    pub config: Option<String>,
}

/// One `title_link` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupTitleLink {
    pub item: crate::model::ItemRef,
    /// `split`, `pin` or `join`.
    pub action: String,
    pub at: Timestamp,
    /// A join's group (copies with the same group stack together).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// Add sources/lists that don't exist (matched by name+kind+url), merge user data.
    Merge,
    /// Wipe all sources and user data first.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RestoreReport {
    pub sources_added: u32,
    pub sources_matched: u32,
    pub user_rows: u32,
    /// Sources restored without credentials (user must re-enter them).
    pub needs_credentials: Vec<String>,
    /// The backup's settings for this machine (`Config::restored`; set by the restore job when
    /// the backup has a readable `config.toml`). The job doesn't apply them: the app does, as a
    /// Settings change (saves `config.toml`, applies live), when the user restores settings.
    pub settings: Option<Box<crate::config::Config>>,
}

/// Credential-free form of a URL (userinfo, credential query values and Xtream path
/// credentials masked as `***`) — what an export without passwords stores.
///
/// Deliberately NOT `redact::redact_url`: that also masks every registered secret, and every
/// query value of every source URL is registered, so harmless parts (`?country=uk`) would be
/// lost from the backup for good.
fn strip_credentials(url: &str) -> String {
    static USERINFO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([a-z][a-z0-9+.-]*://)([^/@\s:]+)(:[^/@\s]*)?@").expect("valid regex"));
    // same credential keys as the log redaction (DESIGN §20)
    static QUERY_CRED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)([?&;](?:username|user|password|pass|pwd|token|auth|key|sig|signature|apikey|api_key|access_token)=)([^&#\s]*)")
            .expect("valid regex")
    });
    static XTREAM_PATH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(/(?:live|movie|series|timeshift)/)([^/\s?#]+)/([^/\s?#]+)/").expect("valid regex"));
    let out = USERINFO.replace(url, "${1}***@");
    let out = QUERY_CRED.replace_all(&out, "${1}***");
    XTREAM_PATH.replace_all(&out, "${1}***/***/").into_owned()
}

/// A URL that went through [`strip_credentials`] and lost something.
fn is_stripped(url: &str) -> bool {
    url.contains("***")
}

pub fn export(conn: &Connection, opts: BackupOptions, config_toml: Option<String>, now: Timestamp) -> Result<Backup> {
    let creds = opts.include_credentials;
    let sources = crate::store::sources::list_sources(conn)?
        .into_iter()
        .map(|s| BackupSource {
            id: s.id.0,
            name: s.name,
            kind: s.kind,
            icon: s.icon,
            color: s.color,
            url: s.url.map(|u| if creds { u.into_inner() } else { strip_credentials(u.expose()) }),
            path: s.path.map(|p| p.to_string_lossy().into_owned()),
            xt_server: s.xtream.as_ref().map(|x| x.server.clone()),
            xt_username: s.xtream.as_ref().map(|x| x.username.clone()),
            xt_password: s.xtream.as_ref().filter(|_| creds).map(|x| x.password.expose().clone()),
            settings: s.settings,
            enabled: s.enabled,
        })
        .collect();
    // provider guides come back with their playlist's refresh; only user guides are exported
    let epg_sources = crate::store::sources::list_epg_sources(conn)?
        .into_iter()
        .filter_map(|e| {
            let (kind, url, path) = match e.kind {
                EpgSourceKind::Url { url } => ("url", Some(if creds { url.into_inner() } else { strip_credentials(url.expose()) }), None),
                EpgSourceKind::File { path } => ("file", None, Some(path.to_string_lossy().into_owned())),
                EpgSourceKind::Provider { .. } => return None,
            };
            Some(BackupEpgSource { name: e.name, kind: kind.into(), url, path, priority: e.priority, offset_min: e.offset_min, enabled: e.enabled })
        })
        .collect();
    let fav_items = {
        let mut stmt = conn.prepare_cached(&format!("SELECT {} FROM fav_item ORDER BY list_id, position, added_at", user::FAV_ITEM_COLS))?;
        stmt.query_map([], user::fav_entry_from_row)?.collect::<rusqlite::Result<_>>()?
    };
    let progress = {
        let mut stmt = conn.prepare_cached(&format!("SELECT {} FROM watch_progress ORDER BY updated_at DESC", user::PROGRESS_COLS))?;
        stmt.query_map([], user::progress_from_row)?.collect::<rusqlite::Result<_>>()?
    };
    let manual_mappings = {
        let mut stmt = conn.prepare_cached(
            "SELECT m.source_id, m.channel_key, es.name, m.xmltv_id FROM channel_epg_map m JOIN epg_source es ON es.id = m.epg_source_id
             WHERE m.method = 'manual' ORDER BY m.source_id, m.channel_key",
        )?;
        stmt.query_map([], |r| {
            Ok(BackupManualMapping { source_id: r.get(0)?, channel_key: r.get(1)?, epg_source_name: r.get(2)?, xmltv_id: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    Ok(Backup {
        format: FORMAT.into(),
        version: VERSION,
        created_at: now,
        app_version: env!("CARGO_PKG_VERSION").into(),
        sources,
        epg_sources,
        fav_lists: user::fav_lists(conn)?,
        fav_items,
        item_overrides: user::item_overrides(conn, None)?,
        group_overrides: user::group_overrides(conn)?,
        progress,
        history: if opts.include_history { user::all_history(conn)? } else { Vec::new() },
        reminders: user::every_reminder(conn)?,
        manual_mappings,
        title_links: crate::store::titles::links(conn)?
            .into_iter()
            .map(|l| BackupTitleLink { item: l.item, action: l.action.as_str().to_owned(), at: l.at, group: l.group })
            .collect(),
        config: config_toml,
    })
}

/// Write pretty JSON with mode 0600.
///
/// Written to `<path>.part` (created 0600) and renamed over `path`, so an interrupted export
/// never leaves a truncated backup behind.
pub fn write_file(path: &Path, b: &Backup) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let json = serde_json::to_vec_pretty(b).map_err(Error::other)?;
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&part)?;
        // an existing .part may have been created with a wider mode
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(&part, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&part);
        Error::io(path, e)
    })
}

/// Read + validate `format`/`version`.
pub fn read_file(path: &Path) -> Result<Backup> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| Error::BadPayload(format!("not a Tuner backup file ({e})")))?;
    if v.get("format").and_then(|f| f.as_str()) != Some(FORMAT) {
        return Err(Error::BadPayload("not a Tuner backup file".into()));
    }
    let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    if version == 0 || version > u64::from(VERSION) {
        return Err(Error::BadPayload(format!("backup version {version} is not supported by this build (max {VERSION})")));
    }
    serde_json::from_value(v).map_err(|e| Error::parse(None, format!("backup: {e}")))
}

/// Restore into the DB in one transaction. The caller then schedules a refresh of every
/// restored source (user data re-attaches on that import).
///
/// Sources whose credentials were not exported (Xtream without password, URLs with masked
/// parts) are restored disabled and listed in `needs_credentials`, so no refresh hits the
/// provider with placeholder credentials. Merge keeps local data on conflicts (progress: the
/// newer row wins). Manual EPG mappings whose guide is not present (provider guides are only
/// recreated by the next refresh) are skipped.
pub fn restore(conn: &mut Connection, b: &Backup, mode: RestoreMode) -> Result<RestoreReport> {
    if b.format != FORMAT || b.version == 0 || b.version > VERSION {
        return Err(Error::BadPayload("not a supported Tuner backup".into()));
    }
    let now = crate::model::now();
    let tx = conn.transaction()?;
    let mut report = RestoreReport::default();

    if mode == RestoreMode::Replace {
        for s in crate::store::sources::list_sources(&tx)? {
            crate::store::sources::purge_source(&tx, s.id)?;
        }
        for e in crate::store::sources::list_epg_sources(&tx)? {
            crate::store::sources::purge_epg_source(&tx, e.id)?;
        }
        tx.execute_batch("DELETE FROM fav_item; DELETE FROM fav_list WHERE is_default = 0; DELETE FROM history;")?;
    }

    // ---- sources
    let existing = crate::store::sources::list_sources(&tx)?;
    let mut source_map: HashMap<i64, SourceId> = HashMap::new();
    for s in &b.sources {
        let matched = (mode == RestoreMode::Merge)
            .then(|| {
                existing.iter().find(|e| {
                    let url = e.url.as_ref().map(|u| u.expose().as_str());
                    let same_url = url == s.url.as_deref() || url.map(strip_credentials).as_deref() == s.url.as_deref();
                    let server = e.xtream.as_ref().map(|x| x.server.as_str()) == s.xt_server.as_deref();
                    // a playlist upgraded to its Xtream login (or switched back) since the backup
                    // is still the same source: same name and playlist URL / file (§27.3)
                    let origin = (e.kind == SourceKind::Xtream) != (s.kind == SourceKind::Xtream)
                        && same_url
                        && e.path.as_ref().map(|p| p.to_string_lossy()).as_deref() == s.path.as_deref();
                    e.name == s.name && ((e.kind == s.kind && same_url && server) || origin)
                })
            })
            .flatten();
        if let Some(e) = matched {
            source_map.insert(s.id, e.id);
            report.sources_matched += 1;
            continue;
        }
        let needs = (s.kind == SourceKind::Xtream && s.xt_password.as_deref().is_none_or(|p| p.is_empty() || is_stripped(p)))
            || s.url.as_deref().is_some_and(is_stripped);
        tx.execute(
            "INSERT INTO source(name, kind, icon, color, url, path, xt_server, xt_username, xt_password, settings, enabled,
                                sort_index, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, (SELECT COALESCE(max(sort_index), -1) + 1 FROM source), ?12)",
            params![
                s.name,
                s.kind.as_str(),
                s.icon,
                enum_str(&s.color),
                s.url,
                s.path,
                s.xt_server,
                s.xt_username,
                s.xt_password,
                sql::to_json(&s.settings)?,
                s.enabled && !needs,
                now,
            ],
        )?;
        source_map.insert(s.id, SourceId(tx.last_insert_rowid()));
        report.sources_added += 1;
        if needs {
            report.needs_credentials.push(s.name.clone());
        }
    }

    // ---- user EPG sources
    let existing_epg = crate::store::sources::list_epg_sources(&tx)?;
    for e in &b.epg_sources {
        let kind_of = |k: &EpgSourceKind| match k {
            EpgSourceKind::Url { .. } => "url",
            EpgSourceKind::File { .. } => "file",
            EpgSourceKind::Provider { .. } => "provider",
        };
        if mode == RestoreMode::Merge && existing_epg.iter().any(|x| x.name == e.name && kind_of(&x.kind) == e.kind) {
            continue;
        }
        if e.kind != "url" && e.kind != "file" {
            continue;
        }
        let stripped = e.url.as_deref().is_some_and(is_stripped);
        tx.execute(
            "INSERT INTO epg_source(name, kind, url, path, priority, offset_min, enabled) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![e.name, e.kind, e.url, e.path, e.priority, e.offset_min, e.enabled && !stripped],
        )?;
    }
    let epg_by_name: HashMap<String, EpgSourceId> = crate::store::sources::list_epg_sources(&tx)?.into_iter().rev().map(|e| (e.name, e.id)).collect();

    // ---- favourite lists
    let local_lists = user::fav_lists(&tx)?;
    let default_list = user::default_fav_list(&tx)?;
    let mut list_map: HashMap<FavListId, FavListId> = HashMap::new();
    for l in &b.fav_lists {
        let id = if l.is_default {
            if mode == RestoreMode::Replace {
                user::rename_fav_list(&tx, default_list, &l.name, &l.icon)?;
            }
            default_list
        } else if let Some(x) = local_lists.iter().find(|x| !x.is_default && x.name == l.name).filter(|_| mode == RestoreMode::Merge) {
            x.id
        } else {
            let id = user::create_fav_list(&tx, &l.name, &l.icon)?;
            tx.execute("UPDATE fav_list SET sort_index = ?2 WHERE id = ?1", params![id, l.sort_index])?;
            id
        };
        list_map.insert(l.id, id);
    }

    let remap = |item: &ItemRef| source_map.get(&item.source.0).map(|s| ItemRef { source: *s, ..item.clone() });
    let mut rows = 0usize;

    // ---- favourites
    let mut offsets: HashMap<FavListId, i64> = HashMap::new();
    for e in &b.fav_items {
        let (Some(list), Some(item)) = (list_map.get(&e.list), remap(&e.item)) else { continue };
        let offset = match offsets.get(list) {
            Some(o) => *o,
            None => {
                let o: i64 = tx.query_row("SELECT COALESCE(max(position), -1) + 1 FROM fav_item WHERE list_id = ?1", [list], |r| r.get(0))?;
                offsets.insert(*list, o);
                o
            }
        };
        rows += tx.execute(
            "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp, orphaned_since)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(list_id, source_id, kind, item_key) DO NOTHING",
            params![
                list,
                item.source,
                item.kind.as_str(),
                item.key.as_str(),
                offset + i64::from(e.position),
                e.added_at,
                sql::fp_to_json(&e.fingerprint),
                e.orphaned_since
            ],
        )?;
    }

    // ---- overrides
    for o in &b.item_overrides {
        let Some(item) = remap(&o.item) else { continue };
        rows += tx.execute(
            "INSERT INTO item_override(source_id, kind, item_key, rename, hidden, locked, user_order, catchup_offset_s, fp, orphaned_since)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(source_id, kind, item_key) DO NOTHING",
            params![
                item.source,
                item.kind.as_str(),
                item.key.as_str(),
                o.ov.rename,
                o.ov.hidden,
                o.ov.locked,
                o.ov.user_order,
                o.ov.catchup_offset_s,
                sql::fp_to_json(&o.fingerprint),
                o.orphaned_since,
            ],
        )?;
    }
    for g in &b.group_overrides {
        let Some(source) = source_map.get(&g.source.0) else { continue };
        rows += tx.execute(
            "INSERT INTO group_override(source_id, kind, group_key, rename, hidden, locked, user_order, pinned_home, shown, explicit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(source_id, kind, group_key) DO NOTHING",
            params![
                source,
                g.kind.as_str(),
                g.key.0,
                g.ov.rename,
                g.ov.hidden,
                g.ov.locked,
                g.ov.user_order,
                g.ov.pinned_home,
                g.ov.shown,
                g.ov.explicit
            ],
        )?;
    }

    // a backup's join groups are numbered after the groups already here (a merge never
    // combines its groups with unrelated ones)
    let group_base: i64 = tx.query_row("SELECT COALESCE(max(join_group), 0) FROM title_link", [], |r| r.get(0))?;
    for t in &b.title_links {
        let Some(item) = remap(&t.item) else { continue };
        let kind = match item.kind {
            crate::model::ItemKind::Movie => "movie",
            crate::model::ItemKind::Series => "series",
            _ => continue,
        };
        let group = t.group.filter(|_| t.action == "join").map(|g| group_base + g);
        if t.action == "join" && group.is_none() {
            continue;
        }
        rows += tx.execute(
            "INSERT INTO title_link(source_id, kind, item_key, action, at, join_group) SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE ?4 IN ('split', 'pin', 'join') ON CONFLICT(source_id, kind, item_key, action) DO NOTHING",
            params![item.source, kind, item.key.as_str(), t.action, t.at, group],
        )?;
    }

    // ---- progress (newer wins), history, reminders
    for p in &b.progress {
        let Some(item) = remap(&p.item) else { continue };
        rows += tx.execute(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(source_id, kind, item_key) DO UPDATE SET position_s = excluded.position_s, duration_s = excluded.duration_s,
               finished = excluded.finished, updated_at = excluded.updated_at, title = excluded.title, fp = excluded.fp
             WHERE excluded.updated_at > watch_progress.updated_at",
            params![
                item.source,
                item.kind.as_str(),
                item.key.as_str(),
                p.position_s,
                p.duration_s,
                p.finished,
                p.updated_at,
                p.title,
                sql::fp_to_json(&p.fingerprint)
            ],
        )?;
    }
    for h in b.history.iter().rev() {
        let Some(item) = remap(&h.item) else { continue };
        // merging the same backup twice must not duplicate entries
        rows += tx.execute(
            "INSERT INTO history(source_id, kind, item_key, title, at) SELECT ?1, ?2, ?3, ?4, ?5
             WHERE NOT EXISTS (SELECT 1 FROM history WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3 AND at = ?5)",
            params![item.source, item.kind.as_str(), item.key.as_str(), h.title, h.at],
        )?;
    }
    tx.execute("DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY at DESC, id DESC LIMIT -1 OFFSET 500)", [])?;
    for r in &b.reminders {
        let Some(item) = remap(&r.channel) else { continue };
        rows += tx.execute(
            "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(source_id, channel_key, start) DO NOTHING",
            params![item.source, item.key.as_str(), r.channel_name, r.programme_title, r.start, r.stop, r.lead_s, enum_str(&r.state)],
        )?;
    }

    // ---- manual EPG mappings (by guide name)
    for m in &b.manual_mappings {
        let (Some(source), Some(epg)) = (source_map.get(&m.source_id), epg_by_name.get(&m.epg_source_name)) else { continue };
        rows += tx.execute(
            "INSERT INTO channel_epg_map(source_id, channel_key, epg_source_id, xmltv_id, method, confidence)
             VALUES (?1, ?2, ?3, ?4, 'manual', 1.0)
             ON CONFLICT(source_id, channel_key, epg_source_id) DO UPDATE SET xmltv_id = excluded.xmltv_id, method = 'manual',
               confidence = 1.0
             WHERE channel_epg_map.method != 'manual'",
            params![source, m.channel_key, epg, m.xmltv_id],
        )?;
    }
    crate::store::epg::resolve_mappings(&tx)?;
    tx.commit()?;
    crate::store::sources::register_secrets(conn)?;
    report.user_rows = rows as u32;
    Ok(report)
}
