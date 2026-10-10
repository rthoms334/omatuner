//! HTTP client shared by all fetchers. Owner: WP-04.
//!
//! * reqwest 0.13 (rustls), `connect_timeout` 10 s, read timeout = `network_timeout_s`,
//!   redirects limited to 10, gzip/brotli/zstd *transfer* decoding.
//! * Large payloads (playlists, XMLTV, big Xtream JSON) are streamed to a cache file with
//!   progress callbacks and cancellation; parsing happens afterwards from disk in
//!   `spawn_blocking` (DESIGN §9.1). File compression (`.gz`/`.xz`) is detected by MAGIC BYTES,
//!   never by Content-Type/extension.
//! * Conditional GET with ETag / Last-Modified.
//! * Every error leaving this module is redacted (`Error::from(reqwest::Error)`).
//! * Files that may contain credentials (playlists from `get.php`, `xmltv.php` guides) are
//!   written to the 0700 `playlists/` / `epg/` cache dirs; the `.part` file is created with
//!   mode 0600 (`OpenOptionsExt::mode`) before the first byte is written. M3U playlists are
//!   deleted after a successful import (ETag/Last-Modified are kept in the DB).

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::{Bytes, BytesMut};
use reqwest::header::{self, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::model::HttpHeaders;
use crate::redact::{Secret, redact, redact_url};
use crate::sources::{PayloadKind, sniff_payload};

/// `get_json` body cap.
const JSON_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Progress granularity of `download` (also its write buffer size).
const PROGRESS_STEP: u64 = 256 * 1024;
/// Read buffer of `open_decompressed`.
const READ_BUF: usize = 256 * 1024;

/// Copy shown when a web page arrives where data was expected (§9.1, §13.12).
const HTML_INSTEAD_OF_DATA: &str =
    "the server returned a web page instead of data — the account may have expired or the server is blocking this app";

#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    default_user_agent: String,
}

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub default_user_agent: String,
    pub connect_timeout_s: u64,
    pub read_timeout_s: u64,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self { default_user_agent: "VLC/3.0.21 LibVLC/3.0.21".into(), connect_timeout_s: 10, read_timeout_s: 30 }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: Secret<String>,
    pub dest: PathBuf,
    pub headers: HttpHeaders,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadOutcome {
    /// 304 — keep the previous import.
    NotModified,
    Downloaded {
        path: PathBuf,
        bytes: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        compression: Compression,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    /// `1f 8b` → `flate2::read::MultiGzDecoder`
    Gzip,
    /// `fd 37 7a 58 5a 00` → `liblzma::read::XzDecoder`
    Xz,
}

/// Progress callback: (bytes so far, total if known).
pub type ProgressFn<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

impl HttpClient {
    pub fn new(opts: &HttpOptions) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(opts.default_user_agent.clone())
            .connect_timeout(std::time::Duration::from_secs(opts.connect_timeout_s))
            .read_timeout(std::time::Duration::from_secs(opts.read_timeout_s))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(crate::error::Error::from)?;
        Ok(Self { inner, default_user_agent: opts.default_user_agent.clone() })
    }

    /// GET a URL and deserialize JSON. Body read fully (bounded at 512 MiB), then
    /// `serde_json::from_slice` in `spawn_blocking`. HTML bodies → `Error::BadPayload`.
    ///
    /// A leading BOM/whitespace is skipped and an empty body parses as JSON `null` (panels
    /// answer "nothing" that way; lenient target types map it to their default).
    pub async fn get_json<T: DeserializeOwned + Send + 'static>(
        &self,
        url: &str,
        headers: &HttpHeaders,
        cancel: &CancellationToken,
    ) -> Result<T> {
        let body = trim_json_start(self.fetch(url, headers, JSON_MAX_BYTES, cancel).await?);
        match sniff_payload(&body[..body.len().min(4096)]) {
            PayloadKind::Html => return Err(Error::BadPayload(HTML_INSTEAD_OF_DATA.into())),
            PayloadKind::M3u | PayloadKind::HlsMedia => {
                return Err(Error::BadPayload("expected JSON but the server sent a playlist".into()));
            }
            PayloadKind::Xmltv => return Err(Error::BadPayload("expected JSON but the server sent an XML document".into())),
            PayloadKind::Json | PayloadKind::Unknown => {}
        }
        let parsed = tokio::task::spawn_blocking(move || {
            let text: &[u8] = if body.is_empty() { b"null" } else { &body };
            serde_json::from_slice::<T>(text)
        })
        .await
        .map_err(Error::other)?;
        parsed.map_err(json_error)
    }

    /// GET small bodies (images, short API responses), bounded at `max_bytes`.
    /// Non-success statuses → `Error::Http`; larger bodies → `Error::BadPayload`.
    pub async fn get_bytes(&self, url: &str, headers: &HttpHeaders, max_bytes: u64, cancel: &CancellationToken) -> Result<bytes::Bytes> {
        self.fetch(url, headers, max_bytes, cancel).await
    }

    /// Stream `req.url` into `req.dest` (via `dest.part` + rename), reporting progress every
    /// ~256 KiB; sniff compression from the first bytes; honour ETag/Last-Modified.
    ///
    /// The destination directory is created 0700 when missing; the `.part` file is created
    /// 0600 before the first byte and removed again on any failure or cancellation.
    pub async fn download(&self, req: &DownloadRequest, progress: ProgressFn<'_>, cancel: &CancellationToken) -> Result<DownloadOutcome> {
        let url = req.url.expose().as_str();
        let mut rb = self.request(url, &req.headers);
        if let Some(etag) = req.etag.as_deref().and_then(header_value) {
            rb = rb.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = req.last_modified.as_deref().and_then(header_value) {
            rb = rb.header(header::IF_MODIFIED_SINCE, lm);
        }
        let mut resp = self.send(rb, url, cancel).await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            tracing::debug!(url = %redact_url(url), "not modified");
            return Ok(DownloadOutcome::NotModified);
        }
        check_status(&resp, url)?;
        let etag = header_string(&resp, header::ETAG);
        let last_modified = header_string(&resp, header::LAST_MODIFIED);
        let total = resp.content_length();

        let part = part_path(&req.dest);
        let file = create_part(&part).await?;
        let written = stream_to_file(&mut resp, file, &part, url, total, progress, cancel).await;
        let result = match written {
            Ok((bytes, compression)) => match tokio::fs::rename(&part, &req.dest).await {
                Ok(()) => Ok(DownloadOutcome::Downloaded { path: req.dest.clone(), bytes, etag, last_modified, compression }),
                Err(e) => Err(Error::io(&req.dest, e)),
            },
            Err(e) => Err(e),
        };
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result
    }

    pub fn default_user_agent(&self) -> &str { &self.default_user_agent }

    /// GET with `headers`; non-success status → `Error::Http`; body bounded at `max`.
    async fn fetch(&self, url: &str, headers: &HttpHeaders, max: u64, cancel: &CancellationToken) -> Result<Bytes> {
        let mut resp = self.send(self.request(url, headers), url, cancel).await?;
        check_status(&resp, url)?;
        read_body(&mut resp, url, max, cancel).await
    }

    fn request(&self, url: &str, headers: &HttpHeaders) -> reqwest::RequestBuilder {
        let mut rb = self.inner.get(url);
        // Invalid values are skipped (reqwest would otherwise fail the whole request with a
        // builder error). Only header names are logged; values may be tokens.
        let named = [
            (header::USER_AGENT, headers.user_agent.as_deref()),
            (header::REFERER, headers.referrer.as_deref()),
            (header::ORIGIN, headers.origin.as_deref()),
        ];
        for (name, value) in named {
            let Some(value) = value.filter(|v| !v.trim().is_empty()) else { continue };
            match header_value(value) {
                Some(v) => rb = rb.header(name, v),
                None => tracing::warn!(header = %name, "skipping invalid HTTP header value"),
            }
        }
        for (name, value) in &headers.extra {
            match (HeaderName::from_bytes(name.trim().as_bytes()), header_value(value)) {
                (Ok(n), Some(v)) => rb = rb.header(n, v),
                _ => tracing::warn!(header = %redact(name), "skipping invalid HTTP header"),
            }
        }
        rb
    }

    async fn send(&self, rb: reqwest::RequestBuilder, url: &str, cancel: &CancellationToken) -> Result<reqwest::Response> {
        tracing::debug!(url = %redact_url(url), "GET");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            r = rb.send() => r.map_err(|e| transport_error(e, url)),
        }
    }
}

