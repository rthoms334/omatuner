//! Is this M3U an Xtream login? (DESIGN §27.2). Owner: P5 sources track.
//!
//! Two signals, both pure (no I/O):
//! * the playlist URL is `…/get.php?username=…&password=…` (`m3u::detect_xtream_url`);
//! * the entries are Xtream stream URLs,
//!   `scheme://host[:port][/prefix]/{live|movie|series}/<user>/<pass>/<id>[.<ext>]`
//!   ([`parse_stream_url`]), and the whole playlist is one account ([`PlaylistScan`]).
//!
//! Credentials found here are registered with `redact::register_secret` by the caller before
//! anything is logged; nothing in this module logs.
//!
//! Details fixed by the implementation:
//! * Non-stream entries are allowed only on a host that also serves stream URLs. One seen
//!   before the first stream URL is held until [`PlaylistScan::finish`], but only the first
//!   [`LEADING_EXTRAS`] of them: a plain playlist (iptv-org, Free-TV) is given up after that
//!   many entries instead of being scanned to the end.
//! * The short live form `host[:port][/prefix]/<user>/<pass>/<id>[.<ext>]` proves nothing on
//!   its own (any CDN path `a/b/123.ts` has that shape), but panels often write their live
//!   section that way, ahead of the `/movie/` and `/series/` entries. An entry of that shape
//!   whose two segments are the playlist's login is one of the account's streams
//!   ([`short_live_id`]); one with any other pair is another account's (a merge) and gives up.
//!   Before the first stream URL the scan holds the one pair seen and `finish` checks it.

use url::Url;

use crate::model::{XtreamCredentials, XtreamStreamKind};

/// Entries a playlist may start with before its first stream URL (provider info clips).
pub const LEADING_EXTRAS: usize = 16;

/// One Xtream stream URL taken apart.
#[derive(Debug, Clone, PartialEq)]
pub struct XtreamStreamUrl {
    /// `server` = `scheme://host[:port][/prefix]` (host lowercased, no trailing `/`); user and
    /// password percent-decoded.
    pub creds: XtreamCredentials,
    pub kind: XtreamStreamKind,
    pub stream_id: i64,
    /// Container extension without the dot (`ts`, `m3u8`, `mkv`…), if the URL has one.
    pub ext: Option<String>,
}

/// Parse `scheme://host[:port][/prefix]/{live|movie|series}/<user>/<pass>/<id>[.<ext>]`
/// (http/https only; `<id>` all digits, > 0; the kind segment is the fourth from the end and
/// matched case-insensitively; query and fragment ignored). `timeshift/…` and the short live
/// form `host/<user>/<pass>/<id>` are NOT matched (§27.2).
pub fn parse_stream_url(url: &str) -> Option<XtreamStreamUrl> {
    let u = http_url(url)?;
    let segs: Vec<&str> = u.path_segments()?.collect();
    let [prefix @ .., kind, user, pass, last] = segs.as_slice() else { return None };
    let kind = match kind.to_ascii_lowercase().as_str() {
        "live" => XtreamStreamKind::Live,
        "movie" => XtreamStreamKind::Movie,
        "series" => XtreamStreamKind::Series,
        _ => return None,
    };
    // `timeshift/<u>/<p>/<min>/<start>/<id>` with a password spelled like a kind
    if prefix.iter().any(|s| s.eq_ignore_ascii_case("timeshift")) {
        return None;
    }
    let (stream_id, ext) = stream_id(last)?;
    let (username, password) = (percent_decode(user), percent_decode(pass));
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some(XtreamStreamUrl { creds: XtreamCredentials { server: server_url(&u, prefix)?, username, password: password.into() }, kind, stream_id, ext })
}

/// The stream id of a short live URL `scheme://host[:port][/prefix…]/<user>/<pass>/<id>[.<ext>]`
/// of `login` (its two segments before the id, percent-decoded, are exactly that login's).
pub fn short_live_id(url: &str, login: &XtreamCredentials) -> Option<i64> {
    let (user, pass, id) = parse_short_url(url)?;
    (user == login.username && pass == *login.password.expose()).then_some(id)
}

