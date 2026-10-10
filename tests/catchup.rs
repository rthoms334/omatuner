//! Catch-up URL building: table tests for every kind (DESIGN §10). Owner: WP-05a.

use tuner::error::Error;
use tuner::model::{CatchupKind, CatchupSpec, LiveFormat, Timestamp, XtreamCredentials};
use tuner::redact::Secret;
use tuner::sources::catchup::{CatchupRequest, build_url, expand, in_window};
use tuner::sources::xtream::urls::{self, LiveExt};

/// 2026-10-09 20:00:00 UTC (21:00 BST in London).
const S: Timestamp = 1_791_576_000;

fn ts(s: &str) -> Timestamp {
    s.parse::<jiff::Timestamp>().expect("timestamp").as_second()
}

/// Europe/London without depending on the system tz database.
fn london() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").expect("posix tz")
}

fn spec(kind: CatchupKind, template: Option<&str>) -> CatchupSpec {
    CatchupSpec { kind, template: template.map(str::to_owned), days: 3, correction_s: 0 }
}

/// Programme 20:00–21:00 UTC, now 21:30 UTC (offset 5400 s).
fn req<'a>(live_url: &'a str, spec: &'a CatchupSpec) -> CatchupRequest<'a> {
    CatchupRequest {
        live_url,
        spec,
        start: S,
        stop: S + 3600,
        now: S + 5400,
        tvg_shift_min: 0,
        user_offset_s: 0,
        catchup_id: None,
        tz: jiff::tz::TimeZone::UTC,
    }
}

fn url(r: &CatchupRequest<'_>) -> String {
    build_url(r).expect("catch-up url")
}

#[test]
fn design_example_timestamp() {
    assert_eq!(ts("2026-10-09T20:00:00Z"), S);
}

#[test]
fn default_kind() {
    let s = spec(CatchupKind::Default, Some("http://h/archive?start={utc}&end={utcend}"));
    assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/archive?start=1791576000&end=1791579600");
    // No template → behaves like Append without a template (the Shift query).
    let s = spec(CatchupKind::Default, None);
    assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/ch/index.m3u8?utc=1791576000&lutc=1791581400");
    // Blank template counts as absent; a relative template is appended.
    let s = spec(CatchupKind::Default, Some("  "));
    assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/ch/index.m3u8?utc=1791576000&lutc=1791581400");
    let s = spec(CatchupKind::Default, Some("?from={utc}"));
    assert_eq!(url(&req("http://h/ch/index.m3u8?tok=1", &s)), "http://h/ch/index.m3u8?tok=1&from=1791576000");
}

#[test]
fn append_kind() {
    let s = spec(CatchupKind::Append, Some("?utc={utc}&lutc={lutc}"));
    assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/ch/index.m3u8?utc=1791576000&lutc=1791581400");
    assert_eq!(url(&req("http://h/ch/index.m3u8?token=abc", &s)), "http://h/ch/index.m3u8?token=abc&utc=1791576000&lutc=1791581400");
    let s = spec(CatchupKind::Append, Some("&t={utc}"));
    assert_eq!(url(&req("http://h/ch.ts", &s)), "http://h/ch.ts?t=1791576000");
    let s = spec(CatchupKind::Append, Some("/archive-{utc}-{duration}.m3u8"));
    assert_eq!(url(&req("http://h/ch", &s)), "http://h/ch/archive-1791576000-3600.m3u8");
    let s = spec(CatchupKind::Append, None);
    assert_eq!(url(&req("http://h/ch.ts", &s)), "http://h/ch.ts?utc=1791576000&lutc=1791581400");
}

#[test]
fn shift_kind() {
    let s = spec(CatchupKind::Shift, Some("ignored={utc}"));
    assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/ch/index.m3u8?utc=1791576000&lutc=1791581400");
    assert_eq!(url(&req("http://h/ch/index.m3u8?a=b", &s)), "http://h/ch/index.m3u8?a=b&utc=1791576000&lutc=1791581400");
}

