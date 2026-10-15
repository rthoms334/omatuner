//! One provider reader per picture, unchanged HLS bytes, bounded disk, loopback playback.
//! The controller holds the usual connection lease until `Drop` joins the reader and closes
//! its provider sockets. mpv never knows the provider URL. No probe or remux is involved.
mod playlist;
mod store;

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::cast::relay::server::{Capped, Peer};
use crate::cast::relay::token::Token;
use crate::cast::relay::upstream::{Net, Pick, choose_variant, media_ext, request_headers, variants};
use crate::model::{HttpHeaders, Playable, TileId};
use crate::player::reconnect::ReconnectPolicy;
use crate::player::{LiveBufferRange, PlayerErrorKind};
use crate::redact::Secret;
use playlist::{Resource, attr};
use store::{Store, Track};

pub const WINDOW: f64 = 1800.0;
const RESERVE: u64 = 3_000_000_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SEGMENT: usize = 128 << 20;
const MPV_OPTIONS: &str = "force-media-title=Live,cache=yes,cache-on-disk=no,demuxer-seekable-cache=yes,force-seekable=yes,demuxer-donate-buffer=no,cache-secs=130,demuxer-readahead-secs=130,demuxer-max-bytes=128MiB,demuxer-max-back-bytes=128MiB,demuxer-lavf-o=live_start_index=-1,stream-lavf-o=reconnect=0";

/// HLS channel routing is intentionally separate from TS's existing mpv cache transport.
pub fn eligible(p: &Playable) -> bool {
    p.kind.is_live()
        && url::Url::parse(p.url.expose()).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.path().to_ascii_lowercase().ends_with(".m3u8"))
}

#[derive(Clone, Copy)]
pub struct Snapshot {
    pub range: Option<LiveBufferRange>,
    pub memory: bool,
    pub epoch: u64,
    pub files: usize,
    pub bytes: u64,
    pub origin: f64,
    pub next_view: Option<f64>,
}

#[derive(Default)]
struct View {
    id: u64,
    from: Option<f64>,
    origin: Option<f64>,
    next: Option<f64>,
    opened: Option<Instant>,
    served: [Option<u64>; 2],
}

struct Shared {
    store: Mutex<Store>,
    changed: Notify,
    token: Token,
    // A selected variant's separate default audio rendition, if any.
    audio: Mutex<bool>,
    view: Mutex<View>,
    errors: crossbeam_channel::Sender<PlayerErrorKind>,
}

impl Shared {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Buffer {
    shared: Arc<Shared>,
    url: Secret<String>,
    stop: CancellationToken,
    thread: Option<JoinHandle<()>>,
    pub errors: crossbeam_channel::Receiver<PlayerErrorKind>,
}

impl Buffer {
    /// Starts the local listener immediately. Its first playlist request waits for the first
    /// provider window; neither the controller nor mpv opens any other provider connection.
    pub fn start(p: &Playable, cache: &Path, tile: TileId, user_agent: &str, window: f64) -> io::Result<Self> {
        let store = match super::timeshift::make_dir(cache, tile) {
            Ok(dir) => Store::new(dir, window),
            Err(_) => {
                tracing::info!("live rewind: disk unavailable; using up to two minutes in memory until the next channel");
                Store::memory(window)
            }
        };
        let (errors_tx, errors_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            store: Mutex::new(store),
            changed: Notify::new(),
            token: Token::mint(),
            audio: Mutex::new(false),
            view: Mutex::new(View::default()),
            errors: errors_tx,
        });
        let stop = CancellationToken::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let state = shared.clone();
        let kill = stop.clone();
        let url = p.url.clone();
        let headers = p.headers.clone();
        let ua = user_agent.to_owned();
        let thread = std::thread::Builder::new().name("live-hls".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            rt.block_on(async {
                let listener = match tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let addr = match listener.local_addr() {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let net_stop = CancellationToken::new();
                let mut client = match Client::new(&headers, &ua, net_stop.clone()).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let app = axum::Router::new().fallback(serve).with_state(state.clone());
                let capped = Capped::new(listener, 8, Ipv4Addr::LOCALHOST, kill.clone());
                let server = tokio::spawn(async move {
                    let _ = axum::serve(capped, app.into_make_service_with_connect_info::<Peer>()).await;
                });
                let local = format!("http://{addr}/{}/0.m3u8", state.token.expose());
                let _ = tx.send(Ok(local));
                tokio::select! {
                    () = kill.cancelled() => {},
                    () = record(&state, &mut client, url) => {},
                }
                // The dropped request/body must relinquish its actual socket before the
                // controller can release this picture's lease and start the next provider.
                client.net.closed().await;
                net_stop.cancel();
                server.abort();
                let mut s = state.store();
                s.closed = true;
                s.reset();
                state.changed.notify_waiters();
            });
        })?;
        match rx.recv().map_err(io::Error::other)? {
            Ok(url) => Ok(Self { shared, url: Secret::new(url), stop, thread: Some(thread), errors: errors_rx }),
            Err(e) => {
                stop.cancel();
                let _ = thread.join();
                Err(e)
            }
        }
    }

