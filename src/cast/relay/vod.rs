//! Full-length VOD and catch-up on the TV (cast design §8.5 "Full-length VOD", the Phase 2
//! design's §29.6 applied to AirPlay): the TV gets the whole item as a VOD playlist
//! (`#EXT-X-PLAYLIST-TYPE:VOD`, every segment, `#EXT-X-ENDLIST`), so it shows the item's real
//! length and its own scrubber reaches anywhere; each segment is made when the TV asks for it.
//! Jellyfin's `DynamicHlsController` serves Apple TVs this way (GPL-3: facts only, read at
//! `61f9a9df`); Tuner differs in handing ffmpeg absolute cut points, so a segment is the same
//! media whichever run made it.
//!
//! * **The map** ([`Map`], fixed for the item): for copied video, the source's keyframes from
//!   its own index ([`keyframes`]: the MP4 `stss`, the Matroska `Cues`, read by `Range` through
//!   the proxy), whole keyframe runs merged into segments of at most [`SEGMENT_S`] (+0.5 s: a GOP
//!   longer than that is a segment of its own), each `EXTINF` exact. Converted video (L3) is cut
//!   on a [`SEGMENT_S`] grid where its keyframes are forced (`-force_key_frames` at those times:
//!   reproducible by construction), when the source has an index too: only then does `-ss` land
//!   on the keyframe before the point (in MPEG-TS it lands between keyframes and the pictures up
//!   to the next one are lost). `#EXT-X-TARGETDURATION` is the longest segment, rounded, known
//!   before anything is made: no segment can outgrow it (the "segment outgrew the target:
//!   restarting" of an EVENT playlist can't happen), and one target serves every rendition.
//! * **A run** ([`OnDemand`]: one ffmpeg, one provider connection, every rendition) makes the
//!   item from segment *k* on: `-ss` where the demuxer lands on *k*'s keyframe (copied) or at
//!   *k*'s start (converted), `-copyts -start_at_zero` (the item's own clock), ffmpeg's `segment`
//!   muxer with the cut points relative to the run's first picture (`segment.c` counts
//!   `-segment_times` from the reference stream's first timestamp: measured 2026-10-13, ffmpeg
//!   n9.0.2): *k*'s keyframe, or from the item's start, where the picture starts on the item's
//!   clock (the sound may lead it, as in a remux of a broadcast: the map keeps that time, and no
//!   cut comes before it), `-segment_start_number k`, a constant `-output_ts_offset` of 10 s, TS
//!   or fMP4 as the plan says, and a CSV segment list. Measured on a 12 s film with irregular keyframes, a
//!   run from 0 and a run from segment 2 give byte-identical fMP4 init sections and media
//!   segments after the restart's first (but for the `mfhd` sequence number), identical TS
//!   video timestamps, and the restart's first segment one copied audio frame earlier (the mov
//!   demuxer seeks the sound to the frame before the keyframe's decode time). Converted sound
//!   (the stereo AAC of a 5.1 source) is cut within one AAC frame of the same points, with a
//!   phase that depends on the run's start. At a join between two runs that is about one audio
//!   frame of overlap or gap, which players resolve by timestamp.
//! * **Surround** (§8.4) is made as one muxed fMP4 output (picture, stereo, surround), so every
//!   track is cut where the picture is, the same in every run; each finished segment is split into
//!   the `v/`, `a/` and `s/` renditions ([`fmp4`]). The renditions share the map, the target and
//!   their segment numbers.
//! * **Publishing.** A segment is complete once the CSV lists it and its successor's file exists
//!   (ffmpeg closes a segment after listing it), or the run ended. Only then is it moved (or
//!   written) into the served directory under its served name, atomically: the LAN server only
//!   ever sees whole segments, and a newer run's copy replaces an older one in place. Each fMP4
//!   segment comes with the init section (one muxer per segment: [`fmp4`]), the same bytes every
//!   time: it is published once and stripped from the segments. Each segment is
//!   checked against the map (its start, and that its end doesn't pass the next cut point): a map
//!   that doesn't hold (an index ffmpeg reads otherwise) sends `Unindexed`, and the item goes
//!   back to its EVENT playlist where the TV is; during the load (the first run's first
//!   segments) the load itself falls back, before the TV has anything. Copied pictures get Phase
//!   1's IDR check (two misses: `Irregular`, L3 next time). A segment that can't be published (a
//!   write that failed, a muxed segment that can't be split) is lost: the TV's requests for it
//!   get the 404 at once, and the run goes on.
//! * **On demand** ([`OnDemand::segment`], Jellyfin's rule): a published segment is served; one
//!   the running run will reach within [`LOOKAHEAD_S`] is waited for (polling every 100 ms, at
//!   most [`WAIT`]; production's lead cap is lifted while anyone waits), when the run's pace gets
//!   it there soon ([`keeps`]: past its first minute a run reads at real time, which Jellyfin's
//!   rule doesn't assume); anything else (behind the run's start, further ahead, or no run) stops
//!   the run and starts one at that segment. The new ffmpeg's request reaches the provider only
//!   once the old connection has closed (the proxy's one wire, §10). Each rendition's request for
//!   segment *k* follows the same rule.
//! * **Production stays capped** (§8.4): a run reads its first minute as fast as the provider
//!   sends it, then at real time (`-readrate`), and pauses while it is more than three minutes
//!   ahead of the segment the TV fetched last; the 8 GiB cap deletes the published segments
//!   furthest from the TV first (a scrub back to them makes them again). Segments otherwise stay
//!   for the generation, so scrubbing back is free; the directory goes with it.
//! * **The provider connection** is the generation's: a run ending (the item's end, a stop for a
//!   scrub) closes the provider's request but not the feed, and sends no `UpstreamEof` (the TV may
//!   still scrub anywhere); the feed's `UpstreamClosed` comes when the generation ends, or at once
//!   when the provider fails (the controller's reconnect policy reloads where the TV is).

pub mod fmp4;
pub mod keyframes;

use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::RelayEvent;
use super::ffmpeg;
use super::plan::Plan;
use super::playlist::{self, Codec, MAX_TARGET_S};
use super::server::File;
use super::upstream::Feed;
use crate::redact::Secret;

