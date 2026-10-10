//! Background job system on the tokio runtime. Owner: WP-06. DESIGN §5.2, §9.
//!
//! * `JobManager::submit` is non-blocking and callable from any thread (UI included).
//! * Identical jobs coalesce ONLY while the existing one is `Queued` or `Connecting` and has not
//!   read its inputs yet: the call returns its id (a `force` request upgrades it). Once it is
//!   further along, a `rerun` flag is set and the job is submitted again when it finishes —
//!   a newer request is never dropped.
//! * Concurrency limits (semaphores): playlist refreshes 2, EPG refreshes 1 (DB-write heavy),
//!   detail loads 4, everything else 2.
//! * Every job gets a `CancellationToken`; downloads/parsers check it at least every 64 KiB /
//!   1 000 items. Cancelled imports never sweep (see `store::import`).
//! * Progress: `JobEvent::Updated` at most every 100 ms per job (throttled) through the
//!   `EventBus`; `JobEvent::Finished` exactly once. The UI shows active jobs in the status bar
//!   and per source in Settings ▸ Playlists.
//! * Heavy CPU (parsing, JSON decode, matching) AND every SQLite read (library build, matching
//!   inputs, EPG search) runs in `spawn_blocking` with its own reader connection — never on the
//!   3 async workers. DB writes go through `store::Writer` in batches (one transaction each).
//! * Follow-up work (refresh → provider EPG → RebuildLibrary, restore → refresh every source,
//!   AddSource → RefreshSource) is submitted through `JobContext::submit`.
//! * The scheduler skips ticks while `JobContext::player_busy` is set (player Loading/Zapping;
//!   the app updates the flag from every snapshot).
//!
//! Details fixed by the implementation:
//! * A job is `Queued` until it holds its semaphore permit; it turns `Connecting` (and reads its
//!   — possibly upgraded — `JobKind`) atomically with respect to `submit`, so a coalesced request
//!   is either merged into the job or rerun after it, never lost. Refreshes stay `Connecting`
//!   while they talk to the server, so they mark the moment they read their source row
//!   (`Progress::mark_inputs_read`): from then on an identical request is rerun, not merged.
//! * Merging: `RefreshSource`/`RefreshEpg` OR their `force` flags; every other kind takes the
//!   newer request. Cancelling a job drops the reruns requested before it; a request arriving
//!   after the cancel (while the cancelled job winds down) becomes its rerun, so runs of one key
//!   never overlap and that request is not lost.
//! * Every failure is logged at WARN under target `tuner::jobs` as one redacted sentence
//!   ("Couldn't refresh My IPTV: HTTP 404 from …"); the Jobs overlay lists these lines as
//!   "Recent problems" (there is no separate job history). Cancellation is logged at INFO.
//! * A panicking job becomes `Finished(Err)`; the manager keeps running.
//! * `AddSource` runs the first refresh inline (one job, one progress bar for onboarding) and
//!   reuses a never-refreshed source with the same locator instead of inserting a duplicate
//!   (an onboarding retry after a failed import). The find-or-insert is one writer closure, so
//!   two adds of one locator racing each other end up with one source. Adds coalesce by
//!   locator (not name), and a request for an add that is already running joins it (same job
//!   id, no rerun).

pub mod details;
pub mod import_epg;
pub mod import_source;
pub mod maintenance;
pub mod probe;
pub mod scheduler;

mod db;
#[cfg(test)]
mod manager_tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::SharedConfig;
use crate::error::{Error, Result};
use crate::model::{AccountInfo, EpgSourceId, JobId, MovieId, SeriesId, SourceDraft, SourceId, SourceKind, Timestamp};
use crate::msg::{AppEvent, DataScope, EventBus};
use crate::paths::AppPaths;
use crate::sources::http::HttpClient;
use crate::store::backup::{BackupOptions, RestoreMode, RestoreReport};
use crate::store::epg::EpgImportSummary;
use crate::store::import::ImportSummary;
use crate::store::library::Library;
use crate::store::Store;

