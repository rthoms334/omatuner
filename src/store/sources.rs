//! Source + EPG-source repository. Owner: WP-02.

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    AccountInfo, EpgSource, EpgSourceId, EpgSourceKind, Source, SourceDraft, SourceId, SourceSettings, SourceStatus,
    Timestamp,
};

/// All sources ordered by `sort_index, id`, credentials included (UI must only display them
/// through `redact`).
pub fn list_sources(conn: &Connection) -> Result<Vec<Source>> { todo!("WP-02") }

pub fn get_source(conn: &Connection, id: SourceId) -> Result<Source> { todo!("WP-02") }

/// Insert a validated draft; returns the new id. Also registers its secrets with
/// `redact::register_secret` and, for Xtream / M3U-with-`url-tvg`, does NOT create the provider
/// EPG source (the import job does that once it knows the URL).
pub fn insert_source(conn: &Connection, draft: &SourceDraft, now: Timestamp) -> Result<SourceId> { todo!("WP-02") }

/// Update user-editable fields (name, icon, color, url/path/credentials, settings, enabled).
pub fn update_source(conn: &Connection, src: &Source) -> Result<()> { todo!("WP-02") }

/// Delete a source: first `epg::delete_programmes_of(PlaylistSource(id))` in chunks (FTS rows
/// first), then the row (cascades catalogue, provider EPG sources/channels and user data).
pub fn delete_source(conn: &mut Connection, id: SourceId) -> Result<()> { todo!("WP-02") }

/// Persist `sort_index` = position in `order`.
pub fn reorder_sources(conn: &mut Connection, order: &[SourceId]) -> Result<()> { todo!("WP-02") }

pub fn set_source_settings(conn: &Connection, id: SourceId, settings: &SourceSettings) -> Result<()> { todo!("WP-02") }

/// Record the start of a refresh attempt (`last_attempt`).
pub fn mark_attempt(conn: &Connection, id: SourceId, now: Timestamp) -> Result<()> { todo!("WP-02") }

/// Record a successful refresh: `last_refresh`, counts, etag/last-modified, clears `last_error`.
pub fn mark_refreshed(conn: &Connection, id: SourceId, status: &SourceStatus) -> Result<()> { todo!("WP-02") }

/// Record a failure (message must already be redacted).
pub fn mark_failed(conn: &Connection, id: SourceId, error: &str, now: Timestamp) -> Result<()> { todo!("WP-02") }

pub fn set_account(conn: &Connection, id: SourceId, account: &AccountInfo) -> Result<()> { todo!("WP-02") }

/// Sources whose `last_refresh` is older than their `refresh_h` (or `default_h`), enabled only.
pub fn stale_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<SourceId>> { todo!("WP-02") }

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

/// Ordered by `priority, id`.
pub fn list_epg_sources(conn: &Connection) -> Result<Vec<EpgSource>> { todo!("WP-02") }

pub fn get_epg_source(conn: &Connection, id: EpgSourceId) -> Result<EpgSource> { todo!("WP-02") }

/// Insert a user EPG source (URL or file) at the end of the priority list.
pub fn insert_epg_source(conn: &Connection, name: &str, kind: &EpgSourceKind, offset_min: i32) -> Result<EpgSourceId> {
    todo!("WP-02")
}

/// Upsert the provider EPG source for a playlist source (one per distinct URL); removes
/// provider EPG sources of that playlist whose URL is no longer advertised. Provider sources
/// are inserted with priority 0 (before user sources) unless the user reordered them.
pub fn ensure_provider_epg(conn: &mut Connection, source: SourceId, urls: &[String]) -> Result<Vec<EpgSourceId>> {
    todo!("WP-02")
}

pub fn update_epg_source(conn: &Connection, src: &EpgSource) -> Result<()> { todo!("WP-02") }

/// Delete: `epg::delete_programmes_of(EpgSource(id))` in chunks first, then the row (cascades
/// epg channels and mappings).
pub fn delete_epg_source(conn: &mut Connection, id: EpgSourceId) -> Result<()> { todo!("WP-02") }

pub fn reorder_epg_sources(conn: &mut Connection, order: &[EpgSourceId]) -> Result<()> { todo!("WP-02") }

pub fn mark_epg_failed(conn: &Connection, id: EpgSourceId, error: &str, now: Timestamp) -> Result<()> { todo!("WP-02") }

/// Enabled EPG sources due for refresh, or whose `max_stop` is < now + 12 h (guide running out).
pub fn stale_epg_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<EpgSourceId>> {
    todo!("WP-02")
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
