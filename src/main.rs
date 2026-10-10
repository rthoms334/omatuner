//! `tuner` binary: CLI → paths/config/logging → single-instance handoff → services → window.
//! Owner: WP-10b.
//!
//! Startup budget (first frame < 300 ms): font resolution (`fc-match`/`fc-list`, ~145 ms) +
//! font file reads and theme loading (`hyprctl`) run on helper threads in parallel with
//! `Services::start` (mpv init, store open/migrate) and are joined in the eframe creation
//! callback.
//!
//! `tuner --refresh` refreshes stale playlists and guides without a window and exits (for a
//! future systemd user timer): no player, no UI, exit status 1 if a refresh failed. While a
//! windowed instance runs it does nothing — that instance owns the database writer and its
//! scheduler refreshes on its own.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::Receiver;
use eframe::egui;

use tuner::cli::Cli;
use tuner::config::Config;
use tuner::fonts::{FontSet, LoadedFonts};
use tuner::ipc::{Handoff, IpcRequest};
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager};
use tuner::model::JobId;
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::services::Services;
use tuner::sources::http::HttpClient;
use tuner::store::Store;
use tuner::theme::Theme;

/// How long a launch waits for a starting instance to serve its socket before giving up.
const HANDOFF_WAIT: Duration = Duration::from_secs(10);
/// Upper bound for a headless refresh (a stuck download must not keep a timer unit alive).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Quiet period with no running jobs before a headless refresh counts as done (follow-up jobs
/// such as provider EPG → RebuildLibrary are submitted around their parent's end).
const REFRESH_SETTLE: Duration = Duration::from_millis(500);

