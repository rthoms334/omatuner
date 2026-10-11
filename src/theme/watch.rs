//! Live reload via inotify (`notify` 8, non-recursive watches). Owner: WP-01.
//!
//! Watches (omarchy.md §1.4):
//! * `~/.local/state/omarchy/current/` — `theme.name` close-write (LAST write of
//!   `omarchy-theme-set`) or `theme` moved-in/created → reload. Ignore `next-theme`,
//!   `background`.
//! * `~/.config/omarchy/` — `shell.toml` create/modify/delete (text size override).
//! * `~/.config/fontconfig/` — `fonts.conf` → `ThemeEvent::FontsChanged`.
//! * Hyprland config (`~/.config/hypr/*.lua`) → re-query rounding 250 ms later (optional).
//!
//! Debounce 180 ms; reload on a background thread with `Theme::try_load` — on error (e.g. the
//! reload landed between `rm -rf current/theme` and `mv next-theme theme`) keep the current
//! theme and retry after 300 ms (never flash the fallback palette); send
//! `ThemeEvent::Changed(Arc<Theme>)`, skipping reloads that produce the theme already sent.
//! Font changes: resolve + read the font files on the background thread too and send
//! `ThemeEvent::FontsChanged(Arc<LoadedFonts>)` — the UI only installs them.
//! If a watch can't be set up (dir missing), poll the watched file's mtime every 5 s instead;
//! every poll also retries the watch, so a directory created later (first `omarchy-theme-set`,
//! a new `~/.config/fontconfig/`) switches to inotify and polling stops.
//! No hooks are written into the user's config.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};

use super::{Theme, ThemeEvent};
use crate::error::{Error, Result};
use crate::fonts::FontSet;
use crate::msg::{AppEvent, EventBus};
use crate::paths::AppPaths;

/// Quiet period after the last relevant event before reloading.
const DEBOUNCE: Duration = Duration::from_millis(180);
/// Hyprland reloads its config asynchronously; give it a moment before re-querying rounding.
const HYPR_DELAY: Duration = Duration::from_millis(250);
/// Retry delay after a failed (raced) theme load.
const RETRY: Duration = Duration::from_millis(300);
/// Give up after this many consecutive failed loads (~6 s) until the next event.
const MAX_RETRIES: u32 = 20;
/// Poll interval for files whose directory could not be watched.
const POLL: Duration = Duration::from_secs(5);

/// What a file-system change asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// Theme files or the user shell override changed.
    Theme,
    /// fontconfig configuration changed.
    Fonts,
    /// Hyprland config changed (rounding may differ).
    Hypr,
}

/// Directories watched (non-recursively) and the file names in them that matter.
struct Watched {
    current: PathBuf,
    omarchy_config: PathBuf,
    fontconfig: PathBuf,
    hypr: PathBuf,
}

impl Watched {
    fn new(paths: &AppPaths) -> Self {
        Self {
            current: paths.omarchy_current(),
            omarchy_config: paths.omarchy_config(),
            fontconfig: paths.fontconfig_dir(),
            hypr: paths.home.join(".config/hypr"),
        }
    }

    /// Map one notify event to the reload it requires, if any.
    fn classify(&self, ev: &notify::Event) -> Option<Trigger> {
        if matches!(ev.kind, EventKind::Access(AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read))) {
            return None;
        }
        let removal = matches!(ev.kind, EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)));
        ev.paths.iter().find_map(|p| {
            let dir = p.parent()?;
            let name = p.file_name()?.to_str()?;
            if dir == self.current {
                // `theme` removed / `theme.name` deleted are steps in the middle of a switch;
                // the moved-in directory and the final `theme.name` write are what count.
                (!removal && (name == "theme.name" || name == "theme")).then_some(Trigger::Theme)
            } else if dir == self.omarchy_config {
                (name == "shell.toml").then_some(Trigger::Theme)
            } else if dir == self.fontconfig {
                (name == "fonts.conf").then_some(Trigger::Fonts)
            } else if dir == self.hypr {
                (name.ends_with(".lua") || name.ends_with(".conf")).then_some(Trigger::Hypr)
            } else {
                None
            }
        })
    }

    /// Files to poll when their directory can't be watched.
    fn poll_targets(&self, dir: &Path) -> Vec<(PathBuf, Trigger)> {
        if dir == self.current {
            vec![(dir.join("theme.name"), Trigger::Theme)]
        } else if dir == self.omarchy_config {
            vec![(dir.join("shell.toml"), Trigger::Theme)]
        } else if dir == self.fontconfig {
            vec![(dir.join("fonts.conf"), Trigger::Fonts)]
        } else {
            Vec::new() // Hyprland rounding: optional, not worth polling
        }
    }
}

