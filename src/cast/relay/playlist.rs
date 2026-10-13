//! The served-playlist guard and playlist views (DESIGN §40 §8.1.4–5): a stable
//! `#EXT-X-TARGETDURATION`, the IDR check on each new segment (two violations → "irregular"),
//! relative names only, `EXT-X-START` at 0 for EVENT playlists; views that start at segment k
//! (`MEDIA-SEQUENCE:k`, PDT kept, growing only at the end) for seeks inside what's produced.
//! OWNER: CAST-4.
//!
//! The TV never reads ffmpeg's `index.m3u8` as written. Every fetch goes through
//! [`Guard::observe`] (new segments: duration and IDR checks, the timeline) and
//! [`Guard::render`]:
//! * **Target duration.** In copy mode ffmpeg derives `#EXT-X-TARGETDURATION` from the current
//!   window and may change it mid-session, which RFC 8216 §6.2.1 forbids. The served value is
//!   pinned when the first segments are ready ([`pin`]): copied video cuts at the source's
//!   keyframes, so it covers the longest of them plus [`COPY_HEADROOM_S`]; the relay keeps it
//!   for the item across reloads. It never changes while a playlist is served: a segment that
//!   outgrows it (its rounded `EXTINF` above it) is held back with everything after it
//!   ([`Guard::overflow`]: the session restarts the item where the TV is, under a longer pinned
//!   target). That is no violation: a long-GOP film is no irregular source (the 2026-10-13 film
//!   ran 10.09 s and 12.10 s segments, and two raises had sent it to a 700 % CPU conversion). A
//!   segment beyond [`MAX_TARGET_S`] is: keyframes that sparse need converting.
//! * **IDR.** Apple rule 7.4 wants every segment to start with an IDR (H.264 NAL 5, HEVC IRAP
//!   16–21); copy mode may cut on a recovery-point I-frame. A segment whose first picture isn't
//!   one is a violation. Two violations make the item "irregular" (L3 next time, where forced
//!   keyframes fix it).
//! * **Start.** An EVENT playlist (VOD, catch-up) says `#EXT-X-START:TIME-OFFSET=0`: without it
//!   AVPlayer treats the growing playlist as live and starts three target durations from its end
//!   (RFC 8216 §6.3.3; traced 2026-10-13: 12.18 s in, then 9.22 s after a reload), while the
//!   relay started it exactly where playback should be.
//! * **Names.** Only ffmpeg's relative segment names that the LAN server serves
//!   ([`super::server::served_file`]) pass; anything else (an absolute URL, a path) is dropped.
//!
//! A **view** (§8.1.5) starts at segment *k* of what ffmpeg produced: `MEDIA-SEQUENCE:k`, the
//! segments from *k* with their tags (PDT included), the same type, and the
//! `DISCONTINUITY-SEQUENCE` advanced past dropped discontinuities. ffmpeg's EVENT playlist only
//! grows at the end, so a view does too.

use std::collections::BTreeMap;

use super::server::{File, served_file};

/// A segment may exceed the pinned target by this much before it counts (rounding, §6.2.1).
const TARGET_SLACK_S: f64 = 0.5;
/// What copied video's target covers beyond the longest segment of the first window: hlsenc
/// cuts at the first keyframe past each `hls_time` step, so segments run up to `hls_time` plus a
/// GOP, and a source's GOPs vary.
pub const COPY_HEADROOM_S: f64 = 2.0;
/// The longest target declared (a TV reloads a live playlist about once per target): a segment
/// beyond it is a source to convert, not to wait for.
pub const MAX_TARGET_S: u32 = 30;
/// Violations that make an item irregular.
const IRREGULAR_AFTER: u32 = 2;
/// Segments remembered in the timeline (a live window slides for hours; a seek needs only
/// EVENT playlists, which are bounded by the film).
const TIMELINE_CAP: usize = 100_000;

/// One media segment as ffmpeg listed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub seq: u64,
    /// `#EXTINF` seconds.
    pub dur: f64,
    /// The tag lines before the URI, as written (`#EXTINF`, `#EXT-X-PROGRAM-DATE-TIME`,
    /// `#EXT-X-DISCONTINUITY`…).
    pub tags: Vec<String>,
    pub uri: String,
}

