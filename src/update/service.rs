//! The update service (DESIGN §41.4): one per app, in `Services::updater`. The UI reads a
//! snapshot ([`Updater::status`]) each frame and asks for things (check now, install now, the
//! passwordless switch, restart now); the work runs on the runtime and the blocking pool, and
//! every change requests a repaint.
//!
//! * **The loop**: 20 s after start, then every 6 h (wall clock, re-checked every 10 min, so a
//!   suspended laptop checks when it wakes up late), sooner on "Check now" or an `[updates]`
//!   change; after a failure in 1 h, after GitHub's rate limit when it resets.
//! * **A check**: who installs ([`super::Mode`]: `pacman -Qqo /usr/bin/tuner`, `pacman -Q
//!   omatuner`, the helper and its policy, pkexec, the release key); a newer `omatuner`
//!   already installed than what runs (pacman meanwhile, or our install before the restart) is
//!   "Installed"; else the releases ([`super::github`]) and the newest stable one.
//! * **A newer release**: a Tuner that only tells, or with automatic installs off, says so
//!   once per version (desktop notification and toast). Otherwise the package and its
//!   `.minisig` are downloaded into the private `~/.cache/tuner/update/` (0700, files 0600) and
//!   verified here first (a bad signature: deleted, refused for the session). With the
//!   passwordless rule it installs at once; without, it waits for a moment the password dialog
//!   makes sense — Tuner's window has the keyboard and nothing plays, nothing is open (the
//!   app's gate, [`Updater::prompt_due`]) — and a toast says why the dialog appears. A dismissed
//!   dialog isn't shown again by itself this session ("Install now" in Settings).
//! * **Installed**: notification and toast "Tuner updated to X — restarts when idle", the
//!   status-bar chip, then the restart (`super::restart`) once the gate held for 60 s, checked
//!   here every 5 s off the UI thread (the players' snapshots, the jobs, and what the UI last
//!   said, [`UiNow`]), so a window on a hidden workspace restarts too.
//!
//! **Only on the user's own desktop**: off it (unit tests, `TUNER_NO_DESKTOP`, the integration
//! harnesses' `keep_off_desktop`) or with a profile whose home isn't `$HOME`, nothing runs: no
//! request, no pacman, no notification ([`UpdateStatus::inert`]).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::github::{self, FetchError};
use super::helper::{self, Outcome};
use super::restart::{self, Marker, QuietTimer};
use super::verify::{self, MAX_PACKAGE, MAX_SIGNATURE, SIGNATURE_SUFFIX};
use super::{Mode, Phase, Release, SwitchOp, UpdateStatus, Why, pkgver};
use crate::config::SharedConfig;
use crate::keymap::Action;
use crate::model::Timestamp;
use crate::msg::{AppEvent, EventBus, Toast, ToastKind};
use crate::paths::AppPaths;
use crate::platform::PlatformEvent;
use crate::sources::http::{DownloadRequest, HttpClient};

/// The first check waits this long after start (the window and the startup work go first).
const START_DELAY: Duration = Duration::from_secs(20);
/// Checks this often.
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// After a failed check, try again this soon.
const RETRY_S: i64 = 3600;
/// After pacman was busy, try again this soon.
const BUSY_RETRY_S: i64 = 600;
/// The longest single sleep of the loop (the wall clock is looked at again after it).
const WAKE_CHUNK: Duration = Duration::from_secs(600);
/// The restart gate must hold this long.
const QUIET: Duration = Duration::from_secs(60);
/// How often the restart gate is looked at while an update waits for it.
const GATE_TICK: Duration = Duration::from_secs(5);
/// The version Tuner last told about (not again).
const TOLD_FILE: &str = "told";
const TOAST_MS: u32 = 8000;

/// What the updater needs from the computer, injectable for the tests.
#[derive(Debug, Clone)]
pub struct UpdateEnv {
    pub api: String,
    /// The packaged Tuner (`/usr/bin/tuner`).
    pub exe: PathBuf,
    /// This process's executable.
    pub current_exe: PathBuf,
    pub helper: PathBuf,
    pub policy: PathBuf,
    /// pkexec and its options (program first).
    pub pkexec: Vec<OsString>,
    /// pacman (program first).
    pub pacman: Vec<OsString>,
    /// The release key file's text.
    pub key: String,
    pub arch: String,
    /// This Tuner's version.
    pub running: String,
    /// The private downloads folder (`~/.cache/tuner/update`).
    pub dir: PathBuf,
    /// Where the restart marker goes (`$XDG_RUNTIME_DIR/tuner`).
    pub runtime_dir: PathBuf,
    pub start_delay: Duration,
    pub quiet: Duration,
}

impl UpdateEnv {
    /// This computer's.
    pub fn system(paths: &AppPaths) -> Self {
        // after pacman replaced the file under us, /proc/self/exe reads "… (deleted)"
        let current_exe = std::env::current_exe()
            .map(|e| match e.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
                Some(path) if !e.exists() => PathBuf::from(path),
                _ => e,
            })
            .unwrap_or_default();
        Self {
            api: super::RELEASES_API.into(),
            exe: super::EXE.into(),
            current_exe,
            helper: super::HELPER.into(),
            policy: super::POLICY.into(),
            pkexec: vec!["/usr/bin/pkexec".into(), "--disable-internal-agent".into()],
            pacman: vec!["/usr/bin/pacman".into()],
            key: verify::RELEASE_KEY.into(),
            arch: std::env::consts::ARCH.into(),
            running: super::VERSION.into(),
            dir: paths.cache_dir.join("update"),
            runtime_dir: paths.runtime_dir.clone(),
            start_delay: START_DELAY,
            quiet: QUIET,
        }
    }
}

/// What only the UI knows, as it last drew a frame (the app tells every frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiNow {
    /// The window has the keyboard.
    pub focused: bool,
    /// An overlay or dialog is open, a text field has the keyboard, or onboarding shows.
    pub busy: bool,
    pub multiview: bool,
    /// The screen on view, for the restart.
    pub view: Option<crate::cli::ViewArg>,
}

