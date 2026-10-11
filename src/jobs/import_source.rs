//! Playlist import pipelines (M3U + Xtream). Owner: WP-06. DESIGN §9.1–9.2.
//!
//! M3U:   download (conditional) → `open_decompressed` → `m3u::parse` in spawn_blocking →
//!        convert entries (smart names, identity keys, groups) → batches of 2 000 through
//!        `store::import::write_channels` → `finish_catalogue` → provider EPG sources from
//!        `url-tvg` → `RebuildLibrary`.
//! Xtream: authenticate (store AccountInfo; abort on AuthRejected/AccountUnavailable without
//!        touching the catalogue) → categories → live streams → vod → series (each kind
//!        completes independently; a failed kind is not swept) → provider EPG source
//!        (`xmltv.php`) → `RebuildLibrary`.
//!
//! Details fixed by the implementation:
//! * The M3U parser runs on a blocking thread and hands `(new groups, channels)` batches of
//!   2 000 to the async writer loop through a bounded channel (capacity 4): memory stays flat
//!   and the parser waits when the writer falls behind. Malformed entries are skipped (counted
//!   in the log); a read error aborts the import. A playlist without a single entry is an error
//!   (`BadPayload`), never an emptied channel list.
//! * The downloaded playlist (it may embed credentials) is deleted after the import, whatever
//!   the outcome; the ETag / Last-Modified are stored with the successful refresh only.
//! * Xtream kinds: a kind fails alone on a bad/empty reply (`BadPayload`), a network error, a
//!   parse error or an HTTP error (403 → "access denied"); that kind keeps its old rows (not
//!   swept) and the refresh records a redacted "Movies: …" note in `last_error` while the other
//!   kinds are imported. A rejected login or unusable account (also when a list action answers
//!   with the login object or 401) aborts the whole refresh before any sweep. If every kind
//!   fails the refresh fails.
//! * Live channels are finished (swept + re-attached) before VOD starts and a `RebuildLibrary`
//!   is submitted right away, so Live TV is usable while movies and series still import.
//! * Xtream group keys come from category NAMES (`identity::group_key`); streams whose category
//!   is unknown go to an "Uncategorized" group.
//! * One import per source at a time: `AddSource` runs `refresh_source` under its own job key, so
//!   the manager's coalescing alone cannot keep it apart from a `RefreshSource` of the same row
//!   (or from a second onboarding attempt). A per-source async lock serializes them, and each run
//!   downloads to its own file (`<source>-<job>.m3u`).
//!
//! Phase 5 (DESIGN §27.3–27.5):
//! * M3U imports classify entries (`m3u_vod::classify`): Xtream `/movie/` and `/series/` URLs
//!   become movies and series episodes (keeping the key the entry has as a channel); episodes
//!   are collected during the parse and written as whole series after it. Channels, movies and
//!   series all complete (and are swept) at the finish.
//! * An M3U source not kept as M3U checks for its Xtream login when due (forced, never
//!   checked, or the last check is a week old): from a `get.php` URL before any download, else
//!   by scanning the downloaded / local playlist. A working login runs the Xtream import, whose
//!   first finish switches the source (`Upgrade`); a failed login, or an Xtream import failing
//!   before that finish, records the check and imports the same playlist in the same job
//!   (checked again in a week, or in an hour when the server couldn't answer). After that
//!   finish the upgrade stands: a later fatal kind failure or a cancel still reports it, and
//!   the next Xtream refresh of the source applies the user-data moves it left undone.
//!   A playlist without a login records the check too, so it is scanned weekly, not on every
//!   refresh; while it has no failed login on record, the weekly download stays conditional.
//! * An upgraded source the user keeps as M3U goes back to its playlist with a forced import
//!   whose finish switches it and moves Xtream-keyed user data to the playlist's rows.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobKind, JobPhase, Progress, blocking, check, notify, read, write};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{
    AccountInfo, CatchupKind, CatchupSpec, EpgSourceId, EpgSourceKind, GroupKey, HttpHeaders, ItemKey, ItemKind, Source, SourceCounts, SourceId,
    SourceKind, SourceStatus, StreamLocator, Timestamp, Unsupported, XtreamCredentials, XtreamStreamKind,
};
use crate::msg::DataScope;
use crate::sources::PayloadKind;
use crate::sources::http::{DownloadOutcome, DownloadRequest, open_decompressed};
use crate::sources::identity::{self, KeyAllocator, group_key, normalize_group, normalize_name};
use crate::sources::m3u::{self, M3uEntry, M3uError};
use crate::sources::m3u_vod::{self, EntryClass, EpisodeMarker};
use crate::sources::smart_names::{clean_channel_name, clean_vod_name, group_display};
use crate::sources::xtream::XtreamClient;
use crate::sources::xtream::detect::{PlaylistScan, parse_stream_url};
use crate::sources::xtream::types::{XtCategory, XtLiveStream, XtSeries, XtVodStream};
use crate::sources::xtream::urls;
use crate::store::import::{
    self, CatalogueImport, FinishOptions, ImportChannel, ImportEpisode, ImportGroup, ImportMovie, ImportSeries, ImportSeriesEpisodes, ImportSummary,
    KeyMove, KindCounts, NameQuality,
};
use crate::store::sources::UpgradeAttempt;

/// Rows per writer transaction (DESIGN §5.3).
const BATCH: usize = 2_000;
/// An M3U source checks again for a working Xtream login after this long (DESIGN §27.3).
const UPGRADE_RETRY_S: i64 = 7 * 86_400;
/// ... or after this long when the server couldn't answer (offline, timeout, 5xx).
const UPGRADE_RETRY_TRANSIENT_S: i64 = 3_600;
/// Batches in flight between the parser thread and the writer loop.
const CHANNEL_CAP: usize = 4;
/// Entries between two cancellation checks of the parser.
const CANCEL_EVERY: u64 = 1_000;

/// Outcome of a refresh: summary + whether the server said 304.
pub struct RefreshResult {
    pub summary: ImportSummary,
    pub not_modified: bool,
    /// Provider guide URLs discovered (M3U `url-tvg`, Xtream `xmltv.php`).
    pub epg_urls: Vec<String>,
    /// The M3U source was switched to its Xtream login by this refresh (DESIGN §27.3).
    pub upgraded: bool,
}

pub async fn refresh_source(
    ctx: &JobContext,
    source: SourceId,
    force: bool,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<RefreshResult> {
    let lock = source_lock(ctx, source);
    let _running = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        g = lock.lock_owned() => g,
    };
    progress.mark_inputs_read();
    let src = read(ctx, move |c| crate::store::sources::get_source(c, source)).await?;
    progress.set_subject(&src.name);
    let started = crate::model::now();
    write(ctx, move |c| crate::store::sources::mark_attempt(c, source, started)).await?;
    let cfg = crate::config::current(&ctx.config);
    let result = match (src.kind, src.m3u_origin()) {
        (SourceKind::M3uUrl | SourceKind::M3uFile, _) => refresh_playlist(ctx, &src, force, &cfg, progress, cancel).await,
        (SourceKind::Xtream, Some(origin)) if src.settings.keep_m3u => switch_back(ctx, &src, origin, &cfg, progress, cancel).await,
        (SourceKind::Xtream, _) => refresh_xtream(ctx, &src, &cfg, progress, cancel, None).await,
    };
    match &result {
        Ok(_) | Err(Error::Cancelled) => {}
        Err(e) => {
            let msg = e.to_string();
            let now = crate::model::now();
            if let Err(e2) = write(ctx, move |c| crate::store::sources::mark_failed(c, source, &msg, now)).await {
                tracing::warn!("recording the refresh failure: {e2}");
            }
        }
    }
    result
}

/// The import lock of one source (keyed by database file too: tests run several stores in one
/// process). Idle entries are dropped on the next lookup.
fn source_lock(ctx: &JobContext, id: SourceId) -> Arc<tokio::sync::Mutex<()>> {
    type Locks = HashMap<(PathBuf, SourceId), Arc<tokio::sync::Mutex<()>>>;
    static LOCKS: LazyLock<Mutex<Locks>> = LazyLock::new(Mutex::default);
    let mut locks = super::lock(&LOCKS);
    locks.retain(|_, l| Arc::strong_count(l) > 1);
    locks.entry((ctx.store.path().to_path_buf(), id)).or_default().clone()
}

/// Smart-name cleanup for this source (its own setting, else the global one).
fn smart_names(src: &Source, cfg: &Config) -> bool {
    src.settings.smart_names.unwrap_or(cfg.general.smart_names)
}

/// HTTP headers for API calls and downloads of this source.
fn source_headers(src: &Source, cfg: &Config) -> HttpHeaders {
    let ua = src.settings.user_agent.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| cfg.player.user_agent.clone());
    HttpHeaders { user_agent: Some(ua), ..Default::default() }
}

/// Successful refresh bookkeeping: `last_refresh`, counts, validators (clears `last_error`).
async fn mark_refreshed(
    ctx: &JobContext,
    src: &Source,
    counts: SourceCounts,
    etag: Option<String>,
    last_modified: Option<String>,
    now: Timestamp,
) -> Result<()> {
    let status = SourceStatus {
        last_refresh: Some(now),
        last_attempt: Some(now),
        last_error: None,
        counts,
        account: None, // stored by `set_account` (COALESCE keeps it)
        etag,
        last_modified,
    };
    let id = src.id;
    write(ctx, move |c| crate::store::sources::mark_refreshed(c, id, &status)).await
}

