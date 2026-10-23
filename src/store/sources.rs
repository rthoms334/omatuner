//! Source + EPG-source repository. Owner: WP-02.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::Result;
use crate::model::{
    AccountInfo, CategoryFilter, EpgSource, EpgSourceId, EpgSourceKind, Source, SourceCounts, SourceDraft, SourceId, SourceKind, SourceSettings,
    SourceStatus, Timestamp, XtreamCredentials,
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
///
/// Keeps the stored category rule (`set_category_rule` is its only writer; DESIGN §28.3): a
/// source editor opened before the picker saved must not wipe the rule.
pub fn update_source(conn: &Connection, src: &Source) -> Result<()> {
    let x = src.xtream.as_ref();
    let settings = with_stored_category_filter(conn, src.id, &src.settings)?;
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
            sql::to_json(&settings)?,
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
    /// The server could not answer (network, timeout, 5xx): checked again soon, not in a week.
    #[serde(default)]
    pub transient: bool,
}

/// The last upgrade check of an M3U source (DESIGN §27.3). `None`: never, or it switched since.
pub fn upgrade_attempted_at(conn: &Connection, id: SourceId) -> Result<Option<UpgradeAttempt>> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [upgrade_key(id)], |r| r.get(0)).optional()?;
    Ok(from_json_opt(v))
}

/// Record an upgrade check that left the source M3U (see [`upgrade_attempted_at`]).
pub fn record_upgrade_attempt(conn: &Connection, id: SourceId, attempt: &UpgradeAttempt) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![upgrade_key(id), sql::to_json(attempt)?],
    )?;
    Ok(())
}

/// `meta` key marking a source upgraded while no window has said so yet.
fn unseen_upgrade_key(id: SourceId) -> String {
    format!("xtream-upgraded-unseen:{}", id.0)
}

/// This M3U source switched to its Xtream login (written with the switch): until a window
/// toasts it ([`forget_unseen_upgrade`]) — the refresh was headless, or Tuner quit or the job
/// was cancelled before its end was shown — the next window says so ([`take_unseen_upgrades`],
/// DESIGN §27.3).
pub fn note_unseen_upgrade(conn: &Connection, id: SourceId) -> Result<()> {
    conn.execute("INSERT INTO meta(key, value) VALUES (?1, '1') ON CONFLICT(key) DO NOTHING", [unseen_upgrade_key(id)])?;
    Ok(())
}

/// A window has said this source was upgraded.
pub fn forget_unseen_upgrade(conn: &Connection, id: SourceId) -> Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [unseen_upgrade_key(id)])?;
    Ok(())
}

/// The names of the sources whose upgrade no window has said yet (still there and still
/// Xtream), forgetting them: the window that asks shows them.
pub fn take_unseen_upgrades(conn: &Connection) -> Result<Vec<String>> {
    atomic(conn, |c| {
        let mut stmt = c.prepare(
            "SELECT s.name FROM meta m JOIN source s ON m.key = 'xtream-upgraded-unseen:' || s.id WHERE s.kind = 'xtream' ORDER BY s.sort_index, s.id",
        )?;
        let names = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        c.execute("DELETE FROM meta WHERE key LIKE 'xtream-upgraded-unseen:%'", [])?;
        Ok(names)
    })
}

/// `meta` key of the Xtream logins found in an M3U source's entries (redaction only).
fn playlist_logins_key(id: SourceId) -> String {
    format!("playlist-logins:{}", id.0)
}

