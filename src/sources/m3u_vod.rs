//! Movies and series in plain M3U playlists (DESIGN §27.5). Owner: P5 sources track.
//!
//! Used when an Xtream-style playlist stays M3U (the Xtream login failed or the user kept the
//! playlist): entries whose URL is an Xtream `/movie/` or `/series/` stream become movies and
//! series episodes instead of live channels. Everything else, and every entry of a plain
//! playlist (iptv-org, Free-TV), stays live. Pure functions; `jobs::import_source` converts.

use super::m3u::M3uEntry;

/// What an M3U entry is.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryClass {
    Live,
    /// `/movie/<u>/<p>/<id>.<ext>`.
    Movie {
        stream_id: i64,
    },
    /// `/series/<u>/<p>/<id>.<ext>`; `marker` = the parsed `S01E02` part of the title, `None`
    /// when the title has none (§27.5: season 1, numbered in file order).
    Episode {
        stream_id: i64,
        marker: Option<EpisodeMarker>,
    },
}

/// `"Show Name S01 E02 - Title"` taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeMarker {
    /// Text before the marker, separators trimmed; empty when the title starts with it.
    pub series: String,
    pub season: u32,
    pub episode: u32,
    /// Text after the marker, separators trimmed; `None` when empty.
    pub title: Option<String>,
}

/// Classify one entry (§27.5 rules): an Xtream `/movie/` or `/series/` stream URL without
/// entry-level HTTP headers and without DRM is a movie / an episode; anything else is live.
pub fn classify(e: &M3uEntry) -> EntryClass {
    let _ = e;
    todo!("P5-sources")
}

/// Find the season/episode marker in a title (§27.5 grammar: `S01E02`, `S01 E02`, `S1E2`,
/// `S01.E02`, `S01_E02`, `S01-E02`, `S01 EP02`, `1x02`, `Season 1 Episode 2`; case-insensitive,
/// leftmost match wins).
pub fn parse_episode_title(title: &str) -> Option<EpisodeMarker> {
    let _ = title;
    todo!("P5-sources")
}
