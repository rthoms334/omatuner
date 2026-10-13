//! Position (DESIGN §40 §7.3 "Position"): a `playbackInfo` request (PR 2846; reply keys from
//! LIVI, facts only), else an estimate for the rest of the session (advances while `playing`,
//! freezes on `paused`/`loading`, bounded by the media the TV fetched). Never `GET /playback-info`
//! (500 on 26.x and 27.0). OWNER: CAST-2.
//!
//! No reference reads a position from `playbackState`, so that is not relied on.
//! 1. **`playbackInfo`.** `/command {type: playbackInfo, kind: request, messageID: n}`, answered
//!    on the event channel by `{kind: response, messageID: n, info: {position, duration, rate,
//!    …}}`. The client asks every [`POLL_INTERVAL`] while VOD or catch-up plays, and once before
//!    Play here, Stop and every reload of the item (the session asks through `Client::position`).
//!    tvOS 27.0 (AppleTV14,1, 24J361, traced 2026-10-13) answers under `info` with CMTime dicts
//!    (`{value, timescale, flags, epoch}`; a live or still-growing playlist's duration has the
//!    indefinite flag); LIVI's form is `value` with plain numbers. Both are read ([`seconds`]).
//! 2. **The estimate.** When the first request gets no answer within [`ANSWER_TIMEOUT`], or a
//!    non-2xx, the session estimates for the rest of its life: from where the TV said its playhead
//!    jumped (`timeJumped`, sent at every start and seek), else 0 of the current playlist,
//!    advancing while `playing`, frozen on `paused` and `loading`, never past the end of the last
//!    segment the TV fetched (VioletRelay does this for receivers without metadata). Accurate to
//!    about a segment: enough for the 10 s progress saves, a reload and Play here.
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
/// An estimated position is reported this much behind the estimate: it becomes the saved watch
/// progress and the start of a reload, which must never be ahead of what the viewer saw.
pub const ESTIMATE_BEHIND_S: f64 = 2.0;
/// A fetch that starts this far from where the previous one ended doesn't continue it (EXTINF
/// rounding is milliseconds).
const CONTIGUOUS_S: f64 = 0.25;

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

/// Read the response's playback info (`position`, `duration`, `rate`, …: tvOS 27's `info` or
/// LIVI's `value`). `None` without a usable position.
pub fn answer(value: &Value) -> Option<Answer> {
    let position = seconds(value.get("position")).filter(|p| *p >= 0.0)?;
    let duration = seconds(value.get("duration")).filter(|d| *d > 0.0);
    Some(Answer { position, duration })
}

/// `kCMTimeFlags_Valid`; the infinite and indefinite flags (`PositiveInfinity` 4,
/// `NegativeInfinity` 8, `Indefinite` 16) make a time unusable.
const CMTIME_VALID: i64 = 1;
const CMTIME_NOT_A_NUMBER: i64 = 4 | 8 | 16;

/// A time in seconds: a plain number (LIVI), or a CMTime dict `{value, timescale, flags, epoch}`
/// (tvOS 27: `position {value 1096110, timescale 90000, flags 1}` is 12.179 s; a growing
/// playlist's `duration` comes with `flags 17`, valid + indefinite, which is no duration).
pub fn seconds(v: Option<&Value>) -> Option<f64> {
    let s = match v? {
        Value::Dict(_) => {
            let v = v?;
            let flags = queue::int(v.get("flags"))?;
            let scale = queue::int(v.get("timescale")).filter(|t| *t > 0)?;
            if flags & CMTIME_VALID == 0 || flags & CMTIME_NOT_A_NUMBER != 0 {
                return None;
            }
            queue::number(v.get("value"))? / scale as f64
        }
        other => queue::number(Some(other))?,
    };
    s.is_finite().then_some(s)
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

    /// An answered position, or one `timeJumped` gave: the estimate continues from it. A bound it
    /// passes is from before a jump forward, and goes.
    pub fn anchor(&mut self, position: f64, now: Instant) {
        self.base = position;
        if self.since.is_some() {
            self.since = Some(now);
        }
        if self.bound.is_some_and(|b| b < position) {
            self.bound = None;
        }
    }

    /// The TV jumped and its first fetch there is `from..upto`: the playhead is in that segment,
    /// so the estimate continues from its start (behind, never ahead) and is bounded by its end.
    pub fn rebase(&mut self, from: f64, upto: f64, now: Instant) {
        self.anchor(from, now);
        self.rebound(upto);
    }

    /// The TV fetched media up to `upto_s` (only ever raises the bound).
    pub fn bound(&mut self, upto_s: f64) {
        if upto_s.is_finite() {
            self.bound = Some(self.bound.map_or(upto_s, |b| b.max(upto_s)));
        }
    }

    /// A new run of fetches after a jump: its end is the bound, whatever the old run reached.
    pub fn rebound(&mut self, upto_s: f64) {
        self.bound = upto_s.is_finite().then_some(upto_s);
    }

    pub fn at(&self, now: Instant) -> f64 {
        let ran = self.since.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        let p = self.base + ran;
        self.bound.map_or(p, |b| p.min(b)).max(0.0)
    }

    /// What is reported (the progress saved, a reload's start): [`ESTIMATE_BEHIND_S`] behind.
    pub fn reported(&self, now: Instant) -> f64 {
        (self.at(now) - ESTIMATE_BEHIND_S).max(0.0)
    }
}

/// What one segment fetch says about the playhead ([`Fetches::saw`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// The next segment of the run: the playhead is still behind its end (the bound).
    Next,
    /// The run broke (a seek, also one made with the TV's remote, or the first fetch after a
    /// `timeJumped` that gave no position): the playhead is in this segment.
    Jump,
    /// The first fetch after a jump the TV placed (`timeJumped` with a position): the start of a
    /// new run, whose end is the bound from now on.
    NewRun,
    /// The same segment again (AVPlayer's 2-byte probe, a retry): nothing new.
    Again,
}

