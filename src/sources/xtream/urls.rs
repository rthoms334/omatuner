//! Xtream URL builders. IMPLEMENTED IN THE SKELETON and frozen (pure, golden-specified; used by
//! WP-04's client, WP-05a/05b's catch-up and resolve, and WP-06's import). WP-04 owns `tests/xtream.rs`
//! golden tests for them.
//!
//! ```text
//! live      {base}/live/{u}/{p}/{id}.{ts|m3u8}
//! movie     {base}/movie/{u}/{p}/{id}.{ext}
//! episode   {base}/series/{u}/{p}/{episode_id}.{ext}
//! timeshift {base}/timeshift/{u}/{p}/{duration_min}/{YYYY-MM-DD:HH-MM}/{id}.{ts|m3u8}   (server-local time!)
//! xmltv     {base}/xmltv.php?username={u}&password={p}
//! m3u       {base}/get.php?username={u}&password={p}&type=m3u_plus&output={ts|m3u8}
//! ```
//! `u`/`p` are percent-encoded (everything except RFC 3986 unreserved characters), both as
//! path segments and as query values — so `@ : / ? # %` and spaces in credentials are safe.

use crate::model::{LiveFormat, XtreamCredentials};

/// Container for live/timeshift URLs after resolving `Auto` against allowed formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveExt {
    Ts,
    M3u8,
}

impl LiveExt {
    pub fn as_str(self) -> &'static str {
        match self {
            LiveExt::Ts => "ts",
            LiveExt::M3u8 => "m3u8",
        }
    }
}

/// Percent-encode everything except unreserved characters (`A-Z a-z 0-9 - . _ ~`).
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Normalize what the user typed as the server (onboarding "Server" field): add `http://`
/// when no scheme is given, drop any path/query (users paste `…/player_api.php` or
/// `…/get.php?…`), keep the port, no trailing `/`. `None` when no host can be found.
pub fn normalize_server(input: &str) -> Option<String> {
    let t = input.trim();
    let with_scheme = if t.contains("://") { t.to_owned() } else { format!("http://{t}") };
    let u = url::Url::parse(&with_scheme).ok()?;
    let host = u.host_str()?;
    if host.is_empty() {
        return None;
    }
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}", u.scheme()))
}

fn base(c: &XtreamCredentials) -> &str {
    c.server.trim_end_matches('/')
}

fn creds_path(c: &XtreamCredentials, kind: &str) -> String {
    format!("{}/{kind}/{}/{}", base(c), enc(&c.username), enc(c.password.expose()))
}

fn creds_query(c: &XtreamCredentials) -> String {
    format!("username={}&password={}", enc(&c.username), enc(c.password.expose()))
}

/// `Auto` → m3u8 when `allowed` contains it (or is empty), else ts.
pub fn pick_live_ext(pref: LiveFormat, allowed: &[String]) -> LiveExt {
    match pref {
        LiveFormat::Ts => LiveExt::Ts,
        LiveFormat::M3u8 => LiveExt::M3u8,
        LiveFormat::Auto => {
            if allowed.is_empty() || allowed.iter().any(|f| f.eq_ignore_ascii_case("m3u8")) {
                LiveExt::M3u8
            } else {
                LiveExt::Ts
            }
        }
    }
}

pub fn live_url(c: &XtreamCredentials, stream_id: i64, ext: LiveExt) -> String {
    format!("{}/{stream_id}.{}", creds_path(c, "live"), ext.as_str())
}

pub fn movie_url(c: &XtreamCredentials, stream_id: i64, ext: &str) -> String {
    format!("{}/{stream_id}.{}", creds_path(c, "movie"), enc(ext.trim_start_matches('.')))
}

pub fn episode_url(c: &XtreamCredentials, episode_id: &str, ext: &str) -> String {
    format!("{}/{}.{}", creds_path(c, "series"), enc(episode_id), enc(ext.trim_start_matches('.')))
}

/// `start_local` = programme start converted to the server timezone (caller does tz math via
/// jiff); `duration_min` rounded UP.
pub fn timeshift_url(c: &XtreamCredentials, stream_id: i64, start_local: jiff::civil::DateTime, duration_min: u32, ext: LiveExt) -> String {
    format!("{}/{duration_min}/{}/{stream_id}.{}", creds_path(c, "timeshift"), start_local.strftime("%Y-%m-%d:%H-%M"), ext.as_str())
}

pub fn xmltv_url(c: &XtreamCredentials) -> String {
    format!("{}/xmltv.php?{}", base(c), creds_query(c))
}

pub fn m3u_url(c: &XtreamCredentials, ext: LiveExt) -> String {
    format!("{}/get.php?{}&type=m3u_plus&output={}", base(c), creds_query(c), ext.as_str())
}

pub fn player_api_url(c: &XtreamCredentials, action: Option<&str>, extra: &[(&str, String)]) -> String {
    let mut s = format!("{}/player_api.php?{}", base(c), creds_query(c));
    if let Some(a) = action {
        s.push_str("&action=");
        s.push_str(&enc(a));
    }
    for (k, v) in extra {
        s.push('&');
        s.push_str(&enc(k));
        s.push('=');
        s.push_str(&enc(v));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::Secret;

    fn c() -> XtreamCredentials {
        XtreamCredentials { server: "http://h:8080/".into(), username: "u@x".into(), password: Secret::new("p:/?#".into()) }
    }

    #[test]
    fn golden_urls() {
        assert_eq!(live_url(&c(), 7, LiveExt::Ts), "http://h:8080/live/u%40x/p%3A%2F%3F%23/7.ts");
        assert_eq!(movie_url(&c(), 9, "mkv"), "http://h:8080/movie/u%40x/p%3A%2F%3F%23/9.mkv");
        assert_eq!(episode_url(&c(), "11", ".mp4"), "http://h:8080/series/u%40x/p%3A%2F%3F%23/11.mp4");
        assert_eq!(xmltv_url(&c()), "http://h:8080/xmltv.php?username=u%40x&password=p%3A%2F%3F%23");
        assert_eq!(m3u_url(&c(), LiveExt::M3u8), "http://h:8080/get.php?username=u%40x&password=p%3A%2F%3F%23&type=m3u_plus&output=m3u8");
        assert_eq!(
            player_api_url(&c(), Some("get_vod_info"), &[("vod_id", "5".into())]),
            "http://h:8080/player_api.php?username=u%40x&password=p%3A%2F%3F%23&action=get_vod_info&vod_id=5"
        );
        let dt = jiff::civil::date(2026, 10, 9).at(21, 0, 0, 0);
        assert_eq!(timeshift_url(&c(), 3, dt, 60, LiveExt::Ts), "http://h:8080/timeshift/u%40x/p%3A%2F%3F%23/60/2026-10-09:21-00/3.ts");
        assert_eq!(pick_live_ext(LiveFormat::Auto, &["ts".into()]), LiveExt::Ts);
        assert_eq!(pick_live_ext(LiveFormat::Auto, &[]), LiveExt::M3u8);
    }

    #[test]
    fn server_normalization() {
        assert_eq!(normalize_server("example.com:8080").as_deref(), Some("http://example.com:8080"));
        assert_eq!(normalize_server("https://x.tv/player_api.php?username=a").as_deref(), Some("https://x.tv"));
        assert_eq!(normalize_server("http://x.tv:80/").as_deref(), Some("http://x.tv"));
        assert_eq!(normalize_server(""), None);
    }
}
