//! Streaming extended-M3U parser. Owner: WP-03. Spec: DESIGN §9.1 + protocols.md §1.
//!
//! Contract:
//! * Input is a `BufRead` (already decompressed). BOM stripped; `\n`, `\r\n` and CR-only files
//!   handled; non-UTF-8 lines decoded as Windows-1252 via `encoding_rs` (lossy).
//! * Never allocates per-line regexes; attribute tokenizer is a hand-written state machine that
//!   accepts double, single and no quotes, lowercases keys, last duplicate wins, and finds the
//!   title after the first comma OUTSIDE quotes.
//! * Yields one [`M3uEntry`] per stream URL; `#EXTINF` state carries over blank lines,
//!   comments, `#EXTGRP`, `#EXTVLCOPT`, `#KODIPROP` until the URL line. Bare URL lines without
//!   `#EXTINF` produce an entry titled by the last path segment.
//! * Kodi pipe headers (`url|User-Agent=x&Referer=y`) are split off into `headers`.
//! * Relative URLs resolved against `base` (playlist URL) when given.
//! * `#EXT-X-*` before any `#EXTINF` → [`M3uError::HlsMediaPlaylist`] (caller treats the URL
//!   as a single stream).
//! * Performance target: 100k entries / 60 MB in < 1.5 s on the M2 (bench `import`).
//!
//! Details fixed by the implementation (tests in `tests/m3u.rs`):
//! * The first meaningful line decides the payload: `#EXTM3U`, `#EXTINF` or a `scheme://` URL
//!   is a playlist; anything else (including an empty payload) is `NotM3u(kind)` with `kind`
//!   from [`super::sniff_payload`], so an expired account never imports as an empty list.
//! * Iterator errors: `Error::Parse` for a malformed entry (an `#EXTINF` without URL, a URL
//!   line that is only pipe headers, a line over 1 MiB) — skip it and keep going; `Error::Io` when reading fails — the iterator ends
//!   after it and the import must be aborted (never sweep a half-read playlist).
//! * `#EXTGRP` is sticky until an empty `#EXTGRP:` (also for bare URL entries); an entry's
//!   `group-title` overrides it. `#EXTVLCOPT` / `#KODIPROP` lines apply to the next entry
//!   whether they come before or after its `#EXTINF`.
//! * Header precedence (low → high): EXTINF `http-*` attributes, `#EXTVLCOPT`, `#KODIPROP
//!   …stream_headers`, pipe headers (file order within each kind).
//! * Header defaults are inherited by entries: `tvg-shift` and catch-up (per field, see
//!   [`catchup_from_attrs`]; a header `catchup-days` / `catchup-correction` without a kind still
//!   applies to entries that name one). A repeated `#EXTM3U` (concatenated lists) adds its
//!   `url-tvg` URLs and fills header fields that are still unset.
//! * At most 256 attributes are kept per line (later new keys are ignored), so hostile lines
//!   stay linear to parse.
//! * Relative `tvg-logo` values are resolved against `base` like stream URLs (`attrs` keep the
//!   raw text).

// Implemented: opt back into the lints that `lib.rs` silences for the still-stubbed `sources`.
#![warn(dead_code, unused_variables, unused_imports)]

use std::borrow::Cow;
use std::io::{self, BufRead};

use url::Url;

use crate::error::{Error, Result};
use crate::model::{CatchupKind, CatchupSpec, HttpHeaders, XtreamCredentials};

