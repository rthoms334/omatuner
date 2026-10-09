//! Logging: `tracing` subscriber whose single output layer redacts every line, keeps the last
//! N lines in memory for the in-app log viewer (UHF "debug mode"), mirrors to stderr, and
//! appends to `~/.local/state/tuner/tuner.log` (rotated at 5 MB, one `.1` backup).
//!
//! Skeleton: in-memory ring + stderr are implemented. WP-08 owns the file sink/rotation,
//! export ("Save log…") and tests.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::paths::AppPaths;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: crate::model::Timestamp,
    pub level: Level,
    pub target: String,
    /// Already redacted.
    pub message: String,
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
    pub fn export(&self, path: &std::path::Path) -> crate::error::Result<()> {
        todo!("WP-08: export log lines as text")
    }
}

struct RingLayer {
    buf: LogBuffer,
    stderr: bool,
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
            eprintln!("{:>5} {}: {}", meta.level(), meta.target(), message);
        }
        self.buf.push(LogLine {
            at: crate::model::now(),
            level: *meta.level(),
            target: meta.target().to_owned(),
            message,
        });
    }
}

/// Install the global subscriber. `debug` = config `general.debug_log` (or `--debug`).
/// `RUST_LOG` overrides the filter. Returns the buffer for the Logs screen.
pub fn init(paths: &AppPaths, debug: bool) -> LogBuffer {
    let buf = LogBuffer::new(5000);
    let default = if debug { "tuner=debug,info" } else { "tuner=info,warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let layer = RingLayer { buf: buf.clone(), stderr: true };
    let _ = tracing_subscriber::registry().with(filter).with(layer).try_init();
    buf
}
