//! import benchmark. Owner: WP-06. Gate: end-to-end 100k M3U < 5 s, 415k programmes < 8 s (WP-13 re-runs as release gate).
//! Run: `cargo bench --bench import` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! End to end = what a refresh job does, minus the network: a local-file M3U source through
//! `jobs::import_source::refresh_source` (parse on the blocking pool → 2 000-row writer batches →
//! `finish_catalogue`) and a local-file XMLTV guide through `jobs::import_epg::refresh_epg`
//! (gunzip + parse → 5 000-row batches → matching → flip → sweep), each into a fresh temp WAL
//! database. Uses `target/big-fixtures/big.m3u` / `big.xml.gz` (or `$TUNER_BIG_FIXTURES`) when
//! present, otherwise equivalent datasets generated into a temp dir. One timed pass of each is
//! printed first (the gate numbers), then criterion samples them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, SamplingMode, criterion_group, criterion_main};
use flate2::Compression;
use flate2::write::GzEncoder;
use tokio_util::sync::CancellationToken;
use tuner::config::{Config, SharedConfig};
use tuner::jobs::{JobContext, Progress, import_epg, import_source};
use tuner::model::{EpgSourceId, EpgSourceKind, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings};
use tuner::msg::EventBus;
use tuner::paths::AppPaths;
use tuner::sources::http::{HttpClient, HttpOptions};
use tuner::store::Store;

const CHANNELS: usize = 100_000;
const EPG_CHANNELS: usize = 1_235;
const EPG_DAYS: usize = 7;
const M3U_GATE_S: f64 = 5.0;
const EPG_GATE_S: f64 = 8.0;

struct Datasets {
    _tmp: Option<tempfile::TempDir>,
    m3u: PathBuf,
    xmltv: PathBuf,
}

fn datasets() -> Datasets {
    let dir = std::env::var_os("TUNER_BIG_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/big-fixtures"));
    let (m3u, xmltv) = (dir.join("big.m3u"), dir.join("big.xml.gz"));
    if m3u.exists() && xmltv.exists() {
        return Datasets { _tmp: None, m3u, xmltv };
    }
    eprintln!("{} not found (run scripts/gen-big-fixtures.py); generating equivalent datasets", dir.display());
    let tmp = tempfile::tempdir().expect("tempdir");
    let m3u = tmp.path().join("big.m3u");
    let xmltv = tmp.path().join("big.xml.gz");
    std::fs::write(&m3u, synthetic_m3u(CHANNELS)).expect("write m3u");
    std::fs::write(&xmltv, synthetic_xmltv_gz(EPG_CHANNELS, EPG_DAYS)).expect("write xmltv");
    Datasets { _tmp: Some(tmp), m3u, xmltv }
}

/// Same shapes as the generator: groups, `;` lists, tvg ids shared with the guide, catch-up.
fn synthetic_m3u(n: usize) -> Vec<u8> {
    let mut out = String::with_capacity(n * 260);
    out.push_str("#EXTM3U url-tvg=\"http://127.0.0.1/big.xml.gz\"\n");
    for i in 0..n {
        let mut g = format!("Group {}", (i * 7919) % 400);
        if i % 50 == 0 {
            g.push_str(&format!(";Group {}", (i * 104_729) % 400));
        }
        let catchup = if i % 211 == 0 { " catchup=\"append\" catchup-days=\"3\" catchup-source=\"?utc={utc}\"" } else { "" };
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"ch{}.tv\" tvg-name=\"Channel {i}\" tvg-chno=\"{}\" tvg-logo=\"http://logo.invalid/{i}.png\" group-title=\"{g}\"{catchup},UK | Channel {i} HD\nhttp://127.0.0.1/live/u/p/{i}.ts\n",
            i % EPG_CHANNELS,
            i + 1
        ));
    }
    out.into_bytes()
}

/// `channels × days × 48` half-hour programmes from 2026-10-09 (the generator's defaults).
fn synthetic_xmltv_gz(channels: usize, days: usize) -> Vec<u8> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    let mut w = |s: &str| gz.write_all(s.as_bytes()).expect("in-memory write");
    w("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"tuner-bench\">\n");
    for c in 0..channels {
        w(&format!("<channel id=\"ch{c}.tv\"><display-name>Channel {c}</display-name></channel>\n"));
    }
    let start = jiff::civil::date(2026, 10, 9).at(0, 0, 0, 0);
    for c in 0..channels {
        let mut t = start;
        for i in 0..days * 48 {
            let e = t + jiff::Span::new().minutes(30);
            w(&format!(
                "<programme start=\"{} +0000\" stop=\"{} +0000\" channel=\"ch{c}.tv\"><title lang=\"en\">Show {} &amp; friends</title><desc>Lorem ipsum dolor sit amet.</desc></programme>\n",
                t.strftime("%Y%m%d%H%M%S"),
                e.strftime("%Y%m%d%H%M%S"),
                (c * 7919 + i * 104_729) % 100_000,
            ));
            t = e;
        }
    }
    w("</tv>\n");
    gz.finish().expect("gzip")
}

