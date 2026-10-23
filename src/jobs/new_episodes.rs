//! `JobKind::CheckNewEpisodes`: the tracked set, fetch the tracked Xtream copies that changed,
//! diff against the ledger, announce (DESIGN §44 §5). OWNER: N-core. Runs in the "features"
//! lane, one at a time with Following's evaluation and the guide check.
//!
//! * **Triggers.** A playlist import calls [`request`] (Xtream once its series finished; M3U
//!   unless the playlist was unchanged); the app's first library asks once ([`CheckReason::
//!   Launch`], picking up a headless `tuner --refresh`); Settings' "Check now" calls
//!   [`check_now`] (at most every [`CHECK_NOW_EVERY`]). Requests are debounced [`DEBOUNCE`] and
//!   the job is submitted only once no playlist refresh or library build is queued or running
//!   and the app published a library built after the last request: stacks decide what a title
//!   is, and `RefreshStale`'s wave of refreshes makes one check and one round of
//!   notifications. Headless it does nothing, like the guide check.
//! * **Tracking** ([`track_from`]): watchlist and progress changes re-track [`TRACK_DEBOUNCE`]
//!   later, off the UI thread and without the network: titles that became tracked are
//!   baselined before the refresh that brings their next episode (a playlist's import writes
//!   its episodes before any check runs).
//! * **Fetch.** Only tracked Xtream copies, on the refreshed playlists (every playlist for
//!   Launch and Check now): those whose `details_at` is NULL (`last_modified` moved, or never
//!   loaded), then — not at launch — the fallback for providers that never move
//!   `last_modified`: details older than 7 days, every tracked copy on a playlist marked
//!   unreliable; oldest first, at most [`FETCH_CAP`] per playlist per check, serially, paused
//!   while the player is Loading or Zapping. API calls only: no stream, no `SharedBudget`
//!   lease. A failed fetch is retried at the next refresh; a playlist that can't be reached
//!   is skipped for the rest of the check.
//! * **Announce.** One notification per series with Play (the next unwatched episode, Q4) and
//!   Download; four series or more make one summary with Open. Held while Tuner plays a film
//!   or episode full screen and in the news flash's quiet hours, then sent (Home updates at
//!   once). No notification server: `PlatformEvent::NewEpisodesToast`. With notifications off
//!   nothing is sent; the arrivals reach Home all the same.
//!
//! Privacy: INFO logs carry counts and playlist ids only ("newep: 3 arrivals in 2 series
//! (sources 2), 1 quiet; 4 fetched"); no titles, no URLs.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobKind, JobManager, JobPhase, ManagerInner, Progress, check, lock, notify, read, write};
use crate::error::{Error, Result};
use crate::guard::{Field, Guard, Policy, Subject};
use crate::model::{ArrivalId, JobId, SeriesId, SourceId, Timestamp};
use crate::msg::{AppEvent, DataScope, EventBus};
use crate::new_episodes::{CheckReason, Note, NoteAction};
use crate::platform::PlatformEvent;
use crate::store::library::Library;
use crate::store::new_episodes::{self as store_ne, Announcement, CheckInput, FetchCandidate, Opts};

/// Requests within this window make one check.
pub const DEBOUNCE: Duration = Duration::from_secs(30);
/// Watchlist and progress changes within this window make one re-track.
pub const TRACK_DEBOUNCE: Duration = Duration::from_secs(60);
/// Fetches per playlist per check.
pub const FETCH_CAP: usize = 40;
/// "Check now" at most this often (Q11).
pub const CHECK_NOW_EVERY: Duration = Duration::from_secs(10 * 60);
/// More series than this in one check make one summary instead (§3.4).
pub const MAX_SINGLE: usize = 3;
/// How often a held round of notifications looks whether it may go.
const HOLD_POLL: Duration = Duration::from_secs(30);
/// Notifications held longer than this are dropped (Home has them).
const HOLD_MAX: Duration = Duration::from_secs(16 * 3600);
/// How often a debounced request looks whether the refreshes and the library build are done.
const SETTLE_POLL: Duration = Duration::from_secs(2);
/// A request waits at most this long for them.
const SETTLE_MAX: Duration = Duration::from_secs(10 * 60);
/// How often a paused fetch looks whether playback settled.
const BUSY_POLL: Duration = Duration::from_millis(500);

