//! Long-lived services shared by the UI. Owner: WP-10b (core start/stop implemented in the
//! skeleton; every integration call site is wired — the integrations themselves return `Err`
//! until their WP lands, which is logged and ignored).
//!
//! Start order (DESIGN §5.1): paths → config → logging → tokio runtime → store (migrate,
//! register secrets) → HTTP client → job manager → player (mpv + threads) → [window] →
//! video surface → integrations (theme watcher, IPC server, MPRIS, reminders, scheduler,
//! connection limits).
//! Shutdown order: integrations → jobs.cancel_all → video surface destroy (GL, done by the app)
//! → player shutdown (mpv) → runtime (shutdown_timeout 2 s) → store writer drains + joins.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::config::{Config, SharedConfig};
use crate::error::{Error, Result};
use crate::jobs::{JobContext, JobManager};
use crate::logging::LogBuffer;
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, Playable, WatchProgress};
use crate::msg::{DataScope, EventBus};
use crate::paths::AppPaths;
use crate::player::{MediaResolver, Player, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerStore};
use crate::sources::http::{HttpClient, HttpOptions};
use crate::store::{Store, Writer};

/// Optional desktop integrations (each may be absent if its start failed).
#[derive(Default)]
pub struct Integrations {
    pub theme_watch: Option<crate::theme::watch::ThemeWatcher>,
    pub ipc: Option<crate::ipc::IpcServer>,
    pub mpris: Option<crate::platform::mpris::MprisService>,
    pub reminders: Option<crate::platform::notify::ReminderScheduler>,
    pub scheduler: Option<tokio::task::AbortHandle>,
}

pub struct Services {
    pub paths: AppPaths,
    pub config: SharedConfig,
    pub bus: EventBus,
    pub rt: tokio::runtime::Runtime,
    pub store: Store,
    pub http: HttpClient,
    pub jobs: JobManager,
    pub player: Player,
    pub logs: LogBuffer,
    pub integrations: Integrations,
}

impl Services {
    /// Start everything that does not need the window. Fatal errors here abort startup with a
    /// readable message (DB unreadable, mpv missing).
    pub fn start(paths: AppPaths, config: Config, logs: LogBuffer, bus: EventBus) -> Result<Self> {
        paths.ensure()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .thread_name("tuner-rt")
            .enable_all()
            .build()
            .map_err(Error::other)?;
        let store = Store::open(&paths.db_file(), Some(bus.clone()))?;
        if let Err(e) = store.reader().and_then(|c| crate::store::sources::register_secrets(&c)) {
            tracing::error!("registering stored secrets: {e}");
        }
        let http = HttpClient::new(&HttpOptions {
            default_user_agent: config.player.user_agent.clone(),
            connect_timeout_s: 10,
            read_timeout_s: config.player.network_timeout_s.max(30) as u64,
        })?;
        let config: SharedConfig = Arc::new(RwLock::new(Arc::new(config)));
        let jobs = JobManager::new(JobContext::new(store.clone(), http.clone(), bus.clone(), paths.clone(), config.clone()), rt.handle().clone());
        let player = {
            let cfg = crate::config::current(&config);
            let deps = PlayerDeps {
                bus: bus.clone(),
                store: Box::new(WriterPlayerStore(store.writer().clone())),
                resolver: Box::new(DbResolver::new(store.clone(), config.clone())),
            };
            Player::start(&cfg, deps)?
        };
        Ok(Self { paths, config, bus, rt, store, http, jobs, player, logs, integrations: Integrations::default() })
    }

    /// Start optional integrations; each failure is logged, never fatal. Also feeds the
    /// player's connection budget from stored account info.
    pub fn start_integrations(&mut self) {
        let cfg = self.config();
        let rt = self.rt.handle().clone();
        fn ok<T>(what: &str, r: Result<T>) -> Option<T> {
            r.map_err(|e| tracing::info!("{what}: {e}")).ok()
        }
        if cfg.integration.follow_omarchy_theme {
            self.integrations.theme_watch =
                ok("theme watcher", crate::theme::watch::ThemeWatcher::start(self.paths.clone(), cfg.interface.ui_scale, self.bus.clone()));
        }
        self.integrations.ipc = ok("IPC server", crate::ipc::IpcServer::start(&rt, &self.paths.ipc_socket(), self.bus.clone()));
        if cfg.integration.mpris {
            self.integrations.mpris = ok("MPRIS", crate::platform::mpris::MprisService::start(&rt, self.player.handle(), self.bus.clone()));
        }
        if cfg.reminders.enabled {
            self.integrations.reminders = ok(
                "reminders",
                crate::platform::notify::ReminderScheduler::start(&rt, self.store.clone(), self.bus.clone(), cfg.reminders.lead_min as i64 * 60),
            );
        }
        self.integrations.scheduler = Some(crate::jobs::scheduler::spawn(self.jobs.clone()));
        match self.store.reader().and_then(|c| crate::store::sources::account_limits(&c)) {
            Ok(limits) => {
                for (source, max) in limits {
                    self.player.handle().send(PlayerCommand::SetConnectionLimit { source, max });
                }
            }
            Err(e) => tracing::warn!("connection limits: {e}"),
        }
    }

