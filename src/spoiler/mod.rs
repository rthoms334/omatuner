//! The spoiler guard (DESIGN §34): on, but narrow — series you're part-way through and sport
//! you follow, with a quick reveal (the user's decision, 2026-10-11). OWNERS:
//! * S1 (engine): `snapshot.rs`, `verdict.rs`, `tracker.rs` (+ the snapshot builder service),
//!   `guard/*`, `textmatch/scores.rs`, `store::spoiler`, `model/spoiler.rs`, migration 0012;
//! * S-ui: `spoiler/ui.rs` (new: `RevealSet`, Ctrl+B / Ctrl+Shift+B), the painters' widget, the
//!   menu, the rule editor, the Settings page, the status chip, the player's shield card.
//!
//! The guard is display-only (stored data, search and sort never change), fails closed before
//! its first build, runs after `Visibility` (never lists, counts or reveals explicit content)
//! and never bypasses a PIN.
//!
//! Data flow: [`tracker::start`] installs a live [`crate::guard::Guard`] in `AppShared` and runs
//! the builder (a [`snapshot::GuardSnapshot`] on the blocking pool, debounced 1 s, on
//! `Guard::rebuild`, a new library snapshot, a settings change through `Guard::configure` and an
//! hourly tick) and the seen tracker (every 30 s, every published picture). Everything that shows
//! text asks the guard (`guard::Guard::{text, judge, shield, outside_title}`; MCP replies through
//! `guard::reply::guard_json`).

pub mod snapshot;
pub mod tracker;
pub mod ui;
pub mod verdict;

/// S-ui flips this in its last commit.
pub const ENABLED: bool = false;

use crate::model::Timestamp;

/// The local date (`YYYY-MM-DD`) of an instant: event keys are per local day (B3).
pub fn local_date(ts: Timestamp) -> String {
    jiff::Timestamp::from_second(ts).map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date().to_string()).unwrap_or_default()
}

/// The event key of a guide programme (`textmatch::event_key`, shared with Following's
/// ledger): what "seen" is recorded against.
pub fn event_key_of(title: &str, sub_title: Option<&str>, description: Option<&str>, season_episode: Option<(i32, i32)>, start: Timestamp) -> String {
    let date = local_date(start);
    crate::textmatch::event::event_key(&crate::textmatch::event::EventInput {
        title,
        sub_title,
        description,
        season: season_episode.map(|s| s.0),
        episode: season_episode.map(|s| s.1),
        local_date: &date,
    })
}

/// The `spoiler_seen` row for a guide programme marked seen (Ctrl+W, the popover's "Mark
/// seen", MCP `mark_seen`): write it with `store::spoiler::mark_seen`, then `Guard::rebuild`.
pub fn seen_row_for(p: &crate::model::Programme, how: crate::model::SeenHow, now: Timestamp) -> crate::model::SeenRow {
    let se = programme_se(p);
    crate::model::SeenRow {
        event_key: event_key_of(&p.title, p.sub_title.as_deref(), p.description.as_deref(), se, p.start),
        first_start: p.start,
        title: p.title.clone(),
        series_norm: se.map(|_| series_key(&p.title)),
        season: se.map(|s| s.0),
        episode: se.map(|s| s.1),
        seen_at: now,
        how,
    }
}

/// A programme's season and episode, when the guide gave both.
pub fn programme_se(p: &crate::model::Programme) -> Option<(i32, i32)> {
    p.episode.and_then(|e| Some((i32::try_from(e.season?).ok()?, i32::try_from(e.episode?).ok()?)))
}

/// The key series rules and `Subject::{Vod, Episode}::series_norm` use: the catalogue's
/// `series.norm_name`, i.e. `normalize_name` of the series' name (a guide title of the same show
/// gives the same key).
pub fn series_key(name: &str) -> String {
    crate::sources::identity::normalize_name(name)
}

/// Words that make a guide category a sport one.
const SPORT_WORDS: &[&str] = &[
    "sport",
    "football",
    "soccer",
    "rugby",
    "cricket",
    "tennis",
    "golf",
    "motor",
    "formula",
    "f1",
    "boxing",
    "darts",
    "snooker",
    "athletics",
    "basketball",
    "baseball",
    "hockey",
    "cycling",
    "nfl",
    "mma",
    "ufc",
    "wrestling",
    "racing",
    "olympic",
];

/// Is any of the guide categories a sport one ("Sport", "Football", "Sports event" …)?
pub fn is_sport(categories: &[String]) -> bool {
    categories.iter().any(|c| {
        let c = c.to_ascii_lowercase();
        c.split(|ch: char| !ch.is_alphanumeric()).any(|w| SPORT_WORDS.iter().any(|s| w.starts_with(s)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sport_categories() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_sport(&c(&["Sports"])));
        assert!(is_sport(&c(&["Movie", "Football"])));
        assert!(is_sport(&c(&["Sport event"])));
        assert!(!is_sport(&c(&["News", "Documentary"])));
        assert!(!is_sport(&[]));
    }
}
