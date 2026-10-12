//! Remote output: the controller's side of casting (DESIGN §40 §5.1, §5.5, §11.2). Casting is an
//! output of picture A's controller, not a new player: `load_now_by` hands the `Playable` to a
//! [`RemoteOutput`] instead of `loadfile`, transport commands are forwarded, and receiver /
//! relay status comes back as [`RemoteEvent`]s that the controller turns into the existing
//! `state::Input`s, so `PlayerMachine` stays the single source of truth.
//!
//! The player defines this contract; `crate::cast` implements it (dependencies point from `cast`
//! to `player`, never back). OWNERS: the contract is frozen (CAST-0); CAST-5 adds the pure
//! `RemoteEvent → state::Input` adapter here and the controller's use of it.

use std::sync::Arc;
use std::time::Instant;

use super::{PlayerError, PlayerErrorKind};
use crate::model::{CastErrorKind, CastHints, CastLevel, CastPhase, CastView, DeviceId, Playable};

/// Opens cast sessions for the controller (`PlayerDeps::cast`). Implemented by the cast service
/// (through `AppShared`, so the service can start after the player and restart on settings
/// changes); tests inject fakes.
pub trait CastConnector: Send + Sync {
    /// Non-blocking: returns at once; the session connects on the runtime and reports `Phase` /
    /// `Lost`. `Err` when casting is off or the device is unknown or not paired.
    fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError>;
}

/// A session as the controller holds it while the output is remote.
#[derive(Debug)]
pub struct RemoteOutput {
    /// To the session task (never blocks the controller thread).
    pub cmd: tokio::sync::mpsc::UnboundedSender<RemoteCmd>,
    /// From the session (a fifth `select!` arm in `Controller::run`).
    pub events: crossbeam_channel::Receiver<RemoteEvent>,
    /// The initial view (phase `Connecting`) for `PlayerSnapshot::cast`.
    pub view: CastView,
}

/// Controller → session.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteCmd {
    /// Play `playable` as load generation `generation` (first load, zap, reconnect, VOD restart).
    Load {
        generation: u64,
        playable: Arc<Playable>,
        hints: CastHints,
    },
    SetPaused(bool),
    /// Absolute seconds into the item (a replace with a playlist view, or a restart).
    SeekTo(f64),
    /// 0–130 (Tuner's scale); AirPlay ignores it ("Use the TV remote for volume").
    SetVolume(f64),
    /// Stop playback on the TV (the session stays for the next `Load`).
    Stop,
    /// End the session; `fast` at shutdown (≤ 300 ms, no waiting for the upstream close).
    Close {
        fast: bool,
    },
}

/// Session → controller (DESIGN §40 §11.2 maps each to `state::Input`s).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    Phase(CastPhase),
    Level(CastLevel),
    UpstreamOpened {
        generation: u64,
    },
    /// The relay's upstream connection closed: the controller releases A's lease and records
    /// when (the 45 s race window covers relay reconnects too).
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
    /// The upstream ended (EOF): `EndFile { reason: Eof }`. Live: `StreamEnded`, then the
    /// reconnect policy; VOD: wait for the receiver's `Ended`.
    UpstreamEof {
        generation: u64,
    },
    /// The relay's probe of this generation (codec, size, fps, audio): the controller keeps it
    /// for `snapshot.stream_info` while the info panel is open (`RefreshStreamInfo`, §11.3).
    StreamInfo {
        generation: u64,
        info: Box<super::tracks::StreamInfo>,
    },
    /// Live: roughly how far the TV runs behind the live point (`CastView::behind_live_s`);
    /// `None` for catch-up and VOD.
    BehindLive(Option<u16>),
    /// The first is the first picture.
    Playing {
        generation: u64,
    },
    Paused(bool),
    Buffering(bool),
    /// Absolute (the session added the playlist's offset).
    Position {
        generation: u64,
        pos: f64,
        dur: Option<f64>,
    },
    /// The item played to its end (VOD: `Ended`, then Up next).
    Ended {
        generation: u64,
    },
    Failed {
        generation: u64,
        kind: PlayerErrorKind,
    },
    /// The session is gone; `StoppedOnTv` ends casting quietly.
    Lost(CastErrorKind),
}

// CAST-5: the pure `RemoteEvent → state::Input` adapter (first `Playing` → `FileLoaded` +
// `PlaybackRestart`, `Http` then the upstream end → `HttpStatus` + `EndFile{Error}`, …) and its
// unit tests.
