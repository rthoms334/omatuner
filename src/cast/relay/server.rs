//! The LAN media server (DESIGN §40 §9.1): axum (`http1`, `tokio`) bound to the address toward
//! the TV only (never 0.0.0.0), on `cast.media_port`, listening only while a session (or Test on
//! TV) exists. One fallback handler: `GET`/`HEAD` `/c/<tok>/(index.m3u8|init.mp4|s<n>.ts|.m4s)`;
//! every miss (path, method, token, peer) is the same 404. Correct MIME types, `Content-Length`,
//! `Range` → 206, `Cache-Control: no-cache` on playlists, CORS, no redirects, ≤ 8 connections,
//! 30 s idle. OWNER: CAST-4.
//!
//! This is Tuner's first LAN-facing listener (§9.3 threat model): a bearer token per generation
//! ([`super::token`]), one route family parsed by hand (no router, no directory listing, no way
//! to name a file outside the generation's directory or an upstream), a peer check that answers
//! like a wrong token (so the server is no oracle), and nothing listening when idle. Misses are
//! logged at debug without the path's token or the peer's address.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::Sleep;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use super::playlist::{self, Codec, FIRST_PICTURE_BYTES, Guard};
use super::token::{Grants, STOPPED_TTL, Token};
use super::{RelayEvent, renditions};

/// Concurrent connections per session (§9.1).
pub const MAX_CONNECTIONS: usize = 8;
/// A connection with no traffic this long is closed (§9.1).
pub const IDLE: Duration = Duration::from_secs(30);

/// What a served name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum File {
    Playlist,
    /// `init.mp4`, or a rendition's `init_<n>.mp4` (ffmpeg's `-var_stream_map` names, §8.4).
    Init(Option<u8>),
    /// `s<n>.ts` / `s<n>.m4s`: sequence number `n`.
    Segment(u64),
}

impl File {
    /// The file's name on disk (`fmp4`: the output's segment type).
    pub fn name(self, fmp4: bool) -> String {
        match self {
            File::Playlist => "index.m3u8".into(),
            File::Init(None) => "init.mp4".into(),
            File::Init(Some(n)) => format!("init_{n}.mp4"),
            File::Segment(n) => format!("s{n}.{}", if fmp4 { "m4s" } else { "ts" }),
        }
    }
}

/// `^(index\.m3u8|init(_\d)?\.mp4|s\d{1,7}\.(ts|m4s))$` (§9.1), by hand.
pub fn served_file(name: &str) -> Option<File> {
    match name {
        "index.m3u8" => return Some(File::Playlist),
        "init.mp4" => return Some(File::Init(None)),
        _ => {}
    }
    if let Some(n) = name.strip_prefix("init_").and_then(|r| r.strip_suffix(".mp4")) {
        return (n.len() == 1).then(|| n.parse().ok().map(|n| File::Init(Some(n)))).flatten();
    }
    let digits = name.strip_prefix('s')?.strip_suffix(".ts").or_else(|| name.strip_prefix('s')?.strip_suffix(".m4s"))?;
    if digits.is_empty() || digits.len() > 7 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().map(File::Segment)
}

/// `/c/<tok>/[<rendition>/]<file>` → `(tok, rendition, file)`; a rendition is one of the
/// surround output's fixed names (`v`, `a`, `s`). The raw path only: nothing is percent-decoded,
/// so `%2e%2e` or `%2f` never become a traversal; they simply don't match.
pub fn route(path: &str) -> Option<(&str, Option<&'static str>, File)> {
    let rest = path.strip_prefix("/c/")?;
    let (tok, file) = rest.split_once('/')?;
    if tok.is_empty() || !tok.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)) {
        return None;
    }
    let (rendition, file) = match file.split_once('/') {
        Some((r, f)) => ([renditions::VIDEO, renditions::STEREO, renditions::SURROUND].into_iter().find(|n| *n == r)?, f),
        None => return Some((tok, None, served_file(file)?)),
    };
    Some((tok, Some(rendition), served_file(file)?))
}