/// A fresh database + job context (the temp dir lives as long as the context's store).
fn context(root: &Path) -> JobContext {
    let paths = AppPaths::rooted(root);
    paths.ensure().expect("paths");
    let (bus, _rx) = EventBus::new(None);
    let store = Store::open_temp("tuner-bench-import", None).expect("store");
    let http = HttpClient::new(&HttpOptions::default()).expect("http");
    let mut cfg = Config::default();
    cfg.general.epg_keep_past_days = 36_500; // the datasets are dated 2026-10-09
    let config: SharedConfig = Arc::new(RwLock::new(Arc::new(cfg)));
    JobContext::new(store, http, bus, paths, config)
}

fn m3u_source(ctx: &JobContext, path: &Path) -> SourceId {
    let settings = SourceSettings { use_provider_epg: Some(false), ..Default::default() };
    let draft = SourceDraft {
        name: "Bench".into(),
        kind: SourceKind::M3uFile,
        icon: String::new(),
        color: SourceColor::Accent,
        url: None,
        path: Some(path.to_path_buf()),
        xtream: None,
        settings,
    };
    ctx.store.writer().blocking(move |c| tuner::store::sources::insert_source(c, &draft, 0)).expect("source")
}

fn epg_source(ctx: &JobContext, path: &Path) -> EpgSourceId {
    let kind = EpgSourceKind::File { path: path.to_path_buf() };
    ctx.store.writer().blocking(move |c| tuner::store::sources::insert_epg_source(c, "Bench guide", &kind, 0)).expect("epg source")
}

fn import_m3u(rt: &tokio::runtime::Runtime, ctx: &JobContext, source: SourceId) -> u32 {
    let r = rt
        .block_on(import_source::refresh_source(ctx, source, true, &Progress::detached(), &CancellationToken::new()))
        .expect("m3u import");
    r.summary.channels.total
}

fn import_xmltv(rt: &tokio::runtime::Runtime, ctx: &JobContext, id: EpgSourceId) -> u32 {
    let r = rt.block_on(import_epg::refresh_epg(ctx, id, true, &Progress::detached(), &CancellationToken::new())).expect("xmltv import");
    r.summary.programmes
}

fn bench(c: &mut Criterion) {
    let data = datasets();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build().expect("runtime");
    let root = tempfile::tempdir().expect("tempdir");
    let note = if cfg!(debug_assertions) { ", debug build: not comparable" } else { "" };

    // the gate numbers: one pass each into a fresh database
    let ctx = context(root.path());
    let source = m3u_source(&ctx, &data.m3u);
    let t = Instant::now();
    let n = import_m3u(&rt, &ctx, source);
    eprintln!("import: {n} channels from {} in {:.2} s (gate {M3U_GATE_S} s{note})", data.m3u.display(), t.elapsed().as_secs_f64());
    let guide = epg_source(&ctx, &data.xmltv);
    let t = Instant::now();
    let n = import_xmltv(&rt, &ctx, guide);
    eprintln!("import: {n} programmes from {} in {:.2} s (gate {EPG_GATE_S} s{note})", data.xmltv.display(), t.elapsed().as_secs_f64());

    let mut g = c.benchmark_group("import");
    g.sampling_mode(SamplingMode::Flat);
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));
    g.bench_function("m3u_100k_fresh_db", |b| {
        b.iter_batched(
            || {
                let ctx = context(root.path());
                let id = m3u_source(&ctx, &data.m3u);
                (ctx, id)
            },
            |(ctx, id)| (import_m3u(&rt, &ctx, id), ctx), // the store is dropped outside the timing
            BatchSize::PerIteration,
        )
    });
    // re-import of an unchanged playlist (every row an update; nothing swept)
    g.bench_function("m3u_100k_reimport", |b| b.iter(|| import_m3u(&rt, &ctx, source)));
    g.bench_function("xmltv_415k_fresh_db", |b| {
        b.iter_batched(
            || {
                let ctx = context(root.path());
                let id = epg_source(&ctx, &data.xmltv);
                (ctx, id)
            },
            |(ctx, id)| (import_xmltv(&rt, &ctx, id), ctx),
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
