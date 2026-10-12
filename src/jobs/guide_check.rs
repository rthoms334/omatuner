//! `JobKind::GuideCheck`: the fuzzy-suggestion cache the issue list reads, after a guide or
//! playlist import (DESIGN §31 §4.2, B7). OWNER: G-core.
//!
//! * Rule-row derivation runs in `rematch_all` / the guide import (B7), not here; the probes
//!   ("anything now or later?") are cheap and run with the classification
//!   (`store::guide_fix::facts`).
//! * Debounced [`DEBOUNCE`] before it is submitted ([`request`]): requests made meanwhile push
//!   the deadline back, so the job is only active (status bar, "features" lane, headless
//!   `wait_for_jobs`) while it works. Headless `tuner --refresh` doesn't check: the app checks
//!   the stamp once it has published its first library ([`after_first_publish`]).
//! * The cache is bound to `store::guide_fix::stamp` (guides' generations, playlists'
//!   generations, threshold, rules): a different stamp clears it; otherwise only channels
//!   without cached rows are looked at.
//! * Suggestions for unmapped SHOWN channels, in chunks of [`CHUNK`] on at most [`THREADS`]
//!   threads, cancellable between chunks and paused while a picture is Loading or Zapping
//!   (`player_busy`); each chunk is written in its own short transaction, so a cancelled run
//!   leaves a consistent cache that the next run completes.
//! * Same job lane as `EvaluateFollows` (one at a time). Ends with `DataScope::GuideIssues`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobKind, JobManager, JobPhase, ManagerInner, Progress, blocking, check, lock, notify, read, write};
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

/// The debounce deadline and whether a check is running (shared by every clone of the context).
#[derive(Debug, Default)]
pub struct CheckState {
    due: Mutex<Option<tokio::time::Instant>>,
    running: AtomicBool,
}

impl CheckState {
    /// A check is working right now (one atomic load a frame).
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// A requested check waits out its debounce.
    pub fn pending(&self) -> bool {
        lock(&self.due).is_some()
    }
}

/// Ask for a check [`DEBOUNCE`] from now (B7: after a guide import or rematch; a rule or
/// mapping change). The first request in a window starts the timer; later ones push the
/// deadline back. Headless (`JobContext::headless`) it does nothing. Called from a job (a tokio
/// context).
pub(crate) fn request(ctx: &JobContext) {
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
    if !crate::guide_fix::ENABLED || ctx.headless {
        return;
    }
    let state = ctx.guide_check.clone();
    let first = lock(&state.due).replace(tokio::time::Instant::now() + DEBOUNCE).is_none();
    if !first {
        return;
    }
    let submit = ctx.submit.clone();
    rt.spawn(async move {
        loop {
            let at = {
                let mut due = lock(&state.due);
                match *due {
                    Some(at) if at <= tokio::time::Instant::now() => {
                        due.take();
                        break;
                    }
                    Some(at) => at,
                    None => return,
                }
            };
            tokio::time::sleep_until(at).await;
        }
        submit(JobKind::GuideCheck);
    });
}

/// GUI start: once the app has published its first library, check if the cache's stamp is
/// out of date (a headless refresh imported a guide without checking). Holds only a weak
/// reference, so it never keeps the manager alive.
pub(super) async fn after_first_publish(inner: Weak<ManagerInner>) {
    const POLL: Duration = Duration::from_secs(1);
    loop {
        tokio::time::sleep(POLL).await;
        let Some(inner) = inner.upgrade() else { return };
        if inner.closing.load(Ordering::SeqCst) {
            return;
        }
        if inner.ctx.published_library().is_some() {
            let ctx = inner.ctx.clone();
            drop(inner);
            let opts = GuideOpts::from_config(&crate::config::current(&ctx.config));
            match read(&ctx, move |c| Ok(store_gf::cached_stamp(c)? != Some(store_gf::stamp(c, &opts)?))).await {
                Ok(true) => request(&ctx),
                Ok(false) => {}
                Err(e) => tracing::warn!(target: "tuner::jobs", "guide check stamp: {e}"),
            }
            return;
        }
    }
}

/// Run the check (a no-op until `guide_fix::ENABLED`, and headless); [`request`] already
/// debounced it.
pub async fn run(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    if !crate::guide_fix::ENABLED || ctx.headless {
        return Ok(());
    }
    struct Running<'a>(&'a AtomicBool);
    impl Drop for Running<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    ctx.guide_check.running.store(true, Ordering::Relaxed);
    let _running = Running(&ctx.guide_check.running);
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

/// The check itself (tests, and `run`).
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
