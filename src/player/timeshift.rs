//! Local live timeshift, using the existing demuxer and connection.
//!
//! mpv 0.41's disk cache is append-only; max-back-bytes limits METADATA when on disk.
//! Therefore we also inspect file-cache-bytes, stop disk writes near 2 GiB, and continue
//! with a two-minute memory window until the next load. Never reload just to rotate a cache.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use libmpv2::Mpv;
use serde::Deserialize;

use super::LiveBufferRange;
use crate::model::TileId;

const DISK_WINDOW: f64 = 1800.0;
const MEMORY_WINDOW: f64 = 120.0;
const DISK_BYTES: u64 = 2 << 30;
const DISK_RESERVE: u64 = 3_000_000_000;
// Leave room for writes between controller ticks, including a large HLS segment.
const WRITE_HEADROOM: u64 = 32 << 20;
static NEXT_CACHE: AtomicU64 = AtomicU64::new(0);

#[derive(Default, Deserialize)]
pub(super) struct CacheState {
    #[serde(default, rename = "seekable-ranges")]
    ranges: Vec<Range>,
    #[serde(default, rename = "file-cache-bytes")]
    file_bytes: u64,
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub eof: bool,
}

#[derive(Deserialize)]
struct Range {
    start: f64,
    end: f64,
}

impl CacheState {
    pub fn read(mpv: &Mpv) -> Self {
        mpv.get_property::<String>("demuxer-cache-state").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Only the newest contiguous range is live. Do not allow a seek across timestamp gaps.
    pub fn range(&self, window: f64) -> Option<LiveBufferRange> {
        let mut ranges: Vec<_> = self
            .ranges
            .iter()
            .filter(|r| r.start.is_finite() && r.end.is_finite() && r.end > r.start)
            .map(|r| LiveBufferRange { start: r.start, end: r.end })
            .collect();
        ranges.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut merged: Vec<LiveBufferRange> = Vec::new();
        for r in ranges {
            if let Some(last) = merged.last_mut().filter(|last| r.start <= last.end) {
                last.end = last.end.max(r.end);
            } else {
                merged.push(r);
            }
        }
        let mut r = merged.pop()?;
        r.start = r.start.max(r.end - window);
        // Seeking at the exclusive end can escape the cache (and reopen the provider URL).
        r.end -= 0.1;
        (r.end - r.start > 0.2).then_some(r)
    }
}

pub(super) struct LiveCache {
    dir: Option<PathBuf>,
    pub active: bool,
    disk: bool,
    warned: bool,
}

impl LiveCache {
    pub fn new(cache_dir: Option<&Path>, tile: TileId) -> Self {
        let root = cache_dir.map(Path::to_path_buf).or_else(|| crate::paths::AppPaths::from_env().ok().map(|p| p.cache_dir));
        let dir = root.and_then(|root| make_dir(&root, tile).map_err(|e| tracing::warn!("live cache: {e}")).ok());
        Self { dir, active: false, disk: false, warned: false }
    }

    pub fn window(&self) -> f64 {
        if self.disk { DISK_WINDOW } else { MEMORY_WINDOW }
    }

    pub fn prepare(&mut self, live: bool, args: &mut [String]) {
        self.clear();
        self.active = live;
        self.disk = live && self.dir.as_deref().and_then(free_bytes).is_some_and(|n| n >= DISK_RESERVE);
        if !live {
            return;
        }
        if !self.disk {
            self.warn("less than 3 GB free or disk cache unavailable");
        }
        let opts = &mut args[3];
        opts.push_str(",cache=yes,demuxer-seekable-cache=yes,force-seekable=yes,demuxer-donate-buffer=no,demuxer-cache-unlink-files=immediate");
        // Let the controller handle reconnects: every new load gets an empty cache.
        opts.push_str(",stream-lavf-o=reconnect=0");
        if self.disk {
            let dir = self.dir.as_ref().expect("disk has directory");
            opts.push_str(&format!(",demuxer-cache-dir={},cache-on-disk=yes", super::mpv::quote(&dir.to_string_lossy())));
        } else {
            opts.push_str(",cache-on-disk=no");
        }
        for (name, value) in self.settings() {
            opts.push_str(&format!(",{name}={value}"));
        }
    }

