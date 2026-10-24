//! The relay over loopback (DESIGN §40 §14.2): the real `RelaySession` with real ffprobe and
//! ffmpeg (`-hls_time 1`), fed by the fault-injecting `TestServer` (`tests/support/media.rs`)
//! as the provider, and a reqwest "TV" on 127.0.0.1 (the session's peer). Every test SKIPS with
//! a note when ffmpeg is unavailable, and fails loudly when that happens in CI. Waits are polls
//! with deadlines; nothing sleeps a fixed time to prove a negative.

mod support;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use support::media::{Faults, TestServer, ensure_media};
use tuner::cast::relay::{self, LoadSpec, Prepared, RelayConfig, RelayEvent, RelaySession};
use tuner::config::{CastConvert, CastInterlaced};
use tuner::model::{CastErrorKind, CastHints, CastLevel, HttpHeaders, MediaRef, MovieId, Playable, PlayableKind, ReceiverKind};
use tuner::redact::{Secret, register_secret};

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The media directory, or `None` (skip) when ffmpeg is missing; in CI that is a failure.
fn media() -> Option<PathBuf> {
    relay::ffmpeg::set_segment_seconds_for_tests(1.0);
    let m = ensure_media();
    assert!(m.is_some() || std::env::var_os("CI").is_none(), "ffmpeg is required in CI");
    m
}

fn config(dir: &tempfile::TempDir, peer: Ipv4Addr) -> RelayConfig {
    RelayConfig {
        bind: LOOPBACK,
        peer,
        port: 0,
        min_segments: 2,
        convert: CastConvert::Auto,
        interlaced: CastInterlaced::Auto,
        live_dir: dir.path().join("live"),
        vod_dir: dir.path().join("vod"),
        user_agent: "TunerTest/1.0".into(),
        stream: false,
    }
}

fn playable(kind: PlayableKind, url: String, headers: HttpHeaders) -> Arc<Playable> {
    let media = match kind {
        PlayableKind::Movie => MediaRef::Movie(MovieId(1)),
        _ => MediaRef::Url(String::new()),
    };
    Arc::new(Playable {
        from_download: false,
        media,
        kind,
        item: None,
        source_id: None,
        title: "Test".into(),
        subtitle: None,
        logo: None,
        number: None,
        url: Secret::new(url),
        headers,
        start_at: None,
        duration_hint: None,
    })
}

fn spec(generation: u64, playable: Arc<Playable>, force: Option<CastLevel>) -> LoadSpec {
    LoadSpec { generation, playable, hints: CastHints::default(), receiver: ReceiverKind::AppleTv, model: None, force, stereo: false }
}

/// Events until `want` matches one (returned with everything before it), or panic at `within`.
async fn until(relay: &mut RelaySession, within: Duration, mut want: impl FnMut(&RelayEvent) -> bool) -> Vec<RelayEvent> {
    let deadline = tokio::time::Instant::now() + within;
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, relay.next_event()).await {
            Ok(ev) => {
                let hit = want(&ev);
                seen.push(ev);
                if hit {
                    return seen;
                }
            }
            Err(_) => panic!("no matching relay event within {within:?}; saw {seen:?}"),
        }
    }
}

fn tv() -> reqwest::Client {
    // a TV that follows no redirect (Apple rule 8.18: there must be none) and uses no proxy
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap()
}

/// `http://127.0.0.1:P/c/<tok>/` of a prepared load.
fn base_of(p: &Prepared) -> String {
    p.load.url.expose().trim_end_matches("index.m3u8").to_owned()
}

/// One raw HTTP/1.1 exchange (`Connection: close`): the response bytes without the `date`
/// header, so two responses can be compared byte for byte.
fn raw(port: u16, request_line: &str) -> String {
    let mut s = TcpStream::connect((LOOPBACK, port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // a connection the server closes at accept may refuse the write: what came back still counts
    let _ = s.write_all(format!("{request_line}\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes());
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).lines().filter(|l| !l.to_ascii_lowercase().starts_with("date:")).collect::<Vec<_>>().join("\n")
}

fn port_of(url: &str) -> u16 {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).and_then(|h| h.rsplit(':').next()).and_then(|p| p.parse().ok()).unwrap()
}

/// Sockets toward 127.0.0.1:`port` still open, now: not hung up by whoever holds them
/// (ESTABLISHED, SYN_SENT, CLOSE_WAIT, as `tcp_open`). One closed stays in `/proc/net/tcp`
/// while it winds down (FIN_WAIT, TIME_WAIT), even with an inode: a child forked before the
/// close holds it until its exec, after the client's FIN went out (hyper's shutdown).
fn sockets_to(port: u16) -> usize {
    let tcp = std::fs::read_to_string("/proc/net/tcp").unwrap();
    let remote = format!("{:08X}:{port:04X}", u32::from_ne_bytes(LOOPBACK.octets()));
    // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
    tcp.lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.get(2) == Some(&remote.as_str()) && f.get(3).is_some_and(|st| matches!(*st, "01" | "02" | "08")))
        .count()
}

/// The command lines of this process's relay ffmpeg children (the ones writing HLS or a
/// full-length run's segments: not the marker film's analysis, which runs ffmpeg on files beside
/// them).
fn ffmpeg_cmdlines() -> Vec<String> {
    let me = std::process::id().to_string();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // pid (comm) state ppid …: the comm may hold spaces, so split after its last ')'
        let Some(after) = stat.rsplit_once(')').map(|(_, a)| a) else { continue };
        if after.split_whitespace().nth(1) != Some(me.as_str()) {
            continue;
        }
        if let Ok(cmd) = std::fs::read(e.path().join("cmdline")) {
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
            if cmd.contains("ffmpeg") && (cmd.contains(" -f hls ") || cmd.contains(" -f segment ")) {
                out.push(cmd);
            }
        }
    }
    out
}

/// What `max_concurrent` (the relay's one-connection assertions) measures: client sockets open
/// in the kernel at once, not the server's view of them. A client that hangs up on a request
/// before its answer, waits for the kernel to close it (as the relay does: a child forked
/// meanwhile holds a copy until its exec) and only then connects again counts once, though the
/// server's task, still reading the file, notices the hang-up later (the server's own count gave
/// 2 to 4 here with no load, and failed the live channel's assertion once in a full run); one
/// that connects while holding its previous socket counts twice.
#[test]
fn the_servers_connection_count_is_the_clients() {
    use support::media::tcp_open;
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let addr = server.base.trim_start_matches("http://").to_owned();
    let get = |s: &mut TcpStream, path: &str| write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let close = |s: TcpStream| {
        let (ours, theirs) = (s.local_addr().unwrap(), s.peer_addr().unwrap());
        drop(s);
        let deadline = Instant::now() + Duration::from_secs(5);
        while tcp_open(ours, theirs).unwrap() {
            assert!(Instant::now() < deadline, "a closed socket stayed open");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    for path in ["/live/seg003.ts", "/testsrc.mp4"].iter().cycle().take(40) {
        let mut a = TcpStream::connect(&addr).unwrap();
        get(&mut a, path);
        close(a);
        let mut b = TcpStream::connect(&addr).unwrap();
        get(&mut b, "/live/index.m3u8");
        let _ = b.read_to_end(&mut Vec::new());
        close(b);
    }
    assert_eq!(server.max_concurrent(), 1, "{:#?}", server.overlaps());

    let mut a = TcpStream::connect(&addr).unwrap();
    get(&mut a, "/testsrc.mp4");
    let b = TcpStream::connect(&addr).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.max_concurrent() < 2 {
        assert!(Instant::now() < deadline, "a connection made while holding another wasn't counted");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.overlaps().len(), 1, "{:#?}", server.overlaps());
    drop((a, b));
}

/// A live HLS channel (the server's sliding `/live` window of 1 s segments) as the TV sees it:
/// a media playlist that reaches ≥ 6 segments, `PROGRAM-DATE-TIME` on every one, a stable
/// target duration, no redirect, CORS, the right MIME types and lengths, one provider
/// connection at a time across ffprobe and ffmpeg, and the fetch events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_channel_reaches_the_tv_as_an_apple_media_playlist() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let p = playable(PlayableKind::Live, server.url("/live/index.m3u8"), HttpHeaders::default());
    let prepared = relay.load(spec(1, p, None)).await.unwrap();
    assert_eq!(prepared.level, CastLevel::Repackaged, "H.264 + AAC, progressive: copied");
    assert!(prepared.load.live && prepared.load.offset_s == 0.0);
    assert!(prepared.behind_live_s.is_some_and(|s| (3..=30).contains(&s)), "{:?}", prepared.behind_live_s);

    let tv = tv();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut targets = Vec::new();
    let text = loop {
        let r = tv.get(prepared.load.url.expose()).send().await.unwrap();
        assert_eq!(r.status(), 200, "no 3xx, ever");
        let h = r.headers();
        assert_eq!(h["content-type"], "application/vnd.apple.mpegurl");
        assert_eq!(h["cache-control"], "no-cache");
        assert_eq!(h["access-control-allow-origin"], "*");
        assert!(h["access-control-allow-headers"].to_str().unwrap().contains("Range"));
        let text = r.text().await.unwrap();
        assert!(!text.contains("#EXT-X-STREAM-INF"), "a media playlist, never multivariant (§7.6 #11)");
        let target = text.lines().find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:")).unwrap().to_owned();
        targets.push(target);
        if text.lines().filter(|l| l.starts_with("#EXTINF")).count() >= 6 {
            break text;
        }
        assert!(Instant::now() < deadline, "fewer than 6 segments after 45 s:\n{text}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    targets.dedup();
    assert_eq!(targets.len(), 1, "TARGETDURATION changed: {targets:?}");
    let lines: Vec<&str> = text.lines().collect();
    for (i, l) in lines.iter().enumerate().filter(|(_, l)| !l.starts_with('#')) {
        let mut tags = lines[..i].iter().rev().take_while(|p| p.starts_with('#'));
        assert!(tags.any(|p| p.starts_with("#EXT-X-PROGRAM-DATE-TIME:")), "no PDT before {l}:\n{text}");
        assert!(l.starts_with('s') && l.ends_with(".ts") && !l.contains('/'), "relative names only: {l}");
    }
    let seg = lines.iter().rev().find(|l| !l.starts_with('#')).unwrap();
    let r = tv.get(format!("{}{seg}", base_of(&prepared))).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "video/mp2t");
    let len: usize = r.headers()["content-length"].to_str().unwrap().parse().unwrap();
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), len);
    assert_eq!(body[0], 0x47, "an MPEG-TS segment");

    let seen =
        until(&mut relay, Duration::from_secs(10), |e| matches!(e, RelayEvent::SegmentFetched { generation: 1, upto_s, .. } if *upto_s > 0.0)).await;
    assert!(seen.contains(&RelayEvent::UpstreamOpened { generation: 1 }), "{seen:?}");
    assert!(seen.contains(&RelayEvent::PlaylistFetched { generation: 1 }), "{seen:?}");
    assert!(seen.iter().any(|e| matches!(e, RelayEvent::Probed { generation: 1, .. })), "{seen:?}");
    assert_eq!(server.max_concurrent(), 1, "one provider connection at a time, probe included: {:#?}", server.overlaps());

    relay.stop(false).await;
    until(&mut relay, Duration::from_secs(5), |e| matches!(e, RelayEvent::UpstreamClosed { generation: 1, .. })).await;
    assert!(tv.get(prepared.load.url.expose()).send().await.is_err(), "nothing listens once the session is gone");
    assert!(!dir.path().join("live").read_dir().unwrap().any(|_| true), "the session directory is wiped");
}

