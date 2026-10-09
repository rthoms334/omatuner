//! Xtream: fixture deserialization, URL builder goldens and the client against the in-process
//! mock panel (`support::mock_xtream`). Owner: WP-04. No network: everything is 127.0.0.1.

mod support;

use std::time::{Duration, Instant};

use support::mock_xtream::{self, MOCK_ETAG, MockAuth, MockOptions};
use tokio_util::sync::CancellationToken;
use tuner::error::Error;
use tuner::model::{AccountStatus, LiveFormat, XtreamCredentials};
use tuner::redact::Secret;
use tuner::sources::http::{Compression, DownloadOutcome, DownloadRequest, HttpClient, HttpOptions, open_decompressed};
use tuner::sources::xtream::XtreamClient;
use tuner::sources::xtream::types::*;
use tuner::sources::xtream::urls::{self, LiveExt};
use tuner::sources::{PayloadKind, sniff_payload};

const NOW: i64 = 1_791_576_000; // 2026-10-09 18:00 UTC (= auth_ok.json timestamp_now)

fn http() -> HttpClient { HttpClient::new(&HttpOptions::default()).expect("http client") }

fn client(creds: XtreamCredentials) -> XtreamClient { XtreamClient::new(http(), creds, Some("TunerTest/1.0".into())) }

fn read(rel: &str) -> Vec<u8> { std::fs::read(support::fixture(rel)).expect(rel) }

