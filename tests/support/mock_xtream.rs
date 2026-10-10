//! In-process mock Xtream panel (axum) for integration tests. Owner: WP-04.
//!
//! Serves `tests/fixtures/xtream/*.json` for every `player_api.php` action, with switches for
//! the quirks we must survive: auth failure (`auth:0`), expired account, HTML-200 error page,
//! 401/403, slow responses (timeouts), `xmltv.php` (fixtures/xmltv/basic.xml.gz), `get.php`
//! (an M3U built from live_streams.json), and `/live|movie|series|timeshift/...` paths that
//! 302-redirect to local test media (see `media.rs`) so the player can be exercised end to end.
//! Records every request (path + query, credentials included) for assertions.
//!
//! Any non-empty username/password is accepted; [`MockAuth`] decides the login outcome.
//! Login replies that need no switch are picked by USERNAME (so one server can serve them all):
//!
//! | username     | `player_api.php` without action                      |
//! |--------------|------------------------------------------------------|
//! | `html`       | HTML error page with status 200                      |
//! | `empty`      | empty body, status 200                               |
//! | `emptyarray` | `[]`                                                 |
//! | `http401`    | 401                                                  |
//! | `banned`     | `status: "Banned"`                                   |
//! | `disabled`   | `status: "Disabled"`                                 |
//!
//! With username `keyed`, `get_live_categories` answers `live_categories_keyed.json` (an object
//! keyed by id with junk entries, as some forks send). Other usernames pick what EVERY catalogue
//! list action (`get_*_categories`, `get_live_streams`, `get_vod_streams`, `get_series`) answers:
//!
//! | username    | list reply                                                      |
//! |-------------|-----------------------------------------------------------------|
//! | `listempty` | empty body, status 200 (busy / broken panel)                    |
//! | `listnull`  | `null`                                                          |
//! | `listerror` | `{"error":"rate limited"}`                                      |
//! | `listnone`  | `[]` (a genuinely empty catalogue)                              |
//! | `loginreply`| `auth_ok.json` — for every action, details included (forks that |
//! |             | answer unsupported actions with the login object)               |
//!
//! Data served:
//! * list actions: `live_categories.json`, `live_streams.json` (filtered by `category_id`),
//!   `vod_categories.json`, `vod_streams.json`, `series_categories.json`, `series.json`;
//! * `get_vod_info`: `vod_id` 5001 → `vod_info_full.json`, 5002 → `vod_info_empty_info.json`,
//!   anything else `[]`;
//! * `get_series_info`: `series_id` 700 → `series_info_object.json`, 701 →
//!   `series_info_array.json`, 703 only via the fork spelling `series=` (empty via
//!   `series_id=`), anything else `series_info_empty.json`;
//! * `get_short_epg` / `get_simple_data_table`: stream 1001 → `short_epg_b64.json`, else empty;
//! * `xmltv.php` / `get.php`: `ETag` + `Last-Modified`, answering 304 to a matching
//!   `If-None-Match` / `If-Modified-Since`. `xmltv.php` sends the gzip bytes as they are (no
//!   `Content-Encoding`), like real panels serving `.xml.gz`.
//! * `get.php` (m3u_plus style, phase 5): the live streams, then the movies of
//!   `vod_streams.json` (`/movie/…`, group = category name) and the episodes of series 700
//!   from `series_info_object.json` (`/series/…`, titled `Example Show (2020) S01 E01`, group
//!   "Drama"). `playlist.m3u?username=…&password=…` serves the same list under a name that is
//!   not `get.php` (an entry-shaped playlist URL); [`MockXtream::playlist`] returns it as text.
//! * `MockOptions::path_prefix` (`/panel`): every endpoint lives under it, as on panels behind
//!   a reverse proxy; [`MockXtream::server`] is the base with the prefix.
//!
//! With `MockAuth::Rejected` every action answers the login-failure object; with `Http403`
//! every path answers 403 (stream paths too: the "too many connections" case). `html_200`
//! applies to list/detail actions, `xmltv.php` and `get.php` — not to the login.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone, Default)]
pub struct MockOptions {
    pub auth: MockAuth,
    /// Return the HTML error page with status 200 for list actions.
    pub html_200: bool,
    /// Delay every response (ms) — timeout tests.
    pub delay_ms: u64,
    /// Where stream paths redirect (from `media::ensure_media`).
    pub media_base: Option<String>,
    /// Serve everything under this path (`/panel`); other paths answer 404.
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MockAuth {
    #[default]
    Ok,
    Rejected,
    Expired,
    Http403,
}

pub struct MockXtream {
    /// `http://127.0.0.1:<port>` — use as the Xtream server URL (with [`MockXtream::server`]
    /// when there is a path prefix).
    pub base: String,
    prefix: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for MockXtream {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// ETag / Last-Modified served by `xmltv.php` and `get.php`.
pub const MOCK_ETAG: &str = "\"tuner-mock-v1\"";
pub const MOCK_LAST_MODIFIED: &str = "Fri, 09 Oct 2026 18:00:00 GMT";

struct State {
    opts: MockOptions,
    /// Base URL with the path prefix.
    server: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Start on an ephemeral port on the current tokio runtime.
pub async fn start(opts: MockOptions) -> MockXtream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock xtream");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let prefix = opts.path_prefix.clone().unwrap_or_default().trim_end_matches('/').to_owned();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(State { opts, server: format!("{base}{prefix}"), requests: requests.clone() });
    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let state = state.clone();
        async move { handle(&state, uri, headers).await }
    });
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    MockXtream { base, prefix, requests, shutdown: Some(tx) }
}

impl MockXtream {
    /// Snapshot of the recorded requests (`/path?query`).
    pub fn recorded(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Recorded requests whose path (after the prefix) starts with `path`.
    pub fn count(&self, path: &str) -> usize {
        let full = format!("{}{path}", self.prefix);
        self.recorded().iter().filter(|r| r.starts_with(&full)).count()
    }

    /// The Xtream server URL: the base plus the path prefix.
    pub fn server(&self) -> String {
        format!("{}{}", self.base, self.prefix)
    }

    /// Credentials for this panel (any non-empty pair works; `username` picks login quirks).
    pub fn creds(&self, username: &str, password: &str) -> tuner::model::XtreamCredentials {
        tuner::model::XtreamCredentials { server: self.server(), username: username.into(), password: tuner::redact::Secret::new(password.into()) }
    }

    /// The `get.php` playlist of a login (`.ts` live streams), e.g. to save as a local file.
    pub fn playlist(&self, username: &str, password: &str) -> String {
        m3u_from_fixtures(&self.server(), username, password, "ts")
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = super::fixture(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("mock xtream fixture {}: {e}", path.display()))
}

fn json(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn html() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], fixture("xtream/html_200_error.html")).into_response()
}

const AUTH_FAIL: &str = "xtream/auth_fail.json";

async fn handle(st: &State, uri: Uri, headers: HeaderMap) -> Response {
    let path_q = uri.path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_else(|| uri.path().to_owned());
    st.requests.lock().unwrap().push(path_q);
    if st.opts.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(st.opts.delay_ms)).await;
    }
    if st.opts.auth == MockAuth::Http403 {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let q: Vec<(String, String)> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    let param = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let prefix = st.opts.path_prefix.as_deref().unwrap_or("").trim_end_matches('/');
    let Some(path) = uri.path().strip_prefix(prefix) else { return (StatusCode::NOT_FOUND, "not found").into_response() };
    if matches!(path, "/player_api.php" | "/xmltv.php" | "/get.php" | "/playlist.m3u") {
        let (Some(user), Some(pass)) = (param("username"), param("password")) else { return json(fixture(AUTH_FAIL)) };
        if user.is_empty() || pass.is_empty() || st.opts.auth == MockAuth::Rejected {
            return if path == "/player_api.php" { json(fixture(AUTH_FAIL)) } else { (StatusCode::UNAUTHORIZED, "").into_response() };
        }
        return match path {
            "/player_api.php" => player_api(st, user, &param),
            "/xmltv.php" => conditional(st, &headers, fixture("xmltv/basic.xml.gz"), "application/xml"),
            _ => {
                let ext = param("output").unwrap_or("ts");
                conditional(st, &headers, m3u_from_fixtures(&st.server, user, pass, ext).into_bytes(), "audio/x-mpegurl")
            }
        };
    }
    stream_redirect(st, path)
}

