//! The relay and media pipeline (DESIGN §40 §8, §9.1–9.3): Tuner fetches the stream itself (the
//! TV never gets a provider URL), ffmpeg makes HLS from a loopback URL, and a LAN server hands the
//! TV token URLs. The signatures here are frozen (CAST-0); CAST-4 owns the bodies and the
//! modules.
//!
//! ```text
//!  provider ──(reqwest, Playable.headers, A's lease)──▶ upstream proxy 127.0.0.1:P /u/<tok>
//!                                     ffprobe (once) ──┤
//!                                     ffmpeg ──────────┴──▶ session dir (tmpfs live / cache VOD)
//!  TV ◀── LAN server <lan-ip>:47800 /c/<tok>/… (token, peer check, uniform 404) ◀── playlist guard
//! ```
//! A session serves one receiver: [`RelaySession::start`] binds the LAN server, each
//! [`RelaySession::load`] is one generation (stop-before-start, a new token, a new pipeline),
//! and [`RelaySession::stop`] kills the children, closes the listener and wipes the directory.

pub mod ffmpeg;
pub mod plan;
pub mod playlist;
pub mod probe;
pub mod server;
pub mod token;
pub mod upstream;

use std::net::Ipv4Addr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use self::plan::{Segments, Video};
use self::playlist::{Codec, Guard};
use self::token::Token;
use super::receiver::LoadReq;
use super::{CastErrorKind, CastHints, CastLevel, ReceiverKind};
use crate::config::{CastConvert, CastInterlaced};
use crate::model::Playable;
use crate::redact::Secret;

/// Everything a session needs (from `[cast]`, the paths and the network toward the TV).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    /// This computer's address toward the TV (the UDP `connect()` trick): the only bind.
    pub bind: Ipv4Addr,
    /// The receiver's address: the only peer served (everyone else gets the same 404).
    pub peer: Ipv4Addr,
    /// `cast.media_port`.
    pub port: u16,
    /// `cast.min_segments`: segments ready before `load` returns.
    pub min_segments: u32,
    pub convert: CastConvert,
    pub interlaced: CastInterlaced,
    /// `AppPaths::cast_live_dir()` (tmpfs) and `cast_vod_dir()` (disk, capped at 8 GiB).
    pub live_dir: PathBuf,
    pub vod_dir: PathBuf,
    /// The User-Agent for providers whose playlist sets none.
    pub user_agent: String,
}

/// One load generation.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadSpec {
    /// The controller's load generation (echoed in every [`RelayEvent`]).
    pub generation: u64,
    pub playable: Arc<Playable>,
    pub hints: CastHints,
    /// The receiver's profile (what it plays without converting).
    pub receiver: ReceiverKind,
    /// Force a level: the user's "Convert this channel", a cached hint, or the session's one-step
    /// escalation (L2 → L3).
    pub force: Option<CastLevel>,
}

/// A generation ready for the TV: its URL (a bearer token: `Secret`), whether it is a live
/// window, where it starts in the item, and the level chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub load: LoadReq,
    pub level: CastLevel,
    /// Live: how far behind the live point the TV will run (hold-back + ready segments), for
    /// the casting card's "about 15 s behind"; `None` for EVENT playlists.
    pub behind_live_s: Option<u16>,
}

/// What the relay reports. The `Upstream*`, `Http` and `ProviderText` events become the same
/// `state::Input`s mpv produces today (DESIGN §40 §11.2), so the reconnect policy works as is.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    UpstreamOpened {
        generation: u64,
    },
    /// The upstream connection closed (the controller releases the lease and records when).
    UpstreamClosed {
        generation: u64,
        at: Instant,
    },
    /// A real HTTP status from the provider.
    Http {
        generation: u64,
        status: u16,
    },
    /// The provider's text where the stream should be (redacted).
    ProviderText {
        generation: u64,
        text: String,
    },
    /// The upstream ended (EOF).
    UpstreamEof {
        generation: u64,
    },
    /// ffprobe's view of the input (the info panel while casting; the hints cached in
    /// `cast_hint`).
    Probed {
        generation: u64,
        info: Box<crate::player::tracks::StreamInfo>,
    },
    /// The TV fetched the playlist (the 10 s firewall watchdog's evidence).
    PlaylistFetched {
        generation: u64,
    },
    /// The TV fetched media up to `upto_s` of the current playlist (position bound,
    /// `actionAtItemEnd` re-arm).
    SegmentFetched {
        generation: u64,
        upto_s: f64,
    },
    /// The playlist guard tripped twice: mark the item irregular (→ L3 next time).
    Irregular {
        generation: u64,
    },
    /// Converting runs below 0.98× for 20 s.
    CpuTooSlow {
        generation: u64,
    },
    /// The pipeline failed (ffmpeg missing, unsupported input, …).
    Failed {
        generation: u64,
        kind: CastErrorKind,
    },
}

