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
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

use crate::model::{DownloadId, MediaRef, SourceId, TileId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub source: SourceId,
}

/// Pictures and background jobs share the same account limit (DESIGN §43 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Holder {
    Picture(TileId),
    Download(DownloadId),
}
impl From<TileId> for Holder {
    fn from(value: TileId) -> Self {
        Self::Picture(value)
    }
}
impl From<DownloadId> for Holder {
    fn from(value: DownloadId) -> Self {
        Self::Download(value)
    }
}
impl Holder {
    fn picture(self) -> Option<TileId> {
        if let Self::Picture(tile) = self { Some(tile) } else { None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetExceeded {
    #[error("This playlist is being used by download #{id}")]
    Download { id: DownloadId },
    #[error("{}", Self::describe(*in_use, *limit, *holder))]
    Limit { in_use: u32, limit: u32, holder: Option<TileId>, closing: bool },
}
impl BudgetExceeded {
    pub fn closing(&self) -> bool {
        matches!(self, Self::Limit { closing: true, .. })
    }
    pub fn holder(&self) -> Option<TileId> {
        if let Self::Limit { holder, .. } = self { *holder } else { None }
    }
    fn describe(in_use: u32, limit: u32, holder: Option<TileId>) -> String {
        match holder {
            Some(t) => format!("This playlist allows {} at a time and picture {t} is using it", streams(limit)),
            None => format!("connection limit reached for this provider ({in_use}/{limit})"),
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
    open: HashMap<(Holder, SourceId), u32>,
    claims: HashMap<Holder, SourceId>,
    released: HashMap<SourceId, Released>,
    shared: HashMap<(Holder, SourceId), u64>,
    next_shared: u64,
    raw: HashMap<MediaRef, (Holder, SourceId, Weak<crate::downloads::reader::Reader>)>,
    live: HashMap<MediaRef, (Holder, SourceId, Weak<super::hls::Buffer>)>,
}

impl SharedState {
    fn limit(&self, source: SourceId) -> Option<u32> {
        self.limits.get(&source).or_else(|| self.observed.get(&source)).copied()
    }

    /// No other tile claims `source` (whoever holds it only has a stream still closing).
    fn only_closing(&self, tile: Holder, source: SourceId) -> bool {
        !self.claims.iter().any(|(t, s)| *t != tile && *s == source)
    }

    /// The tiles other than `tile` that hold `source` (a claim or an open lease), lowest first.
    fn holders(&self, tile: Holder, source: SourceId) -> Vec<Holder> {
        let mut t: Vec<Holder> = self
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

    fn count(&self, holders: &[Holder], source: SourceId) -> u32 {
        let mut groups = std::collections::HashSet::new();
        let mut count = 0;
        for holder in holders {
            if let Some(group) = self.shared.get(&(*holder, source)) {
                if groups.insert(*group) {
                    count += 1;
                }
            } else {
                count += 1;
            }
        }
        count
    }

    fn own_open(&self, tile: Holder, source: SourceId) -> u32 {
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
    pub fn claim(&self, tile: impl Into<Holder>, source: Option<SourceId>) {
        let tile = tile.into();
        let mut s = self.state();
        match source {
            Some(src) => s.claims.insert(tile, src),
            None => s.claims.remove(&tile),
        };
    }

    /// What `tile` claims.
    pub fn claim_of(&self, tile: impl Into<Holder>) -> Option<SourceId> {
        let tile = tile.into();
        self.state().claims.get(&tile).copied()
    }

    /// May `tile` play from `source` now: the other pictures holding it stay under its limit.
    pub fn admits(&self, tile: impl Into<Holder>, source: SourceId) -> bool {
        let tile = tile.into();
        let s = self.state();
        s.limit(source).is_none_or(|limit| s.count(&s.holders(tile, source), source) < limit)
    }

    /// Another picture holds any playlist (single view: never — zaps skip the copy lookup).
    pub fn others_active(&self, tile: impl Into<Holder>) -> bool {
        let tile = tile.into();
        let s = self.state();
        s.claims.keys().any(|t| *t != tile) || s.open.iter().any(|((t, _), n)| *t != tile && *n > 0)
    }

    /// Another picture targets a playlist (multi-view is on): its claim. A stream it is still
    /// closing doesn't count — it stopped, single view again — the leases wait for that stream
    /// (casting's checks, DESIGN §40 §0.6, §3.9).
    pub fn others_claim(&self, tile: impl Into<Holder>) -> bool {
        let tile = tile.into();
        self.state().claims.keys().any(|t| *t != tile)
    }

    /// The first other picture holding `source` (banner: "TV is in use by picture A").
    pub fn holder(&self, tile: TileId, source: SourceId) -> Option<TileId> {
        self.state().holders(tile.into(), source).into_iter().find_map(Holder::picture)
    }

    /// An active download holding this account, for the Ask prompt and MCP busy result.
    pub fn download_holder(&self, source: SourceId) -> Option<DownloadId> {
        self.state().open.iter().find_map(|((h, s), n)| {
            if *s == source
                && *n > 0
                && let Holder::Download(id) = h
            {
                Some(*id)
            } else {
                None
            }
        })
    }

    /// The recorder stays alive while either its picture or its recording owns it. A weak
    /// registry never keeps a socket open on its own (DESIGN §43 §3, §5).
    pub fn register_live(&self, media: MediaRef, owner: impl Into<Holder>, source: SourceId, buffer: &Arc<super::hls::Buffer>) {
        self.state().live.insert(media, (owner.into(), source, Arc::downgrade(buffer)));
    }

    pub fn has_reader(&self, media: &MediaRef) -> bool {
        let s = self.state();
        s.live.get(media).is_some_and(|(_, _, b)| b.strong_count() > 0)
            || s.raw.get(media).is_some_and(|(_, _, r)| r.upgrade().is_some_and(|r| r.active()))
    }
    pub fn register_raw(&self, media: MediaRef, owner: impl Into<Holder>, source: SourceId, reader: &Arc<crate::downloads::reader::Reader>) {
        self.state().raw.insert(media, (owner.into(), source, Arc::downgrade(reader)));
    }
    pub fn share_raw(&self, media: &MediaRef, owner: impl Into<Holder>) -> Option<(Arc<crate::downloads::reader::Reader>, Lease)> {
        let mut s = self.state();
        let (from, source, weak) = s.raw.get(media)?.clone();
        let reader = weak.upgrade()?;
        if !reader.active() || s.own_open(from, source) == 0 {
            return None;
        }
        let group = if let Some(group) = s.shared.get(&(from, source)) {
            *group
        } else {
            s.next_shared += 1;
            let group = s.next_shared;
            s.shared.insert((from, source), group);
            group
        };
        let owner = owner.into();
        s.shared.insert((owner, source), group);
        *s.open.entry((owner, source)).or_default() += 1;
        Some((reader, Lease { source }))
    }

    pub fn has_live(&self, media: &MediaRef) -> bool {
        self.state().live.get(media).is_some_and(|(_, _, b)| b.upgrade().is_some_and(|b| !b.is_recording()))
    }

    pub fn share_live(&self, media: &MediaRef, owner: impl Into<Holder>) -> Option<(Arc<super::hls::Buffer>, Lease)> {
        let owner = owner.into();
        let mut s = self.state();
        let (mut from, source, weak) = s.live.get(media)?.clone();
        let buffer = weak.upgrade()?;
        if s.own_open(from, source) == 0 {
            // The original picture may have left while the recording kept the reader.
            let group = buffer.budget_group();
            from = s.shared.iter().find(|((_, src), g)| *src == source && Some(**g) == group)?.0.0;
        }
        let group = if let Some(group) = s.shared.get(&(from, source)) {
            *group
        } else {
            s.next_shared += 1;
            let group = s.next_shared;
            s.shared.insert((from, source), group);
            buffer.set_budget_group(group);
            group
        };
        s.shared.insert((owner, source), group);
        *s.open.entry((owner, source)).or_default() += 1;
        Some((buffer, Lease { source }))
    }

    /// Adoption changes the owner without closing or opening a provider connection.
    pub fn transfer(&self, from: impl Into<Holder>, to: impl Into<Holder>, lease: &Lease) {
        let mut s = self.state();
        let (from, to) = (from.into(), to.into());
        if let Some(n) = s.open.get_mut(&(from, lease.source))
            && *n > 0
        {
            *n -= 1;
            if *n == 0 {
                s.open.remove(&(from, lease.source));
            }
            *s.open.entry((to, lease.source)).or_insert(0) += 1;
        }
    }

    /// Open a stream of `tile` on `source`.
    pub fn acquire(&self, tile: impl Into<Holder>, source: SourceId) -> Result<Lease, BudgetExceeded> {
        let tile = tile.into();
        let mut s = self.state();
        let holders = s.holders(tile, source);
        let in_use = s.count(&holders, source) + s.own_open(tile, source);
        if let Some(limit) = s.limit(source)
            && in_use >= limit
        {
            let closing = !holders.is_empty() && s.only_closing(tile, source);
            if let Some(id) = holders.iter().find_map(|h| if let Holder::Download(id) = h { Some(*id) } else { None }) {
                return Err(BudgetExceeded::Download { id });
            }
            return Err(BudgetExceeded::Limit { in_use, limit, holder: holders.iter().find_map(|h| h.picture()), closing });
        }
        *s.open.entry((tile, source)).or_insert(0) += 1;
        Ok(Lease { source })
    }

    /// The leased stream closed. `closed` = what it played and whether it reached Playing (the
    /// race window; `None` for a stream that never really started).
    pub fn release(&self, tile: impl Into<Holder>, lease: Lease, closed: Option<(MediaRef, bool)>) {
        let tile = tile.into();
        let mut s = self.state();
        if let Some(n) = s.open.get_mut(&(tile, lease.source)) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.open.remove(&(tile, lease.source));
                s.shared.remove(&(tile, lease.source));
            }
        }
        if let Some((media, played)) = closed
            && let Holder::Picture(tile) = tile
        {
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
    pub fn observe_refusal(&self, tile: impl Into<Holder>, source: SourceId) -> bool {
        let tile = tile.into();
        let mut s = self.state();
        if s.limits.contains_key(&source) || s.observed.contains_key(&source) || s.holders(tile, source).is_empty() {
            return false;
        }
        s.observed.insert(source, 1);
        true
    }

    /// The tile's player shut down: its claim and any leases left go.
    pub fn forget(&self, tile: impl Into<Holder>) {
        let tile = tile.into();
        let mut s = self.state();
        s.claims.remove(&tile);
        s.open.retain(|(t, _), _| *t != tile);
        s.shared.retain(|(t, _), _| *t != tile);
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
                let mut tiles: Vec<Holder> = s
                    .claims
                    .iter()
                    .filter(|(_, c)| **c == source)
                    .map(|(t, _)| *t)
                    .chain(s.open.iter().filter(|((_, src), n)| *src == source && **n > 0).map(|((t, _), _)| *t))
                    .collect();
                tiles.sort();
                tiles.dedup();
                StreamUse { source, limit: s.limit(source), in_use: s.count(&tiles, source) }
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
        assert_eq!((refused.holder(), refused.closing()), (Some(A), false));
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
        assert!(b.acquire(B, TV).unwrap_err().closing());
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
        assert_eq!(b.acquire(B, TV).unwrap_err().holder(), Some(A));
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
    #[test]
    fn a_download_blocks_only_its_account_and_returns_it_after_pause() {
        let b = SharedBudget::default();
        b.set_limit(TV, Some(1));
        b.set_limit(TV2, Some(1));
        let id = DownloadId(17);
        let download = b.acquire(id, TV).unwrap();
        assert_eq!(b.acquire(A, TV), Err(BudgetExceeded::Download { id }));
        let other = b.acquire(B, TV2).unwrap();
        b.claim(A, Some(TV));
        assert!(!b.admits(A, TV), "a claim cannot steal an open download socket");
        b.release(id, download, None);
        let play = b.acquire(A, TV).unwrap();
        assert!(!b.admits(id, TV));
        b.release(A, play, None);
        b.claim(A, None);
        assert!(b.admits(id, TV), "the paused job may resume after playback stops");
        assert_eq!(b.usage().iter().find(|s| s.source == TV2).unwrap().in_use, 1);
        b.release(B, other, None);
    }
}
