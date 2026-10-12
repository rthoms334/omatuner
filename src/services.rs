//! Long-lived services shared by the UI. Owner: WP-10b.
//!
//! Start order (DESIGN §5.1): paths → config → logging → tokio runtime → store (migrate,
//! register secrets) → HTTP client → job manager → player (mpv + threads) → [window] →
//! video surface → integrations (theme watcher, IPC server, MPRIS, reminders, scheduler,
//! connection limits). The memory maintenance thread (`platform::memory`) starts with the
//! player and follows its snapshots.
//!
//! Multi-view (DESIGN §35, MV-1): `tiles` holds picture A from the start and picture B once
//! [`Services::start_tile_b`] started it on the blocking pool and the app adopted it
//! ([`Services::adopt_tile_b`]). Both share one connection budget (stored limits go to it at
//! startup, `SetConnectionLimit` after a refresh reaches it through any picture), both are
//! watched by the memory thread, MPRIS routes through the [`TileRouter`], and shutdown stops
//! B, then A.
//! Casting (DESIGN §40 §5.6): the cast service starts with the integrations when casting is
//! built in and `cast.enabled` (a failure means no casting, never a crash) and is published with
//! `AppShared::set_cast`; picture A reaches it through `AppShared` from the start. Turning
//! `cast.enabled` off or on in Settings stops or starts it (every other `[cast]` value is read
//! live, so a change never ends a cast). It stops first at shutdown (the TV's stop, its children,
//! a waiting `pkexec`, the LAN listener, the sleep lock), and a signal's direct exit takes the
//! same fast path ([`Services::exit_cleanup`]).
//! Shutdown order: video surface destroy (GL, done by the app) → integrations → jobs.shutdown
//! (cancel all, refuse new ones) → player shutdown (mpv; final progress save) → wait ≤ 5 s for
//! the cancelled jobs' wind-down → runtime (shutdown_timeout 2 s) → store writer drains + joins.
//!
//! Every optional integration may fail to start (no session bus, socket in use…): the failure
//! is logged and the app runs without it. Settings edits re-apply live
//! ([`Services::apply_config`]): toggling the theme follower or MPRIS starts/stops them, a
//! reminder setting restarts the reminder scheduler (it always runs: automatic switches, DESIGN
//! §30.8, don't depend on notification reminders), a font change reloads fonts on a worker,
//! a User-Agent / network timeout change rebuilds the shared HTTP client (jobs and image
//! fetches), and debug logging switches the log filter.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::config::{Config, SharedConfig};
use crate::error::{Error, Result};
use crate::jobs::{JobContext, JobManager};
use crate::logging::LogBuffer;
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, Playable, SourceId, TileId, WatchProgress};
use crate::msg::{AppEvent, DataScope, EventBus};
use crate::paths::AppPaths;
use crate::player::{MediaResolver, Player, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerStore};
use crate::sources::http::{HttpClient, HttpOptions};
use crate::store::{Store, Writer};
use crate::theme::ThemeEvent;

/// Longest wait at exit for cancelled jobs to wind down.
const JOBS_WIND_DOWN: Duration = Duration::from_secs(5);

/// Optional desktop integrations (each may be absent if its start failed).
#[derive(Default)]
pub struct Integrations {
    pub theme_watch: Option<crate::theme::watch::ThemeWatcher>,
    pub ipc: Option<crate::ipc::IpcServer>,
    pub mpris: Option<crate::platform::mpris::MprisService>,
    pub reminders: Option<crate::platform::notify::ReminderScheduler>,
    pub scheduler: Option<tokio::task::AbortHandle>,
    /// Casting to TVs (DESIGN §40).
    pub cast: Option<crate::cast::service::CastService>,
}

