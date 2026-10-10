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

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use crossbeam_channel::Receiver;
use eframe::egui;

use tuner::cli::Cli;
use tuner::config::Config;
use tuner::fonts::{FontSet, LoadedFonts};
use tuner::ipc::IpcRequest;
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager};
use tuner::model::JobId;
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::services::Services;
use tuner::sources::http::{HttpClient, HttpOptions};
use tuner::store::Store;
use tuner::theme::Theme;

/// Upper bound for a headless refresh (a stuck download must not keep a timer unit alive).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Quiet period with no running jobs before a headless refresh counts as done (follow-up jobs
/// such as provider EPG → RebuildLibrary are submitted around their parent's end).
const REFRESH_SETTLE: Duration = Duration::from_millis(500);

fn main() -> anyhow::Result<()> {
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
    if let Some(e) = config_err {
        tracing::error!("{e}; using defaults (your file was not modified)");
    }

    if cli.refresh {
        if instance_running(&paths.ipc_socket()) {
            tracing::info!("Tuner is running; it refreshes stale sources itself");
            return Ok(());
        }
        let failed = refresh_headless(paths, config, REFRESH_TIMEOUT)?;
        anyhow::ensure!(failed == 0, "{failed} refresh job(s) failed (see the log)");
        return Ok(());
    }

    // Paths are made absolute HERE: a running instance receiving them over IPC has another
    // working directory.
    let request = IpcRequest {
        view: cli.view,
        play: cli.play.as_deref().map(absolute_input),
        inputs: cli.inputs.iter().map(|i| absolute_input(i)).collect(),
    };
    if tuner::ipc::try_handoff(&paths.ipc_socket(), &request)? {
        return Ok(());
    }

    let (bus, events) = EventBus::new(None);
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

/// Something listens on the instance socket (a stale file refuses the connection). Only
/// connects — sending a request would raise that instance's window.
fn instance_running(socket: &Path) -> bool { std::os::unix::net::UnixStream::connect(socket).is_ok() }

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
    let http = HttpClient::new(&HttpOptions {
        default_user_agent: config.player.user_agent.clone(),
        connect_timeout_s: 10,
        read_timeout_s: config.player.network_timeout_s.max(30) as u64,
    })?;
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
            Ok(AppEvent::Job(JobEvent::Finished { id, kind, outcome })) => {
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
        let sock = dir.path().join("tuner.sock");
        assert!(!instance_running(&sock));
        // a bound-then-closed socket file refuses connections
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(!instance_running(&sock));
        let _live = std::os::unix::net::UnixListener::bind(dir.path().join("live.sock")).unwrap();
        assert!(instance_running(&dir.path().join("live.sock")));
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
    #[ignore = "needs WP-06"]
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
        let http = HttpClient::new(&HttpOptions { default_user_agent: "t".into(), connect_timeout_s: 1, read_timeout_s: 1 }).unwrap();
        let jobs = JobManager::new(JobContext::new(store.clone(), http, bus, paths, Arc::default()), rt.handle().clone());
        // a root id that never finishes
        let failed = wait_for_jobs(&jobs, &events, JobId(u64::MAX), Duration::from_millis(300));
        assert_eq!(failed, 1);
        assert!(store.close(Duration::from_secs(3)));
    }
}
