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

use std::io::BufRead;

use url::Url;

use crate::error::Result;
use crate::model::{CatchupSpec, HttpHeaders, XtreamCredentials};

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

/// Iterator over entries. Construct with [`parse`].
pub struct M3uParser<R: BufRead> {
    reader: R,
    base: Option<Url>,
    header: M3uHeader,
    line_no: u64,
    buf: Vec<u8>,
    pending: Option<M3uEntry>,
    current_extgrp: Option<String>,
    done: bool,
}

/// Start parsing. Reads ahead to the first `#EXTINF`/URL to populate the header and to
/// classify the payload (HTML/JSON/XML → `M3uError::NotM3u`).
pub fn parse<R: BufRead>(reader: R, base: Option<&Url>) -> std::result::Result<M3uParser<R>, M3uError> { todo!("WP-03") }

impl<R: BufRead> M3uParser<R> {
    pub fn header(&self) -> &M3uHeader { &self.header }
    /// Bytes consumed so far (progress reporting).
    pub fn bytes_read(&self) -> u64 { todo!("WP-03") }
}

impl<R: BufRead> Iterator for M3uParser<R> {
    /// Malformed entries are reported as `Err` and parsing continues with the next line.
    type Item = Result<M3uEntry>;
    fn next(&mut self) -> Option<Self::Item> { todo!("WP-03") }
}

/// Tokenize the part of an `#EXTINF:` line after the duration. Returns (attrs, title).
/// `-1 tvg-id="a,b" group-title='News' tvg-logo=x.png,Title, with comma` →
/// `([("tvg-id","a,b"),("group-title","News"),("tvg-logo","x.png")], "Title, with comma")`.
pub fn parse_extinf_attrs(s: &str) -> (Vec<(String, String)>, String) { todo!("WP-03") }

/// Split Kodi pipe headers: `http://x/y.m3u8|User-Agent=a&Referer=b`.
pub fn split_pipe_headers(url: &str) -> (String, Vec<(String, String)>) { todo!("WP-03") }

/// Build a [`CatchupSpec`] from entry attributes (+ header defaults). Archive window order:
/// `catchup-days` → `timeshift` → `tvg-rec` → `catchup-time`/86400. `timeshift` alone implies
/// `Shift`. Unknown `catchup` values → `None` (logged).
pub fn catchup_from_attrs(attrs: &[(String, String)], defaults: Option<&CatchupSpec>) -> Option<CatchupSpec> { todo!("WP-03") }

/// Recognise an Xtream `get.php?username=..&password=..` (or `player_api.php`) URL pasted as
/// an M3U URL, so onboarding can offer the richer Xtream login.
pub fn detect_xtream_url(url: &str) -> Option<XtreamCredentials> { todo!("WP-03") }
