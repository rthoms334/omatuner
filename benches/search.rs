//! search benchmark. Owner: WP-09. Gate: 170k-item global search tick < 16 ms; ListFilter 20k rows < 5 ms.
//! Run: `cargo bench --bench search` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! The library is synthesized in memory (100k channels + 50k movies + 20k series, realistic
//! names/groups) so the bench needs no generated files.

use std::collections::{HashMap, HashSet};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use tuner::model::{
    ChannelId, ChannelSummary, Group, GroupId, GroupKey, ItemKey, ItemKind, MovieId, SeriesId, SourceColor, SourceId, SourceKind, VodKind, VodSummary,
};
use tuner::search::{ListFilter, SearchIndex, SearchSection};
use tuner::store::library::{Library, LiveIndex, SourceMeta, VodIndex};

const CHANNELS: usize = 100_000;
const MOVIES: usize = 50_000;
const SERIES: usize = 20_000;

const WORDS: [&str; 24] = [
    "News", "Sport", "Movies", "Kids", "Music", "Drama", "Comedy", "Action", "World", "Live", "Cinema", "Premium", "Family", "Nature", "History",
    "Science", "Travel", "Food", "Arena", "Max", "Gold", "Plus", "Classic", "Prime",
];
const BRANDS: [&str; 12] = ["BBC", "ITV", "Sky", "CNN", "ESPN", "HBO", "Canal+", "RTL", "TF1", "Rai", "ZDF", "Fox"];

fn group(id: i64, kind: ItemKind, name: &str) -> Group {
    Group {
        id: GroupId(id),
        source_id: SourceId(1),
        kind,
        key: GroupKey(format!("g{id}")),
        name: name.into(),
        rename: None,
        provider_order: id as i32,
        user_order: None,
        hidden: false,
        user_hidden: false,
        locked: false,
        item_count: 0,
        provider_id: None,
        tag: None,
        filter: Default::default(),
    }
}

fn library() -> Library {
    let channels: Vec<ChannelSummary> = (0..CHANNELS)
        .map(|i| ChannelSummary {
            id: ChannelId(i as i64),
            source_id: SourceId(1 + (i % 2) as i64),
            key: ItemKey(format!("c{i}")),
            name: Arc::from(format!("{} {} {} HD", BRANDS[i % BRANDS.len()], WORDS[(i / 7) % WORDS.len()], i % 500)),
            number: Some(i as u32 + 1),
            logo: None,
            group_ids: vec![GroupId((i % 200) as i64)],
            epg_channel_id: None,
            tvg_shift_min: 0,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: i as i32,
            user_order: None,
            added_at: None,
        })
        .collect();
    let groups = (0..200).map(|g| group(g, ItemKind::Channel, &format!("UK | {} {g}", WORDS[g as usize % WORDS.len()]))).collect();
    let vod = |kind: VodKind, i: usize| VodSummary {
        kind,
        source_id: SourceId(1),
        key: ItemKey(format!("v{i}")),
        name: Arc::from(format!("The {} {} {} ({})", WORDS[i % WORDS.len()], WORDS[(i / 3) % WORDS.len()], i, 1950 + i % 75)),
        year: Some(1950 + (i % 75) as u16),
        poster: None,
        rating: None,
        group_ids: vec![GroupId(1000 + (i % 50) as i64)],
        added_at: None,
        provider_order: i as i32,
        locked: false,
    };
    let movies = (0..MOVIES).map(|i| vod(VodKind::Movie(MovieId(i as i64)), i)).collect();
    let series = (0..SERIES).map(|i| vod(VodKind::Series(SeriesId(i as i64)), i + MOVIES)).collect();
    let vod_groups = (1000..1050).map(|g| group(g, ItemKind::Movie, &format!("VOD | {}", WORDS[g as usize % WORDS.len()]))).collect();
    let source = |id: i64, name: &str| SourceMeta {
        id: SourceId(id),
        name: name.into(),
        icon: String::new(),
        color: SourceColor::Accent,
        kind: SourceKind::Xtream,
        enabled: true,
        last_refresh: None,
        has_vod: true,
        has_series: true,
        category_filter: None,
    };
    Library {
        sources: vec![source(1, "My IPTV"), source(2, "Backup")],
        live: LiveIndex::from_parts(channels, groups, HashMap::new(), HashSet::new()).into(),
        vod: VodIndex::from_parts(movies, series, vod_groups, vec![], HashMap::new()).into(),
        ..Library::default()
    }
}

/// Tick (as the UI does, 10 ms per frame) until `done` or 30 s.
fn settle(idx: &mut SearchIndex, done: impl Fn(&SearchIndex) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(idx) && Instant::now() < deadline {
        idx.tick(10);
    }
}

fn bench_global(c: &mut Criterion) {
    let lib = library();
    let mut idx = SearchIndex::new(Arc::new(|| {}));

    c.bench_function("search/rebuild_170k (UI-thread part)", |b| {
        b.iter(|| idx.rebuild(black_box(&lib), false));
    });

    // wait until the background injection is complete: the last series is findable
    idx.rebuild(&lib, false);
    let last = &lib.vod.series.last().expect("series").name;
    idx.set_query(last);
    settle(&mut idx, |i| i.results(SearchSection::Series, 1).first().is_some_and(|h| &h.item.name == last));
    assert!(!idx.results(SearchSection::Series, 1).is_empty(), "injection did not finish");

    // one frame's tick right after a keystroke (the per-frame gate: < 16 ms)
    let queries = ["b", "bb", "bbc", "bbc n", "bbc ne", "bbc new", "sky", "sky sp", "the dr", "x"];
    let mut q = 0usize;
    c.bench_function("search/tick_after_keystroke_170k", |b| {
        b.iter(|| {
            idx.set_query(queries[q % queries.len()]);
            q += 1;
            black_box(idx.tick(10))
        });
    });

    // full latency from keystroke to final results (informational)
    c.bench_function("search/keystroke_to_results_170k", |b| {
        b.iter(|| {
            idx.set_query(queries[q % queries.len()]);
            q += 1;
            for _ in 0..200 {
                if idx.tick(10) {
                    break;
                }
            }
        });
    });

    idx.set_query("bbc news");
    settle(&mut idx, |i| !i.results(SearchSection::All, 1).is_empty());
    c.bench_function("search/results_top200_all_sections", |b| {
        b.iter(|| {
            for s in [SearchSection::All, SearchSection::Live, SearchSection::Movies, SearchSection::Series] {
                black_box(idx.results(s, 200));
            }
        });
    });
}

fn bench_list_filter(c: &mut Criterion) {
    let rows: Vec<String> = (0..20_000).map(|i| format!("{} {} {} {}", i + 1, BRANDS[i % BRANDS.len()], WORDS[i % WORDS.len()], i % 97)).collect();
    let mut f = ListFilter::default();
    for ch in "sky spo".chars() {
        f.push(ch);
    }
    c.bench_function("list_filter/20k_rows", |b| b.iter(|| black_box(f.apply(rows.iter().map(String::as_str)))));
    let mut f = ListFilter::default();
    for ch in "120".chars() {
        f.push(ch);
    }
    c.bench_function("list_filter/20k_rows_number", |b| b.iter(|| black_box(f.apply(rows.iter().map(String::as_str)))));
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).measurement_time(Duration::from_secs(4));
    targets = bench_global, bench_list_filter
}
criterion_main!(benches);
