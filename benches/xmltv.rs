//! xmltv benchmark. Owner: WP-05a. Gate: 415k-programme XMLTV parse (gate: < 1 s, dataset big.xml.gz).
//! Run: `cargo bench --bench xmltv` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! Measures gunzip + `xmltv::parse` of `target/big-fixtures/big.xml.gz` (override with
//! `TUNER_BIG_FIXTURES=<dir>`). When the file is missing, an equivalent dataset (same shape as
//! the generator's defaults: 1 235 channels × 7 days × 48 programmes) is built in memory so the
//! bench always runs. Also benches `parse_time` alone.

use std::hint::black_box;
use std::io::{BufReader, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use tuner::sources::xmltv::{XmltvItem, XmltvOptions, parse, parse_time};

fn dataset() -> Vec<u8> {
    let dir = std::env::var_os("TUNER_BIG_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/big-fixtures"));
    let path = dir.join("big.xml.gz");
    match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("{} not found (run scripts/gen-big-fixtures.py); using an in-memory equivalent", path.display());
            synthetic_gz(1_235, 7)
        }
    }
}

/// Same document shape as `scripts/gen-big-fixtures.py`.
fn synthetic_gz(channels: usize, days: usize) -> Vec<u8> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    let mut w = |s: &str| gz.write_all(s.as_bytes()).expect("in-memory write");
    w("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"tuner-bench\">\n");
    for c in 0..channels {
        w(&format!(
            "<channel id=\"ch{c}.tv\"><display-name>Channel {c}</display-name><display-name lang=\"de\">Kanal {c}</display-name><icon src=\"http://logo.invalid/{c}.png\"/></channel>\n"
        ));
    }
    let zones = [" +0000", " +0100", "", "Z"];
    let start = jiff::civil::date(2026, 10, 9).at(0, 0, 0, 0);
    for c in 0..channels {
        let z = zones[c % zones.len()];
        let mut t = start;
        for i in 0..days * 48 {
            let e = t + jiff::Span::new().minutes(30);
            w(&format!(
                "<programme start=\"{}{z}\" stop=\"{}{z}\" channel=\"ch{c}.tv\"><title lang=\"en\">Show {} &amp; friends</title><title lang=\"de\">Sendung</title><desc>Lorem ipsum dolor sit amet.</desc><episode-num system=\"xmltv_ns\">1.{}.</episode-num></programme>\n",
                t.strftime("%Y%m%d%H%M%S"),
                e.strftime("%Y%m%d%H%M%S"),
                (c * 7919 + i * 104_729) % 100_000,
                i % 21,
            ));
            t = e;
        }
    }
    w("</tv>\n");
    gz.finish().expect("in-memory gzip")
}

fn parse_all(gz: &[u8], opts: &XmltvOptions) -> u64 {
    let reader = BufReader::with_capacity(256 * 1024, MultiGzDecoder::new(gz));
    let mut n = 0u64;
    parse(
        reader,
        opts,
        &mut |item| {
            if let XmltvItem::Programme(p) = item {
                n += 1;
                black_box(p);
            }
            ControlFlow::Continue(())
        },
        &mut |_| {},
    )
    .expect("parse");
    n
}

fn bench(c: &mut Criterion) {
    let gz = dataset();
    let opts = XmltvOptions { preferred_langs: vec!["en".into()], min_stop: None, max_start: None };
    let programmes = parse_all(&gz, &opts);
    eprintln!("dataset: {} KiB gzip, {programmes} programmes", gz.len() / 1024);

    let mut g = c.benchmark_group("xmltv");
    g.sample_size(10).measurement_time(Duration::from_secs(20)).throughput(Throughput::Elements(programmes));
    g.bench_function("parse_big_xml_gz", |b| b.iter(|| parse_all(black_box(&gz), &opts)));
    g.finish();

    let times = ["20261009200000 +0100", "20261009200000", "202610092000 Z", "20261009200000 BST"];
    c.bench_function("parse_time", |b| b.iter(|| times.iter().filter_map(|t| parse_time(black_box(t))).sum::<i64>()));
}

criterion_group!(benches, bench);
criterion_main!(benches);