/// The debounces, the held notifications and whether a check runs (shared by every clone of
/// the context).
#[derive(Debug, Default)]
pub struct CheckState {
    pending: Mutex<Pending>,
    track_due: Mutex<Option<tokio::time::Instant>>,
    last_manual: Mutex<Option<Instant>>,
    held: Mutex<Vec<ArrivalId>>,
    /// A task waits to send the held notifications.
    holding: AtomicBool,
    running: AtomicBool,
}

impl CheckState {
    /// A check is working right now (one atomic load a frame).
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// The requests made since the last check was submitted.
#[derive(Debug, Default)]
struct Pending {
    due: Option<tokio::time::Instant>,
    /// The last refresh's request (the published library must be built after it).
    since: Timestamp,
    reason: Option<CheckReason>,
    sources: BTreeSet<SourceId>,
    silent: BTreeSet<SourceId>,
}

// ------------------------------------------------------------------ requests

/// Ask for a check after a playlist import of `source` (`first_import`: its first, always a
/// silent baseline) or for `reason`. Debounced; headless it does nothing. Called from a job (a
/// tokio context).
pub(crate) fn request(ctx: &JobContext, reason: CheckReason, source: Option<SourceId>, first_import: bool) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => request_on(ctx, &rt, reason, source, first_import),
        Err(_) => tracing::debug!(target: "tuner::jobs", "new-episode check requested outside the runtime: skipped"),
    }
}

fn request_on(ctx: &JobContext, rt: &tokio::runtime::Handle, reason: CheckReason, source: Option<SourceId>, first_import: bool) {
    if !crate::new_episodes::ENABLED || ctx.headless {
        return;
    }
    let state = ctx.new_episodes.clone();
    let first = {
        let mut p = lock(&state.pending);
        p.reason = Some(p.reason.map_or(reason, |r| r.max(reason)));
        if let Some(s) = source {
            p.sources.insert(s);
            if first_import {
                p.silent.insert(s);
            }
        }
        // a refresh's check waits for the library its import rebuilds
        if reason == CheckReason::Refresh {
            p.since = crate::model::now();
        }
        p.due.replace(tokio::time::Instant::now() + DEBOUNCE).is_none()
    };
    if first {
        rt.spawn(settle_then_submit(ctx.clone()));
    }
}

/// Wait out the debounce, then the refreshes and the library they rebuild, then submit one
/// check for everything requested meanwhile.
async fn settle_then_submit(ctx: JobContext) {
    let state = ctx.new_episodes.clone();
    loop {
        let at = match lock(&state.pending).due {
            Some(at) if at <= tokio::time::Instant::now() => break,
            Some(at) => at,
            None => return,
        };
        tokio::time::sleep_until(at).await;
    }
    let started = Instant::now();
    loop {
        let busy = (ctx.active)()
            .iter()
            .any(|k| matches!(k, JobKind::RefreshSource { .. } | JobKind::AddSource(_) | JobKind::RefreshStale | JobKind::RebuildLibrary));
        let since = lock(&state.pending).since;
        let stale = ctx.shared.is_some() && ctx.published_library().is_none_or(|l| l.built_at < since);
        if !(busy || stale) || started.elapsed() >= SETTLE_MAX {
            break;
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
    let p = std::mem::take(&mut *lock(&state.pending));
    let kind = JobKind::CheckNewEpisodes {
        reason: p.reason.unwrap_or(CheckReason::Refresh),
        sources: p.sources.into_iter().collect(),
        silent: p.silent.into_iter().collect(),
    };
    (ctx.submit)(kind);
}

/// GUI start: once the app has published its first library, one check picks up what a headless
/// refresh changed. Holds only a weak reference, so it never keeps the manager alive.
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
            request(&ctx, CheckReason::Launch, None, false);
            return;
        }
    }
}

/// Settings ▸ New episodes ▸ Check now (Q11): the same check on the tracked series, without a
/// playlist refresh. `Err(wait)`: the last one was less than [`CHECK_NOW_EVERY`] ago; try again
/// in `wait`. Not offered to agents.
pub fn check_now(jobs: &JobManager) -> std::result::Result<JobId, Duration> {
    let mut last = lock(&jobs.context().new_episodes.last_manual);
    if let Some(at) = *last
        && at.elapsed() < CHECK_NOW_EVERY
    {
        return Err(CHECK_NOW_EVERY - at.elapsed());
    }
    *last = Some(Instant::now());
    drop(last);
    Ok(jobs.submit(JobKind::CheckNewEpisodes { reason: CheckReason::Manual, sources: Vec::new(), silent: Vec::new() }))
}