fn main() -> anyhow::Result<()> {
    tuner::platform::limit_malloc_arenas();
    let cli = Cli::parse();
    let paths = match &cli.profile_dir {
        Some(dir) => AppPaths::rooted(dir),
        None => AppPaths::from_env()?,
    };
    let (config, config_err) = match Config::load(&paths.config_file()) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let logs = tuner::logging::init(&paths, cli.debug || config.general.debug_log);
    if cli.debug {
        tuner::logging::pin_debug();
    }

    if cli.refresh {
        if let Some(e) = config_err {
            tracing::error!("{e}; using defaults (your file was not modified)");
        }
        if instance_running(&paths) {
            tracing::info!("Tuner is running; it refreshes stale sources itself");
            return Ok(());
        }
        let failed = refresh_headless(paths, config, REFRESH_TIMEOUT)?;
        anyhow::ensure!(failed == 0, "{failed} refresh job(s) failed (see the log)");
        return Ok(());
    }

    // Paths are made absolute HERE: a running instance receiving them over IPC has another
    // working directory.
    let request =
        IpcRequest { view: cli.view, play: cli.play.as_deref().map(absolute_input), inputs: cli.inputs.iter().map(|i| absolute_input(i)).collect() };
    // held until the process exits: the database has one owner
    let Some(_instance) = become_primary(&paths, &request, HANDOFF_WAIT)? else {
        return Ok(());
    };

    let (bus, events) = EventBus::new(None);
    // only the primary instance: the first Settings change saves over config.toml
    if let Some(e) = config_err {
        let bad = Config::set_aside(&paths.config_file(), e);
        tracing::error!("{}", bad.log_line());
        bus.toast(tuner::msg::Toast::error(bad.toast_text()));
    }
    let ui_scale = config.interface.ui_scale;
    let follow_theme = config.integration.follow_omarchy_theme;
    let font_cfg = config.fonts.clone();
    let fonts_thread = std::thread::Builder::new().name("fonts".into()).spawn(move || FontSet::resolve(&font_cfg).load())?;
    let theme_paths = paths.clone();
    let theme_thread = std::thread::Builder::new().name("theme".into()).spawn(move || {
        if follow_theme {
            Theme::load(&theme_paths, ui_scale)
        } else {
            let t = Theme::fallback();
            Theme::from_parts(t.name, t.palette, t.shell, t.tokens.rounding, ui_scale)
        }
    })?;
    let services = Services::start(paths.clone(), config, logs, bus.clone())?;
    services.watch_signals();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(tuner::APP_NAME)
            .with_app_id(tuner::APP_ID)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([360.0, 240.0]),
        ..Default::default()
    };
    eframe::run_native(
        tuner::APP_NAME,
        options,
        Box::new(move |cc| {
            // The bus was created before the window; attaching the context here makes every
            // clone (player, jobs, store writer) wake the UI when it sends.
            bus.attach_egui(cc.egui_ctx.clone());
            let fonts: LoadedFonts = fonts_thread.join().unwrap_or_default();
            let theme = Arc::new(theme_thread.join().unwrap_or_else(|_| Theme::fallback()));
            tracing::info!("theme '{}' ({:?})", theme.name, theme.palette.mode);
            Ok(Box::new(tuner::app::TunerApp::new(cc, services, events, theme, fonts, request)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("window: {e}"))?;
    Ok(())
}

/// An existing local file → its absolute path; a local `file://` URI (desktop entry `%U`, file
/// managers, `gio open`) → its decoded path; other URLs, channel references (`2:xt:live:1`)
/// and paths that don't exist pass through unchanged.
fn absolute_input(input: &str) -> String {
    let path = match url::Url::parse(input) {
        Ok(u) if u.scheme() == "file" => match u.to_file_path() {
            Ok(p) => p,
            Err(()) => return input.to_owned(), // `file://other-host/…`
        },
        _ if input.contains("://") => return input.to_owned(),
        _ => PathBuf::from(input),
    };
    match std::fs::canonicalize(&path) {
        Ok(p) if p.is_file() => p.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

/// Something listens on the instance socket (a stale file refuses the connection), or an
/// instance is starting (it holds the instance lock). Only connects — sending a request would
/// raise that instance's window.
fn instance_running(paths: &AppPaths) -> bool {
    std::os::unix::net::UnixStream::connect(paths.ipc_socket()).is_ok() || matches!(instance_lock(paths), Ok(None))
}

/// Hand `request` to the running instance, or become it: `Some(lock)` = this process is the
/// primary instance and must hold the lock until it exits; `None` = delivered (exit).
///
/// Single instance: the IPC server only binds once the window is up, so two quick launches
/// would both miss the socket and open the same database. The lock is taken before anything
/// opens the store; a launch that finds it held keeps retrying the handoff while the other
/// instance starts — without removing a socket that refuses (it may be one that instance is
/// binding right now) — and gives up after `wait` (that instance never served its socket). A
/// request the running instance refuses fails right away.
fn become_primary(paths: &AppPaths, request: &IpcRequest, wait: Duration) -> anyhow::Result<Option<InstanceLock>> {
    let deadline = Instant::now() + wait;
    loop {
        let lock = match instance_lock(paths) {
            Ok(Some(lock)) => Some(InstanceLock { _file: Some(lock) }),
            Ok(None) => None,
            Err(e) => {
                // e.g. a filesystem without locks: run as before, unguarded
                tracing::warn!("single-instance lock {}: {e}", paths.runtime_dir.display());
                Some(InstanceLock { _file: None })
            }
        };
        // holding the lock, a socket that refuses is stale; an instance from before the lock
        // existed may still answer
        match tuner::ipc::handoff(&paths.ipc_socket(), request, lock.is_some())? {
            Handoff::Delivered => return Ok(None),
            Handoff::Rejected(e) => anyhow::bail!("the running {} refused the request: {}", tuner::APP_NAME, tuner::redact::redact(&e)),
            Handoff::NoServer if lock.is_some() => return Ok(lock),
            Handoff::NoServer => {}
        }
        anyhow::ensure!(Instant::now() < deadline, "{} is already running but doesn't answer (try again in a moment)", tuner::APP_NAME);
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Held by the primary instance until it exits (`None` inside: locking failed, unguarded).
#[derive(Debug)]
struct InstanceLock {
    _file: Option<std::fs::File>,
}

/// Take the exclusive instance lock (`<runtime dir>/tuner.lock`, released when the returned file
/// closes, i.e. at the latest when the process exits). `Ok(None)` = another process holds it.
fn instance_lock(paths: &AppPaths) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&paths.runtime_dir)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths.runtime_dir.join("tuner.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// `--refresh`: store + HTTP + job manager only (no player, no window); submits
/// `RefreshStale` and waits for it and every follow-up job. Returns how many jobs failed.
fn refresh_headless(paths: AppPaths, config: Config, timeout: Duration) -> anyhow::Result<usize> {
    paths.ensure()?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("tuner-rt").enable_all().build()?;
    let (bus, events) = EventBus::new(None);
    let store = Store::open(&paths.db_file(), Some(bus.clone()))?;
    if let Err(e) = store.reader().and_then(|c| tuner::store::sources::register_secrets(&c)) {
        tracing::error!("registering stored secrets: {e}");
    }
    let http = HttpClient::new(&tuner::services::http_options(&config))?;
    let shared = Arc::new(RwLock::new(Arc::new(config)));
    let jobs = JobManager::new(JobContext::new(store.clone(), http, bus, paths, shared), rt.handle().clone());
    let root = jobs.submit(JobKind::RefreshStale);
    let failed = wait_for_jobs(&jobs, &events, root, timeout);
    jobs.cancel_all();
    rt.shutdown_timeout(Duration::from_secs(2));
    if !store.close(Duration::from_secs(3)) {
        tracing::warn!("database writer did not finish within 3 s");
    }
    Ok(failed)
}

/// Drain job events until `root` finished and no job has been running for
/// [`REFRESH_SETTLE`] (or `timeout` passed: everything is cancelled and counted as failed).
fn wait_for_jobs(jobs: &JobManager, events: &Receiver<AppEvent>, root: JobId, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    let (mut failed, mut root_done) = (0, false);
    let mut idle_since: Option<Instant> = None;
    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(AppEvent::Job(JobEvent::Finished { id, kind, outcome, .. })) => {
                root_done |= id == root;
                match outcome {
                    Ok(_) => tracing::info!("refresh: {kind:?} done"),
                    Err(e) => {
                        tracing::warn!("refresh: {kind:?} failed: {e}");
                        failed += 1;
                    }
                }
            }
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return failed,
        }
        if root_done && jobs.active().is_empty() && events.is_empty() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= REFRESH_SETTLE {
                return failed;
            }
        } else {
            idle_since = None;
        }
        if Instant::now() >= deadline {
            tracing::warn!("refresh: still running after {} s; cancelling", timeout.as_secs());
            let running = jobs.active().len();
            jobs.cancel_all();
            return failed + running.max(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn inputs_become_absolute_only_for_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("list.m3u");
        std::fs::write(&file, "#EXTM3U\n").unwrap();
        let rel = pathdiff(&file);
        assert_eq!(absolute_input(&rel), std::fs::canonicalize(&file).unwrap().to_string_lossy());
        assert_eq!(absolute_input("http://example.invalid/a.m3u8"), "http://example.invalid/a.m3u8");
        assert_eq!(absolute_input("2:xt:live:1234"), "2:xt:live:1234");
        assert_eq!(absolute_input("missing-file.ts"), "missing-file.ts");
        // a directory is not a playable input
        let d = dir.path().to_string_lossy().into_owned();
        assert_eq!(absolute_input(&d), d);
    }

    #[test]
    fn file_uris_become_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("My List.m3u");
        std::fs::write(&file, "#EXTM3U\n").unwrap();
        let abs = std::fs::canonicalize(&file).unwrap();
        let uri = url::Url::from_file_path(&abs).unwrap();
        assert!(uri.as_str().ends_with("/My%20List.m3u"), "{uri}");
        assert_eq!(absolute_input(uri.as_str()), abs.to_string_lossy());
        // a missing file still becomes its (decoded) path; a remote host stays a URI
        assert_eq!(absolute_input("file:///nowhere/A%20B.m3u8"), "/nowhere/A B.m3u8");
        assert_eq!(absolute_input("file://nas/share/a.m3u"), "file://nas/share/a.m3u");
    }

    /// `file` relative to the current directory (`../..` as needed).
    fn pathdiff(file: &Path) -> String {
        let cwd = std::env::current_dir().unwrap();
        let ups = cwd.components().count() - 1;
        let mut s = "../".repeat(ups);
        s.push_str(file.strip_prefix("/").unwrap().to_str().unwrap());
        s
    }

    #[test]
    fn no_instance_on_a_missing_or_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted(dir.path());
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let sock = paths.ipc_socket();
        assert!(!instance_running(&paths));
        // a bound-then-closed socket file refuses connections
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(!instance_running(&paths));
        std::fs::remove_file(&sock).unwrap();
        let live = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(instance_running(&paths));
        // an instance still starting holds the lock but serves no socket yet
        drop(live);
        let _starting = instance_lock(&paths).unwrap().expect("lock");
        assert!(instance_running(&paths));
    }

    /// Audit fix: two quick launches both missed the socket (bound only once the window is up)
    /// and opened the same database. The second one now waits for the first to serve its socket
    /// and hands off, or gives up — it never becomes a second primary, never unlinks the socket
    /// the first one may be binding, and fails at once when its request is refused.
    #[test]
    fn a_second_launch_hands_off_to_a_starting_instance_or_exits() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted(dir.path());
        let req = IpcRequest::default();
        let first = become_primary(&paths, &req, Duration::ZERO).unwrap();
        assert!(first.is_some(), "nobody running: primary");
        let sock = paths.ipc_socket();
        // bound but not listening yet (refuses connections), as while the first one binds it
        drop(UnixListener::bind(&sock).unwrap());
        assert!(become_primary(&paths, &req, Duration::from_millis(300)).is_err(), "never two primaries");
        assert!(sock.exists(), "a waiting launch leaves the starting instance's socket alone");
        std::fs::remove_file(&sock).unwrap();

        // the first instance serves its socket `after` from now and answers `reply`
        let serve = |after: Duration, reply: &'static [u8]| {
            let sock = sock.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                let listener = UnixListener::bind(&sock).unwrap();
                let (stream, _) = listener.accept().unwrap();
                std::io::BufReader::new(&stream).read_line(&mut String::new()).unwrap();
                (&stream).write_all(reply).unwrap();
            })
        };
        let server = serve(Duration::from_millis(300), b"{\"ok\":true}\n");
        assert!(become_primary(&paths, &req, Duration::from_secs(10)).unwrap().is_none(), "handed off");
        server.join().unwrap();

        std::fs::remove_file(&sock).unwrap();
        let server = serve(Duration::ZERO, b"{\"ok\":false,\"error\":\"request too large\"}\n");
        let started = Instant::now();
        let err = become_primary(&paths, &req, Duration::from_secs(10)).unwrap_err().to_string();
        assert!(err.contains("request too large") && started.elapsed() < Duration::from_secs(5), "{err}");
        server.join().unwrap();

        // the first instance is gone (lock released, stale socket): the next launch is primary
        drop(first);
        assert!(become_primary(&paths, &req, Duration::ZERO).unwrap().is_some());
        assert!(!sock.exists(), "the primary removes the stale socket");
    }

    /// The headless refresh terminates and reports the outcome of `RefreshStale` (with the
    /// skeleton job manager every job fails immediately; with WP-06 an empty profile has
    /// nothing stale, so it succeeds).
    #[test]
    fn headless_refresh_finishes_on_an_empty_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted(dir.path());
        let started = Instant::now();
        let failed = refresh_headless(paths.clone(), Config::default(), Duration::from_secs(60)).unwrap();
        assert!(failed <= 1, "{failed}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(paths.db_file().exists(), "the store was opened and migrated");
    }

    #[test]
    fn headless_refresh_of_nothing_stale_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let failed = refresh_headless(AppPaths::rooted(dir.path()), Config::default(), Duration::from_secs(60)).unwrap();
        assert_eq!(failed, 0);
    }

    #[test]
    fn wait_for_jobs_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted(dir.path());
        paths.ensure().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (bus, events) = EventBus::new(None);
        let store = Store::open(&paths.db_file(), None).unwrap();
        let http =
            HttpClient::new(&tuner::sources::http::HttpOptions { default_user_agent: "t".into(), connect_timeout_s: 1, read_timeout_s: 1 }).unwrap();
        let jobs = JobManager::new(JobContext::new(store.clone(), http, bus, paths, Arc::default()), rt.handle().clone());
        // a root id that never finishes
        let failed = wait_for_jobs(&jobs, &events, JobId(u64::MAX), Duration::from_millis(300));
        assert_eq!(failed, 1);
        assert!(store.close(Duration::from_secs(3)));
    }
}
