//! Bounded, per-picture segment ownership and the private mpv playlist dialect.
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;

use bytes::Bytes;

use super::playlist::Resource;
use crate::player::LiveBufferRange;

pub(super) const MEMORY_BYTES: u64 = 128 << 20;

pub(super) struct Blob {
    pub resource: Resource,
    pub data: Option<Bytes>,
    pub size: u64,
    pub extension: &'static str,
}

pub(super) struct Segment {
    pub id: u64,
    pub seq: u64,
    pub start: f64,
    pub end: f64,
    pub disc: u64,
    pub key: Option<(u64, String)>,
    pub map: Option<u64>,
}

#[derive(Default)]
pub(super) struct Track {
    pub segments: VecDeque<Segment>,
    pub edge: f64,
    pub target: f64,
    pub ended: bool,
    pub next_seq: u64,
    pub ready: bool,
    pub discontinuity: u64,
}

pub(super) struct Store {
    pub dir: PathBuf,
    pub tracks: Vec<Track>,
    pub blobs: BTreeMap<u64, Blob>,
    pub window: f64,
    pub memory: bool,
    pub epoch: u64,
    pub closed: bool,
    pub next: u64,
    pub pending: Vec<u64>,
}

impl Store {
    pub fn new(dir: PathBuf, window: f64) -> Self {
        Self {
            dir,
            tracks: vec![Track::default()],
            blobs: BTreeMap::new(),
            window,
            memory: false,
            epoch: 0,
            closed: false,
            next: 0,
            pending: Vec::new(),
        }
    }

    pub fn memory(window: f64) -> Self {
        let mut store = Self::new(PathBuf::new(), window);
        store.memory = true;
        store
    }

    pub fn range(&self) -> Option<LiveBufferRange> {
        let t = self.tracks.first()?;
        let start = t.segments.front()?.start;
        // Leave enough video for an exact seek (audio may extend beyond the last picture).
        let end = (t.edge - 0.5).max(start);
        (end > start).then_some(LiveBufferRange { start, end })
    }

    /// Prune BEFORE allocating the arriving segment. A segment crossing the cutoff stays:
    /// retained duration is at most window + one source segment, including the new write.
    pub fn prepare(&mut self, track: usize, duration: f64) {
        self.pending.clear();
        let t = &mut self.tracks[track];
        let window = if self.memory { 6.0 } else { self.window };
        let cutoff = t.edge + duration - window;
        while t.segments.front().is_some_and(|s| s.end <= cutoff) {
            t.segments.pop_front();
        }
        self.prune();
    }

    pub fn prune(&mut self) {
        self.blobs.retain(|id, b| {
            let used = self.pending.contains(id)
                || self
                    .tracks
                    .iter()
                    .flat_map(|t| &t.segments)
                    .any(|s| s.id == *id || s.map == Some(*id) || s.key.as_ref().is_some_and(|(k, _)| k == id));
            if !used && b.data.is_none() {
                let _ = fs::remove_file(self.dir.join(format!("{id}.{}", b.extension)));
            }
            used
        });
    }

    pub fn reset(&mut self) {
        self.tracks.iter_mut().for_each(|t| *t = Track::default());
        self.epoch += 1;
        self.pending.clear();
        self.prune();
    }

    pub fn memory_fallback(&mut self) {
        if self.memory {
            return;
        }
        // Keep a short local supply for the decoder while its own packet cache takes over.
        for t in &mut self.tracks {
            while t.segments.len() > 3 {
                t.segments.pop_front();
            }
        }
        self.prune();
        while self.blobs.values().map(|b| b.size).sum::<u64>() > MEMORY_BYTES {
            let Some(t) = self.tracks.iter_mut().filter(|t| !t.segments.is_empty()).max_by_key(|t| t.segments.len()) else { break };
            t.segments.pop_front();
            self.prune();
        }
        for (id, b) in &mut self.blobs {
            let path = self.dir.join(format!("{id}.{}", b.extension));
            b.data = Some(fs::read(&path).unwrap_or_default().into());
            let _ = fs::remove_file(path);
        }
        self.memory = true;
    }

