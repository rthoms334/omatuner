//! The loopback upstream proxy (DESIGN §40 §8.1.1): the only code that talks to the provider
//! (reqwest with `Playable.headers`), one upstream request per token at a time, real HTTP
//! statuses and provider text reported, a ≤ 16 MiB head buffer replayed to ffprobe then ffmpeg,
//! HLS upstream reduced to one variant with URIs rewritten to `/u/<tok>/r/<n>`. Never puts a
//! provider URL on a command line. OWNER: CAST-4.
//!
//! ```text
//!  provider ──(one request at a time: Wire)──▶ Tape (bytes of one upstream response, from `start`)
//!                                                │ head mode: kept from byte 0 (≤ 16 MiB), replayed
//!                                                │ trim mode (ffmpeg attached): dropped behind it
//!  ffprobe, then ffmpeg ◀── GET 127.0.0.1:P/u/<tok>[/r/<n>] (Range) ◀────────┘
//! ```
//! * **One upstream request at a time** (§10). Every upstream request goes through the feed's
//!   [`Wire`]: a new one cancels the one in flight and waits for its connection to close first.
//!   reqwest keeps no idle connection (`pool_max_idle_per_host(0)`), so a finished request leaves
//!   nothing open either. "Closed" is the socket's close, not the end of the request: reqwest
//!   closes a connection on hyper's connection task, some time after its response is dropped (or
//!   read to the end), so every provider connection of a session lives on the session's own
//!   runtime ([`Net`]), and a request connects only once no task is left there.
//! * **Head buffer.** A stream's first response is kept from byte 0 (up to [`HEAD_CAP`]; the pump
//!   then waits) so ffprobe and then ffmpeg read the same first bytes from one provider
//!   connection. When the relay arms the final reader ([`Feed::arm_final_reader`]), the next
//!   request is ffmpeg's: the buffer is dropped behind it as it reads.
//! * **Range.** A request inside what a tape holds (or is about to) is served from it; any other
//!   offset cancels the tape and opens the provider again with `Range` (ffmpeg seeking a film,
//!   `-ss`). A reader that reaches the end of a cancelled tape continues on a new request from
//!   where it stopped, in the same response, so ffmpeg (`-reconnect 0`) never sees a gap.
//! * **One client at a time.** Each request supersedes the previous reader, whose response
//!   ends with an error.
//! * **A film's stream that breaks mid-body resumes** where it broke: a sized resource the
//!   provider serves by `Range` (VOD) is asked again from the byte the reader reached, in the
//!   same response to ffmpeg, up to [`RESUMES`] times for a feed, each after progress (the
//!   2026-10-13 film's provider closed its connection 30 s in; that had cost a reconnect and a
//!   reload on the TV). Live and HLS segments fail as before (ffmpeg skips a broken segment; a
//!   live stream reconnects through the controller's policy).
//! * **Why a stream broke** is logged as its cause ([`cause`]): reqwest files every failure to
//!   read a body as "error decoding response body", a provider closing its connection too.
//! * **HLS upstream.** The proxy picks one variant (mpv's, approximated by its height, else the
//!   highest bandwidth the receiver plays), fetches its media playlist itself and serves ffmpeg a
//!   copy whose URIs (segments, `EXT-X-KEY`, `EXT-X-MAP`) are `…/u/<tok>/r/<n>.<ext>`, `n` indexing a
//!   table of `Secret` upstream URLs. ffmpeg's HLS demuxer would otherwise open several variants
//!   at once and break a one-connection account.
//! * **What the controller learns** (§11.2): `UpstreamOpened` on the first 2xx, `Http` for a
//!   provider's error status, `ProviderText` for prose where the stream should be (the
//!   provider-message trap), and exactly one `UpstreamClosed` per feed ([`Feed::close`]), which
//!   releases picture A's connection lease.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::RelayEvent;
use super::plan::Profile;
use super::server::{Capped, Peer, Range, parse_range};
use super::token::{Grants, Token};
use crate::model::HttpHeaders;
use crate::redact::{Secret, redact};

/// The head buffer (§8.1.1): bytes kept from byte 0 until ffmpeg attaches.
pub const HEAD_CAP: usize = 16 << 20;
/// Bytes a tape may run ahead of its reader once trimming.
const AHEAD_CAP: u64 = 16 << 20;
/// A request this far past a filling tape's end waits for it instead of re-opening.
const LOOKAHEAD: u64 = 1 << 20;
/// Largest playlist accepted from the provider.
const MAX_PLAYLIST: usize = 4 << 20;
/// Provider text kept for `ProviderText` (characters).
const MAX_TEXT: usize = 200;
/// Bytes sniffed before deciding what the first response is.
const SNIFF: usize = 512;
/// Toward the provider, the player's default `network-timeout`: for the connect, for the reply
/// to a request, and for each body read on its own ([`read_chunk`]), so a pump held back by its
/// reader (a TV paused at the VOD cap with ffmpeg SIGSTOPped, the head-mode attach wait) never
/// counts as the provider going silent.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);
/// `close` waits this long for the upstream connection to go (stop-before-start, §8.5), and a
/// new request this long for the previous one's socket to close.
pub const CLOSE_WAIT: Duration = Duration::from_millis(1500);
/// How often [`Net::closed`] looks again while a connection is still closing.
const CLOSE_POLL: Duration = Duration::from_millis(1);
/// A cached rewritten playlist is served again for this long (ffprobe → ffmpeg hand-over).
const PLAYLIST_REUSE: Duration = Duration::from_secs(1);
/// Upstream URLs remembered for one HLS feed (a live window rotates through them).
const URI_TABLE_CAP: usize = 4096;
/// Chunk handed to a reader at once.
const READ_CHUNK: usize = 64 * 1024;
/// A film's stream resumes after breaking this many times per feed, each after at least
/// [`RESUME_PROGRESS`] bytes since the last (a provider that drops it at once again is down).
const RESUMES: u32 = 3;
const RESUME_PROGRESS: u64 = 1 << 20;

/// What the events of a feed carry and where they go.
#[derive(Clone)]
pub struct FeedCtx {
    pub generation: Arc<AtomicU64>,
    pub tx: mpsc::UnboundedSender<RelayEvent>,
    /// `cast session <id>` for logs.
    pub label: Arc<str>,
}

impl FeedCtx {
    fn send(&self, ev: impl FnOnce(u64) -> RelayEvent) {
        let _ = self.tx.send(ev(self.generation.load(Ordering::Relaxed)));
    }
}

/// The loopback proxy of one relay session (127.0.0.1, any port).
pub struct Proxy {
    addr: SocketAddr,
    feeds: Arc<Mutex<Grants<Arc<Feed>>>>,
    client: reqwest::Client,
    net: Net,
    kill: CancellationToken,
}

