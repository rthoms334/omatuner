//! Live TV: groups, channels, stream locators, catch-up specs. FROZEN (skeleton).

use serde::{Deserialize, Serialize};

use super::{ChannelId, EpgChannelId, FavListId, GroupId, GroupKey, ItemKey, ItemKind, SourceId, Timestamp};

/// A category of channels, movies or series inside one source (M3U `group-title`, Xtream
/// category). User overrides (rename/hide/lock/order) are merged in by the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: GroupId,
    pub source_id: SourceId,
    /// Channel, Movie or Series.
    pub kind: ItemKind,
    pub key: GroupKey,
    /// Provider name, as imported (after smart cleanup if enabled).
    pub name: String,
    /// User rename, if any.
    pub rename: Option<String>,
    /// Position in the provider's ordering.
    pub provider_order: i32,
    /// User ordering (lower first); `None` = provider order.
    pub user_order: Option<i32>,
    /// Effective: the user's hide, or the source's category filter (see `filter`).
    pub hidden: bool,
    /// The user's own hide (`GroupOverride::hidden`), whatever the rule does.
    pub user_hidden: bool,
    /// PIN-locked (parental).
    pub locked: bool,
    pub item_count: u32,
    /// Xtream category id, if any.
    pub provider_id: Option<String>,
    /// Country / language tag of the provider's raw category name (DESIGN §28.2).
    pub tag: Option<super::CategoryTag>,
    /// What the source's category filter does to this group (DESIGN §28.3).
    pub filter: super::GroupFilter,
    /// Explicit (adult) content, and why (DESIGN §20.3).
    pub explicit: Explicit,
}

impl Group {
    pub fn display_name(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.name)
    }
}

/// Is a category explicit (adult) content, and why (DESIGN §20.3)? Explicit categories and
/// their items are absent from every list while parental control hides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Explicit {
    #[default]
    No,
    /// Its provider name reads as adult content (`sources::explicit::is_explicit_name`).
    Name,
    /// The provider flags it (Xtream `is_adult`).
    Provider,
    /// The user marked it ("Mark as explicit").
    Marked,
    /// Detected or not, the user said "Not explicit".
    Cleared,
}

impl Explicit {
    /// From `grp.explicit` (what the import found) and `group_override.explicit` (the user's
    /// word, which wins).
    pub fn of(stored: i64, user: Option<bool>) -> Self {
        match (user, stored) {
            (Some(true), _) => Explicit::Marked,
            (Some(false), _) => Explicit::Cleared,
            (None, 0) => Explicit::No,
            (None, 2) => Explicit::Provider,
            (None, _) => Explicit::Name,
        }
    }

    /// Hidden while parental control hides explicit content.
    pub fn is_explicit(self) -> bool {
        matches!(self, Explicit::Name | Explicit::Provider | Explicit::Marked)
    }

    /// Detected or marked at some point (Settings ▸ Parental lists it).
    pub fn is_listed(self) -> bool {
        self != Explicit::No
    }
}

/// Extra HTTP request properties for a stream (M3U `#EXTVLCOPT`, `#KODIPROP`, pipe headers,
/// source User-Agent). Passed to mpv as per-file options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpHeaders {
    pub user_agent: Option<String>,
    pub referrer: Option<String>,
    pub origin: Option<String>,
    /// Additional `Name: value` headers (cookies, auth tokens...). Values are secrets for logs.
    pub extra: Vec<(String, String)>,
}

impl HttpHeaders {
    pub fn is_empty(&self) -> bool {
        self.user_agent.is_none() && self.referrer.is_none() && self.origin.is_none() && self.extra.is_empty()
    }
}

/// Xtream stream family, for URL building.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XtreamStreamKind {
    Live,
    Movie,
    Series,
}

/// Where a stream lives. Xtream URLs are built at play time from the source credentials so
/// passwords are never duplicated into catalogue rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum StreamLocator {
    /// Literal URL from an M3U (may embed tokens — redact when displaying).
    Url { url: String },
    /// Xtream stream; `ext` = container extension for movie/series, ignored for live.
    Xtream { kind: XtreamStreamKind, stream_id: i64, ext: Option<String> },
}

