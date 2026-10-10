//! Image pipeline: logos, posters, backdrops. Owner: WP-09. DESIGN §18.
//!
//! ```text
//! UI get(url, kind) ─hit──▶ TextureHandle (LRU touch)
//!        │ miss
//!        ▼
//!  wanted-set (url → last frame requested) ──▶ tokio: disk cache? ──no──▶ HTTP (Semaphore 6,
//!                                               │ yes                     ≤ 2 per host, 8 MiB cap)
//!                                               ▼                          │ write ~/.cache/tuner/img/ab/<blake3>.bin
//!                                     spawn_blocking decode: image (Semaphore 2, shared with disk
//!                                     reads) / resvg (own Semaphore 1, so slow SVGs never starve
//!                                     raster decodes); downscale with fast_image_resize to kind's box
//!                                               ▼
//!                                     decoded queue ──▶ UI `pump`: ≤ 4 MiB of texture uploads per
//!                                                       frame (one backdrop ≈ 3.7 MB), ctx.load_texture
//! ```
//! * Untrusted input: decode with `image::ImageReader` + `Limits { max_image_width: 8192,
//!   max_image_height: 8192, max_alloc: 64 MiB }` (a 20k×20k PNG must not allocate GBs); SVG:
//!   reject sources > 1 MiB, `usvg::Options` with an EMPTY fontdb (never `load_system_fonts`),
//!   no external or embedded `<image>` references (no local file reads, no nested decoders
//!   outside our limits), at most 16 filter primitives (blurs over a large canvas take seconds)
//!   and 10 000 nodes after `<use>` expansion, raster size clamped to the kind's box. A render
//!   still running after 3 s counts as undecodable (`.fail` marker); its thread keeps the one
//!   SVG permit until it really ends, so runaway renders never pile up. gzip-compressed SVG
//!   (svgz) is not accepted.
//!   Downloads are capped at 8 MiB.
//! * A decoder panic (hostile input) counts as an undecodable file: `.fail` marker, no retry.
//! * Fetching goes through a private `trait Fetch` (implemented for `HttpClient`) so WP-09's
//!   tests run without WP-04. Only `http`/`https` URLs are fetched.
//! * The wanted-map is pruned of entries older than 2 s on every `pump`.
//! * Stale requests: a fetch is skipped if its url wasn't requested in the last 2 s (fast
//!   scrolling through 100k channels must not queue 100k downloads). The check runs before the
//!   disk read and again right before the network request (after waiting for a permit).
//! * Failures are negative-cached in memory for 1 h and on disk (`.fail` marker) for 24 h.
//!   Transient network failures (timeouts, 5xx, offline, TLS) are cached in memory only, backing
//!   off per image 2 → 8 → 32 → 60 min (DESIGN §18): a network blip hides logos for minutes,
//!   not a day, and a host that stays unreachable is retried a few times an hour, not every
//!   2 min. A successful load resets the back-off. A failed fetch logs one DEBUG line naming
//!   the host only.
//! * Memory: LRU over texture bytes (default budget 192 MiB GPU, configurable), never exceeded.
//!   Only entries not used for ≥ 2 frames are evicted, so the visible set never thrashes; an
//!   image that doesn't fit next to the visible set waits decoded (shown as loading) until
//!   something scrolls away, and one larger than the whole budget fails.
//! * Placeholder (UI decides): monogram of the name on fg@0.04; missing logo ≠ loading state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::paths::AppPaths;
use crate::sources::http::HttpClient;

/// Download cap for one image.
const MAX_DOWNLOAD_BYTES: u64 = 8 << 20;
/// SVG sources above this size are rejected.
const MAX_SVG_BYTES: usize = 1 << 20;
/// SVGs applying more filter primitives than this are rejected (rendering time).
const MAX_SVG_FILTER_PRIMITIVES: usize = 16;
/// SVGs with more nodes than this (after `<use>` expansion, incl. clip/mask/pattern content)
/// are rejected (rendering time).
const MAX_SVG_NODES: usize = 10_000;
/// Back-off factor between consecutive transient failures of one image.
const TRANSIENT_BACKOFF: u32 = 4;
/// Raster decode limits (untrusted input).
const MAX_DIMENSION: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 64 << 20;
/// Texture upload budget per frame (one 1280×720 backdrop ≈ 3.7 MB).
const UPLOAD_BYTES_PER_FRAME: usize = 4 << 20;
/// Concurrency: downloads overall / per host / raster decodes (incl. disk reads) / SVG renders.
const MAX_FETCHES: usize = 6;
const MAX_FETCHES_PER_HOST: usize = 2;
const MAX_DECODES: usize = 2;
const MAX_SVG_RENDERS: usize = 1;
/// Entries used within this many frames are never evicted.
const EVICT_IDLE_FRAMES: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    /// Channel logos: fit in 128×128 (aspect kept).
    Logo,
    /// Posters / covers: fit in 342×513.
    Poster,
    /// Backdrops: fit in 1280×720.
    Backdrop,
    /// Episode stills: fit in 400×225.
    Still,
}

impl ImageKind {
    pub fn max_size(self) -> (u32, u32) {
        match self {
            ImageKind::Logo => (128, 128),
            ImageKind::Poster => (342, 513),
            ImageKind::Backdrop => (1280, 720),
            ImageKind::Still => (400, 225),
        }
    }

    fn tag(self) -> u8 {
        match self {
            ImageKind::Logo => 0,
            ImageKind::Poster => 1,
            ImageKind::Backdrop => 2,
            ImageKind::Still => 3,
        }
    }
}

#[derive(Clone)]
pub enum ImageState {
    /// Requested; show a neutral placeholder (no spinner for logos).
    Loading,
    Ready(egui::TextureHandle),
    /// Failed/invalid: show the monogram placeholder.
    Failed,
}

// ---------------------------------------------------------------------------- fetch seam

type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<bytes::Bytes>> + Send + 'a>>;

/// Network seam: the HTTP client in production, a fake in tests. `user_agent: None` = the
/// client's default (Settings ▸ Playback User-Agent).
trait Fetch: Send + Sync + 'static {
    fn fetch<'a>(&'a self, url: &'a str, user_agent: Option<&'a str>, max_bytes: u64) -> FetchFuture<'a>;
}

impl Fetch for HttpClient {
    fn fetch<'a>(&'a self, url: &'a str, user_agent: Option<&'a str>, max_bytes: u64) -> FetchFuture<'a> {
        Box::pin(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let headers = crate::model::HttpHeaders { user_agent: user_agent.map(str::to_owned), ..Default::default() };
            self.get_bytes(url, &headers, max_bytes, &cancel).await
        })
    }
}

