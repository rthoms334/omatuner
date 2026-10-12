//! Event identity (B3): one airing's key, channel-agnostic, shared by Following's ledger
//! (`follow_match.event_key`) and the spoiler guard (`spoiler_seen.event_key`). OWNER: F-AB.

use super::fold::fold;

/// What [`event_key`] needs of a programme.
#[derive(Debug, Clone, Copy)]
pub struct EventInput<'a> {
    pub title: &'a str,
    pub sub_title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// Local date of the airing (`YYYY-MM-DD`).
    pub local_date: &'a str,
}

/// * Fixture: the sorted teams + the local date (`arsenal|chelsea@2026-10-18`), the same for
///   "Chelsea v Arsenal (R)" on any channel.
/// * Show with season / episode: the core title + `sNNeNN` (`taskmaster#s20e04`), no date: a
///   repeat next month is the same episode.
/// * An episode title: the core title + the folded episode title (`taskmaster#hot dogs`).
/// * Otherwise: the core title + the local date.
///
/// The design's description-hash fallback for shows is left out: generic descriptions ("Live
/// coverage of the match") would make every week's game one event. A show without season,
/// episode or episode title gets one key per day; a repeat on another day then counts as new
/// unless the guide marks it (Following's "new only" may remind twice — the user can skip).
pub fn event_key(p: &EventInput<'_>) -> String {
    if let Some((a, b)) = super::fixture::parse_fixture(p.title, p.sub_title) {
        let (mut a, mut b) = (side_key(&a), side_key(&b));
        if b < a {
            std::mem::swap(&mut a, &mut b);
        }
        return format!("{a}|{b}@{}", p.local_date);
    }
    let core = core_title(p.title);
    match (p.season, p.episode, p.sub_title.map(str::trim).filter(|s| !s.is_empty())) {
        (s, Some(e), _) => format!("{core}#s{:02}e{e:02}", s.unwrap_or(0)),
        (_, None, Some(sub)) => {
            let mut s = String::new();
            fold(sub, &mut s);
            format!("{core}#{s}")
        }
        _ => format!("{core}@{}", p.local_date),
    }
}

/// A fixture side as a key: folded, a trailing "live" dropped.
fn side_key(s: &str) -> String {
    let mut k = String::new();
    fold(s, &mut k);
    k
}

/// The folded title without markers: "LIVE: ", "New: ", "(R)", "[R]", "(Live)", "- Live", a
/// trailing quality word ("HD", "4K") or "+1".
pub fn core_title(title: &str) -> String {
    let mut t = super::fixture::strip_prefix_markers(title.trim());
    loop {
        let before = t;
        for suffix in ["(R)", "[R]", "(r)", "(Live)", "- Live", "- LIVE", "+1", "HD", "FHD", "UHD", "4K", "SD"] {
            if let Some(rest) = t.strip_suffix(suffix)
                && (suffix.starts_with(['(', '[', '-', '+']) || rest.ends_with(' '))
            {
                t = rest.trim_end();
            }
        }
        if t == before {
            break;
        }
    }
    let mut k = String::new();
    fold(t, &mut k);
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(title: &'a str, sub: Option<&'a str>, se: Option<(i32, i32)>) -> EventInput<'a> {
        EventInput { title, sub_title: sub, description: None, season: se.map(|s| s.0), episode: se.map(|s| s.1), local_date: "2026-10-18" }
    }

    #[test]
    fn one_key_per_event() {
        // the same fixture either way round, live or repeat, on any channel
        assert_eq!(event_key(&input("Arsenal v Chelsea", None, None)), event_key(&input("LIVE: Chelsea v Arsenal", None, None)));
        assert_eq!(event_key(&input("Arsenal v Chelsea", None, None)), event_key(&input("Premier League", Some("Chelsea v Arsenal (R)"), None)));
        assert_eq!(event_key(&input("Arsenal v Chelsea", None, None)), "arsenal|chelsea@2026-10-18");
        assert_eq!(event_key(&input("Taskmaster", None, Some((20, 4)))), "taskmaster#s20e04");
        assert_eq!(event_key(&input("New: Taskmaster (R)", None, Some((20, 4)))), "taskmaster#s20e04");
        assert_ne!(event_key(&input("Taskmaster", None, Some((20, 4)))), event_key(&input("Taskmaster", None, Some((20, 5)))));
        assert_eq!(event_key(&input("Taskmaster", Some("Hot Dogs"), None)), "taskmaster#hot dogs");
        assert_eq!(event_key(&input("BBC News at One HD", None, None)), "bbc news at one@2026-10-18");
    }
}
