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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use super::receiver::LoadReq;
use super::{CastErrorKind, CastHints, CastLevel, ReceiverKind};
use crate::config::{CastConvert, CastInterlaced};
use crate::model::Playable;

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
    // CAST-4: the LAN server, the token table, the current pipeline, the event channel.
    cfg: RelayConfig,
}

impl RelaySession {
    /// Bind the LAN server (`cfg.bind:cfg.port`) for `cfg.peer`.
    pub async fn start(cfg: RelayConfig) -> Result<Self, CastErrorKind> {
        // CAST-4
        let _ = cfg;
        Err(CastErrorKind::Unsupported)
    }

    pub fn config(&self) -> &RelayConfig {
        &self.cfg
    }

    /// Stop the previous generation (waiting ≤ 1.5 s for its upstream to close), mint a token,
    /// plan, probe if needed, start ffmpeg, and return once `min_segments` are ready (or ENDLIST).
    pub async fn load(&mut self, _spec: LoadSpec) -> Result<Prepared, CastErrorKind> {
        // CAST-4
        Err(CastErrorKind::Unsupported)
    }

    /// A playlist view starting at the segment that holds `at_s` (seconds into the item) under a
    /// new token, when it is inside what's produced; `Ok(None)` when it isn't (the caller loads
    /// again with `start_at`, or clamps catch-up).
    pub async fn view(&mut self, _generation: u64, _at_s: f64) -> Result<Option<Prepared>, CastErrorKind> {
        // CAST-4
        Ok(None)
    }

    /// Test on TV: the lavfi test card (15 s, "Tuner" label) through L3 into the same server; no
    /// provider connection.
    pub async fn test_card(&mut self, _generation: u64) -> Result<Prepared, CastErrorKind> {
        // CAST-4
        Err(CastErrorKind::Unsupported)
    }

    /// The next event (cancel-safe: use it in `select!`).
    pub async fn next_event(&mut self) -> RelayEvent {
        // CAST-4
        std::future::pending().await
    }

    /// Kill the children, close the listener, revoke the tokens, wipe the session directory.
    /// `fast`: shutdown's path (no waiting for the upstream close).
    pub async fn stop(&mut self, _fast: bool) {
        // CAST-4
    }
}

/// `ffmpeg` and `ffprobe` are runnable and have `libx264` and `aac` (checked once per run).
/// `Err(NeedsFfmpeg)` when the CLI is missing; `Ok(false)` when L3 can't run (an encoder is
/// missing: the UI says converting is off).
pub async fn check_ffmpeg() -> Result<bool, CastErrorKind> {
    // CAST-4
    Err(CastErrorKind::NeedsFfmpeg)
}