/// Store the Xtream logins (user, password) the last import found in the stream URLs of an M3U
/// source, replacing the previous ones (none: forgotten). [`register_secrets`] registers them
/// at startup, so a playlist that stays M3U has its short-form stream URLs
/// (`host/<prefix>/<user>/<pass>/<id>`, which no URL pattern can tell from a CDN path) masked
/// before its next import. They live where the URLs that carry them already do.
pub fn set_playlist_logins(conn: &Connection, id: SourceId, logins: &[(String, String)]) -> Result<()> {
    if logins.is_empty() {
        conn.execute("DELETE FROM meta WHERE key = ?1", [playlist_logins_key(id)])?;
    } else {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![playlist_logins_key(id), sql::to_json(&logins)?],
        )?;
    }
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
    crate::store::watchlist::before_source_delete(conn, id)?;
    crate::store::new_episodes::before_source_delete(conn, id)?;
    conn.execute("UPDATE download SET orphaned_since=COALESCE(orphaned_since,?2) WHERE source_id=?1", params![id, crate::model::now()])?;
    let scope = crate::store::epg::ProgrammeScope::PlaylistSource(id);
    while crate::store::epg::delete_programmes_in(conn, &scope, PROGRAMME_CHUNK)? > 0 {}
    // big catalogues go in chunks so user writes interleave; the final delete cascades the rest
    delete_chunked(conn, "episode", "source_id = ?1", id.0)?;
    for t in ["channel", "movie", "series"] {
        delete_chunked(conn, t, "source_id = ?1", id.0)?;
    }
    atomic(conn, |c| {
        c.execute(
            "DELETE FROM meta WHERE key LIKE ?1 OR key IN (?2, ?3, ?4)",
            params![format!("import:{}:%", id.0), upgrade_key(id), playlist_logins_key(id), unseen_upgrade_key(id)],
        )?;
        c.execute("DELETE FROM source WHERE id = ?1", [id])?;
        // a join with a copy of this playlist is left with one copy
        crate::store::titles::prune_joins(c)?;
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

/// Replace a source's settings, except the category rule (kept as stored, see
/// [`update_source`]).
pub fn set_source_settings(conn: &Connection, id: SourceId, settings: &SourceSettings) -> Result<()> {
    let settings = with_stored_category_filter(conn, id, settings)?;
    let n = conn.execute("UPDATE source SET settings = ?2 WHERE id = ?1", params![id, sql::to_json(&settings)?])?;
    if n == 0 {
        return Err(sql::not_found("source", id));
    }
    Ok(())
}

/// `settings` with the category rule and the Guide fixes settings currently stored for `id`
/// (`category_filter`, `own_category_filter`, `guide`; none for an unknown id: the caller's
/// update then reports it). Those have their own writers (`set_guide_rules`,
/// `store::guide_fix::apply`), so a stale copy of the settings never reverts them.
fn with_stored_category_filter(conn: &Connection, id: SourceId, settings: &SourceSettings) -> Result<SourceSettings> {
    let stored: Option<String> = conn.prepare_cached("SELECT settings FROM source WHERE id = ?1")?.query_row([id], |r| r.get(0)).optional()?;
    let stored = from_json_opt::<SourceSettings>(stored).unwrap_or_default();
    Ok(SourceSettings {
        category_filter: stored.category_filter,
        own_category_filter: stored.own_category_filter,
        guide: stored.guide,
        ..settings.clone()
    })
}

/// `meta` key of the category rule every playlist shares (DESIGN §28.3; JSON, absent = none).
const SHARED_FILTER_KEY: &str = "category-filter";

/// Which category rule a playlist follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CategoryRule {
    /// The rule every playlist shares ([`shared_category_filter`]).
    Shared,
    /// "Use a different rule for this playlist" (`None` = show everything of it).
    Own(Option<CategoryFilter>),
}

/// The category rule shared by every playlist without its own (DESIGN §28.3); `None` = none.
pub fn shared_category_filter(conn: &Connection) -> Result<Option<CategoryFilter>> {
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [SHARED_FILTER_KEY], |r| r.get(0)).optional()?;
    Ok(from_json_opt::<Option<CategoryFilter>>(v).flatten())
}

/// Set or clear the shared category rule. Callers notify `DataScope::Sources`.
pub fn set_shared_category_filter(conn: &Connection, filter: Option<&CategoryFilter>) -> Result<()> {
    match filter {
        Some(f) => conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SHARED_FILTER_KEY, sql::to_json(f)?],
        )?,
        None => conn.execute("DELETE FROM meta WHERE key = ?1", [SHARED_FILTER_KEY])?,
    };
    Ok(())
}