/// One media playlist of an output and its files: the only one (one stereo playlist, as ever),
/// or a rendition of a surround output (§8.4: `v` video, `a` stereo, `s` surround, each in its
/// own directory).
#[derive(Debug)]
pub struct Rendition {
    /// The URL and directory component (`None`: the output's own directory).
    pub name: Option<&'static str>,
    pub dir: PathBuf,
    pub guard: Mutex<Guard>,
    /// The codec of the IDR check (the video playlist; `None`: radio, audio renditions).
    pub codec: Option<Codec>,
    pub mime: &'static str,
    /// 1 + the highest segment the TV fetched (0: none yet): the VOD cap deletes behind it.
    pub fetched: AtomicU64,
}

impl Rendition {
    pub fn new(name: Option<&'static str>, dir: PathBuf, codec: Option<Codec>, mime: &'static str) -> Self {
        Self { name, dir, guard: Mutex::new(Guard::new(1, codec)), codec, mime, fetched: AtomicU64::new(0) }
    }

    /// ffmpeg's playlist through the guard: `(parsed, rendered for view_first)`. Reads only the
    /// new segments' first bytes (the IDR check).
    pub fn serve_playlist(&self, text: &str, view_first: Option<u64>, fmp4: bool) -> (playlist::MediaPlaylist, String) {
        let pl = playlist::parse(text);
        let mut g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        let dir = &self.dir;
        g.observe(&pl, |file, codec| {
            use std::io::Read;
            let mut head = Vec::with_capacity(FIRST_PICTURE_BYTES);
            let mut f = std::fs::File::open(dir.join(file)).ok()?;
            (&mut f).take(FIRST_PICTURE_BYTES as u64).read_to_end(&mut head).ok()?;
            playlist::first_picture_is_idr(&head, codec, fmp4)
        });
        let body = g.render(&pl, view_first);
        (pl, body)
    }

    /// Read ffmpeg's current playlist into the guard (what's produced, not only what the TV
    /// fetched).
    pub async fn refresh(&self, fmp4: bool) {
        if let Ok(text) = tokio::fs::read_to_string(self.dir.join("index.m3u8")).await {
            self.serve_playlist(&text, None, fmp4);
        }
    }
}

/// One generation's output as the server sees it (shared by its views).
#[derive(Debug)]
pub struct Output {
    /// The generation's directory.
    pub dir: PathBuf,
    /// fMP4 segments (HEVC, surround): `init*.mp4` + `s<n>.m4s`.
    pub fmp4: bool,
    /// The load generation events carry (a view may move it on).
    pub generation: Arc<AtomicU64>,
    /// The main playlist first (the video, or the only one), then the audio renditions.
    pub renditions: Vec<Rendition>,
    /// The multivariant playlist of a surround output, once its renditions are ready.
    pub master: Mutex<Option<String>>,
    irregular_sent: AtomicBool,
    overflow_sent: AtomicBool,
}

