//! Logging: `tracing` subscriber whose single output layer redacts every line, keeps the last
//! N lines in memory for the in-app log viewer (UHF "debug mode"), mirrors to stderr, and
//! appends to `~/.local/state/tuner/tuner.log` (rotated at 5 MB, one `.1` backup).
//!
//! The file sink runs on its own `log-writer` thread fed through a bounded channel, so a log
//! call on the UI thread never waits for the disk; if the writer falls behind, lines are
//! dropped from the file (never from the in-memory ring) and the loss is noted in the file.
//! The log file is 0600 inside the 0700 state dir: lines are redacted, but they still name the
//! user's providers and channels.
//!
//! Filtering: our own targets (`tuner::*`) log at INFO, or DEBUG with debug logging. Other
//! crates (through `tracing` or the `log` bridge) are OFF in normal use — their WARN/ERROR
//! lines are noise to users (a TLS verifier rejecting a logo host, calloop event races) and
//! every real problem has a `tuner::*` line of its own. With debug logging they are recorded
//! from INFO up, DEMOTED to DEBUG and under their real target (not the bridge's `log`).
//! `general.debug_log` applies live ([`set_debug`]); `--debug` pins it on ([`pin_debug`]);
//! `RUST_LOG` replaces the filter entirely.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use tracing::Level;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

use crate::error::{Error, Result};
use crate::paths::AppPaths;

/// Rotate `tuner.log` → `tuner.log.1` when the next line would push it past this size.
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;
/// Lines queued for the file writer before new ones are dropped.
const FILE_QUEUE: usize = 4096;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: crate::model::Timestamp,
    pub level: Level,
    pub target: String,
    /// Already redacted.
    pub message: String,
}

impl LogLine {
    /// `2026-10-10 20:15:03  WARN tuner::player: message` (local time).
    fn to_text(&self) -> String {
        format!("{} {:>5} {}: {}", local_time(self.at), self.level, self.target, self.message)
    }
}

/// Local wall-clock rendering of a unix timestamp (UTC if the zone database is unavailable).
fn local_time(at: crate::model::Timestamp) -> String {
    match jiff::Timestamp::from_second(at) {
        Ok(ts) => ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => at.to_string(),
    }
}

/// Shared ring buffer read by the Logs screen.
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
    cap: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self { inner: Arc::default(), cap }
    }

    pub fn push(&self, line: LogLine) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= self.cap.max(1) {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// Copy of the lines at or above `min` level, oldest first.
    pub fn snapshot(&self, min: Level) -> Vec<LogLine> {
        let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().filter(|l| l.level <= min).cloned().collect()
    }

    /// Write all lines to `path` (Logs screen "Export"). Lines are already redacted.
    /// The file is created (or truncated) with mode 0600. Blocking file I/O: call it from a
    /// worker, not the UI thread.
    pub fn export(&self, path: &Path) -> Result<()> {
        let text = {
            let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut s = String::with_capacity(q.len() * 96);
            for l in q.iter() {
                s.push_str(&l.to_text());
                s.push('\n');
            }
            s
        };
        let mut f = open_private(path, false).map_err(|e| Error::io(path, e))?;
        f.write_all(text.as_bytes()).and_then(|_| f.flush()).map_err(|e| Error::io(path, e))
    }
}

/// Open `path` for writing with mode 0600 (also tightening an existing file): append or
/// truncate.
fn open_private(path: &Path, append: bool) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).mode(0o600).open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

/// Size-rotated append-only log file. Owned by the `log-writer` thread (tests drive it
/// directly).
struct FileSink {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    size: u64,
    max: u64,
}

