//! Explicit content behind parental control (DESIGN §20.3), end to end through the store:
//! detection at import (category names, the provider's `is_adult` flags), the library's one
//! visibility predicate (absent from lists, collections, counts, eligibility, favourites,
//! history and search while hidden), the UI thread's re-filter on relock, the user's
//! per-category word surviving refreshes, and the point lookups the player, reminders and
//! `--play` use.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tuner::model::*;
use tuner::search::{SearchIndex, SearchSection};
use tuner::sources::identity::{group_key, normalize_name, url_key};
use tuner::store::import::{self, ImportChannel, ImportEpisode, ImportGroup, ImportMovie, ImportSeries, ImportSeriesEpisodes};
use tuner::store::library::{self, Library, Visibility};
use tuner::store::{Store, live, sources, user};

const NOW: Timestamp = 1_791_576_000;

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn draft(name: &str) -> SourceDraft {
    SourceDraft {
        name: name.into(),
        kind: SourceKind::M3uUrl,
        icon: "x".into(),
        color: SourceColor::Blue,
        url: Some(format!("http://127.0.0.1/{name}.m3u")),
        path: None,
        xtream: None,
        settings: SourceSettings::default(),
    }
}

/// A category as the provider names it (`raw`), flagged adult or not.
fn grp(kind: ItemKind, raw: &str, adult: bool, order: i32) -> ImportGroup {
    ImportGroup { kind, key: group_key(kind, raw), name: raw.into(), raw_name: Some(raw.into()), provider_id: None, provider_order: order, adult }
}

fn chan(key: &str, name: &str, number: u32, groups: &[&str], adult: bool) -> ImportChannel {
    let url = format!("http://127.0.0.1/live/{key}.ts");
    ImportChannel {
        key: ItemKey(key.into()),
        provider_id: None,
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        number: Some(number),
        logo: None,
        tvg_id: None,
        tvg_name: None,
        tvg_shift_min: 0,
        url_key: url_key(&url),
        stream: StreamLocator::Url { url },
        headers: None,
        catchup: None,
        radio: false,
        unsupported: None,
        provider_order: number as i32,
        added_at: None,
        groups: groups.iter().map(|g| group_key(ItemKind::Channel, g)).collect(),
        quality: None,
        adult,
    }
}

fn movie(key: &str, name: &str, group: &str, adult: bool) -> ImportMovie {
    let url = format!("http://127.0.0.1/movie/{key}.mp4");
    ImportMovie {
        key: ItemKey(key.into()),
        provider_id: None,
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: None,
        poster: None,
        rating: None,
        url_key: url_key(&url),
        stream: StreamLocator::Url { url },
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Movie, group)],
        adult,
    }
}

fn series(key: &str, name: &str, group: &str) -> ImportSeriesEpisodes {
    let url = format!("http://127.0.0.1/series/{key}/1.mp4");
    ImportSeriesEpisodes {
        series: ImportSeries {
            key: ItemKey(key.into()),
            provider_id: None,
            name: name.into(),
            clean_name: name.into(),
            norm_name: normalize_name(name),
            year: None,
            cover: None,
            backdrop: None,
            rating: None,
            plot: None,
            genre: None,
            cast: None,
            director: None,
            release_date: None,
            last_modified: None,
            provider_order: 0,
            added_at: None,
            groups: vec![group_key(ItemKind::Series, group)],
            adult: false,
        },
        episodes: vec![ImportEpisode {
            key: ItemKey(format!("{key}:e1")),
            provider_id: None,
            season: 1,
            episode: 1,
            title: "Pilot".into(),
            plot: None,
            duration_s: None,
            still: None,
            air_date: None,
            rating: None,
            stream: StreamLocator::Url { url },
        }],
    }
}

