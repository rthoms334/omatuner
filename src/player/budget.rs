//! Per-source connection budget (critique gap 4), shared by every picture's controller.
//! Owner: WP-07, MV-1. DESIGN §11.5, §35 (multi-view §4.4).
//!
//! One [`SharedBudget`] serves every tile (picture A, and picture B once multi-view starts it):
//! * **limits**: `AccountInfo.max_connections` per playlist (unknown = unlimited), plus limits
//!   *observed* this session: a second picture refused with a 403 on a playlist with an unknown
//!   limit, not explained by the race window, teaches "one stream at a time" until restart;
//! * **claims**: the playlist each tile targets. Set on play / zap, KEPT through Reconnecting
//!   (a picture that is down keeps its playlist, so the other picture can't take it between two
//!   attempts), cleared on stop, end, final error, or a retarget;
//! * **leases**: streams opened and not yet closed (released on the stream's END_FILE, never
//!   before);
//! * **released**: when each playlist's last stream closed and what it was — the
//!   connection-limit race window (`state` module docs), shared so that picture B starting on
//!   a playlist picture A left a moment ago retries a 403 instead of failing.
//!
//! A tile may open a stream on a playlist iff the OTHER tiles that hold it (a claim or an open
//! lease) plus its own open leases stay under the limit. With the usual one-connection
//! accounts, two pictures therefore come from two playlists; nothing ever opens a stream
//! beyond a known or observed limit.
//!
//! Stop-before-start (within a tile): `loadfile … replace` RETURNS BEFORE the old file is
//! stopped (mpv 0.41 man page) and demuxer teardown is asynchronous. When the old and new
//! streams share a source whose limit is ≤ 1 or unknown, the controller sends `stop`, waits for
//! the old entry's END_FILE (max 1.5 s, `PendingLoad`), and only then loads; the old lease is
//! released on that END_FILE, never before the command (DESIGN §11.5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::model::{MediaRef, SourceId, TileId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub source: SourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.describe())]
pub struct BudgetExceeded {
    pub in_use: u32,
    pub limit: u32,
    /// The other picture holding the playlist (`None`: this picture's own streams).
    pub holder: Option<TileId>,
    /// The other pictures hold the playlist only with streams that are closing (their claims
    /// moved elsewhere): it frees up within moments.
    pub closing: bool,
}

impl BudgetExceeded {
    fn describe(&self) -> String {
        match self.holder {
            Some(t) => format!("This playlist allows {} at a time and picture {t} is using it", streams(self.limit)),
            None => format!("connection limit reached for this provider ({}/{})", self.in_use, self.limit),
        }
    }
}

fn streams(n: u32) -> String {
    if n == 1 { "one stream".into() } else { format!("{n} streams") }
}

/// A leased stream that closed: the connection-limit race window (`controller::race_close`).
#[derive(Debug, Clone, PartialEq)]
pub struct Released {
    pub at: Instant,
    pub media: MediaRef,
    /// It reached Playing: the provider counted it.
    pub played: bool,
    /// The picture whose stream it was.
    pub tile: TileId,
}

/// What the multi-view planner and MCP `status.streams` see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamUse {
    pub source: SourceId,
    /// `None` = unknown / unlimited. An observed limit counts.
    pub limit: Option<u32>,
    /// Pictures holding the playlist (a claim or an open stream).
    pub in_use: u32,
}

/// The budget every picture shares (module docs). Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct SharedBudget(Arc<Mutex<SharedState>>);

#[derive(Debug, Default)]
struct SharedState {
    limits: HashMap<SourceId, u32>,
    /// Limits learned this session (a refused second stream on an unknown limit).
    observed: HashMap<SourceId, u32>,
    open: HashMap<(TileId, SourceId), u32>,
    claims: HashMap<TileId, SourceId>,
    released: HashMap<SourceId, Released>,
}

impl SharedState {
    fn limit(&self, source: SourceId) -> Option<u32> {
        self.limits.get(&source).or_else(|| self.observed.get(&source)).copied()
    }

    /// No other tile claims `source` (whoever holds it only has a stream still closing).
    fn only_closing(&self, tile: TileId, source: SourceId) -> bool {
        !self.claims.iter().any(|(t, s)| *t != tile && *s == source)
    }

    /// The tiles other than `tile` that hold `source` (a claim or an open lease), lowest first.
    fn holders(&self, tile: TileId, source: SourceId) -> Vec<TileId> {
        let mut t: Vec<TileId> = self
            .claims
            .iter()
            .filter(|(t, s)| **t != tile && **s == source)
            .map(|(t, _)| *t)
            .chain(self.open.iter().filter(|((t, s), n)| *t != tile && *s == source && **n > 0).map(|((t, _), _)| *t))
            .collect();
        t.sort();
        t.dedup();
        t
    }

    fn own_open(&self, tile: TileId, source: SourceId) -> u32 {
        self.open.get(&(tile, source)).copied().unwrap_or(0)
    }
}

