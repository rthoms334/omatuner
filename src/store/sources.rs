//! Source + EPG-source repository. Owner: WP-02.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::Result;
use crate::model::{
    AccountInfo, EpgSource, EpgSourceId, EpgSourceKind, Source, SourceCounts, SourceDraft, SourceId, SourceKind, SourceSettings, SourceStatus,
    Timestamp, XtreamCredentials,
};
use crate::redact::Secret;
use crate::store::sql::{self, atomic, enum_parse, enum_str, from_json_opt, or_not_found, u32_of};

/// Catalogue rows deleted per transaction when a whole source goes away (§5.3: no
/// transaction larger than one batch).
const DELETE_CHUNK: i64 = 2_000;
/// Programme rows deleted per transaction (FTS rows first).
const PROGRAMME_CHUNK: usize = 20_000;

const SOURCE_COLS: &str = "id, name, kind, icon, color, url, path, xt_server, xt_username, xt_password, settings, enabled, \
     sort_index, created_at, last_refresh, last_attempt, last_error, etag, last_modified, account, n_channels, n_movies, \
     n_series, n_groups";

fn source_from_row(r: &Row<'_>) -> rusqlite::Result<Source> {
    let kind_s: String = r.get(2)?;
    let kind = enum_parse::<SourceKind>(&kind_s)
        .ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, format!("bad source kind {kind_s:?}").into()))?;
    let xt_server: Option<String> = r.get(7)?;
    let xtream = xt_server.map(|server| -> rusqlite::Result<_> {
        Ok(XtreamCredentials {
            server,
            username: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            password: Secret::new(r.get::<_, Option<String>>(9)?.unwrap_or_default()),
        })
    });
    Ok(Source {
        id: r.get(0)?,
        name: r.get(1)?,
        kind,
        icon: r.get(3)?,
        color: enum_parse(&r.get::<_, String>(4)?).unwrap_or_default(),
        url: r.get::<_, Option<String>>(5)?.map(Secret::new),
        path: r.get::<_, Option<String>>(6)?.map(PathBuf::from),
        xtream: xtream.transpose()?,
        settings: from_json_opt(r.get(10)?).unwrap_or_default(),
        enabled: r.get(11)?,
        sort_index: r.get(12)?,
        created_at: r.get(13)?,
        status: SourceStatus {
            last_refresh: r.get(14)?,
            last_attempt: r.get(15)?,
            last_error: r.get(16)?,
            etag: r.get(17)?,
            last_modified: r.get(18)?,
            account: from_json_opt(r.get(19)?),
            counts: SourceCounts { channels: u32_of(r.get(20)?), movies: u32_of(r.get(21)?), series: u32_of(r.get(22)?), groups: u32_of(r.get(23)?) },
        },
    })
}

/// Register the secrets of one source/URL (new or edited sources; startup uses
/// [`register_secrets`] for the whole DB).
fn register_values(url: Option<&str>, xtream: Option<&XtreamCredentials>) {
    if let Some(x) = xtream {
        crate::redact::register_secret(&x.username);
        crate::redact::register_secret(x.password.expose());
    }
    if let Some(u) = url.and_then(|u| url::Url::parse(u).ok()) {
        for (_, v) in u.query_pairs() {
            crate::redact::register_secret(&v);
        }
        if let Some(p) = u.password() {
            crate::redact::register_secret(p);
        }
    }
}