/// Catch-up flavour (M3U `catchup=` / Xtream `tv_archive`). See DESIGN §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatchupKind {
    /// `catchup-source` is the full URL template (or behaves like Append when missing).
    Default,
    /// `catchup-source` is appended to the live URL.
    Append,
    /// `?utc={utc}&lutc={lutc}` appended (alias `timeshift`).
    Shift,
    /// Flussonic URL rewrite (`flussonic`, `fs`).
    Flussonic,
    FlussonicHls,
    FlussonicTs,
    /// Xtream `timeshift` path (alias `xtream`); also used for native Xtream sources.
    Xc,
    /// `catchup-source` or `{catchup-id}` is a VOD URL.
    Vod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchupSpec {
    pub kind: CatchupKind,
    /// `catchup-source` template, if any.
    pub template: Option<String>,
    /// Archive window in days (catchup-days → timeshift → tvg-rec → catchup-time/86400;
    /// Xtream `tv_archive_duration`).
    pub days: u32,
    /// `catchup-correction` in seconds (subtracted from programme start).
    pub correction_s: i64,
}

/// Why a channel is known not to be playable by mpv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unsupported {
    /// Widevine/PlayReady (`#KODIPROP inputstream.adaptive.license_type`).
    Drm,
}

/// Video quality class of a channel: the SD / HD / FHD / 4K badge next to its name
/// (DESIGN §13.0). Ordered worst → best.
///
/// From the provider name at import (`smart_names`: "BBC One FHD", "ESPN ᴴᴰ", "2160p"…), or
/// measured: the size mpv decoded the last time the channel played, which wins (DESIGN §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Quality {
    Sd,
    Hd,
    Fhd,
    Uhd,
}

impl Quality {
    /// The badge text.
    pub fn label(self) -> &'static str {
        match self {
            Quality::Sd => "SD",
            Quality::Hd => "HD",
            Quality::Fhd => "FHD",
            Quality::Uhd => "4K",
        }
    }

    /// The class of a decoded `w`×`h` picture (mpv `video-params`): the better of what the
    /// height and the width say, so cropped (1920×800) and anamorphic (1440×1080) frames read
    /// right. 1440p is FHD: four classes keep the badge narrow, and a QHD stream is closer to
    /// FHD than to the 4K it would otherwise claim. `None` for a size that isn't one.
    pub fn of_video(w: i64, h: i64) -> Option<Quality> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let by_h = match h {
            2000.. => Quality::Uhd,
            1000.. => Quality::Fhd,
            700.. => Quality::Hd,
            _ => Quality::Sd,
        };
        let by_w = match w {
            3800.. => Quality::Uhd,
            1900.. => Quality::Fhd,
            1260.. => Quality::Hd,
            _ => Quality::Sd,
        };
        Some(by_h.max(by_w))
    }

    /// `channel.quality` (1 = SD … 4 = 4K).
    pub fn code(self) -> u8 {
        self as u8 + 1
    }

    pub fn from_code(c: i64) -> Option<Quality> {
        [Quality::Sd, Quality::Hd, Quality::Fhd, Quality::Uhd].get(usize::try_from(c.checked_sub(1)?).ok()?).copied()
    }
}

/// A channel's quality badge as shown (DESIGN §13.0): the class, and where it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QualityBadge {
    pub class: Quality,
    /// The quality word of the provider name (`channel.quality`), measured or not.
    pub claimed: Option<Quality>,
    /// `class` is what mpv decoded when the channel last played (else it is the claim).
    pub measured: bool,
    /// An unmeasured claim from a source whose claims of this class measured lower too often
    /// ([`LabelTrust::doubts`]): the badge reads `4K?`.
    pub doubtful: bool,
}

impl QualityBadge {
    /// The provider name's claim, not measured.
    pub fn named(class: Quality, doubtful: bool) -> Self {
        Self { class, claimed: Some(class), measured: false, doubtful }
    }

    /// What mpv decoded (`claimed`: the name's word, if any).
    pub fn measured(class: Quality, claimed: Option<Quality>) -> Self {
        Self { class, claimed, measured: true, doubtful: false }
    }

    /// The badge text: `4K`, or `4K?` for a doubtful claim.
    pub fn label(self) -> &'static str {
        match (self.class, self.doubtful) {
            (q, false) => q.label(),
            (Quality::Sd, true) => "SD?",
            (Quality::Hd, true) => "HD?",
            (Quality::Fhd, true) => "FHD?",
            (Quality::Uhd, true) => "4K?",
        }
    }

    /// The tooltip / stream-info line: where the badge comes from.
    pub fn explain(self) -> String {
        let q = self.class.label();
        match (self.measured, self.claimed) {
            (false, _) if self.doubtful => {
                format!("The provider says {q}; not verified. Channels of this playlist labelled {q} often play at a lower quality.")
            }
            (false, _) => format!("The provider says {q}; not verified until it plays."),
            (true, Some(c)) if c != self.class => format!("Played at {q} (the provider says {}).", c.label()),
            (true, _) => format!("Played at {q}."),
        }
    }
}

