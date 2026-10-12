//! Receiver dispatch (DESIGN §40 §5.4): one enum over the protocol clients, no `dyn` async trait.
//! OWNER: CAST-0 (frozen). AirPlay is Phase 1; `GoogleCast` (Phase 2) and `Dlna` (Phase 3) join
//! as variants with the same methods. [`FakeReceiver`] is the scripted stand-in the session
//! orchestrator's tests use (CAST-6); the protocol-level fake TV is
//! `tests/support/fake_airplay.rs` (CAST-3).
//!
//! The TV always starts at 0 of the playlist it is given: a resume or a seek gives it a playlist
//! that starts there (`-ss` restart or a playlist view), and `LoadReq::offset_s` is added to the
//! positions it reports, so the player sees absolute positions.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::airplay;
use super::{CastErrorKind, Credentials, Device, PairVariant, Protocol, ReceiverKind};
use crate::redact::Secret;

/// What to play: the relay's LAN URL (a one-generation bearer token: a `Secret`).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadReq {
    /// `http://<lan-ip>:47800/c/<tok>/index.m3u8`.
    pub url: Secret<String>,
    /// A live sliding-window playlist (else EVENT: catch-up, VOD, the test card).
    pub live: bool,
    /// Seconds into the item where this playlist starts (added to reported positions).
    pub offset_s: f64,
}

/// What the receiver reports (positions are relative to the current playlist; the session adds
/// `offset_s`).
#[derive(Debug, Clone, PartialEq)]
pub enum ReceiverEvent {
    /// The item is loading (also during a replace: `playing → loading → playing`).
    Loading,
    /// Playing (the first one after a load is the first picture).
    Playing,
    Paused,
    Buffering(bool),
    Position {
        pos: f64,
        dur: Option<f64>,
    },
    /// The item played to its end (`itemPlayedToEnd`): VOD's `Ended`, then Up next.
    Ended,
    /// Stopped from the TV (its remote, or another sender took over): `StoppedOnTv`.
    StoppedOnTv,
    /// The session is gone (connection lost, bad frame, refused).
    Lost(CastErrorKind),
}

/// What a session learned about the receiver (one INFO line without addresses; `diagnose`;
/// Settings shows the version).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiverInfo {
    pub model: Option<String>,
    /// "26.6 (23L773)".
    pub os_version: Option<String>,
    /// "960.13.1".
    pub source_version: Option<String>,
    pub pairing: Option<PairVariant>,
}

/// A connected receiver.
pub enum Backend {
    AirPlay(airplay::Client),
    /// Scripted (session tests).
    #[doc(hidden)]
    Fake(FakeReceiver),
}

impl Backend {
    /// Connect and verify (AirPlay: pair-verify, SETUP, event channel, RECORD, `/info`, the
    /// type-130 stream). Returns once the receiver can take a `load`.
    pub async fn connect(device: &Device, creds: &Credentials) -> Result<Self, CastErrorKind> {
        let addr = device.addr().ok_or(CastErrorKind::Unreachable)?;
        match device.protocol {
            Protocol::AirPlay => {
                let profile = match device.kind {
                    ReceiverKind::AirPlayTv => airplay::Profile::ThirdParty,
                    _ => airplay::Profile::AppleTv,
                };
                airplay::Client::connect(addr, creds, profile).await.map(Backend::AirPlay)
            }
            Protocol::GoogleCast | Protocol::Dlna => Err(CastErrorKind::Unsupported),
        }
    }

    /// Start playing `req` (the first item of the session).
    pub async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        match self {
            Backend::AirPlay(c) => c.load(req).await,
            Backend::Fake(f) => f.call(Call::Load(req.clone())),
        }
    }

    /// Replace what plays with `req` on the same session (zap, seek, live reconnect).
    pub async fn replace(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        match self {
            Backend::AirPlay(c) => c.replace(req).await,
            Backend::Fake(f) => f.call(Call::Replace(req.clone())),
        }
    }

    pub async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        match self {
            Backend::AirPlay(c) => c.set_paused(paused).await,
            Backend::Fake(f) => f.call(Call::SetPaused(paused)),
        }
    }

    /// The TV fetched the current playlist's media up to `upto_s` (relative): bounds the
    /// position estimate and, after a replace, re-arms `actionAtItemEnd`.
    pub async fn fetched(&mut self, upto_s: f64) {
        match self {
            Backend::AirPlay(c) => c.fetched(upto_s).await,
            Backend::Fake(f) => {
                let _ = f.call(Call::Fetched(upto_s));
            }
        }
    }

    /// Set the receiver's volume (0–1); `None`: the receiver has no volume path (AirPlay).
    pub async fn set_volume(&mut self, v: f64) -> Option<()> {
        match self {
            Backend::AirPlay(_) => None,
            Backend::Fake(f) => f.call(Call::SetVolume(v)).ok(),
        }
    }

    /// The current position (relative), asked now (`playbackInfo`) or estimated.
    pub async fn position(&mut self) -> Option<f64> {
        match self {
            Backend::AirPlay(c) => c.position().await,
            Backend::Fake(f) => f.state().position,
        }
    }

    /// The next event (cancel-safe: use it in `select!`).
    pub async fn next_event(&mut self) -> ReceiverEvent {
        match self {
            Backend::AirPlay(c) => c.next_event().await,
            Backend::Fake(f) => f.events.recv().await.unwrap_or(ReceiverEvent::Lost(CastErrorKind::Unreachable)),
        }
    }

    /// Stop the TV and close the session (AirPlay: `setRate 0`, TEARDOWN, close).
    pub async fn stop(&mut self) {
        match self {
            Backend::AirPlay(c) => c.stop().await,
            Backend::Fake(f) => {
                let _ = f.call(Call::Stop);
            }
        }
    }

    pub fn info(&self) -> ReceiverInfo {
        match self {
            Backend::AirPlay(c) => c.info(),
            Backend::Fake(f) => f.state().info.clone(),
        }
    }
}

/// What a [`FakeReceiver`] was asked, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Load(LoadReq),
    Replace(LoadReq),
    SetPaused(bool),
    Fetched(f64),
    SetVolume(f64),
    Stop,
}

/// The fake's script and record, shared with the test through [`FakeRemote`].
#[derive(Debug, Default)]
pub struct FakeState {
    pub calls: Vec<Call>,
    /// The next calls fail with these, in order (then they succeed).
    pub fail_next: Vec<CastErrorKind>,
    pub position: Option<f64>,
    pub info: ReceiverInfo,
}

/// A scripted receiver: records calls, fails on demand, emits the events the test pushes.
pub struct FakeReceiver {
    state: Arc<Mutex<FakeState>>,
    events: mpsc::UnboundedReceiver<ReceiverEvent>,
}

/// The test's side of a [`FakeReceiver`].
#[derive(Clone)]
pub struct FakeRemote {
    state: Arc<Mutex<FakeState>>,
    events: mpsc::UnboundedSender<ReceiverEvent>,
}

impl FakeReceiver {
    pub fn new() -> (Self, FakeRemote) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { state: state.clone(), events: rx }, FakeRemote { state, events: tx })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn call(&mut self, c: Call) -> Result<(), CastErrorKind> {
        let mut s = self.state();
        s.calls.push(c);
        if s.fail_next.is_empty() { Ok(()) } else { Err(s.fail_next.remove(0)) }
    }
}

impl FakeRemote {
    /// Send an event as the TV would.
    pub fn emit(&self, ev: ReceiverEvent) {
        let _ = self.events.send(ev);
    }

    /// Change the script / read the record.
    pub fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
