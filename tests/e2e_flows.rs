//! Full user flows end to end over the real app services (WP-13): `Services::start` in a temp
//! profile (store, HTTP client, job manager with its follow-ups, headless mpv), fed by local
//! HTTP servers only — `support/static_http.rs` for playlists and guides, a scripted Xtream
//! panel below for renumbering. No network, no window.
//!
//! * an iptv-org style playlist (`x-tvg-url` header, `Name.cc@Feed` ids, a guide that covers
//!   only a couple of its thousands of channels) → playlist + guide imported → matching →
//!   now/next for exactly the covered channels, coverage numbers queryable;
//! * restart: an Xtream panel added through `AddSource`, services shut down and started again
//!   on the same profile keep sources (with their secrets), favourites, history, progress,
//!   overrides and settings; a refresh after the panel renumbered every stream then moves all
//!   of that user data to the new ids (the store-level reattach rules are in
//!   `store_catalogue.rs`; this checks them through the real job and library).

mod support;
#[path = "support/static_http.rs"]
mod static_http;

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use axum::http::{Uri, header};
use axum::response::{IntoResponse, Response};
use crossbeam_channel::Receiver;
use static_http::StaticServer;
use tuner::config::Config;
use tuner::jobs::{JobEvent, JobKind, JobOutcome};
use tuner::model::{
    EpgMatchMethod, EpgSourceKind, HistoryEntry, ItemKey, ItemKind, ItemRef, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings,
    Timestamp, WatchProgress, XtreamCredentials,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::services::Services;
use tuner::store::library::Library;
use tuner::ui::data::{DbEpg, FrameBudget, NowNextCache};

/// Unique per test file; must never reach a log line or stored error.
const PASSWORD: &str = "pw-e2e-flows-7Hq2";

// ---------------------------------------------------------------------------------------------
// harness: the app's services on a profile directory

type Finished = (JobKind, Result<JobOutcome, String>);

struct App {
    services: Option<Services>,
    events: Receiver<AppEvent>,
    stash: Vec<Finished>,
}

/// Profile paths under `root`; `home` too, so nothing reads the user's theme or fonts.
fn profile(root: &Path) -> AppPaths {
    let mut paths = AppPaths::rooted(root);
    paths.home = root.join("home");
    paths
}

impl App {
    /// Start the services like `main` does: the profile's saved config (defaults if none), then
    /// in-memory test overrides that are never saved (headless mpv, no session-bus or theme
    /// integrations, no launch refresh).
    fn start(root: &Path) -> Self {
        let paths = profile(root);
        let mut cfg = Config::load(&paths.config_file()).expect("config");
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        cfg.integration.mpris = false;
        cfg.integration.follow_omarchy_theme = false;
        cfg.reminders.enabled = false;
        cfg.general.refresh_on_launch_after_h = 0;
        let (bus, events) = EventBus::new(None);
        let services = Services::start(paths, cfg, tuner::logging::LogBuffer::new(1000), bus).expect("services");
        Self { services: Some(services), events, stash: Vec::new() }
    }

    fn services(&self) -> &Services { self.services.as_ref().expect("services alive") }

    fn reader(&self) -> rusqlite::Connection { self.services().store.reader().expect("reader") }

    fn write<R: Send + 'static>(&self, f: impl FnOnce(&mut rusqlite::Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
        self.services().store.writer().blocking(f).expect("write")
    }

    fn drain(&mut self, wait: Duration) {
        if let Ok(ev) = self.events.recv_timeout(wait) {
            self.keep(ev);
        }
        while let Ok(ev) = self.events.try_recv() {
            self.keep(ev);
        }
    }

    fn keep(&mut self, ev: AppEvent) {
        if let AppEvent::Job(JobEvent::Finished { kind, outcome, .. }) = ev {
            if let Err(e) = &outcome {
                assert!(!e.contains(PASSWORD), "secret leaked: {e}");
            }
            self.stash.push((kind, outcome));
        }
    }

    /// The oldest unclaimed `Finished` event of a job matching `pred` (30 s at most).
    fn finished(&mut self, what: &str, pred: impl Fn(&JobKind) -> bool) -> Result<JobOutcome, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(i) = self.stash.iter().position(|(k, _)| pred(k)) {
                return self.stash.remove(i).1;
            }
            assert!(Instant::now() < deadline, "{what}: no such job finished; active: {:?}", self.services().jobs.active());
            self.drain(Duration::from_millis(20));
        }
    }

    /// Wait until no job is queued or running, then forget every unclaimed event.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut idle_since: Option<Instant> = None;
        loop {
            self.drain(Duration::from_millis(10));
            if self.services().jobs.active().is_empty() {
                if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(100) {
                    self.stash.clear();
                    return;
                }
            } else {
                idle_since = None;
            }
            assert!(Instant::now() < deadline, "jobs never settled: {:?}", self.services().jobs.active());
        }
    }

    fn library(&mut self) -> Arc<Library> {
        self.settle();
        self.services().jobs.submit(JobKind::RebuildLibrary);
        match self.finished("library", |k| *k == JobKind::RebuildLibrary) {
            Ok(JobOutcome::Library(l)) => l,
            other => panic!("{other:?}"),
        }
    }

    /// `AddSource` (insert + refresh, as onboarding does); returns the new source id.
    fn add_source(&mut self, draft: SourceDraft) -> SourceId {
        self.services().jobs.submit(JobKind::AddSource(draft));
        match self.finished("add source", |k| matches!(k, JobKind::AddSource(_))) {
            Ok(JobOutcome::SourceAdded(id)) => id,
            other => panic!("{other:?}"),
        }
    }

    fn source(&self, id: SourceId) -> tuner::model::Source { tuner::store::sources::get_source(&self.reader(), id).expect("source") }

    /// Orderly shutdown (final writes drained, WAL checkpointed).
    fn shutdown(mut self) {
        if let Some(s) = self.services.take() {
            s.shutdown();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(s) = self.services.take() {
            s.shutdown();
        }
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

fn channel(source: SourceId, key: &str) -> ItemRef { ItemRef { source, kind: ItemKind::Channel, key: ItemKey(key.into()) } }

fn gzip(text: &str) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(text.as_bytes()).expect("gzip");
    gz.finish().expect("gzip")
}

/// XMLTV `YYYYMMDDhhmmss +0000`.
fn xmltv_time(t: Timestamp) -> String {
    jiff::Timestamp::from_second(t).expect("timestamp").to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y%m%d%H%M%S +0000").to_string()
}

// ---------------------------------------------------------------------------------------------
// (a) iptv-org style: thousands of channels, a hosted guide covering two of them

const PLAYLIST_CHANNELS: usize = 3_000;
const GUIDE_SLOTS: i64 = 52; // 26 h of 30-minute programmes

/// Shaped like `https://iptv-org.github.io/iptv/index.m3u`: CRLF, an `x-tvg-url` header,
/// `Name.cc@Feed` tvg-ids, `;` groups, `http-user-agent` + `#EXTVLCOPT`, and three Al Jazeera
/// feeds of which the guide names only one; plus one entry without a tvg-id whose name is a
/// guide display-name.
fn iptv_org_playlist(guide_url: &str) -> String {
    let mut out = format!("#EXTM3U x-tvg-url=\"{guide_url}\"\r\n");
    for i in 0..PLAYLIST_CHANNELS {
        let feed = if i % 3 == 0 { "HD" } else { "SD" };
        let groups = if i % 4 == 0 { "News;General" } else { "Entertainment" };
        let ua = if i % 25 == 0 { " http-user-agent=\"Mozilla/5.0\"" } else { "" };
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"Channel{i}.us@{feed}\" tvg-logo=\"http://127.0.0.1/logo/{i}.png\"{ua} group-title=\"{groups}\",Channel {i} (720p)\r\n"
        ));
        if !ua.is_empty() {
            out.push_str("#EXTVLCOPT:http-user-agent=Mozilla/5.0\r\n");
        }
        out.push_str(&format!("http://127.0.0.1:9/live/{i}.m3u8\r\n"));
    }
    for (id, name) in [("AlJazeera.qa@Arabic", "Al Jazeera Arabic"), ("AlJazeera.qa@English", "Al Jazeera English (1080p)"), ("AlJazeera.qa@EnglishHD", "Al Jazeera English HD")] {
        out.push_str(&format!("#EXTINF:-1 tvg-id=\"{id}\" group-title=\"News\",{name}\r\nhttp://127.0.0.1:9/aj/{id}.m3u8\r\n"));
    }
    out.push_str("#EXTINF:-1 tvg-id=\"\" group-title=\"General\",ANT1 Europe\r\nhttp://127.0.0.1:9/ant1.m3u8\r\n");
    out
}