    /// Current config (cheap `Arc` clone, no struct copy).
    pub fn config(&self) -> Arc<Config> { crate::config::current(&self.config) }

    /// Orderly shutdown (see module docs). The video surface must already be destroyed.
    pub fn shutdown(mut self) {
        if let Some(m) = &mut self.integrations.mpris {
            m.stop();
        }
        if let Some(s) = self.integrations.scheduler.take() {
            s.abort();
        }
        self.integrations = Integrations::default();
        self.jobs.cancel_all();
        self.player.shutdown(); // final progress save is submitted to the writer here
        self.rt.shutdown_timeout(Duration::from_secs(2));
        if !self.store.close(Duration::from_secs(3)) {
            tracing::warn!("database writer did not finish within 3 s");
        }
    }
}

/// Player persistence → store writer (fire-and-forget; DataScope notifications for the UI).
struct WriterPlayerStore(Writer);

impl PlayerStore for WriterPlayerStore {
    fn save_progress(&mut self, p: WatchProgress) {
        let _ = self.0.submit_notify(DataScope::Progress, move |c| crate::store::user::save_progress(c, &p));
    }
    fn push_history(&mut self, e: HistoryEntry) {
        let _ = self.0.submit_notify(DataScope::History, move |c| crate::store::user::push_history(c, &e));
    }
    fn record_failure(&mut self, item: ItemRef, kind: Option<PlayerErrorKind>) {
        let _ = self.0.submit(move |c| match kind {
            Some(k) => crate::store::live::record_failure(c, &item, &format!("{k:?}"), crate::model::now()),
            None => crate::store::live::clear_failure(c, &item),
        });
    }
}

/// Resolver for the player controller thread: its own lazily opened reader + live config.
/// Panics inside WP code (e.g. `todo!` before WP-05b lands) are caught and become resolve
/// errors so they can never kill the player thread.
struct DbResolver {
    store: Store,
    config: SharedConfig,
    conn: Option<rusqlite::Connection>,
    /// Channels that failed to resolve this session (zapping skips them).
    unresolvable: HashSet<ChannelId>,
}

impl DbResolver {
    fn new(store: Store, config: SharedConfig) -> Self { Self { store, config, conn: None, unresolvable: HashSet::new() } }

    fn conn(&mut self) -> std::result::Result<&rusqlite::Connection, PlayerError> {
        if self.conn.is_none() {
            let c = self.store.reader().map_err(|e| PlayerError { kind: PlayerErrorKind::Resolve, message: e.to_string() })?;
            self.conn = Some(c);
        }
        Ok(self.conn.as_ref().expect("reader"))
    }
}

impl MediaResolver for DbResolver {
    fn resolve(&mut self, media: &MediaRef, resume: bool) -> std::result::Result<Playable, PlayerError> {
        let cfg = crate::config::current(&self.config);
        let conn = self.conn()?;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::sources::resolve::resolve(conn, &cfg, media, resume, crate::model::now())
        }));
        let r = match r {
            Ok(r) => r.map_err(|e| {
                let kind = match &e {
                    Error::BadPayload(m) if m == "drm" => PlayerErrorKind::Drm,
                    _ => PlayerErrorKind::Resolve,
                };
                PlayerError { kind, message: e.to_string() }
            }),
            Err(_) => Err(PlayerError { kind: PlayerErrorKind::Resolve, message: "stream resolution is not implemented yet".into() }),
        };
        if r.is_err()
            && let MediaRef::Channel(c) = media {
                self.unresolvable.insert(*c);
            }
        r
    }

    fn skip_on_zap(&mut self, ch: ChannelId) -> bool {
        if self.unresolvable.contains(&ch) {
            return true;
        }
        let Ok(conn) = self.conn() else { return false };
        let since = crate::model::now() - 86_400;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::live::should_skip(conn, ch, since)))
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(false)
    }
}