/// Playlist "TV": channel categories "UK | News", "|XXX| Live" (explicit by name), "Hot Stuff"
/// (flagged by the provider unless `hot_adult` is false), "Adult Swim" (innocent) and "Kids";
/// a channel the provider flags in "Kids"; movie categories "Movies" and "For Adults"; series
/// categories "Drama" and "Erotic". Playlist "TV 2": only "XXX VOD" movies.
fn import_all(s: &Store, tv: SourceId, tv2: SourceId, hot_adult: bool) {
    w(s, move |c| {
        let imp = import::begin_catalogue(c, tv, NOW)?;
        import::upsert_groups(
            c,
            &imp,
            &[
                grp(ItemKind::Channel, "UK | News", false, 0),
                grp(ItemKind::Channel, "|XXX| Live", false, 1),
                grp(ItemKind::Channel, "Hot Stuff", hot_adult, 2),
                grp(ItemKind::Channel, "Adult Swim", false, 3),
                grp(ItemKind::Channel, "Kids", false, 4),
                grp(ItemKind::Movie, "Movies", false, 0),
                grp(ItemKind::Movie, "For Adults", false, 1),
                grp(ItemKind::Series, "Drama", false, 0),
                grp(ItemKind::Series, "Erotic", false, 1),
            ],
        )?;
        import::write_channels(
            c,
            &imp,
            &[
                chan("m3u:news", "News 24", 1, &["UK | News"], false),
                chan("m3u:late", "Late Night", 2, &["|XXX| Live"], false),
                chan("m3u:hot", "Hot One", 3, &["Hot Stuff"], false),
                chan("m3u:swim", "Swim TV", 4, &["Adult Swim"], false),
                chan("m3u:cartoon", "Cartoons", 5, &["Kids"], false),
                chan("m3u:flagged", "Flagged", 6, &["Kids"], true),
            ],
        )?;
        import::write_movies(c, &imp, &[movie("m3u:dune", "Dune", "Movies", false), movie("m3u:blue", "Blue Film", "For Adults", false)])?;
        import::write_series_episodes(c, &imp, &[series("m3u-series:drama", "Drama Show", "Drama"), series("m3u-series:hot", "Hot Show", "Erotic")])?;
        import::finish_catalogue(c, &imp, &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series], NOW)?;

        let imp = import::begin_catalogue(c, tv2, NOW)?;
        import::upsert_groups(c, &imp, &[grp(ItemKind::Movie, "XXX VOD", false, 0)])?;
        import::write_movies(c, &imp, &[movie("m3u:x2", "Other Film", "XXX VOD", false)])?;
        import::finish_catalogue(c, &imp, &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series], NOW)?;
        Ok(())
    });
}

fn setup(name: &str) -> (Store, SourceId, SourceId) {
    let s = Store::open_temp(name, None).expect("temp store");
    let (tv, tv2) = w(&s, |c| Ok((sources::insert_source(c, &draft("TV"), NOW)?, sources::insert_source(c, &draft("TV 2"), NOW)?)));
    import_all(&s, tv, tv2, true);
    // the user's own lists hold explicit and ordinary channels alike
    w(&s, move |c| {
        let list = user::default_fav_list(c)?;
        for key in ["m3u:late", "m3u:news"] {
            let item = ItemRef { source: tv, kind: ItemKind::Channel, key: ItemKey(key.into()) };
            user::set_favourite(c, list, &item, true, NOW)?;
            user::push_history(c, &HistoryEntry { item, at: NOW, title: key.into() })?;
        }
        Ok(())
    });
    (s, tv, tv2)
}

fn channel_names(lib: &Library) -> Vec<String> {
    let mut v: Vec<String> = lib.live.channels.iter().map(|c| c.name.to_string()).collect();
    v.sort();
    v
}

fn group_names(lib: &Library) -> Vec<String> {
    let mut v: Vec<String> = lib.live.groups.iter().chain(&lib.vod.movie_groups).chain(&lib.vod.series_groups).map(|g| g.name.clone()).collect();
    v.sort();
    v
}

fn vod_names(v: &[VodSummary]) -> Vec<String> {
    v.iter().map(|x| x.name.to_string()).collect()
}

fn names_of(lib: &Library, ids: &[ChannelId]) -> Vec<String> {
    ids.iter().map(|id| lib.live.get(*id).map_or("?".into(), |c| c.name.to_string())).collect()
}

fn ch(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(key.into()) }
}

/// Global search over a snapshot, everything a session could see (locks ignored).
fn search(lib: &Library, q: &str) -> Vec<String> {
    let mut idx = SearchIndex::new(Arc::new(|| {}));
    idx.rebuild(lib, true);
    idx.set_query(q);
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut hits = Vec::new();
    while Instant::now() < deadline {
        idx.tick(10);
        hits = idx.results(SearchSection::All, 50).into_iter().map(|h| h.item.name.to_string()).collect();
        std::thread::sleep(Duration::from_millis(5));
    }
    hits
}

/// What the import stored: the name rules and the provider's flag per category, the provider's
/// flag per item.
#[test]
fn import_classifies_categories_by_name_and_provider_flag() {
    let (s, _, _) = setup("tuner-explicit-import");
    let c = s.reader().unwrap();
    let code = |name: &str| -> i64 { c.query_row("SELECT explicit FROM grp WHERE name = ?1", [name], |r| r.get(0)).unwrap() };
    assert_eq!(["UK | News", "|XXX| Live", "Hot Stuff", "Adult Swim", "For Adults", "Erotic", "XXX VOD"].map(code), [0, 1, 2, 0, 1, 1, 1]);
    let flagged: Vec<String> =
        c.prepare("SELECT key FROM channel WHERE explicit = 1").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(flagged, ["m3u:flagged"]);
}