/// Two channels (like the real hosted guide on 2026-10-10), programmes from two hours ago.
fn iptv_org_guide(now: Timestamp) -> (String, Timestamp) {
    let start = now - now.rem_euclid(1800) - 2 * 3600;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" ?><tv date=\"20261009\">\n");
    out.push_str("<channel id=\"ANT1Europe.gr@SD\"><display-name>ANT1 Europe</display-name></channel>\n");
    out.push_str("<channel id=\"AlJazeera.qa@English\"><display-name>Al Jazeera English</display-name></channel>\n");
    for id in ["AlJazeera.qa@English", "ANT1Europe.gr@SD"] {
        for slot in 0..GUIDE_SLOTS {
            let s = start + slot * 1800;
            out.push_str(&format!(
                "<programme start=\"{}\" stop=\"{}\" channel=\"{id}\"><title lang=\"en\">{id} show {slot} &amp; more</title></programme>\n",
                xmltv_time(s),
                xmltv_time(s + 1800)
            ));
        }
    }
    out.push_str("</tv>\n");
    (out, start)
}

#[test]
fn iptv_org_style_playlist_imports_its_guide_and_maps_only_the_covered_channels() {
    let now = tuner::model::now();
    let server = StaticServer::start();
    let (guide, guide_start) = iptv_org_guide(now);
    server.put("/guide.xml.gz", gzip(&guide), "application/gzip");
    server.put("/index.m3u", iptv_org_playlist(&server.url("/guide.xml.gz")), "audio/x-mpegurl");

    let root = tempfile::tempdir().expect("tempdir");
    let mut app = App::start(root.path());
    let mut d = draft("iptv-org", SourceKind::M3uUrl);
    d.url = Some(server.url("/index.m3u"));
    let source = app.add_source(d);

    // AddSource ran the first import inline
    assert_eq!(app.source(source).status.counts.channels as usize, PLAYLIST_CHANNELS + 4);
    // the x-tvg-url guide is a follow-up of the playlist refresh
    let epg = match app.finished("guide refresh", |k| matches!(k, JobKind::RefreshEpg { .. })) {
        Ok(JobOutcome::EpgRefreshed { epg_source, summary, not_modified: false }) => {
            assert_eq!((summary.channels, summary.programmes), (2, 2 * GUIDE_SLOTS as u32), "{summary:?}");
            assert_eq!(summary.mapped_channels, 4, "{summary:?}");
            epg_source
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(server.hits("/guide.xml.gz"), 1);
    let lib = app.library();

    // coverage, as Settings ▸ EPG and the mapping screen read it
    let conn = app.reader();
    let guides = tuner::store::sources::list_epg_sources(&conn).expect("guides");
    assert_eq!(guides.len(), 1, "{guides:?}");
    let g = &guides[0];
    assert_eq!(g.id, epg);
    assert!(matches!(&g.kind, EpgSourceKind::Provider { source_id, .. } if *source_id == source), "{:?}", g.kind);
    assert_eq!((g.channel_count, g.programme_count), (2, 2 * GUIDE_SLOTS as u32));
    assert_eq!(g.max_stop, Some(guide_start + GUIDE_SLOTS * 1800));
    assert!(g.last_error.is_none() && g.last_refresh.is_some(), "{g:?}");
    assert_eq!(lib.live.channels.len(), PLAYLIST_CHANNELS + 4);
    let mapped: Vec<&tuner::model::ChannelSummary> = lib.live.channels.iter().filter(|c| c.epg_channel_id.is_some()).collect();
    let mut names: Vec<&str> = mapped.iter().map(|c| &*c.name).collect();
    names.sort_unstable();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("Al Jazeera") || n.starts_with("ANT1")), "{names:?}");
    let coverage: i64 = conn
        .query_row("SELECT count(*) FROM channel_epg ce JOIN channel c ON c.id = ce.channel_id WHERE c.source_id = ?1", [source], |r| r.get(0))
        .expect("coverage");
    assert_eq!(coverage, 4);

    // how each one matched (DESIGN §9.4): exact id, @Feed stripped, unique display name
    let method = |key: &str| {
        let summary = lib.live.channels.iter().find(|c| &*c.name == key).unwrap_or_else(|| panic!("{key} missing"));
        let item = channel(source, &summary.key.0);
        let m = tuner::store::epg::mappings_for_channel(&conn, &item).expect("mappings");
        assert_eq!(m.len(), 1, "{key}: {m:?}");
        (m[0].xmltv_id.clone(), m[0].method)
    };
    let english = lib.live.channels.iter().find(|c| c.name.starts_with("Al Jazeera English") && !c.name.contains("HD")).expect("AJE");
    assert_eq!(method(&english.name), ("AlJazeera.qa@English".into(), EpgMatchMethod::TvgIdExact));
    let arabic = lib.live.channels.iter().find(|c| c.name.starts_with("Al Jazeera Arabic")).expect("AJA");
    assert_eq!(method(&arabic.name), ("AlJazeera.qa@English".into(), EpgMatchMethod::FeedStripped));
    let ant1 = lib.live.channels.iter().find(|c| c.name.starts_with("ANT1")).expect("ANT1");
    assert_eq!(method(&ant1.name), ("ANT1Europe.gr@SD".into(), EpgMatchMethod::NormalizedName));

    // now/next through the UI's cache for every mapped channel; nothing for the others
    let mut cache = NowNextCache::default();
    let mut budget = FrameBudget::new(Duration::from_secs(1));
    let mut reader = DbEpg(&conn);
    for c in &mapped {
        cache.begin_frame();
        let key = (c.epg_channel_id.expect("mapped"), c.tvg_shift_min);
        let nn = cache.get(key, now, &mut reader, &mut budget).expect("loaded").clone();
        let (cur, next) = (nn.now.expect("now"), nn.next.expect("next"));
        assert!(cur.start <= now && now < cur.stop, "{}: {cur:?}", c.name);
        assert_eq!(next.start, cur.stop);
        assert!(cur.title.contains(" show "), "{cur:?}");
    }
    let unmapped = lib.live.channels.iter().find(|c| c.epg_channel_id.is_none()).expect("an unmapped channel");
    assert!(tuner::store::epg::epg_channel_for(&conn, unmapped.id).expect("query").is_none());
    drop(conn);
    app.shutdown();
}

