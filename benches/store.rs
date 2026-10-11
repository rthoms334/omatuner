//! store benchmark. Owner: WP-02. Gate: 100k-channel import < 3 s; window query < 1 ms; library::load 100k < 400 ms (seeded DB).
//! Run: `cargo bench --bench store` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! Every dataset is generated in-process into a temp WAL database (`Store::open_temp`): the
//! import benches go through `store::import` exactly like `jobs::import_source` (one writer job
//! per 2 000-row batch); the read benches use the raw-SQL seeders of `tests/support/seed.rs`.

#[path = "../tests/support/seed.rs"]
mod seed;

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, SamplingMode, criterion_group, criterion_main};
use tuner::model::{EpgChannelId, GroupKey, ItemKey, ItemKind, SourceId, StreamLocator, Timestamp};
use tuner::sources::identity::{group_key, normalize_name, url_key};
use tuner::store::import::{self, ImportChannel, ImportGroup};
use tuner::store::{Store, epg, library};

const CHANNELS: usize = 100_000;
const BATCH: usize = 2_000;
const GROUPS: usize = 250;
const T0: Timestamp = 1_791_576_000;

fn groups() -> Vec<ImportGroup> {
    (0..GROUPS)
        .map(|i| {
            let name = format!("Country {} | Category {}", i / 10, i % 10);
            ImportGroup {
                kind: ItemKind::Channel,
                key: group_key(ItemKind::Channel, &name),
                name,
                provider_id: Some(i.to_string()),
                provider_order: i as i32,
            }
        })
        .collect()
}

/// A realistic Xtream-like live catalogue: every channel in one group, 5 % in a second one.
fn channels(n: usize, groups: &[ImportGroup]) -> Vec<ImportChannel> {
    (0..n)
        .map(|i| {
            let name = format!("UK: Channel {i} HD");
            let url = format!("http://panel.test:8080/live/user/pass/{i}.ts");
            let mut member: Vec<GroupKey> = vec![groups[i % groups.len()].key.clone()];
            if i % 20 == 0 {
                member.push(groups[(i / 20) % groups.len()].key.clone());
                member.dedup();
            }
            ImportChannel {
                key: ItemKey(format!("xt:live:{i}")),
                provider_id: Some(i.to_string()),
                clean_name: format!("Channel {i}"),
                norm_name: normalize_name(&name),
                name,
                number: Some(i as u32 + 1),
                logo: Some(format!("http://img.test/logo/{i}.png")),
                tvg_id: Some(format!("channel{i}.uk")),
                tvg_name: None,
                tvg_shift_min: 0,
                url_key: url_key(&url),
                stream: StreamLocator::Url { url },
                headers: None,
                catchup: None,
                radio: false,
                unsupported: None,
                provider_order: i as i32,
                added_at: Some(T0 - i as i64),
                groups: member,
                quality: None,
            }
        })
        .collect()
}

fn new_source(store: &Store) -> SourceId {
    store.writer().blocking(|c| seed::seed_source(c, "Bench")).expect("source")
}

/// One catalogue import as `jobs::import_source` drives it: one writer job per batch.
fn import(store: &Store, src: SourceId, groups: &[ImportGroup], chans: &[ImportChannel], now: Timestamp) {
    let w = store.writer();
    let g = groups.to_vec();
    let imp = w
        .blocking(move |c| {
            let imp = import::begin_catalogue(c, src, now)?;
            import::upsert_groups(c, &imp, &g)?;
            Ok(imp)
        })
        .expect("begin");
    for batch in chans.chunks(BATCH) {
        let (imp, batch) = (imp.clone(), batch.to_vec());
        w.blocking(move |c| import::write_channels(c, &imp, &batch)).expect("batch");
    }
    let summary = w.blocking(move |c| import::finish_catalogue(c, &imp, &[ItemKind::Channel], now)).expect("finish");
    assert_eq!(summary.channels.total as usize, chans.len());
}