/// Detect compression from magic bytes. Implemented in the skeleton (shared contract).
pub fn sniff_compression(first: &[u8]) -> Compression {
    if first.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else if first.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Compression::Xz
    } else {
        Compression::None
    }
}

/// Open a downloaded/local file, transparently decompressing gzip/xz, buffered (256 KiB).
pub fn open_decompressed(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let io = |e| Error::io(path, e);
    let mut file = std::fs::File::open(path).map_err(io)?;
    let mut head = Vec::with_capacity(6);
    (&mut file).take(6).read_to_end(&mut head).map_err(io)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    Ok(match sniff_compression(&head) {
        Compression::None => Box::new(BufReader::with_capacity(READ_BUF, file)),
        // Multi-member: concatenated (`cat a.gz b.gz`) and bgzip-style files are valid inputs.
        Compression::Gzip => Box::new(BufReader::with_capacity(READ_BUF, flate2::read::MultiGzDecoder::new(file))),
        Compression::Xz => {
            Box::new(BufReader::with_capacity(READ_BUF, liblzma::read::XzDecoder::new_multi_decoder(BufReader::new(file))))
        }
    })
}

fn header_value(v: &str) -> Option<HeaderValue> { HeaderValue::from_str(v.trim()).ok() }

fn header_string(resp: &reqwest::Response, name: HeaderName) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned).filter(|s| !s.is_empty())
}

