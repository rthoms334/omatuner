//! Logging: `tracing` subscriber whose single output layer redacts every line, keeps the last
//! N lines in memory for the in-app log viewer (UHF "debug mode"), mirrors to stderr, and
//! appends to `~/.local/state/tuner/tuner.log` (rotated at 5 MB, one `.1` backup).
//!
//! The file sink runs on its own `log-writer` thread fed through a bounded channel, so a log
//! call on the UI thread never waits for the disk; if the writer falls behind, lines are
//! dropped from the file (never from the in-memory ring) and the loss is noted in the file.
//! The log file is 0600 inside the 0700 state dir: lines are redacted, but they still name the
//! user's providers and channels.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use tracing::Level;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

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
    fn to_text(&self) -> String { format!("{} {:>5} {}: {}", local_time(self.at), self.level, self.target, self.message) }
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
    pub fn new(cap: usize) -> Self { Self { inner: Arc::default(), cap } }

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
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .open(path)?;
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

/// Feeds a [`FileSink`] from its own thread. Lines that don't fit the queue are counted and
/// reported in the file once the writer catches up.
#[derive(Clone)]
struct FileWriter {
    tx: Sender<String>,
    dropped: Arc<AtomicU64>,
}

impl FileWriter {
    fn spawn(mut sink: FileSink) -> std::io::Result<Self> {
        let (tx, rx): (Sender<String>, Receiver<String>) = crossbeam_channel::bounded(FILE_QUEUE);
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
                let mut r = sink.write_line(&first);
                while let Ok(line) = rx.try_recv() {
                    r = r.and(sink.write_line(&line));
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
            }
        })?;
        Ok(Self { tx, dropped })
    }

    fn send(&self, line: String) {
        match self.tx.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
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

struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MsgVisitor(String::new());
        event.record(&mut v);
        let message = crate::redact::redact(&v.0).into_owned();
        let meta = event.metadata();
        if self.stderr {
            stderr_line(format_args!("{:>5} {}: {}", meta.level(), meta.target(), message));
        }
        let line = LogLine { at: crate::model::now(), level: *meta.level(), target: meta.target().to_owned(), message };
        if let Some(f) = &self.file {
            f.send(line.to_text());
        }
        self.buf.push(line);
    }
}

/// Install the global subscriber. `debug` = config `general.debug_log` (or `--debug`).
/// `RUST_LOG` overrides the filter. Returns the buffer for the Logs screen.
/// Called before `AppPaths::ensure`, so the state dir is created (0700) here if missing; a log
/// file that cannot be opened is reported on stderr and logging continues without it.
pub fn init(paths: &AppPaths, debug: bool) -> LogBuffer {
    let buf = LogBuffer::new(5000);
    let default = if debug { "tuner=debug,info" } else { "tuner=info,warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let log_file = paths.log_file();
    let file = match FileSink::open(&log_file, ROTATE_BYTES).and_then(FileWriter::spawn) {
        Ok(w) => Some(w),
        Err(e) => {
            stderr_line(format_args!("tuner: log file {} unavailable: {e}", log_file.display()));
            None
        }
    };
    let layer = RingLayer { buf: buf.clone(), stderr: true, file };
    let _ = tracing_subscriber::registry().with(filter).with(layer).try_init();
    buf
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use super::*;

    fn tmp() -> tempfile::TempDir { tempfile::Builder::new().prefix("tuner-logging-").tempdir().unwrap() }

    fn mode(p: &Path) -> u32 { std::fs::metadata(p).unwrap().permissions().mode() & 0o777 }

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
        let layer = RingLayer { buf: buf.clone(), stderr: false, file: Some(file) };
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let text = loop {
            let t = std::fs::read_to_string(&path).unwrap_or_default();
            if t.lines().count() >= 2 || Instant::now() > deadline {
                break t;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("WARN tuner::logging::tests: "), "{text}");
        assert!(text.contains("refresh failed for ***"), "{text}");
        assert!(text.contains("INFO tuner::logging::tests: plain line"), "{text}");
        assert!(!text.contains("hunter2") && !text.contains("layer-secret-91c2"), "{text}");
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