impl Proxy {
    pub async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let addr = listener.local_addr()?;
        let client = reqwest::Client::builder()
            // no `read_timeout`: its clock runs from the last frame, so a body polled again after
            // backpressure would fail at once; reads are timed one by one instead
            .connect_timeout(UPSTREAM_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(10))
            // no idle connection to the provider: a finished request leaves nothing open
            .pool_max_idle_per_host(0)
            // the bytes and their lengths as the provider sends them (Range must stay exact)
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .build()
            .map_err(io::Error::other)?;
        let feeds: Arc<Mutex<Grants<Arc<Feed>>>> = Arc::new(Mutex::new(Grants::default()));
        let kill = CancellationToken::new();
        let net = Net::start(kill.clone()).await?;
        let app = axum::Router::new().fallback(handle).with_state(feeds.clone());
        let capped = Capped::new(listener, super::server::MAX_CONNECTIONS, Ipv4Addr::LOCALHOST, kill.clone());
        let stop = kill.clone();
        tokio::spawn(async move {
            let serve = axum::serve(capped, app.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stop.cancelled_owned());
            if let Err(e) = serve.await {
                tracing::debug!("cast: loopback proxy: {e}");
            }
        });
        Ok(Self { addr, feeds, client, net, kill })
    }

    /// One generation's feed under a new loopback token. Nothing is requested yet: the caller
    /// keeps the feed (so a load dropped mid-way can still end it), then calls [`Feed::open`].
    pub fn feed(&self, ctx: FeedCtx, url: &Secret<String>, headers: &HttpHeaders, user_agent: &str) -> Arc<Feed> {
        let token = Token::mint();
        let base = format!("http://{}/u/{}", self.addr, token.expose());
        let feed = Arc::new(Feed {
            ctx,
            client: self.client.clone(),
            net: self.net.clone(),
            closing: CancellationToken::new(),
            url: url.clone(),
            headers: request_headers(headers, user_agent),
            base: Secret::new(base),
            kind: Mutex::new(Kind::Pending),
            wire: tokio::sync::Mutex::new(Wire::default()),
            tapes: Mutex::new(Vec::new()),
            head: AtomicBool::new(true),
            armed: AtomicBool::new(false),
            attached: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            state: Mutex::new(FeedState::default()),
            changed: Notify::new(),
            encrypted: AtomicBool::new(false),
        });
        self.feeds.lock().unwrap_or_else(|e| e.into_inner()).insert(token, feed.clone());
        feed
    }

    /// Stop serving `feed` (its URL 404s from now on).
    pub fn forget(&self, feed: &Arc<Feed>) {
        self.feeds.lock().unwrap_or_else(|e| e.into_inner()).revoke(|g| Arc::ptr_eq(&g.value, feed));
    }

    /// Stop serving and cancel every feed's upstream requests (no pump outlives the session).
    pub fn shutdown(&self) {
        self.feeds.lock().unwrap_or_else(|e| e.into_inner()).revoke(|g| {
            g.value.cancel_all();
            true
        });
        self.kill.cancel();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The runtime a session's provider connections live on (§10), on a thread of its own.
///
/// reqwest hands a connection's socket to hyper's connection task, which closes it once the
/// response is dropped or read to the end: later, on another task. A request that waited only for
/// the previous one's response to go would connect while that socket is still open (the task
/// that dropped the response wakes the one that waits first, and the connection task runs after
/// it). Requests are sent from here, so their connection tasks are spawned here too, and no task
/// alive on this runtime means no provider socket open. Ending the runtime (the proxy's shutdown)
/// drops whatever is left, which closes it.
#[derive(Clone)]
struct Net {
    handle: tokio::runtime::Handle,
}

impl Net {
    /// Start the thread; it ends (dropping every connection left) when `stop` is cancelled.
    async fn start(stop: CancellationToken) -> io::Result<Self> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new().name("cast-upstream".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => {
                    let _ = tx.send(Ok(rt.handle().clone()));
                    rt.block_on(stop.cancelled());
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })?;
        let handle = rx.await.map_err(io::Error::other)??;
        Ok(Self { handle })
    }

    /// Send `req` from this runtime. Dropping the future aborts the request: its socket closes
    /// with its task, which [`Net::closed`] waits for.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let task = tokio_util::task::AbortOnDropHandle::new(self.handle.spawn(req.send()));
        match task.await {
            Ok(r) => r.map_err(|e| cause(&e)),
            Err(_) => Err("the request was dropped".to_owned()),
        }
    }

    /// Resolves once every provider connection of the session is closed.
    async fn closed(&self) {
        while self.handle.metrics().num_alive_tasks() > 0 {
            tokio::time::sleep(CLOSE_POLL).await;
        }
    }
}

/// How to choose among an HLS upstream's variants.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pick {
    /// The receiver's profile (`None`: any codec).
    pub profile: Option<Profile>,
    /// The height mpv was playing (the hints), if known.
    pub height: Option<u32>,
}