/// What to do.
#[derive(Debug, Clone, PartialEq)]
pub enum JobKind {
    /// Re-import a playlist source (M3U download/parse or Xtream API), then its provider EPG
    /// if stale, then `RebuildLibrary`. `force` ignores ETag/Last-Modified.
    RefreshSource { source: SourceId, force: bool },
    /// Re-import one EPG source, re-match, then emit `DataScope::Epg`.
    RefreshEpg { epg_source: EpgSourceId, force: bool },
    /// Scheduler tick: every stale playlist + EPG source (DESIGN §9.5).
    RefreshStale,
    /// Validate a draft without saving (onboarding "Test" / step 5): login, account info,
    /// item count estimate, detected kind.
    ProbeSource(SourceDraft),
    /// Insert the draft, then run `RefreshSource` for it (onboarding progress screen).
    AddSource(SourceDraft),
    /// Lazy `get_vod_info`.
    LoadMovieDetails(MovieId),
    /// Lazy `get_series_info` (seasons + episodes).
    LoadSeriesDetails(SeriesId),
    /// Re-run automatic EPG matching for all sources and resolve mappings.
    RematchEpg,
    /// Build a fresh in-memory `Library` snapshot (→ `JobOutcome::Library`).
    RebuildLibrary,
    /// Prune old programmes, sweep inactive EPG generations, `PRAGMA optimize`.
    Maintenance,
    ExportBackup { path: PathBuf, opts: BackupOptions },
    /// Restore, then submit `RefreshSource` for every restored source.
    ImportBackup { path: PathBuf, mode: RestoreMode },
}

impl JobKind {
    /// Coalescing key: jobs with equal keys share one execution (see module docs).
    pub fn dedupe_key(&self) -> String {
        match self {
            JobKind::RefreshSource { source, .. } => format!("refresh-source:{}", source.0),
            JobKind::RefreshEpg { epg_source, .. } => format!("refresh-epg:{}", epg_source.0),
            JobKind::RefreshStale => "refresh-stale".into(),
            JobKind::ProbeSource(d) => format!("probe-source:{}", draft_digest(d)),
            // by locator, not by name: two adds of one playlist are one source
            JobKind::AddSource(d) => format!("add-source:{}", locator_digest(d)),
            JobKind::LoadMovieDetails(id) => format!("movie-details:{}", id.0),
            JobKind::LoadSeriesDetails(id) => format!("series-details:{}", id.0),
            JobKind::RematchEpg => "rematch-epg".into(),
            JobKind::RebuildLibrary => "rebuild-library".into(),
            JobKind::Maintenance => "maintenance".into(),
            JobKind::ExportBackup { path, .. } => format!("export-backup:{}", path.display()),
            JobKind::ImportBackup { path, .. } => format!("import-backup:{}", path.display()),
        }
    }

    /// `self` merged with a newer identical request (same dedupe key).
    fn merged_with(&self, newer: &JobKind) -> JobKind {
        match (self, newer) {
            (JobKind::RefreshSource { source, force: a }, JobKind::RefreshSource { force: b, .. }) => {
                JobKind::RefreshSource { source: *source, force: *a || *b }
            }
            (JobKind::RefreshEpg { epg_source, force: a }, JobKind::RefreshEpg { force: b, .. }) => {
                JobKind::RefreshEpg { epg_source: *epg_source, force: *a || *b }
            }
            _ => newer.clone(),
        }
    }
}