fn path_str(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

/// All sources ordered by `sort_index, id`, credentials included (UI must only display them
/// through `redact`).
pub fn list_sources(conn: &Connection) -> Result<Vec<Source>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {SOURCE_COLS} FROM source ORDER BY sort_index, id"))?;
    let rows = stmt.query_map([], source_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_source(conn: &Connection, id: SourceId) -> Result<Source> {
    let r = conn.query_row(&format!("SELECT {SOURCE_COLS} FROM source WHERE id = ?1"), [id], source_from_row);
    or_not_found(r, "source", id)
}

/// Insert a validated draft; returns the new id. Also registers its secrets with
/// `redact::register_secret` and, for Xtream / M3U-with-`url-tvg`, does NOT create the provider
/// EPG source (the import job does that once it knows the URL).
pub fn insert_source(conn: &Connection, draft: &SourceDraft, now: Timestamp) -> Result<SourceId> {
    let x = draft.xtream.as_ref();
    conn.execute(
        "INSERT INTO source(name, kind, icon, color, url, path, xt_server, xt_username, xt_password, settings, enabled,
                            sort_index, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, (SELECT COALESCE(max(sort_index), -1) + 1 FROM source), ?11)",
        params![
            draft.name,
            draft.kind.as_str(),
            draft.icon,
            enum_str(&draft.color),
            draft.url,
            path_str(&draft.path),
            x.map(|x| &x.server),
            x.map(|x| &x.username),
            x.map(|x| x.password.expose()),
            sql::to_json(&draft.settings)?,
            now,
        ],
    )?;
    register_values(draft.url.as_deref(), x);
    Ok(SourceId(conn.last_insert_rowid()))
}

/// Update user-editable fields (name, icon, color, url/path/credentials, settings, enabled).
///
/// Never clears the Xtream login of a row whose stored kind is `xtream` (DESIGN §27.3): a copy
/// read before a silent upgrade has `xtream: None` and would otherwise wipe the new login.
pub fn update_source(conn: &Connection, src: &Source) -> Result<()> {
    let x = src.xtream.as_ref();
    let n = conn.execute(
        "UPDATE source SET name = ?2, icon = ?3, color = ?4, url = ?5, path = ?6,
                xt_server = CASE WHEN ?7 IS NULL AND kind = 'xtream' THEN xt_server ELSE ?7 END,
                xt_username = CASE WHEN ?7 IS NULL AND kind = 'xtream' THEN xt_username ELSE ?8 END,
                xt_password = CASE WHEN ?7 IS NULL AND kind = 'xtream' THEN xt_password ELSE ?9 END,
                settings = ?10, enabled = ?11
         WHERE id = ?1",
        params![
            src.id,
            src.name,
            src.icon,
            enum_str(&src.color),
            src.url.as_ref().map(|u| u.expose()),
            path_str(&src.path),
            x.map(|x| &x.server),
            x.map(|x| &x.username),
            x.map(|x| x.password.expose()),
            sql::to_json(&src.settings)?,
            src.enabled,
        ],
    )?;
    if n == 0 {
        return Err(sql::not_found("source", src.id));
    }
    register_values(src.url.as_ref().map(|u| u.expose().as_str()), x);
    Ok(())
}

/// Switch a source between M3U and its Xtream login (DESIGN §27.3): sets `kind`, stores
/// `xtream` when given (credential columns are otherwise kept, so switching back and forth
/// needs no new detection), clears the ETag / Last-Modified validators (the next M3U download
/// must be complete) and registers the secrets. The M3U URL / file is never cleared.
///
/// Also forgets the recorded upgrade attempt ([`record_upgrade_attempt`]).
pub fn set_source_kind(conn: &Connection, id: SourceId, kind: SourceKind, xtream: Option<&XtreamCredentials>) -> Result<()> {
    let n = conn.execute("UPDATE source SET kind = ?2, etag = NULL, last_modified = NULL WHERE id = ?1", params![id, kind.as_str()])?;
    if n == 0 {
        return Err(sql::not_found("source", id));
    }
    if let Some(x) = xtream {
        conn.execute(
            "UPDATE source SET xt_server = ?2, xt_username = ?3, xt_password = ?4 WHERE id = ?1",
            params![id, x.server, x.username, x.password.expose()],
        )?;
        register_values(None, Some(x));
    }
    conn.execute("DELETE FROM meta WHERE key = ?1", [upgrade_key(id)])?;
    Ok(())
}

/// `meta` key of the last unsuccessful Xtream upgrade check of an M3U source (DESIGN §27.3).
fn upgrade_key(id: SourceId) -> String {
    format!("xtream-upgrade:{}", id.0)
}

/// The last check of an M3U source for an Xtream login that left it M3U.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpgradeAttempt {
    pub at: Timestamp,
    /// A login was found but did not work (`false`: the playlist carries none).
    #[serde(default)]
    pub login_failed: bool,
}