/// The `User-Agent` (the playlist's, else the configured default), `Referer`, `Origin` and the
/// extra headers (cookies, tokens: never logged) of a `Playable`.
fn request_headers(h: &HttpHeaders, default_ua: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    let mut put = |name: HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v.trim()) {
            out.insert(name, v);
        }
    };
    let ua = h.user_agent.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or(default_ua);
    if !ua.trim().is_empty() {
        put(header::USER_AGENT, ua);
    }
    if let Some(r) = h.referrer.as_deref().filter(|s| !s.trim().is_empty()) {
        put(header::REFERER, r);
    }
    if let Some(o) = h.origin.as_deref().filter(|s| !s.trim().is_empty()) {
        put(header::ORIGIN, o);
    }
    for (k, v) in &h.extra {
        if let Ok(name) = HeaderName::from_bytes(k.trim().as_bytes()) {
            put(name, v);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// The playable's own URL (a stream).
    Main,
    /// Entry `n` of the HLS URI table.
    Res(u32),
}

enum Kind {
    Pending,
    Stream,
    Hls(Box<Hls>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Eof,
    /// Cancelled by a newer request (the bytes held are still good).
    Cut,
    /// The connection failed.
    Failed,
}

#[derive(Default)]
struct TapeState {
    /// Where the response started in the resource.
    origin: u64,
    /// Absolute offset of `chunks[0]`.
    base: u64,
    chunks: VecDeque<Bytes>,
    held: u64,
    /// The resource's length, when the provider said.
    total: Option<u64>,
    content_type: Option<HeaderValue>,
    ranges: bool,
    end: Option<End>,
    /// The final reader's position (trim mode drops what's behind it).
    reader: u64,
    keep_head: bool,
}

impl TapeState {
    fn end_offset(&self) -> u64 {
        self.base + self.held
    }

    fn append(&mut self, b: Bytes) {
        self.held += b.len() as u64;
        self.chunks.push_back(b);
    }

    /// Bytes from `pos` (at most `max`); `pos` must be inside.
    fn read(&self, pos: u64, max: usize) -> Bytes {
        let mut off = pos - self.base;
        for c in &self.chunks {
            let len = c.len() as u64;
            if off < len {
                let n = ((len - off) as usize).min(max);
                return c.slice(off as usize..off as usize + n);
            }
            off -= len;
        }
        Bytes::new()
    }

    /// Drop whole chunks behind the reader (trim mode). The head buffer itself goes only once
    /// the reader has passed it (§8.1.1): ffmpeg seeks back into its first megabytes while it
    /// probes (a duration estimate reads the end, then returns to the start).
    fn trim(&mut self) {
        if self.keep_head || self.reader <= self.origin + HEAD_CAP as u64 {
            return;
        }
        while let Some(c) = self.chunks.front() {
            let len = c.len() as u64;
            if self.base + len > self.reader {
                break;
            }
            self.base += len;
            self.held -= len;
            self.chunks.pop_front();
        }
    }
}

/// One upstream response's bytes.
struct Tape {
    key: Key,
    st: Mutex<TapeState>,
    changed: Notify,
    cancel: CancellationToken,
}

impl Tape {
    fn lock(&self) -> std::sync::MutexGuard<'_, TapeState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serves a request for `key` at `pos`: inside what it holds, or (still filling) just ahead.
    fn covers(&self, key: Key, pos: u64) -> bool {
        if self.key != key {
            return false;
        }
        let s = self.lock();
        if pos < s.base {
            // a live stream (no length) starts wherever it is now
            return s.total.is_none() && s.end.is_none() && self.key == Key::Main && !s.keep_head;
        }
        pos < s.end_offset() || (s.end.is_none() && pos <= s.end_offset() + LOOKAHEAD) || (s.end == Some(End::Eof) && pos == s.end_offset())
    }

    fn finish(&self, end: End) {
        let mut s = self.lock();
        s.end.get_or_insert(end);
        drop(s);
        self.changed.notify_waiters();
    }
}

/// The single upstream request in flight.
#[derive(Default)]
struct Wire {
    pump: Option<(Arc<Tape>, JoinHandle<()>)>,
}

impl Wire {
    /// Cancel the request in flight and wait until its pump has let go of the response (its
    /// socket closes after that, on its connection task: [`Net::closed`]).
    async fn hang_up(&mut self) {
        if let Some((tape, task)) = self.pump.take() {
            tape.cancel.cancel();
            let _ = task.await;
        }
    }
}

/// An HLS upstream reduced to one media playlist.
struct Hls {
    /// The chosen media playlist (after redirects).
    playlist: Secret<String>,
    uris: UriTable,
    /// The latest rewritten playlist and when it was fetched.
    cached: Option<(Instant, String)>,
}

/// Upstream URLs by index (`/u/<tok>/r/<n>`).
#[derive(Default)]
pub struct UriTable {
    by_url: HashMap<String, u32>,
    urls: BTreeMap<u32, Secret<String>>,
    next: u32,
}

impl UriTable {
    pub fn index(&mut self, url: &str) -> u32 {
        if let Some(n) = self.by_url.get(url) {
            return *n;
        }
        let n = self.next;
        self.next = self.next.wrapping_add(1);
        self.by_url.insert(url.to_owned(), n);
        self.urls.insert(n, Secret::new(url.to_owned()));
        if self.urls.len() > URI_TABLE_CAP
            && let Some((_, old)) = self.urls.pop_first()
        {
            self.by_url.remove(old.expose());
        }
        n
    }

    pub fn get(&self, n: u32) -> Option<&Secret<String>> {
        self.urls.get(&n)
    }
}

#[derive(Default)]
struct FeedState {
    opened: bool,
    failed: bool,
    closed: bool,
    http_sent: bool,
    /// Mid-body resumes so far, and where the last one started.
    resumes: u32,
    resumed_at: Option<u64>,
}

/// One generation's upstream.
pub struct Feed {
    ctx: FeedCtx,
    client: reqwest::Client,
    net: Net,
    /// Cancelled by [`Feed::close`] and [`Feed::cancel_all`]: a request connecting, or a playlist
    /// being read, gives up at once.
    closing: CancellationToken,
    url: Secret<String>,
    headers: HeaderMap,
    /// `http://127.0.0.1:P/u/<tok>`: the loopback URL (a bearer token while the session lives).
    base: Secret<String>,
    kind: Mutex<Kind>,
    wire: tokio::sync::Mutex<Wire>,
    tapes: Mutex<Vec<Arc<Tape>>>,
    /// Keep tapes from byte 0 (until the final reader attaches).
    head: AtomicBool,
    /// The next request is ffmpeg's.
    armed: AtomicBool,
    attached: AtomicBool,
    /// The current reader (each request supersedes the previous one).
    epoch: AtomicU64,
    state: Mutex<FeedState>,
    changed: Notify,
    encrypted: AtomicBool,
}

impl Feed {
    /// The loopback URL ffprobe and ffmpeg read.
    pub fn loopback_url(&self) -> &Secret<String> {
        &self.base
    }

    /// The upstream is HLS (ffmpeg gets `-http_multiple 0`).
    pub fn is_hls(&self) -> bool {
        matches!(*self.kind.lock().unwrap_or_else(|e| e.into_inner()), Kind::Hls(_))
    }

    /// An HLS key that isn't AES-128 (`SAMPLE-AES`, a DRM `KEYFORMAT`): refused.
    pub fn encrypted(&self) -> bool {
        self.encrypted.load(Ordering::Relaxed)
    }

    /// The provider failed (an error status, prose, a dead connection): the generation is over.
    pub fn failed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).failed
    }

    /// Resolves once the feed has failed.
    pub async fn wait_failed(&self) {
        loop {
            let n = self.changed.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.failed() {
                return;
            }
            n.await;
        }
    }

    /// The next request is ffmpeg's: the head buffer is dropped behind it as it reads.
    pub fn arm_final_reader(&self) {
        self.attached.store(false, Ordering::Relaxed);
        self.armed.store(true, Ordering::Relaxed);
    }

    /// ffmpeg has made its first request (the 10 s attach watchdog, §8.1.1).
    pub fn final_attached(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    /// May a film's stream that broke at `pos` resume there (module docs)?
    fn may_resume(&self, pos: u64) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let progressed = st.resumed_at.is_none_or(|at| pos >= at + RESUME_PROGRESS);
        if st.failed || st.resumes >= RESUMES || !progressed || self.closing.is_cancelled() {
            return false;
        }
        st.resumes += 1;
        st.resumed_at = Some(pos);
        true
    }

    fn fail(&self, status: Option<u16>, text: Option<String>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.http_sent {
            if let Some(status) = status {
                self.ctx.send(|generation| RelayEvent::Http { generation, status });
                st.http_sent = true;
            }
            if let Some(text) = text {
                self.ctx.send(|generation| RelayEvent::ProviderText { generation, text });
                st.http_sent = true;
            }
        }
        st.failed = true;
        drop(st);
        for t in self.tapes.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            t.cancel.cancel();
        }
        self.changed.notify_waiters();
    }

    /// Cancel every upstream request now (no waiting, no event: `Drop`'s path).
    pub fn cancel_all(&self) {
        self.closing.cancel();
        for t in self.tapes.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            t.cancel.cancel();
        }
    }

    /// End the feed: cancel the request in flight, wait (≤ [`CLOSE_WAIT`], unless `fast`) for
    /// its socket to close, then report `UpstreamClosed` (once), which frees the lease.
    pub async fn close(&self, fast: bool) {
        self.cancel_all();
        if !fast {
            let gone = async {
                self.wire.lock().await.hang_up().await;
                self.net.closed().await;
            };
            if tokio::time::timeout(CLOSE_WAIT, gone).await.is_err() {
                tracing::info!("{}: the provider connection is still closing after {CLOSE_WAIT:?}", self.ctx.label);
            }
        }
        let first = !std::mem::replace(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()).closed, true);
        if first {
            let at = Instant::now();
            self.ctx.send(|generation| RelayEvent::UpstreamClosed { generation, at });
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    fn opened(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !std::mem::replace(&mut st.opened, true) {
            self.ctx.send(|generation| RelayEvent::UpstreamOpened { generation });
        }
    }

    /// Every request carries `Range: bytes=<from>-`, from 0 too, as ffmpeg's `http` sends it (so
    /// the provider sees what mpv's requests show it): a 206 or `Accept-Ranges: bytes` tells
    /// the tape the provider honours ranges; a 200 without either, that it doesn't, and ffmpeg is
    /// then told the input can't seek ([`serve_bytes`]).
    fn request(&self, url: &str, from: u64) -> reqwest::RequestBuilder {
        self.client.get(url).headers(self.headers.clone()).header(header::RANGE, format!("bytes={from}-"))
    }

    /// Send on the wire, after hanging up the request in flight and once its socket has closed.
    /// Statuses ≥ 400 are reported and fail the feed when `fatal` says so.
    async fn send(&self, wire: &mut Wire, url: &str, from: u64, fatal: impl Fn(u16) -> bool) -> Option<reqwest::Response> {
        wire.hang_up().await;
        // the provider sees the previous socket close before this one connects (§10)
        if tokio::time::timeout(CLOSE_WAIT, self.net.closed()).await.is_err() {
            tracing::info!("{}: the previous provider connection is still closing after {CLOSE_WAIT:?}", self.ctx.label);
        }
        // ending (`cancel_all` comes before `close` marks the feed closed): nothing new opens, not
        // even a reader continuing past its cut tape
        if self.closing.is_cancelled() {
            return None;
        }
        let sent = tokio::select! {
            biased;
            // the request is aborted with its socket
            () = self.closing.cancelled() => return None,
            r = tokio::time::timeout(UPSTREAM_TIMEOUT, self.net.send(self.request(url, from))) => match r {
                Ok(r) => r,
                Err(_) => Err(format!("no answer within {} s", UPSTREAM_TIMEOUT.as_secs())),
            },
        };
        match sent {
            // ending while connecting: dropping the response closes the connection
            Ok(_) if self.closing.is_cancelled() => None,
            Ok(r) if r.status().is_success() => Some(r),
            Ok(r) => {
                let status = r.status().as_u16();
                tracing::info!("{}: the provider answered HTTP {status}", self.ctx.label);
                if fatal(status) {
                    self.fail(Some(status), None);
                }
                None
            }
            Err(why) => {
                tracing::info!("{}: the provider can't be reached: {why}", self.ctx.label);
                if fatal(0) {
                    self.fail(None, None);
                }
                None
            }
        }
    }

    /// Open the provider: the first request decides what the upstream is (`UpstreamOpened` /
    /// `Http` / `ProviderText`, and for HLS the variant pick). Check [`Feed::failed`] after.
    pub async fn open(self: &Arc<Self>, pick: Pick) {
        let mut wire = self.wire.lock().await;
        let url = self.url.expose().clone();
        let Some(mut resp) = self.send(&mut wire, &url, 0, |_| true).await else { return };
        self.opened();
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
        let mut head = Vec::with_capacity(SNIFF);
        let mut eof = false;
        while head.len() < SNIFF {
            match read_chunk(&mut resp).await {
                Ok(Some(b)) => head.extend_from_slice(&b),
                Ok(None) => {
                    eof = true;
                    break;
                }
                Err(why) => {
                    tracing::info!("{}: the provider's stream broke: {why}", self.ctx.label);
                    self.fail(None, None);
                    return;
                }
            }
        }
        let ct_str = ct.as_ref().and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        if is_playlist(&head) {
            let final_url = resp.url().to_string();
            let Some(text) = read_rest(resp, head, eof).await else {
                self.fail(None, None);
                return;
            };
            self.start_hls(&mut wire, final_url, &text, pick).await;
            return;
        }
        if let Some(text) = prose(&ct_str, &head) {
            tracing::warn!("{}: the provider answered with a message instead of a stream: {text:?}", self.ctx.label);
            self.fail(None, Some(text));
            return;
        }
        *self.kind.lock().unwrap_or_else(|e| e.into_inner()) = Kind::Stream;
        let tape = self.new_tape(Key::Main, 0, &resp);
        {
            let mut s = tape.lock();
            s.append(Bytes::from(head));
            if eof {
                s.end = Some(End::Eof);
            }
        }
        if !eof {
            self.start_pump(&mut wire, tape, resp);
        }
    }

    /// A multivariant playlist becomes its chosen variant's media playlist.
    async fn start_hls(&self, wire: &mut Wire, mut url: String, text: &str, pick: Pick) {
        let mut text = text.to_owned();
        if text.contains("#EXT-X-STREAM-INF") {
            let base = url::Url::parse(&url).ok();
            let vs = variants(&text);
            let Some(v) = choose_variant(&vs, pick) else {
                tracing::info!("{}: the provider's playlist lists no variant", self.ctx.label);
                self.fail(None, None);
                return;
            };
            let Some(next) = base.as_ref().and_then(|b| b.join(&vs[v].uri).ok()) else {
                self.fail(None, None);
                return;
            };
            tracing::debug!("{}: HLS variant {} of {} ({} bit/s)", self.ctx.label, v + 1, vs.len(), vs[v].bandwidth);
            let Some(resp) = self.send(wire, next.as_str(), 0, |_| true).await else { return };
            url = resp.url().to_string();
            match read_rest(resp, Vec::new(), false).await {
                Some(t) if !t.contains("#EXT-X-STREAM-INF") => text = t,
                _ => {
                    self.fail(None, None);
                    return;
                }
            }
        }
        let mut hls = Hls { playlist: Secret::new(url), uris: UriTable::default(), cached: None };
        let rewritten = self.rewrite(&mut hls, &text);
        hls.cached = rewritten.map(|t| (Instant::now(), t));
        *self.kind.lock().unwrap_or_else(|e| e.into_inner()) = Kind::Hls(Box::new(hls));
    }

    /// ffmpeg's copy of an upstream media playlist (see [`rewrite_media`]).
    fn rewrite(&self, hls: &mut Hls, text: &str) -> Option<String> {
        let base = url::Url::parse(hls.playlist.expose()).ok()?;
        let (out, encrypted) = rewrite_media(text, &base, &mut hls.uris, self.base.expose());
        if encrypted {
            self.encrypted.store(true, Ordering::Relaxed);
        }
        Some(out)
    }

    fn new_tape(&self, key: Key, start: u64, resp: &reqwest::Response) -> Arc<Tape> {
        let h = resp.headers();
        let len = h.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let range_total = h.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()?.rsplit('/').next()?.parse::<u64>().ok());
        // a 200 to a ranged request starts at 0 (the provider ignored Range)
        let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let start = if partial { start } else { 0 };
        let st = TapeState {
            base: start,
            total: range_total.or(len.map(|l| l + start)),
            content_type: h.get(header::CONTENT_TYPE).cloned(),
            ranges: partial || h.get(header::ACCEPT_RANGES).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("bytes")),
            origin: start,
            reader: start,
            keep_head: self.head.load(Ordering::Relaxed),
            ..TapeState::default()
        };
        let tape = Arc::new(Tape { key, st: Mutex::new(st), changed: Notify::new(), cancel: CancellationToken::new() });
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).closed {
            tape.cancel.cancel();
        }
        let mut tapes = self.tapes.lock().unwrap_or_else(|e| e.into_inner());
        if self.head.load(Ordering::Relaxed) {
            // head mode keeps earlier tapes for ffmpeg's replay, within the cap
            let mut held: u64 = tapes.iter().map(|t| t.lock().held).sum();
            while held > HEAD_CAP as u64 && !tapes.is_empty() {
                held -= tapes.remove(0).lock().held;
            }
        } else {
            tapes.clear();
        }
        tapes.push(tape.clone());
        tape
    }

    fn start_pump(&self, wire: &mut Wire, tape: Arc<Tape>, resp: reqwest::Response) {
        let ctx = self.ctx.clone();
        let t = tape.clone();
        let task = tokio::spawn(pump(t, resp, ctx));
        wire.pump = Some((tape, task));
    }

    /// The tape that serves `key` at `pos`, opening the provider when none does.
    async fn tape_for(&self, key: Key, pos: u64) -> Result<Arc<Tape>, StatusCode> {
        if let Some(t) = self.find_tape(key, pos) {
            return Ok(t);
        }
        let url = match key {
            Key::Main => self.url.expose().clone(),
            Key::Res(n) => match &*self.kind.lock().unwrap_or_else(|e| e.into_inner()) {
                Kind::Hls(h) => h.uris.get(n).map(|u| u.expose().clone()).ok_or(StatusCode::NOT_FOUND)?,
                _ => return Err(StatusCode::NOT_FOUND),
            },
        };
        let mut wire = self.wire.lock().await;
        // another request may have opened it while this one waited for the wire
        if let Some(t) = self.find_tape(key, pos) {
            return Ok(t);
        }
        // a segment that's gone (404) or broken is skipped by ffmpeg, as mpv does; a refusal
        // (401/403/429/451) or the main stream failing ends the feed
        let fatal = move |s: u16| key == Key::Main || matches!(s, 401 | 403 | 429 | 451);
        let Some(resp) = self.send(&mut wire, &url, pos, fatal).await else { return Err(StatusCode::BAD_GATEWAY) };
        let tape = self.new_tape(key, pos, &resp);
        self.start_pump(&mut wire, tape.clone(), resp);
        Ok(tape)
    }

    fn find_tape(&self, key: Key, pos: u64) -> Option<Arc<Tape>> {
        let tapes = self.tapes.lock().unwrap_or_else(|e| e.into_inner());
        tapes.iter().rev().find(|t| t.covers(key, pos)).cloned()
    }

    /// A new reader: it supersedes the previous one, and the armed final reader ends head mode.
    fn new_reader(&self) -> u64 {
        if self.armed.swap(false, Ordering::Relaxed) {
            self.head.store(false, Ordering::Relaxed);
            self.attached.store(true, Ordering::Relaxed);
            for t in self.tapes.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                let mut s = t.lock();
                s.keep_head = false;
                s.reader = s.base;
                drop(s);
                t.changed.notify_waiters();
            }
        }
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        self.changed.notify_waiters();
        epoch
    }

    fn current(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::Relaxed) == epoch
    }

    /// The rewritten media playlist (fetched again unless just fetched).
    async fn playlist(&self) -> Result<String, StatusCode> {
        let url = match &*self.kind.lock().unwrap_or_else(|e| e.into_inner()) {
            Kind::Hls(h) => {
                if let Some((at, text)) = &h.cached
                    && at.elapsed() < PLAYLIST_REUSE
                {
                    return Ok(text.clone());
                }
                h.playlist.expose().clone()
            }
            _ => return Err(StatusCode::NOT_FOUND),
        };
        let mut wire = self.wire.lock().await;
        let Some(resp) = self.send(&mut wire, &url, 0, |_| true).await else { return Err(StatusCode::BAD_GATEWAY) };
        // read on the wire: no other request connects while this one's is open
        let text = tokio::select! {
            t = read_rest(resp, Vec::new(), false) => t,
            () = self.closing.cancelled() => return Err(StatusCode::BAD_GATEWAY),
        };
        drop(wire);
        let Some(text) = text else {
            self.fail(None, None);
            return Err(StatusCode::BAD_GATEWAY);
        };
        if !text.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U") {
            let msg = prose("", text.as_bytes());
            tracing::warn!("{}: the provider's playlist became a message: {msg:?}", self.ctx.label);
            self.fail(None, msg);
            return Err(StatusCode::BAD_GATEWAY);
        }
        let mut kind = self.kind.lock().unwrap_or_else(|e| e.into_inner());
        let Kind::Hls(h) = &mut *kind else { return Err(StatusCode::NOT_FOUND) };
        let base = url::Url::parse(h.playlist.expose()).map_err(|_| StatusCode::BAD_GATEWAY)?;
        let (out, encrypted) = rewrite_media(&text, &base, &mut h.uris, self.base.expose());
        if encrypted {
            self.encrypted.store(true, Ordering::Relaxed);
        }
        h.cached = Some((Instant::now(), out.clone()));
        Ok(out)
    }
}