    pub fn url(&self) -> &str {
        self.url.expose()
    }
    /// A new local reader, anchored at a retained segment. Reopening only localhost makes
    /// seeking independent of FFmpeg's stale playlist after a long pause or a discontinuity.
    /// The recorder and the provider lease keep running unchanged.
    pub fn view(&self, target: Option<f64>) -> (String, f64) {
        let s = self.shared.store();
        let target = target.zip(s.range()).map(|(p, range)| p.clamp(range.start, range.end));
        let from =
            target.and_then(|p| s.tracks[0].segments.iter().find(|seg| seg.end > p).or_else(|| s.tracks[0].segments.back()).map(|seg| seg.start));
        let mut v = self.shared.view.lock().unwrap_or_else(|e| e.into_inner());
        v.id += 1;
        v.from = from;
        v.origin = None;
        v.next = None;
        v.opened = None;
        v.served = [None; 2];
        let offset = target.zip(from).map_or(0.0, |(p, from)| (p - from).max(0.0));
        tracing::debug!(view = v.id, ?target, ?from, offset, "live rewind: opening local view");
        let url = format!("{}?view={}", self.url(), v.id);
        drop(v);
        drop(s);
        // Release a pending reload from the replaced reader even if the provider stalls.
        self.shared.changed.notify_waiters();
        (url, offset)
    }
    pub fn directory(&self) -> PathBuf {
        self.shared.store().dir.clone()
    }
    pub fn snapshot(&self) -> Snapshot {
        let s = self.shared.store();
        let mut view = self.shared.view.lock().unwrap_or_else(|e| e.into_inner());
        let origin = view.origin.or(view.from).unwrap_or(0.0);
        view.next = view.next.or_else(|| s.boundary(origin));
        Snapshot {
            range: s.range(),
            memory: s.memory,
            epoch: s.epoch,
            files: s.blobs.len(),
            bytes: s.blobs.values().map(|b| b.size).sum(),
            origin,
            next_view: view.next,
        }
    }

    /// Keep local file options independent of provider headers and of user disk-cache options.
    pub fn options(&self) -> &'static str {
        MPV_OPTIONS
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Client {
    net: Net,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
}

impl Client {
    async fn new(headers: &HttpHeaders, ua: &str, stop: CancellationToken) -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::limited(10))
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .build()
            .map_err(io::Error::other)?;
        Ok(Self { net: Net::start(stop).await?, client, headers: request_headers(headers, ua) })
    }

    async fn get(&mut self, r: &Resource, cap: usize) -> Result<(url::Url, Bytes), PlayerErrorKind> {
        // Reuse casting's socket-close acknowledgement, not just response-drop ordering.
        self.net.closed().await;
        let mut req = self.client.get(&r.uri).headers(self.headers.clone());
        if let Some((start, len)) = r.range {
            req = req.header(header::RANGE, format!("bytes={start}-{}", start + len - 1));
        }
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            let mut response = self.net.send(req).await.map_err(|_| PlayerErrorKind::Network)?;
            let status = response.status();
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 | 403 => PlayerErrorKind::HttpForbidden,
                    404 => PlayerErrorKind::HttpNotFound,
                    n => PlayerErrorKind::HttpOther(n),
                });
            }
            let url = response.url().clone();
            let mut bytes = BytesMut::new();
            while let Some(c) = response.chunk().await.map_err(|_| PlayerErrorKind::Network)? {
                if bytes.len() + c.len() > cap {
                    return Err(PlayerErrorKind::Unsupported);
                }
                bytes.extend_from_slice(&c);
            }
            let mut bytes = bytes.freeze();
            if let Some((start, len)) = r.range {
                if status == StatusCode::OK {
                    let end = start.checked_add(len).ok_or(PlayerErrorKind::Unsupported)?;
                    if end > bytes.len() as u64 {
                        return Err(PlayerErrorKind::Network);
                    }
                    bytes = bytes.slice(start as usize..end as usize);
                } else if bytes.len() as u64 != len {
                    return Err(PlayerErrorKind::Network);
                }
            }
            Ok((url, bytes))
        })
        .await
        .map_err(|_| PlayerErrorKind::Timeout)?
    }

    async fn playlist(&mut self, url: &str) -> Result<(url::Url, String), PlayerErrorKind> {
        let (base, bytes) = self.get(&Resource { uri: url.into(), range: None }, 4 << 20).await?;
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| PlayerErrorKind::Unsupported)?;
        Ok((base, text))
    }
}