/// A film (`testsrc.mp4`, moov at the end: ffprobe, the index reader and ffmpeg seek it with
/// `Range` through the proxy) reaches the TV whole (§8.5), and the server's rules: byte-identical
/// 404s for a wrong token, other paths and methods (a segment past the film too); a wrong peer
/// closed unanswered; HEAD and `Range`; a seek is the whole film under a new token that starts
/// the TV there, while the old token 404s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_answers_its_token_and_peer_only_and_a_seek_is_the_whole_film_from_there() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let p = playable(PlayableKind::Movie, server.url("/testsrc.mp4"), HttpHeaders::default());
    let prepared = relay.load(spec(7, p.clone(), None)).await.unwrap();
    assert!(!prepared.load.live && prepared.behind_live_s.is_none() && prepared.load.offset_s == 0.0);
    let base = base_of(&prepared);
    let port = port_of(&base);
    let tok = base.trim_end_matches('/').rsplit('/').next().unwrap().to_owned();
    let tv = tv();

    // the whole film at once: 2 s GOPs, a segment each
    let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!(
        text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n") && text.contains("#EXT-X-MEDIA-SEQUENCE:0\n") && text.ends_with("#EXT-X-ENDLIST\n"),
        "{text}"
    );
    assert_eq!(vod_segments(&text).len(), 5, "{text}");
    assert!(!text.contains("#EXT-X-START") && !text.contains("PROGRAM-DATE-TIME"), "{text}");

    let miss = raw(port, &format!("GET /c/{}/index.m3u8 HTTP/1.1", "a".repeat(26)));
    assert!(miss.starts_with("HTTP/1.1 404"), "{miss}");
    for req in [
        format!("GET /c/{tok}/s1.TS HTTP/1.1"),
        format!("GET /c/{tok}/../index.m3u8 HTTP/1.1"),
        format!("GET /c/{tok}/%2e%2e/index.m3u8 HTTP/1.1"),
        format!("GET /c/{tok}/s999.ts HTTP/1.1"),
        format!("GET /c/{tok}/init.mp4 HTTP/1.1"),
        format!("GET /c/{tok} HTTP/1.1"),
        format!("POST /c/{tok}/index.m3u8 HTTP/1.1"),
        format!("OPTIONS /c/{tok}/index.m3u8 HTTP/1.1"),
        "GET / HTTP/1.1".to_owned(),
        "GET /u/x HTTP/1.1".to_owned(),
    ] {
        assert_eq!(raw(port, &req), miss, "{req}");
    }
    // the right token from another host: closed at accept, unanswered (it never holds a slot)
    let other = tempfile::tempdir().unwrap();
    let mut elsewhere = RelaySession::start(config(&other, Ipv4Addr::new(10, 9, 8, 7))).await.unwrap();
    let theirs = elsewhere.load(spec(1, p, None)).await.unwrap();
    let their_port = port_of(theirs.load.url.expose());
    let their_path = theirs.load.url.expose().split_once(&format!(":{their_port}")).unwrap().1.to_owned();
    assert_eq!(raw(their_port, &format!("GET {their_path} HTTP/1.1")), "", "wrong peer");
    elsewhere.stop(false).await;

    // HEAD and Range on a segment
    let s0 = tv.get(format!("{base}s0.ts")).send().await.unwrap().bytes().await.unwrap();
    let head = tv.head(format!("{base}s0.ts")).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"].to_str().unwrap(), s0.len().to_string());
    assert_eq!(head.headers()["accept-ranges"], "bytes");
    let part = tv.get(format!("{base}s0.ts")).header("Range", "bytes=0-1").send().await.unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"].to_str().unwrap(), format!("bytes 0-1/{}", s0.len()));
    assert_eq!(part.headers()["content-length"], "2");
    assert_eq!(&part.bytes().await.unwrap()[..], &s0[..2]);

    // a seek: the whole film again, starting the TV at 4.5 s; positions are the film's own
    let view = relay.view(8, 4.5).await.unwrap().expect("anywhere in the film");
    assert_eq!(view.load.offset_s, 0.0);
    let vtext = tv.get(view.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!(
        vtext
            .contains("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-START:TIME-OFFSET=4.500,PRECISE=YES\n"),
        "{vtext}"
    );
    let shared = tv.get(format!("{}s2.ts", base_of(&view))).send().await.unwrap();
    assert_eq!(shared.status(), 200);
    assert_eq!(raw(port, &format!("GET /c/{tok}/index.m3u8 HTTP/1.1")), miss, "the old token is revoked");
    let past = relay.view(9, 3600.0).await.unwrap().expect("clamped to the film");
    let ptext = tv.get(past.load.url.expose()).send().await.unwrap().text().await.unwrap();
    let at = ext_x_start(&ptext).expect("a start");
    assert!((8.9..9.1).contains(&at), "a second before the end: {ptext}");
    assert_eq!(server.max_concurrent(), 1, "one provider connection at a time: {:#?}", server.overlaps());
    relay.stop(false).await;
}

/// An item without a keyframe index keeps Phase 1's EVENT playlist (§8.5's fallback matrix), and
/// a seek inside what's produced is a view: `MEDIA-SEQUENCE:k`, the segments from k with their
/// `PROGRAM-DATE-TIME`, under a new token that serves the shared segments; beyond it, `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unindexed_film_keeps_a_growing_playlist_whose_views_share_the_segments() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    // an HLS film: no index to read (1 s GOPs, 60 s)
    let p = playable(PlayableKind::Movie, server.url("/long/index.m3u8"), HttpHeaders::default());
    let prepared = relay.load(spec(7, p, None)).await.unwrap();
    let base = base_of(&prepared);
    let tok = base.trim_end_matches('/').rsplit('/').next().unwrap().to_owned();
    let tv = tv();
    let deadline = Instant::now() + Duration::from_secs(30);
    let text = loop {
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        if text.matches(".ts\n").count() >= 8 {
            break text;
        }
        assert!(Instant::now() < deadline, "too few segments:\n{text}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n") && text.contains("#EXT-X-START:TIME-OFFSET=0\n"), "{text}");

    let view = relay.view(8, 4.5).await.unwrap().expect("inside what's produced");
    assert!((view.load.offset_s - 4.0).abs() < 0.05, "{}", view.load.offset_s);
    let vtext = tv.get(view.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!(vtext.contains("#EXT-X-MEDIA-SEQUENCE:4\n") && vtext.contains("\ns4.ts\n") && !vtext.contains("\ns3.ts\n"), "{vtext}");
    assert!(vtext.contains("#EXT-X-PROGRAM-DATE-TIME:"), "{vtext}");
    let shared = tv.get(format!("{}s4.ts", base_of(&view))).send().await.unwrap();
    assert_eq!(shared.status(), 200);
    let old = tv.get(format!("{base}index.m3u8")).send().await.unwrap();
    assert_eq!(old.status(), 404, "the old token ({tok}) is revoked");
    assert!(relay.view(9, 3600.0).await.unwrap().is_none(), "beyond the film");
    relay.stop(false).await;
}