/// The TV's segment fetches as evidence of where it plays, for when `playbackInfo` goes
/// unanswered: AVPlayer fetches the segments ahead of the playhead in order, so a fetch that
/// doesn't start where the last one ended is the playhead moving there. A jump the TV placed
/// itself (`timeJumped` with a position) starts a new run without moving the estimate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fetches {
    /// The last segment fetched, `(from, upto)` in the current playlist.
    last: Option<(f64, f64)>,
    after: After,
}

/// What the next fetch follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum After {
    /// The run so far (the first fetch of an item starts one).
    #[default]
    Run,
    /// A `timeJumped` without a position: the next fetch is where the TV went.
    Unplaced,
    /// A `timeJumped` with one: the next fetch starts a new run.
    Placed,
}

impl Fetches {
    /// `timeJumped` without a position.
    pub fn jumped(&mut self) {
        self.after = After::Unplaced;
    }

    /// `timeJumped` with one (the estimate is anchored there already).
    pub fn placed(&mut self) {
        self.after = After::Placed;
    }

    /// The TV fetched `from..upto`.
    pub fn saw(&mut self, from: f64, upto: f64) -> Fetch {
        let last = self.last.replace((from, upto));
        if last.is_some_and(|(f, _)| (f - from).abs() < CONTIGUOUS_S) {
            return Fetch::Again;
        }
        match std::mem::take(&mut self.after) {
            After::Unplaced => Fetch::Jump,
            After::Placed => Fetch::NewRun,
            After::Run => match last {
                Some((_, end)) if (from - end).abs() > CONTIGUOUS_S => Fetch::Jump,
                _ => Fetch::Next,
            },
        }
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

    /// A seek made with the TV's remote while `playbackInfo` goes unanswered: the run of fetches
    /// breaks, and the estimate continues from the start of the first segment fetched there
    /// (behind the playhead, never ahead), bounded by its end; a `timeJumped` that says where
    /// places it exactly, and its first fetch only resets the bound.
    #[test]
    fn a_jump_moves_the_estimate_to_where_the_tv_fetches() {
        let t0 = Instant::now();
        let mut e = Estimate::default();
        let mut f = Fetches::default();
        e.run(t0);
        for (from, upto) in [(0.0, 4.0), (4.0, 8.0), (8.0, 12.0)] {
            assert_eq!(f.saw(from, upto), Fetch::Next);
            e.bound(upto);
        }
        assert_eq!(f.saw(8.0, 12.0), Fetch::Again, "the same segment again (a range probe)");
        assert_eq!(e.at(t0 + secs(3)), 3.0);
        // scrubbed to 10 min: the first fetch there is segment 600..604
        assert_eq!(f.saw(600.0, 604.0), Fetch::Jump);
        e.rebase(600.0, 604.0, t0 + secs(5));
        assert_eq!(e.at(t0 + secs(7)), 602.0);
        assert_eq!(e.reported(t0 + secs(7)), 600.0, "reported 2 s behind");
        assert_eq!(e.at(t0 + secs(30)), 604.0, "bounded by what it fetched there");
        assert_eq!(f.saw(604.0, 608.0), Fetch::Next);
        // back to 1 min, announced without a position: the next fetch places it
        f.jumped();
        assert_eq!(f.saw(60.0, 64.0), Fetch::Jump);
        e.rebase(60.0, 64.0, t0 + secs(40));
        assert_eq!(e.at(t0 + secs(41)), 61.0, "the old bound of 608 s is gone");
        // a placed jump: exact, then a new run whose end is the bound
        e.anchor(300.0, t0 + secs(50));
        f.placed();
        assert_eq!(f.saw(296.0, 304.0), Fetch::NewRun);
        e.rebound(304.0);
        assert_eq!(e.at(t0 + secs(52)), 302.0);
        assert_eq!(e.at(t0 + secs(90)), 304.0);
    }

    fn cmtime(value: i64, timescale: i64, flags: i64) -> Value {
        queue::dict([("epoch", Value::Int(0)), ("flags", Value::Int(flags)), ("timescale", Value::Int(timescale)), ("value", Value::Int(value))])
    }

    /// The answer an AppleTV14,1 on tvOS 27.0 (24J361) gave on 2026-10-13, as traced: under
    /// `info`, CMTime positions, the growing playlist's duration indefinite. Read as "no answer",
    /// it had put every session on the estimate, which ran 12 s behind the TV.
    #[test]
    fn tvos_27_answers_with_cmtimes() {
        let info = queue::dict([
            ("duration", cmtime(0, 0, 17)),
            ("position", cmtime(12_179_000_000, 1_000_000_000, 1)),
            ("rate", Value::Real(1.0)),
            ("playbackState", Value::String("playing".into())),
        ]);
        assert_eq!(answer(&info), Some(Answer { position: 12.179, duration: None }));
        let vod = queue::dict([("position", cmtime(829_620, 90_000, 1)), ("duration", cmtime(5_400_000, 1000, 1))]);
        assert_eq!(answer(&vod), Some(Answer { position: 9.218, duration: Some(5400.0) }));
        assert_eq!(seconds(Some(&cmtime(6661, 1000, 3))), Some(6.661), "valid + rounded");
        assert_eq!(seconds(Some(&cmtime(1, 1, 0))), None, "not valid");
        assert_eq!(seconds(Some(&cmtime(1, 1, 5))), None, "+infinity");
        assert_eq!(seconds(Some(&cmtime(1, 0, 1))), None, "no timescale");
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