/// Which rule a playlist follows (DESIGN §28.3): read-modify-write of the settings JSON inside
/// the writer, so the other settings are untouched. Callers notify `DataScope::Sources` (the
/// library is rebuilt with the new rule).
pub fn set_category_rule(conn: &Connection, id: SourceId, rule: &CategoryRule) -> Result<()> {
    let settings: String = or_not_found(conn.query_row("SELECT settings FROM source WHERE id = ?1", [id], |r| r.get(0)), "source", id)?;
    let mut settings: SourceSettings = from_json_opt(Some(settings)).unwrap_or_default();
    (settings.own_category_filter, settings.category_filter) = match rule {
        CategoryRule::Shared => (Some(false), None),
        CategoryRule::Own(f) => (Some(true), f.clone()),
    };
    conn.execute("UPDATE source SET settings = ?2 WHERE id = ?1", params![id, sql::to_json(&settings)?])?;
    Ok(())
}

/// A playlist's Guide fixes rules and catch-up offset (DESIGN §31): the only writer of
/// `SourceSettings::guide`. Callers re-derive the rule rows afterwards
/// (`store::guide_fix::derive_rule_rows`; `store::guide_fix::apply` does it).
pub fn set_guide_rules(conn: &Connection, id: SourceId, rules: &crate::model::GuideRules) -> Result<()> {
    let settings: String = or_not_found(conn.query_row("SELECT settings FROM source WHERE id = ?1", [id], |r| r.get(0)), "source", id)?;
    let mut settings: SourceSettings = from_json_opt(Some(settings)).unwrap_or_default();
    settings.guide = *rules;
    conn.execute("UPDATE source SET settings = ?2 WHERE id = ?1", params![id, sql::to_json(&settings)?])?;
    Ok(())
}

/// Give a playlist its own category rule (`None` = show everything of it):
/// [`set_category_rule`] with [`CategoryRule::Own`].
pub fn set_category_filter(conn: &Connection, id: SourceId, filter: Option<&CategoryFilter>) -> Result<()> {
    set_category_rule(conn, id, &CategoryRule::Own(filter.cloned()))
}

/// The rule that applies to a playlist now (its own, else the shared one).
pub fn effective_category_filter(conn: &Connection, id: SourceId) -> Result<Option<CategoryFilter>> {
    let settings = get_source(conn, id)?.settings;
    if settings.has_own_category_filter() { Ok(settings.category_filter) } else { shared_category_filter(conn) }
}

/// Settings ▸ Hidden ▸ Enter on a hidden tag: add it to the rule that hides it for `id` — the
/// playlist's own, else the shared one (so every playlist shows it). Untagged (`None`) turns
/// `keep_untagged` on. Returns `true` when the shared rule changed. Nothing to do without a rule.
pub fn keep_tag(conn: &Connection, id: SourceId, tag: Option<crate::model::CategoryTag>) -> Result<bool> {
    let settings = get_source(conn, id)?.settings;
    let own = settings.has_own_category_filter();
    let Some(mut rule) = (if own { settings.category_filter } else { shared_category_filter(conn)? }) else { return Ok(false) };
    match tag {
        Some(t) => {
            rule.keep.insert(t);
        }
        None => rule.keep_untagged = true,
    }
    if own {
        set_category_rule(conn, id, &CategoryRule::Own(Some(rule)))?;
    } else {
        set_shared_category_filter(conn, Some(&rule))?;
    }
    Ok(!own)
}