/// A film with 5.1 sound reaches the Apple TV as Apple's HLS authoring spec asks (§8.4): a
/// multivariant playlist with a surround group (the source's AC-3, AAC 5.1 or E-AC-3 copied, its
/// FLAC converted to AC-3) and a stereo AAC group, the video listed once per group with what was
/// produced (codec string from the init segment, size, frame rate, measured bandwidths, the
/// stream's language); three fMP4 media playlists under the token, one pinned target,
/// `EXT-X-START` at 0; and the surround rendition's media really is six channels of that codec.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surround_film_gets_a_stereo_and_a_surround_rendition() {
    let Some(media) = media() else { return };
    let caps = relay::ffmpeg::caps().await.unwrap();
    let server = TestServer::start(media, Faults::default());
    for (file, codec, codecs, lang) in [
        ("/surround.ts", "ac3", "ac-3", "en"),
        ("/surround.mp4", "aac", "mp4a.40.2", "en"),
        ("/surround_eac3.mkv", "eac3", "ec-3", "en"),
        ("/surround_flac.mkv", "ac3", "ac-3", "de"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let p = playable(PlayableKind::Movie, server.url(file), HttpHeaders::default());
        let prepared = relay.load(spec(1, p, None)).await.unwrap_or_else(|e| panic!("{file}: {e:?}"));
        assert!(prepared.surround && prepared.level == CastLevel::Repackaged, "{file}: {prepared:?}");
        let (tv, base) = (tv(), base_of(&prepared));
        let master = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        let lines: Vec<&str> = master.lines().collect();
        assert_eq!(lines[..3], ["#EXTM3U", "#EXT-X-VERSION:7", "#EXT-X-INDEPENDENT-SEGMENTS"], "{file}:\n{master}");
        assert_eq!(
            lines[3],
            format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"surround\",LANGUAGE=\"{lang}\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"6\",URI=\"s/index.m3u8\""
            )
        );
        assert_eq!(
            lines[4],
            format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"stereo\",LANGUAGE=\"{lang}\",NAME=\"Main\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"2\",URI=\"a/index.m3u8\""
            )
        );
        for (line, uri, group, audio) in [(5, 6, "surround", codecs), (7, 8, "stereo", "mp4a.40.2")] {
            let inf = lines[line];
            assert!(
                inf.contains("CODECS=\"avc1.")
                    && inf.contains(&format!(",{audio}\",RESOLUTION=320x180,FRAME-RATE=25.000,VIDEO-RANGE=SDR,AUDIO=\"{group}\"")),
                "{file}: {inf}"
            );
            let bw = |key: &str| inf.split(&format!("{key}=")).nth(1).and_then(|r| r.split(',').next()).and_then(|n| n.parse::<u64>().ok()).unwrap();
            assert!(bw("BANDWIDTH") >= bw("AVERAGE-BANDWIDTH") && (20_000..20_000_000).contains(&bw("BANDWIDTH")), "{file}: {inf}");
            assert_eq!(lines[uri], "v/index.m3u8");
        }
        let mut targets = Vec::new();
        let mut maps = Vec::new();
        for (r, init, mime) in [("v", "init_0.mp4", "video/mp4"), ("a", "init_1.mp4", "audio/mp4"), ("s", "init_2.mp4", "audio/mp4")] {
            let text = tv.get(format!("{base}{r}/index.m3u8")).send().await.unwrap().text().await.unwrap();
            // the MPEG-TS film has no index: an EVENT playlist that starts the TV at 0; the MP4
            // and Matroska ones go full length (§8.5), every rendition on the same map
            let start = if file == "/surround.ts" { "#EXT-X-START:TIME-OFFSET=0\n" } else { "#EXT-X-PLAYLIST-TYPE:VOD\n" };
            assert!(text.contains(&format!("#EXT-X-MAP:URI=\"{init}\"")) && text.contains(start), "{file} {r}:\n{text}");
            if file != "/surround.ts" {
                maps.push(vod_segments(&text));
            }
            targets.push(text.lines().find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:")).unwrap().to_owned());
            for name in [init, "s0.m4s"] {
                let resp = tv.get(format!("{base}{r}/{name}")).send().await.unwrap();
                assert_eq!((resp.status().as_u16(), resp.headers()["content-type"].to_str().unwrap()), (200, mime), "{file} {r}/{name}");
            }
        }
        targets.dedup();
        assert_eq!(targets.len(), 1, "one target for every rendition (Apple rule 8.2): {targets:?}");
        maps.dedup();
        assert!(maps.len() <= 1, "{file}: one map for every rendition: {maps:?}");
        // the surround rendition is the source's 5.1, copied
        let mut mp4 = tv.get(format!("{base}s/init_2.mp4")).send().await.unwrap().bytes().await.unwrap().to_vec();
        mp4.extend_from_slice(&tv.get(format!("{base}s/s0.m4s")).send().await.unwrap().bytes().await.unwrap());
        let probe_at = dir.path().join("surround.mp4");
        std::fs::write(&probe_at, &mp4).unwrap();
        let probe = relay::probe::probe(&caps.ffprobe, probe_at.to_str().unwrap(), false).await.unwrap();
        let a = probe.chosen_audio().unwrap();
        assert_eq!((a.codec.as_str(), a.channels), (codec, Some(6)), "{file}: {probe:?}");
        assert!(tv.get(format!("{base}init_0.mp4")).send().await.unwrap().status() == 404, "only the multivariant playlist at the top");
        relay.stop(false).await;
    }
}

