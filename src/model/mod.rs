//! Core data types shared by every module. FROZEN (skeleton).
//!
//! These are plain data: no I/O, no egui, no mpv. Database row ids (`*Id`) are stable while an
//! item keeps its [`ItemKey`] (imports upsert by key); user data (favourites, overrides,
//! progress, history, reminders) is keyed by [`ItemRef`] = (source, kind, key) so it survives
//! refreshes and re-numbering (see DESIGN §8 "Identity & reattach").

pub mod agent;
pub mod category;
pub mod epg;
pub mod ids;
pub mod live;
pub mod playable;
pub mod source;
pub mod spoiler;
pub mod user;
pub mod vod;

pub use agent::AgentName;
pub use category::*;
pub use epg::*;
pub use ids::*;
pub use live::*;
pub use playable::*;
pub use source::*;
pub use spoiler::*;
pub use user::*;
pub use vod::*;

/// Unix seconds (UTC). All timestamps in the model and DB use this.
pub type Timestamp = i64;

/// Current unix time in seconds.
pub fn now() -> Timestamp {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
