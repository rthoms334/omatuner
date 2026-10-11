//! The screenshot demo profile (`examples/demo_profile.rs`) builds through the real import
//! jobs and gives the app what the README shots need: three playlists, stacks across and
//! inside playlists, country-tagged categories, a guide, quality badges and user data.

#[path = "../examples/demo_profile.rs"]
#[allow(dead_code)]
mod demo;

use tuner::model::{ItemKind, Quality};

#[test]
fn demo_profile_builds_a_library_with_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    let s = demo::build(&profile, None, true).expect("demo profile");
    let lib = &s.library;
    assert_eq!(s.sources.len(), 3);
    assert_eq!(lib.eligible(ItemKind::Channel).len(), 3);
    assert_eq!(lib.eligible(ItemKind::Movie).len(), 2, "Harbor Free has no movies");
    assert!(s.channels >= 55 && s.programmes > 1000, "{}", s);

    // "The Keeper of Wren Point": EN + FR in Northwind, 4K EN in Aurora → one stack of three
    let movies = &lib.vod.movies;
    let keeper = movies.iter().position(|m| m.name.contains("Keeper of Wren Point")).expect("movie") as u32;
    let stack = lib.vod.stacks(true).stack_of(keeper).expect("stacked");
    assert_eq!(lib.vod.stacks(true).members(stack).len(), 3);
    assert!(lib.vod.stacks(false).len() >= 2, "series stack too");
    // smart names took the language prefix and the quality off every title
    assert!(movies.iter().all(|m| !m.name.contains(" - ") && !m.name.contains("[4K]")));

    // country-tagged categories, quality badges, a guide for every channel
    let live = &lib.live;
    let groups = live.visible_groups(None, true);
    assert!(groups.iter().any(|g| g.name == "News") && groups.len() >= 20, "{}", groups.len());
    let c = tuner::store::Store::open(&profile.join("data/tuner.db"), None).unwrap().reader().unwrap();
    let (badged, unmapped): (i64, i64) = c
        .query_row(
            "SELECT count(quality), (SELECT count(*) FROM channel WHERE id NOT IN (SELECT channel_id FROM channel_epg)) FROM channel",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(badged >= 40 && unmapped == 0, "{badged} badged, {unmapped} without guide");
    let uhd: i64 = c.query_row("SELECT count(*) FROM channel WHERE quality = ?1", [Quality::Uhd as i64], |r| r.get(0)).unwrap();
    assert!(uhd >= 4);

    // the user's side: favourites, recently watched, Continue Watching, a reminder
    assert!(!lib.favs.is_empty() && lib.recent_channels.len() >= 4);
    let progress: i64 = c.query_row("SELECT count(*) FROM watch_progress", [], |r| r.get(0)).unwrap();
    let reminders: i64 = c.query_row("SELECT count(*) FROM reminder", [], |r| r.get(0)).unwrap();
    assert_eq!((progress, reminders), (3, 1));

    let refs = std::fs::read_to_string(profile.join("demo/refs.env")).unwrap();
    assert!(refs.contains("NORTHWIND_ID=") && refs.contains("PLAY_DOCS='"), "{refs}");
}
