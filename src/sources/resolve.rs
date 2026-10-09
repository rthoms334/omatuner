//! MediaRef → Playable. Owner: WP-05b.
//!
//! Runs on the player controller thread through `Services`' resolver (own reader connection;
//! point queries only, < 2 ms): loads the channel/movie/episode and its
//! source, builds the URL (Xtream URL builders with credentials, M3U URL as-is, catch-up via
//! `catchup::build_url` with the source's server tz override → `AccountInfo.server_tz` → local),
//! merges headers (entry > source User-Agent > config default), applies `force-media-title`
//! semantics (title from catalogue, never from URL), and fills `start_at` from saved progress
//! (only if > `resume_min_s` and not finished; the UI decides Resume vs Start over before
//! calling with `resume: false` to drop it).
//!
//! Errors: DRM-protected channel → `Error::BadPayload("drm")` (the player maps it to
//! `PlayerErrorKind::Drm`); missing rows → `Error::NotFound`; catch-up outside the archive
//! window → `Error::NotFound("outside catch-up window")`.

use rusqlite::Connection;

use crate::config::Config;
use crate::error::Result;
use crate::model::{MediaRef, Playable, Timestamp};

pub fn resolve(conn: &Connection, cfg: &Config, media: &MediaRef, resume: bool, now: Timestamp) -> Result<Playable> {
    todo!("WP-05b")
}

/// Whether a catch-up of `channel` at `start` is possible (archive flag + window). Used by the
/// guide to show the ⟲ glyph without building URLs.
pub fn catchup_available(has_catchup: bool, catchup_days: u32, start: Timestamp, now: Timestamp) -> bool {
    has_catchup && start < now && start >= now - catchup_days as i64 * 86_400
}