/// Rules saved before the rule was shared (DESIGN §28.3): playlists whose settings carry a
/// `category_filter` but no `own_category_filter`. Without a shared rule, the first such rule
/// (playlist order) becomes it (`known` = what the playlists with that same choice knew) — so a
/// rule saved on one playlist now covers the others (field report: English chosen on "TV",
/// nothing on "TV 2"). Each such playlist then follows the shared rule when its choice is the
/// same, else keeps its rule as its own. Their `category_filter` stays stored (ignored while
/// they follow the shared rule), so an older build still filters. Run when the database opens
/// and after a restore; `true` = something changed. Idempotent.
pub fn adopt_legacy_category_filters(conn: &Connection) -> Result<bool> {
    let legacy: Vec<(SourceId, SourceSettings)> = list_sources(conn)?
        .into_iter()
        .filter(|s| s.settings.own_category_filter.is_none() && s.settings.category_filter.is_some())
        .map(|s| (s.id, s.settings))
        .collect();
    if legacy.is_empty() {
        return Ok(false);
    }
    atomic(conn, |c| {
        let shared = match shared_category_filter(c)? {
            Some(s) => s,
            None => {
                let mut first = legacy[0].1.category_filter.clone().unwrap_or_default();
                for (_, st) in &legacy[1..] {
                    if let Some(r) = st.category_filter.as_ref().filter(|r| r.same_choice(&first)) {
                        first.known.extend(r.known.iter().cloned());
                    }
                }
                // playlists without a rule until now: the rule covers them from here on, and
                // their tags present now are known (not badged "new" in the picker)
                let newly: Vec<SourceId> = list_sources(c)?.iter().map(|s| s.id).filter(|id| legacy.iter().all(|(l, _)| l != id)).collect();
                if !newly.is_empty() {
                    first.known.extend(crate::store::library::category_summary_of(c, &newly, false)?.present());
                    // the next window says so once (`take_adopted_category_rule`)
                    c.execute(
                        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        params![ADOPTED_KEY, get_source(c, legacy[0].0)?.name],
                    )?;
                }
                tracing::info!("category filter: the rule of playlist {} now applies to every playlist", legacy[0].0.0);
                set_shared_category_filter(c, Some(&first))?;
                first
            }
        };
        for (id, st) in &legacy {
            let own = !st.category_filter.as_ref().is_some_and(|r| r.same_choice(&shared));
            let settings = SourceSettings { own_category_filter: Some(own), ..st.clone() };
            c.execute("UPDATE source SET settings = ?2 WHERE id = ?1", params![id, sql::to_json(&settings)?])?;
        }
        Ok(true)
    })
}

/// `meta` key of a rule [`adopt_legacy_category_filters`] made shared that no window has said
/// yet (value: the name of the playlist it came from).
const ADOPTED_KEY: &str = "category-filter-adopted-unseen";

/// The toast for a playlist's old rule that now covers every playlist (once): "Your choice of
/// countries from TV now applies to all your playlists…". `None` when there is nothing to say.
pub fn take_adopted_category_rule(conn: &Connection) -> Result<Option<String>> {
    let name: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [ADOPTED_KEY], |r| r.get(0)).optional()?;
    let Some(name) = name else { return Ok(None) };
    conn.execute("DELETE FROM meta WHERE key = ?1", [ADOPTED_KEY])?;
    let Some(rule) = shared_category_filter(conn)? else { return Ok(None) };
    let from = if name.is_empty() { String::new() } else { format!(" from {name}") };
    let titles = if crate::sources::category_tags::hides_other_languages(&rule) { " · titles in other languages hidden" } else { "" };
    Ok(Some(format!("Your choice of countries{from} now applies to all your playlists{titles} · Settings ▸ Hidden items")))
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
        "UPDATE source SET last_refresh = ?2, last_attempt = COALESCE(?3, last_attempt), last_error = NULL, failing_since = NULL, etag = ?4,
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

/// Record a failure (message must already be redacted). The first failure of a streak sets
/// `failing_since`, which the retry back-off counts from ([`retry_due`]).
pub fn mark_failed(conn: &Connection, id: SourceId, error: &str, now: Timestamp) -> Result<()> {
    // defence in depth: the message is shown in Settings and logged
    let error = crate::redact::redact(error);
    conn.execute(
        "UPDATE source SET last_error = ?2, last_attempt = ?3, failing_since = COALESCE(failing_since, ?3) WHERE id = ?1",
        params![id, error.as_ref(), now],
    )?;
    Ok(())
}

pub fn set_account(conn: &Connection, id: SourceId, account: &AccountInfo) -> Result<()> {
    conn.execute("UPDATE source SET account = ?2 WHERE id = ?1", params![id, sql::to_json(account)?])?;
    Ok(())
}

/// Shortest wait before the scheduler retries a failed refresh (just under its 15-min tick).
const RETRY_MIN_S: i64 = 10 * 60;
/// Longest wait between scheduled retries of a failing refresh, whatever its interval.
const RETRY_MAX_S: i64 = 86_400;

/// Back-off of a refresh whose last attempt failed: the scheduler waits as long after the last
/// attempt as the refresh has been failing (`failing_since` → `last_attempt`), so the first
/// retry comes after [`RETRY_MIN_S`] and the gaps then double — at most the refresh interval
/// (and [`RETRY_MAX_S`]). How overdue the refresh was when it first failed doesn't matter. A
/// row without `failing_since` (failing since before it was stored) waits [`RETRY_MIN_S`]; its
/// next failure starts the streak. A manual Refresh (`force`) never goes through here.
fn retry_wait(last_attempt: Timestamp, failing_since: Option<Timestamp>, interval_s: i64) -> i64 {
    let cap = interval_s.clamp(RETRY_MIN_S, RETRY_MAX_S);
    failing_since.map_or(RETRY_MIN_S, |f| (last_attempt - f).clamp(RETRY_MIN_S, cap))
}