impl Drop for Feed {
    /// The last holder let go: its pumps stop too (a pump parked on backpressure would otherwise
    /// hold the provider connection until Tuner exits).
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// Read a tape into the provider: until EOF, cancellation, failure, or (backpressure) until the
/// reader catches up.
async fn pump(tape: Arc<Tape>, mut resp: reqwest::Response, ctx: FeedCtx) {
    loop {
        // backpressure: the head is capped; in trim mode, so is the lead over the reader
        loop {
            let n = tape.changed.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            let full = {
                let s = tape.lock();
                if s.keep_head { s.held >= HEAD_CAP as u64 } else { s.end_offset().saturating_sub(s.reader) >= AHEAD_CAP }
            };
            if !full {
                break;
            }
            tokio::select! {
                () = tape.cancel.cancelled() => {
                    tape.finish(End::Cut);
                    return;
                }
                () = n => {}
            }
        }
        let chunk = tokio::select! {
            () = tape.cancel.cancelled() => {
                tape.finish(End::Cut);
                return;
            }
            c = read_chunk(&mut resp) => c,
        };
        match chunk {
            Ok(Some(b)) => {
                let mut s = tape.lock();
                s.append(b);
                s.trim();
                drop(s);
                tape.changed.notify_waiters();
            }
            Ok(None) => {
                tape.finish(End::Eof);
                return;
            }
            Err(why) => {
                tracing::info!("{}: the provider's stream broke: {why}", ctx.label);
                tape.finish(End::Failed);
                return;
            }
        }
    }
}

/// One body chunk, its wait timed on its own ([`UPSTREAM_TIMEOUT`]): the clock starts when the
/// caller asks, after any backpressure. `Err` is a redacted reason: our own timeout says
/// "nothing for 15 s", anything else is the provider's side ([`cause`]).
async fn read_chunk(resp: &mut reqwest::Response) -> Result<Option<Bytes>, String> {
    match tokio::time::timeout(UPSTREAM_TIMEOUT, resp.chunk()).await {
        Ok(r) => r.map_err(|e| cause(&e)),
        Err(_) => Err(format!("nothing for {} s", UPSTREAM_TIMEOUT.as_secs())),
    }
}

/// Why a request or a body failed, redacted and without the URL: the causes under reqwest's
/// label. `Response::chunk` labels every failure to read a body "error decoding response body"
/// (reqwest 0.13.5, `async_impl/response.rs`: `res.map_err(crate::error::decode)`), whatever
/// it was; the client sets no read timeout (reads are timed by [`read_chunk`]), so what's under it
/// is hyper's word for the connection: "error reading a body from connection: end of file before
/// message length reached" (the provider hung up with bytes owed), "…: connection reset by peer".
fn cause(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut parts: Vec<String> = Vec::new();
    let mut next = e.source();
    while let Some(c) = next {
        let s = c.to_string();
        if parts.last() != Some(&s) {
            parts.push(s);
        }
        next = c.source();
    }
    let text = if parts.is_empty() { e.to_string() } else { parts.join(": ") };
    let text = if e.is_timeout() && !text.contains("timed out") { format!("timed out: {text}") } else { text };
    redact(&text).into_owned()
}

/// The rest of a small body (a playlist), capped.
async fn read_rest(mut resp: reqwest::Response, mut buf: Vec<u8>, eof: bool) -> Option<String> {
    if !eof {
        loop {
            match read_chunk(&mut resp).await {
                Ok(Some(b)) => {
                    buf.extend_from_slice(&b);
                    if buf.len() > MAX_PLAYLIST {
                        return None;
                    }
                }
                Ok(None) => break,
                Err(_) => return None,
            }
        }
    }
    String::from_utf8(buf).ok()
}

/// An HLS playlist: its first line is `#EXTM3U` (whatever the MIME type says: a restreamer's
/// "Cannot read …" comes as `application/vnd.apple.mpegurl` too).
fn is_playlist(head: &[u8]) -> bool {
    String::from_utf8_lossy(head).trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U")
}

/// The provider's prose where a stream should be ("Max connections reached", an HTML error
/// page): `text/*`, or bytes that are all printable text. Collapsed, tags stripped, at most
/// [`MAX_TEXT`] characters, redacted. Binary media (TS sync bytes, MP4 boxes) never qualifies.
pub fn prose(content_type: &str, head: &[u8]) -> Option<String> {
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        // a multi-byte character cut at the sniff boundary
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    let printable = !text.is_empty() && text.chars().all(|c| !c.is_control() || c.is_whitespace());
    if !content_type.starts_with("text/") && !printable {
        return None;
    }
    let mut out = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if in_tag => {}
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let joined: String = out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TEXT).collect();
    (!joined.is_empty()).then(|| redact(&joined).into_owned())
}

