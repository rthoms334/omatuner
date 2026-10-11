//! Forwarding of mpv's own log lines (requested at `warn` and above) into our log, under the
//! target `tuner::mpv`.
//!
//! * Levels: mpv `fatal` → WARN, `error` → INFO, `warn` → DEBUG. A failing stream is already
//!   reported by the player state (error overlay, reconnects, the controller's own lines); mpv's
//!   lines are the detail behind it, not separate problems.
//! * Rate limit per mpv module (`prefix`): at most [`BURST`] lines per [`WINDOW`]; the rest are
//!   counted and summarised once ("… 37 similar lines suppressed") when the window ends. A dead
//!   DASH/HLS stream retrying every fragment therefore costs a handful of lines, not hundreds.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::Level;

/// Lines per module shown in one window.
const BURST: u32 = 5;
/// Length of a rate-limit window.
const WINDOW: Duration = Duration::from_secs(10);

/// Our level for an mpv log level.
pub fn level_of(mpv_level: &str) -> Level {
    match mpv_level {
        "fatal" => Level::WARN,
        "error" => Level::INFO,
        _ => Level::DEBUG,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Log the line at this level.
    Show(Level),
    /// Counted for the window's summary.
    Suppress,
}

/// One module's window.
struct Window {
    started: Instant,
    shown: u32,
    suppressed: u32,
    /// Most severe level among the suppressed lines.
    worst: Level,
}

/// Per-module rate limiter (owned by the controller thread).
#[derive(Default)]
pub struct MpvLog {
    windows: HashMap<String, Window>,
}

/// A window that ended with suppressed lines: (module, count, level to log the summary at).
pub type Summary = (String, u32, Level);

impl MpvLog {
    /// Decide about one line of `prefix` at `level` (already mapped by [`level_of`]). A
    /// window of this module that has ended is closed first; its summary is returned.
    pub fn admit(&mut self, prefix: &str, level: Level, now: Instant) -> (Verdict, Option<Summary>) {
        let summary = match self.windows.get(prefix) {
            Some(w) if now.duration_since(w.started) >= WINDOW => self.windows.remove(prefix).and_then(|w| summary_of(prefix, w)),
            _ => None,
        };
        let w = self.windows.entry(prefix.to_owned()).or_insert(Window { started: now, shown: 0, suppressed: 0, worst: Level::TRACE });
        let verdict = if w.shown < BURST {
            w.shown += 1;
            Verdict::Show(level)
        } else {
            w.suppressed += 1;
            // smaller = more severe (ERROR < WARN < … < TRACE)
            w.worst = w.worst.min(level);
            Verdict::Suppress
        };
        (verdict, summary)
    }

    /// Close every window that has ended (called from the controller's tick, so a stream that
    /// went quiet still gets its summary). Returns the summaries of windows with suppressed
    /// lines.
    pub fn expire(&mut self, now: Instant) -> Vec<Summary> {
        let ended: Vec<String> = self.windows.iter().filter(|(_, w)| now.duration_since(w.started) >= WINDOW).map(|(k, _)| k.clone()).collect();
        ended.into_iter().filter_map(|k| self.windows.remove(&k).and_then(|w| summary_of(&k, w))).collect()
    }
}

fn summary_of(prefix: &str, w: Window) -> Option<Summary> {
    (w.suppressed > 0).then(|| (prefix.to_owned(), w.suppressed, w.worst))
}

/// Log one forwarded line or summary (tracing needs the level at compile time).
pub fn emit(level: Level, prefix: &str, text: &str) {
    match level {
        Level::ERROR => tracing::error!(target: "tuner::mpv", "{prefix}: {text}"),
        Level::WARN => tracing::warn!(target: "tuner::mpv", "{prefix}: {text}"),
        Level::INFO => tracing::info!(target: "tuner::mpv", "{prefix}: {text}"),
        Level::DEBUG => tracing::debug!(target: "tuner::mpv", "{prefix}: {text}"),
        Level::TRACE => tracing::trace!(target: "tuner::mpv", "{prefix}: {text}"),
    }
}

/// Log a window summary.
pub fn emit_summary((prefix, n, level): &Summary) {
    emit(*level, prefix, &format!("{n} similar lines suppressed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dead DASH stream: hundreds of fragment errors become BURST lines + one summary per
    /// window, and other modules keep their own budget.
    #[test]
    fn a_flood_is_rate_limited_and_summarised() {
        let mut log = MpvLog::default();
        let t0 = Instant::now();
        let mut shown = 0;
        for i in 0..300u64 {
            let (v, summary) = log.admit("ffmpeg", Level::INFO, t0 + Duration::from_millis(i * 20));
            assert!(summary.is_none(), "the window (10 s) hasn't ended at {i}");
            if v != Verdict::Suppress {
                shown += 1;
            }
        }
        assert_eq!(shown, BURST);
        assert_eq!(log.admit("cplayer", Level::WARN, t0).0, Verdict::Show(Level::WARN), "per module");
        assert!(log.expire(t0 + Duration::from_secs(5)).is_empty());
        let summaries = log.expire(t0 + WINDOW);
        assert_eq!(summaries, [("ffmpeg".to_owned(), 300 - BURST, Level::INFO)]);
        // a new window shows lines again; a late line closes the old window itself
        assert_eq!(log.admit("ffmpeg", Level::DEBUG, t0 + WINDOW).0, Verdict::Show(Level::DEBUG));
        for _ in 0..BURST {
            log.admit("ffmpeg", Level::DEBUG, t0 + WINDOW);
        }
        let (v, summary) = log.admit("ffmpeg", Level::DEBUG, t0 + WINDOW * 2);
        assert_eq!(v, Verdict::Show(Level::DEBUG));
        assert_eq!(summary, Some(("ffmpeg".to_owned(), 1, Level::DEBUG)));
    }
}
