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
use crate::cast::relay::upstream::{CLOSE_WAIT, Net, Pick, choose_variant, media_ext, prose, request_headers, variants};
use crate::model::{HttpHeaders, Playable, TileId};
use crate::player::{LiveBufferRange, PlayerErrorKind};
use crate::redact::Secret;
use playlist::{Resource, attr};
use store::{Store, Track};

pub const WINDOW: f64 = 1800.0;
const RESERVE: u64 = 3_000_000_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const PLAYLIST_WAIT: Duration = Duration::from_millis(500);
const MAX_SEGMENT: usize = 128 << 20;
// The recorder publishes complete segments. mpv's automatic cache pause otherwise
// freezes the last half-second of each segment before FFmpeg's next playlist reload,
// even while those packets are ready to play. Recorder errors drive reconnects.
const MPV_OPTIONS: &str = "force-media-title=Live,cache=yes,cache-pause=no,cache-on-disk=no,demuxer-seekable-cache=yes,force-seekable=yes,demuxer-donate-buffer=no,demuxer-lavf-o=live_start_index=-1,stream-lavf-o=reconnect=0";

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
    pub failed: bool,
}

#[derive(Debug)]
pub enum Event {
    Failed(PlayerErrorKind, Option<String>),
    Published,
    NotPlaylist,
}

#[derive(Default)]
struct View {
    id: u64,
    from: Option<f64>,
    targeted: bool,
    origin: Option<f64>,
    next: Option<f64>,
    opened: Option<Instant>,
    served: [Option<u64>; 2],
    read: [Option<u64>; 2],
}

struct Shared {
    store: Mutex<Store>,
    changed: Notify,
    token: Token,
    // A selected variant's separate default audio rendition, if any.
    audio: Mutex<bool>,
    view: Mutex<View>,
    errors: crossbeam_channel::Sender<Event>,
    failed: Mutex<bool>,
    retry: Notify,
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
    pub errors: crossbeam_channel::Receiver<Event>,
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
            failed: Mutex::new(false),
            retry: Notify::new(),
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
                client.closed().await;
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
    /// Keep a little more than one segment ahead of a live seek. FFmpeg reloads
    /// after a full segment duration, while decoder/audio queues need the next
    /// packets before the current segment has completely played. Never reach back
    /// into the previous timestamp epoch to obtain that headroom.
    pub fn live_target(&self) -> Option<f64> {
        let s = self.shared.store();
        let segments = &s.tracks[0].segments;
        let latest = segments.back()?;
        let first = segments.iter().rev().take_while(|seg| seg.disc == latest.disc).last()?;
        Some((latest.start - 1.0).max(first.start))
    }
    /// A new local reader, anchored at a retained segment. Reopening only localhost makes
    /// seeking independent of FFmpeg's stale playlist after a long pause or a discontinuity.
    /// The recorder and the provider lease keep running unchanged.
    pub fn view(&self, target: Option<f64>) -> (String, f64) {
        let s = self.shared.store();
        let targeted = target.is_some();
        let target = s.range().map(|range| target.unwrap_or(range.end).clamp(range.start, (s.tracks[0].edge - 0.5).max(range.end)));
        let from =
            target.and_then(|p| s.tracks[0].segments.iter().find(|seg| seg.end > p).or_else(|| s.tracks[0].segments.back()).map(|seg| seg.start));
        let mut v = self.shared.view.lock().unwrap_or_else(|e| e.into_inner());
        v.id += 1;
        v.from = from;
        v.targeted = targeted;
        v.origin = None;
        v.next = None;
        v.opened = None;
        v.served = [None; 2];
        v.read = [None; 2];
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
            failed: *self.shared.failed.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// The state machine owns retry timing, including first-play refusals and give-up.
    pub fn retry(&self) {
        self.shared.retry.notify_one();
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
    provider_text: Option<String>,
    not_playlist: bool,
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
        Ok(Self { net: Net::start(stop).await?, client, headers: request_headers(headers, ua), provider_text: None, not_playlist: false })
    }

    async fn closed(&self) {
        if tokio::time::timeout(CLOSE_WAIT, self.net.closed()).await.is_err() {
            tracing::info!("live rewind: provider connection still closing after {CLOSE_WAIT:?}");
        }
    }

    fn sniff_playlist(&mut self, bytes: &[u8]) -> Result<(), PlayerErrorKind> {
        self.provider_text = match playlist_head(bytes) {
            Ok(()) => return Ok(()),
            Err(text) => text,
        };
        self.not_playlist = self.provider_text.is_none();
        Err(if self.not_playlist { PlayerErrorKind::Unsupported } else { PlayerErrorKind::ProviderError })
    }

    async fn get(&mut self, r: &Resource, cap: usize, playlist: bool) -> Result<(url::Url, Bytes), PlayerErrorKind> {
        // Reuse casting's socket-close acknowledgement, not just response-drop ordering.
        self.closed().await;
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
                    404 | 410 => PlayerErrorKind::HttpNotFound,
                    n => PlayerErrorKind::HttpOther(n),
                });
            }
            let url = response.url().clone();
            let mut bytes = BytesMut::new();
            let mut sniffed = !playlist;
            while let Some(c) = response.chunk().await.map_err(|_| PlayerErrorKind::Network)? {
                if !sniffed {
                    let take = (512 - bytes.len()).min(c.len());
                    bytes.extend_from_slice(&c[..take]);
                    if bytes.len() < 512 {
                        continue;
                    }
                    self.sniff_playlist(&bytes)?;
                    sniffed = true;
                    if bytes.len() + c.len() - take > cap {
                        return Err(PlayerErrorKind::Unsupported);
                    }
                    bytes.extend_from_slice(&c[take..]);
                    continue;
                }
                if bytes.len() + c.len() > cap {
                    return Err(PlayerErrorKind::Unsupported);
                }
                bytes.extend_from_slice(&c);
            }
            if !sniffed {
                self.sniff_playlist(&bytes)?;
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
        let (base, bytes) = self.get(&Resource { uri: url.into(), range: None }, 4 << 20, true).await?;
        let text = String::from_utf8(bytes.to_vec()).map_err(|_| PlayerErrorKind::Unsupported)?;
        Ok((base, text))
    }
}