/// One `#EXT-X-STREAM-INF` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub height: Option<u32>,
    pub codecs: Option<String>,
}

/// The variants of a multivariant playlist, in order.
pub fn variants(text: &str) -> Vec<Variant> {
    let mut out = Vec::new();
    let mut pending: Option<Variant> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = attributes(a);
            let get = |k: &str| attrs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
            pending = Some(Variant {
                uri: String::new(),
                bandwidth: get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                height: get("RESOLUTION").and_then(|r| r.split_once(['x', 'X'])?.1.parse().ok()),
                codecs: get("CODECS").map(str::to_owned),
            });
        } else if !line.starts_with('#')
            && let Some(mut v) = pending.take()
        {
            v.uri = line.to_owned();
            out.push(v);
        }
    }
    out
}

/// The variant to fetch: mpv's (the hint's height), else the highest bandwidth the receiver
/// plays as is, else the highest bandwidth (L3 converts it).
pub fn choose_variant(vs: &[Variant], pick: Pick) -> Option<usize> {
    let playable = |v: &Variant| match (pick.profile, v.codecs.as_deref()) {
        (Some(p), Some(codecs)) => codecs.split(',').map(str::trim).all(|c| match rfc6381_video(c) {
            Some(name) => p.video.contains(&name),
            None => true, // audio (always convertible) or unknown non-video
        }),
        _ => true,
    };
    let best = |f: &dyn Fn(&Variant) -> bool| vs.iter().enumerate().filter(|(_, v)| f(v)).max_by_key(|(_, v)| v.bandwidth).map(|(i, _)| i);
    pick.height
        .and_then(|h| best(&|v: &Variant| v.height == Some(h) && playable(v)))
        .or_else(|| best(&|v: &Variant| playable(v)))
        .or_else(|| best(&|_: &Variant| true))
}