/// What `Services` hands over for a restart from a window that can't close (hidden
/// workspace): its shutdown flag and the signal watcher's direct-exit cleanup.
#[derive(Clone)]
pub struct RestartHooks {
    pub closing: Arc<AtomicBool>,
    pub cleanup: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

/// The updates. Cheap to clone (an `Arc` inside).
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

struct Inner {
    env: UpdateEnv,
    http: HttpClient,
    bus: EventBus,
    rt: tokio::runtime::Handle,
    config: SharedConfig,
    shared: Arc<crate::shared::AppShared>,
    jobs: Option<crate::jobs::JobManager>,
    state: Mutex<State>,
    wake: tokio::sync::Notify,
    cancel: CancellationToken,
    /// Runs here at all (see the module docs).
    live: bool,
    hooks: Mutex<Option<RestartHooks>>,
}

#[derive(Default)]
struct State {
    status: UpdateStatus,
    ui: Option<UiNow>,
    /// The passwordless switch changed: the app saves it into the config.
    config_change: Option<bool>,
    /// When the next check is due (unix seconds).
    next: Option<Timestamp>,
    quiet: QuietTimer,
    restarting: bool,
    /// A release the helper refused, or whose download didn't verify: not again this session.
    refused: Option<String>,
    /// The "ready" notification went out for this version.
    ready_told: Option<String>,
}

impl Updater {
    pub fn new(
        env: UpdateEnv,
        http: HttpClient,
        bus: EventBus,
        rt: tokio::runtime::Handle,
        config: SharedConfig,
        shared: Arc<crate::shared::AppShared>,
        jobs: Option<crate::jobs::JobManager>,
        live: bool,
    ) -> Self {
        let state = State { status: UpdateStatus { inert: !live, ..Default::default() }, ..Default::default() };
        Self {
            inner: Arc::new(Inner {
                env,
                http,
                bus,
                rt,
                config,
                shared,
                jobs,
                state: Mutex::new(state),
                wake: tokio::sync::Notify::new(),
                cancel: CancellationToken::new(),
                live,
                hooks: Mutex::new(None),
            }),
        }
    }

    /// Production: this computer's, and inert off the desktop or when the profile's home isn't
    /// the user's.
    pub fn for_services(
        paths: &AppPaths,
        http: HttpClient,
        bus: EventBus,
        rt: tokio::runtime::Handle,
        config: SharedConfig,
        shared: Arc<crate::shared::AppShared>,
        jobs: crate::jobs::JobManager,
    ) -> Self {
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
        let live = !crate::platform::off_desktop() && home.is_some_and(|h| h == paths.home);
        Self::new(UpdateEnv::system(paths), http, bus, rt, config, shared, Some(jobs), live)
    }

    /// Start the loop and the restart watch (`Services::start_integrations`); after a restart
    /// into an update, say so and put the window back.
    pub fn start(&self, hooks: RestartHooks) {
        *self.inner.hooks.lock().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
        if !self.inner.live {
            return;
        }
        let pid = std::process::id();
        if let Some(m) = restart::take_marker(&self.inner.env.runtime_dir, pid, crate::model::now()) {
            tracing::info!("update: restarted into Tuner {} (was {})", self.inner.env.running, m.from);
            let mut t = Toast::info(format!("Tuner updated to {} (from {})", self.inner.env.running, m.from));
            t.kind = ToastKind::Success;
            t.ttl_ms = TOAST_MS;
            self.inner.bus.toast(t);
            if let Some(place) = m.place {
                self.inner.rt.spawn_blocking(move || crate::platform::hypr::put_back(pid, &place));
            }
        }
        let this = self.clone();
        self.inner.rt.spawn(async move { this.run().await });
        let this = self.clone();
        self.inner.rt.spawn(async move { this.watch_restart().await });
    }

    /// The snapshot the UI shows.
    pub fn status(&self) -> UpdateStatus {
        self.state().status.clone()
    }

    /// The version installed and waiting for the restart (the status-bar chip).
    pub fn restart_pending(&self) -> Option<String> {
        match &self.state().status.phase {
            Phase::Installed { version } => Some(version.clone()),
            _ => None,
        }
    }

    /// An update waits for the password dialog's moment: the app installs (`install_now`) once
    /// its gate is open.
    pub fn prompt_due(&self) -> bool {
        let st = self.state();
        matches!(st.status.phase, Phase::Ready { declined: false, .. })
            && matches!(st.status.mode, Some(Mode::Managed { .. }))
            && !crate::config::current(&self.inner.config).updates.passwordless
    }

    /// What only the UI knows, every frame.
    pub fn note_ui(&self, ui: UiNow) {
        self.state().ui = Some(ui);
    }

    /// The passwordless switch's new state to save, once (the app's frame).
    pub fn take_config_change(&self) -> Option<bool> {
        self.state().config_change.take()
    }

    /// "Check now", or the `[updates]` settings changed.
    pub fn check_now(&self) {
        if self.inner.live {
            self.inner.wake.notify_one();
        }
    }

    /// "Install now" (Settings, a toast, the chip) or the prompt gate opened: install what is
    /// ready (download it first when it was only told about).
    pub fn install_now(&self) {
        let mut st = self.state();
        let managed = matches!(st.status.mode, Some(Mode::Managed { .. }));
        match st.status.phase.clone() {
            Phase::Ready { release, path, .. } if managed => {
                st.status.phase = Phase::Installing(release.clone());
                drop(st);
                let prompted = !crate::config::current(&self.inner.config).updates.passwordless;
                let this = self.clone();
                self.inner.rt.spawn(async move { this.install(release, path, prompted).await });
            }
            // the user asks: a download that didn't verify (a broken transfer?) is tried again
            Phase::Available(release) if managed && release.package.is_some() => {
                st.refused = None;
                st.status.phase = Phase::Downloading(release.clone());
                drop(st);
                let this = self.clone();
                self.inner.rt.spawn(async move { this.download_and_install(release, true).await });
            }
            _ => {}
        }
        self.inner.bus.request_repaint();
    }

