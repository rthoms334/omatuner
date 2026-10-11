//! Library rebuild, pruning, backup/restore jobs. Owner: WP-06.
//!
//! Details fixed by the implementation:
//! * Maintenance prunes in chunks of 20 000 programmes (one writer transaction each, FTS rows
//!   first — `store::epg::prune`), sweeps inactive EPG generations, runs `fts_cleanup`, trims the
//!   image cache to 500 MiB, then `PRAGMA optimize` + `wal_checkpoint(TRUNCATE)` (DESIGN §7.4;
//!   readers never keep statements open, and the busy timeout bounds the wait — a checkpoint
//!   that could not finish is logged, not an error). The finish time is kept in `meta` for the
//!   scheduler. History is already capped at 500 rows by `store::user::push_history`.
//! * A backup export includes `config.toml` when it exists; a restore registers the restored
//!   secrets with the redactor, emits `DataScope::All` and submits `RefreshSource` for every
//!   enabled source (restored sources that need credentials are disabled by the store). The
//!   backup's settings come back in `RestoreReport::settings` only when the request asks for
//!   them (machine-specific ones and an existing PIN kept, `Config::restored`); the app applies
//!   them like a Settings change.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobKind, JobPhase, Progress, blocking, check, notify, read, write};
use crate::error::Result;
use crate::msg::DataScope;
use crate::store::backup::{BackupOptions, RestoreMode, RestoreReport};
use crate::store::library::Library;

/// Programme rows deleted per maintenance transaction.
const PRUNE_CHUNK: usize = 20_000;
/// Image cache budget on disk (DESIGN §7.4).
const IMAGE_CACHE_BYTES: u64 = 500 * 1024 * 1024;

/// Build a new `Library` from a fresh reader connection on a blocking thread, with the
/// current config's options (the audio languages order the copies of a stack).
pub async fn rebuild_library(ctx: &JobContext) -> Result<Arc<Library>> {
    // visibility read at the start: the app rebuilds again when its parental state changed meanwhile
    let opts = crate::store::library::LoadOptions::from_config(&crate::config::current(&ctx.config), ctx.explicit.visibility());
    read(ctx, move |c| crate::store::library::load_with(c, &opts)).await.map(Arc::new)
}

/// Prune programmes older than max(keep_past_days, max catch-up days), sweep inactive EPG
/// generations, compact the guide search index, prune history, `PRAGMA optimize`,
/// `wal_checkpoint(TRUNCATE)`.
/// Each feature owner's pruning (DESIGN §2.3 of the batch plan): Up next played rows 7 d, the
/// Following ledger 30 d (show rules 2 y), spoiler-guard seen rows 30 d after the window, the
/// guide-fix log 90 d / 200 batches. One transaction.
pub fn prune_features(conn: &rusqlite::Connection, now: crate::model::Timestamp) -> Result<usize> {
    Ok(crate::store::queue::prune(conn, now)?
        + crate::store::follow::prune(conn, now)?
        + crate::store::spoiler::prune(conn, now)?
        + crate::store::guide_fix::prune(conn, now)?)
}

pub async fn maintenance(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    let cfg = crate::config::current(&ctx.config);
    let now = crate::model::now();
    let keep_days = read(ctx, super::db::max_catchup_days).await?.max(cfg.general.epg_keep_past_days);
    let before = now - i64::from(keep_days) * 86_400;

    let mut done = 0u64;
    progress.set(JobPhase::Writing { done, total: None });
    loop {
        check(cancel)?;
        let n = write(ctx, move |c| crate::store::epg::prune(c, before, PRUNE_CHUNK)).await?;
        if n == 0 {
            break;
        }
        done += n as u64;
        progress.set(JobPhase::Writing { done, total: None });
    }

    for g in read(ctx, crate::store::sources::list_epg_sources).await? {
        loop {
            check(cancel)?;
            let id = g.id;
            if write(ctx, move |c| crate::store::epg::sweep_old_generations(c, id, PRUNE_CHUNK)).await? == 0 {
                break;
            }
        }
    }
    // also catches deletions made elsewhere (prunes, removed sources, deferred sweeps);
    // a single no-op step when the index is already compact
    super::import_epg::compact_fts(ctx, cancel).await?;

    let pruned = write(ctx, move |c| prune_features(c, now)).await?;
    if pruned > 0 {
        tracing::info!("maintenance pruned {pruned} old Up next / Following / spoiler-guard / guide-fix rows");
    }
    // the daily tick: Following marks aired rows and re-arms reminders (B6)
    super::after_guide_daily(ctx);

    progress.set(JobPhase::Finishing);
    let orphans = write(ctx, |c| crate::store::epg::fts_cleanup(c)).await?;
    if orphans > 0 {
        tracing::warn!("maintenance removed {orphans} orphaned guide search rows");
    }
    let images = ctx.paths.image_cache();
    match blocking(move || crate::images::prune_disk(&images, IMAGE_CACHE_BYTES)).await {
        Ok(0) => {}
        Ok(freed) => tracing::info!("image cache trimmed by {} MiB", freed / (1024 * 1024)),
        Err(e) => tracing::warn!("trimming the image cache: {e}"),
    }
    check(cancel)?;
    let busy = write(ctx, move |c| {
        c.execute_batch("PRAGMA optimize;")?;
        let busy: i64 = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        super::db::set_last_maintenance(c, now)?;
        Ok(busy != 0)
    })
    .await?;
    if busy {
        tracing::info!("maintenance: the WAL checkpoint was cut short by an open reader");
    }
    tracing::info!("maintenance done: {done} old programmes pruned");
    Ok(())
}

pub async fn export_backup(ctx: &JobContext, path: &Path, opts: BackupOptions) -> Result<()> {
    let config = match tokio::fs::read_to_string(ctx.paths.config_file()).await {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(crate::error::Error::io(ctx.paths.config_file(), e)),
    };
    let now = crate::model::now();
    let backup = read(ctx, move |c| crate::store::backup::export(c, opts, config, now)).await?;
    let path = path.to_path_buf();
    blocking(move || crate::store::backup::write_file(&path, &backup)).await
}

/// Restore, then submit `RefreshSource` for every restored source. With `settings`, the
/// backup's settings are returned in the report for the app to apply (`RestoreReport::settings`);
/// without, the current settings stay untouched.
pub async fn import_backup(ctx: &JobContext, path: &Path, mode: RestoreMode, settings: bool) -> Result<RestoreReport> {
    let p = path.to_path_buf();
    let mut backup = blocking(move || crate::store::backup::read_file(&p)).await?;
    let raw_config = backup.config.take().filter(|_| settings);
    let mut report = write(ctx, move |c| crate::store::backup::restore(c, &backup, mode)).await?;
    if let Some(raw) = raw_config {
        // a broken config.toml in the backup doesn't fail the restore of the data
        match crate::config::Config::restored(&raw, &crate::config::current(&ctx.config)) {
            Ok(cfg) => report.settings = Some(Box::new(cfg)),
            Err(e) => tracing::warn!("restore: settings not restored: {e}"),
        }
    }
    let sources = read(ctx, |c| {
        crate::store::sources::register_secrets(c)?;
        crate::store::sources::list_sources(c)
    })
    .await?;
    notify(ctx, DataScope::All);
    for s in sources.iter().filter(|s| s.enabled) {
        (ctx.submit)(JobKind::RefreshSource { source: s.id, force: true });
    }
    Ok(report)
}