// ---------------------------------------------------------------------------------------------
// scripted Xtream panel: fixtures with every stream id shifted by `offset`

#[derive(Default)]
struct Panel {
    /// Added to every live and VOD stream id (a panel migration renumbers everything).
    offset: AtomicI64,
}

/// The panel on its own runtime (it must outlive a services restart).
struct PanelServer {
    base: String,
    _rt: tokio::runtime::Runtime,
}

fn start_panel(panel: Arc<Panel>) -> PanelServer {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("panel runtime");
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let panel = panel.clone();
        async move { panel_reply(&panel, &uri) }
    });
    rt.spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    PanelServer { base, _rt: rt }
}

fn fixture_json(rel: &str) -> serde_json::Value { serde_json::from_slice(&std::fs::read(support::fixture(rel)).expect("fixture")).expect("json") }

fn json(v: &serde_json::Value) -> Response { ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response() }

fn shifted(rel: &str, field: &str, offset: i64) -> serde_json::Value {
    let mut v = fixture_json(rel);
    for s in v.as_array_mut().expect("array") {
        let id = match &s[field] {
            serde_json::Value::String(t) => t.parse::<i64>().ok(),
            n => n.as_i64(),
        };
        if let Some(id) = id {
            s[field] = (id + offset).into();
        }
    }
    v
}