impl Output {
    pub fn new(dir: PathBuf, fmp4: bool, generation: Arc<AtomicU64>, renditions: Vec<Rendition>) -> Self {
        Self {
            dir,
            fmp4,
            generation,
            renditions,
            master: Mutex::new(None),
            irregular_sent: AtomicBool::new(false),
            overflow_sent: AtomicBool::new(false),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The main playlist (video, or the only one): positions, views, the guard's verdicts.
    pub fn main(&self) -> &Rendition {
        &self.renditions[0]
    }

    fn rendition(&self, name: Option<&str>) -> Option<(usize, &Rendition)> {
        self.renditions.iter().enumerate().find(|(_, r)| r.name == name)
    }

    /// The main playlist's verdicts, each reported once: `(irregular, target overflow)`.
    fn verdicts(&self) -> (bool, bool) {
        let g = self.main().guard.lock().unwrap_or_else(|e| e.into_inner());
        let irregular = g.irregular() && !self.irregular_sent.swap(true, Ordering::Relaxed);
        let overflow = g.overflow().is_some() && !g.irregular() && !self.overflow_sent.swap(true, Ordering::Relaxed);
        (irregular, overflow)
    }
}

/// What a token unlocks.
#[derive(Debug)]
pub struct Served {
    pub out: Arc<Output>,
    /// A playlist view's first segment per rendition (`None`: everything ffmpeg lists).
    pub view_first: Option<Vec<u64>>,
    playlist_seen: bool,
}

struct Shared {
    peer: Ipv4Addr,
    grants: Mutex<Grants<Served>>,
    tx: mpsc::UnboundedSender<RelayEvent>,
    label: Arc<str>,
}

/// The running LAN server of one session.
pub struct LanServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    kill: CancellationToken,
}

impl LanServer {
    /// Bind `bind:port` (port 0: any, for tests) and serve `peer` only.
    pub async fn bind(bind: Ipv4Addr, port: u16, peer: Ipv4Addr, tx: mpsc::UnboundedSender<RelayEvent>, label: Arc<str>) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((bind, port))).await?;
        let addr = listener.local_addr()?;
        let kill = CancellationToken::new();
        let shared = Arc::new(Shared { peer, grants: Mutex::new(Grants::default()), tx, label });
        let app = axum::Router::new().fallback(handle).with_state(shared.clone());
        let capped = Capped::new(listener, MAX_CONNECTIONS, peer, kill.clone());
        let stop = kill.clone();
        tokio::spawn(async move {
            let serve = axum::serve(capped, app.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stop.cancelled_owned());
            if let Err(e) = serve.await {
                tracing::debug!("cast: LAN server: {e}");
            }
        });
        Ok(Self { addr, shared, kill })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serve `out` under `token` and revoke every other token (one per generation: a zap, a
    /// seek or a reconnect revokes the old one, §9.2).
    pub fn grant_only(&self, token: Token, out: Arc<Output>, view_first: Option<Vec<u64>>) {
        let mut g = self.shared.grants.lock().unwrap_or_else(|e| e.into_inner());
        g.clear();
        g.insert(token, Served { out, view_first, playlist_seen: false });
    }

    pub fn revoke_all(&self) {
        self.shared.grants.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Close the listener and every connection now (a segment in flight is cut: the relay going
    /// away is the hard stop, §5.6).
    pub fn shutdown(&self) {
        self.revoke_all();
        self.kill.cancel();
    }
}

impl Drop for LanServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Every miss: the same status, headers and (empty) body.
fn not_found() -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    r
}

async fn handle(State(srv): State<Arc<Shared>>, ConnectInfo(Peer(from)): ConnectInfo<Peer>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    if !head && req.method() != Method::GET {
        return not_found();
    }
    if from.ip() != IpAddr::V4(srv.peer) {
        tracing::debug!("{}: 404 for a request from another host", srv.label);
        return not_found();
    }
    let Some((tok, rendition, file)) = route(req.uri().path()) else {
        tracing::debug!("{}: 404 for an unknown path", srv.label);
        return not_found();
    };
    let now = Instant::now();
    let (out, view_first) = {
        let mut grants = srv.grants.lock().unwrap_or_else(|e| e.into_inner());
        let Some(g) = grants.find_mut(tok) else {
            tracing::debug!("{}: 404 for an unknown or revoked token", srv.label);
            return not_found();
        };
        // §9.2: 60 s after the last fetch once playback has stopped, i.e. the item is fully
        // produced and the TV has fetched its last segment (it needs nothing more from us)
        let out = g.value.out.clone();
        let done = {
            let main = out.main();
            let guard = main.guard.lock().unwrap_or_else(|e| e.into_inner());
            guard.complete() && guard.last_seq().is_some_and(|last| main.fetched.load(Ordering::Relaxed) > last)
        };
        if done && g.last_fetch.is_some_and(|t| now.duration_since(t) >= STOPPED_TTL) {
            grants.revoke(|_| true);
            tracing::debug!("{}: the finished item's token expired", srv.label);
            return not_found();
        }
        g.last_fetch = Some(now);
        (out, g.value.view_first.clone())
    };
    let generation = out.generation();
    // a surround output's own top level holds only the multivariant playlist
    let master = if rendition.is_none() { out.master.lock().unwrap_or_else(|e| e.into_inner()).clone() } else { None };
    let rend = out.rendition(rendition);
    match file {
        File::Playlist => {
            let body = match (master, rend) {
                (Some(master), _) => master,
                (None, Some((index, rend))) => {
                    let Ok(text) = tokio::fs::read_to_string(rend.dir.join("index.m3u8")).await else { return not_found() };
                    let first = view_first.as_ref().and_then(|v| v.get(index).copied());
                    rend.serve_playlist(&text, first, out.fmp4).1
                }
                (None, None) => return not_found(),
            };
            let (irregular, overflow) = out.verdicts();
            if irregular {
                let _ = srv.tx.send(RelayEvent::Irregular { generation });
            }
            if overflow {
                let _ = srv.tx.send(RelayEvent::TargetOverflow { generation });
            }
            if !head && first_fetch(&srv, tok) {
                let _ = srv.tx.send(RelayEvent::PlaylistFetched { generation });
            }
            let mut r = full(head, body.into_bytes(), "application/vnd.apple.mpegurl");
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            r
        }
        File::Init(_) | File::Segment(_) => {
            let Some((index, rend)) = rend else { return not_found() };
            let Ok(data) = tokio::fs::read(rend.dir.join(file.name(out.fmp4))).await else { return not_found() };
            if let File::Segment(n) = file
                && !head
            {
                rend.fetched.fetch_max(n + 1, Ordering::Relaxed);
                // the main playlist's fetches are the position estimate's evidence (§7.3)
                let first = view_first.as_ref().and_then(|v| v.first().copied()).unwrap_or(0);
                let span = (index == 0).then(|| rend.guard.lock().unwrap_or_else(|e| e.into_inner()).span(n, first)).flatten();
                if let Some((from_s, upto_s)) = span {
                    let _ = srv.tx.send(RelayEvent::SegmentFetched { generation, from_s, upto_s });
                }
            }
            ranged(head, req.headers(), data, rend.mime)
        }
    }
}

/// The grant's first playlist fetch (the firewall watchdog's evidence).
fn first_fetch(srv: &Shared, tok: &str) -> bool {
    let mut grants = srv.grants.lock().unwrap_or_else(|e| e.into_inner());
    grants.find_mut(tok).is_some_and(|g| !std::mem::replace(&mut g.value.playlist_seen, true))
}

fn cors(h: &mut HeaderMap) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Range"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("Content-Length, Content-Range"));
}

fn full(head: bool, data: Vec<u8>, mime: &'static str) -> Response {
    let len = data.len();
    let mut r = Response::new(if head { Body::empty() } else { Body::from(data) });
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    cors(h);
    r
}

/// A file with `Range` support (AVPlayer starts with `bytes=0-1`, [AP 1.3]).
fn ranged(head: bool, req: &HeaderMap, data: Vec<u8>, mime: &'static str) -> Response {
    let total = data.len() as u64;
    let range = req.get(header::RANGE).and_then(|v| v.to_str().ok()).map(|v| parse_range(v, total));
    let mut r = match range {
        Some(Range::Unsatisfiable) => {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let h = r.headers_mut();
            h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{total}")).expect("ascii"));
            h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            cors(h);
            return r;
        }
        Some(Range::Bytes(a, b)) => {
            let part = data[a as usize..=b as usize].to_vec();
            let mut r = full(head, part, mime);
            *r.status_mut() = StatusCode::PARTIAL_CONTENT;
            r.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {a}-{b}/{total}")).expect("ascii"));
            r
        }
        Some(Range::Ignore) | None => full(head, data, mime),
    };
    r.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    r
}

/// A `Range` header against a file of `total` bytes (RFC 9110 §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Inclusive byte positions.
    Bytes(u64, u64),
    Unsatisfiable,
    /// Not a single byte range (several ranges, another unit, bad syntax): answered in full.
    Ignore,
}