pub struct Services {
    pub paths: AppPaths,
    pub config: SharedConfig,
    pub bus: EventBus,
    pub rt: tokio::runtime::Runtime,
    pub store: Store,
    pub http: HttpClient,
    pub jobs: JobManager,
    /// The pictures (tile A always; tile B once multi-view starts it, DESIGN §35). Single view
    /// is tile A: `tiles.focused()` / `tiles.handle(TileId::A)`.
    pub tiles: crate::player::tiles::Tiles,
    /// Up next's engine (DESIGN §32; idle until Q-core).
    pub queue: Option<crate::queue::QueueHandle>,
    pub logs: LogBuffer,
    pub integrations: Integrations,
    /// Combined actions on a title's copies that a toast can still undo (DESIGN §29.15).
    pub undo: crate::store::copies::UndoStash,
    /// Drops big snapshots off the UI thread and gives freed heap back to the OS
    /// (`platform::memory`); follows the player's snapshots.
    pub memory: crate::platform::memory::Maintenance,
    /// What the app publishes for code off the UI thread (B2: library snapshot, parental
    /// unlock, the guard, the pictures, Up next; DESIGN §30.4). The agent handler itself lives in
    /// the IPC server.
    pub agent: Arc<crate::shared::AppShared>,
    /// Connecting AI agents to Tuner: detection, status, connect / update / disconnect, the
    /// start-of-version pass (DESIGN §39). Runs nothing off the desktop.
    pub agent_connect: crate::agent::connect::AgentConnect,
    /// Set when [`Services::shutdown`] begins (the signal watcher leaves the exit to it; a
    /// signal after that means it is stuck).
    closing: Arc<AtomicBool>,
    /// The IPC socket we bound, for the signal watcher's direct exit.
    socket: Arc<Mutex<Option<crate::ipc::SocketGuard>>>,
    /// The running cast service's fast stop, for the signal watcher's direct exit.
    cast_stop: Arc<Mutex<Option<crate::cast::service::CastStopper>>>,
}