impl Segment {
    fn discontinuity(&self) -> bool {
        self.tags.iter().any(|t| t == "#EXT-X-DISCONTINUITY")
    }

    /// The file name the LAN server serves it under (`None`: not a plain relative segment name).
    pub fn file(&self) -> Option<&str> {
        let name = self.uri.rsplit('/').next()?;
        (!self.uri.contains("://") && matches!(served_file(name), Some(File::Segment(_)))).then_some(name)
    }
}

/// A media playlist (ffmpeg's `index.m3u8`, or an upstream one).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaPlaylist {
    /// Header tags in order, without `#EXTM3U`, `#EXT-X-TARGETDURATION`, `#EXT-X-MEDIA-SEQUENCE`
    /// and `#EXT-X-DISCONTINUITY-SEQUENCE` (those are re-rendered).
    pub header: Vec<String>,
    pub target: Option<u32>,
    pub media_seq: u64,
    pub disc_seq: u64,
    pub segments: Vec<Segment>,
    pub endlist: bool,
    /// `#EXT-X-STREAM-INF` seen: a multivariant playlist, not a media one.
    pub multivariant: bool,
}

/// Tags that belong to the playlist, not to the next segment.
const HEADER_TAGS: &[&str] = &[
    "#EXT-X-VERSION",
    "#EXT-X-PLAYLIST-TYPE",
    "#EXT-X-INDEPENDENT-SEGMENTS",
    "#EXT-X-MAP",
    "#EXT-X-START",
    "#EXT-X-ALLOW-CACHE",
    "#EXT-X-I-FRAMES-ONLY",
];

fn tag_name(line: &str) -> &str {
    line.split_once(':').map_or(line, |(n, _)| n)
}

/// Parse a media playlist (lenient: unknown tags are kept with their segment).
pub fn parse(text: &str) -> MediaPlaylist {
    let mut pl = MediaPlaylist::default();
    let mut pending: Vec<String> = Vec::new();
    let mut dur = None;
    let mut seq_offset = 0u64;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !line.starts_with('#') {
            let seq = pl.media_seq + seq_offset;
            seq_offset += 1;
            pl.segments.push(Segment { seq, dur: dur.take().unwrap_or(0.0), tags: std::mem::take(&mut pending), uri: line.to_owned() });
            continue;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name {
            "#EXTM3U" => {}
            "#EXT-X-TARGETDURATION" => pl.target = value.trim().parse().ok(),
            "#EXT-X-MEDIA-SEQUENCE" => pl.media_seq = value.trim().parse().unwrap_or(0),
            "#EXT-X-DISCONTINUITY-SEQUENCE" => pl.disc_seq = value.trim().parse().unwrap_or(0),
            "#EXT-X-ENDLIST" => pl.endlist = true,
            "#EXT-X-STREAM-INF" => {
                pl.multivariant = true;
                pending.push(line.to_owned());
            }
            "#EXTINF" => {
                dur = value.split(',').next().and_then(|d| d.trim().parse::<f64>().ok()).filter(|d| d.is_finite() && *d >= 0.0);
                pending.push(line.to_owned());
            }
            _ if HEADER_TAGS.contains(&name) && pl.segments.is_empty() && pending.is_empty() => pl.header.push(line.to_owned()),
            _ if line.starts_with("#EXT") => pending.push(line.to_owned()),
            _ => {} // comments
        }
    }
    pl
}

/// The video codec a segment's first picture is checked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// The pinned `#EXT-X-TARGETDURATION` (§8.1.4): `ceil(max(hls_time, probed max GOP))`, never
/// below the longest segment of the first ready window (rounded, as §6.2.1 compares), and for
/// copied video (`copy`) [`COPY_HEADROOM_S`] above the longest of them all. hlsenc cuts at the
/// first keyframe after `hls_time · n`, so a segment can run up to `hls_time + GOP` even when the
/// GOP is regular (a 3 s GOP with 4 s segments gives 6 s, 3 s, 3 s…), and a film's scene cuts make
/// its GOPs anything up to the encoder's maximum; converted video has fixed 4 s segments.
pub fn pin(hls_time: f64, gop: Option<f64>, first: &MediaPlaylist, copy: bool) -> u32 {
    let longest = first.segments.iter().map(|s| s.dur).fold(0.0, f64::max);
    let base = hls_time.max(gop.unwrap_or(0.0)).ceil().max(longest.round());
    let target = if copy { base.max((hls_time.max(gop.unwrap_or(0.0)).max(longest) + COPY_HEADROOM_S).ceil()) } else { base };
    target.max(1.0) as u32
}

