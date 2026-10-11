//! XMLTV import pipeline. Owner: WP-06. DESIGN §9.3.
//!
//! download (conditional, to `~/.cache/tuner/epg/<id>.xml[.gz]`) → `open_decompressed` →
//! `xmltv::parse` on a blocking thread feeding a bounded channel (cap 4 batches) →
//! async consumer submits `write_epg_channels` / `write_programmes` batches of 5 000 to the
//! writer → re-match this EPG source against all playlist sources (`epg_match::match_all` +
//! `replace_auto_mappings`) → `finish_epg_import` (flip + `resolve_mappings`) →
//! `sweep_old_generations` in chunks → FTS compaction (`merge_fts_step` until done) →
//! `DataScope::Epg`.
//! Back-pressure: the parser blocks when the channel is full, so memory stays bounded
//! (~4 × 5 000 programmes) regardless of file size. Target: 415k programmes < 8 s total.
//!
//! Details fixed by the implementation:
//! * Automatic mappings of the new guide are replaced BEFORE `finish_epg_import`, whose
//!   `resolve_mappings` then applies them in the same short flip transaction (one resolve pass,
//!   and the returned `mapped_channels` already counts them). The matcher sees the new
//!   generation's channels (`store::epg::epg_channels` includes an import in progress).
//! * Any failure or cancellation after `begin_epg_import` and BEFORE the flip runs
//!   `abort_epg_import`: the previous guide stays active. If the automatic mappings were already
//!   replaced from the aborted file, they are matched again against the active guide (best
//!   effort, not cancellable). Failures (not cancellations) are recorded in `last_error`.
//! * Once `finish_epg_import` committed, the new guide is live and is never aborted: the sweep of
//!   the old generations is best effort (a cancellation or error defers it to the next import or
//!   `Maintenance`, which sweep the rest), and the job reports success.
//! * The downloaded file is deleted after the import (its URL may carry credentials, and the
//!   ETag kept in the DB makes the next download conditional). Validators are stored only after
//!   a successful import, so a failed one is retried in full.
//! * Programmes ending before `now − max(epg_keep_past_days, longest catch-up window)` and
//!   starting after `now + 14 d` are skipped while parsing.
//! * Matching: `provider_guide` (Xtream `epg_channel_id` tagging) applies to the guide's own
//!   playlist source; name matching follows `config.epg.auto_name_match`.

use std::ops::ControlFlow;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobPhase, Progress, blocking, check, notify, read, write};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{EpgMapping, EpgSource, EpgSourceId, EpgSourceKind, HttpHeaders, SourceId};
use crate::msg::DataScope;
use crate::sources::epg_match;
use crate::sources::http::{DownloadOutcome, DownloadRequest, open_decompressed};
use crate::sources::xmltv::{self, XmltvItem, XmltvOptions};
use crate::store::epg::{self as store_epg, EpgImport, EpgImportSummary, ImportEpgChannel, ImportProgramme};

/// Programmes (or channels) per writer transaction (DESIGN §5.3).
const BATCH: usize = 5_000;
/// Batches in flight between the parser thread and the writer loop.
const CHANNEL_CAP: usize = 4;
/// Programmes deleted per sweep transaction.
const SWEEP_CHUNK: usize = 20_000;
/// Guide data further ahead than this is skipped.
const MAX_AHEAD_S: i64 = 14 * 86_400;

pub struct EpgRefreshResult {
    pub summary: EpgImportSummary,
    pub not_modified: bool,
}

pub async fn refresh_epg(
    ctx: &JobContext,
    id: EpgSourceId,
    force: bool,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<EpgRefreshResult> {
    progress.mark_inputs_read();
    let src = read(ctx, move |c| crate::store::sources::get_epg_source(c, id)).await?;
    progress.set_subject(&src.name);
    let result = run(ctx, &src, force, progress, cancel).await;
    match &result {
        Ok(_) | Err(Error::Cancelled) => {}
        Err(e) => {
            let msg = e.to_string();
            let now = crate::model::now();
            if let Err(e2) = write(ctx, move |c| crate::store::sources::mark_epg_failed(c, id, &msg, now)).await {
                tracing::warn!("recording the guide failure: {e2}");
            }
        }
    }
    result
}

/// Deletes a downloaded guide when dropped.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(&p);
        }
    }
}