fn player_api<'a>(st: &State, user: &str, param: &impl Fn(&str) -> Option<&'a str>) -> Response {
    let Some(action) = param("action") else { return login(st, user) };
    if st.opts.auth == MockAuth::Expired {
        return json(fixture("xtream/auth_expired.json"));
    }
    if st.opts.html_200 {
        return html();
    }
    if user == "loginreply" {
        return json(fixture("xtream/auth_ok.json"));
    }
    let is_list = matches!(
        action,
        "get_live_categories" | "get_vod_categories" | "get_series_categories" | "get_live_streams" | "get_vod_streams" | "get_series"
    );
    let list_quirk: Option<&[u8]> = match user {
        "listempty" => Some(b""),
        "listnull" => Some(b"null"),
        "listerror" => Some(br#"{"error":"rate limited"}"#),
        "listnone" => Some(b"[]"),
        _ => None,
    };
    if let Some(body) = list_quirk.filter(|_| is_list) {
        return json(body.to_vec());
    }
    let id = |k: &str| param(k).and_then(|v| v.parse::<i64>().ok());
    match action {
        "get_live_categories" if user == "keyed" => json(fixture("xtream/live_categories_keyed.json")),
        "get_live_categories" => json(fixture("xtream/live_categories.json")),
        "get_vod_categories" => json(fixture("xtream/vod_categories.json")),
        "get_series_categories" => json(fixture("xtream/series_categories.json")),
        "get_live_streams" => match param("category_id") {
            Some(cat) => {
                let all: Vec<serde_json::Value> = serde_json::from_slice(&fixture("xtream/live_streams.json")).expect("live_streams.json");
                let matches = |v: &serde_json::Value| v.get("category_id").map(|c| c.to_string().trim_matches('"') == cat).unwrap_or(false);
                json(serde_json::to_vec(&all.into_iter().filter(matches).collect::<Vec<_>>()).expect("serialize"))
            }
            None => json(fixture("xtream/live_streams.json")),
        },
        "get_vod_streams" => json(fixture("xtream/vod_streams.json")),
        "get_series" => json(fixture("xtream/series.json")),
        "get_vod_info" => match id("vod_id") {
            Some(5001) => json(fixture("xtream/vod_info_full.json")),
            Some(5002) => json(fixture("xtream/vod_info_empty_info.json")),
            _ => json(b"[]".to_vec()),
        },
        "get_series_info" => match (id("series_id"), id("series")) {
            (Some(700), _) => json(fixture("xtream/series_info_object.json")),
            (Some(701), _) => json(fixture("xtream/series_info_array.json")),
            (None, Some(703)) => json(fixture("xtream/series_info_object.json")),
            _ => json(fixture("xtream/series_info_empty.json")),
        },
        "get_short_epg" | "get_simple_data_table" => match id("stream_id") {
            Some(1001) => json(fixture("xtream/short_epg_b64.json")),
            _ => json(br#"{"epg_listings":[]}"#.to_vec()),
        },
        _ => json(b"[]".to_vec()),
    }
}

fn login(st: &State, user: &str) -> Response {
    match user {
        "html" => return html(),
        "empty" => return json(Vec::new()),
        "emptyarray" => return json(b"[]".to_vec()),
        "http401" => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
        "banned" | "disabled" => {
            let status = if user == "banned" { "Banned" } else { "Disabled" };
            let body = format!(r#"{{"user_info":{{"username":"{user}","auth":1,"status":"{status}"}},"server_info":{{}}}}"#);
            return json(body.into_bytes());
        }
        _ => {}
    }
    match st.opts.auth {
        MockAuth::Expired => json(fixture("xtream/auth_expired.json")),
        _ => json(fixture("xtream/auth_ok.json")),
    }
}

/// 200 with validators, or 304 when the client already has this version.
fn conditional(st: &State, headers: &HeaderMap, body: Vec<u8>, content_type: &'static str) -> Response {
    if st.opts.html_200 {
        return html();
    }
    let h = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).map(str::to_owned);
    let etag_hit = h(header::IF_NONE_MATCH).is_some_and(|v| v.split(',').any(|t| t.trim() == MOCK_ETAG || t.trim() == "*"));
    let lm_hit = h(header::IF_NONE_MATCH).is_none() && h(header::IF_MODIFIED_SINCE).as_deref() == Some(MOCK_LAST_MODIFIED);
    if etag_hit || lm_hit {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, MOCK_ETAG)]).into_response();
    }
    ([(header::CONTENT_TYPE, content_type), (header::ETAG, MOCK_ETAG), (header::LAST_MODIFIED, MOCK_LAST_MODIFIED)], Body::from(body)).into_response()
}