/// While hidden, explicit categories and items are absent from everything the library feeds:
/// lists, collections (the zap lists), numbers, counts, eligible playlists, favourites,
/// history and search. Shown, they are all there.
#[test]
fn hidden_explicit_content_is_absent_everywhere() {
    let (s, tv, tv2) = setup("tuner-explicit-hidden");
    let c = s.reader().unwrap();
    let hidden = library::load_visible(&c, Visibility::NO_EXPLICIT).unwrap();
    let favs = library::load_favs(&c).unwrap();

    assert_eq!(channel_names(&hidden), ["Cartoons", "News 24", "Swim TV"]);
    assert_eq!(group_names(&hidden), ["Adult Swim", "Drama", "Kids", "Movies", "UK | News"]);
    let all = hidden.live.collection(&Collection::AllChannels, &favs, &hidden.recent_channels, SortMode::Provider);
    assert_eq!(names_of(&hidden, &all), ["News 24", "Swim TV", "Cartoons"]);
    let kids = hidden.live.groups.iter().find(|g| g.name == "Kids").unwrap().id;
    assert_eq!(names_of(&hidden, hidden.live.group_members(kids)), ["Cartoons"], "the provider-flagged channel left its ordinary category");
    assert_eq!(hidden.live.by_number(2), None, "numeric entry can't reach it");
    assert_eq!(hidden.count(ItemKind::Channel, tv), 3);
    assert_eq!((hidden.count(ItemKind::Movie, tv), hidden.count(ItemKind::Movie, tv2)), (1, 0));
    assert_eq!(hidden.eligible(ItemKind::Movie).iter().map(|s| s.id).collect::<Vec<_>>(), [tv], "TV 2 has nothing else");
    assert_eq!(vod_names(&hidden.vod.movies), ["Dune"]);
    assert_eq!(vod_names(&hidden.vod.series), ["Drama Show"]);
    let default = favs.default_list().unwrap();
    assert_eq!(names_of(&hidden, &hidden.live.collection(&Collection::Favourites(default), &favs, &[], SortMode::Provider)), ["News 24"]);
    assert_eq!(names_of(&hidden, &hidden.recent_channels), ["News 24"]);
    assert_eq!(search(&hidden, "late night"), Vec::<String>::new());
    assert_eq!(search(&hidden, "film"), Vec::<String>::new());
    assert_eq!(search(&hidden, "news 24"), ["News 24"]);
    assert!(hidden.conceals(&ch(tv, "m3u:late")) && hidden.conceals(&ch(tv, "m3u:flagged")));
    assert!(!hidden.conceals(&ch(tv, "m3u:news")));
    assert!(!hidden.shows_explicit());

    let shown = library::load_visible(&c, Visibility::ALL).unwrap();
    assert_eq!(channel_names(&shown), ["Cartoons", "Flagged", "Hot One", "Late Night", "News 24", "Swim TV"]);
    assert_eq!(vod_names(&shown.vod.movies), ["Dune", "Blue Film", "Other Film"]);
    assert_eq!(shown.eligible(ItemKind::Movie).len(), 2);
    assert_eq!(names_of(&shown, &shown.recent_channels).len(), 2);
    assert_eq!(search(&shown, "late night"), ["Late Night"]);
    assert!(shown.shows_explicit() && !shown.conceals(&ch(tv, "m3u:late")));
    assert_eq!(shown.explicit.groups.len(), 5);
}

/// The relock path: the UI thread's re-filter of a shown snapshot equals a hidden build, and
/// is a new snapshot screens notice.
#[test]
fn relock_refilter_matches_a_hidden_build() {
    let (s, tv, _) = setup("tuner-explicit-refilter");
    let c = s.reader().unwrap();
    let shown = library::load_visible(&c, Visibility::ALL).unwrap();
    let built = library::load_visible(&c, Visibility::NO_EXPLICIT).unwrap();
    let refiltered = shown.hiding_explicit();
    assert_ne!(refiltered.generation, shown.generation);
    assert!(refiltered.built_at > shown.built_at, "screens keyed on the build rebuild too");
    assert_eq!(refiltered.visibility, Visibility::NO_EXPLICIT);
    assert_eq!(channel_names(&refiltered), channel_names(&built));
    assert_eq!(group_names(&refiltered), group_names(&built));
    assert_eq!(vod_names(&refiltered.vod.movies), vod_names(&built.vod.movies));
    assert_eq!(vod_names(&refiltered.vod.series), vod_names(&built.vod.series));
    assert_eq!(names_of(&refiltered, &refiltered.recent_channels), names_of(&built, &built.recent_channels));
    assert_eq!(refiltered.sources, built.sources, "eligibility too");
    for g in refiltered.vod.movie_groups.iter().chain(&refiltered.vod.series_groups) {
        let n = |lib: &Library| lib.vod.group_items(g.id, SortMode::Provider).len();
        assert_eq!(n(&refiltered), n(&built), "members re-pointed: {}", g.name);
    }
    let movies = refiltered.vod.movie_groups.iter().find(|g| g.name == "Movies").unwrap().id;
    assert_eq!(
        refiltered
            .vod
            .group_items(movies, SortMode::Provider)
            .iter()
            .map(|i| refiltered.vod.movies[*i as usize].name.to_string())
            .collect::<Vec<_>>(),
        ["Dune"]
    );
    assert_eq!(refiltered.count(ItemKind::Channel, tv), built.count(ItemKind::Channel, tv));
}