/// Inspect only the first 512 bytes, so a mislabeled continuous TS stream can be
/// handed to mpv without waiting for the playlist cap or the response to end.
fn playlist_head(bytes: &[u8]) -> Result<(), Option<String>> {
    if String::from_utf8_lossy(bytes).trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U") { Ok(()) } else { Err(prose("", bytes)) }
}

struct Source {
    url: String,
    // Fresh initial playlists aren't fetched twice at startup.
    initial: Option<(url::Url, String)>,
    last: Option<u64>,
    first: Option<u64>,
    discontinuity: Option<u64>,
    gap: bool,
    regressed: bool,
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
            let mut out =
                vec![Source { url, initial: Some((base, text)), last: None, first: None, discontinuity: None, gap: false, regressed: false }];
            if let Some(url) = audio {
                out.push(Source { url, initial: None, last: None, first: None, discontinuity: None, gap: false, regressed: false });
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

async fn blob(state: &Shared, client: &mut Client, r: &Resource, ext: &'static str, fresh: bool) -> Result<u64, PlayerErrorKind> {
    if !fresh && let Some(id) = state.store().resource(r) {
        return Ok(id);
    }
    let (_, bytes) = client.get(r, if ext == "key" { 16 } else { MAX_SEGMENT }, false).await?;
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
    // A finished playlist (`#EXT-X-ENDLIST`, the "loop" channels some providers serve) has no
    // live edge to follow: mpv plays it from the start, as before live rewind.
    if source.last.is_none() && media.ended {
        client.not_playlist = true;
        return Err(PlayerErrorKind::Unsupported);
    }
    let newest = media.segments.last().expect("parser requires segments").seq;
    let first = media.segments[0].seq;
    if source.regression(first, newest, media.segments.len()) {
        return Ok(refresh_delay(media.target));
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
        // A restarted provider may reuse key/map URLs for different bytes. Retain the
        // old blobs for historical playback, but fetch fresh ones for the new epoch.
        let fresh = initial && !state.store().tracks[track].segments.is_empty();
        let key = if let Some(k) = &seg.key {
            let id = blob(state, client, &k.resource, "key", fresh).await?;
            Some((id, k.iv.clone().unwrap_or_else(|| format!("0x{:032x}", seg.seq))))
        } else {
            None
        };
        let map = if let Some(m) = &seg.map { Some(blob(state, client, m, "mp4", fresh).await?) } else { None };
        let ext = if map.is_some() { "m4s" } else { url::Url::parse(&seg.resource.uri).ok().and_then(|u| media_ext(u.path())).unwrap_or("ts") };
        // Segments must be fetched again if a provider reuses a URI at a new sequence.
        let (_, bytes) = client.get(&seg.resource, MAX_SEGMENT, false).await?;
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
            t.discontinuity = if t.segments.is_empty() { seg.discontinuity } else { t.discontinuity.saturating_add(1) };
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
    Ok(refresh_delay(media.target))
}

fn refresh_delay(target: f64) -> Duration {
    Duration::from_secs_f64((target / 2.0).clamp(0.1, 5.0))
}

impl Source {
    /// Ignore a one-off stale window. A confirmed restart begins a new discontinuity,
    /// retaining the old recording and its local clock for paused/rewound viewers.
    fn regression(&mut self, first: u64, newest: u64, len: usize) -> bool {
        let regression = self.last.unwrap_or(newest).saturating_sub(newest).max(self.first.unwrap_or(first).saturating_sub(first));
        if regression == 0 {
            self.regressed = false;
        } else if !self.regressed && regression <= len as u64 {
            self.regressed = true;
            return true;
        } else {
            self.last = None;
            self.discontinuity = None;
            self.gap = false;
            self.regressed = false;
        }
        false
    }
}

async fn record(state: &Shared, client: &mut Client, root: Secret<String>) {
    let mut selected: Vec<Source> = Vec::new();
    let mut failed_edge = None;
    let mut reselect = false;
    loop {
        check_disk(state);
        let before = state.store().tracks[0].edge;
        let result = async {
            if selected.is_empty() || reselect {
                let mut fresh = sources(client, root.expose()).await?;
                if fresh.len() == selected.len() {
                    for (new, old) in fresh.iter_mut().zip(&selected) {
                        new.last = old.last;
                        new.first = old.first;
                        new.discontinuity = old.discontinuity;
                        new.gap = old.gap;
                        new.regressed = old.regressed;
                    }
                }
                selected = fresh;
                reselect = false;
                let mut s = state.store();
                s.tracks.resize_with(selected.len(), Track::default);
                *state.audio.lock().unwrap_or_else(|e| e.into_inner()) = selected.len() > 1;
            }
            let mut delay = Duration::from_secs(5);
            for (i, source) in selected.iter_mut().enumerate() {
                delay = delay.min(refresh(state, client, source, i).await?);
            }
            // A successful but stale playlist is not recovery. Never replay its last segment.
            if failed_edge.is_some_and(|edge| state.store().tracks[0].edge <= edge) {
                return Err(PlayerErrorKind::Stalled);
            }
            Ok::<_, PlayerErrorKind>(delay)
        }
        .await;
        match result {
            Ok(delay) => {
                *state.failed.lock().unwrap_or_else(|e| e.into_inner()) = false;
                if failed_edge.take().is_some() || state.store().tracks[0].edge > before {
                    let _ = state.errors.send(Event::Published);
                }
                tokio::time::sleep(delay).await;
            }
            Err(kind) => {
                *state.failed.lock().unwrap_or_else(|e| e.into_inner()) = true;
                failed_edge.get_or_insert(state.store().tracks[0].edge);
                if client.not_playlist {
                    let _ = state.errors.send(Event::NotPlaylist);
                } else {
                    let _ = state.errors.send(Event::Failed(kind, client.provider_text.take()));
                }
                state.changed.notify_waiters();
                // No independent polling after give-up; the controller's normal backoff
                // authorises exactly one attempt. Drop cancels this wait and closes sockets.
                state.retry.notified().await;
                // Refresh signed master/variant URLs on the next authorised attempt,
                // retaining cursors only once the new master has actually arrived.
                reselect |= matches!(kind, PlayerErrorKind::HttpForbidden | PlayerErrorKind::HttpNotFound);
            }
        }
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
        let mut timed_out = false;
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
                    if view.from.is_none() {
                        view.from = s.tracks[0].segments.back().map(|seg| seg.start);
                    }
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
                    // FFmpeg also refreshes BEFORE reading already-listed segments. A
                    // targeted reader must get those immediately, but once it has read the
                    // newest segment, wait briefly for publication just like the initial
                    // live reader. Returning a stale list at that point makes FFmpeg sleep
                    // half a target duration even if new media arrives a millisecond later.
                    let at_edge = t.segments.back().is_some_and(|seg| view.read[index] == Some(seg.seq));
                    let unchanged =
                        (!view.targeted || at_edge) && view.served[index] == Some(t.next_seq) && opened.elapsed() > Duration::from_millis(250);
                    if t.ready && (!unchanged || timed_out || t.ended || view.next.is_some()) {
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
            if tokio::time::timeout(PLAYLIST_WAIT, changed).await.is_err() {
                tracing::debug!(view = ?requested_view, track, elapsed_ms = started.elapsed().as_millis(), "live rewind: local playlist timed out");
                timed_out = true;
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
        {
            let mut view = state.view.lock().unwrap_or_else(|e| e.into_inner());
            if view.id == current_view {
                for (index, track) in s.tracks.iter().enumerate() {
                    if let Some(segment) = track.segments.iter().find(|seg| seg.id == id) {
                        view.read[index] = Some(segment.seq);
                    }
                }
            }
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
