//! `TermSet`: one Aho-Corasick automaton over many folded terms with a whole-token check
//! (B3). OWNER: F-AB.

/// Index of a term's owner (a Following rule, a spoiler-guard topic).
pub type Owner = u32;

/// The automaton; empty (matches nothing) until F-AB.
#[derive(Debug, Default)]
pub struct TermSet {
    owners: Vec<Owner>,
}

impl TermSet {
    /// Build from `(folded term, owner)` pairs.
    pub fn new(_terms: &[(String, Owner)]) -> Self {
        Self::default()
    }

    /// Owners whose terms occur in `folded` as whole tokens ("arsenal's" matches "arsenal",
    /// "arsenale" doesn't), each once.
    pub fn owners_in(&self, _folded: &str) -> Vec<Owner> {
        let _ = &self.owners;
        Vec::new()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "F-AB implements TermSet"]
    fn whole_tokens_only() {
        let set = TermSet::new(&[("arsenal".into(), 1), ("man utd".into(), 2), ("manchester united".into(), 2)]);
        assert_eq!(set.owners_in("arsenal v chelsea"), [1]);
        assert_eq!(set.owners_in("arsenal's day"), [1]);
        assert!(set.owners_in("arsenale venezia").is_empty());
        assert_eq!(set.owners_in("man utd v spurs"), [2]);
    }
}