/// One cast session's relay.
pub struct RelaySession {
    cfg: RelayConfig,
    /// `cast session <4 hex>`: how logs name the session (§9.2), never an address or URL.
    label: Arc<str>,
    /// The session's directory name under the live and VOD roots.
    sid: String,
    tx: mpsc::UnboundedSender<RelayEvent>,
    rx: mpsc::UnboundedReceiver<RelayEvent>,
    lan: Option<server::LanServer>,
    proxy: Option<upstream::Proxy>,
    current: Option<Pipeline>,
    /// Pipelines started (each gets its own directory; a view shares its pipeline's).
    started: u64,
}

/// The generation now producing.
struct Pipeline {
    generation: Arc<AtomicU64>,
    live: bool,
    level: CastLevel,
    /// Seconds into the item where ffmpeg started (`-ss`).
    offset_s: f64,
    out: Arc<server::Output>,
    feed: Option<Arc<upstream::Feed>>,
    /// Ends the monitor quietly (it kills ffmpeg first).
    stop: CancellationToken,
    monitor: JoinHandle<()>,
}

/// The monitor's view of a pipeline: loading → ready, or loading → ended (load decides).
const LOADING: u8 = 0;
const READY: u8 = 1;
const ENDED: u8 = 2;

/// No first segments this long after the start: the load budget of §5.5 (foxCast: 30 s).
const LOAD_BUDGET: Duration = Duration::from_secs(30);
/// ffmpeg must have asked the proxy for its input by then, else the attempt restarts (§8.1.1).
const ATTACH_WITHIN: Duration = Duration::from_secs(10);
/// How often the readiness wait looks at ffmpeg's playlist.
const READY_POLL: Duration = Duration::from_millis(200);
/// The VOD directory's cap (§8.4) and how often it is checked.
const VOD_CAP: u64 = 8 << 30;
const VOD_CAP_POLL: Duration = Duration::from_secs(5);

/// How a launch failed (before the TV was given anything).
enum Fail {
    /// The provider failed: its events (`Http` / `ProviderText`, then `UpstreamClosed`) tell.
    Upstream,
    /// ffmpeg exited without producing (the re-probe path).
    Exited,
    Kind(CastErrorKind),
}

/// What [`RelaySession::launch`] needs besides the argv.
struct Launch {
    argv: Vec<String>,
    live: bool,
    level: CastLevel,
    offset_s: f64,
    dir: PathBuf,
    fmp4: bool,
    codec: Option<Codec>,
    hls_time: f64,
    gop: Option<f64>,
    feed: Option<Arc<upstream::Feed>>,
    generation: Arc<AtomicU64>,
    watch_speed: bool,
}

/// The `Err` of [`RelaySession::load`] when the *provider* failed: the events are already queued
/// (see `load`), and `Unreachable` is the kind whose reconnect (one retry) is harmless if a
/// caller does turn it into a failure.
const UPSTREAM_FAILED: CastErrorKind = CastErrorKind::Unreachable;

