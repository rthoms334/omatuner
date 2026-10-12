//! Fixtures "A v B" in programme text (B3). OWNER: F-AB.

/// Strong separators: a fixture for sure.
const STRONG: [&str; 5] = [" vs. ", " vs ", " versus ", " v ", " @ "];
/// Dashes separate fixtures too, and many other things ("Mission: Impossible - Ghost
/// Protocol"): accepted only between two short capitalised names without digits.
const DASHES: [&str; 2] = [" - ", " – "];

/// The two sides of a fixture in `title` or `sub_title` (separators v / vs / versus / - / – /
/// @; team-suffix words such as Women, U21 are kept on the side so rules can leave them out).
/// A competition prefix ("Premier League: ") and markers ("Live:", "(R)", "- Live") are cut.
pub fn parse_fixture(title: &str, sub_title: Option<&str>) -> Option<(String, String)> {
    let texts = [Some(title), sub_title];
    for strong in [true, false] {
        for t in texts.iter().flatten() {
            if let Some(f) = split(t, strong) {
                return Some(f);
            }
        }
    }
    None
}

fn split(text: &str, strong: bool) -> Option<(String, String)> {
    let t = strip_prefix_markers(text.trim());
    let lower = t.to_ascii_lowercase();
    let seps: &[&str] = if strong { &STRONG } else { &DASHES };
    let (pos, len) = seps.iter().filter_map(|s| lower.find(s).map(|p| (p, s.len()))).min_by_key(|(p, _)| *p)?;
    let left = left_side(&t[..pos]);
    let right = right_side(&t[pos + len..]);
    let ok = |s: &str| {
        let words = s.split_whitespace().count();
        (1..=5).contains(&words) && s.chars().next().is_some_and(char::is_alphanumeric)
    };
    if !ok(left) || !ok(right) {
        return None;
    }
    if !strong {
        let name = |s: &str| {
            s.split_whitespace().count() <= 4
                && s.chars().next().is_some_and(char::is_uppercase)
                && !s.chars().any(|c| c.is_ascii_digit())
                && !s.starts_with("The ")
        };
        if !name(left) || !name(right) {
            return None;
        }
    }
    Some((left.to_owned(), right.to_owned()))
}

/// "Live: ", "LIVE ", "New: " at the start.
pub(crate) fn strip_prefix_markers(s: &str) -> &str {
    let mut t = s;
    loop {
        let before = t;
        for p in ["Live:", "LIVE:", "Live -", "Live –", "New:", "NEW:"] {
            if t.len() >= p.len() && t.is_char_boundary(p.len()) && t[..p.len()].eq_ignore_ascii_case(p) {
                t = t[p.len()..].trim_start();
            }
        }
        if let Some(rest) = t.strip_prefix("LIVE ") {
            t = rest.trim_start();
        }
        if t == before {
            return t;
        }
    }
}

/// The left side: after the competition prefix ("Premier League Football: Arsenal").
fn left_side(s: &str) -> &str {
    let cut = [":", " - ", " – ", ","].iter().filter_map(|c| s.rfind(c).map(|p| p + c.len())).max().unwrap_or(0);
    strip_prefix_markers(s[cut..].trim())
}

/// The right side: up to a trailer (" - Live", " (R)", ": Day 2", ", Premier League").
fn right_side(s: &str) -> &str {
    let cut = [":", " - ", " – ", " (", " [", ","].iter().filter_map(|c| s.find(c)).min().unwrap_or(s.len());
    let t = s[..cut].trim();
    t.strip_suffix(" Live").or_else(|| t.strip_suffix(" LIVE")).unwrap_or(t).trim_end_matches(['.', '!']).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uk_guide_fixtures() {
        let f = |a: &str, b: &str| Some((a.to_owned(), b.to_owned()));
        assert_eq!(parse_fixture("Premier League Football", Some("Arsenal v Chelsea")), f("Arsenal", "Chelsea"));
        assert_eq!(parse_fixture("Live: Arsenal vs. Chelsea", None), f("Arsenal", "Chelsea"));
        assert_eq!(parse_fixture("Arsenal: The Invincibles", None), None);
        assert_eq!(parse_fixture("Arsenal Women v Chelsea Women", None), f("Arsenal Women", "Chelsea Women"));
        assert_eq!(parse_fixture("Premier League Football: Arsenal v Chelsea (R)", None), f("Arsenal", "Chelsea"));
        assert_eq!(parse_fixture("England v Australia - Day 2", None), f("England", "Australia"));
        assert_eq!(parse_fixture("Lakers @ Celtics", None), f("Lakers", "Celtics"));
        assert_eq!(parse_fixture("Football - Premier League", Some("Spurs v Arsenal")), f("Spurs", "Arsenal"), "a strong separator first");
        assert_eq!(parse_fixture("Spurs - Arsenal", None), f("Spurs", "Arsenal"));
        for none in
            ["Mission: Impossible - Ghost Protocol 2", "Taskmaster - Series 20", "Formula 1: Japanese Grand Prix Qualifying", "Escape to the Country"]
        {
            assert_eq!(parse_fixture(none, None), None, "{none}");
        }
    }
}