/// Playlist-level attributes from `#EXTM3U`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct M3uHeader {
    /// `url-tvg` / `x-tvg-url` / `tvg-url`, comma-split, de-duplicated.
    pub tvg_urls: Vec<String>,
    /// Default `tvg-shift` in hours.
    pub tvg_shift_h: Option<f32>,
    /// Defaults inherited by entries without their own `catchup*` attributes.
    pub catchup: Option<CatchupSpec>,
    /// All other header attributes (lowercased keys).
    pub attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct M3uEntry {
    /// 1-based line number of the URL line (diagnostics).
    pub line: u64,
    pub title: String,
    /// All EXTINF attributes, keys lowercased, in order.
    pub attrs: Vec<(String, String)>,
    pub tvg_id: Option<String>,
    pub tvg_name: Option<String>,
    pub tvg_logo: Option<String>,
    pub tvg_chno: Option<u32>,
    /// `group-title` split on `;` (trimmed, empty removed); falls back to `#EXTGRP`.
    pub groups: Vec<String>,
    pub tvg_shift_h: Option<f32>,
    /// Entry catch-up (own attributes, else header defaults); `None` if no archive.
    pub catchup: Option<CatchupSpec>,
    /// From EXTINF `http-user-agent`/`http-referrer`, `#EXTVLCOPT`, `#KODIPROP
    /// inputstream.adaptive.stream_headers`, and pipe headers.
    pub headers: HttpHeaders,
    pub radio: bool,
    /// `#KODIPROP inputstream.adaptive.license_type` present (Widevine etc.).
    pub drm: bool,
    /// Stream URL with pipe headers removed, resolved against base.
    pub url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum M3uError {
    #[error("this is an HLS media playlist, not a channel list")]
    HlsMediaPlaylist,
    #[error("not an M3U playlist (looks like {0:?})")]
    NotM3u(super::PayloadKind),
}

/// Longest line kept in memory; longer lines are skipped and reported (`Error::Parse`).
const MAX_LINE: usize = 1 << 20;

/// Most attributes kept per `#EXTINF` / `#EXTM3U` line, and header attributes / XMLTV URLs in
/// total. Real playlists use a handful; the cap keeps hostile lines linear.
const MAX_ATTRS: usize = 256;

/// Archive window when an entry names a catch-up kind but no window at all (Kodi's default).
const DEFAULT_CATCHUP_DAYS: u32 = 5;

/// Header keys that carry XMLTV URLs.
const TVG_URL_KEYS: &[&str] = &["url-tvg", "x-tvg-url", "tvg-url"];

/// Header keys consumed into [`M3uHeader::catchup`] / `tvg_shift_h` (not repeated in `attrs`).
const CATCHUP_KEYS: &[&str] =
    &["catchup", "catchup-type", "catchup-source", "catchup-days", "catchup-correction", "timeshift", "tvg-rec", "catchup-time"];

/// An `#EXTINF` waiting for its URL line.
#[derive(Debug)]
struct PendingInf {
    line: u64,
    attrs: Vec<(String, String)>,
    title: String,
}

/// Iterator over entries. Construct with [`parse`].
pub struct M3uParser<R: BufRead> {
    reader: R,
    base: Option<Url>,
    header: M3uHeader,
    /// Header catch-up fields, also those `header.catchup` cannot show (days without a kind).
    catchup_defaults: CatchupFields,
    /// Physical lines read so far (= 1-based number of the current line).
    line_no: u64,
    bytes_read: u64,
    /// Current line (terminator removed).
    buf: Vec<u8>,
    /// Current line exceeded [`MAX_LINE`] (its tail was dropped).
    overlong: bool,
    /// Last line ended with CR: a directly following LF belongs to it.
    skip_lf: bool,
    header_seen: bool,
    pending: Option<PendingInf>,
    /// Headers from `#EXTVLCOPT` since the last entry, in file order.
    vlc_headers: Vec<(String, String)>,
    /// Headers from `#KODIPROP …_headers` since the last entry (applied after `vlc_headers`).
    kodi_headers: Vec<(String, String)>,
    directive_drm: bool,
    /// Sticky `#EXTGRP` groups.
    current_extgrp: Vec<String>,
    /// Produced during read-ahead, yielded first.
    ready: Option<Result<M3uEntry>>,
    done: bool,
}

/// Start parsing. Reads ahead to the first `#EXTINF`/URL to populate the header and to
/// classify the payload (HTML/JSON/XML → `M3uError::NotM3u`).
pub fn parse<R: BufRead>(reader: R, base: Option<&Url>) -> std::result::Result<M3uParser<R>, M3uError> {
    let mut p = M3uParser {
        reader,
        base: base.cloned(),
        header: M3uHeader::default(),
        catchup_defaults: CatchupFields::default(),
        line_no: 0,
        bytes_read: 0,
        buf: Vec::with_capacity(512),
        overlong: false,
        skip_lf: false,
        header_seen: false,
        pending: None,
        vlc_headers: Vec::new(),
        kodi_headers: Vec::new(),
        directive_drm: false,
        current_extgrp: Vec::new(),
        ready: None,
        done: false,
    };
    loop {
        match p.read_line() {
            Ok(true) => {}
            Ok(false) if p.header_seen => return Ok(p),
            Ok(false) => return Err(M3uError::NotM3u(super::PayloadKind::Unknown)),
            Err(e) => {
                // M3uError has no I/O variant: hand the failure to the first `next()`.
                p.ready = Some(Err(io_error(e)));
                p.done = true;
                return Ok(p);
            }
        }
        if p.overlong {
            if !p.header_seen {
                // e.g. a minified JSON/HTML answer: classify by its first 1 MiB.
                let first = std::mem::take(&mut p.buf);
                return Err(M3uError::NotM3u(p.sniff_rest(&first)));
            }
            continue;
        }
        let buf = std::mem::take(&mut p.buf);
        let text = decode(&buf);
        let line = clean_line(&text);
        let verdict = if line.is_empty() {
            None
        } else if starts_with_ci(line, "#EXTM3U") {
            p.merge_header(&line[7..]);
            None
        } else if starts_with_ci(line, "#EXT-X-") {
            Some(Err(M3uError::HlsMediaPlaylist))
        } else if starts_with_ci(line, "#EXTINF") {
            p.handle_line(line);
            Some(Ok(()))
        } else if line.starts_with('#') {
            // Directives/comments before the first entry: remembered, but they don't make the
            // payload a playlist on their own.
            p.handle_line(line);
            None
        } else if p.header_seen || (has_scheme(line) && line.contains("://")) {
            p.ready = p.handle_line(line);
            Some(Ok(()))
        } else {
            Some(Err(M3uError::NotM3u(p.sniff_rest(&buf))))
        };
        p.buf = buf;
        match verdict {
            None => {}
            Some(Ok(())) => return Ok(p),
            Some(Err(e)) => return Err(e),
        }
    }
}

impl<R: BufRead> M3uParser<R> {
    pub fn header(&self) -> &M3uHeader { &self.header }
    /// Bytes consumed so far (progress reporting).
    pub fn bytes_read(&self) -> u64 { self.bytes_read }

    /// Read the next physical line into `buf` (terminator removed; `\n`, `\r\n` and `\r` all
    /// end a line). `Ok(false)` at end of input.
    fn read_line(&mut self) -> io::Result<bool> {
        self.buf.clear();
        self.overlong = false;
        let mut any = false;
        loop {
            let avail = match self.reader.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if avail.is_empty() {
                if any {
                    self.line_no += 1;
                }
                return Ok(any);
            }
            if std::mem::take(&mut self.skip_lf) && avail[0] == b'\n' {
                self.reader.consume(1);
                self.bytes_read += 1;
                continue;
            }
            any = true;
            match memchr::memchr2(b'\n', b'\r', avail) {
                Some(i) => {
                    let cr = avail[i] == b'\r';
                    push_capped(&mut self.buf, &mut self.overlong, &avail[..i]);
                    self.reader.consume(i + 1);
                    self.bytes_read += i as u64 + 1;
                    self.skip_lf = cr;
                    self.line_no += 1;
                    return Ok(true);
                }
                None => {
                    let n = avail.len();
                    push_capped(&mut self.buf, &mut self.overlong, avail);
                    self.reader.consume(n);
                    self.bytes_read += n as u64;
                }
            }
        }
    }

    /// Classify a payload that is not a playlist from its first meaningful line plus whatever
    /// the reader already buffered after it (an XHTML page starts with `<?xml` and only says
    /// `<html` on a later line). Never `M3u`/`HlsMedia`: the line itself already ruled those out.
    fn sniff_rest(&mut self, first_line: &[u8]) -> super::PayloadKind {
        let mut head = first_line.to_vec();
        if let Ok(more) = self.reader.fill_buf() {
            head.push(b'\n');
            head.extend_from_slice(&more[..more.len().min(4096)]);
        }
        match super::sniff_payload(&head) {
            super::PayloadKind::M3u | super::PayloadKind::HlsMedia => super::PayloadKind::Unknown,
            k => k,
        }
    }

    /// Process one trimmed, non-empty line. Returns an item when the line completes an entry
    /// (URL line) or reveals a malformed one.
    fn handle_line(&mut self, line: &str) -> Option<Result<M3uEntry>> {
        if !line.starts_with('#') {
            return Some(self.make_entry(line));
        }
        if let Some(rest) = strip_prefix_ci(line, "#EXTINF") {
            let rest = rest.strip_prefix(':').unwrap_or(rest);
            let (attrs, title) = parse_extinf_attrs(rest);
            let prev = self.pending.replace(PendingInf { line: self.line_no, attrs, title });
            return prev.map(|p| Err(dangling(&p)));
        }
        if let Some(rest) = strip_prefix_ci(line, "#EXTM3U") {
            self.merge_header(rest);
        } else if let Some(rest) = strip_prefix_ci(line, "#EXTGRP:") {
            self.current_extgrp = split_groups(rest);
        } else if let Some(rest) = strip_prefix_ci(line, "#EXTVLCOPT:") {
            if let Some((k, v)) = rest.split_once('=') {
                let name = match k.trim().to_ascii_lowercase().as_str() {
                    "http-user-agent" => "User-Agent",
                    "http-referrer" | "http-referer" => "Referer",
                    "http-origin" => "Origin",
                    "http-cookie" => "Cookie",
                    _ => return None, // player options (network-caching, http-reconnect…)
                };
                self.vlc_headers.push((name.to_owned(), v.trim().to_owned()));
            }
        } else if let Some(rest) = strip_prefix_ci(line, "#KODIPROP:")
            && let Some((k, v)) = rest.split_once('=')
        {
            match k.trim().to_ascii_lowercase().as_str() {
                "inputstream.adaptive.license_type" | "inputstream.adaptive.license_key" => self.directive_drm = true,
                "inputstream.adaptive.stream_headers" | "inputstream.adaptive.common_headers" | "inputstream.adaptive.manifest_headers" => {
                    self.kodi_headers.extend(split_header_pairs(v));
                }
                _ => {}
            }
        }
        // Everything else (`# comments`, `#EXT-X-*` after the first entry, `#PLAYLIST`, …).
        None
    }

    fn merge_header(&mut self, rest: &str) {
        let (attrs, _) = tokenize(rest, false);
        let h = &mut self.header;
        for (k, v) in &attrs {
            if TVG_URL_KEYS.contains(&k.as_str()) {
                for u in v.split(',').map(str::trim).filter(|u| !u.is_empty()) {
                    if h.tvg_urls.len() < MAX_ATTRS && !h.tvg_urls.iter().any(|x| x == u) {
                        h.tvg_urls.push(u.to_owned());
                    }
                }
            } else if k == "tvg-shift" {
                if h.tvg_shift_h.is_none() {
                    h.tvg_shift_h = parse_shift_h(v);
                }
            } else if !CATCHUP_KEYS.contains(&k.as_str()) && h.attrs.len() < MAX_ATTRS && !h.attrs.iter().any(|(hk, _)| hk == k) {
                h.attrs.push((k.clone(), v.clone()));
            }
        }
        if let Some(fields) = CatchupFields::from_attrs(&attrs) {
            self.catchup_defaults.fill(fields);
        }
        self.header.catchup = CatchupFields::default().resolve(&self.catchup_defaults);
        self.header_seen = true;
    }

    /// Forget the entry being assembled (its `#EXTINF` and directives).
    fn drop_pending(&mut self) {
        self.pending = None;
        self.vlc_headers.clear();
        self.kodi_headers.clear();
        self.directive_drm = false;
    }

    fn make_entry(&mut self, url_line: &str) -> Result<M3uEntry> {
        let (raw_url, pipe) = split_pipe_headers(url_line);
        if raw_url.is_empty() {
            // `|User-Agent=x` alone: resolving "" would yield the playlist URL itself, which
            // may carry the account's credentials.
            self.drop_pending();
            return Err(Error::parse(Some(self.line_no), "entry has an empty stream URL"));
        }
        let inf = self.pending.take();
        let drm = std::mem::take(&mut self.directive_drm);
        let vlc_headers = std::mem::take(&mut self.vlc_headers);
        let kodi_headers = std::mem::take(&mut self.kodi_headers);
        let url = self.resolve(&raw_url).into_owned();
        let (attrs, title) = inf.map(|p| (p.attrs, p.title)).unwrap_or_default();
        let get = |key: &str| attr(&attrs, key).filter(|v| !v.is_empty()).map(str::to_owned);

        let mut headers = HttpHeaders::default();
        for (key, name) in [("http-user-agent", "User-Agent"), ("http-referrer", "Referer"), ("http-referer", "Referer"), ("http-origin", "Origin")] {
            if let Some(v) = attr(&attrs, key) {
                apply_header(&mut headers, name, v);
            }
        }
        for (name, v) in vlc_headers.iter().chain(&kodi_headers).chain(&pipe) {
            apply_header(&mut headers, name, v);
        }

        let groups = match attr(&attrs, "group-title").map(split_groups) {
            Some(g) if !g.is_empty() => g,
            _ => self.current_extgrp.clone(),
        };
        let tvg_name = get("tvg-name");
        let title = if !title.is_empty() {
            title
        } else if let Some(n) = &tvg_name {
            n.clone()
        } else {
            title_from_url(&url).to_owned()
        };
        let tvg_logo = get("tvg-logo").map(|l| self.resolve(&l).into_owned());
        let radio = attr(&attrs, "radio").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            || attr(&attrs, "tvg-type").is_some_and(|v| v.eq_ignore_ascii_case("radio"));
        Ok(M3uEntry {
            line: self.line_no,
            tvg_id: get("tvg-id"),
            tvg_logo,
            tvg_chno: attr(&attrs, "tvg-chno").and_then(|v| v.parse().ok()),
            groups,
            tvg_shift_h: attr(&attrs, "tvg-shift").and_then(parse_shift_h).or(self.header.tvg_shift_h),
            catchup: CatchupFields::from_attrs(&attrs).and_then(|f| f.resolve(&self.catchup_defaults)),
            headers,
            radio,
            drm,
            url,
            title,
            tvg_name,
            attrs,
        })
    }

    /// Resolve a relative reference against the playlist URL; anything with a scheme (or no
    /// base) is returned verbatim (`udp://@239.0.0.1:1234` must not be re-serialized).
    fn resolve<'a>(&self, s: &'a str) -> Cow<'a, str> {
        match &self.base {
            Some(base) if !has_scheme(s) => match base.join(s) {
                Ok(u) => Cow::Owned(u.into()),
                Err(_) => Cow::Borrowed(s),
            },
            _ => Cow::Borrowed(s),
        }
    }
}

