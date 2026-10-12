//! Scores and results in titles (B3). OWNER: S1.
//!
//! A hand-rolled scanner over whitespace tokens rather than one regex: the rule is about the
//! words AROUND a number ("letters, a space, the score, a space, letters"), which is what keeps
//! times, seasons, "9-1-1" and "Catch-22" out.

use std::borrow::Cow;

/// "Arsenal 2–1 Chelsea" → "Arsenal v Chelsea" (also "(aet)", "4-3 pens", "Eng 245/6 v Aus",
/// several scores); leaves times, seasons, "9-1-1", "Catch-22", "S2-E5" alone.
///
/// A run of score tokens is rewritten only when a word ending in a letter sits before it and a
/// word starting with a letter after it (each with a space): the run becomes "v", or vanishes
/// when the next word already is "v" / "vs". Once a score went, bracketed leftovers ("(aet)",
/// "(4-3 pens)") go too. Whitespace is normalised in a rewritten title.
pub fn neutralise_scores(title: &str) -> Cow<'_, str> {
    // cheap exit: nearly every title has no digit
    if !title.bytes().any(|b| b.is_ascii_digit()) {
        return Cow::Borrowed(title);
    }
    let toks = merge_spaced(title);
    let mut out: Vec<&str> = Vec::with_capacity(toks.len());
    let mut changed = false;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if !is_score(t) {
            out.push(t);
            i += 1;
            continue;
        }
        // the run: scores and their modifiers ("6-4 6-3", "4-3 pens", "2-1 (aet)")
        let mut j = i + 1;
        while j < toks.len() && (is_score(toks[j]) || is_modifier(toks[j])) {
            j += 1;
        }
        let before = out.last().is_some_and(|p| p.chars().last().is_some_and(char::is_alphabetic));
        let after = toks.get(j).is_some_and(|n| n.chars().next().is_some_and(char::is_alphabetic));
        if before && after {
            changed = true;
            if !matches!(toks[j].to_ascii_lowercase().as_str(), "v" | "vs" | "vs." | "v.") {
                out.push("v");
            }
        } else {
            out.extend_from_slice(&toks[i..j]);
        }
        i = j;
    }
    if !changed {
        return Cow::Borrowed(title);
    }
    Cow::Owned(drop_score_brackets(&out).join(" "))
}

/// Whitespace tokens of `s`, a spaced score ("2 - 1") kept as one token (a slice of `s`).
fn merge_spaced(s: &str) -> Vec<&str> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let at = |t: &str| t.as_ptr() as usize - s.as_ptr() as usize;
    let mut out: Vec<&str> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if i + 2 < toks.len() && is_number(toks[i], 3) && matches!(toks[i + 1], "-" | "–" | "—") && is_number(toks[i + 2], 3) {
            out.push(&s[at(toks[i])..at(toks[i + 2]) + toks[i + 2].len()]);
            i += 3;
        } else {
            out.push(toks[i]);
            i += 1;
        }
    }
    out
}

/// Bracketed groups that only hold a score's leftovers: "(aet)", "(pens)", "(4-3 pens)".
fn drop_score_brackets<'a>(toks: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i].starts_with('(')
            && let Some(k) = (i..toks.len()).find(|&k| toks[k].ends_with(')'))
            && toks[i..=k].iter().all(|t| {
                let bare = t.trim_matches(|c: char| matches!(c, '(' | ')'));
                is_score(bare) || is_modifier(bare)
            })
        {
            i = k + 1;
            continue;
        }
        out.push(toks[i]);
        i += 1;
    }
    out
}

fn is_number(t: &str, max: usize) -> bool {
    !t.is_empty() && t.len() <= max && t.bytes().all(|b| b.is_ascii_digit())
}