/// The segment length aimed at (Apple's HLS authoring rule 7.5; the tests' override when set).
pub const SEGMENT_S: f64 = 6.0;
/// Keyframe runs are merged while the segment stays within the aim plus this (it still rounds to
/// the aim in `#EXT-X-TARGETDURATION`).
const MERGE_SLACK_S: f64 = 0.5;
/// A segment the running run will reach within this is waited for (Jellyfin: 24 s of segments).
pub const LOOKAHEAD_S: f64 = 24.0;
/// ... and only when it can be made within this much media past where the run's pace lets it
/// be by now ([`keeps`]): a segment or so, a few seconds of real time at worst.
const PACED_LOOKAHEAD_S: f64 = 8.0;
/// A request for a segment waits at most this long (the load budget of §5.5).
pub const WAIT: Duration = Duration::from_secs(30);
/// How often a waiting request and a run's publisher look again.
const POLL: Duration = Duration::from_millis(100);
/// How often a run's caps are checked.
const CAP_POLL: Duration = Duration::from_secs(5);
/// Cut points handed to one run (`-segment_times`, and the forced keyframes): 3000 × 6 s is five
/// hours, and keeps each argument far below Linux's 128 KiB.
const MAX_CUTS: usize = 3000;
/// A published segment's start (and end) may differ this much from the map's: copied cuts are
/// exact; converted keyframes fall on the first frame at or after the point.
const COPY_TOLERANCE_S: f64 = 0.05;
const GRID_TOLERANCE_S: f64 = 0.1;
/// Runs in a row that die without publishing anything before the item is given up.
const MAX_FAILS: u32 = 2;
/// ffmpeg must ask the proxy for its input this soon (§8.1.1).
const ATTACH_WITHIN: Duration = Duration::from_secs(10);

/// The segment length aimed at: [`SEGMENT_S`], or the tests' override.
pub fn segment_s() -> f64 {
    ffmpeg::segment_override().unwrap_or(SEGMENT_S)
}

/// Why the item isn't full-length (logged; it keeps Phase 1's EVENT playlist).
pub type Why = &'static str;

/// Where the cut points come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cuts {
    /// The source's keyframes (copied video).
    Index(keyframes::Container),
    /// A fixed grid with forced keyframes (converted video).
    Grid,
}

/// The item's segments, on its own clock (seconds from the file's start: what `-copyts
/// -start_at_zero` gives, what `-ss` counts, and the TV's playlist time).
#[derive(Debug, Clone, PartialEq)]
pub struct Map {
    /// Segment *k* runs from `starts[k]` to `starts[k + 1]` (the last one to `total`).
    starts: Vec<f64>,
    /// `-ss` of a run from segment *k* (`seeks[0]` unused: a run from 0 doesn't seek).
    seeks: Vec<f64>,
    /// Where the picture starts: what a run from 0 counts its cut points from (module docs).
    picture: f64,
    total: f64,
    target: u32,
    pub cuts: Cuts,
}

impl Map {
    /// The map of copied video from its keyframe `index` (module docs): `start_s` is the file's
    /// start on the index's clock (ffprobe's `format.start_time`), `total` its length. The
    /// picture starts at the first keyframe (later than the file when the sound leads).
    pub fn from_index(index: &keyframes::Index, start_s: f64, total: f64, aim: f64) -> Result<Map, Why> {
        if !(total.is_finite() && total > 0.0) {
            return Err("its length is unknown");
        }
        let picture = index.keyframes.iter().map(|k| k.pts - start_s).reduce(f64::min).filter(|p| p.is_finite()).unwrap_or(0.0).max(0.0);
        let last = (aim / 2.0).min(1.0);
        let kfs: Vec<(f64, f64)> = index
            .keyframes
            .iter()
            .filter_map(|k| Some((k.pts - start_s, k.seek? - start_s)))
            .filter(|(pts, seek)| *pts > picture + 0.001 && *pts < total - last && seek >= pts)
            .collect();
        let (limit, min) = (aim + MERGE_SLACK_S, (aim / 2.0).min(1.0));
        let (mut starts, mut seeks) = (vec![0.0], vec![0.0]);
        let (mut from, mut i) = (0.0, 0);
        // the rest fits in one segment: done
        while total - from > limit {
            // the furthest keyframe within the limit (and not too close), else the first beyond it
            let mut best = None;
            while i < kfs.len() && kfs[i].0 - from <= limit {
                if kfs[i].0 - from >= min {
                    best = Some(i);
                }
                i += 1;
            }
            let cut = match best {
                Some(j) => j,
                None if i < kfs.len() => {
                    i += 1;
                    i - 1
                }
                None => break,
            };
            starts.push(kfs[cut].0);
            seeks.push(kfs[cut].1);
            from = kfs[cut].0;
        }
        Map::finish(starts, seeks, picture, total, Cuts::Index(index.container))
    }

    /// Converted video's map: a cut every `step` seconds (the last segment may be up to half a
    /// step longer rather than a sliver), none within half a step after where the `picture`
    /// starts (ffprobe's: the sound may lead it).
    pub fn grid(total: f64, step: f64, picture: f64) -> Result<Map, Why> {
        if !(total.is_finite() && total > 0.0 && step > 0.0) {
            return Err("its length is unknown");
        }
        let picture = if picture.is_finite() { picture.max(0.0) } else { 0.0 };
        let n = ((total - step / 2.0) / step).ceil().max(1.0) as usize;
        let starts: Vec<f64> = (0..n).map(|k| k as f64 * step).filter(|s| *s == 0.0 || *s >= picture + step / 2.0).collect();
        Map::finish(starts.clone(), starts, picture, total, Cuts::Grid)
    }

    fn finish(starts: Vec<f64>, seeks: Vec<f64>, picture: f64, total: f64, cuts: Cuts) -> Result<Map, Why> {
        let mut map = Map { starts, seeks, picture, total, target: 1, cuts };
        let longest = (0..map.len()).map(|k| map.dur(k)).fold(0.0, f64::max);
        // RFC 8216 §4.3.3.1: every EXTINF, rounded, at most the target
        map.target = (longest.round() as u32).max(1);
        if map.target > MAX_TARGET_S {
            return Err("keyframes more than 30 s apart");
        }
        Ok(map)
    }