fn bench_imports(c: &mut Criterion) {
    let groups = groups();
    let chans = channels(CHANNELS, &groups);
    let mut g = c.benchmark_group("store_import");
    g.sample_size(10).sampling_mode(SamplingMode::Flat).measurement_time(Duration::from_secs(40)).warm_up_time(Duration::from_secs(1));
    // gate: < 3 s release
    g.bench_function("import_100k_channels_fresh", |b| {
        b.iter_batched(
            || {
                let s = Store::open_temp("tuner-bench-import", None).expect("store");
                let src = new_source(&s);
                (s, src)
            },
            |(s, src)| import(&s, src, &groups, &chans, T0),
            BatchSize::PerIteration,
        )
    });
    // the common daily refresh: same catalogue, every row id kept
    let s = Store::open_temp("tuner-bench-reimport", None).expect("store");
    let src = new_source(&s);
    import(&s, src, &groups, &chans, T0);
    let mut now = T0;
    g.bench_function("reimport_100k_channels_unchanged", |b| {
        b.iter(|| {
            now += 1;
            import(&s, src, &groups, &chans, now)
        })
    });
    g.finish();
}

fn bench_epg_reads(c: &mut Criterion) {
    // 5 000 channels × 24 h of 30-minute programmes = 240k programmes (+ FTS rows)
    let s = Store::open_temp("tuner-bench-epg", None).expect("store");
    let ids = s
        .writer()
        .blocking(|c| {
            let src = seed::seed_source(c, "Bench")?;
            let ids = seed::seed_channels(c, src, 5_000)?;
            seed::seed_epg(c, &ids, T0, 24)?;
            Ok(ids)
        })
        .expect("seed");
    let conn = s.reader().expect("reader");
    let epg_ids: Vec<EpgChannelId> = ids.iter().map(|ch| epg::epg_channel_for(&conn, *ch).expect("query").expect("mapped")).collect();
    let mut g = c.benchmark_group("store_epg");
    let mut i = 0usize;
    // gate: < 1 ms
    g.bench_function("window_6h", |b| {
        b.iter(|| {
            i = (i + 7919) % epg_ids.len();
            let from = T0 + 6 * 3600 + (i as i64 % 12) * 1800;
            black_box(epg::window(&conn, epg_ids[i], from, from + 6 * 3600).expect("window"))
        })
    });
    let keys: Vec<(EpgChannelId, i32)> = epg_ids.iter().step_by(100).map(|e| (*e, 0)).collect();
    g.bench_function("now_next_50_channels", |b| b.iter(|| black_box(epg::now_next(&conn, &keys, T0 + 12 * 3600 + 600).expect("now_next"))));
    g.bench_function("search_title_prefix", |b| b.iter(|| black_box(epg::search(&conn, "show 1", T0 + 12 * 3600, 50).expect("search"))));
    g.finish();
}

fn bench_library_load(c: &mut Criterion) {
    // 100k channels + 50k movies + 20k series (the §5.4 budget)
    let s = Store::open_temp("tuner-bench-library", None).expect("store");
    s.writer()
        .blocking(|c| {
            let src = seed::seed_source(c, "Bench")?;
            seed::seed_channels(c, src, CHANNELS)?;
            seed::seed_movies(c, src, 50_000)?;
            seed::seed_series_with_episodes(c, src, 20_000, 0, 0)?;
            Ok(())
        })
        .expect("seed");
    let conn = s.reader().expect("reader");
    let mut g = c.benchmark_group("store_library");
    g.sample_size(10).sampling_mode(SamplingMode::Flat).measurement_time(Duration::from_secs(10));
    // gate: < 400 ms release
    g.bench_function("load_100k_channels_50k_movies_20k_series", |b| {
        b.iter(|| {
            let lib = library::load(&conn).expect("load");
            assert_eq!(lib.live.channels.len(), CHANNELS);
            black_box(lib)
        })
    });
    g.bench_function("load_favs", |b| b.iter(|| black_box(library::load_favs(&conn).expect("favs"))));
    g.finish();
}

criterion_group!(benches, bench_imports, bench_epg_reads, bench_library_load);
criterion_main!(benches);
