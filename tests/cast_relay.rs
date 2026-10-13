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
    }
}

fn playable(kind: PlayableKind, url: String, headers: HttpHeaders) -> Arc<Playable> {
    let media = match kind {
        PlayableKind::Movie => MediaRef::Movie(MovieId(1)),
        _ => MediaRef::Url(String::new()),
    };
    Arc::new(Playable {
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
    LoadSpec { generation, playable, hints: CastHints::default(), receiver: ReceiverKind::AppleTv, force, stereo: false }
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

/// The command lines of this process's ffmpeg children.
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
            if cmd.contains("ffmpeg") {
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

/// A film (`testsrc.mp4`, moov at the end: ffprobe and ffmpeg seek it with `Range` through the
/// proxy) as an EVENT playlist, and the server's rules: byte-identical 404s for a wrong token,
/// other paths and methods; a wrong peer closed unanswered; HEAD and `Range`; a view's token serves the shared
/// segments while the old token 404s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_answers_its_token_and_peer_only_and_views_share_the_segments() {
    let Some(media) = media() else { return };
    let server = TestServer::start(media, Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let mut relay = RelaySession::start(config(&dir, LOOPBACK)).await.unwrap();
    let p = playable(PlayableKind::Movie, server.url("/testsrc.mp4"), HttpHeaders::default());
    let prepared = relay.load(spec(7, p.clone(), None)).await.unwrap();
    assert!(!prepared.load.live && prepared.behind_live_s.is_none());
    let base = base_of(&prepared);
    let port = port_of(&base);
    let tok = base.trim_end_matches('/').rsplit('/').next().unwrap().to_owned();
    let tv = tv();

    // wait for the whole film (EVENT + ENDLIST: ffmpeg reads 4× real time)
    let deadline = Instant::now() + Duration::from_secs(30);
    let text = loop {
        let text = tv.get(prepared.load.url.expose()).send().await.unwrap().text().await.unwrap();
        if text.contains("#EXT-X-ENDLIST") {
            break text;
        }
        assert!(Instant::now() < deadline, "no ENDLIST:\n{text}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT\n") && text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"), "{text}");

    let miss = raw(port, &format!("GET /c/{}/index.m3u8 HTTP/1.1", "a".repeat(26)));
    assert!(miss.starts_with("HTTP/1.1 404"), "{miss}");
    for req in [
        format!("GET /c/{tok}/s1.TS HTTP/1.1"),
        format!("GET /c/{tok}/../index.m3u8 HTTP/1.1"),
        format!("GET /c/{tok}/%2e%2e/index.m3u8 HTTP/1.1"),
        format!("GET /c/{tok}/s999.ts HTTP/1.1"),
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

    // a seek inside what's produced: a view from the segment holding 4.5 s (2 s GOPs)
    let view = relay.view(8, 4.5).await.unwrap().expect("inside what's produced");
    assert!((view.load.offset_s - 4.0).abs() < 0.05, "{}", view.load.offset_s);
    let vtext = tv.get(view.load.url.expose()).send().await.unwrap().text().await.unwrap();
    assert!(vtext.contains("#EXT-X-MEDIA-SEQUENCE:2\n") && vtext.contains("\ns2.ts\n") && !vtext.contains("\ns1.ts\n"), "{vtext}");
    assert!(vtext.contains("#EXT-X-PROGRAM-DATE-TIME:") && vtext.ends_with("#EXT-X-ENDLIST\n"), "{vtext}");
    let shared = tv.get(format!("{}s2.ts", base_of(&view))).send().await.unwrap();
    assert_eq!(shared.status(), 200);
    assert_eq!(raw(port, &format!("GET /c/{tok}/index.m3u8 HTTP/1.1")), miss, "the old token is revoked");
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
        for (r, init, mime) in [("v", "init_0.mp4", "video/mp4"), ("a", "init_1.mp4", "audio/mp4"), ("s", "init_2.mp4", "audio/mp4")] {
            let text = tv.get(format!("{base}{r}/index.m3u8")).send().await.unwrap().text().await.unwrap();
            assert!(text.contains(&format!("#EXT-X-MAP:URI=\"{init}\"")) && text.contains("#EXT-X-START:TIME-OFFSET=0\n"), "{file} {r}:\n{text}");
            targets.push(text.lines().find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:")).unwrap().to_owned());
            for name in [init, "s0.m4s"] {
                let resp = tv.get(format!("{base}{r}/{name}")).send().await.unwrap();
                assert_eq!((resp.status().as_u16(), resp.headers()["content-type"].to_str().unwrap()), (200, mime), "{file} {r}/{name}");
            }
        }
        targets.dedup();
        assert_eq!(targets.len(), 1, "one target for every rendition (Apple rule 8.2): {targets:?}");
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