impl<R: BufRead> Iterator for M3uParser<R> {
    /// Malformed entries are reported as `Err` and parsing continues with the next line.
    type Item = Result<M3uEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.ready.take() {
            return Some(item);
        }
        if self.done {
            return None;
        }
        loop {
            match self.read_line() {
                Ok(true) => {}
                Ok(false) => {
                    self.done = true;
                    return self.pending.take().map(|p| Err(dangling(&p)));
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(io_error(e)));
                }
            }
            if self.overlong {
                // Most likely the URL of the pending entry: drop the whole entry rather than
                // attaching its #EXTINF to the next URL.
                self.drop_pending();
                return Some(Err(Error::parse(Some(self.line_no), "line longer than 1 MiB skipped")));
            }
            let buf = std::mem::take(&mut self.buf);
            let item = {
                let text = decode(&buf);
                let line = clean_line(&text);
                if line.is_empty() { None } else { self.handle_line(line) }
            };
            self.buf = buf;
            if item.is_some() {
                return item;
            }
        }
    }
}

/// Tokenize the part of an `#EXTINF:` line after the duration. Returns (attrs, title).
/// `-1 tvg-id="a,b" group-title='News' tvg-logo=x.png,Title, with comma` →
/// `([("tvg-id","a,b"),("group-title","News"),("tvg-logo","x.png")], "Title, with comma")`.
pub fn parse_extinf_attrs(s: &str) -> (Vec<(String, String)>, String) {
    let s = s.trim_start();
    // Optional duration (`-1`, `0`, `-1.0`); attribute-only lines (`tvg-id=…`) have none.
    let end = s.find(|c: char| c.is_ascii_whitespace() || c == ',').unwrap_or(s.len());
    let dur = &s[..end];
    let is_duration = !dur.is_empty() && dur.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.'));
    tokenize(if is_duration { &s[end..] } else { s }, true)
}

