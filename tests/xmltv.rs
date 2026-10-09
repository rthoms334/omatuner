//! XMLTV parser: fixtures parsed exactly, options, cancellation, progress. Owner: WP-05a.

mod support;

use std::fs::File;
use std::io::{BufReader, Read};
use std::ops::ControlFlow;

use tuner::error::Error;
use tuner::model::{EpisodeNum, Timestamp};
use tuner::sources::identity::normalize_name;
use tuner::sources::xmltv::{XmltvItem, XmltvOptions, XmltvStats, parse, parse_episode_num, parse_time};
use tuner::store::epg::{ImportEpgChannel, ImportProgramme};

/// 2026-10-09 20:00:00 UTC.
const S: Timestamp = 1_791_576_000;
const H: Timestamp = 3600;

fn opts(langs: &[&str]) -> XmltvOptions {
    XmltvOptions { preferred_langs: langs.iter().map(|s| s.to_string()).collect(), min_stop: None, max_start: None }
}

fn run<R: std::io::BufRead>(r: R, o: &XmltvOptions) -> (Vec<XmltvItem>, XmltvStats) {
    let mut items = Vec::new();
    let stats = parse(r, o, &mut |i| {
        items.push(i);
        ControlFlow::Continue(())
    }, &mut |_| {})
    .expect("parse");
    (items, stats)
}

fn run_file(rel: &str, o: &XmltvOptions) -> (Vec<XmltvItem>, XmltvStats) {
    run(BufReader::new(File::open(support::fixture(rel)).expect("fixture")), o)
}

fn chan(id: &str, names: &[&str], icon: Option<&str>) -> XmltvItem {
    XmltvItem::Channel(ImportEpgChannel {
        xmltv_id: id.into(),
        display_names: names.iter().map(|n| (n.to_string(), normalize_name(n))).collect(),
        icon: icon.map(str::to_owned),
    })
}

fn prog(ch: &str, start: Timestamp, stop: Timestamp, title: &str) -> ImportProgramme {
    ImportProgramme {
        xmltv_id: ch.into(),
        start,
        stop,
        title: title.into(),
        sub_title: None,
        description: None,
        categories: vec![],
        icon: None,
        episode: None,
        rating: None,
        star_rating: None,
        catchup_id: None,
        is_new: false,
        is_live: false,
        previously_shown: false,
    }
}

fn ep(season: Option<u32>, episode: Option<u32>) -> Option<EpisodeNum> { Some(EpisodeNum { season, episode }) }

fn basic_expected(title_bbc: &str) -> Vec<XmltvItem> {
    vec![
        chan("bbc1.uk", &["BBC One", "BBC 1 HD"], Some("http://logos.example/bbc1.png")),
        chan("cnn.us@SD", &["CNN International"], None),
        chan("espn", &["ESPN"], None),
        // 20:00–21:00 +0100
        XmltvItem::Programme(ImportProgramme {
            sub_title: Some("Headlines & weather".into()),
            description: Some("Tom & Jerry's <special> report.".into()),
            categories: vec!["News".into(), "Current affairs".into()],
            icon: Some("http://img.example/p1.jpg".into()),
            episode: ep(Some(2), Some(5)),
            catchup_id: Some("abc123".into()),
            is_new: true,
            ..prog("bbc1.uk", S - H, S, title_bbc)
        }),
        XmltvItem::Programme(ImportProgramme { episode: ep(Some(3), Some(14)), previously_shown: true, ..prog("bbc1.uk", S, S + H, "Drama") }),
        // no zone → UTC
        XmltvItem::Programme(prog("cnn.us@SD", S - H, S, "World Report")),
        // BST = +0100
        XmltvItem::Programme(ImportProgramme { is_live: true, ..prog("espn", S - H, S - H / 2, "Sport") }),
        // unknown channel: emitted as-is (the store auto-creates the epg channel)
        XmltvItem::Programme(prog("unknown.channel", S + H / 2, S + H, "Programme for a channel without a channel element")),
        // `202610092000 Z` without stop and without a later programme → start + 1 h, at EOF
        XmltvItem::Programme(prog("cnn.us@SD", S, S + H, "No stop time")),
    ]
}

#[test]
fn basic_parsed_exactly() {
    let (items, stats) = run_file("xmltv/basic.xml", &opts(&["en"]));
    assert_eq!(items, basic_expected("News at Eight"));
    let len = std::fs::metadata(support::fixture("xmltv/basic.xml")).unwrap().len();
    assert_eq!(stats, XmltvStats { channels: 3, programmes: 6, skipped: 1, bytes: len });
}