impl Services {
    /// Start everything that does not need the window. Fatal errors here abort startup with a
    /// readable message (DB unreadable, mpv missing).
    pub fn start(paths: AppPaths, config: Config, logs: LogBuffer, bus: EventBus) -> Result<Self> {
        paths.ensure()?;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).thread_name("tuner-rt").enable_all().build().map_err(Error::other)?;
        let store = Store::open(&paths.db_file(), Some(bus.clone()))?;
        if let Err(e) = store.reader().and_then(|c| crate::store::sources::register_secrets(&c)) {
            tracing::error!("registering stored secrets: {e}");
        }
        // playlists switched to their Xtream login while no window could say so (`tuner
        // --refresh`, or Tuner quit first); named when it could be any of several playlists
        let toasts = bus.clone();
        let _ = store.writer().submit(move |c| {
            let several = crate::store::sources::list_sources(c).map_or(true, |s| s.len() > 1);
            match crate::store::sources::take_unseen_upgrades(c) {
                Ok(names) => names.iter().for_each(|n| toasts.toast(crate::msg::Toast::upgraded(several.then_some(n.as_str())))),
                Err(e) => tracing::warn!("upgrades of the last background refresh: {e}"),
            }
            // a playlist's old country choice that now covers every playlist (DESIGN §28.3)
            match crate::store::sources::take_adopted_category_rule(c) {
                Ok(Some(text)) => toasts.toast(crate::msg::Toast::info(text)),
                Ok(None) => {}
                Err(e) => tracing::warn!("adopted category rule: {e}"),
            }
            Ok(())
        });
        let http = HttpClient::new(&http_options(&config))?;
        let config: SharedConfig = Arc::new(RwLock::new(Arc::new(config)));
        let agent = Arc::new(crate::shared::AppShared::default());
        let ctx = JobContext::new(store.clone(), http.clone(), bus.clone(), paths.clone(), config.clone()).with_shared(agent.clone());
        let jobs = JobManager::new(ctx, rt.handle().clone());
        let player = {
            let cfg = crate::config::current(&config);
            // picture A casts (DESIGN §40 §5.5): through `AppShared`, which answers "casting isn't
            // available" until the cast service publishes its handle
            let cast: Arc<dyn crate::player::remote::CastConnector> = agent.clone();
            Player::start(&cfg, PlayerDeps { cast: Some(cast), ..player_deps(&store, &config, &bus, &agent) })?
        };
        let mut memory = crate::platform::memory::Maintenance::start();
        memory.watch_player(rt.handle(), TileId::A, player.handle().subscribe());
        let tiles = crate::player::tiles::Tiles::single(player);
        agent.set_main_player(tiles.main_watch());
        let agent_connect = crate::agent::connect::AgentConnect::for_services(&paths, store.clone(), bus.clone(), rt.handle().clone());
        Ok(Self {
            paths,
            config,
            bus,
            rt,
            store,
            http,
            jobs,
            tiles,
            queue: None,
            logs,
            integrations: Integrations::default(),
            undo: Default::default(),
            memory,
            agent,
            agent_connect,
            closing: Arc::default(),
            socket: Arc::default(),
            cast_stop: Arc::default(),
        })
    }

    /// Start optional integrations; each failure is logged, never fatal. Also feeds the
    /// player's connection budget from stored account info.
    pub fn start_integrations(&mut self) {
        let cfg = self.config();
        if cfg.integration.follow_omarchy_theme {
            self.start_theme_watch(&cfg);
        }
        let agent: Arc<dyn crate::agent::server::AgentService> = Arc::new(
            crate::agent::server::AgentHandler::new(
                self.agent.clone(),
                self.store.clone(),
                self.config.clone(),
                self.tiles.handle(crate::model::TileId::A),
                self.bus.clone(),
                self.jobs.clone(),
            )
            .with_tiles(self.tiles.registry()),
        );
        self.integrations.ipc =
            ok("IPC server", crate::ipc::IpcServer::start_with_agent(self.rt.handle(), &self.paths.ipc_socket(), self.bus.clone(), Some(agent)));
        *self.socket.lock().unwrap_or_else(|e| e.into_inner()) = self.integrations.ipc.as_ref().map(|s| s.socket());
        // Up next before MPRIS: its Next/Previous go to the queue while it drives
        if crate::queue::ENABLED {
            let deps = crate::queue::service::EngineDeps {
                store: self.store.clone(),
                config: self.config.clone(),
                player: Arc::new(self.tiles.handle(TileId::A)),
                bus: self.bus.clone(),
            };
            self.queue = Some(crate::queue::service::spawn(self.rt.handle(), self.agent.clone(), deps));
            self.agent.set_queue_handle(self.queue.clone());
        }
        if cfg.integration.mpris {
            self.start_mpris();
        }
        if crate::cast::ENABLED && cfg.cast.enabled {
            self.start_cast();
        }
        // always: automatic switches (DESIGN §30.8) run with notification reminders off too
        self.start_reminders();
        let sleep = crate::reminders::sleep::SleepService::start(
            self.rt.handle(),
            self.tiles.main_watch(),
            self.agent.subscribe_queue(),
            // every picture at fire time (multi-view's B starts later, lazily)
            {
                let tiles = self.tiles.registry();
                Arc::new(move |a| crate::reminders::sleep::player_actuator(tiles.all())(a))
            },
            self.bus.clone(),
        );
        self.agent.set_sleep(Some(sleep));
        self.integrations.scheduler = Some(crate::jobs::scheduler::spawn(self.jobs.clone()));
        match self.store.reader().and_then(|c| connection_limits(&c)) {
            Ok(cmds) => cmds.into_iter().for_each(|c| {
                if let PlayerCommand::SetConnectionLimit { source, max } = &c {
                    self.tiles.budget().set_limit(*source, *max);
                }
                self.tiles.handles().iter().for_each(|h| h.send(c.clone()));
            }),
            Err(e) => tracing::warn!("connection limits: {e}"),
        }
        // the batch features' background parts (each a no-op until its track turns it on)
        if crate::spoiler::ENABLED {
            crate::spoiler::tracker::start(self.rt.handle(), self.agent.clone(), self.store.clone());
        }
        // the guard's builder reads the live settings (identity guard: no-op)
        self.agent.guard().configure(cfg.clone());
    }

    fn start_theme_watch(&mut self, cfg: &Config) {
        if crate::theme::fixed_theme_dir().is_some() {
            return; // a developer's fixed theme (screenshots) never changes
        }
        self.integrations.theme_watch =
            ok("theme watcher", crate::theme::watch::ThemeWatcher::start(self.paths.clone(), cfg.interface.ui_scale, self.bus.clone()));
    }

    fn start_mpris(&mut self) {
        let router = crate::platform::mpris::TileRouter::new(self.tiles.registry(), self.agent.clone(), self.queue.clone());
        self.integrations.mpris = ok("MPRIS", crate::platform::mpris::MprisService::start(self.rt.handle(), router, self.bus.clone()));
    }

    /// Start the cast service and publish it (the UI, the agent and picture A see it through
    /// `AppShared::cast`).
    fn start_cast(&mut self) {
        let deps = crate::cast::service::CastDeps::new(self.store.clone(), self.config.clone(), self.paths.clone());
        self.integrations.cast = ok("casting", crate::cast::service::CastService::start(self.rt.handle(), deps));
        self.agent.set_cast(self.integrations.cast.as_ref().map(|c| c.handle()));
        *self.cast_stop.lock().unwrap_or_else(|e| e.into_inner()) = self.integrations.cast.as_ref().map(|c| c.stopper());
    }

    /// Stop the cast service (Settings turned casting off, or shutdown): the inert handle answers
    /// from then on.
    fn stop_cast(&mut self) {
        self.agent.set_cast(None);
        self.cast_stop.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut c) = self.integrations.cast.take() {
            c.stop();
        }
    }

    /// Start picture B's player on the blocking pool (multi-view's first request; mpv init
    /// takes 30–80 ms and must not run on the UI thread). The app hears
    /// `AppEvent::Tile(TileEvent::Ready(B))` — or `Failed(B, why)` — and then calls
    /// [`Services::adopt_tile_b`]. False when B exists or is already starting.
    pub fn start_tile_b(&mut self) -> bool {
        let Some(slot) = self.tiles.begin_b() else { return false };
        let cfg = self.config();
        let deps = player_deps(&self.store, &self.config, &self.bus, &self.agent);
        let (budget, bus) = (self.tiles.budget().clone(), self.bus.clone());
        self.rt.spawn_blocking(move || {
            let started = Player::start_tile(&cfg, deps, TileId::B, budget).map_err(|e| e.to_string());
            let ev = match &started {
                Ok(_) => crate::player::tiles::TileEvent::Ready(TileId::B),
                Err(e) => {
                    tracing::warn!("player B: couldn't start: {e}");
                    crate::player::tiles::TileEvent::Failed(TileId::B, e.clone())
                }
            };
            slot.fill(started);
            bus.send(AppEvent::Tile(ev));
        });
        true
    }

    /// Take picture B's player once it started (on `TileEvent::Ready`): it joins the routers
    /// and the memory thread. `None` while it is still starting; `Some(Err)` when it failed (a
    /// later [`Services::start_tile_b`] tries again). The app then makes B's video surface.
    pub fn adopt_tile_b(&mut self) -> Option<std::result::Result<(), String>> {
        let adopted = self.tiles.adopt_b()?;
        if adopted.is_ok()
            && let Some(b) = self.tiles.b()
        {
            self.memory.watch_player(self.rt.handle(), TileId::B, b.handle().subscribe());
            self.agent.set_tile_b(Some(b.handle().subscribe()));
        }
        Some(adopted)
    }

    fn start_reminders(&mut self) {
        // settings are read live from the shared config; systemd timers only for the default
        // profile (and only once `reminders::ENABLED` arms anything)
        let armer = crate::platform::systemd::SystemdArmer::for_profile(&self.paths).map(|a| Arc::new(a) as Arc<dyn crate::platform::systemd::Armer>);
        let deps = crate::platform::notify::SchedulerDeps {
            store: self.store.clone(),
            bus: self.bus.clone(),
            shared: self.agent.clone(),
            config: self.config.clone(),
            armer,
        };
        self.integrations.reminders = Some(crate::platform::notify::ReminderScheduler::start_with(self.rt.handle(), deps));
        self.agent.set_reminder_waker(self.integrations.reminders.as_ref().map(|r| r.waker()));
    }

    /// Re-apply a Settings edit to the integrations (the app calls it after swapping the shared
    /// config; the player gets `PlayerCommand::ApplyConfig` from the app). Never blocks: theme
    /// and font loading run on the blocking pool and arrive as `ThemeEvent`s.
    pub(crate) fn apply_config(&mut self, old: &Config, new: &Config) {
        if old.integration.follow_omarchy_theme != new.integration.follow_omarchy_theme {
            if new.integration.follow_omarchy_theme {
                self.start_theme_watch(new);
                let (paths, scale, bus) = (self.paths.clone(), new.interface.ui_scale, self.bus.clone());
                self.rt.spawn_blocking(move || {
                    let theme = crate::theme::Theme::load(&paths, scale);
                    bus.send(AppEvent::Theme(ThemeEvent::Changed(Arc::new(theme))));
                });
            } else {
                self.integrations.theme_watch = None;
            }
        }
        if old.integration.mpris != new.integration.mpris {
            if let Some(mut m) = self.integrations.mpris.take() {
                m.stop();
            }
            if new.integration.mpris {
                self.start_mpris();
            }
        }
        // the service reads every other `[cast]` value live (a cast picks it up at its next load)
        if old.cast.enabled != new.cast.enabled {
            self.stop_cast();
            if crate::cast::ENABLED && new.cast.enabled {
                self.start_cast();
            }
        }
        if old.reminders != new.reminders || old.follow != new.follow {
            // the scheduler reads its settings live: a restart would forget the countdowns it
            // already gave (a second countdown, or a second fire)
            match &self.integrations.reminders {
                Some(r) => r.refresh(),
                None => self.start_reminders(),
            }
        }
        // the threshold is part of the suggestion cache's stamp
        if old.epg.fuzzy_threshold != new.epg.fuzzy_threshold {
            crate::jobs::guide_check::request_from(&self.jobs);
        }
        if old.spoiler != new.spoiler || old.follow != new.follow {
            self.agent.guard().configure(self.config());
        }
        if old.general.debug_log != new.general.debug_log {
            crate::logging::set_debug(new.general.debug_log);
        }
        // the app sends ApplyConfig to the focused picture; the others get it here
        let focused = self.tiles.focus();
        for tile in [TileId::A, TileId::B].into_iter().filter(|t| *t != focused) {
            if let Some(p) = self.tiles.player(tile) {
                p.handle().send(PlayerCommand::ApplyConfig(Box::new(new.clone())));
            }
        }
        // the audio languages pick the top copy of every stack (DESIGN §29.11)
        if old.player.audio_langs != new.player.audio_langs {
            self.jobs.submit(crate::jobs::JobKind::RebuildLibrary);
        }
        // User-Agent / timeout: every clone of the client (jobs, image fetches) follows
        if http_options(old) != http_options(new)
            && let Err(e) = self.http.reconfigure(&http_options(new))
        {
            tracing::warn!("network settings: {e}");
        }
        if old.fonts != new.fonts {
            let (fonts, bus) = (new.fonts.clone(), self.bus.clone());
            self.rt.spawn_blocking(move || {
                let loaded = crate::fonts::FontSet::resolve(&fonts).load();
                bus.send(AppEvent::Theme(ThemeEvent::FontsChanged(Arc::new(loaded))));
            });
        }
    }

    /// Current config (cheap `Arc` clone, no struct copy).
    pub fn config(&self) -> Arc<Config> {
        crate::config::current(&self.config)
    }

    /// Quit cleanly on SIGTERM / SIGINT / SIGHUP (`platform::signals`): close the window, or
    /// exit directly when it can't close. Called by `main` only — tests keep the default
    /// signal handling. Both tasks run until the runtime stops, past `shutdown`'s slow steps.
    pub fn watch_signals(&self) {
        let _rt = self.rt.enter();
        let (signals, _forwarder) = match crate::platform::signals::forward() {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("signal handling: {e}");
                return;
            }
        };
        let cleanup = self.exit_cleanup();
        self.rt.spawn(crate::platform::signals::quit_on_signals(
            signals,
            self.bus.clone(),
            self.closing.clone(),
            crate::platform::signals::GRACE,
            move || async move {
                cleanup.await;
                crate::logging::flush(Duration::from_millis(500));
                std::process::exit(0);
            },
        ));
    }

    /// What the direct exit does instead of [`Services::shutdown`] (the UI thread can't run
    /// it): stop casting (the TV's stop within 300 ms, ffmpeg and a waiting pkexec killed, the
    /// session directories wiped), stop the player (its final progress save goes to the writer),
    /// flush the writer queue, remove the IPC socket. mpv and the WAL are left to process exit /
    /// the next open.
    pub(crate) fn exit_cleanup(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (players, writer, socket) = (self.tiles.handles(), self.store.writer().clone(), self.socket.clone());
        let cast = self.cast_stop.clone();
        async move {
            let stopper = cast.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(stopper) = stopper {
                stopper.stop_fast().await;
            }
            for player in &players {
                player.send(PlayerCommand::Shutdown);
            }
            for player in &players {
                let mut snap = player.subscribe();
                let _ = tokio::time::timeout(Duration::from_secs(1), snap.wait_for(|s| s.state == crate::player::PlayerState::Idle)).await;
            }
            if tokio::time::timeout(Duration::from_secs(2), writer.run(|_| Ok(()))).await.is_err() {
                tracing::warn!("database writer busy at exit");
            }
            if let Some(s) = socket.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                s.remove();
            }
        }
    }

    /// Orderly shutdown (see module docs). The video surface must already be destroyed.
    pub fn shutdown(mut self) {
        // the signal watcher stays until the runtime stops: a signal during a stuck shutdown
        // still exits directly (`platform::signals`)
        self.closing.store(true, Ordering::SeqCst);
        // agent CLIs still running are killed with what they started
        self.agent_connect.shutdown();
        // the TV first: a fire-and-forget stop, then the relay's children and a waiting pkexec
        self.stop_cast();
        if let Some(m) = &mut self.integrations.mpris {
            m.stop();
        }
        if let Some(s) = self.integrations.scheduler.take() {
            s.abort();
        }
        self.integrations = Integrations::default();
        self.jobs.shutdown();
        self.tiles.shutdown(); // final progress saves are submitted to the writer here
        // cancelled jobs finish their wind-down (an aborted guide import discards its partial
        // data and restores the channel matches) before the runtime drops their tasks
        if !self.jobs.wait_idle(JOBS_WIND_DOWN) {
            tracing::warn!("background jobs still running after {} s; stopping them", JOBS_WIND_DOWN.as_secs());
        }
        self.rt.shutdown_timeout(Duration::from_secs(2));
        if !self.store.close(Duration::from_secs(3)) {
            tracing::warn!("database writer did not finish within 3 s");
        }
    }
}

