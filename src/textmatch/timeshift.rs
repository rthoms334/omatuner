//! "+1" channels (B3; the Guide fixes design put it in `sources/timeshift.rs`). OWNER: G-core.
//!
//! The grammar is strict on purpose (a wrong rule match maps a channel to another channel's
//! guide): the marker is a `+` (or the word "plus") followed by 1–24 hours at the END of the
//! name, after trailing quality words and bracketed tags are dropped. `+` alone ("Sky Sports+",
//! "ITV4+") and digits inside a word ("5USA", "BBC1") are not markers.

/// Quality words dropped from the end of a name (the same words `identity::normalize_name`
/// drops).
const QUALITY: &[&str] = &["hd", "fhd", "uhd", "4k", "sd", "1080p", "720p", "1080i", "hevc", "h265", "h.265", "h264", "h.264", "raw"];

/// The hours a channel can lag its base by (`+1` … `+24`).
const MAX_HOURS: u32 = 24;

/// `(base name, minutes)` for a time-shifted channel name: "Channel 4 +1" → ("channel 4", 60),
/// "E4+1 HD" → ("e4", 60), "BBC One +24", "Quest Red + 1"; none for "Sky Sports+", "ITV4+",
/// "Canal+ 1", "5USA", "Dave ja vu". The base is lower-cased with its quality words dropped; compare it to
/// a channel through `identity::normalize_name`.
pub fn marker(name: &str) -> Option<(String, i32)> {
    // fast path: an ASCII name without '+' or "plus" (nearly all of them; called per channel)
    let b = name.as_bytes();
    if name.is_ascii() && !b.contains(&b'+') && !b.windows(4).any(|w| w.eq_ignore_ascii_case(b"plus")) {
        return None;
    }
    let lower = deunicode::deunicode(name).to_lowercase();
    let mut toks: Vec<&str> = lower.split_whitespace().collect();
    // trailing quality words and bracketed tags: "E4 +1 HD", "Film4 +1 (UK)", "Dave +1 [backup]"
    while let Some(last) = toks.last() {
        let bare = last.trim_matches(|c: char| matches!(c, '|' | '-' | ','));
        let bracketed = (last.starts_with('(') && last.ends_with(')')) || (last.starts_with('[') && last.ends_with(']'));
        if toks.len() > 1 && (QUALITY.contains(&bare) || bracketed || bare.is_empty()) {
            toks.pop();
        } else {
            break;
        }
    }
    let tail = toks.join(" ");
    let s = tail.strip_suffix('h').unwrap_or(&tail);
    let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || digits > 2 {
        return None;
    }
    let hours: u32 = s[s.len() - digits..].parse().ok()?;
    if hours == 0 || hours > MAX_HOURS {
        return None;
    }
    let raw = &s[..s.len() - digits];
    let rest = raw.trim_end();
    let base = if let Some(b) = rest.strip_suffix('+') {
        // a '+' glued to its word is the name's ("Canal+ 1", "Disney+ 1") unless the hours are
        // glued to it too ("E4+1"); "Quest Red + 1" stands apart
        if rest.len() != raw.len() && !b.is_empty() && !b.ends_with(char::is_whitespace) {
            return None;
        }
        b
    } else {
        // "plus 1" needs a space on both sides
        let b = rest.strip_suffix("plus")?;
        if !b.ends_with(' ') {
            return None;
        }
        b
    };
    let base = base.trim().trim_end_matches(['-', '|', ',', ':']).trim();
    if base.is_empty() || base.ends_with('+') || !base.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((base.to_owned(), (hours * 60) as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_one_names() {
        assert_eq!(marker("Channel 4 +1"), Some(("channel 4".into(), 60)));
        assert_eq!(marker("E4+1 HD"), Some(("e4".into(), 60)));
        assert_eq!(marker("Quest Red + 1"), Some(("quest red".into(), 60)));
        assert_eq!(marker("BBC One +24"), Some(("bbc one".into(), 1440)));
        assert_eq!(marker("UK: Film4 +1 (UK)"), Some(("uk: film4".into(), 60)));
        assert_eq!(marker("Dave plus 1"), Some(("dave".into(), 60)));
        assert_eq!(marker("ITV2 +2h"), Some(("itv2".into(), 120)));
        for none in [
            "Sky Sports+",
            "ITV4+",
            "5USA",
            "Dave ja vu",
            "BBC1",
            "Channel 4",
            "+1",
            "Sky Sports+ HD",
            "Channel +0",
            "News +100",
            "Daveplus 1",
            "Canal+ 1",
            "Disney+ 1",
        ] {
            assert_eq!(marker(none), None, "{none}");
        }
    }
}