/// How far a source's quality words can be trusted (DESIGN §13.0.1), from the channels of it
/// that were measured (`quality_check`: one row per channel name, however often it
/// played). Measured quality always wins; this only marks the *unmeasured* claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LabelTrust {
    /// Channels of the source measured (with or without a quality word).
    pub measured: u32,
    /// Per claimed class (`Quality as usize`): channels measured with that word…
    pub claimed: [u32; 4],
    /// …and those of them that played at a lower class.
    pub wrong: [u32; 4],
}

impl LabelTrust {
    /// Too little evidence before this many channels of the source were measured.
    pub const MIN_MEASURED: u32 = 3;
    /// …and this many claims of the class: one bad stream is no pattern.
    pub const MIN_CLAIMS: u32 = 2;

    /// Record one measured channel.
    pub fn add(&mut self, claimed: Option<Quality>, measured: Quality) {
        self.measured += 1;
        if let Some(c) = claimed {
            self.claimed[c as usize] += 1;
            self.wrong[c as usize] += u32::from(measured < c);
        }
    }

    /// Are unmeasured `class` claims doubtful? Only the high classes (4K, FHD: the ones that
    /// get inflated), once [`Self::MIN_MEASURED`] channels and [`Self::MIN_CLAIMS`] claims of the
    /// class were measured and at least half of those claims played lower.
    pub fn doubts(&self, class: Quality) -> bool {
        let (claimed, wrong) = (self.claimed[class as usize], self.wrong[class as usize]);
        class >= Quality::Fhd && self.measured >= Self::MIN_MEASURED && claimed >= Self::MIN_CLAIMS && 2 * wrong >= claimed
    }
}

/// A live channel, fully loaded (detail views, playback). Lists use [`ChannelSummary`].
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub id: ChannelId,
    pub source_id: SourceId,
    pub key: ItemKey,
    /// Xtream stream_id (text) or M3U `tvg-id`-less → None.
    pub provider_id: Option<String>,
    /// Raw provider name.
    pub name: String,
    /// Smart-cleaned name (== name when cleanup disabled).
    pub clean_name: String,
    /// User rename.
    pub rename: Option<String>,
    /// `tvg-chno` / Xtream `num`.
    pub number: Option<u32>,
    pub logo: Option<String>,
    /// All groups (M3U `;`-lists / XUI `category_ids`), primary first.
    pub group_ids: Vec<GroupId>,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    /// `tvg-shift` in minutes (applied to EPG times for this channel).
    pub tvg_shift_min: i32,
    pub stream: StreamLocator,
    pub headers: HttpHeaders,
    pub catchup: Option<CatchupSpec>,
    /// Per-channel user catch-up offset in seconds (UHF 1.100.1), applied on top of correction;
    /// `None`: the playlist's (`GuideRules::catchup_offset_s`), so an own 0 overrides it.
    pub catchup_offset_s: Option<i64>,
    pub radio: bool,
    pub unsupported: Option<Unsupported>,
    pub provider_order: i32,
    pub user_order: Option<i32>,
    pub hidden: bool,
    pub locked: bool,
    pub added_at: Option<Timestamp>,
    /// Resolved EPG channel (manual mapping > automatic), if any.
    pub epg_channel_id: Option<EpgChannelId>,
}

impl Channel {
    pub fn display_name(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.clean_name)
    }
}

