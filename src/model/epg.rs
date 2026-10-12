//! EPG: sources, channels, programmes, mappings. FROZEN (skeleton).

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{EpgChannelId, EpgSourceId, ItemRef, ProgrammeId, SourceId, Timestamp};
use crate::redact::Secret;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum EpgSourceKind {
    /// User-added XMLTV URL (`.xml`, `.xml.gz`, `.xml.xz`).
    Url {
        url: Secret<String>,
    },
    File {
        path: PathBuf,
    },
    /// Provider guide of a playlist source: Xtream `xmltv.php` or M3U header `url-tvg`
    /// (one EpgSource per URL). Created/removed automatically with the source.
    Provider {
        source_id: SourceId,
        url: Secret<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpgSource {
    pub id: EpgSourceId,
    pub name: String,
    pub kind: EpgSourceKind,
    /// Lower = higher priority. First source with data for a channel wins (UHF users asked
    /// for orderable EPG sources).
    pub priority: i32,
    /// Global offset in minutes added to every programme time of this source.
    pub offset_min: i32,
    pub enabled: bool,
    pub refresh_h: Option<u32>,
    /// Generation whose programmes are visible; imports write `active_generation + 1` and flip.
    pub active_generation: i64,
    pub last_refresh: Option<Timestamp>,
    pub last_error: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub channel_count: u32,
    pub programme_count: u32,
    /// Latest programme stop time (staleness: guide "ends in N h").
    pub max_stop: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpgChannel {
    pub id: EpgChannelId,
    pub epg_source_id: EpgSourceId,
    pub xmltv_id: String,
    pub display_names: Vec<String>,
    pub icon: Option<String>,
}

/// XMLTV `<episode-num>` decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EpisodeNum {
    /// 1-based season.
    pub season: Option<u32>,
    /// 1-based episode.
    pub episode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Programme {
    pub id: ProgrammeId,
    pub epg_channel_id: EpgChannelId,
    /// UTC seconds, source offset already applied; EPG time (channel tvg-shift NOT applied —
    /// see [`ProgrammeSlim::shifted`]).
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub icon: Option<String>,
    pub episode: Option<EpisodeNum>,
    pub rating: Option<String>,
    pub star_rating: Option<String>,
    /// Kodi `catchup-id` attribute.
    pub catchup_id: Option<String>,
    pub is_new: bool,
    pub is_live: bool,
    pub previously_shown: bool,
}

/// Grid / now-next representation (cheap to clone; title shared).
#[derive(Debug, Clone, PartialEq)]
pub struct ProgrammeSlim {
    pub id: ProgrammeId,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: Arc<str>,
    pub sub_title: Option<Arc<str>>,
    pub flags: ProgrammeFlags,
    /// The guide's season / episode (the spoiler guard judges series airings by it).
    pub episode: Option<EpisodeNum>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgrammeFlags {
    pub is_new: bool,
    pub is_live: bool,
    pub has_catchup_id: bool,
}

impl ProgrammeSlim {
    /// The programme as shown on a channel with `tvg-shift` = `shift_min` minutes:
    /// `start/stop + shift_min·60` (a "+1" channel with `tvg-shift="1"` → shift_min = 60 shows
    /// the 20:00 programme at 21:00). Store reads return EPG time; every UI caller (rows, grid,
    /// OSD, search hits) applies this exactly once. `store::epg::now_next` is the one read that
    /// takes the shifts itself (it must query at `now − shift`). Catch-up converts back
    /// (`sources::catchup` subtracts the shift from the displayed start).
    pub fn shifted(&self, shift_min: i32) -> ProgrammeSlim {
        if shift_min == 0 {
            return self.clone();
        }
        let d = shift_min as i64 * 60;
        ProgrammeSlim { start: self.start + d, stop: self.stop + d, ..self.clone() }
    }
    pub fn contains(&self, t: Timestamp) -> bool {
        self.start <= t && t < self.stop
    }
    /// 0..=1 progress at `t`.
    pub fn progress(&self, t: Timestamp) -> f32 {
        let d = (self.stop - self.start).max(1) as f32;
        ((t - self.start) as f32 / d).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NowNext {
    pub now: Option<ProgrammeSlim>,
    pub next: Option<ProgrammeSlim>,
}

/// How a channel got its EPG mapping (priority order = enum order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EpgMatchMethod {
    /// User picked it in the EPG mapping screen; never overwritten.
    Manual,
    TvgIdExact,
    TvgIdCaseless,
    /// Xtream `epg_channel_id` against the provider's own `xmltv.php`.
    XtreamEpgId,
    /// iptv-org `Name.cc@Feed` with the `@Feed` suffix stripped.
    FeedStripped,
    /// Normalized name equals a normalized display-name.
    NormalizedName,
    /// Fuzzy suggestion the user accepted (stored as Manual) or pending (not applied).
    Fuzzy,
    /// Guide fixes rule "+1/+2 follow their base" (DESIGN §31): a +N channel mapped to its base
    /// channel's guide channel with a time shift (`channel_epg_map.shift_min`). Re-derived on
    /// every rematch; ranks after every other automatic method.
    Sibling,
    /// Guide fixes rule "Copies share a guide": the same channel (`store::copies` family key) in
    /// another playlist resolves, so this one takes its guide channel.
    Copy,
}

/// A channel → EPG channel assignment, keyed by the channel's stable [`ItemRef`].
#[derive(Debug, Clone, PartialEq)]
pub struct EpgMapping {
    pub channel: ItemRef,
    pub epg_source_id: EpgSourceId,
    pub xmltv_id: String,
    pub method: EpgMatchMethod,
    /// 1.0 for exact methods; similarity for fuzzy.
    pub confidence: f32,
}

/// A fuzzy candidate shown in the "Assign EPG" picker.
#[derive(Debug, Clone, PartialEq)]
pub struct EpgSuggestion {
    pub epg_channel_id: EpgChannelId,
    pub epg_source_id: EpgSourceId,
    pub xmltv_id: String,
    pub display_name: String,
    pub score: f32,
}

/// A programme search hit (FTS) joined with the channels that show it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgrammeHit {
    pub programme: ProgrammeSlim,
    pub epg_channel_id: EpgChannelId,
    pub channel_ids: Vec<super::ChannelId>,
}

// ------------------------------------------------------------------ guide fixes (DESIGN §31)

/// What is wrong with a channel's guide data (Guide fixes, computed by `guide_fix::issues`,
/// never stored; `item_override.guide_ok` holds one bit per kind the user dismissed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueKind {
    /// Unmapped, and nothing scores above the fuzzy threshold.
    NoGuide,
    /// Unmapped, a cached suggestion scores above the threshold.
    Suggested,
    /// "+1" in the name, effective shift 0 and the base's guide channel.
    PlusNoShift,
    /// Unmapped; the same channel in another playlist (`store::copies` family) has guide data.
    CopyHasGuide,
    /// Mapped, but the guide has nothing now or later for it.
    GuideEnded,
    /// A manual mapping whose guide channel left the active guide generation.
    GoneFromGuide,
    /// An automatic tvg-id mapping whose guide channel shares no trigram with the name.
    LooksWrong,
}

impl IssueKind {
    pub const ALL: [IssueKind; 7] = [
        IssueKind::NoGuide,
        IssueKind::Suggested,
        IssueKind::PlusNoShift,
        IssueKind::CopyHasGuide,
        IssueKind::GuideEnded,
        IssueKind::GoneFromGuide,
        IssueKind::LooksWrong,
    ];

    /// The bit of `item_override.guide_ok` that dismisses this kind for a channel.
    pub fn bit(self) -> i64 {
        1 << (self as i64)
    }
}

/// A guide channel a fix points at (agent refs: `guide:<epg_source_id>:<xmltv_id>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GuideTarget {
    pub epg_source: EpgSourceId,
    pub xmltv_id: String,
    /// Display name when chosen (finds a renamed id again: `GoneFromGuide`).
    pub name: String,
}

/// One channel's change in a Guide fixes batch. Each `Some` field is changed; `Some(None)`
/// resets it to automatic.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FixChange {
    pub channel: Option<ItemRef>,
    pub map: Option<Option<GuideTarget>>,
    /// Minutes the stream airs after the guide (`item_override.shift_min`).
    pub shift_min: Option<Option<i32>>,
    pub catchup_offset_s: Option<Option<i64>>,
    /// "It's right": dismiss this issue kind for the channel.
    pub dismiss: Option<IssueKind>,
}

/// A batch to apply (the screen's confirm card, or Claude's `guide_fix propose`), bound to the
/// guide check it was computed from.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Proposal {
    pub changes: Vec<FixChange>,
    pub rules: Vec<(SourceId, super::GuideRules)>,
    /// `meta guide_check:stamp` when proposed: a different stamp at apply time = stale.
    pub stamp: [u8; 16],
}

/// Who made a guide fix (`guide_fix_log.actor`, `channel_epg_map.actor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FixActor {
    #[default]
    You,
    Claude,
}
