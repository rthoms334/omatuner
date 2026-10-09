//! M3U parser and smart-name tests. Owner: WP-03. DESIGN §9.1, §24.2 (WP-03 acceptance).

use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::PathBuf;

use tuner::error::Error;
use tuner::model::{CatchupKind, CatchupSpec, HttpHeaders};
use tuner::sources::PayloadKind;
use tuner::sources::identity::normalize_name;
use tuner::sources::m3u::{self, M3uEntry, M3uError, M3uHeader};
use tuner::sources::smart_names::{Quality, clean_channel_name};
use url::Url;

fn fixture(rel: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m3u").join(rel) }

fn read_fixture(rel: &str) -> Vec<u8> { std::fs::read(fixture(rel)).expect("fixture") }

const BASE: &str = "http://lists.example/playlists/edge.m3u";

/// Parse everything; panics on any per-entry error.
fn parse_all<R: BufRead>(r: R, base: Option<&Url>) -> (M3uHeader, Vec<M3uEntry>, u64) {
    let mut p = m3u::parse(r, base).expect("is a playlist");
    let entries: Vec<M3uEntry> = p.by_ref().map(|e| e.expect("entry")).collect();
    (p.header().clone(), entries, p.bytes_read())
}

fn parse_bytes(b: &[u8]) -> (M3uHeader, Vec<M3uEntry>) {
    let (h, e, _) = parse_all(Cursor::new(b), None);
    (h, e)
}

fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }

fn s(v: &str) -> Option<String> { Some(v.to_owned()) }

fn spec(kind: CatchupKind, days: u32) -> Option<CatchupSpec> { Some(CatchupSpec { kind, template: None, days, correction_s: 0 }) }

fn header_shift() -> Option<CatchupSpec> { spec(CatchupKind::Shift, 3) }