/// Short digest of a draft's identity (kind + locator + credentials): keys never carry secrets.
fn draft_digest(d: &SourceDraft) -> String {
    let mut h = blake3::Hasher::new();
    let mut field = |s: &str| {
        h.update(s.as_bytes());
        h.update(b"\x1f");
    };
    field(d.kind.as_str());
    field(&d.name);
    field(d.url.as_deref().unwrap_or(""));
    field(&d.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    if let Some(x) = &d.xtream {
        field(&x.server);
        field(&x.username);
        field(x.password.expose());
    }
    h.finalize().to_hex()[..16].to_owned()
}

/// Short digest of what a draft points at (kind + URL / file / server + account), the identity
/// `probe::add_source` reuses a saved source by; names, colours and settings don't count.
fn locator_digest(d: &SourceDraft) -> String {
    let mut h = blake3::Hasher::new();
    let mut field = |s: &str| {
        h.update(s.as_bytes());
        h.update(b"\x1f");
    };
    field(d.kind.as_str());
    match d.kind {
        SourceKind::M3uUrl => field(d.url.as_deref().unwrap_or("").trim()),
        SourceKind::M3uFile => field(&d.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()),
        SourceKind::Xtream => {
            if let Some(x) = &d.xtream {
                field(x.server.trim_end_matches('/'));
                field(&x.username);
                field(x.password.expose());
            }
        }
    }
    h.finalize().to_hex()[..16].to_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobPhase {
    Queued,
    Connecting,
    /// bytes done / total (if known).
    Downloading { done: u64, total: Option<u64> },
    /// items parsed so far.
    Parsing { items: u64 },
    /// rows written / expected.
    Writing { done: u64, total: Option<u64> },
    Matching,
    Indexing,
    Finishing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    pub id: JobId,
    pub kind: JobKind,
    /// e.g. "Refreshing My Provider" (no secrets) — built by the manager with source names.
    pub label: String,
    pub phase: JobPhase,
    pub started_at: Timestamp,
    /// 0..=1 when computable.
    pub fraction: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// May differ from the draft (an Xtream `get.php` URL pasted as M3U is upgraded).
    pub kind: SourceKind,
    pub xtream: Option<crate::model::XtreamCredentials>,
    pub account: Option<AccountInfo>,
    /// Rough counts (Xtream: categories; M3U: first 1 MiB sample extrapolated).
    pub estimate_channels: Option<u32>,
    pub suggested_name: Option<String>,
    pub epg_urls: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum JobOutcome {
    Nothing,
    /// `account` = fresh Xtream account info (the app forwards `max_connections` to the
    /// player budget with `PlayerCommand::SetConnectionLimit`).
    SourceRefreshed { source: SourceId, summary: ImportSummary, not_modified: bool, account: Option<AccountInfo> },
    EpgRefreshed { epg_source: EpgSourceId, summary: EpgImportSummary, not_modified: bool },
    Probe(ProbeResult),
    SourceAdded(SourceId),
    Library(Arc<Library>),
    BackupWritten(PathBuf),
    Restored(RestoreReport),
}

/// Why a job failed, for reactions that must not parse the (user-facing) message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Cancelled,
    /// The server could not be reached (DNS, connection, TLS, timeout): `Error::Network`. An
    /// HTTP error status means the network works and is `Other`.
    Network,
    Other,
}

impl FailureKind {
    pub fn of(e: &Error) -> Self {
        match e {
            Error::Cancelled => FailureKind::Cancelled,
            Error::Network(_) => FailureKind::Network,
            _ => FailureKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // progress/finish messages, ≤ 10 Hz per job
pub enum JobEvent {
    Updated(JobStatus),
    /// `Err` holds a redacted, user-presentable message; `failure` is `Some` exactly when
    /// `outcome` is `Err` and says what kind of failure it was.
    Finished { id: JobId, kind: JobKind, outcome: Result<JobOutcome, String>, failure: Option<FailureKind> },
}

/// Submits a follow-up job (returns its id). Installed by [`JobManager::new`].
pub type JobSubmitter = Arc<dyn Fn(JobKind) -> JobId + Send + Sync>;

/// Shared dependencies handed to every job.
#[derive(Clone)]
pub struct JobContext {
    pub store: Store,
    pub http: HttpClient,
    pub bus: EventBus,
    pub paths: AppPaths,
    /// Live config (Settings changes are visible to the next job).
    pub config: SharedConfig,
    /// Follow-up jobs (refresh → EPG → RebuildLibrary, restore → refresh...).
    pub submit: JobSubmitter,
    /// Set by the app while the player is Loading/Zapping (scheduler skips its tick).
    pub player_busy: Arc<AtomicBool>,
}

impl JobContext {
    /// `submit` is a no-op placeholder until [`JobManager::new`] installs the real one.
    pub fn new(store: Store, http: HttpClient, bus: EventBus, paths: AppPaths, config: SharedConfig) -> Self {
        Self { store, http, bus, paths, config, submit: Arc::new(|_| JobId(0)), player_busy: Arc::default() }
    }
}

// ------------------------------------------------------------------ helpers for job bodies

/// Poison-tolerant lock (a panicking job must not wedge the manager).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }

/// Run `f` with a fresh reader connection on the blocking pool (never on an async worker).
async fn read<R, F>(ctx: &JobContext, f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce(&Connection) -> Result<R> + Send + 'static,
{
    let store = ctx.store.clone();
    blocking(move || f(&store.reader()?)).await
}

/// Run `f` on the single writer thread.
async fn write<R, F>(ctx: &JobContext, f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
{
    ctx.store.writer().run(f).await
}

/// `spawn_blocking` with the join error (a panic) mapped to `Error`.
async fn blocking<R, F>(f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce() -> Result<R> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Other(format!("background task failed: {e}")))?
}

/// `Err(Cancelled)` once `cancel` fired.
fn check(cancel: &CancellationToken) -> Result<()> { if cancel.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) } }

// ------------------------------------------------------------------ progress

/// Minimum interval between two `JobEvent::Updated` of one job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Throttled progress reporter given to a running job.
#[derive(Clone)]
pub struct Progress {
    id: JobId,
    bus: EventBus,
    status: Arc<Mutex<JobStatus>>,
    last_sent: Arc<Mutex<Instant>>,
    /// What the job works on ("My IPTV"), once known: label + failure sentence.
    subject: Arc<Mutex<Option<String>>>,
    /// The job has read its inputs (source row…): identical requests no longer coalesce.
    inputs_read: Arc<AtomicBool>,
}

impl Progress {
    /// A reporter attached to nothing (unit tests of job functions; events go nowhere).
    pub fn detached() -> Self {
        let (bus, _rx) = EventBus::new(None);
        let status = JobStatus {
            id: JobId(0),
            kind: JobKind::Maintenance,
            label: "test".into(),
            phase: JobPhase::Queued,
            started_at: 0,
            fraction: None,
        };
        Self::attached(bus, status)
    }

    fn attached(bus: EventBus, status: JobStatus) -> Self {
        let last = Instant::now().checked_sub(PROGRESS_INTERVAL).unwrap_or_else(Instant::now);
        Self {
            id: status.id,
            bus,
            status: Arc::new(Mutex::new(status)),
            last_sent: Arc::new(Mutex::new(last)),
            subject: Arc::default(),
            inputs_read: Arc::default(),
        }
    }

    /// Update the phase; emits `JobEvent::Updated` if ≥ 100 ms since the last one.
    pub fn set(&self, phase: JobPhase) {
        let snapshot = {
            let mut s = lock(&self.status);
            s.fraction = match &phase {
                JobPhase::Downloading { done, total: Some(t) } | JobPhase::Writing { done, total: Some(t) } if *t > 0 => {
                    Some((*done as f64 / *t as f64).clamp(0.0, 1.0) as f32)
                }
                JobPhase::Downloading { .. } | JobPhase::Writing { .. } => None,
                _ => s.fraction,
            };
            s.phase = phase;
            s.clone()
        };
        self.emit(snapshot, false);
    }

    pub fn set_fraction(&self, f: Option<f32>) {
        let snapshot = {
            let mut s = lock(&self.status);
            s.fraction = f.map(|f| f.clamp(0.0, 1.0));
            s.clone()
        };
        self.emit(snapshot, false);
    }

    /// Name what the job works on (a source name…): rebuilds the label, always emitted.
    fn set_subject(&self, subject: &str) {
        *lock(&self.subject) = Some(subject.to_owned());
        let snapshot = {
            let mut s = lock(&self.status);
            s.label = label_for(&s.kind, Some(subject));
            s.clone()
        };
        self.emit(snapshot, true);
    }

    fn subject(&self) -> Option<String> { lock(&self.subject).clone() }

    /// Called right BEFORE a job reads the inputs a newer request could change (its source
    /// row): a request submitted after this is rerun instead of merged into this run.
    fn mark_inputs_read(&self) { self.inputs_read.store(true, Ordering::SeqCst); }

    fn has_read_inputs(&self) -> bool { self.inputs_read.load(Ordering::SeqCst) }

    fn emit(&self, status: JobStatus, always: bool) {
        {
            let mut last = lock(&self.last_sent);
            if !always && last.elapsed() < PROGRESS_INTERVAL {
                return;
            }
            *last = Instant::now();
        }
        debug_assert_eq!(status.id, self.id);
        self.bus.send(AppEvent::Job(JobEvent::Updated(status)));
    }
}

/// Status-bar label ("Refreshing My IPTV"); no secrets (names are user-chosen).
fn label_for(kind: &JobKind, subject: Option<&str>) -> String {
    let or = |default: &str| subject.map(str::to_owned).unwrap_or_else(|| default.to_owned());
    match kind {
        JobKind::RefreshSource { .. } => format!("Refreshing {}", or("playlist")),
        JobKind::RefreshEpg { .. } => format!("Updating {}", or("guide")),
        JobKind::RefreshStale => "Checking for updates".into(),
        JobKind::ProbeSource(d) => format!("Testing {}", subject.unwrap_or(&d.name)),
        JobKind::AddSource(d) => format!("Adding {}", subject.unwrap_or(&d.name)),
        JobKind::LoadMovieDetails(_) => format!("Loading {}", or("movie details")),
        JobKind::LoadSeriesDetails(_) => format!("Loading {}", or("series details")),
        JobKind::RematchEpg => "Matching guide channels".into(),
        JobKind::RebuildLibrary => "Updating library".into(),
        JobKind::Maintenance => "Tidying up the database".into(),
        JobKind::ExportBackup { .. } => "Exporting backup".into(),
        JobKind::ImportBackup { .. } => "Restoring backup".into(),
    }
}

/// First half of the WARN line logged when a job fails ("Couldn't refresh My IPTV").
fn failure_for(kind: &JobKind, subject: Option<&str>) -> String {
    let or = |default: &str| subject.map(str::to_owned).unwrap_or_else(|| default.to_owned());
    match kind {
        JobKind::RefreshSource { .. } => format!("Couldn't refresh {}", or("the playlist")),
        JobKind::RefreshEpg { .. } => format!("Couldn't update {}", or("the guide")),
        JobKind::RefreshStale => "Couldn't check for updates".into(),
        JobKind::ProbeSource(d) => format!("Couldn't connect to {}", subject.unwrap_or(&d.name)),
        JobKind::AddSource(d) => format!("Couldn't add {}", subject.unwrap_or(&d.name)),
        JobKind::LoadMovieDetails(_) => format!("Couldn't load the details of {}", or("a movie")),
        JobKind::LoadSeriesDetails(_) => format!("Couldn't load the episodes of {}", or("a series")),
        JobKind::RematchEpg => "Couldn't match guide channels".into(),
        JobKind::RebuildLibrary => "Couldn't load the library".into(),
        JobKind::Maintenance => "Database maintenance failed".into(),
        JobKind::ExportBackup { .. } => "Couldn't export the backup".into(),
        JobKind::ImportBackup { .. } => "Couldn't restore the backup".into(),
    }
}

// ------------------------------------------------------------------ manager

/// Handle owned by `Services`. Cheap to clone. Private fields are WP-06's to reshape.
#[derive(Clone)]
pub struct JobManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    ctx: JobContext,
    rt: tokio::runtime::Handle,
    jobs: Mutex<Jobs>,
    next_id: AtomicU64,
    limits: Limits,
    /// Shutting down: new submissions (follow-ups, reruns) are refused.
    closing: AtomicBool,
}

#[derive(Default)]
struct Jobs {
    by_id: HashMap<JobId, Entry>,
    /// dedupe key → the queued/running job with that key.
    by_key: HashMap<String, JobId>,
}

struct Entry {
    key: String,
    /// The (possibly upgraded) request; read by the task when it leaves `Queued`.
    kind: JobKind,
    progress: Progress,
    cancel: CancellationToken,
    /// Submit this when the job finishes (a request that arrived too late to coalesce).
    rerun: Option<JobKind>,
}

impl Entry {
    /// Run `kind` once this job has finished (merged with an already pending rerun).
    fn queue_rerun(&mut self, kind: JobKind) {
        let rerun = match &self.rerun {
            Some(r) => r.merged_with(&kind),
            None => kind,
        };
        tracing::debug!("job {:?} busy; rerun queued", self.progress.id);
        self.rerun = Some(rerun);
    }

    /// Cancel this run; requests made before the cancel go with it.
    fn cancel_run(&mut self) {
        self.cancel.cancel();
        self.rerun = None;
    }
}

/// Concurrency limits (DESIGN §5.2).
struct Limits {
    playlists: Arc<Semaphore>,
    epg: Arc<Semaphore>,
    details: Arc<Semaphore>,
    other: Arc<Semaphore>,
}

impl Limits {
    fn new() -> Self {
        Self {
            playlists: Arc::new(Semaphore::new(2)),
            epg: Arc::new(Semaphore::new(1)),
            details: Arc::new(Semaphore::new(4)),
            other: Arc::new(Semaphore::new(2)),
        }
    }

    fn for_kind(&self, kind: &JobKind) -> Arc<Semaphore> {
        match kind {
            JobKind::RefreshSource { .. } | JobKind::AddSource(_) | JobKind::ProbeSource(_) => self.playlists.clone(),
            JobKind::RefreshEpg { .. } | JobKind::RematchEpg => self.epg.clone(),
            JobKind::LoadMovieDetails(_) | JobKind::LoadSeriesDetails(_) => self.details.clone(),
            _ => self.other.clone(),
        }
    }
}

impl JobManager {
    /// Create the manager on an existing runtime (owned by `Services`); installs
    /// `ctx.submit` (a weak reference back to this manager — no reference cycle).
    pub fn new(mut ctx: JobContext, rt: tokio::runtime::Handle) -> Self {
        let inner = Arc::new_cyclic(|weak: &std::sync::Weak<ManagerInner>| {
            let weak = weak.clone();
            ctx.submit = Arc::new(move |kind| match weak.upgrade() {
                Some(inner) => JobManager { inner }.submit(kind),
                None => JobId(0),
            });
            ManagerInner { ctx, rt, jobs: Mutex::default(), next_id: AtomicU64::new(1), limits: Limits::new(), closing: AtomicBool::new(false) }
        });
        Self { inner }
    }

    /// Queue a job (non-blocking). Returns the id (existing one when coalesced).
    pub fn submit(&self, kind: JobKind) -> JobId {
        if self.inner.closing.load(Ordering::SeqCst) {
            tracing::debug!(target: "tuner::jobs", "shutting down: {} not started", label_for(&kind, None));
            return JobId(0);
        }
        let key = kind.dedupe_key();
        let mut jobs = lock(&self.inner.jobs);
        if let Some(id) = jobs.by_key.get(&key).copied()
            && let Some(e) = jobs.by_id.get_mut(&id)
        {
            let phase = lock(&e.progress.status).phase.clone();
            match phase {
                // cancelled (winding down): never merge into it, run afterwards
                _ if e.cancel.is_cancelled() => e.queue_rerun(kind),
                JobPhase::Queued => {
                    // not started: the job will run the merged request
                    e.kind = e.kind.merged_with(&kind);
                    lock(&e.progress.status).kind = e.kind.clone();
                }
                JobPhase::Connecting if !e.progress.has_read_inputs() && e.kind.merged_with(&kind) == e.kind => {}
                // the running add answers this request too: a rerun after it would find the
                // source already refreshed and add it a second time
                _ if matches!(kind, JobKind::AddSource(_)) => {}
                _ => e.queue_rerun(kind),
            }
            return id;
        }

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let status = JobStatus {
            id,
            kind: kind.clone(),
            label: label_for(&kind, None),
            phase: JobPhase::Queued,
            started_at: crate::model::now(),
            fraction: None,
        };
        let progress = Progress::attached(self.inner.ctx.bus.clone(), status.clone());
        let cancel = CancellationToken::new();
        let semaphore = self.inner.limits.for_kind(&kind);
        jobs.by_key.insert(key.clone(), id);
        jobs.by_id.insert(id, Entry { key, kind, progress: progress.clone(), cancel: cancel.clone(), rerun: None });
        drop(jobs);

        progress.emit(status, true);
        let inner = self.inner.clone();
        self.inner.rt.spawn(async move { inner.run(id, semaphore, progress, cancel).await });
        id
    }

    /// Request cancellation (no-op if finished). Reruns requested so far are dropped.
    pub fn cancel(&self, id: JobId) {
        if let Some(e) = lock(&self.inner.jobs).by_id.get_mut(&id) {
            e.cancel_run();
        }
    }

    /// Cancel everything (shutdown, backup restore).
    pub fn cancel_all(&self) {
        for e in lock(&self.inner.jobs).by_id.values_mut() {
            e.cancel_run();
        }
    }

    /// App exit: refuse new jobs (follow-ups, reruns) and cancel everything. Cancelled jobs
    /// still wind down (discard a partial guide, restore its channel matches) — wait for that
    /// with [`Self::wait_idle`] before the runtime goes away.
    pub fn shutdown(&self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        self.cancel_all();
    }

    /// Block until no job is queued or running, at most `timeout`; false on timeout. Not for
    /// the UI thread while it runs frames (exit only).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if lock(&self.inner.jobs).by_id.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Snapshot of running/queued jobs for the status bar.
    pub fn active(&self) -> Vec<JobStatus> {
        let mut out: Vec<JobStatus> = lock(&self.inner.jobs).by_id.values().map(|e| lock(&e.progress.status).clone()).collect();
        out.sort_by_key(|s| s.id);
        out
    }

    pub fn context(&self) -> &JobContext { &self.inner.ctx }
    pub fn runtime(&self) -> &tokio::runtime::Handle { &self.inner.rt }
}

impl ManagerInner {
    /// The task of one job: wait for a permit (Queued), run, report, rerun.
    async fn run(self: Arc<Self>, id: JobId, semaphore: Arc<Semaphore>, progress: Progress, cancel: CancellationToken) {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            p = semaphore.acquire_owned() => p.ok(),
        };
        let (kind, result) = match permit.and_then(|p| self.start(id).map(|k| (p, k))) {
            None => (lock(&progress.status).kind.clone(), Err(Error::Cancelled)),
            Some((permit, kind)) => {
                // own task: a panic surfaces as a JoinError instead of unwinding through us
                let ctx = self.ctx.clone();
                let (k, p, c) = (kind.clone(), progress.clone(), cancel.clone());
                let r = match tokio::spawn(async move { execute(&ctx, &k, &p, &c).await }).await {
                    Ok(r) => r,
                    Err(e) if e.is_panic() => {
                        tracing::error!(target: "tuner::jobs", "job {kind:?} panicked");
                        Err(Error::Other("internal error (see the log)".into()))
                    }
                    Err(_) => Err(Error::Cancelled),
                };
                drop(permit);
                (kind, r)
            }
        };
        self.finish(id, kind, result, &progress);
    }

    /// Leave `Queued`: read the (possibly merged) kind and turn `Connecting`, under the jobs
    /// lock so `submit` sees a consistent state. `None` if the entry vanished.
    fn start(&self, id: JobId) -> Option<JobKind> {
        let jobs = lock(&self.jobs);
        let e = jobs.by_id.get(&id)?;
        let mut s = lock(&e.progress.status);
        s.phase = JobPhase::Connecting;
        s.kind = e.kind.clone();
        s.started_at = crate::model::now();
        Some(e.kind.clone())
    }

    fn finish(&self, id: JobId, kind: JobKind, result: Result<JobOutcome>, progress: &Progress) {
        let rerun = {
            let mut jobs = lock(&self.jobs);
            let entry = jobs.by_id.remove(&id);
            match entry {
                Some(e) => {
                    if jobs.by_key.get(&e.key) == Some(&id) {
                        jobs.by_key.remove(&e.key);
                    }
                    e.rerun
                }
                None => None,
            }
        };
        let subject = progress.subject();
        let failure = result.as_ref().err().map(FailureKind::of);
        let outcome = match result {
            Ok(o) => {
                tracing::debug!(target: "tuner::jobs", "{} done", label_for(&kind, subject.as_deref()));
                Ok(o)
            }
            Err(Error::Cancelled) => {
                tracing::info!(target: "tuner::jobs", "{} cancelled", label_for(&kind, subject.as_deref()));
                Err(Error::Cancelled.to_string())
            }
            Err(e) => {
                let msg = crate::redact::redact(&e.to_string()).into_owned();
                tracing::warn!(target: "tuner::jobs", "{}: {msg}", failure_for(&kind, subject.as_deref()));
                Err(msg)
            }
        };
        self.ctx.bus.send(AppEvent::Job(JobEvent::Finished { id, kind, outcome, failure }));
        // (reruns of a cancelled job were requested after the cancel)
        if let Some(k) = rerun {
            (self.ctx.submit)(k);
        }
    }
}

/// Run one job body.
async fn execute(ctx: &JobContext, kind: &JobKind, progress: &Progress, cancel: &CancellationToken) -> Result<JobOutcome> {
    match kind {
        JobKind::RefreshSource { source, force } => {
            let r = import_source::refresh_source(ctx, *source, *force, progress, cancel).await?;
            let source = *source;
            let account = read(ctx, move |c| Ok(crate::store::sources::get_source(c, source)?.status.account)).await?;
            Ok(JobOutcome::SourceRefreshed { source, summary: r.summary, not_modified: r.not_modified, account })
        }
        JobKind::RefreshEpg { epg_source, force } => {
            let r = import_epg::refresh_epg(ctx, *epg_source, *force, progress, cancel).await?;
            Ok(JobOutcome::EpgRefreshed { epg_source: *epg_source, summary: r.summary, not_modified: r.not_modified })
        }
        JobKind::RefreshStale => {
            scheduler::refresh_stale(ctx).await?;
            Ok(JobOutcome::Nothing)
        }
        JobKind::ProbeSource(draft) => Ok(JobOutcome::Probe(probe::probe(ctx, draft, progress, cancel).await?)),
        JobKind::AddSource(draft) => Ok(JobOutcome::SourceAdded(probe::add_source(ctx, draft, progress, cancel).await?)),
        JobKind::LoadMovieDetails(id) => {
            details::load_movie(ctx, *id, progress, cancel).await?;
            Ok(JobOutcome::Nothing)
        }
        JobKind::LoadSeriesDetails(id) => {
            details::load_series(ctx, *id, progress, cancel).await?;
            Ok(JobOutcome::Nothing)
        }
        JobKind::RematchEpg => {
            import_epg::rematch_all(ctx, progress, cancel).await?;
            Ok(JobOutcome::Nothing)
        }
        JobKind::RebuildLibrary => {
            progress.set(JobPhase::Indexing);
            Ok(JobOutcome::Library(maintenance::rebuild_library(ctx).await?))
        }
        JobKind::Maintenance => {
            maintenance::maintenance(ctx, progress, cancel).await?;
            Ok(JobOutcome::Nothing)
        }
        JobKind::ExportBackup { path, opts } => {
            progress.set(JobPhase::Writing { done: 0, total: None });
            maintenance::export_backup(ctx, path, *opts).await?;
            Ok(JobOutcome::BackupWritten(path.clone()))
        }
        JobKind::ImportBackup { path, mode } => {
            progress.set(JobPhase::Writing { done: 0, total: None });
            Ok(JobOutcome::Restored(maintenance::import_backup(ctx, path, *mode).await?))
        }
    }
}

/// `DataScope` notification helper for job bodies.
fn notify(ctx: &JobContext, scope: DataScope) { ctx.bus.data_changed(scope) }

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{SourceColor, SourceSettings, XtreamCredentials};

    fn draft(name: &str, pw: &str) -> SourceDraft {
        SourceDraft {
            name: name.into(),
            kind: SourceKind::Xtream,
            icon: String::new(),
            color: SourceColor::Accent,
            url: None,
            path: None,
            xtream: Some(XtreamCredentials { server: "http://h".into(), username: "u".into(), password: pw.to_owned().into() }),
            settings: SourceSettings::default(),
        }
    }

    #[test]
    fn dedupe_keys_ignore_force_and_hide_secrets() {
        let a = JobKind::RefreshSource { source: SourceId(3), force: false };
        let b = JobKind::RefreshSource { source: SourceId(3), force: true };
        assert_eq!(a.dedupe_key(), b.dedupe_key());
        assert_ne!(a.dedupe_key(), JobKind::RefreshSource { source: SourceId(4), force: false }.dedupe_key());
        assert_ne!(a.dedupe_key(), JobKind::RefreshEpg { epg_source: EpgSourceId(3), force: false }.dedupe_key());
        let p = JobKind::ProbeSource(draft("x", "hunter2-secret"));
        assert!(!p.dedupe_key().contains("hunter2"));
        assert_eq!(p.dedupe_key(), JobKind::ProbeSource(draft("x", "hunter2-secret")).dedupe_key());
        assert_ne!(p.dedupe_key(), JobKind::ProbeSource(draft("x", "other")).dedupe_key());
        assert_ne!(p.dedupe_key(), JobKind::AddSource(draft("x", "hunter2-secret")).dedupe_key());
        // adds coalesce by what they point at, not by name
        let add = |name: &str, pw: &str| JobKind::AddSource(draft(name, pw)).dedupe_key();
        assert_eq!(add("x", "pw"), add("Other name", "pw"));
        assert_ne!(add("x", "pw"), add("x", "pw2"));
        assert!(!add("x", "hunter2-secret").contains("hunter2"));
    }

    #[test]
    fn merging_ors_force() {
        let a = JobKind::RefreshEpg { epg_source: EpgSourceId(1), force: true };
        let b = JobKind::RefreshEpg { epg_source: EpgSourceId(1), force: false };
        assert_eq!(a.merged_with(&b), a);
        assert_eq!(b.merged_with(&a), a);
        assert_eq!(JobKind::RebuildLibrary.merged_with(&JobKind::RebuildLibrary), JobKind::RebuildLibrary);
    }

    fn status(id: u64) -> JobStatus {
        JobStatus { id: JobId(id), kind: JobKind::Maintenance, label: "t".into(), phase: JobPhase::Queued, started_at: 0, fraction: None }
    }

    #[test]
    fn progress_is_throttled_to_ten_hertz() {
        let (bus, rx) = EventBus::new(None);
        let p = Progress::attached(bus, status(7));
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed() < Duration::from_millis(350) {
            n += 1;
            p.set(JobPhase::Parsing { items: n });
        }
        let updates: Vec<JobStatus> = rx
            .try_iter()
            .filter_map(|e| match e {
                AppEvent::Job(JobEvent::Updated(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert!(n > 100, "the loop ran {n} times");
        // the first call emits; then at most one per 100 ms
        assert!((1..=5).contains(&updates.len()), "{} updates", updates.len());
        assert!(updates.iter().all(|s| s.id == JobId(7)));
        // the status itself is always current (the status bar polls `active()`)
        assert_eq!(lock(&p.status).phase, JobPhase::Parsing { items: n });
    }

    #[test]
    fn progress_computes_fractions() {
        let p = Progress::detached();
        p.set(JobPhase::Downloading { done: 25, total: Some(100) });
        assert_eq!(lock(&p.status).fraction, Some(0.25));
        p.set(JobPhase::Writing { done: 10, total: None });
        assert_eq!(lock(&p.status).fraction, None);
        p.set_fraction(Some(3.0));
        assert_eq!(lock(&p.status).fraction, Some(1.0));
        p.set(JobPhase::Matching);
        assert_eq!(lock(&p.status).fraction, Some(1.0));
    }

    #[test]
    fn labels_name_the_subject() {
        let k = JobKind::RefreshSource { source: SourceId(1), force: false };
        assert_eq!(label_for(&k, None), "Refreshing playlist");
        assert_eq!(label_for(&k, Some("My IPTV")), "Refreshing My IPTV");
        assert_eq!(failure_for(&k, Some("My IPTV")), "Couldn't refresh My IPTV");
        let p = Progress::attached(EventBus::new(None).0, JobStatus { kind: k, ..status(1) });
        p.set_subject("My IPTV");
        assert_eq!(lock(&p.status).label, "Refreshing My IPTV");
        assert_eq!(p.subject().as_deref(), Some("My IPTV"));
    }
}