/// A watchlist or progress change (`DataScope::{Watchlists, Progress}`, from the UI thread):
/// re-track [`TRACK_DEBOUNCE`] later, locally — titles that became tracked are baselined
/// silently, titles no longer tracked lose their ledger. Never alerts, never fetches.
pub fn track_from(jobs: &JobManager) {
    let ctx = jobs.context();
    if !crate::new_episodes::ENABLED || ctx.headless || !crate::config::current(&ctx.config).new_episodes.enabled {
        return;
    }
    let first = lock(&ctx.new_episodes.track_due).replace(tokio::time::Instant::now() + TRACK_DEBOUNCE).is_none();
    if !first {
        return;
    }
    let ctx = ctx.clone();
    jobs.runtime().spawn(async move {
        loop {
            let at = {
                let mut due = lock(&ctx.new_episodes.track_due);
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
        if let Err(e) = track(&ctx).await {
            tracing::warn!(target: "tuner::jobs", "new episodes: tracking: {e}");
        }
    });
}

/// The local re-track (the app's published library only: before it, the launch check does it).
async fn track(ctx: &JobContext) -> Result<()> {
    let Some(lib) = ctx.published_library() else { return Ok(()) };
    let cfg = crate::config::current(&ctx.config);
    if !cfg.new_episodes.enabled {
        return Ok(());
    }
    let opts = Opts::from_config(&cfg.new_episodes, crate::model::now());
    let config = ctx.config.clone();
    let Some(r) = write(ctx, move |c| check_if_on(c, &config, &lib, &opts, &CheckInput::default())).await? else { return Ok(()) };
    if r.baselined > 0 || r.first_run {
        notify(ctx, DataScope::NewEpisodes);
    }
    tracing::debug!(target: "tuner::jobs", "newep: tracking {} series, {} copies baselined", r.tracked, r.baselined);
    Ok(())
}

// ------------------------------------------------------------------ the check

/// [`store_ne::check`] unless the master switch is off by now: read inside the write, so a
/// check (or a re-track) that was fetching when the switch went off never writes a ledger or
/// arrivals after the switch's `forget_ledger`. `None`: switched off, nothing written.
fn check_if_on(
    c: &mut rusqlite::Connection,
    config: &crate::config::SharedConfig,
    lib: &Library,
    opts: &Opts,
    input: &CheckInput,
) -> Result<Option<store_ne::CheckReport>> {
    if !crate::config::current(config).new_episodes.enabled {
        return Ok(None);
    }
    store_ne::check(c, lib, opts, input).map(Some)
}

/// One copy to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fetch {
    pub series: SeriesId,
    pub source: SourceId,
    /// Fetched by the fallback (details old, `last_modified` unchanged), not because
    /// `last_modified` moved.
    pub fallback: bool,
}

/// Which tracked copies to fetch (§5): on `scope`'s playlists (`None`: every playlist), those
/// whose details went stale through `last_modified` (or never loaded) first, then with
/// `fallback` those 7 days old or on an unreliable playlist, oldest first; at most
/// [`FETCH_CAP`] per playlist.
pub(crate) fn plan_fetches(candidates: &[FetchCandidate], scope: Option<&HashSet<SourceId>>, fallback: bool, now: Timestamp) -> Vec<Fetch> {
    /// A playlist's copies whose `last_modified` moved, and the fallback's with their age.
    type Lists = (Vec<Fetch>, Vec<(Timestamp, Fetch)>);
    let mut per_source: BTreeMap<SourceId, Lists> = BTreeMap::new();
    let mut seen = HashSet::new();
    for c in candidates.iter().filter(|c| scope.is_none_or(|s| s.contains(&c.source))) {
        if !seen.insert(c.series) {
            continue;
        }
        let (due, old) = per_source.entry(c.source).or_default();
        match c.details_at {
            None => due.push(Fetch { series: c.series, source: c.source, fallback: false }),
            Some(at) if fallback && (c.unreliable || now - at >= super::details::DETAILS_TTL_S) => {
                old.push((at, Fetch { series: c.series, source: c.source, fallback: true }));
            }
            Some(_) => {}
        }
    }
    let mut out = Vec::new();
    for (_, (due, mut old)) in per_source {
        old.sort_by_key(|(at, f)| (*at, f.series));
        out.extend(due.into_iter().chain(old.into_iter().map(|(_, f)| f)).take(FETCH_CAP));
    }
    out
}

/// Run a check (a no-op until `new_episodes::ENABLED`, headless, and with the switch off —
/// which forgets the ledger, so switching it on again baselines silently).
pub async fn run(
    ctx: &JobContext,
    reason: CheckReason,
    sources: &[SourceId],
    silent: &[SourceId],
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<()> {
    if !crate::new_episodes::ENABLED || ctx.headless {
        return Ok(());
    }
    let cfg = crate::config::current(&ctx.config);
    if !cfg.new_episodes.enabled {
        write(ctx, |c| store_ne::forget_ledger(c).map(drop)).await?;
        return Ok(());
    }
    struct Running<'a>(&'a AtomicBool);
    impl Drop for Running<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    ctx.new_episodes.running.store(true, Ordering::Relaxed);
    let _running = Running(&ctx.new_episodes.running);
    progress.mark_inputs_read();
    progress.set(JobPhase::Indexing);
    let started = Instant::now();
    let lib = match ctx.published_library() {
        Some(lib) => lib,
        None => super::maintenance::rebuild_library(ctx).await?,
    };
    let opts = Opts::from_config(&cfg.new_episodes, crate::model::now());
    let (l, o) = (lib.clone(), opts);
    let candidates = read(ctx, move |c| store_ne::fetch_candidates(c, &l, &o)).await?;
    let scope: Option<HashSet<SourceId>> = (reason == CheckReason::Refresh).then(|| sources.iter().copied().collect());
    let plan = plan_fetches(&candidates, scope.as_ref(), reason != CheckReason::Launch, opts.now);
    let (fetched, fallback) = fetch(ctx, &plan, progress, cancel).await?;
    check(cancel)?;

    progress.set(JobPhase::Matching);
    let opts = Opts { now: crate::model::now(), ..opts };
    let input = CheckInput { diff: true, silent: silent.iter().copied().collect(), fallback };
    let (l, config) = (lib.clone(), ctx.config.clone());
    let report = write(ctx, move |c| {
        let Some(r) = check_if_on(c, &config, &l, &opts, &input)? else { return Ok(None) };
        store_ne::mark_checked(c, &fetched, opts.now)?;
        Ok(Some(r))
    })
    .await?;
    let Some(report) = report else {
        tracing::debug!(target: "tuner::jobs", "newep: switched off during the check; nothing written");
        return Ok(());
    };
    notify(ctx, DataScope::NewEpisodes);
    tracing::info!(
        target: "tuner::jobs",
        "newep: {} arrivals in {} series (sources {}), {} quiet; {} fetched, {} tracked, {} baselined, {:.2} s",
        report.arrivals,
        report.titles,
        if sources.is_empty() { "all".to_owned() } else { sources.iter().map(|s| s.0.to_string()).collect::<Vec<_>>().join(", ") },
        report.quiet,
        plan.len(),
        report.tracked,
        report.baselined,
        started.elapsed().as_secs_f64()
    );
    for s in &report.unreliable {
        tracing::info!(target: "tuner::jobs", "newep: source {} doesn't update series dates; its tracked series are fetched at every refresh", s.0);
    }
    if cfg.new_episodes.notify && !report.announce.is_empty() {
        announce(ctx, report.announce).await;
    }
    Ok(())
}

/// Fetch `plan` serially, paused while the player is busy. Returns the copies fetched and
/// those the fallback fetched.
async fn fetch(ctx: &JobContext, plan: &[Fetch], progress: &Progress, cancel: &CancellationToken) -> Result<(Vec<SeriesId>, HashSet<SeriesId>)> {
    let (mut fetched, mut fallback) = (Vec::new(), HashSet::new());
    let mut unreachable: HashSet<SourceId> = HashSet::new();
    for (i, f) in plan.iter().enumerate() {
        while ctx.player_busy.load(Ordering::Relaxed) {
            check(cancel)?;
            tokio::time::sleep(BUSY_POLL).await;
        }
        check(cancel)?;
        progress.set(JobPhase::Downloading { done: i as u64, total: Some(plan.len() as u64) });
        if unreachable.contains(&f.source) {
            continue;
        }
        match super::details::load_series_checked(ctx, f.series, 0, cancel).await {
            Ok(true) => {
                fetched.push(f.series);
                if f.fallback {
                    fallback.insert(f.series);
                }
            }
            Ok(false) => {}
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            // retried at the next refresh, never in a loop
            Err(e) => {
                tracing::debug!(target: "tuner::jobs", "newep: fetching series {} of source {}: {e}", f.series.0, f.source.0);
                if matches!(e, Error::Network(_)) {
                    unreachable.insert(f.source);
                }
            }
        }
    }
    Ok((fetched, fallback))
}

// ------------------------------------------------------------------ notifications

/// Queue `ids` for notifications: sent now, or once the hold lifts (§Q7).
async fn announce(ctx: &JobContext, ids: Vec<ArrivalId>) {
    lock(&ctx.new_episodes.held).extend(ids);
    if held_now(ctx) {
        wait_for_the_hold(ctx);
    } else {
        flush(ctx).await;
    }
}

/// Notifications wait while Tuner plays a film or episode full screen, and in the news flash's
/// quiet hours (Q7).
fn held_now(ctx: &JobContext) -> bool {
    let cfg = crate::config::current(&ctx.config);
    if crate::follow::flash::in_quiet_hours(&cfg.follow.news_flash, crate::reminders::local_minute_of_day(crate::model::now())) {
        return true;
    }
    let Some(shared) = &ctx.shared else { return false };
    shared.window().is_some_and(|w| w.fullscreen) && shared.main_player().is_some_and(|rx| plays_vod(&rx.borrow()))
}

/// A film or episode is on (playing or paused).
fn plays_vod(p: &crate::player::PlayerSnapshot) -> bool {
    use crate::model::PlayableKind as K;
    (p.state.is_active() || p.state == crate::player::PlayerState::Paused)
        && p.playable.as_ref().is_some_and(|pl| matches!(pl.kind, K::Movie | K::Episode { .. }))
}

/// One task waits for the hold to lift, then sends everything held (dropped after
/// [`HOLD_MAX`]: Home has them).
fn wait_for_the_hold(ctx: &JobContext) {
    let state = ctx.new_episodes.clone();
    if state.holding.swap(true, Ordering::AcqRel) {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let since = Instant::now();
        while held_now(&ctx) && since.elapsed() < HOLD_MAX {
            tokio::time::sleep(HOLD_POLL).await;
        }
        state.holding.store(false, Ordering::Release);
        if since.elapsed() >= HOLD_MAX {
            lock(&state.held).clear();
        } else {
            flush(&ctx).await;
        }
    });
}