/// The exact expected parse of `edge_cases.m3u` against `BASE`.
fn expected_edge_cases() -> (M3uHeader, Vec<M3uEntry>) {
    let header = M3uHeader {
        tvg_urls: vec!["http://epg.example/a.xml.gz".into(), "http://epg.example/b.xml".into()],
        tvg_shift_h: Some(-1.5),
        catchup: header_shift(),
        attrs: vec![],
    };
    let base = M3uEntry { tvg_shift_h: Some(-1.5), catchup: header_shift(), ..Default::default() };
    let entries = vec![
        M3uEntry {
            line: 3,
            title: "UK | BBC One FHD".into(),
            attrs: kv(&[
                ("tvg-id", "bbc1.uk"),
                ("tvg-name", "BBC One"),
                ("tvg-logo", "http://logos.example/bbc1.png"),
                ("tvg-chno", "101"),
                ("group-title", "UK;News"),
            ]),
            tvg_id: s("bbc1.uk"),
            tvg_name: s("BBC One"),
            tvg_logo: s("http://logos.example/bbc1.png"),
            tvg_chno: Some(101),
            groups: vec!["UK".into(), "News".into()],
            url: "http://streams.example/live/bbc1.m3u8".into(),
            ..base.clone()
        },
        // single quotes, unquoted logo, title with a comma, #EXTVLCOPT headers, relative URL
        M3uEntry {
            line: 8,
            title: "CNN, International".into(),
            attrs: kv(&[("tvg-id", "cnn.us@SD"), ("group-title", "News"), ("tvg-logo", "cnn.png")]),
            tvg_id: s("cnn.us@SD"),
            tvg_logo: s("http://lists.example/playlists/cnn.png"),
            groups: vec!["News".into()],
            headers: HttpHeaders { user_agent: s("Mozilla/5.0 (X11)"), referrer: s("https://cnn.example/"), ..Default::default() },
            url: "http://lists.example/playlists/cnn/index.m3u8".into(),
            ..base.clone()
        },
        // `#EXTGRP` before the EXTINF, mixed-case key, comma inside a quoted value, pipe headers
        M3uEntry {
            line: 11,
            title: "ESPN HD".into(),
            attrs: kv(&[("tvg-id", "espn"), ("tvg-name", "ESPN, HD")]),
            tvg_id: s("espn"),
            tvg_name: s("ESPN, HD"),
            groups: vec!["Sports".into()],
            headers: HttpHeaders { user_agent: s("VLC"), referrer: s("http://a.example/"), ..Default::default() },
            url: "http://streams.example/espn.ts".into(),
            ..base.clone()
        },
        M3uEntry {
            line: 15,
            title: "Premium DRM".into(),
            attrs: kv(&[("tvg-id", "drm1"), ("group-title", "Premium")]),
            tvg_id: s("drm1"),
            groups: vec!["Premium".into()],
            drm: true,
            url: "https://drm.example/manifest.mpd".into(),
            ..base.clone()
        },
        // own catch-up beats the header default
        M3uEntry {
            line: 17,
            title: "Archive Channel".into(),
            attrs: kv(&[("tvg-id", "arch"), ("catchup", "xc"), ("catchup-days", "7"), ("group-title", "Archive")]),
            tvg_id: s("arch"),
            groups: vec!["Archive".into()],
            catchup: spec(CatchupKind::Xc, 7),
            url: "http://panel.example:8080/live/user/pass/1234.ts".into(),
            ..base.clone()
        },
        // legacy `timeshift` = Shift with its own window
        M3uEntry {
            line: 19,
            title: "Legacy Timeshift".into(),
            attrs: kv(&[("timeshift", "2"), ("group-title", "Archive")]),
            groups: vec!["Archive".into()],
            catchup: spec(CatchupKind::Shift, 2),
            url: "http://fs.example/ch/mpegts".into(),
            ..base.clone()
        },
        M3uEntry {
            line: 21,
            title: "Radio One".into(),
            attrs: kv(&[("radio", "true"), ("group-title", "Radio")]),
            groups: vec!["Radio".into()],
            radio: true,
            url: "http://radio.example/one.aac".into(),
            ..base.clone()
        },
        // bare URL (after a comment): titled by its last segment, sticky #EXTGRP
        M3uEntry {
            line: 23,
            title: "bare-stream.m3u8".into(),
            groups: vec!["Sports".into()],
            url: "http://bare.example/path/bare-stream.m3u8".into(),
            ..base.clone()
        },
        // same name as entry 2 (KeyAllocator's job), no `#EXTVLCOPT` leak from entry 2
        M3uEntry {
            line: 25,
            title: "CNN, International".into(),
            attrs: kv(&[("group-title", "News")]),
            groups: vec!["News".into()],
            url: "http://streams.example/cnn-backup.m3u8".into(),
            ..base
        },
    ];
    (header, entries)
}

#[test]
fn edge_cases_exact() {
    let bytes = read_fixture("edge_cases.m3u");
    let base = Url::parse(BASE).unwrap();
    let (header, entries, read) = parse_all(Cursor::new(&bytes), Some(&base));
    let (eh, ee) = expected_edge_cases();
    assert_eq!(header, eh);
    assert_eq!(entries.len(), ee.len());
    for (got, want) in entries.iter().zip(&ee) {
        assert_eq!(got, want, "entry at line {}", want.line);
    }
    assert_eq!(read, bytes.len() as u64, "bytes_read reaches the end");
}

/// Same parse as `edge_cases_exact`, pinned as an `insta` snapshot (reviewable diff when the
/// parser output changes on purpose: `cargo insta review`).
#[test]
fn edge_cases_snapshot() {
    let base = Url::parse(BASE).unwrap();
    let (header, entries, _) = parse_all(Cursor::new(read_fixture("edge_cases.m3u")), Some(&base));
    insta::with_settings!({ snapshot_path => "fixtures/m3u/snapshots", prepend_module_to_snapshot => false }, {
        insta::assert_debug_snapshot!("edge_cases", (header, entries));
    });
}

#[test]
fn edge_cases_without_base_keep_relative_references() {
    let (_, entries) = parse_bytes(&read_fixture("edge_cases.m3u"));
    assert_eq!(entries[1].url, "cnn/index.m3u8");
    assert_eq!(entries[1].tvg_logo.as_deref(), Some("cnn.png"));
    assert_eq!(entries[0].url, "http://streams.example/live/bbc1.m3u8");
}