fn de<T: serde::de::DeserializeOwned>(rel: &str) -> T {
    serde_json::from_slice(&read(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Assert that neither the raw nor the percent-encoded secret shows up in an error.
#[track_caller]
fn assert_clean(e: &Error, secret: &str) {
    let shown = format!("{e} | {e:?}");
    assert!(!shown.contains(secret) && !shown.contains(&urls::enc(secret)), "secret leaked: {shown}");
}

// ---------------------------------------------------------------------------------------------
// fixtures

#[test]
fn every_fixture_deserializes() {
    let dir = support::fixture("xtream");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let rel = format!("xtream/{name}");
        match name.as_str() {
            "auth_ok.json" | "auth_expired.json" | "auth_fail.json" => {
                let _: XtAuth = de(&rel);
            }
            n if n.starts_with("live_categories") || n.ends_with("_categories.json") => {
                let v: Vec<serde_json::Value> = match serde_json::from_slice::<serde_json::Value>(&read(&rel)).unwrap() {
                    serde_json::Value::Array(a) => a,
                    serde_json::Value::Object(m) => m.into_iter().map(|(_, v)| v).filter(|v| v.is_object()).collect(),
                    other => panic!("{rel}: {other}"),
                };
                for c in v {
                    let _: XtCategory = serde_json::from_value(c).unwrap();
                }
            }
            "live_streams.json" => {
                let _: Vec<XtLiveStream> = de(&rel);
            }
            "vod_streams.json" => {
                let _: Vec<XtVodStream> = de(&rel);
            }
            "series.json" => {
                let _: Vec<XtSeries> = de(&rel);
            }
            n if n.starts_with("vod_info") => {
                let _: XtVodInfo = de(&rel);
            }
            n if n.starts_with("series_info") => {
                let _: XtSeriesInfo = de(&rel);
            }
            n if n.starts_with("short_epg") => {
                let _: XtEpgResponse = de(&rel);
            }
            "html_200_error.html" => assert_eq!(sniff_payload(&read(&rel)), PayloadKind::Html),
            other => panic!("fixture {other} has no deserialization check — add one"),
        }
        seen += 1;
    }
    assert!(seen >= 20, "only {seen} fixtures");
}

#[test]
fn auth_fixtures_field_values() {
    let ok: XtAuth = de("xtream/auth_ok.json");
    assert_eq!(ok.user_info.auth, Some(1));
    assert_eq!(ok.user_info.exp_date, Some(1_798_761_600));
    assert_eq!(ok.user_info.max_connections, Some(2));
    assert_eq!(ok.user_info.allowed_output_formats, vec!["m3u8", "ts", "rtmp"]);
    assert_eq!(ok.server_info.timezone.as_deref(), Some("Europe/London"));
    assert_eq!(ok.server_info.timestamp_now, Some(NOW));
    let expired: XtAuth = de("xtream/auth_expired.json");
    assert_eq!(expired.user_info.status.as_deref(), Some("Expired"));
    assert_eq!(expired.user_info.active_cons, Some(1));
    assert_eq!(expired.user_info.allowed_output_formats, vec!["ts"]);
    let fail: XtAuth = de("xtream/auth_fail.json");
    assert_eq!(fail.user_info.auth, Some(0));
    assert!(fail.server_info.timezone.is_none(), "server_info: [] → default");
}

#[test]
fn catalogue_fixture_quirks() {
    let live: Vec<XtLiveStream> = de("xtream/live_streams.json");
    assert_eq!(live.len(), 3);
    assert_eq!((live[0].stream_id, live[0].num, live[0].tv_archive, live[0].tv_archive_duration), (Some(1001), Some(1), Some(1), Some(7)));
    assert_eq!(live[0].epg_channel_id.as_deref(), Some("bbc1.uk"));
    assert_eq!(live[0].direct_source, None, "empty string → None");
    assert_eq!((live[1].stream_id, live[1].num, live[1].tv_archive), (Some(1002), Some(2), Some(0)));
    assert_eq!(live[1].category_id.as_deref(), Some("2"), "number → string");
    assert_eq!(live[1].category_ids, vec!["2", "1"]);
    assert_eq!(live[1].stream_icon, None);
    assert_eq!((live[2].stream_id, live[2].num), (None, None), "broken entry survives with no id");

    let cats: Vec<XtCategory> = de("xtream/live_categories.json");
    assert_eq!(cats.iter().map(|c| c.category_id.clone()).collect::<Vec<_>>(), vec![Some("1".into()), Some("2".into()), Some("3".into())]);
    assert_eq!(cats[1].parent_id, Some(0));
    assert_eq!(cats[2].category_name, None);

    let vod: Vec<XtVodStream> = de("xtream/vod_streams.json");
    assert_eq!((vod[0].stream_id, vod[0].rating), (Some(5001), Some(8.7)));
    assert_eq!((vod[1].stream_id, vod[1].rating), (Some(5002), None));
    assert_eq!(vod[1].category_id.as_deref(), Some("10"));

    let series: Vec<XtSeries> = de("xtream/series.json");
    assert_eq!((series[0].series_id, series[0].last_modified, series[0].rating), (Some(700), Some(1_700_000_400), Some(7.0)));
    assert_eq!(series[0].backdrop_path, vec!["http://img.example/showb.jpg"], "string → vec");
    assert_eq!(series[0].release_date.as_deref(), Some("2020-01-01"));
    assert_eq!((series[0].plot.clone(), series[0].cast.clone()), (None, None));
}

#[test]
fn detail_fixture_quirks() {
    let full: XtVodInfo = de("xtream/vod_info_full.json");
    assert_eq!(full.info.duration_secs, Some(8160));
    assert_eq!(full.info.rating, Some(8.7));
    assert_eq!(full.info.backdrop_path.len(), 2);
    assert_eq!((full.movie_data.stream_id, full.movie_data.container_extension.as_deref()), (Some(5001), Some("mkv")));
    let empty: XtVodInfo = de("xtream/vod_info_empty_info.json");
    assert!(empty.info.plot.is_none(), "info: [] → default");
    assert_eq!(empty.movie_data.stream_id, Some(5002));

    let obj: XtSeriesInfo = de("xtream/series_info_object.json");
    assert_eq!(obj.seasons.len(), 1);
    assert_eq!(obj.seasons[0].episode_count, Some(2));
    assert_eq!(obj.info.plot.as_deref(), Some("Plot"));
    let s1 = &obj.episodes[&1];
    assert_eq!(s1.len(), 2);
    assert_eq!((s1[0].id.as_deref(), s1[0].info.duration_secs), (Some("9001"), Some(2520)));
    assert_eq!((s1[1].id.as_deref(), s1[1].episode_num, s1[1].info.plot.clone()), (Some("9002"), Some(2), None));

    let arr: XtSeriesInfo = de("xtream/series_info_array.json");
    assert_eq!(arr.episodes.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(arr.episodes[&2][0].id.as_deref(), Some("9201"));
    assert!(arr.info.plot.is_none());

    let empty: XtSeriesInfo = de("xtream/series_info_empty.json");
    assert!(empty.episodes.is_empty() && empty.seasons.is_empty());

    let flat: XtSeriesInfo = de("xtream/series_info_flat.json");
    assert!(flat.seasons.is_empty(), "seasons: null");
    assert_eq!(flat.episodes[&2].iter().map(|e| e.id.clone().unwrap()).collect::<Vec<_>>(), vec!["9301", "9302"]);
    assert_eq!(flat.episodes[&1][0].id.as_deref(), Some("9303"));

    let keyed: XtSeriesInfo = de("xtream/series_info_keyed.json");
    assert_eq!(keyed.episodes[&3][0].title.as_deref(), Some("Keyed"));
    assert_eq!(keyed.episodes[&4][0].title.as_deref(), Some("Odd key"), "non-numeric key → episode season");
}

#[test]
fn short_epg_fixtures_decode_per_response() {
    let mut b64: XtEpgResponse = de("xtream/short_epg_b64.json");
    decode_epg_texts(&mut b64.epg_listings);
    let l = &b64.epg_listings;
    assert_eq!(l[0].title.as_deref(), Some("News"));
    assert_eq!(l[0].description.as_deref(), Some("Tom & Jerry"));
    assert_eq!(l[1].title.as_deref(), Some("Drama ünïcödé"));
    assert_eq!(l[1].description, None);
    assert_eq!((l[0].start_timestamp, l[0].stop_timestamp), (Some(NOW), Some(NOW + 3600)));

    let mut plain: XtEpgResponse = de("xtream/short_epg_plain.json");
    decode_epg_texts(&mut plain.epg_listings);
    let titles: Vec<_> = plain.epg_listings.iter().map(|l| l.title.clone().unwrap()).collect();
    assert_eq!(titles, vec!["News", "Film", "Tom & Jerry"], "plain text stays plain");
}

// ---------------------------------------------------------------------------------------------
// URL builders (frozen bodies, golden)

fn golden_creds() -> XtreamCredentials {
    XtreamCredentials { server: "https://panel.example:8443".into(), username: "jo hn@x".into(), password: Secret::new("p&ss/w?rd#1%".into()) }
}

#[test]
fn url_builders_golden() {
    let c = golden_creds();
    let (u, p) = ("jo%20hn%40x", "p%26ss%2Fw%3Frd%231%25");
    assert_eq!(urls::live_url(&c, 1001, LiveExt::M3u8), format!("https://panel.example:8443/live/{u}/{p}/1001.m3u8"));
    assert_eq!(urls::live_url(&c, 1001, LiveExt::Ts), format!("https://panel.example:8443/live/{u}/{p}/1001.ts"));
    assert_eq!(urls::movie_url(&c, 5001, ".mkv"), format!("https://panel.example:8443/movie/{u}/{p}/5001.mkv"));
    assert_eq!(urls::episode_url(&c, "9001", "mp4"), format!("https://panel.example:8443/series/{u}/{p}/9001.mp4"));
    let start = jiff::civil::date(2026, 3, 29).at(1, 30, 0, 0);
    assert_eq!(
        urls::timeshift_url(&c, 1001, start, 45, LiveExt::Ts),
        format!("https://panel.example:8443/timeshift/{u}/{p}/45/2026-03-29:01-30/1001.ts")
    );
    assert_eq!(urls::xmltv_url(&c), format!("https://panel.example:8443/xmltv.php?username={u}&password={p}"));
    assert_eq!(urls::m3u_url(&c, LiveExt::Ts), format!("https://panel.example:8443/get.php?username={u}&password={p}&type=m3u_plus&output=ts"));
    assert_eq!(urls::player_api_url(&c, None, &[]), format!("https://panel.example:8443/player_api.php?username={u}&password={p}"));
    assert_eq!(
        urls::player_api_url(&c, Some("get_series_info"), &[("series_id", "7".into())]),
        format!("https://panel.example:8443/player_api.php?username={u}&password={p}&action=get_series_info&series_id=7")
    );
    // trailing slash on the stored server is tolerated
    let slash = XtreamCredentials { server: "http://h/".into(), username: "a".into(), password: Secret::new("b".into()) };
    assert_eq!(urls::live_url(&slash, 1, LiveExt::Ts), "http://h/live/a/b/1.ts");
    // every credential-bearing URL is fully redacted for display
    for url in [urls::live_url(&c, 1, LiveExt::Ts), urls::xmltv_url(&c), urls::m3u_url(&c, LiveExt::M3u8), urls::player_api_url(&c, None, &[])] {
        let shown = tuner::redact::redact_url(&url);
        assert!(!shown.contains(p) && !shown.contains(u), "{shown}");
    }
}

#[test]
fn url_helpers_golden() {
    assert_eq!(urls::enc("aZ09-._~"), "aZ09-._~");
    assert_eq!(urls::enc("a b+ü"), "a%20b%2B%C3%BC");
    for (input, want) in [
        ("panel.tv", Some("http://panel.tv")),
        ("  panel.tv:25461  ", Some("http://panel.tv:25461")),
        ("https://panel.tv:443/", Some("https://panel.tv")),
        ("http://panel.tv:8080/get.php?username=a&password=b&type=m3u_plus", Some("http://panel.tv:8080")),
        ("HTTP://Panel.TV:80/player_api.php", Some("http://panel.tv")),
        ("http://", None),
        ("   ", None),
    ] {
        assert_eq!(urls::normalize_server(input).as_deref(), want, "{input:?}");
    }
    let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(urls::pick_live_ext(LiveFormat::Auto, &f(&["m3u8", "ts", "rtmp"])), LiveExt::M3u8);
    assert_eq!(urls::pick_live_ext(LiveFormat::Auto, &f(&["M3U8"])), LiveExt::M3u8);
    assert_eq!(urls::pick_live_ext(LiveFormat::Auto, &f(&["ts", "rtmp"])), LiveExt::Ts);
    assert_eq!(urls::pick_live_ext(LiveFormat::Auto, &[]), LiveExt::M3u8);
    assert_eq!(urls::pick_live_ext(LiveFormat::Ts, &f(&["m3u8"])), LiveExt::Ts);
    assert_eq!(urls::pick_live_ext(LiveFormat::M3u8, &f(&["ts"])), LiveExt::M3u8);
    assert_eq!(LiveExt::M3u8.as_str(), "m3u8");
}

// ---------------------------------------------------------------------------------------------
// client against the mock panel

#[tokio::test]
async fn authenticate_ok_maps_account_info() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let pw = "pw-auth-ok-Qm3";
    let (auth, info) = client(mock.creds("demo", pw)).authenticate(NOW - 30, &CancellationToken::new()).await.unwrap();
    assert_eq!(auth.user_info.username.as_deref(), Some("demo"));
    assert_eq!(info.status, AccountStatus::Active);
    assert_eq!(info.exp_date, Some(1_798_761_600));
    assert!(!info.is_trial);
    assert_eq!((info.active_cons, info.max_connections), (Some(0), Some(2)));
    assert_eq!(info.created_at, Some(1_700_000_000));
    assert_eq!(info.allowed_formats, vec!["m3u8", "ts", "rtmp"]);
    assert_eq!(info.server_tz.as_deref(), Some("Europe/London"));
    assert_eq!(info.clock_skew_s, Some(30));
    assert_eq!(info.message, None);
    // requests go to the typed server, never to server_info.url ("internal.panel.lan")
    assert_eq!(mock.recorded(), vec![format!("/player_api.php?username=demo&password={pw}")]);
}

#[tokio::test]
async fn authenticate_failure_outcomes() {
    let pw = "pw-auth-outcomes-Lk7";
    let ok = mock_xtream::start(MockOptions::default()).await;
    let cancel = CancellationToken::new();
    type Check = fn(&Error) -> bool;
    let cases: Vec<(MockAuth, &str, Check)> = vec![
        (MockAuth::Rejected, "demo", |e| matches!(e, Error::AuthRejected)),
        (MockAuth::Http403, "demo", |e| matches!(e, Error::AuthRejected)),
        (MockAuth::Expired, "demo", |e| matches!(e, Error::AccountUnavailable { status } if status == "expired")),
        (MockAuth::Ok, "html", |e| matches!(e, Error::BadPayload(m) if m.contains("web page"))),
        (MockAuth::Ok, "empty", |e| matches!(e, Error::AuthRejected)),
        (MockAuth::Ok, "emptyarray", |e| matches!(e, Error::AuthRejected)),
        (MockAuth::Ok, "http401", |e| matches!(e, Error::AuthRejected)),
        (MockAuth::Ok, "banned", |e| matches!(e, Error::AccountUnavailable { status } if status == "banned")),
        (MockAuth::Ok, "disabled", |e| matches!(e, Error::AccountUnavailable { status } if status == "disabled")),
    ];
    for (auth, user, check) in cases {
        let mock = if auth == MockAuth::Ok { None } else { Some(mock_xtream::start(MockOptions { auth, ..Default::default() }).await) };
        let creds = mock.as_ref().unwrap_or(&ok).creds(user, pw);
        let err = client(creds).authenticate(NOW, &cancel).await.unwrap_err();
        assert!(check(&err), "{auth:?}/{user}: got {err:?}");
        assert_clean(&err, pw);
    }
    assert_eq!(Error::AccountUnavailable { status: "expired".into() }.to_string(), "account expired");
}

#[tokio::test]
async fn catalogue_lists() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let c = client(mock.creds("demo", "pw-lists-Tz1"));
    let cancel = CancellationToken::new();
    assert_eq!(c.live_categories(&cancel).await.unwrap().len(), 3);
    let live = c.live_streams(&cancel).await.unwrap();
    assert_eq!(live.iter().map(|s| s.stream_id).collect::<Vec<_>>(), vec![Some(1001), Some(1002), None]);
    assert_eq!(c.vod_categories(&cancel).await.unwrap().len(), 2);
    assert_eq!(c.vod_streams(&cancel).await.unwrap().len(), 2);
    assert_eq!(c.series_categories(&cancel).await.unwrap()[0].category_id.as_deref(), Some("20"));
    assert_eq!(c.series(&cancel).await.unwrap()[0].series_id, Some(700));
    let reqs = mock.recorded();
    // live streams in ONE call without a category filter
    let live_reqs: Vec<_> = reqs.iter().filter(|r| r.contains("action=get_live_streams")).collect();
    assert_eq!(live_reqs.len(), 1);
    assert!(!live_reqs[0].contains("category_id"));
    assert_eq!(c.credentials().username, "demo");
}

#[tokio::test]
async fn id_keyed_list_reply_is_flattened() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let c = client(mock.creds("keyed", "pw-keyed-Jd3"));
    let cats = c.live_categories(&CancellationToken::new()).await.unwrap();
    // junk values ("not a category", null) are skipped; the rest keep the panel's order
    let ids: Vec<_> = cats.iter().map(|c| c.category_id.clone().unwrap()).collect();
    assert_eq!(ids, vec!["2", "1"]);
    assert_eq!(cats[1].category_name.as_deref(), Some("News"));
}

