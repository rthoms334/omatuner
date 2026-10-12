//! `JobKind::GuideCheck`: the fuzzy-suggestion cache the issue list reads, after a guide or
//! playlist import (DESIGN §31 §4.2, B7). OWNER: G-core.
//!
//! * Rule-row derivation runs in `rematch_all` / the guide import (B7), not here; the probes
//!   ("anything now or later?") are cheap and run with the classification
//!   (`store::guide_fix::facts`).
//! * Debounced [`DEBOUNCE`]: the job waits before reading anything, and requests made meanwhile
//!   coalesce into it (`JobManager::submit` merges into a job that hasn't read its inputs).
//! * The cache is bound to `store::guide_fix::stamp` (guides' generations, playlists'
//!   generations, threshold, rules): a different stamp clears it; otherwise only channels
//!   without cached rows are looked at.
//! * Suggestions for unmapped SHOWN channels, in chunks of [`CHUNK`] on at most [`THREADS`]
//!   threads, cancellable between chunks and paused while a picture is Loading or Zapping
//!   (`player_busy`); each chunk is written in its own short transaction, so a cancelled run
//!   leaves a consistent cache that the next run completes.
//! * Same job lane as `EvaluateFollows` (one at a time). Ends with `DataScope::GuideIssues`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobPhase, Progress, blocking, check, notify, read, write};
use crate::error::Result;
use crate::model::{EpgSuggestion, ItemKind, ItemRef};
use crate::msg::DataScope;
use crate::store::guide_fix::{self as store_gf, GuideOpts};
use crate::store::library::Library;

/// Requests within this window make one check.
pub const DEBOUNCE: Duration = Duration::from_secs(30);
/// Channels per chunk.
pub const CHUNK: usize = 256;
/// Threads the suggestions use (two 4K decodes already take ~1.7 of the M2's cores).
pub const THREADS: usize = 2;
/// Suggestions are cached from this score up (the issue list applies the user's threshold), so
/// most channels get a row and aren't scored again on the next run.
pub const CACHE_FLOOR: f64 = 0.8;
/// How often a paused check looks whether playback settled.
const BUSY_POLL: Duration = Duration::from_millis(500);

/// What one check did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// The stamp had changed: the cache was rebuilt from scratch.
    pub rebuilt: bool,
    /// Channels scored.
    pub scored: usize,
}

/// Run the check (a no-op until `guide_fix::ENABLED`): wait [`DEBOUNCE`], then [`check_now`].
pub async fn run(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    if !crate::guide_fix::ENABLED {
        return Ok(());
    }
    tokio::select! {
        _ = cancel.cancelled() => return Err(crate::error::Error::Cancelled),
        _ = tokio::time::sleep(DEBOUNCE) => {}
    }
    let lib = job_library(ctx).await?;
    check_now(ctx, lib, progress, cancel).await.map(|_| ())
}

/// The visibility snapshot the check works on.
///
/// CROSS-TRACK: jobs have no `AppShared` yet; once `JobContext` carries it, this becomes
/// `ctx.shared.library()` (no second build).
async fn job_library(ctx: &JobContext) -> Result<Arc<Library>> {
    super::maintenance::rebuild_library(ctx).await
}

/// The check without the debounce (tests, and `run` after it).
pub async fn check_now(ctx: &JobContext, lib: Arc<Library>, progress: &Progress, cancel: &CancellationToken) -> Result<CheckReport> {
    progress.mark_inputs_read();
    progress.set(JobPhase::Matching);
    let opts = GuideOpts::from_config(&crate::config::current(&ctx.config));
    let (stamp, cached) = read(ctx, move |c| Ok((store_gf::stamp(c, &opts)?, store_gf::cached_stamp(c)?))).await?;
    let rebuilt = cached != Some(stamp);
    if rebuilt {
        // the stamp goes with the clear: a cancelled run leaves a cache the next one completes
        write(ctx, move |c| {
            store_gf::clear_suggestions(c)?;
            store_gf::set_cached_stamp(c, &stamp)
        })
        .await?;
    }
    let have = read(ctx, store_gf::suggested_keys).await?;
    // unmapped shown channels without cached rows
    let todo: Vec<(ItemRef, String)> = lib
        .live
        .channels
        .iter()
        .filter(|c| c.epg_channel_id.is_none() && !c.unsupported && !lib.live.hidden.contains(&c.id))
        .filter(|c| !have.contains(&(c.source_id, c.key.as_str().to_owned())))
        .map(|c| (ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }, c.name.to_string()))
        .collect();
    drop(lib);
    check(cancel)?;
    let store = ctx.store.clone();
    let catalog = blocking(move || crate::guide_fix::catalog::shared(&store.reader()?)).await?;
    let total = todo.len();
    let mut done = 0usize;
    for chunk in todo.chunks(CHUNK * THREADS) {
        while ctx.player_busy.load(Ordering::Relaxed) {
            check(cancel)?;
            tokio::time::sleep(BUSY_POLL).await;
        }
        check(cancel)?;
        let (cat, n, chunk) = (catalog.clone(), chunk.len(), chunk.to_vec());
        let rows: Vec<(ItemRef, Vec<EpgSuggestion>)> = blocking(move || Ok(score(&cat, &chunk))).await?;
        write(ctx, move |c| store_gf::write_suggestions(c, &rows)).await?;
        done += n;
        progress.set(JobPhase::Writing { done: done as u64, total: Some(total as u64) });
    }
    notify(ctx, DataScope::GuideIssues);
    tracing::info!(target: "tuner::jobs", "guide check: {total} channels scored{}", if rebuilt { " (cache rebuilt)" } else { "" });
    Ok(CheckReport { rebuilt, scored: total })
}

/// Score `chunk` on up to [`THREADS`] threads.
fn score(cat: &crate::guide_fix::catalog::Catalog, chunk: &[(ItemRef, String)]) -> Vec<(ItemRef, Vec<EpgSuggestion>)> {
    let per = chunk.len().div_ceil(THREADS).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = chunk
            .chunks(per)
            .map(|part| {
                s.spawn(move || {
                    part.iter().map(|(r, name)| (r.clone(), cat.suggest(name, store_gf::SUGGESTIONS, CACHE_FLOOR, None))).collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}