/// The ffmpeg name of an RFC 6381 video codec string (`avc1.640028` → `h264`); `None` for audio
/// and unknown entries.
fn rfc6381_video(c: &str) -> Option<&'static str> {
    let fourcc = c.split('.').next().unwrap_or(c);
    match fourcc {
        "avc1" | "avc3" => Some("h264"),
        "hvc1" | "hev1" => Some("hevc"),
        "dvh1" | "dvhe" => Some("dolby_vision"),
        "vp09" | "vp9" => Some("vp9"),
        "av01" => Some("av1"),
        "mp4v" => Some("mpeg4"),
        _ => None,
    }
}

/// The extension of a URL path when ffmpeg's HLS demuxer accepts it for a segment.
fn media_ext(path: &str) -> Option<&'static str> {
    const ALLOWED: &[&str] = &[
        "3gp", "aac", "ac3", "avi", "cmfa", "cmfv", "eac3", "flac", "m2t", "m2ts", "m4a", "m4s", "m4v", "mkv", "mov", "mp2", "mp3", "mp4", "mpeg",
        "mpegts", "mpg", "mts", "oga", "ogg", "ogv", "ts", "vob", "wav",
    ];
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    ALLOWED.iter().find(|a| **a == ext).copied()
}

/// `KEY=VALUE,KEY="quoted, value"` attribute lists (RFC 8216 §4.2).
fn attributes(s: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(end) => (&q[..end], q[end + 1..].trim_start_matches(',')),
                None => (q, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end + 1..]),
                None => (after, ""),
            }
        };
        out.push((name.trim(), value));
        rest = next;
    }
    out
}