/// Attribute tokenizer shared by `#EXTINF` (`title_comma`: the first comma outside quotes
/// starts the title) and `#EXTM3U` (no title; unquoted values may contain commas). Without a
/// title comma, the title is made of the words that are not `key=value` pairs.
fn tokenize(s: &str, title_comma: bool) -> (Vec<(String, String)>, String) {
    let b = s.as_bytes();
    let n = b.len();
    let ends_word = |c: u8| c.is_ascii_whitespace() || (title_comma && c == b',');
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut bare: Vec<&str> = Vec::new();
    let mut no_close_from = [usize::MAX; 2];
    let mut i = 0;
    loop {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        if title_comma && b[i] == b',' {
            return (attrs, s[i + 1..].trim().to_owned());
        }
        let ks = i;
        while i < n && b[i] != b'=' && !ends_word(b[i]) {
            i += 1;
        }
        if i < n && b[i] == b'=' && i > ks {
            let key = s[ks..i].to_ascii_lowercase();
            let (value, next) = read_value(s, i + 1, title_comma, &mut no_close_from);
            i = next;
            let full = attrs.len() >= MAX_ATTRS;
            match attrs.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                // Past the cap keep tokenizing (the title comma must still be found) but stop
                // storing, so the duplicate lookup stays bounded on hostile lines.
                None if !full => attrs.push((key, value)),
                None => {}
            }
        } else {
            // A word that is not `key=value` (title text when the comma is missing).
            while i < n && !ends_word(b[i]) {
                i += 1;
            }
            bare.push(&s[ks..i]);
        }
    }
    (attrs, bare.join(" "))
}

