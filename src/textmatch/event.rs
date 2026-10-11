//! Event identity (B3): one airing's key, channel-agnostic, shared by Following's ledger
//! (`follow_match.event_key`) and the spoiler guard (`spoiler_seen.event_key`). OWNER: F-AB.

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

/// Fixture: sorted teams + local date. Show: series + `sNNeNN`, else the episode title, else a
/// hash of the description's first sentence. Otherwise: the core title (LIVE / (R) / +1 /
/// quality stripped). Placeholder until F-AB: the lower-cased title and date.
pub fn event_key(p: &EventInput<'_>) -> String {
    format!("{}@{}", p.title.trim().to_lowercase(), p.local_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(title: &'a str, sub: Option<&'a str>, se: Option<(i32, i32)>) -> EventInput<'a> {
        EventInput { title, sub_title: sub, description: None, season: se.map(|s| s.0), episode: se.map(|s| s.1), local_date: "2026-10-18" }
    }

    #[test]
    #[ignore = "F-AB implements event_key"]
    fn one_key_per_event() {
        // the same fixture either way round, live or repeat, on any channel
        assert_eq!(event_key(&input("Arsenal v Chelsea", None, None)), event_key(&input("LIVE: Chelsea v Arsenal", None, None)));
        assert_eq!(event_key(&input("Taskmaster", None, Some((20, 4)))), "taskmaster#s20e04");
        assert_ne!(event_key(&input("Taskmaster", None, Some((20, 4)))), event_key(&input("Taskmaster", None, Some((20, 5)))));
    }
}