/// Where a produced segment sits in the item (seconds from the generation's first segment).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    start: f64,
    dur: f64,
}

/// Per-generation guard state (shared by every view of the generation).
#[derive(Debug, Clone)]
pub struct Guard {
    target: u32,
    codec: Option<Codec>,
    violations: u32,
    /// The first segment that outgrew the target: it and everything after it are held back.
    hold: Option<u64>,
    /// The target the held segments need (with the headroom), for the restart.
    wanted: u32,
    /// The highest sequence number checked.
    checked: Option<u64>,
    timeline: BTreeMap<u64, Slot>,
    endlist: bool,
}

impl Guard {
    /// `target` from [`pin`]; `codec`: the IDR check (none for radio).
    pub fn new(target: u32, codec: Option<Codec>) -> Self {
        Self { target, codec, violations: 0, hold: None, wanted: target, checked: None, timeline: BTreeMap::new(), endlist: false }
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    pub fn violations(&self) -> u32 {
        self.violations
    }

    /// Two violations: mark the item irregular (§8.1.4 escalation).
    pub fn irregular(&self) -> bool {
        self.violations >= IRREGULAR_AFTER
    }

    /// A segment outgrew the target: the target the item needs (the session restarts it under
    /// that, where the TV is). `None` while everything fits.
    pub fn overflow(&self) -> Option<u32> {
        self.hold.map(|_| self.wanted)
    }

    /// ffmpeg wrote ENDLIST (the item is fully produced).
    pub fn complete(&self) -> bool {
        self.endlist
    }

    /// The last produced sequence number.
    pub fn last_seq(&self) -> Option<u64> {
        self.timeline.keys().next_back().copied()
    }

    /// Check the segments new since the last call. `first_picture` reads a segment file and
    /// answers whether its first picture is an IDR/IRAP (`None`: can't tell, not counted).
    /// Returns the number of new violations.
    pub fn observe(&mut self, pl: &MediaPlaylist, mut first_picture: impl FnMut(&str, Codec) -> Option<bool>) -> u32 {
        let before = self.violations;
        self.endlist |= pl.endlist;
        let from = self.checked;
        for seg in pl.segments.iter().filter(|s| from.is_none_or(|c| s.seq > c)) {
            self.checked = Some(seg.seq);
            let start = match self.timeline.range(..seg.seq).next_back() {
                Some((_, prev)) => prev.start + prev.dur,
                None => 0.0,
            };
            self.timeline.insert(seg.seq, Slot { start, dur: seg.dur });
            if self.timeline.len() > TIMELINE_CAP {
                self.timeline.pop_first();
            }
            if seg.dur > f64::from(self.target) + TARGET_SLACK_S {
                // never a new TARGETDURATION mid-stream (§6.2.1): held back from here
                let wanted = ((seg.dur + COPY_HEADROOM_S).ceil() as u32).max(self.target + 1);
                if self.hold.is_none() {
                    tracing::debug!(
                        "cast: segment {} lasts {:.2} s, past the {} s target: held back (needs {wanted} s)",
                        seg.seq,
                        seg.dur,
                        self.target
                    );
                }
                self.hold.get_or_insert(seg.seq);
                self.wanted = self.wanted.max(wanted);
                if wanted > MAX_TARGET_S && self.violations < IRREGULAR_AFTER {
                    // keyframes this sparse: converting is the way (L3's are every 2 s)
                    tracing::debug!("cast: segment {} lasts {:.2} s: past any target", seg.seq, seg.dur);
                    self.violations = IRREGULAR_AFTER;
                }
            }
            if let (Some(codec), Some(file)) = (self.codec, seg.file())
                && first_picture(file, codec) == Some(false)
            {
                self.violations += 1;
                tracing::debug!("cast: segment {} doesn't start with an IDR", seg.seq);
            }
        }
        self.violations - before
    }

    /// The playlist the TV gets: `view_first` = the view's first segment (`None`: everything
    /// listed, the live window); nothing from a held segment on.
    pub fn render(&self, pl: &MediaPlaylist, view_first: Option<u64>) -> String {
        let first = view_first.unwrap_or(0);
        let held = |s: &Segment| self.hold.is_some_and(|h| s.seq >= h);
        let shown: Vec<&Segment> = pl.segments.iter().filter(|s| s.seq >= first && s.file().is_some() && !held(s)).collect();
        let media_seq = shown.first().map_or(first.max(pl.media_seq), |s| s.seq);
        let dropped_disc = pl.segments.iter().filter(|s| s.seq < media_seq && s.discontinuity()).count() as u64;
        let mut out = String::with_capacity(64 + shown.len() * 96);
        out.push_str("#EXTM3U\n");
        // a header tag naming a file (`#EXT-X-MAP`) passes only with a served relative name
        let header = |version: bool| {
            pl.header.iter().filter(move |t| (tag_name(t) == "#EXT-X-VERSION") == version && tag_uri(t).is_none_or(|u| served_file(u).is_some()))
        };
        // ffmpeg's order: VERSION, then the re-rendered numbers, then the rest
        for t in header(true) {
            out.push_str(t);
            out.push('\n');
        }
        out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{media_seq}\n", self.target));
        if pl.disc_seq + dropped_disc > 0 {
            out.push_str(&format!("#EXT-X-DISCONTINUITY-SEQUENCE:{}\n", pl.disc_seq + dropped_disc));
        }
        for t in header(false) {
            out.push_str(t);
            out.push('\n');
        }
        // the TV starts where the relay started (module docs)
        let event = pl.header.iter().any(|t| t == "#EXT-X-PLAYLIST-TYPE:EVENT");
        if event && !pl.header.iter().any(|t| tag_name(t) == "#EXT-X-START") {
            out.push_str("#EXT-X-START:TIME-OFFSET=0\n");
        }
        for s in shown {
            for t in &s.tags {
                out.push_str(t);
                out.push('\n');
            }
            out.push_str(s.file().unwrap_or_default());
            out.push('\n');
        }
        if pl.endlist && self.hold.is_none() {
            out.push_str("#EXT-X-ENDLIST\n");
        }
        out
    }

    /// The produced segment holding `rel_s` (seconds from the generation's first segment) and
    /// its start: a view's first segment (§8.5 seek inside what's produced).
    pub fn segment_at(&self, rel_s: f64) -> Option<(u64, f64)> {
        let (seq, slot) = self.timeline.range(..).rev().find(|(_, slot)| slot.start <= rel_s)?;
        (rel_s < slot.start + slot.dur || (self.endlist && Some(*seq) == self.last_seq() && rel_s <= slot.start + slot.dur))
            .then_some((*seq, slot.start))
    }

    /// Segment `seq` as `(start, end)` measured from the start of `view_first` (what the TV's
    /// fetch says about its playhead, `SegmentFetched`).
    pub fn span(&self, seq: u64, view_first: u64) -> Option<(f64, f64)> {
        let slot = self.timeline.get(&seq)?;
        let origin = self.timeline.get(&view_first).map_or(0.0, |s| s.start);
        Some(((slot.start - origin).max(0.0), (slot.start + slot.dur - origin).max(0.0)))
    }

    /// The start of segment `seq` (seconds from the generation's first segment).
    pub fn start_of(&self, seq: u64) -> Option<f64> {
        self.timeline.get(&seq).map(|s| s.start)
    }
}

/// The `URI="…"` attribute of a tag.
fn tag_uri(tag: &str) -> Option<&str> {
    let rest = &tag[tag.find("URI=\"")? + 5..];
    rest.split('"').next()
}

/// Bytes of a segment read for the first-picture check (the first access unit's NAL headers sit
/// in its first PES packet; an fMP4 `moof` is small).
pub const FIRST_PICTURE_BYTES: usize = 256 * 1024;

/// Whether the first picture of a segment is an IDR / IRAP (`None`: no video found or not
/// parseable in the bytes given).
pub fn first_picture_is_idr(data: &[u8], codec: Codec, fmp4: bool) -> Option<bool> {
    if fmp4 { fmp4_first_picture(data, codec) } else { ts_first_picture(data, codec) }
}

/// The first VCL NAL unit's verdict: H.264 types 1–5 (5 = IDR), HEVC types 0–31 (16–21 = IRAP).
fn vcl_verdict(header: u8, codec: Codec) -> Option<bool> {
    match codec {
        Codec::H264 => {
            let t = header & 0x1f;
            (1..=5).contains(&t).then_some(t == 5)
        }
        Codec::Hevc => {
            let t = (header >> 1) & 0x3f;
            (t <= 31).then_some((16..=21).contains(&t))
        }
    }
}

/// Scan Annex B bytes for the first VCL NAL unit.
fn annex_b_verdict(es: &[u8], codec: Codec) -> Option<bool> {
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            if let Some(v) = vcl_verdict(es[i + 3], codec) {
                return Some(v);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// MPEG-TS: PAT → PMT → the video PID (stream type 0x1B H.264, 0x24 HEVC) → its first PES →
/// the first VCL NAL unit (ISO/IEC 13818-1 §2.4.3, §2.4.4).
fn ts_first_picture(data: &[u8], codec: Codec) -> Option<bool> {
    const PKT: usize = 188;
    let start = (0..PKT.min(data.len())).find(|&i| data[i] == 0x47 && data.get(i + PKT).is_none_or(|b| *b == 0x47))?;
    let want_type = match codec {
        Codec::H264 => 0x1b,
        Codec::Hevc => 0x24,
    };
    let (mut pmt_pid, mut video_pid) = (None, None);
    let mut es: Vec<u8> = Vec::new();
    for p in data[start..].as_chunks::<PKT>().0 {
        if p[0] != 0x47 {
            return None;
        }
        let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        if afc & 1 == 0 {
            continue;
        }
        let off = if afc & 2 != 0 { 5 + usize::from(p[4]) } else { 4 };
        let Some(payload) = p.get(off..) else { continue };
        if pid == 0 && pusi && pmt_pid.is_none() {
            let sec = psi_section(payload)?;
            // program loop: 4 bytes each, after the 8-byte header, before the CRC
            pmt_pid = sec.get(8..sec.len().saturating_sub(4))?.as_chunks::<4>().0.iter().find_map(|e| {
                let program = u16::from_be_bytes([e[0], e[1]]);
                (program != 0).then(|| (u16::from(e[2] & 0x1f) << 8) | u16::from(e[3]))
            });
        } else if Some(pid) == pmt_pid && pusi && video_pid.is_none() {
            let sec = psi_section(payload)?;
            let info_len = usize::from(u16::from_be_bytes([*sec.get(10)?, *sec.get(11)?]) & 0x0fff);
            let mut i = 12 + info_len;
            let end = sec.len().saturating_sub(4);
            while i + 5 <= end {
                let ty = sec[i];
                let epid = (u16::from(sec[i + 1] & 0x1f) << 8) | u16::from(sec[i + 2]);
                let es_info = usize::from(u16::from_be_bytes([sec[i + 3], sec[i + 4]]) & 0x0fff);
                if ty == want_type {
                    video_pid = Some(epid);
                    break;
                }
                i += 5 + es_info;
            }
            video_pid?;
        } else if video_pid == Some(pid) {
            if pusi {
                if !es.is_empty() {
                    break; // the next PES: the first one held no VCL NAL
                }
                // PES header: 00 00 01 id len(2) flags(2) header_data_length
                if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                    return None;
                }
                es.extend_from_slice(payload.get(9 + usize::from(payload[8])..)?);
            } else if !es.is_empty() {
                es.extend_from_slice(payload);
            }
            if let Some(v) = annex_b_verdict(&es, codec) {
                return Some(v);
            }
        }
    }
    annex_b_verdict(&es, codec)
}

/// The section a PSI payload starts (pointer field skipped), cut to `section_length`.
fn psi_section(payload: &[u8]) -> Option<&[u8]> {
    let ptr = usize::from(*payload.first()?);
    let sec = payload.get(1 + ptr..)?;
    let len = usize::from(u16::from_be_bytes([*sec.get(1)?, *sec.get(2)?]) & 0x0fff);
    sec.get(..3 + len)
}

/// ISO BMFF box iterator over `data`: `(type, body, offset of the box)`.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8], usize)> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let h = data.get(at..at + 8)?;
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let kind = [h[4], h[5], h[6], h[7]];
        let (hdr, size) = match size {
            1 => (16, usize::try_from(u64::from_be_bytes(data.get(at + 8..at + 16)?.try_into().ok()?)).ok()?),
            0 => (8, data.len() - at),
            n => (8, n),
        };
        if size < hdr {
            return None;
        }
        let body = data.get(at + hdr..at + size).unwrap_or_else(|| &data[(at + hdr).min(data.len())..]);
        let here = at;
        at += size;
        Some((kind, body, here))
    })
}

