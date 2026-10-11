//! The spoiler guard (DESIGN §34): on, but narrow — series you're part-way through and sport
//! you follow, with a quick reveal (the user's decision, 2026-10-11). Batch base wrote the
//! contract; OWNERS:
//! * S1 (engine): `snapshot.rs`, `verdict.rs`, `tracker.rs`, `guard/*`, `textmatch/scores.rs`,
//!   `store::spoiler`, `model/spoiler.rs`, migration 0012;
//! * S-ui: `spoiler/ui.rs` (new: `RevealSet`, Ctrl+B / Ctrl+Shift+B), the painters' widget, the
//!   menu, the rule editor, the Settings page, the status chip, the player's shield card.
//!
//! The guard is display-only (stored data, search and sort never change), fails closed before
//! its first build, runs after `Visibility` (never lists, counts or reveals explicit content)
//! and never bypasses a PIN.

pub mod snapshot;
pub mod tracker;
pub mod verdict;

/// S-ui flips this in its last commit.
pub const ENABLED: bool = false;