#[tokio::test]
async fn lists_with_rejected_login_or_html() {
    let cancel = CancellationToken::new();
    let rejected = mock_xtream::start(MockOptions { auth: MockAuth::Rejected, ..Default::default() }).await;
    let c = client(rejected.creds("demo", "pw-list-rej-Hh4"));
    assert!(matches!(c.live_streams(&cancel).await, Err(Error::AuthRejected)));
    assert!(matches!(c.vod_info(5001, &cancel).await, Err(Error::AuthRejected)));
    assert!(matches!(c.series_info(700, &cancel).await, Err(Error::AuthRejected)));
    assert!(matches!(c.short_epg(1001, 4, &cancel).await, Err(Error::AuthRejected)));

    let html = mock_xtream::start(MockOptions { html_200: true, ..Default::default() }).await;
    let c = client(html.creds("demo", "pw-list-html-Hh5"));
    c.authenticate(NOW, &cancel).await.expect("login itself is fine");
    let e = c.live_categories(&cancel).await.unwrap_err();
    assert!(matches!(e, Error::BadPayload(_)), "{e:?}");
}

#[tokio::test]
async fn details() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let c = client(mock.creds("demo", "pw-details-Pp2"));
    let cancel = CancellationToken::new();
    let full = c.vod_info(5001, &cancel).await.unwrap();
    assert_eq!(full.info.tmdb_id.as_deref(), Some("603"));
    let plain = c.vod_info(5002, &cancel).await.unwrap();
    assert!(plain.info.plot.is_none());
    assert_eq!(plain.movie_data.container_extension.as_deref(), Some("mp4"));
    assert!(matches!(c.vod_info(9999, &cancel).await, Err(Error::NotFound(_))));

    assert_eq!(c.series_info(700, &cancel).await.unwrap().episodes[&1].len(), 2);
    assert_eq!(c.series_info(701, &cancel).await.unwrap().episodes.len(), 2);
    let before = mock.recorded().len();
    // fork spelling: empty for series_id=, data for series=
    let fork = c.series_info(703, &cancel).await.unwrap();
    assert_eq!(fork.episodes[&1][0].title.as_deref(), Some("Pilot"));
    let tried: Vec<_> = mock.recorded()[before..].to_vec();
    assert_eq!(tried.len(), 2);
    assert!(tried[0].ends_with("action=get_series_info&series_id=703"), "{tried:?}");
    assert!(tried[1].ends_with("action=get_series_info&series=703"), "{tried:?}");
    // empty both ways → the empty result, not an error
    let none = c.series_info(999, &cancel).await.unwrap();
    assert!(none.episodes.is_empty());

    let epg = c.short_epg(1001, 4, &cancel).await.unwrap();
    assert_eq!(epg.iter().map(|l| l.title.clone().unwrap()).collect::<Vec<_>>(), vec!["News", "Drama ünïcödé"]);
    assert!(mock.recorded().last().unwrap().ends_with("action=get_short_epg&stream_id=1001&limit=4"));
    assert!(c.short_epg(2, 4, &cancel).await.unwrap().is_empty());
}

