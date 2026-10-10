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
//!   before the first stream URL is held until [`PlaylistScan::finish`], but only among the
//!   first [`LEADING_EXTRAS`] entries: a plain playlist (iptv-org, Free-TV) is given up after
//!   that many entries instead of being scanned to the end.

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
    let u = Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
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
    let (stem, ext) = match last.split_once('.') {
        Some((stem, ext)) => (stem, Some(ext.to_ascii_lowercase()).filter(|e| !e.is_empty())),
        None => (*last, None),
    };
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let stream_id = stem.parse::<i64>().ok().filter(|id| *id > 0)?;
    let (username, password) = (percent_decode(user), percent_decode(pass));
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some(XtreamStreamUrl { creds: XtreamCredentials { server: server_url(&u, prefix)?, username, password: password.into() }, kind, stream_id, ext })
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
    let u = Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str().filter(|h| !h.is_empty())?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

/// Whole-playlist detection (§27.2). Feed every entry URL in file order with [`push`]; stop
/// early when it returns `false`. The playlist is one Xtream account when it has at least one
/// stream URL, all of them carry the same username + password, and every other entry is
/// served by a host that also serves stream URLs. The API is then tried on the most frequent
/// `server` (first seen on ties).
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
    entries: usize,
    failed: bool,
}

impl PlaylistScan {
    /// Account for one entry URL. `false` once the playlist cannot be one Xtream account.
    pub fn push(&mut self, url: &str) -> bool {
        if self.failed {
            return false;
        }
        self.entries += 1;
        self.failed = !self.accept(url);
        !self.failed
    }

    fn accept(&mut self, url: &str) -> bool {
        let Some(host) = host_port(url) else { return false };
        let Some(s) = parse_stream_url(url) else {
            if self.stream_hosts.contains(&host) {
                return true;
            }
            if self.login.is_none() && self.entries <= LEADING_EXTRAS {
                if !self.pending.contains(&host) {
                    self.pending.push(host);
                }
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
        if !self.stream_hosts.contains(&host) {
            self.stream_hosts.push(host);
        }
        true
    }

    /// The login to try, if the playlist qualified.
    pub fn finish(self) -> Option<XtreamCredentials> {
        if self.failed || self.pending.iter().any(|h| !self.stream_hosts.contains(h)) {
            return None;
        }
        let (username, password) = self.login?;
        // `max_by_key` keeps the last maximum: walk backwards so the first seen wins ties
        let server = self.servers.into_iter().rev().max_by_key(|(_, n)| *n)?.0;
        Some(XtreamCredentials { server, username, password: password.into() })
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
    }
}