/// Rewrite an upstream media playlist for ffmpeg: every URI (segment lines and `URI="…"`
/// attributes) resolved against `base` and replaced with `proxy/r/<n>`. Returns the text and
/// whether a key needs DRM (`SAMPLE-AES*`, or a `KEYFORMAT` other than `identity`). Low-latency
/// parts and hints are dropped (ffmpeg reads whole segments).
pub fn rewrite_media(text: &str, base: &url::Url, table: &mut UriTable, proxy: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut encrypted = false;
    // ffmpeg's HLS demuxer opens only names with a media extension (`allowed_segment_extensions`)
    // and checks it against what it probes, so each URI keeps its upstream extension
    let map = |uri: &str, table: &mut UriTable, default: &str| {
        let u = base.join(uri).ok()?;
        let ext = media_ext(u.path()).unwrap_or(default).to_owned();
        Some(format!("{proxy}/r/{}.{ext}", table.index(u.as_str())))
    };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !line.starts_with('#') {
            if let Some(u) = map(line, table, "ts") {
                out.push_str(&u);
                out.push('\n');
            }
            continue;
        }
        let name = line.split_once(':').map_or(line, |(n, _)| n);
        if matches!(name, "#EXT-X-PART" | "#EXT-X-PRELOAD-HINT" | "#EXT-X-RENDITION-REPORT" | "#EXT-X-PART-INF" | "#EXT-X-SERVER-CONTROL") {
            continue;
        }
        if name == "#EXT-X-KEY" {
            let attrs = attributes(line.split_once(':').map_or("", |(_, a)| a));
            let get = |k: &str| attrs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
            let method = get("METHOD").unwrap_or("NONE");
            let identity = get("KEYFORMAT").is_none_or(|f| f == "identity");
            encrypted |= method.starts_with("SAMPLE-AES") || (method != "NONE" && !identity);
        }
        match line.find("URI=\"") {
            Some(i) => {
                let after = &line[i + 5..];
                let Some(end) = after.find('"') else { continue };
                let default = if name == "#EXT-X-MAP" { "mp4" } else { "key" };
                let Some(u) = map(&after[..end], table, default) else { continue };
                out.push_str(&line[..i + 5]);
                out.push_str(&u);
                out.push_str(&after[end..]);
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    (out, encrypted)
}

// ---- the loopback server -------------------------------------------------------------------

/// Every miss: the same empty 404.
fn not_found() -> Response {
    status(StatusCode::NOT_FOUND)
}

fn status(code: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = code;
    r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    r
}

async fn handle(State(feeds): State<Arc<Mutex<Grants<Arc<Feed>>>>>, req: Request) -> Response {
    if req.method() != Method::GET {
        return not_found();
    }
    let path = req.uri().path();
    let Some(rest) = path.strip_prefix("/u/") else { return not_found() };
    let (tok, key) = match rest.split_once('/') {
        None => (rest, Key::Main),
        Some((tok, r)) => match r.strip_prefix("r/").and_then(|n| n.split_once('.').map_or(n, |(n, _)| n).parse::<u32>().ok()) {
            Some(n) => (tok, Key::Res(n)),
            None => return not_found(),
        },
    };
    let Some(feed) = feeds.lock().unwrap_or_else(|e| e.into_inner()).find_mut(tok).map(|g| g.value.clone()) else { return not_found() };
    if feed.state.lock().unwrap_or_else(|e| e.into_inner()).closed {
        return not_found();
    }
    let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let epoch = feed.new_reader();
    let hls = feed.is_hls();
    match (hls, key) {
        (true, Key::Main) => match feed.playlist().await {
            Ok(text) => {
                let mut r = Response::new(Body::from(text));
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));
                r
            }
            Err(code) => status(code),
        },
        (false, Key::Res(_)) => not_found(),
        _ => serve_bytes(feed, key, range.as_deref(), epoch).await,
    }
}

/// A stream or an HLS resource from its tape(s), from the requested offset on.
async fn serve_bytes(feed: Arc<Feed>, key: Key, range: Option<&str>, epoch: u64) -> Response {
    // the offset only; an end is honoured by length (ffmpeg asks open-ended)
    let (from, upto) = range
        .and_then(|r| r.trim().strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok())))
        .unwrap_or((0, None));
    let tape = match feed.tape_for(key, from).await {
        Ok(t) => t,
        Err(code) => return status(code),
    };
    let (total, ct, ranges) = {
        let s = tape.lock();
        (s.total, s.content_type.clone(), s.ranges)
    };
    if let Some(t) = total
        && parse_range(&format!("bytes={from}-"), t) == Range::Unsatisfiable
        && t > 0
    {
        let mut resp = status(StatusCode::RANGE_NOT_SATISFIABLE);
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{t}")) {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        return resp;
    }
    let last = match (total, upto) {
        (Some(t), Some(u)) => Some(u.min(t.saturating_sub(1))),
        (Some(t), None) => Some(t.saturating_sub(1)),
        (None, Some(u)) => Some(u),
        (None, None) => None,
    };
    let limit = last.map(|l| l + 1 - from);
    let body = reader(feed, tape, from, limit, epoch);
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    if let Some(ct) = ct {
        h.insert(header::CONTENT_TYPE, ct);
    }
    if ranges {
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(n) = limit {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    // seekable only when the provider is: a 206 for a provider that ignores Range would have
    // ffmpeg seek to bytes the proxy can only reach by buffering everything before them
    if range.is_some()
        && ranges
        && let (Some(t), Some(l)) = (total, last)
    {
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {from}-{l}/{t}")) {
            resp.headers_mut().insert(header::CONTENT_RANGE, v);
        }
    }
    resp
}

/// A tape a broken read may resume: the main stream of a sized resource the provider serves by
/// `Range`.
fn resumable(tape: &Tape) -> bool {
    let s = tape.lock();
    tape.key == Key::Main && s.total.is_some() && s.ranges
}

struct ReadState {
    feed: Arc<Feed>,
    tape: Arc<Tape>,
    pos: u64,
    left: Option<u64>,
    epoch: u64,
    done: bool,
}

/// The response body of one reader: bytes from `pos`, across tapes when one is cut, until the
/// end, `limit`, a newer reader, or a failure (an error: hyper aborts the response, so the
/// reader sees a broken transfer rather than a clean end).
fn reader(feed: Arc<Feed>, tape: Arc<Tape>, pos: u64, limit: Option<u64>, epoch: u64) -> Body {
    let st = ReadState { feed, tape, pos, left: limit, epoch, done: false };
    Body::from_stream(futures_util::stream::unfold(st, |mut s| async move {
        if s.done || s.left == Some(0) {
            return None;
        }
        match next_chunk(&mut s).await {
            Ok(Some(b)) => Some((Ok::<Bytes, io::Error>(b), s)),
            Ok(None) => None,
            Err(e) => {
                s.done = true;
                Some((Err(e), s))
            }
        }
    }))
}