/// The last upgrade check of an M3U source (DESIGN §27.3). `None`: never, or it switched since.
pub fn upgrade_attempted_at(conn: &Connection, id: SourceId) -> Result<Option<UpgradeAttempt>> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [upgrade_key(id)], |r| r.get(0)).optional()?;
    Ok(from_json_opt(v))
}

/// Record an upgrade check that left the source M3U (see [`upgrade_attempted_at`]).
pub fn record_upgrade_attempt(conn: &Connection, id: SourceId, at: Timestamp, login_failed: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![upgrade_key(id), sql::to_json(&UpgradeAttempt { at, login_failed })?],
    )?;
    Ok(())
}

/// Delete `table` rows matching `cond` (which binds `?1`) in chunks, one savepoint each.
pub(crate) fn delete_chunked(conn: &Connection, table: &str, cond: &str, arg: i64) -> Result<usize> {
    let sql = format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE {cond} LIMIT {DELETE_CHUNK})");
    let mut total = 0;
    loop {
        let n = atomic(conn, |c| Ok(c.execute(&sql, [arg])?))?;
        total += n;
        if n == 0 {
            return Ok(total);
        }
    }
}

/// Body of [`delete_source`] on a shared connection (also used by backup restore).
pub(crate) fn purge_source(conn: &Connection, id: SourceId) -> Result<()> {
    let scope = crate::store::epg::ProgrammeScope::PlaylistSource(id);
    while crate::store::epg::delete_programmes_in(conn, &scope, PROGRAMME_CHUNK)? > 0 {}
    // big catalogues go in chunks so user writes interleave; the final delete cascades the rest
    delete_chunked(conn, "episode", "source_id = ?1", id.0)?;
    for t in ["channel", "movie", "series"] {
        delete_chunked(conn, t, "source_id = ?1", id.0)?;
    }
    atomic(conn, |c| {
        c.execute("DELETE FROM meta WHERE key LIKE ?1 OR key = ?2", params![format!("import:{}:%", id.0), upgrade_key(id)])?;
        c.execute("DELETE FROM source WHERE id = ?1", [id])?;
        Ok(())
    })
}

/// Delete a source: first `epg::delete_programmes_of(PlaylistSource(id))` in chunks (FTS rows
/// first), then the row (cascades catalogue, provider EPG sources/channels and user data).
pub fn delete_source(conn: &mut Connection, id: SourceId) -> Result<()> {
    purge_source(conn, id)?;
    // other sources' channels mapped to this source's provider guide fall back to the next one
    crate::store::epg::resolve_mappings(conn)
}

