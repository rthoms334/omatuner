//! Position (DESIGN §40 §7.3 "Position"): a `playbackInfo` request (PR 2846; reply keys from
//! LIVI, facts only), else an estimate for the rest of the session (advances while `playing`,
//! freezes on `paused`/`loading`, bounded by the media the TV fetched). Never `GET /playback-info`
//! (500 on 26.x and 27.0). OWNER: CAST-2.
//!
//! No reference reads a position from `playbackState`, so that is not relied on.
//! 1. **`playbackInfo`.** `/command {type: playbackInfo, kind: request, messageID: n}`, answered
//!    on the event channel by `{kind: response, messageID: n, value: {position, duration, rate,
//!    …}}`. The client asks every [`POLL_INTERVAL`] while VOD or catch-up plays, and once before
//!    Play here or Stop (the session asks through `Client::position`).
//! 2. **The estimate.** When the first request gets no answer within [`ANSWER_TIMEOUT`], or a
//!    non-2xx, the session estimates for the rest of its life: from 0 of the current playlist,
//!    advancing while `playing`, frozen on `paused` and `loading`, never past the end of the last
//!    segment the TV fetched (VioletRelay does this for receivers without metadata). Accurate to
//!    about a segment: enough for the 10 s progress saves and for Play here.
//!
//! Positions here are relative to the current playlist; the cast session adds the item's offset
//! (the `-ss` start or the playlist view's first segment).

use std::time::Duration;

use tokio::time::Instant;

use super::bplist::Value;
use super::queue;

/// How often a position is reported while VOD or catch-up plays (DESIGN §40 §7.3).
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// How long a `playbackInfo` request may take before the estimate takes over.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Where positions come from in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Nothing asked yet.
    Untried,
    /// The TV answers `playbackInfo`.
    Answers,
    /// The first request went unanswered: the estimate, for the rest of the session.
    Estimate,
}

/// A `playbackInfo` answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Answer {
    pub position: f64,
    /// `None` when the TV gives none, or one that isn't a positive finite number (live).
    pub duration: Option<f64>,
}

/// Read the response's `value` (LIVI's reply keys: `position`, `duration`, `rate`, …). `None`
/// without a usable position.
pub fn answer(value: &Value) -> Option<Answer> {
    let position = queue::number(value.get("position")).filter(|p| p.is_finite() && *p >= 0.0)?;
    let duration = queue::number(value.get("duration")).filter(|d| d.is_finite() && *d > 0.0);
    Some(Answer { position, duration })
}

/// The estimate of the current playlist's position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Estimate {
    /// Where it was when it last froze (or was anchored).
    base: f64,
    /// Running since (while `playing`).
    since: Option<Instant>,
    /// The end of the media the TV fetched (`None`: nothing reported yet).
    bound: Option<f64>,
}

impl Estimate {
    /// A new playlist: 0, frozen, unbounded.
    pub fn restart(&mut self) {
        *self = Self::default();
    }

    /// `playing`.
    pub fn run(&mut self, now: Instant) {
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    /// `paused` / `loading` (and our own `setRate 0`).
    pub fn freeze(&mut self, now: Instant) {
        self.base = self.at(now);
        self.since = None;
    }

    /// An answered position: the estimate continues from it.
    pub fn anchor(&mut self, position: f64, now: Instant) {
        self.base = position;
        if self.since.is_some() {
            self.since = Some(now);
        }
    }

    /// The TV fetched media up to `upto_s` (only ever raises the bound).
    pub fn bound(&mut self, upto_s: f64) {
        if upto_s.is_finite() {
            self.bound = Some(self.bound.map_or(upto_s, |b| b.max(upto_s)));
        }
    }

    pub fn at(&self, now: Instant) -> f64 {
        let ran = self.since.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        let p = self.base + ran;
        self.bound.map_or(p, |b| p.min(b)).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn the_estimate_runs_while_playing_and_freezes_on_pause_and_loading() {
        let t0 = Instant::now();
        let mut e = Estimate::default();
        assert_eq!(e.at(t0 + secs(5)), 0.0, "loading: frozen at 0");
        e.run(t0 + secs(2));
        assert_eq!(e.at(t0 + secs(12)), 10.0);
        e.run(t0 + secs(8)); // a repeated `playing` doesn't restart the clock
        assert_eq!(e.at(t0 + secs(12)), 10.0);
        e.freeze(t0 + secs(12)); // paused
        assert_eq!(e.at(t0 + secs(60)), 10.0);
        e.run(t0 + secs(60));
        e.freeze(t0 + secs(65)); // loading (a stall)
        assert_eq!(e.at(t0 + secs(90)), 15.0);
        e.run(t0 + secs(90));
        e.anchor(40.0, t0 + secs(100)); // an answer moves it
        assert_eq!(e.at(t0 + secs(104)), 44.0);
        e.restart();
        assert_eq!(e.at(t0 + secs(200)), 0.0, "a new playlist starts at 0, frozen");
    }

    #[test]
    fn the_estimate_never_passes_what_the_tv_fetched() {
        let t0 = Instant::now();
        let mut e = Estimate::default();
        e.bound(8.0);
        e.run(t0);
        assert_eq!(e.at(t0 + secs(6)), 6.0);
        assert_eq!(e.at(t0 + secs(30)), 8.0, "stuck at the last fetched segment's end");
        e.bound(4.0); // an older report never lowers it
        e.bound(24.0);
        assert_eq!(e.at(t0 + secs(30)), 24.0);
        e.freeze(t0 + secs(30));
        e.bound(100.0);
        assert_eq!(e.at(t0 + secs(90)), 24.0, "frozen where the bound held it");
    }

    #[test]
    fn answers_need_a_position() {
        let v = queue::dict([("duration", Value::Real(5400.0)), ("position", Value::Real(4200.5)), ("rate", Value::Real(1.0))]);
        assert_eq!(answer(&v), Some(Answer { position: 4200.5, duration: Some(5400.0) }));
        let live = queue::dict([("position", Value::Int(12)), ("duration", Value::Real(f64::INFINITY))]);
        assert_eq!(answer(&live), Some(Answer { position: 12.0, duration: None }));
        assert_eq!(answer(&queue::dict([("rate", Value::Real(1.0))])), None);
        assert_eq!(answer(&queue::dict([("position", Value::Real(f64::NAN))])), None);
    }
}