/// Send the held arrivals' notifications (or their toasts) and mark them announced.
async fn flush(ctx: &JobContext) {
    let ids = std::mem::take(&mut *lock(&ctx.new_episodes.held));
    if ids.is_empty() {
        return;
    }
    let cfg = crate::config::current(&ctx.config);
    if !cfg.new_episodes.enabled || !cfg.new_episodes.notify {
        return;
    }
    let lib: Option<Arc<Library>> = ctx.published_library();
    let ann = read(ctx, move |c| {
        let lib = match lib {
            Some(lib) => lib,
            None => Arc::new(crate::store::library::load_visible(c, crate::store::library::Visibility::NO_EXPLICIT)?),
        };
        let ann = store_ne::announcements(c, &lib, &ids)?;
        let names: Vec<Option<String>> = ann.iter().map(|a| a.playlist.and_then(|s| lib.source(s)).map(|s| s.name.clone())).collect();
        Ok((ann, names))
    })
    .await;
    let (ann, playlists) = match ann {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "tuner::jobs", "new episodes: notifications: {e}");
            return;
        }
    };
    let guard = ctx.shared.as_ref().map(|s| s.guard()).unwrap_or_default();
    let now = crate::model::now();
    for (note, ids) in notes(&ann, &playlists, &guard, now) {
        let actions: Vec<(&str, String)> = actions_of(&note);
        let refs: Vec<(&str, &str)> = actions.iter().map(|(id, l)| (*id, l.as_str())).collect();
        let shown = crate::platform::notify::show_with_actions(&note.summary, &note.body, &refs, on_action(ctx.bus.clone(), &note)).await;
        if !shown {
            ctx.bus.send(AppEvent::Platform(PlatformEvent::NewEpisodesToast(note)));
        }
        if let Err(e) = write(ctx, move |c| store_ne::mark_notified(c, &ids, now)).await {
            tracing::warn!(target: "tuner::jobs", "new episodes: marking notified: {e}");
        }
    }
    notify(ctx, DataScope::NewEpisodes);
}

