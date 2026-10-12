//! `TermSet`: one Aho-Corasick automaton over many folded terms with a whole-token check
//! (B3). OWNER: F-AB.

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};

/// Index of a term's owner (a Following rule, a spoiler-guard topic).
pub type Owner = u32;

/// The automaton over every owner's terms (empty: matches nothing).
#[derive(Debug, Default)]
pub struct TermSet {
    ac: Option<AhoCorasick>,
    /// Owners per pattern (a term two rules share is one pattern).
    owners: Vec<Vec<Owner>>,
}

impl TermSet {
    /// Build from `(term, owner)` pairs; terms are folded (again) here, empty ones dropped.
    pub fn new(terms: &[(String, Owner)]) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        let mut owners: Vec<Vec<Owner>> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut buf = String::new();
        for (term, owner) in terms {
            super::fold::fold(term, &mut buf);
            if buf.is_empty() {
                continue;
            }
            let i = *index.entry(buf.clone()).or_insert_with(|| {
                patterns.push(buf.clone());
                owners.push(Vec::new());
                patterns.len() - 1
            });
            if !owners[i].contains(owner) {
                owners[i].push(*owner);
            }
        }
        if patterns.is_empty() {
            return Self::default();
        }
        // Standard semantics so overlapping matches are all seen ("man" and "man utd"): the
        // whole-token check then picks the valid ones. A DFA: the set is built once per
        // evaluation and scanned over every guide row.
        let ac = AhoCorasick::builder().match_kind(MatchKind::Standard).kind(Some(AhoCorasickKind::DFA)).build(&patterns).ok();
        Self { ac, owners }
    }

    /// Owners whose terms occur in `folded` as whole tokens ("arsenal's" matches "arsenal",
    /// "arsenale" doesn't), each once, ascending.
    pub fn owners_in(&self, folded: &str) -> Vec<Owner> {
        let mut out = Vec::new();
        self.owners_into(folded, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// [`TermSet::owners_in`] appending to `out` (unsorted, may repeat): the scan's hot path
    /// reuses one buffer.
    pub fn owners_into(&self, folded: &str, out: &mut Vec<Owner>) {
        let Some(ac) = &self.ac else { return };
        let h = folded.as_bytes();
        for m in ac.find_overlapping_iter(folded) {
            let (s, e) = (m.start(), m.end());
            if (s == 0 || h[s - 1] == b' ') && (e == h.len() || h[e] == b' ') {
                out.extend_from_slice(&self.owners[m.pattern().as_usize()]);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_tokens_only() {
        let set = TermSet::new(&[("arsenal".into(), 1), ("man utd".into(), 2), ("manchester united".into(), 2), ("Man".into(), 3)]);
        assert_eq!(set.owners_in("arsenal v chelsea"), [1]);
        assert_eq!(set.owners_in("arsenal s day"), [1]);
        assert!(set.owners_in("arsenale venezia").is_empty());
        assert_eq!(set.owners_in("man utd v spurs"), [2, 3], "overlapping terms both count");
        assert!(TermSet::new(&[(" ".into(), 1)]).is_empty());
    }
}
