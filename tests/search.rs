//! Global search (nucleo index over a library snapshot) and EPG search. Owner: WP-09.
//! DESIGN §14 / §24.2 WP-09.

mod support;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tuner::model::{
    ChannelId, ChannelSummary, Group, GroupId, GroupKey, ItemKey, ItemKind, MovieId, SeriesId, SourceColor, SourceId, SourceKind, VodKind, VodSummary,
};
use tuner::search::{EpgSearch, SearchHit, SearchIndex, SearchSection, SearchTarget};
use tuner::store::library::{Library, LiveIndex, SourceMeta, VodIndex};

fn source(id: i64, name: &str) -> SourceMeta {
    SourceMeta {
        id: SourceId(id),
        name: name.into(),
        icon: String::new(),
        color: SourceColor::Accent,
        kind: SourceKind::M3uUrl,
        enabled: true,
        last_refresh: None,
        has_vod: true,
        has_series: true,
        category_filter: None,
    }
}

fn group(id: i64, kind: ItemKind, name: &str, hidden: bool, locked: bool) -> Group {
    Group {
        id: GroupId(id),
        source_id: SourceId(1),
        kind,
        key: GroupKey(format!("g{id}")),
        name: name.into(),
        rename: None,
        provider_order: id as i32,
        user_order: None,
        hidden,
        user_hidden: hidden,
        locked,
        item_count: 0,
        provider_id: None,
        tag: None,
        filter: Default::default(),
    }
}

fn channel(id: i64, source: i64, name: &str, groups: &[i64], locked: bool) -> ChannelSummary {
    ChannelSummary {
        id: ChannelId(id),
        source_id: SourceId(source),
        key: ItemKey(format!("c{id}")),
        name: Arc::from(name),
        number: Some(id as u32),
        logo: Some(Arc::from(format!("http://logos.example/{id}.png"))),
        group_ids: groups.iter().map(|g| GroupId(*g)).collect(),
        epg_channel_id: None,
        tvg_shift_min: 0,
        has_catchup: false,
        catchup_days: 0,
        radio: false,
        locked,
        unsupported: false,
        provider_order: id as i32,
        user_order: None,
        added_at: None,
    }
}

fn vod(kind: VodKind, name: &str, groups: &[i64], locked: bool) -> VodSummary {
    VodSummary {
        kind,
        source_id: SourceId(1),
        key: ItemKey(name.to_lowercase()),
        name: Arc::from(name),
        year: Some(2019),
        poster: None,
        rating: None,
        group_ids: groups.iter().map(|g| GroupId(*g)).collect(),
        added_at: None,
        provider_order: 0,
        locked,
    }
}

/// Two sources; groups: 1 UK (channels), 2 Hidden (channels, hidden), 3 Adult (channels,
/// locked), 10 Documentaries (movies), 11 Kids Films (movies, locked), 20 Drama (series).
fn library() -> Library {
    let groups = vec![
        group(1, ItemKind::Channel, "UK", false, false),
        group(2, ItemKind::Channel, "Hidden Stuff", true, false),
        group(3, ItemKind::Channel, "Adult", false, true),
    ];
    let channels = vec![
        channel(1, 1, "BBC One HD", &[1], false),
        channel(2, 2, "BBC Two", &[1], false),
        channel(3, 1, "ITV", &[1], false),
        channel(4, 1, "BBC Hidden Channel", &[1], false), // hidden itself
        channel(5, 1, "BBC Secret", &[2], false),         // only in a hidden group
        channel(6, 1, "BBC Late", &[3], false),           // locked via group
        channel(7, 1, "BBC Locked", &[1], true),          // locked itself
        channel(8, 1, "BBC Both", &[1, 2], false),        // one visible group → visible
    ];
    let members = HashMap::from([
        (GroupId(1), vec![ChannelId(1), ChannelId(2), ChannelId(3), ChannelId(4), ChannelId(7), ChannelId(8)]),
        (GroupId(2), vec![ChannelId(5), ChannelId(8)]),
        (GroupId(3), vec![ChannelId(6)]),
    ]);
    let live = LiveIndex::from_parts(channels, groups, members, HashSet::from([ChannelId(4)]));
    let movies = vec![
        vod(VodKind::Movie(MovieId(1)), "BBC Earth: Oceans", &[10], false),
        vod(VodKind::Movie(MovieId(2)), "Planet Earth", &[10], false),
        vod(VodKind::Movie(MovieId(3)), "Bbc Kids Movie", &[11], false),
    ];
    let series =
        vec![vod(VodKind::Series(SeriesId(1)), "Bbc Drama Hour", &[20], false), vod(VodKind::Series(SeriesId(2)), "Line of Duty", &[20], false)];
    let vod_index = VodIndex::from_parts(
        movies,
        series,
        vec![group(10, ItemKind::Movie, "Documentaries", false, false), group(11, ItemKind::Movie, "Kids Films", false, true)],
        vec![group(20, ItemKind::Series, "Drama", false, false)],
        HashMap::new(),
    );
    Library { sources: vec![source(1, "My IPTV"), source(2, "Free")], live: live.into(), vod: vod_index.into(), ..Library::default() }
}