/// The notes of one round (§2.2, §3.4): one per series, or one summary for more than
/// [`MAX_SINGLE`] series. Names pass the spoiler guard (`Subject::Notification`; a hidden one
/// is left out, never guessed); an episode title shows only when exactly one episode is new and
/// the guard allows it. Each note comes with the arrivals it announces.
pub(crate) fn notes(ann: &[Announcement], playlists: &[Option<String>], guard: &Guard, now: Timestamp) -> Vec<(Note, Vec<ArrivalId>)> {
    let name = |t: &str| guard.text(&Subject::Notification { title: t }, Field::Title, t, Policy::Normal, now).text().map(str::to_owned);
    if ann.len() > MAX_SINGLE {
        let names: Vec<String> = ann.iter().filter_map(|a| name(&a.title)).collect();
        let body = match names.len() {
            0 => String::new(),
            n if n <= 3 => names.join(", "),
            n => format!("{} and {} more", names[..3].join(", "), n - 3),
        };
        let note = Note { summary: format!("New episodes in {} series", ann.len()), body, action: NoteAction::Open, download: Vec::new() };
        return vec![(note, ann.iter().flat_map(|a| a.arrivals.iter().copied()).collect())];
    }
    ann.iter()
        .zip(playlists.iter().chain(std::iter::repeat(&None)))
        .filter_map(|(a, playlist)| {
            let n = a.episodes.len();
            let what = if n == 1 { "new episode".to_owned() } else { format!("{n} new episodes") };
            let summary = match name(&a.title) {
                Some(t) => format!("{t}: {what}"),
                None => format!("{}{}", what[..1].to_uppercase(), &what[1..]),
            };
            let mut body = coords(&a.episodes);
            if let ([(s, e)], Some(title)) = (a.episodes.as_slice(), a.episode_title.as_deref()) {
                let subject = Subject::Episode { series_norm: &a.series_norm, season: *s as i32, episode: *e as i32 };
                if let Some(t) = guard.text(&subject, Field::EpisodeTitle, title, Policy::Normal, now).text() {
                    body = format!("{body} · {t}");
                }
            }
            if let Some(p) = playlist {
                body = format!("{body} · on {p}");
            }
            if a.downloading {
                body = format!("{body} · Downloading");
            }
            // the next unwatched episode (Q4), else the first new one
            let first = || Some((a.copies.first()?.clone(), a.refs.first()?.clone(), a.episodes.first()?.0, a.episodes.first()?.1));
            let (series, episode, s, e) = a.next.clone().or_else(first)?;
            let action = NoteAction::Play { label: format!("Play S{s}E{e}"), series, episode };
            let download = if crate::downloads::ENABLED && !a.downloading { a.refs.clone() } else { Vec::new() };
            Some((Note { summary, body, action, download }, a.arrivals.clone()))
        })
        .collect()
}

