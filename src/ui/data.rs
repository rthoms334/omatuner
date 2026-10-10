//! UI-thread data access primitives. IMPLEMENTED IN THE SKELETON and frozen (every UI WP uses
//! them every frame). DESIGN §5.3, §13.4.
//!
//! * [`FrameBudget`]: hard cap on synchronous SQLite time per frame on the UI thread (2 ms).
//!   The UI thread is mpv's render thread (25–60 Hz while video shows), so DB time directly
//!   costs video frames. Work that doesn't fit is deferred to the next frame (callers paint a
//!   skeleton and request a repaint).
//! * [`EpgReader`]: the seam between UI code and EPG queries. Production uses [`DbEpg`]
//!   (`store::epg::{window, now_next}` on the UI connection); tests use fakes, so the guide
//!   grid and rows are testable without WP-02.
//! * [`NowNextCache`]: `(epg channel, tvg-shift) → NowNext` valid until the current programme
//!   ends; at most 8 misses per frame; invalidated on `DataScope::Epg`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::model::{EpgChannelId, NowNext, ProgrammeSlim, Timestamp};

/// Per-frame time budget for synchronous UI-thread DB work.
#[derive(Debug)]
pub struct FrameBudget {
    limit: Duration,
    spent: Duration,
    deferred: bool,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new(Self::UI_DB)
    }
}

impl FrameBudget {
    /// The UI-thread DB budget per frame.
    pub const UI_DB: Duration = Duration::from_millis(2);

    pub fn new(limit: Duration) -> Self {
        Self { limit, spent: Duration::ZERO, deferred: false }
    }

    /// Reset (app, start of every frame).
    pub fn begin_frame(&mut self) {
        self.spent = Duration::ZERO;
        self.deferred = false;
    }

    pub fn exhausted(&self) -> bool {
        self.spent >= self.limit
    }

    /// Run `f` if budget remains (timing it); `None` = deferred to a later frame.
    pub fn run<R>(&mut self, f: impl FnOnce() -> R) -> Option<R> {
        if self.exhausted() {
            self.deferred = true;
            return None;
        }
        let t = Instant::now();
        let r = f();
        self.spent += t.elapsed();
        Some(r)
    }

    /// Something was deferred this frame: the app requests a repaint.
    pub fn deferred(&self) -> bool {
        self.deferred
    }
    pub fn spent(&self) -> Duration {
        self.spent
    }
}

/// EPG reads used by UI code. Times are EPG time except where noted (see `store::epg`).
pub trait EpgReader {
    /// Programmes of `ch` overlapping `[from, to)` (EPG time, unshifted).
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>>;
    /// Now/next per (channel, tvg-shift minutes), ALREADY shifted.
    fn now_next(&mut self, keys: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>>;
}

/// Production reader over the UI connection.
pub struct DbEpg<'a>(pub &'a rusqlite::Connection);

impl EpgReader for DbEpg<'_> {
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>> {
        crate::store::epg::window(self.0, ch, from, to)
    }
    fn now_next(&mut self, keys: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
        crate::store::epg::now_next(self.0, keys, now)
    }
}

/// Now/next per (epg channel, tvg-shift), valid until the current programme ends.
#[derive(Debug, Default)]
pub struct NowNextCache {
    map: HashMap<(EpgChannelId, i32), (NowNext, Timestamp)>,
    misses: u32,
    pending: bool,
}

impl NowNextCache {
    pub const MAX_MISSES_PER_FRAME: u32 = 8;
    /// Re-check channels without a current programme this often (seconds).
    pub const EMPTY_TTL: i64 = 300;

    /// Reset the per-frame miss counter (app, start of every frame).
    pub fn begin_frame(&mut self) {
        self.misses = 0;
        self.pending = false;
    }

    /// Cached value, loading it if allowed (≤ 8 misses per frame, within `budget`). A stale
    /// value is returned while a reload is deferred; `None` = not loaded yet (draw a skeleton
    /// and request a repaint — see [`NowNextCache::pending`]).
    pub fn get(&mut self, key: (EpgChannelId, i32), now: Timestamp, reader: &mut dyn EpgReader, budget: &mut FrameBudget) -> Option<&NowNext> {
        let fresh = self.map.get(&key).is_some_and(|(_, until)| now < *until);
        if !fresh {
            if self.misses < Self::MAX_MISSES_PER_FRAME {
                self.misses += 1;
                match budget.run(|| reader.now_next(&[key], now)) {
                    Some(Ok(mut m)) => {
                        let nn = m.remove(&key).unwrap_or_default();
                        let until = nn
                            .now
                            .as_ref()
                            .map(|p| p.stop)
                            .or(nn.next.as_ref().map(|p| p.start))
                            .unwrap_or(now + Self::EMPTY_TTL)
                            .min(now + Self::EMPTY_TTL * 12)
                            .max(now + 1);
                        self.map.insert(key, (nn, until));
                    }
                    Some(Err(e)) => {
                        tracing::debug!("now/next: {e}");
                        self.map.insert(key, (NowNext::default(), now + Self::EMPTY_TTL));
                    }
                    None => self.pending = true,
                }
            } else {
                self.pending = true;
            }
        }
        self.map.get(&key).map(|(nn, _)| nn)
    }

    /// Some lookups were deferred this frame (request a repaint).
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Drop everything (`DataScope::Epg`).
    pub fn invalidate(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgrammeFlags, ProgrammeId};
    use std::sync::Arc;

    struct Fake(u32);
    impl EpgReader for Fake {
        fn window(&mut self, _: EpgChannelId, _: Timestamp, _: Timestamp) -> Result<Vec<ProgrammeSlim>> {
            Ok(vec![])
        }
        fn now_next(&mut self, keys: &[(EpgChannelId, i32)], _now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
            self.0 += 1;
            let p = ProgrammeSlim {
                id: ProgrammeId(1),
                start: 0,
                stop: 100,
                title: Arc::from("News"),
                sub_title: None,
                flags: ProgrammeFlags::default(),
            };
            Ok(keys.iter().map(|k| (*k, NowNext { now: Some(p.shifted(k.1)), next: None })).collect())
        }
    }

    #[test]
    fn caches_until_programme_end_and_limits_misses() {
        let mut c = NowNextCache::default();
        let mut r = Fake(0);
        let mut b = FrameBudget::new(Duration::from_secs(1));
        assert!(c.get((EpgChannelId(1), 0), 10, &mut r, &mut b).is_some());
        assert!(c.get((EpgChannelId(1), 0), 50, &mut r, &mut b).is_some());
        assert_eq!(r.0, 1, "cached until stop");
        for i in 2..20 {
            c.get((EpgChannelId(i), 0), 10, &mut r, &mut b);
        }
        assert_eq!(r.0, NowNextCache::MAX_MISSES_PER_FRAME);
        assert!(c.pending());
        // shift is part of the key and applied by the reader
        c.begin_frame();
        let nn = c.get((EpgChannelId(1), 60), 10, &mut r, &mut b).unwrap();
        assert_eq!(nn.now.as_ref().unwrap().start, 3600);
    }
}
