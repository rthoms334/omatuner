//! The source's keyframe index (cast design §8.5 "Full-length VOD", Phase 2 design §29.6): the
//! video track's keyframe presentation times, read from the container's own index through the
//! loopback proxy by `Range` (never the whole film: the `moov` box, or the Matroska `SeekHead`,
//! `Info`, `Tracks` and `Cues`), and for each one a `-ss` time that lands ffmpeg's demuxer on it.
//!
//! * **MP4** (ISO/IEC 14496-12): the top-level boxes are walked by their sizes (the `moov` at the
//!   start, or after the `mdat` at the end: one request each), then the first `vide` track with
//!   more than one sample: `stts` (decode times), `ctts` (composition offsets, read signed as
//!   ffmpeg does), `stss` (sync samples; none means every sample is one) and `elst`. Times are
//!   normalised as ffmpeg's mov demuxer does (`mov_fix_index`): the edit's `media_time` is time 0,
//!   a leading empty edit delays it, and a video track whose first presented picture is still
//!   after 0 is shifted to 0. More than one non-empty edit, a fragmented file (`mvex`) or a
//!   damaged box is no index (the item keeps its EVENT playlist).
//! * **Matroska** (RFC 9559): `SeekHead` locates `Info` (`TimestampScale`, `Duration`), `Tracks`
//!   (the first video track) and `Cues`; every `CuePoint` with a `CueTrackPositions` of the video
//!   track is a keyframe at `CueTime × TimestampScale`. Sparse cues give longer segments, never
//!   wrong ones; no cues (a file written to a pipe) is no index.
//! * **Seek windows.** ffmpeg's `-ss X` seeks the video to the last keyframe at or before X on its
//!   index: the mov demuxer searches decode times (`mov_seek_stream`: "we consider timestamp to be
//!   PTS, hence try to offset it so that we can search over the DTS timeline"), the Matroska one
//!   the cue times. So X lands on keyframe *k* when it is at or after *k*'s presentation time and
//!   before the next keyframe's decode time (MP4) or cue (Matroska): X = pts + min(0.5 s, half the
//!   gap), Jellyfin's "+0.5 s to get the exact keyframe" made exact by the index. A keyframe with
//!   no such window (a B-frame delay longer than the gap to the next one) is no cut point.
//!
//! Times here are on the container's clock; the segment map makes them relative to the file's
//! start (ffprobe's `format.start_time`, which `-copyts -start_at_zero` subtracts).

use std::time::Duration;

use reqwest::header;

/// Bytes read at the start of the file in one request (served from the proxy's head buffer).
const HEAD: u64 = 256 * 1024;
/// The largest `moov` read (a three-hour film's is a few MiB).
const MAX_MOOV: u64 = 64 << 20;
/// The largest Matroska element read (`Cues` of a long film with a cue per keyframe: < 1 MiB).
const MAX_ELEMENT: u64 = 32 << 20;
/// Top-level boxes walked before giving up on finding the `moov`.
const MAX_BOXES: usize = 64;
/// Samples accepted in a video track (a 24-hour recording at 60 fps is 5.2 M).
const MAX_SAMPLES: u64 = 8_000_000;
/// Each read through the proxy gets this long (the provider's own timeout is 15 s).
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// A seek lands at most this far after its keyframe (Jellyfin's 0.5 s).
const SEEK_LEAD_S: f64 = 0.5;
/// A seek window narrower than this is none (timestamps are rounded to microseconds on the way).
const MIN_WINDOW_S: f64 = 0.002;

/// Which index the times came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Matroska,
}

impl Container {
    pub fn name(self) -> &'static str {
        match self {
            Container::Mp4 => "the MP4 index",
            Container::Matroska => "the Matroska cues",
        }
    }
}

/// One keyframe of the video track (seconds on the container's clock).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    /// Its presentation time.
    pub pts: f64,
    /// A `-ss` time that lands on it (module docs); `None`: none does reliably.
    pub seek: Option<f64>,
}

/// The video track's keyframes, in presentation order.
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    pub container: Container,
    pub keyframes: Vec<Keyframe>,
    /// Where the video track ends (the last picture's end; Matroska: the segment's duration).
    pub end: Option<f64>,
}

/// Why there is no index (logged when the item falls back to its EVENT playlist).
pub type Why = &'static str;

/// Byte ranges of the item: through the loopback proxy (a `Range` request each), or in memory.
pub struct Reader {
    inner: Inner,
    size: u64,
}

enum Inner {
    Http { client: reqwest::Client, url: String },
    Mem(Vec<u8>),
}

impl Reader {
    /// The proxy's loopback URL of a resource of `size` bytes the provider serves by `Range`.
    pub fn http(url: &str, size: u64) -> Result<Self, Why> {
        let client = reqwest::Client::builder().no_proxy().build().map_err(|_| "no HTTP client")?;
        Ok(Self { inner: Inner::Http { client, url: url.to_owned() }, size })
    }