/// When the scheduler next retries a refresh whose last attempt (at `last_attempt`) failed
/// (what the status bar quotes: "retrying in 10 min"). The scheduler only ticks every 15 min,
/// so the real retry can come a little later.
pub fn retry_at(last_attempt: Timestamp, failing_since: Option<Timestamp>, interval_s: i64) -> Timestamp {
    last_attempt.saturating_add(retry_wait(last_attempt, failing_since, interval_s))
}

fn retry_due(now: Timestamp, last_attempt: Timestamp, failing_since: Option<Timestamp>, interval_s: i64) -> bool {
    now >= retry_at(last_attempt, failing_since, interval_s)
}

/// Sources whose `last_refresh` is older than their `refresh_h` (or `default_h`), enabled only;
/// one whose last refresh failed only once its back-off is over ([`retry_due`]).
pub fn stale_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<SourceId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, settings, last_refresh, last_attempt, last_error IS NOT NULL, failing_since FROM source
         WHERE enabled = 1 ORDER BY sort_index, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, SourceId>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<Timestamp>>(2)?,
            r.get::<_, Option<Timestamp>>(3)?,
            r.get::<_, bool>(4)?,
            r.get::<_, Option<Timestamp>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, settings, last, attempt, failed, since) = row?;
        let h = from_json_opt::<SourceSettings>(settings).and_then(|s| s.refresh_h).unwrap_or(default_h);
        let interval = i64::from(h) * 3600;
        if last.is_some_and(|t| now < t.saturating_add(interval)) {
            continue;
        }
        if let Some(at) = attempt.filter(|_| failed)
            && !retry_due(now, at, since, interval)
        {
            continue;
        }
        out.push(id);
    }
    Ok(out)
}

/// Register every stored credential/URL secret with the redactor. Called once at startup
/// (implemented in the skeleton: it runs on every launch). Registers Xtream usernames and
/// passwords, the logins found in M3U entries ([`set_playlist_logins`]) and every
/// query-parameter value of M3U / XMLTV URLs.
pub fn register_secrets(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key LIKE 'playlist-logins:%'")?;
    for v in stmt.query_map([], |r| r.get::<_, Option<String>>(0))? {
        for (user, pass) in from_json_opt::<Vec<(String, String)>>(v?).unwrap_or_default() {
            crate::redact::register_secret(&user);
            crate::redact::register_secret(&pass);
        }
    }
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

/// Record a failed guide refresh (the first of a streak sets `failing_since`, as [`mark_failed`]).
pub fn mark_epg_failed(conn: &Connection, id: EpgSourceId, error: &str, now: Timestamp) -> Result<()> {
    let error = crate::redact::redact(error);
    conn.execute(
        "UPDATE epg_source SET last_error = ?2, last_attempt = ?3, failing_since = COALESCE(failing_since, ?3) WHERE id = ?1",
        params![id, error.as_ref(), now],
    )?;
    Ok(())
}

/// Enabled EPG sources due for refresh, or whose `max_stop` is < now + 12 h (guide running out);
/// one whose last refresh failed only once its back-off is over ([`retry_due`]).
///
/// The "running out" rule only applies once the last successful refresh is at least an hour
/// old: a provider whose guide always covers less than 12 h must not be re-downloaded on every
/// 15-minute scheduler tick.
pub fn stale_epg_sources(conn: &Connection, now: Timestamp, default_h: u32) -> Result<Vec<EpgSourceId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, COALESCE(refresh_h, ?2), last_attempt, last_error IS NOT NULL, failing_since FROM epg_source
         WHERE enabled = 1
           AND (last_refresh IS NULL
                OR last_refresh <= ?1 - COALESCE(refresh_h, ?2) * 3600
                OR (max_stop IS NOT NULL AND max_stop < ?1 + 43200 AND last_refresh <= ?1 - 3600))
         ORDER BY priority, id",
    )?;
    let rows = stmt.query_map(params![now, default_h], |r| {
        Ok((
            r.get::<_, EpgSourceId>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<Timestamp>>(2)?,
            r.get::<_, bool>(3)?,
            r.get::<_, Option<Timestamp>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, h, attempt, failed, since) = row?;
        let interval = h.saturating_mul(3600);
        if let Some(at) = attempt.filter(|_| failed)
            && !retry_due(now, at, since, interval)
        {
            continue;
        }
        out.push(id);
    }
    Ok(out)
}

/// A guide worth a word in the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideNote {
    pub name: String,
    pub last_refresh: Option<Timestamp>,
    /// Its latest attempt failed (after the last success): when the scheduler retries it.
    pub retry_at: Option<Timestamp>,
}

