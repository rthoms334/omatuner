//! "Follow this…" suggestions from one programme (pure; DESIGN §33 §3.5). OWNER: F-AB.

use crate::follow::{Airings, FollowKind, FollowSpec, Shape};
use crate::textmatch::fold::{folded, has_token};

/// One line of the sheet ("Arsenal: every match · Team").
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// The sheet's line ("Arsenal: every match").
    pub label: String,
    /// The rule's name ("Arsenal").
    pub name: String,
    pub kind: FollowKind,
    pub spec: FollowSpec,
}

/// Suggestions for a programme, best first: one Team per fixture side, an Event for a
/// competition ("Premier League Football: every live game"), "New episodes of X" for S/E data
/// or an episode title, a News flash for a news bulletin, "Every airing of X" always.
/// `headlines_min`: a news flash's default length (`follow.news_flash.headlines_min`).
pub fn suggest(title: &str, sub_title: Option<&str>, categories: &[String], has_episode: bool, headlines_min: u16) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let core = core_name(title);
    let sport = categories.iter().any(|c| {
        let c = folded(c);
        ["sport", "sports", "football", "soccer", "rugby", "cricket", "tennis", "motor", "formula 1", "golf", "boxing", "darts", "snooker"]
            .iter()
            .any(|w| has_token(&c, w))
    });
    let fixture = crate::textmatch::fixture::parse_fixture(title, sub_title);
    if let Some((a, b)) = &fixture {
        for side in [a, b] {
            out.push(Suggestion {
                label: format!("{side}: every match"),
                name: side.clone(),
                kind: FollowKind::Team,
                spec: FollowSpec::template(FollowKind::Team, vec![side.clone()], headlines_min),
            });
        }
    }
    // the competition: the text before ":" or the title itself when the fixture is elsewhere
    let title_is_fixture = crate::textmatch::fixture::parse_fixture(title, None).is_some();
    let competition = if let Some((before, _)) = title.split_once(':').filter(|_| fixture.is_some() || sport) {
        Some(crate::textmatch::fixture::strip_prefix_markers(before.trim()).to_owned())
    } else if (fixture.is_some() && !title_is_fixture) || (sport && fixture.is_none()) {
        Some(core.clone())
    } else {
        None
    }
    .filter(|c| !c.is_empty() && fixture.as_ref().is_none_or(|(a, b)| c != a && c != b));
    if let Some(comp) = competition {
        out.push(Suggestion {
            label: format!("{comp}: every live game"),
            name: comp.clone(),
            kind: FollowKind::Event,
            spec: FollowSpec::template(FollowKind::Event, vec![comp], headlines_min),
        });
    }
    let news = has_token(&folded(title), "news") || categories.iter().any(|c| has_token(&folded(c), "news"));
    if news && fixture.is_none() {
        out.push(Suggestion {
            label: format!("News flash: {core} every day"),
            name: core.clone(),
            kind: FollowKind::News,
            spec: FollowSpec::template(FollowKind::News, vec![core.clone()], headlines_min),
        });
    }
    if fixture.is_none() && !news && (has_episode || sub_title.is_some_and(|s| !s.trim().is_empty())) {
        out.push(Suggestion {
            label: format!("New episodes of {core}"),
            name: core.clone(),
            kind: FollowKind::Show,
            spec: FollowSpec::template(FollowKind::Show, vec![core.clone()], headlines_min),
        });
    }
    out.push(Suggestion {
        label: format!("Every airing of \"{core}\""),
        name: core.clone(),
        kind: FollowKind::Custom,
        spec: FollowSpec {
            shape: Shape::TitleIs { spin_offs: false },
            airings: Airings::Any,
            ..FollowSpec::template(FollowKind::Custom, vec![core], headlines_min)
        },
    });
    out
}

/// The title as a name: markers ("Live:", "(R)", "HD") dropped, case kept.
fn core_name(title: &str) -> String {
    let t = crate::textmatch::fixture::strip_prefix_markers(title.trim());
    let mut t = t.trim();
    for suffix in ["(R)", "[R]", "(Live)", "- Live", " HD", " FHD", " UHD", " 4K"] {
        t = t.strip_suffix(suffix).unwrap_or(t).trim_end();
    }
    t.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|s| s.label.as_str()).collect()
    }

    #[test]
    fn sheet_suggestions() {
        let sport = ["Sport".to_owned(), "Football".to_owned()];
        let s = suggest("Premier League Football", Some("Arsenal v Chelsea"), &sport, false, 5);
        assert_eq!(
            labels(&s),
            [
                "Arsenal: every match",
                "Chelsea: every match",
                "Premier League Football: every live game",
                "Every airing of \"Premier League Football\""
            ]
        );
        assert_eq!((s[0].kind, &s[0].spec.shape), (FollowKind::Team, &Shape::Fixture));
        let s = suggest("Formula 1: Live Japanese Grand Prix Qualifying", None, &sport, false, 5);
        assert_eq!(labels(&s)[0], "Formula 1: every live game");
        let s = suggest("Taskmaster", Some("Hot Dogs"), &[], true, 5);
        assert_eq!(labels(&s), ["New episodes of Taskmaster", "Every airing of \"Taskmaster\""]);
        assert_eq!(s[0].spec.airings, Airings::NewOnly);
        let s = suggest("BBC News at One", None, &["News".to_owned()], false, 5);
        assert_eq!(labels(&s), ["News flash: BBC News at One every day", "Every airing of \"BBC News at One\""]);
        assert_eq!(s[0].spec.flash.map(|f| f.length), Some(crate::follow::flash::FlashLength::Headlines(5)));
    }
}
