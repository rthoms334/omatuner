//! The AirPlay 2 video client (DESIGN §40 §7): native Rust, ported from proven open-source
//! senders (pyatv, MIT, ported faithfully; foxCast, VioletRelay, castcli, doubletake and LIVI for
//! protocol facts only — §7.0). One code path for tvOS 26 and 27 (§7.6): nothing here branches on
//! the TV's version.
//!
//! The declarations and signatures in this file are frozen (CAST-0); CAST-2 owns the bodies.
//! OWNERS of the modules:
//! * CAST-1 (crypto and codecs): `tlv8`, `srp`, `hap`, `pair`, `bplist`, `features`.
//! * CAST-2 (session): `rtsp` (HTTP/RTSP mux on one connection, one request in flight, stray-CSeq
//!   guard), `session` (SETUP with declared PTP, RECORD / `skipRecord`, `/info`, type 130,
//!   feedback, TEARDOWN), `queue` (`/command` builders per profile, replace), `events` (event
//!   channel: keys, acks, reassembly, decode), `position` (`playbackInfo` + estimate), `trace`
//!   (`TUNER_CAST_TRACE` redacted transcript).
//!
//! Every ported file starts with `// Ported from pyatv <path> @ b277a4c8 (MIT, © 2020 Pierre
//! Ståhl).` and the notice is in `THIRD_PARTY_NOTICES.md`.

pub mod bplist;
pub mod events;
pub mod features;
pub mod hap;
pub mod pair;
pub mod position;
pub mod queue;
pub mod rtsp;
pub mod session;
pub mod srp;
pub mod tlv8;
pub mod trace;

use std::net::SocketAddr;

use super::receiver::{LoadReq, ReceiverEvent, ReceiverInfo};
use super::{CastErrorKind, Credentials, Identity, PairError, PairVariant, PeerCredentials, Pin};

/// The AirPlay control port (RTSP and HTTP on one TCP connection).
pub const CONTROL_PORT: u16 = 7000;
/// `X-Apple-Client-Name` on pin-start and every pair-setup (tvOS 27 shows no PIN without it).
pub const CLIENT_NAME: &str = "Tuner";

/// Which `/command` dialect a receiver gets (DESIGN §40 §7.3 for the Apple TV, §7.7 for
/// third-party TVs: the type-120 SETUP, `SETPEERSX`, stream-id header negotiation,
/// `HLS-Content-Location`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    AppleTv,
    ThirdParty,
}

/// One verified session with a TV (the session outlives zaps; a failed replace restarts it).
pub struct Client {
    // CAST-2: the control connection (`rtsp`), the event channel (`events`), feedback, position.
    info: ReceiverInfo,
}

impl Client {
    /// Pair-verify (encryption on at once), SETUP (declared PTP), the event channel, RECORD
    /// (skipped on `skipRecord`; errors non-fatal), `GET /info`, the type-130 SETUP.
    pub async fn connect(_addr: SocketAddr, _creds: &Credentials, _profile: Profile) -> Result<Self, CastErrorKind> {
        // CAST-2
        Err(CastErrorKind::Unsupported)
    }

    /// `insertPlayQueueItem` (fatal on non-2xx), `isInterestedInDateRange` and
    /// `actionAtItemEnd = 1` (non-fatal), `setRate 1` (fatal); then feedback every 2 s.
    pub async fn load(&mut self, _req: &LoadReq) -> Result<(), CastErrorKind> {
        // CAST-2
        Err(CastErrorKind::Unsupported)
    }

    /// VioletRelay's replace: `setRate 0`, `actionAtItemEnd 0`, insert after the old item,
    /// remove the old one, `setRate 1` (and `actionAtItemEnd 1` after the first fetch).
    pub async fn replace(&mut self, _req: &LoadReq) -> Result<(), CastErrorKind> {
        // CAST-2
        Err(CastErrorKind::Unsupported)
    }

    /// `setRate 0.0` / `1.0`.
    pub async fn set_paused(&mut self, _paused: bool) -> Result<(), CastErrorKind> {
        // CAST-2
        Err(CastErrorKind::Unsupported)
    }

    /// The TV fetched media up to `upto_s` of the current playlist.
    pub async fn fetched(&mut self, _upto_s: f64) {
        // CAST-2
    }

    /// `playbackInfo` (every 10 s while VOD plays, once before Play here / Stop), else the
    /// estimate. Relative to the current playlist.
    pub async fn position(&mut self) -> Option<f64> {
        // CAST-2
        None
    }

    /// The next decoded event-channel event (cancel-safe).
    pub async fn next_event(&mut self) -> ReceiverEvent {
        // CAST-2
        std::future::pending().await
    }

    /// `setRate 0`, TEARDOWN, close the event channel and the connection.
    pub async fn stop(&mut self) {
        // CAST-2
    }

    pub fn info(&self) -> ReceiverInfo {
        self.info.clone()
    }
}

/// One pairing (DESIGN §40 §7.2): `/pair-pin-start` and the whole pair-setup on ONE TCP
/// connection (the TV accepts the PIN only there), held open while the user types.
pub struct PairingSession {
    // CAST-2: the connection (`rtsp`) and the client state machine (`pair::PairSetup`).
    variant: PairVariant,
}

impl PairingSession {
    /// Connect and ask the TV to show a code with `variant`'s headers (200 or 453: success).
    pub async fn start(_addr: SocketAddr, _identity: &Identity, variant: PairVariant) -> Result<Self, PairError> {
        // CAST-2
        let _ = variant;
        Err(PairError::Unreachable)
    }

    pub fn variant(&self) -> PairVariant {
        self.variant
    }

    /// M1–M6 with `pin` (proof and signature verified, error TLVs first, unknown TLVs ignored).
    /// After `WrongPin` the TV shows a new code: the session stays usable.
    pub async fn finish(&mut self, _pin: &Pin) -> Result<PeerCredentials, PairError> {
        // CAST-2
        Err(PairError::Failed)
    }
}

/// `GET /info` (unauthenticated): "Add by address…" and the version line.
pub async fn probe_info(_addr: SocketAddr) -> Result<features::AirPlayInfo, CastErrorKind> {
    // CAST-2 (with `features::from_info`)
    Err(CastErrorKind::Unreachable)
}