#[tokio::test]
async fn slow_panel_times_out_and_cancels_quickly() {
    let pw = "pw-slow-Vb6";
    let mock = mock_xtream::start(MockOptions { delay_ms: 3_000, ..Default::default() }).await;
    let short = HttpClient::new(&HttpOptions { read_timeout_s: 1, ..Default::default() }).unwrap();
    let c = XtreamClient::new(short, mock.creds("demo", pw), None);
    let start = Instant::now();
    let e = c.authenticate(NOW, &CancellationToken::new()).await.unwrap_err();
    assert!(start.elapsed() < Duration::from_millis(2_500), "{:?}", start.elapsed());
    assert!(matches!(e, Error::Network(_)) && e.is_transient(), "{e:?}");
    assert_clean(&e, pw);

    let c = client(mock.creds("demo", pw));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
        Instant::now()
    });
    let e = c.live_streams(&cancel).await.unwrap_err();
    let elapsed = Instant::now().duration_since(canceller.await.unwrap());
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(elapsed < Duration::from_millis(200), "cancel took {elapsed:?}");
}

#[tokio::test]
async fn provider_guide_and_playlist_downloads() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let creds = mock.creds("demo", "pw-dl-Ew9");
    let dir = tempfile::tempdir().unwrap();
    let h = http();
    let cancel = CancellationToken::new();
    let noop = |_: u64, _: Option<u64>| {};

    let dest = dir.path().join("epg/7.xml");
    let mut req = DownloadRequest {
        url: Secret::new(urls::xmltv_url(&creds)),
        dest: dest.clone(),
        headers: Default::default(),
        etag: None,
        last_modified: None,
    };
    let out = h.download(&req, &noop, &cancel).await.unwrap();
    let DownloadOutcome::Downloaded { compression, etag, last_modified, bytes, .. } = out else { panic!("{out:?}") };
    assert_eq!(compression, Compression::Gzip, "sniffed from magic bytes, not headers");
    assert_eq!(bytes, read("xmltv/basic.xml.gz").len() as u64);
    assert_eq!(etag.as_deref(), Some(MOCK_ETAG));
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut open_decompressed(&dest).unwrap(), &mut xml).unwrap();
    assert_eq!(xml, String::from_utf8(read("xmltv/basic.xml")).unwrap());

    req.etag = etag;
    req.last_modified = last_modified;
    assert_eq!(h.download(&req, &noop, &cancel).await.unwrap(), DownloadOutcome::NotModified);

    let m3u = dir.path().join("playlists/7.m3u");
    let req = DownloadRequest { url: Secret::new(urls::m3u_url(&creds, LiveExt::M3u8)), dest: m3u.clone(), headers: Default::default(), etag: None, last_modified: None };
    h.download(&req, &noop, &cancel).await.unwrap();
    let text = std::fs::read_to_string(&m3u).unwrap();
    assert_eq!(sniff_payload(text.as_bytes()), PayloadKind::M3u);
    assert!(text.contains(&format!("{}/live/demo/pw-dl-Ew9/1001.m3u8", mock.base)), "{text}");
    assert!(text.contains("group-title=\"UK | News\""));
    assert!(!text.contains("Broken entry"), "entries without id are skipped");
}

