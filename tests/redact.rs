//! Redaction & logging (WP-08, DESIGN §20): URLs, headers, registered secrets, mpv log lines,
//! error Display, and the redacting log pipeline (ring buffer + rotated file).
//!
//! The secret registry is process-global and tests run in parallel: every test registers its
//! own unique secrets and never calls `clear_secrets`.

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use tuner::error::Error;
use tuner::logging::{LogBuffer, LogLine};
use tuner::redact::{Secret, redact, redact_url, register_secret};

#[test]
fn xtream_urls_in_every_form() {
    let cases = [
        ("http://h.tv:8080/player_api.php?username=alice&password=wonder1", "http://h.tv:8080/player_api.php?username=***&password=***"),
        ("http://h.tv:8080/xmltv.php?username=alice&password=wonder1", "http://h.tv:8080/xmltv.php?username=***&password=***"),
        ("http://h.tv:8080/live/alice/wonder1/1234.m3u8", "http://h.tv:8080/live/***/***/1234.m3u8"),
        ("http://h.tv:8080/movie/alice/wonder1/77.mkv", "http://h.tv:8080/movie/***/***/77.mkv"),
        ("http://h.tv:8080/series/alice/wonder1/88.mp4", "http://h.tv:8080/series/***/***/88.mp4"),
        ("http://h.tv:8080/alice/wonder1/1234", "http://h.tv:8080/***/***/1234"),
        ("http://h.tv:8080/timeshift/alice/wonder1/120/2026-10-09:20-00/1234.ts", "http://h.tv:8080/timeshift/***/***/120/2026-10-09:20-00/1234.ts"),
        ("https://alice:wonder1@iptv.example/list.m3u", "https://***@iptv.example/list.m3u"),
        ("http://x.tv/s.m3u8?token=abc123&expires=99", "http://x.tv/s.m3u8?token=***&expires=99"),
        ("http://x.tv/s.m3u8?SIG=abc123", "http://x.tv/s.m3u8?SIG=***"),
    ];
    for (input, want) in cases {
        assert_eq!(redact_url(input), want, "{input}");
    }
    // nothing to hide → unchanged
    for clean in ["https://i.imgur.com/logo.png", "http://h.tv/epg/guide.xml.gz", "BBC One HD", "https://news.tv/2024/10/12/story"] {
        assert_eq!(redact(clean), clean);
    }
}

#[test]
fn registered_secrets_vanish_everywhere() {
    let user = "rs-user-5d1e";
    let pass = "rs-p@ss/w0rd 5d1e";
    register_secret(user);
    register_secret(pass);
    let enc = "rs-p%40ss%2Fw0rd%205d1e";
    let lines = [
        format!("login failed for {user}"),
        format!("http://h.tv/c/{user}/{enc}/stream?x=1"),
        format!("http://h.tv/get.php?u={user}&p={enc}"),
        format!("http://h.tv/get.php?u={user}&p=rs-p%40ss%2Fw0rd+5d1e"),
        format!("{{\"u\":\"{user}\",\"p\":\"{pass}\"}}"),
        format!("password is {pass}."),
    ];
    for l in &lines {
        let r = redact(l);
        assert!(!r.contains(user) && !r.contains(pass) && !r.contains(enc), "{l} → {r}");
        assert!(r.contains("***"), "{r}");
    }
}

/// A playlist kept as M3U whose short-form stream URLs sit behind a path prefix: no URL pattern
/// can tell `host/<prefix>/<user>/<pass>/<id>` from a CDN path, so the login its last import
/// found is stored and registered again at the next start (this test binary has never seen it).
#[test]
fn a_kept_playlists_login_is_registered_again_at_startup() {
    use tuner::model::{SourceColor, SourceDraft, SourceKind, SourceSettings};
    let store = tuner::store::Store::open_temp("tuner-redact-logins", None).unwrap();
    let draft = SourceDraft {
        name: "Kept".into(),
        kind: SourceKind::M3uFile,
        icon: String::new(),
        color: SourceColor::Accent,
        url: None,
        path: Some("/tmp/kept.m3u".into()),
        xtream: None,
        settings: SourceSettings { keep_m3u: true, ..Default::default() },
    };
    let login = [("kl-user-3e8b".to_owned(), "kl-pass-3e8b".to_owned())];
    store
        .writer()
        .blocking(move |c| {
            let id = tuner::store::sources::insert_source(c, &draft, 0)?;
            tuner::store::sources::set_playlist_logins(c, id, &login)
        })
        .unwrap();
    let line = "mpv: Opening http://cdn.example/pre/kl-user-3e8b/kl-pass-3e8b/1001.ts";
    assert_eq!(redact(line), line, "nothing registered yet: the prefixed short form is not a URL pattern");
    // what `Services::start` does on every launch
    tuner::store::sources::register_secrets(&store.reader().unwrap()).unwrap();
    assert_eq!(redact(line), "mpv: Opening http://cdn.example/pre/***/***/1001.ts");
}

