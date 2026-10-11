//! Classification (pure over the snapshot; < 100 ms for 79k channels; DESIGN §31 §4.3).
//! OWNER: G-core.

use crate::model::{ChannelId, FixChange, IssueKind};
use crate::store::library::Library;

/// One row of the issue list.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueRow {
    pub channel: ChannelId,
    pub kind: IssueKind,
    /// The proposed fix (`None`: search by hand).
    pub proposal: Option<FixChange>,
    /// 0–1; ✓ = 1.0 (rule-derived).
    pub confidence: f32,
    /// A fixed phrase ("+1 of E4", "same channel in TV"), never a URL.
    pub why: String,
}

impl IssueRow {
    /// A one-key fix (✓ or ≥ 95 %).
    pub fn one_key(&self) -> bool {
        self.proposal.is_some() && self.confidence >= 0.95
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IssueReport {
    pub coverage: Vec<super::SourceCoverage>,
    pub rows: Vec<IssueRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClassifyOpts {
    pub include_hidden: bool,
}

/// Classify every shown channel. Placeholder until G-core: coverage only, no rows.
pub fn classify(lib: &Library, _opts: ClassifyOpts) -> IssueReport {
    IssueReport { coverage: super::coverage(lib), rows: Vec::new() }
}
