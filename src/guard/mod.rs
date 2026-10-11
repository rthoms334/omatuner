//! The guard text helper (B4): every place that shows programme or episode text outside a
//! plain row asks it what to show. Batch base: the API with an IDENTITY implementation
//! (nothing is ever hidden); S1 implements it over the spoiler snapshot (`spoiler::snapshot`).
//! OWNER: S1.
//!
//! | Consumer | Policy |
//! |---|---|
//! | UI painters (`ui/widgets/spoiler.rs`, base-B call sites) | Normal; a revealed item uses Reveal |
//! | `notify.rs`, MPRIS `xesam:title`, switch / reminder toasts | Normal |
//! | MCP `guard_reply` (every read result) | Normal; `spoiler_safe` → SpoilerSafe; `reveal_spoilers` → Reveal |
//! | Following summary, toasts, Upcoming, MCP for spoiler-safe rules | SpoilerSafe |
//! | Up next rows and card, multi-view strips | Normal |
//!
//! Never consulted for parental decisions: the guard never unlocks or reveals explicit content.

use std::borrow::Cow;
use std::sync::Arc;

use crate::model::{ChannelId, Timestamp};

/// The S1 track flips this when the guard really guards.
pub const ENABLED: bool = false;

/// Which text of a subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Title,
    SubTitle,
    Description,
    Plot,
    EpisodeTitle,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Policy {
    /// What the guard's rules say.
    #[default]
    Normal,
    /// Strip sub-titles, descriptions and plots, neutralise scores — for everything (MCP
    /// `spoiler_safe`, spoiler-safe Following rules).
    SpoilerSafe,
    /// The user asked to see it (Ctrl+B on the item, MCP `reveal_spoilers` for that ref).
    Reveal,
}

/// Why something is hidden (tooltips, the description block, MCP `spoiler` annotations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    /// The rule's label ("Arsenal", "Severance").
    pub rule: String,
    pub kind: ReasonKind,
    /// When it lifts by itself (`None`: when watched / never).
    pub until: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonKind {
    Embargo,
    Series,
    Blackout,
    /// `Policy::SpoilerSafe`.
    SpoilerSafe,
}

/// What to show for one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guarded<'a> {
    /// As is.
    Shown(Cow<'a, str>),
    /// Rewritten ("Arsenal 2–1 Chelsea" → "Arsenal v Chelsea") — show with the dim glyph.
    Rewritten(Cow<'a, str>),
    /// Not shown (a fixed-width bar / "Description hidden").
    Hidden { reason: Option<Reason> },
}

impl<'a> Guarded<'a> {
    /// The text to show, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Guarded::Shown(t) | Guarded::Rewritten(t) => Some(t),
            Guarded::Hidden { .. } => None,
        }
    }
}

/// What the text belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject<'a> {
    /// A guide programme (title + categories + S/E + its start decide).
    Programme { title: &'a str, sub_title: Option<&'a str>, categories: &'a [String], season_episode: Option<(i32, i32)>, start: Timestamp },
    /// A movie or series of the library (`series_norm` for series rules).
    Vod { title: &'a str, series_norm: Option<&'a str> },
    /// An episode of a series.
    Episode { series_norm: &'a str, season: i32, episode: i32 },
    /// Text going outside the window (notifications, MPRIS).
    Notification { title: &'a str },
}

/// The verdict on a subject (all fields at once; painters use [`Guard::text`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Verdict {
    pub hide_sub: bool,
    pub hide_descr: bool,
    pub obscure_image: bool,
    /// Rewrite scores in the title.
    pub neutralise_title: bool,
    pub reason: Option<Reason>,
}

impl Verdict {
    pub fn guarded(&self) -> bool {
        self.hide_sub || self.hide_descr || self.obscure_image || self.neutralise_title
    }
}

/// A topic term set the guard treats as a rule (Following's spoiler-safe rules, B4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardTerm {
    pub label: String,
    pub terms: Vec<String>,
    /// Scope: guide category Sport only (Team and Event rules).
    pub sport_only: bool,
}

/// The guard, cheap to clone (shared through `AppShared`, the reminder scheduler, MPRIS and
/// the MCP handler). Identity until S1: everything is `Shown`.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    _snapshot: Arc<()>,
}

impl Guard {
    /// What to show for `f` of `subj`.
    pub fn text<'a>(&self, _subj: &Subject<'_>, _f: Field, text: &'a str, _p: Policy, _now: Timestamp) -> Guarded<'a> {
        Guarded::Shown(Cow::Borrowed(text))
    }

    /// The verdict on a subject now.
    pub fn judge(&self, _subj: &Subject<'_>, _now: Timestamp) -> Verdict {
        Verdict::default()
    }

    /// A title that leaves the window (notification summary, MPRIS `xesam:title`, a switch
    /// toast): rewritten when guarded, "Reminder" when it must not show at all.
    pub fn outside_title<'a>(&self, title: &'a str, now: Timestamp) -> Cow<'a, str> {
        match self.text(&Subject::Notification { title }, Field::Title, title, Policy::Normal, now) {
            Guarded::Shown(t) | Guarded::Rewritten(t) => t,
            Guarded::Hidden { .. } => Cow::Borrowed("Reminder"),
        }
    }

    /// The zap shield (B1: `UserZap` only): `Some` when tuning `channel` now could show a
    /// guarded result (its programme text mentions an embargoed topic, or a blackout covers
    /// it).
    pub fn shield(&self, _channel: ChannelId, _now: Timestamp) -> Option<Reason> {
        None
    }
}