#[test]
fn basic_title_language_preference() {
    // No preference → the first title; preference order is respected.
    let (items, _) = run_file("xmltv/basic.xml", &opts(&[]));
    assert_eq!(items, basic_expected("Newyddion"));
    let (items, _) = run_file("xmltv/basic.xml", &opts(&["de", "cy", "en"]));
    assert_eq!(items, basic_expected("Newyddion"));
    let (items, _) = run_file("xmltv/basic.xml", &opts(&["en-GB"]));
    assert_eq!(items, basic_expected("News at Eight"));
}

#[test]
fn basic_gz_matches_plain() {
    let gz = flate2::read::MultiGzDecoder::new(File::open(support::fixture("xmltv/basic.xml.gz")).unwrap());
    let (items, stats) = run(BufReader::new(gz), &opts(&["en"]));
    assert_eq!(items, basic_expected("News at Eight"));
    assert_eq!((stats.channels, stats.programmes, stats.skipped), (3, 6, 1));
}

#[test]
fn edge_cases_parsed_exactly() {
    let (items, stats) = run_file("xmltv/edge_cases.xml", &opts(&["en", "de"]));
    // 18:00 +0200 = 16:00 UTC
    let p1 = ImportProgramme {
        description: Some("Die Nachrichten.".into()),
        categories: vec!["News".into()],
        episode: ep(Some(5), Some(10)), // xmltv_ns wins over onscreen / dd_progid
        rating: Some("12".into()),
        star_rating: Some("3.5/5".into()),
        is_new: true,
        is_live: true,
        ..prog("early.de", S - 4 * H, S - 3 * H, "News")
    };
    let expected = vec![
        XmltvItem::Programme(p1),
        chan("early.de", &["Das Erste HD", "ARD"], Some("http://logos.example/ard.png")),
        // stop-less: ends at the next later programme of the channel (the duplicate is skipped)
        XmltvItem::Programme(ImportProgramme {
            sub_title: Some("Folge 1".into()),
            episode: ep(Some(3), Some(14)),
            ..prog("early.de", S - 3 * H, S - 3 * H + 900, "Tatort: Mord & Totschlag")
        }),
        XmltvItem::Programme(ImportProgramme {
            icon: Some("http://img.example/kurz.png".into()),
            catchup_id: Some("x&y".into()),
            previously_shown: true,
            ..prog("early.de", S - 3 * H + 900, S - 3 * H + 2700, "Kurz")
        }),
        chan("unicode.jp", &["NHK 総合"], None),
        // EOF flush, ordered by (channel, start)
        XmltvItem::Programme(prog("late.uk", S + 3 * H, S + 4 * H, "Late")),
        XmltvItem::Programme(ImportProgramme {
            description: Some("Line one\nline two".into()),
            ..prog("unicode.jp", S, S + H, "日本語 – Ñoño")
        }),
    ];
    assert_eq!(items, expected);
    assert_eq!((stats.channels, stats.programmes, stats.skipped), (2, 5, 1));
}

#[test]
fn edge_cases_without_language_preference() {
    let (items, _) = run_file("xmltv/edge_cases.xml", &opts(&[]));
    let XmltvItem::Programme(p) = &items[0] else { panic!("first item is a programme") };
    assert_eq!(p.title, "Tagesschau");
    assert_eq!(p.description.as_deref(), Some("Le journal."));
    assert_eq!(p.categories, ["Nachrichten"]);
}

#[test]
fn min_stop_and_max_start_filter_without_counting() {
    let o = XmltvOptions { min_stop: Some(S + 1), max_start: Some(S), ..opts(&["en"]) };
    let (items, stats) = run_file("xmltv/basic.xml", &o);
    let titles: Vec<&str> = items
        .iter()
        .filter_map(|i| if let XmltvItem::Programme(p) = i { Some(p.title.as_str()) } else { None })
        .collect();
    assert_eq!(titles, ["Drama", "No stop time"]);
    assert_eq!((stats.channels, stats.programmes, stats.skipped), (3, 2, 1));
}

#[test]
fn sink_break_cancels() {
    let mut calls = 0;
    let r = parse(File::open(support::fixture("xmltv/basic.xml")).map(BufReader::new).unwrap(), &opts(&[]), &mut |_| {
        calls += 1;
        if calls == 2 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }, &mut |_| {});
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert_eq!(calls, 2, "no item after Break");
    // Cancelling on the EOF flush also reports Cancelled.
    let doc = r#"<tv><programme start="20261009200000" channel="a"><title>x</title></programme></tv>"#;
    let r = parse(doc.as_bytes(), &opts(&[]), &mut |_| ControlFlow::Break(()), &mut |_| {});
    assert!(matches!(r, Err(Error::Cancelled)));
}

