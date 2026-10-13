//! Restarting into the new version (DESIGN §41.6). The process keeps running the old binary
//! (pacman replaced the file; the old inode lives on until exit), so the update counts once Tuner
//! restarts: by itself when idle, or on "Restart now".
//!
//! * **When** ([`may_restart`], held for a quiet period by [`QuietTimer`]): nothing plays or is
//!   paused in either picture, no multi-view, no overlay or dialog, no text field with the
//!   keyboard, no onboarding, no background job (an import would start again), the helper isn't
//!   installing — and the window doesn't have the keyboard (unfocused, which includes not
//!   visible). Casting (DESIGN §40, its own branch) joins the gate when it lands.
//! * **How**: the updater asks the window to close (`PlatformEvent::Quit`, the normal `on_exit`
//!   path) and `main` re-execs `/usr/bin/tuner` once `run_native` returns ([`take`], [`exec`]):
//!   same process id, session, environment and profile (`--profile-dir`, `--debug`), the screen
//!   it was on (`--view`). A window on a hidden workspace gets no frames and can't close; like the
//!   signal watcher (`platform::signals`), after [`GRACE`] the updater stops the player, flushes
//!   the database writer, removes the socket and execs from its own thread.
//! * **Then**: the marker ([`Marker`], `$XDG_RUNTIME_DIR/tuner/restart.json`, this process id,
//!   two minutes at most) tells the new process to say "Tuner updated to …" and, under Hyprland,
//!   to put its window back on its workspace and give the keyboard back to the window that had
//!   it (`platform::hypr::put_back`), so a restart while the user works elsewhere doesn't take
//!   the keyboard. The MCP bridges (`tuner mcp`) keep their old binary and one connection per
//!   request: they carry on with the new Tuner.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cli::ViewArg;
use crate::model::Timestamp;

/// How long the window gets to close before the updater restarts directly.
pub const GRACE: Duration = crate::platform::signals::GRACE;
/// The marker's file name in the runtime directory.
const MARKER: &str = "restart.json";
/// A marker older than this is stale.
const MARKER_MAX_AGE_S: i64 = 120;

/// What the restart gate looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quiet {
    /// A picture plays, is paused, buffering or reconnecting (not idle).
    pub playing: bool,
    pub multiview: bool,
    /// An overlay or dialog is open, a text field has the keyboard, or onboarding shows.
    pub busy_ui: bool,
    /// The window has the keyboard.
    pub focused: bool,
    /// Background jobs run (an import, a guide refresh).
    pub jobs: bool,
    /// The helper is installing.
    pub installing: bool,
}

/// Whether Tuner may restart now (see the module docs).
pub fn may_restart(q: &Quiet) -> bool {
    !q.playing && !q.multiview && !q.busy_ui && !q.focused && !q.jobs && !q.installing
}

/// The gate must hold this long, without a break, before the restart.
#[derive(Debug, Default)]
pub struct QuietTimer {
    since: Option<Instant>,
}

impl QuietTimer {
    /// `quiet` now; true once it has been for `period`.
    pub fn due(&mut self, quiet: bool, now: Instant, period: Duration) -> bool {
        if !quiet {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= period
    }

    pub fn reset(&mut self) {
        self.since = None;
    }
}

/// How this process was started, for the re-exec (`main` sets it once).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launch {
    pub profile_dir: Option<PathBuf>,
    pub debug: bool,
}

/// A restart asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Plan {
    /// The screen to open on.
    pub view: Option<ViewArg>,
}

static LAUNCH: OnceLock<Launch> = OnceLock::new();
static PLAN: Mutex<Option<Plan>> = Mutex::new(None);

/// `main`: how this Tuner was started.
pub fn set_launch(launch: Launch) {
    let _ = LAUNCH.set(launch);
}

/// Ask for the restart (`main` execs once the window closed).
pub fn request(plan: Plan) {
    *PLAN.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan);
}

/// The restart asked for, if any (`main`, after the window closed).
pub fn take() -> Option<Plan> {
    PLAN.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// `/usr/bin/tuner [--profile-dir D] [--debug] [--view V]` (element 0 is the program). Never
/// the original command line: a `--play` or a playlist file must not happen twice.
pub fn argv(launch: &Launch, plan: &Plan) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![super::EXE.into()];
    if let Some(dir) = &launch.profile_dir {
        a.push("--profile-dir".into());
        a.push(dir.into());
    }
    if launch.debug {
        a.push("--debug".into());
    }
    if let Some(v) = plan.view.and_then(|v| clap::ValueEnum::to_possible_value(&v)) {
        a.push("--view".into());
        a.push(v.get_name().into());
    }
    a
}