#[test]
fn flussonic_kinds() {
    for kind in [CatchupKind::Flussonic, CatchupKind::FlussonicHls, CatchupKind::FlussonicTs] {
        let s = spec(kind, None);
        assert_eq!(url(&req("http://h/ch/index.m3u8", &s)), "http://h/ch/timeshift_rel-5400.m3u8");
        assert_eq!(url(&req("https://h:8443/a/b/index.m3u8?token=x", &s)), "https://h:8443/a/b/timeshift_rel-5400.m3u8?token=x");
        assert_eq!(url(&req("http://h/ch/mono.m3u8", &s)), "http://h/ch/mono-timeshift_rel-5400.m3u8");
        assert_eq!(url(&req("http://h/ch/video.m3u8", &s)), "http://h/ch/video-timeshift_rel-5400.m3u8");
        assert_eq!(url(&req("http://h/ch/mpegts", &s)), "http://h/ch/timeshift_abs-1791576000.ts");
        assert_eq!(url(&req("http://h/ch/mpegts?token=1", &s)), "http://h/ch/timeshift_abs-1791576000.ts?token=1");
        let err = build_url(&req("http://h/plain.ts", &s)).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }), "{err:?}");
    }
}

#[test]
fn flussonic_offset_is_second_precise() {
    let s = spec(CatchupKind::Flussonic, None);
    let mut r = req("http://h/ch/index.m3u8", &s);
    r.start = S + 29;
    r.stop = S + 3600;
    assert_eq!(url(&r), "http://h/ch/timeshift_rel-5371.m3u8");
}

#[test]
fn xc_kind_uses_server_local_time() {
    let s = spec(CatchupKind::Xc, None);
    let mut r = req("http://h:8080/live/u/p/1234.ts", &s);
    r.tz = london();
    assert_eq!(url(&r), "http://h:8080/timeshift/u/p/60/2026-10-09:21-00/1234.ts");
    r.live_url = "http://h:8080/u/p/1234";
    assert_eq!(url(&r), "http://h:8080/timeshift/u/p/60/2026-10-09:21-00/1234.ts");
    r.live_url = "http://h:8080/live/u/p/1234.m3u8";
    assert_eq!(url(&r), "http://h:8080/timeshift/u/p/60/2026-10-09:21-00/1234.m3u8");
    r.live_url = "http://h:8080/live/u/p/1234.ts?token=1";
    assert_eq!(url(&r), "http://h:8080/timeshift/u/p/60/2026-10-09:21-00/1234.ts");
    // A user template is ignored for Xc.
    let s2 = spec(CatchupKind::Xc, Some("http://other/{utc}"));
    let r2 = CatchupRequest { spec: &s2, live_url: "http://h:8080/live/u/p/1234.ts", ..r.clone() };
    assert_eq!(url(&r2), "http://h:8080/timeshift/u/p/60/2026-10-09:21-00/1234.ts");
    r.live_url = "http://h/just/a/path/too/deep.ts";
    assert!(matches!(build_url(&r), Err(Error::Parse { .. })));
    r.live_url = "rtmp://h/live/u/p/1.ts";
    assert!(matches!(build_url(&r), Err(Error::Parse { .. })));
}

#[test]
fn xc_minutes_round_up_and_start_floors_to_the_minute() {
    let s = spec(CatchupKind::Xc, None);
    let mut r = req("http://h/live/u/p/9.ts", &s);
    r.start = S + 30; // 20:00:30 → 20:00, end 20:45:30 → 46 minutes
    r.stop = S + 45 * 60 + 30;
    assert_eq!(url(&r), "http://h/timeshift/u/p/46/2026-10-09:20-00/9.ts");
}

