//! New episodes (DESIGN §44): when a new episode of a series you care about appears in a
//! playlist's series catalogue (on demand, not live TV), Tuner tells you once — a desktop
//! notification with Play, a "New episodes" row on Home, a "New" chip in Watchlists. The series
//! you care about are the ones on any watchlist plus the ones you binged; nothing to set up.
//!
//! OWNERS:
//! * N0 (skeleton): this module, `model.rs`, migration 0018, `[new_episodes]`,
//!   `JobKind::CheckNewEpisodes`, `DataScope::NewEpisodes`, the `PlatformEvent` variants;
//! * N-core: `store::new_episodes` (tracked set, binge rule, baseline, diff, clearing, mute,
//!   backup), `jobs::new_episodes` (debounce, fetch, announce), `details::load_series_checked`,
//!   `platform::notify::show_with_actions`;
//! * N-ui: the Home row, the Watchlists chip and dot, the series page chips, Settings ▸ New
//!   episodes, the first-run card, the toast fallback (`PlatformEvent::NewEpisodesToast`);
//! * N-agent: the read-only `new_episodes` MCP tool.
//!
//! The flow: a playlist refresh finishes → `jobs::new_episodes::request` (debounced, after the
//! library rebuild) → `JobKind::CheckNewEpisodes`: the tracked set, fetch the tracked Xtream
//! copies that changed, diff against the ledger, write arrivals → `DataScope::NewEpisodes` →
//! notifications (held while a film or episode plays full screen and in the news flash's quiet
//! hours). Watchlist and progress changes only re-track (`jobs::new_episodes::track_from`):
//! local, no network.

pub mod model;

pub use model::*;

/// The kill switch, as with `downloads::ENABLED`: off, nothing is checked, fetched or shown
/// whatever `[new_episodes] enabled` says.
pub const ENABLED: bool = true;

/// Why a check runs (`JobKind::CheckNewEpisodes`); every request coalesces into one job
/// ("new-episodes") and the merged one keeps the broadest fetch scope ([`CheckReason::merge`]).
/// The job's `sources` are the playlists refreshed meanwhile: their fallback is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckReason {
    /// The app published its first library: pick up what a headless `tuner --refresh` changed
    /// (cleared `details_at`, new playlist episodes). Fetches only copies whose details went
    /// stale through `last_modified`, on every playlist (and the fallback of the refreshes
    /// merged into it).
    Launch,
    /// Playlist refreshes finished (`sources`): their tracked copies that changed, plus the
    /// fallback for providers that never move `last_modified`.
    Refresh,
    /// "Check now" in Settings ▸ New episodes (at most every 10 min): every playlist, as a
    /// refresh would, without refreshing them.
    Manual,
}

impl CheckReason {
    /// Two requests as one check that does what both would: Check now covers everything; a
    /// launch's every-playlist scope survives a refresh merged into it (whose playlists keep
    /// their fallback through the merged `sources`).
    pub fn merge(self, other: CheckReason) -> CheckReason {
        use CheckReason::*;
        match (self, other) {
            (Manual, _) | (_, Manual) => Manual,
            (Launch, _) | (_, Launch) => Launch,
            (Refresh, Refresh) => Refresh,
        }
    }
}