    pub fn len(&self) -> u64 {
        self.starts.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn total(&self) -> f64 {
        self.total
    }

    /// `#EXT-X-TARGETDURATION`.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Where segment `k` starts (`len()`: the item's end).
    pub fn start(&self, k: u64) -> f64 {
        self.starts.get(k as usize).copied().unwrap_or(self.total)
    }

    pub fn dur(&self, k: u64) -> f64 {
        (self.start(k + 1) - self.start(k)).max(0.0)
    }

    /// The segment holding `t` (clamped to the item).
    pub fn segment_at(&self, t: f64) -> u64 {
        let i = self.starts.partition_point(|s| *s <= t);
        i.saturating_sub(1) as u64
    }

    /// Segment `k` as `(start, end)` from the start of segment `first` (`SegmentFetched`).
    pub fn span(&self, k: u64, first: u64) -> Option<(f64, f64)> {
        (k < self.len()).then(|| {
            let origin = self.start(first);
            ((self.start(k) - origin).max(0.0), (self.start(k + 1) - origin).max(0.0))
        })
    }

    /// `-ss` of a run from segment `k` (`None` from 0).
    pub fn seek(&self, k: u64) -> Option<f64> {
        (k > 0).then(|| self.seeks.get(k as usize).copied()).flatten()
    }

    /// A run from segment `k`: its cut points relative to its first picture (`-segment_times`:
    /// `k`'s keyframe, or where the picture starts in a run from 0), and the first segment it
    /// can't finish (a run cut short by [`MAX_CUTS`] doesn't know where its last segment ends;
    /// `len()` when it reaches the item's end).
    pub fn cuts_from(&self, k: u64) -> (String, u64) {
        let from = self.start(k).max(self.picture);
        let next = (k as usize + 1).min(self.starts.len());
        let upto = (next + MAX_CUTS).min(self.starts.len());
        let cuts = self.starts[next..upto].iter().map(|s| format!("{:.6}", s - from)).collect::<Vec<_>>().join(",");
        let limit = if upto == self.starts.len() { self.len() } else { upto as u64 - 1 };
        (cuts, limit)
    }

    /// Converted video's forced keyframes for a run from `k` up to `limit` (the item's clock).
    pub fn forced_from(&self, k: u64, limit: u64) -> String {
        let upto = (limit as usize + 1).min(self.starts.len());
        self.starts[(k as usize).min(upto)..upto].iter().map(|s| format!("{s:.6}")).collect::<Vec<_>>().join(",")
    }

    /// A rendition's media playlist from segment `first` (a whole one: 0), every segment to the
    /// end: `fmp4` with its init section's name, `start`: where the TV should start, from the
    /// playlist's first segment (`EXT-X-START`, RFC 8216 §4.4.2.2).
    pub fn playlist(&self, fmp4: bool, init: Option<&str>, first: u64, start: Option<f64>) -> String {
        let first = first.min(self.len().saturating_sub(1));
        let mut out = String::with_capacity(256 + (self.len() - first) as usize * 32);
        out.push_str(&format!(
            "#EXTM3U\n#EXT-X-VERSION:{}\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{first}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
            if fmp4 { 7 } else { 3 },
            self.target
        ));
        if let Some(at) = start.filter(|s| *s > 0.0) {
            out.push_str(&start_tag(at));
            out.push('\n');
        }
        if let (true, Some(init)) = (fmp4, init) {
            out.push_str(&format!("#EXT-X-MAP:URI=\"{init}\"\n"));
        }
        for k in first..self.len() {
            out.push_str(&format!("#EXTINF:{:.6},\n{}\n", self.dur(k), File::Segment(k).name(fmp4)));
        }
        out.push_str("#EXT-X-ENDLIST\n");
        out
    }
}

/// `#EXT-X-START` at `at` seconds into the playlist, exactly there (`PRECISE=YES`: a resume
/// starts at its point, not at its segment).
pub fn start_tag(at: f64) -> String {
    format!("#EXT-X-START:TIME-OFFSET={at:.3},PRECISE=YES")
}

/// How a run's output is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One MPEG-TS media playlist (H.264 in stereo).
    Ts,
    /// One fMP4 media playlist with `init.mp4` (HEVC in stereo).
    Fmp4,
    /// The `v/`, `a/`, `s/` renditions split from one muxed fMP4 output, `init_<n>.mp4` each.
    Surround,
}

impl Layout {
    pub fn fmp4(self) -> bool {
        self != Layout::Ts
    }
}

/// What every run of the item is made with.
pub struct Spec {
    pub ffmpeg: PathBuf,
    /// The proxy's loopback URL (a bearer token while the session lives).
    pub url: Secret<String>,
    pub plan: Plan,
    /// Copied HEVC (`-tag:v hvc1`).
    pub hevc: bool,
    pub fps: Option<f64>,
    pub audio_index: Option<usize>,
    /// The IDR check's codec (copied pictures only: converted ones are forced IDRs).
    pub idr: Option<Codec>,
}

/// What an [`OnDemand`] reports to and through.
pub struct Ctx {
    pub feed: Arc<Feed>,
    pub tx: mpsc::UnboundedSender<RelayEvent>,
    pub generation: Arc<AtomicU64>,
    pub label: Arc<str>,
}

/// A run's state, shared by its supervisor and [`OnDemand::decide`].
struct RunCtx {
    dir: PathBuf,
    /// When ffmpeg started (its `-readrate` clock: [`keeps`]).
    born: Instant,
    first: u64,
    /// The first segment it can't finish ([`Map::cuts_from`]).
    limit: u64,
    /// The next segment it will publish.
    made: AtomicU64,
    /// [`RUNNING`], [`DONE`], [`FAILED`] or [`KILLED`].
    state: AtomicU8,
    stop: CancellationToken,
}

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const FAILED: u8 = 2;
const KILLED: u8 = 3;

/// [`OnDemand`]'s phase bits: the TV has the playlist ([`OnDemand::serve`]); the map didn't hold.
const SERVING: u8 = 1;
const BROKEN: u8 = 2;

/// Whether a request for segment `k` waits for the run from `first` that has made the segments
/// before `made`, can make those before `limit` and started `elapsed` seconds ago (module docs, "On
/// demand"): it hasn't passed `k`, `k` starts within [`LOOKAHEAD_S`] of what it made, and `k`
/// ends within [`PACED_LOOKAHEAD_S`] of where `-readrate` lets the run be by now (its first
/// [`ffmpeg::READ_BURST`] seconds from where it started at once, then real time). Past its burst,
/// a run about a minute ahead of the TV reads at 1×: a segment 24 s beyond what it made would take
/// it up to 30 s of real time, where a run started there makes it in seconds.
fn keeps(map: &Map, first: u64, made: u64, limit: u64, elapsed: f64, k: u64) -> bool {
    let frontier = map.start(first) + ffmpeg::READ_BURST + elapsed.max(0.0);
    made <= k && k < limit && map.start(k) - map.start(made) <= LOOKAHEAD_S && map.start(k + 1) <= frontier + PACED_LOOKAHEAD_S
}

