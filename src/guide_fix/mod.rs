//! Guide fixes: fixing guide data at scale (DESIGN §31) — a coverage dashboard, an issue list
//! with a proposed fix on every row, per-playlist rules that persist, logged batches with Undo.
//! Batch base wrote the contract; OWNERS:
//! * G-core: `catalog.rs`, `issues.rs`, `preview.rs`, `textmatch/timeshift.rs`,
//!   `sources/epg_match.rs`, `store::epg`, `store::guide_fix`, `store::retime`,
//!   `jobs::guide_check`, `jobs::import_epg`, `sources::resolve`, migration 0009;
//! * G-ui: `guide_fix/ui.rs` (new), the Guide fixes screen (`ui/screens/epg_mapping.rs`
//!   rewrite), the time-shift row of `ui/overlays/edit_item.rs`.
//!
//! The universe is the library snapshot: explicit channels are absent while hidden (rows,
//! counts, coverage, previews, MCP). Every write is a logged batch; shifts move reminders, Up
//! next slots and Following rows in the same transaction (B8).

pub mod catalog;
pub mod issues;
pub mod preview;
pub mod ui;

pub use crate::store::guide_fix::GuideOpts;

use crate::model::SourceId;
use crate::store::library::Library;

/// On (G-ui): the guide check runs, rule rows are derived, the screen shows Guide fixes.
pub const ENABLED: bool = true;

/// One playlist's guide coverage over its SHOWN channels (the dashboard card, Settings ▸ EPG,
/// Settings ▸ Playlists, the Live "not in your guide" note, onboarding, MCP `sources`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCoverage {
    pub source: SourceId,
    /// Channels shown (not hidden, supported; explicit ones only while shown).
    pub shown: usize,
    /// Of those, the ones with guide data.
    pub with_guide: usize,
    /// Hidden channels that have guide data (for "Include hidden").
    pub hidden_with_guide: usize,
}

/// Coverage per playlist of `lib`, in the library's playlist order — the ONE coverage function
/// (base-B moves today's call sites here; G-core may refine it).
pub fn coverage(lib: &Library) -> Vec<SourceCoverage> {
    let mut out: Vec<SourceCoverage> =
        lib.sources.iter().map(|s| SourceCoverage { source: s.id, shown: 0, with_guide: 0, hidden_with_guide: 0 }).collect();
    for c in lib.live.channels.iter() {
        let Some(cov) = out.iter_mut().find(|v| v.source == c.source_id) else { continue };
        let guide = c.epg_channel_id.is_some();
        if lib.live.hidden.contains(&c.id) || c.unsupported {
            cov.hidden_with_guide += usize::from(guide);
        } else {
            cov.shown += 1;
            cov.with_guide += usize::from(guide);
        }
    }
    out.retain(|c| c.shown > 0 || c.hidden_with_guide > 0);
    out
}