async fn run(ctx: &JobContext, src: &EpgSource, force: bool, progress: &Progress, cancel: &CancellationToken) -> Result<EpgRefreshResult> {
    let cfg = crate::config::current(&ctx.config);
    let id = src.id;
    let (path, _guard, validators) = match &src.kind {
        EpgSourceKind::File { path } => (path.clone(), TempFile(None), None),
        EpgSourceKind::Url { url } | EpgSourceKind::Provider { url, .. } => {
            progress.set(JobPhase::Connecting);
            let req = DownloadRequest {
                url: url.clone(),
                dest: ctx.paths.epg_cache().join(format!("{}.xml", id.0)),
                headers: guide_headers(ctx, src, &cfg).await,
                etag: if force { None } else { src.etag.clone() },
                last_modified: if force { None } else { src.last_modified.clone() },
            };
            let p = progress.clone();
            match ctx.http.download(&req, &move |done, total| p.set(JobPhase::Downloading { done, total }), cancel).await? {
                DownloadOutcome::NotModified => {
                    let now = crate::model::now();
                    write(ctx, move |c| super::db::mark_epg_not_modified(c, id, now)).await?;
                    let summary =
                        EpgImportSummary { channels: src.channel_count, programmes: src.programme_count, max_stop: src.max_stop, mapped_channels: 0 };
                    return Ok(EpgRefreshResult { summary, not_modified: true });
                }
                DownloadOutcome::Downloaded { path, etag, last_modified, .. } => (path.clone(), TempFile(Some(path)), Some((etag, last_modified))),
            }
        }
    };
    check(cancel)?;

    let imp = write(ctx, move |c| store_epg::begin_epg_import(c, id)).await?;
    let abort_imp = EpgImport { channel_ids: Default::default(), ..imp.clone() };
    let now = crate::model::now();
    let mut remapped = false;
    let flipped = async {
        let keep_days = read(ctx, super::db::max_catchup_days).await?.max(cfg.general.epg_keep_past_days);
        let opts = XmltvOptions {
            preferred_langs: preferred_langs(&cfg),
            min_stop: Some(now - i64::from(keep_days) * 86_400),
            max_start: Some(now + MAX_AHEAD_S),
        };
        let imp = write_guide(ctx, src, path, imp, opts, progress, cancel).await?;
        progress.set(JobPhase::Matching);
        remapped = true;
        rematch_source(ctx, id, provider_of(src), cfg.epg.auto_name_match, cancel).await?;
        progress.set(JobPhase::Finishing);
        write(ctx, move |c| store_epg::finish_epg_import(c, &imp, now)).await
    };
    match flipped.await {
        Ok(summary) => {
            // the new guide is live from here on: nothing below may undo it
            if let Err(e) = sweep_old(ctx, id, cancel).await {
                tracing::info!(target: "tuner::jobs", "{}: removing the previous guide data was deferred ({e})", src.name);
            }
            if let Some((etag, lm)) = validators
                && let Err(e) = write(ctx, move |c| super::db::set_epg_validators(c, id, etag.as_deref(), lm.as_deref())).await
            {
                tracing::warn!("storing the guide's download validators: {e}");
            }
            notify(ctx, DataScope::Epg);
            super::after_guide(ctx, crate::follow::FollowReason::Guide);
            Ok(EpgRefreshResult { summary, not_modified: false })
        }
        Err(e) => {
            if let Err(e2) = write(ctx, move |c| store_epg::abort_epg_import(c, &abort_imp)).await {
                tracing::warn!("discarding the partial guide import: {e2}");
            }
            if remapped {
                restore_mappings(ctx, src, &cfg).await;
            }
            Err(e)
        }
    }
}

/// After an abort: automatic mappings were rebuilt from the discarded file; match the active
/// guide again (not cancellable — the job is already winding down) and resolve.
async fn restore_mappings(ctx: &JobContext, src: &EpgSource, cfg: &Config) {
    let restored = async {
        rematch_source(ctx, src.id, provider_of(src), cfg.epg.auto_name_match, &CancellationToken::new()).await?;
        write(ctx, |c| store_epg::resolve_mappings(c)).await
    };
    if let Err(e) = restored.await {
        tracing::warn!("{}: restoring the guide channel matches failed ({e}); use “Match again” in the guide settings", src.name);
    }
}

