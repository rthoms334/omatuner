//! What the player is asked to play. FROZEN (skeleton).
//!
//! UI code names *what* to play with a [`MediaRef`]; `sources::resolve` turns it into a
//! [`Playable`] (URL + headers + metadata) using source credentials and catch-up rules. The
//! player never sees `MediaRef`s and never talks to the store for URLs.

use std::sync::Arc;

use super::{ChannelId, Collection, EpisodeId, HttpHeaders, ItemRef, MovieId, SourceId, Timestamp};
use crate::redact::Secret;

/// UI-level reference to something playable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MediaRef {
    Channel(ChannelId),
    /// Catch-up of a past programme on `channel` (times are programme start/stop, UTC).
    Catchup { channel: ChannelId, start: Timestamp, stop: Timestamp, catchup_id: Option<String> },
    Movie(MovieId),
    Episode(EpisodeId),
    /// Ad-hoc URL (CLI `tuner <url>` of a single stream, drag & drop).
    Url(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayableKind {
    Live,
    Catchup { start: Timestamp, stop: Timestamp },
    Movie,
    Episode { season: u32, episode: u32 },
    Url,
}

impl PlayableKind {
    pub fn is_live(&self) -> bool { matches!(self, PlayableKind::Live) }
    /// Seekable content (VOD, catch-up, files) — Left/Right seek instead of zap/recall.
    pub fn is_seekable(&self) -> bool { !self.is_live() }
}

/// Fully resolved playback request.
#[derive(Debug, Clone, PartialEq)]
pub struct Playable {
    pub media: MediaRef,
    pub kind: PlayableKind,
    /// Durable identity for progress/history/favourites (None for ad-hoc URLs).
    pub item: Option<ItemRef>,
    pub source_id: Option<SourceId>,
    /// Title shown in OSD, MPRIS, notifications (also mpv `force-media-title`, so mpv never
    /// derives a title from the credential-bearing URL).
    pub title: String,
    /// e.g. current programme or "S02E05 · Episode title".
    pub subtitle: Option<String>,
    pub logo: Option<String>,
    pub number: Option<u32>,
    /// Stream URL. Contains credentials for Xtream — Secret so it can't be logged by accident.
    pub url: Secret<String>,
    pub headers: HttpHeaders,
    /// Resume position (seconds) for VOD.
    pub start_at: Option<f64>,
    /// Known duration (VOD metadata / catch-up length) in seconds.
    pub duration_hint: Option<f64>,
}

/// Ordered list used for zapping (Up/Down, MPRIS Next/Previous) — the collection the
/// channel was opened from, already filtered (hidden removed) and sorted.
#[derive(Debug, Clone, PartialEq)]
pub struct ZapList {
    pub collection: Collection,
    pub channels: Arc<[ChannelId]>,
}

/// Where a play request came from (affects placement and history recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayOrigin {
    LivePreview,
    Guide,
    Fullscreen,
    Home,
    Search,
    Mpris,
    Reminder,
    Cli,
}
