//! Live / new / repeat / highlights from guide flags and UK markers (B3). OWNER: F-AB.

use super::fold::{fold, has_token};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiringKind {
    Live,
    New,
    Repeat,
    Highlights,
    Unknown,
}

/// `programme.flags` bits (XMLTV `<new/>`, `<live/>`, `<previously-shown/>`).
pub const FLAG_NEW: u32 = 1;
pub const FLAG_LIVE: u32 = 1 << 1;
pub const FLAG_PREVIOUSLY_SHOWN: u32 = 1 << 2;

/// From `programme.flags` bits 0–2 and markers ("Live", "(R)", "[R]", "Repeat", "Replay",
/// "Rewind", "Highlights", "Match of the Day"). Highlights before repeat before live before
/// new: a replayed live match is a repeat, highlights of a live game are highlights.
pub fn airing_kind(title: &str, sub_title: Option<&str>, flags: u32) -> AiringKind {
    let mut buf = String::new();
    let sub = sub_title.unwrap_or("");
    let marked = |buf: &mut String, words: &[&str]| {
        [title, sub].iter().any(|t| {
            fold(t, buf);
            words.iter().any(|w| has_token(buf, w))
        })
    };
    fold(title, &mut buf);
    if buf.starts_with("match of the day") || marked(&mut buf, &["highlights", "extended highlights"]) {
        return AiringKind::Highlights;
    }
    if flags & FLAG_PREVIOUSLY_SHOWN != 0 || has_repeat_mark(title) || has_repeat_mark(sub) || marked(&mut buf, &["repeat", "replay", "rewind"]) {
        return AiringKind::Repeat;
    }
    if flags & FLAG_LIVE != 0 || live_mark(title) || live_mark(sub) {
        return AiringKind::Live;
    }
    if flags & FLAG_NEW != 0 || starts_with_word(title, "New:") {
        return AiringKind::New;
    }
    AiringKind::Unknown
}

/// "(R)" / "[R]" anywhere.
fn has_repeat_mark(s: &str) -> bool {
    s.contains("(R)") || s.contains("[R]") || s.contains("(r)")
}

/// "Live:" / "Live -" / "LIVE …" at the start, "(Live)" / "- Live" at the end. Not "Live at the
/// Apollo": a capitalised "Live" without a colon or dash is a title word.
pub(crate) fn live_mark(s: &str) -> bool {
    let t = s.trim();
    starts_with_word(t, "Live:")
        || starts_with_word(t, "Live -")
        || starts_with_word(t, "Live –")
        || t.starts_with("LIVE ")
        || t.starts_with("LIVE:")
        || t.ends_with("(Live)")
        || t.ends_with("- Live")
        || t.ends_with("- LIVE")
}

fn starts_with_word(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers() {
        assert_eq!(airing_kind("Chelsea v Arsenal (R)", None, 0), AiringKind::Repeat);
        assert_eq!(airing_kind("Live: Arsenal v Chelsea", None, 0), AiringKind::Live);
        assert_eq!(airing_kind("Match of the Day", None, 0), AiringKind::Highlights);
        assert_eq!(airing_kind("Taskmaster", None, 0), AiringKind::Unknown);
        assert_eq!(airing_kind("Premier League Football", Some("Arsenal v Chelsea"), FLAG_LIVE), AiringKind::Live);
        assert_eq!(airing_kind("Premier League Football", Some("Arsenal v Chelsea"), FLAG_LIVE | FLAG_PREVIOUSLY_SHOWN), AiringKind::Repeat);
        assert_eq!(airing_kind("Premier League Highlights", None, FLAG_LIVE), AiringKind::Highlights);
        assert_eq!(airing_kind("Live at the Apollo", None, 0), AiringKind::Unknown);
        assert_eq!(airing_kind("LIVE Formula 1", None, 0), AiringKind::Live);
        assert_eq!(airing_kind("Taskmaster", Some("Replay: Hot Dogs"), 0), AiringKind::Repeat);
        assert_eq!(airing_kind("New: Taskmaster", None, 0), AiringKind::New);
        assert_eq!(airing_kind("Taskmaster", None, FLAG_NEW), AiringKind::New);
    }
}