    /// A whole file in memory (tests, and the parsers' own checks).
    pub fn bytes(data: Vec<u8>) -> Self {
        Self { size: data.len() as u64, inner: Inner::Mem(data) }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// `len` bytes from `at` (fewer at the end of the file).
    pub async fn read(&mut self, at: u64, len: u64) -> Result<Vec<u8>, Why> {
        if at >= self.size || len == 0 {
            return Ok(Vec::new());
        }
        let end = at.saturating_add(len).min(self.size);
        match &self.inner {
            Inner::Mem(data) => Ok(data[at as usize..end as usize].to_vec()),
            Inner::Http { client, url } => {
                let req = client.get(url).header(header::RANGE, format!("bytes={at}-{}", end - 1));
                let body = async {
                    let resp = req.send().await.ok()?;
                    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                        return None;
                    }
                    resp.bytes().await.ok()
                };
                match tokio::time::timeout(READ_TIMEOUT, body).await {
                    Ok(Some(b)) if b.len() as u64 == end - at => Ok(b.to_vec()),
                    _ => Err("a range of it can't be read"),
                }
            }
        }
    }
}

/// Read the index of `container` ([`sniff`] the first bytes when it isn't known).
pub async fn read(r: &mut Reader) -> Result<Index, Why> {
    let head = r.read(0, HEAD).await?;
    match sniff(&head) {
        Some(Container::Mp4) => mp4(r, head).await,
        Some(Container::Matroska) => matroska(r, head).await,
        None => Err("neither MP4 nor Matroska"),
    }
}

/// What the first bytes say: an `ftyp` box (or a `moov`/`free` one first), or the EBML magic.
pub fn sniff(head: &[u8]) -> Option<Container> {
    if head.len() >= 4 && head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return Some(Container::Matroska);
    }
    let kind = head.get(4..8)?;
    matches!(kind, b"ftyp" | b"moov" | b"free" | b"skip" | b"wide" | b"mdat").then_some(Container::Mp4)
}

// ---- MP4 -----------------------------------------------------------------------------------

/// A box header at `at`: `(type, header length, total size)`; size 0 runs to `file_size`.
fn box_header(h: &[u8], at: u64, file_size: u64) -> Option<([u8; 4], u64, u64)> {
    let size = u32::from_be_bytes(h.get(..4)?.try_into().ok()?);
    let kind: [u8; 4] = h.get(4..8)?.try_into().ok()?;
    let (hdr, size) = match size {
        1 => (16, u64::from_be_bytes(h.get(8..16)?.try_into().ok()?)),
        0 => (8, file_size.checked_sub(at)?),
        n => (8, u64::from(n)),
    };
    (size >= hdr).then_some((kind, hdr, size))
}

async fn mp4(r: &mut Reader, head: Vec<u8>) -> Result<Index, Why> {
    let size = r.size();
    let mut at = 0u64;
    for _ in 0..MAX_BOXES {
        if at + 8 > size {
            break;
        }
        let h = if at + 16 <= head.len() as u64 { head[at as usize..at as usize + 16].to_vec() } else { r.read(at, 16).await? };
        let Some((kind, hdr, len)) = box_header(&h, at, size) else { return Err("a damaged MP4 box") };
        match &kind {
            b"moov" => {
                if len > MAX_MOOV {
                    return Err("its moov is too big");
                }
                let body = if at + len <= head.len() as u64 {
                    head[(at + hdr) as usize..(at + len) as usize].to_vec()
                } else {
                    let all = r.read(at, len).await?;
                    if (all.len() as u64) < len {
                        return Err("its moov is cut short");
                    }
                    all[hdr as usize..].to_vec()
                };
                return parse_moov(&body);
            }
            b"moof" => return Err("a fragmented MP4"),
            _ => {}
        }
        at = at.checked_add(len).ok_or("a damaged MP4 box")?;
    }
    Err("no moov")
}