    /// The passwordless switch: the helper writes (or removes) the polkit rule, asking for the
    /// password; the config follows only once it did ([`Updater::take_config_change`]).
    pub fn set_passwordless(&self, on: bool) {
        {
            let mut st = self.state();
            if !matches!(st.status.mode, Some(Mode::Managed { .. })) || matches!(st.status.switch, Some(SwitchOp::Working(_))) {
                return;
            }
            st.status.switch = Some(SwitchOp::Working(on));
        }
        self.inner.bus.request_repaint();
        let this = self.clone();
        self.inner.rt.spawn(async move {
            let arg = if on { "on" } else { "off" };
            let outcome = this.helper(&[OsStr::new("passwordless"), OsStr::new(arg)], helper::SWITCH_TIMEOUT).await;
            let ready = {
                let mut st = this.state();
                st.status.switch = match &outcome {
                    Outcome::Done(_) | Outcome::Dismissed | Outcome::Stopped => None,
                    o => Some(SwitchOp::Failed(o.text())),
                };
                if matches!(outcome, Outcome::Done(_)) {
                    st.config_change = Some(on);
                }
                match &st.status.phase {
                    Phase::Ready { release, path, .. } if on && matches!(outcome, Outcome::Done(_)) => {
                        let r = (release.clone(), path.clone());
                        st.status.phase = Phase::Installing(r.0.clone());
                        Some(r)
                    }
                    _ => None,
                }
            };
            if let Outcome::Done(_) = outcome {
                this.toast(Toast::info(if on {
                    "Tuner now installs its updates without asking for your password"
                } else {
                    "Tuner asks for your password again to install updates"
                }));
            }
            this.inner.bus.request_repaint();
            // an update waiting for the password goes in now, without it
            if let Some((release, path)) = ready {
                this.install(release, path, false).await;
            }
        });
    }

    /// "Restart now" (the chip, the toast, Settings).
    pub fn restart_now(&self) {
        if self.restart_pending().is_some() && !self.state().restarting {
            let this = self.clone();
            self.inner.rt.spawn(async move { this.restart().await });
        }
    }

    /// Stop the loop and the waits (`Services::shutdown`). An install already in pacman's hands
    /// finishes (it runs as root).
    pub fn shutdown(&self) {
        self.inner.cancel.cancel();
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn toast(&self, mut t: Toast) {
        t.ttl_ms = t.ttl_ms.max(TOAST_MS);
        self.inner.bus.toast(t);
    }

    /// A desktop notification, off the runtime's threads (it waits for the server).
    fn notify(&self, summary: String, body: String) {
        self.inner.rt.spawn_blocking(move || crate::platform::notify::notify(&summary, &body, false));
    }

    // ------------------------------------------------------------------------------ the loop

    async fn run(self) {
        if !self.sleep(self.inner.env.start_delay).await {
            return;
        }
        loop {
            self.check().await;
            let due = self.state().next.unwrap_or_else(|| crate::model::now() + EVERY.as_secs() as i64);
            loop {
                let left = due - crate::model::now();
                if left <= 0 {
                    break;
                }
                tokio::select! {
                    () = self.inner.cancel.cancelled() => return,
                    () = self.inner.wake.notified() => break,
                    () = tokio::time::sleep(WAKE_CHUNK.min(Duration::from_secs(left as u64))) => {}
                }
            }
        }
    }

    /// Sleep unless shutting down (false then).
    async fn sleep(&self, d: Duration) -> bool {
        tokio::select! {
            () = self.inner.cancel.cancelled() => false,
            () = tokio::time::sleep(d) => true,
        }
    }

    /// One check (see the module docs).
    pub(crate) async fn check(&self) {
        self.state().status.checking = true;
        self.inner.bus.request_repaint();
        let env = self.inner.env.clone();
        let mode = self.inner.rt.spawn_blocking(move || detect_mode(&env)).await.unwrap_or(Mode::TellOnly(Why::NoHelper));
        let now = crate::model::now();
        {
            let mut st = self.state();
            st.status.mode = Some(mode.clone());
            st.next = Some(now + EVERY.as_secs() as i64);
        }
        // pacman has a newer omatuner than what runs: it waits for the restart
        if let Mode::Managed { installed } = &mode
            && verify::vercmp(pkgver(installed), &self.inner.env.running) == std::cmp::Ordering::Greater
        {
            let installing = matches!(self.state().status.phase, Phase::Installing(_));
            if !installing {
                self.installed(pkgver(installed).to_owned(), false);
            }
            self.finish_check(None);
            return;
        }
        let fetched = github::fetch(&self.inner.http, &self.inner.env.api, &self.prepare_dir(), now, &self.inner.cancel).await;
        let releases = match fetched {
            Ok(r) => r,
            Err(FetchError::Limited { until }) => {
                self.state().next = Some(until);
                let at = jiff::Timestamp::from_second(until).ok().map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string());
                self.finish_check(Some(format!("GitHub's limit for update checks is used up; Tuner checks again at {}", at.unwrap_or_default())));
                return;
            }
            Err(FetchError::Failed(why)) => {
                self.state().next = Some(now + RETRY_S);
                self.finish_check(Some(format!("Couldn't check for updates: {why}")));
                return;
            }
        };
        let picked = github::pick(&releases, &self.inner.env.arch);
        match picked {
            Some(r) if super::is_newer(&r, &mode, &self.inner.env.running) => {
                self.finish_check(None);
                self.found(r, mode).await;
            }
            _ => {
                let mut st = self.state();
                if !matches!(st.status.phase, Phase::Installed { .. } | Phase::Installing(_)) {
                    st.status.phase = Phase::UpToDate;
                }
                drop(st);
                self.finish_check(None);
            }
        }
    }

    fn finish_check(&self, problem: Option<String>) {
        let mut st = self.state();
        st.status.checking = false;
        st.status.last_check = Some(crate::model::now());
        // a newer release's own problem (no package for this computer) comes after, in `found`
        st.status.problem = problem;
        drop(st);
        self.inner.bus.request_repaint();
    }

