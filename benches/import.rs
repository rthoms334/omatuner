//! import benchmark. Owner: WP-06 (pipelines), gates re-run by WP-13 as the release gate.
//! Gates (DESIGN §9.1/§9.3, §24.2 WP-13; release build): 100k-channel M3U end to end < 5 s;
//! 415k-programme XMLTV end to end < 8 s.
//! Run: `cargo bench --bench import` (harness = false). Prints one PASS/FAIL line per gate and
//! exits non-zero when one fails. `TUNER_CRITERION=1` additionally samples the bare pipelines
//! (local files, no job system) with criterion.
//!
//! End to end = what the app does, through the real `JobManager` and its follow-ups, from a
//! local HTTP server (`tests/support/static_http.rs`, 127.0.0.1) into a WAL database on disk:
//! * M3U: `RefreshSource` → download → sniff → parse (blocking pool) → 2 000-row writer
//!   batches → `finish_catalogue` → follow-up `RebuildLibrary` → `Library` delivered;
//! * XMLTV: `RefreshEpg` on a `.xml.gz` URL (served gzip bytes, like real hosts) → download →
//!   gunzip + parse → 5 000-row batches → match against the 100k playlist channels → flip →
//!   sweep → then the `RebuildLibrary` the app runs on `DataScope::Epg`.
//!
//! Each is timed fresh (empty tables) and again as a forced re-import (the daily refresh:
//! every row updated, or a new guide generation with the old one swept). Each case is run
//! [`RUNS`] times (`TUNER_BENCH_RUNS=n` to change); the gate uses the median.
//!
//! Datasets: `target/big-fixtures/big.m3u` / `big.xml.gz` from `scripts/gen-big-fixtures.py`
//! (or `$TUNER_BIG_FIXTURES`) when present, otherwise equivalent deterministic datasets
//! generated in memory here. Nothing is committed.

#[path = "../tests/support/static_http.rs"]
mod static_http;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use flate2::Compression;
use flate2::write::GzEncoder;
use static_http::StaticServer;
use tokio_util::sync::CancellationToken;
use tuner::config::{Config, SharedConfig};
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager, JobOutcome, JobPhase, Progress, import_epg, import_source};
use tuner::model::{EpgSourceId, EpgSourceKind, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings};
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::sources::http::{HttpClient, HttpOptions};
use tuner::store::Store;

const CHANNELS: usize = 100_000;
const EPG_CHANNELS: usize = 1_235;
const EPG_DAYS: usize = 7;
const RUNS: usize = 3;
const M3U_GATE: Duration = Duration::from_secs(5);
const EPG_GATE: Duration = Duration::from_secs(8);

struct Datasets {
    label: String,
    m3u: Vec<u8>,
    xmltv_gz: Vec<u8>,
}

fn datasets() -> Datasets {
    let dir = std::env::var_os("TUNER_BIG_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/big-fixtures"));
    if let (Ok(m3u), Ok(xmltv_gz)) = (std::fs::read(dir.join("big.m3u")), std::fs::read(dir.join("big.xml.gz"))) {
        return Datasets { label: format!("{} (gen-big-fixtures.py)", dir.display()), m3u, xmltv_gz };
    }
    Datasets {
        label: "generated in memory".into(),
        m3u: synthetic_m3u(CHANNELS),
        xmltv_gz: synthetic_xmltv_gz(EPG_CHANNELS, EPG_DAYS),
    }
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
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
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

fn shared_config() -> SharedConfig {
    let mut cfg = Config::default();
    cfg.general.epg_keep_past_days = 36_500; // the datasets are dated 2026-10-09
    Arc::new(RwLock::new(Arc::new(cfg)))
}

/// A fresh profile + database + job manager, like `Services::start` builds them.
struct Env {
    _dir: tempfile::TempDir,
    store: Store,
    jobs: JobManager,
    events: Receiver<AppEvent>,
    /// Phase changes of the jobs waited on: (phase name, when it was first reported).
    phases: std::cell::RefCell<Vec<(&'static str, Instant)>>,
}

impl Env {
    fn new(rt: &tokio::runtime::Runtime) -> Self {
        let dir = tempfile::Builder::new().prefix("tuner-bench-import").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
        let paths = AppPaths::rooted(dir.path());
        paths.ensure().expect("paths");
        let (bus, events) = EventBus::new(None);
        let store = Store::open(&paths.db_file(), Some(bus.clone())).expect("store");
        let http = HttpClient::new(&HttpOptions::default()).expect("http");
        let jobs = JobManager::new(JobContext::new(store.clone(), http, bus, paths, shared_config()), rt.handle().clone());
        Self { _dir: dir, store, jobs, events, phases: Default::default() }
    }

