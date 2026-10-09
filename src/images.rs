//! Image pipeline: logos, posters, backdrops. Owner: WP-09. DESIGN §18.
//! SKELETON SAFE DEFAULTS (DESIGN §24.1): `get` → `Failed` (callers draw the monogram),
//! `pump`/`clear_memory` no-ops, `stats` → (0, 0), so every screen runs before WP-09 lands.
//!
//! ```text
//! UI get(url, kind) ─hit──▶ TextureHandle (LRU touch)
//!        │ miss
//!        ▼
//!  wanted-set (url → last frame requested) ──▶ tokio: disk cache? ──no──▶ HTTP (Semaphore 6,
//!                                               │ yes                     ≤ 2 per host, 8 MiB cap)
//!                                               ▼                          │ write ~/.cache/tuner/img/ab/<blake3>.bin
//!                                     spawn_blocking decode (Semaphore 2): image / resvg (SVG),
//!                                     downscale with fast_image_resize to kind's box, premultiply
//!                                               ▼
//!                                     decoded queue ──▶ UI `pump`: ≤ 4 MiB of texture uploads per
//!                                                       frame (one backdrop ≈ 3.7 MB), ctx.load_texture
//! ```
//! * Untrusted input: decode with `image::ImageReader` + `Limits { max_image_width: 8192,
//!   max_image_height: 8192, max_alloc: 64 MiB }` (a 20k×20k PNG must not allocate GBs); SVG:
//!   reject sources > 1 MiB, `usvg::Options` with an EMPTY fontdb (never `load_system_fonts`),
//!   raster size clamped to the kind's box. Downloads are capped at 8 MiB.
//! * Fetching goes through a private `trait Fetch` (implemented for `HttpClient`) so WP-09's
//!   tests run without WP-04.
//! * The wanted-map is pruned of entries older than 2 s on every `pump`.
//! * Stale requests: a fetch is skipped if its url wasn't requested in the last 2 s (fast
//!   scrolling through 100k channels must not queue 100k downloads).
//! * Failures are negative-cached in memory for 1 h and on disk (`.fail` marker) for 24 h.
//! * Memory: LRU over texture bytes (default budget 192 MiB GPU, configurable), evicting entries
//!   not used for ≥ 2 frames first.
//! * Placeholder (UI decides): monogram of the name on fg@0.04; missing logo ≠ loading state.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::error::Result;
use crate::paths::AppPaths;
use crate::sources::http::HttpClient;

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
}

#[derive(Clone)]
pub enum ImageState {
    /// Requested; show a neutral placeholder (no spinner for logos).
    Loading,
    Ready(egui::TextureHandle),
    /// Failed/invalid: show the monogram placeholder.
    Failed,
}

/// UI-thread facade. Not `Send` (owns textures); background work happens on the runtime.
/// Private fields are WP-09's to reshape; only the pub API is frozen.
pub struct ImageCache {
    dir: PathBuf,
    http: HttpClient,
    rt: tokio::runtime::Handle,
    egui: egui::Context,
    budget_bytes: usize,
    frame: u64,
    inner: Option<Box<dyn std::any::Any>>, // WP-09: replace with real state (LRU, queues)
}

impl ImageCache {
    pub fn new(paths: &AppPaths, http: HttpClient, rt: tokio::runtime::Handle, egui: egui::Context, budget_bytes: usize) -> Self {
        Self { dir: paths.image_cache(), http, rt, egui, budget_bytes, frame: 0, inner: None }
    }

    /// Cheap lookup; on miss enqueues a fetch and returns `Loading`. Call every frame for
    /// visible items only.
    pub fn get(&mut self, url: &str, kind: ImageKind) -> ImageState { ImageState::Failed }

    /// Per frame (start of `App::ui`): upload ≤ 4 MiB of decoded images, evict over budget,
    /// prune stale wanted entries.
    pub fn pump(&mut self) {
        self.frame += 1;
    }

    /// Drop all textures (theme/scale change does NOT require this; memory pressure does).
    pub fn clear_memory(&mut self) {}

    /// (textures, bytes) for the debug overlay.
    pub fn stats(&self) -> (usize, usize) { (0, 0) }
}

/// Disk cache path for a URL: `<dir>/<hh>/<blake3 hex>.bin`.
pub fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let h = blake3::hash(url.as_bytes()).to_hex();
    dir.join(&h[..2]).join(format!("{h}.bin"))
}

/// Decode PNG/JPEG/WebP/GIF(first frame)/SVG bytes and downscale into `kind`'s box.
pub fn decode(bytes: &[u8], kind: ImageKind) -> Result<egui::ColorImage> { todo!("WP-09") }

/// Delete least-recently-modified cache files until the directory is under `max_bytes`
/// (maintenance job; default 500 MiB).
pub fn prune_disk(dir: &Path, max_bytes: u64) -> Result<u64> { todo!("WP-09") }
