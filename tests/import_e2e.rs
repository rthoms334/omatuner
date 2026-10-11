//! Import pipelines end to end: `JobManager` + real store + the in-process mock Xtream panel
//! (`support::mock_xtream`) or a small scripted panel defined here (renumbering, one failing
//! kind). Owner: WP-06. No network: every URL is 127.0.0.1 (provider guides that would point
//! elsewhere are switched off per source).

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use support::mock_xtream::{self, MockAuth, MockOptions};
use tuner::config::{Config, SharedConfig};
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager, JobOutcome};
use tuner::logging::LogBuffer;
use tuner::model::{
    AccountStatus, EpgMatchMethod, ItemKey, ItemKind, ItemRef, JobId, Source, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings,
    XtreamCredentials,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::sources::http::{HttpClient, HttpOptions};
use tuner::store::Store;
use tuner::store::library::Library;

/// Unique per test file; must never show up in any message, log line or stored error.
const PASSWORD: &str = "pw-e2e-Zq81kX";
/// The fixtures' guide is for 2026-10-09: keep "past" programmes for a century.
const KEEP_PAST_DAYS: u32 = 36_500;

/// The global log buffer (installed once per test binary; WARN lines of `tuner::jobs` are what
/// the Jobs overlay shows).
fn logs() -> &'static LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        tuner::logging::init(&tuner::paths::AppPaths::rooted(&dir), false)
    })
}

fn job_warnings() -> Vec<String> {
    logs().snapshot(tracing::Level::WARN).into_iter().filter(|l| l.target.starts_with("tuner::jobs")).map(|l| l.message).collect()
}

type Finished = (JobId, JobKind, Result<JobOutcome, String>);

struct Harness {
    _dir: tempfile::TempDir,
    jobs: JobManager,
    store: Store,
    events: crossbeam_channel::Receiver<AppEvent>,
    stash: Mutex<Vec<Finished>>,
    paths: tuner::paths::AppPaths,
}

fn harness() -> Harness {
    logs();
    tuner::redact::register_secret(PASSWORD);
    let (dir, paths) = support::temp_paths();
    let (bus, events) = EventBus::new(None);
    let store = Store::open_temp("tuner-import-e2e", Some(bus.clone())).expect("store");
    let http = HttpClient::new(&HttpOptions::default()).expect("http");
    let mut cfg = Config::default();
    cfg.general.epg_keep_past_days = KEEP_PAST_DAYS;
    let config: SharedConfig = Arc::new(RwLock::new(Arc::new(cfg)));
    let ctx = JobContext::new(store.clone(), http, bus, paths.clone(), config);
    let jobs = JobManager::new(ctx, tokio::runtime::Handle::current());
    Harness { _dir: dir, jobs, store, events, stash: Mutex::default(), paths }
}

impl Harness {
    fn drain(&self) {
        let mut stash = self.stash.lock().unwrap();
        while let Ok(ev) = self.events.try_recv() {
            if let AppEvent::Job(JobEvent::Finished { id, kind, outcome, .. }) = ev {
                stash.push((id, kind, outcome));
            }
        }
    }