struct Source {
    url: String,
    // Fresh initial playlists aren't fetched twice at startup.
    initial: Option<(url::Url, String)>,
    last: Option<u64>,
    first: Option<u64>,
    discontinuity: Option<u64>,
    gap: bool,
}

async fn sources(client: &mut Client, root: &str) -> Result<Vec<Source>, PlayerErrorKind> {
    let mut url = root.to_owned();
    let mut audio = None;
    for _ in 0..4 {
        let (base, text) = client.playlist(&url).await?;
        let variants = variants(&text);
        if let Some(i) = choose_variant(&variants, Pick::default()) {
            // Same highest-bandwidth default as mpv. Retain the selected variant's default
            // external audio, serialized through this same reader (never another lease).
            let infos: Vec<_> = text.lines().filter_map(|l| l.trim().strip_prefix("#EXT-X-STREAM-INF:")).collect();
            if let Some(group) = infos.get(i).and_then(|s| attr(s, "AUDIO")) {
                let choices: Vec<_> = text
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("#EXT-X-MEDIA:"))
                    .filter(|s| attr(s, "TYPE") == Some("AUDIO") && attr(s, "GROUP-ID") == Some(group))
                    .collect();
                if let Some(uri) = choices.iter().find(|s| attr(s, "DEFAULT") == Some("YES")).or_else(|| choices.first()).and_then(|s| attr(s, "URI"))
                {
                    audio = Some(base.join(uri).map_err(|_| PlayerErrorKind::Unsupported)?.to_string());
                }
            }
            url = base.join(&variants[i].uri).map_err(|_| PlayerErrorKind::Unsupported)?.into();
        } else {
            let mut out = vec![Source { url, initial: Some((base, text)), last: None, first: None, discontinuity: None, gap: false }];
            if let Some(url) = audio {
                out.push(Source { url, initial: None, last: None, first: None, discontinuity: None, gap: false });
            }
            return Ok(out);
        }
    }
    Err(PlayerErrorKind::Unsupported)
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(dir).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

fn check_disk(state: &Shared) {
    let free = free_bytes(&state.store().dir);
    check_space(state, free);
}

fn check_space(state: &Shared, free: Option<u64>) {
    let mut s = state.store();
    if !s.memory && free.is_none_or(|n| n < RESERVE) {
        tracing::info!("live rewind: less than 3 GB free; using up to two minutes in memory until the next channel");
        s.memory_fallback();
    }
}

async fn blob(state: &Shared, client: &mut Client, r: &Resource, ext: &'static str) -> Result<u64, PlayerErrorKind> {
    if let Some(id) = state.store().resource(r) {
        return Ok(id);
    }
    let (_, bytes) = client.get(r, if ext == "key" { 16 } else { MAX_SEGMENT }).await?;
    if ext == "key" && bytes.len() != 16 {
        return Err(PlayerErrorKind::Unsupported);
    }
    check_disk(state);
    let mut s = state.store();
    match s.insert(r.clone(), bytes.clone(), ext) {
        Ok(id) => Ok(id),
        Err(_) => {
            tracing::info!("live rewind: disk unavailable; using up to two minutes in memory until the next channel");
            s.memory_fallback();
            s.insert(r.clone(), bytes, ext).map_err(|_| PlayerErrorKind::Mpv)
        }
    }
}