/// Read an attribute value starting at `i` (just after `=`). Returns (trimmed value, index
/// after it). A quote closes the value only when followed by the end, a comma, or whitespace
/// and then a comma / the end / another `key=` — so stray inner quotes survive
/// (`tvg-name="The "Best" One"`). `no_close_from` (per quote char, one per line) is the earliest
/// position known to have no plausible closing quote after it.
fn read_value(s: &str, i: usize, title_comma: bool, no_close_from: &mut [usize; 2]) -> (String, usize) {
    let b = s.as_bytes();
    let n = b.len();
    let scan_word = |from: usize| {
        let mut j = from;
        while j < n && !b[j].is_ascii_whitespace() && !(title_comma && b[j] == b',') {
            j += 1;
        }
        j
    };
    if i < n && (b[i] == b'"' || b[i] == b'\'') {
        let q = b[i];
        let slot = usize::from(q == b'\'');
        let vs = i + 1;
        // Whether a quote at `j` closes a value depends on `j` alone, so once a scan from some
        // position found none, no scan from a later one can: skip it (keeps a line full of
        // unbalanced quotes linear instead of quadratic).
        if vs < no_close_from[slot] {
            let mut pos = vs;
            while let Some(off) = memchr::memchr(q, &b[pos..]) {
                let j = pos + off;
                if closes_value(b, j + 1) {
                    return (s[vs..j].trim().to_owned(), j + 1);
                }
                pos = j + 1;
            }
            no_close_from[slot] = vs;
        }
        if let Some(j) = memchr::memchr(q, &b[vs..]).map(|off| vs + off) {
            return (s[vs..j].trim().to_owned(), j + 1);
        }
        // Unterminated quote: behave like an unquoted value.
        let end = scan_word(vs);
        return (s[vs..end].trim().to_owned(), end);
    }
    let end = scan_word(i);
    (s[i..end].to_owned(), end)
}

/// Is the quote just before `k` a plausible closing quote?
fn closes_value(b: &[u8], mut k: usize) -> bool {
    let n = b.len();
    if k >= n || b[k] == b',' {
        return true;
    }
    if !b[k].is_ascii_whitespace() {
        return false;
    }
    while k < n && b[k].is_ascii_whitespace() {
        k += 1;
    }
    if k >= n || b[k] == b',' {
        return true;
    }
    let ks = k;
    while k < n && (b[k].is_ascii_alphanumeric() || matches!(b[k], b'-' | b'_' | b':' | b'.')) {
        k += 1;
    }
    k > ks && k < n && b[k] == b'='
}

/// Split Kodi pipe headers: `http://x/y.m3u8|User-Agent=a&Referer=b`.
pub fn split_pipe_headers(url: &str) -> (String, Vec<(String, String)>) {
    match url.split_once('|') {
        None => (url.trim().to_owned(), Vec::new()),
        Some((u, rest)) => (u.trim().to_owned(), split_header_pairs(rest)),
    }
}

/// `Name=value&Name2=value2` with percent-decoded values (`+` is kept: user agents use it).
fn split_header_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_owned(), percent_decode(v.trim())))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Build a [`CatchupSpec`] from entry attributes (+ header defaults). Archive window order:
/// `catchup-days` → `timeshift` → `tvg-rec` → `catchup-time`/86400. `timeshift` alone implies
/// `Shift`. Unknown `catchup` values → `None` (logged).
///
/// Kind: own `catchup` / `catchup-type` → `timeshift` (Shift) → the defaults' kind → `Default`
/// when only `catchup-source` is given; nothing → `None`. Every other field falls back to the
/// defaults individually (Kodi semantics), then to 5 days / no template / no correction.
/// `catchup-correction` is in hours (may be fractional).
pub fn catchup_from_attrs(attrs: &[(String, String)], defaults: Option<&CatchupSpec>) -> Option<CatchupSpec> {
    CatchupFields::from_attrs(attrs)?.resolve(&defaults.map(CatchupFields::from_spec).unwrap_or_default())
}

/// The catch-up fields one line gives, before defaults apply. The `#EXTM3U` header keeps one of
/// these so a header `catchup-days` / `catchup-correction` without a kind is still inherited.
#[derive(Debug, Clone, Default)]
struct CatchupFields {
    /// Own `catchup` / `catchup-type`, else `Shift` when `timeshift` gives a window.
    kind: Option<CatchupKind>,
    days: Option<u32>,
    template: Option<String>,
    correction_s: Option<i64>,
}