    /// Wait for the `Finished` event of a job matching `pred`; other events are dropped.
    fn wait(&self, what: &str, pred: impl Fn(&JobKind) -> bool) -> JobOutcome {
        loop {
            match self.events.recv_timeout(Duration::from_secs(120)) {
                Ok(AppEvent::Job(JobEvent::Finished { kind, outcome, .. })) if pred(&kind) => {
                    return outcome.unwrap_or_else(|e| panic!("{what}: {e}"));
                }
                Ok(AppEvent::Job(JobEvent::Updated(st))) => {
                    let name = phase_name(&st.phase);
                    let mut phases = self.phases.borrow_mut();
                    if phases.last().is_none_or(|(p, _)| *p != name) {
                        phases.push((name, Instant::now()));
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("{what}: {e}; active: {:?}", self.jobs.active()),
            }
        }
    }

    fn m3u_source(&self, url: String) -> SourceId {
        let settings = SourceSettings { use_provider_epg: Some(false), ..Default::default() };
        let draft = SourceDraft {
            name: "Bench".into(),
            kind: SourceKind::M3uUrl,
            icon: String::new(),
            color: SourceColor::Accent,
            url: Some(url),
            path: None,
            xtream: None,
            settings,
        };
        self.store.writer().blocking(move |c| tuner::store::sources::insert_source(c, &draft, 0)).expect("source")
    }

    fn epg_source(&self, url: String) -> EpgSourceId {
        let kind = EpgSourceKind::Url { url: url.into() };
        self.store.writer().blocking(move |c| tuner::store::sources::insert_epg_source(c, "Bench guide", &kind, 0)).expect("epg source")
    }

    /// `RefreshSource` → its `RebuildLibrary` follow-up; (channels, elapsed).
    fn import_m3u(&self, source: SourceId) -> (u32, Duration) {
        let t = Instant::now();
        self.jobs.submit(JobKind::RefreshSource { source, force: true });
        let channels = match self.wait("m3u refresh", |k| matches!(k, JobKind::RefreshSource { .. })) {
            JobOutcome::SourceRefreshed { summary, .. } => summary.channels.total,
            other => panic!("{other:?}"),
        };
        match self.wait("library", |k| *k == JobKind::RebuildLibrary) {
            JobOutcome::Library(lib) => assert_eq!(lib.live.channels.len(), channels as usize),
            other => panic!("{other:?}"),
        }
        (channels, t.elapsed())
    }

    /// `RefreshEpg` → the app's `RebuildLibrary`; (programmes, mapped channels, import, total).
    /// Prints when each job phase was first reported (finishing = flip + sweep of the old
    /// generation).
    fn import_xmltv(&self, id: EpgSourceId) -> (u32, u32, Duration, Duration) {
        self.phases.borrow_mut().clear();
        let t = Instant::now();
        self.jobs.submit(JobKind::RefreshEpg { epg_source: id, force: true });
        let summary = match self.wait("xmltv refresh", |k| matches!(k, JobKind::RefreshEpg { .. })) {
            JobOutcome::EpgRefreshed { summary, .. } => summary,
            other => panic!("{other:?}"),
        };
        let import = t.elapsed();
        let steps: Vec<String> = self.phases.borrow().iter().map(|(p, at)| format!("{p} @{:.2}", (*at - t).as_secs_f64())).collect();
        println!("  xmltv phases (s): {} | done @{:.2}", steps.join(", "), import.as_secs_f64());
        self.jobs.submit(JobKind::RebuildLibrary);
        match self.wait("library", |k| *k == JobKind::RebuildLibrary) {
            JobOutcome::Library(lib) => std::hint::black_box(lib),
            other => panic!("{other:?}"),
        };
        (summary.programmes, summary.mapped_channels, import, t.elapsed())
    }
}

fn phase_name(p: &JobPhase) -> &'static str {
    match p {
        JobPhase::Queued => "queued",
        JobPhase::Connecting => "connecting",
        JobPhase::Downloading { .. } => "downloading",
        JobPhase::Parsing { .. } => "parsing",
        JobPhase::Writing { .. } => "writing",
        JobPhase::Matching => "matching",
        JobPhase::Indexing => "indexing",
        JobPhase::Finishing => "finishing",
    }
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn secs(d: Duration) -> String { format!("{:.2} s", d.as_secs_f64()) }

fn gate(name: &str, value: Duration, limit: Duration) -> bool {
    let ok = value < limit;
    println!("gate {name:<28} {:>7.2} s   (< {} s)   {}", value.as_secs_f64(), limit.as_secs(), if ok { "PASS" } else { "FAIL" });
    ok
}

fn main() {
    let data = datasets();
    println!(
        "import: datasets {}: M3U {:.1} MB, XMLTV {:.1} MB gzip{}",
        data.label,
        data.m3u.len() as f64 / 1e6,
        data.xmltv_gz.len() as f64 / 1e6,
        if cfg!(debug_assertions) { " — debug build: numbers not comparable" } else { "" }
    );
    let server = StaticServer::start();
    server.put("/big.m3u", data.m3u.clone(), "audio/x-mpegurl");
    server.put("/big.xml.gz", data.xmltv_gz.clone(), "application/gzip");
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).thread_name("tuner-rt").enable_all().build().expect("runtime");

    let (mut m3u_fresh, mut m3u_again, mut epg_fresh, mut epg_again) = (vec![], vec![], vec![], vec![]);
    let runs = std::env::var("TUNER_BENCH_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(RUNS).max(1);
    for run in 0..runs {
        let env = Env::new(&rt);
        let source = env.m3u_source(server.url("/big.m3u"));
        let (n, fresh) = env.import_m3u(source);
        let (_, again) = env.import_m3u(source);
        let guide = env.epg_source(server.url("/big.xml.gz"));
        let (p, mapped, import, total) = env.import_xmltv(guide);
        let (_, _, import2, total2) = env.import_xmltv(guide);
        println!(
            "run {run}: m3u {n} channels: fresh {}, re-import {} | xmltv {p} programmes, {mapped} channels mapped: fresh {} (+ library {}), re-import {} (+ library {})",
            secs(fresh),
            secs(again),
            secs(import),
            secs(total - import),
            secs(import2),
            secs(total2 - import2)
        );
        m3u_fresh.push(fresh);
        m3u_again.push(again);
        epg_fresh.push(total);
        epg_again.push(total2);
        drop(env);
    }
    let ok = [
        gate("m3u 100k e2e (fresh)", median(&mut m3u_fresh), M3U_GATE),
        gate("m3u 100k e2e (re-import)", median(&mut m3u_again), M3U_GATE),
        gate("xmltv 415k e2e (fresh)", median(&mut epg_fresh), EPG_GATE),
        gate("xmltv 415k e2e (re-import)", median(&mut epg_again), EPG_GATE),
    ];
    if std::env::var_os("TUNER_CRITERION").is_some() {
        criterion_pipelines(&rt, &data);
    }
    if ok.contains(&false) {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------------------
// optional criterion sampling of the bare pipelines (local files, no job manager)

fn criterion_pipelines(rt: &tokio::runtime::Runtime, data: &Datasets) {
    use criterion::{BatchSize, Criterion, SamplingMode};

    let files = tempfile::Builder::new().prefix("tuner-bench-files").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let (m3u, xmltv) = (files.path().join("big.m3u"), files.path().join("big.xml.gz"));
    std::fs::write(&m3u, &data.m3u).expect("write m3u");
    std::fs::write(&xmltv, &data.xmltv_gz).expect("write xmltv");
    let root = files.path().join("profile");

    let context = |root: &Path| {
        let paths = AppPaths::rooted(root);
        paths.ensure().expect("paths");
        let (bus, _rx) = EventBus::new(None);
        let store = Store::open_temp("tuner-bench-import", None).expect("store");
        let http = HttpClient::new(&HttpOptions::default()).expect("http");
        JobContext::new(store, http, bus, paths, shared_config())
    };
    let file_source = |ctx: &JobContext| {
        let draft = SourceDraft {
            name: "Bench".into(),
            kind: SourceKind::M3uFile,
            icon: String::new(),
            color: SourceColor::Accent,
            url: None,
            path: Some(m3u.clone()),
            xtream: None,
            settings: SourceSettings { use_provider_epg: Some(false), ..Default::default() },
        };
        ctx.store.writer().blocking(move |c| tuner::store::sources::insert_source(c, &draft, 0)).expect("source")
    };
    let file_guide = |ctx: &JobContext| {
        let kind = EpgSourceKind::File { path: xmltv.clone() };
        ctx.store.writer().blocking(move |c| tuner::store::sources::insert_epg_source(c, "Bench guide", &kind, 0)).expect("epg source")
    };
    let run_m3u = |ctx: &JobContext, id| {
        rt.block_on(import_source::refresh_source(ctx, id, true, &Progress::detached(), &CancellationToken::new())).expect("m3u import").summary.channels.total
    };
    let run_xmltv = |ctx: &JobContext, id| {
        rt.block_on(import_epg::refresh_epg(ctx, id, true, &Progress::detached(), &CancellationToken::new())).expect("xmltv import").summary.programmes
    };

    let mut c = Criterion::default().configure_from_args();
    let mut g = c.benchmark_group("import");
    g.sampling_mode(SamplingMode::Flat).sample_size(10).measurement_time(Duration::from_secs(60));
    g.bench_function("m3u_100k_fresh_db", |b| {
        b.iter_batched(
            || {
                let ctx = context(&root);
                let id = file_source(&ctx);
                (ctx, id)
            },
            |(ctx, id)| (run_m3u(&ctx, id), ctx), // the store is dropped outside the timing
            BatchSize::PerIteration,
        )
    });
    g.bench_function("xmltv_415k_fresh_db", |b| {
        b.iter_batched(
            || {
                let ctx = context(&root);
                let id = file_guide(&ctx);
                (ctx, id)
            },
            |(ctx, id)| (run_xmltv(&ctx, id), ctx),
            BatchSize::PerIteration,
        )
    });
    g.finish();
    c.final_summary();
}