/// Every rendition shares the picture's timeline (cast design §8.4; `relay::ffmpeg` module docs,
/// "One timeline"), measured on what the TV is served: the `avsync` marker film (at each second
/// k a white picture and a beep lasting 40 ms × (1 + k mod 23)) from the start and resumed
/// between keyframes, copied and converted, with surround (fMP4 renditions) and in stereo
/// (MPEG-TS). The picture's times come from the boxes (`tfdt`, durations, composition offsets:
/// what the Apple TV reads, no edit list), the sound's from ffmpeg on that timeline. On
/// 2026-10-13 the converted, resumed surround cast played its copied 5.1 10 s late.
///
/// The film is cast full length (§8.5): its segments are made on demand by runs that start where
/// the TV starts (`EXT-X-START`), and where it scrubs to (back from 31 s to 17 s: a second run,
/// whose segments join the first's); converted, it is cut on the grid of its forced keyframes.
/// Without cues, or from a provider that ignores `Range`, it keeps its EVENT playlist. Either way
/// the TV's picture starts where the session counts from, never later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_rendition_shares_the_pictures_timeline() {
    let Some(media) = media() else { return };
    let caps = relay::ffmpeg::caps().await.unwrap();
    let server = TestServer::start(media.clone(), Faults::default());
    // a provider that ignores `Range`: no full length (§8.5)
    let whole = TestServer::start(media, Faults::default());
    whole.set_ranges(false);
    let converted = Some(CastLevel::Converted);
    let (cued, uncued) = ("/avsync.mkv", "/avsync_nocues.mkv");
    for (file, ranges, receiver, force, start_at, scrub) in [
        (cued, true, ReceiverKind::AppleTv, None, None, None),
        (cued, true, ReceiverKind::AppleTv, None, Some(13.3), None),
        (cued, true, ReceiverKind::AppleTv, converted, Some(13.3), None),
        (cued, true, ReceiverKind::AirPlayTv, None, Some(13.3), None),
        (cued, true, ReceiverKind::AirPlayTv, converted, Some(13.3), None),
        (cued, true, ReceiverKind::AppleTv, None, Some(31.0), Some(17.0)),
        // no index: an EVENT playlist, copied or converted
        (uncued, true, ReceiverKind::AppleTv, None, Some(13.3), None),
        (uncued, true, ReceiverKind::AppleTv, converted, Some(13.3), None),
        (uncued, true, ReceiverKind::AirPlayTv, None, Some(13.3), None),
        (cued, false, ReceiverKind::AppleTv, None, None, None),
    ] {
        let full = ranges && file == cued;
        let case = format!("{file}{} {receiver:?} {force:?} from {start_at:?} scrubbed to {scrub:?}", if ranges { "" } else { " (no Range)" });
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let url = if ranges { server.url(file) } else { whole.url(file) };
        let mut p = (*playable(PlayableKind::Movie, url, HttpHeaders::default())).clone();
        p.start_at = start_at;
        let spec = LoadSpec { receiver, ..spec(1, Arc::new(p), force) };
        let prepared = relay.load(spec).await.unwrap_or_else(|e| panic!("{case}: {e:?}"));
        assert_eq!(prepared.surround, receiver == ReceiverKind::AppleTv, "{case}");
        // the info panel's report of what the TV is fed (§11.3), full length or not: queued
        // before the load returns
        let seen = until(&mut relay, Duration::from_secs(5), |e| matches!(e, RelayEvent::Probed { info, .. } if info.cast.is_some())).await;
        let RelayEvent::Probed { info, .. } = seen.last().unwrap() else { unreachable!() };
        let fed = info.cast.clone().unwrap();
        assert!(fed.probed && fed.target_s.is_some(), "{case}: {fed:?}");
        assert_eq!((fed.level, fed.surround.is_some(), fed.full_length.is_some()), (prepared.level, prepared.surround, full), "{case}: {fed:?}");
        let (tv, base) = (tv(), base_of(&prepared));
        let renditions: &[&str] = if prepared.surround { &["v/", "a/", "s/"] } else { &[""] };
        // full length: the TV fetches where the playlist starts it, then (a scrub) jumps
        let mut from = None;
        let mut files = Vec::new();
        for r in renditions {
            let text = tv.get(format!("{base}{r}index.m3u8")).send().await.unwrap().text().await.unwrap();
            assert_eq!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"), full, "{case} {r}:\n{text}");
            let (names, starts): (Vec<String>, Option<f64>) = if full {
                let segs = vod_segments(&text);
                // the whole playlist: the panel's count and target are the TV's
                assert_eq!(fed.full_length, Some(segs.len() as u64), "{case} {r}");
                let target = fed.target_s.unwrap();
                assert!(text.contains(&format!("#EXT-X-TARGETDURATION:{target}\n")), "{case} {r}: target {target}:\n{text}");
                assert_eq!(ext_x_start(&text), start_at, "{case} {r}");
                let at = |t: f64| segs.iter().rposition(|s| s.0 <= t).unwrap();
                let k = at(start_at.unwrap_or(0.0));
                if from.is_none() && scrub.is_some() {
                    assert_eq!(tv.get(format!("{base}{r}{}", segs[k].2)).send().await.unwrap().status(), 200, "{case}: where the TV starts");
                }
                let first = scrub.map_or(k, at);
                from = Some(segs[first].0);
                (segs[first..].iter().map(|s| s.2.clone()).collect(), from)
            } else {
                // the whole film: ffmpeg reads the first minute at once
                let deadline = Instant::now() + Duration::from_secs(60);
                let text = loop {
                    let text = tv.get(format!("{base}{r}index.m3u8")).send().await.unwrap().text().await.unwrap();
                    if text.contains("#EXT-X-ENDLIST") {
                        break text;
                    }
                    assert!(Instant::now() < deadline, "{case} {r}: no ENDLIST:\n{text}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                };
                (text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect(), None)
            };
            let init = text.lines().find_map(|l| l.strip_prefix("#EXT-X-MAP:URI=\"").map(|u| u.trim_end_matches('"').to_owned()));
            let mut bytes = Vec::new();
            for name in init.iter().chain(&names) {
                let resp = tv.get(format!("{base}{r}{name}")).send().await.unwrap();
                assert_eq!(resp.status(), 200, "{case} {r}{name}");
                bytes.extend_from_slice(&resp.bytes().await.unwrap());
            }
            let ext = if init.is_some() { "mp4" } else { "ts" };
            let path = dir.path().join(format!("{}.{ext}", if r.is_empty() { "muxed" } else { r.trim_end_matches('/') }));
            std::fs::write(&path, &bytes).unwrap();
            files.push(path);
            let _ = starts;
        }
        relay.stop(false).await;
        let (ffmpeg, ffprobe) = (caps.ffmpeg.clone(), caps.ffprobe.clone());
        let (offsets, first) =
            tokio::task::spawn_blocking(move || (avsync::offsets(&ffmpeg, &files), avsync::first_picture(&ffprobe, &files[0]))).await.unwrap();
        let (picture, sounds) = offsets.split_first().unwrap();
        for (i, sound) in sounds.iter().enumerate() {
            let av_ms = (sound - picture) * 1000.0;
            assert!(av_ms.abs() < 40.0, "{case}: rendition {i}'s sound is {av_ms:+.1} ms off the picture ({offsets:?})");
        }
        // where the TV's picture starts in the film is where the session counts from: a
        // full-length playlist's segment start (the film's own time: the session adds nothing),
        // an EVENT playlist's offset; a copy starts at the keyframe before the point asked (10 s
        // for 13.3 s: 5 s GOPs); positions and saved progress never run ahead of the picture
        let shown = first - picture;
        let said = if full { from.unwrap() } else { prepared.load.offset_s };
        if full {
            assert_eq!(prepared.load.offset_s, 0.0, "{case}");
        }
        assert!((said - shown).abs() < 0.1, "{case}: the session counts from {said:.3} s, the TV's picture starts at {shown:.3} s");
        assert!(said <= shown + 0.03, "{case}: ahead of the picture ({said:.3} > {shown:.3})");
        if force.is_none() && start_at.is_some() && scrub.is_none() {
            assert!((said - 10.0).abs() < 0.05, "{case}: the copy starts at the keyframe: {said:.3}");
        }
        if let Some(to) = scrub {
            assert!((said - 15.0).abs() < 0.05, "{case}: the scrub to {to} s starts at its keyframe: {said:.3}");
        }
    }
}

/// The marker film's timing (see `every_rendition_shares_the_pictures_timeline`).
mod avsync {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A marker at second k lasts `STEP` × (1 + k mod `CODES`).
    const STEP: f64 = 0.04;
    const CODES: i64 = 23;

    /// For the files of one cast (the picture's first: muxed sound in it, or demuxed renditions
    /// after it), where content second 0 falls on each one's timeline: the picture's, then each
    /// sound's.
    pub fn offsets(ffmpeg: &Path, files: &[PathBuf]) -> Vec<f64> {
        let picture = offset(&flashes(ffmpeg, &files[0]), None);
        let sounds: &[PathBuf] = if files.len() == 1 { files } else { &files[1..] };
        let mut out = vec![picture];
        out.extend(sounds.iter().map(|f| offset(&beeps(ffmpeg, f), Some(picture))));
        out
    }

    /// The picture's first presentation time on the file's timeline (fMP4: from the boxes, as
    /// the TV reads them; MPEG-TS: the PTS).
    pub fn first_picture(ffprobe: &Path, file: &Path) -> f64 {
        let times = if file.extension().is_some_and(|e| e == "mp4") {
            picture_times(&std::fs::read(file).unwrap())
        } else {
            let out = Command::new(ffprobe)
                .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-read_intervals", "%+#16", "-of", "csv=p=0"])
                .arg(file)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect()
        };
        times.into_iter().reduce(f64::min).expect("a picture")
    }

    /// The median of (onset − its second) over the markers named by their lengths; `near`: the
    /// offset a marker's second is chosen around (else the first marker's code is its second).
    fn offset(runs: &[(f64, f64)], near: Option<f64>) -> f64 {
        let mut near = near;
        let mut offs: Vec<f64> = runs[..runs.len().saturating_sub(1)]
            .iter()
            .filter_map(|&(t, d)| {
                let code = (d / STEP).round() as i64 - 1;
                if !(0..CODES).contains(&code) {
                    return None;
                }
                let anchor = *near.get_or_insert(t - code as f64);
                let k = code + CODES * ((t - anchor - code as f64) / CODES as f64).round() as i64;
                Some(t - k as f64)
            })
            .collect();
        assert!(offs.len() >= 5, "too few markers: {runs:?}");
        offs.sort_by(f64::total_cmp);
        offs[offs.len() / 2]
    }

    fn ffmpeg_out(ffmpeg: &Path, args: &[&str]) -> String {
        let out = Command::new(ffmpeg).args(["-nostdin", "-hide_banner", "-loglevel", "error"]).args(args).output().unwrap();
        assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `(onset, length)` of each lit run of `(time, on)` samples.
    fn runs(points: impl IntoIterator<Item = (f64, bool)>) -> Vec<(f64, f64)> {
        let (mut out, mut start) = (Vec::new(), None);
        for (t, on) in points {
            match (on, start) {
                (true, None) => start = Some(t),
                (false, Some(s)) => {
                    out.push((s, t - s));
                    start = None;
                }
                _ => {}
            }
        }
        out
    }

    /// `(pts_time, value)` of each frame `ffmpeg … metadata=print:key=<key>` prints.
    fn printed(text: &str, key: &str) -> Vec<(f64, f64)> {
        let mut pts = None;
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(p) = line.split("pts_time:").nth(1) {
                pts = p.split_whitespace().next().and_then(|v| v.parse().ok());
            } else if let (Some(v), Some(t)) = (line.strip_prefix(&format!("{key}=")), pts) {
                out.push((t, v.parse().unwrap_or(-200.0)));
            }
        }
        out
    }

    fn input(file: &Path) -> Vec<&str> {
        // the timeline as written: no start shift, and (fMP4) no edit list
        let mut a = vec!["-copyts"];
        if file.extension().is_some_and(|e| e == "mp4") {
            a.extend(["-ignore_editlist", "1"]);
        }
        a.extend(["-i", file.to_str().unwrap()]);
        a
    }

    /// The white pictures. fMP4: their presentation times from the boxes, matched in order to
    /// the decoded frames (the leading pictures of an open GOP don't decode: they come first);
    /// MPEG-TS: the PTS.
    fn flashes(ffmpeg: &Path, file: &Path) -> Vec<(f64, f64)> {
        let key = "lavfi.signalstats.YAVG";
        let mut args = input(file);
        args.extend([
            "-map",
            "0:v:0",
            "-vf",
            "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
            "-fps_mode",
            "passthrough",
            "-f",
            "null",
            "-",
        ]);
        let frames = printed(&ffmpeg_out(ffmpeg, &args), key);
        if file.extension().is_some_and(|e| e == "ts") {
            return runs(frames.into_iter().map(|(t, y)| (t, y > 128.0)));
        }
        let times = picture_times(&std::fs::read(file).unwrap());
        let skip = times.len().saturating_sub(frames.len());
        runs(times[skip..].iter().zip(&frames).map(|(&t, &(_, y))| (t, y > 128.0)))
    }

    /// The beeps (peak level over 1 ms, mono, on ffmpeg's timeline of the file).
    fn beeps(ffmpeg: &Path, file: &Path) -> Vec<(f64, f64)> {
        let key = "lavfi.astats.Overall.Peak_level";
        let mut args = input(file);
        let af = format!("aresample=8000,pan=mono|c0=c0,asetnsamples=n=8,astats=metadata=1:reset=1,ametadata=print:key={key}:file=-");
        args.extend(["-map", "0:a:0", "-af", &af, "-f", "null", "-"]);
        runs(printed(&ffmpeg_out(ffmpeg, &args), key).into_iter().map(|(t, db)| (t, db > -12.0)))
    }

    /// ISO/IEC 14496-12 boxes of `b`: `(type, body)`.
    fn boxes(mut b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
        std::iter::from_fn(move || {
            let size = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
            let kind: [u8; 4] = b.get(4..8)?.try_into().ok()?;
            let (hdr, size) = if size == 1 { (16, u64::from_be_bytes(b.get(8..16)?.try_into().ok()?) as usize) } else { (8, size) };
            let body = b.get(hdr..size)?;
            b = &b[size..];
            Some((kind, body))
        })
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// The presentation times (s) of track 1's samples (ffmpeg maps the picture first), sorted:
    /// each fragment's `tfdt`, plus the durations before, plus the composition offset (signed in
    /// `trun` version 1). Edit lists are not applied.
    fn picture_times(file: &[u8]) -> Vec<f64> {
        let child = |b: &[u8], k: &[u8; 4]| boxes(b).find(|(kind, _)| kind == k).map(|(_, body)| body.to_vec());
        let moov = child(file, b"moov").expect("an init segment first");
        let trak = child(&moov, b"trak").unwrap();
        let mdhd = child(&child(&trak, b"mdia").unwrap(), b"mdhd").unwrap();
        let timescale = f64::from(if mdhd[0] == 1 { be32(&mdhd, 20) } else { be32(&mdhd, 12) });
        let trex_duration = child(&child(&moov, b"mvex").unwrap(), b"trex").map_or(0, |t| be32(&t, 12));
        let mut out = Vec::new();
        for (_, moof) in boxes(file).filter(|(k, _)| k == b"moof") {
            for (_, traf) in boxes(moof).filter(|(k, _)| k == b"traf") {
                let tfhd = child(traf, b"tfhd").unwrap();
                if be32(&tfhd, 4) != 1 {
                    continue;
                }
                let flags = be32(&tfhd, 0) & 0xff_ffff;
                let at = 8 + if flags & 1 != 0 { 8 } else { 0 } + if flags & 2 != 0 { 4 } else { 0 };
                let default_duration = if flags & 8 != 0 { be32(&tfhd, at) } else { trex_duration };
                let tfdt = child(traf, b"tfdt").unwrap();
                let mut t = if tfdt[0] == 1 { u64::from_be_bytes(tfdt[4..12].try_into().unwrap()) as i64 } else { i64::from(be32(&tfdt, 4)) };
                let trun = child(traf, b"trun").unwrap();
                let (version, flags, n) = (trun[0], be32(&trun, 0) & 0xff_ffff, be32(&trun, 4));
                let mut j = 8 + if flags & 1 != 0 { 4 } else { 0 } + if flags & 4 != 0 { 4 } else { 0 };
                for _ in 0..n {
                    let mut duration = default_duration;
                    if flags & 0x100 != 0 {
                        duration = be32(&trun, j);
                        j += 4;
                    }
                    j += if flags & 0x200 != 0 { 4 } else { 0 } + if flags & 0x400 != 0 { 4 } else { 0 };
                    let mut cto = 0i64;
                    if flags & 0x800 != 0 {
                        let raw = be32(&trun, j);
                        cto = if version == 1 { i64::from(raw as i32) } else { i64::from(raw) };
                        j += 4;
                    }
                    out.push((t + cto) as f64 / timescale);
                    t += i64::from(duration);
                }
            }
        }
        out.sort_by(f64::total_cmp);
        out
    }
}

/// Live MPEG-TS (`bars.ts`) converted (2 s GOPs): ffprobe and ffmpeg read one provider request
/// (the head buffer replays byte 0 to ffmpeg).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_and_stream_share_one_provider_request() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let p = playable(PlayableKind::Live, server.url("/bars.ts"), HttpHeaders::default());
    let prepared = relay.load(spec(1, p, Some(CastLevel::Converted))).await.unwrap();
    assert_eq!(prepared.level, CastLevel::Converted);
    let seen = until(&mut relay, Duration::from_secs(5), |e| matches!(e, RelayEvent::Probed { .. })).await;
    let RelayEvent::Probed { info, .. } = seen.last().unwrap() else { unreachable!() };
    assert_eq!((info.width, info.height), (Some(1280), Some(720)));
    assert_eq!(server.requests_for("/bars.ts").len(), 1, "{:?}", server.requests());
    assert_eq!(server.max_concurrent(), 1, "{:#?}", server.overlaps());
    relay.stop(false).await;
}

/// A request that supersedes one still downloading (a seek, a reconnect, the next segment asked
/// for early) cuts it, and the provider sees that connection close before the next one opens
/// (§10). A closed feed's socket is gone by the time `close` returns (its `UpstreamClosed` frees
/// the lease: mpv or the next load may connect at once). The socket closes on hyper's connection
/// task some time after the proxy drops the response, so both have to wait for that, not only for
/// the pump. Tuner forks children while it streams (ffprobe, ffmpeg: [`Forks`]), each holding a
/// copy of the sockets open at its fork until its exec; the provider still sees the connection
/// close, as hyper half-closes it (FIN) before letting go. The throttle keeps segment A mid-body
/// when B is asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_request_closes_before_the_next_one_connects() {
    use tuner::cast::relay::upstream::{FeedCtx, Pick, Proxy};
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let _forks = Forks::start();
    let proxy = Proxy::start().await.unwrap();
    let client = tv();
    let port = port_of(&server.base);
    for round in 0..10 {
        // a fresh feed each round: no tape holds A or B yet
        server.set_throttle(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = FeedCtx { generation: Arc::new(std::sync::atomic::AtomicU64::new(round)), tx, label: "cast session test".into() };
        let feed = proxy.feed(ctx, &Secret::new(server.url("/live/index.m3u8")), &HttpHeaders::default(), "TunerTest/1.0");
        feed.open(Pick::default()).await;
        assert!(feed.is_hls() && !feed.failed());
        let text = client.get(feed.loopback_url().expose()).send().await.unwrap().text().await.unwrap();
        let segs: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(segs.len() >= 2, "{text}");
        server.set_throttle(Some(16_000)); // about 3 s per segment: A is mid-body when B is asked for
        let a = client.get(segs[0]).send().await.unwrap();
        assert_eq!(a.status(), 200);
        let b = client.get(segs[1]).send().await.unwrap();
        assert_eq!(b.status(), 200);
        assert_eq!(
            server.max_concurrent(),
            1,
            "round {round}: a provider connection opened before the previous one closed: {:#?}",
            server.overlaps()
        );
        drop((a, b));
        proxy.forget(&feed);
        // the next round opens right away, as the next load would
        feed.close(false).await;
    }
    assert_eq!(sockets_to(port), 0, "the last feed closed with its socket open");
    assert_eq!(server.max_concurrent(), 1, "{:#?}", server.overlaps());
    proxy.shutdown();
}

/// Children forked and exec'd all along, as the relay's (`pre_exec` makes it fork + exec), each
/// 20 ms between the two (a big process, a loaded machine): each holds a copy of every socket
/// open at its fork until its exec. Stops when dropped.
struct Forks {
    stop: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Forks {
    fn start() -> Self {
        use std::os::unix::process::CommandExt;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads = (0..2)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let mut child = std::process::Command::new("true");
                        // SAFETY: the hook only sleeps (nanosleep is async-signal-safe)
                        unsafe {
                            child.pre_exec(|| {
                                std::thread::sleep(Duration::from_millis(20));
                                Ok(())
                            })
                        };
                        let _ = child.status();
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for Forks {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// A load dropped mid-way (the session's `play` is cancelled by a zap or a stop) leaves nothing
/// running: `stop` (as the next load would) closes its provider connection and kills its ffmpeg,
/// and the generation still reports `UpstreamClosed` (§10: a one-connection account must never
/// see the abandoned connection beside the next one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_load_dropped_mid_way_leaves_no_connection_or_ffmpeg() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    // about 24 s to serve bars.ts: an abandoned reader would hold the connection past every wait
    server.set_throttle(Some(8_000));
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let ours = dir.path().to_string_lossy().into_owned();
    let running = || ffmpeg_cmdlines().iter().filter(|c| c.contains(&ours)).count();
    let p = playable(PlayableKind::Live, server.url("/bars.ts"), HttpHeaders::default());
    // hints that suffice: straight to ffmpeg, no probe
    let hints = CastHints { vcodec: Some("h264".into()), acodec: Some("aac".into()), channels: Some(1), interlaced: Some(false), height: Some(720) };
    let load = LoadSpec { hints, ..spec(1, p, Some(CastLevel::Converted)) };
    tokio::select! {
        r = relay.load(load) => panic!("ready before it could be dropped: {r:?}"),
        () = poll(Duration::from_secs(20), || running() > 0) => {}
    }
    assert_eq!(server.active(), 1, "the dropped load's provider connection");
    relay.stop(false).await;
    poll(Duration::from_secs(5), || server.active() == 0 && running() == 0).await;
    until(&mut relay, Duration::from_secs(1), |e| matches!(e, RelayEvent::UpstreamClosed { generation: 1, .. })).await;
}

/// Resolves once `done` holds (checked every 50 ms), or panics at `within`.
async fn poll(within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "not within {within:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The provider's answers become the controller's inputs (§11.2): a 403 is `Http(403)` then the
/// close; prose where the playlist should be is `ProviderText`; the provider vanishing mid-stream
/// closes the generation, and the next load opens again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failures_are_reported_as_statuses_text_and_closes() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();

    server.set_status("/bars.ts", 403);
    let p = playable(PlayableKind::Live, server.url("/bars.ts"), HttpHeaders::default());
    assert_eq!(relay.load(spec(1, p, None)).await.err(), Some(CastErrorKind::Unreachable));
    let seen = until(&mut relay, Duration::from_secs(5), |e| matches!(e, RelayEvent::UpstreamClosed { generation: 1, .. })).await;
    assert_eq!(seen.first(), Some(&RelayEvent::Http { generation: 1, status: 403 }), "{seen:?}");
    assert!(!seen.iter().any(|e| matches!(e, RelayEvent::UpstreamOpened { .. })), "{seen:?}");

    let message = "Cannot read /home/nxt/storage/streams/717661_.m3u8";
    server.set_body("/live/index.m3u8", Some(message));
    let p = playable(PlayableKind::Live, server.url("/live/index.m3u8"), HttpHeaders::default());
    assert_eq!(relay.load(spec(2, p.clone(), None)).await.err(), Some(CastErrorKind::Unreachable));
    let seen = until(&mut relay, Duration::from_secs(5), |e| matches!(e, RelayEvent::UpstreamClosed { generation: 2, .. })).await;
    assert!(seen.contains(&RelayEvent::ProviderText { generation: 2, text: message.into() }), "{seen:?}");

    server.set_body("/live/index.m3u8", None);
    relay.load(spec(3, p.clone(), None)).await.unwrap();
    server.kill();
    let seen = until(&mut relay, Duration::from_secs(20), |e| matches!(e, RelayEvent::UpstreamClosed { generation: 3, .. })).await;
    assert!(!seen.iter().any(|e| matches!(e, RelayEvent::UpstreamEof { .. })), "a dead provider isn't the end of the stream: {seen:?}");
    server.revive();
    relay.load(spec(4, p, None)).await.unwrap();
    until(&mut relay, Duration::from_secs(5), |e| *e == RelayEvent::UpstreamOpened { generation: 4 }).await;
    relay.stop(false).await;
}

/// No provider host, port, path or credential in anything the TV receives, in ffmpeg's command
/// line (`/proc/<pid>/cmdline` is world-readable) or in a log line (§9.3). The only test in this
/// binary that installs the global log subscriber.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_of_the_provider_reaches_the_tv_the_argv_or_the_logs() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let state = tempfile::tempdir().unwrap();
    let logs = tuner::logging::init(&tuner::paths::AppPaths::rooted(state.path()), true);
    let secret = "cast-relay-s3cr3t-71b2";
    register_secret(secret); // as the controller does for every extra header value
    let provider = server.base.trim_start_matches("http://").to_owned(); // 127.0.0.1:<port>
    let url = format!("http://tuner:{secret}@{provider}/live/index.m3u8");
    let headers = HttpHeaders { extra: vec![("X-Api-Key".into(), secret.into())], ..HttpHeaders::default() };

    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let prepared = relay.load(spec(1, playable(PlayableKind::Live, url, headers), None)).await.unwrap();
    let request = server.requests_for("/live/index.m3u8").into_iter().next().expect("the provider was asked");
    assert_eq!(request.header("x-api-key"), Some(secret), "the provider gets its headers");
    assert_eq!(request.header("user-agent"), Some("TunerTest/1.0"));
    assert!(!prepared.load.url.expose().contains(&provider));

    let leaks = |what: &str, text: &str| {
        for bad in [provider.as_str(), secret, "/live/index.m3u8", "/live/seg", "seg0"] {
            assert!(!text.contains(bad), "{bad:?} leaked into {what}: {text}");
        }
    };
    let cmdlines = ffmpeg_cmdlines();
    assert!(!cmdlines.is_empty(), "ffmpeg runs as our child");
    for c in &cmdlines {
        leaks("ffmpeg's argv", c);
        assert!(c.contains("http://127.0.0.1:") && c.contains("/u/"), "the loopback token URL: {c}");
    }
    let tv = tv();
    let base = base_of(&prepared);
    let r = tv.get(prepared.load.url.expose()).send().await.unwrap();
    leaks("the playlist's headers", &format!("{:?}", r.headers()));
    let text = r.text().await.unwrap();
    leaks("the playlist", &text);
    let seg = text.lines().find(|l| !l.starts_with('#')).unwrap();
    let r = tv.get(format!("{base}{seg}")).send().await.unwrap();
    leaks("a segment's headers", &format!("{:?}", r.headers()));
    let body = r.bytes().await.unwrap();
    leaks("a segment", &String::from_utf8_lossy(&body));
    relay.stop(false).await;

    tuner::logging::flush(Duration::from_secs(2));
    let lines: String =
        logs.snapshot(tracing::Level::TRACE).iter().filter(|l| l.target.starts_with("tuner")).map(|l| format!("{}\n", l.message)).collect();
    assert!(lines.contains("cast session"), "the relay logged: {lines}");
    leaks("the log", &lines);
}

/// Broadcast-shaped fixtures probed by the real ffprobe and planned for the Apple TV (§4.2):
/// interlaced H.264 is deinterlaced (Apple rule 1.14), MP2 and AAC-LATM sound becomes AAC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_fixtures_probe_to_the_apple_tv_plan() {
    use tuner::cast::relay::plan::{self, Audio, Options, Video};
    let Ok(caps) = relay::ffmpeg::caps().await else {
        assert!(std::env::var_os("CI").is_none(), "ffmpeg is required in CI");
        return;
    };
    let opts = Options { convert: CastConvert::Auto, interlaced: CastInterlaced::Auto, force: None, can_convert: caps.l3, stereo: false };
    for (name, level, video, audio) in [
        ("idr.ts", CastLevel::Repackaged, Video::Copy, Audio::Copy),
        ("mp2.ts", CastLevel::Repackaged, Video::Copy, Audio::Aac),
        ("latm.ts", CastLevel::Repackaged, Video::Copy, Audio::Aac),
        ("interlaced.ts", CastLevel::Converted, Video::Encode { deinterlace: true }, Audio::Aac),
    ] {
        let path = support::fixture("cast/ts").join(name);
        let probe = relay::probe::probe(&caps.ffprobe, path.to_str().unwrap(), false).await.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let got = plan::plan(ReceiverKind::AppleTv, &probe.facts(&CastHints::default()), opts).unwrap();
        assert_eq!((got.level, got.video, got.audio), (level, video, audio), "{name}: {probe:?}");
    }
}

// ---- full-length films (§8.5) -------------------------------------------------------------

/// A VOD playlist's segments: `(start, EXTINF, name)`, starts summed from its first.
fn vod_segments(text: &str) -> Vec<(f64, f64, String)> {
    let mut out = Vec::new();
    let mut at = 0.0;
    let mut dur = None;
    for line in text.lines() {
        if let Some(d) = line.strip_prefix("#EXTINF:") {
            dur = d.trim_end_matches(',').parse::<f64>().ok();
        } else if !line.starts_with('#') && !line.is_empty() {
            let d = dur.take().expect("an EXTINF before each segment");
            out.push((at, d, line.to_owned()));
            at += d;
        }
    }
    out
}

/// `#EXT-X-START:TIME-OFFSET=<t>` of a playlist.
fn ext_x_start(text: &str) -> Option<f64> {
    text.lines().find_map(|l| l.strip_prefix("#EXT-X-START:TIME-OFFSET=")).and_then(|v| v.split(',').next()?.parse().ok())
}

/// The keyframes' presentation times of a local file's picture, as ffprobe reads them.
fn keyframes(ffprobe: &std::path::Path, file: &std::path::Path) -> Vec<f64> {
    let out = std::process::Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains(",K")).filter_map(|l| l.split(',').next()?.parse().ok()).collect()
}

/// The packets of one stream of a segment (`v` or `a`): `pts,dts,size,flags` lines, as ffprobe
/// reads them on the segment's own timeline (MPEG-TS: what the TV decodes).
fn packets(ffprobe: &std::path::Path, file: &std::path::Path, stream: &str) -> Vec<String> {
    let out = std::process::Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts,dts,size,flags", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect()
}

/// The same packets (`pts,dts,size,flags` lines), timestamps within a millisecond (90 ticks).
fn same_frames(a: &[String], b: &[String]) -> bool {
    let parse = |l: &String| {
        let f: Vec<&str> = l.split(',').collect();
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
        (n(0), n(1), f.get(2).map(|s| s.to_string()), f.get(3).map(|s| s.to_string()))
    };
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            let (x, y) = (parse(x), parse(y));
            let close = |p: Option<i64>, q: Option<i64>| matches!((p, q), (Some(p), Some(q)) if (p - q).abs() <= 90);
            close(x.0, y.0) && close(x.1, y.1) && x.2 == y.2 && x.3 == y.3
        })
}