impl RunCtx {
    fn made(&self) -> u64 {
        self.made.load(Ordering::Acquire)
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct Runs {
    run: Option<(Arc<RunCtx>, JoinHandle<()>)>,
    next: u32,
    /// Runs in a row that died without publishing anything.
    fails: u32,
    /// The first segment the source can't give (a run reached the input's end before it).
    end: Option<u64>,
}

/// One full-length item's segments, made on demand (module docs). Shared by the relay (which
/// starts and ends it) and the LAN server (which asks it for segments).
pub struct OnDemand {
    pub map: Map,
    pub layout: Layout,
    /// The generation's directory (runs are made in `r<n>/` under it).
    dir: PathBuf,
    /// Where each rendition is published, main first (the output's renditions).
    rends: Vec<PathBuf>,
    spec: Spec,
    ctx: Ctx,
    runs: tokio::sync::Mutex<Runs>,
    /// A segment was published or a run ended.
    changed: Notify,
    /// Requests waiting for a segment (the lead cap is lifted meanwhile).
    waiters: AtomicUsize,
    /// 1 + the main rendition's segment the TV fetched last (0: none yet).
    last_fetch: AtomicU64,
    /// The first run's init section (the surround split needs its `trex` defaults).
    init: Mutex<Option<Vec<u8>>>,
    violations: AtomicU32,
    irregular_sent: AtomicBool,
    /// [`SERVING`] and [`BROKEN`]: the map didn't hold, nothing more is published (`Unindexed`
    /// sent once the TV has the playlist; before, the load falls back).
    phase: AtomicU8,
    /// Segments made but not published (module docs, "Publishing"): requests for them fail at
    /// once instead of making them again and again.
    lost: Mutex<BTreeSet<u64>>,
    /// Ended (the generation, or the provider): every request is a miss.
    dead: AtomicBool,
}

impl OnDemand {
    pub fn new(map: Map, layout: Layout, dir: PathBuf, rends: Vec<PathBuf>, spec: Spec, ctx: Ctx) -> Self {
        Self {
            map,
            layout,
            dir,
            rends,
            spec,
            ctx,
            runs: tokio::sync::Mutex::new(Runs::default()),
            changed: Notify::new(),
            waiters: AtomicUsize::new(0),
            last_fetch: AtomicU64::new(0),
            init: Mutex::new(None),
            violations: AtomicU32::new(0),
            irregular_sent: AtomicBool::new(false),
            phase: AtomicU8::new(0),
            lost: Mutex::new(BTreeSet::new()),
            dead: AtomicBool::new(false),
        }
    }

    /// The map didn't hold (the relay serves the item's next load as an EVENT playlist).
    pub fn broken(&self) -> bool {
        self.phase.load(Ordering::SeqCst) & BROKEN != 0
    }

    /// The load hands the TV the playlist: from now on a map that doesn't hold is reported
    /// (`Unindexed`). `false` when it already didn't hold: the load falls back instead (one RMW
    /// each side, so exactly one of them acts).
    pub fn serve(&self) -> bool {
        self.phase.fetch_or(SERVING, Ordering::SeqCst) & BROKEN == 0
    }

    fn is_lost(&self, k: u64) -> bool {
        self.lost.lock().unwrap_or_else(|e| e.into_inner()).contains(&k)
    }

    fn send(&self, ev: impl FnOnce(u64) -> RelayEvent) {
        let _ = self.ctx.tx.send(ev(self.ctx.generation.load(Ordering::Relaxed)));
    }

    /// Rendition `i`'s init section name (`None`: MPEG-TS).
    pub fn init_file(&self, i: usize) -> Option<File> {
        match self.layout {
            Layout::Ts => None,
            Layout::Fmp4 => Some(File::Init(None)),
            Layout::Surround => u8::try_from(i).ok().map(|n| File::Init(Some(n))),
        }
    }

    fn segment_path(&self, i: usize, k: u64) -> Option<PathBuf> {
        Some(self.rends.get(i)?.join(File::Segment(k).name(self.layout.fmp4())))
    }

    /// Rendition `i`'s playlist from segment `first`, starting the TV at `start` (see
    /// [`Map::playlist`]).
    pub fn playlist(&self, i: usize, first: u64, start: Option<f64>) -> String {
        let init = self.init_file(i).map(|f| f.name(true));
        self.map.playlist(self.layout.fmp4(), init.as_deref(), first, start)
    }

    /// The TV fetched segment `k` of rendition `i` (the main one is the lead cap's reference).
    pub fn fetched(&self, i: usize, k: u64) {
        if i == 0 {
            self.last_fetch.store(k + 1, Ordering::Relaxed);
        }
    }

    /// Whether segment `k` is published in every rendition (and the init sections are).
    pub async fn ready(&self, k: u64) -> bool {
        for i in 0..self.rends.len() {
            if let Some(f) = self.init_file(i)
                && !exists(&self.rends[i].join(f.name(true))).await
            {
                return false;
            }
            match self.segment_path(i, k) {
                Some(p) if exists(&p).await => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether production has ended for good (the provider failed, a run kept failing, the map
    /// broke): the load or the request gives up.
    pub fn over(&self) -> bool {
        self.dead.load(Ordering::Relaxed) || self.broken()
    }

    /// The running run ended without publishing anything (a load's first run that failed).
    pub async fn failed_start(&self) -> bool {
        let st = self.runs.lock().await;
        st.fails > 0 || st.run.as_ref().is_some_and(|(r, _)| r.state() != RUNNING && r.made() == r.first)
    }

    /// Start production at segment `k` (a load: the TV's first segment).
    pub async fn start(self: &Arc<Self>, k: u64) {
        let mut st = self.runs.lock().await;
        self.restart(&mut st, k).await;
    }

    /// Segment `k` of rendition `i` once published (module docs, "On demand"): `None` when it
    /// can't be (past the item, production over, or not within [`WAIT`]).
    pub async fn segment(self: &Arc<Self>, k: u64, i: usize) -> Option<PathBuf> {
        let path = self.segment_path(i, k).filter(|_| k < self.map.len())?;
        self.wait_for(&path, Some(k)).await.then_some(path)
    }

    /// Rendition `i`'s init section once published.
    pub async fn init(self: &Arc<Self>, i: usize) -> Option<PathBuf> {
        let path = self.rends.get(i)?.join(self.init_file(i)?.name(true));
        self.wait_for(&path, None).await.then_some(path)
    }

    async fn wait_for(self: &Arc<Self>, path: &Path, k: Option<u64>) -> bool {
        if exists(path).await {
            return true;
        }
        let _waiting = Waiting::new(&self.waiters);
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if exists(path).await {
                return true;
            }
            if self.over() || !self.decide(k, path).await {
                return false;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::info!(
                    "{}: segment {} wasn't ready within {} s",
                    self.ctx.label,
                    k.map_or_else(|| "init".into(), |k| k.to_string()),
                    WAIT.as_secs()
                );
                return false;
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// Jellyfin's rule for segment `k` (`None`: the init section, which any run makes): keep the
    /// running run when it will reach `k` within [`LOOKAHEAD_S`], else start one at `k`. `false`
    /// when `k` can't be made.
    async fn decide(self: &Arc<Self>, k: Option<u64>, path: &Path) -> bool {
        let mut st = self.runs.lock().await;
        if self.over() {
            return false;
        }
        // what a run that ended says
        if let Some((r, _)) = &st.run
            && r.state() != RUNNING
        {
            let (state, made, first) = (r.state(), r.made(), r.first);
            if state == DONE && made < self.map.len() {
                // the input ended before the map does
                st.end = Some(st.end.map_or(made, |e| e.min(made)));
            }
            if state == FAILED && made == first {
                st.fails += 1;
            }
            st.run = None;
        }
        if st.fails >= MAX_FAILS {
            drop(st);
            tracing::warn!("{}: ffmpeg can't make this item's segments", self.ctx.label);
            self.dead.store(true, Ordering::Relaxed);
            self.send(|generation| RelayEvent::Failed { generation, kind: super::CastErrorKind::Unsupported });
            self.changed.notify_waiters();
            return false;
        }
        let Some(k) = k else {
            if st.run.is_none() {
                self.restart(&mut st, 0).await;
            }
            return true;
        };
        if st.end.is_some_and(|e| k >= e) || self.is_lost(k) {
            return false;
        }
        // the run hasn't passed `k` (a segment it made and the cap deleted is made again)
        let keep = st.run.as_ref().is_some_and(|(r, _)| keeps(&self.map, r.first, r.made(), r.limit, r.born.elapsed().as_secs_f64(), k));
        // published meanwhile (a run publishes the file before it counts it as made)
        if !keep && !exists(path).await {
            tracing::debug!("{}: making segment {k} ({:.1} s) on demand", self.ctx.label, self.map.start(k));
            self.restart(&mut st, k).await;
        }
        true
    }

    /// Stop the running run (ffmpeg killed and gone) and start one at `k`.
    async fn restart(self: &Arc<Self>, st: &mut Runs, k: u64) {
        if let Some((r, task)) = st.run.take() {
            r.stop.cancel();
            let _ = task.await;
            if r.made() > r.first {
                st.fails = 0;
            }
        }
        if self.over() {
            return;
        }
        st.next += 1;
        match self.spawn(k, st.next).await {
            Ok(run) => st.run = Some(run),
            Err(e) => {
                tracing::warn!("{}: ffmpeg didn't start: {e}", self.ctx.label);
                st.fails += 1;
            }
        }
    }

    async fn spawn(self: &Arc<Self>, k: u64, n: u32) -> std::io::Result<(Arc<RunCtx>, JoinHandle<()>)> {
        let dir = self.dir.join(format!("r{n}"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let (cuts, limit) = self.map.cuts_from(k);
        let grid = self.map.cuts == Cuts::Grid;
        let forced = grid.then(|| self.map.forced_from(k, limit));
        let run = ffmpeg::Run {
            url: self.spec.url.expose(),
            seek_s: self.map.seek(k),
            dir: &dir,
            first: k,
            cuts: &cuts,
            // copied cuts land exactly on their keyframe; forced ones within a frame after
            delta: if grid { (segment_s() / 2.0).min(1.0) } else { 0.001 },
            forced: forced.as_deref(),
            hevc: self.spec.hevc,
            fps: self.spec.fps,
            audio_index: self.spec.audio_index,
        };
        let argv = ffmpeg::run_argv(&run, &self.spec.plan);
        self.ctx.feed.arm_final_reader();
        let progress = ffmpeg::Progress { watch_speed: false, on_slow: Box::new(|| {}) };
        let child = ffmpeg::spawn(&self.spec.ffmpeg, &argv, self.ctx.label.clone(), progress)?;
        let ctx = Arc::new(RunCtx {
            dir,
            born: Instant::now(),
            first: k,
            limit,
            made: AtomicU64::new(k),
            state: AtomicU8::new(RUNNING),
            stop: CancellationToken::new(),
        });
        let task = tokio::spawn(self.clone().supervise(child, ctx.clone()));
        Ok((ctx, task))
    }

    /// One run's life: publish what it finishes, keep it within the caps, end it when told, when
    /// it can't attach, or when the provider fails.
    async fn supervise(self: Arc<Self>, mut child: ffmpeg::Child, run: Arc<RunCtx>) {
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut caps = tokio::time::interval(CAP_POLL);
        caps.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen = 0usize;
        let mut suspended = false;
        let started = Instant::now();
        let failed = self.ctx.feed.wait_failed();
        tokio::pin!(failed);
        let exit = loop {
            tokio::select! {
                biased;
                () = run.stop.cancelled() => {
                    child.kill().await;
                    break ffmpeg::Exit::Killed;
                }
                e = child.wait() => break e,
                () = &mut failed => {
                    child.kill().await;
                    break ffmpeg::Exit::Killed;
                }
                _ = tick.tick() => {
                    if !self.scan(&run, &mut seen, false).await {
                        child.kill().await;
                        break ffmpeg::Exit::Killed;
                    }
                    if suspended && self.waiters.load(Ordering::Relaxed) > 0 {
                        suspended = false;
                        child.suspend(false);
                    }
                    if !self.ctx.feed.final_attached() && started.elapsed() >= ATTACH_WITHIN {
                        tracing::info!("{}: ffmpeg didn't open its input within {ATTACH_WITHIN:?}", self.ctx.label);
                        child.kill().await;
                        break ffmpeg::Exit::Failed;
                    }
                }
                _ = caps.tick() => suspended = self.caps(&child, &run, suspended).await,
            }
        };
        if exit == ffmpeg::Exit::Failed && !self.ctx.feed.failed() {
            tracing::info!("{}: ffmpeg stopped with an error making segment {}", self.ctx.label, run.made());
        }
        // whatever it finished (its last listed segment too, when it ended by itself)
        self.scan(&run, &mut seen, exit == ffmpeg::Exit::Done).await;
        let state = match exit {
            ffmpeg::Exit::Done => DONE,
            ffmpeg::Exit::Failed => FAILED,
            ffmpeg::Exit::Killed => KILLED,
        };
        run.state.store(state, Ordering::Release);
        let _ = tokio::fs::remove_dir_all(&run.dir).await;
        self.changed.notify_waiters();
    }

    /// Publish the segments `run` has finished since `seen` (module docs, "Publishing");
    /// `false` when the run must stop (the map broke, its cut points ran out). A segment that
    /// can't be published is lost, and the run goes on.
    async fn scan(&self, run: &RunCtx, seen: &mut usize, ended: bool) -> bool {
        let Ok(text) = tokio::fs::read_to_string(run.dir.join(ffmpeg::RUN_LIST)).await else { return true };
        let entries: Vec<(u64, f64, f64)> = text.lines().filter_map(list_entry).collect();
        let fmp4 = self.layout.fmp4();
        while *seen < entries.len() {
            let (k, start, end) = entries[*seen];
            let complete = *seen + 1 < entries.len() || ended || exists(&run.dir.join(File::Segment(k + 1).name(fmp4))).await;
            if !complete {
                break;
            }
            if k >= run.limit || self.broken() {
                return false;
            }
            if !self.holds(run, k, start, end) {
                let was = self.phase.fetch_or(BROKEN, Ordering::SeqCst);
                if was & BROKEN == 0 {
                    tracing::warn!(
                        "{}: segment {k} came out at {:.3}–{:.3} s, the map says {:.3}–{:.3} s: the item goes back to a growing playlist",
                        self.ctx.label,
                        start - ffmpeg::RUN_OFFSET_S,
                        end - ffmpeg::RUN_OFFSET_S,
                        self.map.start(k),
                        self.map.start(k + 1)
                    );
                    // a load still waiting falls back by itself (`serve`)
                    if was & SERVING != 0 {
                        self.send(|generation| RelayEvent::Unindexed { generation });
                    }
                }
                self.changed.notify_waiters();
                return false;
            }
            match self.publish(run, k).await {
                Ok(()) => {
                    self.lost.lock().unwrap_or_else(|e| e.into_inner()).remove(&k);
                }
                Err(e) => {
                    tracing::warn!("{}: segment {k} can't be published ({e}): the TV's requests for it go unanswered", self.ctx.label);
                    self.lost.lock().unwrap_or_else(|e| e.into_inner()).insert(k);
                }
            }
            *seen += 1;
            run.made.store(k + 1, Ordering::Release);
            self.changed.notify_waiters();
        }
        true
    }

    /// Whether segment `k` (listed from `start` to `end`, the run's clock) is where the map
    /// says: its start (unless it is the run's first, which the list doesn't time) and its end
    /// no later than the next cut point (a missed cut would make it two). Open GOPs end a little
    /// early: their leading pictures show before the next keyframe and belong to the next segment.
    fn holds(&self, run: &RunCtx, k: u64, start: f64, end: f64) -> bool {
        let tolerance = if self.map.cuts == Cuts::Grid { GRID_TOLERANCE_S } else { COPY_TOLERANCE_S };
        let at = |s: f64| s + ffmpeg::RUN_OFFSET_S;
        (k == run.first || (start - at(self.map.start(k))).abs() <= tolerance)
            && (k + 1 >= self.map.len() || end <= at(self.map.start(k + 1)) + tolerance)
    }

    /// Move segment `k` from the run's directory into the served one(s): as it is (MPEG-TS),
    /// without its init section (fMP4), or split into the renditions (surround).
    async fn publish(&self, run: &RunCtx, k: u64) -> std::io::Result<()> {
        let name = File::Segment(k).name(self.layout.fmp4());
        let raw = run.dir.join(&name);
        let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
        let main = match self.layout {
            Layout::Ts => {
                let to = self.rends[0].join(&name);
                tokio::fs::rename(&raw, &to).await?;
                to
            }
            Layout::Fmp4 | Layout::Surround => {
                let data = tokio::fs::read(&raw).await?;
                let media = match fmp4::split_header(&data) {
                    Some(h) => {
                        self.keep_init(&data[..h]).await?;
                        &data[h..]
                    }
                    None => &data[..],
                };
                if self.layout == Layout::Fmp4 {
                    write_atomic(&self.rends[0], &name, media).await?;
                } else {
                    let init = self.init.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| bad("no init section yet"))?;
                    let tracks = fmp4::tracks(&init).filter(|t| t.len() == self.rends.len()).ok_or_else(|| bad("not one track per rendition"))?;
                    // the picture last: its file says the segment is there
                    for (i, track) in tracks.iter().enumerate().rev() {
                        let part = fmp4::track_media(media, *track, fmp4::trex_size(&init, *track)).ok_or_else(|| bad("a track can't be split"))?;
                        write_atomic(&self.rends[i], &name, &part).await?;
                    }
                }
                let _ = tokio::fs::remove_file(&raw).await;
                self.rends[0].join(&name)
            }
        };
        if let Some(codec) = self.spec.idr {
            self.idr_check(&main, codec, k).await;
        }
        Ok(())
    }

    /// The first run's init section, published once per rendition (every run's is the same).
    async fn keep_init(&self, header: &[u8]) -> std::io::Result<()> {
        if self.init.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return Ok(());
        }
        let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
        match self.layout {
            Layout::Ts => {}
            Layout::Fmp4 => write_atomic(&self.rends[0], &File::Init(None).name(true), header).await?,
            Layout::Surround => {
                let tracks = fmp4::tracks(header).filter(|t| t.len() == self.rends.len()).ok_or_else(|| bad("not one track per rendition"))?;
                for (i, track) in tracks.iter().enumerate() {
                    let one = fmp4::track_init(header, *track).ok_or_else(|| bad("a track's init can't be split"))?;
                    let name = self.init_file(i).map(|f| f.name(true)).ok_or_else(|| bad("no init name"))?;
                    write_atomic(&self.rends[i], &name, &one).await?;
                }
            }
        }
        *self.init.lock().unwrap_or_else(|e| e.into_inner()) = Some(header.to_vec());
        Ok(())
    }

    /// Phase 1's IDR check (§8.1.4) on a published picture segment: two misses make the item
    /// irregular (L3 next time).
    async fn idr_check(&self, path: &Path, codec: Codec, k: u64) {
        use tokio::io::AsyncReadExt;
        let mut head = Vec::with_capacity(playlist::FIRST_PICTURE_BYTES);
        let Ok(f) = tokio::fs::File::open(path).await else { return };
        if f.take(playlist::FIRST_PICTURE_BYTES as u64).read_to_end(&mut head).await.is_err() {
            return;
        }
        if playlist::first_picture_is_idr(&head, codec, self.layout.fmp4()) == Some(false) {
            tracing::debug!("{}: segment {k} doesn't start with an IDR", self.ctx.label);
            if self.violations.fetch_add(1, Ordering::Relaxed) + 1 >= 2 && !self.irregular_sent.swap(true, Ordering::Relaxed) {
                self.send(|generation| RelayEvent::Irregular { generation });
            }
        }
    }

    /// The caps of §8.4 for a run: the 8 GiB directory (published segments furthest from the TV
    /// go first, the ones around it and the run's stay), and the lead (paused while more than
    /// [`super::LEAD_MAX`] ahead of the segment the TV fetched last, until back under
    /// [`super::LEAD_RESUME`]; never while a request waits). Returns whether ffmpeg is paused.
    async fn caps(&self, child: &ffmpeg::Child, run: &RunCtx, suspended: bool) -> bool {
        let fetched = self.last_fetch.load(Ordering::Relaxed).checked_sub(1);
        let mut files = Vec::new();
        let mut total = 0u64;
        for dir in &self.rends {
            let Ok(mut rd) = tokio::fs::read_dir(dir).await else { continue };
            while let Ok(Some(e)) = rd.next_entry().await {
                let Ok(meta) = e.metadata().await else { continue };
                if !meta.is_file() {
                    continue;
                }
                total += meta.len();
                if let Some(File::Segment(n)) = e.file_name().to_str().and_then(super::server::served_file) {
                    files.push((n, e.path(), meta.len()));
                }
            }
        }
        if total > super::VOD_CAP {
            let near = fetched.unwrap_or(run.first);
            let keep = |n: u64| n + 2 >= near && n <= run.made().max(near) + 1;
            files.sort_by_key(|f| std::cmp::Reverse(f.0.abs_diff(near)));
            for (n, path, len) in files {
                if total <= super::VOD_CAP {
                    break;
                }
                if keep(n) {
                    continue;
                }
                if tokio::fs::remove_file(&path).await.is_ok() {
                    total -= len;
                }
            }
        }
        let full = total > super::VOD_CAP;
        let lead = fetched.map(|f| self.map.start(run.made()) - self.map.start(f + 1));
        let waiting = self.waiters.load(Ordering::Relaxed) > 0;
        let ahead = !waiting && lead.is_some_and(|s| s > if suspended { super::LEAD_RESUME } else { super::LEAD_MAX });
        let over = full || ahead;
        if over != suspended {
            match (over, full) {
                (true, true) => tracing::info!("{}: pausing production at the {} GiB cap", self.ctx.label, super::VOD_CAP >> 30),
                (true, false) => tracing::info!("{}: pausing production {:.0} s ahead of the TV", self.ctx.label, lead.unwrap_or_default()),
                (false, _) => tracing::info!("{}: resuming production", self.ctx.label),
            }
            child.suspend(over);
        }
        over
    }

    /// End production now, without waiting (the relay dropped without `stop`): the running run
    /// is told to kill ffmpeg (`kill_on_drop` covers the rest).
    pub fn abort(&self) {
        self.dead.store(true, Ordering::Relaxed);
        if let Ok(st) = self.runs.try_lock()
            && let Some((r, _)) = &st.run
        {
            r.stop.cancel();
        }
        self.changed.notify_waiters();
    }

    /// End production: the running run's ffmpeg is killed and gone.
    pub async fn shutdown(&self) {
        self.dead.store(true, Ordering::Relaxed);
        let run = self.runs.lock().await.run.take();
        if let Some((r, task)) = run {
            r.stop.cancel();
            let _ = task.await;
        }
        self.changed.notify_waiters();
    }

    /// The generation's supervisor (the relay's pipeline monitor): until the relay ends it, or
    /// the provider fails, which ends production and reports the feed closed (the controller
    /// reloads where the TV is).
    pub async fn monitor(self: Arc<Self>, stop: CancellationToken) {
        let failed = tokio::select! {
            biased;
            () = stop.cancelled() => false,
            () = self.ctx.feed.wait_failed() => true,
        };
        self.shutdown().await;
        if failed {
            tracing::info!("{}: the provider failed: production stops", self.ctx.label);
            self.ctx.feed.close(false).await;
        }
    }

    /// The published segments `from..from + n` of rendition `i` as a playlist (the multivariant
    /// playlist measures its bandwidths on them).
    pub fn published(&self, from: u64, n: u64) -> playlist::MediaPlaylist {
        let fmp4 = self.layout.fmp4();
        let segments = (from..(from + n).min(self.map.len()))
            .map(|k| playlist::Segment { seq: k, dur: self.map.dur(k), tags: Vec::new(), uri: File::Segment(k).name(fmp4) })
            .collect();
        playlist::MediaPlaylist { segments, ..playlist::MediaPlaylist::default() }
    }
}

impl std::fmt::Debug for OnDemand {
    /// The map's shape only (the spec holds the loopback URL's token).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnDemand")
            .field("segments", &self.map.len())
            .field("target", &self.map.target())
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

/// Counts a waiting request while it lives.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::Relaxed);
        Self(n)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn exists(p: &Path) -> bool {
    tokio::fs::metadata(p).await.is_ok()
}

/// Write `data` as `dir/name` all at once: a temporary name (never a served one), then a rename.
async fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, dir.join(name)).await
}

/// One line of the segment muxer's CSV list: `s<k>.<ext>,<start>,<end>`.
fn list_entry(line: &str) -> Option<(u64, f64, f64)> {
    let mut f = line.trim().split(',');
    let name = f.next()?;
    let k = name.strip_prefix('s')?.split('.').next()?.parse().ok()?;
    let start = f.next()?.trim().parse().ok()?;
    let end = f.next()?.trim().parse().ok()?;
    Some((k, start, end))
}

#[cfg(test)]
mod tests {
    use super::keyframes::{Container, Index, Keyframe};
    use super::*;

    fn index(pts: &[f64]) -> Index {
        let keyframes = pts.iter().map(|p| Keyframe { pts: *p, seek: Some(p + 0.2) }).collect();
        Index { container: Container::Mp4, keyframes, end: None }
    }

    /// Whole keyframe runs merged up to 6.5 s, the longest segment's rounding as the target, the
    /// seek of each cut's keyframe, and the item's own clock (the file's start subtracted).
    #[test]
    fn the_map_merges_keyframe_runs_and_pins_the_target() {
        // 2 s GOPs: 6 s segments
        let m = Map::from_index(&index(&(0..30).map(|k| f64::from(k) * 2.0).collect::<Vec<_>>()), 0.0, 60.0, 6.0).unwrap();
        assert_eq!(m.starts, (0..10).map(|k| f64::from(k) * 6.0).collect::<Vec<_>>());
        assert_eq!((m.target(), m.len(), m.total()), (6, 10, 60.0));
        // a film's irregular GOPs: a long one stands alone, short ones merge, never past 6.5 s
        let m = Map::from_index(&index(&[0.0, 1.32, 2.68, 4.12, 5.0, 6.6, 8.04, 21.0, 22.0, 23.5, 26.0]), 0.0, 30.0, 6.0).unwrap();
        assert_eq!(m.starts, [0.0, 5.0, 8.04, 21.0, 26.0]);
        assert_eq!(m.target(), 13, "the 12.96 s GOP");
        assert!((0..m.len()).all(|k| m.dur(k) <= 6.5 || k == 2), "{m:?}");
        assert_eq!(m.seek(1), Some(5.2));
        assert_eq!(m.seek(0), None, "a run from 0 doesn't seek");
        // the file's start (an MPEG-TS-born MKV starting at 1.4 s): its clock starts there
        let m = Map::from_index(&index(&[1.4, 3.4, 5.4, 7.4, 9.4]), 1.4, 10.0, 6.0).unwrap();
        assert_eq!(m.starts, [0.0, 6.0]);
        assert!((m.seek(1).unwrap() - 6.2).abs() < 1e-9);
        // keyframes too sparse: no map (Phase 1's guard converts such a source)
        assert_eq!(Map::from_index(&index(&[0.0, 40.0]), 0.0, 60.0, 6.0), Err("keyframes more than 30 s apart"));
        // a keyframe without a seek window is no cut point
        let mut i = index(&[0.0, 6.0, 12.0]);
        i.keyframes[1].seek = None;
        assert_eq!(Map::from_index(&i, 0.0, 18.0, 6.0).unwrap().starts, [0.0, 12.0]);
        assert!(Map::from_index(&index(&[0.0]), 0.0, f64::NAN, 6.0).is_err());
    }

    #[test]
    fn the_grid_cuts_every_step_and_the_end_absorbs_a_sliver() {
        let m = Map::grid(20.0, 6.0, 0.0).unwrap();
        assert_eq!(m.starts, [0.0, 6.0, 12.0]);
        assert_eq!((m.dur(2), m.target()), (8.0, 8), "20 − 12: not a 2 s sliver after 18");
        let m = Map::grid(21.5, 6.0, 0.0).unwrap();
        assert_eq!(m.starts, [0.0, 6.0, 12.0, 18.0]);
        assert_eq!(m.target(), 6);
        assert_eq!(Map::grid(2.0, 6.0, 0.0).unwrap().starts, [0.0]);
        assert_eq!(m.seek(2), Some(12.0), "converted runs start exactly at the cut");
        assert_eq!(m.segment_at(-1.0), 0);
        assert_eq!(m.segment_at(12.0), 2);
        assert_eq!(m.segment_at(99.0), 3);
        assert_eq!(m.span(3, 2), Some((6.0, 9.5)));
        assert_eq!(m.span(4, 0), None);
    }

    /// A run's arguments: cut points relative to its first segment, the forced keyframes on the
    /// item's clock, and where a run cut short by the argument limit stops publishing.
    #[test]
    fn a_runs_cut_points() {
        let m = Map::grid(60.0, 6.0, 0.0).unwrap();
        assert_eq!(m.cuts_from(7), ("6.000000,12.000000".into(), 10));
        assert_eq!(m.forced_from(7, 10), "42.000000,48.000000,54.000000");
        assert_eq!(m.cuts_from(9), (String::new(), 10), "the last segment: no cut");
        let long = Map::grid(6.0 * (MAX_CUTS as f64 + 10.0), 6.0, 0.0).unwrap();
        let (cuts, limit) = long.cuts_from(0);
        assert_eq!((cuts.split(',').count(), limit), (MAX_CUTS, MAX_CUTS as u64), "segment 3000 would run to the end");
        assert_eq!(long.cuts_from(9).1, long.len(), "from 9 on, the list reaches the end");
    }

    /// The sound leads the picture (a remux of a broadcast: the picture at 0.5 s): the playlist
    /// still starts at the item's start, no cut comes before the picture (its first keyframe is
    /// no cut point), and a run from 0 counts its cut points from the picture, as `segment.c`
    /// does (5.5 s is 5 s after it); a run from a later segment from that segment's keyframe.
    #[test]
    fn a_run_from_the_start_counts_from_the_picture() {
        let m = Map::from_index(&index(&[0.5, 1.82, 3.0, 5.5, 7.1, 9.0, 12.5]), 0.0, 14.0, 6.0).unwrap();
        assert_eq!(m.starts, [0.0, 5.5, 9.0]);
        assert_eq!(m.cuts_from(0).0, "5.000000,8.500000");
        assert_eq!(m.cuts_from(1).0, "3.500000");
        // a picture from the file's start: as before
        let m = Map::from_index(&index(&[0.0, 1.82, 3.0, 5.5, 7.1]), 0.0, 9.0, 6.0).unwrap();
        assert_eq!((m.starts.as_slice(), m.cuts_from(0).0.as_str()), ([0.0, 5.5].as_slice(), "5.500000"));
        // converted: the grid's first cut at least half a step after the picture, counted from it
        let g = Map::grid(30.0, 6.0, 3.5).unwrap();
        assert_eq!(g.starts, [0.0, 12.0, 18.0, 24.0]);
        assert_eq!(g.cuts_from(0).0, "8.500000,14.500000,20.500000");
        assert_eq!(Map::grid(30.0, 6.0, 0.5).unwrap().starts, [0.0, 6.0, 12.0, 18.0, 24.0]);
    }

    /// Jellyfin's rule at the run's pace: a fresh run (its first minute read at once) is waited
    /// for up to 24 s ahead; a run past its burst, reading at real time, only for what it reaches
    /// within a few seconds: a scrub 75 s past the TV restarts rather than wait half a minute.
    #[test]
    fn a_request_waits_only_for_a_run_that_gets_there_soon() {
        let m = Map::grid(600.0, 6.0, 0.0).unwrap();
        // just started at 0: 18–24 s is waited for, 30 s is beyond the look-ahead
        assert!(keeps(&m, 0, 0, m.len(), 0.0, 3));
        assert!(!keeps(&m, 0, 0, m.len(), 0.0, 5));
        // 100 s in, it has made 0–156 s (the burst, then 1×: its frontier is 160 s)
        let made = m.segment_at(156.0);
        assert!(keeps(&m, 0, made, m.len(), 100.0, made), "the next segment");
        assert!(keeps(&m, 0, made, m.len(), 100.0, made + 1), "the one after: 162–168 s, within 8 s");
        assert!(!keeps(&m, 0, made, m.len(), 100.0, made + 3), "174–180 s: 20 s of real time away");
        // behind what it made, past its limit, or no run of its own: never
        assert!(!keeps(&m, 0, made, m.len(), 100.0, made - 1));
        assert!(!keeps(&m, 0, made, made + 1, 100.0, made + 1));
        // a run restarted at 300 s has a fresh burst
        assert!(keeps(&m, 50, 50, m.len(), 1.0, 53));
    }

    #[test]
    fn the_vod_playlist_lists_every_segment() {
        let m = Map::from_index(&index(&[0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0]), 0.0, 13.5, 6.0).unwrap();
        assert_eq!(
            m.playlist(false, None, 0, None),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXTINF:6.000000,\ns0.ts\n#EXTINF:6.000000,\ns1.ts\n#EXTINF:1.500000,\ns2.ts\n#EXT-X-ENDLIST\n"
        );
        let resumed = m.playlist(true, Some("init_0.mp4"), 0, Some(7.25));
        assert!(resumed.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"), "{resumed}");
        assert!(
            resumed.contains(
                "#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-START:TIME-OFFSET=7.250,PRECISE=YES\n#EXT-X-MAP:URI=\"init_0.mp4\"\n#EXTINF:6.000000,\ns0.m4s\n"
            ),
            "{resumed}"
        );
        // a view from segment 1 (a TV that doesn't start where EXT-X-START says)
        let view = m.playlist(true, Some("init.mp4"), 1, None);
        assert!(view.contains("#EXT-X-MEDIA-SEQUENCE:1\n") && !view.contains("s0.m4s") && view.ends_with("s2.m4s\n#EXT-X-ENDLIST\n"), "{view}");
        assert_eq!(list_entry("s12.m4s,22.680000,28.040000"), Some((12, 22.68, 28.04)));
        assert_eq!(list_entry("garbage"), None);
    }
}
