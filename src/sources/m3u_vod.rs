//! Movies and series in plain M3U playlists (DESIGN §27.5). Owner: P5 sources track.
//!
//! Used when an Xtream-style playlist stays M3U (the Xtream login failed or the user kept the
//! playlist): entries whose URL is an Xtream `/movie/` or `/series/` stream become movies and
//! series episodes instead of live channels. Everything else, and every entry of a plain
//! playlist (iptv-org, Free-TV), stays live. Pure functions; `jobs::import_source` converts.
//!
//! Details fixed by the implementation:
//! * An `S01E02` marker must not follow a letter or digit (`Glass1e2` is no marker) and its
//!   episode number must not run on into more digits.
//! * Series text keeps a leading `[`/`(` (`[UK] Show S01E02` → `[UK] Show`); trailing
//!   separators and an opening bracket are trimmed. The episode title loses leading separators
//!   and a closing bracket (`Show (S01E02) Pilot` → `Pilot`).

use std::sync::LazyLock;

use regex::Regex;

use super::m3u::M3uEntry;
use super::xtream::detect::parse_stream_url;
use crate::model::XtreamStreamKind;

/// What an M3U entry is.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryClass {
    Live,
    /// `/movie/<u>/<p>/<id>.<ext>`.
    Movie {
        stream_id: i64,
    },
    /// `/series/<u>/<p>/<id>.<ext>`; `marker` = the parsed `S01E02` part of the title, `None`
    /// when the title has none (§27.5: season 1, numbered in file order).
    Episode {
        stream_id: i64,
        marker: Option<EpisodeMarker>,
    },
}

/// `"Show Name S01 E02 - Title"` taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeMarker {
    /// Text before the marker, separators trimmed; empty when the title starts with it.
    pub series: String,
    pub season: u32,
    pub episode: u32,
    /// Text after the marker, separators trimmed; `None` when empty.
    pub title: Option<String>,
}

/// Classify one entry (§27.5 rules): an Xtream `/movie/` or `/series/` stream URL without
/// entry-level HTTP headers and without DRM is a movie / an episode; anything else is live.
pub fn classify(e: &M3uEntry) -> EntryClass {
    // movies and episodes play with the source User-Agent only: keep such entries live
    if !e.headers.is_empty() || e.drm {
        return EntryClass::Live;
    }
    match parse_stream_url(&e.url) {
        Some(s) if s.kind == XtreamStreamKind::Movie => EntryClass::Movie { stream_id: s.stream_id },
        Some(s) if s.kind == XtreamStreamKind::Series => {
            let marker = parse_episode_title(&e.title).or_else(|| e.tvg_name.as_deref().and_then(parse_episode_title));
            EntryClass::Episode { stream_id: s.stream_id, marker }
        }
        _ => EntryClass::Live,
    }
}

/// The three marker grammars; the capture groups are (marker, season, episode).
static MARKERS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        r"(?i)(?:^|[^\p{L}\p{N}])(S(\d{1,4})[ ._-]*EP?[ ._-]*(\d{1,4}))(?:\D|$)",
        r"(?i)\b((\d{1,2})x(\d{1,3}))\b",
        r"(?i)(Season\s*(\d{1,4})\s*[,.-]?\s*Episode\s*(\d{1,4}))",
    ]
    .map(|r| Regex::new(r).expect("static regex"))
});

/// Separators around a marker (`Show - S01E02 | Title`).
fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '–' | '|' | ':' | '.' | '_' | ',')
}