/// Persist `sort_index` = position in `order`.
pub fn reorder_sources(conn: &mut Connection, order: &[SourceId]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE source SET sort_index = ?2 WHERE id = ?1")?;
        for (i, id) in order.iter().enumerate() {
            stmt.execute(params![id, i as i64])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn set_source_settings(conn: &Connection, id: SourceId, settings: &SourceSettings) -> Result<()> {
    let n = conn.execute("UPDATE source SET settings = ?2 WHERE id = ?1", params![id, sql::to_json(settings)?])?;
    if n == 0 {
        return Err(sql::not_found("source", id));
    }
    Ok(())
}

/// Record the start of a refresh attempt (`last_attempt`).
pub fn mark_attempt(conn: &Connection, id: SourceId, now: Timestamp) -> Result<()> {
    conn.execute("UPDATE source SET last_attempt = ?2 WHERE id = ?1", params![id, now])?;
    Ok(())
}

/// Record a successful refresh: `last_refresh`, counts, etag/last-modified, clears `last_error`.
pub fn mark_refreshed(conn: &Connection, id: SourceId, status: &SourceStatus) -> Result<()> {
    let c = &status.counts;
    conn.execute(
        "UPDATE source SET last_refresh = ?2, last_attempt = COALESCE(?3, last_attempt), last_error = NULL, etag = ?4,
                last_modified = ?5, n_channels = ?6, n_movies = ?7, n_series = ?8, n_groups = ?9,
                account = COALESCE(?10, account)
         WHERE id = ?1",
        params![
            id,
            status.last_refresh,
            status.last_attempt,
            status.etag,
            status.last_modified,
            c.channels,
            c.movies,
            c.series,
            c.groups,
            status.account.as_ref().map(sql::to_json).transpose()?,
        ],
    )?;
    Ok(())
}

/// Record a failure (message must already be redacted).
pub fn mark_failed(conn: &Connection, id: SourceId, error: &str, now: Timestamp) -> Result<()> {
    // defence in depth: the message is shown in Settings and logged
    let error = crate::redact::redact(error);
    conn.execute("UPDATE source SET last_error = ?2, last_attempt = ?3 WHERE id = ?1", params![id, error.as_ref(), now])?;
    Ok(())
}

pub fn set_account(conn: &Connection, id: SourceId, account: &AccountInfo) -> Result<()> {
    conn.execute("UPDATE source SET account = ?2 WHERE id = ?1", params![id, sql::to_json(account)?])?;
    Ok(())
}

/// Sources whose `last_refresh` is older than their `refresh_h` (or `default_h`), enabled only.
pub fn stale_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<SourceId>> {
    let mut stmt = conn.prepare_cached("SELECT id, settings, last_refresh FROM source WHERE enabled = 1 ORDER BY sort_index, id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<Timestamp>>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, settings, last) = row?;
        let h = from_json_opt::<SourceSettings>(settings).and_then(|s| s.refresh_h).unwrap_or(default_h);
        if last.is_none_or(|t| t <= now - i64::from(h) * 3600) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Register every stored credential/URL secret with the redactor. Called once at startup
/// (implemented in the skeleton: it runs on every launch). Registers Xtream usernames and
/// passwords and every query-parameter value of M3U / XMLTV URLs.
pub fn register_secrets(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT xt_username, xt_password, url FROM source UNION ALL SELECT NULL, NULL, url FROM epg_source")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
    for row in rows {
        let (user, pass, url) = row?;
        for s in [user, pass].into_iter().flatten() {
            crate::redact::register_secret(&s);
        }
        if let Some(u) = url.as_deref().and_then(|u| url::Url::parse(u).ok()) {
            for (_, v) in u.query_pairs() {
                crate::redact::register_secret(&v);
            }
            if let Some(p) = u.password() {
                crate::redact::register_secret(p);
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ EPG sources

const EPG_COLS: &str = "id, name, kind, url, path, playlist_source_id, priority, offset_min, enabled, refresh_h, active_gen, \
     last_refresh, last_error, etag, last_modified, n_channels, n_programmes, max_stop";

fn epg_source_from_row(r: &Row<'_>) -> rusqlite::Result<EpgSource> {
    let kind: String = r.get(2)?;
    let url = || -> rusqlite::Result<Secret<String>> { Ok(Secret::new(r.get::<_, Option<String>>(3)?.unwrap_or_default())) };
    let kind = match kind.as_str() {
        "url" => EpgSourceKind::Url { url: url()? },
        "file" => EpgSourceKind::File { path: PathBuf::from(r.get::<_, Option<String>>(4)?.unwrap_or_default()) },
        "provider" => EpgSourceKind::Provider { source_id: r.get::<_, Option<SourceId>>(5)?.unwrap_or(SourceId(0)), url: url()? },
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, format!("bad EPG kind {other:?}").into()));
        }
    };
    Ok(EpgSource {
        id: r.get(0)?,
        name: r.get(1)?,
        kind,
        priority: r.get(6)?,
        offset_min: r.get(7)?,
        enabled: r.get(8)?,
        refresh_h: r.get::<_, Option<i64>>(9)?.map(u32_of),
        active_generation: r.get(10)?,
        last_refresh: r.get(11)?,
        last_error: r.get(12)?,
        etag: r.get(13)?,
        last_modified: r.get(14)?,
        channel_count: u32_of(r.get(15)?),
        programme_count: u32_of(r.get(16)?),
        max_stop: r.get(17)?,
    })
}

/// (kind, url, path, playlist source) columns of an EPG source kind.
fn epg_kind_cols(kind: &EpgSourceKind) -> (&'static str, Option<&str>, Option<String>, Option<SourceId>) {
    match kind {
        EpgSourceKind::Url { url } => ("url", Some(url.expose().as_str()), None, None),
        EpgSourceKind::File { path } => ("file", None, Some(path.to_string_lossy().into_owned()), None),
        EpgSourceKind::Provider { source_id, url } => ("provider", Some(url.expose().as_str()), None, Some(*source_id)),
    }
}

/// Ordered by `priority, id`.
pub fn list_epg_sources(conn: &Connection) -> Result<Vec<EpgSource>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {EPG_COLS} FROM epg_source ORDER BY priority, id"))?;
    let rows = stmt.query_map([], epg_source_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_epg_source(conn: &Connection, id: EpgSourceId) -> Result<EpgSource> {
    let r = conn.query_row(&format!("SELECT {EPG_COLS} FROM epg_source WHERE id = ?1"), [id], epg_source_from_row);
    or_not_found(r, "EPG source", id)
}

/// Insert a user EPG source (URL or file) at the end of the priority list.
pub fn insert_epg_source(conn: &Connection, name: &str, kind: &EpgSourceKind, offset_min: i32) -> Result<EpgSourceId> {
    let (k, url, path, playlist) = epg_kind_cols(kind);
    // provider guides go first (priority 0); user guides after everything else
    let priority_sql = if playlist.is_some() { "0" } else { "(SELECT COALESCE(max(priority), 99) + 1 FROM epg_source)" };
    conn.execute(
        &format!(
            "INSERT INTO epg_source(name, kind, url, path, playlist_source_id, priority, offset_min)
             VALUES (?1, ?2, ?3, ?4, ?5, {priority_sql}, ?6)"
        ),
        params![name, k, url, path, playlist, offset_min],
    )?;
    register_values(url, None);
    Ok(EpgSourceId(conn.last_insert_rowid()))
}

/// Upsert the provider EPG source for a playlist source (one per distinct URL); removes
/// provider EPG sources of that playlist whose URL is no longer advertised. Provider sources
/// are inserted with priority 0 (before user sources) unless the user reordered them.
pub fn ensure_provider_epg(conn: &mut Connection, source: SourceId, urls: &[String]) -> Result<Vec<EpgSourceId>> {
    let name: String = or_not_found(conn.query_row("SELECT name FROM source WHERE id = ?1", [source], |r| r.get(0)), "source", source)?;
    let mut wanted: Vec<&str> = Vec::new();
    for u in urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
        if !wanted.contains(&u) {
            wanted.push(u);
        }
    }
    let existing: Vec<(EpgSourceId, String)> = {
        let mut stmt = conn.prepare_cached("SELECT id, COALESCE(url, '') FROM epg_source WHERE kind = 'provider' AND playlist_source_id = ?1")?;
        let rows = stmt.query_map([source], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // advertised no more: programmes first (chunked), then the row (cascades channels/mappings)
    let mut purged = false;
    for (id, _) in existing.iter().filter(|(_, u)| !wanted.contains(&u.as_str())) {
        purge_epg_source(conn, *id)?;
        purged = true;
    }
    let tx = conn.transaction()?;
    if purged {
        // channels that used a dropped guide fall back to the next source
        crate::store::epg::resolve_mappings(&tx)?;
    }
    let mut ids = Vec::with_capacity(wanted.len());
    for (i, url) in wanted.iter().enumerate() {
        if let Some((id, _)) = existing.iter().find(|(_, u)| u == url) {
            ids.push(*id);
            continue;
        }
        let label = if wanted.len() > 1 { format!("{name} guide {}", i + 1) } else { format!("{name} guide") };
        let kind = EpgSourceKind::Provider { source_id: source, url: Secret::new((*url).to_owned()) };
        ids.push(insert_epg_source(&tx, &label, &kind, 0)?);
    }
    tx.commit()?;
    Ok(ids)
}

pub fn update_epg_source(conn: &Connection, src: &EpgSource) -> Result<()> {
    let (k, url, path, playlist) = epg_kind_cols(&src.kind);
    let n = conn.execute(
        "UPDATE epg_source SET name = ?2, kind = ?3, url = ?4, path = ?5, playlist_source_id = ?6, priority = ?7,
                offset_min = ?8, enabled = ?9, refresh_h = ?10
         WHERE id = ?1",
        params![src.id, src.name, k, url, path, playlist, src.priority, src.offset_min, src.enabled, src.refresh_h],
    )?;
    if n == 0 {
        return Err(sql::not_found("EPG source", src.id));
    }
    register_values(url, None);
    Ok(())
}

/// Body of [`delete_epg_source`] on a shared connection.
pub(crate) fn purge_epg_source(conn: &Connection, id: EpgSourceId) -> Result<()> {
    let scope = crate::store::epg::ProgrammeScope::EpgSource(id);
    while crate::store::epg::delete_programmes_in(conn, &scope, PROGRAMME_CHUNK)? > 0 {}
    atomic(conn, |c| {
        c.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("epg-import:{}:%", id.0)])?;
        c.execute("DELETE FROM epg_source WHERE id = ?1", [id])?;
        Ok(())
    })
}

/// Delete: `epg::delete_programmes_of(EpgSource(id))` in chunks first, then the row (cascades
/// epg channels and mappings).
pub fn delete_epg_source(conn: &mut Connection, id: EpgSourceId) -> Result<()> {
    purge_epg_source(conn, id)?;
    // channels that used this guide fall back to the next source
    crate::store::epg::resolve_mappings(conn)
}

pub fn reorder_epg_sources(conn: &mut Connection, order: &[EpgSourceId]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("UPDATE epg_source SET priority = ?2 WHERE id = ?1")?;
        for (i, id) in order.iter().enumerate() {
            stmt.execute(params![id, i as i64])?;
        }
    }
    crate::store::epg::resolve_mappings(&tx)?;
    tx.commit()?;
    Ok(())
}

pub fn mark_epg_failed(conn: &Connection, id: EpgSourceId, error: &str, now: Timestamp) -> Result<()> {
    let error = crate::redact::redact(error);
    conn.execute("UPDATE epg_source SET last_error = ?2, last_attempt = ?3 WHERE id = ?1", params![id, error.as_ref(), now])?;
    Ok(())
}

/// Enabled EPG sources due for refresh, or whose `max_stop` is < now + 12 h (guide running out).
///
/// The "running out" rule only applies once the last successful refresh is at least an hour
/// old: a provider whose guide always covers less than 12 h must not be re-downloaded on every
/// 15-minute scheduler tick.
pub fn stale_epg_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<EpgSourceId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM epg_source
         WHERE enabled = 1
           AND (last_refresh IS NULL
                OR last_refresh <= ?1 - COALESCE(refresh_h, ?2) * 3600
                OR (max_stop IS NOT NULL AND max_stop < ?1 + 43200 AND last_refresh <= ?1 - 3600))
         ORDER BY priority, id",
    )?;
    let rows = stmt.query_map(params![now, default_h], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Stored provider connection limits (`AccountInfo.max_connections`) per source, for the
/// player's connection budget at startup. Implemented in the skeleton (reached on every launch).
pub fn account_limits(conn: &Connection) -> Result<Vec<(SourceId, Option<u32>)>> {
    let mut stmt = conn.prepare("SELECT id, account FROM source")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, SourceId>(0)?, r.get::<_, Option<String>>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, acct) = row?;
        let max = acct.and_then(|j| serde_json::from_str::<AccountInfo>(&j).ok()).and_then(|a| a.max_connections);
        out.push((id, max));
    }
    Ok(out)
}