#[test]
fn catchup_kinds_go_through_parse_kind() {
    let text = "#EXTM3U\n\
        #EXTINF:-1 catchup=\"fs\",A\nhttp://h/a/index.m3u8\n\
        #EXTINF:-1 catchup-type=\"xtream\",B\nhttp://h/b\n\
        #EXTINF:-1 catchup=\"TIMESHIFT\",C\nhttp://h/c\n\
        #EXTINF:-1 catchup=\"flussonic-ts\",D\nhttp://h/d\n\
        #EXTINF:-1 catchup=\"append\" catchup-source=\"?utc={utc}\" catchup-correction=\"0.5\" tvg-rec=\"4\",E\nhttp://h/e\n\
        #EXTINF:-1 catchup=\"vod\" catchup-time=\"7200\",F\nhttp://h/f\n\
        #EXTINF:-1 catchup=\"nonsense\",G\nhttp://h/g\n\
        #EXTINF:-1,H\nhttp://h/h\n";
    let (h, e) = parse_bytes(text.as_bytes());
    assert_eq!(h.catchup, None);
    let kinds: Vec<_> = e.iter().map(|e| e.catchup.as_ref().map(|c| c.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            Some(CatchupKind::Flussonic),
            Some(CatchupKind::Xc),
            Some(CatchupKind::Shift),
            Some(CatchupKind::FlussonicTs),
            Some(CatchupKind::Append),
            Some(CatchupKind::Vod),
            None,
            None
        ]
    );
    assert_eq!(e[4].catchup, Some(CatchupSpec { kind: CatchupKind::Append, template: s("?utc={utc}"), days: 4, correction_s: 1800 }));
    assert_eq!(e[5].catchup.as_ref().unwrap().days, 1);
}

// ---------------------------------------------------------------------------------------------
// line endings, BOM, encodings, buffering

fn variants(lf: &str) -> Vec<(&'static str, Vec<u8>)> {
    let crlf = lf.replace('\n', "\r\n");
    let cr = lf.replace('\n', "\r");
    // rotate LF / CRLF / CR, but never CR before an empty LF-terminated line (that is CRLF)
    let lines: Vec<&str> = lf.split('\n').collect();
    let mixed: String = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let next_empty = lines.get(i + 1).is_none_or(|n| n.is_empty());
            let term = if i % 3 == 2 && !next_empty { "\r" } else if i % 2 == 0 { "\n" } else { "\r\n" };
            format!("{l}{term}")
        })
        .collect();
    vec![
        ("lf", lf.as_bytes().to_vec()),
        ("crlf", crlf.clone().into_bytes()),
        ("cr-only", cr.into_bytes()),
        ("mixed", mixed.into_bytes()),
        ("bom+crlf", [b"\xEF\xBB\xBF".as_slice(), crlf.as_bytes()].concat()),
        ("no-final-newline", lf.trim_end().as_bytes().to_vec()),
    ]
}

#[test]
fn line_endings_and_bom_give_identical_results() {
    let raw = read_fixture("edge_cases.m3u");
    let lf = String::from_utf8(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap().to_vec()).unwrap().replace("\r\n", "\n");
    let base = Url::parse(BASE).unwrap();
    let (eh, ee) = expected_edge_cases();
    for (name, bytes) in variants(&lf) {
        let (h, e, read) = parse_all(Cursor::new(&bytes), Some(&base));
        assert_eq!(h, eh, "{name}");
        assert_eq!(e, ee, "{name}");
        assert_eq!(read, bytes.len() as u64, "{name}");
        // tiny buffers split CR/LF pairs, BOMs and attributes across reads
        for cap in [1, 2, 7, 64] {
            let (h2, e2, _) = parse_all(BufReader::with_capacity(cap, Cursor::new(&bytes)), Some(&base));
            assert_eq!((h2, e2), (eh.clone(), ee.clone()), "{name} cap {cap}");
        }
    }
}