/// `(user, pass, id)` of a URL shaped like the short live form; not a stream URL itself.
fn parse_short_url(url: &str) -> Option<(String, String, i64)> {
    let u = http_url(url)?;
    let segs: Vec<&str> = u.path_segments()?.collect();
    let [prefix @ .., user, pass, last] = segs.as_slice() else { return None };
    if prefix.iter().any(|s| s.eq_ignore_ascii_case("timeshift")) {
        return None;
    }
    let (id, _) = stream_id(last)?;
    let (user, pass) = (percent_decode(user), percent_decode(pass));
    (!user.is_empty() && !pass.is_empty()).then_some((user, pass, id))
}

/// An absolute http(s) URL.
fn http_url(url: &str) -> Option<Url> {
    Url::parse(url.trim()).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// `<id>[.<ext>]`: the stem all ASCII digits, > 0; the extension lowercased, `None` if empty.
fn stream_id(last: &str) -> Option<(i64, Option<String>)> {
    let (stem, ext) = match last.split_once('.') {
        Some((stem, ext)) => (stem, Some(ext.to_ascii_lowercase()).filter(|e| !e.is_empty())),
        None => (last, None),
    };
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((stem.parse::<i64>().ok().filter(|id| *id > 0)?, ext))
}

/// `scheme://host[:port]` + the non-empty `prefix` segments (no trailing `/`). Also used by
/// `m3u::detect_xtream_url`, whose server keeps the path before `get.php`.
pub(crate) fn server_url(u: &Url, prefix: &[&str]) -> Option<String> {
    let host = u.host_str().filter(|h| !h.is_empty())?;
    let mut s = format!("{}://{host}", u.scheme());
    if let Some(port) = u.port() {
        s.push_str(&format!(":{port}"));
    }
    for seg in prefix.iter().filter(|s| !s.is_empty()) {
        s.push('/');
        s.push_str(seg);
    }
    Some(s)
}

/// `%XX` → byte (invalid escapes kept as they are); not UTF-8 → lossy.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(hi), Some(lo)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `host[:port]` of an http(s) URL (lowercased by the URL parser).
fn host_port(url: &str) -> Option<String> {
    let u = http_url(url)?;
    let host = u.host_str().filter(|h| !h.is_empty())?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

/// Whole-playlist detection (§27.2). Feed every entry URL in file order with [`push`]; stop
/// early when it returns `false`. The playlist is one Xtream account when it has at least one
/// stream URL, all of them carry the same username + password, and every other entry is
/// served by a host that also serves stream URLs, or is the short live form of that login (any
/// other short-form entry rules the playlist out). The
/// API is then tried on the most frequent `server` (first seen on ties).
///
/// [`push`]: PlaylistScan::push
#[derive(Debug, Default)]
pub struct PlaylistScan {
    /// The one login, from the first stream URL.
    login: Option<(String, String)>,
    /// Stream-URL servers with their counts, in first-seen order.
    servers: Vec<(String, u32)>,
    /// `host[:port]`s serving stream URLs.
    stream_hosts: Vec<String>,
    /// Hosts of leading non-stream entries, checked by `finish`.
    pending: Vec<String>,
    /// The (user, pass) of short-form entries before the first stream URL, checked by `finish`.
    short: Option<(String, String)>,
    failed: bool,
}

impl PlaylistScan {
    /// Account for one entry URL. `false` once the playlist cannot be one Xtream account.
    pub fn push(&mut self, url: &str) -> bool {
        if self.failed {
            return false;
        }
        self.failed = !self.accept(url);
        !self.failed
    }

    fn accept(&mut self, url: &str) -> bool {
        let Some(host) = host_port(url) else { return false };
        let Some(s) = parse_stream_url(url) else {
            if let Some((user, pass, _)) = parse_short_url(url) {
                let pair = (user, pass);
                return match self.login.as_ref().or(self.short.as_ref()) {
                    // the account's own short live form: its host serves the account
                    Some(l) if *l == pair => {
                        push_new(&mut self.stream_hosts, host);
                        true
                    }
                    Some(_) => false,
                    None => {
                        push_new(&mut self.stream_hosts, host);
                        self.short = Some(pair);
                        true
                    }
                };
            }
            if self.stream_hosts.contains(&host) {
                return true;
            }
            if self.login.is_none() && self.pending.len() < LEADING_EXTRAS {
                self.pending.push(host);
                return true;
            }
            return false;
        };
        let login = (s.creds.username, s.creds.password.into_inner());
        match &self.login {
            Some(l) if *l != login => return false,
            Some(_) => {}
            None => self.login = Some(login),
        }
        match self.servers.iter_mut().find(|(srv, _)| *srv == s.creds.server) {
            Some((_, n)) => *n += 1,
            None => self.servers.push((s.creds.server, 1)),
        }
        push_new(&mut self.stream_hosts, host);
        true
    }

    /// The login to try, if the playlist qualified.
    pub fn finish(self) -> Option<XtreamCredentials> {
        let (username, password) = self.login?;
        let short_ok = self.short.is_none_or(|(u, p)| u == username && p == password);
        if self.failed || !short_ok || self.pending.iter().any(|h| !self.stream_hosts.contains(h)) {
            return None;
        }
        // `max_by_key` keeps the last maximum: walk backwards so the first seen wins ties
        let server = self.servers.into_iter().rev().max_by_key(|(_, n)| *n)?.0;
        Some(XtreamCredentials { server, username, password: password.into() })
    }
}

fn push_new(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> Option<(String, String, String, XtreamStreamKind, i64, Option<String>)> {
        parse_stream_url(url).map(|s| (s.creds.server, s.creds.username, s.creds.password.into_inner(), s.kind, s.stream_id, s.ext))
    }

    #[test]
    fn stream_url_shapes() {
        use XtreamStreamKind::*;
        let p = |server: &str, user: &str, pass: &str, kind, id, ext: Option<&str>| {
            Some((server.to_owned(), user.to_owned(), pass.to_owned(), kind, id, ext.map(str::to_owned)))
        };
        assert_eq!(parsed("http://panel.example:8080/live/u/p/1001.ts"), p("http://panel.example:8080", "u", "p", Live, 1001, Some("ts")));
        // prefix kept, host lowercased, kind and extension case-insensitive, query/fragment ignored
        assert_eq!(
            parsed("https://CDN.Example/sub/dir/MOVIE/u/p/5001.MKV?token=x#t"),
            p("https://cdn.example/sub/dir", "u", "p", Movie, 5001, Some("mkv"))
        );
        assert_eq!(parsed("http://h/series/u/p/9001"), p("http://h", "u", "p", Series, 9001, None));
        assert_eq!(parsed("http://h/series/u/p/9001.").map(|t| t.5), Some(None));
        // percent-encoded credentials; userinfo ignored; default port dropped
        assert_eq!(parsed("http://x:y@h:80/live/u%40x/p%2Fw%25/7.m3u8"), p("http://h", "u@x", "p/w%", Live, 7, Some("m3u8")));
        // not stream URLs
        for url in [
            "http://h/timeshift/u/p/60/2026-10-09:20-00/1001.ts",
            "http://h/timeshift/u/live/60/2026-10-09:20-00/1001.ts",
            "http://h/u/p/1001.ts",
            "http://h/live/u/p/0.ts",
            "http://h/live/u/p/12a.ts",
            "http://h/live/u/p/.ts",
            "http://h/live//p/1.ts",
            "http://h/vod/u/p/1.mp4",
            "ftp://h/live/u/p/1.ts",
            "rtmp://h/live/u/p/1",
            "/local/live/u/p/1.ts",
            "http://h/live/u/p/99999999999999999999.ts",
        ] {
            assert_eq!(parsed(url), None, "{url}");
        }
    }

    fn scan(urls: &[&str]) -> (bool, Option<XtreamCredentials>) {
        let mut s = PlaylistScan::default();
        let ok = urls.iter().all(|u| s.push(u));
        (ok, s.finish())
    }

    #[test]
    fn a_playlist_is_one_account_on_its_own_hosts() {
        // one login over two servers + an extra on a stream host (before and after the streams)
        let (ok, c) = scan(&[
            "http://h1/info.mp4",
            "http://h1/live/u/p/1.ts",
            "http://h2:8080/movie/u/p/2.mkv",
            "http://h2:8080/series/u/p/3.mp4",
            "http://h1/other.ts",
        ]);
        assert!(ok);
        let c = c.expect("one account");
        assert_eq!((c.server.as_str(), c.username.as_str(), c.password.expose().as_str()), ("http://h2:8080", "u", "p"));
        // ties: the first server seen
        assert_eq!(scan(&["http://a/live/u/p/1.ts", "http://b/live/u/p/2.ts"]).1.map(|c| c.server).as_deref(), Some("http://a"));
        // a second login
        assert_eq!(scan(&["http://h/live/u/p/1.ts", "http://h/live/u2/p/2.ts"]), (false, None));
        // an entry on a foreign host (merged playlist), before or after the streams
        assert_eq!(scan(&["http://h/live/u/p/1.ts", "http://cdn.other/a.m3u8"]), (false, None));
        let (ok, c) = scan(&["http://cdn.other/a.m3u8", "http://h/live/u/p/1.ts"]);
        assert!(ok && c.is_none());
        // no stream URL at all; a plain playlist is given up after the leading entries
        assert_eq!(scan(&["http://h/a.m3u8"]), (true, None));
        let plain: Vec<String> = (0..=LEADING_EXTRAS).map(|i| format!("http://h/{i}.m3u8")).collect();
        let mut s = PlaylistScan::default();
        assert_eq!(plain.iter().position(|u| !s.push(u)), Some(LEADING_EXTRAS));
        // a local file entry is never served by the panel
        assert_eq!(scan(&["http://h/live/u/p/1.ts", "/home/me/clip.ts"]), (false, None));

        // a live section in the short form, longer than the leading extras, then the VOD
        let mut panel: Vec<String> = (1..=2 * LEADING_EXTRAS).map(|i| format!("http://h:8080/u/p/{i}.ts")).collect();
        panel.push("http://h:8080/movie/u/p/5001.mkv".into());
        panel.push("http://h:8080/u/p/99".into());
        let panel: Vec<&str> = panel.iter().map(String::as_str).collect();
        let (ok, c) = scan(&panel);
        assert!(ok);
        assert_eq!(c.map(|c| c.server).as_deref(), Some("http://h:8080"));
        // ... but only with the login of the stream URLs; a second pair gives up at once
        assert_eq!(scan(&["http://h/u/p/1.ts", "http://h/movie/u2/p2/5001.mkv"]).1, None);
        assert_eq!(scan(&["http://h/u/p/1.ts", "http://h/u2/p/2.ts"]), (false, None));
        assert_eq!(scan(&["http://h/live/u/p/1.ts", "http://h/u2/p/2.ts"]), (false, None));
        // a catch-up URL is not the short form
        assert!(scan(&["http://h/live/u/p/1.ts", "http://h/timeshift/u/p/60/2026-10-09:20-00/1.ts"]).1.is_some());
        // the short form alone proves nothing
        assert_eq!(scan(&["http://h/u/p/1.ts", "http://h/u/p/2.ts"]), (true, None));
        let login = XtreamCredentials { server: "http://h".into(), username: "u@x".into(), password: String::from("p").into() };
        assert_eq!(short_live_id("http://other/pre/u%40x/p/42.m3u8", &login), Some(42));
        assert_eq!(short_live_id("http://h/u/q/42.ts", &login), None);
    }
}