/// The boxes of `data`: `(type, body)`; stops at the first damaged one.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let (hdr, size) = match size {
            1 => (16, usize::try_from(u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?)).ok()?),
            0 => (8, rest.len()),
            n => (8, n),
        };
        let body = rest.get(hdr..size)?;
        rest = &rest[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `mvhd` / `mdhd` timescale (version 0: after two 32-bit times; version 1: two 64-bit ones).
fn timescale(full_box: &[u8]) -> Option<u32> {
    let at = if *full_box.first()? == 1 { 20 } else { 12 };
    be32(full_box, at).filter(|t| *t > 0)
}

/// `(count, value)` entries of a table box (`stts`, `ctts`): version/flags, entry count, pairs.
fn pairs(b: &[u8]) -> Option<Vec<(u32, u32)>> {
    let n = be32(b, 4)? as usize;
    let table = b.get(8..8 + n.checked_mul(8)?)?;
    Some(
        table
            .as_chunks::<8>()
            .0
            .iter()
            .map(|e| (u32::from_be_bytes([e[0], e[1], e[2], e[3]]), u32::from_be_bytes([e[4], e[5], e[6], e[7]])))
            .collect(),
    )
}

/// One `elst` entry: `(segment_duration in the movie timescale, media_time; -1: empty)`.
fn edits(elst: &[u8]) -> Option<Vec<(u64, i64)>> {
    let v1 = *elst.first()? == 1;
    let n = be32(elst, 4)? as usize;
    let size = if v1 { 20 } else { 12 };
    let table = elst.get(8..8 + n.checked_mul(size)?)?;
    Some(
        table
            .chunks_exact(size)
            .map(|e| {
                if v1 {
                    (u64::from_be_bytes(e[..8].try_into().unwrap_or_default()), i64::from_be_bytes(e[8..16].try_into().unwrap_or_default()))
                } else {
                    (
                        u64::from(u32::from_be_bytes(e[..4].try_into().unwrap_or_default())),
                        i64::from(i32::from_be_bytes(e[4..8].try_into().unwrap_or_default())),
                    )
                }
            })
            .collect(),
    )
}

/// The video track of a `moov` body (module docs).
pub fn parse_moov(moov: &[u8]) -> Result<Index, Why> {
    if child(moov, b"mvex").is_some() {
        return Err("a fragmented MP4");
    }
    let movie_scale = child(moov, b"mvhd").and_then(timescale).ok_or("no mvhd")?;
    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else { continue };
        if child(mdia, b"hdlr").and_then(|h| h.get(8..12)) != Some(b"vide") {
            continue;
        }
        let stbl = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).ok_or("no sample table")?;
        let stts = child(stbl, b"stts").and_then(pairs).ok_or("no stts")?;
        let samples: u64 = stts.iter().map(|(n, _)| u64::from(*n)).sum();
        if samples < 2 {
            continue; // a still (cover art, a thumbnail track): not the picture ffmpeg maps
        }
        let scale = child(mdia, b"mdhd").and_then(timescale).ok_or("no mdhd")?;
        let ctts = match child(stbl, b"ctts") {
            Some(b) => Some(pairs(b).ok_or("a damaged ctts")?),
            None => None,
        };
        let sync = match child(stbl, b"stss") {
            Some(b) => {
                let n = be32(b, 4).ok_or("a damaged stss")? as usize;
                let list = b.get(8..8 + n.checked_mul(4).ok_or("a damaged stss")?).ok_or("a damaged stss")?;
                Some(list.as_chunks::<4>().0.iter().map(|e| u32::from_be_bytes(*e)).collect::<Vec<_>>())
            }
            None => None,
        };
        let edit = match child(trak, b"edts").and_then(|e| child(e, b"elst")) {
            Some(b) => Some(edits(b).ok_or("a damaged elst")?),
            None => None,
        };
        return track(Track { scale, movie_scale, stts, ctts, sync, edit });
    }
    Err("no video track")
}

/// A video track's tables (see [`parse_moov`]).
struct Track {
    scale: u32,
    movie_scale: u32,
    stts: Vec<(u32, u32)>,
    ctts: Option<Vec<(u32, u32)>>,
    /// 1-based sync sample numbers; `None`: every sample is one.
    sync: Option<Vec<u32>>,
    edit: Option<Vec<(u64, i64)>>,
}

fn track(t: Track) -> Result<Index, Why> {
    let samples: u64 = t.stts.iter().map(|(n, _)| u64::from(*n)).sum();
    if samples > MAX_SAMPLES {
        return Err("too many samples");
    }
    // decode times, presentation times (raw: dts + ctts) and durations, per sample
    let n = samples as usize;
    let mut dts = Vec::with_capacity(n);
    let mut dur = Vec::with_capacity(n);
    let mut clock = 0i64;
    for (count, delta) in &t.stts {
        for _ in 0..*count {
            dts.push(clock);
            dur.push(i64::from(*delta));
            clock += i64::from(*delta);
        }
    }
    let mut pts = dts.clone();
    if let Some(ctts) = &t.ctts {
        let mut i = 0usize;
        // ffmpeg reads the offsets signed whatever the box version says
        for (count, off) in ctts {
            for _ in 0..*count {
                if let Some(p) = pts.get_mut(i) {
                    *p += i64::from(*off as i32);
                }
                i += 1;
            }
        }
    }
    // the edit list: leading empty edits, then one edit at rate 1 (anything else: no index)
    let (empty, media_time) = match &t.edit {
        None => (0u64, None),
        Some(list) => {
            let mut empty = 0u64;
            let mut media = None;
            for (duration, time) in list {
                match (*time, media) {
                    (-1, None) => empty += duration,
                    (m, None) if m >= 0 => media = Some(m),
                    _ => return Err("an edit list ffmpeg plays as several edits"),
                }
            }
            (empty, media)
        }
    };
    let empty_ts = (empty as f64 / f64::from(t.movie_scale) * f64::from(t.scale)).round() as i64;
    // the presented samples, and the shift that puts the first of them at 0 (mov_fix_index)
    let presented = |i: usize| media_time.is_none_or(|m| pts[i] >= m);
    let base = media_time.unwrap_or(0);
    let first = (0..n).filter(|i| presented(*i)).map(|i| pts[i] - base).min().ok_or("no presented picture")?;
    // never shifted later: a first picture before 0 (negative composition offsets) stays there
    let base = base + first.max(0);
    let secs = |v: i64| (v - base + empty_ts) as f64 / f64::from(t.scale);
    let end = (0..n).filter(|i| presented(*i)).map(|i| pts[i] + dur[i]).max().map(secs);
    let sync: Vec<usize> = match &t.sync {
        Some(list) => list.iter().filter_map(|s| (*s as usize).checked_sub(1)).filter(|i| *i < n).collect(),
        None => (0..n).collect(),
    };
    let mut keyframes = Vec::with_capacity(sync.len());
    for (j, &i) in sync.iter().enumerate() {
        if !presented(i) {
            continue;
        }
        let at = secs(pts[i]);
        // up to the next keyframe's decode time (where the demuxer would land instead)
        let next = sync.get(j + 1).map(|&k| secs(dts[k])).or(end);
        let seek = next.and_then(|next| window(at, next));
        keyframes.push(Keyframe { pts: at, seek });
    }
    keyframes.sort_by(|a, b| a.pts.total_cmp(&b.pts));
    keyframes.dedup_by(|a, b| (a.pts - b.pts).abs() < 1e-9);
    if keyframes.is_empty() {
        return Err("no keyframe");
    }
    Ok(Index { container: Container::Mp4, keyframes, end })
}