/// A film reaches the TV whole (§8.5): a VOD playlist listing every segment at once, cut only at
/// the source's keyframes (the MP4 index with its moov at the end, the Matroska cues; the picture
/// starting 0.5 s after the sound, as in a remux of a broadcast, or one B-frame delay after it in
/// an MP4 without an edit list), whole keyframe runs, one target for all, nothing more than one provider connection at a time, and
/// the directory gone with the session. Then the TV starts where `EXT-X-START` says (a resume
/// at 25 s: the first run starts there) and scrubs back to 10 s with its own remote: that
/// segment is made on demand by a run started there, and it is the same media as the same segment of a run from the start: identical picture
/// packets (timestamps, sizes, keyframes); the next segment's sound identical too, the scrub's
/// first segment holding at most one audio frame more (the demuxer's seek lands the sound a
/// frame before the keyframe).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_film_reaches_the_tv_whole_and_any_segment_is_made_on_demand_as_the_same_media() {
    let Some(media) = media() else { return };
    let caps = relay::ffmpeg::caps().await.unwrap();
    // the film, its length, where it resumes and where the TV scrubs back to
    for (file, length, resume, back) in [
        ("/vod.mp4", 30.0, 25.0, 10.0),
        ("/vod.mkv", 30.0, 25.0, 10.0),
        ("/vod_hevc.mp4", 12.0, 9.0, 3.0),
        ("/vod_late.mkv", 30.5, 25.0, 10.0),
        ("/vod_late.mp4", 30.5, 25.0, 10.0),
        ("/vod_noelst.mp4", 30.08, 25.0, 10.0),
    ] {
        let local = media.join(file.trim_start_matches('/'));
        let keys = keyframes(&caps.ffprobe, &local);
        let hevc = file.contains("hevc");
        let work = tempfile::tempdir().unwrap();
        let tv = tv();
        // a segment as the TV gets it: MPEG-TS, or fMP4 after its init section
        let fetch = |base: String, init: Option<Vec<u8>>, name: String, path: std::path::PathBuf| {
            let tv = tv.clone();
            async move {
                let resp = tv.get(format!("{base}{name}")).send().await.unwrap();
                assert_eq!(resp.status(), 200, "{name}");
                let mut bytes = init.unwrap_or_default();
                bytes.extend_from_slice(&resp.bytes().await.unwrap());
                std::fs::write(&path, bytes).unwrap();
                path
            }
        };
        let init_of = |base: String, text: String| {
            let tv = tv.clone();
            async move {
                let name = text.lines().find_map(|l| l.strip_prefix("#EXT-X-MAP:URI=\"").map(|u| u.trim_end_matches('"').to_owned()))?;
                Some(tv.get(format!("{base}{name}")).send().await.unwrap().bytes().await.unwrap().to_vec())
            }
        };

        // from the start, every segment in order: one run
        let server = TestServer::start(media.clone(), Faults::default());
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let p = playable(PlayableKind::Movie, server.url(file), HttpHeaders::default());
        let prepared = relay.load(spec(1, p.clone(), None)).await.unwrap_or_else(|e| panic!("{file}: {e:?}"));
        assert_eq!((prepared.load.offset_s, prepared.surround), (0.0, false), "{file}");
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        let version = if hevc { "#EXT-X-VERSION:7\n" } else { "#EXT-X-VERSION:3\n" };
        assert!(text.starts_with(&format!("#EXTM3U\n{version}#EXT-X-TARGETDURATION:")), "{file}:\n{text}");
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n") && text.ends_with("#EXT-X-ENDLIST\n"), "{file}:\n{text}");
        assert_eq!(text.contains("#EXT-X-MAP:URI=\"init.mp4\"\n"), hevc, "{file}: HEVC goes in fMP4:\n{text}");
        let segs = vod_segments(&text);
        let total: f64 = segs.iter().map(|s| s.1).sum();
        assert!((total - length).abs() < 0.1, "{file}: the film's length, {total}");
        let target: f64 = text.lines().find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:")).unwrap().parse().unwrap();
        assert_eq!(target, segs.iter().map(|s| s.1.round()).fold(0.0, f64::max), "{file}: the longest segment, rounded");
        for (start, dur, name) in &segs[1..] {
            assert!(keys.iter().any(|k| (k - start).abs() < 0.002), "{file}: {name} starts at {start}, not a keyframe of {keys:?}");
            // whole keyframe runs of at most 1.5 s (the tests' 1 s aim), or one longer GOP
            let gop = keys.iter().filter(|k| **k > start + 0.002).fold(f64::MAX, |m, k| m.min(k - start));
            assert!(*dur <= 1.5 + 1e-6 || (*dur - gop).abs() < 0.002 || name == &segs.last().unwrap().2, "{file}: {name} lasts {dur}");
        }
        let base = base_of(&prepared);
        let init = init_of(base.clone(), text.clone()).await;
        let mut linear = Vec::new();
        for (i, (_, _, name)) in segs.iter().enumerate() {
            linear.push(fetch(base.clone(), init.clone(), name.clone(), work.path().join(format!("linear{i}"))).await);
        }
        assert_eq!(server.max_concurrent(), 1, "{file}: {:#?}", server.overlaps());
        relay.stop(false).await;
        assert!(std::fs::read_dir(dir.path().join("vod")).map_or(true, |mut d| d.next().is_none()), "{file}: the session's directory is gone");
        drop(server);

        // resumed, then scrubbed back on the TV
        let server = TestServer::start(media.clone(), Faults::default());
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let mut resumed = (*p).clone();
        resumed.url = Secret::new(server.url(file));
        resumed.start_at = Some(resume);
        let prepared = relay.load(spec(2, Arc::new(resumed), None)).await.unwrap();
        assert_eq!(prepared.load.offset_s, 0.0, "{file}: positions are the film's own");
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        assert_eq!(ext_x_start(&text), Some(resume), "{file}:\n{text}");
        assert_eq!(vod_segments(&text), segs, "{file}: the same map");
        let at = |t: f64| segs.iter().rposition(|s| s.0 <= t).unwrap();
        let (k, n) = (at(resume), at(back));
        let base = base_of(&prepared);
        let again = init_of(base.clone(), text.clone()).await;
        assert_eq!(again, init, "{file}: the same init section");
        let _ = fetch(base.clone(), None, segs[k].2.clone(), work.path().join("start")).await;
        let mut scrubbed = Vec::new();
        for i in [n, n + 1] {
            scrubbed.push(fetch(base.clone(), init.clone(), segs[i].2.clone(), work.path().join(format!("scrub{i}"))).await);
        }
        for (j, i) in [n, n + 1].into_iter().enumerate() {
            let (a, b) = (&linear[i], &scrubbed[j]);
            assert_eq!(packets(&caps.ffprobe, a, "v"), packets(&caps.ffprobe, b, "v"), "{file}: segment {i}'s pictures");
            // the sound: the same frames (Matroska's millisecond clock lets a run's first few
            // timestamps differ by a fraction of one)
            let (sa, sb) = (packets(&caps.ffprobe, a, "a"), packets(&caps.ffprobe, b, "a"));
            let extra = if j == 0 && sb.len() == sa.len() + 1 { 1 } else { 0 };
            assert!(same_frames(&sa, &sb[extra..]), "{file}: segment {i}'s sound:\n{sa:?}\n{sb:?}");
        }
        assert_eq!(server.max_concurrent(), 1, "{file}: one provider connection across the runs: {:#?}", server.overlaps());
        relay.stop(false).await;
    }
}