/// `get.php?type=m3u_plus` equivalent of the fixtures: live streams, movies, the episodes of
/// series 700.
fn m3u_from_fixtures(base: &str, user: &str, pass: &str, ext: &str) -> String {
    use serde_json::Value;
    let enc = tuner::sources::xtream::urls::enc;
    let list = |name: &str| -> Vec<Value> { serde_json::from_slice(&fixture(&format!("xtream/{name}"))).expect(name) };
    let text = |v: &Value, k: &str| match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let category = |cats: &[Value], item: &Value| {
        let id = text(item, "category_id");
        cats.iter().find(|c| text(c, "category_id") == id).map(|c| text(c, "category_name")).unwrap_or_default()
    };
    let (u, p) = (enc(user), enc(pass));
    let mut out = format!("#EXTM3U url-tvg=\"{base}/xmltv.php?username={u}&password={p}\"\n");
    let live_cats = list("live_categories.json");
    for s in list("live_streams.json") {
        let id = text(&s, "stream_id");
        if id.is_empty() {
            continue;
        }
        let (name, group) = (text(&s, "name"), category(&live_cats, &s));
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{name}\" tvg-logo=\"{}\" group-title=\"{group}\",{name}\n{base}/live/{u}/{p}/{id}.{ext}\n",
            text(&s, "epg_channel_id"),
            text(&s, "stream_icon"),
        ));
    }
    let vod_cats = list("vod_categories.json");
    for m in list("vod_streams.json") {
        let (id, name, group) = (text(&m, "stream_id"), text(&m, "name"), category(&vod_cats, &m));
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"\" tvg-name=\"{name}\" tvg-logo=\"{}\" group-title=\"{group}\",{name}\n{base}/movie/{u}/{p}/{id}.{}\n",
            text(&m, "stream_icon"),
            text(&m, "container_extension"),
        ));
    }
    let series_cats = list("series_categories.json");
    let info: Value = serde_json::from_slice(&fixture("xtream/series_info_object.json")).expect("series_info_object.json");
    for s in list("series.json").iter().filter(|s| text(s, "series_id") == "700") {
        let (show, cover, group) = (text(s, "name"), text(s, "cover"), category(&series_cats, s));
        for e in info["episodes"].as_object().into_iter().flat_map(|seasons| seasons.values()).flat_map(|eps| eps.as_array().into_iter().flatten()) {
            let name =
                format!("{show} S{:02} E{:02}", text(e, "season").parse::<u32>().unwrap_or(1), text(e, "episode_num").parse::<u32>().unwrap_or(0));
            out.push_str(&format!(
                "#EXTINF:-1 tvg-id=\"\" tvg-name=\"{name}\" tvg-logo=\"{cover}\" group-title=\"{group}\",{name}\n{base}/series/{u}/{p}/{}.{}\n",
                text(e, "id"),
                text(e, "container_extension"),
            ));
        }
    }
    out
}

/// `/live|movie|series|timeshift/<u>/<p>/…` → 302 to the test media.
fn stream_redirect(st: &State, path: &str) -> Response {
    let kind = path.trim_start_matches('/').split('/').next().unwrap_or("");
    if !matches!(kind, "live" | "movie" | "series" | "timeshift") || path.split('/').count() < 5 {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if st.opts.auth == MockAuth::Rejected {
        return (StatusCode::UNAUTHORIZED, "").into_response();
    }
    let Some(media) = st.opts.media_base.as_deref() else { return (StatusCode::NOT_FOUND, "no test media").into_response() };
    let file = match kind {
        "movie" | "series" => "testsrc.mp4",
        _ if path.ends_with(".m3u8") => "hls/index.m3u8",
        _ => "bars.ts",
    };
    let location = format!("{}/{file}", media.trim_end_matches('/'));
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}
