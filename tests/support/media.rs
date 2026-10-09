//! Test media generated with ffmpeg (scripts/gen-test-media.sh) — never committed. Owner: WP-07.
//! Tests that need media call `ensure_media()` and SKIP (return early with a note) when ffmpeg
//! is unavailable, so `cargo test` stays green on minimal machines.

use std::path::PathBuf;

/// Directory with `testsrc.mp4` (10 s 640×360 25 fps + 1 kHz sine), `bars.ts` (MPEG-TS),
/// `hls/index.m3u8` (6 × 2 s segments) — generated on first use into `target/test-media`.
pub fn ensure_media() -> Option<PathBuf> { todo!("WP-07") }

/// Serve `dir` over HTTP on an ephemeral port with optional fault injection (drop the
/// connection after N bytes, 404 every Nth segment) — reconnect tests. Returns the base URL.
pub async fn serve(dir: PathBuf, faults: Faults) -> String { todo!("WP-07") }

#[derive(Debug, Clone, Copy, Default)]
pub struct Faults {
    pub drop_after_bytes: Option<u64>,
    pub segment_404_every: Option<u32>,
}