/// What the status bar and the Guide screen say about the enabled guides beyond "updated N ago"
/// (`newest`): a failed update, and a guide much older than the newest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuideHealth {
    /// Newest `last_refresh` of the enabled guides.
    pub newest: Option<Timestamp>,
    /// More than one guide is enabled (a note then names its guide).
    pub several: bool,
    /// The guide whose failed update is retried first.
    pub failing: Option<GuideNote>,
    /// The oldest guide that is stale (`stale_s` old) and `stale_s` older than the newest.
    pub old: Option<GuideNote>,
}

/// [`GuideHealth`] of the enabled guides at `now`; `default_h` is the default refresh interval
/// (hours) of a guide without its own — `general.epg_refresh_h`, 0 = manual.
pub fn guide_health(conn: &Connection, now: Timestamp, default_h: u32, stale_s: i64) -> Result<GuideHealth> {
    let default_h = if default_h == 0 { u32::MAX } else { default_h };
    let mut stmt = conn.prepare_cached(
        "SELECT name, COALESCE(refresh_h, ?1), last_refresh, last_attempt, last_error IS NOT NULL, failing_since FROM epg_source
         WHERE enabled = 1 ORDER BY priority, id",
    )?;
    let rows = stmt.query_map([default_h], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<Timestamp>>(2)?,
            r.get::<_, Option<Timestamp>>(3)?,
            r.get::<_, bool>(4)?,
            r.get::<_, Option<Timestamp>>(5)?,
        ))
    })?;
    let mut notes = Vec::new();
    for row in rows {
        let (name, h, last_refresh, attempt, failed, since) = row?;
        // the latest attempt failed after the last success (a guide that never imported counts)
        let retry_at = attempt.filter(|at| failed && last_refresh.is_none_or(|ok| *at >= ok)).map(|at| retry_at(at, since, h.saturating_mul(3600)));
        notes.push(GuideNote { name, last_refresh, retry_at });
    }
    let newest = notes.iter().filter_map(|n| n.last_refresh).max();
    let failing = notes.iter().filter(|n| n.retry_at.is_some()).min_by_key(|n| n.retry_at).cloned();
    let old = notes
        .iter()
        .filter(|n| n.last_refresh.zip(newest).is_some_and(|(t, new)| new - t >= stale_s && now - t >= stale_s))
        .min_by_key(|n| n.last_refresh)
        .cloned();
    Ok(GuideHealth { newest, several: notes.len() > 1, failing, old })
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

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;
    const T0: Timestamp = 1_000_000_000;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        crate::store::migrations::migrate(&mut c).unwrap();
        c
    }

    /// Playlist 1 and guide 1 (name `g1`), both refreshed every 12 h, last refreshed at `ok`.
    fn rows(c: &Connection, ok: Timestamp) {
        c.execute("INSERT INTO source(id, name, kind, created_at, last_refresh) VALUES (1, 'p', 'm3u-url', 0, ?1)", [ok]).unwrap();
        c.execute("INSERT INTO epg_source(id, name, kind, last_refresh, refresh_h) VALUES (1, 'g1', 'url', ?1, 12)", [ok]).unwrap();
    }

    fn due(c: &Connection, now: Timestamp) -> (Vec<SourceId>, Vec<EpgSourceId>) {
        (stale_sources(c, now, 12).unwrap(), stale_epg_sources(c, now, 12).unwrap())
    }

    fn failing_since(c: &Connection) -> (Option<Timestamp>, Option<Timestamp>) {
        let q = |t: &str| c.query_row(&format!("SELECT failing_since FROM {t} WHERE id = 1"), [], |r| r.get(0)).unwrap();
        (q("source"), q("epg_source"))
    }

    fn fail(c: &Connection, at: Timestamp) {
        mark_failed(c, SourceId(1), "dns error", at).unwrap();
        mark_epg_failed(c, EpgSourceId(1), "dns error", at).unwrap();
    }

    #[test]
    fn a_long_overdue_refresh_is_retried_soon_after_its_first_failure() {
        let c = db();
        // last refreshed 19 h ago; the app was closed, so it is 7 h overdue at the failed launch refresh
        let now = T0 + 7 * H;
        rows(&c, T0 - 12 * H);
        assert_eq!(due(&c, now), (vec![SourceId(1)], vec![EpgSourceId(1)]));
        fail(&c, now);
        assert_eq!(failing_since(&c), (Some(now), Some(now)));
        assert_eq!(due(&c, now + RETRY_MIN_S - 1), (vec![], vec![]));
        assert_eq!(due(&c, now + RETRY_MIN_S), (vec![SourceId(1)], vec![EpgSourceId(1)]));
    }

    #[test]
    fn successive_failures_back_off_up_to_the_interval() {
        let c = db();
        rows(&c, T0 - 12 * H);
        let mut at = T0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            fail(&c, at);
            let next = (T0..T0 + 48 * H).step_by(60).find(|t| *t > at && !due(&c, *t).1.is_empty()).unwrap();
            assert_eq!(due(&c, next).0, vec![SourceId(1)], "playlists and guides back off alike");
            waits.push(next - at);
            at = next;
        }
        let m = RETRY_MIN_S;
        // 10, 10, 20, 40, 80, 160 min, then the 12 h interval caps it
        assert_eq!(waits, [m, m, 2 * m, 4 * m, 8 * m, 16 * m, 32 * m, 64 * m]);
        assert_eq!(retry_at(at, Some(at - 100 * H), 12 * H), at + 12 * H);
        // the failing_since of the streak never moved
        assert_eq!(failing_since(&c), (Some(T0), Some(T0)));
    }

    #[test]
    fn rows_from_before_the_column_wait_the_minimum() {
        assert_eq!(retry_at(T0, None, 12 * H), T0 + RETRY_MIN_S);
    }

    #[test]
    fn success_clears_the_streak() {
        let c = db();
        rows(&c, T0 - 12 * H);
        fail(&c, T0);
        let status = crate::model::SourceStatus { last_refresh: Some(T0 + 1), last_attempt: Some(T0 + 1), ..Default::default() };
        mark_refreshed(&c, SourceId(1), &status).unwrap();
        assert_eq!(failing_since(&c), (None, Some(T0)));
        // a later failure starts a new streak
        fail(&c, T0 + 20 * H);
        assert_eq!(failing_since(&c), (Some(T0 + 20 * H), Some(T0)));
    }

    #[test]
    fn health_reports_a_failed_update_and_an_old_guide() {
        let c = db();
        let now = T0 + 30 * H;
        rows(&c, T0);
        let health = |c: &Connection| guide_health(c, now, 12, 2 * 86_400).unwrap();
        assert_eq!(health(&c), GuideHealth { newest: Some(T0), several: false, failing: None, old: None });
        fail(&c, now);
        let h = health(&c);
        assert_eq!(h.failing, Some(GuideNote { name: "g1".into(), last_refresh: Some(T0), retry_at: Some(now + RETRY_MIN_S) }));
        // a second guide, failing for days
        c.execute(
            "INSERT INTO epg_source(id, name, kind, last_refresh, last_attempt, last_error) VALUES (2, 'g2', 'url', ?1, ?2, 'refused')",
            params![T0 - 5 * 86_400, now - H],
        )
        .unwrap();
        let h = health(&c);
        assert!(h.several);
        assert_eq!(h.old.as_ref().map(|n| (n.name.as_str(), n.retry_at)), Some(("g2", Some(now - H + RETRY_MIN_S))));
        // a failure older than the last success is not shown
        c.execute("UPDATE epg_source SET last_refresh = ?1 WHERE id = 1", [now + 1]).unwrap();
        assert_eq!(health(&c).failing.map(|n| n.name), Some("g2".into()));
    }
}