/// fMP4 (ISO/IEC 14496-12 §8.8): the first `moof` → the video `traf` (ffmpeg maps video first:
/// track 1) → `tfhd` / `trun` → the first sample's length-prefixed NAL units (4-byte lengths, as
/// ffmpeg writes `hvcC`/`avcC`).
fn fmp4_first_picture(data: &[u8], codec: Codec) -> Option<bool> {
    let (_, moof, moof_at) = boxes(data).find(|(k, _, _)| k == b"moof")?;
    for (kind, traf, _) in boxes(moof) {
        if &kind != b"traf" {
            continue;
        }
        let (_, tfhd, _) = boxes(traf).find(|(k, _, _)| k == b"tfhd")?;
        let flags = u32::from_be_bytes([0, *tfhd.get(1)?, *tfhd.get(2)?, *tfhd.get(3)?]);
        let track = u32::from_be_bytes(tfhd.get(4..8)?.try_into().ok()?);
        if track != 1 {
            continue;
        }
        let mut i = 8;
        let base = if flags & 0x1 != 0 {
            let b = u64::from_be_bytes(tfhd.get(i..i + 8)?.try_into().ok()?);
            i += 8;
            usize::try_from(b).ok()?
        } else {
            moof_at
        };
        for (bit, len) in [(0x2, 4), (0x8, 4)] {
            if flags & bit != 0 {
                i += len;
            }
        }
        let default_size = (flags & 0x10 != 0).then(|| tfhd.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))).flatten();
        let (_, trun, _) = boxes(traf).find(|(k, _, _)| k == b"trun")?;
        let tflags = u32::from_be_bytes([0, *trun.get(1)?, *trun.get(2)?, *trun.get(3)?]);
        let mut j = 8; // version+flags, sample_count
        let data_offset = if tflags & 0x1 != 0 {
            let o = i32::from_be_bytes(trun.get(j..j + 4)?.try_into().ok()?);
            j += 4;
            o
        } else {
            0
        };
        if tflags & 0x4 != 0 {
            j += 4;
        }
        if tflags & 0x100 != 0 {
            j += 4;
        }
        let size = if tflags & 0x200 != 0 { trun.get(j..j + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])) } else { default_size }?;
        let at = usize::try_from(i64::try_from(base).ok()? + i64::from(data_offset)).ok()?;
        let sample = data.get(at..at.checked_add(size as usize)?.min(data.len()))?;
        let mut k = 0;
        while k + 5 <= sample.len() {
            let n = u32::from_be_bytes(sample[k..k + 4].try_into().ok()?) as usize;
            if let Some(v) = vcl_verdict(sample[k + 4], codec) {
                return Some(v);
            }
            k = k.checked_add(4 + n)?;
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg 9's EVENT playlist, as `-hls_flags program_date_time` writes it.
    const EVENT: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
        #EXT-X-INDEPENDENT-SEGMENTS\n\
        #EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:00.000+0100\ns0.ts\n\
        #EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:04.000+0100\ns1.ts\n\
        #EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:08.000+0100\ns2.ts\n\
        #EXTINF:2.500000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:12.000+0100\ns3.ts\n";

    #[test]
    fn a_view_starts_at_segment_k_keeps_pdt_and_grows_only_at_the_end() {
        let pl = parse(EVENT);
        assert_eq!((pl.segments.len(), pl.target, pl.endlist), (4, Some(4), false));
        let mut g = Guard::new(pin(4.0, Some(2.0), &pl, false), Some(Codec::H264));
        assert_eq!(g.observe(&pl, |_, _| Some(true)), 0);
        assert_eq!(g.segment_at(9.0), Some((2, 8.0)));
        assert_eq!(g.segment_at(14.6), None, "beyond what's produced");
        let view = g.render(&pl, Some(2));
        assert_eq!(
            view,
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:2\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-START:TIME-OFFSET=0\n\
             #EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:08.000+0100\ns2.ts\n\
             #EXTINF:2.500000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:12.000+0100\ns3.ts\n"
        );
        assert_eq!(g.span(3, 2), Some((4.0, 6.5)), "positions are relative to the view");

        // ffmpeg appends and ends the item: the view only grows at its end
        let more = format!("{EVENT}#EXTINF:4.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-10-12T21:00:14.500+0100\ns4.ts\n#EXT-X-ENDLIST\n");
        let pl = parse(&more);
        g.observe(&pl, |_, _| Some(true));
        let grown = g.render(&pl, Some(2));
        assert!(grown.starts_with(view.as_str()), "{grown}");
        assert!(grown.ends_with("s4.ts\n#EXT-X-ENDLIST\n"));
        assert!(g.complete());
        assert_eq!(g.segment_at(18.5), Some((4, 14.5)), "the very end of a complete item");
    }

    /// The 2026-10-13 film (HEVC copied, GOPs up to 12 s): the target covers the first window's
    /// longest segment with headroom; a later, longer one is held back (never a new
    /// TARGETDURATION mid-stream) and asks for a restart under a longer target; neither is a
    /// violation. Only a missing IDR is.
    #[test]
    fn the_target_never_changes_and_a_longer_segment_is_held_back() {
        // ffmpeg rewrites TARGETDURATION from its window (10 here, 12 later)
        let first = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
            #EXTINF:10.09,\ns0.ts\n#EXTINF:4.2,\ns1.ts\n";
        let pl = parse(first);
        let mut g = Guard::new(pin(4.0, None, &pl, true), Some(Codec::Hevc));
        assert_eq!(g.target(), 13, "10.09 s + 2 s headroom, rounded up");
        assert_eq!(g.observe(&pl, |_, _| Some(true)), 0);
        let later = format!("{first}#EXTINF:12.10,\ns2.ts\n#EXTINF:8.0,\ns3.ts\n#EXTINF:13.6,\ns4.ts\n#EXTINF:4.0,\ns5.ts\n#EXT-X-ENDLIST\n");
        let pl = parse(&later);
        assert_eq!(g.observe(&pl, |_, _| Some(true)), 0, "a long GOP is no violation");
        let out = g.render(&pl, None);
        assert!(out.contains("#EXT-X-TARGETDURATION:13\n") && out.contains("\ns3.ts\n"), "12.10 s fits 13: {out}");
        assert!(!out.contains("s4.ts") && !out.contains("s5.ts") && !out.contains("ENDLIST"), "held back from s4: {out}");
        assert_eq!(g.overflow(), Some(16), "13.6 s + 2 s, rounded up");
        assert!(!g.irregular());
        assert_eq!(g.observe(&parse("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:6\n#EXTINF:4,\ns6.ts\n"), |_, _| Some(false)), 1, "s6 starts on no IDR");
        assert!(!g.irregular());

        // converted video: fixed 4 s segments, no headroom; keyframes 40 s apart: irregular
        assert_eq!(pin(4.0, Some(2.0), &parse("#EXTM3U\n#EXTINF:4.0,\ns0.ts\n#EXTINF:4.0,\ns1.ts\n"), false), 4);
        assert_eq!(pin(4.0, Some(6.0), &parse(""), false), 6, "max(hls_time, GOP)");
        let mut g = Guard::new(8, Some(Codec::H264));
        g.observe(&parse("#EXTM3U\n#EXTINF:40.0,\ns0.ts\n"), |_, _| Some(true));
        assert!(g.irregular(), "past any target");
    }

    #[test]
    fn only_relative_served_names_reach_the_tv() {
        let hostile = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"http://203.0.113.9/init.mp4\"\n\
            #EXTINF:4,\nhttp://provider.example/live/u/p/1.ts\n#EXTINF:4,\n/run/user/1000/tuner/cast/ab12/g1/s1.ts\n\
            #EXTINF:4,\n../s2.ts\n#EXTINF:4,\ns3.ts\n#EXTINF:4,\nindex.m3u8\n";
        let out = Guard::new(4, None).render(&parse(hostile), None);
        assert!(!out.contains("://") && !out.contains("/run/") && !out.contains(".."), "{out}");
        assert!(out.contains("\ns1.ts\n") && out.contains("\ns3.ts\n"), "ffmpeg's own names (path stripped) are kept: {out}");
        assert!(!out.contains("index.m3u8") && !out.contains("EXT-X-MAP"), "{out}");
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:1\n"), "the first listed segment: {out}");
    }

    #[test]
    fn a_view_past_a_discontinuity_advances_the_discontinuity_sequence() {
        let pl = parse("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\ns0.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\ns1.ts\n#EXTINF:4,\ns2.ts\n");
        let out = Guard::new(4, None).render(&pl, Some(2));
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:2\n#EXT-X-DISCONTINUITY-SEQUENCE:1\n"), "{out}");
    }

    fn fixture(name: &str) -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cast/ts").join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// The committed broadcast-shaped fixtures (160×90/96, ≈ 20 KB each), made with ffmpeg 9.0.2
    /// and `-fflags +bitexact`:
    /// * `idr.ts`, `mp2.ts`, `latm.ts`: testsrc2 + sine, x264 `keyint=10`, AAC / MP2 /
    ///   AAC-LATM (`-mpegts_flags latm`) sound;
    /// * `recovery_point.ts`: the second 1 s piece of an x264 `open-gop=1:repeat-headers=1`
    ///   stream cut by ffmpeg's segment muxer in copy mode: it starts on a non-IDR I-frame with a
    ///   recovery-point SEI;
    /// * `interlaced.ts`: x264 `interlaced=1:tff=1` (field-coded, `field_order=tt`) with MP2;
    /// * `hevc_s1.m4s`: the second fMP4 segment of libx265 `keyint=10` HLS (`hvc1`).
    #[test]
    fn the_idr_check_on_broadcast_fixtures() {
        for (name, want) in [("idr.ts", Some(true)), ("mp2.ts", Some(true)), ("latm.ts", Some(true)), ("interlaced.ts", Some(true))] {
            assert_eq!(first_picture_is_idr(&fixture(name), Codec::H264, false), want, "{name}");
        }
        assert_eq!(first_picture_is_idr(&fixture("recovery_point.ts"), Codec::H264, false), Some(false), "copy mode cut on a recovery point");
        assert_eq!(first_picture_is_idr(&fixture("idr.ts"), Codec::Hevc, false), None, "no HEVC stream in it");
        assert_eq!(first_picture_is_idr(b"Cannot read /home/x.m3u8", Codec::H264, false), None);

        let mut m4s = fixture("hevc_s1.m4s");
        assert_eq!(first_picture_is_idr(&m4s, Codec::Hevc, true), Some(true));
        // the same segment with its first VCL NAL rewritten to TRAIL_R (type 1): not an IRAP
        let (_, moof, at) = boxes(&m4s).find(|(k, _, _)| k == b"moof").unwrap();
        let mdat = at + 8 + moof.len() + 8;
        let mut k = mdat;
        loop {
            let n = u32::from_be_bytes(m4s[k..k + 4].try_into().unwrap()) as usize;
            if (m4s[k + 4] >> 1) & 0x3f <= 31 {
                m4s[k + 4] = 1 << 1;
                break;
            }
            k += 4 + n;
        }
        assert_eq!(first_picture_is_idr(&m4s, Codec::Hevc, true), Some(false));
    }
}
