//! Live / new / repeat / highlights from guide flags and UK markers (B3). OWNER: F-AB.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiringKind {
    Live,
    New,
    Repeat,
    Highlights,
    Unknown,
}

/// From `ProgrammeFlags` bits 0–2 and markers ("Live", "(R)", "[R]", "Repeat", "Replay",
/// "Rewind", "Highlights", "Match of the Day"). Placeholder until F-AB: Unknown.
pub fn airing_kind(_title: &str, _sub_title: Option<&str>, _flags: u32) -> AiringKind {
    AiringKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "F-AB implements airing_kind"]
    fn markers() {
        assert_eq!(airing_kind("Chelsea v Arsenal (R)", None, 0), AiringKind::Repeat);
        assert_eq!(airing_kind("Live: Arsenal v Chelsea", None, 0), AiringKind::Live);
        assert_eq!(airing_kind("Match of the Day", None, 0), AiringKind::Highlights);
        assert_eq!(airing_kind("Taskmaster", None, 0), AiringKind::Unknown);
    }
}