/// A `-ss` time inside `[at, next)` (module docs, "Seek windows").
fn window(at: f64, next: f64) -> Option<f64> {
    let gap = next - at;
    (gap > MIN_WINDOW_S && gap.is_finite()).then(|| at + SEEK_LEAD_S.min(gap / 2.0))
}

// ---- Matroska ------------------------------------------------------------------------------

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CLUSTER: u32 = 0x1F43_B675;

/// An element header at the start of `b`: `(id, header length, data size; None: unknown)`.
fn element(b: &[u8]) -> Option<(u32, usize, Option<u64>)> {
    let first = *b.first()?;
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return None;
    }
    let id = b.get(..id_len)?.iter().fold(0u32, |acc, x| (acc << 8) | u32::from(*x));
    let s = *b.get(id_len)?;
    let size_len = s.leading_zeros() as usize + 1;
    if size_len > 8 {
        return None;
    }
    let raw = b.get(id_len..id_len + size_len)?;
    let mut value = u64::from(s) & (0xff >> size_len);
    for x in &raw[1..] {
        value = (value << 8) | u64::from(*x);
    }
    let unknown = value == (1u64 << (7 * size_len)) - 1;
    Some((id, id_len + size_len, (!unknown).then_some(value)))
}

/// The children of an element body: `(id, data)`; an unknown size runs to the end.
fn elements(body: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut rest = body;
    std::iter::from_fn(move || {
        let (id, hdr, size) = element(rest)?;
        let end = match size {
            Some(s) => hdr.checked_add(usize::try_from(s).ok()?)?,
            None => rest.len(),
        };
        let data = rest.get(hdr..end)?;
        rest = &rest[end..];
        Some((id, data))
    })
}

fn uint(data: &[u8]) -> Option<u64> {
    (data.len() <= 8).then(|| data.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x)))
}