/// The playlist source whose provider guide `src` is.
fn provider_of(src: &EpgSource) -> Option<SourceId> {
    match &src.kind {
        EpgSourceKind::Provider { source_id, .. } => Some(*source_id),
        _ => None,
    }
}

/// User-Agent of the guide's playlist source (provider guides), else the configured default.
async fn guide_headers(ctx: &JobContext, src: &EpgSource, cfg: &Config) -> HttpHeaders {
    let mut ua = None;
    if let EpgSourceKind::Provider { source_id, .. } = &src.kind {
        let sid = *source_id;
        ua = read(ctx, move |c| Ok(crate::store::sources::get_source(c, sid)?.settings.user_agent)).await.ok().flatten();
    }
    HttpHeaders { user_agent: Some(ua.filter(|u| !u.trim().is_empty()).unwrap_or_else(|| cfg.player.user_agent.clone())), ..Default::default() }
}

enum EpgBatch {
    Channels(Vec<ImportEpgChannel>),
    Programmes(Vec<ImportProgramme>),
}

/// Parse the file and write its channels and programmes into the new generation (not active
/// yet). On error the caller aborts the import.
async fn write_guide(
    ctx: &JobContext,
    src: &EpgSource,
    path: PathBuf,
    mut imp: EpgImport,
    opts: XmltvOptions,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<EpgImport> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<EpgBatch>(CHANNEL_CAP);
    let parse_cancel = cancel.clone();
    let parser = tokio::task::spawn_blocking(move || parse_file(&path, &opts, &tx, &parse_cancel));
    let mut written = 0u64;
    while let Some(batch) = rx.recv().await {
        check(cancel)?;
        let (back, n) = write(ctx, move |c| {
            let n = match &batch {
                EpgBatch::Channels(chs) => {
                    store_epg::write_epg_channels(c, &mut imp, chs)?;
                    0
                }
                EpgBatch::Programmes(ps) => store_epg::write_programmes(c, &mut imp, ps)?,
            };
            Ok((imp, n))
        })
        .await?;
        imp = back;
        written += n as u64;
        progress.set(JobPhase::Writing { done: written, total: None });
    }
    let stats = parser.await.map_err(|e| Error::Other(format!("guide parser failed: {e}")))??;
    check(cancel)?;
    if stats.skipped > 0 {
        tracing::info!("{}: skipped {} unusable programmes", src.name, stats.skipped);
    }
    Ok(imp)
}

/// Blocking: parse into batches. Channels are flushed before the first following programme.
fn parse_file(
    path: &std::path::Path,
    opts: &XmltvOptions,
    tx: &tokio::sync::mpsc::Sender<EpgBatch>,
    cancel: &CancellationToken,
) -> Result<xmltv::XmltvStats> {
    let reader = open_decompressed(path)?;
    let mut channels: Vec<ImportEpgChannel> = Vec::new();
    let mut programmes: Vec<ImportProgramme> = Vec::with_capacity(BATCH);
    let mut gone = false;
    let mut sink = |item: XmltvItem| {
        let full = match item {
            XmltvItem::Channel(c) => {
                channels.push(c);
                (channels.len() >= BATCH).then(|| EpgBatch::Channels(std::mem::take(&mut channels)))
            }
            XmltvItem::Programme(p) => {
                if !channels.is_empty() && tx.blocking_send(EpgBatch::Channels(std::mem::take(&mut channels))).is_err() {
                    gone = true;
                    return ControlFlow::Break(());
                }
                programmes.push(p);
                (programmes.len() >= BATCH).then(|| EpgBatch::Programmes(std::mem::replace(&mut programmes, Vec::with_capacity(BATCH))))
            }
        };
        if let Some(b) = full
            && tx.blocking_send(b).is_err()
        {
            gone = true;
            return ControlFlow::Break(());
        }
        if cancel.is_cancelled() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    };
    let stats = xmltv::parse(reader, opts, &mut sink, &mut |_| {});
    let stats = match stats {
        // the writer loop stopped: its error is the one that matters
        Err(_) if gone => return Err(Error::Cancelled),
        r => r?,
    };
    for b in [EpgBatch::Channels(channels), EpgBatch::Programmes(programmes)] {
        let empty = match &b {
            EpgBatch::Channels(v) => v.is_empty(),
            EpgBatch::Programmes(v) => v.is_empty(),
        };
        if !empty {
            tx.blocking_send(b).map_err(|_| Error::Cancelled)?;
        }
    }
    Ok(stats)
}

