//! One downloads actor, per-account workers, and a bounded watch snapshot (DESIGN §43 §5).
//! Commands are acknowledged after their effects: PauseAndPlay cannot race a closing socket.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{DownloadCommand, DownloadReply, DownloadsSnapshot, Job, Kind, SeriesScope, State, Target, planning, scheduler, worker};
use crate::config::{SharedConfig, current};
use crate::error::{Error, Result};
use crate::model::{DownloadId, ItemKind, ItemRef, SourceId};
use crate::msg::{EventBus, Toast};
use crate::player::budget::{Lease, SharedBudget};
use crate::player::tiles::TileRegistry;
use crate::shared::AppShared;
use crate::sources::http::HttpClient;
use crate::store::{Store, downloads as db};

#[derive(Clone, Debug)]
pub struct DownloadsHandle {
    tx: mpsc::UnboundedSender<(DownloadCommand, Option<oneshot::Sender<DownloadReply>>)>,
    snapshot: watch::Receiver<Arc<DownloadsSnapshot>>,
    budget: SharedBudget,
}
impl DownloadsHandle {
    pub fn budget(&self) -> &SharedBudget {
        &self.budget
    }
    pub fn send(&self, command: DownloadCommand) {
        let _ = self.tx.send((command, None));
    }
    pub async fn request(&self, command: DownloadCommand) -> DownloadReply {
        let (tx, rx) = oneshot::channel();
        if self.tx.send((command, Some(tx))).is_err() {
            return DownloadReply { error: Some("Downloads are unavailable".into()), ..Default::default() };
        }
        rx.await.unwrap_or_else(|_| DownloadReply { error: Some("Downloads stopped".into()), ..Default::default() })
    }
    pub fn snapshot(&self) -> Arc<DownloadsSnapshot> {
        self.snapshot.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<DownloadsSnapshot>> {
        self.snapshot.clone()
    }
}

pub struct Deps {
    pub store: Store,
    pub config: SharedConfig,
    pub shared: Arc<AppShared>,
    pub budget: SharedBudget,
    pub http: HttpClient,
    pub bus: EventBus,
    pub tiles: TileRegistry,
    pub paths: crate::paths::AppPaths,
}

struct Running {
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: Arc<Mutex<worker::Progress>>,
    task: tokio::task::JoinHandle<(Job, Result<()>)>,
    lease: Option<Lease>,
}

pub fn spawn(rt: &tokio::runtime::Handle, deps: Deps) -> DownloadsHandle {
    let arming =
        super::arming::RecordingArmer::for_profile(&deps.paths).map(|a| Arc::new(Mutex::new(crate::platform::systemd::ArmSync::new(Arc::new(a)))));
    let (tx, rx) = mpsc::unbounded_channel();
    let (snapshot, watch) = watch::channel(Arc::default());
    let command_tx = tx.clone();
    let budget = deps.budget.clone();
    rt.spawn(async move {
        let mut engine = Engine {
            deps,
            resources: super::limits::Resources::default(),
            arming,
            command_tx,
            running: HashMap::new(),
            paused: false,
            failed: HashMap::new(),
            paused_accounts: HashSet::new(),
            undo: HashMap::new(),
            snapshot,
            last_progress: (Instant::now(), 0),
            closing: false,
            maintenance_at: Instant::now() - Duration::from_secs(60),
        };
        if engine.reconcile().await.is_err() {
            engine.deps.bus.toast(Toast::error("Downloads could not read their saved state"));
        }
        if let Ok(c) = engine.deps.store.reader()
            && let Ok(rows) = db::jobs(&c)
        {
            for mut job in rows {
                engine.resources.job(job.id).reset(job.bytes_done);
                if job.state == State::Cancelled && job.reason.as_deref() == Some("delete pending") {
                    let left = job.fp.get("download_delete_at").and_then(|v| v.as_i64()).unwrap_or(0) - crate::model::now();
                    let elapsed = (10 - left.clamp(0, 10)) as u64;
                    job.state = job.fp.get("download_deleted_state").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or(State::Waiting);
                    job.reason = None;
                    engine.undo.insert(job.id, (Instant::now() - Duration::from_secs(elapsed), job));
                }
            }
        }
        engine.run(rx).await;
    });
    DownloadsHandle { tx, snapshot: watch, budget }
}

struct Engine {
    resources: super::limits::Resources,
    deps: Deps,
    arming: Option<Arc<Mutex<crate::platform::systemd::ArmSync>>>,
    command_tx: mpsc::UnboundedSender<(DownloadCommand, Option<oneshot::Sender<DownloadReply>>)>,
    running: HashMap<DownloadId, Running>,
    paused: bool,
    failed: HashMap<SourceId, u8>,
    paused_accounts: HashSet<SourceId>,
    undo: HashMap<DownloadId, (Instant, Job)>,
    snapshot: watch::Sender<Arc<DownloadsSnapshot>>,
    last_progress: (Instant, u64),
    closing: bool,
    maintenance_at: Instant,
}

impl Engine {
    async fn reconcile(&self) -> Result<()> {
        let cfg = current(&self.deps.config);
        self.deps
            .store
            .writer()
            .run(move |c| {
                let root = db::root(c, &cfg.downloads)?;
                db::reconcile(c, &root)
            })
            .await
    }

    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<(DownloadCommand, Option<oneshot::Sender<DownloadReply>>)>) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some((command,reply)) = command else { break };
                    let result = self.command(command).await;
                    let result = result.unwrap_or_else(|_| DownloadReply { error:Some("Downloads could not save that change".into()),..Default::default() });
                    if let Some(error) = &result.error { self.deps.bus.toast(Toast::error(error)); }
                    if let Some(reply) = reply { let _ = reply.send(result); }
                }
                _ = tick.tick() => { if self.tick().await.is_err() { tracing::warn!("downloads: state update failed"); } }
            }
            if self.closing {
                break;
            }
        }
        let ids: Vec<_> = self.running.keys().copied().collect();
        for id in ids {
            let _ = self.stop(id, State::Waiting, None).await;
        }
    }

    fn root(&self) -> Result<PathBuf> {
        db::root(&self.deps.store.reader()?, &current(&self.deps.config).downloads)
    }

    async fn save(&self, job: Job) -> Result<()> {
        self.deps.store.writer().run(move |c| db::put(c, &job)).await
    }

    fn visible(&self, job: &Job) -> bool {
        let Ok(c) = self.deps.store.reader() else { return false };
        let cfg = current(&self.deps.config);
        !planning::concealed(&c, &self.deps.shared.library(), self.deps.shared.parental(&cfg.parental), job)
    }

    async fn add(&self, targets: Vec<Target>, agent: bool) -> Result<DownloadReply> {
        let mut reply = DownloadReply::default();
        let cfg = current(&self.deps.config);
        let lib = self.deps.shared.library();
        let view = self.deps.shared.parental(&cfg.parental);
        let c = self.deps.store.reader()?;
        let root = db::root(&c, &cfg.downloads)?;
        let reserved: u64 = db::jobs(&c)?
            .iter()
            .filter(|j| matches!(j.state, State::Waiting | State::Scheduled | State::Running))
            .map(|j| db::estimate(j).saturating_sub(j.bytes_done))
            .sum();
        let used = db::total_bytes(&c)?;
        let free = crate::sources::http::free_bytes(&root).unwrap_or(0);
        let mut jobs = Vec::new();
        for target in targets {
            match planning::new_job(&c, &lib, view, &cfg, &target, agent, crate::model::now()) {
                Ok(job) => {
                    if db::duplicate(&c, &job)? || jobs.iter().any(|other| db::same_target(&job, other)) {
                        reply.refused.push((target.item, "duplicate".into()));
                    } else {
                        jobs.push(job);
                    }
                }
                Err(e) => reply.refused.push((
                    target.item,
                    match e {
                        Error::Other(s) => s,
                        _ => "not_found".into(),
                    },
                )),
            }
        }
        // A menu choice is admitted as a whole, including a season larger than the MCP
        // batch limit. Refusing space must not leave the first half queued (§2.3).
        let size: u64 = jobs.iter().map(db::estimate).sum();
        if !scheduler::space_allows(
            used.saturating_add(reserved),
            free.saturating_sub(reserved),
            cfg.downloads.cap_gb.saturating_mul(1_000_000_000),
            cfg.downloads.reserve_gb.saturating_mul(1_000_000_000),
            size,
        ) {
            reply.refused.extend(jobs.iter().filter_map(|j| j.item()).map(|i| (i, "no_space".into())));
            return Ok(reply);
        }
        reply.added = self.deps.store.writer().run(move |c| crate::store::sql::atomic(c, |c| jobs.iter().map(|j| db::add(c, j)).collect())).await?;
        reply.size_bytes = size;
        Ok(reply)
    }

    async fn stop(&mut self, id: DownloadId, state: State, reason: Option<&str>) -> Result<()> {
        if let Some(mut run) = self.running.remove(&id) {
            run.cancel.cancel();
            let result = run.task.await.map_err(Error::other)?;
            if let Some(lease) = run.lease.take() {
                self.deps.budget.release(id, lease, None);
            }
            let (mut job, _) = result;
            let progress = run.progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
            update_progress(&mut job, &progress);
            self.resources.job(id).reset(job.bytes_done);
            if !job.state.playable() && (job.state != State::Finishing || !job.kind.recording()) {
                job.state = state;
            }
            job.reason = reason.map(str::to_owned);
            self.save(job).await?;
        } else {
            let reason = reason.map(str::to_owned);
            self.deps.store.writer().run(move |c| db::set_state(c, id, state, reason.as_deref())).await?;
        }
        Ok(())
    }

    async fn command(&mut self, command: DownloadCommand) -> Result<DownloadReply> {
        let done = DownloadReply::default();
        match command {
            DownloadCommand::Add { targets, agent } => {
                if agent && targets.len() > 50 {
                    return Ok(DownloadReply { error: Some("Add up to 50 items at a time".into()), ..done });
                }
                return self.add(targets, agent).await;
            }
            DownloadCommand::Series { item, scope, agent } => {
                let c = self.deps.store.reader()?;
                let targets = planning::series_targets(&c, &item, scope)?;
                if let SeriesScope::Keep(n) = scope {
                    // Validate the same concealed/locked reference before persisting a rule.
                    let cfg = current(&self.deps.config);
                    if let Some(target) = targets.first() {
                        planning::new_job(
                            &c,
                            &self.deps.shared.library(),
                            self.deps.shared.parental(&cfg.parental),
                            &cfg,
                            target,
                            agent,
                            crate::model::now(),
                        )?;
                    }
                    let sid = planning::series_id(&c, &item)?;
                    let series = crate::store::vod::series(&c, sid)?;
                    let item = ItemRef { source: series.source_id, kind: ItemKind::Series, key: series.key };
                    let mut reply = self.add(targets, agent).await?;
                    if reply.refused.iter().any(|(_, reason)| reason != "duplicate") {
                        return Ok(reply);
                    }
                    let ids = reply.added.clone();
                    let id = self
                        .deps
                        .store
                        .writer()
                        .run(move |c| {
                            crate::store::sql::atomic(c, |c| {
                                let id = db::keep(c, &item, n, crate::model::now())?;
                                for job in ids {
                                    c.execute(
                                        "UPDATE download SET fp=json_set(fp,'$.download_keep_rule',?2) WHERE id=?1",
                                        rusqlite::params![job, id],
                                    )?;
                                }
                                Ok(id)
                            })
                        })
                        .await?;
                    reply.rule = Some(id);
                    reply.refused.clear();
                    return Ok(reply);
                }
                return self.add(targets, agent).await;
            }
            DownloadCommand::Cancel(ids) => {
                for id in ids {
                    if let Some(job) = db::job(&self.deps.store.reader()?, id)?
                        && self.visible(&job)
                    {
                        self.stop(id, State::Cancelled, None).await?;
                        let mut job = db::job(&self.deps.store.reader()?, id)?.unwrap_or(job);
                        remove_files(&self.root()?, &job)?;
                        job.state = State::Cancelled;
                        job.store_name = None;
                        job.bytes_done = 0;
                        self.resources.job(job.id).reset(0);
                        job.reason = Some("deleted".into());
                        self.save(job).await?;
                    }
                }
            }
            DownloadCommand::Pause(ids) => {
                for id in ids {
                    if db::job(&self.deps.store.reader()?, id)?
                        .is_some_and(|j| self.visible(&j) && !j.state.playable() && j.state != State::Cancelled)
                    {
                        self.stop(id, State::Paused, None).await?;
                    }
                }
            }
            DownloadCommand::Resume(ids) | DownloadCommand::Retry(ids) => {
                for id in ids {
                    if let Some(mut job) = db::job(&self.deps.store.reader()?, id)?
                        && self.visible(&job)
                        && !job.state.playable()
                        && job.state != State::Cancelled
                    {
                        job.state = if job.kind.recording() && job.path(&self.root()?).is_some_and(|p| p.is_dir()) {
                            State::Finishing
                        } else {
                            State::Waiting
                        };
                        job.attempts = 0;
                        job.done_at = None;
                        job.reason = None;
                        job.fp["download_capture_failed"] = false.into();
                        job.fp["download_retry_at"] = serde_json::Value::Null;
                        job.fp["download_finish_failed"] = false.into();
                        if let Some(source) = job.source_id {
                            self.paused_accounts.remove(&source);
                            self.failed.remove(&source);
                        }
                        self.save(job).await?;
                    }
                }
            }
            DownloadCommand::Delete(ids) => {
                for id in ids {
                    if let Some(job) = db::job(&self.deps.store.reader()?, id)?
                        && self.visible(&job)
                    {
                        self.stop(id, State::Cancelled, Some("delete pending")).await?;
                        let mut kept = db::job(&self.deps.store.reader()?, id)?.unwrap_or_else(|| job.clone());
                        if kept.state != State::Finishing {
                            kept.state = job.state;
                        }
                        kept.reason = job.reason;
                        let mut pending = kept.clone();
                        pending.fp["download_deleted_state"] = serde_json::to_value(kept.state).unwrap_or_default();
                        pending.fp["download_delete_at"] = (crate::model::now() + 10).into();
                        pending.state = State::Cancelled;
                        pending.reason = Some("delete pending".into());
                        self.save(pending).await?;
                        self.undo.insert(id, (Instant::now(), kept));
                    }
                }
            }
            DownloadCommand::Undo(ids) => {
                for id in ids {
                    if let Some((_, mut job)) = self.undo.remove(&id) {
                        if job.state == State::Running {
                            job.state = State::Waiting;
                        }
                        self.save(job).await?;
                    }
                }
            }
            DownloadCommand::Move { id, to } => {
                self.deps.store.writer().run(move |c| db::move_to(c, id, to)).await?;
            }
            DownloadCommand::Pin { id, item } => {
                self.stop(id, State::Paused, None).await?;
                if let Some(mut job) = db::job(&self.deps.store.reader()?, id)?
                    && self.visible(&job)
                {
                    remove_files(&self.root()?, &job)?;
                    planning::change_copy(&self.deps.store.reader()?, &self.deps.shared.library(), &current(&self.deps.config), &mut job, item)?;
                    job.any_copy = false;
                    job.bytes_done = 0;
                    self.resources.job(job.id).reset(0);
                    job.bytes_total = None;
                    job.store_name = None;
                    job.fp["download_validators"] = serde_json::Value::Null;
                    job.state = State::Waiting;
                    self.save(job).await?;
                }
            }
            DownloadCommand::PauseAll(on) => {
                self.paused = on;
                if on {
                    let ids: Vec<_> = self.running.keys().copied().collect();
                    for id in ids {
                        self.stop(id, State::Paused, Some("all")).await?;
                    }
                } else {
                    let rows = db::jobs(&self.deps.store.reader()?)?;
                    for mut job in rows {
                        if job.reason.as_deref() == Some("all") {
                            job.state = if job.path(&self.root()?).is_some_and(|p| p.is_dir()) { State::Finishing } else { State::Waiting };
                            job.reason = None;
                            self.save(job).await?;
                        }
                    }
                }
            }
            DownloadCommand::PauseForPlay(source) => {
                let ids: Vec<_> =
                    self.running.iter().filter(|(_, r)| r.lease.as_ref().is_some_and(|l| l.source == source)).map(|(id, _)| *id).collect();
                for id in ids {
                    self.stop(id, State::Paused, Some("playback")).await?;
                }
            }
            DownloadCommand::Clash { id, record, alternate } => {
                if let Some(mut job) = db::job(&self.deps.store.reader()?, id)?
                    && self.visible(&job)
                {
                    if !record {
                        job.reason = Some("watching".into());
                    } else if alternate {
                        if let Some(item) = planning::alternate(&self.deps.store.reader()?, &self.deps.shared.library(), &self.deps.budget, &job)? {
                            planning::change_copy(
                                &self.deps.store.reader()?,
                                &self.deps.shared.library(),
                                &current(&self.deps.config),
                                &mut job,
                                item,
                            )?;
                            job.any_copy = false;
                            job.state = State::Waiting;
                            job.reason = None;
                        } else {
                            return Ok(DownloadReply { error: Some("No other account is free".into()), ..done });
                        }
                    } else {
                        let downloads: Vec<_> = self
                            .running
                            .iter()
                            .filter(|(other, run)| **other != id && run.lease.as_ref().is_some_and(|l| Some(l.source) == job.source_id))
                            .map(|(id, _)| *id)
                            .collect();
                        for other in downloads {
                            self.stop(other, State::Paused, Some("recording")).await?;
                        }
                        for player in self.deps.tiles.all() {
                            if player.snapshot().playable.as_ref().is_some_and(|p| p.source_id == job.source_id) {
                                player.send(crate::player::PlayerCommand::Stop);
                            }
                        }
                        job.state = State::Waiting;
                        job.reason = None;
                        job.fp["download_clash_at"] = serde_json::Value::Null;
                    }
                    self.save(job).await?;
                }
            }
            DownloadCommand::StopRecording(id) => {
                if let Some(run) = self.running.get(&id) {
                    run.stop.cancel();
                }
            }
            DownloadCommand::MoveFolder(path) => {
                self.move_folder(path).await?;
            }
            DownloadCommand::Refresh => {}
            DownloadCommand::Shutdown => {
                self.closing = true;
                let ids: Vec<_> = self.running.keys().copied().collect();
                for id in ids {
                    self.stop(id, State::Waiting, None).await?;
                }
            }
        }
        Ok(done)
    }

    fn failed_job(&mut self, source: SourceId) {
        let count = self.failed.entry(source).or_default();
        *count += 1;
        if *count >= 3 {
            self.paused_accounts.insert(source);
            self.deps.bus.toast(Toast::info("Downloads paused on this account: 3 jobs failed in a row"));
        }
    }

    async fn tick(&mut self) -> Result<()> {
        if self.maintenance_at.elapsed() >= Duration::from_secs(5) {
            self.maintenance_at = Instant::now();
            self.maintain().await?;
        }
        let completed: Vec<_> = self.running.iter().filter(|(_, r)| r.task.is_finished()).map(|(id, _)| *id).collect();
        for id in completed {
            let mut run = self.running.remove(&id).expect("completed worker");
            let (mut job, result) = run.task.await.map_err(Error::other)?;
            let at_limit = job
                .source_id
                .is_some_and(|source| self.deps.budget.usage().iter().any(|u| u.source == source && u.limit.is_some_and(|limit| u.in_use >= limit)));
            if let Some(lease) = run.lease.take() {
                self.deps.budget.release(id, lease, None);
            }
            update_progress(&mut job, &run.progress.lock().unwrap_or_else(|e| e.into_inner()));
            match result {
                Ok(()) => {
                    if let Some(source) = job.source_id {
                        if job.state == State::Failed {
                            self.failed_job(source);
                        } else {
                            self.failed.remove(&source);
                        }
                    }
                }
                Err(e) => {
                    if job.state == State::Finishing {
                        job.reason = Some("finishing failed".into());
                        job.fp["download_finish_failed"] = true.into();
                    } else if matches!(e,Error::Other(ref s) if s=="space") {
                        job.state = State::Waiting;
                        job.reason = Some("space".into());
                    } else if matches!(e, Error::Http { status: 403, .. }) && at_limit {
                        job.state = State::Waiting;
                        job.reason = Some("busy".into());
                        job.fp["download_retry_at"] = (crate::model::now() + 30).into();
                    } else {
                        job.attempts += 1;
                        job.reason = Some(match e {
                            Error::Http { status, .. } => format!("status-{status}"),
                            _ => "connection failed".into(),
                        });
                        if let Some(delay) = scheduler::retry_delay(job.attempts) {
                            job.state = State::Waiting;
                            job.fp["download_retry_at"] = (crate::model::now() + delay).into();
                        } else {
                            job.state = State::Failed;
                            if let Some(source) = job.source_id {
                                self.failed_job(source);
                            }
                        }
                    }
                }
            }
            self.resources.job(id).reset(job.bytes_done);
            let complete = job.done_at.is_some();
            self.save(job).await?;
            if complete {
                let _ = std::fs::remove_file(self.root()?.join(format!("{id}.mkv.ok")));
            }
        }
        let expired: Vec<_> = self.undo.iter().filter(|(_, (at, _))| at.elapsed() >= Duration::from_secs(10)).map(|(id, _)| *id).collect();
        for id in expired {
            if let Some((_, mut job)) = self.undo.remove(&id) {
                remove_files(&self.root()?, &job)?;
                job.state = State::Cancelled;
                job.store_name = None;
                job.bytes_done = 0;
                self.resources.job(job.id).reset(0);
                job.reason = Some("deleted".into());
                self.save(job).await?;
            }
        }
        let cfg = current(&self.deps.config);
        let mut rows = db::jobs(&self.deps.store.reader()?)?;
        for job in &mut rows {
            if let Some(run) = self.running.get_mut(&job.id) {
                let p = run.progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
                update_progress(job, &p);
                if p.finishing.is_some() {
                    job.fp["download_partial"] = p.partial.into();
                    job.fp["download_capture_failed"] = p.capture_failed.into();
                    job.state = State::Finishing;
                    if let Some(lease) = run.lease.take() {
                        self.deps.budget.release(job.id, lease, None);
                    }
                }
                self.save(job.clone()).await?;
            }
        }
        let now = crate::model::now();
        let actual_used: u64 = rows.iter().map(|j| j.bytes_done).sum();
        let mut used = actual_used;
        let root = self.root()?;
        let free = crate::sources::http::free_bytes(&root).unwrap_or(0);
        let cap = cfg.downloads.cap_gb.saturating_mul(1_000_000_000);
        let reserve = cfg.downloads.reserve_gb.saturating_mul(1_000_000_000);
        self.resources.configure(cap, cfg.downloads.max_mbps);
        let mut pending = rows.clone();
        pending.sort_by_key(|j| (!j.kind.recording(), j.position));
        for mut job in pending {
            if self.running.contains_key(&job.id) || self.undo.contains_key(&job.id) {
                continue;
            }
            if job.state == State::Paused
                && matches!(job.reason.as_deref(), Some("playback" | "recording"))
                && job.source_id.is_some_and(|s| self.deps.budget.admits(job.id, s))
            {
                job.state = State::Waiting;
                job.reason = None;
                self.save(job.clone()).await?;
            }
            if self.paused
                || !matches!(job.state, State::Waiting | State::Scheduled | State::Finishing)
                || job.source_id.is_some_and(|s| self.paused_accounts.contains(&s))
            {
                continue;
            }
            if job.fp.get("download_retry_at").and_then(serde_json::Value::as_i64).is_some_and(|at| now < at)
                || job.fp.get("download_finish_failed").and_then(serde_json::Value::as_bool) == Some(true)
            {
                continue;
            }
            if job.state != State::Finishing {
                if !job.kind.recording() {
                    let time = jiff::Zoned::now();
                    let minute = time.hour() as u32 * 60 + time.minute() as u32;
                    if !scheduler::window_allows(&cfg.downloads.window, minute) {
                        job.reason = Some("window".into());
                        self.save(job).await?;
                        continue;
                    }
                }
                let c = self.deps.store.reader()?;
                let adopt = job.kind == Kind::Live && planning::media(&c, &job).is_ok_and(|m| self.deps.budget.has_live(&m));
                let busy = !adopt && job.source_id.is_some_and(|s| !self.deps.budget.admits(job.id, s));
                let catchup = catchup_available(&c, &job, now);
                match scheduler::due(&job, now, busy, catchup) {
                    scheduler::Due::Wait => {
                        if job.fp.get("download_clash_at").and_then(serde_json::Value::as_i64).is_some_and(|at| now >= at + 60) {
                            job.reason = Some("watching".into());
                            self.save(job).await?;
                        }
                        continue;
                    }
                    scheduler::Due::Ask => {
                        if !self.visible(&job) {
                            job.reason = Some("watching".into());
                            self.save(job).await?;
                            continue;
                        }
                        if job.fp.get("download_clash_at").and_then(serde_json::Value::as_i64).is_none() {
                            job.fp["download_clash_at"] = now.into();
                            job.reason = Some("clash".into());
                            job.fp["download_alternate"] =
                                planning::alternate(&c, &self.deps.shared.library(), &self.deps.budget, &job)?.is_some().into();
                            self.notify_due(&job);
                            self.save(job).await?;
                        }
                        continue;
                    }
                    scheduler::Due::Catchup => {
                        job.kind = Kind::Catchup;
                        job.state = State::Waiting;
                        job.reason = Some("saved from catch-up".into());
                    }
                    scheduler::Due::Missed => {
                        job.state = State::Missed;
                        if job.reason.as_deref() != Some("watching") {
                            job.reason = Some("missed".into());
                        }
                        self.save(job).await?;
                        continue;
                    }
                    scheduler::Due::Start => {}
                }
                let Some(item) = (if adopt { job.item() } else { planning::pick(&c, &self.deps.shared.library(), &cfg, &self.deps.budget, &job)? })
                else {
                    job.reason = Some("busy".into());
                    self.save(job).await?;
                    continue;
                };
                if job.item().as_ref() != Some(&item) {
                    planning::change_copy(&c, &self.deps.shared.library(), &cfg, &mut job, item)?;
                }
                let size = db::estimate(&job).saturating_sub(job.bytes_done);
                if !scheduler::space_allows(used, free, cap, reserve, size) {
                    job.reason = Some("space".into());
                    self.save(job).await?;
                    continue;
                }
                used = used.saturating_add(size);
            }
            if self.start(job.clone(), cap.saturating_sub(used), reserve).await.is_err() {
                job.attempts += 1;
                job.reason = Some("Could not open this item".into());
                job.state = if let Some(delay) = scheduler::retry_delay(job.attempts) {
                    job.fp["download_retry_at"] = (now + delay).into();
                    State::Waiting
                } else {
                    if let Some(source) = job.source_id {
                        self.failed_job(source);
                    }
                    State::Failed
                };
                self.save(job).await?;
            }
        }
        self.publish(&rows, actual_used, free, cap);
        Ok(())
    }

    async fn start(&mut self, mut job: Job, _available: u64, reserve: u64) -> Result<()> {
        let cfg = current(&self.deps.config);
        let root = self.root()?;
        let (playable, lease, adopted) = if job.state == State::Finishing {
            (None, None, None)
        } else {
            let c = self.deps.store.reader()?;
            let media = planning::media(&c, &job)?;
            let p = crate::sources::resolve::resolve_provider(&c, &cfg, &media, false, crate::model::now())?;
            let Some(source) = p.source_id else {
                return Err(Error::other("not_found"));
            };
            let shared = if job.kind == Kind::Live && self.deps.budget.has_live(&media) { self.deps.budget.share_live(&media, job.id) } else { None };
            let (lease, adopted) = if let Some((buffer, lease)) = shared {
                let from =
                    buffer.snapshot().range.map_or(0.0, |r| r.end - (crate::model::now() - job.start().unwrap_or(crate::model::now())).max(0) as f64);
                if buffer
                    .adopt(
                        &root.join(job.id.to_string()),
                        from,
                        crate::player::hls::RecordingLimits { reserve_bytes: reserve, max_bytes: u64::MAX, limits: Some(self.resources.job(job.id)) },
                    )
                    .is_err()
                {
                    self.deps.budget.release(job.id, lease, None);
                    return Err(Error::other("Could not keep the live recording"));
                }
                (lease, Some(buffer))
            } else {
                let Ok(lease) = self.deps.budget.acquire(job.id, source) else {
                    return Ok(());
                };
                (lease, None)
            };
            let hls = url::Url::parse(p.url.expose()).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".m3u8"));
            if hls {
                job.store_name = Some(format!("{}/", job.id));
            }
            job.state = State::Running;
            (Some(p), Some(lease), adopted)
        };
        self.save(job.clone()).await?;
        let progress = Arc::new(Mutex::new(worker::Progress::default()));
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let work = worker::Work {
            job: job.clone(),
            playable,
            root,
            cache: self.deps.paths.cache_dir.clone(),
            budget: self.deps.budget.clone(),
            adopted,
            http: self.deps.http.clone(),
            max_mbps: cfg.downloads.max_mbps,
            limits: self.resources.job(job.id),
            reserve,
            available: u64::MAX,
            progress: progress.clone(),
            cancel: cancel.clone(),
            stop_recording: stop.clone(),
        };
        let task = tokio::spawn(work.run());
        self.running.insert(job.id, Running { cancel, stop, progress, task, lease });
        tracing::info!("download: #{} running ({}, source {})", job.id, job.kind.as_str(), job.source_id.map_or(0, |s| s.0));
        Ok(())
    }

    fn publish(&mut self, rows: &[Job], used: u64, free: u64, cap: u64) {
        let elapsed = self.last_progress.0.elapsed().as_secs_f64().max(0.001);
        let previous = self.snapshot.borrow().clone();
        let mut jobs: Vec<_> = rows.iter().filter(|j| j.state != State::Cancelled && self.visible(j)).cloned().collect();
        let mut speed = 0.0;
        for job in &mut jobs {
            let before = previous.jobs.iter().find(|j| j.id == job.id).map_or(job.bytes_done, |j| j.bytes_done);
            let rate = if job.state == State::Running { job.bytes_done.saturating_sub(before) as f64 / elapsed } else { 0.0 };
            job.fp["download_speed_bps"] = rate.into();
            speed += rate;
        }
        // Active work always survives the bound; retain the newest completed files next.
        jobs.sort_by_key(|j| {
            (
                if j.state.active() {
                    0
                } else if matches!(j.state, State::Waiting | State::Scheduled | State::Paused) {
                    1
                } else {
                    2
                },
                if j.state.playable() { -j.done_at.unwrap_or(j.added_at) } else { j.position },
            )
        });
        jobs.truncate(500);
        self.last_progress = (Instant::now(), used);
        self.snapshot.send_replace(Arc::new(DownloadsSnapshot {
            jobs,
            used_bytes: used,
            free_bytes: free,
            cap_bytes: cap,
            speed_bps: speed,
            paused: self.paused,
        }));
    }

    fn notify_due(&self, job: &Job) {
        if self.deps.shared.window().is_some_and(|w| w.visible) {
            return;
        }
        let id = job.id;
        let title = self.deps.shared.guard().outside_title(&job.title, crate::model::now()).into_owned();
        let tx = self.command_tx.clone();
        let alternate = self
            .deps
            .store
            .reader()
            .ok()
            .and_then(|c| planning::alternate(&c, &self.deps.shared.library(), &self.deps.budget, job).ok().flatten())
            .is_some();
        tokio::task::spawn_blocking(move || {
            let mut note = notify_rust::Notification::new();
            note.summary("Recording starts in 60 seconds")
                .body(&title)
                .appname("Tuner")
                .timeout(60_000)
                .action("record", "Record (stops this)")
                .action("skip", "Skip");
            if alternate {
                note.action("alternate", "Record from another account");
            }
            if let Ok(note) = note.show() {
                note.wait_for_action(|action| {
                    if matches!(action, "record" | "alternate" | "skip") {
                        let _ = tx.send((DownloadCommand::Clash { id, record: action != "skip", alternate: action == "alternate" }, None));
                    }
                });
            }
        });
    }

    async fn maintain(&mut self) -> Result<()> {
        let cfg = current(&self.deps.config);
        let lib = self.deps.shared.library();
        let view = self.deps.shared.parental(&cfg.parental);
        let now = crate::model::now();
        let c = self.deps.store.reader()?;
        for mut job in db::jobs(&c)? {
            if !self.visible(&job) || !job.state.playable() {
                continue;
            }
            let progress =
                (!job.kind.recording()).then(|| job.item()).flatten().and_then(|i| super::maintenance::progress(&c, &lib, &i).ok().flatten());
            let watched = progress.as_ref().map_or(job.watched_at.is_some(), |p| p.finished);
            if let Some(p) = progress {
                job.fp["download_position_s"] = p.position_s.into();
                self.save(job.clone()).await?;
            }
            if watched && job.watched_at.is_none() {
                job.watched_at = Some(now);
                self.save(job.clone()).await?;
            }
            if !watched && job.item().is_some() && job.watched_at.is_some() {
                job.watched_at = None;
                self.save(job.clone()).await?;
            }
            if cfg.downloads.delete_after_watched_days > 0
                && job.watched_at.is_some_and(|at| now >= at + i64::from(cfg.downloads.delete_after_watched_days) * 86400)
            {
                remove_files(&self.root()?, &job)?;
                job.state = State::Cancelled;
                job.store_name = None;
                job.bytes_done = 0;
                self.resources.job(job.id).reset(0);
                job.reason = Some("deleted watched".into());
                self.save(job).await?;
            }
        }
        self.deps
            .store
            .writer()
            .run(move |c| {
                super::maintenance::following(c, &lib, &cfg, now)?;
                super::maintenance::retime(c, now)?;
                super::maintenance::refill(c, &lib, &cfg, view, now)
            })
            .await?;
        if let Some(arming) = &self.arming {
            let want = super::arming::plan(&self.deps.store.reader()?, now)?;
            let arming = arming.clone();
            tokio::task::spawn_blocking(move || arming.lock().unwrap_or_else(|e| e.into_inner()).sync(&want)).await.map_err(Error::other)?;
        }
        Ok(())
    }

    async fn move_folder(&mut self, path: PathBuf) -> Result<()> {
        if !path.is_absolute() {
            return Err(Error::other("Choose an absolute folder path"));
        }
        let ids: Vec<_> = self.running.keys().copied().collect();
        for id in ids {
            self.stop(id, State::Waiting, None).await?;
        }
        let old = self.root()?;
        if std::fs::canonicalize(&old).ok().zip(std::fs::canonicalize(&path).ok()).is_some_and(|(a, b)| a == b) {
            return Ok(());
        }
        let to = path.clone();
        let from = old.clone();
        tokio::task::spawn_blocking(move || copy_folder(&from, &to)).await.map_err(Error::other)??;
        let mut cfg = (*current(&self.deps.config)).clone();
        cfg.downloads.dir = path.to_string_lossy().into_owned();
        cfg.save(&self.deps.paths.config_file())?;
        *self.deps.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
        if old != path {
            std::fs::remove_dir_all(&old).map_err(|e| Error::io(&old, e))?;
        }
        Ok(())
    }
}