/// Replace this process with the new Tuner. Returns only on failure.
pub fn exec(plan: &Plan) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let argv = argv(LAUNCH.get().unwrap_or(&Launch::default()), plan);
    tracing::info!("update: restarting as {}", argv.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    std::process::Command::new(&argv[0]).args(&argv[1..]).exec()
}

/// What the restarting process leaves for the new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The version that ran, and the one installed.
    pub from: String,
    pub to: String,
    /// The process id (an exec keeps it): a marker of another process is ignored.
    pub pid: u32,
    pub at: Timestamp,
    /// Where the window was (Hyprland), to put it back.
    pub place: Option<crate::platform::hypr::Placement>,
}

/// Leave the marker (best effort).
pub fn write_marker(runtime_dir: &Path, m: &Marker) {
    let path = runtime_dir.join(MARKER);
    let done = serde_json::to_vec(m).map_err(std::io::Error::other).and_then(|b| std::fs::write(&path, b));
    if let Err(e) = done {
        tracing::warn!("update: the restart marker {}: {e}", path.display());
    }
}

/// The marker this process left before its exec, taken (the file goes); `None` when there is
/// none, it is another process's or older than two minutes.
pub fn take_marker(runtime_dir: &Path, pid: u32, now: Timestamp) -> Option<Marker> {
    let path = runtime_dir.join(MARKER);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let m: Marker = serde_json::from_slice(&bytes).ok()?;
    (m.pid == pid && (0..=MARKER_MAX_AGE_S).contains(&(now - m.at))).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any one thing going on holds the restart back; only an idle Tuner without the keyboard
    /// restarts.
    #[test]
    fn restarts_only_when_nothing_goes_on_and_the_window_is_away() {
        assert!(may_restart(&Quiet::default()));
        let busy = [
            Quiet { playing: true, ..Default::default() },
            Quiet { multiview: true, ..Default::default() },
            Quiet { busy_ui: true, ..Default::default() },
            Quiet { focused: true, ..Default::default() },
            Quiet { jobs: true, ..Default::default() },
            Quiet { installing: true, ..Default::default() },
        ];
        for q in busy {
            assert!(!may_restart(&q), "{q:?}");
        }
    }

    #[test]
    fn the_gate_must_hold_for_the_whole_quiet_period() {
        let mut t = QuietTimer::default();
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let period = Duration::from_secs(60);
        assert!(!t.due(true, at(0), period));
        assert!(!t.due(true, at(59), period));
        // a moment of playback starts it again
        assert!(!t.due(false, at(60), period));
        assert!(!t.due(true, at(61), period));
        assert!(!t.due(true, at(120), period));
        assert!(t.due(true, at(121), period));
    }

    /// The re-exec keeps the profile and debug logging, opens the same screen, and never
    /// repeats a play or a file of the first command line.
    #[test]
    fn the_new_process_gets_the_profile_and_the_screen_only() {
        let launch = Launch { profile_dir: Some("/tmp/p".into()), debug: true };
        let a = argv(&launch, &Plan { view: Some(ViewArg::Guide) });
        assert_eq!(a, ["/usr/bin/tuner", "--profile-dir", "/tmp/p", "--debug", "--view", "guide"]);
        assert_eq!(argv(&Launch::default(), &Plan::default()), ["/usr/bin/tuner"]);
        let parsed = <crate::cli::Cli as clap::Parser>::try_parse_from(&a).unwrap();
        assert_eq!((parsed.view, parsed.debug, parsed.play, parsed.inputs.len()), (Some(ViewArg::Guide), true, None, 0));
    }

    #[test]
    fn only_this_processs_fresh_marker_counts() {
        let dir = tempfile::tempdir().unwrap();
        let m = Marker { from: "0.2.5".into(), to: "0.2.6".into(), pid: 42, at: 1_000, place: None };
        write_marker(dir.path(), &m);
        assert_eq!(take_marker(dir.path(), 42, 1_030), Some(m.clone()));
        assert_eq!(take_marker(dir.path(), 42, 1_030), None, "taken once");
        write_marker(dir.path(), &m);
        assert_eq!(take_marker(dir.path(), 43, 1_030), None, "another process's");
        write_marker(dir.path(), &m);
        assert_eq!(take_marker(dir.path(), 42, 1_000 + MARKER_MAX_AGE_S + 1), None, "stale");
        assert!(!dir.path().join(MARKER).exists());
    }
}