/// `N-M` / `N–M` / `N—M` / `N:M` (N, M ≤ 3 digits, not a clock time; spaces around a dash
/// allowed) or cricket `N/W`.
fn is_score(t: &str) -> bool {
    let compact: Cow<'_, str> = if t.contains(' ') { Cow::Owned(t.split_whitespace().collect()) } else { Cow::Borrowed(t) };
    let t = compact.as_ref();
    let Some((pos, sep)) = t.char_indices().find(|(_, c)| matches!(c, '-' | '–' | '—' | ':' | '/')) else { return false };
    let (n, m) = (&t[..pos], &t[pos + sep.len_utf8()..]);
    if !is_number(n, 3) {
        return false;
    }
    match sep {
        // cricket: runs / wickets
        '/' => is_number(m, 2),
        // a clock time: "20:00", "9:30" (two-digit minutes, hour ≤ 23)
        ':' if m.len() == 2 && is_number(m, 2) && n.parse::<u32>().is_ok_and(|h| h <= 23) => false,
        _ => is_number(m, 3),
    }
}

/// Words that belong to a score: "aet", "pens", "(aet)", and "(4-3 pens)" seen as "(4-3" +
/// "pens)".
fn is_modifier(t: &str) -> bool {
    let bare = t.trim_matches(|c: char| matches!(c, '(' | ')' | ',')).to_ascii_lowercase();
    matches!(bare.as_str(), "aet" | "a.e.t." | "a.e.t" | "pens" | "pen" | "penalties" | "agg") || (t.starts_with('(') && is_score(&bare))
}

/// Result verbs as whole tokens of folded text.
const RESULT_VERBS: &[&str] = &[
    "beat",
    "beats",
    "beaten",
    "win",
    "wins",
    "won",
    "draw",
    "draws",
    "drew",
    "lose",
    "loses",
    "lost",
    "thrash",
    "thrashes",
    "thrashed",
    "edge",
    "edges",
    "edged",
    "stun",
    "stuns",
    "stunned",
    "crowned",
    "defeat",
    "defeats",
    "defeated",
    "victory",
    "triumph",
    "triumphs",
    "hammer",
    "hammers",
    "hammered",
    "rout",
    "routs",
    "routed",
    "sink",
    "sinks",
    "sank",
    "eliminated",
    "relegated",
    "promoted",
];

/// Two-word results.
const RESULT_PHRASES: &[(&str, &str)] = &[("knocked", "out"), ("knock", "out"), ("knocks", "out"), ("crashes", "out"), ("crash", "out")];

/// A result verb ("beat", "wins", "thrash", "crowned", "knocked out" …) in folded text.
pub fn has_result_verb(folded: &str) -> bool {
    let words: Vec<&str> = folded.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    words.iter().any(|w| RESULT_VERBS.contains(w)) || words.windows(2).any(|p| RESULT_PHRASES.contains(&(p[0], p[1])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_scores_only() {
        for (from, to) in [
            ("Arsenal 2–1 Chelsea", "Arsenal v Chelsea"),
            ("Arsenal 2-1 Chelsea (aet)", "Arsenal v Chelsea"),
            ("Spain 4-3 pens Italy", "Spain v Italy"),
            ("Spain 1-1 Italy (4-3 pens)", "Spain v Italy"),
            ("Nadal 6-4 6-3 Federer", "Nadal v Federer"),
            ("Eng 245/6 v Aus", "Eng v Aus"),
            ("Bayern 2:1 Dortmund", "Bayern v Dortmund"),
            ("Arsenal 2 - 1 Chelsea", "Arsenal v Chelsea"),
            ("Highlights: Arsenal 3-0 Spurs", "Highlights: Arsenal v Spurs"),
        ] {
            assert_eq!(neutralise_scores(from), to, "{from}");
        }
        for same in [
            "20:00 News",
            "Live 19:30 Arsenal v Chelsea",
            "Premier League 2023-24",
            "9-1-1: Lone Star",
            "Catch-22",
            "S2-E5",
            "Mission: Impossible 2",
            "Arsenal v Chelsea",
            "Top 10 Goals",
        ] {
            assert!(matches!(neutralise_scores(same), Cow::Borrowed(t) if t == same), "{same}: unchanged, no allocation");
        }
    }

    #[test]
    fn result_verbs_are_whole_words() {
        assert!(has_result_verb("hamilton wins in monza"));
        assert!(has_result_verb("arsenal knocked out of the cup"));
        assert!(!has_result_verb("winsford united v chester"));
        assert!(!has_result_verb("arsenal v chelsea"));
    }
}