fn update_progress(job: &mut Job, p: &worker::Progress) {
    if p.busy {
        job.reason = Some("busy".into());
    } else if job.reason.as_deref() == Some("busy") {
        job.reason = None;
    }
    if let Some(name) = &p.store_name
        && job.done_at.is_none()
    {
        job.store_name = Some(name.clone());
    }
    if p.bytes > 0 && job.done_at.is_none() {
        job.bytes_done = p.bytes;
    }
    if p.total.is_some() && job.done_at.is_none() {
        job.bytes_total = p.total;
    }
    if let Some(v) = &p.validators {
        job.etag = v.etag.clone();
        job.last_modified = v.last_modified.clone();
        job.fp["download_validators"] = serde_json::to_value(v).unwrap_or_default();
    }
    if let Some(n) = p.finishing {
        job.fp["download_finishing"] = n.into();
    }
}

fn catchup_available(c: &rusqlite::Connection, j: &Job, now: i64) -> bool {
    let Ok(crate::model::MediaRef::Channel(id)) = planning::media(c, j) else { return false };
    crate::store::live::channel(c, id)
        .ok()
        .and_then(|ch| ch.catchup)
        .is_some_and(|spec| j.prog_start.is_some_and(|start| start >= now - i64::from(spec.days) * 86_400))
}

