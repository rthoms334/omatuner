//! Fatal startup errors made visible (DESIGN §21.7). Owner: WP-08 (with `main`).
//!
//! Tuner is started from the Omarchy launcher (uwsm / `gtk-launch`): nobody reads stderr, so an
//! error that ends `main` before the window is up looked like "the release is broken" (0.2.6:
//! a database left at schema v14 by a dev build refused to open, printed one stderr line and
//! exited 1). [`report`] now says it three ways:
//! * **stderr** as before (`Error: …` with the cause chain, exit status 1 — the caller's);
//! * **`tuner.log`**: one ERROR line through `logging::record_fatal` (the subscriber's writer,
//!   or appended directly when the error came before logging started);
//! * a **desktop notification** "Tuner couldn't start" whose body is [`user_message`]: the
//!   reason in plain words and what to do, redacted (§20). Only for a launch that would have
//!   opened a window (not `--refresh` / `--rearm`, whose journal has stderr, nor `tuner mcp`,
//!   whose stderr is its client's), and only until the window is up ([`window_up`]): an
//!   eframe error at logout is no startup failure. No notification server → nothing more, the
//!   other two still happened; reporting never panics.
//!
//! Where a report goes is process state (`main` fills it in as it learns the profile; the
//! update service's restart from a hidden window reports from its own thread), set with
//! [`notify_desktop`], [`log_to`] and [`after_update`].
//!
//! Updates (§41.6): a process re-exec'd into an installed update reads the restart marker
//! first ([`after_update`]); its failed start then says "Tuner was updated to X but couldn't
//! start: …" and how to go back to the previous package from pacman's cache — never done
//! automatically (the new build may have migrated the database). An exec that fails in the old
//! process is a [`RestartFailed`]: "Tuner couldn't restart", although its window was up.
//!
//! Not covered: a missing `libmpv.so` fails in the dynamic loader before `main` runs (the
//! package depends on mpv), and panics keep Rust's default report (a panic hook would also
//! fire for the panics the store and services catch on purpose).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Error;

/// Title of the notification for a failed start.
pub const TITLE: &str = "Tuner couldn't start";
/// Title of the notification for a failed re-exec into an installed update.
pub const RESTART_TITLE: &str = "Tuner couldn't restart";

/// Set once the window exists (eframe's creation callback): later errors aren't startup errors.
static WINDOW_UP: AtomicBool = AtomicBool::new(false);

/// Where a fatal error of this process goes: nothing but stderr at first, then the log file
/// once the profile's paths are known, and the desktop for a windowed launch.
#[derive(Debug, Default, Clone)]
struct Sinks {
    log_file: Option<PathBuf>,
    notify: bool,
    update: Option<Updated>,
}

static SINKS: Mutex<Sinks> = Mutex::new(Sinks { log_file: None, notify: false, update: None });

fn sinks() -> std::sync::MutexGuard<'static, Sinks> {
    SINKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// This process was just re-exec'd into an installed update (the restart marker's versions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub from: String,
    pub to: String,
}

/// The window is up: from now on [`report`] no longer notifies (except a [`RestartFailed`]).
pub fn window_up() {
    WINDOW_UP.store(true, Ordering::Relaxed);
}

/// The profile's `tuner.log` (`AppPaths::log_file`).
pub fn log_to(log_file: PathBuf) {
    sinks().log_file = Some(log_file);
}

/// This launch would open a window: tell the desktop when it can't.
pub fn notify_desktop(on: bool) {
    sinks().notify = on;
}

/// This start follows a restart into an update from `from` to `to` (the marker, read by `main`
/// before anything else can fail). Versions that don't look like one are ignored: they end up in
/// a notification and a shell command.
pub fn after_update(from: &str, to: &str) {
    if version_like(from) && version_like(to) {
        sinks().update = Some(Updated { from: from.into(), to: to.into() });
    }
}

/// A plausible version string (`0.2.7`, `0.3.0rc1`, `1.0.0-1`): short, no spaces, no markup. For
/// versions read from files (the database's `meta.app_version`, the restart marker).
pub fn version_like(v: &str) -> bool {
    !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b".+-_".contains(&b))
}

/// `eframe::run_native` failed (no Wayland/X11 display, no usable GL): typed so
/// [`user_message`] can say so. Its text is eframe's (no secrets: no URL reaches it).
#[derive(Debug)]
pub struct WindowError(pub String);

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "window: {}", self.0)
    }
}

impl std::error::Error for WindowError {}

/// The re-exec into an installed update failed (`update::restart::exec`, from `main` after the
/// window closed or from the updater's thread when a hidden window couldn't): the process is
/// going down with its window already gone, so it is reported on the desktop too.
#[derive(Debug)]
pub struct RestartFailed {
    /// The version it tried to start (the restart marker's), when known.
    pub to: Option<String>,
    pub why: std::io::Error,
}

impl std::fmt::Display for RestartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let to = self.to.as_deref().map(|v| format!(" (Tuner {v})")).unwrap_or_default();
        write!(f, "couldn't restart into the update{to}: {}", self.why)
    }
}