fn check_status(resp: &reqwest::Response, url: &str) -> Result<()> {
    let status = resp.status();
    if status.is_success() { Ok(()) } else { Err(Error::Http { status: status.as_u16(), url: redact_url(url) }) }
}

/// Transport failure → `Error::Network` naming only the host. The reqwest message is taken
/// without its URL (that would carry `username=…&password=…`) and redacted on top.
fn transport_error(e: reqwest::Error, url: &str) -> Error {
    let timeout = e.is_timeout();
    let e = e.without_url();
    let mut parts: Vec<String> = vec![e.to_string()];
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        let m = s.to_string();
        if parts.last() != Some(&m) {
            parts.push(m);
        }
        src = s.source();
    }
    let mut msg = parts.join(": ");
    if timeout && !msg.contains("timed out") {
        msg.push_str(" (timed out)");
    }
    match host_of(url) {
        Some(h) => Error::net(format!("{h}: {msg}")),
        None => Error::net(msg),
    }
}

/// `host[:port]` of a URL (no userinfo, path or query).
fn host_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

fn too_large(max: u64) -> Error {
    if max >= 1024 * 1024 {
        Error::BadPayload(format!("response larger than {} MiB", max / (1024 * 1024)))
    } else {
        Error::BadPayload(format!("response larger than {max} bytes"))
    }
}

async fn read_body(resp: &mut reqwest::Response, url: &str, max: u64, cancel: &CancellationToken) -> Result<Bytes> {
    let declared = resp.content_length();
    if declared.is_some_and(|len| len > max) {
        return Err(too_large(max));
    }
    let mut buf = BytesMut::with_capacity(declared.unwrap_or(16 * 1024).min(8 * 1024 * 1024) as usize);
    while let Some(chunk) = next_chunk(resp, url, cancel).await? {
        if (buf.len() + chunk.len()) as u64 > max {
            return Err(too_large(max));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

async fn next_chunk(resp: &mut reqwest::Response, url: &str, cancel: &CancellationToken) -> Result<Option<Bytes>> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        c = resp.chunk() => c.map_err(|e| transport_error(e, url)),
    }
}

/// Skip a UTF-8 BOM and leading whitespace (zero-copy).
fn trim_json_start(body: Bytes) -> Bytes {
    let rest = body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&body);
    let start = body.len() - rest.len() + rest.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(rest.len());
    body.slice(start..)
}