/// Timing knobs (tests shorten them).
#[derive(Debug, Clone, Copy)]
struct Tuning {
    /// A request not repeated by `get` within this window is skipped / pruned.
    stale_after: Duration,
    /// Memory negative cache for permanent failures.
    fail_ttl: Duration,
    /// Memory negative cache for the first transient (network) failure; later ones back off
    /// ×4 each, up to `fail_ttl`.
    transient_fail_ttl: Duration,
    /// An SVG render taking longer counts as undecodable.
    svg_timeout: Duration,
    /// Age limit of a `.fail` marker on disk.
    disk_fail_ttl: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(2),
            fail_ttl: Duration::from_secs(3600),
            transient_fail_ttl: Duration::from_secs(120),
            svg_timeout: Duration::from_secs(3),
            disk_fail_ttl: Duration::from_secs(24 * 3600),
        }
    }
}

/// Cache key: (kind, url) hashed. 128 bits of blake3, so collisions are not a concern.
type Key = u128;

fn key_of(url: &str, kind: ImageKind) -> Key {
    let mut h = blake3::Hasher::new();
    h.update(&[kind.tag()]);
    h.update(url.as_bytes());
    let mut k = [0u8; 16];
    k.copy_from_slice(&h.finalize().as_bytes()[..16]);
    u128::from_le_bytes(k)
}

enum Outcome {
    Ready(egui::ColorImage),
    Failed {
        transient: bool,
    },
    /// Not requested recently; dropped without work. A later `get` re-requests it.
    Skipped,
}

struct Done {
    key: Key,
    outcome: Outcome,
}

/// State shared between the UI facade and the loader tasks.
struct Shared {
    dir: PathBuf,
    fetch: Arc<dyn Fetch>,
    tuning: Tuning,
    /// key → last time `get` asked for it.
    wanted: Mutex<HashMap<Key, Instant>>,
    /// Host → User-Agent of the playlist served from that host ([`ImageCache::set_source_agents`]).
    agents: Mutex<HashMap<String, String>>,
    net: Semaphore,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    decode: Arc<Semaphore>,
    /// Owned permits: a timed-out render keeps its slot until its thread ends.
    svg: Arc<Semaphore>,
    /// [`decode`]; a seam so tests can simulate a decoder panic.
    decoder: fn(&[u8], ImageKind) -> Result<egui::ColorImage>,
    tx: crossbeam_channel::Sender<Done>,
    egui: egui::Context,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn is_stale(&self, key: Key) -> bool {
        lock(&self.wanted).get(&key).is_none_or(|t| t.elapsed() >= self.tuning.stale_after)
    }

    fn host_permits(&self, host: &str) -> Arc<Semaphore> {
        lock(&self.hosts).entry(host.to_owned()).or_insert_with(|| Arc::new(Semaphore::new(MAX_FETCHES_PER_HOST))).clone()
    }
}

/// Sends the outcome exactly once, also when the task panics or is dropped (runtime shutdown),
/// so the UI never keeps an entry in `Loading` forever.
struct Reply {
    shared: Arc<Shared>,
    key: Key,
    sent: bool,
}

impl Reply {
    fn send(mut self, outcome: Outcome) {
        self.sent = true;
        self.deliver(outcome);
    }

    fn deliver(&self, outcome: Outcome) {
        let _ = self.shared.tx.send(Done { key: self.key, outcome });
        self.shared.egui.request_repaint();
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if !self.sent {
            self.deliver(Outcome::Failed { transient: true });
        }
    }
}

// ---------------------------------------------------------------------------- UI facade

struct Tex {
    handle: egui::TextureHandle,
    bytes: usize,
    last_frame: u64,
}

/// UI-thread facade. Not `Send` (owns textures); background work happens on the runtime.
/// Private fields are WP-09's to reshape; only the pub API is frozen.
pub struct ImageCache {
    shared: Arc<Shared>,
    rt: tokio::runtime::Handle,
    egui: egui::Context,
    budget_bytes: usize,
    frame: u64,
    textures: lru::LruCache<Key, Tex>,
    texture_bytes: usize,
    /// Requested and not yet uploaded (loading, decoding or waiting in `ready`).
    inflight: HashSet<Key>,
    /// Negative cache: key → expiry.
    failed: HashMap<Key, Instant>,
    /// Consecutive transient failures per image (back-off), reset by a successful load.
    strikes: HashMap<Key, u32>,
    /// Decoded images waiting for their upload slot.
    ready: VecDeque<(Key, egui::ColorImage)>,
    rx: crossbeam_channel::Receiver<Done>,
}

impl ImageCache {
    pub fn new(paths: &AppPaths, http: HttpClient, rt: tokio::runtime::Handle, egui: egui::Context, budget_bytes: usize) -> Self {
        Self::with_fetch(paths.image_cache(), Arc::new(http), rt, egui, budget_bytes, Tuning::default())
    }