#[tokio::test]
async fn stream_paths_redirect_to_test_media() {
    let mock = mock_xtream::start(MockOptions { media_base: Some("http://media.invalid/m/".into()), ..Default::default() }).await;
    let creds = mock.creds("demo", "pw-media-Rr2");
    let raw = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    for (url, file) in [
        (urls::live_url(&creds, 1001, LiveExt::Ts), "bars.ts"),
        (urls::live_url(&creds, 1001, LiveExt::M3u8), "hls/index.m3u8"),
        (urls::movie_url(&creds, 5001, "mkv"), "testsrc.mp4"),
        (urls::episode_url(&creds, "9001", "mp4"), "testsrc.mp4"),
        (urls::timeshift_url(&creds, 1001, jiff::civil::date(2026, 10, 9).at(20, 0, 0, 0), 60, LiveExt::Ts), "bars.ts"),
    ] {
        let resp = raw.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FOUND, "{url}");
        assert_eq!(resp.headers()["location"], format!("http://media.invalid/m/{file}").as_str());
    }
    // no media configured → 404; 403 switch covers stream paths (too many connections)
    let bare = mock_xtream::start(MockOptions::default()).await;
    let resp = raw.get(urls::live_url(&bare.creds("demo", "x"), 1, LiveExt::Ts)).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let limited = mock_xtream::start(MockOptions { auth: MockAuth::Http403, ..Default::default() }).await;
    let resp = raw.get(urls::live_url(&limited.creds("demo", "x"), 1, LiveExt::Ts)).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// No error Display/Debug produced by the Xtream client or downloads contains the password,
