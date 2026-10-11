//! Events → channel choice → caps → clashes → reconcile against the ledger (pure; DESIGN §33
//! §5.4). OWNER: F-AB.

use crate::model::{FollowRuleId, ItemRef, Timestamp};

/// The state of one ledger row (`follow_match.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchState {
    Scheduled,
    Adopted,
    Listed,
    OverCap,
    Skipped,
    Gone,
    Aired,
}

/// One matched airing (preview rows and the Upcoming list).
#[derive(Debug, Clone, PartialEq)]
pub struct Upcoming {
    pub rule: FollowRuleId,
    pub event_key: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub channel: Option<ItemRef>,
    pub alternates: Vec<ItemRef>,
    pub state: MatchState,
    /// `clash:<rule>` | `locked` | `no-interrupt` | a left-out reason in previews.
    pub note: Option<String>,
}

/// A preview run's answer ("In the guide now · 4 match · 2 left out").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Preview {
    pub matches: Vec<Upcoming>,
    pub left_out: Vec<Upcoming>,
    /// "'England' also matches rugby and cricket".
    pub warnings: Vec<String>,
}

/// What an evaluation writes (one transaction, `store::follow`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub create: Vec<Upcoming>,
    pub moved: Vec<Upcoming>,
    pub gone: Vec<i64>,
}
