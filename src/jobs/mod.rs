//! Background job system on the tokio runtime. Owner: WP-06. DESIGN §5.2, §9.
//!
//! * `JobManager::submit` is non-blocking and callable from any thread (UI included).
//! * Identical jobs coalesce ONLY while the existing one is `Queued` or `Connecting` (it has not
//!   read its inputs yet): the call returns its id (a `force` request upgrades it). Once it is
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
//! * SKELETON SAFE DEFAULTS (DESIGN §24.1): until WP-06 lands, `submit` immediately reports
//!   `Finished(Err("not implemented"))`, `active` is empty and cancel is a no-op, so other WPs'
//!   code can call them.

pub mod details;
pub mod import_epg;
pub mod import_source;
pub mod maintenance;
pub mod probe;
pub mod scheduler;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::config::SharedConfig;
use crate::model::{AccountInfo, EpgSourceId, JobId, MovieId, SeriesId, SourceDraft, SourceId, SourceKind, Timestamp};
use crate::msg::EventBus;
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
    pub fn dedupe_key(&self) -> String { todo!("WP-06") }
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

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // progress/finish messages, ≤ 10 Hz per job
pub enum JobEvent {
    Updated(JobStatus),
    /// `Err` holds a redacted, user-presentable message.
    Finished { id: JobId, kind: JobKind, outcome: Result<JobOutcome, String> },
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

/// Throttled progress reporter given to a running job.
#[derive(Clone)]
pub struct Progress {
    id: JobId,
    bus: EventBus,
    status: Arc<Mutex<JobStatus>>,
    last_sent: Arc<Mutex<std::time::Instant>>,
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
        Self { id: JobId(0), bus, status: Arc::new(Mutex::new(status)), last_sent: Arc::new(Mutex::new(std::time::Instant::now())) }
    }

    /// Update the phase; emits `JobEvent::Updated` if ≥ 100 ms since the last one.
    pub fn set(&self, phase: JobPhase) { todo!("WP-06") }
    pub fn set_fraction(&self, f: Option<f32>) { todo!("WP-06") }
}

/// Handle owned by `Services`. Cheap to clone. Private fields are WP-06's to reshape.
#[derive(Clone)]
pub struct JobManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    ctx: JobContext,
    rt: tokio::runtime::Handle,
    running: Mutex<HashMap<JobId, (JobStatus, CancellationToken)>>,
    next_id: std::sync::atomic::AtomicU64,
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
            ManagerInner { ctx, rt, running: Mutex::default(), next_id: std::sync::atomic::AtomicU64::new(1) }
        });
        Self { inner }
    }

    /// Queue a job (non-blocking). Returns the id (existing one when coalesced).
    /// SKELETON SAFE DEFAULT: reports `Finished(Err)` immediately.
    pub fn submit(&self, kind: JobKind) -> JobId {
        let id = JobId(self.inner.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        tracing::info!("job {kind:?} not implemented yet (WP-06)");
        self.inner.ctx.bus.send(crate::msg::AppEvent::Job(JobEvent::Finished {
            id,
            kind,
            outcome: Err("Background tasks are not implemented yet".into()),
        }));
        id
    }

    /// Request cancellation (no-op if finished).
    pub fn cancel(&self, id: JobId) {
        if let Some((_, t)) = self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            t.cancel();
        }
    }

    /// Cancel everything (shutdown, backup restore).
    pub fn cancel_all(&self) {
        for (_, t) in self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).values() {
            t.cancel();
        }
    }

    /// Snapshot of running/queued jobs for the status bar.
    pub fn active(&self) -> Vec<JobStatus> {
        self.inner.running.lock().unwrap_or_else(|e| e.into_inner()).values().map(|(s, _)| s.clone()).collect()
    }

    pub fn context(&self) -> &JobContext { &self.inner.ctx }
    pub fn runtime(&self) -> &tokio::runtime::Handle { &self.inner.rt }
}
