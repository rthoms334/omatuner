//! The agent-connection service (DESIGN §39.5): one per app, in `Services::agent_connect`.
//!
//! The UI reads snapshots ([`AgentConnect::rows`] etc.) each frame and asks for work; all agent
//! CLIs run off the UI thread. Status reads (a refresh) run in parallel, one thread per agent;
//! changes go through ONE queue (a worker thread), so two CLI changes never overlap and a second
//! request waits for the first. Every result is toasted on the bus and the agent's status read
//! again. The start-of-version pass ([`AgentConnect::start_pass`]) waits a little after start,
//! refreshes, keeps Claude Code's plugin in sync when asked to, and leaves a [`PassOutcome`] for
//! the app.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::agents;
use super::runner::{CommandRunner, ConnectEnv, NullRunner, SystemRunner};
use super::{AgentId, AgentRow, Op, OpResult, PassOutcome, Status, Via, decide, shell_line};
use crate::config::AgentsConfig;
use crate::model::Timestamp;
use crate::msg::{EventBus, Toast, ToastKind};
use crate::paths::AppPaths;
use crate::store::Store;

/// The pass waits this long after start (the window shows first; startup work goes first).
const PASS_DELAY: Duration = Duration::from_secs(3);
/// A refresh older than this is redone when Settings ▸ AI agents opens.
const STALE: Duration = Duration::from_secs(30);
/// The pass waits at most this long for a refresh someone else started (every status read
/// has its own 15 s limit; this only guards against a refresh that never ends).
const REFRESH_WAIT: Duration = Duration::from_secs(60);
const TOAST_MS: u32 = 5000;

type Job = Box<dyn FnOnce() + Send>;

/// Agent connections. Cheap to clone (an `Arc` inside).
#[derive(Clone)]
pub struct AgentConnect {
    inner: Arc<Inner>,
}

struct Inner {
    runner: RwLock<Arc<dyn CommandRunner>>,
    env: RwLock<ConnectEnv>,
    store: Store,
    bus: EventBus,
    rt: tokio::runtime::Handle,
    state: Mutex<State>,
    /// The change queue's worker (started on first use).
    queue: Mutex<Option<crossbeam_channel::Sender<Job>>>,
    /// Set at shutdown: running commands are killed, queued ones skipped.
    stop: RwLock<Arc<AtomicBool>>,
    pass_delay: RwLock<Duration>,
    /// Background tasks running or queued (tests wait for 0).
    work: AtomicUsize,
    /// Agent CLIs can run here: false with the [`NullRunner`] (off the desktop, a test profile),
    /// where the start-of-version pass isn't even started.
    live: AtomicBool,
}

#[derive(Default)]
struct State {
    /// Every agent (`NotFound` ones too), with what the UI shows.
    rows: BTreeMap<AgentId, AgentRow>,
    checked: bool,
    refreshing: bool,
    refreshed_at: Option<Instant>,
    /// Bumped when an op re-read an agent: an older refresh doesn't overwrite it.
    changed: BTreeMap<AgentId, u64>,
    usage: Vec<(String, Timestamp)>,
    outcome: Option<PassOutcome>,
    /// Claude Code's plugin version its own update called up to date (a GitHub marketplace
    /// behind this Tuner): no longer shown as an update.
    claude_current: Option<String>,
    /// opencode's settings file as of the last check (the page's note reads it every frame).
    opencode_file: Option<std::path::PathBuf>,
}

impl State {
    fn fresh() -> Self {
        let rows = AgentId::ALL.iter().map(|&id| (id, AgentRow { id, status: Status::Checking, busy: None, last: None, last_used: None })).collect();
        Self { rows, ..Default::default() }
    }
}

impl AgentConnect {
    pub fn new(runner: Arc<dyn CommandRunner>, env: ConnectEnv, store: Store, bus: EventBus, rt: tokio::runtime::Handle) -> Self {
        Self::with_stop(runner, env, store, bus, rt, Arc::default())
    }