impl SharedBudget {
    fn state(&self) -> MutexGuard<'_, SharedState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `AccountInfo.max_connections` (None ⇒ unknown; 0 is treated as 1). A known limit
    /// replaces an observed one.
    pub fn set_limit(&self, source: SourceId, max: Option<u32>) {
        let mut s = self.state();
        match max {
            Some(m) => {
                s.limits.insert(source, m.max(1));
                s.observed.remove(&source);
            }
            None => {
                s.limits.remove(&source);
            }
        }
    }

    /// The playlist's limit: known, else observed this session (`None` = unknown).
    pub fn limit(&self, source: SourceId) -> Option<u32> {
        self.state().limit(source)
    }

    /// The limit was learned from a refused second stream (the error card: "This playlist
    /// seems to allow one stream at a time").
    pub fn learned(&self, source: SourceId) -> bool {
        let s = self.state();
        !s.limits.contains_key(&source) && s.observed.contains_key(&source)
    }

    /// The source `tile` targets (`None`: nothing). The controller keeps it in step with its
    /// state machine (`Controller::sync_claim`).
    pub fn claim(&self, tile: TileId, source: Option<SourceId>) {
        let mut s = self.state();
        match source {
            Some(src) => s.claims.insert(tile, src),
            None => s.claims.remove(&tile),
        };
    }

    /// What `tile` claims.
    pub fn claim_of(&self, tile: TileId) -> Option<SourceId> {
        self.state().claims.get(&tile).copied()
    }

    /// May `tile` play from `source` now: the other pictures holding it stay under its limit.
    pub fn admits(&self, tile: TileId, source: SourceId) -> bool {
        let s = self.state();
        s.limit(source).is_none_or(|limit| (s.holders(tile, source).len() as u32) < limit)
    }

    /// Another picture holds any playlist (single view: never — zaps skip the copy lookup).
    pub fn others_active(&self, tile: TileId) -> bool {
        let s = self.state();
        s.claims.keys().any(|t| *t != tile) || s.open.iter().any(|((t, _), n)| *t != tile && *n > 0)
    }

    /// The first other picture holding `source` (banner: "TV is in use by picture A").
    pub fn holder(&self, tile: TileId, source: SourceId) -> Option<TileId> {
        self.state().holders(tile, source).first().copied()
    }

    /// Open a stream of `tile` on `source`.
    pub fn acquire(&self, tile: TileId, source: SourceId) -> Result<Lease, BudgetExceeded> {
        let mut s = self.state();
        let holders = s.holders(tile, source);
        let in_use = holders.len() as u32 + s.own_open(tile, source);
        if let Some(limit) = s.limit(source)
            && in_use >= limit
        {
            let closing = !holders.is_empty() && s.only_closing(tile, source);
            return Err(BudgetExceeded { in_use, limit, holder: holders.first().copied(), closing });
        }
        *s.open.entry((tile, source)).or_insert(0) += 1;
        Ok(Lease { source })
    }