/// HTTP client options from the config (default User-Agent, read timeout ≥ 30 s).
pub fn http_options(cfg: &Config) -> HttpOptions {
    HttpOptions {
        default_user_agent: cfg.player.user_agent.clone(),
        connect_timeout_s: 10,
        read_timeout_s: cfg.player.network_timeout_s.max(30) as u64,
    }
}

/// A player's persistence and resolver (picture A at startup, picture B later on the blocking
/// pool: each gets its own database reader). No cast connector: only picture A casts.
fn player_deps(store: &Store, config: &SharedConfig, bus: &EventBus, shared: &Arc<crate::shared::AppShared>) -> PlayerDeps {
    PlayerDeps {
        bus: bus.clone(),
        store: Box::new(WriterPlayerStore(store.writer().clone())),
        resolver: Box::new(DbResolver::new(store.clone(), config.clone()).with_copies(shared.clone())),
        cast: None,
    }
}

/// Log an optional integration's start failure and carry on without it.
fn ok<T>(what: &str, r: Result<T>) -> Option<T> {
    r.map_err(|e| tracing::info!("{what}: {e}")).ok()
}

/// `SetConnectionLimit` for every source with stored account info, so the budget is known
/// before the first play rather than after the next refresh (DESIGN §11.5).
fn connection_limits(conn: &rusqlite::Connection) -> Result<Vec<PlayerCommand>> {
    let limits = crate::store::sources::account_limits(conn)?;
    Ok(limits.into_iter().map(|(source, max)| PlayerCommand::SetConnectionLimit { source, max }).collect())
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
            Some(k) => crate::store::live::record_failure(c, &item, &failure_reason(k), crate::model::now()),
            None => crate::store::live::clear_failure(c, &item),
        });
    }
    fn record_video(&mut self, item: ItemRef, width: i64, height: i64) {
        // no DataScope: the UI shows this session's measurements itself (`ui::data::MeasuredQuality`)
        let _ = self.0.submit(move |c| crate::store::live::record_video(c, &item, width, height, crate::model::now()));
    }
}