/// A TV that ignores `EXT-X-START` (§8.5): one that fetches where it is told keeps whole
/// playlists; one whose first segment under a resumed playlist is the film's first is reported
/// once (`StartIgnored`, with where it should have started), and from then on the session's
/// seeks and loads are views that start there (`MEDIA-SEQUENCE:k`, the offset its start: the TV's
/// positions count from it, as Phase 1's views).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tv_that_ignores_where_the_playlist_starts_gets_views_from_there() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let mut p = (*playable(PlayableKind::Movie, server.url("/vod.mkv"), HttpHeaders::default())).clone();
    p.start_at = Some(20.0);
    let p = Arc::new(p);
    let tv = tv();
    let prepared = relay.load(spec(1, p.clone(), None)).await.unwrap();
    let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert_eq!((prepared.load.offset_s, ext_x_start(&text)), (0.0, Some(20.0)), "{text}");
    let segs = vod_segments(&text);
    let at = |t: f64| segs.iter().rposition(|s| s.0 <= t).unwrap();
    // as told: the segment holding 20 s first
    assert_eq!(tv.get(format!("{}{}", base_of(&prepared), segs[at(20.0)].2)).send().await.unwrap().status(), 200);
    let view = relay.view(2, 5.0).await.unwrap().unwrap();
    let vtext = tv.get(view.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert_eq!((view.load.offset_s, ext_x_start(&vtext)), (0.0, Some(5.0)), "still whole: {vtext}");
    // from the film's start instead
    assert!(at(5.0) >= 2, "{segs:?}");
    assert_eq!(tv.get(format!("{}{}", base_of(&view), segs[0].2)).send().await.unwrap().status(), 200);
    until(&mut relay, Duration::from_secs(5), |e| *e == RelayEvent::StartIgnored { generation: 2, at_s: 5.0 }).await;
    let view = relay.view(3, 5.0).await.unwrap().unwrap();
    let vtext = tv.get(view.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!((view.load.offset_s - segs[at(5.0)].0).abs() < 1e-3 && ext_x_start(&vtext).is_none(), "{vtext}");
    assert!(vtext.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", at(5.0))) && vtext.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"), "{vtext}");
    assert_eq!(vod_segments(&vtext).first().map(|s| s.2.clone()), Some(segs[at(5.0)].2.clone()));
    // a load of the next item starts with a view too
    let prepared = relay.load(spec(4, p, None)).await.unwrap();
    let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!((prepared.load.offset_s - segs[at(20.0)].0).abs() < 1e-3 && ext_x_start(&text).is_none(), "{text}");
    relay.stop(false).await;
}