pub fn parse_range(v: &str, total: u64) -> Range {
    let Some(spec) = v.trim().strip_prefix("bytes=") else { return Range::Ignore };
    if spec.contains(',') {
        return Range::Ignore;
    }
    let Some((a, b)) = spec.split_once('-') else { return Range::Ignore };
    let (a, b) = (a.trim(), b.trim());
    let parsed = match (a.is_empty(), b.is_empty()) {
        (true, true) => return Range::Ignore,
        // suffix: the last n bytes
        (true, false) => match b.parse::<u64>() {
            Ok(0) => return Range::Unsatisfiable,
            Ok(n) => (total.saturating_sub(n), total.saturating_sub(1)),
            Err(_) => return Range::Ignore,
        },
        (false, _) => {
            let Ok(start) = a.parse::<u64>() else { return Range::Ignore };
            let end = if b.is_empty() {
                total.saturating_sub(1)
            } else {
                match b.parse::<u64>() {
                    Ok(e) if e >= start => e.min(total.saturating_sub(1)),
                    _ => return Range::Ignore,
                }
            };
            (start, end)
        }
    };
    if total == 0 || parsed.0 >= total { Range::Unsatisfiable } else { Range::Bytes(parsed.0, parsed.1) }
}

/// The peer's address as the handler sees it (axum's `ConnectInfo` for [`Capped`]).
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub SocketAddr);