/// The inotify watcher, shared with the worker (it adds watches for directories that appear).
type SharedWatcher = Arc<Mutex<notify::RecommendedWatcher>>;

/// Keeps the watchers alive; dropping stops them.
pub struct ThemeWatcher {
    _watcher: Option<SharedWatcher>,
    _poller: Option<std::thread::JoinHandle<()>>,
    /// Last sender of the worker's channel besides the watchers' callbacks: dropping the
    /// watcher disconnects the channel and the worker thread exits.
    _alive: Sender<Trigger>,
}

impl ThemeWatcher {
    /// Start watching. Reloaded themes use the config's current `interface.ui_scale` (re-read
    /// on each reload, so a Settings change isn't undone); `ui_scale` is the fallback while the
    /// config file is unreadable or malformed.
    /// Directories that can't be watched are polled instead, so this fails only when the
    /// worker thread can't be spawned.
    pub fn start(paths: AppPaths, ui_scale: f32, bus: EventBus) -> Result<Self> {
        let watched = Arc::new(Watched::new(&paths));
        let (tx, rx) = crossbeam_channel::unbounded::<Trigger>();
        let mut polled: Vec<(PathBuf, Trigger)> = Vec::new();
        let dirs = [&watched.current, &watched.omarchy_config, &watched.fontconfig, &watched.hypr];

        let callback = {
            let (watched, tx) = (watched.clone(), tx.clone());
            move |res: notify::Result<notify::Event>| match res {
                Ok(ev) => {
                    if let Some(t) = watched.classify(&ev) {
                        let _ = tx.send(t);
                    }
                }
                Err(e) => tracing::debug!("theme watcher: {e}"),
            }
        };
        let mut watcher = None;
        match notify::recommended_watcher(callback) {
            Ok(mut w) => {
                for dir in dirs {
                    if let Err(e) = w.watch(dir, RecursiveMode::NonRecursive) {
                        tracing::debug!("theme watcher: cannot watch {}: {e}; polling", dir.display());
                        polled.extend(watched.poll_targets(dir));
                    }
                }
                watcher = Some(Arc::new(Mutex::new(w)));
            }
            Err(e) => {
                tracing::info!("theme watcher: inotify unavailable ({e}); polling every {}s", POLL.as_secs());
                for dir in dirs {
                    polled.extend(watched.poll_targets(dir));
                }
            }
        }

        let worker = Worker { paths, ui_scale, bus, poller: Poller::new(polled), watcher: watcher.as_ref().map(Arc::downgrade), last_sent: None };
        let handle: JoinHandle<()> = std::thread::Builder::new()
            .name("theme-watch".into())
            .spawn(move || worker.run(rx))
            .map_err(|e| Error::other(format!("theme watcher thread: {e}")))?;
        Ok(Self { _watcher: watcher, _poller: Some(handle), _alive: tx })
    }
}

/// mtime + size of a file; `None` while it doesn't exist.
type Fingerprint = Option<(SystemTime, u64)>;

/// mtime/size fingerprints of polled files.
struct Poller {
    files: Vec<(PathBuf, Trigger, Fingerprint)>,
}

impl Poller {
    fn new(targets: Vec<(PathBuf, Trigger)>) -> Self {
        let files = targets
            .into_iter()
            .map(|(path, trigger)| {
                let seen = fingerprint(&path);
                (path, trigger, seen)
            })
            .collect();
        Self { files }
    }

    fn is_active(&self) -> bool {
        !self.files.is_empty()
    }

    /// Stop polling the files of every directory `watch` could watch now (it exists by now);
    /// returns those directories and the triggers of one last poll of their files, taken after
    /// the watch was added (a change before it is caught here, one after it by inotify).
    fn upgrade(&mut self, mut watch: impl FnMut(&Path) -> bool) -> (Vec<PathBuf>, Vec<Trigger>) {
        let mut dirs: Vec<PathBuf> = self.files.iter().filter_map(|(p, ..)| p.parent().map(Path::to_path_buf)).collect();
        dirs.sort();
        dirs.dedup();
        let watched: Vec<PathBuf> = dirs.into_iter().filter(|d| d.is_dir() && watch(d)).collect();
        let mut changed = Vec::new();
        self.files.retain(|(p, trigger, last)| {
            if !watched.iter().any(|d| p.parent() == Some(d.as_path())) {
                return true;
            }
            if fingerprint(p) != *last {
                changed.push(*trigger);
            }
            false
        });
        (watched, changed)
    }