fn index(include_locked: bool) -> SearchIndex {
    let mut idx = SearchIndex::new(Arc::new(|| {}));
    idx.rebuild(&library(), include_locked);
    idx
}

/// Tick like the overlay does until `section` yields `n` hits (or time out).
fn wait(idx: &mut SearchIndex, section: SearchSection, n: usize) -> Vec<SearchHit> {
    wait_for(idx, section, |r| r.len() == n)
}

fn wait_for(idx: &mut SearchIndex, section: SearchSection, done: impl Fn(&[SearchHit]) -> bool) -> Vec<SearchHit> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        idx.tick(10);
        let r = idx.results(section, 200);
        if done(&r) || Instant::now() > deadline {
            return r;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn targets(hits: &[SearchHit]) -> Vec<SearchTarget> {
    hits.iter().map(|h| h.item.target).collect()
}

#[test]
fn sections_split_by_kind_and_all_groups_kinds() {
    let mut idx = index(false);
    idx.set_query("bbc");
    // visible, unlocked: channels 1, 2, 8; movie 1; series 1
    let all = wait(&mut idx, SearchSection::All, 5);
    assert_eq!(all.len(), 5, "{:?}", targets(&all));
    // grouped by kind: no kind appears again after another kind started
    let kinds: Vec<u8> = all
        .iter()
        .map(|h| match h.item.target {
            SearchTarget::Channel(_) => 0,
            SearchTarget::Movie(_) => 1,
            SearchTarget::Series(_) => 2,
        })
        .collect();
    let mut seen = Vec::new();
    for k in &kinds {
        if seen.last() != Some(k) {
            assert!(!seen.contains(k), "kind {k} split: {kinds:?}");
            seen.push(*k);
        }
    }
    // inside a kind, score order
    for w in all.windows(2) {
        if kinds_eq(&w[0], &w[1]) {
            assert!(w[0].score >= w[1].score);
        }
    }

    let live = idx.results(SearchSection::Live, 200);
    let mut live_ids = targets(&live);
    live_ids.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(live_ids, vec![SearchTarget::Channel(ChannelId(1)), SearchTarget::Channel(ChannelId(2)), SearchTarget::Channel(ChannelId(8))]);
    assert_eq!(targets(&idx.results(SearchSection::Movies, 200)), vec![SearchTarget::Movie(MovieId(1))]);
    assert_eq!(targets(&idx.results(SearchSection::Series, 200)), vec![SearchTarget::Series(SeriesId(1))]);
    assert!(idx.results(SearchSection::Guide, 200).is_empty(), "guide comes from EpgSearch");
    assert_eq!(idx.results(SearchSection::All, 2).len(), 2, "limit");
}

fn kinds_eq(a: &SearchHit, b: &SearchHit) -> bool {
    std::mem::discriminant(&a.item.target) == std::mem::discriminant(&b.item.target)
}

#[test]
fn hidden_never_appear_and_locked_need_unlock() {
    let mut idx = index(false);
    idx.set_query("bbc");
    let all = wait(&mut idx, SearchSection::All, 5);
    let ids = targets(&all);
    for hidden in [ChannelId(4), ChannelId(5)] {
        assert!(!ids.contains(&SearchTarget::Channel(hidden)));
    }
    for locked in [SearchTarget::Channel(ChannelId(6)), SearchTarget::Channel(ChannelId(7)), SearchTarget::Movie(MovieId(3))] {
        assert!(!ids.contains(&locked), "{locked:?} is locked");
    }
    assert!(ids.contains(&SearchTarget::Channel(ChannelId(8))), "visible through its other group");

    // session unlocked: locked items appear, flagged; hidden still don't
    let mut idx = index(true);
    idx.set_query("bbc");
    let all = wait(&mut idx, SearchSection::All, 8);
    let ids = targets(&all);
    assert_eq!(all.len(), 8, "{ids:?}");
    for locked in [SearchTarget::Channel(ChannelId(6)), SearchTarget::Channel(ChannelId(7)), SearchTarget::Movie(MovieId(3))] {
        let hit = all.iter().find(|h| h.item.target == locked).expect("locked item listed when unlocked");
        assert!(hit.item.locked);
    }
    assert!(!ids.contains(&SearchTarget::Channel(ChannelId(4))));
    assert!(!ids.contains(&SearchTarget::Channel(ChannelId(5))));
}

#[test]
fn append_reparse_matches_a_fresh_query_and_backspace_widens_again() {
    let mut idx = index(false);
    for q in ["b", "bb", "bbc", "bbc "] {
        idx.set_query(q);
        idx.tick(10);
    }
    idx.set_query("bbc on");
    let narrowed = wait(&mut idx, SearchSection::All, 2);
    let mut fresh = index(false);
    fresh.set_query("bbc on");
    let expected = wait(&mut fresh, SearchSection::All, 2);
    assert_eq!(targets(&narrowed), targets(&expected));
    let mut got = targets(&narrowed);
    got.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(got, vec![SearchTarget::Channel(ChannelId(1)), SearchTarget::Movie(MovieId(1))]);

    // not an extension (backspace): full rescore brings items back
    idx.set_query("bbc");
    assert_eq!(wait(&mut idx, SearchSection::All, 5).len(), 5);
    assert_eq!(idx.query(), "bbc");
    // blank query: nothing
    idx.set_query("");
    idx.tick(10);
    assert!(idx.results(SearchSection::All, 200).is_empty());
}

#[test]
fn group_and_source_names_match_but_highlights_stay_in_the_name() {
    let mut idx = index(false);
    idx.set_query("documentaries");
    let hits = wait(&mut idx, SearchSection::Movies, 2);
    assert_eq!(hits.len(), 2, "both documentaries: {:?}", targets(&hits));
    let name_len = |h: &SearchHit| h.item.name.chars().count() as u32;
    for h in &hits {
        assert!(h.indices.iter().all(|i| *i < name_len(h)), "{:?}", h.indices);
        assert_eq!(h.item.group.as_deref(), Some("Documentaries"));
    }

    idx.set_query("free");
    let hits = wait(&mut idx, SearchSection::Live, 1);
    assert_eq!(targets(&hits), vec![SearchTarget::Channel(ChannelId(2))], "source name of channel 2");

    // "itv" also fuzzy-matches the source name "My IPTV"; the exact name wins
    idx.set_query("itv");
    let hits = wait_for(&mut idx, SearchSection::Live, |r| r.first().is_some_and(|h| h.item.target == SearchTarget::Channel(ChannelId(3))));
    assert_eq!(hits[0].indices, vec![0, 1, 2]);
    assert_eq!(hits[0].item.logo.as_deref(), Some("http://logos.example/3.png"));
    assert_eq!(hits[0].item.source_id, SourceId(1));
}

#[test]
fn rebuild_replaces_items() {
    let mut idx = index(false);
    idx.set_query("line of duty");
    assert_eq!(wait(&mut idx, SearchSection::Series, 1).len(), 1);
    idx.rebuild(&Library::default(), false);
    assert!(wait(&mut idx, SearchSection::Series, 0).is_empty());
    idx.rebuild(&library(), false);
    assert_eq!(wait(&mut idx, SearchSection::Series, 1).len(), 1, "query kept across rebuilds");
}

#[test]
fn notify_fires_when_results_change() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mut idx = SearchIndex::new(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    idx.rebuild(&library(), false);
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

/// [e2e after WP-02] real FTS through `store::epg::search`: debounce + stale-result drop.
#[test]
fn epg_search_against_the_store() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let store = tuner::store::Store::open_temp("tuner-search-e2e", None).unwrap();
    let now = 1_700_000_000;
    store
        .writer()
        .blocking(move |c| {
            let src = support::seed::seed_source(c, "Seed")?;
            let chans = support::seed::seed_channels(c, src, 20)?;
            support::seed::seed_epg(c, &chans, now - 3600, 6)?;
            Ok(())
        })
        .unwrap();
    let mut s = EpgSearch::new(rt.handle().clone(), store);
    let repaints = Arc::new(AtomicUsize::new(0));
    let r = repaints.clone();
    let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        r.fetch_add(1, Ordering::SeqCst);
    });
    // typed quickly: one search for the last text
    for q in ["s", "sh", "sho", "show 1"] {
        s.set_query(q, now, repaint.clone());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.results().is_empty() {
        assert!(Instant::now() < deadline, "no EPG results");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(repaints.load(Ordering::SeqCst), 1, "debounced to a single search");
    let hits = s.results();
    assert!(hits.iter().all(|h| fts_matches(&h.programme.title, "show 1")), "{hits:?}");
    assert!(hits.iter().all(|h| !h.channel_ids.is_empty()), "joined to playlist channels");
    // airing first
    assert!(hits[0].programme.start <= now && now < hits[0].programme.stop);

    // a newer query supersedes; results switch to it
    s.set_query("show 16", now, repaint.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    while repaints.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "no results for the newer query");
        std::thread::sleep(Duration::from_millis(10));
    }
    let hits = s.results();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| fts_matches(&h.programme.title, "show 16")), "{hits:?}");
    s.set_query("", now, repaint);
    assert!(s.results().is_empty());
}

/// FTS5 semantics of the store's query (each token quoted, the last one a prefix): every query
/// token but the last equals a title token; the last is a prefix of one.
fn fts_matches(title: &str, query: &str) -> bool {
    let words: Vec<String> = title.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect();
    let q: Vec<&str> = query.split_whitespace().collect();
    let Some((last, rest)) = q.split_last() else { return false };
    rest.iter().all(|t| words.iter().any(|w| w == t)) && words.iter().any(|w| w.starts_with(last))
}