fn panel_reply(p: &Panel, uri: &Uri) -> Response {
    if uri.path() != "/player_api.php" {
        return (axum::http::StatusCode::NOT_FOUND, "").into_response();
    }
    let q: HashMap<String, String> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    let offset = p.offset.load(Ordering::SeqCst);
    match q.get("action").map(String::as_str) {
        None => json(&fixture_json("xtream/auth_ok.json")),
        Some("get_live_categories") => json(&fixture_json("xtream/live_categories.json")),
        Some("get_live_streams") => json(&shifted("xtream/live_streams.json", "stream_id", offset)),
        Some("get_vod_categories") => json(&fixture_json("xtream/vod_categories.json")),
        Some("get_vod_streams") => json(&shifted("xtream/vod_streams.json", "stream_id", offset)),
        Some("get_series_categories") => json(&fixture_json("xtream/series_categories.json")),
        Some("get_series") => json(&fixture_json("xtream/series.json")),
        Some(_) => json(&serde_json::json!([])),
    }
}

fn xtream(base: &str) -> SourceDraft {
    tuner::redact::register_secret(PASSWORD);
    let mut d = draft("Panel", SourceKind::Xtream);
    d.xtream = Some(XtreamCredentials { server: base.to_owned(), username: "demo".into(), password: PASSWORD.to_owned().into() });
    d.settings.use_provider_epg = Some(false); // no guide on this panel
    d
}