async fn refresh(state: &Shared, client: &mut Client, source: &mut Source, track: usize) -> Result<Duration, PlayerErrorKind> {
    let (base, text) = match source.initial.take() {
        Some(p) => p,
        None => client.playlist(&source.url).await?,
    };
    let media = playlist::parse(&text, &base).map_err(|_| PlayerErrorKind::Unsupported)?;
    let newest = media.segments.last().expect("parser requires segments").seq;
    let first = media.segments[0].seq;
    if source.last.is_some_and(|last| newest < last) || source.first.is_some_and(|old| first < old) {
        // Sequence regression is a new broadcast timeline, not a retry of old files.
        state.store().reset();
        source.last = None;
    }
    source.first = Some(first);
    let initial = source.last.is_none();
    // Start on the newest complete provider segment. No multi-segment ready wait:
    // first-frame latency includes only the playlist, required key/map, and this segment.
    let skip = if initial { media.segments.len().saturating_sub(1) } else { 0 };
    for seg in media.segments.iter().skip(skip) {
        if source.last.is_some_and(|last| seg.seq <= last) {
            continue;
        }
        if seg.gap {
            source.last = Some(seg.seq);
            source.gap = true;
            continue;
        }
        {
            let mut s = state.store();
            // A sequence jump is represented by a discontinuity, with contiguous local
            // counters; never invent missing segment bytes or fetch expired old URLs.
            s.prepare(track, seg.duration);
        }
        let key = if let Some(k) = &seg.key {
            let id = blob(state, client, &k.resource, "key").await?;
            Some((id, k.iv.clone().unwrap_or_else(|| format!("0x{:032x}", seg.seq))))
        } else {
            None
        };
        let map = if let Some(m) = &seg.map { Some(blob(state, client, m, "mp4").await?) } else { None };
        let ext = if map.is_some() { "m4s" } else { url::Url::parse(&seg.resource.uri).ok().and_then(|u| media_ext(u.path())).unwrap_or("ts") };
        // Segments must be fetched again if a provider reuses a URI at a new sequence.
        let (_, bytes) = client.get(&seg.resource, MAX_SEGMENT).await?;
        check_disk(state);
        let mut s = state.store();
        let id = match s.insert(seg.resource.clone(), bytes.clone(), ext) {
            Ok(id) => id,
            Err(_) => {
                tracing::info!("live rewind: disk unavailable; using up to two minutes in memory until the next channel");
                s.memory_fallback();
                s.insert(seg.resource.clone(), bytes, ext).map_err(|_| PlayerErrorKind::Mpv)?
            }
        };
        let t = &mut s.tracks[track];
        let seq = t.next_seq;
        t.next_seq += 1;
        let gap = source.gap || source.last.is_some_and(|last| seg.seq > last + 1);
        if source.last.is_none() {
            t.discontinuity = seg.discontinuity;
        } else if gap || source.discontinuity.is_some_and(|d| d != seg.discontinuity) {
            t.discontinuity = t.discontinuity.saturating_add(1);
        }
        let disc = t.discontinuity;
        t.segments.push_back(store::Segment { id, seq, start: t.edge, end: t.edge + seg.duration, disc, key, map });
        t.edge += seg.duration;
        t.target = media.target;
        t.ended = media.ended;
        source.last = Some(seg.seq);
        source.discontinuity = Some(seg.discontinuity);
        source.gap = false;
        s.pending.clear();
    }
    {
        let mut store = state.store();
        let t = &mut store.tracks[track];
        t.ready = true;
        t.target = media.target;
        t.ended = media.ended;
    }
    state.changed.notify_waiters();
    Ok(Duration::from_secs_f64((media.target / 2.0).clamp(0.1, 5.0)))
}