fn synthetic(programmes: usize) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?>\n<tv>\n<channel id=\"c\"><display-name>C</display-name></channel>\n");
    for i in 0..programmes {
        let start = 20261001000000u64 + (i as u64 % 24) * 10_000;
        s.push_str(&format!(
            "<programme start=\"{start} +0000\" stop=\"{start} +0000\" channel=\"c{i}\"><title lang=\"en\">Show {i} &amp; friends</title><desc>Lorem ipsum dolor sit amet, consectetur adipiscing elit.</desc></programme>\n"
        ));
    }
    s.push_str("</tv>\n");
    s
}

#[test]
fn progress_every_mib_and_bytes_counted() {
    let doc = synthetic(40_000);
    assert!(doc.len() > 5 << 20);
    let mut reports = Vec::new();
    let mut n = 0u64;
    let stats = parse(doc.as_bytes(), &opts(&[]), &mut |_| {
        n += 1;
        ControlFlow::Continue(())
    }, &mut |b| reports.push(b))
    .unwrap();
    assert_eq!(stats.bytes, doc.len() as u64);
    assert_eq!(n, 40_001);
    assert!(reports.len() >= 5, "{reports:?}");
    assert!(reports.windows(2).all(|w| w[0] < w[1] || (w[0] == w[1] && w[1] == stats.bytes)));
    assert!(reports.windows(2).take(reports.len() - 2).all(|w| w[1] - w[0] >= 1 << 20));
    assert_eq!(*reports.last().unwrap(), stats.bytes);
}

#[test]
fn bom_and_crlf() {
    let doc = "\u{feff}<?xml version=\"1.0\"?>\r\n<tv>\r\n<programme start=\"20261009200000\" stop=\"20261009210000\" channel=\"a\">\r\n<desc>one\r\ntwo</desc><title>T</title></programme></tv>";
    let (items, stats) = run(doc.as_bytes(), &opts(&[]));
    assert_eq!(stats.programmes, 1);
    let XmltvItem::Programme(p) = &items[0] else { panic!() };
    assert_eq!(p.title, "T");
    assert_eq!(p.description.as_deref(), Some("one\ntwo"));
}

#[test]
fn malformed_and_non_xml_inputs_error_without_panicking() {
    for doc in [
        &b"<tv><programme start=\"2026\" channel=\"a\"><title>x</titl></programme></tv>"[..],
        b"<tv><programme start=\"20261009200000\" channel=\"a\"><title>x</title></programme",
        b"<tv><programme start=\"20261009200000 channel=\"a\"></programme></tv>",
    ] {
        let r = parse(doc, &opts(&[]), &mut |_| ControlFlow::Continue(()), &mut |_| {});
        assert!(matches!(r, Err(Error::Parse { .. })), "{:?} → {r:?}", String::from_utf8_lossy(doc));
    }
    // An HTML error page served with 200 is not a guide.
    let html = b"<!DOCTYPE html><html><body><p>503 Service Unavailable</p></body></html>";
    let r = parse(&html[..], &opts(&[]), &mut |_| ControlFlow::Continue(()), &mut |_| {});
    assert!(matches!(r, Err(Error::BadPayload(_))), "{r:?}");
    // Undeclared non-UTF-8 bytes are a parse error, not a panic.
    let bad = b"<tv><programme start=\"20261009200000\" channel=\"a\"><title>Caf\xe9</title></programme></tv>";
    let r = parse(&bad[..], &opts(&[]), &mut |_| ControlFlow::Continue(()), &mut |_| {});
    assert!(matches!(r, Err(Error::Parse { .. })), "{r:?}");
    // Empty input: nothing, no error.
    let (items, stats) = run(&b""[..], &opts(&[]));
    assert!(items.is_empty());
    assert_eq!(stats, XmltvStats::default());
}

#[test]
fn bare_ampersands_are_tolerated() {
    let doc = r#"<tv><channel id="a"><display-name>A & B</display-name><icon src="http://l/a.png?x=1&y=2"/></channel>
        <programme start="20261009200000" stop="20261009210000" channel="a"><title>Tom & Jerry &amp; co</title>
        <icon src="http://i/p.jpg?w=1&amp;h=2"/></programme></tv>"#;
    let (items, stats) = run(doc.as_bytes(), &opts(&[]));
    assert_eq!((stats.channels, stats.programmes, stats.skipped), (1, 1, 0));
    assert_eq!(items[0], chan("a", &["A & B"], Some("http://l/a.png?x=1&y=2")));
    let XmltvItem::Programme(p) = &items[1] else { panic!() };
    assert_eq!(p.title, "Tom & Jerry & co");
    assert_eq!(p.icon.as_deref(), Some("http://i/p.jpg?w=1&h=2"));
}