/// §8.5's fallback matrix on real files: a film without a keyframe index (MPEG-TS, Matroska
/// without cues), converted or not, and one from a provider that ignores `Range` keep their
/// EVENT playlists; converted (L3), an indexed film goes full length on the grid of its forced
/// keyframes (1 s here), each segment starting with a keyframe at its grid time, and a segment
/// made by a run started at a scrub has the same pictures, at the same times, as a run from the
/// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_index_a_film_keeps_a_growing_playlist_unless_it_is_converted() {
    let Some(media) = media() else { return };
    let caps = relay::ffmpeg::caps().await.unwrap();
    let tv = tv();
    let converted = Some(CastLevel::Converted);
    for (file, ranges, force) in [
        ("/vod.ts", true, None),
        ("/vod.ts", true, converted),
        ("/vod_nocues.mkv", true, None),
        ("/vod.mkv", false, None),
        ("/vod.mkv", false, converted),
    ] {
        let server = TestServer::start(media.clone(), Faults::default());
        server.set_ranges(ranges);
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let prepared = relay.load(spec(1, playable(PlayableKind::Movie, server.url(file), HttpHeaders::default()), force)).await.unwrap();
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n"), "{file} (Range: {ranges}, {force:?}):\n{text}");
        relay.stop(false).await;
    }

    let server = TestServer::start(media, Faults::default());
    let work = tempfile::tempdir().unwrap();
    let mut made: Vec<Vec<std::path::PathBuf>> = Vec::new();
    for (start_at, fetch) in [(None, vec![9, 10, 11]), (Some(25.0), vec![10, 11])] {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
        let mut p = (*playable(PlayableKind::Movie, server.url("/vod.mkv"), HttpHeaders::default())).clone();
        p.start_at = start_at;
        let prepared = relay.load(spec(1, Arc::new(p), converted)).await.unwrap();
        assert_eq!((prepared.level, prepared.load.offset_s), (CastLevel::Converted, 0.0));
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n") && text.contains("#EXT-X-TARGETDURATION:1\n"), "{text}");
        let segs = vod_segments(&text);
        assert_eq!(segs.len(), 30, "{text}");
        assert!(segs.iter().enumerate().all(|(k, s)| (s.0 - k as f64).abs() < 1e-6), "a cut every second: {segs:?}");
        let base = base_of(&prepared);
        if start_at.is_some() {
            // where the TV starts, then the scrub back
            assert_eq!(tv.get(format!("{base}s25.ts")).send().await.unwrap().status(), 200);
        }
        let mut files = Vec::new();
        for k in fetch {
            let resp = tv.get(format!("{base}s{k}.ts")).send().await.unwrap();
            assert_eq!(resp.status(), 200, "s{k}.ts");
            let path = work.path().join(format!("{}-{k}.ts", made.len()));
            std::fs::write(&path, resp.bytes().await.unwrap()).unwrap();
            files.push(path);
        }
        made.push(files);
        relay.stop(false).await;
    }
    // the pictures of segments 10 and 11: from the start, and from a run started at 10
    for (a, b) in made[0][1..].iter().zip(&made[1]) {
        let pts = |f: &std::path::Path| {
            packets(&caps.ffprobe, f, "v").iter().map(|l| l.split(',').next().unwrap_or_default().to_owned()).collect::<Vec<_>>()
        };
        assert_eq!(pts(a), pts(b), "{a:?} / {b:?}");
        let first = packets(&caps.ffprobe, b, "v")
            .into_iter()
            .min_by_key(|l| l.split(',').next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(i64::MAX))
            .unwrap();
        assert!(first.ends_with("K__,") || first.contains(",K"), "a segment starts with a keyframe: {first}");
    }
}