    /// Triggers for files whose fingerprint changed since the last poll.
    fn poll(&mut self) -> Vec<Trigger> {
        let mut out = Vec::new();
        for (path, trigger, last) in &mut self.files {
            let now = fingerprint(path);
            if now != *last {
                *last = now;
                out.push(*trigger);
            }
        }
        out
    }
}

fn fingerprint(p: &Path) -> Fingerprint {
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

struct Worker {
    paths: AppPaths,
    /// Start-up `ui_scale`, used only while the config is unreadable or malformed.
    ui_scale: f32,
    bus: EventBus,
    poller: Poller,
    /// Retried for polled directories on every poll (`None`: inotify unavailable). Weak: the
    /// watcher holds the channel sender, so dropping `ThemeWatcher` must free it to end this loop.
    watcher: Option<std::sync::Weak<Mutex<notify::RecommendedWatcher>>>,
    /// The theme most recently sent (spurious events that change nothing send nothing).
    last_sent: Option<Arc<Theme>>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Trigger>) {
        let mut theme_due: Option<Instant> = None;
        let mut fonts_due: Option<Instant> = None;
        let mut retries = 0u32;
        let mut poll_due = self.poller.is_active().then(|| Instant::now() + POLL);
        loop {
            let next = [theme_due, fonts_due, poll_due].into_iter().flatten().min();
            let msg = match next {
                Some(at) => rx.recv_deadline(at),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            let mut triggers = match msg {
                Ok(t) => vec![t],
                Err(RecvTimeoutError::Timeout) => Vec::new(),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            triggers.extend(rx.try_iter());
            let now = Instant::now();
            if poll_due.is_some_and(|d| d <= now) {
                if let Some(w) = self.watcher.as_ref().and_then(std::sync::Weak::upgrade) {
                    let watch = |d: &Path| w.lock().unwrap_or_else(|e| e.into_inner()).watch(d, RecursiveMode::NonRecursive).is_ok();
                    let (dirs, changed) = self.poller.upgrade(watch);
                    triggers.extend(changed);
                    for d in dirs {
                        tracing::info!("theme watcher: {} exists now; watching it instead of polling", d.display());
                    }
                }
                triggers.extend(self.poller.poll());
                poll_due = self.poller.is_active().then(|| now + POLL);
            }
            for t in triggers {
                match t {
                    Trigger::Theme => {
                        theme_due = Some(now + DEBOUNCE);
                        retries = 0;
                    }
                    Trigger::Hypr => {
                        theme_due = Some(theme_due.map_or(now + HYPR_DELAY, |d| d.max(now + HYPR_DELAY)));
                        retries = 0;
                    }
                    Trigger::Fonts => fonts_due = Some(now + DEBOUNCE),
                }
            }

            if theme_due.is_some_and(|d| d <= now) {
                theme_due = None;
                match Theme::try_load(&self.paths, self.current_ui_scale()) {
                    Ok(theme) => {
                        retries = 0;
                        self.publish(theme);
                    }
                    Err(e) if retries < MAX_RETRIES => {
                        retries += 1;
                        tracing::debug!("theme reload: {e}; retrying (keeping the current theme)");
                        theme_due = Some(Instant::now() + RETRY);
                    }
                    Err(e) => tracing::warn!("theme reload: {e}; keeping the current theme"),
                }
            }
            if fonts_due.is_some_and(|d| d <= now) {
                fonts_due = None;
                let cfg = crate::config::Config::load(&self.paths.config_file()).map(|c| c.fonts).unwrap_or_default();
                let fonts = FontSet::resolve(&cfg).load();
                tracing::info!("fonts reloaded ({fonts:?})");
                self.bus.send(AppEvent::Theme(ThemeEvent::FontsChanged(Arc::new(fonts))));
            }
        }
    }

    /// `interface.ui_scale` as currently saved (Settings may have changed it since start-up);
    /// the start-up value while the config is unreadable or malformed.
    fn current_ui_scale(&self) -> f32 {
        crate::config::Config::load(&self.paths.config_file()).map_or(self.ui_scale, |c| c.interface.ui_scale)
    }

    fn publish(&mut self, theme: Theme) {
        if self.last_sent.as_deref().is_some_and(|prev| same_theme(prev, &theme)) {
            return;
        }
        tracing::info!("theme '{}' ({:?}) reloaded", theme.name, theme.palette.mode);
        let theme = Arc::new(theme);
        self.last_sent = Some(theme.clone());
        self.bus.send(AppEvent::Theme(ThemeEvent::Changed(theme)));
    }
}

fn same_theme(a: &Theme, b: &Theme) -> bool {
    a.name == b.name && a.palette == b.palette && a.shell == b.shell && a.tokens == b.tokens
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, DataChange, RemoveKind};

    use super::*;

    fn watched() -> Watched {
        let mut paths = AppPaths::rooted(Path::new("/tmp/x"));
        paths.home = PathBuf::from("/h");
        Watched::new(&paths)
    }

    fn ev(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn classifies_theme_set_sequence() {
        let w = watched();
        let cur = "/h/.local/state/omarchy/current";
        let close_write = EventKind::Access(AccessKind::Close(AccessMode::Write));
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        // ignored: staging dir, background symlink, removal of the old theme, reads
        assert_eq!(w.classify(&ev(EventKind::Create(CreateKind::Folder), &[&format!("{cur}/next-theme")])), None);
        assert_eq!(w.classify(&ev(EventKind::Create(CreateKind::Any), &[&format!("{cur}/background")])), None);
        assert_eq!(w.classify(&ev(EventKind::Remove(RemoveKind::Folder), &[&format!("{cur}/theme")])), None);
        assert_eq!(w.classify(&ev(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &[&format!("{cur}/theme")])), None);
        assert_eq!(w.classify(&ev(EventKind::Access(AccessKind::Open(notify::event::AccessMode::Any)), &[&format!("{cur}/theme.name")])), None);
        // relevant: theme moved in, theme.name written
        assert_eq!(w.classify(&ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[&format!("{cur}/theme")])), Some(Trigger::Theme));
        assert_eq!(
            w.classify(&ev(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &[&format!("{cur}/next-theme"), &format!("{cur}/theme")])),
            Some(Trigger::Theme)
        );
        assert_eq!(w.classify(&ev(close_write, &[&format!("{cur}/theme.name")])), Some(Trigger::Theme));
        assert_eq!(w.classify(&ev(modify, &[&format!("{cur}/theme.name")])), Some(Trigger::Theme));
        // files inside the theme dir are not watched (non-recursive), and would be ignored
        assert_eq!(w.classify(&ev(modify, &[&format!("{cur}/theme/colors.toml")])), None);
    }

    #[test]
    fn classifies_override_fonts_hypr() {
        let w = watched();
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert_eq!(w.classify(&ev(modify, &["/h/.config/omarchy/shell.toml"])), Some(Trigger::Theme));
        assert_eq!(w.classify(&ev(EventKind::Remove(RemoveKind::File), &["/h/.config/omarchy/shell.toml"])), Some(Trigger::Theme));
        assert_eq!(w.classify(&ev(modify, &["/h/.config/omarchy/shell.json"])), None);
        assert_eq!(w.classify(&ev(modify, &["/h/.config/fontconfig/fonts.conf"])), Some(Trigger::Fonts));
        assert_eq!(w.classify(&ev(modify, &["/h/.config/fontconfig/fonts.conf.bak"])), None);
        assert_eq!(w.classify(&ev(modify, &["/h/.config/hypr/looknfeel.lua"])), Some(Trigger::Hypr));
        assert_eq!(w.classify(&ev(modify, &["/h/.config/hypr/hyprland.conf"])), Some(Trigger::Hypr));
        assert_eq!(w.classify(&ev(modify, &["/elsewhere/shell.toml"])), None);
    }

    /// A polled directory that appeared is watched instead; the others stay polled.
    #[test]
    fn polled_directories_switch_to_watches_once_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("omarchy"), dir.path().join("fontconfig"));
        let mut p = Poller::new(vec![(a.join("shell.toml"), Trigger::Theme), (b.join("fonts.conf"), Trigger::Fonts)]);
        let mut asked = Vec::new();
        let (dirs, _) = p.upgrade(|d| {
            asked.push(d.to_path_buf());
            true
        });
        assert!(dirs.is_empty());
        assert!(asked.is_empty(), "missing directories are not tried");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        // written before the watch was added: reported by the upgrade, not lost
        std::fs::write(a.join("shell.toml"), "x\n").unwrap();
        // the watch of `b` fails (e.g. out of inotify watches): it keeps being polled
        assert_eq!(p.upgrade(|d| d == a), (vec![a.clone()], vec![Trigger::Theme]));
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].0, b.join("fonts.conf"));
        assert!(p.is_active());
        assert_eq!(p.upgrade(|_| true), (vec![b], vec![]));
        assert!(!p.is_active(), "nothing left to poll");
    }

    #[test]
    fn poller_reports_changes_once() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("theme.name");
        let mut p = Poller::new(vec![(f.clone(), Trigger::Theme)]);
        assert!(p.poll().is_empty());
        std::fs::write(&f, "theme-a\n").unwrap();
        assert_eq!(p.poll(), vec![Trigger::Theme]);
        assert!(p.poll().is_empty());
        std::fs::remove_file(&f).unwrap();
        assert_eq!(p.poll(), vec![Trigger::Theme]);
    }
}