impl FileSink {
    /// Open (creating the 0700 parent dir if needed). A file already over `max` is rotated
    /// first, so a restart never appends to an oversized log.
    fn open(path: &Path, max: u64) -> std::io::Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut sink = Self { path: path.to_path_buf(), file: None, size: 0, max };
        sink.reopen()?;
        if sink.size >= max {
            sink.rotate()?;
        }
        Ok(sink)
    }

    fn reopen(&mut self) -> std::io::Result<()> {
        let f = open_private(&self.path, true)?;
        self.size = f.metadata()?.len();
        self.file = Some(BufWriter::new(f));
        Ok(())
    }

    /// Current file → `<name>.1` (replacing the previous backup), then start a new file.
    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(mut f) = self.file.take() {
            f.flush()?;
        }
        let mut backup = self.path.clone().into_os_string();
        backup.push(".1");
        std::fs::rename(&self.path, PathBuf::from(backup))?;
        self.reopen()
    }

    /// Append one line (a newline is added). Rotates before writing when the line would push
    /// the file past `max`.
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let len = line.len() as u64 + 1;
        if self.size > 0 && self.size + len > self.max {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.reopen()?;
        }
        let f = self.file.as_mut().ok_or_else(|| std::io::Error::other("log file closed"))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        self.size += len;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// What the `log-writer` thread is asked to do.
enum FileCmd {
    Line(String),
    /// Answer once everything queued before it is on disk.
    Flush(Sender<()>),
}

/// Feeds a [`FileSink`] from its own thread. Lines that don't fit the queue are counted and
/// reported in the file once the writer catches up.
#[derive(Clone)]
struct FileWriter {
    tx: Sender<FileCmd>,
    dropped: Arc<AtomicU64>,
}

impl FileWriter {
    fn spawn(mut sink: FileSink) -> std::io::Result<Self> {
        let (tx, rx): (Sender<FileCmd>, Receiver<FileCmd>) = crossbeam_channel::bounded(FILE_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        std::thread::Builder::new().name("log-writer".into()).spawn(move || {
            // Never log from here (the layer would feed this thread its own errors).
            let mut failed = false;
            while let Ok(first) = rx.recv() {
                let n = lost.swap(0, Ordering::Relaxed);
                if n > 0 {
                    let _ = sink.write_line(&format!("[{n} log lines dropped: writer fell behind]"));
                }
                let (mut r, mut acks) = (Ok(()), Vec::new());
                for cmd in std::iter::once(first).chain(rx.try_iter()) {
                    match cmd {
                        FileCmd::Line(line) => r = r.and(sink.write_line(&line)),
                        FileCmd::Flush(ack) => acks.push(ack),
                    }
                }
                r = r.and(sink.flush());
                match r {
                    Ok(()) => failed = false,
                    Err(e) if !failed => {
                        failed = true;
                        stderr_line(format_args!("tuner: writing {}: {e}", sink.path.display()));
                        sink.file = None; // reopened on the next line
                    }
                    Err(_) => sink.file = None,
                }
                for ack in acks {
                    let _ = ack.send(());
                }
            }
        })?;
        Ok(Self { tx, dropped })
    }

    fn send(&self, line: String) {
        match self.tx.try_send(FileCmd::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Block until the lines sent so far are written, or for at most about `timeout`.
    fn flush(&self, timeout: Duration) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.tx.send_timeout(FileCmd::Flush(tx), timeout).is_ok() {
            let _ = rx.recv_timeout(timeout);
        }
    }
}

/// `eprintln!` without the panic: stderr may be a closed terminal (EIO) or a pipe whose reader
/// has exited (EPIPE), and a log call must never take the calling thread down with it.
fn stderr_line(args: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr().lock(), "{args}");
}

struct RingLayer {
    buf: LogBuffer,
    stderr: bool,
    file: Option<FileWriter>,
}

/// Message text plus the `log` bridge's real target (`log.target`; its other `log.*` fields —
/// module path, file, line — are dropped).
#[derive(Default)]
struct MsgVisitor {
    text: String,
    log_target: Option<String>,
}

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => {
                let _ = write!(self.text, "{value:?}");
            }
            "log.target" => self.log_target = Some(format!("{value:?}").trim_matches('"').to_owned()),
            n if n.starts_with("log.") => {}
            n => {
                let _ = write!(self.text, " {n}={value:?}");
            }
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.text.push_str(value),
            "log.target" => self.log_target = Some(value.to_owned()),
            n if n.starts_with("log.") => {}
            n => {
                let _ = write!(self.text, " {n}={value}");
            }
        }
    }
}

