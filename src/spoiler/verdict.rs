//! Judging a subject against the snapshot (time enters only as a binary search over a topic's
//! windows). The public types are `guard::{Subject, Verdict}` (B4). OWNER: S1.

use crate::guard::{Subject, Verdict};
use crate::model::Timestamp;
use crate::spoiler::snapshot::GuardSnapshot;

/// The verdict on `subj` at `now`. Placeholder until S1: nothing guarded.
pub fn judge(_snap: &GuardSnapshot, _subj: &Subject<'_>, _now: Timestamp) -> Verdict {
    Verdict::default()
}