/// ISO-639-1 codes from the audio/subtitle preferences ("eng", "en" → "en").
fn preferred_langs(cfg: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in cfg.player.audio_langs.iter().chain(&cfg.player.sub_langs) {
        let l = l.trim().to_ascii_lowercase();
        if l.len() == 2 && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// Sweep inactive generations of `id` in chunks (one transaction each), then compact the search
/// index if anything was removed.
async fn sweep_old(ctx: &JobContext, id: EpgSourceId, cancel: &CancellationToken) -> Result<()> {
    let mut deleted = 0;
    loop {
        check(cancel)?;
        match write(ctx, move |c| store_epg::sweep_old_generations(c, id, SWEEP_CHUNK)).await? {
            0 => break,
            n => deleted += n,
        }
    }
    if deleted > 0 {
        compact_fts(ctx, cancel).await?;
    }
    Ok(())
}

/// Drop deleted entries from the programme search index in short transactions (sweeps leave
/// them behind: 'deletemerge' is off). A cancellation leaves a partial merge that the next
/// compaction finishes.
pub(super) async fn compact_fts(ctx: &JobContext, cancel: &CancellationToken) -> Result<()> {
    loop {
        check(cancel)?;
        if !write(ctx, store_epg::merge_fts_step).await? {
            return Ok(());
        }
    }
}

/// Match guide `id` against every playlist source and replace its automatic mappings (no
/// resolve). Returns the number of automatic mappings.
async fn rematch_source(
    ctx: &JobContext,
    id: EpgSourceId,
    provider_of: Option<SourceId>,
    name_match: bool,
    cancel: &CancellationToken,
) -> Result<u32> {
    let store = ctx.store.clone();
    let maps: Vec<(SourceId, Vec<EpgMapping>)> = blocking(move || {
        let conn = store.reader()?;
        let idx = epg_match::build_index(id, &store_epg::epg_channels(&conn, id)?);
        let mut out = Vec::new();
        for s in crate::store::sources::list_sources(&conn)? {
            let rows = store_epg::channels_for_matching(&conn, s.id)?;
            out.push((s.id, epg_match::match_all(&idx, &rows, provider_of == Some(s.id), name_match)));
        }
        Ok(out)
    })
    .await?;
    let mut total = 0u32;
    for (source, m) in maps {
        check(cancel)?;
        total += u32::try_from(m.len()).unwrap_or(u32::MAX);
        write(ctx, move |c| store_epg::replace_auto_mappings(c, source, id, &m)).await?;
    }
    Ok(total)
}

/// Re-run automatic matching of every enabled EPG source against every playlist source.
pub async fn rematch_all(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<u32> {
    progress.set(JobPhase::Matching);
    let cfg = crate::config::current(&ctx.config);
    let guides = read(ctx, crate::store::sources::list_epg_sources).await?;
    let mut total = 0;
    for g in guides.iter().filter(|g| g.enabled) {
        total += rematch_source(ctx, g.id, provider_of(g), cfg.epg.auto_name_match, cancel).await?;
    }
    progress.set(JobPhase::Finishing);
    write(ctx, |c| store_epg::resolve_mappings(c)).await?;
    notify(ctx, DataScope::Epg);
    super::after_guide(ctx, crate::follow::FollowReason::Guide);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_langs_keep_two_letter_codes_in_order() {
        let mut cfg = Config::default();
        cfg.player.audio_langs = vec!["eng".into(), "en".into(), "DE".into()];
        cfg.player.sub_langs = vec!["en".into(), "fr".into()];
        assert_eq!(preferred_langs(&cfg), ["en", "de", "fr"]);
    }
}