/// across every failure path we can provoke (unique secret; never relies on `register_secret`).
#[tokio::test]
async fn errors_never_contain_the_password() {
    let pw = "Unique-Secret-xt-9QpW/@";
    let cancel = CancellationToken::new();
    let mut errors: Vec<Error> = Vec::new();

    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let dead = XtreamCredentials { server: closed, username: "someuser".into(), password: Secret::new(pw.into()) };
    errors.push(client(dead.clone()).authenticate(NOW, &cancel).await.unwrap_err());
    errors.push(client(dead.clone()).live_streams(&cancel).await.unwrap_err());

    let dir = tempfile::tempdir().unwrap();
    for auth in [MockAuth::Rejected, MockAuth::Expired, MockAuth::Http403] {
        let mock = mock_xtream::start(MockOptions { auth, ..Default::default() }).await;
        let c = client(mock.creds("someuser", pw));
        errors.push(c.authenticate(NOW, &cancel).await.unwrap_err());
        errors.push(c.vod_streams(&cancel).await.unwrap_err());
        let req = DownloadRequest {
            url: Secret::new(urls::xmltv_url(&mock.creds("someuser", pw))),
            dest: dir.path().join("x"),
            headers: Default::default(),
            etag: None,
            last_modified: None,
        };
        // (an expired account still gets its guide from this mock)
        if let Err(e) = http().download(&req, &|_, _| {}, &cancel).await {
            errors.push(e);
        }
    }
    let html = mock_xtream::start(MockOptions { html_200: true, ..Default::default() }).await;
    errors.push(client(html.creds("html", pw)).authenticate(NOW, &cancel).await.unwrap_err());
    errors.push(client(html.creds("someuser", pw)).series(&cancel).await.unwrap_err());
    let ok = mock_xtream::start(MockOptions::default()).await;
    errors.push(client(ok.creds("someuser", pw)).vod_info(1, &cancel).await.unwrap_err());
    errors.push(http().get_json::<Vec<u8>>(&urls::player_api_url(&ok.creds("someuser", pw), None, &[]), &Default::default(), &cancel).await.unwrap_err());
    let http404 = http()
        .get_bytes(&format!("{}/nope?username=someuser&password={}", ok.base, urls::enc(pw)), &Default::default(), 10, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(http404, Error::Http { status: 404, .. }));
    errors.push(http404);

    assert!(errors.len() >= 12);
    for e in &errors {
        assert_clean(e, pw);
        assert_clean(e, "someuser");
    }
}
