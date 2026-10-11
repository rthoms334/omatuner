//! Fixtures "A v B" in programme text (B3). OWNER: F-AB.

/// The two sides of a fixture in `title` or `sub_title` (separators v / vs / versus / - / – /
/// @; team-suffix exclusions such as Women, U21 are kept on the side so rules can leave them
/// out). Placeholder until F-AB: none.
pub fn parse_fixture(_title: &str, _sub_title: Option<&str>) -> Option<(String, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "F-AB implements parse_fixture"]
    fn uk_guide_fixtures() {
        assert_eq!(parse_fixture("Premier League Football", Some("Arsenal v Chelsea")), Some(("Arsenal".into(), "Chelsea".into())));
        assert_eq!(parse_fixture("Live: Arsenal vs. Chelsea", None), Some(("Arsenal".into(), "Chelsea".into())));
        assert_eq!(parse_fixture("Arsenal: The Invincibles", None), None);
        assert_eq!(parse_fixture("Arsenal Women v Chelsea Women", None), Some(("Arsenal Women".into(), "Chelsea Women".into())));
    }
}