#[test]
fn credential_headers_lose_their_values() {
    let cases = [
        ("Authorization: Basic YWxpY2U6d29uZGVy", "Authorization: ***"),
        ("authorization:Bearer eyJ.abc.def", "authorization:***"),
        ("Proxy-Authorization: Basic Zm9vOmJhcg==", "Proxy-Authorization: ***"),
        ("Cookie: session=abc; theme=dark", "Cookie: ***"),
        ("X-Api-Key: 12345678", "X-Api-Key: ***"),
        // mpv's comma-separated header list keeps the harmless neighbours
        (
            "Set property: http-header-fields=Referer: https://site.tv/,Cookie: sid=42,User-Agent: Tuner",
            "Set property: http-header-fields=Referer: https://site.tv/,Cookie: ***,User-Agent: Tuner",
        ),
        ("User-Agent: VLC/3.0.20 LibVLC/3.0.20", "User-Agent: VLC/3.0.20 LibVLC/3.0.20"),
    ];
    for (input, want) in cases {
        assert_eq!(redact(input), want, "{input}");
    }
}

/// Per-stream header values are not registered secrets, so the quoted forms a `{:?}` or a
/// serialised `HttpHeaders` produces must be caught by pattern.
#[test]
fn quoted_header_forms_lose_their_values() {
    let h = tuner::model::HttpHeaders {
        user_agent: Some("Tuner/1.0".into()),
        extra: vec![
            ("Cookie".into(), "sid=QUOTEDCOOKIE-5c1e".into()),
            ("Authorization".into(), "Bearer QUOTEDTOKEN-5c1e".into()),
            ("X-Forwarded-For".into(), "10.0.0.1".into()),
        ],
        ..Default::default()
    };
    let debug = format!("{h:?}");
    let json = serde_json::to_string(&h).expect("serialise headers");
    for s in [&debug, &json] {
        let r = redact(s);
        assert!(!r.contains("QUOTEDCOOKIE") && !r.contains("QUOTEDTOKEN"), "{s} -> {r}");
        assert!(r.contains("Tuner/1.0") && r.contains("10.0.0.1"), "neighbours stay readable: {r}");
    }
    assert_eq!(redact(&debug), debug.replace("sid=QUOTEDCOOKIE-5c1e", "***").replace("Bearer QUOTEDTOKEN-5c1e", "***"));
    assert_eq!(redact(r#"{"Authorization": "Bearer QUOTEDTOKEN-5c1e", "Accept": "*/*"}"#), r#"{"Authorization": "***", "Accept": "*/*"}"#);
}

#[test]
fn mpv_log_lines() {
    let secret = "mpv-secret-8a77";
    register_secret(secret);
    let cases = [
        (
            "[ffmpeg/demuxer] hls: Opening 'http://h.tv/live/u1/p1/9.m3u8' for reading".to_owned(),
            "[ffmpeg/demuxer] hls: Opening 'http://h.tv/live/***/***/9.m3u8' for reading".to_owned(),
        ),
        (
            "[ffmpeg] http: HTTP error 403 Forbidden (http://h.tv/u1/p1/9)".to_owned(),
            "[ffmpeg] http: HTTP error 403 Forbidden (http://h.tv/***/***/9)".to_owned(),
        ),
        (format!("[stream] Opening http://h.tv/s.ts?auth={secret}"), "[stream] Opening http://h.tv/s.ts?auth=***".to_owned()),
        (format!("[cplayer] Failed to open http://cdn.tv/{secret}/a.ts."), "[cplayer] Failed to open http://cdn.tv/***/a.ts.".to_owned()),
    ];
    for (input, want) in cases {
        assert_eq!(redact(&input), want, "{input}");
    }
}

#[test]
fn provider_json_in_errors() {
    let body = r#"{"user_info":{"username":"jd-7c1","password":"pw\"7c1","auth":1,"status":"Expired"},"server_info":{"url":"h.tv"}}"#;
    let e = Error::BadPayload(redact(body).into_owned());
    let shown = e.to_string();
    assert!(!shown.contains("jd-7c1") && !shown.contains("7c1\""), "{shown}");
    assert!(shown.contains(r#""status":"Expired""#), "{shown}");
}

#[test]
fn error_constructors_redact() {
    let secret = "err-secret-31f0";
    register_secret(secret);
    let e = Error::net(format!("error sending request for url (http://h.tv/live/u/{secret}/1.ts): connection refused"));
    assert_eq!(e.to_string(), "network error: error sending request for url (http://h.tv/live/***/***/1.ts): connection refused");
    let e = Error::other(format!("token {secret} rejected"));
    assert_eq!(e.to_string(), "token *** rejected");
}

#[test]
fn secret_type_never_prints_its_value() {
    let s = Secret::new("http://h.tv/get.php?username=st-u&password=st-p".to_owned());
    assert_eq!(format!("{s:?}"), "Secret(***)");
    assert_eq!(format!("{:?}", Some(&s)), "Some(Secret(***))");
    assert_eq!(s.redacted(), "http://h.tv/get.php?username=***&password=***");
    assert_eq!(s.expose(), "http://h.tv/get.php?username=st-u&password=st-p");
    // serializes transparently (DB / backup need the value)
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(json, "\"http://h.tv/get.php?username=st-u&password=st-p\"");
    let back: Secret<String> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
}

/// Deterministic fuzz: many generated secrets (URL-special characters included) embedded in
/// typical contexts never survive redaction, raw or percent-encoded.
#[test]
fn generated_secrets_never_leak() {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.~!$&'()*+,;=:@/? %#";
    let mut seed: u64 = 0x5eed_1234_abcd_ef01;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in 0..300 {
        let len = 6 + (next() % 14) as usize;
        let body: String = (0..len).map(|_| CHARS[(next() % CHARS.len() as u64) as usize] as char).collect();
        let secret = format!("g{i}x{body}");
        register_secret(&secret);
        let enc: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
        let contexts = [
            format!("user {secret} failed"),
            format!("http://h.tv/get.php?username=u&password={enc}&type=m3u"),
            format!("Authorization: Basic {secret}"),
            format!("{{\"password\":\"{}\"}}", secret.replace('\\', "\\\\").replace('"', "\\\"")),
        ];
        for c in &contexts {
            let r = redact(c);
            assert!(!r.contains(secret.as_str()), "secret {secret:?} leaked from {c:?}: {r:?}");
            assert!(!r.contains(enc.as_str()), "encoded {enc:?} leaked from {c:?}: {r:?}");
        }
    }
}

#[test]
fn log_buffer_ring_snapshot_and_export() {
    let buf = LogBuffer::new(3);
    for (i, level) in [tracing::Level::DEBUG, tracing::Level::INFO, tracing::Level::WARN, tracing::Level::ERROR].into_iter().enumerate() {
        buf.push(LogLine { at: 1_760_000_000 + i as i64, level, target: "tuner::test".into(), message: format!("line {i}") });
    }
    let all = buf.snapshot(tracing::Level::TRACE);
    assert_eq!(all.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["line 1", "line 2", "line 3"]);
    let warn = buf.snapshot(tracing::Level::WARN);
    assert_eq!(warn.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["line 2", "line 3"]);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("tuner-log.txt");
    buf.export(&out).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.lines().last().unwrap().ends_with("ERROR tuner::test: line 3"), "{text}");
    assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
}

/// The only test in this binary that installs the global subscriber.
#[test]
fn logging_init_redacts_lines_into_the_ring_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = tuner::paths::AppPaths::rooted(dir.path());
    let buf = tuner::logging::init(&paths, true);
    let secret = "init-secret-0b9c";
    register_secret(secret);
    // (lines of other crates — this test binary included — are recorded as DEBUG at most)
    tracing::warn!(target: "tuner::jobs", "refresh of http://h.tv/get.php?username=bob&password={secret} failed");
    tracing::debug!(target: "tuner::player", "mpv: Opening http://h.tv/live/bob/{secret}/1.ts");

    let lines = buf.snapshot(tracing::Level::TRACE);
    let joined: String = lines.iter().map(|l| format!("{}\n", l.message)).collect();
    assert!(joined.contains("username=***&password=*** failed"), "{joined}");
    assert!(joined.contains("Opening http://h.tv/live/***/***/1.ts"), "{joined}");
    assert!(!joined.contains(secret), "{joined}");

    let file = paths.log_file();
    let deadline = Instant::now() + Duration::from_secs(5);
    let text = loop {
        let t = std::fs::read_to_string(&file).unwrap_or_default();
        if t.contains("1.ts") || Instant::now() > deadline {
            break t;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(text.contains(" WARN tuner::jobs: refresh of http://h.tv/get.php?username=***&password=*** failed"), "{text}");
    assert!(text.contains("DEBUG tuner::player: mpv: Opening http://h.tv/live/***/***/1.ts"), "{text}");
    assert!(!text.contains(secret), "{text}");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&paths.state_dir).unwrap().permissions().mode() & 0o777, 0o700);
}