#[test]
fn xc_server_timezone_across_dst_boundary() {
    // BST ends 2026-10-25 01:00 UTC: 00:30 UTC is 01:30 BST and 01:30 UTC is 01:30 GMT.
    let s = spec(CatchupKind::Xc, None);
    let now = ts("2026-10-26T12:00:00Z");
    let at = |start: &str| {
        let start = ts(start);
        let r = CatchupRequest { start, stop: start + 1800, now, tz: london(), ..req("http://h/live/u/p/7.ts", &s) };
        url(&r)
    };
    assert_eq!(at("2026-10-24T20:00:00Z"), "http://h/timeshift/u/p/30/2026-10-24:21-00/7.ts");
    assert_eq!(at("2026-10-25T00:30:00Z"), "http://h/timeshift/u/p/30/2026-10-25:01-30/7.ts");
    assert_eq!(at("2026-10-25T01:30:00Z"), "http://h/timeshift/u/p/30/2026-10-25:01-30/7.ts");
    assert_eq!(at("2026-10-26T08:00:00Z"), "http://h/timeshift/u/p/30/2026-10-26:08-00/7.ts");
    // Spring forward (2026-03-29 01:00 UTC): 00:30 UTC = 00:30 GMT, 01:30 UTC = 02:30 BST.
    let s = CatchupSpec { days: 7, ..spec(CatchupKind::Xc, None) };
    let now = ts("2026-03-30T00:00:00Z");
    let at = |start: &str| {
        let start = ts(start);
        url(&CatchupRequest { start, stop: start + 3600, now, tz: london(), ..req("http://h/live/u/p/7.ts", &s) })
    };
    assert_eq!(at("2026-03-29T00:30:00Z"), "http://h/timeshift/u/p/60/2026-03-29:00-30/7.ts");
    assert_eq!(at("2026-03-29T01:30:00Z"), "http://h/timeshift/u/p/60/2026-03-29:02-30/7.ts");
}

#[test]
fn xc_matches_native_xtream_builder() {
    // The M3U `catchup="xc"` path and the native Xtream builder agree, credentials included.
    let creds = XtreamCredentials { server: "http://h:8080/".into(), username: "u@x".into(), password: Secret::new("p:/?#.1".into()) };
    for ext in [LiveExt::Ts, LiveExt::M3u8] {
        let live = urls::live_url(&creds, 42, ext);
        let s = spec(CatchupKind::Xc, None);
        let r = CatchupRequest { tz: london(), ..req(&live, &s) };
        let start_local = london().to_datetime(jiff::Timestamp::from_second(S).unwrap());
        assert_eq!(url(&r), urls::timeshift_url(&creds, 42, start_local, 60, ext));
    }
    assert_eq!(urls::pick_live_ext(LiveFormat::Auto, &[]), LiveExt::M3u8);
}

#[test]
fn vod_kind() {
    let s = spec(CatchupKind::Vod, Some("http://vod/{catchup-id}.mp4?s={utc}"));
    let mut r = req("http://h/ch.ts", &s);
    r.catchup_id = Some("abc123");
    assert_eq!(url(&r), "http://vod/abc123.mp4?s=1791576000");
    let s = spec(CatchupKind::Vod, None);
    let mut r = req("http://h/ch.ts", &s);
    r.catchup_id = Some("http://vod/direct/abc.mp4");
    assert_eq!(url(&r), "http://vod/direct/abc.mp4");
    r.catchup_id = None;
    assert!(matches!(build_url(&r), Err(Error::NotFound(_))));
}

#[test]
fn correction_tvg_shift_and_user_offset() {
    // Displayed start 21:00 on a `tvg-shift=1` channel = EPG 20:00. Correction 120 s earlier,
    // user offset +30 s later: archive start = 21:00 − 60 min − 120 s + 30 s = 19:58:30.
    let s = CatchupSpec { correction_s: 120, ..spec(CatchupKind::Shift, None) };
    let r =
        CatchupRequest { start: S + 3600, stop: S + 7200, now: S + 4 * 3600, tvg_shift_min: 60, user_offset_s: 30, ..req("http://h/ch.m3u8", &s) };
    let start = S - 90;
    assert_eq!(url(&r), format!("http://h/ch.m3u8?utc={start}&lutc={}", S + 4 * 3600));
    assert_eq!(expand("{utc}-{utcend}-{duration}", &r), format!("{start}-{}-3600", start + 3600));
    // Negative shift moves the archive start later.
    let r = CatchupRequest { tvg_shift_min: -30, user_offset_s: 0, ..r };
    assert_eq!(expand("{utc}", &r), (S + 3600 - 120 + 1800).to_string());
}

