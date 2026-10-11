//! Short playlist codes (DESIGN §13.0 "Playlist codes"): the 1–3 letters or digits that say on a
//! card, a row or a search hit which playlist an item comes from ("NW" for Northwind). The user
//! may pick one per playlist (`SourceSettings::code`); otherwise it is derived from the name.
//! Every playlist's code is unique among the user's playlists: the library build resolves them
//! all at once ([`assign`]), so they are computed once per library generation.

/// Longest code, in characters.
pub const MAX_LEN: usize = 3;

/// A code the user typed: trimmed and upper-cased; `Ok(None)` when empty (= derive it from the
/// name); an error for anything but 1–3 letters or digits.
pub fn parse(s: &str) -> Result<Option<String>, &'static str> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if !s.chars().all(char::is_alphanumeric) {
        return Err("Use only letters and digits");
    }
    let code = upper(s.chars());
    if code.chars().count() > MAX_LEN { Err("Up to 3 letters or digits") } else { Ok(Some(code)) }
}

/// Upper-cased, one character per input character (ß → S), so the length stays put.
fn upper(chars: impl Iterator<Item = char>) -> String {
    chars.map(|c| c.to_uppercase().next().unwrap_or(c)).collect()
}

/// The words of a name: runs of letters or digits, also split where a lower-case letter meets an
/// upper-case one ("NorthWind") and where letters meet digits ("TV2").
fn words(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev = None;
            continue;
        }
        let split = prev.is_some_and(|p| (p.is_lowercase() && c.is_uppercase()) || (p.is_numeric() != c.is_numeric()));
        if split && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_vowel(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

/// Where a one-word name made of two words splits ("Northwind" → 5: "North" + "wind"): at a
/// run of three or more consonants inside the word, before its last consonant, or before its
/// last two when they belong together ("Sunshine" → "Sun" + "shine").
fn compound_split(word: &[char]) -> Option<usize> {
    let consonant = |c: char| c.is_alphabetic() && !is_vowel(c);
    let mut i = 1;
    while i < word.len() {
        if !consonant(word[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < word.len() && consonant(word[i]) {
            i += 1;
        }
        // inside the word (a vowel follows), not its first letters
        if i - start >= 3 && i < word.len() && start > 0 {
            let pair: String = word[i - 2..i].iter().collect::<String>().to_lowercase();
            let at = if matches!(pair.as_str(), "th" | "sh" | "ch" | "ph" | "wh") { i - 2 } else { i - 1 };
            if at >= 2 && word.len() - at >= 2 {
                return Some(at);
            }
        }
    }
    None
}

/// The code a name suggests, before uniqueness: the whole name when it has 3 letters and digits
/// or fewer ("TV", "TV 2" → "TV2"); else the initials of its words (digits whole: "Channel 44" →
/// "C44"); a single word gives the initials of its two halves ("Northwind" → "NW") or its first
/// two letters ("Aurora" → "AU"). "P" for a name without letters or digits.
pub fn auto(name: &str) -> String {
    let alnum: Vec<char> = name.chars().filter(|c| c.is_alphanumeric()).collect();
    if alnum.is_empty() {
        return "P".to_owned();
    }
    if alnum.len() <= MAX_LEN {
        return upper(alnum.into_iter());
    }
    let ws = words(name);
    let code: String = if ws.len() >= 2 {
        let parts = ws.iter().map(|w| if w.chars().all(char::is_numeric) { w.clone() } else { w.chars().take(1).collect() });
        upper(parts.collect::<String>().chars().take(MAX_LEN))
    } else {
        let w: Vec<char> = alnum;
        match compound_split(&w) {
            Some(at) => upper([w[0], w[at]].into_iter()),
            None => upper(w.into_iter().take(2)),
        }
    };
    code
}

/// `base`, or the first variant of it nobody has: its first two characters with a digit
/// ("TV" → "TV2", "TV3", …), then with a letter, then its first character with a number.
fn unique(base: &str, taken: &impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    let stem: String = base.chars().take(MAX_LEN - 1).collect();
    let first: String = base.chars().take(1).collect();
    let candidates = ('2'..='9')
        .chain('A'..='Z')
        .map(|c| format!("{stem}{c}"))
        .chain((10..100).map(|n| format!("{first}{n}")))
        .chain((100..1000).map(|n| n.to_string()));
    for c in candidates {
        if !taken(&c) {
            return c;
        }
    }
    base.to_owned()
}

/// The code of every playlist, in the order given (`(name, user code)`, playlist order): the
/// user's codes first (a later duplicate of one gets a variant), then derived codes for the
/// rest in order, each the first free variant of [`auto`]. Unique and deterministic.
pub fn assign<'a>(playlists: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> Vec<String> {
    let list: Vec<(&str, Option<String>)> = playlists.into_iter().map(|(n, c)| (n, c.and_then(|c| parse(c).ok().flatten()))).collect();
    let mut out: Vec<Option<String>> = vec![None; list.len()];
    let mut taken: Vec<String> = Vec::with_capacity(list.len());
    for (i, (_, user)) in list.iter().enumerate() {
        if let Some(u) = user {
            let code = unique(u, &|c: &str| taken.iter().any(|t| t == c));
            taken.push(code.clone());
            out[i] = Some(code);
        }
    }
    for (i, (name, _)) in list.iter().enumerate() {
        if out[i].is_none() {
            let code = unique(&auto(name), &|c: &str| taken.iter().any(|t| t == c));
            taken.push(code.clone());
            out[i] = Some(code);
        }
    }
    out.into_iter().map(Option::unwrap_or_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_suggest_readable_codes() {
        for (name, code) in [
            ("TV", "TV"),
            ("TV 2", "TV2"),
            ("tv2", "TV2"),
            ("Northwind", "NW"),
            ("NorthWind", "NW"),
            ("Aurora", "AU"),
            ("Harbor Free", "HF"),
            ("Sunshine", "SS"),
            ("Channel 44", "C44"),
            ("The Movie Box", "TMB"),
            ("Ä", "Ä"),
            ("★★", "P"),
        ] {
            assert_eq!(auto(name), code, "{name}");
        }
    }

    #[test]
    fn codes_are_unique_and_users_win() {
        let names = |v: &[(&'static str, Option<&'static str>)]| assign(v.iter().copied());
        assert_eq!(names(&[("TV", None), ("TV 2", None), ("TV", None)]), ["TV", "TV2", "TV3"]);
        // a user's code is kept even when an earlier playlist would derive it
        assert_eq!(names(&[("Northwind", None), ("Nightwatch", Some("nw"))]), ["NW2", "NW"]);
        // two users chose the same: the later one gets a variant
        assert_eq!(names(&[("A", Some("X")), ("B", Some("x"))]), ["X", "X2"]);
        // an invalid stored code falls back to the name's
        assert_eq!(names(&[("Aurora", Some("toolong"))]), ["AU"]);
        let many: Vec<(&str, Option<&str>)> = (0..60).map(|_| ("Aurora", None)).collect();
        let codes = assign(many);
        let mut dedup = codes.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), 60, "{codes:?}");
        assert!(codes.iter().all(|c| (1..=MAX_LEN).contains(&c.chars().count())));
        assert_eq!(assign(vec![("Aurora", None), ("Aurora", None)]), assign(vec![("Aurora", None), ("Aurora", None)]), "deterministic");
    }

    #[test]
    fn typed_codes_are_checked() {
        assert_eq!(parse("  "), Ok(None));
        assert_eq!(parse(" nw "), Ok(Some("NW".into())));
        assert_eq!(parse("tv2"), Ok(Some("TV2".into())));
        assert!(parse("ABCD").is_err());
        assert!(parse("A-B").is_err());
    }
}