fn json_error(e: serde_json::Error) -> Error {
    let line = (e.line() > 0).then_some(e.line() as u64);
    // serde messages quote offending values; never let one carry a secret.
    Error::parse(line, redact(&format!("invalid JSON: {e}")).into_owned())
}

/// `<dest>.part` next to the destination (same filesystem → atomic rename).
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "download".into());
    name.push(".part");
    dest.with_file_name(name)
}

/// Create `part` with mode 0600 (replacing a stale leftover); the parent directory is created
/// 0700 when missing.
async fn create_part(part: &Path) -> Result<tokio::fs::File> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = part.parent().filter(|d| !d.as_os_str().is_empty())
        && !tokio::fs::try_exists(dir).await.unwrap_or(false)
    {
        tokio::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).await.map_err(|e| Error::io(dir, e))?;
    }
    // A leftover from a crashed run may have other permissions; `mode` only applies on create.
    match tokio::fs::remove_file(part).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(part, e)),
    }
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(part)
        .await
        .map_err(|e| Error::io(part, e))?;
    // The umask can only remove bits, but be explicit: nobody but the owner may read it.
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).await.map_err(|e| Error::io(part, e))?;
    Ok(file)
}

/// Stream the body into `file`; returns (bytes written, sniffed compression).
async fn stream_to_file(
    resp: &mut reqwest::Response,
    file: tokio::fs::File,
    part: &Path,
    url: &str,
    total: Option<u64>,
    progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<(u64, Compression)> {
    let mut out = tokio::io::BufWriter::with_capacity(PROGRESS_STEP as usize, file);
    let mut head: Vec<u8> = Vec::with_capacity(6);
    let mut done = 0u64;
    let mut next_report = PROGRESS_STEP;
    progress(0, total);
    while let Some(chunk) = next_chunk(resp, url, cancel).await? {
        if head.len() < 6 {
            let take = (6 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
        }
        out.write_all(&chunk).await.map_err(|e| Error::io(part, e))?;
        done += chunk.len() as u64;
        if done >= next_report {
            progress(done, total);
            next_report = (done / PROGRESS_STEP + 1) * PROGRESS_STEP;
        }
    }
    out.flush().await.map_err(|e| Error::io(part, e))?;
    // No fsync: the file is a re-downloadable cache entry that is deleted after the import.
    progress(done, total);
    Ok((done, sniff_compression(&head)))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;

    const BIG: usize = 1024 * 1024 + 10;

    /// (bytes so far, total) as reported to a progress callback.
    type ProgressCall = (u64, Option<u64>);

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut e = liblzma::write::XzEncoder::new(Vec::new(), 1);
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn plain_body() -> Vec<u8> { (0..BIG).map(|i| b"abcdefghij\n"[i % 11]).collect() }

    fn hdr(h: &HeaderMap, name: &str) -> String { h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned() }

    /// 64 KiB every 20 ms for ~20 s.
    fn slow_body() -> Body {
        let s = futures_util::stream::unfold(0u32, |n| async move {
            if n >= 1000 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 64 * 1024])), n + 1))
        });
        Body::from_stream(s)
    }

    async fn conditional(h: HeaderMap, body: Vec<u8>) -> axum::response::Response {
        if hdr(&h, "if-none-match") == "\"v1\"" || hdr(&h, "if-modified-since") == "Fri, 09 Oct 2026 18:00:00 GMT" {
            return StatusCode::NOT_MODIFIED.into_response();
        }
        ([("etag", "\"v1\""), ("last-modified", "Fri, 09 Oct 2026 18:00:00 GMT")], body).into_response()
    }

    async fn server() -> String {
        let app = Router::new()
            .route("/json", get(|| async { r#"{"a":1}"# }))
            .route("/bom", get(|| async { "\u{feff} \n{\"a\":2}" }))
            .route("/empty", get(|| async { "" }))
            .route("/html", get(|| async { "<!DOCTYPE html><html><body>Account expired</body></html>" }))
            .route("/xml", get(|| async { "<?xml version=\"1.0\"?><tv></tv>" }))
            .route("/m3u", get(|| async { "#EXTM3U\n#EXTINF:-1,A\nhttp://a/1.ts\n" }))
            .route("/broken", get(|| async { r#"{"a":"#.to_owned() + "hunter-in-body" }))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route("/forbidden", get(|| async { StatusCode::FORBIDDEN }))
            .route(
                "/headers",
                get(|h: HeaderMap| async move {
                    serde_json::json!({
                        "ua": hdr(&h, "user-agent"), "referer": hdr(&h, "referer"), "origin": hdr(&h, "origin"),
                        "token": hdr(&h, "x-token"), "if_none_match": hdr(&h, "if-none-match"),
                    })
                    .to_string()
                }),
            )
            .route("/big", get(|| async { plain_body() }))
            .route(
                "/chunked",
                get(|| async {
                    let chunks = (0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'y'; 512 * 1024])));
                    Body::from_stream(futures_util::stream::iter(chunks))
                }),
            )
            .route("/file.gz", get(|h: HeaderMap| conditional(h, gz(&plain_body()))))
            .route("/file.xz", get(|| async { xz(b"<tv>xz</tv>") }))
            .route("/file.txt", get(|h: HeaderMap| conditional(h, plain_body())))
            .route("/slow", get(|| async { slow_body() }))
            .route(
                "/hang",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    "late"
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        base
    }

    fn client() -> HttpClient { HttpClient::new(&HttpOptions::default()).unwrap() }

    fn none() -> HttpHeaders { HttpHeaders::default() }

    fn req(url: String, dest: PathBuf) -> DownloadRequest {
        DownloadRequest { url: Secret::new(url), dest, headers: none(), etag: None, last_modified: None }
    }

    #[tokio::test]
    async fn get_json_variants() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let v: serde_json::Value = c.get_json(&format!("{base}/json"), &none(), &tok).await.unwrap();
        assert_eq!(v["a"], 1);
        let v: serde_json::Value = c.get_json(&format!("{base}/bom"), &none(), &tok).await.unwrap();
        assert_eq!(v["a"], 2);
        let v: Option<serde_json::Value> = c.get_json(&format!("{base}/empty"), &none(), &tok).await.unwrap();
        assert!(v.is_none(), "empty body parses as null");
        let html = c.get_json::<serde_json::Value>(&format!("{base}/html"), &none(), &tok).await.unwrap_err();
        assert!(matches!(html, Error::BadPayload(ref m) if m.contains("web page")), "{html}");
        let xml = c.get_json::<serde_json::Value>(&format!("{base}/xml"), &none(), &tok).await.unwrap_err();
        assert!(matches!(xml, Error::BadPayload(_)), "{xml}");
        let m3u = c.get_json::<serde_json::Value>(&format!("{base}/m3u"), &none(), &tok).await.unwrap_err();
        assert!(matches!(m3u, Error::BadPayload(_)), "{m3u}");
        let broken = c.get_json::<serde_json::Value>(&format!("{base}/broken"), &none(), &tok).await.unwrap_err();
        assert!(matches!(broken, Error::Parse { line: Some(1), .. }), "{broken}");
        let missing = c.get_json::<serde_json::Value>(&format!("{base}/missing?password=pw404"), &none(), &tok).await.unwrap_err();
        match missing {
            Error::Http { status, ref url } => {
                assert_eq!(status, 404);
                assert!(url.ends_with("/missing?password=***"), "{url}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn headers_are_sent_and_invalid_ones_skipped() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let v: serde_json::Value = c.get_json(&format!("{base}/headers"), &none(), &tok).await.unwrap();
        assert_eq!(v["ua"], "VLC/3.0.21 LibVLC/3.0.21");
        assert_eq!(c.default_user_agent(), "VLC/3.0.21 LibVLC/3.0.21");
        let h = HttpHeaders {
            user_agent: Some("IPTVSmarters/1.0".into()),
            referrer: Some("http://ref.example/".into()),
            origin: Some("http://ref.example".into()),
            extra: vec![("X-Token".into(), "abc".into()), ("Bad Header".into(), "x".into()), ("X-Nl".into(), "a\nb".into())],
        };
        let v: serde_json::Value = c.get_json(&format!("{base}/headers"), &h, &tok).await.unwrap();
        assert_eq!(v["ua"], "IPTVSmarters/1.0");
        assert_eq!(v["referer"], "http://ref.example/");
        assert_eq!(v["origin"], "http://ref.example");
        assert_eq!(v["token"], "abc");
        // an invalid User-Agent falls back to the default instead of failing the request
        let bad_ua = HttpHeaders { user_agent: Some("bad\r\nua".into()), ..Default::default() };
        let v: serde_json::Value = c.get_json(&format!("{base}/headers"), &bad_ua, &tok).await.unwrap();
        assert_eq!(v["ua"], "VLC/3.0.21 LibVLC/3.0.21");
    }

    #[tokio::test]
    async fn get_bytes_is_bounded() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let b = c.get_bytes(&format!("{base}/big"), &none(), BIG as u64, &tok).await.unwrap();
        assert_eq!(b.len(), BIG);
        // declared length over the cap
        let e = c.get_bytes(&format!("{base}/big"), &none(), 1000, &tok).await.unwrap_err();
        assert!(matches!(e, Error::BadPayload(ref m) if m.contains("1000 bytes")), "{e}");
        // no Content-Length: the cap applies while streaming
        let e = c.get_bytes(&format!("{base}/chunked"), &none(), 1024 * 1024, &tok).await.unwrap_err();
        assert!(matches!(e, Error::BadPayload(ref m) if m.contains("1 MiB")), "{e}");
        let e = c.get_bytes(&format!("{base}/forbidden"), &none(), 10, &tok).await.unwrap_err();
        assert!(matches!(e, Error::Http { status: 403, .. }));
    }

    #[tokio::test]
    async fn download_progress_mode_and_validators() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cache/playlists/1.m3u");
        let part = dest.with_file_name("1.m3u.part");
        let calls: Arc<Mutex<Vec<ProgressCall>>> = Arc::default();
        let part_modes: Arc<Mutex<Vec<u32>>> = Arc::default();
        let progress = {
            let (calls, part_modes, part) = (calls.clone(), part_modes.clone(), part.clone());
            move |done: u64, total: Option<u64>| {
                calls.lock().unwrap().push((done, total));
                if let Ok(m) = std::fs::metadata(&part) {
                    part_modes.lock().unwrap().push(m.permissions().mode() & 0o777);
                }
            }
        };
        let out = c.download(&req(format!("{base}/file.txt"), dest.clone()), &progress, &tok).await.unwrap();
        let DownloadOutcome::Downloaded { path, bytes, etag, last_modified, compression } = out else { panic!("expected a download") };
        assert_eq!((path.as_path(), bytes, compression), (dest.as_path(), BIG as u64, Compression::None));
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        assert_eq!(last_modified.as_deref(), Some("Fri, 09 Oct 2026 18:00:00 GMT"));
        assert_eq!(std::fs::read(&dest).unwrap(), plain_body());
        assert!(!part.exists(), ".part renamed away");

        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.first(), Some(&(0, Some(BIG as u64))));
        assert_eq!(calls.last(), Some(&(BIG as u64, Some(BIG as u64))));
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0), "monotonic: {calls:?}");
        // 1 MiB at 256 KiB steps: start + 4 steps + final (steps may coincide with the final)
        assert!((5..=7).contains(&calls.len()), "{calls:?}");
        let modes = part_modes.lock().unwrap().clone();
        assert!(!modes.is_empty() && modes.iter().all(|&m| m == 0o600), "part modes {modes:?}");
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
        let parent_mode = std::fs::metadata(dest.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(parent_mode, 0o700, "missing cache dirs are created private");

        // conditional requests
        let noop = |_: u64, _: Option<u64>| {};
        let mut r = req(format!("{base}/file.txt"), dest.clone());
        r.etag = Some("\"v1\"".into());
        assert_eq!(c.download(&r, &noop, &tok).await.unwrap(), DownloadOutcome::NotModified);
        let mut r = req(format!("{base}/file.txt"), dest.clone());
        r.last_modified = Some("Fri, 09 Oct 2026 18:00:00 GMT".into());
        assert_eq!(c.download(&r, &noop, &tok).await.unwrap(), DownloadOutcome::NotModified);
        let mut r = req(format!("{base}/file.txt"), dest.clone());
        r.etag = Some("\"stale\"".into());
        assert!(matches!(c.download(&r, &noop, &tok).await.unwrap(), DownloadOutcome::Downloaded { .. }));
    }

    #[tokio::test]
    async fn download_sniffs_compression_and_decompresses() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();
        let noop = |_: u64, _: Option<u64>| {};
        for (path, want) in [("file.gz", Compression::Gzip), ("file.xz", Compression::Xz), ("file.txt", Compression::None)] {
            let dest = dir.path().join(path.replace('.', "_"));
            let out = c.download(&req(format!("{base}/{path}"), dest.clone()), &noop, &tok).await.unwrap();
            assert!(matches!(out, DownloadOutcome::Downloaded { compression, .. } if compression == want), "{path}: {out:?}");
            let mut text = Vec::new();
            open_decompressed(&dest).unwrap().read_to_end(&mut text).unwrap();
            let expected = if path == "file.xz" { b"<tv>xz</tv>".to_vec() } else { plain_body() };
            assert_eq!(text, expected, "{path}");
        }
    }

    #[tokio::test]
    async fn download_failures_leave_no_files() {
        let base = server().await;
        let c = client();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x.xml");
        let noop = |_: u64, _: Option<u64>| {};
        let e = c.download(&req(format!("{base}/missing"), dest.clone()), &noop, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(e, Error::Http { status: 404, .. }));
        assert!(!dest.exists() && !part_path(&dest).exists());

        // a stale leftover .part with loose permissions is replaced by a private one
        std::fs::write(part_path(&dest), b"old").unwrap();
        std::fs::set_permissions(part_path(&dest), std::fs::Permissions::from_mode(0o644)).unwrap();
        let tok = CancellationToken::new();
        let modes: Arc<Mutex<Vec<u32>>> = Arc::default();
        let progress = {
            let (modes, part) = (modes.clone(), part_path(&dest));
            move |_: u64, _: Option<u64>| {
                if let Ok(m) = std::fs::metadata(&part) {
                    modes.lock().unwrap().push(m.permissions().mode() & 0o777);
                }
            }
        };
        let canceller = {
            let tok = tok.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                tok.cancel();
                Instant::now()
            })
        };
        let e = c.download(&req(format!("{base}/slow"), dest.clone()), &progress, &tok).await.unwrap_err();
        let returned = Instant::now();
        let cancelled_at = canceller.await.unwrap();
        assert!(matches!(e, Error::Cancelled), "{e}");
        assert!(returned.duration_since(cancelled_at) < Duration::from_millis(200), "cancel took {:?}", returned - cancelled_at);
        assert!(!dest.exists() && !part_path(&dest).exists(), "cancelled download cleaned up");
        let modes = modes.lock().unwrap().clone();
        assert!(!modes.is_empty() && modes.iter().all(|&m| m == 0o600), "{modes:?}");
    }

    #[tokio::test]
    async fn cancel_while_waiting_for_headers() {
        let base = server().await;
        let c = client();
        let tok = CancellationToken::new();
        let t = tok.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t.cancel();
        });
        let start = Instant::now();
        let e = c.get_json::<serde_json::Value>(&format!("{base}/hang"), &none(), &tok).await.unwrap_err();
        assert!(matches!(e, Error::Cancelled));
        assert!(start.elapsed() < Duration::from_millis(300), "{:?}", start.elapsed());
        // already cancelled: no request at all
        let e = c.get_bytes(&format!("{base}/json"), &none(), 10, &tok).await.unwrap_err();
        assert!(matches!(e, Error::Cancelled));
    }

    #[tokio::test]
    async fn read_timeout_is_a_transient_network_error() {
        let base = server().await;
        let c = HttpClient::new(&HttpOptions { read_timeout_s: 1, ..Default::default() }).unwrap();
        let start = Instant::now();
        let e = c.get_json::<serde_json::Value>(&format!("{base}/hang?password=pwTimeout1"), &none(), &CancellationToken::new()).await.unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        assert!(matches!(e, Error::Network(_)) && e.is_transient(), "{e:?}");
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(!e.to_string().contains("pwTimeout1"));
    }

    #[tokio::test]
    async fn errors_never_contain_the_url_password() {
        let base = server().await;
        let secret = "pw-http-errors-Zx81q";
        let c = client();
        let tok = CancellationToken::new();
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let q = format!("username=someone&password={secret}");
        let urls = [
            format!("{closed}/player_api.php?{q}"),
            format!("{base}/missing?{q}"),
            format!("{base}/html?{q}"),
            format!("{base}/broken?{q}"),
            format!("http://someone:{secret}@127.0.0.1:1/x"),
            format!("{closed}/live/someone/{secret}/1.ts"),
            format!("not a url {secret}"),
        ];
        for url in &urls {
            let e = c.get_json::<serde_json::Value>(url, &none(), &tok).await.unwrap_err();
            let shown = format!("{e} {e:?}");
            assert!(!shown.contains(secret), "{shown}");
            let e = c.get_bytes(url, &none(), 2, &tok).await.unwrap_err();
            assert!(!e.to_string().contains(secret), "{e}");
            // (HTML and broken bodies download fine; the pipeline sniffs them afterwards)
            let dir = tempfile::tempdir().unwrap();
            if let Err(e) = c.download(&req(url.clone(), dir.path().join("f")), &|_, _| {}, &tok).await {
                assert!(!e.to_string().contains(secret), "{e}");
            }
        }
        let refused = c.get_bytes(&urls[0], &none(), 2, &tok).await.unwrap_err();
        assert!(matches!(refused, Error::Network(ref m) if m.contains("127.0.0.1")), "{refused}");
    }

    #[test]
    fn open_decompressed_by_magic_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let read = |name: &str, data: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, data).unwrap();
            let mut s = String::new();
            open_decompressed(&p).unwrap().read_to_string(&mut s).unwrap();
            s
        };
        // names lie on purpose: only the magic bytes count
        assert_eq!(read("a.xml", &gz(b"<tv>gz</tv>")), "<tv>gz</tv>");
        assert_eq!(read("b.gz", &xz(b"<tv>xz</tv>")), "<tv>xz</tv>");
        assert_eq!(read("c.xz", b"<tv>plain</tv>"), "<tv>plain</tv>");
        assert_eq!(read("tiny", b"#"), "#");
        assert_eq!(read("empty", b""), "");
        let mut multi = gz(b"first ");
        multi.extend(gz(b"second"));
        assert_eq!(read("multi.gz", &multi), "first second");
        let mut multi = xz(b"one ");
        multi.extend(xz(b"two"));
        assert_eq!(read("multi.xz", &multi), "one two");
        assert!(matches!(open_decompressed(&dir.path().join("nope")), Err(Error::Io { .. })));
        assert_eq!(sniff_compression(&gz(b"x")[..2]), Compression::Gzip);
    }

    #[test]
    fn json_start_trimming_and_part_paths() {
        assert_eq!(&trim_json_start(Bytes::from_static(b"\xEF\xBB\xBF \r\n[1]"))[..], b"[1]");
        assert_eq!(&trim_json_start(Bytes::from_static(b"  ")), &b""[..]);
        assert_eq!(part_path(Path::new("/c/epg/3.xml")), Path::new("/c/epg/3.xml.part"));
        assert_eq!(host_of("http://u:p@h.tv:8080/x?password=y").as_deref(), Some("h.tv:8080"));
        assert_eq!(host_of("nonsense"), None);
    }
}