    fn with_fetch(dir: PathBuf, fetch: Arc<dyn Fetch>, rt: tokio::runtime::Handle, egui: egui::Context, budget_bytes: usize, tuning: Tuning) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            dir,
            fetch,
            tuning,
            wanted: Mutex::default(),
            agents: Mutex::default(),
            net: Semaphore::new(MAX_FETCHES),
            hosts: Mutex::default(),
            decode: Arc::new(Semaphore::new(MAX_DECODES)),
            svg: Arc::new(Semaphore::new(MAX_SVG_RENDERS)),
            decoder: decode,
            tx,
            egui: egui.clone(),
        });
        Self {
            shared,
            rt,
            egui,
            budget_bytes,
            frame: 0,
            textures: lru::LruCache::unbounded(),
            texture_bytes: 0,
            inflight: HashSet::new(),
            failed: HashMap::new(),
            strikes: HashMap::new(),
            ready: VecDeque::new(),
            rx,
        }
    }

    /// Cheap lookup; on miss enqueues a fetch and returns `Loading`. Call every frame for
    /// visible items only.
    pub fn get(&mut self, url: &str, kind: ImageKind) -> ImageState {
        let url = url.trim();
        if url.is_empty() {
            return ImageState::Failed;
        }
        let key = key_of(url, kind);
        if let Some(t) = self.textures.get_mut(&key) {
            t.last_frame = self.frame;
            return ImageState::Ready(t.handle.clone());
        }
        let now = Instant::now();
        if let Some(until) = self.failed.get(&key) {
            if *until > now {
                return ImageState::Failed;
            }
            self.failed.remove(&key);
        }
        lock(&self.shared.wanted).insert(key, now);
        if self.inflight.insert(key) {
            let reply = Reply { shared: self.shared.clone(), key, sent: false };
            self.rt.spawn(load(reply, url.to_owned(), kind));
        }
        ImageState::Loading
    }

    /// Per frame (start of `App::ui`): upload ≤ 4 MiB of decoded images, evict over budget,
    /// prune stale wanted entries.
    pub fn pump(&mut self) {
        self.frame += 1;
        self.drain_results();
        self.upload();
        self.evict();
        let stale_after = self.shared.tuning.stale_after;
        lock(&self.shared.wanted).retain(|_, t| t.elapsed() < stale_after);
        if !self.failed.is_empty() {
            let now = Instant::now();
            self.failed.retain(|_, until| *until > now);
        }
    }

    /// Per-source User-Agents by host (DESIGN §18): an image on the host a playlist is served
    /// from (an Xtream panel's logos and posters) is fetched with that playlist's User-Agent;
    /// images elsewhere (CDNs, wikis) with the default one. `ImageCache::get` has no source:
    /// the host is what ties a URL to a provider.
    pub fn set_source_agents(&mut self, agents: HashMap<String, String>) {
        *lock(&self.shared.agents) = agents;
    }

    /// Drop all textures (theme/scale change does NOT require this; memory pressure does).
    pub fn clear_memory(&mut self) {
        self.textures.clear();
        self.texture_bytes = 0;
        for (key, _) in self.ready.drain(..) {
            self.inflight.remove(&key);
        }
    }

    /// (textures, bytes) for the debug overlay.
    pub fn stats(&self) -> (usize, usize) {
        (self.textures.len(), self.texture_bytes)
    }

    fn drain_results(&mut self) {
        while let Ok(Done { key, outcome }) = self.rx.try_recv() {
            match outcome {
                Outcome::Ready(img) => {
                    self.strikes.remove(&key);
                    self.ready.push_back((key, img));
                }
                Outcome::Failed { transient } => {
                    self.inflight.remove(&key);
                    let t = &self.shared.tuning;
                    let ttl = if transient {
                        let strikes = self.strikes.entry(key).or_default();
                        let ttl = t.transient_fail_ttl.saturating_mul(TRANSIENT_BACKOFF.saturating_pow(*strikes));
                        *strikes = strikes.saturating_add(1);
                        ttl.min(t.fail_ttl)
                    } else {
                        self.strikes.remove(&key);
                        t.fail_ttl
                    };
                    self.failed.insert(key, Instant::now() + ttl);
                }
                Outcome::Skipped => {
                    self.inflight.remove(&key);
                }
            }
        }
    }

    fn upload(&mut self) {
        let mut uploaded = 0usize;
        while let Some((_, img)) = self.ready.front() {
            let bytes = image_bytes(img);
            // always allow one upload per frame so an oversized image can't stall the queue
            if uploaded > 0 && uploaded + bytes > UPLOAD_BYTES_PER_FRAME {
                break;
            }
            let Some((key, img)) = self.ready.pop_front() else { break };
            if self.shared.is_stale(key) {
                self.inflight.remove(&key);
                continue; // scrolled away while decoding: don't spend upload budget on it
            }
            if bytes > self.budget_bytes {
                // can never fit: the placeholder instead of evicting everything else
                self.inflight.remove(&key);
                self.failed.insert(key, Instant::now() + self.shared.tuning.fail_ttl);
                continue;
            }
            if !self.make_room(bytes) {
                // the visible set fills the budget: wait (still loading) until something
                // scrolls away and turns idle a couple of frames later — a slow retry, not a
                // repaint loop, so it isn't stuck until the next input
                self.ready.push_front((key, img));
                self.egui.request_repaint_after(Duration::from_millis(250));
                return;
            }
            self.inflight.remove(&key);
            let handle = self.egui.load_texture(format!("img-{key:032x}"), img, egui::TextureOptions::LINEAR);
            if let Some(old) = self.textures.put(key, Tex { handle, bytes, last_frame: self.frame }) {
                self.texture_bytes -= old.bytes;
            }
            self.texture_bytes += bytes;
            uploaded += bytes;
        }
        if !self.ready.is_empty() {
            self.egui.request_repaint();
        }
    }

    /// Evict idle entries until `extra` more bytes fit the budget; false if the entries used in
    /// the last frames (the visible set) leave no room.
    fn make_room(&mut self, extra: usize) -> bool {
        while self.texture_bytes + extra > self.budget_bytes {
            match self.textures.peek_lru() {
                Some((_, t)) if self.frame.saturating_sub(t.last_frame) >= EVICT_IDLE_FRAMES => {}
                // everything left was used in the last frames: keep the visible set
                _ => return false,
            }
            if let Some((_, t)) = self.textures.pop_lru() {
                self.texture_bytes -= t.bytes;
            }
        }
        true
    }

    fn evict(&mut self) {
        self.make_room(0);
    }
}

fn image_bytes(img: &egui::ColorImage) -> usize {
    img.size[0] * img.size[1] * 4
}

// ---------------------------------------------------------------------------- loader task

async fn load(reply: Reply, url: String, kind: ImageKind) {
    let outcome = run_load(&reply.shared, reply.key, &url, kind).await;
    reply.send(outcome);
}

enum DiskResult {
    /// The cached original bytes.
    Hit(Vec<u8>),
    /// A fresh `.fail` marker.
    Failed,
    Miss,
}

async fn run_load(shared: &Arc<Shared>, key: Key, url: &str, kind: ImageKind) -> Outcome {
    if shared.is_stale(key) {
        return Outcome::Skipped;
    }
    let path = cache_path(&shared.dir, url);

    // 1. disk cache (read under the decode permit: bounded blocking-pool use)
    let disk = {
        let Ok(_permit) = shared.decode.acquire().await else { return Outcome::Failed { transient: true } };
        if shared.is_stale(key) {
            return Outcome::Skipped;
        }
        let (p, ttl) = (path.clone(), shared.tuning.disk_fail_ttl);
        match tokio::task::spawn_blocking(move || read_disk(&p, ttl)).await {
            Ok(r) => r,
            Err(_) => return Outcome::Failed { transient: true },
        }
    };
    match disk {
        DiskResult::Hit(bytes) => return decode_and_settle(shared, path, bytes.into(), kind, true).await,
        DiskResult::Failed => return Outcome::Failed { transient: false },
        DiskResult::Miss => {}
    }

    // 2. network
    let host = match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u.host_str().map(str::to_owned),
        _ => None,
    };
    let Some(host) = host else {
        tracing::debug!(url = %crate::redact::redact_url(url), "image url not fetchable");
        return Outcome::Failed { transient: false };
    };
    let fetched = {
        let host_permits = shared.host_permits(&host);
        let Ok(_host) = host_permits.acquire().await else { return Outcome::Failed { transient: true } };
        let Ok(_net) = shared.net.acquire().await else { return Outcome::Failed { transient: true } };
        if shared.is_stale(key) {
            return Outcome::Skipped;
        }
        let agent = lock(&shared.agents).get(&host).cloned();
        shared.fetch.fetch(url, agent.as_deref(), MAX_DOWNLOAD_BYTES).await
    };
    let bytes = match fetched {
        Ok(b) => b,
        Err(e) => {
            // one quiet line naming the host and the reason, never the URL (paths may carry
            // tokens; network and HTTP error texts include it)
            let why = match &e {
                Error::Http { status, .. } => format!("HTTP {status}"),
                Error::Network(_) => "network error".to_owned(),
                Error::Cancelled => "cancelled".to_owned(),
                Error::BadPayload(why) => why.clone(), // no URL in these ("response larger than…")
                _ => "error".to_owned(),
            };
            tracing::debug!(target: "tuner::images", "image from {host} failed: {why}");
            if e.is_transient() || matches!(e, Error::Cancelled) {
                return Outcome::Failed { transient: true };
            }
            let marker = path.with_extension("fail");
            let _ = tokio::task::spawn_blocking(move || write_fail_marker(&marker)).await;
            return Outcome::Failed { transient: false };
        }
    };

    // 3. decode, then persist the original bytes
    decode_and_settle(shared, path, bytes, kind, false).await
}

