//! The screenshot demo profile (`examples/demo_profile.rs`) builds through the real import
//! jobs and gives the app what the README shots need: three playlists, stacks across and
//! inside playlists, country-tagged categories, a guide, quality badges, user data and the v0.2
//! features (Up next, Following, reminders, Guide fixes, multi-view's connection limits).

#[path = "../examples/demo_profile.rs"]
#[allow(dead_code)]
mod demo;

use tuner::model::{ItemKind, Quality};

#[test]
fn demo_profile_builds_a_library_with_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    let (earth, launch) = (dir.path().join("earth.mp4"), dir.path().join("launch.mp4"));
    let s = demo::build(&profile, Some(&earth), Some(&launch), true).expect("demo profile");
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
    assert!(badged >= 40 && unmapped == 2, "{badged} badged, {unmapped} without guide (Guide fixes)");
    let uhd: i64 = c.query_row("SELECT count(*) FROM channel WHERE quality = ?1", [Quality::Uhd as i64], |r| r.get(0)).unwrap();
    assert!(uhd >= 4);

    // the user's side: channel favourites, saved titles, Continue Watching, reminders
    assert!(!lib.favs.is_empty() && lib.recent_channels.len() >= 4);
    let n = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(n("SELECT count(*) FROM fav_item WHERE kind<>'channel'"), 0);
    assert_eq!(n("SELECT count(*) FROM watchlist_item"), 5);
    assert_eq!(n("SELECT count(*) FROM watch_progress"), 3);
    assert_eq!(n("SELECT count(*) FROM reminder WHERE rule_id IS NULL"), 3);
    assert_eq!(n("SELECT count(*) FROM reminder WHERE rule_id IS NULL AND mode = 'switch'"), 1);
    // nothing fires within the hour (screenshots run on a copy, never interrupted)
    let soon = tuner::model::now() + 3600;
    assert_eq!(n(&format!("SELECT count(*) FROM reminder WHERE start - CASE WHEN lead_s > 0 THEN lead_s ELSE 0 END < {soon}")), 0);

    // Up next: the trilogy (three titles, not one stack) and an episode, the first film played
    let rows = tuner::store::queue::load(&c).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows.iter().filter(|r| r.state == tuner::queue::RowState::Played).count(), 1);
    let trilogy: Vec<u32> = movies.iter().enumerate().filter(|(_, m)| m.name.contains("Starling Bay")).map(|(i, _)| i as u32).collect();
    let stacks: std::collections::HashSet<_> = trilogy.iter().map(|i| lib.vod.stacks(true).stack_of(*i).map_or(u32::MAX - i, |s| s.0)).collect();
    assert_eq!(stacks.len(), 3, "the three films are separate titles");

    // Following: a team with two fixtures in the guide, a news flash, a listed show
    assert_eq!(n("SELECT count(*) FROM follow_rule"), 3);
    let team = "SELECT count(*) FROM reminder r JOIN follow_rule f ON f.id = r.rule_id WHERE f.kind = 'team'";
    assert_eq!(n(team), 2, "Northport Albion's two live fixtures");
    assert!(n("SELECT count(*) FROM follow_match WHERE state = 'listed'") >= 1);

    // multi-view: one connection per paid playlist
    let limits = tuner::store::sources::account_limits(&c).unwrap();
    assert_eq!(limits.iter().filter(|(_, m)| *m == Some(1)).count(), 2);

    // multi-view's two pictures play different clips: Polar Nature the first, Lumen Docs the second
    let m3u = std::fs::read_to_string(profile.join("demo/northwind.m3u")).unwrap();
    let url_after = |name: &str| m3u.lines().skip_while(|l| !l.contains(&format!(",{name}"))).nth(1).unwrap().to_string();
    assert!(url_after("Polar Nature").ends_with("/earth.mp4") && url_after("Lumen Docs").ends_with("/launch.mp4"));

    let refs = std::fs::read_to_string(profile.join("demo/refs.env")).unwrap();
    assert!(refs.contains("NORTHWIND_ID=") && refs.contains("PLAY_DOCS='"), "{refs}");
}