#[test]
fn bom_inside_concatenated_playlists_and_repeated_headers() {
    let text = "\u{feff}#EXTM3U x-tvg-url=\"http://e/1.xml\" refresh=\"3600\"\n#EXTINF:-1,A\nhttp://h/a\n\
        \u{feff}#EXTM3U url-tvg=\"http://e/2.xml,http://e/1.xml\" tvg-shift=\"2\" refresh=\"60\" m3uautoload=1\n#EXTINF:-1,B\nhttp://h/b\n";
    let (h, e) = parse_bytes(text.as_bytes());
    assert_eq!(h.tvg_urls, vec!["http://e/1.xml".to_owned(), "http://e/2.xml".to_owned()]);
    assert_eq!(h.attrs, kv(&[("refresh", "3600"), ("m3uautoload", "1")]));
    assert_eq!(h.tvg_shift_h, Some(2.0), "filled by the second header");
    assert_eq!(e.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["A", "B"]);
}

#[test]
fn windows_1252_lines_are_decoded() {
    let mut bytes = b"#EXTM3U\n#EXTINF:-1 group-title=\"Fran\xe7ais\",Caf\xe9 TV \x80\n".to_vec();
    bytes.extend_from_slice("http://h/x\n#EXTINF:-1,Ünïcödé ✓\nhttp://h/y\n".as_bytes());
    let (_, e) = parse_bytes(&bytes);
    assert_eq!(e[0].title, "Café TV €");
    assert_eq!(e[0].groups, vec!["Français".to_owned()]);
    assert_eq!(e[1].title, "Ünïcödé ✓");
}

// ---------------------------------------------------------------------------------------------
// payload classification

#[test]
fn html_error_page_is_rejected() {
    let r = m3u::parse(Cursor::new(read_fixture("html_error.m3u")), None);
    assert!(matches!(r, Err(M3uError::NotM3u(PayloadKind::Html))), "{:?}", r.err());
}

#[test]
fn hls_media_playlist_is_detected() {
    let r = m3u::parse(Cursor::new(read_fixture("hls_media.m3u8")), None);
    assert!(matches!(r, Err(M3uError::HlsMediaPlaylist)));
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nlow/index.m3u8\n";
    assert!(matches!(m3u::parse(Cursor::new(master), None), Err(M3uError::HlsMediaPlaylist)));
    // after the first entry, #EXT-X lines are just unknown directives
    let late = "#EXTM3U\n#EXTINF:-1,A\n#EXT-X-SOMETHING:1\nhttp://h/a\n";
    assert_eq!(parse_bytes(late.as_bytes()).1.len(), 1);
}