/// `stream_failure.reason`: the kind in kebab case (`HttpForbidden` → `http-forbidden`,
/// `HttpOther(502)` → `http-other(502)`).
fn failure_reason(kind: PlayerErrorKind) -> String {
    let mut out = String::new();
    for (i, ch) in format!("{kind:?}").chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Resolver for the player controller thread: its own lazily opened reader + live config.
/// A panic while resolving (a bug, or a body not merged yet) is caught and becomes a resolve
/// error so it can never kill the player thread.
struct DbResolver {
    store: Store,
    config: SharedConfig,
    conn: Option<rusqlite::Connection>,
    /// Channels that failed to resolve this session (zapping skips them).
    unresolvable: HashSet<ChannelId>,
    /// The published library, for channel copies (B5: the family rule needs the snapshot).
    shared: Option<Arc<crate::shared::AppShared>>,
}

impl DbResolver {
    fn new(store: Store, config: SharedConfig) -> Self {
        Self { store, config, conn: None, unresolvable: HashSet::new(), shared: None }
    }

    /// Answer `channel_copies` from the library the app publishes.
    fn with_copies(mut self, shared: Arc<crate::shared::AppShared>) -> Self {
        self.shared = Some(shared);
        self
    }

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
            Err(_) => {
                tracing::error!("stream resolution panicked");
                Err(PlayerError { kind: PlayerErrorKind::Resolve, message: "couldn't work out the stream address (see Logs)".into() })
            }
        };
        // a channel that resolves again (e.g. its source was re-enabled) is zappable again
        if let MediaRef::Channel(c) = media {
            if r.is_err() {
                self.unresolvable.insert(*c);
            } else {
                self.unresolvable.remove(c);
            }
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

    fn is_locked(&mut self, ch: ChannelId) -> bool {
        let Ok(conn) = self.conn() else { return true };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::live::channel(conn, ch)))
            .ok()
            .and_then(|r| r.ok())
            .is_none_or(|c| c.locked)
    }

    fn is_explicit(&mut self, ch: ChannelId) -> bool {
        let Ok(conn) = self.conn() else { return true };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::live::channel_explicit(conn, ch)))
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(true)
    }

    fn channel_source(&mut self, ch: ChannelId) -> Option<SourceId> {
        let conn = self.conn().ok()?;
        conn.prepare_cached("SELECT source_id FROM channel WHERE id = ?1").and_then(|mut s| s.query_row([ch], |r| r.get(0))).ok()
    }

    /// The copies in the published library (nothing before the app published one). Hidden
    /// copies never come back; locked and explicit ones are flagged for the controller's gate.
    fn channel_copies(&mut self, ch: ChannelId) -> Vec<crate::store::copies::ChannelCopy> {
        let Some(shared) = self.shared.clone() else { return Vec::new() };
        let lib = shared.library();
        let Ok(conn) = self.conn() else { return Vec::new() };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::store::copies::channel_copies(&lib, conn, ch)))
            .ok()
            .and_then(|r| r.map_err(|e| tracing::debug!("channel copies: {e}")).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, SourceId};

    /// One source per `accounts` entry (account JSON or none) and a channel `m3u:ch` on the
    /// first one.
    fn seeded(name: &str, bus: Option<EventBus>, accounts: Vec<Option<String>>) -> Store {
        let store = Store::open_temp(name, bus).unwrap();
        store
            .writer()
            .blocking(move |c| {
                for a in &accounts {
                    c.execute(
                        "INSERT INTO source(name, kind, url, created_at, account) VALUES ('S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, ?1)",
                        [a],
                    )?;
                }
                c.execute(
                    "INSERT INTO channel(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                     VALUES (1, 'm3u:ch', 'C', 'C', 'c', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/c.ts\"}', 0, 1)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        store
    }

    fn shared_default() -> SharedConfig {
        Arc::new(RwLock::new(Arc::new(Config::default())))
    }

    #[test]
    fn stored_account_limits_become_connection_limit_commands() {
        let account = |max| serde_json::to_string(&crate::model::AccountInfo { max_connections: max, ..Default::default() }).ok();
        let store = seeded("tuner-services-limits", None, vec![account(Some(1)), account(None), None, Some("not json".into())]);
        let cmds = connection_limits(&store.reader().unwrap()).unwrap();
        let limit = |s, max| PlayerCommand::SetConnectionLimit { source: SourceId(s), max };
        assert_eq!(cmds, [limit(1, Some(1)), limit(2, None), limit(3, None), limit(4, None)]);
        assert!(store.close(Duration::from_secs(5)));
    }

    #[test]
    fn failure_reasons_are_kebab_case() {
        assert_eq!(failure_reason(PlayerErrorKind::HttpForbidden), "http-forbidden");
        assert_eq!(failure_reason(PlayerErrorKind::HttpOther(502)), "http-other(502)");
        assert_eq!(failure_reason(PlayerErrorKind::Network), "network");
    }

    /// Player persistence goes through the writer and notifies the UI; two failures make
    /// zapping skip the channel, a successful play clears it.
    #[test]
    fn player_store_and_resolver_share_the_failure_cache() {
        let (bus, events) = EventBus::new(None);
        let store = seeded("tuner-services-fail", Some(bus), vec![None]);
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("m3u:ch".into()) };
        let ch: ChannelId = store.reader().unwrap().query_row("SELECT id FROM channel", [], |r| r.get(0)).unwrap();
        let mut ps = WriterPlayerStore(store.writer().clone());
        let mut resolver = DbResolver::new(store.clone(), shared_default());
        let flush = || store.writer().blocking(|_| Ok(())).unwrap();

        ps.record_failure(item.clone(), Some(PlayerErrorKind::HttpNotFound));
        flush();
        assert!(!resolver.skip_on_zap(ch), "one failure is not enough");
        ps.record_failure(item.clone(), Some(PlayerErrorKind::HttpNotFound));
        flush();
        assert!(resolver.skip_on_zap(ch));
        let reason: String = store.reader().unwrap().query_row("SELECT reason FROM stream_failure", [], |r| r.get(0)).unwrap();
        assert_eq!(reason, "http-not-found");
        ps.record_failure(item.clone(), None);
        flush();
        assert!(!resolver.skip_on_zap(ch));

        ps.push_history(HistoryEntry { item, at: 1, title: "C".into() });
        flush();
        let scopes: Vec<_> = events.try_iter().filter_map(|e| if let AppEvent::Data(s) = e { Some(s) } else { None }).collect();
        assert_eq!(scopes, [DataScope::History]);
        assert!(store.close(Duration::from_secs(5)));
    }

    /// Whatever resolution does with an unknown channel (an error, or a panic from a body not
    /// merged yet), the player gets an error and zapping skips that channel afterwards.
    #[test]
    fn unresolvable_channels_error_and_are_skipped_when_zapping() {
        let store = seeded("tuner-services-resolve", None, vec![None]);
        let mut resolver = DbResolver::new(store.clone(), shared_default());
        let missing = ChannelId(4242);
        assert!(!resolver.skip_on_zap(missing));
        let err = resolver.resolve(&MediaRef::Channel(missing), false).unwrap_err();
        assert_eq!(err.kind, PlayerErrorKind::Resolve);
        assert!(resolver.skip_on_zap(missing));
        assert!(store.close(Duration::from_secs(5)));
    }

    /// A channel of a disabled source is refused and skipped; once the source is enabled again
    /// a direct play resolves it and zapping stops skipping it.
    #[test]
    fn a_channel_that_resolves_again_is_no_longer_skipped() {
        let store = seeded("tuner-services-reenable", None, vec![None]);
        let ch: ChannelId = store.reader().unwrap().query_row("SELECT id FROM channel", [], |r| r.get(0)).unwrap();
        let set_enabled = |on: bool| store.writer().blocking(move |c| Ok(c.execute("UPDATE source SET enabled = ?1", [on])?)).unwrap();
        let mut resolver = DbResolver::new(store.clone(), shared_default());
        set_enabled(false);
        assert!(resolver.resolve(&MediaRef::Channel(ch), false).is_err());
        assert!(resolver.skip_on_zap(ch));
        set_enabled(true);
        resolver.resolve(&MediaRef::Channel(ch), false).unwrap();
        assert!(!resolver.skip_on_zap(ch));
        assert!(store.close(Duration::from_secs(5)));
    }
}