fn counts_of(s: &ImportSummary) -> SourceCounts {
    SourceCounts { channels: s.channels.total, movies: s.movies.total, series: s.series.total, groups: s.groups.total }
}

/// Upsert the provider guide sources of `src` (or drop them when the user turned provider EPG
/// off), then submit refreshes for the stale ones. Returns their ids.
async fn provider_epg(ctx: &JobContext, src: &Source, urls: Vec<String>, cfg: &Config) -> Result<Vec<EpgSourceId>> {
    let urls = if src.settings.use_provider_epg.unwrap_or(true) { urls } else { Vec::new() };
    let id = src.id;
    let ids = write(ctx, move |c| crate::store::sources::ensure_provider_epg(c, id, &urls)).await?;
    submit_stale_epg(ctx, ids.clone(), cfg).await?;
    Ok(ids)
}

/// Submit `RefreshEpg` for those of `ids` that are due (never imported, older than
/// `epg_refresh_h`, or running out).
async fn submit_stale_epg(ctx: &JobContext, ids: Vec<EpgSourceId>, cfg: &Config) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let h = match cfg.general.epg_refresh_h {
        0 => u32::MAX, // manual only: just the never-imported ones (and guides running out)
        h => h,
    };
    let now = crate::model::now();
    let stale = read(ctx, move |c| crate::store::sources::stale_epg_sources(c, now, h)).await?;
    for id in ids.into_iter().filter(|id| stale.contains(id)) {
        (ctx.submit)(JobKind::RefreshEpg { epg_source: id, force: false });
    }
    Ok(())
}

/// Provider guide sources of a playlist source (304 path: no playlist header to read).
async fn provider_epg_ids(ctx: &JobContext, source: SourceId) -> Result<Vec<EpgSourceId>> {
    read(ctx, move |c| {
        Ok(crate::store::sources::list_epg_sources(c)?
            .into_iter()
            .filter(|e| matches!(e.kind, EpgSourceKind::Provider { source_id, .. } if source_id == source))
            .map(|e| e.id)
            .collect())
    })
    .await
}

// ------------------------------------------------------------------ M3U

/// Deletes a downloaded playlist when dropped (it may carry credentials).
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take()
            && let Err(e) = std::fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("removing {}: {e}", p.display());
        }
    }
}

/// A playlist ready to parse: a download (deleted when dropped) or the source's own file.
struct Playlist {
    path: PathBuf,
    base: Option<url::Url>,
    _guard: TempFile,
    etag: Option<String>,
    last_modified: Option<String>,
}

enum Fetched {
    NotModified,
    Playlist(Playlist),
}