impl std::error::Error for RestartFailed {}

/// Report `err` on stderr, in the log and (see the module docs) as a notification. The caller
/// then exits with status 1.
pub fn report(err: &anyhow::Error) {
    let sinks = sinks().clone();
    let restart = err.chain().any(|c| c.is::<RestartFailed>());
    let notify = sinks.notify && (restart || !WINDOW_UP.load(Ordering::Relaxed));
    let desktop = |title: &str, body: &str| crate::platform::notify::notify(title, body, true);
    report_with(err, sinks.log_file.as_deref(), sinks.update.as_ref(), notify.then_some(&desktop as Notifier<'_>));
}

/// Shows a notification (title, body); false: no notification server answered.
type Notifier<'a> = &'a dyn Fn(&str, &str) -> bool;

/// [`report`] with its sinks passed in (tests).
fn report_with(err: &anyhow::Error, log_file: Option<&Path>, update: Option<&Updated>, notify: Option<Notifier<'_>>) {
    // what `fn main() -> anyhow::Result<()>` printed (the chain in its Debug form)
    let text = crate::redact::redact(&format!("{err:?}")).into_owned();
    let _ = std::io::Write::write_all(&mut std::io::stderr().lock(), format!("Error: {text}\n").as_bytes());
    if let Some(path) = log_file {
        crate::logging::record_fatal(path, &format!("{} exits: {err:#}", crate::APP_NAME));
    }
    if let Some(notify) = notify {
        let title = if err.chain().any(|c| c.is::<RestartFailed>()) { RESTART_TITLE } else { TITLE };
        // false: no notification server — stderr and the log have it
        notify(title, &user_message(err, update));
    }
}

/// The notification body for a fatal error: the reason in plain words and what to do, one or
/// two sentences, redacted. Known causes anywhere in the chain get their own words; anything
/// else is its own message as a sentence. Right after a restart into an update (`update`), it
/// names the new version and the way back.
pub fn user_message(err: &anyhow::Error, update: Option<&Updated>) -> String {
    let text = err.chain().find_map(|cause| {
        if let Some(e) = cause.downcast_ref::<Error>() {
            known(e)
        } else if let Some(r) = cause.downcast_ref::<RestartFailed>() {
            let to = r.to.as_deref().filter(|v| version_like(v)).map(|v| format!(" to {v}")).unwrap_or_default();
            Some(format!("Tuner was updated{to} but couldn't restart into it ({}). Start Tuner again from the launcher.", r.why))
        } else {
            cause.downcast_ref::<WindowError>().map(|w| {
                format!(
                    "It couldn't open its window ({}). Start it from your desktop session; if that doesn't help, check your graphics driver.",
                    w.0
                )
            })
        }
    });
    let mut text = text.unwrap_or_else(|| sentence(&format!("{err:#}")));
    if let Some(u) = update
        && !err.chain().any(|c| c.is::<RestartFailed>())
    {
        text = format!(
            "Tuner was updated to {to} but couldn't start: {text} To go back to {from}: sudo pacman -U /var/cache/pacman/pkg/omatuner-{from}-*.pkg.tar.zst, then turn off automatic installs in Settings ▸ About & logs ▸ Updates.",
            to = u.to,
            from = u.from
        );
    }
    crate::redact::redact(&text).into_owned()
}

/// Plain words for the crate errors a start can end with; `None`: say the error itself.
fn known(e: &Error) -> Option<String> {
    use rusqlite::ErrorCode as C;
    Some(match e {
        Error::NewerSchema { found, supported, opened_by: Some(v) } => format!(
            "Your library was last opened by Tuner {v} (database version {found}; this is {supported}). Install Tuner {v} again, or a newer one."
        ),
        Error::NewerSchema { found, supported, opened_by: None } => format!(
            "Your library was last opened by a newer Tuner (database version {found}; this is {supported}). Install that version again, or a newer one."
        ),
        Error::Db(db) => match db.sqlite_error_code() {
            Some(C::DatabaseCorrupt | C::NotADatabase) => {
                "Your library file (tuner.db) is damaged. Move it aside to start afresh, then restore a backup in Settings if you made one.".into()
            }
            Some(C::DatabaseBusy | C::DatabaseLocked) => "Your library is in use by another program. Close it and start Tuner again.".into(),
            Some(C::DiskFull) => "Your disk is full, so Tuner can't open its library. Free some space and start it again.".into(),
            Some(C::CannotOpen | C::ReadOnly | C::PermissionDenied) => {
                "Tuner can't write its library file (tuner.db). Check that its folder belongs to you and the disk isn't read-only.".into()
            }
            _ => sentence(&format!("Tuner couldn't open its library: {db}")),
        },
        Error::Io { path, source } => {
            format!("Tuner can't use {} ({source}). Check that it belongs to you and the disk isn't full or read-only.", path.display())
        }
        Error::Mpv(why) => format!("The video player (libmpv) couldn't start: {why}. Reinstall mpv and start Tuner again."),
        _ => return None,
    })
}

/// `text` as a sentence: capital first letter, a full stop at the end.
fn sentence(text: &str) -> String {
    let mut chars = text.trim().chars();
    let mut s: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if !s.ends_with(['.', '!', '?', ')']) {
        s.push('.');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn anyhow_of(e: Error) -> anyhow::Error {
        e.into()
    }

    /// The 0.2.6 incident: the schema refusal reads as what happened and what to do, with the
    /// version that wrote the database when it is known.
    #[test]
    fn a_newer_schema_says_which_tuner_to_install() {
        let unknown = anyhow_of(Error::NewerSchema { found: 14, supported: 13, opened_by: None });
        assert_eq!(
            user_message(&unknown, None),
            "Your library was last opened by a newer Tuner (database version 14; this is 13). Install that version again, or a newer one."
        );
        let known = anyhow_of(Error::NewerSchema { found: 14, supported: 13, opened_by: Some("0.3.0".into()) }).context("starting services");
        assert_eq!(
            user_message(&known, None),
            "Your library was last opened by Tuner 0.3.0 (database version 14; this is 13). Install Tuner 0.3.0 again, or a newer one."
        );
    }

    #[test]
    fn other_causes_get_plain_words_and_never_a_secret() {
        let corrupt = anyhow_of(Error::Db(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB), None)));
        assert!(user_message(&corrupt, None).starts_with("Your library file (tuner.db) is damaged"), "{}", user_message(&corrupt, None));
        let window = anyhow::Error::new(WindowError("no Wayland display".into()));
        assert!(user_message(&window, None).contains("couldn't open its window (no Wayland display)"), "{}", user_message(&window, None));
        let mpv = anyhow_of(Error::Mpv("init failed".into()));
        assert!(user_message(&mpv, None).contains("libmpv"), "{}", user_message(&mpv, None));
        // anything else is its own text as a sentence — redacted
        let leaky = anyhow::anyhow!("fetching http://h.tv/live/alice/s3cret/1.ts failed");
        let msg = user_message(&leaky, None);
        assert!(msg.starts_with("Fetching ") && msg.ends_with('.'), "{msg}");
        assert!(!msg.contains("alice") && !msg.contains("s3cret"), "{msg}");
        let io = anyhow_of(Error::io("/home/u/.local/share/tuner", std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
        assert!(user_message(&io, None).starts_with("Tuner can't use /home/u/.local/share/tuner (permission denied)"), "{}", user_message(&io, None));
    }

    /// Right after a restart into an update, a failed start names the new version and the way
    /// back (the old package from pacman's cache, done by the user, never by Tuner); a failed
    /// re-exec in the old process says so under its own title.
    #[test]
    fn after_an_update_the_message_names_it_and_the_way_back() {
        let update = Updated { from: "0.2.7".into(), to: "0.2.8".into() };
        let db = anyhow_of(Error::Db(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT), None)));
        assert_eq!(
            user_message(&db, Some(&update)),
            "Tuner was updated to 0.2.8 but couldn't start: Your library file (tuner.db) is damaged. Move it aside to start afresh, then \
             restore a backup in Settings if you made one. To go back to 0.2.7: sudo pacman -U \
             /var/cache/pacman/pkg/omatuner-0.2.7-*.pkg.tar.zst, then turn off automatic installs in Settings ▸ About & logs ▸ Updates."
        );
        let exec = anyhow::Error::new(RestartFailed { to: Some("0.2.8".into()), why: std::io::Error::from(std::io::ErrorKind::NotFound) });
        assert_eq!(
            user_message(&exec, Some(&update)),
            "Tuner was updated to 0.2.8 but couldn't restart into it (entity not found). Start Tuner again from the launcher."
        );
        let shown = RefCell::new(Vec::new());
        let sink = |t: &str, b: &str| {
            shown.borrow_mut().push((t.to_owned(), b.to_owned()));
            true
        };
        report_with(&exec, None, None, Some(&sink));
        assert_eq!(shown.into_inner()[0].0, RESTART_TITLE);
        // a marker with something odd in it is not quoted (it goes into a shell command)
        assert!(version_like("0.3.0rc1") && !version_like("0.2.7; rm -rf ~") && !version_like(""));
    }

    /// The report reaches all three places: stderr (not checked here), the log file and the
    /// notification sink — and a missing notification server is no failure.
    #[test]
    fn a_report_goes_to_the_log_and_the_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("state/tuner.log");
        let err = anyhow_of(Error::NewerSchema { found: 99, supported: 13, opened_by: None });
        let shown = RefCell::new(Vec::new());
        let sink = |t: &str, b: &str| {
            shown.borrow_mut().push((t.to_owned(), b.to_owned()));
            true
        };
        report_with(&err, Some(&log), None, Some(&sink));
        let shown = shown.into_inner();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0, TITLE);
        assert!(shown[0].1.contains("database version 99; this is 13"), "{}", shown[0].1);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("ERROR tuner: Tuner exits: database schema v99 is newer than this build (v13)"), "{text}");
        // no notification server, no log file: still no panic
        report_with(&err, None, None, Some(&|_: &str, _: &str| false));
    }
}
