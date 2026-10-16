//! Bounded, per-picture segment ownership and the private mpv playlist dialect.
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;

use bytes::Bytes;

use super::playlist::Resource;
use crate::player::LiveBufferRange;

pub(super) const MEMORY_BYTES: u64 = 128 << 20;

#[derive(Clone)]
pub(super) struct Blob {
    pub resource: Resource,
    pub data: Option<Bytes>,
    pub size: u64,
    pub extension: &'static str,
}

#[derive(Clone)]
pub(super) struct Segment {
    pub id: u64,
    pub seq: u64,
    pub start: f64,
    pub end: f64,
    pub disc: u64,
    pub key: Option<(u64, String)>,
    pub map: Option<u64>,
}

#[derive(Clone, Default)]
pub(super) struct Track {
    pub segments: VecDeque<Segment>,
    pub edge: f64,
    pub target: f64,
    pub ended: bool,
    pub next_seq: u64,
    pub ready: bool,
    pub discontinuity: u64,
}

#[derive(Clone)]
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
    /// A download owns these bytes past the live window and past `Drop` (DESIGN §43 §3).
    pub keep: bool,
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
            keep: false,
        }
    }

    /// Resume with local counters only; a fresh provider window begins a discontinuity.
    pub fn resume(dir: PathBuf, window: f64) -> std::io::Result<Self> {
        let index = super::finish::Recording::load(&dir).map_err(std::io::Error::other)?;
        let mut store = Self::new(dir, window);
        store.keep = true;
        store.tracks.clear();
        for segments in index.tracks {
            let mut track = Track::default();
            for s in segments {
                let extension = match s.extension.as_str() {
                    "ts" => "ts",
                    "m4s" => "m4s",
                    "mp4" => "mp4",
                    "aac" => "aac",
                    "mp3" => "mp3",
                    "ac3" => "ac3",
                    "ec3" => "ec3",
                    _ => unreachable!(),
                };
                let mut blobs = vec![(s.id, extension)];
                if let Some((key, _)) = &s.key {
                    blobs.push((*key, "key"));
                }
                if let Some(map) = s.map {
                    blobs.push((map, "mp4"));
                }
                for (id, ext) in blobs {
                    let size = fs::metadata(store.dir.join(format!("{id}.{ext}")))?.len();
                    store.blobs.insert(id, Blob { resource: Resource { uri: format!("kept:{id}"), range: None }, data: None, size, extension: ext });
                    store.next = store.next.max(id + 1);
                }
                track.segments.push_back(Segment {
                    id: s.id,
                    seq: track.next_seq,
                    start: s.start,
                    end: s.end,
                    disc: s.discontinuity,
                    key: s.key,
                    map: s.map,
                });
                track.next_seq += 1;
                track.edge = s.end;
                track.discontinuity = s.discontinuity;
            }
            store.tracks.push(track);
        }
        // A crash may leave an unindexed blob; never reuse its counter.
        for entry in fs::read_dir(&store.dir)? {
            let entry = entry?;
            if let Some(id) = entry.file_name().to_str().and_then(|n| n.split('.').next()).and_then(|s| s.parse::<u64>().ok()) {
                store.next = store.next.max(id + 1);
            }
        }
        Ok(store)
    }

    pub fn memory(window: f64) -> Self {
        let mut store = Self::new(PathBuf::new(), window);
        store.memory = true;
        store
    }

    pub fn range(&self) -> Option<LiveBufferRange> {
        let t = self.tracks.first()?;
        let start = t.segments.front()?.start;
        // The live point is the newest complete segment's start: a seek has a full
        // segment to play while the next one is published. Badges use this same point.
        let end = t.segments.back()?.start;
        Some(LiveBufferRange { start, end })
    }

    /// Prune BEFORE allocating the arriving segment. A segment crossing the cutoff stays:
    /// retained duration is at most window + one source segment, including the new write.
    pub fn prepare(&mut self, track: usize, duration: f64) {
        self.pending.clear();
        if self.keep {
            return;
        }
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
        if self.keep {
            for t in &mut self.tracks {
                t.ended = true;
            }
            let _ = self.save();
            return;
        }
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
            if let Err(e) = super::finish::write_private(&path, &bytes) {
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

    /// Preserve the retained history from the programme's start (or the earliest available
    /// segment), then stop pruning. The provider reader and its lease stay where they are.
    pub fn adopt(&mut self, dir: PathBuf, from: f64) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        if self.memory {
            return Err(std::io::Error::other("recording store unavailable"));
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let mut next = self.clone();
        next.dir = dir;
        next.keep = false;
        next.memory = false;
        for track in &mut next.tracks {
            while track.segments.front().is_some_and(|s| s.end <= from) {
                track.segments.pop_front();
            }
        }
        next.prune();
        // Switch only after every retained blob and the durable index are safe. A failed
        // move leaves both the current reader and its rewind history untouched.
        for (id, blob) in &next.blobs {
            let name = format!("{id}.{}", blob.extension);
            let to = next.dir.join(&name);
            if !to.exists() && fs::hard_link(self.dir.join(&name), &to).is_err() {
                let bytes = fs::read(self.dir.join(&name))?;
                super::finish::write_private(&to, &bytes)?;
            }
        }
        next.keep = true;
        if let Err(error) = next.save() {
            next.keep = false;
            return Err(error);
        }
        *self = next;
        Ok(())
    }

    pub fn resume_rolling(&mut self, dir: PathBuf) -> std::io::Result<()> {
        self.save()?;
        let mut next = self.clone();
        next.dir = dir;
        next.keep = false;
        let from = next.tracks[0].edge - next.window;
        for track in &mut next.tracks {
            while track.segments.front().is_some_and(|s| s.end < from) {
                track.segments.pop_front();
            }
        }
        next.prune();
        for (id, blob) in &next.blobs {
            let name = format!("{id}.{}", blob.extension);
            if std::fs::hard_link(self.dir.join(&name), next.dir.join(&name)).is_err() {
                let bytes = fs::read(self.dir.join(&name))?;
                super::finish::write_private(&next.dir.join(name), &bytes)?;
            }
        }
        *self = next;
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        if !self.keep {
            return Ok(());
        }
        let tracks = self
            .tracks
            .iter()
            .map(|t| {
                t.segments
                    .iter()
                    .filter_map(|s| {
                        let blob = self.blobs.get(&s.id)?;
                        Some(super::finish::Segment {
                            id: s.id,
                            extension: blob.extension.into(),
                            start: s.start,
                            end: s.end,
                            discontinuity: s.disc,
                            key: s.key.clone(),
                            map: s.map,
                        })
                    })
                    .collect()
            })
            .collect();
        super::finish::Recording { tracks }.save(&self.dir)
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

    /// A relay consumes RFC discontinuities itself; unlike mpv's per-epoch reader it must
    /// see every run and may refresh independently of a picture's seek (DESIGN §43 §2.4).
    pub fn relay_playlist(&self, track: usize) -> Option<String> {
        let t = self.tracks.get(track)?;
        let first = t.segments.front()?;
        let target = t.target.max(t.segments.iter().map(|s| s.end - s.start).fold(1.0, f64::max)).ceil();
        let mut out = format!(
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
            first.seq, first.disc
        );
        let mut disc = first.disc;
        for s in &t.segments {
            if s.disc != disc {
                out.push_str("#EXT-X-DISCONTINUITY\n");
                disc = s.disc;
            }
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
        if t.ended {
            out.push_str("#EXT-X-ENDLIST\n");
        }
        Some(out)
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
        if !self.keep && !self.dir.as_os_str().is_empty() {
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
        assert_eq!(s.range(), Some(LiveBufferRange { start: 60.0, end: 78.0 }));
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
    #[test]
    fn kept_store_resumes_after_crash_without_reusing_blob_counters() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::new(root.path().to_owned(), 1800.0);
        store.keep = true;
        add(&mut store, 2.0);
        add(&mut store, 2.0);
        store.save().unwrap();
        fs::write(root.path().join("9.ts"), b"unindexed crash tail").unwrap();
        drop(store);
        let mut resumed = Store::resume(root.path().to_owned(), 1800.0).unwrap();
        assert_eq!(resumed.next, 10);
        assert_eq!(resumed.tracks[0].edge, 4.0);
        add(&mut resumed, 2.0);
        resumed.save().unwrap();
        assert_eq!(fs::read(root.path().join("0.ts")).unwrap(), b"segment");
        assert_eq!(fs::read(root.path().join("9.ts")).unwrap(), b"unindexed crash tail");
        assert_eq!(super::super::finish::Recording::load(root.path()).unwrap().duration(), 6.0);
    }

    #[test]
    fn adopt_keeps_history_then_finishes_without_stopping_the_picture() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let rolling = root.path().join("1");
        fs::create_dir(&rolling).unwrap();
        let mut s = Store::new(rolling.clone(), 1800.0);
        for _ in 0..900 {
            add(&mut s, 2.0);
        }
        let kept = root.path().join("2");
        let original_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(rolling.join("300.ts")).unwrap());
        s.adopt(kept.clone(), 600.0).unwrap();
        assert!(!rolling.exists());
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&fs::metadata(kept.join("300.ts")).unwrap()), original_inode);
        assert_eq!(s.range().unwrap().start, 600.0);
        for _ in 0..900 {
            add(&mut s, 2.0);
        }
        s.save().unwrap();
        assert_eq!(s.range().unwrap().start, 600.0, "keep mode has no rolling cut");
        assert!(kept.join("300.ts").exists());
        assert_eq!(fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(kept.join("300.ts")).unwrap().permissions().mode() & 0o777, 0o600);
        let index = fs::read_to_string(kept.join("index.json")).unwrap();
        assert!(!index.contains("http") && !index.contains("secret"));
        let next = root.path().join("3");
        fs::create_dir(&next).unwrap();
        s.resume_rolling(next.clone()).unwrap();
        assert!(!s.keep);
        assert!(s.range().unwrap().start >= 1798.0);
        drop(s);
        assert!(!next.exists());
        assert!(kept.join("300.ts").exists(), "the finishing worker owns the kept bytes");
    }
}