fn remove_files(root: &std::path::Path, j: &Job) -> Result<()> {
    for entry in std::fs::read_dir(root).map_err(|e| Error::io(root, e))? {
        let entry = entry.map_err(|e| Error::io(root, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.split('.').next() != Some(j.id.0.to_string().as_str()) {
            continue;
        }
        let path = entry.path();
        if entry.file_type().map_err(|e| Error::io(&path, e))?.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|e| Error::io(&path, e))?;
        } else {
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        }
    }
    Ok(())
}

fn copy_folder(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if from == to {
        return Ok(());
    }
    let canonical_from = std::fs::canonicalize(from).map_err(|e| Error::io(from, e))?;
    let canonical_to = if to.exists() {
        std::fs::canonicalize(to).map_err(|e| Error::io(to, e))?
    } else {
        std::fs::canonicalize(to.parent().ok_or_else(|| Error::other("Choose a folder"))?)
            .map_err(|e| Error::io(to, e))?
            .join(to.file_name().unwrap_or_default())
    };
    if canonical_from == canonical_to {
        return Ok(());
    }
    if canonical_from.starts_with(&canonical_to) || canonical_to.starts_with(&canonical_from) {
        return Err(Error::other("Choose a folder outside the current downloads folder"));
    }
    if to.exists() && std::fs::read_dir(to).map_err(|e| Error::io(to, e))?.next().is_some() {
        return Err(Error::other("Choose an empty folder"));
    }
    std::fs::create_dir_all(to).map_err(|e| Error::io(to, e))?;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| Error::io(from, e))? {
        let entry = entry.map_err(|e| Error::io(from, e))?;
        let path = to.join(entry.file_name());
        if entry.file_type().map_err(|e| Error::io(entry.path(), e))?.is_symlink() {
            return Err(Error::other("The downloads folder contains a link"));
        }
        if entry.file_type().map_err(|e| Error::io(entry.path(), e))?.is_dir() {
            copy_folder(&entry.path(), &path)?;
        } else {
            std::fs::copy(entry.path(), &path).map_err(|e| Error::io(&path, e))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| Error::io(&path, e))?;
        }
    }
    Ok(())
}