    fn with_stop(
        runner: Arc<dyn CommandRunner>,
        env: ConnectEnv,
        store: Store,
        bus: EventBus,
        rt: tokio::runtime::Handle,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                runner: RwLock::new(runner),
                env: RwLock::new(env),
                store,
                bus,
                rt,
                state: Mutex::new(State::fresh()),
                queue: Mutex::new(None),
                stop: RwLock::new(stop),
                pass_delay: RwLock::new(PASS_DELAY),
                work: AtomicUsize::new(0),
                live: AtomicBool::new(true),
            }),
        }
    }

    /// Production: the system's CLIs, or nothing at all off the desktop (tests,
    /// `TUNER_NO_DESKTOP`) or when the app's home isn't the user's (a test profile with a fake
    /// home): no agent is ever found, no file of the user's is read and no pass runs.
    pub fn for_services(paths: &AppPaths, store: Store, bus: EventBus, rt: tokio::runtime::Handle) -> Self {
        let env = ConnectEnv::from_process();
        if crate::platform::off_desktop() || !same_dir(&paths.home, &env.home) {
            let hub = Self::new(Arc::new(NullRunner), ConnectEnv::rooted(std::path::Path::new("/nonexistent/tuner-off-desktop")), store, bus, rt);
            hub.inner.live.store(false, Ordering::SeqCst);
            return hub;
        }
        let runner = SystemRunner::default();
        let stop = runner.stopper();
        Self::with_stop(Arc::new(runner), env, store, bus, rt, stop)
    }

    /// Rows for the agents found (before the first refresh finished: `Checking` rows for all).
    pub fn rows(&self) -> Vec<AgentRow> {
        let st = self.state();
        st.rows
            .values()
            .filter(|r| !st.checked || r.status != Status::NotFound)
            .map(|r| AgentRow { last_used: st.usage.iter().find(|(n, _)| n == r.id.friendly()).map(|(_, t)| *t), ..r.clone() })
            .collect()
    }

    /// Agents not found on this computer.
    pub fn missing(&self) -> Vec<AgentId> {
        let st = self.state();
        if !st.checked {
            return Vec::new();
        }
        st.rows.values().filter(|r| r.status == Status::NotFound).map(|r| r.id).collect()
    }

    /// The newest "used" entry of any agent name ("Claude", when).
    pub fn last_used(&self) -> Option<(String, Timestamp)> {
        self.state().usage.first().cloned()
    }

    /// A refresh has finished at least once.
    pub fn checked(&self) -> bool {
        self.state().checked
    }

    pub fn refreshing(&self) -> bool {
        self.state().refreshing
    }

    /// The paths the copy texts use (`snippet`, `manual_disconnect`).
    pub fn env(&self) -> ConnectEnv {
        self.inner.env.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// [`super::manual_disconnect`] with opencode's file as of the last check (no file system
    /// look from the UI thread).
    pub fn manual_disconnect(&self, id: AgentId, env: &ConnectEnv) -> Option<super::Snippet> {
        let cached = self.state().opencode_file.clone();
        super::manual_disconnect_in(id, env, || cached.unwrap_or_else(|| agents::opencode_file(env)))
    }

    /// Re-detect and re-read every agent in the background. No-op while one runs.
    pub fn refresh(&self) {
        if !self.begin_refresh() {
            return;
        }
        self.inner.work.fetch_add(1, Ordering::SeqCst);
        // however the task ends (done, panicked, or dropped unrun at shutdown), it isn't
        // "refreshing" any more
        let done = RefreshDone { hub: self.clone(), counted: true };
        self.inner.rt.spawn_blocking(move || done.hub.read_all());
    }

    /// [`AgentConnect::refresh`] unless the last one is recent (Settings ▸ AI agents opening).
    pub fn refresh_if_stale(&self) {
        let fresh = self.state().refreshed_at.is_some_and(|t| t.elapsed() < STALE);
        if !fresh {
            self.refresh();
        }
    }

    /// Run `op` on these agents in order, in the background (after any change already queued).
    /// Marks them busy now; toasts each result and reads the agent's status again.
    pub fn run(&self, ids: &[AgentId], op: Op) {
        let ids: Vec<AgentId> = ids.iter().copied().filter(|id| id.can(op)).collect();
        if ids.is_empty() {
            return;
        }
        self.mark_busy(&ids, op);
        let this = self.clone();
        let each = ids.clone();
        self.enqueue(
            ids,
            Box::new(move || {
                for id in each {
                    this.execute(id, op, false);
                }
            }),
        );
    }

    /// The start-of-version pass (the app calls it at start when `connect::should_check`).
    /// Waits a little, refreshes, updates Claude Code's plugin when `keep_in_sync` and it's out
    /// of date, then decides what to offer (`take_pass_outcome`).
    pub fn start_pass(&self, cfg: &AgentsConfig) {
        if !self.inner.live.load(Ordering::SeqCst) {
            return;
        }
        let (offer, sync) = (cfg.offer_connect, cfg.keep_in_sync);
        let this = self.clone();
        self.inner.work.fetch_add(1, Ordering::SeqCst);
        self.inner.rt.spawn_blocking(move || {
            this.pass(offer, sync);
            this.inner.work.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// The pass's outcome, exactly once.
    pub fn take_pass_outcome(&self) -> Option<PassOutcome> {
        self.state().outcome.take()
    }

    /// Kill running agent CLIs and skip queued changes (`Services::shutdown`).
    pub fn shutdown(&self) {
        self.stop_flag().store(true, Ordering::SeqCst);
        self.inner.queue.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Swap in a scripted runner and a home of its own (tests): state starts over, the pass
    /// doesn't wait.
    #[cfg(test)]
    pub(crate) fn with_fake(&self, runner: Arc<dyn CommandRunner>, env: ConnectEnv) {
        *self.inner.runner.write().unwrap() = runner;
        *self.inner.env.write().unwrap() = env;
        *self.inner.stop.write().unwrap() = Arc::default();
        *self.inner.pass_delay.write().unwrap() = Duration::ZERO;
        self.inner.live.store(true, Ordering::SeqCst);
        *self.state() = State::fresh();
    }

    /// Wait until no refresh, pass or change runs or is queued (tests).
    #[cfg(test)]
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while self.inner.work.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    // ---- internals -------------------------------------------------------------------------

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        self.inner.runner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn stop_flag(&self) -> Arc<AtomicBool> {
        self.inner.stop.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn stopped(&self) -> bool {
        self.stop_flag().load(Ordering::SeqCst)
    }

    fn begin_refresh(&self) -> bool {
        let mut st = self.state();
        if st.refreshing {
            return false;
        }
        st.refreshing = true;
        drop(st);
        self.inner.bus.request_repaint();
        true
    }

    fn mark_busy(&self, ids: &[AgentId], op: Op) {
        let mut st = self.state();
        for id in ids {
            if let Some(r) = st.rows.get_mut(id) {
                r.busy = Some(op);
            }
        }
        drop(st);
        self.inner.bus.request_repaint();
    }

    /// Hand a change on `ids` to the queue's worker (started on first use). The rows are idle
    /// again once it ran, was skipped (closing) or couldn't be queued.
    fn enqueue(&self, ids: Vec<AgentId>, job: Job) {
        self.inner.work.fetch_add(1, Ordering::SeqCst);
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_none() && !self.stopped() {
            let (tx, rx) = crossbeam_channel::unbounded::<Job>();
            let started = std::thread::Builder::new().name("agent-connect".into()).spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            });
            match started {
                Ok(_) => *q = Some(tx),
                Err(e) => tracing::warn!("agent connections: couldn't start the worker: {e}"),
            }
        }
        let this = self.clone();
        let done = ids.clone();
        let wrapped: Job = Box::new(move || {
            if !this.stopped() {
                job();
            }
            this.clear_busy(&done);
            this.inner.work.fetch_sub(1, Ordering::SeqCst);
        });
        let sent = q.as_ref().is_some_and(|tx| tx.send(wrapped).is_ok());
        drop(q);
        if !sent {
            self.clear_busy(&ids);
            self.inner.work.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn clear_busy(&self, ids: &[AgentId]) {
        let mut st = self.state();
        for id in ids {
            if let Some(r) = st.rows.get_mut(id) {
                r.busy = None;
            }
        }
        drop(st);
        self.inner.bus.request_repaint();
    }

    /// Read every agent now (blocking; the caller set `refreshing`).
    fn read_all(&self) {
        let before = self.state().changed.clone();
        let (runner, env) = (self.runner(), self.env());
        let (runner, env) = (&*runner, &env);
        let read: Vec<(AgentId, Status)> = std::thread::scope(|s| {
            let handles: Vec<_> = AgentId::ALL.iter().map(|&id| (id, s.spawn(move || read_status(runner, env, id)))).collect();
            handles.into_iter().map(|(id, h)| (id, h.join().unwrap_or_else(|_| Status::Unreadable("the check failed".into())))).collect()
        });
        let usage = self.inner.store.reader().and_then(|c| super::usage::read(&c)).unwrap_or_else(|e| {
            tracing::debug!("agent usage: {e}");
            Vec::new()
        });
        let opencode = agents::opencode_file(env);
        let mut st = self.state();
        for (id, status) in read {
            // an op re-read this agent meanwhile: its answer is newer
            if st.changed.get(&id) != before.get(&id) {
                continue;
            }
            let status = settle_claude(&st, id, status);
            if let Some(r) = st.rows.get_mut(&id) {
                r.status = status;
                // a failure stays (Settings shows it) until the agent's next op
                if r.last.as_ref().is_some_and(|l| l.ok) {
                    r.last = None;
                }
            }
        }
        st.opencode_file = Some(opencode);
        st.usage = usage;
        st.checked = true;
        st.refreshing = false;
        st.refreshed_at = Some(Instant::now());
        drop(st);
        self.inner.bus.request_repaint();
    }

    /// One agent's op, its toast, and its status read again (on the queue's worker).
    fn execute(&self, id: AgentId, op: Op, quiet: bool) {
        let (runner, env) = (self.runner(), self.env());
        let result = run_steps(&*runner, &env, id, op);
        let status = read_status(&*runner, &env, id);
        let message = match &result {
            Ok(done) => match (op, done) {
                (Op::Connect, _) => "connected".to_owned(),
                (Op::Disconnect, _) => "disconnected".to_owned(),
                (Op::Update, Done::UpToDate) => "up to date".to_owned(),
                (Op::Update, Done::Updated(Some(v))) => format!("updated to {v}"),
                (Op::Update, _) => "updated".to_owned(),
            },
            Err(e) => e.clone(),
        };
        let toast = toast(id, op, &result, &status, quiet);
        {
            let mut st = self.state();
            // Claude said "up to date" although the row read older than this Tuner (a GitHub
            // marketplace not yet at this version): that version is current now
            if let (Ok(Done::UpToDate), Status::UpdateAvailable { installed, .. }) = (&result, &status) {
                st.claude_current = Some(installed.clone());
            }
            let status = settle_claude(&st, id, status);
            *st.changed.entry(id).or_default() += 1;
            if let Some(r) = st.rows.get_mut(&id) {
                r.status = status;
                r.busy = None;
                r.last = Some(OpResult { op, ok: result.is_ok(), message });
            }
        }
        if let Some(t) = toast {
            self.inner.bus.toast(t);
        }
        self.inner.bus.request_repaint();
    }

    fn pass(&self, offer: bool, sync: bool) {
        let delay = *self.inner.pass_delay.read().unwrap_or_else(|e| e.into_inner());
        let end = Instant::now() + delay;
        while Instant::now() < end {
            if self.stopped() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50).min(end.saturating_duration_since(Instant::now())));
        }
        // a refresh already running (Settings opened early) is waited for instead
        if self.begin_refresh() {
            let done = RefreshDone { hub: self.clone(), counted: false };
            done.hub.read_all();
        } else {
            let end = Instant::now() + REFRESH_WAIT;
            while self.refreshing() && !self.stopped() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if self.stopped() {
            return;
        }
        let claude_behind = matches!(self.state().rows.get(&AgentId::Claude).map(|r| &r.status), Some(Status::UpdateAvailable { .. }));
        if sync && claude_behind {
            self.mark_busy(&[AgentId::Claude], Op::Update);
            let (tx, rx) = crossbeam_channel::bounded::<()>(1);
            let this = self.clone();
            self.enqueue(
                vec![AgentId::Claude],
                Box::new(move || {
                    this.execute(AgentId::Claude, Op::Update, true);
                    let _ = tx.send(());
                }),
            );
            let _ = rx.recv(); // the sender goes when the job ran or was skipped
        }
        let rows: Vec<AgentRow> = self.state().rows.values().cloned().collect();
        self.state().outcome = Some(decide(&rows, offer));
        self.inner.bus.request_repaint();
    }
}

/// Ends a refresh however it ends: `refreshing` off (and the task counted done).
struct RefreshDone {
    hub: AgentConnect,
    /// It was counted in `work`.
    counted: bool,
}

impl Drop for RefreshDone {
    fn drop(&mut self) {
        self.hub.state().refreshing = false;
        if self.counted {
            self.hub.inner.work.fetch_sub(1, Ordering::SeqCst);
        }
        self.hub.inner.bus.request_repaint();
    }
}

/// Claude Code's "update available" for the version its own update called current is current.
fn settle_claude(st: &State, id: AgentId, status: Status) -> Status {
    match status {
        Status::UpdateAvailable { installed, .. } if id == AgentId::Claude && st.claude_current.as_deref() == Some(installed.as_str()) => {
            Status::Connected { via: Via::Plugin, version: Some(installed) }
        }
        s => s,
    }
}

/// The same directory (as spelled, or once resolved).
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// How an op that worked ended.
enum Done {
    Changed,
    /// Claude's plugin update: the version it now has, when it said.
    Updated(Option<String>),
    UpToDate,
}

/// Is the agent there, and is Tuner in its settings? (blocking)
fn read_status(runner: &dyn CommandRunner, env: &ConnectEnv, id: AgentId) -> Status {
    let found = runner.which(id.program(), env);
    let status = match (id, found) {
        (AgentId::Cursor, found) if found.is_some() || agents::cursor_present(env) => agents::status_from_files(id, env),
        (_, None) => Status::NotFound,
        (_, Some(path)) => match agents::status_cmd(id, env) {
            Some(mut cmd) => {
                cmd.argv[0] = path.display().to_string();
                agents::parse_status(id, runner.run(&cmd, env), env)
            }
            None => agents::status_from_files(id, env),
        },
    };
    tracing::debug!("agent connections: {}: {}", id.label(), status_kind(&status));
    status
}

/// The status without detail, for the debug log.
fn status_kind(s: &Status) -> &'static str {
    match s {
        Status::Checking => "checking",
        Status::NotFound => "not found",
        Status::NotConnected => "not connected",
        Status::Connected { .. } => "connected",
        Status::UpdateAvailable { .. } => "update available",
        Status::TurnedOff => "turned off",
        Status::NeedsRepair => "needs repair",
        Status::Unreadable(_) => "unreadable",
    }
}

/// Run the op's commands in order, stopping at the first failure; each one logged with its
/// redacted result line (never the output itself).
fn run_steps(runner: &dyn CommandRunner, env: &ConnectEnv, id: AgentId, op: Op) -> Result<Done, String> {
    let steps = agents::op_steps(id, op, env);
    if steps.is_empty() {
        return Err(format!("Tuner doesn't {} {} itself", op.verb(), id.label()));
    }
    let Some(path) = runner.which(id.program(), env) else {
        tracing::info!("agent connections: {}: `{}` not found", id.label(), id.program());
        return Err(format!("`{}` isn't on your PATH any more", id.program()));
    };
    let mut done = Done::Changed;
    for mut cmd in steps {
        let line = shell_line(&cmd.argv);
        cmd.argv[0] = path.display().to_string();
        let out = runner.run(&cmd, env);
        let code = match &out {
            Ok(o) => o.code.map_or_else(|| "killed".to_owned(), |c| format!("exit {c}")),
            Err(e) => format!("{e:?}"),
        };
        if id == AgentId::Claude
            && op == Op::Update
            && let Ok(o) = &out
            && let Some(r) = agents::claude_result(&o.stdout)
            && r.outcome == "ok"
            && let Some(u) = r.update_outcome.as_deref()
        {
            done = if u == "up_to_date" { Done::UpToDate } else { Done::Updated(r.new_version) };
        }
        match agents::step_result(id, op, out) {
            Ok(msg) => tracing::info!("agent connections: {}: {line} → ok ({code}): {msg}", id.label()),
            Err(msg) => {
                tracing::info!("agent connections: {}: {line} → {code}: {msg}", id.label());
                return Err(msg);
            }
        }
    }
    Ok(done)
}

/// The toast for an op's result (`None`: a quiet pass's "up to date").
fn toast(id: AgentId, op: Op, result: &Result<Done, String>, status: &Status, quiet: bool) -> Option<Toast> {
    let success = |text: String| Some(Toast { kind: ToastKind::Success, text, action: None, ttl_ms: TOAST_MS });
    let label = id.label();
    match (op, result) {
        (Op::Connect, Ok(_)) if id == AgentId::Claude => success(format!("{label} is connected to Tuner — restart {label} to use it")),
        (Op::Connect, Ok(_)) if id == AgentId::Gemini => success(format!("{label} is connected to Tuner — Gemini uses it in folders you trust")),
        (Op::Connect, Ok(_)) => success(format!("{label} is connected to Tuner — start a new {label} session to use it")),
        (Op::Update, Ok(Done::UpToDate)) if quiet => None,
        (Op::Update, Ok(Done::UpToDate)) => Some(Toast { ttl_ms: TOAST_MS, ..Toast::info(format!("Tuner's plugin in {label} is up to date")) }),
        (Op::Update, Ok(done)) => {
            let version = match (done, status) {
                (Done::Updated(Some(v)), _) => Some(v.clone()),
                (_, Status::Connected { via: Via::Plugin, version }) => version.clone(),
                _ => None,
            };
            let to = version.map(|v| format!(" to {v}")).unwrap_or_default();
            success(format!("Updated Tuner's plugin in {label}{to} — restart {label} to apply"))
        }
        (Op::Disconnect, Ok(_)) => Some(Toast { ttl_ms: TOAST_MS, ..Toast::info(format!("Disconnected {label} from Tuner")) }),
        (Op::Update, Err(e)) => Some(Toast::error(format!("Couldn't update Tuner's plugin in {label}: {e}"))),
        (Op::Connect, Err(e)) => Some(Toast::error(format!("Couldn't connect {label}: {e} — Settings ▸ AI agents tries again"))),
        (op, Err(e)) => Some(Toast::error(format!("Couldn't {} {label}: {e}", op.verb()))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::fake::FakeRunner;
    use super::super::runner::{CmdOutput, RunError};
    use super::*;
    use crate::msg::AppEvent;

    const LIST_022: &str = r#"[{"id":"omatuner@omatuner","version":"0.2.2","scope":"user","enabled":true,"readFromFolder":"/usr/share/tuner/claude/plugins/omatuner","folderVersion":"0.2.2"}]"#;

    struct Rig {
        hub: AgentConnect,
        fake: Arc<FakeRunner>,
        events: crossbeam_channel::Receiver<AppEvent>,
        _rt: tokio::runtime::Runtime,
        _home: tempfile::TempDir,
    }

    fn rig(fake: FakeRunner) -> Rig {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (bus, events) = EventBus::new(None);
        let store = Store::open_temp("agent-connect-hub", None).unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut env = ConnectEnv::rooted(home.path());
        env.tuner_version = "0.2.3".into();
        let hub = AgentConnect::new(Arc::new(NullRunner), ConnectEnv::rooted(home.path()), store, bus, rt.handle().clone());
        let fake = Arc::new(fake);
        hub.with_fake(fake.clone(), env);
        Rig { hub, fake, events, _rt: rt, _home: home }
    }

    impl Rig {
        fn idle(&self) {
            assert!(self.hub.wait_idle(Duration::from_secs(10)), "the hub never went idle");
        }
        fn toasts(&self) -> Vec<(ToastKind, String)> {
            self.events.try_iter().filter_map(|e| if let AppEvent::Toast(t) = e { Some((t.kind, t.text)) } else { None }).collect()
        }
        fn status(&self, id: AgentId) -> Status {
            self.hub.rows().into_iter().find(|r| r.id == id).map_or(Status::NotFound, |r| r.status)
        }
    }

    fn ok(stdout: &str) -> Result<CmdOutput, RunError> {
        Ok(CmdOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
    }

    #[test]
    fn claude_connect_runs_both_steps_in_order_and_reads_again() {
        let r = rig(FakeRunner::default()
            .found(&["claude", "codex"])
            .on_ok(&["claude", "plugin", "list"], "[]")
            .on_ok(
                &["claude", "plugin", "marketplace", "add"],
                r#"{"command":"marketplace-add","outcome":"ok","message":"Successfully added marketplace: omatuner"}"#,
            )
            .on_ok(
                &["claude", "plugin", "install"],
                r#"{"command":"install","outcome":"ok","message":"Successfully installed plugin: omatuner@omatuner (scope: user)"}"#,
            )
            .on_fail(&["codex", "mcp", "get"], 1, "", "Error: No MCP server named 'tuner' found.\n"));
        assert!(r.hub.rows().iter().all(|row| row.status == Status::Checking) && r.hub.rows().len() == 6, "before the first check");
        r.hub.refresh();
        r.idle();
        assert_eq!(r.hub.rows().iter().map(|row| row.id).collect::<Vec<_>>(), [AgentId::Claude, AgentId::Codex]);
        assert_eq!(r.hub.missing(), [AgentId::Gemini, AgentId::Copilot, AgentId::OpenCode, AgentId::Cursor]);
        assert_eq!(r.status(AgentId::Claude), Status::NotConnected);
        r.fake.set(&["claude", "plugin", "list"], ok(LIST_022));
        r.toasts();
        r.hub.run(&[AgentId::Claude], Op::Connect);
        assert_eq!(r.hub.rows()[0].busy, Some(Op::Connect), "busy at once");
        r.idle();
        let lines = r.fake.lines();
        let tail = &lines[lines.len() - 3..];
        assert_eq!(
            tail,
            [
                "claude plugin marketplace add --json rthoms334/omatuner",
                "claude plugin install --json omatuner@omatuner --scope user",
                "claude plugin list --json"
            ]
        );
        let row = &r.hub.rows()[0];
        assert_eq!((row.busy, row.status.clone()), (None, Status::Connected { via: Via::Plugin, version: Some("0.2.2".into()) }));
        assert_eq!(row.last, Some(OpResult { op: Op::Connect, ok: true, message: "connected".into() }));
        assert_eq!(r.toasts(), [(ToastKind::Success, "Claude Code is connected to Tuner — restart Claude Code to use it".into())]);
    }

    #[test]
    fn a_failing_step_stops_the_op_and_toasts_the_redacted_reason() {
        let clone_failed = r#"{"outcome":"failed","failureCode":"error_not_found","message":"Failed to clone https://u:sekrit@github.com/rthoms334/omatuner.git\nfatal: repository not found"}"#;
        let r = rig(FakeRunner::default()
            .found(&["claude"])
            .on_ok(&["claude", "plugin", "list"], "[]")
            .on_fail(&["claude", "plugin", "marketplace", "add"], 1, clone_failed, "✘ Failed to clone")
            .on_ok(&["claude", "plugin", "uninstall"], "")
            .on_fail(&["claude", "plugin", "marketplace", "remove"], 1, r#"{"outcome":"failed","failureCode":"not_configured"}"#, ""));
        r.hub.run(&[AgentId::Claude], Op::Connect);
        r.idle();
        assert!(!r.fake.lines().iter().any(|l| l.contains("install")), "{:?}", r.fake.lines());
        let toasts = r.toasts();
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].0, ToastKind::Error);
        assert!(toasts[0].1.starts_with("Couldn't connect Claude Code: Failed to clone") && !toasts[0].1.contains("sekrit"), "{toasts:?}");
        assert!(!r.hub.rows()[0].last.as_ref().unwrap().ok);
        // a disconnect whose marketplace is already gone is done
        r.hub.run(&[AgentId::Claude], Op::Disconnect);
        r.idle();
        assert_eq!(r.toasts(), [(ToastKind::Info, "Disconnected Claude Code from Tuner".into())]);
        // an agent that left PATH meanwhile
        r.hub.run(&[AgentId::Codex], Op::Connect);
        r.idle();
        assert_eq!(
            r.toasts(),
            [(ToastKind::Error, "Couldn't connect Codex: `codex` isn't on your PATH any more — Settings ▸ AI agents tries again".into())]
        );
    }

    #[test]
    fn changes_are_queued_one_at_a_time_in_order() {
        let r = rig(FakeRunner::default()
            .found(&["codex", "gemini"])
            .slow(Duration::from_millis(30))
            .on_ok(&["codex", "mcp", "add"], "Added global MCP server 'tuner'.")
            .on_ok(&["codex", "mcp", "get"], "{}")
            .on_ok(&["gemini", "mcp", "add"], "")
            .on_ok(&["gemini", "--version"], "0.63.0"));
        r.hub.run(&[AgentId::Codex], Op::Connect);
        r.hub.run(&[AgentId::Gemini], Op::Connect);
        r.idle();
        assert_eq!(
            r.fake.lines(),
            ["codex mcp add tuner -- tuner mcp", "codex mcp get --json tuner", "gemini mcp add -s user tuner tuner mcp", "gemini --version"]
        );
        assert_eq!(r.toasts().len(), 2);
        // not automated: nothing runs
        r.hub.run(&[AgentId::Cursor], Op::Connect);
        r.idle();
        assert_eq!(r.fake.calls().len(), 4);
    }

    fn pass(fake: FakeRunner, cfg: AgentsConfig) -> (Rig, Option<PassOutcome>) {
        let r = rig(fake);
        r.hub.start_pass(&cfg);
        r.idle();
        let out = r.hub.take_pass_outcome();
        assert_eq!(r.hub.take_pass_outcome(), None, "exactly once");
        (r, out)
    }

    #[test]
    fn the_pass_syncs_claudes_plugin_and_decides_the_offer() {
        let behind = LIST_022.replace(r#""folderVersion":"0.2.2""#, r#""folderVersion":"0.2.3""#);
        let updated = r#"{"command":"update","outcome":"ok","message":"Plugin \"omatuner\" updated from 0.2.2 to 0.2.3 for scope user. Restart to apply changes.","updateOutcome":"updated","oldVersion":"0.2.2","newVersion":"0.2.3"}"#;
        let fake = || {
            FakeRunner::default()
                .found(&["claude", "codex"])
                .on_ok(&["claude", "plugin", "list"], &behind)
                .on_ok(&["claude", "plugin", "marketplace", "update"], r#"{"command":"marketplace-update","outcome":"ok"}"#)
                .on_ok(&["claude", "plugin", "update"], updated)
                .on_fail(&["codex", "mcp", "get"], 1, "", "Error: No MCP server named 'tuner' found.")
        };
        let (r, out) = pass(fake(), AgentsConfig::default());
        assert_eq!(out, Some(PassOutcome::Offer(super::super::Offer { agents: vec![AgentId::Codex], connected: vec![AgentId::Claude] })));
        assert!(r.fake.lines().contains(&"claude plugin update --json omatuner@omatuner".to_owned()));
        assert_eq!(r.toasts(), [(ToastKind::Success, "Updated Tuner's plugin in Claude Code to 0.2.3 — restart Claude Code to apply".into())]);

        // keep-in-sync off: no update; "Don't ask again": nothing offered, but recorded
        let (r, out) = pass(fake(), AgentsConfig { keep_in_sync: false, offer_connect: false, ..Default::default() });
        assert_eq!(out, Some(PassOutcome::Settled));
        assert!(!r.fake.lines().iter().any(|l| l.contains("update")));

        // current plugin: no update, and Claude alone has nothing to offer
        let current = FakeRunner::default().found(&["claude"]).on_ok(&["claude", "plugin", "list"], LIST_022);
        let (r, out) = pass(current, AgentsConfig::default());
        assert_eq!(out, Some(PassOutcome::Settled));
        assert_eq!(r.fake.lines(), ["claude plugin list --json"]);
        assert!(r.toasts().is_empty());

        // nothing found: look again next time
        assert_eq!(pass(FakeRunner::default(), AgentsConfig::default()).1, Some(PassOutcome::Retry));
    }

    /// A GitHub install reads older than this Tuner; when Claude's own update says it's up to
    /// date (GitHub hasn't this version yet), the row stops offering an update.
    #[test]
    fn an_up_to_date_answer_settles_a_github_plugin() {
        let github = r#"[{"id":"omatuner@omatuner","version":"0.2.2","scope":"user","enabled":true}]"#;
        let r = rig(FakeRunner::default()
            .found(&["claude"])
            .on_ok(&["claude", "plugin", "list"], github)
            .on_ok(&["claude", "plugin", "marketplace", "update"], r#"{"outcome":"ok"}"#)
            .on_ok(&["claude", "plugin", "update"], r#"{"outcome":"ok","updateOutcome":"up_to_date","newVersion":"0.2.2"}"#));
        r.hub.refresh();
        r.idle();
        assert_eq!(r.status(AgentId::Claude), Status::UpdateAvailable { installed: "0.2.2".into(), available: "0.2.3".into() });
        r.hub.run(&[AgentId::Claude], Op::Update);
        r.idle();
        assert_eq!(r.toasts(), [(ToastKind::Info, "Tuner's plugin in Claude Code is up to date".into())]);
        let current = Status::Connected { via: Via::Plugin, version: Some("0.2.2".into()) };
        assert_eq!(r.status(AgentId::Claude), current);
        r.hub.refresh();
        r.idle();
        assert_eq!(r.status(AgentId::Claude), current, "a later check agrees");
    }

    #[test]
    fn usage_shows_on_the_row_and_as_the_newest() {
        let r = rig(FakeRunner::default().found(&["claude"]).on_ok(&["claude", "plugin", "list"], LIST_022));
        r.hub
            .inner
            .store
            .writer()
            .blocking(|c| {
                super::super::usage::write(c, "Claude", 500)?;
                super::super::usage::write(c, "Zed", 900)
            })
            .unwrap();
        r.hub.refresh();
        r.idle();
        assert_eq!(r.hub.last_used(), Some(("Zed".into(), 900)));
        assert_eq!(r.hub.rows()[0].last_used, Some(500));
        assert!(r.hub.checked() && !r.hub.refreshing());
    }
}