/// Decode off the UI thread under the matching permit (SVG renders have their own), then
/// settle the disk cache: a cached file is touched (LRU by mtime for `prune_disk`), fetched
/// bytes are persisted. Undecodable input, including a decoder panic on hostile data, drops
/// the cached file and leaves a `.fail` marker, so it is not decoded again.
async fn decode_and_settle(shared: &Shared, path: PathBuf, bytes: bytes::Bytes, kind: ImageKind, cached: bool) -> Outcome {
    let svg = looks_like_svg(&bytes);
    let permits = if svg { &shared.svg } else { &shared.decode };
    let Ok(permit) = permits.clone().acquire_owned().await else { return Outcome::Failed { transient: true } };
    // set when a render timed out: its late result is dropped (the marker is written already)
    let abandoned = Arc::new(AtomicBool::new(false));
    let (decoder, p, gone) = (shared.decoder, path.clone(), abandoned.clone());
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit; // held until the decode really ends
        let img = decoder(&bytes, kind)?;
        if gone.load(AtomicOrdering::SeqCst) {
            return Err(Error::Cancelled);
        }
        if cached {
            let _ = std::fs::File::open(&p).and_then(|f| f.set_modified(SystemTime::now()));
        } else if let Err(e) = write_atomic(&p, &bytes) {
            tracing::debug!(error = %e, "image cache write failed");
        }
        Ok::<_, Error>(img)
    });
    let joined = if svg {
        let timeout = shared.tuning.svg_timeout;
        tokio::time::timeout(timeout, task).await.map_err(|_| {
            abandoned.store(true, AtomicOrdering::SeqCst);
            tracing::debug!(target: "tuner::images", "svg render took over {timeout:?}; treating the image as invalid");
        })
    } else {
        Ok(task.await)
    };
    match joined {
        Ok(Ok(Ok(img))) => return Outcome::Ready(img),
        Ok(Ok(Err(e))) => tracing::debug!(error = %e, cached, "image decode failed"),
        Ok(Err(e)) if e.is_panic() => tracing::warn!(cached, "image decoder panicked; treating the image as invalid"),
        Ok(Err(_)) => return Outcome::Failed { transient: true }, // runtime shutting down
        Err(()) => {}                                             // timed out
    }
    let _ = tokio::task::spawn_blocking(move || {
        if cached {
            let _ = std::fs::remove_file(&path);
        }
        write_fail_marker(&path.with_extension("fail"));
    })
    .await;
    Outcome::Failed { transient: false }
}

fn read_disk(path: &Path, fail_ttl: Duration) -> DiskResult {
    let marker = path.with_extension("fail");
    if let Ok(meta) = std::fs::metadata(&marker) {
        let age = meta.modified().ok().and_then(|m| m.elapsed().ok()).unwrap_or(Duration::MAX);
        if age < fail_ttl {
            return DiskResult::Failed;
        }
        let _ = std::fs::remove_file(&marker);
    }
    let Ok(file) = std::fs::File::open(path) else { return DiskResult::Miss };
    let len = file.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
    if len > MAX_DOWNLOAD_BYTES {
        let _ = std::fs::remove_file(path);
        return DiskResult::Miss;
    }
    let mut bytes = Vec::with_capacity(len as usize);
    if std::io::Read::read_to_end(&mut &file, &mut bytes).is_err() {
        return DiskResult::Miss;
    }
    DiskResult::Hit(bytes)
}

fn write_fail_marker(marker: &Path) {
    if let Err(e) = write_atomic(marker, b"") {
        tracing::debug!(error = %e, "image .fail marker write failed");
    }
}

/// Write via a unique temp file + rename, so concurrent readers never see partial files.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let tmp = path.with_extension(format!("tmp{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let r = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(bytes)).and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io(path, e));
    }
    Ok(())
}

// ---------------------------------------------------------------------------- free functions

/// Disk cache path for a URL: `<dir>/<hh>/<blake3 hex>.bin`.
pub fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let h = blake3::hash(url.as_bytes()).to_hex();
    dir.join(&h[..2]).join(format!("{h}.bin"))
}

/// Decode PNG/JPEG/WebP/GIF(first frame)/SVG bytes and downscale into `kind`'s box.
pub fn decode(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> {
    if looks_like_svg(bytes) { decode_svg(bytes, kind) } else { decode_raster(bytes, kind) }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    if image::guess_format(bytes).is_ok() {
        return false;
    }
    let head = &bytes[..bytes.len().min(4096)];
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = head.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(head.len());
    head[start..].starts_with(b"<") && memchr::memmem::find(head, b"<svg").is_some()
}

/// Largest size with the source's aspect ratio that fits `max` (`upscale` allows growing).
fn fit(w: f32, h: f32, max: (u32, u32), upscale: bool) -> (u32, u32, f32) {
    let mut scale = (max.0 as f32 / w).min(max.1 as f32 / h);
    if !upscale {
        scale = scale.min(1.0);
    }
    let dw = ((w * scale).round() as u32).clamp(1, max.0);
    let dh = ((h * scale).round() as u32).clamp(1, max.1);
    (dw, dh, scale)
}

fn decode_raster(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> {
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| Error::BadPayload(format!("image: {e}")))?;
    if reader.format().is_none() {
        return Err(Error::BadPayload("not an image".into()));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let rgba = reader.decode().map_err(|e| Error::BadPayload(format!("image: {e}")))?.into_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Err(Error::BadPayload("empty image".into()));
    }
    let (dw, dh, _) = fit(w as f32, h as f32, kind.max_size(), false);
    if (dw, dh) == (w, h) {
        return Ok(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw()));
    }
    use fast_image_resize as fr;
    let src = fr::images::Image::from_vec_u8(w, h, rgba.into_raw(), fr::PixelType::U8x4).map_err(Error::other)?;
    let mut dst = fr::images::Image::new(dw, dh, fr::PixelType::U8x4);
    // default options: Lanczos3, alpha premultiplied during the resize
    fr::Resizer::new().resize(&src, &mut dst, None).map_err(Error::other)?;
    Ok(egui::ColorImage::from_rgba_unmultiplied([dw as usize, dh as usize], dst.buffer()))
}