/// Download the playlist of `src` (`kind` = where it lives: URL or file; a switched-back
/// source is still Xtream when this runs). `validators` = send the stored ETag /
/// Last-Modified.
async fn fetch_playlist(
    ctx: &JobContext,
    src: &Source,
    kind: SourceKind,
    validators: bool,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<Fetched> {
    if kind == SourceKind::M3uFile {
        let path = src.path.clone().ok_or_else(|| Error::Config("this playlist has no file".into()))?;
        return Ok(Fetched::Playlist(Playlist { path, base: None, _guard: TempFile(None), etag: None, last_modified: None }));
    }
    let url = src.url.clone().ok_or_else(|| Error::Config("this playlist has no URL".into()))?;
    progress.set(JobPhase::Connecting);
    let dest = ctx.paths.playlist_cache().join(format!("{}-{}.m3u", src.id.0, progress.id.0));
    let req = DownloadRequest {
        url: url.clone(),
        dest,
        headers: source_headers(src, cfg),
        etag: if validators { src.status.etag.clone() } else { None },
        last_modified: if validators { src.status.last_modified.clone() } else { None },
    };
    let p = progress.clone();
    Ok(match ctx.http.download(&req, &move |done, total| p.set(JobPhase::Downloading { done, total }), cancel).await? {
        DownloadOutcome::NotModified => Fetched::NotModified,
        DownloadOutcome::Downloaded { path, etag, last_modified, .. } => {
            let base = url::Url::parse(url.expose().trim()).ok();
            Fetched::Playlist(Playlist { path: path.clone(), base, _guard: TempFile(Some(path)), etag, last_modified })
        }
    })
}

/// An M3U source: switch to the Xtream login it carries when an upgrade check is due and the
/// login works (DESIGN §27.3), else import the playlist.
async fn refresh_playlist(
    ctx: &JobContext,
    src: &Source,
    force: bool,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<RefreshResult> {
    let opts = FinishOptions { m3u: true, ..Default::default() };
    if src.settings.keep_m3u {
        return refresh_m3u(ctx, src, src.kind, force, cfg, progress, cancel, opts).await;
    }
    let id = src.id;
    let last = read(ctx, move |c| crate::store::sources::upgrade_attempted_at(c, id)).await?;
    let now = crate::model::now();
    let retry_s = |l: &UpgradeAttempt| if l.transient { UPGRADE_RETRY_TRANSIENT_S } else { UPGRADE_RETRY_S };
    if !force && last.as_ref().is_some_and(|l| now - l.at < retry_s(l)) {
        return refresh_m3u(ctx, src, src.kind, force, cfg, progress, cancel, opts).await;
    }
    // a `get.php` playlist kept after its login failed: downloaded in full the first time, as
    // the import before this check may predate the movies / series fallback
    let refetch = force || last.is_none();

    // 1. the login: from a `get.php` URL, else from the entries (§27.2)
    let get_php = match src.kind {
        SourceKind::M3uUrl => src.url.as_ref().and_then(|u| m3u::detect_xtream_url(u.expose())),
        _ => None,
    };
    let (creds, playlist) = match get_php {
        Some(c) => (Some(c), None),
        None => {
            // never a 304 before the first check, nor while a login it found is failing
            let validators = !force && last.as_ref().is_some_and(|l| !l.login_failed);
            let playlist = match fetch_playlist(ctx, src, src.kind, validators, cfg, progress, cancel).await? {
                Fetched::NotModified => return m3u_not_modified(ctx, src, cfg).await,
                Fetched::Playlist(p) => p,
            };
            let (path, base, scan_cancel) = (playlist.path.clone(), playlist.base.clone(), cancel.clone());
            progress.set(JobPhase::Parsing { items: 0 });
            (blocking(move || scan_playlist(&path, base, &scan_cancel)).await?, Some(playlist))
        }
    };
    check(cancel)?;
    let Some(creds) = creds else {
        // a plain playlist: check again in a week
        let attempt = UpgradeAttempt { at: now, login_failed: false, transient: false };
        write(ctx, move |c| crate::store::sources::record_upgrade_attempt(c, id, &attempt)).await?;
        return keep_playlist(ctx, src, playlist, force, cfg, progress, cancel).await;
    };

    // 2. try the login (the playlist's own User-Agent)
    crate::redact::register_secret(&creds.username);
    crate::redact::register_secret(creds.password.expose());
    let client = XtreamClient::new(ctx.http.clone(), creds.clone(), source_headers(src, cfg).user_agent);
    progress.set(JobPhase::Connecting);
    let account = match client.authenticate(crate::model::now(), cancel).await {
        Ok((_, account)) => account,
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) => {
            upgrade_failed(ctx, src, &creds, &e, false).await?;
            return keep_playlist(ctx, src, playlist, refetch, cfg, progress, cancel).await;
        }
    };

    // 3. switch: the Xtream import commits it at its first finish
    let login = creds.clone();
    let moves = read(ctx, move |c| import::m3u_to_xtream_moves(c, id, &login)).await?;
    let mut upgrade = Upgrade { creds, account, moves, committed: false, transient: false };
    match refresh_xtream(ctx, src, cfg, progress, cancel, Some(&mut upgrade)).await {
        Err(e) if !upgrade.committed && !matches!(e, Error::Cancelled) => {
            upgrade_failed(ctx, src, &upgrade.creds, &e, upgrade.transient).await?;
            keep_playlist(ctx, src, playlist, refetch, cfg, progress, cancel).await
        }
        r => r,
    }
}

/// An upgrade check found no usable Xtream login: record it (retried in a week, or on a forced
/// refresh; in an hour when the server couldn't answer — offline, a timeout, a 5xx or 429 —
/// as that says nothing about the login; also `transient` when every list failed that way)
/// and say why in the log.
async fn upgrade_failed(ctx: &JobContext, src: &Source, creds: &XtreamCredentials, e: &Error, transient: bool) -> Result<()> {
    let transient = transient || is_transient(e);
    let (id, attempt) = (src.id, UpgradeAttempt { at: crate::model::now(), login_failed: true, transient });
    write(ctx, move |c| crate::store::sources::record_upgrade_attempt(c, id, &attempt)).await?;
    tracing::info!(
        "Couldn't use the Xtream login of {} at {} ({}); keeping the playlist",
        src.name,
        crate::redact::redact_url(&creds.server),
        crate::redact::redact(&e.to_string())
    );
    Ok(())
}

/// The source stays M3U: import the playlist already at hand (or download it now).
async fn keep_playlist(
    ctx: &JobContext,
    src: &Source,
    playlist: Option<Playlist>,
    force: bool,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<RefreshResult> {
    let opts = FinishOptions { m3u: true, ..Default::default() };
    match playlist {
        Some(p) => import_m3u(ctx, src, p, cfg, progress, cancel, opts).await,
        None => refresh_m3u(ctx, src, src.kind, force, cfg, progress, cancel, opts).await,
    }
}

/// An upgraded source whose user chose "Keep as M3U": back to its playlist, the switch
/// committed by the import's finish (with the Xtream → M3U key moves, `FinishOptions::m3u`).
async fn switch_back(
    ctx: &JobContext,
    src: &Source,
    origin: SourceKind,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<RefreshResult> {
    let opts = FinishOptions { switch_kind: Some((origin, None)), m3u: true, ..Default::default() };
    refresh_m3u(ctx, src, origin, true, cfg, progress, cancel, opts).await
}

/// Blocking: the Xtream login a playlist carries in its entries (§27.2), `None` as soon as an
/// entry rules it out.
fn scan_playlist(path: &Path, base: Option<url::Url>, cancel: &CancellationToken) -> Result<Option<XtreamCredentials>> {
    let mut parser = m3u::parse(open_decompressed(path)?, base.as_ref()).map_err(m3u_error)?;
    let mut scan = PlaylistScan::default();
    let mut n = 0u64;
    for item in parser.by_ref() {
        match item {
            Ok(e) if !scan.push(&e.url) => return Ok(None),
            Ok(_) | Err(Error::Parse { .. }) => {}
            Err(e) => return Err(e),
        }
        n += 1;
        if n.is_multiple_of(CANCEL_EVERY) && cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
    }
    Ok(scan.finish())
}

#[allow(clippy::too_many_arguments)]
async fn refresh_m3u(
    ctx: &JobContext,
    src: &Source,
    kind: SourceKind,
    force: bool,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
    opts: FinishOptions,
) -> Result<RefreshResult> {
    match fetch_playlist(ctx, src, kind, !force, cfg, progress, cancel).await? {
        Fetched::NotModified => m3u_not_modified(ctx, src, cfg).await,
        Fetched::Playlist(p) => import_m3u(ctx, src, p, cfg, progress, cancel, opts).await,
    }
}

/// Parse and import a playlist: channels, plus Xtream-style movies and series (§27.5).
async fn import_m3u(
    ctx: &JobContext,
    src: &Source,
    playlist: Playlist,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
    opts: FinishOptions,
) -> Result<RefreshResult> {
    check(cancel)?;
    let Playlist { path, base, _guard, etag, last_modified } = playlist;
    let now = crate::model::now();
    let id = src.id;
    let imp = write(ctx, move |c| import::begin_catalogue(c, id, now)).await?;
    let smart = smart_names(src, cfg);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<M3uBatch>(CHANNEL_CAP);
    let parse_cancel = cancel.clone();
    let parser = tokio::task::spawn_blocking(move || parse_m3u_file(&path, base, smart, &tx, &parse_cancel));

    let mut done = 0u64;
    while let Some(batch) = rx.recv().await {
        check(cancel)?;
        let n = batch.len() as u64;
        let imp = imp.clone();
        write(ctx, move |c| {
            if !batch.groups.is_empty() {
                import::upsert_groups(c, &imp, &batch.groups)?;
            }
            if !batch.channels.is_empty() {
                import::write_channels(c, &imp, &batch.channels)?;
            }
            if !batch.movies.is_empty() {
                import::write_movies(c, &imp, &batch.movies)?;
            }
            if !batch.series.is_empty() {
                import::write_series_episodes(c, &imp, &batch.series)?;
            }
            Ok(())
        })
        .await?;
        done += n;
        progress.set(JobPhase::Writing { done, total: None });
    }
    let parsed = parser.await.map_err(|e| Error::Other(format!("playlist parser failed: {e}")))??;
    check(cancel)?;
    if parsed.entries == 0 {
        return Err(Error::BadPayload("the playlist contains no channels".into()));
    }
    if parsed.skipped > 0 {
        tracing::info!("{}: skipped {} malformed playlist entries", src.name, parsed.skipped);
    }

    progress.set(JobPhase::Finishing);
    let (fin, switched) = (imp.clone(), opts.switch_kind.is_some());
    let completed = [ItemKind::Channel, ItemKind::Movie, ItemKind::Series];
    let summary = write(ctx, move |c| import::finish_catalogue_with(c, &fin, &completed, now, &opts)).await?;
    if switched {
        notify(ctx, DataScope::Sources);
    }
    let logins = parsed.logins;
    write(ctx, move |c| crate::store::sources::set_playlist_logins(c, id, &logins)).await?;
    mark_refreshed(ctx, src, counts_of(&summary), etag, last_modified, now).await?;
    provider_epg(ctx, src, parsed.tvg_urls.clone(), cfg).await?;
    (ctx.submit)(JobKind::RebuildLibrary);
    Ok(RefreshResult { summary, not_modified: false, epg_urls: parsed.tvg_urls, upgraded: false })
}

/// 304: the stored catalogue is current. Bookkeeping only; due provider guides still refresh.
async fn m3u_not_modified(ctx: &JobContext, src: &Source, cfg: &Config) -> Result<RefreshResult> {
    let now = crate::model::now();
    mark_refreshed(ctx, src, src.status.counts, src.status.etag.clone(), src.status.last_modified.clone(), now).await?;
    if src.settings.use_provider_epg.unwrap_or(true) {
        let ids = provider_epg_ids(ctx, src.id).await?;
        submit_stale_epg(ctx, ids, cfg).await?;
    }
    Ok(RefreshResult { summary: ImportSummary::default(), not_modified: true, epg_urls: Vec::new(), upgraded: false })
}

/// One writer transaction's worth of rows plus the groups first seen in it.
#[derive(Default)]
struct M3uBatch {
    groups: Vec<ImportGroup>,
    channels: Vec<ImportChannel>,
    movies: Vec<ImportMovie>,
    /// Whole series, written after the parse.
    series: Vec<ImportSeriesEpisodes>,
}

impl M3uBatch {
    /// Entries in the batch (episodes counted one by one).
    fn len(&self) -> usize {
        self.channels.len() + self.movies.len() + self.series.iter().map(|s| s.episodes.len()).sum::<usize>()
    }
}

struct M3uParsed {
    tvg_urls: Vec<String>,
    entries: u64,
    skipped: u64,
    /// The Xtream logins (user, password) of the entries' stream URLs, registered with the
    /// redactor as they were found ([`MAX_PLAYLIST_LOGINS`] at most).
    logins: Vec<(String, String)>,
}

/// Distinct Xtream logins an M3U import keeps for redaction (a merged playlist may carry many).
const MAX_PLAYLIST_LOGINS: usize = 16;
/// Pairs seen on one entry only, held while waiting for a second (a plain playlist has many).
const MAX_LOGIN_CANDIDATES: usize = 64;

/// The logins of an M3U import's Xtream stream URLs: the playlist's short-form entries with a
/// path prefix carry them too, and no URL pattern can mask those. A pair counts once two
/// entries with different stream ids carry it — every entry of an account does, while a CDN
/// path that merely looks like `/live/<a>/<b>/<n>` must not make "channel" a secret.
#[derive(Default)]
struct PlaylistLogins {
    /// Registered with the redactor as they were confirmed.
    found: Vec<(String, String)>,
    /// Seen on one entry so far, with its stream id.
    once: Vec<((String, String), i64)>,
}

impl PlaylistLogins {
    fn note(&mut self, url: &str) {
        if self.found.len() >= MAX_PLAYLIST_LOGINS {
            return;
        }
        let Some(s) = parse_stream_url(url) else { return };
        let login = (s.creds.username, s.creds.password.into_inner());
        if self.found.contains(&login) {
            return;
        }
        match self.once.iter().position(|(l, _)| *l == login) {
            Some(i) if self.once[i].1 != s.stream_id => {
                self.once.swap_remove(i);
                crate::redact::register_secret(&login.0);
                crate::redact::register_secret(&login.1);
                self.found.push(login);
            }
            Some(_) => {}
            None if self.once.len() < MAX_LOGIN_CANDIDATES => self.once.push((login, s.stream_id)),
            None => {}
        }
    }
}

/// Blocking: parse `path` and feed batches into `tx`. Stops (`Cancelled`) when the receiver
/// is gone (the writer loop failed) or the job was cancelled.
fn parse_m3u_file(
    path: &std::path::Path,
    base: Option<url::Url>,
    smart: bool,
    tx: &tokio::sync::mpsc::Sender<M3uBatch>,
    cancel: &CancellationToken,
) -> Result<M3uParsed> {
    let reader = open_decompressed(path)?;
    let mut parser = m3u::parse(reader, base.as_ref()).map_err(m3u_error)?;
    let mut keys = KeyAllocator::default();
    let mut seen_groups: HashSet<String> = HashSet::new();
    let mut new_groups: Vec<ImportGroup> = Vec::new();
    let mut batch = M3uBatch::default();
    let mut episodes = EpisodeCollector::default();
    let (mut entries, mut skipped) = (0u64, 0u64);
    let mut logins = PlaylistLogins::default();
    let send = |b: M3uBatch| tx.blocking_send(b).map_err(|_| Error::Cancelled);
    for item in parser.by_ref() {
        match item {
            Ok(e) => {
                let order = i32::try_from(entries).unwrap_or(i32::MAX);
                logins.note(&e.url);
                match m3u_vod::classify(&e) {
                    EntryClass::Live => batch.channels.push(m3u_entry_to_channel(&e, order, smart, &mut keys, &mut new_groups)),
                    EntryClass::Movie { stream_id } => batch.movies.push(m3u_entry_to_movie(&e, stream_id, order, smart, &mut keys, &mut new_groups)),
                    EntryClass::Episode { stream_id, marker } => episodes.push(&e, stream_id, marker, order, smart, &mut keys, &mut new_groups),
                }
                for mut g in new_groups.drain(..) {
                    if seen_groups.insert(g.key.0.clone()) {
                        g.provider_order = i32::try_from(seen_groups.len() - 1).unwrap_or(i32::MAX);
                        batch.groups.push(g);
                    }
                }
                entries += 1;
                if batch.channels.len() + batch.movies.len() >= BATCH {
                    send(std::mem::take(&mut batch))?;
                }
                if entries.is_multiple_of(CANCEL_EVERY) && cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
            }
            Err(Error::Parse { line, msg }) => {
                skipped += 1;
                tracing::debug!("playlist entry skipped (line {line:?}): {msg}");
            }
            // a read error: never import (and sweep) a half-read playlist
            Err(e) => return Err(e),
        }
    }
    if batch.len() > 0 || !batch.groups.is_empty() {
        send(batch)?;
    }
    for series in episodes.into_batches(smart) {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        send(M3uBatch { series, ..Default::default() })?;
    }
    Ok(M3uParsed { tvg_urls: parser.header().tvg_urls.clone(), entries, skipped, logins: logins.found })
}

pub(super) fn m3u_error(e: M3uError) -> Error {
    match e {
        M3uError::NotM3u(PayloadKind::Html) => {
            Error::BadPayload("the server returned a web page instead of a playlist — the account may have expired".into())
        }
        M3uError::HlsMediaPlaylist => Error::BadPayload("this address is a single HLS stream, not a channel list — add it as a single stream".into()),
        M3uError::NotM3u(kind) => Error::BadPayload(format!("this is not an M3U playlist (it looks like {})", payload_name(kind))),
    }
}

fn payload_name(kind: PayloadKind) -> &'static str {
    match kind {
        PayloadKind::M3u | PayloadKind::HlsMedia => "a playlist",
        PayloadKind::Xmltv => "an XMLTV guide",
        PayloadKind::Json => "JSON data",
        PayloadKind::Html => "a web page",
        PayloadKind::Unknown => "something else",
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Convert one M3U entry. `smart` = smart-name cleanup on. Group keys are returned in
/// `ImportChannel.groups`; `groups_out` collects new `ImportGroup`s (deduped by key).
pub fn m3u_entry_to_channel(
    e: &M3uEntry,
    order: i32,
    smart: bool,
    keys: &mut crate::sources::identity::KeyAllocator,
    groups_out: &mut Vec<ImportGroup>,
) -> ImportChannel {
    let EntryIdentity { name, norm_name, tvg_id, key } = entry_identity(e, keys);
    let cleaned = clean_channel_name(&name);
    let quality = NameQuality::of(&cleaned);
    let clean_name = if smart { cleaned.name } else { name.clone() };
    let groups = entry_groups(e, ItemKind::Channel, smart, groups_out);
    // the Xtream id of a live stream URL: switching back from Xtream finds the channel by it
    let provider_id = parse_stream_url(&e.url).filter(|s| s.kind == XtreamStreamKind::Live).map(|s| s.stream_id.to_string());

    ImportChannel {
        key,
        provider_id,
        name,
        clean_name,
        norm_name,
        number: e.tvg_chno.filter(|n| *n > 0),
        logo: non_empty(e.tvg_logo.as_deref()),
        tvg_id,
        tvg_name: non_empty(e.tvg_name.as_deref()),
        tvg_shift_min: e.tvg_shift_h.map(|h| (h * 60.0).round() as i32).unwrap_or(0),
        url_key: identity::url_key(&e.url),
        stream: StreamLocator::Url { url: e.url.clone() },
        headers: (!e.headers.is_empty()).then(|| e.headers.clone()),
        catchup: e.catchup.clone(),
        radio: e.radio,
        unsupported: e.drm.then_some(Unsupported::Drm),
        provider_order: order,
        added_at: None,
        groups,
        quality,
        adult: false,
    }
}

/// Name and key of an entry, whatever it becomes: movies and episodes keep the key the entry
/// had as a channel, so user data of legacy imports follows them (DESIGN §27.4).
struct EntryIdentity {
    name: String,
    norm_name: String,
    tvg_id: Option<String>,
    key: ItemKey,
}

fn entry_identity(e: &M3uEntry, keys: &mut KeyAllocator) -> EntryIdentity {
    let title = e.title.trim();
    let name = if title.is_empty() { e.tvg_name.as_deref().map(str::trim).unwrap_or("").to_owned() } else { title.to_owned() };
    let name = if name.is_empty() { e.url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&e.url).to_owned() } else { name };
    let norm_name = normalize_name(&name);
    let tvg_id = non_empty(e.tvg_id.as_deref());
    let primary_group = e.groups.first().map(|g| normalize_group(g));
    let key = keys.alloc(identity::m3u_base_key(tvg_id.as_deref(), &norm_name, primary_group.as_deref()));
    EntryIdentity { name, norm_name, tvg_id, key }
}

/// Group keys of an entry as `kind` groups ("Uncategorized" when it has none); new groups are
/// appended to `groups_out` (deduped by key).
fn entry_groups(e: &M3uEntry, kind: ItemKind, smart: bool, groups_out: &mut Vec<ImportGroup>) -> Vec<GroupKey> {
    let mut groups = Vec::with_capacity(e.groups.len().max(1));
    let raws: Vec<&str> = if e.groups.is_empty() { vec![""] } else { e.groups.iter().map(String::as_str).collect() };
    for raw in raws {
        let k = group_key(kind, raw);
        if groups.contains(&k) {
            continue;
        }
        // playlists are grouped: the newest group is the likeliest hit
        if !groups_out.iter().rev().any(|g| g.key == k) {
            let provider_order = i32::try_from(groups_out.len()).unwrap_or(i32::MAX);
            groups_out.push(ImportGroup {
                kind,
                key: k.clone(),
                name: group_display(raw, smart),
                raw_name: non_empty(Some(raw)),
                provider_id: None,
                provider_order,
                adult: false,
            });
        }
        groups.push(k);
    }
    groups
}

/// An Xtream-style `/movie/` entry (§27.5): title, poster and groups are all a playlist has.
fn m3u_entry_to_movie(
    e: &M3uEntry,
    stream_id: i64,
    order: i32,
    smart: bool,
    keys: &mut KeyAllocator,
    groups_out: &mut Vec<ImportGroup>,
) -> ImportMovie {
    let EntryIdentity { name, norm_name, key, .. } = entry_identity(e, keys);
    let (cleaned, year) = clean_vod_name(&name);
    ImportMovie {
        key,
        provider_id: Some(stream_id.to_string()),
        clean_name: if smart { cleaned } else { name.clone() },
        norm_name,
        year,
        poster: non_empty(e.tvg_logo.as_deref()),
        rating: None,
        stream: StreamLocator::Url { url: e.url.clone() },
        url_key: identity::url_key(&e.url),
        provider_order: order,
        added_at: None,
        groups: entry_groups(e, ItemKind::Movie, smart, groups_out),
        adult: false,
        name,
    }
}

/// Episode entries collected during the parse and assembled into whole series at its end
/// (§27.5): one compact record per entry.
#[derive(Default)]
struct EpisodeCollector {
    series: Vec<SeriesAcc>,
    /// `normalize_name(series title)` → index in `series`.
    index: HashMap<String, usize>,
    episodes: Vec<EpisodeEntry>,
}

struct SeriesAcc {
    title: String,
    norm: String,
    cover: Option<String>,
    groups: Vec<GroupKey>,
    order: i32,
    /// Episodes without a marker so far (they are numbered in file order).
    unmarked: u32,
}

struct EpisodeEntry {
    series: usize,
    season: u32,
    episode: u32,
    order: i32,
    title: Option<String>,
    key: ItemKey,
    stream_id: i64,
    url: String,
    logo: Option<String>,
}

impl EpisodeCollector {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        e: &M3uEntry,
        stream_id: i64,
        marker: Option<EpisodeMarker>,
        order: i32,
        smart: bool,
        keys: &mut KeyAllocator,
        groups_out: &mut Vec<ImportGroup>,
    ) {
        let EntryIdentity { name, key, .. } = entry_identity(e, keys);
        let (title, season, episode, episode_title) = match marker {
            Some(m) => {
                let series = if m.series.is_empty() { e.groups.first().cloned().unwrap_or_else(|| name.clone()) } else { m.series };
                (series, m.season, Some(m.episode), m.title)
            }
            None => (if smart { clean_vod_name(&name).0 } else { name.clone() }, 1, None, None),
        };
        let norm = normalize_name(&title);
        let logo = non_empty(e.tvg_logo.as_deref());
        let i = match self.index.get(&norm) {
            Some(&i) => i,
            None => {
                self.series.push(SeriesAcc { title, norm: norm.clone(), cover: None, groups: Vec::new(), order, unmarked: 0 });
                self.index.insert(norm, self.series.len() - 1);
                self.series.len() - 1
            }
        };
        let s = &mut self.series[i];
        for g in entry_groups(e, ItemKind::Series, smart, groups_out) {
            if !s.groups.contains(&g) {
                s.groups.push(g);
            }
        }
        if s.cover.is_none() {
            s.cover.clone_from(&logo);
        }
        let episode = episode.unwrap_or_else(|| {
            s.unmarked += 1;
            s.unmarked
        });
        self.episodes.push(EpisodeEntry { series: i, season, episode, order, title: episode_title, key, stream_id, url: e.url.clone(), logo });
    }

    /// Whole series in writer batches of about [`BATCH`] episodes, episodes ordered by
    /// (season, episode, file order).
    fn into_batches(mut self, smart: bool) -> Vec<Vec<ImportSeriesEpisodes>> {
        self.episodes.sort_by_key(|e| (e.series, e.season, e.episode, e.order));
        let mut out: Vec<Vec<ImportSeriesEpisodes>> = Vec::new();
        let (mut batch, mut size) = (Vec::new(), 0usize);
        let mut eps = self.episodes.into_iter().peekable();
        for (i, s) in self.series.into_iter().enumerate() {
            let mut episodes = Vec::new();
            while let Some(e) = eps.next_if(|e| e.series == i) {
                episodes.push(ImportEpisode {
                    key: e.key,
                    provider_id: Some(e.stream_id.to_string()),
                    season: e.season,
                    episode: e.episode,
                    title: e.title.unwrap_or_default(),
                    plot: None,
                    duration_s: None,
                    still: e.logo.filter(|l| s.cover.as_ref() != Some(l)),
                    air_date: None,
                    rating: None,
                    stream: StreamLocator::Url { url: e.url },
                });
            }
            let (cleaned, year) = clean_vod_name(&s.title);
            size += episodes.len();
            batch.push(ImportSeriesEpisodes {
                series: ImportSeries {
                    key: identity::m3u_series_key(&s.norm),
                    provider_id: None,
                    clean_name: if smart { cleaned } else { s.title.clone() },
                    norm_name: s.norm,
                    year,
                    cover: s.cover,
                    backdrop: None,
                    rating: None,
                    plot: None,
                    genre: None,
                    cast: None,
                    director: None,
                    release_date: None,
                    last_modified: None,
                    provider_order: s.order,
                    added_at: None,
                    groups: s.groups,
                    adult: false,
                    name: s.title,
                },
                episodes,
            });
            if size >= BATCH {
                out.push(std::mem::take(&mut batch));
                size = 0;
            }
        }
        if !batch.is_empty() {
            out.push(batch);
        }
        out
    }
}

// ------------------------------------------------------------------ Xtream

/// Errors that end the whole refresh (anything else fails one kind only).
fn is_fatal(e: &Error) -> bool {
    matches!(e, Error::Cancelled | Error::AuthRejected | Error::AccountUnavailable { .. } | Error::Db(_) | Error::Io { .. })
}

/// The server couldn't answer (offline, a timeout, a 5xx or 429): says nothing about the login.
fn is_transient(e: &Error) -> bool {
    matches!(e, Error::Network(_) | Error::Http { status: 429 | 500..=599, .. })
}

/// A list action's error in refresh terms: 401 = the login no longer works; 403 = the panel
/// refuses this request (blocked account, too many devices, User-Agent filter).
fn list_error(e: Error) -> Error {
    match e {
        Error::Http { status: 401, .. } => Error::AuthRejected,
        Error::Http { status: 403, .. } => {
            Error::Other("access denied by the provider (HTTP 403) — the account may be blocked or limited to other devices".into())
        }
        e => e,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtKind {
    Live,
    Movies,
    Series,
}

impl XtKind {
    fn item_kind(self) -> ItemKind {
        match self {
            XtKind::Live => ItemKind::Channel,
            XtKind::Movies => ItemKind::Movie,
            XtKind::Series => ItemKind::Series,
        }
    }
    fn label(self) -> &'static str {
        match self {
            XtKind::Live => "Channels",
            XtKind::Movies => "Movies",
            XtKind::Series => "Series",
        }
    }
}

/// An M3U source switching to its Xtream login in this refresh (DESIGN §27.3), or — already
/// `committed` — an upgraded source whose earlier refresh left M3U → Xtream moves undone.
struct Upgrade {
    creds: XtreamCredentials,
    /// From the login of the upgrade check (stored with the switch).
    account: AccountInfo,
    /// M3U → Xtream user-data moves not applied yet: each finish applies those of its kinds,
    /// so moved data never meets the M3U rows of a kind that is not re-imported yet.
    moves: Vec<KeyMove>,
    /// The source is Xtream now (its first finish switched it).
    committed: bool,
    /// Not committed because every list failed only as [`is_transient`] (their combined
    /// error no longer says so).
    transient: bool,
}

impl Upgrade {
    /// Options of a finish completing `kinds`: the switch at the first one, and the moves from
    /// or to those kinds (episodes go with series).
    fn finish_options(&mut self, kinds: &[ItemKind]) -> FinishOptions {
        let touches = |k: ItemKind| kinds.contains(&if k == ItemKind::Episode { ItemKind::Series } else { k });
        let (now, later) = std::mem::take(&mut self.moves).into_iter().partition(|m| touches(m.from.kind) || touches(m.to.kind));
        self.moves = later;
        FinishOptions {
            switch_kind: (!self.committed).then(|| (SourceKind::Xtream, Some(self.creds.clone()))),
            account: (!self.committed).then(|| self.account.clone()),
            moves: now,
            m3u: false,
        }
    }
}

/// `finish_catalogue` of an Xtream import; the first finish of an upgrade commits it.
async fn finish_xtream(
    ctx: &JobContext,
    imp: &CatalogueImport,
    kinds: Vec<ItemKind>,
    now: Timestamp,
    mut upgrade: Option<&mut Upgrade>,
) -> Result<ImportSummary> {
    let opts = upgrade.as_deref_mut().map(|u| u.finish_options(&kinds)).unwrap_or_default();
    let fin = imp.clone();
    let summary = write(ctx, move |c| import::finish_catalogue_with(c, &fin, &kinds, now, &opts)).await?;
    if let Some(u) = upgrade
        && !u.committed
    {
        u.committed = true;
        notify(ctx, DataScope::Sources);
    }
    Ok(summary)
}

/// An Xtream import; with `upgrade`, the one that switches an M3U source (DESIGN §27.3). Once
/// that switch committed, the refresh reports success (`upgraded`) even when a later kind
/// fails fatally or the job is cancelled: those kinds keep their rows and the next refresh
/// imports them, with the user-data moves still pending.
async fn refresh_xtream(
    ctx: &JobContext,
    src: &Source,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancellationToken,
    upgrade: Option<&mut Upgrade>,
) -> Result<RefreshResult> {
    let switching = upgrade.as_ref().is_some_and(|u| !u.committed);
    let creds = match &upgrade {
        Some(u) => u.creds.clone(),
        None => src.xtream.clone().ok_or_else(|| Error::Config("this source has no Xtream login".into()))?,
    };
    let client = XtreamClient::new(ctx.http.clone(), creds.clone(), source_headers(src, cfg).user_agent);
    let id = src.id;
    let mut leftover = None;
    if upgrade.is_none() {
        progress.set(JobPhase::Connecting);
        let (_, account) = client.authenticate(crate::model::now(), cancel).await?;
        write(ctx, move |c| crate::store::sources::set_account(c, id, &account)).await?;
        // an upgrade whose import stopped after the switch: the kinds it did not finish still
        // have their M3U rows, and the user data on them moves with this import's finishes
        if src.m3u_origin().is_some() {
            let login = creds.clone();
            let moves = read(ctx, move |c| import::m3u_to_xtream_moves(c, id, &login)).await?;
            if !moves.is_empty() {
                leftover = Some(Upgrade { creds: creds.clone(), account: AccountInfo::default(), moves, committed: true, transient: false });
            }
        }
    }
    let mut upgrade = upgrade.or(leftover.as_mut());
    let committed_here = |u: &Option<&mut Upgrade>| switching && u.as_ref().is_some_and(|u| u.committed);

    let now = crate::model::now();
    let imp = write(ctx, move |c| import::begin_catalogue(c, id, now)).await?;
    let smart = smart_names(src, cfg);
    let mut failures: Vec<(XtKind, Error)> = Vec::new();

    // live first: finished on its own so Live TV is usable while VOD imports
    let mut live: Option<ImportSummary> = None;
    match import_xt_kind(ctx, &client, &imp, XtKind::Live, smart, progress, cancel).await {
        Ok(()) => {
            progress.set(JobPhase::Finishing);
            live = Some(finish_xtream(ctx, &imp, vec![ItemKind::Channel], now, upgrade.as_deref_mut()).await?);
            (ctx.submit)(JobKind::RebuildLibrary);
        }
        Err(e) if is_fatal(&e) => return Err(e),
        Err(e) => failures.push((XtKind::Live, e)),
    }

    let mut vod_done: Vec<ItemKind> = Vec::new();
    if src.settings.import_vod.unwrap_or(true) {
        for kind in [XtKind::Movies, XtKind::Series] {
            match import_xt_kind(ctx, &client, &imp, kind, smart, progress, cancel).await {
                Ok(()) => vod_done.push(kind.item_kind()),
                Err(e) if is_fatal(&e) && !committed_here(&upgrade) => return Err(e),
                // switched at the live finish: the upgrade stands, the rest waits for the next
                // refresh (no sweep, no bookkeeping: the source stays due)
                Err(Error::Cancelled) => {
                    tracing::info!("{} now uses its Xtream login ({}); import cancelled", src.name, crate::redact::redact_url(&creds.server));
                    let summary = live.unwrap_or_default();
                    return Ok(RefreshResult { summary, not_modified: false, epg_urls: Vec::new(), upgraded: true });
                }
                Err(e) if is_fatal(&e) => {
                    failures.push((kind, e));
                    break;
                }
                Err(e) => failures.push((kind, e)),
            }
        }
    } else {
        // VOD import switched off: nothing was seen, so finishing sweeps the old rows
        vod_done.extend([ItemKind::Movie, ItemKind::Series]);
    }

    let summary = match (live, vod_done.is_empty()) {
        (None, true) => {
            if let Some(u) = upgrade {
                u.transient = failures.iter().all(|(_, e)| is_transient(e));
            }
            return Err(combine(failures));
        }
        (Some(s), true) => s,
        (live, false) => {
            progress.set(JobPhase::Finishing);
            let mut s = finish_xtream(ctx, &imp, vod_done, now, upgrade.as_deref_mut()).await?;
            if let Some(l) = live {
                s.channels = KindCounts { total: s.channels.total, ..l.channels };
                s.groups.added += l.groups.added;
                s.groups.updated += l.groups.updated;
                s.groups.removed += l.groups.removed;
                s.reattached += l.reattached;
                s.orphaned += l.orphaned;
            }
            s
        }
    };

    mark_refreshed(ctx, src, counts_of(&summary), None, None, now).await?;
    if !failures.is_empty() {
        let note = failures.iter().map(|(k, e)| format!("{}: {e}", k.label())).collect::<Vec<_>>().join("; ");
        for (k, e) in &failures {
            tracing::warn!(target: "tuner::jobs", "Couldn't import the {} of {}: {e}", k.label().to_lowercase(), src.name);
        }
        let at = crate::model::now();
        write(ctx, move |c| crate::store::sources::mark_failed(c, id, &note, at)).await?;
    }
    let epg_urls = vec![urls::xmltv_url(&creds)];
    provider_epg(ctx, src, epg_urls.clone(), cfg).await?;
    (ctx.submit)(JobKind::RebuildLibrary);
    let upgraded = committed_here(&upgrade);
    if upgraded {
        tracing::info!("{} now uses its Xtream login ({})", src.name, crate::redact::redact_url(&creds.server));
    }
    Ok(RefreshResult { summary, not_modified: false, epg_urls, upgraded })
}

/// Every kind failed: one error naming each kind (the first error alone when only one).
fn combine(mut failures: Vec<(XtKind, Error)>) -> Error {
    if failures.len() == 1 {
        return failures.remove(0).1;
    }
    Error::Other(failures.iter().map(|(k, e)| format!("{}: {e}", k.label())).collect::<Vec<_>>().join("; "))
}

/// Fetch, convert and write one kind (categories + items). Nothing is swept here.
async fn import_xt_kind(
    ctx: &JobContext,
    client: &XtreamClient,
    imp: &CatalogueImport,
    kind: XtKind,
    smart: bool,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<()> {
    progress.set(JobPhase::Connecting);
    let (groups, rows) = match kind {
        XtKind::Live => {
            let cats = client.live_categories(cancel).await.map_err(list_error)?;
            let items = client.live_streams(cancel).await.map_err(list_error)?;
            progress.set(JobPhase::Parsing { items: items.len() as u64 });
            blocking(move || Ok(convert_xt(ItemKind::Channel, &cats, &items, smart, xt_live_to_channel, |s| ids_of(&s.category_id, &s.category_ids))))
                .await
                .map(|(g, r)| (g, Rows::Channels(r)))?
        }
        XtKind::Movies => {
            let cats = client.vod_categories(cancel).await.map_err(list_error)?;
            let items = client.vod_streams(cancel).await.map_err(list_error)?;
            progress.set(JobPhase::Parsing { items: items.len() as u64 });
            blocking(move || Ok(convert_xt(ItemKind::Movie, &cats, &items, smart, xt_vod_to_movie, |s| ids_of(&s.category_id, &s.category_ids))))
                .await
                .map(|(g, r)| (g, Rows::Movies(r)))?
        }
        XtKind::Series => {
            let cats = client.series_categories(cancel).await.map_err(list_error)?;
            let items = client.series(cancel).await.map_err(list_error)?;
            progress.set(JobPhase::Parsing { items: items.len() as u64 });
            blocking(move || Ok(convert_xt(ItemKind::Series, &cats, &items, smart, xt_series_to_series, |s| ids_of(&s.category_id, &s.category_ids))))
                .await
                .map(|(g, r)| (g, Rows::Series(r)))?
        }
    };
    check(cancel)?;

    let gi = imp.clone();
    write(ctx, move |c| import::upsert_groups(c, &gi, &groups)).await?;
    let total = rows.len() as u64;
    let mut done = 0u64;
    progress.set(JobPhase::Writing { done, total: Some(total) });
    for chunk in rows.into_chunks() {
        check(cancel)?;
        let n = chunk.len() as u64;
        let bi = imp.clone();
        write(ctx, move |c| chunk.write(c, &bi)).await?;
        done += n;
        progress.set(JobPhase::Writing { done, total: Some(total) });
    }
    Ok(())
}

/// Converted rows of one kind.
enum Rows {
    Channels(Vec<ImportChannel>),
    Movies(Vec<ImportMovie>),
    Series(Vec<ImportSeries>),
}

impl Rows {
    fn len(&self) -> usize {
        match self {
            Rows::Channels(v) => v.len(),
            Rows::Movies(v) => v.len(),
            Rows::Series(v) => v.len(),
        }
    }

    /// Writer-transaction-sized pieces.
    fn into_chunks(self) -> Vec<Rows> {
        fn split<T>(mut v: Vec<T>, wrap: fn(Vec<T>) -> Rows) -> Vec<Rows> {
            let mut out = Vec::with_capacity(v.len() / BATCH + 1);
            while v.len() > BATCH {
                let rest = v.split_off(BATCH);
                out.push(wrap(std::mem::replace(&mut v, rest)));
            }
            if !v.is_empty() {
                out.push(wrap(v));
            }
            out
        }
        match self {
            Rows::Channels(v) => split(v, Rows::Channels),
            Rows::Movies(v) => split(v, Rows::Movies),
            Rows::Series(v) => split(v, Rows::Series),
        }
    }

    fn write(&self, c: &mut rusqlite::Connection, imp: &CatalogueImport) -> Result<(u32, u32)> {
        match self {
            Rows::Channels(v) => import::write_channels(c, imp, v),
            Rows::Movies(v) => import::write_movies(c, imp, v),
            Rows::Series(v) => import::write_series(c, imp, v),
        }
    }
}

/// Category ids of an item, deduped, `category_ids` (XUI.one: all of them) first.
fn ids_of<'a>(single: &'a Option<String>, many: &'a [String]) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::with_capacity(many.len() + 1);
    for id in many.iter().map(String::as_str).chain(single.as_deref()).map(str::trim).filter(|s| !s.is_empty()) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Categories → groups, items → rows. Each item gets only its own categories (an id-indexed
/// lookup instead of a scan of every category per item), plus "Uncategorized" when needed.
fn convert_xt<T, R>(
    kind: ItemKind,
    cats: &[XtCategory],
    items: &[T],
    smart: bool,
    convert: fn(&T, i32, bool, &[XtCategory]) -> Option<R>,
    category_ids: impl Fn(&T) -> Vec<&str>,
) -> (Vec<ImportGroup>, Vec<R>)
where
    R: HasGroups,
{
    let mut groups = xt_categories_to_groups(kind, cats, smart);
    let by_id: HashMap<&str, &XtCategory> =
        cats.iter().filter_map(|c| c.category_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|id| (id, c))).collect();
    let mut rows = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let mine: Vec<XtCategory> = category_ids(item).into_iter().filter_map(|id| by_id.get(id).map(|c| (*c).clone())).collect();
        if let Some(r) = convert(item, i32::try_from(i).unwrap_or(i32::MAX), smart, &mine) {
            rows.push(r);
        }
    }
    let uncategorized = group_key(kind, "");
    if !groups.iter().any(|g| g.key == uncategorized) && rows.iter().any(|r| r.group_keys().contains(&uncategorized)) {
        let provider_order = i32::try_from(groups.len()).unwrap_or(i32::MAX);
        groups.push(ImportGroup {
            kind,
            key: uncategorized,
            name: "Uncategorized".into(),
            raw_name: None,
            provider_id: None,
            provider_order,
            adult: false,
        });
    }
    (groups, rows)
}

trait HasGroups {
    fn group_keys(&self) -> &[crate::model::GroupKey];
}
impl HasGroups for ImportChannel {
    fn group_keys(&self) -> &[crate::model::GroupKey] {
        &self.groups
    }
}
impl HasGroups for ImportMovie {
    fn group_keys(&self) -> &[crate::model::GroupKey] {
        &self.groups
    }
}
impl HasGroups for ImportSeries {
    fn group_keys(&self) -> &[crate::model::GroupKey] {
        &self.groups
    }
}

/// Group keys of an item: its categories found in `cats` (by id), else "uncategorized".
fn xt_group_keys(kind: ItemKind, single: &Option<String>, many: &[String], cats: &[XtCategory]) -> Vec<crate::model::GroupKey> {
    let mut keys = Vec::new();
    for id in ids_of(single, many) {
        if let Some(c) = cats.iter().find(|c| c.category_id.as_deref().map(str::trim) == Some(id)) {
            let k = group_key(kind, c.category_name.as_deref().unwrap_or(""));
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    if keys.is_empty() {
        keys.push(group_key(kind, ""));
    }
    keys
}

fn positive(v: Option<i64>) -> Option<i64> {
    v.filter(|v| *v > 0)
}

/// Provider name, or a stand-in so an unnamed stream stays reachable.
fn xt_name(name: &Option<String>, fallback: impl FnOnce() -> String) -> String {
    non_empty(name.as_deref()).unwrap_or_else(fallback)
}

pub fn xt_live_to_channel(s: &XtLiveStream, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportChannel> {
    let id = positive(s.stream_id)?;
    let name = xt_name(&s.name, || format!("Channel {id}"));
    let cleaned = clean_channel_name(&name);
    let quality = NameQuality::of(&cleaned);
    let clean_name = if smart { cleaned.name } else { name.clone() };
    let catchup = match (positive(s.tv_archive), positive(s.tv_archive_duration)) {
        (Some(_), Some(days)) => {
            Some(CatchupSpec { kind: CatchupKind::Xc, template: None, days: u32::try_from(days).unwrap_or(u32::MAX), correction_s: 0 })
        }
        _ => None,
    };
    let direct = non_empty(s.direct_source.as_deref());
    Some(ImportChannel {
        key: identity::xtream_key(ItemKind::Channel, id),
        provider_id: Some(id.to_string()),
        norm_name: normalize_name(&name),
        clean_name,
        number: positive(s.num).and_then(|n| u32::try_from(n).ok()),
        logo: non_empty(s.stream_icon.as_deref()),
        tvg_id: non_empty(s.epg_channel_id.as_deref()),
        tvg_name: None,
        tvg_shift_min: 0,
        stream: StreamLocator::Xtream { kind: XtreamStreamKind::Live, stream_id: id, ext: None },
        url_key: direct.as_deref().and_then(identity::url_key),
        headers: None,
        catchup,
        radio: false,
        unsupported: None,
        provider_order: order,
        added_at: positive(s.added),
        groups: xt_group_keys(ItemKind::Channel, &s.category_id, &s.category_ids, cats),
        quality,
        adult: s.is_adult,
        name,
    })
}

/// "1999", "1999-03-31" → 1999.
fn year_of(s: Option<&str>) -> Option<u16> {
    let s = s?.trim();
    let digits: String = s.chars().take(4).collect();
    digits.parse::<u16>().ok().filter(|y| (1870..=2200).contains(y))
}

/// 0..10 rating; panels send 0 for "unrated".
fn rating_of(r: Option<f64>) -> Option<f32> {
    r.filter(|r| r.is_finite() && *r > 0.0).map(|r| r.min(10.0) as f32)
}

pub fn xt_vod_to_movie(s: &XtVodStream, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportMovie> {
    let id = positive(s.stream_id)?;
    let name = xt_name(&s.name, || format!("Movie {id}"));
    let (cleaned, title_year) = clean_vod_name(&name);
    let clean_name = if smart { cleaned } else { name.clone() };
    Some(ImportMovie {
        key: identity::xtream_key(ItemKind::Movie, id),
        provider_id: Some(id.to_string()),
        norm_name: normalize_name(&name),
        clean_name,
        year: year_of(s.year.as_deref()).or(title_year),
        poster: non_empty(s.stream_icon.as_deref()),
        rating: rating_of(s.rating),
        stream: StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: id, ext: non_empty(s.container_extension.as_deref()) },
        url_key: None,
        provider_order: order,
        added_at: positive(s.added),
        groups: xt_group_keys(ItemKind::Movie, &s.category_id, &s.category_ids, cats),
        adult: s.is_adult,
        name,
    })
}

pub fn xt_series_to_series(s: &XtSeries, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportSeries> {
    let id = positive(s.series_id)?;
    let name = xt_name(&s.name, || format!("Series {id}"));
    let (cleaned, title_year) = clean_vod_name(&name);
    let clean_name = if smart { cleaned } else { name.clone() };
    Some(ImportSeries {
        key: identity::xtream_key(ItemKind::Series, id),
        provider_id: Some(id.to_string()),
        norm_name: normalize_name(&name),
        clean_name,
        year: year_of(s.release_date.as_deref()).or(title_year),
        cover: non_empty(s.cover.as_deref()),
        backdrop: s.backdrop_path.iter().find_map(|b| non_empty(Some(b))),
        rating: rating_of(s.rating),
        plot: non_empty(s.plot.as_deref()),
        genre: non_empty(s.genre.as_deref()),
        cast: non_empty(s.cast.as_deref()),
        director: non_empty(s.director.as_deref()),
        release_date: non_empty(s.release_date.as_deref()),
        last_modified: positive(s.last_modified),
        provider_order: order,
        added_at: positive(s.last_modified),
        groups: xt_group_keys(ItemKind::Series, &s.category_id, &s.category_ids, cats),
        adult: s.is_adult,
        name,
    })
}

pub fn xt_categories_to_groups(kind: crate::model::ItemKind, cats: &[XtCategory], smart: bool) -> Vec<ImportGroup> {
    let mut out: Vec<ImportGroup> = Vec::with_capacity(cats.len());
    let mut seen: HashSet<crate::model::GroupKey> = HashSet::with_capacity(cats.len());
    for c in cats {
        let Some(id) = non_empty(c.category_id.as_deref()) else { continue };
        let raw = c.category_name.as_deref().unwrap_or("");
        let key = group_key(kind, raw);
        if !seen.insert(key.clone()) {
            continue;
        }
        let provider_order = i32::try_from(out.len()).unwrap_or(i32::MAX);
        out.push(ImportGroup {
            kind,
            key,
            name: group_display(raw, smart),
            raw_name: non_empty(Some(raw)),
            provider_id: Some(id),
            provider_order,
            adult: c.is_adult,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GroupKey, Quality};

    fn entry(title: &str, tvg: Option<&str>, groups: &[&str], url: &str) -> M3uEntry {
        M3uEntry {
            title: title.into(),
            tvg_id: tvg.map(str::to_owned),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            url: url.into(),
            ..Default::default()
        }
    }

    /// A CDN path shaped like a stream URL must not turn its segments into secrets (they would
    /// be masked in every log line); an account's login is on many entries.
    #[test]
    fn a_playlist_login_counts_once_two_streams_carry_it() {
        let mut logins = PlaylistLogins::default();
        logins.note("https://cdn.example/live/channel-9d2/hd1-9d2/1080.m3u8");
        logins.note("https://cdn.example/live/channel-9d2/hd1-9d2/1080.m3u8");
        logins.note("http://panel.example/live/pl-user-71c/pl-pass-71c/11.ts");
        assert!(logins.found.is_empty(), "one stream each");
        logins.note("http://panel.example/pre/movie/pl-user-71c/pl-pass-71c/12.mkv");
        assert_eq!(logins.found, [("pl-user-71c".to_owned(), "pl-pass-71c".to_owned())]);
        assert_eq!(crate::redact::redact("http://cdn.example/pre/pl-user-71c/pl-pass-71c/11.ts"), "http://cdn.example/pre/***/***/11.ts");
        assert_eq!(crate::redact::redact("channel-9d2 hd1-9d2"), "channel-9d2 hd1-9d2");
    }

    #[test]
    fn m3u_entries_convert_with_stable_keys_and_groups() {
        let mut keys = KeyAllocator::default();
        let mut groups = Vec::new();
        let e = M3uEntry {
            tvg_logo: Some("http://l/x.png".into()),
            tvg_chno: Some(101),
            tvg_shift_h: Some(1.5),
            drm: true,
            ..entry("UK | BBC One FHD", Some("bbc1.uk"), &["UK | News", "Favourites"], "http://h/u/p/1.ts?token=x")
        };
        let a = m3u_entry_to_channel(&e, 0, true, &mut keys, &mut groups);
        assert_eq!(a.name, "UK | BBC One FHD");
        assert_eq!(a.clean_name, "BBC One");
        assert_eq!(a.norm_name, "bbcone");
        assert_eq!(a.quality, Some(NameQuality { class: Quality::Fhd, family: "bbcone".into() }));
        assert_eq!(a.number, Some(101));
        assert_eq!(a.tvg_shift_min, 90);
        assert_eq!(a.unsupported, Some(Unsupported::Drm));
        assert_eq!(a.url_key.as_deref(), Some("http://h/1.ts"));
        assert_eq!(a.groups, vec![GroupKey("channel:uknews".into()), GroupKey("channel:favourites".into())]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "News");
        assert_eq!(groups[1].provider_order, 1);
        // the URL is not part of the key; a duplicate in the same import gets `#2`
        let b =
            m3u_entry_to_channel(&entry("UK | BBC One FHD", Some("bbc1.uk"), &["UK | News"], "http://other/2.ts"), 1, false, &mut keys, &mut groups);
        assert_eq!(b.key.0, format!("{}#2", a.key.0));
        assert_eq!(b.clean_name, "UK | BBC One FHD", "smart names off keeps the raw name");
        assert_eq!(b.quality, a.quality, "the quality word counts with smart names off too");
        assert_eq!(groups.len(), 2, "known groups are not repeated");
        // no group → uncategorized
        let c = m3u_entry_to_channel(&entry("Bare", None, &[], "http://h/bare.ts"), 2, true, &mut keys, &mut groups);
        assert_eq!(c.groups, vec![GroupKey("channel:uncategorized".into())]);
        assert_eq!(groups.last().map(|g| g.name.as_str()), Some("Uncategorized"));
        // same inputs → same key in a later import
        let mut keys2 = KeyAllocator::default();
        assert_eq!(m3u_entry_to_channel(&e, 0, false, &mut keys2, &mut Vec::new()).key, a.key);
    }

    #[test]
    fn m3u_entry_without_title_is_named_after_its_url() {
        let ch = m3u_entry_to_channel(&entry("  ", None, &[], "http://h/path/stream.ts"), 0, true, &mut KeyAllocator::default(), &mut Vec::new());
        assert_eq!(ch.name, "stream.ts");
    }

    fn cats(json: &str) -> Vec<XtCategory> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn xtream_categories_dedupe_by_name_key() {
        let c = cats(
            r#"[{"category_id":"1","category_name":"UK | NEWS"},{"category_id":"2","category_name":"uk|news"},
                {"category_id":"3","category_name":""},{"category_name":"no id"}]"#,
        );
        let g = xt_categories_to_groups(ItemKind::Channel, &c, true);
        assert_eq!(g.iter().map(|g| g.key.0.as_str()).collect::<Vec<_>>(), ["channel:uknews", "channel:uncategorized"]);
        assert_eq!(g[0].name, "News");
        assert_eq!(g[0].provider_id.as_deref(), Some("1"));
        assert_eq!(g[1].name, "Uncategorized");
        assert_eq!(g[1].provider_order, 1);
        assert_eq!(xt_categories_to_groups(ItemKind::Movie, &c, false)[0].name, "UK | NEWS");
    }

    #[test]
    fn xtream_live_stream_converts() {
        let c = cats(r#"[{"category_id":"1","category_name":"UK | News"},{"category_id":2,"category_name":"Sports"}]"#);
        let s: Vec<XtLiveStream> = serde_json::from_str(
            r#"[{"num":1,"name":"UK: BBC One HD","stream_id":"1001","stream_icon":"http://l/b.png","epg_channel_id":"bbc1.uk",
                 "added":"1700000000","category_id":"1","tv_archive":1,"tv_archive_duration":"7"},
                {"name":"ESPN","stream_id":1002,"category_id":2,"category_ids":[2,1],"tv_archive":"0"},
                {"name":"Lost","stream_id":1003,"category_id":"99"},
                {"name":"no id","stream_id":""}]"#,
        )
        .unwrap();
        let a = xt_live_to_channel(&s[0], 0, true, &c).unwrap();
        assert_eq!(a.key.0, "xt:live:1001");
        assert_eq!(a.provider_id.as_deref(), Some("1001"));
        assert_eq!(a.clean_name, "BBC One");
        assert_eq!(a.quality.map(|q| q.class), Some(Quality::Hd));
        assert_eq!(a.number, Some(1));
        assert_eq!(a.tvg_id.as_deref(), Some("bbc1.uk"));
        assert_eq!(a.added_at, Some(1_700_000_000));
        assert_eq!(a.stream, StreamLocator::Xtream { kind: XtreamStreamKind::Live, stream_id: 1001, ext: None });
        assert_eq!(a.catchup, Some(CatchupSpec { kind: CatchupKind::Xc, template: None, days: 7, correction_s: 0 }));
        assert_eq!(a.groups, vec![GroupKey("channel:uknews".into())]);
        let b = xt_live_to_channel(&s[1], 1, true, &c).unwrap();
        assert_eq!(b.quality, None);
        assert_eq!(b.catchup, None);
        assert_eq!(b.groups, vec![GroupKey("channel:sports".into()), GroupKey("channel:uknews".into())]);
        assert_eq!(xt_live_to_channel(&s[2], 2, true, &c).unwrap().groups, vec![GroupKey("channel:uncategorized".into())]);
        assert!(xt_live_to_channel(&s[3], 3, true, &c).is_none());

        let (groups, rows) =
            convert_xt(ItemKind::Channel, &c, &s, true, xt_live_to_channel, |s: &XtLiveStream| ids_of(&s.category_id, &s.category_ids));
        assert_eq!(rows.len(), 3);
        assert_eq!(groups.last().map(|g| g.key.0.as_str()), Some("channel:uncategorized"), "added for the lost stream");
    }

    /// `is_adult` on categories and on live / movie / series streams reaches the import rows
    /// the store classifies with (DESIGN §20.3); missing means not adult.
    #[test]
    fn xtream_adult_flags_reach_the_import_rows() {
        let c = cats(
            r#"[{"category_id":"1","category_name":"Hot","is_adult":"1"},{"category_id":"2","category_name":"News","is_adult":0},{"category_id":"3","category_name":"Kids"}]"#,
        );
        let groups = xt_categories_to_groups(ItemKind::Channel, &c, true);
        assert_eq!(groups.iter().map(|g| g.adult).collect::<Vec<_>>(), [true, false, false]);
        let live: Vec<XtLiveStream> =
            serde_json::from_str(r#"[{"stream_id":5,"name":"A","category_id":"2","is_adult":"1"},{"stream_id":6,"name":"B","category_id":"2"}]"#)
                .unwrap();
        assert_eq!(live.iter().map(|s| xt_live_to_channel(s, 0, true, &c).unwrap().adult).collect::<Vec<_>>(), [true, false]);
        let m: XtVodStream = serde_json::from_str(r#"{"stream_id":7,"name":"M","category_id":"2","is_adult":1}"#).unwrap();
        assert!(xt_vod_to_movie(&m, 0, true, &c).unwrap().adult);
        let x: XtSeries = serde_json::from_str(r#"{"series_id":8,"name":"S","category_id":"2","is_adult":true}"#).unwrap();
        assert!(xt_series_to_series(&x, 0, true, &c).unwrap().adult);
    }

    #[test]
    fn xtream_vod_and_series_convert() {
        let c = cats(r#"[{"category_id":"10","category_name":"Movies | Action"}]"#);
        let m: XtVodStream = serde_json::from_str(
            r#"{"name":"The Matrix (1999) [MULTI-SUB]","stream_id":"5001","stream_icon":"http://i/m.jpg","rating":"8.7",
                "added":"1700000200","category_id":"10","container_extension":"mkv"}"#,
        )
        .unwrap();
        let mv = xt_vod_to_movie(&m, 0, true, &c).unwrap();
        assert_eq!(mv.key.0, "xt:vod:5001");
        assert_eq!(mv.clean_name, "The Matrix");
        assert_eq!(mv.year, Some(1999));
        assert_eq!(mv.rating, Some(8.7));
        assert_eq!(mv.stream, StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 5001, ext: Some("mkv".into()) });
        assert_eq!(mv.groups, vec![GroupKey("movie:moviesaction".into())]);
        let unrated: XtVodStream = serde_json::from_str(r#"{"name":"Plain","stream_id":5002,"rating":""}"#).unwrap();
        assert_eq!(xt_vod_to_movie(&unrated, 1, true, &c).unwrap().rating, None);

        let s: XtSeries = serde_json::from_str(
            r#"{"name":"Example Show (2020)","series_id":"700","cover":"http://i/s.jpg","plot":"","genre":"Drama",
                "releaseDate":"2020-01-01","last_modified":"1700000400","rating":"7","backdrop_path":"http://i/b.jpg","category_id":"20"}"#,
        )
        .unwrap();
        let se = xt_series_to_series(&s, 0, true, &[]).unwrap();
        assert_eq!(se.key.0, "xt:series:700");
        assert_eq!(se.clean_name, "Example Show");
        assert_eq!(se.year, Some(2020));
        assert_eq!(se.plot, None);
        assert_eq!(se.backdrop.as_deref(), Some("http://i/b.jpg"));
        assert_eq!(se.last_modified, Some(1_700_000_400));
        assert_eq!(se.groups, vec![GroupKey("series:uncategorized".into())]);
    }

    #[test]
    fn list_errors_map_auth_statuses() {
        assert!(matches!(list_error(Error::Http { status: 401, url: String::new() }), Error::AuthRejected));
        let e = list_error(Error::Http { status: 403, url: String::new() });
        assert!(e.to_string().contains("HTTP 403"), "{e}");
        assert!(!is_fatal(&e));
        assert!(matches!(list_error(Error::Http { status: 500, url: String::new() }), Error::Http { status: 500, .. }));
        assert!(is_fatal(&Error::AuthRejected) && is_fatal(&Error::Cancelled));
        assert!(!is_fatal(&Error::BadPayload("x".into())));
    }

    #[test]
    fn rows_split_into_writer_batches() {
        let ch = m3u_entry_to_channel(&entry("A", None, &[], "http://h/a.ts"), 0, false, &mut KeyAllocator::default(), &mut Vec::new());
        let rows = Rows::Channels(vec![ch; BATCH * 2 + 5]);
        let chunks = rows.into_chunks();
        assert_eq!(chunks.iter().map(Rows::len).collect::<Vec<_>>(), [BATCH, BATCH, 5]);
    }
}
