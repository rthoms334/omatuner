//! Scores and results in titles (B3). OWNER: S1.

use std::borrow::Cow;

/// "Arsenal 2–1 Chelsea" → "Arsenal v Chelsea" (also "(aet)", "4-3 pens", "Eng 245/6 v Aus",
/// several scores); leaves times, seasons, "9-1-1", "Catch-22", "S2-E5" alone. Placeholder
/// until S1: unchanged.
pub fn neutralise_scores(title: &str) -> Cow<'_, str> {
    Cow::Borrowed(title)
}

/// A result verb ("beat", "wins", "thrash", "crowned", …) in folded text.
pub fn has_result_verb(_folded: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "S1 implements neutralise_scores"]
    fn rewrites_scores_only() {
        for (from, to) in [
            ("Arsenal 2–1 Chelsea", "Arsenal v Chelsea"),
            ("Arsenal 2-1 Chelsea (aet)", "Arsenal v Chelsea"),
            ("Spain 4-3 pens Italy", "Spain v Italy"),
            ("Nadal 6-4 6-3 Federer", "Nadal v Federer"),
        ] {
            assert_eq!(neutralise_scores(from), to, "{from}");
        }
        for same in ["20:00 News", "Premier League 2023-24", "9-1-1: Lone Star", "Catch-22", "S2-E5", "Mission: Impossible 2"] {
            assert_eq!(neutralise_scores(same), same);
        }
    }
}