async fn record(state: &Shared, client: &mut Client, root: Secret<String>) {
    let policy = ReconnectPolicy::default();
    let mut selected: Vec<Source> = Vec::new();
    let mut attempt = 0;
    let mut failed_at = Instant::now();
    loop {
        check_disk(state);
        let result = async {
            if selected.is_empty() {
                selected = sources(client, root.expose()).await?;
                let mut s = state.store();
                s.tracks.resize_with(selected.len(), Track::default);
                *state.audio.lock().unwrap_or_else(|e| e.into_inner()) = selected.len() > 1;
            }
            let mut delay = Duration::from_secs(5);
            for i in 0..selected.len() {
                let epoch = state.store().epoch;
                delay = delay.min(refresh(state, client, &mut selected[i], i).await?);
                if state.store().epoch != epoch {
                    for (j, source) in selected.iter_mut().enumerate() {
                        if i != j {
                            source.last = None;
                            source.first = None;
                            source.discontinuity = None;
                            source.gap = false;
                        }
                    }
                }
            }
            Ok::<_, PlayerErrorKind>(delay)
        }
        .await;
        let delay = match result {
            Ok(delay) => {
                attempt = 0;
                delay
            }
            Err(kind) => {
                if attempt == 0 {
                    failed_at = Instant::now();
                    tracing::info!("live rewind: upstream interrupted; refreshing HLS");
                    let _ = state.errors.send(kind);
                }
                attempt += 1;
                // Fetch playlists again on EVERY retry: segment query tokens can have expired.
                // Refresh the master too if a signed variant URL has expired; retain sequence
                // cursors so a continuing sequence preserves the recording.
                if matches!(kind, PlayerErrorKind::HttpForbidden | PlayerErrorKind::HttpNotFound)
                    && !selected.is_empty()
                    && let Ok(mut fresh) = sources(client, root.expose()).await
                    && fresh.len() == selected.len()
                {
                    for (new, old) in fresh.iter_mut().zip(&selected) {
                        new.last = old.last;
                        new.first = old.first;
                        new.discontinuity = old.discontinuity;
                        new.gap = old.gap;
                    }
                    selected = fresh;
                }
                policy.sticky_delay(attempt, kind, failed_at.elapsed()).unwrap_or(Duration::from_secs(30))
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn serve(State(state): State<Arc<Shared>>, req: Request) -> Response {
    if req.method() != Method::GET {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    let Some((token, name)) = req.uri().path().trim_start_matches('/').split_once('/') else { return response(StatusCode::NOT_FOUND, Body::empty()) };
    if !state.token.matches(token) {
        return response(StatusCode::NOT_FOUND, Body::empty());
    }
    let started = Instant::now();
    let requested_view = req.uri().query().and_then(|q| q.strip_prefix("view=")).and_then(|v| v.parse::<u64>().ok());
    if let Some(track) = name.strip_suffix(".m3u8").and_then(|s| s.parse::<usize>().ok()) {
        tracing::debug!(view = ?requested_view, track, "live rewind: local playlist requested");
        let mut waited = false;
        loop {
            let changed = state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let playlist = {
                let s = state.store();
                if s.closed || (track >= s.tracks.len() && !(track == 2 && s.tracks.len() == 2)) {
                    tracing::debug!(view = ?requested_view, track, "live rewind: local playlist unavailable");
                    return response(StatusCode::NOT_FOUND, Body::empty());
                }
                let mut view = state.view.lock().unwrap_or_else(|e| e.into_inner());
                if requested_view.is_some_and(|id| id != view.id) {
                    tracing::debug!(view = ?requested_view, current_view = view.id, elapsed_ms = started.elapsed().as_millis(), "live rewind: local reader replaced");
                    return response(StatusCode::NOT_FOUND, Body::empty());
                }
                let main = track == 0 || track == 2;
                if main && view.origin.is_none() && s.tracks[0].ready {
                    view.origin = s.tracks[0].segments.iter().find(|seg| view.from.is_none_or(|p| seg.end > p)).map(|seg| seg.start);
                }
                if main && let Some(origin) = view.origin {
                    view.next = view.next.or_else(|| s.boundary(origin));
                }
                if track == 0 && *state.audio.lock().unwrap_or_else(|e| e.into_inner()) {
                    // 0 is the stable entry URL; actual media is 2 (video) and 1 (audio).
                    Some("#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"1.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\n2.m3u8\n".to_owned())
                } else {
                    let index = if track == 2 { 0 } else { track };
                    let t = &s.tracks[index];
                    let opened = *view.opened.get_or_insert_with(Instant::now);
                    // Only pace the initial live reader. FFmpeg also refreshes playlists
                    // BEFORE reading already-listed segments, so a targeted view must serve
                    // retained media immediately, however old/unchanged its playlist is.
                    // Time since opening a view does not tell us that it is at the edge.
                    let unchanged = view.from.is_none() && view.served[index] == Some(t.next_seq) && opened.elapsed() > Duration::from_millis(250);
                    if t.ready && (!unchanged || t.ended || view.next.is_some()) {
                        view.served[index] = Some(t.next_seq);
                        tracing::debug!(view = view.id, track, from = ?view.from, origin = ?view.origin, next_seq = t.next_seq, edge = t.edge,
                            ended = t.ended, boundary = ?view.next, "live rewind: local playlist ready");
                        s.playlist(index, view.from)
                    } else {
                        if !waited {
                            tracing::debug!(
                                view = view.id,
                                track,
                                ready = t.ready,
                                unchanged,
                                next_seq = t.next_seq,
                                "live rewind: local playlist waiting for publication"
                            );
                        }
                        None
                    }
                }
            };
            if let Some(p) = playlist {
                tracing::debug!(view = ?requested_view, track, waited, bytes = p.len(), elapsed_ms = started.elapsed().as_millis(),
                    "live rewind: local playlist served");
                let mut r = response(StatusCode::OK, Body::from(p));
                r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/vnd.apple.mpegurl"));
                return r;
            }
            waited = true;
            if tokio::time::timeout(REQUEST_TIMEOUT, changed).await.is_err() {
                tracing::debug!(view = ?requested_view, track, elapsed_ms = started.elapsed().as_millis(), "live rewind: local playlist timed out");
                return response(StatusCode::SERVICE_UNAVAILABLE, Body::empty());
            }
        }
    }
    let Some((id, ext)) = name.split_once('.').and_then(|(id, ext)| Some((id.parse::<u64>().ok()?, ext))) else {
        return response(StatusCode::NOT_FOUND, Body::empty());
    };
    let current_view = state.view.lock().unwrap_or_else(|e| e.into_inner()).id;
    tracing::debug!(view = current_view, resource = id, "live rewind: local resource requested");
    let bytes = {
        let s = state.store();
        let Some(b) = s.blobs.get(&id) else {
            tracing::debug!(view = current_view, resource = id, "live rewind: local resource expired");
            return response(StatusCode::NOT_FOUND, Body::empty());
        };
        if b.extension != ext {
            return response(StatusCode::NOT_FOUND, Body::empty());
        }
        match &b.data {
            Some(b) => b.clone(),
            None => match std::fs::read(s.dir.join(format!("{id}.{ext}"))) {
                Ok(b) => Bytes::from(b),
                Err(_) => return response(StatusCode::NOT_FOUND, Body::empty()),
            },
        }
    };
    let total = bytes.len();
    let range = req.headers().get(header::RANGE).and_then(|r| r.to_str().ok());
    let (code, bytes, content_range) = if let Some(range) = range {
        match crate::cast::relay::server::parse_range(range, total as u64) {
            crate::cast::relay::server::Range::Bytes(start, end) => {
                (StatusCode::PARTIAL_CONTENT, bytes.slice(start as usize..=end as usize), Some(format!("bytes {start}-{end}/{total}")))
            }
            _ => return response(StatusCode::RANGE_NOT_SATISFIABLE, Body::empty()),
        }
    } else {
        (StatusCode::OK, bytes, None)
    };
    let len = bytes.len();
    let mut r = response(code, Body::from(bytes));
    r.headers_mut().insert(header::CONTENT_LENGTH, len.into());
    r.headers_mut().insert(header::ACCEPT_RANGES, header::HeaderValue::from_static("bytes"));
    if let Some(range) = content_range {
        r.headers_mut().insert(header::CONTENT_RANGE, range.parse().expect("numeric range"));
    }
    tracing::debug!(
        view = current_view,
        resource = id,
        status = code.as_u16(),
        bytes = len,
        elapsed_ms = started.elapsed().as_millis(),
        "live rewind: local resource served"
    );
    r
}

fn response(code: StatusCode, body: Body) -> Response {
    let mut r = Response::new(body);
    *r.status_mut() = code;
    r.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    r
}

#[cfg(test)]
mod tests;
