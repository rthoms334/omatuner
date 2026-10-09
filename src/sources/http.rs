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

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::model::HttpHeaders;
use crate::redact::Secret;

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
    pub async fn get_json<T: DeserializeOwned + Send + 'static>(
        &self,
        url: &str,
        headers: &HttpHeaders,
        cancel: &CancellationToken,
    ) -> Result<T> {
        todo!("WP-04")
    }

    /// GET small bodies (images, short API responses), bounded at `max_bytes`.
    pub async fn get_bytes(&self, url: &str, headers: &HttpHeaders, max_bytes: u64, cancel: &CancellationToken) -> Result<bytes::Bytes> {
        todo!("WP-04")
    }

    /// Stream `req.url` into `req.dest` (via `dest.part` + rename), reporting progress every
    /// ~256 KiB; sniff compression from the first bytes; honour ETag/Last-Modified.
    pub async fn download(&self, req: &DownloadRequest, progress: ProgressFn<'_>, cancel: &CancellationToken) -> Result<DownloadOutcome> {
        todo!("WP-04")
    }

    pub fn default_user_agent(&self) -> &str { &self.default_user_agent }
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
pub fn open_decompressed(path: &Path) -> Result<Box<dyn BufRead + Send>> { todo!("WP-04") }