    /// The next (oldest unclaimed) `Finished` event of a job matching `pred`.
    async fn finished(&self, pred: impl Fn(&JobKind) -> bool) -> (JobId, Result<JobOutcome, String>) {
        for _ in 0..6000 {
            self.drain();
            let hit = {
                let mut stash = self.stash.lock().unwrap();
                stash.iter().position(|(_, k, _)| pred(k)).map(|i| stash.remove(i))
            };
            if let Some((id, _, outcome)) = hit {
                return (id, outcome);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no matching job finished; active: {:?}", self.jobs.active());
    }

    /// Wait until no job is queued or running, then forget every unclaimed event.
    async fn settle(&self) {
        for _ in 0..6000 {
            if self.jobs.active().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if self.jobs.active().is_empty() {
                    self.drain();
                    self.stash.lock().unwrap().clear();
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("jobs never settled: {:?}", self.jobs.active());
    }

    async fn refresh(&self, source: SourceId) -> Result<JobOutcome, String> {
        self.jobs.submit(JobKind::RefreshSource { source, force: false });
        self.finished(|k| matches!(k, JobKind::RefreshSource { source: s, .. } if *s == source)).await.1
    }

    async fn library(&self) -> Arc<Library> {
        self.settle().await;
        self.jobs.submit(JobKind::RebuildLibrary);
        match self.finished(|k| *k == JobKind::RebuildLibrary).await.1 {
            Ok(JobOutcome::Library(l)) => l,
            other => panic!("{other:?}"),
        }
    }

    async fn insert(&self, draft: SourceDraft) -> SourceId {
        self.store.writer().run(move |c| tuner::store::sources::insert_source(c, &draft, 0)).await.expect("insert source")
    }

    async fn source(&self, id: SourceId) -> Source {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || tuner::store::sources::get_source(&store.reader()?, id)).await.unwrap().expect("source")
    }

    async fn update(&self, src: Source) {
        self.store.writer().run(move |c| tuner::store::sources::update_source(c, &src)).await.expect("update source");
    }

    /// First column of every row as text, on a reader (`?1` = the source).
    async fn strings(&self, sql: &'static str, source: SourceId) -> Vec<String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let c = store.reader().unwrap();
            let mut stmt = c.prepare(sql).unwrap();
            stmt.query_map([source], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
        })
        .await
        .unwrap()
    }

    async fn refresh_forced(&self, source: SourceId) -> Result<JobOutcome, String> {
        self.jobs.submit(JobKind::RefreshSource { source, force: true });
        self.finished(|k| matches!(k, JobKind::RefreshSource { source: s, .. } if *s == source)).await.1
    }

    /// `SELECT count(*)`-style scalar on a reader.
    async fn scalar(&self, sql: &'static str, source: SourceId) -> i64 {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.reader().unwrap().query_row(sql, [source], |r| r.get::<_, i64>(0)).unwrap()).await.unwrap()
    }
}

fn draft(name: &str, kind: SourceKind) -> SourceDraft {
    SourceDraft {
        name: name.into(),
        kind,
        icon: String::new(),
        color: SourceColor::Accent,
        url: None,
        path: None,
        xtream: None,
        settings: SourceSettings::default(),
    }
}

fn xtream_draft(name: &str, creds: XtreamCredentials, provider_epg: bool) -> SourceDraft {
    let mut d = draft(name, SourceKind::Xtream);
    d.xtream = Some(creds);
    d.settings.use_provider_epg = Some(provider_epg);
    d
}

#[track_caller]
fn assert_clean(text: &str) {
    assert!(!text.contains(PASSWORD), "secret leaked: {text}");
}

// ---------------------------------------------------------------------------------------------
// mock panel (support::mock_xtream)

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn adding_an_xtream_source_imports_live_vod_series_and_the_provider_guide() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let h = harness();
    h.jobs.submit(JobKind::AddSource(xtream_draft("Mock TV", mock.creds("demo", PASSWORD), true)));
    let source = match h.finished(|k| matches!(k, JobKind::AddSource(_))).await.1 {
        Ok(JobOutcome::SourceAdded(id)) => id,
        other => panic!("{other:?}"),
    };

    // provider guide: a follow-up job, imported and mapped
    let epg = match h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await.1 {
        Ok(JobOutcome::EpgRefreshed { summary, not_modified: false, .. }) => summary,
        other => panic!("{other:?}"),
    };
    assert_eq!(epg.channels, 4, "bbc1.uk, cnn.us@SD, espn + unknown.channel (auto-created by a programme)");
    assert!(epg.programmes >= 5, "{epg:?}");
    assert_eq!(epg.mapped_channels, 2, "BBC One by epg_channel_id, ESPN by name");

    let lib = h.library().await;
    assert_eq!(lib.live.channels.len(), 2);
    assert_eq!(lib.vod.movies.len(), 2);
    assert_eq!(lib.vod.series.len(), 1);
    let bbc = lib.live.channels.iter().find(|c| c.key.0 == "xt:live:1001").expect("BBC One");
    assert_eq!(&*bbc.name, "BBC One");

    // account stored with the source
    let src = h.source(source).await;
    let account = src.status.account.clone().expect("account info stored");
    assert_eq!((account.status, account.max_connections), (AccountStatus::Active, Some(2)));
    assert_eq!(src.status.counts.channels, 2);
    assert!(src.status.last_error.is_none(), "{:?}", src.status.last_error);

    // mappings: Xtream epg id for BBC, normalized name for ESPN; the guide has programmes
    let store = h.store.clone();
    let (bbc_map, espn_map, programmes) = tokio::task::spawn_blocking(move || {
        let c = store.reader().unwrap();
        let item = |k: &str| ItemRef { source, kind: ItemKind::Channel, key: ItemKey(k.into()) };
        let bbc = tuner::store::epg::mappings_for_channel(&c, &item("xt:live:1001")).unwrap();
        let espn = tuner::store::epg::mappings_for_channel(&c, &item("xt:live:1002")).unwrap();
        let ch = tuner::store::live::channel_by_ref(&c, &item("xt:live:1001")).unwrap().unwrap();
        let epg_ch = tuner::store::epg::epg_channel_for(&c, ch.id).unwrap().expect("resolved guide channel");
        let progs = tuner::store::epg::window(&c, epg_ch, 1_791_550_000, 1_791_550_000 + 86_400).unwrap();
        (bbc, espn, progs)
    })
    .await
    .unwrap();
    assert_eq!((bbc_map[0].xmltv_id.as_str(), bbc_map[0].method), ("bbc1.uk", EpgMatchMethod::XtreamEpgId));
    assert_eq!((espn_map[0].xmltv_id.as_str(), espn_map[0].method), ("espn", EpgMatchMethod::NormalizedName));
    assert!(programmes.iter().any(|p| &*p.title == "News at Eight"), "{programmes:?}");

    // a refresh reports the fresh account (the app feeds the player's connection budget)
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { source: s, summary, not_modified: false, account: Some(a), upgraded: false }) => {
            assert_eq!(s, source);
            assert_eq!(a.max_connections, Some(2));
            assert_eq!((summary.channels.total, summary.channels.added, summary.channels.updated), (2, 0, 2));
            assert_eq!((summary.movies.total, summary.series.total), (2, 1));
        }
        other => panic!("{other:?}"),
    }

    // lazy details
    let movie = lib.vod.movies.iter().find(|m| m.key.0 == "xt:vod:5001").expect("movie");
    let series = &lib.vod.series[0];
    let (tuner::model::VodKind::Movie(mid), tuner::model::VodKind::Series(sid)) = (movie.kind, series.kind) else { panic!("kinds") };
    h.jobs.submit(JobKind::LoadMovieDetails(mid));
    h.jobs.submit(JobKind::LoadSeriesDetails(sid));
    assert!(h.finished(|k| matches!(k, JobKind::LoadMovieDetails(_))).await.1.is_ok());
    assert!(h.finished(|k| matches!(k, JobKind::LoadSeriesDetails(_))).await.1.is_ok());
    let store = h.store.clone();
    let (m, eps) = tokio::task::spawn_blocking(move || {
        let c = store.reader().unwrap();
        (tuner::store::vod::movie(&c, mid).unwrap(), tuner::store::vod::episodes(&c, sid, None).unwrap())
    })
    .await
    .unwrap();
    assert_eq!(m.plot.as_deref(), Some("A hacker learns the truth."));
    assert_eq!(m.duration_s, Some(8160));
    assert!(m.details_loaded_at.is_some());
    assert_eq!(eps.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["Pilot", "Second"]);

    for line in job_warnings() {
        assert_clean(&line);
    }
    let requests = mock.recorded();
    assert!(requests.iter().any(|r| r.starts_with("/xmltv.php")), "{requests:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_failing_refresh_keeps_the_old_catalogue_and_records_a_redacted_error() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let h = harness();
    let source = h.insert(xtream_draft("Keepme TV", mock.creds("demo", PASSWORD), false)).await;
    assert!(h.refresh(source).await.is_ok());
    let before = h.source(source).await;
    assert_eq!((before.status.counts.channels, before.status.counts.movies, before.status.counts.series), (2, 2, 1));

    // every catalogue list answers an empty body: all kinds fail → the refresh fails
    let mut broken = before.clone();
    broken.xtream = Some(mock.creds("listempty", PASSWORD));
    h.update(broken).await;
    let err = h.refresh(source).await.expect_err("all kinds failed");
    assert!(err.contains("empty or invalid list"), "{err}");
    assert_clean(&err);

    let after = h.source(source).await;
    let stored = after.status.last_error.clone().expect("error recorded");
    assert_clean(&stored);
    assert_eq!(after.status.last_refresh, before.status.last_refresh, "a failed refresh is no refresh");
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 2);
    assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1", source).await, 2);
    assert_eq!(h.scalar("SELECT count(*) FROM series WHERE source_id = ?1", source).await, 1);
    assert_eq!(h.scalar("SELECT generation FROM source WHERE id = ?1", source).await, 1, "nothing finished");

    // the Jobs overlay's problem list: a WARN line under tuner::jobs naming the source
    let warnings = job_warnings();
    let line = warnings.iter().find(|l| l.contains("Couldn't refresh Keepme TV")).unwrap_or_else(|| panic!("{warnings:?}"));
    assert_clean(line);

    // `null` and `{"error":…}` replies behave the same
    for user in ["listnull", "listerror"] {
        let mut s = after.clone();
        s.xtream = Some(mock.creds(user, PASSWORD));
        h.update(s).await;
        assert!(h.refresh(source).await.is_err(), "{user}");
        assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 2, "{user}");
    }

    // a genuinely empty catalogue (`[]`) is a successful import that empties the lists
    let mut s = after.clone();
    s.xtream = Some(mock.creds("listnone", PASSWORD));
    h.update(s).await;
    assert!(h.refresh(source).await.is_ok());
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn rejected_or_expired_accounts_abort_before_touching_the_catalogue() {
    let ok = mock_xtream::start(MockOptions::default()).await;
    let expired = mock_xtream::start(MockOptions { auth: MockAuth::Expired, ..Default::default() }).await;
    let forbidden = mock_xtream::start(MockOptions { auth: MockAuth::Http403, ..Default::default() }).await;
    let h = harness();
    let source = h.insert(xtream_draft("Acct", ok.creds("demo", PASSWORD), false)).await;
    assert!(h.refresh(source).await.is_ok());
    let src = h.source(source).await;

    // (the banned panel's username is registered as a secret, so its status word is masked)
    for (mock, user, expect) in [(&expired, "demo", "account expired"), (&forbidden, "demo", "login rejected"), (&ok, "banned", "account ")] {
        let mut s = src.clone();
        s.xtream = Some(mock.creds(user, PASSWORD));
        h.update(s).await;
        let err = h.refresh(source).await.expect_err(expect);
        assert!(err.contains(expect), "{err}");
        assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 2);
        assert_eq!(h.scalar("SELECT generation FROM source WHERE id = ?1", source).await, 1);
    }
    // nothing but the login was asked of the expired panel
    assert!(expired.recorded().iter().all(|r| !r.contains("action=")), "{:?}", expired.recorded());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn m3u_url_import_uses_conditional_downloads_and_the_url_tvg_guide() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let h = harness();
    let mut d = draft("Playlist", SourceKind::M3uUrl);
    d.url = Some(format!("{}/get.php?username=demo&password={PASSWORD}&type=m3u_plus&output=ts", mock.base));
    d.settings.keep_m3u = true; // else it switches to its Xtream login
    let source = h.insert(d).await;

    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, not_modified: false, account: None, upgraded: false, .. }) => {
            assert_eq!(summary.channels.total, 2);
            assert_eq!(summary.channels.added, 2);
            assert_eq!((summary.movies.total, summary.series.total), (2, 1), "the playlist's movies and series");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(mock.count("/player_api.php"), 0, "a kept playlist never tries its login");
    // url-tvg → provider guide (xmltv.php) imported as a follow-up
    match h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await.1 {
        Ok(JobOutcome::EpgRefreshed { summary, .. }) => assert!(summary.programmes > 0 && summary.mapped_channels == 2, "{summary:?}"),
        other => panic!("{other:?}"),
    }
    h.settle().await;
    let src = h.source(source).await;
    assert_eq!(src.status.etag.as_deref(), Some(mock_xtream::MOCK_ETAG));

    // second refresh: 304 → nothing re-imported, catalogue intact
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { not_modified: true, .. }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 2);
    // the guide is fresh: no second guide download was submitted
    h.settle().await;
    let guide_downloads = mock.recorded().iter().filter(|r| r.starts_with("/xmltv.php")).count();
    assert_eq!(guide_downloads, 1);
    // an unforced guide refresh gets a 304 and keeps the stored guide
    let epg = h.scalar("SELECT id FROM epg_source WHERE playlist_source_id = ?1", source).await;
    h.jobs.submit(JobKind::RefreshEpg { epg_source: tuner::model::EpgSourceId(epg), force: false });
    match h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await.1 {
        Ok(JobOutcome::EpgRefreshed { not_modified: true, summary, .. }) => assert!(summary.programmes > 0),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn m3u_file_import() {
    let h = harness();
    let mut d = draft("Edge cases", SourceKind::M3uFile);
    d.path = Some(support::fixture("m3u/edge_cases.m3u"));
    d.settings.use_provider_epg = Some(false); // its url-tvg points at the internet
    let source = h.insert(d).await;
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, not_modified: false, .. }) => assert_eq!(summary.channels.total, 9),
        other => panic!("{other:?}"),
    }
    let lib = h.library().await;
    let names: Vec<&str> = lib.live.channels.iter().map(|c| &*c.name).collect();
    assert!(names.contains(&"BBC One"), "{names:?}");
    assert!(names.contains(&"Radio One"), "{names:?}");
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1 AND unsupported IS NOT NULL", source).await, 1, "DRM entry flagged");
    assert_eq!(h.scalar("SELECT count(*) FROM epg_source WHERE playlist_source_id = ?1", source).await, 0);
    assert!(h.scalar("SELECT count(*) FROM grp WHERE source_id = ?1", source).await >= 5);
    // the file is read in place, never deleted
    assert!(support::fixture("m3u/edge_cases.m3u").exists());

    // a web page instead of a playlist fails without touching the channels
    let dir = tempfile::tempdir().unwrap();
    let html = dir.path().join("expired.m3u");
    std::fs::write(&html, std::fs::read(support::fixture("m3u/html_error.m3u")).unwrap()).unwrap();
    let mut src = h.source(source).await;
    src.path = Some(html);
    h.update(src).await;
    let err = h.refresh(source).await.expect_err("html");
    assert!(err.contains("web page"), "{err}");
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 9);
}