    /// The leased stream closed. `closed` = what it played and whether it reached Playing (the
    /// race window; `None` for a stream that never really started).
    pub fn release(&self, tile: TileId, lease: Lease, closed: Option<(MediaRef, bool)>) {
        let mut s = self.state();
        if let Some(n) = s.open.get_mut(&(tile, lease.source)) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.open.remove(&(tile, lease.source));
            }
        }
        if let Some((media, played)) = closed {
            s.released.insert(lease.source, Released { at: Instant::now(), media, played, tile });
        }
    }

    /// The last stream of `source` that closed (any picture's).
    pub fn released(&self, source: SourceId) -> Option<Released> {
        self.state().released.get(&source).cloned()
    }

    /// `tile` was refused with a 403 on `source` outside the race window. With an unknown
    /// limit while another picture holds the playlist, the playlist evidently allows one
    /// stream at a time: learned for the session. Returns true when this taught it.
    pub fn observe_refusal(&self, tile: TileId, source: SourceId) -> bool {
        let mut s = self.state();
        if s.limits.contains_key(&source) || s.observed.contains_key(&source) || s.holders(tile, source).is_empty() {
            return false;
        }
        s.observed.insert(source, 1);
        true
    }

    /// The tile's player shut down: its claim and any leases left go.
    pub fn forget(&self, tile: TileId) {
        let mut s = self.state();
        s.claims.remove(&tile);
        s.open.retain(|(t, _), _| *t != tile);
    }

    /// Per playlist with a limit, a claim or an open stream: its limit and how many pictures
    /// hold it.
    pub fn usage(&self) -> Vec<StreamUse> {
        let s = self.state();
        let mut sources: Vec<SourceId> =
            s.limits.keys().chain(s.observed.keys()).chain(s.claims.values()).chain(s.open.keys().map(|(_, src)| src)).copied().collect();
        sources.sort();
        sources.dedup();
        sources
            .into_iter()
            .map(|source| {
                let mut tiles: Vec<TileId> = s
                    .claims
                    .iter()
                    .filter(|(_, c)| **c == source)
                    .map(|(t, _)| *t)
                    .chain(s.open.iter().filter(|((_, src), n)| *src == source && **n > 0).map(|((t, _), _)| *t))
                    .collect();
                tiles.sort();
                tiles.dedup();
                StreamUse { source, limit: s.limit(source), in_use: tiles.len() as u32 }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV: SourceId = SourceId(1);
    const TV2: SourceId = SourceId(2);
    const A: TileId = TileId::A;
    const B: TileId = TileId::B;

    /// A picture that is reconnecting keeps its playlist: B can't take TV between A's
    /// attempts (A has a claim but no open stream), and can once A stops.
    #[test]
    fn a_reconnecting_picture_keeps_its_playlist() {
        let b = SharedBudget::default();
        b.set_limit(TV, Some(1));
        b.claim(A, Some(TV));
        let lease = b.acquire(A, TV).expect("A plays");
        // A's stream dropped: lease closed, claim kept (Reconnecting)
        b.release(A, lease, Some((MediaRef::Url("a".into()), true)));
        assert!(!b.admits(B, TV));
        let refused = b.acquire(B, TV).unwrap_err();
        assert_eq!((refused.holder, refused.closing), (Some(A), false));
        assert_eq!(refused.to_string(), "This playlist allows one stream at a time and picture A is using it");
        assert!(b.admits(B, TV2), "the other playlist is free");
        assert!(b.admits(A, TV), "A itself may reconnect");
        b.claim(A, None); // A stopped
        assert!(b.admits(B, TV));
        b.acquire(B, TV).expect("B plays");
        assert_eq!(b.usage(), vec![StreamUse { source: TV, limit: Some(1), in_use: 1 }]);
    }

    /// A retarget moves the claim: A leaving TV for TV 2 frees TV for B. An open lease counts
    /// like a claim (a stream not closed yet).
    #[test]
    fn claims_follow_retargets_and_open_streams_count() {
        let b = SharedBudget::default();
        b.set_limit(TV, Some(1));
        b.set_limit(TV2, Some(1));
        b.claim(A, Some(TV));
        assert!(!b.admits(B, TV));
        b.claim(A, Some(TV2));
        assert!(b.admits(B, TV) && !b.admits(B, TV2));
        // a stream of A on TV still open (its END_FILE not seen yet): TV is still busy, but
        // only until it closes
        let lease = b.acquire(A, TV).unwrap();
        assert!(!b.admits(B, TV));
        assert!(b.acquire(B, TV).unwrap_err().closing);
        b.release(A, lease, None);
        assert!(b.admits(B, TV));
        // limit 2: both pictures on one playlist
        b.set_limit(TV2, Some(2));
        b.claim(B, Some(TV2));
        b.acquire(B, TV2).expect("room for two");
        assert_eq!(b.usage().iter().find(|u| u.source == TV2).map(|u| u.in_use), Some(2));
    }

    /// An unknown limit lets both pictures share the playlist until a refusal teaches one
    /// stream at a time; a refusal with nobody else on the playlist teaches nothing, and a
    /// known limit replaces the learned one.
    #[test]
    fn an_unknown_limit_is_learned_from_a_refused_second_stream() {
        let b = SharedBudget::default();
        b.claim(A, Some(TV));
        b.acquire(A, TV).unwrap();
        assert!(b.admits(B, TV));
        assert!(!b.observe_refusal(A, TV), "nobody else on it: the playlist's own problem");
        assert!(b.observe_refusal(B, TV));
        assert!(!b.observe_refusal(B, TV), "once");
        assert!(b.learned(TV) && b.limit(TV) == Some(1));
        assert!(!b.admits(B, TV));
        assert_eq!(b.acquire(B, TV).unwrap_err().holder, Some(A));
        b.set_limit(TV, Some(2));
        assert!(!b.learned(TV) && b.admits(B, TV));
    }

    /// The race window is shared: B starting on a playlist A left a moment ago sees A's close
    /// (`controller::race_close` decides what it means).
    #[test]
    fn closes_are_shared_between_pictures() {
        let b = SharedBudget::default();
        let lease = b.acquire(A, TV).unwrap();
        b.release(A, lease, Some((MediaRef::Url("a".into()), false)));
        let r = b.released(TV).expect("recorded");
        assert_eq!((r.tile, r.media, r.played), (A, MediaRef::Url("a".into()), false));
        assert!(b.released(TV2).is_none());
    }

    #[test]
    fn a_shut_down_tile_holds_nothing() {
        let b = SharedBudget::default();
        b.set_limit(TV, Some(1));
        b.claim(B, Some(TV));
        let _ = b.acquire(B, TV).unwrap();
        b.forget(B);
        assert!(b.admits(A, TV) && b.claim_of(B).is_none());
        b.acquire(A, TV).expect("free");
    }
}