fn float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f64::from(f32::from_be_bytes(data.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

/// What the Matroska index reading has found so far.
#[derive(Default)]
struct Mkv {
    info: Option<Vec<u8>>,
    tracks: Option<Vec<u8>>,
    cues: Option<Vec<u8>>,
    /// `SeekHead` positions (absolute) of the elements not read yet.
    seeks: Vec<(u32, u64)>,
}

impl Mkv {
    fn keep(&mut self, id: u32, data: &[u8]) {
        match id {
            INFO => self.info = Some(data.to_vec()),
            TRACKS => self.tracks = Some(data.to_vec()),
            CUES => self.cues = Some(data.to_vec()),
            _ => {}
        }
    }

    fn seek_head(&mut self, data: &[u8], segment: u64) {
        for (id, seek) in elements(data) {
            if id != SEEK {
                continue;
            }
            let (mut target, mut pos) = (None, None);
            for (id, v) in elements(seek) {
                match id {
                    SEEK_ID => target = uint(v).and_then(|v| u32::try_from(v).ok()),
                    SEEK_POSITION => pos = uint(v),
                    _ => {}
                }
            }
            if let (Some(t), Some(p)) = (target, pos) {
                self.seeks.push((t, segment.saturating_add(p)));
            }
        }
    }
}

async fn matroska(r: &mut Reader, head: Vec<u8>) -> Result<Index, Why> {
    let (id, hdr, size) = element(&head).ok_or("a damaged EBML header")?;
    if id != EBML {
        return Err("no EBML header");
    }
    let at = hdr as u64 + size.ok_or("a damaged EBML header")?;
    let rest = head.get(at as usize..).ok_or("a damaged EBML header")?;
    let (id, hdr, _) = element(rest).ok_or("no Segment")?;
    if id != SEGMENT {
        return Err("no Segment");
    }
    let segment = at + hdr as u64;
    let mut mkv = Mkv::default();
    // the top-level elements in the head (up to the first cluster)
    let mut pos = segment;
    while let Some(b) = head.get(pos as usize..) {
        let Some((id, hdr, size)) = element(b) else { break };
        if id == CLUSTER {
            break;
        }
        let Some(size) = size else { return Err("an element of unknown size") };
        let end = pos + hdr as u64 + size;
        if end <= head.len() as u64 {
            let data = &head[(pos + hdr as u64) as usize..end as usize];
            match id {
                SEEK_HEAD => mkv.seek_head(data, segment),
                _ => mkv.keep(id, data),
            }
        } else if matches!(id, SEEK_HEAD | INFO | TRACKS | CUES) {
            mkv.seeks.push((id, pos));
        }
        pos = end;
    }
    // what the SeekHead points to (one more SeekHead at most, as mkvmerge writes)
    let mut hops = 0;
    while let Some((want, at)) = mkv.seeks.pop() {
        let have = match want {
            INFO => mkv.info.is_some(),
            TRACKS => mkv.tracks.is_some(),
            CUES => mkv.cues.is_some(),
            SEEK_HEAD => hops > 0,
            _ => true,
        };
        if have {
            continue;
        }
        let h = r.read(at, 16).await?;
        let (id, hdr, size) = element(&h).ok_or("a damaged element")?;
        if id != want {
            continue; // a stale SeekHead entry
        }
        let size = size.ok_or("an element of unknown size")?;
        if size > MAX_ELEMENT {
            return Err("its cues are too big");
        }
        let all = r.read(at, hdr as u64 + size).await?;
        let data = all.get(hdr..).filter(|d| d.len() as u64 == size).ok_or("an element is cut short")?;
        if want == SEEK_HEAD {
            hops += 1;
            mkv.seek_head(data, segment);
        } else {
            mkv.keep(want, data);
        }
    }
    parse_matroska(mkv.info.as_deref(), mkv.tracks.as_deref().ok_or("no Tracks")?, mkv.cues.as_deref().ok_or("no cues")?)
}

/// The video track's cues (module docs) from the bodies of `Info`, `Tracks` and `Cues`.
fn parse_matroska(info: Option<&[u8]>, tracks: &[u8], cues: &[u8]) -> Result<Index, Why> {
    let mut scale = 1_000_000u64;
    let mut duration = None;
    for (id, v) in elements(info.unwrap_or_default()) {
        match id {
            TIMESTAMP_SCALE => scale = uint(v).filter(|s| *s > 0).unwrap_or(scale),
            DURATION => duration = float(v).filter(|d| d.is_finite() && *d > 0.0),
            _ => {}
        }
    }
    let video = elements(tracks)
        .filter(|(id, _)| *id == TRACK_ENTRY)
        .find_map(|(_, entry)| {
            let get = |want: u32| elements(entry).find(|(id, _)| *id == want).and_then(|(_, v)| uint(v));
            (get(TRACK_TYPE) == Some(1)).then(|| get(TRACK_NUMBER)).flatten()
        })
        .ok_or("no video track")?;
    let secs = |ticks: u64| ticks as f64 * scale as f64 / 1e9;
    let mut times: Vec<u64> = elements(cues)
        .filter(|(id, _)| *id == CUE_POINT)
        .filter_map(|(_, point)| {
            let time = elements(point).find(|(id, _)| *id == CUE_TIME).and_then(|(_, v)| uint(v))?;
            elements(point)
                .filter(|(id, _)| *id == CUE_TRACK_POSITIONS)
                .any(|(_, p)| elements(p).any(|(id, v)| id == CUE_TRACK && uint(v) == Some(video)))
                .then_some(time)
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    if times.is_empty() {
        return Err("no cues for the picture");
    }
    let end = duration.map(|d| d * scale as f64 / 1e9);
    let keyframes = times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let at = secs(*t);
            let next = times.get(i + 1).map(|n| secs(*n)).or(end);
            Keyframe { pts: at, seek: next.and_then(|n| window(at, n)) }
        })
        .collect();
    Ok(Index { container: Container::Matroska, keyframes, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- an MP4 writer for the tests

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(version: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![version, 0, 0, 0];
        v.extend_from_slice(rest);
        v
    }

    fn table(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut v = (entries.len() as u32).to_be_bytes().to_vec();
        for (a, b) in entries {
            v.extend_from_slice(&a.to_be_bytes());
            v.extend_from_slice(&b.to_be_bytes());
        }
        v
    }

    fn mdhd(scale: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(&scale.to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        bx(b"mdhd", &full(0, &b))
    }

    struct Trak<'a> {
        handler: &'a [u8; 4],
        scale: u32,
        stts: &'a [(u32, u32)],
        ctts: Option<&'a [(u32, i32)]>,
        stss: Option<&'a [u32]>,
        elst: Option<&'a [(u32, i32)]>,
    }

    fn trak(t: &Trak<'_>) -> Vec<u8> {
        let mut stbl = bx(b"stts", &full(0, &table(t.stts)));
        if let Some(c) = t.ctts {
            let e: Vec<(u32, u32)> = c.iter().map(|(n, o)| (*n, *o as u32)).collect();
            stbl.extend(bx(b"ctts", &full(1, &table(&e))));
        }
        if let Some(s) = t.stss {
            let mut b = (s.len() as u32).to_be_bytes().to_vec();
            for n in s {
                b.extend_from_slice(&n.to_be_bytes());
            }
            stbl.extend(bx(b"stss", &full(0, &b)));
        }
        let minf = bx(b"minf", &bx(b"stbl", &stbl));
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(t.handler);
        hdlr.extend_from_slice(&[0; 12]);
        let mut mdia = mdhd(t.scale);
        mdia.extend(bx(b"hdlr", &full(0, &hdlr)));
        mdia.extend(minf);
        let mut body = Vec::new();
        if let Some(e) = t.elst {
            let mut b = (e.len() as u32).to_be_bytes().to_vec();
            for (d, m) in e {
                b.extend_from_slice(&d.to_be_bytes());
                b.extend_from_slice(&m.to_be_bytes());
                b.extend_from_slice(&[0, 1, 0, 0]);
            }
            body.extend(bx(b"edts", &bx(b"elst", &full(0, &b))));
        }
        body.extend(bx(b"mdia", &mdia));
        bx(b"trak", &body)
    }

    fn moov(traks: &[Trak<'_>]) -> Vec<u8> {
        let mut mvhd = vec![0u8; 8];
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0; 84]);
        let mut body = bx(b"mvhd", &full(0, &mvhd));
        for t in traks {
            body.extend(trak(t));
        }
        bx(b"moov", &body)
    }

    /// The 12 s film of the design's experiment at 25 fps (timescale 12800, 512 per frame), B
    /// frames two deep (ffmpeg's edit list of 1024), keyframes at 0, 1.32, 2.68, 4.12 and 5.0 s.
    fn film() -> Vec<u8> {
        let frames = 300u32;
        let keys = [0u32, 33, 67, 103, 125];
        // I P B B …: a keyframe and every P show two frames late, each B one frame early
        let ctts: Vec<(u32, i32)> = (0..frames).map(|i| (1, if keys.contains(&i) || i % 3 == 1 { 1024 } else { 512 })).collect();
        let stss: Vec<u32> = keys.iter().map(|k| k + 1).collect();
        let video =
            Trak { handler: b"vide", scale: 12800, stts: &[(frames, 512)], ctts: Some(&ctts), stss: Some(&stss), elst: Some(&[(12000, 1024)]) };
        let sound = Trak { handler: b"soun", scale: 48000, stts: &[(562, 1024)], ctts: None, stss: None, elst: Some(&[(12000, 1024)]) };
        moov(&[sound, video])
    }

    fn run<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    /// The keyframes of the sound-first film: presentation times as ffmpeg gives them (the edit
    /// is time 0), and every seek between the keyframe and the next one's decode time.
    #[test]
    fn mp4_keyframes_come_from_stss_stts_ctts_and_the_edit() {
        let i = parse_moov(&film()[8..]).unwrap();
        let pts: Vec<f64> = i.keyframes.iter().map(|k| (k.pts * 1000.0).round() / 1000.0).collect();
        assert_eq!(pts, [0.0, 1.32, 2.68, 4.12, 5.0]);
        for (k, next) in i.keyframes.iter().zip(i.keyframes.iter().skip(1)) {
            let s = k.seek.unwrap();
            assert!(s >= k.pts && s < next.pts - 0.08 && s <= k.pts + 0.5, "{k:?} → {s} (next decodes at {})", next.pts - 0.08);
        }
        // the last picture shown ends the track: frame 298 (a P), shown at 11.92 s for 40 ms
        assert!((i.end.unwrap() - 11.96).abs() < 1e-9, "{:?}", i.end);
        // an entry count past the picture's edit table (the sound's comes first): damaged, no index
        let mut damaged = film();
        let at = damaged.windows(4).rposition(|w| w == b"elst").unwrap() + 4;
        damaged[at + 4..at + 8].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(parse_moov(&damaged[8..]), Err("a damaged elst"));
    }

    #[test]
    fn mp4_edge_cases() {
        // no stss: every sample is a sync sample; no edit list and no ctts: times from 0
        let intra = moov(&[Trak { handler: b"vide", scale: 25, stts: &[(4, 1)], ctts: None, stss: None, elst: None }]);
        let i = parse_moov(&intra[8..]).unwrap();
        assert_eq!(i.keyframes.iter().map(|k| k.pts).collect::<Vec<_>>(), [0.0, 0.04, 0.08, 0.12]);
        assert!(i.keyframes.iter().all(|k| k.seek.is_some()));
        // no edit list but B-frames: ffmpeg shifts the first picture to 0
        let b = moov(&[Trak { handler: b"vide", scale: 100, stts: &[(4, 10)], ctts: Some(&[(4, 20)]), stss: Some(&[1, 3]), elst: None }]);
        let i = parse_moov(&b[8..]).unwrap();
        assert_eq!(i.keyframes.iter().map(|k| k.pts).collect::<Vec<_>>(), [0.0, 0.2]);
        // keyframes one frame apart with two frames of delay: the first has no seek window
        assert_eq!(i.keyframes[0].seek, None, "the next keyframe decodes before this one shows");
        // an empty edit then the media: delayed by it
        let delayed = moov(&[Trak { handler: b"vide", scale: 100, stts: &[(4, 10)], ctts: None, stss: None, elst: Some(&[(2000, -1), (40, 0)]) }]);
        let i = parse_moov(&delayed[8..]).unwrap();
        assert!((i.keyframes[0].pts - 2.0).abs() < 1e-9, "{i:?}");
        // two media edits, a fragmented file, no video, a still: no index
        let two = moov(&[Trak { handler: b"vide", scale: 100, stts: &[(4, 10)], ctts: None, stss: None, elst: Some(&[(20, 0), (20, 20)]) }]);
        assert!(parse_moov(&two[8..]).is_err());
        let mut frag = moov(&[Trak { handler: b"vide", scale: 100, stts: &[(4, 10)], ctts: None, stss: None, elst: None }]);
        let mvex = bx(b"mvex", &[]);
        frag.extend_from_slice(&mvex);
        let len = frag.len() as u32;
        frag[..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_moov(&frag[8..]), Err("a fragmented MP4"));
        let sound = moov(&[Trak { handler: b"soun", scale: 100, stts: &[(4, 10)], ctts: None, stss: None, elst: None }]);
        assert_eq!(parse_moov(&sound[8..]), Err("no video track"));
        let still = moov(&[Trak { handler: b"vide", scale: 100, stts: &[(1, 10)], ctts: None, stss: None, elst: None }]);
        assert_eq!(parse_moov(&still[8..]), Err("no video track"));
        // damaged tables
        let mut cut = film();
        let at = cut.windows(4).position(|w| w == b"stss").unwrap() + 8;
        cut[at..at + 4].copy_from_slice(&1000u32.to_be_bytes());
        assert!(parse_moov(&cut[8..]).is_err());
    }

    /// The moov after a big mdat (ffmpeg's default) is found by the top-level sizes, with one read
    /// past the head; a file without one, a moof first or a damaged size is no index.
    #[test]
    fn the_moov_is_found_at_either_end() {
        let mut file = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
        let mdat = vec![0u8; 400 * 1024];
        file.extend(bx(b"mdat", &mdat));
        file.extend(film());
        let i = run(read(&mut Reader::bytes(file.clone()))).unwrap();
        assert_eq!((i.container, i.keyframes.len()), (Container::Mp4, 5));
        let mut front = bx(b"ftyp", b"isom\0\0\x02\0isom");
        front.extend(film());
        front.extend(bx(b"mdat", &[0; 64]));
        assert_eq!(run(read(&mut Reader::bytes(front))).unwrap().keyframes.len(), 5);
        let mut none = bx(b"ftyp", b"isom\0\0\x02\0isom");
        none.extend(bx(b"mdat", &[0; 64]));
        assert_eq!(run(read(&mut Reader::bytes(none))), Err("no moov"));
        let mut frag = bx(b"ftyp", b"isom\0\0\x02\0isom");
        frag.extend(bx(b"moof", &[0; 8]));
        assert_eq!(run(read(&mut Reader::bytes(frag))), Err("a fragmented MP4"));
        let mut damaged = file;
        let at = file_len_at(&damaged);
        damaged[at..at + 4].copy_from_slice(&3u32.to_be_bytes());
        assert!(run(read(&mut Reader::bytes(damaged))).is_err());
        assert_eq!(run(read(&mut Reader::bytes(b"G@\0\x10 not a film".to_vec()))), Err("neither MP4 nor Matroska"));
    }

    fn file_len_at(file: &[u8]) -> usize {
        file.windows(4).position(|w| w == b"mdat").unwrap() - 4
    }

    // ---- a Matroska writer for the tests

    fn ebml(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id.to_be_bytes().iter().copied().skip_while(|b| *b == 0).collect();
        // an 8-byte size, as ffmpeg writes for some elements
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    fn ebml_uint(id: u32, n: u64) -> Vec<u8> {
        ebml(id, &n.to_be_bytes())
    }

    fn cue(time: u64, track: u64) -> Vec<u8> {
        let mut p = ebml_uint(CUE_TIME, time);
        p.extend(ebml(CUE_TRACK_POSITIONS, &ebml_uint(CUE_TRACK, track)));
        ebml(CUE_POINT, &p)
    }

    /// A Matroska file as ffmpeg writes it: a SeekHead naming Info, Tracks and the Cues at the
    /// end, sound as track 1 and the picture as track 2, cues for both.
    fn mkv(cues: Option<&[(u64, u64)]>, scale: u64) -> Vec<u8> {
        let mut info = ebml_uint(TIMESTAMP_SCALE, scale);
        info.extend(ebml(DURATION, &12000.0f64.to_be_bytes()));
        let info = ebml(INFO, &info);
        let entry = |n: u64, kind: u64| {
            let mut e = ebml_uint(TRACK_NUMBER, n);
            e.extend(ebml_uint(TRACK_TYPE, kind));
            ebml(TRACK_ENTRY, &e)
        };
        let mut tracks = entry(1, 2);
        tracks.extend(entry(2, 1));
        let tracks = ebml(TRACKS, &tracks);
        let cluster = ebml(CLUSTER, &vec![0u8; 300 * 1024]);
        let cues = cues.map(|list| ebml(CUES, &list.iter().flat_map(|(t, k)| cue(*t, *k)).collect::<Vec<_>>()));
        // positions relative to the segment's data: the SeekHead is 3 × (Seek) long
        let seek = |id: u32, pos: u64| {
            let mut s = ebml(SEEK_ID, &id.to_be_bytes());
            s.extend(ebml_uint(SEEK_POSITION, pos));
            ebml(SEEK, &s)
        };
        let head_len = ebml(SEEK_HEAD, &[seek(INFO, 0), seek(TRACKS, 0), seek(CUES, 0)].concat()).len() as u64;
        let info_at = head_len;
        let tracks_at = info_at + info.len() as u64;
        let cues_at = tracks_at + tracks.len() as u64 + cluster.len() as u64;
        let mut entries = vec![seek(INFO, info_at), seek(TRACKS, tracks_at)];
        if cues.is_some() {
            entries.push(seek(CUES, cues_at));
        } else {
            entries.push(seek(0xEC, 0)); // a Void, to keep the length
        }
        let mut segment = ebml(SEEK_HEAD, &entries.concat());
        assert_eq!(segment.len() as u64, head_len);
        segment.extend(info);
        segment.extend(tracks);
        segment.extend(cluster);
        segment.extend(cues.unwrap_or_default());
        let mut file = ebml(EBML, &ebml_uint(0x4282, 0x6d6174726f736b61));
        file.extend(ebml(SEGMENT, &segment));
        file
    }

    #[test]
    fn matroska_keyframes_are_the_picture_tracks_cues() {
        let cues = [(0, 1), (0, 2), (1320, 2), (1000, 1), (2680, 2), (4120, 2), (5000, 2), (6000, 1)];
        let i = run(read(&mut Reader::bytes(mkv(Some(&cues), 1_000_000)))).unwrap();
        assert_eq!(i.container, Container::Matroska);
        assert_eq!(i.keyframes.iter().map(|k| k.pts).collect::<Vec<_>>(), [0.0, 1.32, 2.68, 4.12, 5.0], "the sound's cues aren't keyframes");
        assert_eq!(i.keyframes[0].seek, Some(0.5));
        assert!((i.keyframes[3].seek.unwrap() - 4.56).abs() < 1e-9, "half the gap to 5.0: {:?}", i.keyframes[3]);
        assert_eq!(i.end, Some(12.0));
        // a TimestampScale of 100 µs
        let i = run(read(&mut Reader::bytes(mkv(Some(&[(0, 2), (13200, 2)]), 100_000)))).unwrap();
        assert_eq!(i.keyframes.iter().map(|k| k.pts).collect::<Vec<_>>(), [0.0, 1.32]);
        assert_eq!(i.end, Some(1.2), "the duration is in ticks too");
        // no cues (a file written to a pipe), or only the sound's
        assert_eq!(run(read(&mut Reader::bytes(mkv(None, 1_000_000)))), Err("no cues"));
        assert_eq!(run(read(&mut Reader::bytes(mkv(Some(&[(0, 1)]), 1_000_000)))), Err("no cues for the picture"));
        // cut short where the cues should be
        let mut cut = mkv(Some(&cues), 1_000_000);
        cut.truncate(cut.len() - 20);
        assert!(run(read(&mut Reader::bytes(cut))).is_err());
    }

    #[test]
    fn ebml_vints() {
        assert_eq!(element(&[0x1a, 0x45, 0xdf, 0xa3, 0x84]), Some((EBML, 5, Some(4))));
        assert_eq!(element(&[0xae, 0xff]), Some((0xae, 2, None)), "all ones: unknown size");
        assert_eq!(element(&[0xae, 0x40, 0x02]), Some((0xae, 3, Some(2))));
        assert_eq!(element(&[0x00, 0x81]), None);
        assert_eq!(element(&[0xae]), None);
    }
}