impl RelaySession {
    /// Bind the LAN server (`cfg.bind:cfg.port`) for `cfg.peer`.
    pub async fn start(cfg: RelayConfig) -> Result<Self, CastErrorKind> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut id = [0u8; 2];
        let _ = aws_lc_rs::rand::SecureRandom::fill(&aws_lc_rs::rand::SystemRandom::new(), &mut id);
        let sid = format!("{:02x}{:02x}", id[0], id[1]);
        let label: Arc<str> = format!("cast session {sid}").into();
        let lan = server::LanServer::bind(cfg.bind, cfg.port, cfg.peer, tx.clone(), label.clone()).await.map_err(|e| {
            // no address at WARN (§9.3): the port says enough
            tracing::warn!("{label}: can't listen on port {} toward the TV: {e}", cfg.port);
            CastErrorKind::Unreachable
        })?;
        let proxy = upstream::Proxy::start().await.map_err(|e| {
            tracing::warn!("{label}: can't start the loopback proxy: {e}");
            CastErrorKind::Unreachable
        })?;
        tracing::debug!("{label}: listening on port {}", lan.addr().port());
        Ok(Self { cfg, label, sid, tx, rx, lan: Some(lan), proxy: Some(proxy), current: None, started: 0 })
    }

    pub fn config(&self) -> &RelayConfig {
        &self.cfg
    }

    /// Stop the previous generation (waiting ≤ 1.5 s for its upstream to close), mint a token,
    /// plan, probe if needed, start ffmpeg, and return once `min_segments` are ready (or ENDLIST).
    ///
    /// `Err` kinds: `NeedsFfmpeg` (the CLI or, when converting is needed, an encoder is
    /// missing), `Unsupported` (DRM, a format the receiver can't take with converting off, or
    /// input ffmpeg can't read), `CpuTooSlow` (converting didn't produce the first segments in
    /// time). A *provider* failure (an error status, its prose, a dead connection) is not a cast
    /// failure (§3.4 "Upstream errors: unchanged"): its events are already queued (`Http` or
    /// `ProviderText`, then `UpstreamClosed`), so the controller's reconnect policy acts on them as
    /// it does for mpv, and `load` returns `Err(Unreachable)` with nothing more to add.
    pub async fn load(&mut self, spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        self.end_current(false).await;
        let caps = ffmpeg::caps().await?;
        let p = spec.playable.clone();
        let live = p.kind.is_live();
        let offset_s = if live { 0.0 } else { p.start_at.filter(|s| *s > 0.0).unwrap_or(0.0) };
        let generation = Arc::new(AtomicU64::new(spec.generation));
        let ctx = upstream::FeedCtx { generation: generation.clone(), tx: self.tx.clone(), label: self.label.clone() };
        let pick = upstream::Pick { profile: plan::profile(spec.receiver), height: spec.hints.height };
        let proxy = self.proxy.as_ref().ok_or(CastErrorKind::Unreachable)?;
        let feed = proxy.open(ctx, &p.url, &p.headers, &self.cfg.user_agent, pick).await;
        if feed.failed() {
            self.drop_feed(&feed).await;
            return Err(UPSTREAM_FAILED);
        }
        let loopback = feed.loopback_url().expose().clone();

        let mut probed = None;
        if !plan::hints_suffice(&spec.hints) {
            match self.probe(&caps, &feed, &loopback, spec.generation).await {
                Ok(pr) => probed = Some(pr),
                Err(kind) => {
                    self.drop_feed(&feed).await;
                    return Err(kind);
                }
            }
        }
        loop {
            let mut facts = probed.as_ref().map_or_else(|| plan::Facts::from_hints(&spec.hints), |pr| pr.facts(&spec.hints));
            facts.encrypted |= feed.encrypted();
            let opts = plan::Options { convert: self.cfg.convert, interlaced: self.cfg.interlaced, force: spec.force, can_convert: caps.l3 };
            let plan = match plan::plan(spec.receiver, &facts, opts) {
                Ok(plan) => plan,
                Err(kind) => {
                    tracing::info!("{}: {:?} can't go to the TV: {kind:?}", self.label, facts.vcodec);
                    self.drop_feed(&feed).await;
                    return Err(kind);
                }
            };
            let gop = probed.as_ref().and_then(|pr| pr.max_gop_s);
            // copy mode cuts on the source's keyframes: never shorter than its GOP (§8.4);
            // converting forces 2 s GOPs
            let hls_time = match plan.video {
                Video::Copy => ffmpeg::segment_s().max(gop.unwrap_or(0.0)),
                _ => ffmpeg::segment_s(),
            };
            let codec = match (plan.video, facts.vcodec.as_deref()) {
                (Video::Encode { .. }, _) | (Video::Copy, Some("h264")) => Some(Codec::H264),
                (Video::Copy, Some("hevc")) => Some(Codec::Hevc),
                _ => None,
            };
            let dir = self.new_dir(live)?;
            let input = ffmpeg::Input::Proxy { url: &loopback, hls: feed.is_hls(), seek_s: offset_s, readrate: !live };
            let out = ffmpeg::Output { dir: &dir, live, hls_time, fps: facts.fps, audio_index: facts.audio_index };
            let argv = ffmpeg::argv(&input, &plan, &out);
            tracing::info!(
                "{}: {} {} {}{}",
                self.label,
                level_name(plan.level),
                facts.vcodec.as_deref().unwrap_or("no video"),
                facts.acodec.as_deref().unwrap_or("no audio"),
                if live { " (live)" } else { "" }
            );
            let launch = Launch {
                argv,
                live,
                level: plan.level,
                offset_s,
                dir: dir.clone(),
                fmp4: plan.segments == Segments::Fmp4,
                codec,
                hls_time,
                gop,
                feed: Some(feed.clone()),
                generation: generation.clone(),
                watch_speed: live && plan.level == CastLevel::Converted,
            };
            match self.launch(&caps, launch).await {
                Ok(prepared) => return Ok(prepared),
                // a failure with the hints alone re-probes once (§8.1.2)
                Err(Fail::Exited) if probed.is_none() && !feed.failed() => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    tracing::info!("{}: ffmpeg failed with the cached hints: probing", self.label);
                    match self.probe(&caps, &feed, &loopback, spec.generation).await {
                        Ok(pr) => probed = Some(pr),
                        Err(kind) => {
                            self.drop_feed(&feed).await;
                            return Err(kind);
                        }
                    }
                }
                Err(fail) => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    self.drop_feed(&feed).await;
                    return Err(match fail {
                        Fail::Upstream => UPSTREAM_FAILED,
                        Fail::Exited => CastErrorKind::Unsupported,
                        Fail::Kind(k) => k,
                    });
                }
            }
        }
    }

    /// ffprobe on the head buffer: `Probed` for the info panel; a provider failure is
    /// [`UPSTREAM_FAILED`], an unreadable input `Unsupported`.
    async fn probe(&self, caps: &ffmpeg::Caps, feed: &upstream::Feed, url: &str, generation: u64) -> Result<probe::Probe, CastErrorKind> {
        match probe::probe(&caps.ffprobe, url).await {
            Ok(pr) => {
                let _ = self.tx.send(RelayEvent::Probed { generation, info: Box::new(pr.stream_info()) });
                Ok(pr)
            }
            Err(_) if feed.failed() => Err(UPSTREAM_FAILED),
            Err(probe::ProbeError::Spawn) => Err(CastErrorKind::NeedsFfmpeg),
            Err(probe::ProbeError::Failed) => {
                tracing::info!("{}: ffprobe can't read the stream", self.label);
                Err(CastErrorKind::Unsupported)
            }
        }
    }

    /// End a feed that never reached a pipeline (its `UpstreamClosed` releases the lease).
    async fn drop_feed(&self, feed: &Arc<upstream::Feed>) {
        if let Some(proxy) = &self.proxy {
            proxy.forget(feed);
        }
        feed.close(false).await;
    }

    /// A fresh 0700 directory for one pipeline: tmpfs for a live window, the cache for EVENT
    /// playlists (a film doesn't fit in tmpfs).
    fn new_dir(&mut self, live: bool) -> Result<PathBuf, CastErrorKind> {
        self.started += 1;
        let root = if live { &self.cfg.live_dir } else { &self.cfg.vod_dir };
        let dir = root.join(&self.sid).join(format!("p{}", self.started));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| {
            tracing::warn!("{}: can't create the HLS directory: {e}", self.label);
            CastErrorKind::Unsupported
        })?;
        Ok(dir)
    }

    /// Start ffmpeg and wait for the first segments (§8.4 `min_segments`, VioletRelay's
    /// `wait_until_ready(segments=2)`), then hand the TV a token. One restart when ffmpeg never
    /// asks the proxy for its input.
    async fn launch(&mut self, caps: &ffmpeg::Caps, l: Launch) -> Result<Prepared, Fail> {
        let min = self.cfg.min_segments.max(1) as usize;
        'attempt: for attempt in 0..2 {
            if let Some(f) = &l.feed {
                f.arm_final_reader();
            }
            let tx = self.tx.clone();
            let generation = l.generation.clone();
            let label = self.label.clone();
            let progress = ffmpeg::Progress {
                watch_speed: l.watch_speed,
                on_slow: Box::new(move || {
                    tracing::warn!("{label}: converting runs slower than real time");
                    let _ = tx.send(RelayEvent::CpuTooSlow { generation: generation.load(Ordering::Relaxed) });
                }),
            };
            let child = match ffmpeg::spawn(&caps.ffmpeg, &l.argv, self.label.clone(), progress) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("{}: ffmpeg didn't start: {e}", self.label);
                    return Err(Fail::Kind(CastErrorKind::NeedsFfmpeg));
                }
            };
            let out = Arc::new(server::Output::new(l.dir.clone(), l.fmp4, l.generation.clone(), Guard::new(1, l.codec), l.codec));
            let phase = Arc::new(AtomicU8::new(LOADING));
            let stop = CancellationToken::new();
            let (exit_tx, mut exit_rx) = watch::channel(None);
            let monitor = tokio::spawn(monitor(
                child,
                Watch {
                    feed: l.feed.clone(),
                    phase: phase.clone(),
                    stop: stop.clone(),
                    exit: exit_tx,
                    tx: self.tx.clone(),
                    generation: l.generation.clone(),
                    live: l.live,
                    out: out.clone(),
                    label: self.label.clone(),
                },
            ));
            let quit = |stop: CancellationToken, monitor: JoinHandle<()>| async move {
                stop.cancel();
                let _ = monitor.await;
            };
            let started = Instant::now();
            let mut tick = tokio::time::interval(READY_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let ready = loop {
                tokio::select! {
                    _ = exit_rx.changed() => {
                        let exit = *exit_rx.borrow();
                        // a short stream that ended: what's there is all there will be
                        if exit == Some(ffmpeg::Exit::Done) && let Some(pl) = read_playlist(&l.dir).await && !pl.segments.is_empty() {
                            break pl;
                        }
                        quit(stop, monitor).await;
                        return Err(if l.feed.as_ref().is_some_and(|f| f.failed()) { Fail::Upstream } else { Fail::Exited });
                    }
                    _ = tick.tick() => {
                        if let Some(pl) = read_playlist(&l.dir).await && (pl.segments.len() >= min || pl.endlist) {
                            break pl;
                        }
                        if l.feed.as_ref().is_some_and(|f| !f.final_attached()) && started.elapsed() >= ATTACH_WITHIN {
                            tracing::info!("{}: ffmpeg didn't open its input within {ATTACH_WITHIN:?}", self.label);
                            quit(stop, monitor).await;
                            if attempt == 0 {
                                continue 'attempt;
                            }
                            return Err(Fail::Exited);
                        }
                        if started.elapsed() >= LOAD_BUDGET {
                            quit(stop, monitor).await;
                            return Err(if l.feed.as_ref().is_some_and(|f| f.failed()) {
                                Fail::Upstream
                            } else if l.level == CastLevel::Converted {
                                Fail::Kind(CastErrorKind::CpuTooSlow)
                            } else {
                                // copy mode without two segments in 30 s: the provider stalls
                                Fail::Upstream
                            });
                        }
                    }
                }
            };
            // ready: pin the target duration before the TV reads anything (§8.1.4)
            let target = playlist::pin(l.hls_time, l.gop, &ready);
            *out.guard.lock().unwrap_or_else(|e| e.into_inner()) = Guard::new(target, l.codec);
            let ended = phase.compare_exchange(LOADING, READY, Ordering::AcqRel, Ordering::Acquire).is_err();
            if ended {
                // ffmpeg ended before the TV got anything: a short stream is what it is
                if *exit_rx.borrow() != Some(ffmpeg::Exit::Done) {
                    quit(stop, monitor).await;
                    return Err(if l.feed.as_ref().is_some_and(|f| f.failed()) { Fail::Upstream } else { Fail::Exited });
                }
            }
            let token = Token::mint();
            let lan = self.lan.as_ref().ok_or(Fail::Kind(CastErrorKind::Unreachable))?;
            let url = format!("http://{}/c/{}/index.m3u8", lan.addr(), token.expose());
            lan.grant_only(token, out.clone(), None);
            if let Ok(text) = tokio::fs::read_to_string(l.dir.join("index.m3u8")).await {
                out.serve_playlist(&text, None); // the first segments' IDR check
            }
            self.current = Some(Pipeline {
                generation: l.generation.clone(),
                live: l.live,
                level: l.level,
                offset_s: l.offset_s,
                out,
                feed: l.feed.clone(),
                stop,
                monitor,
            });
            let behind_live_s = l.live.then(|| (3.0 * f64::from(target) + l.hls_time).round().min(f64::from(u16::MAX)) as u16);
            tracing::debug!("{}: ready, target duration {target} s", self.label);
            if ended && let Some(feed) = &l.feed {
                // the monitor left this to us: the input's end, then the close
                let _ = self.tx.send(RelayEvent::UpstreamEof { generation: l.generation.load(Ordering::Relaxed) });
                feed.close(false).await;
            }
            return Ok(Prepared { load: LoadReq { url: Secret::new(url), live: l.live, offset_s: l.offset_s }, level: l.level, behind_live_s });
        }
        Err(Fail::Exited)
    }

    /// A playlist view starting at the segment that holds `at_s` (seconds into the item) under a
    /// new token, when it is inside what's produced; `Ok(None)` when it isn't (the caller loads
    /// again with `start_at`, or clamps catch-up).
    pub async fn view(&mut self, generation: u64, at_s: f64) -> Result<Option<Prepared>, CastErrorKind> {
        let Some(p) = &self.current else { return Ok(None) };
        if p.live {
            return Ok(None);
        }
        let rel = at_s - p.offset_s;
        if rel < 0.0 || !rel.is_finite() {
            return Ok(None);
        }
        // what ffmpeg has written by now, not only what the TV fetched
        if let Ok(text) = tokio::fs::read_to_string(p.out.dir.join("index.m3u8")).await {
            p.out.serve_playlist(&text, None);
        }
        let Some((seq, start)) = p.out.guard.lock().unwrap_or_else(|e| e.into_inner()).segment_at(rel) else { return Ok(None) };
        let ext = if p.out.fmp4 { "m4s" } else { "ts" };
        if tokio::fs::metadata(p.out.dir.join(format!("s{seq}.{ext}"))).await.is_err() {
            return Ok(None); // deleted by the VOD cap: restart there instead
        }
        p.generation.store(generation, Ordering::Relaxed);
        let lan = self.lan.as_ref().ok_or(CastErrorKind::Unreachable)?;
        let token = Token::mint();
        let url = format!("http://{}/c/{}/index.m3u8", lan.addr(), token.expose());
        lan.grant_only(token, p.out.clone(), Some(seq));
        tracing::debug!("{}: view from segment {seq} ({start:.1} s)", self.label);
        Ok(Some(Prepared { load: LoadReq { url: Secret::new(url), live: false, offset_s: p.offset_s + start }, level: p.level, behind_live_s: None }))
    }

    /// Seconds into the item where the last segment ffmpeg has written starts: how far a
    /// catch-up seek can go (§8.5: beyond, it clamps here). `None` for live, or before the
    /// first segment.
    pub async fn produced_s(&mut self) -> Option<f64> {
        let p = self.current.as_ref().filter(|p| !p.live)?;
        // what ffmpeg has written by now, not only what the TV fetched (as `view`)
        if let Ok(text) = tokio::fs::read_to_string(p.out.dir.join("index.m3u8")).await {
            p.out.serve_playlist(&text, None);
        }
        let guard = p.out.guard.lock().unwrap_or_else(|e| e.into_inner());
        Some(p.offset_s + guard.start_of(guard.last_seq()?)?)
    }

    /// Test on TV: the lavfi test card (15 s, "Tuner" label) through L3 into the same server; no
    /// provider connection.
    pub async fn test_card(&mut self, generation: u64) -> Result<Prepared, CastErrorKind> {
        self.end_current(false).await;
        let caps = ffmpeg::caps().await?;
        if !caps.l3 {
            return Err(CastErrorKind::NeedsFfmpeg);
        }
        let plan =
            plan::Plan { level: CastLevel::Converted, video: Video::Encode { deinterlace: false }, audio: plan::Audio::Aac, segments: Segments::Ts };
        let dir = self.new_dir(true)?;
        let hls_time = ffmpeg::segment_s();
        let argv = ffmpeg::argv(
            &ffmpeg::Input::TestCard { label: caps.drawtext },
            &plan,
            &ffmpeg::Output { dir: &dir, live: false, hls_time, fps: Some(25.0), audio_index: None },
        );
        let launch = Launch {
            argv,
            live: false,
            level: CastLevel::Converted,
            offset_s: 0.0,
            dir: dir.clone(),
            fmp4: false,
            codec: Some(Codec::H264),
            hls_time,
            gop: Some(2.0),
            feed: None,
            generation: Arc::new(AtomicU64::new(generation)),
            watch_speed: false,
        };
        match self.launch(&caps, launch).await {
            Ok(p) => Ok(p),
            Err(fail) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(match fail {
                    Fail::Kind(k) => k,
                    Fail::Upstream | Fail::Exited => CastErrorKind::NeedsFfmpeg,
                })
            }
        }
    }

    /// The next event (cancel-safe: use it in `select!`).
    pub async fn next_event(&mut self) -> RelayEvent {
        match self.rx.recv().await {
            Some(ev) => ev,
            // unreachable: the session holds a sender
            None => std::future::pending().await,
        }
    }

    /// Kill the children, close the listener, revoke the tokens, wipe the session directory.
    /// `fast`: shutdown's path (no waiting for the upstream close).
    pub async fn stop(&mut self, fast: bool) {
        self.end_current(fast).await;
        if let Some(lan) = self.lan.take() {
            lan.shutdown();
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.shutdown();
        }
        self.wipe();
    }

    /// Stop-before-start (§8.5): the monitor kills ffmpeg, the feed closes (≤ 1.5 s for the
    /// upstream connection unless `fast`) and reports `UpstreamClosed`, the token goes, and so
    /// does the pipeline's directory.
    async fn end_current(&mut self, fast: bool) {
        let Some(p) = self.current.take() else { return };
        p.stop.cancel();
        if let Some(lan) = &self.lan {
            lan.revoke_all();
        }
        if !fast {
            let _ = tokio::time::timeout(Duration::from_secs(2), p.monitor).await;
        }
        if let Some(feed) = &p.feed {
            if let Some(proxy) = &self.proxy {
                proxy.forget(feed);
            }
            feed.close(fast).await;
        }
        if fast {
            let _ = std::fs::remove_dir_all(&p.out.dir);
        } else {
            let _ = tokio::fs::remove_dir_all(&p.out.dir).await;
        }
    }

    /// Remove the session's directories (both roots).
    fn wipe(&self) {
        for root in [&self.cfg.live_dir, &self.cfg.vod_dir] {
            let _ = std::fs::remove_dir_all(root.join(&self.sid));
        }
    }
}