    /// A release newer than this Tuner.
    async fn found(&self, r: Release, mode: Mode) {
        enum Next {
            Nothing,
            Install(PathBuf),
            Tell(bool),
            Download,
        }
        let cfg = crate::config::current(&self.inner.config).updates.clone();
        let managed = matches!(mode, Mode::Managed { .. });
        let next = {
            let mut st = self.state();
            let ready_path = match &st.status.phase {
                // already on it, or waiting for the restart
                Phase::Downloading(x) | Phase::Installing(x) if x.version == r.version => Some(None),
                Phase::Installed { .. } => Some(None),
                // waiting for the password; with the rule (or after pacman was busy) go on
                Phase::Ready { release, path, .. } if release.version == r.version => Some((cfg.passwordless && managed).then(|| path.clone())),
                _ => None,
            };
            match ready_path {
                Some(Some(path)) => {
                    st.status.phase = Phase::Installing(r.clone());
                    Next::Install(path)
                }
                Some(None) => Next::Nothing,
                None => {
                    let installable = managed && r.package.is_some() && st.refused.as_deref() != Some(&r.version);
                    if !installable || !cfg.auto_install {
                        if managed && r.package.is_none() {
                            st.status.problem =
                                Some(format!("Tuner {} has no signed package for this computer ({}) yet", r.version, self.inner.env.arch));
                        }
                        st.status.phase = Phase::Available(r.clone());
                        Next::Tell(installable)
                    } else {
                        st.status.phase = Phase::Downloading(r.clone());
                        Next::Download
                    }
                }
            }
        };
        match next {
            Next::Nothing => {}
            Next::Install(path) => self.install(r, path, false).await,
            Next::Tell(installable) => self.tell_available(&r, &mode, installable),
            Next::Download => self.download_and_install(r, false).await,
        }
    }

    /// Say once per version that `r` is out (a notification and a toast).
    fn tell_available(&self, r: &Release, mode: &Mode, installable: bool) {
        let told = self.inner.env.dir.join(TOLD_FILE);
        if std::fs::read_to_string(&told).is_ok_and(|v| v.trim() == r.version) {
            return;
        }
        let _ = std::fs::write(&told, &r.version);
        let summary = format!("Tuner {} is available", r.version);
        let (body, mut toast) = if installable {
            ("Install it from Settings ▸ About & logs ▸ Updates.".to_owned(), Toast::info(summary.clone()))
        } else {
            let how = match mode {
                Mode::TellOnly(Why::NotPackaged(_)) => "Update it the way you installed it.".to_owned(),
                Mode::TellOnly(why) => why.text(),
                Mode::Managed { .. } => "It has no package for this computer yet.".to_owned(),
            };
            (format!("{how} {}", r.page), Toast::info(format!("{summary} — Settings ▸ About & logs says more")))
        };
        if installable {
            toast.action = Some(("Install".into(), Action::InstallUpdate));
        }
        self.toast(toast);
        self.notify(summary, body);
    }

    /// Download and verify `r`, then install it at once (passwordless, or `user` asked) or wait
    /// for the prompt.
    async fn download_and_install(&self, r: Release, user: bool) {
        self.inner.bus.request_repaint();
        let path = match self.fetch_package(&r).await {
            Ok(p) => p,
            Err((why, bad)) => {
                let r_version = r.version.clone();
                let mut st = self.state();
                if bad {
                    st.refused = Some(r.version.clone());
                }
                st.status.problem = Some(format!("Tuner {} wasn't installed: {why}", r.version));
                st.status.phase = Phase::Available(r);
                drop(st);
                if bad {
                    self.toast(Toast::error(format!("Tuner {} was refused: the download didn't verify (Settings ▸ About & logs)", r_version)));
                }
                self.inner.bus.request_repaint();
                return;
            }
        };
        let passwordless = crate::config::current(&self.inner.config).updates.passwordless;
        if passwordless || user {
            self.state().status.phase = Phase::Installing(r.clone());
            self.install(r, path, !passwordless).await;
            return;
        }
        let tell = {
            let mut st = self.state();
            st.status.phase = Phase::Ready { release: r.clone(), path, declined: false };
            st.status.problem = None;
            let tell = st.ready_told.as_deref() != Some(r.version.as_str());
            if tell {
                st.ready_told = Some(r.version.clone());
            }
            tell
        };
        if tell {
            self.notify(
                format!("Tuner {} is ready to install", r.version),
                "Downloaded and verified. Tuner asks for your password when you're back at it with nothing playing — or turn on \"Install updates without asking for my password\" in Settings ▸ About & logs.".into(),
            );
        }
        self.inner.bus.request_repaint();
    }

    /// The package and its signature into the private folder, the signature checked here first.
    /// `Err((why, bad))`: `bad` when the download is what's wrong (it doesn't verify).
    async fn fetch_package(&self, r: &Release) -> Result<PathBuf, (String, bool)> {
        let p = r.package.as_ref().ok_or(("it has no package for this computer".to_owned(), false))?;
        if p.file.size > MAX_PACKAGE {
            return Err((format!("its package is larger than {} MiB", MAX_PACKAGE >> 20), true));
        }
        let dir = self.prepare_dir();
        let dest = dir.join(&p.file.name);
        let sig_path = dir.join(format!("{}{SIGNATURE_SUFFIX}", p.file.name));
        clear_old_downloads(&dir, &p.file.name);
        let sig = self.inner.http.get_bytes(&p.signature.url, &github::asset_headers(), MAX_SIGNATURE, &self.inner.cancel).await;
        let sig = sig.map_err(|e| (format!("downloading its signature: {e}"), false))?;
        let sig = String::from_utf8(sig.to_vec()).map_err(|_| ("its signature isn't text".to_owned(), true))?;
        let req = DownloadRequest {
            url: crate::redact::Secret::new(p.file.url.clone()),
            dest: dest.clone(),
            headers: github::asset_headers(),
            etag: None,
            last_modified: None,
        };
        self.inner.http.download(&req, &|_, _| {}, &self.inner.cancel).await.map_err(|e| (format!("downloading it: {e}"), false))?;
        let (key, file, text) = (self.inner.env.key.clone(), dest.clone(), sig.clone());
        let verified = self
            .inner
            .rt
            .spawn_blocking(move || {
                let key = verify::release_key(&key).ok_or("this build has no release key")?;
                let bytes = read_capped(&file, MAX_PACKAGE).map_err(|e| e.to_string())?;
                verify::verify(&key, &bytes, &text)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        if let Err(why) = verified {
            let _ = std::fs::remove_file(&dest);
            tracing::warn!("update: {} refused: {why}", p.file.name);
            return Err((why, true));
        }
        write_private(&sig_path, sig.as_bytes()).map_err(|e| (format!("{}: {e}", sig_path.display()), false))?;
        tracing::info!("update: {} downloaded and verified", p.file.name);
        Ok(dest)
    }

    /// The private downloads folder, made (or made private again) as needed.
    fn prepare_dir(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.inner.env.dir.clone();
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))) {
            tracing::warn!("update: {}: {e}", dir.display());
        }
        dir
    }

