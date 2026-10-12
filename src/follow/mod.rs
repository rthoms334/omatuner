//! Following: standing guide searches that set reminders for you (DESIGN §33), and the news
//! flash preset (big-features additions). Batch base wrote the contract; OWNERS:
//! * F-AB (engine): `spec.rs`, `matcher.rs`, `plan.rs`, `suggest.rs`, `flash.rs`,
//!   `store::follow`, `jobs::follow`, migration 0011;
//! * F-C (UI): `follow/ui.rs` (new), `ui/screens/settings/following.rs`,
//!   `ui/overlays/follow_sheet.rs`.
//!
//! Following adds no firing mechanism: it writes ordinary reminders with `rule_id` (B6), routed
//! at fire time by `reminders::fire_policy` (a rule's switch never interrupts a film or episode
//! unless `follow.interrupt_vod`; news flash neither). Matching lives in `textmatch` (B3); a
//! spoiler-safe rule is a spoiler-guard topic by data (B4).

pub mod flash;
pub mod matcher;
pub mod plan;
pub mod preview;
pub mod spec;
pub mod suggest;
pub mod ui;

pub use spec::{Airings, ChannelScope, FollowKind, FollowMode, FollowSpec, Prefer, Shape};

use crate::model::{FollowRuleId, Timestamp};

/// The owning track flips this in its last commit; until then nothing evaluates, previews or
/// shows (jobs exit at once, the UI hides its entries, MCP answers `unsupported`).
pub const ENABLED: bool = true;

/// One rule (`follow_rule` row).
#[derive(Debug, Clone, PartialEq)]
pub struct FollowRule {
    pub id: FollowRuleId,
    pub name: String,
    pub kind: FollowKind,
    pub spec: FollowSpec,
    pub mode: FollowMode,
    pub lead_s: i64,
    pub offset_s: i64,
    pub spoiler_safe: bool,
    pub max_per_week: u32,
    /// List order; lower wins a clash.
    pub priority: i64,
    pub enabled: bool,
    /// Made by an agent (`follow add`, marked in Settings).
    pub by_agent: bool,
    /// Which agent, when `by_agent` ("added by Codex"; unknown = "your agent").
    pub agent: crate::model::AgentName,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Why an evaluation runs (`JobKind::EvaluateFollows`); all coalesce into one job ("follows").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowReason {
    /// A guide import or rematch finished (B7; also headless `tuner --refresh`).
    Guide,
    /// Guide fixes changed a mapping (B7).
    Mapping,
    /// A new library (visibility, favourites, locks, playlists).
    Library,
    /// A rule was saved, deleted or reordered (app or `follow add`).
    Rules,
    /// The daily maintenance tick (re-arm, mark aired).
    Daily,
}

impl FollowRule {
    /// A new, unsaved rule from `kind`'s template (`id` 0, last in the list): spoiler-safe for
    /// Team and Event rules when `follow.spoiler_safe_default` (one key with the spoiler
    /// guard's "Sport I follow", B4); a news flash pops up (mode Flash) with the configured
    /// headlines length; a show reminds, an event or team reminds 2 min before.
    pub fn draft(name: impl Into<String>, kind: FollowKind, terms: Vec<String>, cfg: &crate::config::FollowConfig, now: Timestamp) -> Self {
        Self {
            id: FollowRuleId(0),
            name: name.into(),
            kind,
            spec: FollowSpec::template(kind, terms, cfg.news_flash.headlines_min),
            mode: if kind == FollowKind::News { FollowMode::Flash } else { FollowMode::Notify },
            lead_s: if kind == FollowKind::News { 0 } else { 120 },
            offset_s: 0,
            spoiler_safe: cfg.spoiler_safe_default && matches!(kind, FollowKind::Team | FollowKind::Event),
            max_per_week: if kind == FollowKind::News { 14 } else { 10 },
            priority: i64::MAX,
            enabled: true,
            by_agent: false,
            agent: Default::default(),
            created_at: now,
            updated_at: now,
        }
    }

    /// A switch (or a news flash, which plays by itself): routed at fire time (B1/B6).
    pub fn switches(&self) -> bool {
        matches!(self.mode, FollowMode::Switch | FollowMode::Flash)
    }

    /// What passive surfaces (notifications, the summary, Upcoming, toasts, MCP) may say about
    /// one of its matches: the rule name for a spoiler-safe rule, else the programme title.
    pub fn safe_title<'a>(&'a self, title: &'a str) -> &'a str {
        if self.spoiler_safe { &self.name } else { title }
    }
}

/// What the app hears from Following's services.
#[derive(Debug, Clone, PartialEq)]
pub enum FollowEvent {
    /// The editor's / sheet's "In the guide now" preview (newest `rev` wins).
    PreviewReady { rev: u64, preview: plan::Preview },
    /// New matches to announce in the app (no notification server answered): "Following: 3
    /// new" with up to two lines ("Arsenal · Sat 12:30 · Sky Sports Main Event") and "+1
    /// more"; the toast's Open goes to Settings ▸ Following. Spoiler-safe already.
    Summary { title: String, lines: Vec<String> },
    /// The user picked Open on the summary's desktop notification (the window was raised
    /// already): show Settings ▸ Following.
    Open,
}

/// The folded terms of spoiler-safe rules, as virtual spoiler-guard topics ("until seen",
/// Sport scope for Team / Event rules; B4). Pure.
pub fn spoiler_terms(rules: &[FollowRule]) -> Vec<crate::guard::GuardTerm> {
    rules
        .iter()
        .filter(|r| r.enabled && r.spoiler_safe)
        .map(|r| crate::guard::GuardTerm {
            label: r.name.clone(),
            terms: r.spec.terms.iter().map(|t| crate::textmatch::fold::folded(t)).filter(|t| !t.is_empty()).collect(),
            sport_only: matches!(r.kind, FollowKind::Team | FollowKind::Event),
        })
        .collect()
}
