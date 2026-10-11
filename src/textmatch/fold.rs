//! Folding for matching (B3). OWNER: F-AB.

/// Fold `s` into `out` (cleared first): lower case, ASCII fast path, `deunicode` for bytes ≥
/// 0x80 (as `sources::identity::normalize_name`), "&" → "and", "vs" / "vs." → "v",
/// punctuation → single spaces. Placeholder until F-AB: lower case only.
pub fn fold(s: &str, out: &mut String) {
    out.clear();
    out.extend(s.chars().flat_map(char::to_lowercase));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "F-AB implements fold"]
    fn folds_like_the_guide_writes() {
        let f = |s: &str| {
            let mut o = String::new();
            fold(s, &mut o);
            o
        };
        assert_eq!(f("Arsenal vs. Chelsea"), "arsenal v chelsea");
        assert_eq!(f("Brighton & Hove Albion"), "brighton and hove albion");
        assert_eq!(f("Atlético Madrid"), "atletico madrid");
        assert_eq!(f("ＡＲＳＥＮＡＬ"), "arsenal");
        assert_eq!(f("Live: Arsenal v Chelsea"), "live arsenal v chelsea");
    }
}