    pub fn insert(&mut self, resource: Resource, bytes: Bytes, extension: &'static str) -> std::io::Result<u64> {
        let id = self.next;
        self.next += 1;
        let size = bytes.len() as u64;
        let data = if self.memory {
            // The fetcher holds at most one additional capped response. Slow localhost
            // clients cannot hold deleted disk inodes: serving copies a single blob to RAM.
            while self.blobs.values().map(|b| b.size).sum::<u64>() + size > MEMORY_BYTES {
                let Some(t) = self.tracks.iter_mut().filter(|t| !t.segments.is_empty()).max_by_key(|t| t.segments.len()) else { break };
                t.segments.pop_front();
                self.prune();
            }
            Some(bytes)
        } else {
            let path = self.dir.join(format!("{id}.{extension}"));
            if let Err(e) = fs::write(&path, &bytes) {
                let _ = fs::remove_file(path);
                return Err(e);
            }
            None
        };
        self.pending.push(id);
        self.blobs.insert(id, Blob { resource, data, size, extension });
        Ok(id)
    }

    pub fn resource(&self, r: &Resource) -> Option<u64> {
        self.blobs.iter().find(|(_, b)| b.resource == *r).map(|(id, _)| *id)
    }

    pub fn boundary(&self, from: f64) -> Option<f64> {
        self.tracks
            .iter()
            .filter_map(|t| {
                let first = t.segments.iter().find(|s| s.end > from)?;
                t.segments.iter().find(|s| s.seq > first.seq && s.disc != first.disc).map(|s| s.start)
            })
            .min_by(f64::total_cmp)
    }

    pub fn playlist(&self, track: usize, from: Option<f64>) -> Option<String> {
        let t = self.tracks.get(track)?;
        let first = t.segments.iter().find(|s| from.is_none_or(|p| s.end > p)).or_else(|| t.segments.back())?;
        let target = t.target.max(t.segments.iter().map(|s| s.end - s.start).fold(0.0, f64::max)).ceil().max(1.0);
        // libavformat refuses seeks in ordinary sliding live HLS. EVENT enables them,
        // while advancing MEDIA-SEQUENCE updates its first timestamp as files are pruned.
        // This is a private mpv interface, deliberately not an RFC EVENT publication.
        let mut out = format!(
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
            first.seq, first.disc
        );
        for s in t.segments.iter().filter(|s| s.seq >= first.seq).take_while(|s| s.disc == first.disc) {
            if let Some((id, iv)) = &s.key {
                out.push_str(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{id}.key\",IV={iv}\n"));
            } else {
                out.push_str("#EXT-X-KEY:METHOD=NONE\n");
            }
            if let Some(id) = s.map {
                out.push_str(&format!("#EXT-X-MAP:URI=\"{id}.mp4\"\n"));
            }
            let b = self.blobs.get(&s.id)?;
            out.push_str(&format!("#EXTINF:{:.6},\n{}.{}\n", s.end - s.start, s.id, b.extension));
        }
        if t.ended || t.segments.iter().any(|s| s.seq > first.seq && s.disc != first.disc) {
            // End this local decoder epoch before a provider timestamp reset. The
            // controller opens the next retained segment without touching the recorder.
            out.push_str("#EXT-X-ENDLIST\n");
        }
        Some(out)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if !self.dir.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn add(s: &mut Store, duration: f64) {
        s.prepare(0, duration);
        let id =
            s.insert(Resource { uri: format!("https://example.test/secret/{}", s.next), range: None }, Bytes::from_static(b"segment"), "ts").unwrap();
        let t = &mut s.tracks[0];
        t.segments.push_back(Segment { id, seq: id, start: t.edge, end: t.edge + duration, disc: 0, key: None, map: None });
        t.edge += duration;
    }
    #[test]
    fn rolling_prunes_before_writing_and_has_no_private_metadata() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::new(root.path().to_owned(), 20.0);
        for _ in 0..40 {
            add(&mut s, 2.0);
            assert!(fs::read_dir(root.path()).unwrap().count() <= 10);
        }
        assert_eq!(s.range(), Some(LiveBufferRange { start: 60.0, end: 79.5 }));
        let p = s.playlist(0, None).unwrap();
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:30\n"));
        assert!(!p.contains("secret") && !p.contains("https:"));
        s.reset();
        assert!(s.range().is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn variable_segments_keep_the_cutoff_segment_and_memory_removes_disk() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::new(root.path().to_owned(), 20.0);
        for d in [3.0, 7.0, 4.0, 8.0, 5.0, 6.0] {
            add(&mut s, d);
        }
        let r = s.range().unwrap();
        assert!(r.start <= 13.0 && 33.0 - r.start < 28.0);
        s.memory_fallback();
        assert!(s.memory);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        add(&mut s, 2.0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