// ---------------------------------------------------------------------------------------------
// scripted panel: renumbering, one failing kind

#[derive(Default)]
struct Panel {
    /// Added to every live stream id (a panel migration renumbers streams).
    live_offset: AtomicI64,
    /// `get_vod_streams` answers an empty body.
    vod_broken: AtomicBool,
    /// Status of `get_live_streams` (0 = 200 with data).
    live_status: AtomicU16,
    /// The API answers 403 to any other User-Agent (a provider's player filter).
    required_ua: Mutex<Option<String>>,
}

async fn start_panel(panel: Arc<Panel>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(move |uri: Uri, headers: axum::http::HeaderMap| {
        let panel = panel.clone();
        async move {
            let required = panel.required_ua.lock().unwrap().clone();
            let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
            match required {
                Some(r) if uri.path() == "/player_api.php" && ua != Some(r.as_str()) => (StatusCode::FORBIDDEN, "").into_response(),
                _ => panel_reply(&panel, &uri),
            }
        }
    });
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    (base, tx)
}

fn fixture_json(rel: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(support::fixture(rel)).unwrap()).unwrap()
}

fn json(v: &serde_json::Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(v).unwrap()).into_response()
}

fn panel_reply(p: &Panel, uri: &Uri) -> Response {
    let file = |rel: &str| std::fs::read(support::fixture(rel)).unwrap().into_response();
    match uri.path() {
        "/player_api.php" => {}
        "/list.m3u" => return file("m3u/edge_cases.m3u"),
        "/page.m3u" => return file("m3u/html_error.m3u"),
        "/stream.m3u8" => return file("m3u/hls_media.m3u8"),
        _ => return (StatusCode::NOT_FOUND, "").into_response(),
    }
    let q: HashMap<String, String> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    match q.get("action").map(String::as_str) {
        None => json(&fixture_json("xtream/auth_ok.json")),
        Some("get_live_categories") => json(&fixture_json("xtream/live_categories.json")),
        Some("get_live_streams") => match p.live_status.load(Ordering::SeqCst) {
            0 => {
                let offset = p.live_offset.load(Ordering::SeqCst);
                let mut v = fixture_json("xtream/live_streams.json");
                for s in v.as_array_mut().unwrap() {
                    let id = match &s["stream_id"] {
                        serde_json::Value::String(t) => t.parse::<i64>().ok(),
                        n => n.as_i64(),
                    };
                    if let Some(id) = id {
                        s["stream_id"] = (id + offset).into();
                    }
                }
                json(&v)
            }
            status => (StatusCode::from_u16(status).unwrap(), "").into_response(),
        },
        Some("get_vod_categories") => json(&fixture_json("xtream/vod_categories.json")),
        Some("get_vod_streams") if p.vod_broken.load(Ordering::SeqCst) => ([(header::CONTENT_TYPE, "application/json")], "").into_response(),
        Some("get_vod_streams") => json(&fixture_json("xtream/vod_streams.json")),
        Some("get_series_categories") => json(&fixture_json("xtream/series_categories.json")),
        Some("get_series") => json(&fixture_json("xtream/series.json")),
        Some(_) => json(&serde_json::json!([])),
    }
}