/// Slim, `Clone`-cheap row kept in the in-memory library index for every visible channel.
/// Everything a list row / grid header / search result needs without touching SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelSummary {
    pub id: ChannelId,
    pub source_id: SourceId,
    pub key: ItemKey,
    /// Display name (rename > clean name; a dropped tag appended when it collides with another
    /// channel of its category, `TV5 · RAW` — `store::library`).
    pub name: std::sync::Arc<str>,
    /// The provider's name when it differs from `name` (stream info).
    pub provider_name: Option<std::sync::Arc<str>>,
    pub number: Option<u32>,
    pub logo: Option<std::sync::Arc<str>>,
    pub group_ids: Vec<GroupId>,
    pub epg_channel_id: Option<EpgChannelId>,
    pub tvg_shift_min: i32,
    pub has_catchup: bool,
    pub catchup_days: u32,
    pub radio: bool,
    pub locked: bool,
    pub unsupported: bool,
    pub provider_order: i32,
    pub user_order: Option<i32>,
    pub added_at: Option<Timestamp>,
    /// The quality badge: measured when the channel has played (`channel_video`), else from
    /// its provider name (`channel.quality`, doubtful when the source's measured claims of that
    /// class were often wrong); `None` = unknown.
    pub quality: Option<QualityBadge>,
    /// Hash of the normalised cleaned name (`channel.family`, else `channel.norm_name`; quality
    /// words dropped): the SD / HD / 4K duplicates of one channel share it (best-first
    /// ordering, DESIGN §13.2).
    pub family: u64,
}

/// Ordering choices for lists (UHF users asked for date added / year / rating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SortMode {
    /// Provider order, with user reordering applied.
    #[default]
    Provider,
    Number,
    NameAsc,
    NameDesc,
    RecentlyAdded,
    /// VOD only.
    Rating,
    /// VOD only.
    Year,
}

/// The ordered set of channels the user is browsing; defines zapping order (UHF: up/down zaps
/// *within the collection you opened the channel from*).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Collection {
    AllChannels,
    Group(GroupId),
    Favourites(FavListId),
    /// Recently watched channels, most recent first.
    History,
    /// Ad-hoc (search results).
    Adhoc(Vec<ChannelId>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoded size → badge: the better of height and width, 1440p counts as FHD.
    #[test]
    fn video_size_classes() {
        let q = |w, h| Quality::of_video(w, h).map(Quality::label);
        assert_eq!(q(720, 576), Some("SD"));
        assert_eq!(q(1280, 720), Some("HD"));
        assert_eq!(q(1920, 1080), Some("FHD"));
        assert_eq!(q(1920, 800), Some("FHD"), "cropped");
        assert_eq!(q(1440, 1080), Some("FHD"), "anamorphic");
        assert_eq!(q(2560, 1440), Some("FHD"));
        assert_eq!(q(3840, 2160), Some("4K"));
        assert_eq!(q(0, 0), None);
    }

    /// The trust thresholds: enough channels measured, enough claims of the class, at least
    /// half of them lower; only 4K / FHD claims are doubted.
    #[test]
    fn label_trust_thresholds() {
        use Quality::*;
        let trust = |rows: &[(Option<Quality>, Quality)]| {
            let mut t = LabelTrust::default();
            for (c, m) in rows {
                t.add(*c, *m);
            }
            t
        };
        // two 4K claims, both HD, but only two channels measured: not yet
        assert!(!trust(&[(Some(Uhd), Hd), (Some(Uhd), Hd)]).doubts(Uhd));
        // a third channel (no claim) makes the evidence enough
        let t = trust(&[(Some(Uhd), Hd), (Some(Uhd), Hd), (None, Sd)]);
        assert!(t.doubts(Uhd));
        assert!(!t.doubts(Fhd), "per class");
        // one wrong claim is no pattern
        assert!(!trust(&[(Some(Uhd), Hd), (None, Hd), (None, Hd), (None, Hd)]).doubts(Uhd));
        // exactly half wrong doubts, less than half does not
        assert!(trust(&[(Some(Uhd), Fhd), (Some(Uhd), Uhd), (None, Hd)]).doubts(Uhd));
        assert!(!trust(&[(Some(Uhd), Fhd), (Some(Uhd), Uhd), (Some(Uhd), Uhd), (None, Hd)]).doubts(Uhd));
        // a claim that played better is not wrong; HD / SD claims are never doubted
        assert!(!trust(&[(Some(Fhd), Uhd), (Some(Fhd), Uhd), (None, Hd)]).doubts(Fhd));
        assert!(!trust(&[(Some(Hd), Sd), (Some(Hd), Sd), (Some(Hd), Sd)]).doubts(Hd));
        // the badge
        let b = QualityBadge::named(Uhd, true);
        assert_eq!((b.label(), QualityBadge::named(Uhd, false).label()), ("4K?", "4K"));
        assert!(b.explain().starts_with("The provider says 4K; not verified"));
        assert_eq!(QualityBadge::measured(Hd, Some(Uhd)).explain(), "Played at HD (the provider says 4K).");
    }
}