    /// Run the helper's install on `path` and act on how it ended; `prompted`: the password
    /// dialog will show (a toast says why it appears).
    async fn install(&self, r: Release, path: PathBuf, prompted: bool) {
        if prompted {
            self.toast(Toast::info(format!("Installing Tuner {}: enter your password to go on", r.version)));
        }
        self.inner.bus.request_repaint();
        // the helper follows no symlink: give it the real path (a ~/.cache that is a link is fine)
        let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let outcome = self.helper(&[OsStr::new("install"), real.as_os_str()], helper::INSTALL_TIMEOUT).await;
        tracing::info!("update: install of Tuner {}: {outcome:?}", r.version);
        let ready = |declined| Phase::Ready { release: r.clone(), path: path.clone(), declined };
        let mut st = self.state();
        match &outcome {
            Outcome::Done(_) => {
                drop(st);
                clear_old_downloads(&self.inner.env.dir, "");
                self.installed(r.version.clone(), true);
                return;
            }
            Outcome::Stopped => return,
            Outcome::Dismissed => {
                st.status.phase = ready(true);
                drop(st);
                self.toast(Toast::info(format!("Tuner {} isn't installed yet: Settings ▸ About & logs ▸ Install now", r.version)));
                self.inner.bus.request_repaint();
                return;
            }
            Outcome::Busy => {
                // the next check (soon) tries again
                st.status.phase = ready(false);
                st.next = Some(crate::model::now() + BUSY_RETRY_S);
            }
            Outcome::Refused(_) => {
                st.refused = Some(r.version.clone());
                st.status.phase = Phase::Available(r.clone());
            }
            Outcome::NoKey => {
                st.status.mode = Some(Mode::TellOnly(Why::NoKey));
                st.status.phase = Phase::Available(r.clone());
            }
            _ => st.status.phase = ready(true),
        }
        let text = outcome.text();
        st.status.problem = Some(format!("Tuner {} wasn't installed: {text}", r.version));
        drop(st);
        if matches!(outcome, Outcome::Refused(_) | Outcome::Failed(_)) {
            self.toast(Toast::error(format!("Tuner {} wasn't installed: {text}", r.version)));
        }
        self.inner.bus.request_repaint();
    }

    /// `pkexec <helper> <args>` on the blocking pool.
    async fn helper(&self, args: &[&OsStr], timeout: Duration) -> Outcome {
        let argv = helper::argv(&self.inner.env.pkexec, &self.inner.env.helper, args);
        let stop = self.inner.cancel.clone();
        self.inner.rt.spawn_blocking(move || helper::run(&argv, timeout, &stop)).await.unwrap_or(Outcome::Stopped)
    }

    /// `version` is installed and waits for the restart; `ours` when Tuner installed it.
    fn installed(&self, version: String, ours: bool) {
        {
            let mut st = self.state();
            if matches!(&st.status.phase, Phase::Installed { version: v } if *v == version) {
                return;
            }
            st.status.phase = Phase::Installed { version: version.clone() };
            st.status.problem = None;
            st.quiet.reset();
        }
        let mut t = Toast::info(format!("Tuner updated to {version} — it restarts when idle"));
        t.kind = ToastKind::Success;
        t.action = Some(("Restart now".into(), Action::RestartToUpdate));
        self.toast(t);
        let passwordless = crate::config::current(&self.inner.config).updates.passwordless;
        let hint = if ours && !passwordless { " Settings ▸ About & logs can install the next ones without asking for your password." } else { "" };
        self.notify(
            format!("Tuner updated to {version}"),
            format!("It restarts by itself when nothing is playing and you're elsewhere, or now from its status bar.{hint}"),
        );
        self.inner.bus.request_repaint();
    }

    // --------------------------------------------------------------------------- the restart

    /// Every [`GATE_TICK`] while an update waits: the gate, held for the quiet period.
    async fn watch_restart(self) {
        loop {
            if !self.sleep(GATE_TICK).await {
                return;
            }
            let due = {
                let mut st = self.state();
                if !matches!(st.status.phase, Phase::Installed { .. }) || st.restarting {
                    st.quiet.reset();
                    continue;
                }
                let quiet = restart::may_restart(&self.quiet(&st));
                st.quiet.due(quiet, Instant::now(), self.inner.env.quiet)
            };
            if due {
                self.restart().await;
                return;
            }
        }
    }

    /// The gate's inputs: the players (their snapshots, off the UI thread), the jobs, what the
    /// UI last said (nothing yet counts as busy).
    fn quiet(&self, st: &State) -> restart::Quiet {
        use crate::model::TileId;
        let idle = |tile| self.inner.shared.tile_player(tile).is_none_or(|rx| rx.borrow().state == crate::player::PlayerState::Idle);
        let ui = st.ui.unwrap_or(UiNow { focused: true, busy: true, ..Default::default() });
        restart::Quiet {
            playing: !idle(TileId::A) || !idle(TileId::B),
            multiview: ui.multiview,
            busy_ui: ui.busy,
            focused: ui.focused,
            jobs: self.inner.jobs.as_ref().is_some_and(|j| !j.active().is_empty()),
            installing: matches!(st.status.phase, Phase::Installing(_)),
        }
    }

    /// Close the window and re-exec (see `super::restart`).
    async fn restart(&self) {
        let (to, view) = {
            let mut st = self.state();
            let version = match &st.status.phase {
                Phase::Installed { version } if !st.restarting => version.clone(),
                _ => return,
            };
            st.restarting = true;
            (version, st.ui.and_then(|u| u.view))
        };
        let Some(hooks) = self.inner.hooks.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            self.state().restarting = false;
            return;
        };
        let pid = std::process::id();
        let place = self.inner.rt.spawn_blocking(move || crate::platform::hypr::placement(pid)).await.ok().flatten();
        let marker = Marker { from: self.inner.env.running.clone(), to: to.clone(), pid, at: crate::model::now(), place };
        restart::write_marker(&self.inner.env.runtime_dir, &marker);
        tracing::info!("update: restarting into Tuner {to}");
        restart::request(restart::Plan { view });
        self.inner.bus.send(AppEvent::Platform(PlatformEvent::Quit));
        // the window closed: `main` execs after the normal shutdown (which drops this task)
        if !self.sleep(restart::GRACE).await || hooks.closing.load(Ordering::SeqCst) {
            return;
        }
        tracing::warn!("update: the window didn't close within {} s (hidden workspace?); restarting directly", restart::GRACE.as_secs());
        (hooks.cleanup)().await;
        crate::logging::flush(Duration::from_millis(500));
        if let Some(plan) = restart::take() {
            let e = restart::exec(&plan);
            tracing::error!("update: couldn't restart: {e}");
            crate::logging::flush(Duration::from_millis(500));
        }
        std::process::exit(0);
    }
}