fn decode_svg(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> {
    use resvg::{tiny_skia, usvg};
    if bytes.len() > MAX_SVG_BYTES {
        return Err(Error::BadPayload(format!("svg larger than {} KiB", MAX_SVG_BYTES >> 10)));
    }
    let opt = usvg::Options {
        // never load system fonts (slow, and text in logos is decorative); text is dropped
        fontdb: Arc::new(usvg::fontdb::Database::new()),
        // no local files, no nested decoders that bypass our limits
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: Box::new(|_, _, _| None), resolve_string: Box::new(|_, _| None) },
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &opt).map_err(|e| Error::BadPayload(format!("svg: {e}")))?;
    // counted on the resolved tree, so `<use>` copies of a filtered element count too
    let primitives = filter_primitives(tree.root());
    if primitives > MAX_SVG_FILTER_PRIMITIVES {
        return Err(Error::BadPayload(format!("svg: {primitives} filter primitives")));
    }
    let nodes = node_count(tree.root());
    if nodes > MAX_SVG_NODES {
        return Err(Error::BadPayload(format!("svg: {nodes} nodes")));
    }
    let size = tree.size();
    let (dw, dh, scale) = fit(size.width(), size.height(), kind.max_size(), true);
    let mut pixmap = tiny_skia::Pixmap::new(dw, dh).ok_or_else(|| Error::BadPayload("svg: empty canvas".into()))?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok(egui::ColorImage::from_rgba_premultiplied([dw as usize, dh as usize], pixmap.data()))
}

/// Filter primitives applied anywhere in the tree (groups, and patterns/masks/clips/filter
/// content via subroots).
fn filter_primitives(group: &resvg::usvg::Group) -> usize {
    let own: usize = group.filters().iter().map(|f| f.primitives().len()).sum();
    group
        .children()
        .iter()
        .map(|node| {
            let mut n = 0;
            if let resvg::usvg::Node::Group(g) = node {
                n += filter_primitives(g);
            }
            node.subroots(|sub| n += filter_primitives(sub));
            n
        })
        .fold(own, usize::saturating_add)
}

/// Nodes anywhere in the tree (children, and pattern/mask/clip/filter content via subroots).
fn node_count(group: &resvg::usvg::Group) -> usize {
    group
        .children()
        .iter()
        .map(|node| {
            let mut n = 1usize;
            if let resvg::usvg::Node::Group(g) = node {
                n = n.saturating_add(node_count(g));
            }
            node.subroots(|sub| n = n.saturating_add(node_count(sub)));
            n
        })
        .fold(0, usize::saturating_add)
}