impl CatchupFields {
    /// `None` when the line names an unknown kind (`catchup="disabled"`): no archive at all.
    fn from_attrs(attrs: &[(String, String)]) -> Option<Self> {
        let get = |key: &str| {
            attrs.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.trim()).filter(|v| !v.is_empty())
        };
        let own_kind = match get("catchup").or_else(|| get("catchup-type")) {
            Some(v) => match super::catchup::parse_kind(v) {
                Some(k) => Some(k),
                None => {
                    tracing::debug!(value = v, "ignoring unknown M3U catchup type");
                    return None;
                }
            },
            None => None,
        };
        let timeshift_days = get("timeshift").and_then(parse_days);
        Some(Self {
            kind: own_kind.or(timeshift_days.map(|_| CatchupKind::Shift)),
            days: get("catchup-days")
                .and_then(parse_days)
                .or(timeshift_days)
                .or_else(|| get("tvg-rec").and_then(parse_days))
                .or_else(|| get("catchup-time").and_then(seconds_to_days)),
            template: get("catchup-source").map(str::to_owned),
            correction_s: get("catchup-correction").and_then(hours_to_secs),
        })
    }

    fn from_spec(spec: &CatchupSpec) -> Self {
        Self { kind: Some(spec.kind), days: Some(spec.days), template: spec.template.clone(), correction_s: Some(spec.correction_s) }
    }

    /// Fill the fields that are still unset (a repeated `#EXTM3U` never overrides).
    fn fill(&mut self, other: Self) {
        self.kind = self.kind.or(other.kind);
        self.days = self.days.or(other.days);
        self.template = self.template.take().or(other.template);
        self.correction_s = self.correction_s.or(other.correction_s);
    }

    /// Apply `defaults` field by field. Kind: own → defaults' → `Default` when a template is
    /// known; none of them → no archive.
    fn resolve(self, defaults: &CatchupFields) -> Option<CatchupSpec> {
        let template = self.template.or_else(|| defaults.template.clone());
        let kind = self.kind.or(defaults.kind).or(template.as_ref().map(|_| CatchupKind::Default))?;
        Some(CatchupSpec {
            kind,
            days: self.days.or(defaults.days).unwrap_or(DEFAULT_CATCHUP_DAYS),
            template,
            correction_s: self.correction_s.or(defaults.correction_s).unwrap_or(0),
        })
    }
}

/// Recognise an Xtream `get.php?username=..&password=..` (or `player_api.php`) URL pasted as
/// an M3U URL, so onboarding can offer the richer Xtream login.
pub fn detect_xtream_url(url: &str) -> Option<XtreamCredentials> {
    let t = url.trim();
    let with_scheme: Cow<'_, str> = if t.contains("://") { Cow::Borrowed(t) } else { Cow::Owned(format!("http://{t}")) };
    let u = Url::parse(&with_scheme).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let last = u.path_segments()?.rfind(|s| !s.is_empty())?;
    if !(last.eq_ignore_ascii_case("get.php") || last.eq_ignore_ascii_case("player_api.php")) {
        return None;
    }
    let (mut username, mut password) = (None, None);
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "username" => username = Some(v.into_owned()),
            "password" => password = Some(v.into_owned()),
            _ => {}
        }
    }
    let username = username.filter(|s| !s.is_empty())?;
    let password = password.filter(|s| !s.is_empty())?;
    let server = super::xtream::urls::normalize_server(&with_scheme)?;
    Some(XtreamCredentials { server, username, password: password.into() })
}

// ---------------------------------------------------------------------------------------------
// helpers

fn push_capped(buf: &mut Vec<u8>, overlong: &mut bool, bytes: &[u8]) {
    let room = MAX_LINE.saturating_sub(buf.len());
    if bytes.len() > room {
        *overlong = true;
    }
    buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

/// UTF-8 when valid, else Windows-1252. A leading UTF-8 BOM is dropped first so it never turns
/// into `ï»¿` on a Windows-1252 line.
fn decode(buf: &[u8]) -> Cow<'_, str> {
    let buf = buf.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(buf);
    match std::str::from_utf8(buf) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(buf).0,
    }
}

/// Trim whitespace and any BOM (concatenated playlists carry one per part).
fn clean_line(s: &str) -> &str { s.trim().trim_start_matches('\u{feff}').trim_start() }

fn io_error(e: io::Error) -> Error { Error::io("playlist", e) }

fn dangling(p: &PendingInf) -> Error { Error::parse(Some(p.line), format!("#EXTINF \"{}\" has no stream URL", p.title)) }

fn starts_with_ci(s: &str, prefix: &str) -> bool { s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) }

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> { starts_with_ci(s, prefix).then(|| &s[prefix.len()..]) }

/// Last value of an attribute (keys are already lowercase and unique).
fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.trim())
}

/// `a; b;;a` → `["a", "b"]` (trimmed, empty and duplicates removed).
fn split_groups(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in s.split(';').map(str::trim).filter(|g| !g.is_empty()) {
        if !out.iter().any(|x| x == g) {
            out.push(g.to_owned());
        }
    }
    out
}

/// RFC 3986 scheme followed by `:` (`http:`, `rtmp:`, `udp:`, `file:`…). Single letters are
/// not schemes (`C:` drive paths).
fn has_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else { return false };
    let scheme = &s[..colon];
    scheme.len() >= 2
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
}

/// Last non-empty path segment (query/fragment removed); the whole URL if there is none.
fn title_from_url(url: &str) -> &str {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let path = no_query.split_once("://").map_or(no_query, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url)
}