/// "S2E9, S2E10", "S2E8, S2E9, S2E10, S3E1 +2 more".
fn coords(eps: &[(u32, u32)]) -> String {
    const SHOWN: usize = 4;
    let mut s: Vec<String> = eps.iter().take(SHOWN).map(|(s, e)| format!("S{s}E{e}")).collect();
    if eps.len() > SHOWN {
        s.push(format!("+{} more", eps.len() - SHOWN));
    }
    s.join(", ")
}

/// A note's buttons: Play (or Open) first — also the body click — then Download.
fn actions_of(note: &Note) -> Vec<(&'static str, String)> {
    let mut out = match &note.action {
        NoteAction::Play { label, .. } => vec![("play", label.clone())],
        NoteAction::Open => vec![("open", "Open".to_owned())],
    };
    if !note.download.is_empty() {
        out.push(("download", "Download".to_owned()));
    }
    out
}

/// What picking a note's action does: Play / Open raise Tuner and tell the app; Download only
/// tells it (nothing to look at).
pub(crate) fn on_action(bus: EventBus, note: &Note) -> Box<dyn FnOnce(&str) + Send> {
    let (action, download) = (note.action.clone(), note.download.clone());
    Box::new(move |id: &str| {
        let ev = match (id, action) {
            ("download", _) => PlatformEvent::NewEpisodeDownload { episodes: download },
            (_, NoteAction::Play { series, episode, .. }) => {
                crate::platform::hypr::focus_self();
                PlatformEvent::NewEpisodePlay { series, episode }
            }
            (_, NoteAction::Open) => {
                crate::platform::hypr::focus_self();
                PlatformEvent::NewEpisodesOpen
            }
        };
        bus.send(AppEvent::Platform(ev));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef};

    fn cand(series: i64, source: i64, details_at: Option<Timestamp>, unreliable: bool) -> FetchCandidate {
        FetchCandidate { series: SeriesId(series), source: SourceId(source), details_at, unreliable }
    }

    /// §8.9: only tracked copies are candidates (the store's job); here the order, the
    /// fallback, the unreliable playlist and the per-playlist cap.
    #[test]
    fn fetches_are_planned_per_playlist_with_the_fallback_and_the_cap() {
        const NOW: Timestamp = 1_800_000_000;
        let week = super::super::details::DETAILS_TTL_S;
        let mut c = vec![
            cand(1, 1, None, false),             // last_modified moved
            cand(2, 1, Some(NOW - 60), false),   // fresh: no
            cand(3, 1, Some(NOW - week), false), // a week old: the fallback
            cand(4, 2, Some(NOW - 60), true),    // unreliable playlist: every refresh
            cand(5, 3, None, false),             // not refreshed
        ];
        let scope: HashSet<SourceId> = [SourceId(1), SourceId(2)].into();
        let ids = |f: Vec<Fetch>| f.iter().map(|f| (f.series.0, f.fallback)).collect::<Vec<_>>();
        assert_eq!(ids(plan_fetches(&c, Some(&scope), true, NOW)), [(1, false), (3, true), (4, true)]);
        assert_eq!(ids(plan_fetches(&c, None, false, NOW)), [(1, false), (5, false)], "launch: no fallback, every playlist");
        // the cap: changed copies first, then the oldest
        c = (0..50).map(|i| cand(100 + i, 1, Some(NOW - week - i), false)).collect();
        c.push(cand(99, 1, None, false));
        let planned = plan_fetches(&c, None, true, NOW);
        assert_eq!(planned.len(), FETCH_CAP);
        assert_eq!((planned[0].series.0, planned[1].series.0), (99, 149));
    }

    fn ann(title: &str, eps: &[(u32, u32)]) -> Announcement {
        let series = ItemRef { source: SourceId(1), kind: ItemKind::Series, key: ItemKey(title.into()) };
        let ep = |s: u32, e: u32| ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(format!("{title}{s}{e}")) };
        Announcement {
            title: title.into(),
            series_norm: title.to_lowercase(),
            copies: vec![series.clone()],
            arrivals: eps.iter().enumerate().map(|(i, _)| ArrivalId(i as i64)).collect(),
            episodes: eps.to_vec(),
            refs: eps.iter().map(|&(s, e)| ep(s, e)).collect(),
            playlist: Some(SourceId(1)),
            next: Some((series, ep(eps[0].0, eps[0].1), eps[0].0, eps[0].1)),
            episode_title: (eps.len() == 1).then(|| "Tomorrow".to_owned()),
            downloading: false,
        }
    }

    /// §8.10: three series give three notifications, more give one summary with Open.
    #[test]
    fn one_note_per_series_or_one_summary() {
        let guard = Guard::default();
        let tv = Some("TV".to_owned());
        let three = [ann("Landman", &[(2, 9), (2, 10)]), ann("The Bear", &[(3, 1)]), ann("Severance", &[(2, 8)])];
        let n = notes(&three, &[tv.clone(), tv.clone(), tv.clone()], &guard, 0);
        assert_eq!(n.len(), 3);
        assert_eq!((n[0].0.summary.as_str(), n[0].0.body.as_str()), ("Landman: 2 new episodes", "S2E9, S2E10 · on TV"));
        assert_eq!((n[1].0.summary.as_str(), n[1].0.body.as_str()), ("The Bear: new episode", "S3E1 · Tomorrow · on TV"));
        assert!(matches!(&n[0].0.action, NoteAction::Play { label, .. } if label == "Play S2E9"));
        assert_eq!(actions_of(&n[0].0).iter().map(|a| a.0).collect::<Vec<_>>(), ["play", "download"]);
        let six: Vec<Announcement> = ["A", "B", "C", "D", "E", "F"].iter().map(|t| ann(t, &[(1, 2)])).collect();
        let n = notes(&six, &[], &guard, 0);
        assert_eq!(n.len(), 1);
        assert_eq!((n[0].0.summary.as_str(), n[0].0.body.as_str()), ("New episodes in 6 series", "A, B, C and 3 more"));
        assert_eq!((n[0].0.action.clone(), n[0].1.len()), (NoteAction::Open, 6));
    }

    /// §3.4: a check that was still fetching when the master switch went off writes nothing
    /// (the switch's `forget_ledger` stands, so switching on again baselines silently).
    #[test]
    fn a_check_finishing_after_the_switch_went_off_writes_nothing() {
        let store = crate::store::Store::open_temp("new-episodes-switch", None).unwrap();
        let config: crate::config::SharedConfig = Default::default();
        store
            .writer()
            .blocking(move |c| {
                c.execute_batch(
                    "INSERT INTO source(id,name,kind,created_at) VALUES (1,'TV','m3u-url',0);
                     INSERT INTO series(id,source_id,key,name,clean_name,norm_name,year,provider_order,seen_gen,details_at)
                       VALUES (1,1,'landman','Landman','Landman','landman',2024,0,1,1);
                     INSERT INTO episode(series_id,source_id,key,season,episode,title,duration_s,stream) VALUES (1,1,'e1',1,1,'Episode',3000,'{}');",
                )?;
                crate::store::titles::refresh_all(c)?;
                let lib = crate::store::library::load(c)?;
                let item = ItemRef { source: SourceId(1), kind: ItemKind::Series, key: ItemKey("landman".into()) };
                let list = crate::store::watchlist::default_list(c)?;
                let entry = crate::store::watchlist::new_entry(c, &item)?;
                crate::store::watchlist::add(c, &lib, list, &[entry], crate::watchlist::Insert::End, 0)?;
                let opts = Opts::from_config(&crate::config::NewEpisodesConfig::default(), 1_800_000_000);
                let input = CheckInput { diff: true, ..Default::default() };
                let ledger = |c: &rusqlite::Connection| c.query_row("SELECT count(*) FROM newep_copy", [], |r| r.get::<_, i64>(0));
                let mut off = (*crate::config::current(&config)).clone();
                off.new_episodes.enabled = false;
                *config.write().unwrap() = std::sync::Arc::new(off);
                assert!(check_if_on(c, &config, &lib, &opts, &input)?.is_none());
                assert_eq!(ledger(c)?, 0, "switched off: no ledger");
                let mut on = (*crate::config::current(&config)).clone();
                on.new_episodes.enabled = true;
                *config.write().unwrap() = std::sync::Arc::new(on);
                assert_eq!(check_if_on(c, &config, &lib, &opts, &input)?.map(|r| r.baselined), Some(1));
                assert_eq!(ledger(c)?, 1);
                Ok(())
            })
            .unwrap();
    }

    /// §8.12: Play and the body click send `NewEpisodePlay`, Download `NewEpisodeDownload`.
    #[test]
    fn note_actions_become_platform_events() {
        let note = notes(&[ann("Landman", &[(2, 9)])], &[], &Guard::default(), 0).remove(0).0;
        for (id, play) in [("play", true), ("default", true), ("download", false)] {
            let (bus, rx) = EventBus::new(None);
            on_action(bus, &note)(id);
            match rx.try_recv().unwrap() {
                AppEvent::Platform(PlatformEvent::NewEpisodePlay { episode, .. }) if play => assert_eq!(episode.key.0, "Landman29"),
                AppEvent::Platform(PlatformEvent::NewEpisodeDownload { episodes }) if !play => assert_eq!(episodes.len(), 1),
                other => panic!("{id}: {other:?}"),
            }
        }
    }
}