/// Delete least-recently-modified cache files until the directory is under `max_bytes`
/// (maintenance job; default 500 MiB). Returns the number of bytes freed. A missing directory
/// is empty.
pub fn prune_disk(dir: &Path, max_bytes: u64) -> Result<u64> {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    let mut visit = |p: &Path, meta: &std::fs::Metadata| {
        total += meta.len();
        files.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len(), p.to_path_buf()));
    };
    let top = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::io(dir, e)),
    };
    // entries can vanish concurrently (the loader renames/removes): skip them silently
    for entry in top.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let Ok(sub) = std::fs::read_dir(entry.path()) else { continue };
            for f in sub.flatten() {
                if let Ok(m) = f.metadata()
                    && m.is_file()
                {
                    visit(&f.path(), &m);
                }
            }
        } else if meta.is_file() {
            visit(&entry.path(), &meta);
        }
    }
    if total <= max_bytes {
        return Ok(0);
    }
    files.sort_by_key(|f| f.0);
    let mut freed = 0u64;
    for (_, len, path) in files {
        if total - freed <= max_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => freed += len,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(path, e)),
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture(name: &str) -> Vec<u8> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images").join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[derive(Clone, Copy)]
    enum Answer {
        Bytes,
        NotFound,
        Offline,
        Garbage,
        Panic,
    }

    /// Fake network: counts calls, tracks max concurrency (overall and per host) and the
    /// User-Agents asked for.
    struct FakeFetch {
        body: Vec<u8>,
        reply: Answer,
        delay: Duration,
        calls: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        agents: Mutex<Vec<(String, Option<String>)>>,
    }

    impl FakeFetch {
        fn new(body: Vec<u8>, reply: Answer, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                body,
                reply,
                delay,
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                agents: Mutex::default(),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Fetch for FakeFetch {
        fn fetch<'a>(&'a self, url: &'a str, user_agent: Option<&'a str>, max_bytes: u64) -> FetchFuture<'a> {
            Box::pin(async move {
                assert_eq!(max_bytes, MAX_DOWNLOAD_BYTES);
                lock(&self.agents).push((url.to_owned(), user_agent.map(str::to_owned)));
                self.calls.fetch_add(1, Ordering::SeqCst);
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                match self.reply {
                    Answer::Bytes => Ok(bytes::Bytes::from(self.body.clone())),
                    Answer::NotFound => Err(Error::Http { status: 404, url: "x".into() }),
                    Answer::Offline => Err(Error::Network("connection refused".into())),
                    Answer::Garbage => Ok(bytes::Bytes::from_static(b"<html>nope</html>")),
                    Answer::Panic => panic!("fetch exploded"),
                }
            })
        }
    }

    struct Rig {
        _rt: tokio::runtime::Runtime,
        dir: tempfile::TempDir,
        cache: ImageCache,
    }

    fn rig(fetch: Arc<dyn Fetch>, budget: usize, tuning: Tuning) -> Rig {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::with_fetch(dir.path().to_path_buf(), fetch, rt.handle().clone(), egui::Context::default(), budget, tuning);
        Rig { _rt: rt, dir, cache }
    }

    /// Call `get` + `pump` like the UI does until the state settles.
    fn drive(c: &mut ImageCache, url: &str, kind: ImageKind) -> ImageState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match c.get(url, kind) {
                ImageState::Loading if Instant::now() < deadline => {}
                s => return s,
            }
            c.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Like [`drive`] for many urls at once (all visible in the same frames).
    fn drive_all(c: &mut ImageCache, urls: &[String], kind: ImageKind) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let pending = urls.iter().filter(|u| matches!(c.get(u, kind), ImageState::Loading)).count();
            if pending == 0 || Instant::now() > deadline {
                return;
            }
            c.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn size_of(s: &ImageState) -> [usize; 2] {
        match s {
            ImageState::Ready(t) => t.size(),
            ImageState::Loading => panic!("still loading"),
            ImageState::Failed => panic!("failed"),
        }
    }

    #[test]
    fn disk_hit_does_no_network() {
        let fake = FakeFetch::new(vec![], Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake.clone(), 64 << 20, Tuning::default());
        let url = "http://logos.example/one.png";
        let p = cache_path(r.dir.path(), url);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, fixture("logo_600x300.png")).unwrap();
        let s = drive(&mut r.cache, url, ImageKind::Logo);
        assert_eq!(size_of(&s), [128, 64]);
        assert_eq!(fake.calls(), 0);
        assert_eq!(r.cache.stats(), (1, 128 * 64 * 4));
    }

    #[test]
    fn network_miss_writes_disk_cache_reused_by_next_instance() {
        let fake = FakeFetch::new(png(300, 300), Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake.clone(), 64 << 20, Tuning::default());
        let url = "https://img.example/p/1.png";
        assert_eq!(size_of(&drive(&mut r.cache, url, ImageKind::Poster)), [300, 300]);
        assert_eq!(size_of(&drive(&mut r.cache, url, ImageKind::Logo)), [128, 128], "same bytes, other box");
        assert_eq!(fake.calls(), 1, "second kind decoded from disk");
        assert!(cache_path(r.dir.path(), url).is_file());

        let fake2 = FakeFetch::new(vec![], Answer::NotFound, Duration::ZERO);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let mut c2 = ImageCache::with_fetch(
            r.dir.path().to_path_buf(),
            fake2.clone(),
            rt.handle().clone(),
            egui::Context::default(),
            1 << 20,
            Tuning::default(),
        );
        assert_eq!(size_of(&drive(&mut c2, url, ImageKind::Poster)), [300, 300]);
        assert_eq!(fake2.calls(), 0);
    }

    #[test]
    fn at_most_six_concurrent_fetches_and_two_per_host() {
        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::from_millis(60));
        let mut r = rig(fake.clone(), 64 << 20, Tuning::default());
        let urls: Vec<String> = (0..20).map(|i| format!("http://h{i}.example/l.png")).collect();
        drive_all(&mut r.cache, &urls, ImageKind::Logo);
        assert_eq!(fake.calls(), 20);
        let max = fake.max_active.load(Ordering::SeqCst);
        assert!((2..=MAX_FETCHES).contains(&max), "max concurrent fetches {max}");

        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::from_millis(40));
        let mut r = rig(fake.clone(), 64 << 20, Tuning::default());
        let urls: Vec<String> = (0..8).map(|i| format!("http://one-host.example/{i}.png")).collect();
        drive_all(&mut r.cache, &urls, ImageKind::Logo);
        assert_eq!(fake.calls(), 8);
        assert!(fake.max_active.load(Ordering::SeqCst) <= MAX_FETCHES_PER_HOST);
    }

    #[test]
    fn stale_requests_are_skipped_and_can_be_requested_again() {
        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::from_millis(300));
        let tuning = Tuning { stale_after: Duration::from_millis(100), ..Tuning::default() };
        let mut r = rig(fake.clone(), 64 << 20, tuning);
        // "fast scroll": 30 urls requested once each, never again
        let urls: Vec<String> = (0..30).map(|i| format!("http://h{i}.example/l.png")).collect();
        for u in &urls {
            assert!(matches!(r.cache.get(u, ImageKind::Logo), ImageState::Loading));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !r.cache.inflight.is_empty() && Instant::now() < deadline {
            r.cache.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(r.cache.inflight.is_empty());
        let calls = fake.calls();
        assert!(calls <= MAX_FETCHES, "only the first permits fetched, got {calls}");
        assert!(r.cache.failed.is_empty(), "skips are not failures");
        assert!(r.cache.stats().0 <= calls);
        // a skipped url becomes visible again: it is requested anew and loads
        let last = urls.last().unwrap();
        assert!(matches!(drive(&mut r.cache, last, ImageKind::Logo), ImageState::Ready(_)));
    }

    #[test]
    fn uploads_at_most_4_mib_per_frame() {
        let fake = FakeFetch::new(png(1280, 720), Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake, 256 << 20, Tuning::default());
        let urls: Vec<String> = (0..3).map(|i| format!("http://b.example/{i}.png")).collect();
        let deadline = Instant::now() + Duration::from_secs(20);
        while r.cache.ready.len() < 3 && Instant::now() < deadline {
            for u in &urls {
                r.cache.get(u, ImageKind::Backdrop);
            }
            r.cache.drain_results();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(r.cache.ready.len(), 3, "all decoded before the first upload");
        for n in 1..=3 {
            for u in &urls {
                r.cache.get(u, ImageKind::Backdrop);
            }
            r.cache.pump();
            assert_eq!(r.cache.stats().0, n, "one backdrop (3.7 MB) per frame");
        }
    }

    #[test]
    fn lru_budget_evicts_idle_entries_only() {
        let fake = FakeFetch::new(png(40, 20), Answer::Bytes, Duration::ZERO);
        let one = 40 * 20 * 4;
        let mut r = rig(fake.clone(), 2 * one, Tuning::default());
        let urls: Vec<String> = (0..4).map(|i| format!("http://t.example/{i}.png")).collect();
        for u in &urls {
            assert!(matches!(drive(&mut r.cache, u, ImageKind::Logo), ImageState::Ready(_)));
        }
        // everything was used within the last frames: nothing evictable yet
        assert!(r.cache.stats().1 <= 4 * one);
        // only url 3 stays visible for a few frames
        for _ in 0..3 {
            r.cache.get(&urls[3], ImageKind::Logo);
            r.cache.pump();
        }
        let (n, bytes) = r.cache.stats();
        assert!(bytes <= 2 * one, "over budget: {bytes}");
        assert_eq!(n, 2);
        assert!(r.cache.textures.contains(&key_of(&urls[3], ImageKind::Logo)), "visible entry kept");
        assert!(!r.cache.textures.contains(&key_of(&urls[0], ImageKind::Logo)), "least recently used evicted");
        // an evicted entry reloads from disk, not from the network
        let calls = fake.calls();
        assert!(matches!(drive(&mut r.cache, &urls[0], ImageKind::Logo), ImageState::Ready(_)));
        assert_eq!(fake.calls(), calls);

        r.cache.clear_memory();
        assert_eq!(r.cache.stats(), (0, 0));
    }

    /// The budget holds even when everything on screen wants a texture: what doesn't fit waits
    /// (loading) until something scrolls away; an image larger than the whole budget fails.
    #[test]
    fn the_budget_is_never_exceeded_by_visible_images() {
        let fake = FakeFetch::new(png(40, 20), Answer::Bytes, Duration::ZERO);
        let one = 40 * 20 * 4;
        let mut r = rig(fake.clone(), 2 * one, Tuning::default());
        let urls: Vec<String> = (0..4).map(|i| format!("http://v.example/{i}.png")).collect();
        let ready = |c: &mut ImageCache, us: &[String]| us.iter().filter(|u| matches!(c.get(u, ImageKind::Logo), ImageState::Ready(_))).count();
        let deadline = Instant::now() + Duration::from_secs(10);
        while fake.calls() < 4 || r.cache.ready.len() < 2 {
            assert!(Instant::now() < deadline, "loads never finished");
            ready(&mut r.cache, &urls); // all four visible every frame
            r.cache.pump();
            assert!(r.cache.stats().1 <= 2 * one, "over budget: {:?}", r.cache.stats());
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..5 {
            ready(&mut r.cache, &urls);
            r.cache.pump();
        }
        assert_eq!(ready(&mut r.cache, &urls), 2, "two fit, two wait");
        assert!(r.cache.stats().1 <= 2 * one);
        // the first two scroll away: the waiting ones take their place (from memory, no refetch)
        for _ in 0..4 {
            ready(&mut r.cache, &urls[2..]);
            r.cache.pump();
        }
        assert_eq!(ready(&mut r.cache, &urls[2..]), 2);
        assert_eq!(r.cache.stats(), (2, 2 * one));
        assert_eq!(fake.calls(), 4);

        let big = FakeFetch::new(png(300, 300), Answer::Bytes, Duration::ZERO);
        let mut r = rig(big, 2 * one, Tuning::default());
        assert!(matches!(drive(&mut r.cache, "http://v.example/big.png", ImageKind::Logo), ImageState::Failed));
        assert_eq!(r.cache.stats(), (0, 0));
    }

    #[test]
    fn permanent_failures_are_negative_cached_in_memory_and_on_disk() {
        let fake = FakeFetch::new(vec![], Answer::NotFound, Duration::ZERO);
        let mut r = rig(fake.clone(), 1 << 20, Tuning::default());
        let url = "http://x.example/missing.png";
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        assert!(matches!(r.cache.get(url, ImageKind::Logo), ImageState::Failed));
        assert_eq!(fake.calls(), 1);
        assert!(cache_path(r.dir.path(), url).with_extension("fail").is_file());

        // a fresh instance (new session) trusts the marker: no request
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake2 = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::ZERO);
        let mut c2 = ImageCache::with_fetch(
            r.dir.path().to_path_buf(),
            fake2.clone(),
            rt.handle().clone(),
            egui::Context::default(),
            1 << 20,
            Tuning::default(),
        );
        assert!(matches!(drive(&mut c2, url, ImageKind::Logo), ImageState::Failed));
        assert_eq!(fake2.calls(), 0);

        // an expired marker is ignored
        let tuning = Tuning { disk_fail_ttl: Duration::ZERO, ..Tuning::default() };
        let mut c3 =
            ImageCache::with_fetch(r.dir.path().to_path_buf(), fake2.clone(), rt.handle().clone(), egui::Context::default(), 1 << 20, tuning);
        assert!(matches!(drive(&mut c3, url, ImageKind::Logo), ImageState::Ready(_)));
        assert_eq!(fake2.calls(), 1);
    }

    #[test]
    fn transient_failures_stay_in_memory_and_expire() {
        let fake = FakeFetch::new(vec![], Answer::Offline, Duration::ZERO);
        let tuning = Tuning { transient_fail_ttl: Duration::from_millis(50), ..Tuning::default() };
        let mut r = rig(fake.clone(), 1 << 20, tuning);
        let url = "http://x.example/blip.png";
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        assert!(!cache_path(r.dir.path(), url).with_extension("fail").exists(), "no disk marker for network errors");
        std::thread::sleep(Duration::from_millis(80));
        assert!(matches!(r.cache.get(url, ImageKind::Logo), ImageState::Loading), "retried after the short ttl");
        // still failing: the next retry waits 4× longer, capped at the permanent ttl
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        let key = key_of(url, ImageKind::Logo);
        let left = r.cache.failed[&key].saturating_duration_since(Instant::now());
        assert!(left > Duration::from_millis(50) && left <= Duration::from_millis(200), "longer than the first ttl: {left:?}");
        r.cache.strikes.insert(key, 20);
        r.cache.failed.clear();
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        let left = r.cache.failed[&key].saturating_duration_since(Instant::now());
        assert!(left <= Duration::from_secs(3600) && left > Duration::from_secs(3500), "capped at 1 h: {left:?}");
        assert_eq!(fake.calls(), 3);
    }

    #[test]
    fn a_load_resets_the_transient_back_off() {
        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake, 1 << 20, Tuning::default());
        let url = "http://x.example/back.png";
        r.cache.strikes.insert(key_of(url, ImageKind::Logo), 3);
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Ready(_)));
        assert!(r.cache.strikes.is_empty());
    }

    #[test]
    fn undecodable_bytes_fail_with_marker_and_no_cache_file() {
        let fake = FakeFetch::new(vec![], Answer::Garbage, Duration::ZERO);
        let mut r = rig(fake, 1 << 20, Tuning::default());
        let url = "http://x.example/html.png";
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        let p = cache_path(r.dir.path(), url);
        assert!(!p.exists());
        assert!(p.with_extension("fail").is_file());
    }

    #[test]
    fn unfetchable_urls_and_panics_fail_without_hanging() {
        let fake = FakeFetch::new(vec![], Answer::Panic, Duration::ZERO);
        let mut r = rig(fake.clone(), 1 << 20, Tuning::default());
        assert!(matches!(r.cache.get("", ImageKind::Logo), ImageState::Failed));
        assert!(matches!(drive(&mut r.cache, "file:///etc/passwd", ImageKind::Logo), ImageState::Failed));
        assert!(matches!(drive(&mut r.cache, "not a url", ImageKind::Logo), ImageState::Failed));
        assert_eq!(fake.calls(), 0);
        assert!(matches!(drive(&mut r.cache, "http://boom.example/x.png", ImageKind::Logo), ImageState::Failed));
        assert!(r.cache.inflight.is_empty());
    }

    fn with_decoder(r: &mut Rig, decoder: fn(&[u8], ImageKind) -> Result<egui::ColorImage>) {
        Arc::get_mut(&mut r.cache.shared).expect("no task started yet").decoder = decoder;
    }

    fn panicking_decoder(_: &[u8], _: ImageKind) -> Result<egui::ColorImage> {
        panic!("decoder exploded")
    }

    #[test]
    fn decoder_panics_are_permanent_failures_with_a_disk_marker() {
        // fetched bytes
        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake.clone(), 1 << 20, Tuning::default());
        with_decoder(&mut r, panicking_decoder);
        let url = "http://x.example/evil.png";
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        let p = cache_path(r.dir.path(), url);
        assert!(!p.exists(), "not cached");
        assert!(p.with_extension("fail").is_file(), "marker written");
        let until = r.cache.failed[&key_of(url, ImageKind::Logo)];
        assert!(until > Instant::now() + Duration::from_secs(600), "memory ttl of a permanent failure");

        // a cached file that panics the decoder is dropped and marked
        let mut r = rig(fake.clone(), 1 << 20, Tuning::default());
        with_decoder(&mut r, panicking_decoder);
        let url = "http://x.example/cached.png";
        let p = cache_path(r.dir.path(), url);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, png(4, 4)).unwrap();
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        assert!(!p.exists());
        assert!(p.with_extension("fail").is_file());
        assert_eq!(fake.calls(), 1, "only the first rig fetched");
    }

    static SVG_ACTIVE: AtomicUsize = AtomicUsize::new(0);
    static SVG_MAX_ACTIVE: AtomicUsize = AtomicUsize::new(0);

    /// Real decoder, but SVG renders take 600 ms (a pathological vector file).
    fn slow_svg_decoder(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> {
        if looks_like_svg(bytes) {
            let now = SVG_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
            SVG_MAX_ACTIVE.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(600));
            SVG_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
        decode(bytes, kind)
    }

    #[test]
    fn slow_svgs_render_one_at_a_time_and_never_block_rasters() {
        let fake = FakeFetch::new(vec![], Answer::NotFound, Duration::ZERO);
        let mut r = rig(fake.clone(), 64 << 20, Tuning::default());
        with_decoder(&mut r, slow_svg_decoder);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#;
        let svgs: Vec<String> = (0..3).map(|i| format!("http://s.example/{i}.svg")).collect();
        let raster = "http://r.example/logo.png".to_owned();
        for (u, bytes) in svgs.iter().map(|u| (u, svg.to_vec())).chain([(&raster, png(8, 8))]) {
            let p = cache_path(r.dir.path(), u);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        // SVGs requested first, so they would take both decode permits
        let all: Vec<&String> = svgs.iter().chain([&raster]).collect();
        let t = Instant::now();
        let raster_ready = loop {
            let states: Vec<ImageState> = all.iter().map(|u| r.cache.get(u, ImageKind::Logo)).collect();
            if matches!(states[3], ImageState::Ready(_)) {
                break t.elapsed();
            }
            assert!(t.elapsed() < Duration::from_secs(10), "raster never loaded");
            r.cache.pump();
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(raster_ready < Duration::from_millis(500), "raster waited for SVG renders: {raster_ready:?}");
        drive_all(&mut r.cache, &svgs, ImageKind::Logo);
        assert!(svgs.iter().all(|u| matches!(r.cache.get(u, ImageKind::Logo), ImageState::Ready(_))));
        assert_eq!(SVG_MAX_ACTIVE.load(Ordering::SeqCst), MAX_SVG_RENDERS);
        assert_eq!(fake.calls(), 0);
    }

    /// Real decoder, but SVG renders take 400 ms.
    fn slower_svg_decoder(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> {
        if looks_like_svg(bytes) {
            std::thread::sleep(Duration::from_millis(400));
        }
        decode(bytes, kind)
    }

    #[test]
    fn svg_renders_are_bounded_in_time_and_complexity() {
        let fake = FakeFetch::new(vec![], Answer::NotFound, Duration::ZERO);
        let mut r = rig(fake, 64 << 20, Tuning { svg_timeout: Duration::from_millis(100), ..Tuning::default() });
        with_decoder(&mut r, slower_svg_decoder);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#;
        let url = "http://s.example/slow.svg";
        let p = cache_path(r.dir.path(), url);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, svg).unwrap();
        // the render itself would succeed: failing means it was abandoned at the timeout
        assert!(matches!(drive(&mut r.cache, url, ImageKind::Logo), ImageState::Failed));
        assert!(p.with_extension("fail").is_file(), "marker written");
        // the abandoned render finishes later without re-caching anything
        std::thread::sleep(Duration::from_millis(500));
        assert!(!p.exists());

        // many nodes (here 12 000 rects, ~400 KiB) are rejected before rendering
        let mut many = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">"#);
        for i in 0..12_000 {
            many.push_str(&format!(r#"<rect x="{}" width="1" height="1"/>"#, i % 10));
        }
        many.push_str("</svg>");
        assert!(many.len() < MAX_SVG_BYTES);
        let e = decode(many.as_bytes(), ImageKind::Logo).unwrap_err();
        assert!(e.to_string().contains("nodes"), "{e}");
        assert!(decode(svg, ImageKind::Logo).is_ok());
    }

    #[test]
    fn images_on_a_playlists_host_use_its_user_agent() {
        let fake = FakeFetch::new(png(4, 4), Answer::Bytes, Duration::ZERO);
        let mut r = rig(fake.clone(), 1 << 20, Tuning::default());
        r.cache.set_source_agents(HashMap::from([("panel.example".to_owned(), "IPTVSmarters/1.0".to_owned())]));
        assert!(matches!(drive(&mut r.cache, "http://panel.example:8080/logos/1.png", ImageKind::Logo), ImageState::Ready(_)));
        assert!(matches!(drive(&mut r.cache, "https://cdn.example/2.png", ImageKind::Logo), ImageState::Ready(_)));
        let mut agents = lock(&fake.agents).clone();
        agents.sort();
        assert_eq!(
            agents,
            [
                ("http://panel.example:8080/logos/1.png".to_owned(), Some("IPTVSmarters/1.0".to_owned())),
                ("https://cdn.example/2.png".to_owned(), None)
            ]
        );
    }

    #[test]
    fn fit_keeps_aspect_and_bounds() {
        assert_eq!(fit(600.0, 300.0, (128, 128), false).0, 128);
        assert_eq!(fit(600.0, 300.0, (128, 128), false).1, 64);
        assert_eq!(fit(10.0, 5.0, (128, 128), false), (10, 5, 1.0));
        assert_eq!(fit(10.0, 5.0, (128, 128), true).0, 128);
        assert_eq!(fit(1.0, 10000.0, (128, 128), false).0, 1, "never 0");
    }

    #[test]
    fn keys_differ_by_kind_and_url() {
        let a = key_of("http://a", ImageKind::Logo);
        assert_ne!(a, key_of("http://a", ImageKind::Poster));
        assert_ne!(a, key_of("http://b", ImageKind::Logo));
        assert_eq!(a, key_of("http://a", ImageKind::Logo));
    }
}