#[test]
fn declared_legacy_encodings_are_transcoded() {
    let latin1 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n<tv><channel id=\"a\"><display-name>T\xe9l\xe9 Caf\xe9</display-name></channel><programme start=\"20261009200000\" stop=\"20261009210000\" channel=\"a\"><title>Caf\xe9 &amp; cr\xe8me \xbd</title></programme></tv>";
    let (items, _) = run(&latin1[..], &opts(&[]));
    assert_eq!(items[0], chan("a", &["Télé Café"], None));
    let XmltvItem::Programme(p) = &items[1] else { panic!() };
    assert_eq!(p.title, "Café & crème ½");
    // Same through a tiny buffer (declaration split across reads) and with single quotes.
    let cp = b"<?xml version='1.0' encoding='windows-1252' ?><tv><programme start=\"20261009200000\" stop=\"20261009210000\" channel=\"a\"><title>\x80 5</title></programme></tv>";
    let (items, _) = run(BufReader::with_capacity(4, &cp[..]), &opts(&[]));
    let XmltvItem::Programme(p) = &items[0] else { panic!() };
    assert_eq!(p.title, "€ 5");
    // An explicit UTF-8 declaration is passed through untouched.
    let utf8 = "<?xml version=\"1.0\" encoding=\"utf-8\"?><tv><programme start=\"20261009200000\" stop=\"20261009210000\" channel=\"a\"><title>Ünïcödé</title></programme></tv>";
    let (items, _) = run(utf8.as_bytes(), &opts(&[]));
    let XmltvItem::Programme(p) = &items[0] else { panic!() };
    assert_eq!(p.title, "Ünïcödé");
    // A UTF-16 label on ASCII-compatible bytes is a mislabel: parsed as UTF-8, not garbled.
    let mislabel = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><tv><programme start=\"20261009200000\" stop=\"20261009210000\" channel=\"a\"><title>Ünïcödé</title></programme></tv>";
    let (items, _) = run(mislabel.as_bytes(), &opts(&[]));
    let XmltvItem::Programme(p) = &items[0] else { panic!() };
    assert_eq!(p.title, "Ünïcödé");
}

#[test]
fn truncated_download_is_an_error() {
    // A cut-off file (failed download) must not silently become a short guide.
    let full = std::fs::read(support::fixture("xmltv/basic.xml")).unwrap();
    let cut = full.windows(12).position(|w| w == b"<programme s").unwrap() + 400;
    let r = parse(&full[..cut], &opts(&[]), &mut |_| ControlFlow::Continue(()), &mut |_| {});
    assert!(matches!(r, Err(Error::Parse { .. })), "{r:?}");
    // Cut exactly between two elements: still an error.
    let cut = full.windows(13).rposition(|w| w == b"</programme>\n").unwrap() + 13;
    let r = parse(&full[..cut], &opts(&[]), &mut |_| ControlFlow::Continue(()), &mut |_| {});
    assert!(matches!(r, Err(Error::Parse { .. })), "{r:?}");
}

#[test]
fn parse_time_forms_from_the_spec() {
    assert_eq!(parse_time("20261009200000 +0100"), Some(S - H));
    assert_eq!(parse_time("20261009200000 Z"), Some(S));
    assert_eq!(parse_time("20261009200000 BST"), Some(S - H));
    assert_eq!(parse_time("20261009200000"), Some(S));
    assert_eq!(parse_time("202610092000"), Some(S));
    assert_eq!(parse_time("20261009"), Some(S - 20 * H));
    assert_eq!(parse_time("garbage"), None);
    assert_eq!(parse_episode_num(Some("xmltv_ns"), "1.0.0/1"), ep(Some(2), Some(1)));
    assert_eq!(parse_episode_num(Some("onscreen"), "S03E14"), ep(Some(3), Some(14)));
}

#[test]
fn reader_input_is_streamed() {
    // A reader that hands out tiny chunks still parses the same result.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len()).min(7);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let data = std::fs::read(support::fixture("xmltv/basic.xml")).unwrap();
    let (items, _) = run(BufReader::with_capacity(16, Trickle(&data)), &opts(&["en"]));
    assert_eq!(items, basic_expected("News at Eight"));
}