impl Drop for RelaySession {
    /// Dropped without `stop` (a panic, the runtime going down): kill and wipe what can be
    /// reached synchronously; `kill_on_drop` and `PR_SET_PDEATHSIG` cover the children.
    fn drop(&mut self) {
        if let Some(p) = self.current.take() {
            p.stop.cancel();
            p.monitor.abort();
            if let Some(feed) = &p.feed {
                feed.cancel_all();
            }
        }
        if let Some(lan) = self.lan.take() {
            lan.shutdown();
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.shutdown();
        }
        self.wipe();
    }
}

/// ffmpeg's playlist, when it has written one.
async fn read_playlist(dir: &Path) -> Option<playlist::MediaPlaylist> {
    tokio::fs::read_to_string(dir.join("index.m3u8")).await.ok().map(|t| playlist::parse(&t))
}

fn level_name(l: CastLevel) -> &'static str {
    match l {
        CastLevel::Original => "L1 original",
        CastLevel::Repackaged => "L2 repackaged",
        CastLevel::Converted => "L3 converted",
    }
}

/// What the monitor of one pipeline holds.
struct Watch {
    feed: Option<Arc<upstream::Feed>>,
    phase: Arc<AtomicU8>,
    stop: CancellationToken,
    exit: watch::Sender<Option<ffmpeg::Exit>>,
    tx: mpsc::UnboundedSender<RelayEvent>,
    generation: Arc<AtomicU64>,
    live: bool,
    out: Arc<server::Output>,
    label: Arc<str>,
}