/// Apply one header; the well-known ones get their dedicated fields.
fn apply_header(h: &mut HttpHeaders, name: &str, value: &str) {
    let value = value.trim();
    let name = name.trim();
    if value.is_empty() || name.is_empty() {
        return;
    }
    match name.to_ascii_lowercase().as_str() {
        "user-agent" => h.user_agent = Some(value.to_owned()),
        "referer" | "referrer" => h.referrer = Some(value.to_owned()),
        "origin" => h.origin = Some(value.to_owned()),
        _ => match h.extra.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some(slot) => slot.1 = value.to_owned(),
            None => h.extra.push((name.to_owned(), value.to_owned())),
        },
    }
}

/// `tvg-shift` hours (`"-1.5"`, `"+2"`, `"1,5"`); out-of-range values are ignored.
fn parse_shift_h(v: &str) -> Option<f32> {
    let v = v.trim().replace(',', ".");
    v.parse::<f32>().ok().filter(|h| h.is_finite() && h.abs() <= 24.0)
}

/// Positive whole days (fractions round up).
fn parse_days(v: &str) -> Option<u32> {
    let v = v.trim();
    v.parse::<u32>().ok().or_else(|| v.parse::<f64>().ok().filter(|f| f.is_finite() && *f > 0.0).map(|f| f.ceil().min(u32::MAX as f64) as u32)).filter(|d| *d > 0)
}

/// `catchup-time` seconds → days, rounded up so a 12 h archive stays usable.
fn seconds_to_days(v: &str) -> Option<u32> {
    let secs = v.trim().parse::<f64>().ok().filter(|f| f.is_finite() && *f > 0.0)?;
    Some((secs / 86_400.0).ceil().min(u32::MAX as f64) as u32)
}