/// "Not explicit" / "Mark as explicit" on categories (the user's word beats the name rules and
/// the provider's flags, also on items) survive a refresh; a provider flag that goes away
/// takes its category's content back to visible.
#[test]
fn overrides_win_and_survive_a_refresh() {
    let (s, tv, tv2) = setup("tuner-explicit-overrides");
    let group = |name: &'static str| {
        let c = s.reader().unwrap();
        let (kind, key): (String, String) = c.query_row("SELECT kind, key FROM grp WHERE name = ?1", [name], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        GroupRef { source: tv, kind: if kind == "channel" { ItemKind::Channel } else { ItemKind::Movie }, key: GroupKey(key) }
    };
    let (xxx, swim, kids) = (group("|XXX| Live"), group("Adult Swim"), group("Kids"));
    w(&s, move |c| {
        user::set_group_explicit(c, &xxx, Some(false))?;
        user::set_group_explicit(c, &swim, Some(true))?;
        user::set_group_explicit(c, &kids, Some(false))
    });
    let hidden = |s: &Store| library::load_visible(&s.reader().unwrap(), Visibility::NO_EXPLICIT).unwrap();
    assert_eq!(channel_names(&hidden(&s)), ["Cartoons", "Flagged", "Late Night", "News 24"]);
    let lib = hidden(&s);
    let explicit_of = |name: &str| lib.live.groups.iter().chain(&lib.vod.movie_groups).find(|g| g.name == name).map(|g| g.explicit);
    assert_eq!(explicit_of("|XXX| Live"), Some(Explicit::Cleared));
    assert_eq!(explicit_of("Adult Swim"), None, "marked: absent");

    // a refresh: the provider no longer flags "Hot Stuff"; the user's words stay
    import_all(&s, tv, tv2, false);
    assert_eq!(channel_names(&hidden(&s)), ["Cartoons", "Flagged", "Hot One", "Late Night", "News 24"]);
    let shown = library::load_visible(&s.reader().unwrap(), Visibility::ALL).unwrap();
    assert_eq!(shown.live.groups.iter().find(|g| g.name == "Adult Swim").map(|g| g.explicit), Some(Explicit::Marked));

    // back to what was detected: no override row left
    let xxx = group("|XXX| Live");
    w(&s, move |c| user::set_group_explicit(c, &xxx, None));
    assert!(!channel_names(&hidden(&s)).contains(&"Late Night".to_owned()));
}

/// The point lookups (player zapping, reminders, `--play`, job labels) agree with the library,
/// episodes through their series.
#[test]
fn point_lookups_agree_with_the_library() {
    let (s, tv, tv2) = setup("tuner-explicit-lookups");
    let c = s.reader().unwrap();
    let shown = library::load_visible(&c, Visibility::ALL).unwrap();
    for ch in &shown.live.channels {
        let r = ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key.clone() };
        assert_eq!(live::channel_explicit(&c, ch.id).unwrap(), shown.explicit.channels.contains(&ch.id), "{}", ch.name);
        assert_eq!(live::item_explicit(&c, &r).unwrap(), shown.explicit.contains(&r), "{}", ch.name);
    }
    for v in shown.vod.movies.iter().chain(&shown.vod.series) {
        let kind = if matches!(v.kind, VodKind::Movie(_)) { ItemKind::Movie } else { ItemKind::Series };
        let r = ItemRef { source: v.source_id, kind, key: v.key.clone() };
        assert_eq!(live::item_explicit(&c, &r).unwrap(), shown.explicit.vod.contains(&v.kind), "{}", v.name);
    }
    let episode = |key: &str| ItemRef { source: tv, kind: ItemKind::Episode, key: ItemKey(key.into()) };
    assert!(live::item_explicit(&c, &episode("m3u-series:hot:e1")).unwrap());
    assert!(!live::item_explicit(&c, &episode("m3u-series:drama:e1")).unwrap());
    assert!(!live::item_explicit(&c, &ch(tv2, "m3u:none")).unwrap(), "unknown rows aren't");
}