/// Our own crate (`tuner`, `tuner::*`).
fn is_own(target: &str) -> bool {
    target == "tuner" || target.starts_with("tuner::")
}

/// The level a line is recorded at: other crates' lines never above DEBUG (see module docs).
fn recorded_level(target: &str, level: Level) -> Level {
    if is_own(target) { level } else { level.max(Level::DEBUG) }
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        let message = crate::redact::redact(&v.text).into_owned();
        let meta = event.metadata();
        let target = v.log_target.unwrap_or_else(|| meta.target().to_owned());
        let level = recorded_level(&target, *meta.level());
        if self.stderr {
            stderr_line(format_args!("{level:>5} {target}: {message}"));
        }
        let line = LogLine { at: crate::model::now(), level, target, message };
        if let Some(f) = &self.file {
            f.send(line.to_text());
        }
        self.buf.push(line);
    }
}

/// The filter for normal use / debug logging (see module docs).
fn default_filter(debug: bool) -> EnvFilter {
    EnvFilter::new(if debug { "tuner=debug,info" } else { "tuner=info,off" })
}

/// Live control of the installed filter.
struct FilterControl {
    handle: reload::Handle<EnvFilter, Registry>,
    /// `RUST_LOG` chose the filter: Settings don't touch it.
    from_env: bool,
    /// `--debug`: debug logging stays on whatever Settings say.
    pinned: AtomicBool,
}

impl FilterControl {
    fn set_debug(&self, debug: bool) {
        if self.from_env {
            return;
        }
        let debug = debug || self.pinned.load(Ordering::Relaxed);
        if let Err(e) = self.handle.reload(default_filter(debug)) {
            stderr_line(format_args!("tuner: changing the log filter: {e}"));
        }
    }
}

static FILTER: OnceLock<FilterControl> = OnceLock::new();
/// The installed subscriber's file writer, for [`flush`].
static FILE: OnceLock<FileWriter> = OnceLock::new();

/// Wait (at most about `timeout`) until the lines logged so far are in the log file: for a
/// direct `exit`, which skips the writer thread's own drain.
pub fn flush(timeout: Duration) {
    if let Some(w) = FILE.get() {
        w.flush(timeout);
    }
}

/// Apply `general.debug_log` to the running subscriber (Settings edit; no restart needed).
pub fn set_debug(debug: bool) {
    if let Some(f) = FILTER.get() {
        f.set_debug(debug);
    }
}

/// `--debug`: keep debug logging on even when the setting is off.
pub fn pin_debug() {
    if let Some(f) = FILTER.get() {
        f.pinned.store(true, Ordering::Relaxed);
        f.set_debug(true);
    }
}