#[test]
fn other_payloads_are_not_m3u() {
    let cases: &[(&[u8], PayloadKind)] = &[
        (b"{\"user_info\":{\"auth\":0}}", PayloadKind::Json),
        (b"[]", PayloadKind::Json),
        (b"<?xml version=\"1.0\"?><tv></tv>", PayloadKind::Xmltv),
        (b"  \n\r\n<html><body>Just a moment...</body></html>", PayloadKind::Html),
        (b"", PayloadKind::Unknown),
        (b" \n\n \r\n", PayloadKind::Unknown),
        (b"\xEF\xBB\xBF", PayloadKind::Unknown),
        (b"garbage\n#EXTINF:-1,A\nhttp://h/a", PayloadKind::Unknown),
        (b"# just a comment\n", PayloadKind::Unknown),
        (b"relative/path.ts\n", PayloadKind::Unknown),
        // XHTML error page: only a later line says `<html`
        (b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0//EN\">\n<html><body>Expired</body></html>", PayloadKind::Html),
        (b"\xEF\xBB\xBF<!DOCTYPE html>\n<html></html>", PayloadKind::Html),
    ];
    for (bytes, kind) in cases {
        match m3u::parse(Cursor::new(bytes), None) {
            Err(M3uError::NotM3u(k)) => assert_eq!(k, *kind, "{:?}", String::from_utf8_lossy(bytes)),
            other => panic!("{:?}: {:?}", String::from_utf8_lossy(bytes), other.map(|_| ())),
        }
    }
}

#[test]
fn headerless_playlists_are_accepted() {
    let (h, e) = parse_bytes(b"#EXTINF:-1,A\nhttp://h/a\n");
    assert_eq!((h, e.len()), (M3uHeader::default(), 1));
    let (_, e) = parse_bytes(b"\n\nrtmp://h/live/a\nudp://@239.0.0.1:1234\n");
    assert_eq!(e.iter().map(|e| (e.title.as_str(), e.url.as_str())).collect::<Vec<_>>(), [
        ("a", "rtmp://h/live/a"),
        ("udp://@239.0.0.1:1234", "udp://@239.0.0.1:1234")
    ]);
    // a header alone is an empty (but valid) playlist
    let (_, e) = parse_bytes(b"#EXTM3U\n");
    assert!(e.is_empty());
}

// ---------------------------------------------------------------------------------------------
// entries, directives, errors

#[test]
fn directives_headers_and_precedence() {
    let text = "#EXTM3U\n\
        #KODIPROP:inputstream.adaptive.stream_headers=User-Agent=kodi%2F1&X-Token=abc\n\
        #EXTINF:-1 http-user-agent=\"attr-ua\" http-referrer=\"http://attr/\" http-origin=\"http://o/\",One\n\
        #EXTVLCOPT:http-user-agent=vlc-ua\n\
        #EXTVLCOPT:network-caching=1000\n\
        #EXTVLCOPT:http-cookie=a=b\n\
        http://h/one|User-Agent=pipe-ua&x-token=def\n\
        #EXTINF:-1 http-user-agent=\"only-attr\",Two\n\
        http://h/two\n\
        #EXTINF:-1,Three\n\
        #KODIPROP:inputstream.adaptive.license_key=https://lic/\n\
        http://h/three\n\
        #EXTINF:-1,Four\nhttp://h/four\n";
    let (_, e) = parse_bytes(text.as_bytes());
    assert_eq!(e[0].headers, HttpHeaders {
        user_agent: s("pipe-ua"),
        referrer: s("http://attr/"),
        origin: s("http://o/"),
        extra: kv(&[("X-Token", "def"), ("Cookie", "a=b")]),
    });
    assert_eq!(e[0].url, "http://h/one");
    assert_eq!(e[1].headers, HttpHeaders { user_agent: s("only-attr"), ..Default::default() });
    assert!(!e[1].drm && e[2].drm && !e[3].drm, "DRM flag applies to its own entry only");
    assert!(e[3].headers.is_empty());
}

#[test]
fn extgrp_is_sticky_until_cleared_and_group_title_wins() {
    let text = "#EXTM3U\n#EXTGRP:Kids; Family ;Kids\n#EXTINF:-1,A\nhttp://h/a\n#EXTINF:-1 group-title=\"Own\",B\nhttp://h/b\n\
        #EXTINF:-1 group-title=\" ; \",C\nhttp://h/c\n#EXTGRP:\n#EXTINF:-1,D\nhttp://h/d\n";
    let (_, e) = parse_bytes(text.as_bytes());
    let groups: Vec<Vec<String>> = e.iter().map(|e| e.groups.clone()).collect();
    assert_eq!(groups, vec![vec!["Kids".to_owned(), "Family".to_owned()], vec!["Own".to_owned()], vec!["Kids".to_owned(), "Family".to_owned()], vec![]]);
}

#[test]
fn title_fallbacks_and_misc_attributes() {
    let text = "#EXTM3U tvg-shift=\"+1\"\n\
        #EXTINF:-1 tvg-name=\"From tvg-name\",\nhttp://h/a.ts\n\
        #EXTINF:-1 tvg-chno=\"x12\" tvg-type=\"radio\" tvg-shift=\"-2\",\nhttp://h/dir/stream.aac?token=1\n\
        #EXTINF:-1 tvg-id=\"\" tvg-logo=\"\" Words Without Comma\nhttp://h/c\n\
        #EXTINF:-1 radio=\"false\" tvg-id=abc tvg-chno=7,\"Quoted\" title\nhttp://h/d\n";
    let (_, e) = parse_bytes(text.as_bytes());
    assert_eq!(e[0].title, "From tvg-name");
    assert_eq!(e[0].tvg_shift_h, Some(1.0), "inherits the header tvg-shift");
    assert_eq!(e[1].title, "stream.aac");
    assert_eq!((e[1].tvg_chno, e[1].radio, e[1].tvg_shift_h), (None, true, Some(-2.0)));
    assert_eq!(e[2].title, "Words Without Comma");
    assert_eq!((e[2].tvg_id.as_deref(), e[2].tvg_logo.as_deref()), (None, None), "empty attributes are None");
    assert_eq!((e[3].title.as_str(), e[3].tvg_id.as_deref(), e[3].tvg_chno, e[3].radio), ("\"Quoted\" title", Some("abc"), Some(7), false));
}

#[test]
fn malformed_entries_are_reported_and_parsing_continues() {
    let text = "#EXTM3U\n#EXTINF:-1,Lost\n\n#EXTINF:-1,Kept\nhttp://h/kept\n#EXTINF:-1,Dangling at EOF\n";
    let mut p = m3u::parse(Cursor::new(text), None).unwrap();
    match p.next() {
        Some(Err(Error::Parse { line, msg })) => {
            assert_eq!(line, Some(2));
            assert!(msg.contains("Lost"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(p.next().unwrap().unwrap().title, "Kept");
    assert!(matches!(p.next(), Some(Err(Error::Parse { line: Some(6), .. }))));
    assert!(p.next().is_none());
    assert!(p.next().is_none(), "fused after the end");
}

#[test]
fn overlong_lines_are_skipped() {
    let mut text = String::from("#EXTM3U\n#EXTINF:-1,Huge\nhttp://h/");
    text.push_str(&"x".repeat(2 << 20));
    text.push_str("\n#EXTINF:-1,After\nhttp://h/after\n");
    let mut p = m3u::parse(BufReader::with_capacity(4096, Cursor::new(text.as_bytes())), None).unwrap();
    assert!(matches!(p.next(), Some(Err(Error::Parse { line: Some(3), .. }))));
    let next = p.next().unwrap().unwrap();
    assert_eq!((next.title.as_str(), next.line), ("After", 5));
    assert!(p.next().is_none());
    assert_eq!(p.bytes_read(), text.len() as u64);
}

/// Serves `data`, then fails.
struct FailingReader {
    data: Cursor<Vec<u8>>,
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.data.read(buf)? {
            0 => Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom")),
            n => Ok(n),
        }
    }
}

#[test]
fn io_errors_end_the_iteration() {
    let r = FailingReader { data: Cursor::new(b"#EXTM3U\n#EXTINF:-1,A\nhttp://h/a\n#EXTINF:-1,B\nhttp://h/".to_vec()) };
    let mut p = m3u::parse(BufReader::with_capacity(8, r), None).unwrap();
    assert_eq!(p.next().unwrap().unwrap().title, "A");
    assert!(matches!(p.next(), Some(Err(Error::Io { .. }))));
    assert!(p.next().is_none());

    // failure during the read-ahead surfaces on the first `next`
    let r = FailingReader { data: Cursor::new(b"#EXTM3U\n# comment".to_vec()) };
    let mut p = m3u::parse(BufReader::new(r), None).unwrap();
    assert!(matches!(p.next(), Some(Err(Error::Io { .. }))));
    assert!(p.next().is_none());
}

#[test]
fn relative_urls_resolve_against_the_playlist() {
    let base = Url::parse("https://cdn.example/lists/tv/all.m3u?token=1").unwrap();
    let text = "#EXTM3U\n#EXTINF:-1 tvg-logo=\"/logos/a.png\",A\n../a/index.m3u8\n#EXTINF:-1,B\n/abs/b.ts\n#EXTINF:-1,C\nc.ts|Referer=x\n\
        #EXTINF:-1,D\nrtsp://cam/stream\n#EXTINF:-1 tvg-logo=\"data:image/png;base64,AAAA\",E\nhttps://other.example/e\n";
    let (_, e, _) = parse_all(Cursor::new(text), Some(&base));
    let urls: Vec<&str> = e.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(urls, [
        "https://cdn.example/lists/a/index.m3u8",
        "https://cdn.example/abs/b.ts",
        "https://cdn.example/lists/tv/c.ts",
        "rtsp://cam/stream",
        "https://other.example/e"
    ]);
    assert_eq!(e[0].tvg_logo.as_deref(), Some("https://cdn.example/logos/a.png"));
    assert_eq!(e[4].tvg_logo.as_deref(), Some("data:image/png;base64,AAAA"));
    assert_eq!(e[2].headers.referrer.as_deref(), Some("x"));
}

#[test]
fn extinf_attribute_tokenizer_contract() {
    // the documented example of `parse_extinf_attrs`
    let (attrs, title) = m3u::parse_extinf_attrs(r#"-1 tvg-id="a,b" group-title='News' tvg-logo=x.png,Title, with comma"#);
    assert_eq!(attrs, kv(&[("tvg-id", "a,b"), ("group-title", "News"), ("tvg-logo", "x.png")]));
    assert_eq!(title, "Title, with comma");
    assert_eq!(m3u::split_pipe_headers("http://x/y.m3u8|User-Agent=a&Referer=b"), ("http://x/y.m3u8".to_owned(), kv(&[("User-Agent", "a"), ("Referer", "b")])));
}

#[test]
fn detects_xtream_urls() {
    let c = m3u::detect_xtream_url("http://panel.example:8080/get.php?username=user&password=pass&type=m3u_plus&output=ts").unwrap();
    assert_eq!((c.server.as_str(), c.username.as_str(), c.password.expose().as_str()), ("http://panel.example:8080", "user", "pass"));
    let c = m3u::detect_xtream_url("https://panel.example/player_api.php?username=u&password=s3cr3tPW").unwrap();
    assert_eq!(c.server, "https://panel.example");
    assert!(!format!("{c:?}").contains("s3cr3tPW"), "the password never shows up in Debug output");
    assert!(m3u::detect_xtream_url("https://iptv-org.github.io/iptv/index.m3u").is_none());
    assert!(m3u::detect_xtream_url("http://panel.example/get.php?type=m3u").is_none());
}

// ---------------------------------------------------------------------------------------------
// smart names

#[test]
fn smart_names_table() {
    let tsv = String::from_utf8(read_fixture("smart_names.tsv")).unwrap();
    let mut rows = 0;
    for (i, line) in tsv.lines().enumerate().skip(1).filter(|(_, l)| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert!(cols.len() >= 4, "row {} has {} columns", i + 1, cols.len());
        let (raw, clean, country, quality) = (cols[0], cols[1], cols[2], cols[3]);
        let got = clean_channel_name(raw);
        assert_eq!(got.name, clean, "row {}: {raw:?}", i + 1);
        assert_eq!(got.country.as_deref().unwrap_or(""), country, "row {}: {raw:?}", i + 1);
        let q = match quality {
            "" => None,
            "Sd" => Some(Quality::Sd),
            "Hd" => Some(Quality::Hd),
            "Fhd" => Some(Quality::Fhd),
            "Uhd" => Some(Quality::Uhd),
            other => panic!("row {}: bad quality {other:?}", i + 1),
        };
        assert_eq!(got.quality, q, "row {}: {raw:?}", i + 1);
        if let Some(tags) = cols.get(4) {
            let want: Vec<&str> = tags.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
            assert_eq!(got.tags, want, "row {}: {raw:?}", i + 1);
        }
        // display cleanup and identity agree on what the channel is (identity keeps trailing
        // `[…]` tags such as `[Geo-blocked]`; the display drops them)
        if !raw.trim_end().ends_with(']') {
            assert_eq!(normalize_name(&got.name), normalize_name(raw), "row {}: {raw:?}", i + 1);
        }
        rows += 1;
    }
    assert!(rows >= 8, "only {rows} rows");
}

#[test]
fn smart_names_never_empty_and_stable() {
    for raw in ["", " ", "HD", "|EN|", "[VIP]", "UK:", "4K", "ᴴᴰ", "-", "(backup)", "الجزيرة", "日本 ニュース HD"] {
        let c = clean_channel_name(raw);
        if !raw.trim().is_empty() {
            assert!(!c.name.is_empty(), "{raw:?}");
        }
        // cleaning is idempotent on its own output
        assert_eq!(clean_channel_name(&c.name).name, c.name, "{raw:?}");
    }
}
