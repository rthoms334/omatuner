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
    /// `http://127.0.0.1:<port>` — use as the Xtream server URL.
    pub base: String,
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
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Start on an ephemeral port on the current tokio runtime.
pub async fn start(opts: MockOptions) -> MockXtream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock xtream");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(State { opts, base: base.clone(), requests: requests.clone() });
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
    MockXtream { base, requests, shutdown: Some(tx) }
}

impl MockXtream {
    /// Snapshot of the recorded requests (`/path?query`).
    pub fn recorded(&self) -> Vec<String> { self.requests.lock().unwrap().clone() }

    /// Credentials for this panel (any non-empty pair works; `username` picks login quirks).
    pub fn creds(&self, username: &str, password: &str) -> tuner::model::XtreamCredentials {
        tuner::model::XtreamCredentials {
            server: self.base.clone(),
            username: username.into(),
            password: tuner::redact::Secret::new(password.into()),
        }
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = super::fixture(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("mock xtream fixture {}: {e}", path.display()))
}

fn json(body: Vec<u8>) -> Response { ([(header::CONTENT_TYPE, "application/json")], body).into_response() }

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
    let path = uri.path();
    if path == "/player_api.php" || path == "/xmltv.php" || path == "/get.php" {
        let (Some(user), Some(pass)) = (param("username"), param("password")) else { return json(fixture(AUTH_FAIL)) };
        if user.is_empty() || pass.is_empty() || st.opts.auth == MockAuth::Rejected {
            return if path == "/player_api.php" { json(fixture(AUTH_FAIL)) } else { (StatusCode::UNAUTHORIZED, "").into_response() };
        }
        return match path {
            "/player_api.php" => player_api(st, user, &param),
            "/xmltv.php" => conditional(st, &headers, fixture("xmltv/basic.xml.gz"), "application/xml"),
            _ => {
                let ext = param("output").unwrap_or("ts");
                conditional(st, &headers, m3u_from_fixtures(&st.base, user, pass, ext).into_bytes(), "audio/x-mpegurl")
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
                let matches =
                    |v: &serde_json::Value| v.get("category_id").map(|c| c.to_string().trim_matches('"') == cat).unwrap_or(false);
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
    (
        [(header::CONTENT_TYPE, content_type), (header::ETAG, MOCK_ETAG), (header::LAST_MODIFIED, MOCK_LAST_MODIFIED)],
        Body::from(body),
    )
        .into_response()
}

/// `get.php?type=m3u_plus` equivalent of the live fixtures.
fn m3u_from_fixtures(base: &str, user: &str, pass: &str, ext: &str) -> String {
    use serde_json::Value;
    let enc = tuner::sources::xtream::urls::enc;
    let cats: Vec<Value> = serde_json::from_slice(&fixture("xtream/live_categories.json")).expect("live_categories.json");
    let streams: Vec<Value> = serde_json::from_slice(&fixture("xtream/live_streams.json")).expect("live_streams.json");
    let text = |v: &Value, k: &str| match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let mut out = format!("#EXTM3U url-tvg=\"{base}/xmltv.php?username={}&password={}\"\n", enc(user), enc(pass));
    for s in &streams {
        let id = text(s, "stream_id");
        if id.is_empty() {
            continue;
        }
        let cat_id = text(s, "category_id");
        let group = cats.iter().find(|c| text(c, "category_id") == cat_id).map(|c| text(c, "category_name")).unwrap_or_default();
        let name = text(s, "name");
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{name}\" tvg-logo=\"{}\" group-title=\"{group}\",{name}\n{base}/live/{}/{}/{id}.{ext}\n",
            text(s, "epg_channel_id"),
            text(s, "stream_icon"),
            enc(user),
            enc(pass),
        ));
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