/// `catchup-correction` hours → seconds.
fn hours_to_secs(v: &str) -> Option<i64> {
    let h = v.trim().replace(',', ".").parse::<f64>().ok().filter(|f| f.is_finite() && f.abs() <= 48.0)?;
    Some((h * 3600.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }

    #[test]
    fn extinf_doc_example() {
        let (attrs, title) = parse_extinf_attrs(r#"-1 tvg-id="a,b" group-title='News' tvg-logo=x.png,Title, with comma"#);
        assert_eq!(attrs, kv(&[("tvg-id", "a,b"), ("group-title", "News"), ("tvg-logo", "x.png")]));
        assert_eq!(title, "Title, with comma");
    }

    #[test]
    fn extinf_quirks() {
        // no duration, uppercase key, duplicate (last wins, first position kept)
        let (a, t) = parse_extinf_attrs(r#"Tvg-ID="x" tvg-id="y" group-title="",T"#);
        assert_eq!(a, kv(&[("tvg-id", "y"), ("group-title", "")]));
        assert_eq!(t, "T");
        // stray inner quotes
        let (a, t) = parse_extinf_attrs(r#"-1 tvg-name="The "Best" One" group-title="X",Title"#);
        assert_eq!(a, kv(&[("tvg-name", r#"The "Best" One"#), ("group-title", "X")]));
        assert_eq!(t, "Title");
        // missing comma: leftover words are the title
        let (a, t) = parse_extinf_attrs(r#"-1 group-title="News" Some Title"#);
        assert_eq!(a, kv(&[("group-title", "News")]));
        assert_eq!(t, "Some Title");
        // unterminated quote
        let (a, t) = parse_extinf_attrs(r#"-1 tvg-id="abc,Name"#);
        assert_eq!(a, kv(&[("tvg-id", "abc")]));
        assert_eq!(t, "Name");
        // nothing at all
        assert_eq!(parse_extinf_attrs("-1,"), (vec![], String::new()));
        assert_eq!(parse_extinf_attrs(""), (vec![], String::new()));
        // duration only + title
        assert_eq!(parse_extinf_attrs("-1.0,Plain"), (vec![], "Plain".to_owned()));
        // unicode around delimiters
        let (a, t) = parse_extinf_attrs("-1 tvg-name=\"Ça, té\" group-title=日本,東京 ニュース");
        assert_eq!(a, kv(&[("tvg-name", "Ça, té"), ("group-title", "日本")]));
        assert_eq!(t, "東京 ニュース");
    }

    #[test]
    fn header_tokenizer_keeps_commas_in_unquoted_values() {
        let (a, t) = tokenize(" url-tvg=http://a/1.xml,http://b/2.xml tvg-shift=2", false);
        assert_eq!(a, kv(&[("url-tvg", "http://a/1.xml,http://b/2.xml"), ("tvg-shift", "2")]));
        assert!(t.is_empty());
    }

    #[test]
    fn pipe_headers() {
        assert_eq!(
            split_pipe_headers("http://x/y.m3u8|User-Agent=a&Referer=b"),
            ("http://x/y.m3u8".to_owned(), kv(&[("User-Agent", "a"), ("Referer", "b")]))
        );
        assert_eq!(split_pipe_headers(" http://x/y "), ("http://x/y".to_owned(), vec![]));
        assert_eq!(
            split_pipe_headers("http://x|User-Agent=Mozilla%2F5.0%20(X11)+x&=bad&noeq&Cookie=a=b"),
            ("http://x".to_owned(), kv(&[("User-Agent", "Mozilla/5.0 (X11)+x"), ("Cookie", "a=b")]))
        );
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%41%42"), "AB");
    }

    #[test]
    fn catchup_precedence() {
        let spec = |kind, days, template: Option<&str>, correction_s| CatchupSpec { kind, days, template: template.map(str::to_owned), correction_s };
        assert_eq!(catchup_from_attrs(&kv(&[]), None), None);
        assert_eq!(catchup_from_attrs(&kv(&[("catchup", "bogus"), ("catchup-days", "3")]), None), None);
        assert_eq!(catchup_from_attrs(&kv(&[("catchup", "xc")]), None), Some(spec(CatchupKind::Xc, DEFAULT_CATCHUP_DAYS, None, 0)));
        assert_eq!(catchup_from_attrs(&kv(&[("catchup-type", "fs"), ("catchup-days", "2")]), None), Some(spec(CatchupKind::Flussonic, 2, None, 0)));
        // window order
        let all = kv(&[("catchup", "shift"), ("catchup-days", "1"), ("timeshift", "2"), ("tvg-rec", "3"), ("catchup-time", "345600")]);
        assert_eq!(catchup_from_attrs(&all, None).unwrap().days, 1);
        assert_eq!(catchup_from_attrs(&all[..1].iter().chain(&all[2..]).cloned().collect::<Vec<_>>(), None).unwrap().days, 2);
        assert_eq!(catchup_from_attrs(&all[..1].iter().chain(&all[3..]).cloned().collect::<Vec<_>>(), None).unwrap().days, 3);
        assert_eq!(catchup_from_attrs(&all[..1].iter().chain(&all[4..]).cloned().collect::<Vec<_>>(), None).unwrap().days, 4);
        assert_eq!(catchup_from_attrs(&kv(&[("catchup", "default"), ("catchup-time", "43200")]), None).unwrap().days, 1);
        // timeshift alone implies Shift (even over a header default kind)
        let header = spec(CatchupKind::Append, 3, Some("?utc={utc}"), 3600);
        assert_eq!(catchup_from_attrs(&kv(&[("timeshift", "2")]), None), Some(spec(CatchupKind::Shift, 2, None, 0)));
        assert_eq!(catchup_from_attrs(&kv(&[("timeshift", "2")]), Some(&header)), Some(spec(CatchupKind::Shift, 2, Some("?utc={utc}"), 3600)));
        assert_eq!(catchup_from_attrs(&kv(&[("timeshift", "0")]), None), None);
        // defaults per field
        assert_eq!(catchup_from_attrs(&kv(&[]), Some(&header)), Some(header.clone()));
        assert_eq!(
            catchup_from_attrs(&kv(&[("catchup-days", "7"), ("catchup-correction", "-1.5")]), Some(&header)),
            Some(spec(CatchupKind::Append, 7, Some("?utc={utc}"), -5400))
        );
        // catchup-source alone → Default
        assert_eq!(
            catchup_from_attrs(&kv(&[("catchup-source", "http://h/a?s={utc}")]), None),
            Some(spec(CatchupKind::Default, DEFAULT_CATCHUP_DAYS, Some("http://h/a?s={utc}"), 0))
        );
        // an explicit unknown kind beats header defaults
        assert_eq!(catchup_from_attrs(&kv(&[("catchup", "disabled")]), Some(&header)), None);
    }

    #[test]
    fn xtream_detection() {
        let c = detect_xtream_url("http://panel.example:8080/get.php?username=u%40x&password=p%26w&type=m3u_plus&output=ts").unwrap();
        assert_eq!(c.server, "http://panel.example:8080");
        assert_eq!(c.username, "u@x");
        assert_eq!(c.password.expose(), "p&w");
        let c = detect_xtream_url(" panel.example/player_api.php?password=b&username=a ").unwrap();
        assert_eq!((c.server.as_str(), c.username.as_str(), c.password.expose().as_str()), ("http://panel.example", "a", "b"));
        let c = detect_xtream_url("https://H.example/sub/GET.PHP?username=a&password=b").unwrap();
        assert_eq!(c.server, "https://h.example");
        assert!(detect_xtream_url("http://h/get.php?username=a").is_none());
        assert!(detect_xtream_url("http://h/get.php?username=&password=b").is_none());
        assert!(detect_xtream_url("http://h/list.m3u?username=a&password=b").is_none());
        assert!(detect_xtream_url("ftp://h/get.php?username=a&password=b").is_none());
        assert!(detect_xtream_url("not a url at all").is_none());
    }

    #[test]
    fn helpers() {
        assert!(has_scheme("udp://@239.0.0.1:1234") && has_scheme("rtmp://h/x") && has_scheme("file:///a"));
        assert!(!has_scheme("cnn/index.m3u8") && !has_scheme("C:\\x") && !has_scheme("/abs/path.ts") && !has_scheme("a/b:c"));
        assert_eq!(title_from_url("http://h/path/bare.m3u8?token=1"), "bare.m3u8");
        assert_eq!(title_from_url("http://h/a/b/"), "b");
        assert_eq!(title_from_url("http://h"), "http://h");
        assert_eq!(title_from_url("udp://@239.0.0.1:1234"), "udp://@239.0.0.1:1234");
        assert_eq!(title_from_url("relative/x.ts"), "x.ts");
        assert_eq!(parse_shift_h("+1"), Some(1.0));
        assert_eq!(parse_shift_h("-1,5"), Some(-1.5));
        assert_eq!(parse_shift_h("99"), None);
        assert_eq!(parse_days("2.5"), Some(3));
        assert_eq!(parse_days("-1"), None);
        assert_eq!(hours_to_secs("0.5"), Some(1800));
        assert_eq!(split_groups(" a ; b;;a ;"), vec!["a".to_owned(), "b".to_owned()]);
    }
}
