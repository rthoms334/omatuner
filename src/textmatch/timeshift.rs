//! "+1" channels (B3; the Guide fixes design put it in `sources/timeshift.rs`). OWNER: G-core.

/// `(base name, minutes)` for a time-shifted channel name: "Channel 4 +1" → ("channel 4", 60),
/// "E4+1 HD" → ("e4", 60), "BBC One +24", "Quest Red + 1"; none for "Sky Sports+", "ITV4+",
/// "5USA", "Dave ja vu". Placeholder until G-core: none.
pub fn marker(_name: &str) -> Option<(String, i32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "G-core implements marker"]
    fn plus_one_names() {
        assert_eq!(marker("Channel 4 +1"), Some(("channel 4".into(), 60)));
        assert_eq!(marker("E4+1 HD"), Some(("e4".into(), 60)));
        assert_eq!(marker("Quest Red + 1"), Some(("quest red".into(), 60)));
        assert_eq!(marker("BBC One +24"), Some(("bbc one".into(), 1440)));
        for none in ["Sky Sports+", "ITV4+", "5USA", "Dave ja vu"] {
            assert_eq!(marker(none), None, "{none}");
        }
    }
}