#[test]
fn granularity() {
    let s = spec(CatchupKind::Default, None);
    let r = CatchupRequest { start: S + 45, stop: S + 3600, ..req("http://h/x", &s) };
    // 1 s tokens keep the seconds…
    assert_eq!(expand("{utc}", &r), (S + 45).to_string());
    assert_eq!(expand("${start}", &r), (S + 45).to_string());
    assert_eq!(expand("{offset:1}", &r), (5400 - 45).to_string());
    assert_eq!(expand("{H}{M}{S}", &r), "200045");
    // …everything else floors to the minute (the end stays put, so the duration grows).
    assert_eq!(expand("{utcend}|{duration}", &r), format!("{}|3600", S + 3600));
    assert_eq!(expand("${end}", &r), (S + 3600).to_string());
    assert_eq!(expand("{Y}-{m}-{d}:{H}-{M}", &r), "2026-10-09:20-00");
    assert_eq!(expand("${offset}", &r), "5400");
}

#[test]
fn duration_is_capped_at_now() {
    let s = spec(CatchupKind::Default, Some("http://h/a?s={utc}&e={utcend}&d={duration}&m={duration:60}"));
    // Running programme: 20:00–21:00, now 20:20.
    let r = CatchupRequest { now: S + 1200, ..req("http://h/x", &s) };
    assert_eq!(url(&r), format!("http://h/a?s={S}&e={}&d=1200&m=20", S + 1200));
    // Unknown stop → 3600 s, still capped.
    let r = CatchupRequest { stop: 0, now: S + 600, ..req("http://h/x", &s) };
    assert_eq!(url(&r), format!("http://h/a?s={S}&e={}&d=600&m=10", S + 600));
    let r = CatchupRequest { stop: S, now: S + 7200, ..req("http://h/x", &s) };
    assert_eq!(url(&r), format!("http://h/a?s={S}&e={}&d=3600&m=60", S + 3600));
    // Shifted channel: archive start moves earlier, the cap applies in archive time.
    let r = CatchupRequest { start: S + 600, stop: S + 4200, now: S + 1200, tvg_shift_min: 60, ..req("http://h/x", &s) };
    assert_eq!(url(&r), format!("http://h/a?s={}&e={}&d=3600&m=60", S + 600 - 3600, S + 600));
}

#[test]
fn placeholders() {
    let s = spec(CatchupKind::Default, None);
    let r = CatchupRequest { tz: london(), catchup_id: Some("cid"), ..req("http://h/x", &s) };
    let cases: &[(&str, String)] = &[
        ("{utc}", S.to_string()),
        ("${start}", S.to_string()),
        ("{utcend}", (S + 3600).to_string()),
        ("${end}", (S + 3600).to_string()),
        ("{lutc}", (S + 5400).to_string()),
        ("${now}", (S + 5400).to_string()),
        ("${timestamp}", (S + 5400).to_string()),
        ("{duration}", "3600".into()),
        ("${duration}", "3600".into()),
        ("{duration:60}", "60".into()),
        ("{duration:7}", "515".into()),
        ("{offset:60}", "90".into()),
        ("{offset:7}", "771".into()),
        ("${offset}", "5400".into()),
        ("{Y}{m}{d}{H}{M}{S}", "20261009210000".into()),
        ("{utc:Y-m-d H:M:S}", "2026-10-09 21:00:00".into()),
        ("${start:YmdHMS}", "20261009210000".into()),
        ("{utcend:YmdHM}", "202610092200".into()),
        ("${end:H}", "22".into()),
        ("{lutc:Y/m/d}", "2026/10/09".into()),
        ("${now:H-M}", "22-30".into()),
        ("{catchup-id}", "cid".into()),
        // unknown / malformed tokens stay verbatim
        ("{foo}", "{foo}".into()),
        ("${bar}", "${bar}".into()),
        ("{offset}", "{offset}".into()),
        ("{offset:0}", "{offset:0}".into()),
        ("{duration:x}", "{duration:x}".into()),
        ("${utc}", "${utc}".into()),
        ("{start}", "{start}".into()),
        ("{utc", "{utc".into()),
        ("$x$", "$x$".into()),
        ("{{utc}", format!("{{{S}")),
        ("a{}b", "a{}b".into()),
        ("plain", "plain".into()),
        ("ü{utc}ü", format!("ü{S}ü")),
    ];
    for (t, want) in cases {
        assert_eq!(&expand(t, &r), want, "{t}");
    }
    let r = CatchupRequest { catchup_id: None, ..r };
    assert_eq!(expand("id={catchup-id}", &r), "id=");
}

