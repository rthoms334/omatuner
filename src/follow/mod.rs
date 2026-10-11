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

pub use spec::{Airings, ChannelScope, FollowKind, FollowMode, FollowSpec, Prefer, Shape};

use crate::model::{FollowRuleId, Timestamp};

/// The owning track flips this in its last commit; until then nothing evaluates, previews or
/// shows (jobs exit at once, the UI hides its entries, MCP answers `unsupported`).
pub const ENABLED: bool = false;

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
    /// Made by Claude (`follow add`, marked in Settings).
    pub by_agent: bool,
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

/// What the app hears from Following's services.
#[derive(Debug, Clone, PartialEq)]
pub enum FollowEvent {
    /// The editor's / sheet's "In the guide now" preview (newest `rev` wins).
    PreviewReady { rev: u64, preview: plan::Preview },
}

/// The folded terms of spoiler-safe rules, as virtual spoiler-guard topics ("until seen",
/// Sport scope for Team / Event rules; B4). Pure.
pub fn spoiler_terms(rules: &[FollowRule]) -> Vec<crate::guard::GuardTerm> {
    rules
        .iter()
        .filter(|r| r.enabled && r.spoiler_safe)
        .map(|r| crate::guard::GuardTerm {
            label: r.name.clone(),
            terms: r.spec.terms.clone(),
            sport_only: matches!(r.kind, FollowKind::Team | FollowKind::Event),
        })
        .collect()
}
