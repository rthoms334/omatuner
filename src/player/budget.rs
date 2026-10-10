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
