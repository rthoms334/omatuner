//! "Follow this…" suggestions from one programme (pure; DESIGN §33 §3.5). OWNER: F-AB.

use crate::follow::{FollowKind, FollowSpec};

/// One line of the sheet ("Arsenal: every match · Team").
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub kind: FollowKind,
    pub spec: FollowSpec,
}

/// Suggestions for a programme: one Team per fixture side, an Event for a competition prefix,
/// "New episodes of X" for S/E data, a News flash for a news bulletin, "Every airing of X"
/// always.
pub fn suggest(_title: &str, _sub_title: Option<&str>, _categories: &[String], _has_episode: bool) -> Vec<Suggestion> {
    Vec::new()
}