async fn next_chunk(s: &mut ReadState) -> io::Result<Option<Bytes>> {
    loop {
        if !s.feed.current(s.epoch) {
            return Err(io::Error::other("superseded by a newer reader"));
        }
        let (feed, tape) = (s.feed.clone(), s.tape.clone());
        let feed_changed = feed.changed.notified();
        let tape_changed = tape.changed.notified();
        tokio::pin!(feed_changed, tape_changed);
        feed_changed.as_mut().enable();
        tape_changed.as_mut().enable();
        let mut moved = false;
        let next = {
            let mut st = s.tape.lock();
            if s.pos < st.base {
                if st.total.is_none() && s.tape.key == Key::Main {
                    s.pos = st.base; // a live stream: whatever is current
                } else {
                    return Err(io::Error::other("behind the buffer"));
                }
            }
            if s.pos < st.end_offset() {
                let max = s.left.map_or(READ_CHUNK, |l| (l as usize).min(READ_CHUNK));
                let b = st.read(s.pos, max);
                s.pos += b.len() as u64;
                if let Some(l) = &mut s.left {
                    *l -= b.len() as u64;
                }
                if !st.keep_head {
                    st.reader = s.pos;
                    st.trim();
                }
                Some(Ok(b))
            } else {
                // waiting ahead of what's there: the pump's lead is measured from here, or a full
                // tape (16 MiB past where the previous reader stopped) would never fill up to us
                let at = s.pos.min(st.end_offset());
                if !st.keep_head && st.end.is_none() && at > st.reader {
                    st.reader = at;
                    st.trim();
                    moved = true;
                }
                match st.end {
                    None => None,
                    Some(End::Eof) => Some(Err(None)),
                    Some(End::Failed) => Some(Err(Some(End::Failed))),
                    Some(End::Cut) => Some(Err(Some(End::Cut))),
                }
            }
        };
        match next {
            Some(Ok(b)) => {
                s.tape.changed.notify_waiters(); // the pump may be waiting for room
                return Ok(Some(b));
            }
            Some(Err(None)) => return Ok(None),
            Some(Err(Some(End::Cut))) if !s.feed.failed() => {
                // continue on a new request from here (same response)
                s.tape = s.feed.tape_for(s.tape.key, s.pos).await.map_err(|_| io::Error::other("the provider stopped"))?;
                continue;
            }
            Some(Err(Some(End::Failed))) if resumable(&s.tape) && s.feed.may_resume(s.pos) => {
                // a film's connection dropped: the same bytes from where it broke (module docs)
                tracing::info!("{}: resuming the provider's stream at byte {}", s.feed.ctx.label, s.pos);
                s.tape = s.feed.tape_for(s.tape.key, s.pos).await.map_err(|_| io::Error::other("the provider stopped"))?;
                continue;
            }
            Some(Err(Some(_))) => {
                s.feed.fail(None, None);
                return Err(io::Error::other("the provider's stream broke"));
            }
            None if moved => s.tape.changed.notify_waiters(), // room for the pump; look again
            None => {
                tokio::select! {
                    () = &mut feed_changed => {}
                    () = &mut tape_changed => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:4\n\
        #EXT-X-STREAM-INF:BANDWIDTH=80000000,RESOLUTION=3840x2160,CODECS=\"vp09.00.50.08,mp4a.40.2\"\nuhd/vp9.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\nhd/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=12000000,RESOLUTION=1920x1080,CODECS=\"hvc1.2.4.L123.B0,ec-3\"\nhd/hevc.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=960x540\nsd/index.m3u8\n";

    /// A provider that hangs up mid-body (as the 2026-10-13 film's did, 30 s in) is logged as
    /// that, not as reqwest's "error decoding response body", and our own read timeout can't be
    /// the cause (the client has none).
    #[tokio::test]
    async fn a_body_cut_by_the_provider_says_so() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n").await;
            let _ = sock.write_all(&[0x47; 1000]).await;
            // the provider hangs up with 99 000 bytes owed
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut resp = client.get(format!("http://{addr}/movie/u/p/1.mkv")).send().await.unwrap();
        let why = loop {
            match read_chunk(&mut resp).await {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("a cut body ended cleanly"),
                Err(why) => break why,
            }
        };
        assert_eq!(why, "error reading a body from connection: end of file before message length reached");
        assert!(!why.contains("decoding") && !why.contains("timed out") && !why.contains("/movie/"), "{why}");
    }

    #[test]
    fn the_variant_is_mpvs_else_the_best_the_receiver_plays() {
        let vs = variants(MASTER);
        assert_eq!(vs.len(), 4);
        assert_eq!((vs[1].bandwidth, vs[1].height), (6_000_000, Some(1080)));
        let airplay = Some(super::super::plan::APPLE_TV);
        assert_eq!(choose_variant(&vs, Pick { profile: airplay, height: None }), Some(2), "HEVC 12M; never the VP9 one");
        assert_eq!(choose_variant(&vs, Pick { profile: airplay, height: Some(540) }), Some(3), "what mpv was playing");
        assert_eq!(choose_variant(&vs, Pick { profile: airplay, height: Some(720) }), Some(2), "no 720p: the best playable");
        assert_eq!(choose_variant(&vs, Pick { profile: None, height: None }), Some(0));
        assert_eq!(choose_variant(&vs[..1], Pick { profile: airplay, height: None }), Some(0), "only VP9: L3 converts it");
        assert_eq!(choose_variant(&[], Pick::default()), None);
    }

    #[test]
    fn rewritten_playlists_name_only_the_proxy() {
        let media = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:812\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/k?token=s3cr3t\",IV=0x01\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-PART:DURATION=1,URI=\"p1.m4s\"\n\
            #EXTINF:6.0,\n812.ts?sig=abc\n#EXTINF:6.0,\n/live/u/p/813.ts\n#EXTINF:6.0,\nhttp://cdn.example/814.ts\n";
        let base = url::Url::parse("http://provider.example:8080/live/u/p/index.m3u8").unwrap();
        let mut table = UriTable::default();
        let (out, encrypted) = rewrite_media(media, &base, &mut table, "http://127.0.0.1:4000/u/tok");
        assert!(!encrypted, "AES-128 with an identity key is ffmpeg's to decrypt");
        assert!(!out.contains("example") && !out.contains("s3cr3t") && !out.contains("sig="), "{out}");
        assert!(out.contains("#EXT-X-KEY:METHOD=AES-128,URI=\"http://127.0.0.1:4000/u/tok/r/0.key\",IV=0x01\n"), "{out}");
        assert!(out.contains("#EXT-X-MAP:URI=\"http://127.0.0.1:4000/u/tok/r/1.mp4\"\n"), "{out}");
        assert!(out.contains("#EXTINF:6.0,\nhttp://127.0.0.1:4000/u/tok/r/2.ts\n") && !out.contains("EXT-X-PART"), "{out}");
        assert_eq!(table.get(2).map(|u| u.expose().as_str()), Some("http://provider.example:8080/live/u/p/812.ts?sig=abc"));
        assert_eq!(table.get(3).map(|u| u.expose().as_str()), Some("http://provider.example:8080/live/u/p/813.ts"));
        assert_eq!(table.get(4).map(|u| u.expose().as_str()), Some("http://cdn.example/814.ts"));
        // the same upstream segment keeps its index on the next reload
        let (again, _) = rewrite_media("#EXTM3U\n#EXTINF:6.0,\n812.ts?sig=abc\n", &base, &mut table, "http://127.0.0.1:4000/u/tok");
        assert!(again.ends_with("/r/2.ts\n"), "{again}");
        let (php, _) = rewrite_media("#EXTM3U\n#EXTINF:6.0,\nseg.php?id=9\n", &base, &mut table, "http://127.0.0.1:4000/u/tok");
        assert!(php.ends_with("/r/5.ts\n"), "no media extension upstream: named as TS for ffmpeg: {php}");

        let fairplay = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:6,\na.ts\n";
        assert!(rewrite_media(fairplay, &base, &mut table, "http://127.0.0.1:1/u/t").1);
        let widevine = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"data:x\",KEYFORMAT=\"urn:uuid:edef8ba9\"\n#EXTINF:6,\na.ts\n";
        assert!(rewrite_media(widevine, &base, &mut table, "http://127.0.0.1:1/u/t").1);
    }

    #[test]
    fn provider_prose_is_told_apart_from_media() {
        assert_eq!(
            prose("", b"Cannot read /home/nxt/storage/streams/717661_.m3u8\n").as_deref(),
            Some("Cannot read /home/nxt/storage/streams/717661_.m3u8")
        );
        assert_eq!(prose("text/html", b"<html><body><h1>Max connections</h1>\n reached</body></html>").as_deref(), Some("Max connections reached"));
        let ts = [0x47u8, 0x40, 0x00, 0x10, 0x00, 0x00, 0xb0, 0x0d];
        assert_eq!(prose("video/mp2t", &ts), None);
        assert_eq!(prose("", b"\x00\x00\x00\x20ftypisom"), None);
        assert_eq!(prose("application/octet-stream", b""), None);
        assert!(is_playlist(b"\xef\xbb\xbf#EXTM3U\n") && !is_playlist(&ts));
        assert!(!is_playlist(b"Cannot read /x.m3u8"), "prose under a playlist MIME type");
    }

    #[test]
    fn attribute_lists_with_quoted_commas() {
        let a = attributes("BANDWIDTH=1,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=640x360");
        assert_eq!(a, vec![("BANDWIDTH", "1"), ("CODECS", "avc1.4d401f,mp4a.40.2"), ("RESOLUTION", "640x360")]);
    }
}