/// Install the global subscriber. `debug` = config `general.debug_log` (or `--debug`; call
/// [`pin_debug`] too so a Settings change can't turn it off). `RUST_LOG` overrides the filter.
/// Returns the buffer for the Logs screen.
/// Called before `AppPaths::ensure`, so the state dir is created (0700) here if missing; a log
/// file that cannot be opened is reported on stderr and logging continues without it.
pub fn init(paths: &AppPaths, debug: bool) -> LogBuffer {
    let buf = LogBuffer::new(5000);
    let env = EnvFilter::try_from_default_env().ok();
    let from_env = env.is_some();
    let (filter, handle) = reload::Layer::new(env.unwrap_or_else(|| default_filter(debug)));
    let log_file = paths.log_file();
    let file = match FileSink::open(&log_file, ROTATE_BYTES).and_then(FileWriter::spawn) {
        Ok(w) => Some(w),
        Err(e) => {
            stderr_line(format_args!("tuner: log file {} unavailable: {e}", log_file.display()));
            None
        }
    };
    let layer = RingLayer { buf: buf.clone(), stderr: true, file: file.clone() };
    if tracing_subscriber::registry().with(filter).with(layer).try_init().is_ok() {
        let _ = FILTER.set(FilterControl { handle, from_env, pinned: AtomicBool::new(false) });
        if let Some(f) = file {
            let _ = FILE.set(f);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("tuner-logging-").tempdir().unwrap()
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn rotates_at_five_megabytes_with_one_backup() {
        let dir = tmp();
        let path = dir.path().join("state/tuner.log");
        let mut sink = FileSink::open(&path, ROTATE_BYTES).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let line = "x".repeat(1023); // 1 KiB with the newline
        for _ in 0..(5 * 1024) {
            sink.write_line(&line).unwrap();
        }
        sink.flush().unwrap();
        let backup = dir.path().join("state/tuner.log.1");
        assert!(!backup.exists(), "exactly 5 MiB fits without rotating");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), ROTATE_BYTES);
        sink.write_line("first line after rotation").unwrap();
        sink.flush().unwrap();
        assert_eq!(std::fs::metadata(&backup).unwrap().len(), ROTATE_BYTES);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first line after rotation\n");
        assert_eq!(mode(&path), 0o600);
        // a second rotation replaces the old backup (only one is kept)
        for _ in 0..(5 * 1024) {
            sink.write_line(&line).unwrap();
        }
        sink.flush().unwrap();
        assert!(std::fs::read_to_string(&backup).unwrap().starts_with("first line after rotation\n"));
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn oversized_file_is_rotated_on_open_and_appends_otherwise() {
        let dir = tmp();
        let path = dir.path().join("tuner.log");
        std::fs::write(&path, "old\n").unwrap();
        let mut sink = FileSink::open(&path, 16).unwrap();
        sink.write_line("new").unwrap();
        sink.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        drop(sink);
        std::fs::write(&path, "0123456789abcdefXYZ\n").unwrap();
        let sink = FileSink::open(&path, 16).unwrap();
        assert_eq!(sink.size, 0);
        assert_eq!(std::fs::read_to_string(dir.path().join("tuner.log.1")).unwrap(), "0123456789abcdefXYZ\n");
    }

    #[test]
    fn layer_redacts_ring_and_file() {
        let dir = tmp();
        let path = dir.path().join("tuner.log");
        crate::redact::register_secret("layer-secret-91c2");
        let buf = LogBuffer::new(10);
        let file = FileWriter::spawn(FileSink::open(&path, ROTATE_BYTES).unwrap()).unwrap();
        let layer = RingLayer { buf: buf.clone(), stderr: false, file: Some(file.clone()) };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(url = "http://h.tv/get.php?username=bob&password=hunter2", "refresh failed for layer-secret-91c2");
            tracing::info!("plain line");
        });
        let lines = buf.snapshot(Level::TRACE);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].message.contains("refresh failed for ***"), "{}", lines[0].message);
        assert!(lines[0].message.contains("url=http://h.tv/get.php?username=***&password=***"), "{}", lines[0].message);
        assert_eq!(lines[0].level, Level::WARN);
        assert_eq!(buf.snapshot(Level::WARN).len(), 1);
        file.flush(Duration::from_secs(5)); // what a direct exit relies on
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("WARN tuner::logging::tests: "), "{text}");
        assert!(text.contains("refresh failed for ***"), "{text}");
        assert!(text.contains("INFO tuner::logging::tests: plain line"), "{text}");
        assert!(!text.contains("hunter2") && !text.contains("layer-secret-91c2"), "{text}");
    }

    /// Other crates' lines (direct or through the `log` bridge) are off in normal use; with
    /// debug logging they are recorded as DEBUG under their real target. The filter follows
    /// the setting live.
    #[test]
    fn third_party_lines_are_filtered_demoted_and_follow_the_setting_live() {
        let buf = LogBuffer::new(20);
        let (filter, handle) = reload::Layer::new(default_filter(false));
        let control = FilterControl { handle, from_env: false, pinned: AtomicBool::new(false) };
        let subscriber = tracing_subscriber::registry().with(filter).with(RingLayer { buf: buf.clone(), stderr: false, file: None });
        let emit = || {
            // what `tracing-log` makes of `log::error!` in rustls-platform-verifier
            tracing::event!(
                target: "log",
                Level::ERROR,
                log.target = "rustls_platform_verifier::verification",
                log.module_path = "rustls_platform_verifier::verification",
                log.line = 7u32,
                "failed to verify TLS certificate: invalid peer certificate: UnknownIssuer"
            );
            tracing::warn!(target: "calloop::loop_logic", "Received an event for non-existence source");
            tracing::info!(target: "tuner::images", "own line");
            tracing::debug!(target: "tuner::images", "own debug line");
        };
        tracing::subscriber::with_default(subscriber, || {
            emit();
            let lines = buf.snapshot(Level::TRACE);
            assert_eq!(lines.iter().map(|l| (l.level, l.target.as_str())).collect::<Vec<_>>(), [(Level::INFO, "tuner::images")]);

            control.set_debug(true);
            emit();
            let lines = buf.snapshot(Level::TRACE);
            let new: Vec<_> = lines[1..].iter().map(|l| (l.level, l.target.as_str(), l.message.as_str())).collect();
            assert_eq!(
                new,
                [
                    (
                        Level::DEBUG,
                        "rustls_platform_verifier::verification",
                        "failed to verify TLS certificate: invalid peer certificate: UnknownIssuer"
                    ),
                    (Level::DEBUG, "calloop::loop_logic", "Received an event for non-existence source"),
                    (Level::INFO, "tuner::images", "own line"),
                    (Level::DEBUG, "tuner::images", "own debug line"),
                ]
            );
            assert!(buf.snapshot(Level::WARN).is_empty(), "no WARN/ERROR from other crates, even in debug");

            // the setting turned off again: back to INFO; `--debug` (pinned) wins over it
            control.set_debug(false);
            emit();
            assert_eq!(buf.snapshot(Level::TRACE).len(), 6);
            control.pinned.store(true, Ordering::Relaxed);
            control.set_debug(false);
            emit();
            assert_eq!(buf.snapshot(Level::TRACE).len(), 10);
        });
    }

    #[test]
    fn export_writes_every_line_privately() {
        let dir = tmp();
        let buf = LogBuffer::new(2);
        for (i, level) in [Level::INFO, Level::DEBUG, Level::ERROR].into_iter().enumerate() {
            buf.push(LogLine { at: 1_760_000_000 + i as i64, level, target: "tuner::t".into(), message: format!("m{i}") });
        }
        let out = dir.path().join("export.txt");
        std::fs::write(&out, "previous content that is longer than the export").unwrap();
        buf.export(&out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "ring keeps the newest `cap` lines: {text}");
        assert!(lines[0].ends_with("DEBUG tuner::t: m1"), "{text}");
        assert!(lines[1].ends_with("ERROR tuner::t: m2"), "{text}");
        assert_eq!(mode(&out), 0o600);
        assert!(buf.export(&dir.path().join("missing/dir/x.txt")).is_err());
    }

    #[test]
    fn full_queue_counts_dropped_lines() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let w = FileWriter { tx, dropped: Arc::new(AtomicU64::new(0)) };
        w.send("a".into());
        w.send("b".into());
        w.send("c".into());
        assert_eq!(w.dropped.load(Ordering::Relaxed), 2);
    }
}