/// Favourite two channels and a movie, watch a channel, half-watch the movie.
fn add_user_data(app: &App, source: SourceId, movie_key: &str) {
    let (bbc, espn) = (channel(source, "xt:live:1001"), channel(source, "xt:live:1002"));
    let movie = ItemRef { source, kind: ItemKind::Movie, key: ItemKey(movie_key.into()) };
    app.write(move |c| {
        let list = tuner::store::user::default_fav_list(c)?;
        tuner::store::user::set_favourite(c, list, &bbc, true, 1)?;
        tuner::store::user::set_favourite(c, list, &espn, true, 2)?;
        tuner::store::user::set_favourite(c, list, &movie, true, 3)?;
        tuner::store::user::push_history(c, &HistoryEntry { item: espn.clone(), at: tuner::model::now() - 60, title: "ESPN".into() })?;
        let fingerprint = tuner::store::user::fingerprint_of(c, &movie)?;
        let p = WatchProgress {
            item: movie,
            position_s: 1_234.0,
            duration_s: Some(8_160.0),
            updated_at: tuner::model::now() - 30,
            finished: false,
            title: "The Matrix".into(),
            fingerprint,
        };
        tuner::store::user::save_progress(c, &p)
    });
}

/// (favourite keys in list order, most recent channel key, continue-watching keys).
fn user_data(app: &App) -> (Vec<String>, Vec<String>, Vec<(String, f64)>) {
    let conn = app.reader();
    let list = tuner::store::user::fav_lists(&conn).expect("lists").into_iter().find(|l| l.is_default).expect("default list").id;
    let favs = tuner::store::user::fav_items(&conn, list).expect("favs");
    assert!(favs.iter().all(|f| f.orphaned_since.is_none()), "{favs:?}");
    let recent = tuner::store::user::recent_channels(&conn, 10).expect("recent");
    let cw = tuner::store::user::continue_watching(&conn, 10).expect("continue");
    (
        favs.iter().map(|f| f.item.key.0.clone()).collect(),
        recent.iter().map(|h| h.item.key.0.clone()).collect(),
        cw.iter().map(|p| (p.item.key.0.clone(), p.position_s)).collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// (b) + (c) restart persistence, then a renumbered Xtream refresh on the restored profile

#[test]
fn a_restart_keeps_sources_user_data_and_settings_and_a_renumbered_refresh_moves_them() {
    let panel = Arc::new(Panel::default());
    let server = start_panel(panel.clone());
    let guide_server = StaticServer::start();
    let now = tuner::model::now();
    let (guide, _) = iptv_org_guide(now);
    guide_server.put("/guide.xml.gz", gzip(&guide), "application/gzip");
    let root = tempfile::tempdir().expect("tempdir");
    let paths = profile(root.path());

    // first run: add a panel and a guide, collect user data, change settings
    let (source, guide_id) = {
        let mut app = App::start(root.path());
        let mut d = xtream(&server.base);
        d.settings.refresh_h = Some(6);
        let source = app.add_source(d);
        let kind = EpgSourceKind::Url { url: guide_server.url("/guide.xml.gz").into() };
        let guide_id = app.write(move |c| tuner::store::sources::insert_epg_source(c, "AJ guide", &kind, 15));
        app.services().jobs.submit(JobKind::RefreshEpg { epg_source: guide_id, force: false });
        assert!(app.finished("guide", |k| matches!(k, JobKind::RefreshEpg { .. })).is_ok());
        app.settle();
        add_user_data(&app, source, "xt:vod:5001");
        let bbc = channel(source, "xt:live:1001");
        app.write(move |c| {
            let ov = tuner::model::ItemOverride { hidden: true, ..Default::default() };
            tuner::store::user::set_item_override(c, &bbc, &ov)?;
            tuner::store::user::create_fav_list(c, "Sport", "")?;
            Ok(())
        });
        // Settings edits are saved like the app saves them (whole file, atomically)
        let mut cfg = Config::load(&paths.config_file()).expect("config");
        cfg.player.zap_delay_ms = 750;
        cfg.player.audio_langs = vec!["de".into(), "en".into()];
        cfg.interface.density = tuner::config::Density::Compact;
        cfg.save(&paths.config_file()).expect("save config");
        app.shutdown();
        (source, guide_id)
    };
    // the writer checkpointed the WAL on close: everything is in the main database file
    let wal = paths.db_file().with_extension("db-wal");
    assert!(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) == 0, "WAL not checkpointed");

    // second run on the same profile
    let mut app = App::start(root.path());
    let cfg = app.services().config();
    assert_eq!(cfg.player.zap_delay_ms, 750);
    assert_eq!(cfg.player.audio_langs, ["de", "en"]);
    assert_eq!(cfg.interface.density, tuner::config::Density::Compact);

    let conn = app.reader();
    let sources = tuner::store::sources::list_sources(&conn).expect("sources");
    assert_eq!(sources.len(), 1);
    let s = &sources[0];
    assert_eq!((s.id, s.name.as_str(), s.settings.refresh_h), (source, "Panel", Some(6)));
    let creds = s.xtream.as_ref().expect("credentials");
    assert_eq!(creds.password.expose(), PASSWORD, "the secret survives the restart");
    assert!(s.status.last_refresh.is_some() && s.status.account.is_some(), "{:?}", s.status);
    let guides = tuner::store::sources::list_epg_sources(&conn).expect("guides");
    assert_eq!(guides.len(), 1);
    assert_eq!((guides[0].id, guides[0].offset_min, guides[0].programme_count), (guide_id, 15, 2 * GUIDE_SLOTS as u32));
    let lists: Vec<String> = tuner::store::user::fav_lists(&conn).expect("lists").into_iter().map(|l| l.name).collect();
    assert!(lists.contains(&"Sport".to_owned()), "{lists:?}");
    drop(conn);
    assert_eq!(
        user_data(&app),
        (
            vec!["xt:live:1001".into(), "xt:live:1002".into(), "xt:vod:5001".into()],
            vec!["xt:live:1002".into()],
            vec![("xt:vod:5001".into(), 1_234.0)]
        )
    );
    let lib = app.library();
    assert_eq!(lib.live.channels.len(), 2);
    let bbc = lib.live.by_ref(&channel(source, "xt:live:1001")).expect("BBC");
    assert!(lib.live.hidden.contains(&bbc), "the hidden override survives");
    assert_eq!(lib.vod.movies.len(), 2);

    // the panel renumbers every stream (a panel migration); the refresh on the restored
    // profile moves favourites, history, progress and overrides to the new ids
    panel.offset.store(7_000, Ordering::SeqCst);
    app.services().jobs.submit(JobKind::RefreshSource { source, force: true });
    match app.finished("renumbered refresh", |k| matches!(k, JobKind::RefreshSource { .. })) {
        Ok(JobOutcome::SourceRefreshed { summary, .. }) => {
            assert_eq!((summary.channels.added, summary.channels.removed, summary.channels.total), (2, 2, 2), "{summary:?}");
            assert_eq!((summary.movies.added, summary.movies.removed), (2, 2), "{summary:?}");
            assert_eq!(summary.orphaned, 0, "{summary:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        user_data(&app),
        (
            vec!["xt:live:8001".into(), "xt:live:8002".into(), "xt:vod:12001".into()],
            vec!["xt:live:8002".into()],
            vec![("xt:vod:12001".into(), 1_234.0)]
        )
    );
    // and the library (what Home, Live and the zap list read) resolves every one of them
    let lib = app.library();
    let fav_refs = lib.favs.items(lib.favs.default_list().expect("default list"));
    assert_eq!(fav_refs.len(), 3);
    assert!(lib.live.by_ref(&fav_refs[0]).is_some() && lib.live.by_ref(&fav_refs[1]).is_some());
    let espn = lib.live.by_ref(&channel(source, "xt:live:8002")).expect("ESPN");
    assert_eq!(lib.recent_channels, [espn]);
    let bbc = lib.live.by_ref(&channel(source, "xt:live:8001")).expect("BBC");
    assert!(lib.live.hidden.contains(&bbc), "the hidden override follows the renumbered channel");
    assert!(lib.vod.movies.iter().any(|m| m.key.0 == "xt:vod:12001"), "the favourite movie is in the library");
    app.shutdown();
}