/// Find the season/episode marker in a title (§27.5 grammar: `S01E02`, `S01 E02`, `S1E2`,
/// `S01.E02`, `S01_E02`, `S01-E02`, `S01 EP02`, `1x02`, `Season 1 Episode 2`; case-insensitive,
/// leftmost match wins).
pub fn parse_episode_title(title: &str) -> Option<EpisodeMarker> {
    let caps = MARKERS.iter().filter_map(|re| re.captures(title)).min_by_key(|c| c.get(1).map_or(usize::MAX, |m| m.start()))?;
    let (marker, season, episode) = (caps.get(1)?, caps.get(2)?, caps.get(3)?);
    let series = title[..marker.start()].trim_start_matches(is_sep).trim_end_matches(|c| is_sep(c) || matches!(c, '(' | '['));
    let rest = title[marker.end()..].trim_start_matches(|c| is_sep(c) || matches!(c, ')' | ']'));
    let rest = rest.trim_end_matches(|c| is_sep(c) || matches!(c, '(' | '['));
    Some(EpisodeMarker {
        series: series.to_owned(),
        season: season.as_str().parse().ok()?,
        episode: episode.as_str().parse().ok()?,
        title: (!rest.is_empty()).then(|| rest.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HttpHeaders;

    fn m(series: &str, season: u32, episode: u32, title: Option<&str>) -> Option<EpisodeMarker> {
        Some(EpisodeMarker { series: series.into(), season, episode, title: title.map(str::to_owned) })
    }

    #[test]
    fn episode_markers() {
        for t in ["Show S01E02", "Show S01 E02", "Show S1E2", "Show S01.E02", "Show S01_E02", "Show S01-E02", "Show S01 EP02", "Show s01e02"] {
            assert_eq!(parse_episode_title(t), m("Show", 1, 2, None), "{t}");
        }
        assert_eq!(parse_episode_title("Example Show (2020) S01 E02 - Second"), m("Example Show (2020)", 1, 2, Some("Second")));
        assert_eq!(parse_episode_title("Show 1x02 | The One"), m("Show", 1, 2, Some("The One")));
        assert_eq!(parse_episode_title("Show: Season 3, Episode 10"), m("Show", 3, 10, None));
        assert_eq!(parse_episode_title("Show - season 2 episode 4: Title."), m("Show", 2, 4, Some("Title")));
        // separators and brackets around the marker; the series may be empty
        assert_eq!(parse_episode_title("[UK] Show (S01E02) Pilot"), m("[UK] Show", 1, 2, Some("Pilot")));
        assert_eq!(parse_episode_title("S00E00"), m("", 0, 0, None));
        assert_eq!(parse_episode_title("Show_S02E03_Title"), m("Show", 2, 3, Some("Title")));
        // leftmost marker wins across grammars
        assert_eq!(parse_episode_title("Show 2x05 S01E01"), m("Show", 2, 5, Some("S01E01")));
        // not markers
        for t in ["Movie 1920x1080", "Glass1e2", "Show S01E00012", "Mass Effect", "Show S01", "Episode 4"] {
            assert_eq!(parse_episode_title(t), None, "{t}");
        }
    }

    #[test]
    fn classification() {
        let e = |title: &str, url: &str| M3uEntry { title: title.into(), url: url.into(), ..Default::default() };
        assert_eq!(classify(&e("Film", "http://h/movie/u/p/5001.mkv")), EntryClass::Movie { stream_id: 5001 });
        assert_eq!(
            classify(&e("Show S01 E02", "http://h/series/u/p/9002.mp4")),
            EntryClass::Episode { stream_id: 9002, marker: m("Show", 1, 2, None) }
        );
        // the marker may be in tvg-name only
        let named = M3uEntry { tvg_name: Some("Show S02E01".into()), ..e("Show", "http://h/series/u/p/9003.mp4") };
        assert_eq!(classify(&named), EntryClass::Episode { stream_id: 9003, marker: m("Show", 2, 1, None) });
        assert_eq!(classify(&e("Show", "http://h/series/u/p/9004.mp4")), EntryClass::Episode { stream_id: 9004, marker: None });
        // live streams, plain URLs, and entries needing their own headers or DRM stay live
        assert_eq!(classify(&e("BBC", "http://h/live/u/p/1001.ts")), EntryClass::Live);
        assert_eq!(classify(&e("Film", "https://cdn.example/film.mp4")), EntryClass::Live);
        let headers = HttpHeaders { referrer: Some("http://r".into()), ..Default::default() };
        assert_eq!(classify(&M3uEntry { headers, ..e("Film", "http://h/movie/u/p/5001.mkv") }), EntryClass::Live);
        assert_eq!(classify(&M3uEntry { drm: true, ..e("Film", "http://h/movie/u/p/5001.mkv") }), EntryClass::Live);
    }
}