/// One pipeline's supervisor: ffmpeg's exit, the feed failing (ffmpeg is killed: the
/// generation is over), the VOD cap. Before the TV has the pipeline (`LOADING`), `load` decides
/// what an exit means; after (`READY`), the monitor reports it: `UpstreamEof` for a clean end,
/// then the feed's `UpstreamClosed` either way (the `Http` of a provider failure is already out).
async fn monitor(mut child: ffmpeg::Child, w: Watch) {
    let mut cap = tokio::time::interval(VOD_CAP_POLL);
    cap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut suspended = false;
    let failed = async {
        match &w.feed {
            Some(f) => f.wait_failed().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(failed);
    let exit = loop {
        tokio::select! {
            biased;
            () = w.stop.cancelled() => {
                child.kill().await;
                let _ = w.exit.send(Some(ffmpeg::Exit::Killed));
                return;
            }
            e = child.wait() => break e,
            () = &mut failed => {
                child.kill().await;
                break ffmpeg::Exit::Killed;
            }
            _ = cap.tick(), if !w.live => {
                suspended = vod_cap(&w.out, &child, suspended, &w.label).await;
            }
        }
    };
    let _ = w.exit.send(Some(exit));
    if w.phase.compare_exchange(LOADING, ENDED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        return;
    }
    let Some(feed) = &w.feed else { return };
    match exit {
        ffmpeg::Exit::Done if !feed.failed() => {
            tracing::debug!("{}: the input ended", w.label);
            let _ = w.tx.send(RelayEvent::UpstreamEof { generation: w.generation.load(Ordering::Relaxed) });
        }
        ffmpeg::Exit::Failed if !feed.failed() => tracing::warn!("{}: ffmpeg stopped with an error", w.label),
        _ => {}
    }
    feed.close(false).await;
}

/// The VOD directory's 8 GiB cap (§8.4): delete segments the TV has already fetched (oldest
/// first, keeping two behind it); if that isn't enough, pause ffmpeg until it is. Returns
/// whether ffmpeg is paused.
async fn vod_cap(out: &server::Output, child: &ffmpeg::Child, suspended: bool, label: &str) -> bool {
    let mut files = Vec::new();
    let mut total = 0u64;
    if let Ok(mut rd) = tokio::fs::read_dir(&out.dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(meta) = e.metadata().await else { continue };
            total += meta.len();
            if let Some(server::File::Segment(n)) = e.file_name().to_str().and_then(server::served_file) {
                files.push((n, e.path(), meta.len()));
            }
        }
    }
    if total > VOD_CAP {
        let behind = out.fetched.load(Ordering::Relaxed).saturating_sub(2);
        files.sort_by_key(|f| f.0);
        for (n, path, len) in files {
            if total <= VOD_CAP || n >= behind {
                break;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total -= len;
            }
        }
    }
    let over = total > VOD_CAP;
    if over != suspended {
        tracing::info!("{label}: {} production at the {} GiB cap", if over { "pausing" } else { "resuming" }, VOD_CAP >> 30);
        child.suspend(over);
    }
    over
}

/// `ffmpeg` and `ffprobe` are runnable and have `libx264` and `aac` (checked once per run).
/// `Err(NeedsFfmpeg)` when the CLI is missing; `Ok(false)` when L3 can't run (an encoder is
/// missing: the UI says converting is off).
pub async fn check_ffmpeg() -> Result<bool, CastErrorKind> {
    ffmpeg::caps().await.map(|c| c.l3)
}