#[test]
fn archive_window() {
    let s = spec(CatchupKind::Shift, None); // 3 days
    let now = S + 10 * 86_400;
    let at = |start: Timestamp| build_url(&CatchupRequest { start, stop: start + 1800, now, ..req("http://h/x", &s) });
    assert!(at(now - 3 * 86_400).is_ok(), "window start is inclusive");
    assert!(matches!(at(now - 3 * 86_400 - 1), Err(Error::NotFound(_))));
    assert!(at(now - 1).is_ok());
    assert!(matches!(at(now), Err(Error::NotFound(_))));
    assert!(matches!(at(now + 60), Err(Error::NotFound(_))));
    assert!(in_window(&s, now - 60, now) && !in_window(&s, now, now));
    // A displayed start in the past whose archive start is still ahead (negative tvg-shift).
    let r = CatchupRequest { start: now - 600, stop: now + 600, now, tvg_shift_min: -60, ..req("http://h/x", &s) };
    assert!(matches!(build_url(&r), Err(Error::NotFound(_))));
}

#[test]
fn errors_never_contain_the_live_url() {
    let s = spec(CatchupKind::Xc, None);
    let r = req("http://h/secret-token-123/x", &s);
    let e = build_url(&r).unwrap_err().to_string();
    assert!(!e.contains("secret-token-123"), "{e}");
    let s = spec(CatchupKind::Flussonic, None);
    let e = build_url(&req("http://h/secret-token-123", &s)).unwrap_err().to_string();
    assert!(!e.contains("secret-token-123"), "{e}");
}

#[test]
fn live_url_is_not_expanded() {
    let s = spec(CatchupKind::Shift, None);
    assert_eq!(url(&req("http://h/{utc}/ch", &s)), "http://h/{utc}/ch?utc=1791576000&lutc=1791581400");
}

#[test]
fn hostile_provider_values_do_not_overflow() {
    // `{duration:X}` with a huge divisor still rounds up instead of overflowing.
    let s = spec(CatchupKind::Default, Some("http://h/a?d={duration:9223372036854775807}&o={offset:9223372036854775807}"));
    assert_eq!(url(&req("http://h/x", &s)), "http://h/a?d=1&o=0");
    // Extreme corrections / offsets saturate and land outside the window instead of panicking.
    for correction_s in [i64::MIN, i64::MAX] {
        let s = CatchupSpec { correction_s, ..spec(CatchupKind::Default, Some("http://h/a?s={utc}&e={utcend}&o=${offset}")) };
        assert!(matches!(build_url(&req("http://h/x", &s)), Err(Error::NotFound(_))), "{correction_s}");
        let r = req("http://h/x", &s);
        let _ = expand("{utc}{utcend}{duration}{duration:60}{offset:60}${offset}{Y}{utc:Y-m-d}", &r);
    }
    for user_offset_s in [i64::MIN, i64::MAX] {
        let s = spec(CatchupKind::Xc, None);
        let r = CatchupRequest { user_offset_s, ..req("http://h/live/u/p/1.ts", &s) };
        assert!(matches!(build_url(&r), Err(Error::NotFound(_))), "{user_offset_s}");
        let _ = expand("{utc}{utcend}{duration:60}{offset:1}${offset}{M}", &r);
    }
    // Programme times at the extremes of the range.
    let s = spec(CatchupKind::Default, Some("http://h/a?d={duration}"));
    let r = CatchupRequest { start: i64::MIN, stop: i64::MAX, ..req("http://h/x", &s) };
    let _ = expand("{utc}{duration}{duration:7}${offset}", &r);
    // A correction more than a day beyond the archive depth is not served.
    let s = CatchupSpec { correction_s: 5 * 86_400, ..spec(CatchupKind::Shift, None) };
    assert!(matches!(build_url(&req("http://h/x", &s)), Err(Error::NotFound(_))));
}