/// Who installs (blocking: runs pacman twice).
fn detect_mode(env: &UpdateEnv) -> Mode {
    if env.current_exe != env.exe {
        return Mode::TellOnly(Why::NotPackaged(env.current_exe.clone()));
    }
    let query = |args: &[&OsStr]| -> Option<String> {
        let mut cmd = std::process::Command::new(&env.pacman[0]);
        cmd.args(&env.pacman[1..]).args(args).env("LC_ALL", "C").stdin(std::process::Stdio::null());
        let out = cmd.output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    if query(&[OsStr::new("-Qqo"), env.exe.as_os_str()]).as_deref() != Some(verify::PACKAGE) {
        return Mode::TellOnly(Why::NotPackaged(env.current_exe.clone()));
    }
    let installed = query(&[OsStr::new("-Q"), OsStr::new(verify::PACKAGE)])
        .and_then(|l| l.strip_prefix(verify::PACKAGE).map(|v| v.trim().to_owned()))
        .filter(|v| !v.is_empty());
    let Some(installed) = installed else { return Mode::TellOnly(Why::NotPackaged(env.current_exe.clone())) };
    if verify::release_key(&env.key).is_none() {
        Mode::TellOnly(Why::NoKey)
    } else if !env.helper.exists() || !env.policy.exists() {
        Mode::TellOnly(Why::NoHelper)
    } else if !Path::new(&env.pkexec[0]).exists() {
        Mode::TellOnly(Why::NoPkexec)
    } else {
        Mode::Managed { installed }
    }
}

/// Remove packages (and their signatures) in `dir` other than `keep`.
fn clear_old_downloads(dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let pkg = name.strip_suffix(SIGNATURE_SUFFIX).unwrap_or(&name);
        let pkg = pkg.strip_suffix(".part").unwrap_or(pkg);
        if pkg.starts_with("omatuner-") && (keep.is_empty() || pkg != keep) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Write `bytes` to a new file only the user can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The file's bytes, refused over `max`.
fn read_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    if f.metadata()?.len() > max {
        return Err(std::io::Error::other(format!("larger than {} MiB", max >> 20)));
    }
    let mut bytes = Vec::new();
    f.take(max + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use axum::Router;
    use axum::extract::Path as UrlPath;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;
    use crate::update::test_signer::TestKey;

    const ARCH: &str = "x86_64";
    const PKG: &str = "omatuner-9.0.0-1-x86_64.pkg.tar.zst";

    /// What the mock GitHub serves, and what was asked of it ("list 200", "list 304", "pkg"…).
    #[derive(Clone, Default)]
    struct Served {
        list: String,
        package: Vec<u8>,
        signature: String,
        /// 403 with the rate limit used up, resetting then.
        limited_until: Option<i64>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl Served {
        fn hit(&self, what: &str) {
            self.hits.lock().unwrap().push(what.into());
        }
    }

    async fn hub(listener: tokio::net::TcpListener, s: Served) {
        let (list, files) = (s.clone(), s);
        let app = Router::new()
            .route(
                "/releases",
                get(move |h: HeaderMap| {
                    let s = list.clone();
                    async move {
                        if let Some(until) = s.limited_until {
                            s.hit("list 403");
                            let reset = until.to_string();
                            return (StatusCode::FORBIDDEN, [("x-ratelimit-remaining", "0".to_owned()), ("x-ratelimit-reset", reset)], "limit")
                                .into_response();
                        }
                        let ua = h.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
                        assert!(ua.starts_with("Tuner/"), "{ua}");
                        if h.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"r1\"") {
                            s.hit("list 304");
                            return StatusCode::NOT_MODIFIED.into_response();
                        }
                        s.hit("list 200");
                        ([("etag", "\"r1\"")], s.list.clone()).into_response()
                    }
                }),
            )
            .route(
                "/dl/{name}",
                get(move |UrlPath(name): UrlPath<String>| {
                    let s = files.clone();
                    async move {
                        if name.ends_with(".minisig") {
                            s.hit("sig");
                            s.signature.clone().into_bytes()
                        } else {
                            s.hit("pkg");
                            s.package.clone()
                        }
                    }
                }),
            );
        let _ = axum::serve(listener, app).await;
    }

    /// An updater against the mock GitHub, a fake pacman and a fake pkexec (shell scripts run
    /// by `/bin/sh`: nothing freshly written is exec'd), its own key, and nothing on the desktop.
    struct Rig {
        rt: tokio::runtime::Runtime,
        dir: tempfile::TempDir,
        updater: Updater,
        events: crossbeam_channel::Receiver<AppEvent>,
        served: Served,
    }

    struct Opts {
        /// pacman's installed omatuner.
        installed: &'static str,
        /// This process's executable.
        exe: &'static str,
        /// The package bytes are changed after signing.
        tampered: bool,
        limited: bool,
        passwordless: bool,
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts { installed: "0.2.5-1", exe: "/usr/bin/tuner", tampered: false, limited: false, passwordless: false }
        }
    }

    impl Rig {
        fn new(o: Opts) -> Rig {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            let key = TestKey::new(5);
            let package: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
            let signature = key.sign(&package);
            let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let asset =
                |name: &str, size: usize| serde_json::json!({ "name": name, "size": size, "browser_download_url": format!("{base}/dl/{name}") });
            let list = serde_json::json!([
                { "tag_name": "v9.1.0-rc1", "prerelease": true, "html_url": "https://example.invalid/v9.1.0-rc1", "assets": [] },
                { "tag_name": "v9.0.0", "html_url": "https://example.invalid/v9.0.0",
                  "assets": [asset(PKG, package.len()), asset(&format!("{PKG}.minisig"), signature.len())] },
            ]);
            let mut served_package = package.clone();
            if o.tampered {
                served_package[777] ^= 1;
            }
            let served = Served {
                list: list.to_string(),
                package: served_package,
                signature,
                limited_until: o.limited.then(|| crate::model::now() + 1800),
                hits: Arc::default(),
            };
            rt.spawn(hub(listener, served.clone()));
            std::fs::write(d.join("pacman.sh"), format!("case \"$1\" in\n-Qqo) echo omatuner ;;\n-Q) echo 'omatuner {}' ;;\nesac\n", o.installed))
                .unwrap();
            std::fs::write(d.join("pkexec.sh"), format!("printf '%s\\n' \"$@\" >> '{0}/pkexec.argv'\nexit $(cat '{0}/pkexec.code')\n", d.display()))
                .unwrap();
            std::fs::write(d.join("pkexec.code"), "0").unwrap();
            for f in ["helper", "policy"] {
                std::fs::write(d.join(f), "").unwrap();
            }
            let env = UpdateEnv {
                api: format!("{base}/releases"),
                exe: "/usr/bin/tuner".into(),
                current_exe: o.exe.into(),
                helper: d.join("helper"),
                policy: d.join("policy"),
                pkexec: vec!["/bin/sh".into(), d.join("pkexec.sh").into()],
                pacman: vec!["/bin/sh".into(), d.join("pacman.sh").into()],
                key: key.public_file(),
                arch: ARCH.into(),
                running: "0.2.5".into(),
                dir: d.join("cache/update"),
                runtime_dir: d.join("run"),
                start_delay: Duration::ZERO,
                quiet: QUIET,
            };
            let mut cfg = crate::config::Config::default();
            cfg.updates.passwordless = o.passwordless;
            let config: SharedConfig = Arc::new(std::sync::RwLock::new(Arc::new(cfg)));
            let (bus, events) = EventBus::new(None);
            let http = HttpClient::new(&crate::sources::http::HttpOptions::default()).unwrap();
            let shared = Arc::new(crate::shared::AppShared::default());
            let updater = Updater::new(env, http, bus, rt.handle().clone(), config, shared, None, true);
            Rig { rt, dir, updater, events, served }
        }

        fn check(&self) -> UpdateStatus {
            self.rt.block_on(self.updater.check());
            self.updater.status()
        }

        /// Wait (at most 10 s) until the status says `done`.
        fn until(&self, what: &str, done: impl Fn(&UpdateStatus) -> bool) -> UpdateStatus {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let st = self.updater.status();
                if done(&st) {
                    return st;
                }
                assert!(Instant::now() < end, "{what}: {st:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn pkexec_code(&self, code: u8) {
            std::fs::write(self.dir.path().join("pkexec.code"), code.to_string()).unwrap();
        }

        /// Everything the fake pkexec was started with, one argument a line.
        fn pkexec_argv(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("pkexec.argv")).unwrap_or_default().lines().map(String::from).collect()
        }

        fn hits(&self) -> Vec<String> {
            self.served.hits.lock().unwrap().clone()
        }

        fn toasts(&self) -> Vec<Toast> {
            self.events.try_iter().filter_map(|e| if let AppEvent::Toast(t) = e { Some(t) } else { None }).collect()
        }

        fn update_dir(&self) -> PathBuf {
            self.dir.path().join("cache/update")
        }
    }

    /// The whole way with the password: the newest stable release is found, downloaded into the
    /// private folder and verified, then waits for the app's moment; "install" runs the helper
    /// through pkexec on that file, the downloads go, the restart waits. The next check asks
    /// GitHub conditionally (304) and changes nothing.
    #[test]
    fn downloads_verifies_then_installs_through_the_helper() {
        let rig = Rig::new(Opts::default());
        let st = rig.check();
        assert_eq!(st.mode, Some(Mode::Managed { installed: "0.2.5-1".into() }));
        let Phase::Ready { release, path, declined: false } = &st.phase else { panic!("{st:?}") };
        assert_eq!(release.version, "9.0.0", "not the pre-release");
        assert_eq!(path, &rig.update_dir().join(PKG));
        let sig = rig.update_dir().join(format!("{PKG}.minisig"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&rig.update_dir()), mode(path), mode(&sig)), (0o700, 0o600, 0o600), "the helper needs them private");
        assert!(rig.updater.prompt_due());
        assert!(rig.pkexec_argv().is_empty(), "no password dialog by itself: the app's gate decides");

        rig.updater.install_now();
        let st = rig.until("installed", |s| matches!(s.phase, Phase::Installed { .. }));
        assert_eq!(st.phase, Phase::Installed { version: "9.0.0".into() });
        let helper = rig.dir.path().join("helper").display().to_string();
        assert_eq!(rig.pkexec_argv(), [helper, "install".into(), path.display().to_string()]);
        assert!(!path.exists() && !sig.exists(), "the downloads are removed");
        assert_eq!(rig.updater.restart_pending().as_deref(), Some("9.0.0"));
        assert!(!rig.updater.prompt_due());
        let toasts = rig.toasts();
        assert!(toasts.iter().any(|t| t.text == "Installing Tuner 9.0.0: enter your password to go on"), "{toasts:?}");
        let done = toasts.iter().find(|t| t.text == "Tuner updated to 9.0.0 — it restarts when idle").expect("the installed toast");
        assert_eq!(done.action, Some(("Restart now".into(), Action::RestartToUpdate)));

        let st = rig.check();
        assert_eq!(st.phase, Phase::Installed { version: "9.0.0".into() });
        assert_eq!(rig.hits(), ["list 200", "sig", "pkg", "list 304"]);
        assert_eq!(rig.pkexec_argv().len(), 3, "installed once");
    }

    /// With the passwordless rule the install follows the download at once; a dismissed
    /// password dialog leaves it ready, and not asked again by itself.
    #[test]
    fn passwordless_installs_at_once_and_a_dismissed_dialog_waits_for_install_now() {
        let rig = Rig::new(Opts { passwordless: true, ..Default::default() });
        let st = rig.check();
        assert_eq!(st.phase, Phase::Installed { version: "9.0.0".into() });
        assert!(!rig.toasts().iter().any(|t| t.text.contains("enter your password")));

        let rig = Rig::new(Opts::default());
        rig.check();
        rig.pkexec_code(126);
        rig.updater.install_now();
        let st = rig.until("dismissed", |s| matches!(s.phase, Phase::Ready { declined: true, .. }));
        assert!(st.problem.is_none(), "{st:?}");
        assert!(!rig.updater.prompt_due(), "not again by itself");
        assert!(rig.toasts().iter().any(|t| t.text.contains("Install now")));
        rig.pkexec_code(0);
        rig.updater.install_now();
        rig.until("installed", |s| matches!(s.phase, Phase::Installed { .. }));
    }

    /// A download that doesn't verify is deleted and never reaches pkexec; that release isn't
    /// downloaded again this session.
    #[test]
    fn a_download_that_doesnt_verify_is_refused() {
        let rig = Rig::new(Opts { tampered: true, ..Default::default() });
        let st = rig.check();
        assert!(matches!(st.phase, Phase::Available(_)), "{st:?}");
        assert!(st.problem.as_deref().is_some_and(|p| p.contains("doesn't match")), "{st:?}");
        assert!(!rig.update_dir().join(PKG).exists() && !rig.update_dir().join(format!("{PKG}.minisig")).exists());
        assert!(rig.toasts().iter().any(|t| t.kind == ToastKind::Error && t.text.contains("refused")));
        rig.check();
        let pkg_hits = || rig.hits().iter().filter(|h| *h == "pkg").count();
        assert_eq!(pkg_hits(), 1, "not again by itself this session");
        // Install now tries again (a broken transfer fails the same way), still nothing installs
        rig.updater.install_now();
        rig.until("tried again", |s| matches!(s.phase, Phase::Available(_)) && pkg_hits() == 2);
        assert!(rig.pkexec_argv().is_empty());
    }

    /// Not pacman's /usr/bin/tuner: it only tells, once per version, and downloads nothing.
    #[test]
    fn a_tuner_not_installed_with_pacman_only_tells() {
        let rig = Rig::new(Opts { exe: "/home/u/.cargo/bin/tuner", ..Default::default() });
        let st = rig.check();
        assert_eq!(st.mode, Some(Mode::TellOnly(Why::NotPackaged("/home/u/.cargo/bin/tuner".into()))));
        assert!(matches!(&st.phase, Phase::Available(r) if r.version == "9.0.0"), "{st:?}");
        assert!(rig.toasts().iter().any(|t| t.text == "Tuner 9.0.0 is available — Settings ▸ About & logs says more" && t.action.is_none()));
        rig.check();
        assert!(rig.toasts().is_empty(), "told once");
        assert!(!rig.hits().iter().any(|h| h == "pkg" || h == "sig"));
        rig.updater.install_now();
        rig.updater.set_passwordless(true);
        assert!(rig.pkexec_argv().is_empty() && rig.updater.status().switch.is_none());
    }

    /// GitHub's rate limit: no error toast, the next check when it resets.
    #[test]
    fn the_rate_limit_waits_for_its_reset() {
        let rig = Rig::new(Opts { limited: true, ..Default::default() });
        let st = rig.check();
        assert!(st.problem.as_deref().is_some_and(|p| p.contains("limit")), "{st:?}");
        assert_eq!(rig.updater.state().next, rig.served.limited_until);
        assert!(rig.toasts().is_empty());
    }

    /// pacman has a newer omatuner than what runs (installed meanwhile): waiting for the
    /// restart, without asking GitHub.
    #[test]
    fn a_newer_installed_package_waits_for_the_restart() {
        let rig = Rig::new(Opts { installed: "9.0.0-1", ..Default::default() });
        let st = rig.check();
        assert_eq!(st.phase, Phase::Installed { version: "9.0.0".into() });
        assert!(rig.hits().is_empty());
    }

    /// The switch changes the config only once the helper wrote the rule: a dismissed dialog
    /// changes nothing, a refusal says why.
    #[test]
    fn the_passwordless_switch_follows_the_helper() {
        let rig = Rig::new(Opts::default());
        rig.check();
        rig.pkexec_code(126);
        rig.updater.set_passwordless(true);
        rig.until("dismissed", |s| s.switch.is_none());
        assert_eq!(rig.updater.take_config_change(), None);
        rig.pkexec_code(127);
        rig.updater.set_passwordless(true);
        let st = rig.until("refused", |s| matches!(s.switch, Some(SwitchOp::Failed(_))));
        assert!(matches!(st.switch, Some(SwitchOp::Failed(m)) if m.contains("didn't allow")));
        rig.pkexec_code(0);
        rig.updater.set_passwordless(true);
        // and with the rule, the update waiting for the password goes in now
        rig.until("installed", |s| s.switch.is_none() && matches!(s.phase, Phase::Installed { .. }));
        assert_eq!(rig.updater.take_config_change(), Some(true));
        let argv = rig.pkexec_argv();
        let n = argv.len();
        assert_eq!(argv[n - 6..n - 3], [rig.dir.path().join("helper").display().to_string(), "passwordless".into(), "on".into()]);
        assert_eq!(argv[n - 2], "install");
    }

    /// The restart gate reads the pictures off the UI thread: any picture not idle holds it,
    /// and so does a UI that hasn't said anything yet.
    #[test]
    fn the_restart_gate_reads_the_players_and_the_ui() {
        let rig = Rig::new(Opts::default());
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(crate::player::PlayerSnapshot::default()));
        rig.updater.inner.shared.set_main_player(rx);
        let quiet = || restart::may_restart(&rig.updater.quiet(&rig.updater.state()));
        assert!(!quiet(), "no frame yet");
        rig.updater.note_ui(UiNow { focused: false, ..Default::default() });
        assert!(quiet());
        tx.send_replace(Arc::new(crate::player::PlayerSnapshot { state: crate::player::PlayerState::Paused, ..Default::default() }));
        assert!(!quiet(), "paused counts as playing");
        tx.send_replace(Arc::new(crate::player::PlayerSnapshot::default()));
        rig.updater.note_ui(UiNow { focused: true, ..Default::default() });
        assert!(!quiet(), "the user is at Tuner");
    }
}
