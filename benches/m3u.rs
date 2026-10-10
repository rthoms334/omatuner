//! m3u benchmark. Owner: WP-03. Gate: 100k-entry M3U parse (gate: < 1.5 s release on the M2, dataset big.m3u).
//! Run: `cargo bench --bench m3u` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! Uses `target/big-fixtures/big.m3u` (or `$TUNER_BIG_FIXTURES/big.m3u`) when present; otherwise
//! an equivalent 100k-entry playlist is generated in memory (same shapes as the script: mixed
//! quoting, CRLF, `;` groups, tvg-shift, catch-up attrs, DRM props, duplicate names, pipe
//! headers, `url-tvg` header). The parse runs from memory so disk speed is not measured.

use std::hint::black_box;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tuner::sources::m3u;

const CHANNELS: usize = 100_000;
const GATE_S: f64 = 1.5;

fn dataset() -> (String, Vec<u8>) {
    let dir = std::env::var_os("TUNER_BIG_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/big-fixtures"));
    let path = dir.join("big.m3u");
    match std::fs::read(&path) {
        Ok(bytes) => (path.display().to_string(), bytes),
        Err(_) => ("generated in memory".to_owned(), generate(CHANNELS)),
    }
}

/// Deterministic stand-in for `gen-big-fixtures.py`'s big.m3u.
fn generate(channels: usize) -> Vec<u8> {
    let mut seed = 42u64;
    let mut rnd = move |n: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as usize) % n
    };
    let countries = ["UK", "US", "DE", "FR", "ES", "IT", "NL", "PL"];
    let mut out = String::with_capacity(channels * 330);
    out.push_str("#EXTM3U url-tvg=\"http://127.0.0.1/big.xml.gz\" catchup=\"default\" catchup-days=\"3\"\r\n");
    for i in 0..channels {
        let q = ["\"", "'", "\""][rnd(3)];
        let mut g = format!("Group {}", rnd(400));
        if i % 50 == 0 {
            g = format!("{g};Group {}", rnd(400));
        }
        let name = format!("Channel {}", i % (channels - 500).max(1));
        let mut extra = String::new();
        if i % 97 == 0 {
            extra.push_str(" tvg-shift=\"+1\"");
        }
        if i % 211 == 0 {
            extra.push_str(" catchup=\"append\" catchup-source=\"?utc={utc}&lutc={lutc}\"");
        }
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id={q}ch{}.tv{q} tvg-name=\"{name}\" tvg-chno=\"{}\" tvg-logo=\"http://logo.invalid/{i}.png\" group-title=\"{g}\"{extra},{} | {name} HD\r\n",
            i % 1235,
            i + 1,
            countries[rnd(countries.len())]
        ));
        if i % 1000 == 0 {
            out.push_str("#KODIPROP:inputstream.adaptive.license_type=com.widevine.alpha\r\n");
        }
        out.push_str(&format!("http://127.0.0.1/live/u/p/{i}.ts"));
        if i % 333 == 0 {
            out.push_str("|User-Agent=Mozilla/5.0&Referer=http://ref.invalid/");
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

fn parse_count(bytes: &[u8]) -> usize {
    let p = m3u::parse(Cursor::new(bytes), None).expect("playlist");
    p.filter(|e| e.is_ok()).count()
}

fn bench(c: &mut Criterion) {
    let (origin, bytes) = dataset();
    // One timed pass up front so the gate is visible even with `--quick` / `--no-run` habits.
    let t = Instant::now();
    let n = parse_count(&bytes);
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "m3u: {n} entries, {:.1} MB ({origin}) parsed in {secs:.3} s (gate {GATE_S} s{})",
        bytes.len() as f64 / 1e6,
        if cfg!(debug_assertions) { ", debug build: not comparable" } else { "" }
    );

    let mut g = c.benchmark_group("m3u");
    g.sample_size(10);
    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.bench_function("parse_100k", |b| b.iter(|| black_box(parse_count(black_box(&bytes)))));
    g.finish();

    let line =
        r#"-1 tvg-id="bbc1.uk" tvg-name="BBC One" tvg-logo="http://logos.example/bbc1.png" tvg-chno="101" group-title="UK;News",UK | BBC One FHD"#;
    c.bench_function("extinf_attrs", |b| b.iter(|| black_box(m3u::parse_extinf_attrs(black_box(line)))));
    c.bench_function("clean_channel_name", |b| {
        b.iter(|| black_box(tuner::sources::smart_names::clean_channel_name(black_box("[VIP] US: ESPN 2 4K (backup)"))))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