    fn settings(&self) -> [(&'static str, String); 4] {
        // More than the exposed window: a paused reader is advanced to the oldest point
        // before prefetch stops. Disk limits count metadata, memory limits count payload too.
        [
            ("cache-secs", (self.window() + 10.0).to_string()),
            ("demuxer-readahead-secs", (self.window() + 10.0).to_string()),
            ("demuxer-max-bytes", if self.disk { "64MiB" } else { "128MiB" }.into()),
            ("demuxer-max-back-bytes", if self.disk { "32MiB" } else { "128MiB" }.into()),
        ]
    }

    pub fn apply(&self, mpv: &Mpv) {
        if !self.active {
            return;
        }
        for (name, value) in self.settings() {
            set(mpv, name, &value);
        }
    }

    pub fn maintain(&mut self, mpv: &Mpv, state: &CacheState) {
        if self.disk && (state.file_bytes >= DISK_BYTES - WRITE_HEADROOM || self.dir.as_deref().and_then(free_bytes).is_none_or(|n| n < DISK_RESERVE))
        {
            // Existing packets remain readable on disk. mpv closes that file on the next
            // channel, reconnect or stop; disabling this option only stops further writes.
            set(mpv, "cache-on-disk", "no");
            self.disk = false;
            self.apply(mpv);
            self.warn("disk write limit or free-space reserve reached");
        }
    }

    fn warn(&mut self, why: &str) {
        if !self.warned {
            tracing::info!("live rewind: {why}; using up to two minutes in memory until the next load");
            self.warned = true;
        }
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.disk = false;
        if let Some(dir) = &self.dir {
            clear_contents(dir);
        }
    }
}

impl Drop for LiveCache {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn set(mpv: &Mpv, name: &str, value: &str) {
    if let Err(e) = mpv.set_property(name, value) {
        tracing::warn!("live cache {name}: {e}");
    }
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(dir).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

fn clear_contents(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn make_dir(cache: &Path, tile: TileId) -> std::io::Result<PathBuf> {
    let root = cache.join("live-rewind");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
    if fs::symlink_metadata(&root)?.file_type().is_symlink() {
        return Err(std::io::Error::other("live cache directory is a symlink"));
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    // Private per-controller folders keep parallel tests, pictures and profiles independent.
    // A crashed process's empty folder is removed at the next start (files are unlinked by mpv).
    for entry in fs::read_dir(&root)?.flatten() {
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.split('-').next()).and_then(|n| n.parse::<u32>().ok())
            && !Path::new("/proc").join(pid.to_string()).exists()
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let dir = root.join(format!("{}-{}-{}", std::process::id(), tile.letter(), NEXT_CACHE.fetch_add(1, Ordering::Relaxed)));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_ranges_merge_overlap_but_never_bridge_gaps() {
        let state: CacheState =
            serde_json::from_str(r#"{"seekable-ranges":[{"start":100,"end":200},{"start":0,"end":50},{"start":180,"end":300}]}"#).unwrap();
        assert_eq!(state.range(1800.0), Some(LiveBufferRange { start: 100.0, end: 299.9 }));
        assert_eq!(state.range(120.0), Some(LiveBufferRange { start: 180.0, end: 299.9 }));
        assert_eq!(CacheState::default().range(1800.0), None);
    }

    #[test]
    fn private_cache_cleans_on_new_load_and_exit() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = LiveCache::new(Some(root.path()), TileId::A);
        let path = cache.dir.clone().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        fs::write(path.join("old"), b"old stream").unwrap();
        cache.prepare(false, &mut [String::new(), String::new(), String::new(), String::new()]);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        drop(cache);
        assert!(!path.exists());
    }

    #[test]
    fn disk_cap_switches_mpv_to_memory_without_a_reload() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = LiveCache::new(Some(root.path()), TileId::A);
        cache.active = true;
        cache.disk = true;
        let mut cfg = crate::config::Config::default();
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        let mpv = super::super::mpv::create(&cfg, TileId::A).unwrap();
        mpv.set_property("cache-on-disk", true).unwrap();
        cache.maintain(&mpv, &CacheState { file_bytes: DISK_BYTES, ..Default::default() });
        assert!(!mpv.get_property::<bool>("cache-on-disk").unwrap());
        assert_eq!(mpv.get_property::<i64>("demuxer-max-back-bytes").unwrap(), 128 << 20);
        assert_eq!(mpv.get_property::<f64>("cache-secs").unwrap(), 130.0);
        assert_eq!(cache.window(), MEMORY_WINDOW);
        assert!(cache.warned);
        // Freeing disk later doesn't reopen the append-only file during this stream.
        cache.maintain(&mpv, &CacheState::default());
        assert!(!cache.disk);
    }
}
