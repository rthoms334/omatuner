//! Rule matching over guide rows (DESIGN §33 §5.4), on top of `textmatch` (fold, terms,
//! fixture, airing, event key). OWNER: F-AB.

use crate::follow::FollowRule;
use crate::model::FollowRuleId;

/// One guide row as the scan reads it (borrowed text, folded by the matcher).
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    pub title: &'a str,
    pub sub_title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub categories: Option<&'a str>,
    /// `ProgrammeFlags` bits.
    pub flags: u32,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

/// The matcher over every enabled rule's terms (one automaton).
#[derive(Debug, Default)]
pub struct Matcher {
    _terms: crate::textmatch::terms::TermSet,
}

impl Matcher {
    pub fn new(_rules: &[FollowRule]) -> Self {
        Self::default()
    }

    /// The rules this row matches (shape, without, fields, airings applied).
    pub fn matches(&self, _row: &Row<'_>, _buf: &mut String) -> Vec<FollowRuleId> {
        Vec::new()
    }
}
