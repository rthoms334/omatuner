//! Per-source connection budget (critique gap 4). Owner: WP-07. DESIGN §11.5.
//!
//! v1 has exactly one mpv instance, so at most one stream is open; the budget still exists so
//! that (a) the rule "close the old stream before opening the new one" is explicit and (b)
//! future features (multiview, recording,
//! guide preview in a second instance) must acquire a lease and will be refused when a
//! provider's `max_connections` is reached.
//!
//! Stop-before-start: `loadfile … replace` RETURNS BEFORE the old file is stopped (mpv 0.41
//! man page) and demuxer teardown is asynchronous. When the old and new streams share a source
//! whose limit is ≤ 1 or unknown, the controller sends `stop`, waits for the old entry's
//! END_FILE (max 1.5 s, `PendingLoad`), and only then loads; the old lease is released on that
//! END_FILE, never before the command (DESIGN §11.5).

use std::collections::HashMap;

use crate::model::SourceId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub source: SourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("connection limit reached for this provider ({in_use}/{limit})")]
pub struct BudgetExceeded {
    pub in_use: u32,
    pub limit: u32,
}

#[derive(Debug, Default)]
pub struct ConnectionBudget {
    limits: HashMap<SourceId, u32>,
    open: HashMap<SourceId, u32>,
}

impl ConnectionBudget {
    /// Set from `AccountInfo.max_connections` (None ⇒ unlimited; 0 is treated as 1).
    pub fn set_limit(&mut self, source: SourceId, max: Option<u32>) {
        match max {
            Some(m) => {
                self.limits.insert(source, m.max(1));
            }
            None => {
                self.limits.remove(&source);
            }
        }
    }

    pub fn try_acquire(&mut self, source: SourceId) -> Result<Lease, BudgetExceeded> {
        let in_use = *self.open.get(&source).unwrap_or(&0);
        if let Some(&limit) = self.limits.get(&source)
            && in_use >= limit
        {
            return Err(BudgetExceeded { in_use, limit });
        }
        *self.open.entry(source).or_insert(0) += 1;
        Ok(Lease { source })
    }

    pub fn release(&mut self, lease: Lease) {
        if let Some(n) = self.open.get_mut(&lease.source) {
            *n = n.saturating_sub(1);
        }
    }

    /// Known provider limit (`None` = unknown / unlimited).
    pub fn limit(&self, source: SourceId) -> Option<u32> {
        self.limits.get(&source).copied()
    }

    pub fn in_use(&self, source: SourceId) -> u32 {
        *self.open.get(&source).unwrap_or(&0)
    }
}

/// The connection budget shared by every picture's controller (multi-view, DESIGN §35 §4.4):
/// limits, the source each picture claims (kept through Reconnecting), open leases, the race
/// window and limits learned from a refused second stream. Batch base: the API; MV-1 moves the
/// controller's own [`ConnectionBudget`] and the race window here. OWNER: MV-1.
#[derive(Debug, Clone, Default)]
pub struct SharedBudget(std::sync::Arc<std::sync::Mutex<SharedState>>);

#[derive(Debug, Default)]
struct SharedState {
    limits: HashMap<SourceId, u32>,
    claims: HashMap<crate::model::TileId, SourceId>,
}

/// What the multi-view planner and MCP `status.streams` see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamUse {
    pub source: SourceId,
    /// `None` = unknown / unlimited.
    pub limit: Option<u32>,
    pub in_use: u32,
}

impl SharedBudget {
    fn state(&self) -> std::sync::MutexGuard<'_, SharedState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `AccountInfo.max_connections` (None ⇒ unknown; 0 is treated as 1).
    pub fn set_limit(&self, source: SourceId, max: Option<u32>) {
        let mut s = self.state();
        match max {
            Some(m) => s.limits.insert(source, m.max(1)),
            None => s.limits.remove(&source),
        };
    }

    pub fn limit(&self, source: SourceId) -> Option<u32> {
        self.state().limits.get(&source).copied()
    }

    /// The source `tile` targets (set on play / zap, kept through Reconnecting, cleared on
    /// stop, end, final error or a retarget).
    pub fn claim(&self, tile: crate::model::TileId, source: Option<SourceId>) {
        let mut s = self.state();
        match source {
            Some(src) => s.claims.insert(tile, src),
            None => s.claims.remove(&tile),
        };
    }

    /// May `tile` open a stream on `source`: the OTHER pictures' claims stay under its limit.
    pub fn admits(&self, tile: crate::model::TileId, source: SourceId) -> bool {
        let s = self.state();
        let others = s.claims.iter().filter(|(t, src)| **t != tile && **src == source).count() as u32;
        s.limits.get(&source).is_none_or(|limit| others < *limit)
    }

    /// Per playlist with a known limit or a claim: limit and streams in use.
    pub fn usage(&self) -> Vec<StreamUse> {
        let s = self.state();
        let mut sources: Vec<SourceId> = s.limits.keys().chain(s.claims.values()).copied().collect();
        sources.sort();
        sources.dedup();
        sources
            .into_iter()
            .map(|source| StreamUse {
                source,
                limit: s.limits.get(&source).copied(),
                in_use: s.claims.values().filter(|c| **c == source).count() as u32,
            })
            .collect()
    }
}
