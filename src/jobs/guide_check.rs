//! `JobKind::GuideCheck`: the fuzzy-suggestion cache the issue list reads, after a guide or
//! playlist import (DESIGN §31 §4.2, B7). OWNER: G-core.
//!
//! * Rule-row derivation runs in `rematch_all` / the guide import (B7), not here; the probes
//!   ("anything now or later?") are cheap and run with the classification
//!   (`store::guide_fix::facts`).
//! * Debounced [`DEBOUNCE`] at request time ([`request`]): the job is submitted once the window
//!   has passed (requests meanwhile join the waiting one), so it never holds the "features" lane
//!   or shows in the Jobs overlay while it waits. The app only: a headless `tuner --refresh`
//!   (no `JobContext::shared`) never checks.
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

use super::{JobContext, JobKind, JobManager, JobPhase, Progress, blocking, check, notify, read, write};
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
/// Suggestions are cached from this score up, or from the user's threshold when it is lower
/// (the issue list applies the threshold). Channels with none get a marker row
/// (`store::guide_fix::write_suggestions`), so no channel is scored twice for one stamp.
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

/// Ask for a check in [`DEBOUNCE`] (B7: after a guide import or rematch; a rule change). A
/// request while one waits joins it; headless (no `JobContext::shared`) it does nothing. Called
/// from a job (a tokio context).
pub fn request(ctx: &JobContext) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => request_on(ctx, &rt),
        Err(_) => tracing::debug!(target: "tuner::jobs", "guide check requested outside the runtime: skipped"),
    }
}

/// [`request`] from the UI thread.
pub fn request_from(jobs: &JobManager) {
    request_on(jobs.context(), jobs.runtime())
}

fn request_on(ctx: &JobContext, rt: &tokio::runtime::Handle) {
    if !crate::guide_fix::ENABLED || ctx.shared.is_none() || ctx.guide_check_pending.swap(true, Ordering::SeqCst) {
        return;
    }
    let (submit, pending) = (ctx.submit.clone(), ctx.guide_check_pending.clone());
    rt.spawn(async move {
        tokio::time::sleep(DEBOUNCE).await;
        pending.store(false, Ordering::SeqCst);
        submit(JobKind::GuideCheck);
    });
}

/// A requested check waits out its debounce (the screen says "Checking…" meanwhile).
pub fn pending(ctx: &JobContext) -> bool {
    ctx.guide_check_pending.load(Ordering::SeqCst)
}

/// Run the check (a no-op until `guide_fix::ENABLED`, and headless).
pub async fn run(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    if !crate::guide_fix::ENABLED || ctx.shared.is_none() {
        return Ok(());
    }
    let lib = job_library(ctx).await?;
    check_now(ctx, lib, progress, cancel).await.map(|_| ())
}

/// The visibility snapshot the check works on: the app's published one, else a build
/// (headless, or before the app published its first).
async fn job_library(ctx: &JobContext) -> Result<Arc<Library>> {
    match ctx.published_library() {
        Some(lib) => Ok(lib),
        None => super::maintenance::rebuild_library(ctx).await,
    }
}

/// The check itself (`run`, tests).
pub async fn check_now(ctx: &JobContext, lib: Arc<Library>, progress: &Progress, cancel: &CancellationToken) -> Result<CheckReport> {
    progress.mark_inputs_read();
    progress.set(JobPhase::Matching);
    let opts = GuideOpts::from_config(&crate::config::current(&ctx.config));
    let floor = opts.fuzzy_threshold.min(CACHE_FLOOR);
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
        let rows: Vec<(ItemRef, Vec<EpgSuggestion>)> = blocking(move || Ok(score(&cat, &chunk, floor))).await?;
        write(ctx, move |c| store_gf::write_suggestions(c, &rows)).await?;
        done += n;
        progress.set(JobPhase::Writing { done: done as u64, total: Some(total as u64) });
    }
    notify(ctx, DataScope::GuideIssues);
    tracing::info!(target: "tuner::jobs", "guide check: {total} channels scored{}", if rebuilt { " (cache rebuilt)" } else { "" });
    Ok(CheckReport { rebuilt, scored: total })
}

/// Score `chunk` on up to [`THREADS`] threads.
fn score(cat: &crate::guide_fix::catalog::Catalog, chunk: &[(ItemRef, String)], floor: f64) -> Vec<(ItemRef, Vec<EpgSuggestion>)> {
    let per = chunk.len().div_ceil(THREADS).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = chunk
            .chunks(per)
            .map(|part| {
                s.spawn(move || part.iter().map(|(r, name)| (r.clone(), cat.suggest(name, store_gf::SUGGESTIONS, floor, None))).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::follow::FollowReason;
    use crate::msg::{AppEvent, EventBus};

    /// After a guide import the check is requested, not started: it waits out [`DEBOUNCE`]
    /// outside the jobs (the "features" lane stays free, nothing shows in the Jobs overlay),
    /// repeated requests make one check, and a headless refresh never checks.
    #[test]
    fn a_check_waits_outside_the_jobs_and_never_runs_headless() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manager = |shared: bool| {
            let paths = crate::paths::AppPaths::rooted(dir.path());
            paths.ensure().unwrap();
            let store = crate::store::Store::open(&paths.db_file(), None).unwrap();
            let http = crate::sources::http::HttpClient::new(&Default::default()).unwrap();
            let (bus, events) = EventBus::new(None);
            let mut ctx = JobContext::new(store, http, bus, paths, Arc::default());
            if shared {
                ctx = ctx.with_shared(Arc::default());
            }
            (JobManager::new(ctx, rt.handle().clone()), events)
        };
        let checks = |events: &crossbeam_channel::Receiver<AppEvent>| {
            events.try_iter().filter(|e| matches!(e, AppEvent::Job(crate::jobs::JobEvent::Finished { kind: JobKind::GuideCheck, .. }))).count()
        };
        let wait = |jobs: &JobManager| {
            rt.block_on(async {
                tokio::time::sleep(DEBOUNCE + Duration::from_secs(1)).await;
                while !jobs.active().is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };

        let (headless, events) = manager(false);
        rt.block_on(async { crate::jobs::after_guide(headless.context(), FollowReason::Guide) });
        assert!(!pending(headless.context()));
        wait(&headless);
        assert_eq!(checks(&events), 0, "headless: no check");

        let (app, events) = manager(true);
        rt.block_on(async {
            crate::jobs::after_guide(app.context(), FollowReason::Guide);
            crate::jobs::after_guide(app.context(), FollowReason::Guide);
        });
        assert!(pending(app.context()));
        assert!(app.active().iter().all(|j| j.kind != JobKind::GuideCheck), "nothing runs while it waits");
        wait(&app);
        assert!(!pending(app.context()));
        assert_eq!(checks(&events), 1, "two requests, one check");
    }
}