/// `vod.mkv` with every cue but the first 300 ms later than its keyframe: an index ffmpeg doesn't
/// cut where it says (`CueTime`s rewritten in place, same widths).
fn shifted_cues(mut file: Vec<u8>) -> Vec<u8> {
    // an EBML element at `at`: (id, header length, data size)
    fn element(b: &[u8], at: usize) -> Option<(u32, usize, usize)> {
        let first = *b.get(at)?;
        let id_len = first.leading_zeros() as usize + 1;
        let id = b.get(at..at + id_len)?.iter().fold(0u32, |a, x| (a << 8) | u32::from(*x));
        let s = *b.get(at + id_len)?;
        let size_len = s.leading_zeros() as usize + 1;
        let raw = b.get(at + id_len..at + id_len + size_len)?;
        let size = raw[1..].iter().fold(u64::from(s) & (0xff >> size_len), |a, x| (a << 8) | u64::from(*x));
        Some((id, id_len + size_len, usize::try_from(size).ok()?))
    }
    // the children of the element whose data is `from..to`: (id, data start, data end)
    fn children(b: &[u8], from: usize, to: usize) -> Vec<(u32, usize, usize)> {
        let mut out = Vec::new();
        let mut at = from;
        while at < to
            && let Some((id, hdr, size)) = element(b, at)
        {
            out.push((id, at + hdr, at + hdr + size));
            at += hdr + size;
        }
        out
    }
    let top = children(&file, 0, file.len());
    let segment = top.iter().find(|(id, ..)| *id == 0x1853_8067).copied().expect("a Segment");
    let cues = children(&file, segment.1, segment.2).into_iter().find(|(id, ..)| *id == 0x1C53_BB6B).expect("Cues");
    let mut shifted = 0;
    for (_, p0, p1) in children(&file, cues.1, cues.2).into_iter().filter(|(id, ..)| *id == 0xBB) {
        for (_, t0, t1) in children(&file, p0, p1).into_iter().filter(|(id, ..)| *id == 0xB3) {
            let v = file[t0..t1].iter().fold(0u64, |a, x| (a << 8) | u64::from(*x));
            let bigger = v + 300;
            if v > 0 && bigger < 1 << (8 * (t1 - t0)) {
                file[t0..t1].copy_from_slice(&bigger.to_be_bytes()[8 - (t1 - t0)..]);
                shifted += 1;
            }
        }
    }
    assert!(shifted >= 10, "{shifted} cues shifted");
    file
}

/// Events the relay has queued by now.
fn queued(relay: &mut RelaySession) -> Vec<RelayEvent> {
    std::iter::from_fn(|| relay.next_event().now_or_never()).collect()
}

/// A keyframe index ffmpeg doesn't cut where it says (§8.5's fallback matrix, "segments not where
/// the index said"), found on the load's first run: the load itself falls back to a growing
/// playlist at once (not a failed cast, nor an `Unindexed` that would restart what the TV is
/// about to get), with one provider connection throughout; a later load of the item in the
/// session goes there directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_that_doesnt_hold_on_the_first_run_falls_back_at_once() {
    let Some(media) = media() else { return };
    let served = tempfile::tempdir().unwrap();
    std::fs::write(served.path().join("film.mkv"), shifted_cues(std::fs::read(media.join("vod.mkv")).unwrap())).unwrap();
    let server = TestServer::start(served.path().to_owned(), Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let tv = tv();
    let p = playable(PlayableKind::Movie, server.url("/film.mkv"), HttpHeaders::default());
    for generation in [1, 2] {
        let prepared = relay.load(spec(generation, p.clone(), None)).await.unwrap_or_else(|e| panic!("load {generation}: {e:?}"));
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n"), "load {generation}:\n{text}");
        let first = text.lines().find(|l| !l.is_empty() && !l.starts_with('#')).expect("a segment");
        assert_eq!(tv.get(format!("{}{first}", base_of(&prepared))).send().await.unwrap().status(), 200);
        let events = queued(&mut relay);
        assert!(!events.iter().any(|e| matches!(e, RelayEvent::Unindexed { .. } | RelayEvent::Failed { .. })), "load {generation}: {events:?}");
    }
    assert_eq!(server.max_concurrent(), 1, "{:#?}", server.overlaps());
    relay.stop(false).await;
}

/// A surround film whose sound ends 4 s before its picture (silent credits on a trimmed track):
/// the segments without sound are still whole in every rendition (the sound's are fragments
/// without samples), so the TV gets every segment of every rendition, and the item is made by one
/// run, not by a run restarted for each request of a segment that can't be split.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surround_film_whose_sound_ends_early_is_served_to_the_end() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let file = "/vod_quiet_end.mkv";
    let prepared = relay.load(spec(1, playable(PlayableKind::Movie, server.url(file), HttpHeaders::default()), None)).await.unwrap();
    assert!(prepared.surround, "{prepared:?}");
    let (tv, base) = (tv(), base_of(&prepared));
    for r in ["v/", "a/", "s/"] {
        let text = tv.get(format!("{base}{r}index.m3u8")).send().await.unwrap().text().await.unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"), "{r}:\n{text}");
        let segs = vod_segments(&text);
        assert!(segs.len() >= 8, "{r}: {segs:?}");
        let init = text.lines().find_map(|l| l.strip_prefix("#EXT-X-MAP:URI=\"").map(|u| u.trim_end_matches('"').to_owned())).unwrap();
        for name in std::iter::once(&init).chain(segs.iter().map(|s| &s.2)) {
            let resp = tv.get(format!("{base}{r}{name}")).send().await.unwrap();
            assert_eq!(resp.status(), 200, "{r}{name}");
            assert!(!resp.bytes().await.unwrap().is_empty(), "{r}{name}");
        }
    }
    // the provider's requests: the open, the probe's and the index's reads, and one run
    let requests = server.requests_for(file);
    assert!(requests.len() <= 6, "{} requests: {requests:#?}", requests.len());
    relay.stop(false).await;
}

/// The keyframe index is an extra read (the Matroska cues at the end of a film bigger than the
/// proxy's head buffer: a provider request of its own) that the cast can do without: a provider that refuses it (one-connection panels answer a connection made right
/// after the previous one closed with 503 or 509) costs the full-length playlist, not the cast
/// (the feed doesn't fail: no `Http`), and the film plays as a growing playlist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_index_read_falls_back_without_failing_the_feed() {
    let Some(media) = media() else { return };
    let size = std::fs::metadata(media.join("vod_big.mkv")).unwrap().len();
    let server = TestServer::start(media, Faults::default());
    server.fail_ranges_from("/vod_big.mkv", size / 2, 503, 1);
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let prepared = relay.load(spec(1, playable(PlayableKind::Movie, server.url("/vod_big.mkv"), HttpHeaders::default()), None)).await.unwrap();
    assert!(server.requests_for("/vod_big.mkv").iter().any(|r| r.status == 503), "the index read was refused");
    let tv = tv();
    let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n"), "{text}");
    let first = text.lines().find(|l| !l.is_empty() && !l.starts_with('#')).expect("a segment");
    assert_eq!(tv.get(format!("{}{first}", base_of(&prepared))).send().await.unwrap().status(), 200);
    let events = queued(&mut relay);
    // (the 20 s film is all read in its first burst: its end closes the feed, as any film's)
    assert!(!events.iter().any(|e| matches!(e, RelayEvent::Http { .. } | RelayEvent::ProviderText { .. } | RelayEvent::Failed { .. })), "{events:?}");
    relay.stop(false).await;
}

/// A DLNA renderer's stream (DESIGN §40 Appendix E): the URL is `stream.ts`; a HEAD answers the
/// DLNA headers and starts nothing; a GET is the film's TS segments back to back from its start
/// (no length: chunked), each a `SegmentFetched`, then `StreamEnded`; a view (a seek) is a new
/// token whose stream starts at its segment, and the old token's stream is gone. The film is
/// never full length here: a stream has no playlist to be whole in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dlna_renderer_gets_one_continuous_stream() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(RelayConfig { stream: true, ..config(&dir, LOOPBACK) }).await.unwrap();
    let p = playable(PlayableKind::Movie, server.url("/vod.mp4"), HttpHeaders::default());
    let prepared = relay.load(LoadSpec { receiver: ReceiverKind::Dlna, ..spec(1, p, None) }).await.unwrap();
    let url = prepared.load.url.expose().clone();
    assert!(url.ends_with("/stream.ts") && !prepared.load.fmp4, "{url}");
    let tv = tv();

    let head = tv.head(&url).send().await.unwrap();
    assert_eq!(head.status(), 200);
    let h = head.headers();
    assert_eq!(h["content-type"], "video/mpeg");
    assert_eq!(h["transfermode.dlna.org"], "Streaming");
    assert!(h["contentfeatures.dlna.org"].to_str().unwrap().starts_with("DLNA.ORG_OP=00;"));
    assert!(h.get("content-length").is_none() || h["content-length"] == "0");

    let r = tv.get(&url).header("Range", "bytes=0-").send().await.unwrap();
    assert_eq!(r.status(), 200, "a Range is ignored: the stream can't seek");
    assert!(r.headers().get("content-length").is_none(), "no length: it is made as it goes");
    let body = tokio::time::timeout(Duration::from_secs(60), r.bytes()).await.expect("the whole film in time").unwrap();
    assert_eq!(body[0], 0x47);
    assert_eq!(body.len() % 188, 0, "whole TS packets");
    let packets = body.len() / 188;
    assert!((0..packets).all(|i| body[i * 188] == 0x47), "in sync throughout");
    let seen = until(&mut relay, Duration::from_secs(10), |e| matches!(e, RelayEvent::StreamEnded { generation: 1 })).await;
    let spans: Vec<(f64, f64)> =
        seen.iter().filter_map(|e| if let RelayEvent::SegmentFetched { from_s, upto_s, .. } = e { Some((*from_s, *upto_s)) } else { None }).collect();
    assert!(spans.len() >= 3 && spans[0].0 == 0.0, "{spans:?}");
    assert!(spans.windows(2).all(|w| (w[1].0 - w[0].1).abs() < 0.01), "back to back: {spans:?}");
    assert!(spans.last().unwrap().1 > 25.0, "the whole 30 s film: {spans:?}");

    // a seek: a view under a new token, from the segment holding 12 s
    let view = relay.view(2, 12.0).await.unwrap().expect("inside what's produced");
    assert!(view.load.url.expose().ends_with("/stream.ts"));
    assert_eq!(tv.get(&url).send().await.unwrap().status(), 404, "the old token is revoked");
    let from = view.load.offset_s;
    assert!(from > 0.0 && from <= 12.0, "{from}");
    let part = tv.get(view.load.url.expose()).send().await.unwrap().bytes().await.unwrap();
    assert!(!part.is_empty() && part.len() < body.len() && part[0] == 0x47, "{} of {}", part.len(), body.len());
    relay.stop(false).await;
}