impl Connected<IncomingStream<'_, Capped>> for Peer {
    fn connect_info(stream: IncomingStream<'_, Capped>) -> Self {
        Peer(*stream.remote_addr())
    }
}

/// A listener that serves `peer` only, holds at most `n` of its connections (the next waits in
/// the kernel's backlog), closes any connection idle for [`IDLE`], and cuts them all when `kill`
/// fires (axum's graceful shutdown would let a long download finish).
///
/// A connection from any other host is closed as soon as it is accepted, before it takes a slot
/// or sends a byte: the ufw rule may admit the whole subnet, and a host that held the 8 slots
/// (idle, or dripping a byte under the idle limit) would stall the TV's fetches in the backlog
/// (§9.3). The handler's peer check stays as the second line.
pub struct Capped {
    inner: TcpListener,
    permits: Arc<Semaphore>,
    peer: Ipv4Addr,
    kill: CancellationToken,
}

impl Capped {
    pub fn new(inner: TcpListener, n: usize, peer: Ipv4Addr, kill: CancellationToken) -> Self {
        Self { inner, permits: Arc::new(Semaphore::new(n)), peer, kill }
    }
}

impl Listener for Capped {
    type Io = Conn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Conn, SocketAddr) {
        loop {
            match self.inner.accept().await {
                Ok((io, addr)) if addr.ip() != IpAddr::V4(self.peer) => {
                    drop(io);
                    tracing::debug!("cast: closed a connection from another host");
                }
                Ok((io, addr)) => {
                    // the receiver's connection waits here (accepted) while all its slots are busy
                    let permit = self.permits.clone().acquire_owned().await.expect("the semaphore is never closed");
                    let _ = io.set_nodelay(true);
                    return (Conn::new(io, permit, &self.kill), addr);
                }
                Err(e) => {
                    tracing::debug!("cast: accept: {e}");
                    // EMFILE and friends: back off instead of spinning
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// One accepted connection (see [`Capped`]).
pub struct Conn {
    io: TcpStream,
    _permit: OwnedSemaphorePermit,
    kill: Pin<Box<WaitForCancellationFutureOwned>>,
    idle: Pin<Box<Sleep>>,
}

impl Conn {
    fn new(io: TcpStream, permit: OwnedSemaphorePermit, kill: &CancellationToken) -> Self {
        Self { io, _permit: permit, kill: Box::pin(kill.clone().cancelled_owned()), idle: Box::pin(tokio::time::sleep(IDLE)) }
    }

    fn touch(&mut self) {
        self.idle.as_mut().reset(tokio::time::Instant::now() + IDLE);
    }

    /// Killed, or idle too long (checked when the socket has nothing to do).
    fn dead(&mut self, cx: &mut Context<'_>, pending: bool) -> Option<io::Error> {
        if self.kill.as_mut().poll(cx).is_ready() {
            return Some(io::ErrorKind::ConnectionAborted.into());
        }
        (pending && self.idle.as_mut().poll(cx).is_ready()).then(|| io::ErrorKind::TimedOut.into())
    }
}

impl AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(e) = this.dead(cx, false) {
            return Poll::Ready(Err(e));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.io).poll_read(cx, buf) {
            Poll::Ready(r) => {
                if buf.filled().len() > before {
                    this.touch();
                }
                Poll::Ready(r)
            }
            Poll::Pending => match this.dead(cx, true) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(e) = this.dead(cx, false) {
            return Poll::Ready(Err(e));
        }
        match Pin::new(&mut this.io).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.touch();
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.dead(cx, true) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(e) = this.dead(cx, false) {
            return Poll::Ready(Err(e));
        }
        match Pin::new(&mut this.io).poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    this.touch();
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.dead(cx, true) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_matcher_admits_one_family_only() {
        let tok = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(route(&format!("/c/{tok}/index.m3u8")), Some((tok, None, File::Playlist)));
        assert_eq!(route(&format!("/c/{tok}/init.mp4")), Some((tok, None, File::Init(None))));
        assert_eq!(route(&format!("/c/{tok}/s0.ts")), Some((tok, None, File::Segment(0))));
        assert_eq!(route(&format!("/c/{tok}/s9999999.m4s")), Some((tok, None, File::Segment(9_999_999))));
        // a surround output's renditions (§8.4)
        assert_eq!(route(&format!("/c/{tok}/v/index.m3u8")), Some((tok, Some("v"), File::Playlist)));
        assert_eq!(route(&format!("/c/{tok}/a/init_1.mp4")), Some((tok, Some("a"), File::Init(Some(1)))));
        assert_eq!(route(&format!("/c/{tok}/s/s12.m4s")), Some((tok, Some("s"), File::Segment(12))));
        for bad in [
            format!("/c/{tok}/w/index.m3u8"),
            format!("/c/{tok}/v/v/index.m3u8"),
            format!("/c/{tok}/v/../index.m3u8"),
            format!("/c/{tok}/init_12.mp4"),
            format!("/c/{tok}/init_.mp4"),
            format!("/c/{tok}/v/"),
            format!("/c/{tok}/s10000000.ts"),
            format!("/c/{tok}/s.ts"),
            format!("/c/{tok}/s1.TS"),
            format!("/c/{tok}/s-1.ts"),
            format!("/c/{tok}/s1.ts/"),
            format!("/c/{tok}/../index.m3u8"),
            format!("/c/{tok}/%2e%2e/index.m3u8"),
            format!("/c/{tok}/x/s1.ts"),
            format!("/c/{tok}/index.m3u8x"),
            format!("/c/{tok}/"),
            format!("/c/{tok}"),
            "/c//index.m3u8".to_owned(),
            format!("/c/{}/index.m3u8", tok.to_uppercase()),
            format!("/u/{tok}/index.m3u8"),
            format!("//c/{tok}/index.m3u8"),
            "/".to_owned(),
        ] {
            assert_eq!(route(&bad), None, "{bad}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-1", 100), Range::Bytes(0, 1));
        assert_eq!(parse_range("bytes=10-", 100), Range::Bytes(10, 99));
        assert_eq!(parse_range("bytes=90-200", 100), Range::Bytes(90, 99));
        assert_eq!(parse_range("bytes=-10", 100), Range::Bytes(90, 99));
        assert_eq!(parse_range("bytes=-200", 100), Range::Bytes(0, 99));
        assert_eq!(parse_range("bytes=100-", 100), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Range::Ignore);
        assert_eq!(parse_range("items=0-1", 100), Range::Ignore);
        assert_eq!(parse_range("bytes=5-1", 100), Range::Ignore);
    }
}