fn panel_creds(base: &str) -> XtreamCredentials {
    XtreamCredentials { server: base.to_owned(), username: "demo".into(), password: PASSWORD.to_owned().into() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn renumbered_stream_ids_keep_favourites() {
    let panel = Arc::new(Panel::default());
    let (base, _stop) = start_panel(panel.clone()).await;
    let h = harness();
    let source = h.insert(xtream_draft("Renumber", panel_creds(&base), false)).await;
    assert!(h.refresh(source).await.is_ok());

    let item = |k: &str| ItemRef { source, kind: ItemKind::Channel, key: ItemKey(k.into()) };
    let (bbc, espn) = (item("xt:live:1001"), item("xt:live:1002"));
    let (b2, e2) = (bbc.clone(), espn.clone());
    let list = h
        .store
        .writer()
        .run(move |c| {
            let list = tuner::store::user::default_fav_list(c)?;
            tuner::store::user::set_favourite(c, list, &b2, true, 1)?;
            tuner::store::user::set_favourite(c, list, &e2, true, 2)?;
            Ok(list)
        })
        .await
        .unwrap();
    let ids_before = h.scalar("SELECT max(id) FROM channel WHERE source_id = ?1", source).await;

    panel.live_offset.store(5000, Ordering::SeqCst);
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, .. }) => {
            assert_eq!((summary.channels.added, summary.channels.removed, summary.channels.total), (2, 2, 2));
            assert_eq!(summary.reattached, 2, "{summary:?}");
            assert_eq!(summary.orphaned, 0);
        }
        other => panic!("{other:?}"),
    }
    assert!(h.scalar("SELECT min(id) FROM channel WHERE source_id = ?1", source).await > ids_before);
    let store = h.store.clone();
    let favs = tokio::task::spawn_blocking(move || tuner::store::user::fav_items(&store.reader().unwrap(), list).unwrap()).await.unwrap();
    let keys: Vec<&str> = favs.iter().map(|f| f.item.key.0.as_str()).collect();
    assert_eq!(keys, ["xt:live:6001", "xt:live:6002"]);
    assert!(favs.iter().all(|f| f.orphaned_since.is_none()), "{favs:?}");
    let lib = h.library().await;
    let fav_ids: Vec<&str> = lib.favs.items(list).iter().map(|i| i.key.0.as_str()).collect();
    assert_eq!(fav_ids, ["xt:live:6001", "xt:live:6002"]);
    assert!(lib.live.by_ref(&lib.favs.items(list)[0]).is_some(), "the favourite points at a live channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn one_failing_kind_keeps_its_rows_while_the_others_import() {
    let panel = Arc::new(Panel::default());
    let (base, _stop) = start_panel(panel.clone()).await;
    let h = harness();
    let source = h.insert(xtream_draft("Partial", panel_creds(&base), false)).await;
    assert!(h.refresh(source).await.is_ok());
    assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1", source).await, 2);

    // VOD list answers an empty body (WP-04: BadPayload), the live streams are renumbered
    panel.vod_broken.store(true, Ordering::SeqCst);
    panel.live_offset.store(100, Ordering::SeqCst);
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, .. }) => {
            assert_eq!(summary.channels.removed, 2, "channels completed and were swept");
            assert_eq!(summary.movies.total, 2, "movies kept");
            assert_eq!(summary.movies.removed, 0);
            assert_eq!(summary.series.total, 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1 AND key = 'xt:live:1101'", source).await, 1);
    assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1", source).await, 2);
    let src = h.source(source).await;
    let note = src.status.last_error.clone().expect("per-kind error recorded");
    assert!(note.starts_with("Movies: "), "{note}");
    assert_clean(&note);
    assert!(src.status.last_refresh.is_some());
    let warnings = job_warnings();
    assert!(warnings.iter().any(|l| l.contains("Couldn't import the movies of Partial")), "{warnings:?}");

    // a 403 on the live list: that kind fails with an explanation, VOD imports again
    panel.vod_broken.store(false, Ordering::SeqCst);
    panel.live_status.store(403, Ordering::SeqCst);
    assert!(h.refresh(source).await.is_ok());
    let note = h.source(source).await.status.last_error.expect("live failure recorded");
    assert!(note.starts_with("Channels: access denied") && note.contains("HTTP 403"), "{note}");
    assert_eq!(h.scalar("SELECT count(*) FROM channel WHERE source_id = ?1", source).await, 2, "channels kept");

    // a 401 on a list action means the login no longer works: the whole refresh fails
    panel.live_status.store(401, Ordering::SeqCst);
    let err = h.refresh(source).await.expect_err("401");
    assert!(err.contains("login rejected"), "{err}");
    assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1", source).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_onboarding_retry_saves_the_corrected_draft() {
    const UA: &str = "Tuner-e2e-player/2";
    let panel = Arc::new(Panel::default());
    *panel.required_ua.lock().unwrap() = Some(UA.into());
    let (base, _stop) = start_panel(panel).await;
    let h = harness();

    // first attempt: the provider refuses the default User-Agent; the source stays saved
    h.jobs.submit(JobKind::AddSource(xtream_draft("Picky", panel_creds(&base), false)));
    let err = h.finished(|k| matches!(k, JobKind::AddSource(_))).await.1.expect_err("UA filtered");
    assert_clean(&err);
    let saved = h.scalar("SELECT max(id) FROM source WHERE ?1 = ?1", SourceId(0)).await;
    // as a restored source waiting for its credentials is
    h.store.writer().run(move |c| Ok(c.execute("UPDATE source SET enabled = 0 WHERE id = ?1", [saved])?)).await.unwrap();

    // the retry names the User-Agent (and a new name): same row, updated, enabled, imported
    let mut retry = xtream_draft("Picky TV", panel_creds(&base), false);
    retry.settings.user_agent = Some(UA.into());
    h.jobs.submit(JobKind::AddSource(retry));
    let id = match h.finished(|k| matches!(k, JobKind::AddSource(_))).await.1 {
        Ok(JobOutcome::SourceAdded(id)) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(id.0, saved, "the source saved by the first attempt is reused");
    assert_eq!(h.scalar("SELECT count(*) FROM source WHERE ?1 = ?1", id).await, 1);
    let src = h.source(id).await;
    assert_eq!(src.name, "Picky TV");
    assert_eq!(src.settings.user_agent.as_deref(), Some(UA));
    assert!(src.enabled, "entering the credentials enables the source");
    assert_eq!(src.status.counts.channels, 2);
    assert!(src.status.last_refresh.is_some() && src.status.last_error.is_none(), "{:?}", src.status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn probing_upgrades_xtream_urls_and_classifies_playlists() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let panel = Arc::new(Panel::default());
    let (base, _stop) = start_panel(panel).await;
    let h = harness();
    let probe = |url: String| {
        let mut d = draft("Probe", SourceKind::M3uUrl);
        d.url = Some(url);
        JobKind::ProbeSource(d)
    };

    // a pasted get.php URL is an Xtream login
    h.jobs.submit(probe(format!("{}/get.php?username=demo&password={PASSWORD}&type=m3u_plus", mock.base)));
    match h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1 {
        Ok(JobOutcome::Probe(p)) => {
            assert_eq!(p.kind, SourceKind::Xtream);
            assert_eq!(p.xtream.as_ref().map(|x| x.username.as_str()), Some("demo"));
            assert_eq!(p.account.as_ref().and_then(|a| a.max_connections), Some(2));
            assert_eq!(p.estimate_channels, Some(3), "live categories");
            assert_eq!(p.suggested_name.as_deref(), Some("127.0.0.1"));
            assert!(p.epg_urls.is_empty());
            assert_eq!(p.upgraded_from, Some(SourceKind::M3uUrl));
        }
        other => panic!("{other:?}"),
    }

    // a playlist whose entries are one account's stream URLs is upgraded too...
    let entry_shaped = format!("{}/playlist.m3u?username=demo&password={PASSWORD}", mock.base);
    h.jobs.submit(probe(entry_shaped.clone()));
    match h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1 {
        Ok(JobOutcome::Probe(p)) => {
            assert_eq!((p.kind, p.upgraded_from), (SourceKind::Xtream, Some(SourceKind::M3uUrl)));
            assert_eq!(p.xtream.map(|x| x.server), Some(mock.base.clone()));
        }
        other => panic!("{other:?}"),
    }
    // ...unless the draft keeps the playlist; a login that fails never fails the probe
    let mut kept = draft("Probe", SourceKind::M3uUrl);
    kept.url = Some(entry_shaped);
    kept.settings.keep_m3u = true;
    h.jobs.submit(JobKind::ProbeSource(kept));
    h.jobs.submit(probe(format!("{}/get.php?username=http401&password={PASSWORD}", mock.base)));
    for _ in 0..2 {
        match h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1 {
            Ok(JobOutcome::Probe(p)) => {
                assert_eq!((p.kind, p.upgraded_from, p.estimate_channels), (SourceKind::M3uUrl, None, Some(6)), "2 live, 2 movies, 2 episodes");
                assert!(p.xtream.is_none() && p.account.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    // a plain playlist: counted exactly, url-tvg reported, nothing left in the cache
    h.jobs.submit(probe(format!("{base}/list.m3u")));
    match h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1 {
        Ok(JobOutcome::Probe(p)) => {
            assert_eq!(p.kind, SourceKind::M3uUrl);
            assert_eq!(p.estimate_channels, Some(9));
            assert_eq!(p.epg_urls, ["http://epg.example/a.xml.gz", "http://epg.example/b.xml"]);
            assert!(p.account.is_none() && p.xtream.is_none());
        }
        other => panic!("{other:?}"),
    }

    // a web page and a single HLS stream are refused with their own copy
    h.jobs.submit(probe(format!("{base}/page.m3u")));
    let err = h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1.expect_err("html");
    assert!(err.contains("web page"), "{err}");
    h.jobs.submit(probe(format!("{base}/stream.m3u8")));
    let err = h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1.expect_err("hls");
    assert!(err.contains("single HLS stream"), "{err}");

    // rejected logins are reported, never saved
    let mut d = draft("Bad", SourceKind::Xtream);
    d.xtream = Some(mock.creds("http401", PASSWORD));
    h.jobs.submit(JobKind::ProbeSource(d));
    let err = h.finished(|k| matches!(k, JobKind::ProbeSource(_))).await.1.expect_err("401");
    assert!(err.contains("login rejected"), "{err}");
    assert_eq!(h.scalar("SELECT count(*) FROM source WHERE id >= ?1", SourceId(0)).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn maintenance_backup_and_restore_jobs() {
    let h = harness();
    let mut d = draft("Edge cases", SourceKind::M3uFile);
    d.path = Some(support::fixture("m3u/edge_cases.m3u"));
    d.settings.use_provider_epg = Some(false);
    let source = h.insert(d).await;
    assert!(h.refresh(source).await.is_ok());
    let guide = h
        .store
        .writer()
        .run(|c| {
            let kind = tuner::model::EpgSourceKind::File { path: support::fixture("xmltv/basic.xml.gz") };
            tuner::store::sources::insert_epg_source(c, "Fixture guide", &kind, 0)
        })
        .await
        .unwrap();
    h.jobs.submit(JobKind::RefreshEpg { epg_source: guide, force: false });
    match h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await.1 {
        // a local file: no download, no validators, imported in place
        Ok(JobOutcome::EpgRefreshed { summary, not_modified: false, .. }) => assert!(summary.mapped_channels >= 2, "{summary:?}"),
        other => panic!("{other:?}"),
    }
    assert!(support::fixture("xmltv/basic.xml.gz").exists());

    h.jobs.submit(JobKind::RematchEpg);
    assert!(h.finished(|k| *k == JobKind::RematchEpg).await.1.is_ok());
    h.jobs.submit(JobKind::Maintenance);
    assert!(h.finished(|k| *k == JobKind::Maintenance).await.1.is_ok());
    let store = h.store.clone();
    let stamp: Option<String> = tokio::task::spawn_blocking(move || {
        use rusqlite::OptionalExtension;
        store.reader().unwrap().query_row("SELECT value FROM meta WHERE key = 'jobs:last-maintenance'", [], |r| r.get(0)).optional().unwrap()
    })
    .await
    .unwrap();
    assert!(stamp.is_some(), "the scheduler's daily marker is stored");

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("backup.json");
    let mut cfg = Config::default();
    cfg.player.cache_secs = 44;
    std::fs::write(h.paths.config_file(), toml::to_string_pretty(&cfg).unwrap()).unwrap();
    h.jobs.submit(JobKind::ExportBackup { path: file.clone(), opts: Default::default() });
    match h.finished(|k| matches!(k, JobKind::ExportBackup { .. })).await.1 {
        Ok(JobOutcome::BackupWritten(p)) => assert_eq!(p, file),
        other => panic!("{other:?}"),
    }
    h.settle().await;
    // a Merge without "Restore settings" leaves the settings alone; with it they come back
    for settings in [false, true] {
        h.jobs.submit(JobKind::ImportBackup { path: file.clone(), mode: tuner::store::backup::RestoreMode::Merge, settings });
        match h.finished(|k| matches!(k, JobKind::ImportBackup { .. })).await.1 {
            Ok(JobOutcome::Restored(r)) => {
                assert_eq!((r.sources_added, r.sources_matched), (0, 1));
                assert_eq!(r.settings.map(|c| c.player.cache_secs), settings.then_some(44));
            }
            other => panic!("{other:?}"),
        }
        // restore → every enabled source is refreshed
        assert!(h.finished(|k| matches!(k, JobKind::RefreshSource { source: s, .. } if *s == source)).await.1.is_ok());
        h.settle().await;
    }
}

// ---------------------------------------------------------------------------------------------
// phase 5: Xtream upgrade of M3U sources, movies and series in playlists (DESIGN §27)

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_xtream_playlist_upgrades_silently_keeping_user_data_and_can_switch_back() {
    use tuner::model::{HistoryEntry, Reminder, ReminderId, ReminderState, WatchProgress};
    let mock = mock_xtream::start(MockOptions::default()).await;
    let h = harness();
    let mut d = draft("Provider list", SourceKind::M3uUrl);
    d.url = Some(format!("{}/playlist.m3u?username=demo&password={PASSWORD}", mock.base));
    d.settings.use_provider_epg = Some(false);
    d.settings.keep_m3u = true;
    let source = h.insert(d).await;

    // kept as a playlist: its movies and series come from the entries
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, upgraded: false, .. }) => {
            assert_eq!((summary.channels.total, summary.movies.total, summary.series.total), (2, 2, 1));
        }
        other => panic!("{other:?}"),
    }
    let one = |mut v: Vec<String>| {
        assert_eq!(v.len(), 1, "{v:?}");
        v.remove(0)
    };
    let bbc = one(h.strings("SELECT key FROM channel WHERE source_id = ?1 AND provider_id = '1001'", source).await);
    let matrix = one(h.strings("SELECT key FROM movie WHERE source_id = ?1 AND provider_id = '5001'", source).await);
    let pilot = one(h.strings("SELECT key FROM episode WHERE source_id = ?1 AND provider_id = '9001'", source).await);
    let item = |kind, key: &str| ItemRef { source, kind, key: ItemKey(key.into()) };
    let (b, m, p) = (item(ItemKind::Channel, &bbc), item(ItemKind::Movie, &matrix), item(ItemKind::Episode, &pilot));
    h.store
        .writer()
        .run(move |c| {
            use tuner::store::user;
            let list = user::default_fav_list(c)?;
            user::set_favourite(c, list, &b, true, 1)?;
            user::set_favourite(c, list, &m, true, 2)?;
            let progress = WatchProgress {
                item: p,
                position_s: 300.0,
                duration_s: Some(2520.0),
                updated_at: 3,
                finished: false,
                title: "Pilot".into(),
                fingerprint: Default::default(),
            };
            user::save_progress(c, &progress)?;
            user::push_history(c, &HistoryEntry { item: m, at: 4, title: "The Matrix".into() })?;
            let r = Reminder {
                id: ReminderId(0),
                channel: b,
                channel_name: "BBC One".into(),
                programme_title: "News".into(),
                start: 2_000_000_000,
                stop: 2_000_003_600,
                lead_s: 120,
                state: ReminderState::Pending,
            };
            user::add_reminder(c, &r)?;
            Ok(())
        })
        .await
        .unwrap();
    let user_data = || async {
        let mut v = h.strings("SELECT kind || ' ' || item_key FROM fav_item WHERE source_id = ?1 AND orphaned_since IS NULL", source).await;
        v.sort();
        for sql in [
            "SELECT 'progress ' || kind || ' ' || item_key FROM watch_progress WHERE source_id = ?1 AND orphaned_since IS NULL",
            "SELECT 'history ' || kind || ' ' || item_key FROM history WHERE source_id = ?1",
            "SELECT 'reminder ' || channel_key FROM reminder WHERE source_id = ?1 AND orphaned_since IS NULL",
        ] {
            v.extend(h.strings(sql, source).await);
        }
        v
    };
    let on_playlist_keys = vec![
        format!("channel {bbc}"),
        format!("movie {matrix}"),
        format!("progress episode {pilot}"),
        format!("history movie {matrix}"),
        format!("reminder {bbc}"),
    ];
    assert_eq!(user_data().await, on_playlist_keys);

    // a source from before this release: its next refresh switches to the login, the stored
    // ETag ignored once (a 304 would skip the check)
    let mut s = h.source(source).await;
    s.settings.keep_m3u = false;
    h.update(s).await;
    let stale = h.source(source).await; // a settings editor's copy from before the switch
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, upgraded: true, not_modified: false, account: Some(_), .. }) => {
            assert_eq!((summary.channels.total, summary.movies.total, summary.series.total), (2, 2, 1));
        }
        other => panic!("{other:?}"),
    }
    let src = h.source(source).await;
    assert_eq!((src.kind, src.m3u_origin()), (SourceKind::Xtream, Some(SourceKind::M3uUrl)));
    assert_eq!(src.xtream.as_ref().map(|x| x.server.as_str()), Some(mock.base.as_str()));
    assert_eq!(src.status.account.and_then(|a| a.max_connections), Some(2));
    assert_eq!(
        user_data().await,
        ["channel xt:live:1001", "movie xt:vod:5001", "progress episode xt:ep:9001", "history movie xt:vod:5001", "reminder xt:live:1001"]
    );
    let m3u_rows = "SELECT (SELECT count(*) FROM channel WHERE source_id = ?1 AND key LIKE 'm3u:%')
                         + (SELECT count(*) FROM movie WHERE source_id = ?1 AND key LIKE 'm3u:%')
                         + (SELECT count(*) FROM series WHERE source_id = ?1 AND key LIKE 'm3u:%')";
    assert_eq!(h.scalar(m3u_rows, source).await, 0);
    // noted with the switch: no window saw this refresh end, so the next one says so
    assert_eq!(h.scalar("SELECT count(*) FROM meta WHERE key = 'xtream-upgraded-unseen:' || ?1", source).await, 1);

    // saving the stale copy (no login) keeps the new login
    h.update(stale).await;
    assert!(h.source(source).await.xtream.is_some());

    // "Keep as M3U" in the source's settings: back to the playlist and its keys
    let mut s = h.source(source).await;
    s.settings.keep_m3u = true;
    h.update(s).await;
    assert!(matches!(h.refresh_forced(source).await, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })));
    let src = h.source(source).await;
    assert_eq!(src.kind, SourceKind::M3uUrl);
    assert!(src.xtream.is_some(), "the login is kept for switching again");
    assert_eq!(user_data().await, on_playlist_keys);
    assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1 AND key LIKE 'xt:%'", source).await, 0);
    let unseen = h.store.writer().run(|c| tuner::store::sources::take_unseen_upgrades(c)).await.unwrap();
    assert!(unseen.is_empty(), "back on its playlist: not announced");
    for line in job_warnings() {
        assert_clean(&line);
    }
}

/// The switch commits at the live finish; a movie list then answering 401 (fatal) or a cancel
/// must not hide the upgrade, and the user data of the kinds not imported yet moves with the
/// next refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_upgrade_stands_when_vod_fails_after_the_switch_and_the_next_refresh_moves_the_rest() {
    use tuner::model::WatchProgress;
    let opts = MockOptions::default();
    let vod_401 = opts.vod_401.clone();
    let mock = mock_xtream::start(opts).await;
    let h = harness();
    let mut d = draft("Provider list", SourceKind::M3uUrl);
    d.url = Some(format!("{}/playlist.m3u?username=demo&password={PASSWORD}", mock.base));
    d.settings.use_provider_epg = Some(false);
    d.settings.keep_m3u = true;
    let source = h.insert(d).await;
    assert!(h.refresh(source).await.is_ok());
    let pilot = h.strings("SELECT key FROM episode WHERE source_id = ?1 AND provider_id = '9001'", source).await.remove(0);
    let p = ItemRef { source, kind: ItemKind::Episode, key: ItemKey(pilot.clone()) };
    h.store
        .writer()
        .run(move |c| {
            let progress = WatchProgress {
                item: p,
                position_s: 300.0,
                duration_s: Some(2520.0),
                updated_at: 3,
                finished: false,
                title: "Pilot".into(),
                fingerprint: Default::default(),
            };
            tuner::store::user::save_progress(c, &progress)
        })
        .await
        .unwrap();
    let progress = || h.strings("SELECT kind || ' ' || item_key FROM watch_progress WHERE source_id = ?1 AND orphaned_since IS NULL", source);

    let mut s = h.source(source).await;
    s.settings.keep_m3u = false;
    h.update(s).await;
    vod_401.store(true, Ordering::SeqCst);
    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, upgraded: true, .. }) => assert_eq!(summary.channels.total, 2),
        other => panic!("{other:?}"),
    }
    let src = h.source(source).await;
    assert_eq!(src.kind, SourceKind::Xtream);
    assert!(src.status.last_error.as_deref().is_some_and(|e| e.contains("Movies")), "{:?}", src.status.last_error);
    assert_eq!(progress().await, [format!("episode {pilot}")], "series not imported yet: the episode's data waits");

    vod_401.store(false, Ordering::SeqCst);
    assert!(matches!(h.refresh(source).await, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })));
    assert_eq!(progress().await, ["episode xt:ep:9001"]);
    assert!(h.source(source).await.status.last_error.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_provider_file_and_a_prefixed_get_php_url_switch_to_the_api_behind_the_prefix() {
    let mock = mock_xtream::start(MockOptions { path_prefix: Some("/panel".into()), ..Default::default() }).await;
    let h = harness();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("provider.m3u");
    std::fs::write(&file, mock.playlist("demo", PASSWORD)).unwrap();
    let mut f = draft("Provider file", SourceKind::M3uFile);
    f.path = Some(file.clone());
    let mut u = draft("Provider URL", SourceKind::M3uUrl);
    u.url = Some(format!("{}/get.php?username=demo&password={PASSWORD}&type=m3u_plus&output=ts", mock.server()));
    for mut d in [f, u] {
        d.settings.use_provider_epg = Some(false);
        let kind = d.kind;
        let source = h.insert(d).await;
        match h.refresh(source).await {
            Ok(JobOutcome::SourceRefreshed { summary, upgraded: true, .. }) => {
                assert_eq!((summary.channels.total, summary.movies.total, summary.series.total), (2, 2, 1), "{kind:?}");
            }
            other => panic!("{kind:?}: {other:?}"),
        }
        let src = h.source(source).await;
        assert_eq!((src.kind, src.m3u_origin()), (SourceKind::Xtream, Some(kind)));
        assert_eq!(src.xtream.map(|x| x.server), Some(mock.server()), "the path prefix is part of the server");
        assert_eq!(h.scalar("SELECT count(*) FROM movie WHERE source_id = ?1 AND key LIKE 'xt:vod:%'", source).await, 2);
    }
    assert!(file.exists(), "a playlist file is never removed");
    assert_eq!(mock.count("/get.php"), 0, "a get.php login is tried before any download");

    // added as M3U ("Add anyway"), upgraded by its first import: onboarding says what was
    // added, so the next window has no upgrade to announce
    let mut d = draft("Added", SourceKind::M3uUrl);
    d.url = Some(format!("{}/get.php?username=demo&password={PASSWORD}", mock.server()));
    d.settings.use_provider_epg = Some(false);
    h.jobs.submit(JobKind::AddSource(d));
    let source = match h.finished(|k| matches!(k, JobKind::AddSource(_))).await.1 {
        Ok(JobOutcome::SourceAdded(id)) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(h.source(source).await.kind, SourceKind::Xtream);
    assert_eq!(h.scalar("SELECT count(*) FROM meta WHERE key = 'xtream-upgraded-unseen:' || ?1", source).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_playlist_whose_login_fails_stays_m3u_and_checks_again_weekly() {
    let mock = mock_xtream::start(MockOptions::default()).await;
    let h = harness();
    // login refused (401) / signed in but every list fails: the M3U import runs in the same job
    for (name, user) in [("Refused", "http401"), ("Broken lists", "listempty")] {
        let mut d = draft(name, SourceKind::M3uUrl);
        d.url = Some(format!("{}/get.php?username={user}&password={PASSWORD}", mock.base));
        d.settings.use_provider_epg = Some(false);
        // imported before this release: an ETag on record
        d.settings.keep_m3u = true;
        let source = h.insert(d).await;
        assert!(h.refresh(source).await.is_ok(), "{name}");
        let mut s = h.source(source).await;
        s.settings.keep_m3u = false;
        h.update(s).await;
        let logins = || mock.recorded().iter().filter(|r| r.starts_with("/player_api.php") && !r.contains("action=") && r.contains(user)).count();
        // the first check downloads the playlist in full (the old import may predate the fallback)
        match h.refresh(source).await {
            Ok(JobOutcome::SourceRefreshed { summary, upgraded: false, not_modified: false, .. }) => {
                assert_eq!((summary.channels.total, summary.movies.total, summary.series.total), (2, 2, 1), "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
        let src = h.source(source).await;
        assert_eq!(src.kind, SourceKind::M3uUrl, "{name}");
        assert!(src.status.last_error.is_none(), "{name}: {:?}", src.status.last_error);
        assert_eq!(logins(), 1, "{name}");
        assert_eq!(h.scalar("SELECT count(*) FROM meta WHERE key = 'xtream-upgrade:' || ?1", source).await, 1, "{name}");

        // within the week: a plain (conditional) refresh, no login
        assert!(matches!(h.refresh(source).await, Ok(JobOutcome::SourceRefreshed { not_modified: true, upgraded: false, .. })), "{name}");
        assert_eq!(logins(), 1, "{name}");
        // Settings ▸ Refresh tries again
        assert!(h.refresh_forced(source).await.is_ok(), "{name}");
        assert_eq!(logins(), 2, "{name}");
        assert_eq!(h.source(source).await.kind, SourceKind::M3uUrl, "{name}");
    }

    // the server couldn't answer (503) — to the login, or to every list after it: that says
    // nothing about the login, checked in an hour
    for (name, user) in [("Unavailable", "http503"), ("Busy lists", "list503")] {
        let mut d = draft(name, SourceKind::M3uUrl);
        d.url = Some(format!("{}/get.php?username={user}&password={PASSWORD}", mock.base));
        d.settings.use_provider_epg = Some(false);
        let source = h.insert(d).await;
        let logins = || mock.recorded().iter().filter(|r| r.starts_with("/player_api.php") && !r.contains("action=") && r.contains(user)).count();
        assert!(matches!(h.refresh(source).await, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })), "{name}");
        assert_eq!((logins(), h.source(source).await.kind), (1, SourceKind::M3uUrl), "{name}");
        assert!(matches!(h.refresh(source).await, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })), "{name}");
        assert_eq!(logins(), 1, "{name}: not again within the hour");
        h.store
            .writer()
            .run(move |c| {
                let sql = "UPDATE meta SET value = json_set(value, '$.at', json_extract(value, '$.at') - 3601) WHERE key = 'xtream-upgrade:' || ?1";
                Ok(c.execute(sql, [source])?)
            })
            .await
            .unwrap();
        assert!(matches!(h.refresh(source).await, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })), "{name}");
        assert_eq!(logins(), 2, "{name}: an hour later a plain refresh checks again");
    }

    let lines = logs().snapshot(tracing::Level::INFO);
    assert!(lines.iter().any(|l| l.message.contains("Couldn't use the Xtream login of Refused")), "the reason is logged");
    for l in &lines {
        assert_clean(&l.message);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn legacy_channels_of_a_kept_playlist_become_movies_and_series_with_their_user_data() {
    use tuner::model::{GroupOverride, HistoryEntry, ItemOverride};
    use tuner::sources::identity::{KeyAllocator, group_key};
    let h = harness();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("legacy.m3u");
    std::fs::write(
        &file,
        "#EXTM3U\n\
         #EXTINF:-1 group-title=\"News\",BBC One\nhttp://panel.example/live/u/pw/1001.ts\n\
         #EXTINF:-1 tvg-logo=\"http://img.example/f.jpg\" group-title=\"Adult\",Late Film (1999)\nhttp://panel.example/movie/u/pw/5001.mkv\n\
         #EXTINF:-1 tvg-logo=\"http://img.example/s.jpg\" group-title=\"Drama\",Example Show S01 E02 - Second\nhttp://panel.example/series/u/pw/9002.mp4\n\
         #EXTINF:-1 group-title=\"Drama\",Example Show S01E01\nhttp://panel.example/series/u/pw/9001.mp4\n\
         #EXTINF:-1 group-title=\"Docs\",Nature Hour\nhttp://panel.example/series/u/pw/9100.mp4\n\
         #EXTINF:-1 group-title=\"Films\",Needs a referrer\n#EXTVLCOPT:http-referrer=http://panel.example/\nhttp://panel.example/movie/u/pw/5002.mp4\n",
    )
    .unwrap();
    let mut d = draft("Legacy", SourceKind::M3uFile);
    d.path = Some(file.clone());
    d.settings.keep_m3u = true;
    d.settings.use_provider_epg = Some(false);
    let source = h.insert(d).await;

    // the database of an older Tuner: every entry was a channel, with user data on some
    let keys: Vec<String> = h
        .store
        .writer()
        .run(move |c| {
            use tuner::store::{import, user};
            let text = std::fs::read(&file).unwrap();
            let entries: Vec<_> = tuner::sources::m3u::parse(&text[..], None).unwrap().map(Result::unwrap).collect();
            let (mut keys, mut groups) = (KeyAllocator::default(), Vec::new());
            let channels: Vec<_> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| tuner::jobs::import_source::m3u_entry_to_channel(e, i as i32, true, &mut keys, &mut groups))
                .collect();
            let imp = import::begin_catalogue(c, source, 1)?;
            import::upsert_groups(c, &imp, &groups)?;
            import::write_channels(c, &imp, &channels)?;
            import::finish_catalogue(c, &imp, &[ItemKind::Channel], 1)?;
            let ch = |i: usize| ItemRef { source, kind: ItemKind::Channel, key: channels[i].key.clone() };
            let list = user::default_fav_list(c)?;
            user::set_favourite(c, list, &ch(1), true, 2)?;
            user::set_item_override(c, &ch(1), &ItemOverride { locked: true, ..Default::default() })?;
            user::push_history(c, &HistoryEntry { item: ch(2), at: 3, title: "Example Show".into() })?;
            let adult = group_key(ItemKind::Channel, "Adult");
            user::set_group_override(c, source, ItemKind::Channel, &adult, &GroupOverride { locked: true, ..Default::default() })?;
            // a first refresh that wrote its movie groups and was then cancelled (no finish)
            let cancelled = import::begin_catalogue(c, source, 4)?;
            let movies = import::ImportGroup {
                adult: false,
                kind: ItemKind::Movie,
                key: group_key(ItemKind::Movie, "Adult"),
                name: "Adult".into(),
                raw_name: None,
                provider_id: None,
                provider_order: 0,
            };
            import::upsert_groups(c, &cancelled, &[movies])?;
            Ok(channels.iter().map(|c| c.key.0.clone()).collect())
        })
        .await
        .unwrap();

    match h.refresh(source).await {
        Ok(JobOutcome::SourceRefreshed { summary, upgraded: false, .. }) => {
            // the entry with its own HTTP headers stays a channel
            assert_eq!((summary.channels.total, summary.movies.total, summary.series.total), (2, 1, 2));
            assert_eq!(summary.orphaned, 0);
        }
        other => panic!("{other:?}"),
    }
    // same keys, new kinds, user data with them
    assert_eq!(h.strings("SELECT key FROM movie WHERE source_id = ?1", source).await, [keys[1].clone()]);
    let mut episodes = h.strings("SELECT key FROM episode WHERE source_id = ?1", source).await;
    episodes.sort();
    let mut expected = vec![keys[2].clone(), keys[3].clone(), keys[4].clone()];
    expected.sort();
    assert_eq!(episodes, expected);
    assert_eq!(h.strings("SELECT kind || ' ' || item_key FROM fav_item WHERE source_id = ?1", source).await, [format!("movie {}", keys[1])]);
    assert_eq!(
        h.strings("SELECT kind || ' ' || item_key FROM item_override WHERE source_id = ?1 AND locked = 1", source).await,
        [format!("movie {}", keys[1])]
    );
    assert_eq!(h.strings("SELECT kind || ' ' || item_key FROM history WHERE source_id = ?1", source).await, [format!("episode {}", keys[2])]);
    // a locked group of channels stays locked as a group of movies, even one an unfinished
    // import created
    assert_eq!(h.scalar("SELECT locked FROM group_override WHERE source_id = ?1 AND group_key = 'movie:adult'", source).await, 1);

    // series by title, seasons and episodes from the S01E02 markers (file order without one)
    let series = h
        .strings(
            "SELECT s.name || ' |' || (SELECT group_concat(x, ',') FROM
                 (SELECT e.season || 'x' || e.episode || ':' || e.title AS x FROM episode e WHERE e.series_id = s.id ORDER BY e.season, e.episode))
             FROM series s WHERE s.source_id = ?1 ORDER BY s.provider_order",
            source,
        )
        .await;
    assert_eq!(series, ["Example Show |1x1:,1x2:Second", "Nature Hour |1x1:"]);
    assert_eq!(h.scalar("SELECT count(*) FROM series WHERE source_id = ?1 AND details_at IS NULL", source).await, 0, "nothing to fetch");
    assert_eq!(h.scalar("SELECT count(*) FROM season WHERE series_id IN (SELECT id FROM series WHERE source_id = ?1)", source).await, 2);
    // the login of its stream URLs is kept for the redactor's next start (tests/redact.rs)
    assert_eq!(h.strings("SELECT value FROM meta WHERE key = 'playlist-logins:' || ?1", source).await, [r#"[["u","pw"]]"#]);
}
