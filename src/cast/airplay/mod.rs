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
//!
//! How a [`Client`] runs (CAST-2): the handshake is `session::Session::establish`; after it the
//! control connection lives in a task that runs one request at a time (commands, `/feedback`
//! every 2 s, TEARDOWN) and the event channel in another that acks and decodes. Both hand raw
//! events to the client, which turns them into `ReceiverEvent`s in [`Client::next_event`]:
//! * `loading` → `Loading`, `playing` → `Playing`, `paused` → `Paused` (only once the item has
//!   played: a replace's own `setRate 0` doesn't show), `playbackLikelyToKeepUp` →
//!   `Buffering(false)`, `itemPlayedToEnd` → `Ended`.
//! * **End of media** (foxCast `waitQueue`): `stopped` after the item played, or either
//!   connection closed by the TV after something played (tvOS closes when the remote stops it and
//!   sends no `stopped`) → `StoppedOnTv`. A close before anything played, or a frame that doesn't
//!   open → `Lost(Unreachable)` (the controller re-verifies once).
//! * **Position** (`position.rs`): while VOD or catch-up plays the client asks `playbackInfo`
//!   every 10 s and reports `Position`; once the first request goes unanswered, the estimate.
//!   Positions are relative to the current playlist.

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

use std::collections::VecDeque;
use std::net::SocketAddr;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::receiver::{LoadReq, ReceiverEvent, ReceiverInfo};
use super::{CastErrorKind, Credentials, Identity, PairError, PairVariant, PeerCredentials, Pin};
use events::{Event, PlaybackState};
use position::{Estimate, Source};
use rtsp::{Connection, Request};
use session::{Channel, Raw, Session};

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

/// The item the TV has now.
#[derive(Debug, Clone, PartialEq)]
struct Item {
    uuid: String,
    live: bool,
    /// The last state the TV reported for it (repeats are dropped).
    state: Option<PlaybackState>,
    played: bool,
    ended: bool,
}

/// One verified session with a TV (the session outlives zaps; a failed replace restarts it).
/// Its state is boxed: the receiver dispatch (`receiver::Backend`) holds it by value.
pub struct Client(Box<State>);

/// What a [`Client`] keeps.
struct State {
    addr: SocketAddr,
    creds: Credentials,
    profile: Profile,
    info: ReceiverInfo,
    session: Session,
    /// From the control and event tasks (this is the only receiver: `None` once both ended).
    raw: mpsc::UnboundedReceiver<Raw>,
    item: Option<Item>,
    /// Events translated but not handed out yet (collected while a command or a position request
    /// waited).
    pending: VecDeque<ReceiverEvent>,
    /// Something played in this session: a close by the TV is a stop from the TV, not a loss.
    played: bool,
    /// The session ended (stopped from either side, or lost): no more events; the next load
    /// starts a new session.
    done: bool,
    estimate: Estimate,
    source: Source,
    /// The last `playbackInfo` messageID (PR 2846 counts from 1).
    message_id: i64,
    /// When the next position is due (VOD and catch-up, once playing).
    next_sample: Option<Instant>,
    /// After a replace: `actionAtItemEnd 1` once the TV fetched the new item.
    rearm: bool,
}

impl Client {
    /// Pair-verify (encryption on at once), SETUP (declared PTP), the event channel, RECORD
    /// (skipped on `skipRecord`; errors non-fatal), `GET /info`, the type-130 SETUP.
    pub async fn connect(addr: SocketAddr, creds: &Credentials, profile: Profile) -> Result<Self, CastErrorKind> {
        State::connect(addr, creds, profile).await.map(|s| Self(Box::new(s)))
    }

    /// `insertPlayQueueItem` (fatal on non-2xx), `isInterestedInDateRange` and
    /// `actionAtItemEnd = 1` (non-fatal), `setRate 1` (fatal); then feedback every 2 s.
    ///
    /// With an item already playing this is [`Client::replace`]; on a session that ended it
    /// starts a new session first.
    pub async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.load(req).await
    }

    /// VioletRelay's replace: `setRate 0`, `actionAtItemEnd 0`, insert after the old item,
    /// remove the old one, `setRate 1` (and `actionAtItemEnd 1` after the first fetch).
    ///
    /// Any error in the sequence tears the session down and runs the whole proven start path
    /// again (pair-verify … insert): about 2 s longer, never a re-pairing.
    pub async fn replace(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.replace(req).await
    }

    /// `setRate 0.0` / `1.0`.
    pub async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        self.0.set_paused(paused).await
    }

    /// The TV fetched media up to `upto_s` of the current playlist.
    pub async fn fetched(&mut self, upto_s: f64) {
        self.0.fetched(upto_s).await
    }

    /// `playbackInfo` (every 10 s while VOD plays, once before Play here / Stop), else the
    /// estimate. Relative to the current playlist.
    ///
    /// This asks now (the session calls it before Play here and Stop); the 10 s reports come as
    /// `ReceiverEvent::Position` from [`Client::next_event`] on their own.
    pub async fn position(&mut self) -> Option<f64> {
        self.0.position().await
    }

    /// The next decoded event-channel event (cancel-safe).
    pub async fn next_event(&mut self) -> ReceiverEvent {
        self.0.next_event().await
    }

    /// `setRate 0`, TEARDOWN, close the event channel and the connection.
    pub async fn stop(&mut self) {
        self.0.stop().await
    }

    pub fn info(&self) -> ReceiverInfo {
        self.0.info.clone()
    }
}

impl State {
    async fn connect(addr: SocketAddr, creds: &Credentials, profile: Profile) -> Result<Self, CastErrorKind> {
        let (session, info, raw) = Session::establish(addr, creds, profile).await?;
        Ok(Self {
            addr,
            creds: creds.clone(),
            profile,
            info,
            session,
            raw,
            item: None,
            pending: VecDeque::new(),
            played: false,
            done: false,
            estimate: Estimate::default(),
            source: Source::Untried,
            message_id: 0,
            next_sample: None,
            rearm: false,
        })
    }

    async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.forget_stale();
        if self.done {
            return self.restart(req).await;
        }
        if self.item.is_some() {
            return self.replace(req).await;
        }
        self.start(req).await
    }

    async fn replace(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.forget_stale();
        if self.done {
            return self.restart(req).await;
        }
        let Some(old) = self.item.as_ref().map(|i| i.uuid.clone()) else { return self.start(req).await };
        let new = uuid();
        for cmd in queue::replace(self.profile, &old, &new, req.url.expose()) {
            let failed = match self.session.command(&cmd).await {
                Ok(r) if r.ok() => continue,
                Ok(r) => format!("answered {}", r.status),
                Err(_) => "failed".to_owned(),
            };
            tracing::info!("cast: a replace step ({}) {failed}; starting the session again", queue::str_of(cmd.get("type")).unwrap_or("?"));
            return self.restart(req).await;
        }
        self.begin_item(new, req.live);
        self.rearm = true;
        Ok(())
    }

    async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        let r = self.session.command(&queue::set_rate(if paused { 0.0 } else { 1.0 })).await?;
        if !r.ok() {
            tracing::info!("cast: setRate answered {}", r.status);
            return Err(CastErrorKind::ReceiverFailed);
        }
        if paused {
            self.estimate.freeze(Instant::now());
        }
        Ok(())
    }

    async fn fetched(&mut self, upto_s: f64) {
        self.estimate.bound(upto_s);
        if std::mem::take(&mut self.rearm) && !self.done {
            // VioletRelay `finish_replacement`, best effort: until it lands the TV would advance
            // at the item's end (`actionAtItemEnd 0`), which is harmless with one item queued
            match self.session.command(&queue::action_at_item_end(1)).await {
                Ok(r) if r.ok() => {}
                Ok(r) => tracing::debug!("cast: actionAtItemEnd 1 after the replace answered {}", r.status),
                Err(_) => tracing::debug!("cast: actionAtItemEnd 1 after the replace failed"),
            }
        }
    }

    async fn position(&mut self) -> Option<f64> {
        self.item.as_ref()?;
        if self.source != Source::Estimate
            && !self.done
            && let Some(a) = self.ask().await
        {
            return Some(a.position);
        }
        Some(self.estimate.at(Instant::now()))
    }

    /// Cancel-safe: the only waits are the event queue and the position timer; a position
    /// request cut short is simply not reported.
    async fn next_event(&mut self) -> ReceiverEvent {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return ev;
            }
            if self.done {
                return std::future::pending().await;
            }
            match self.next_sample {
                Some(at) => tokio::select! {
                    raw = self.raw.recv() => self.absorb(raw.unwrap_or_else(gone)),
                    () = tokio::time::sleep_until(at) => self.sample().await,
                },
                None => {
                    let raw = self.raw.recv().await;
                    self.absorb(raw.unwrap_or_else(gone));
                }
            }
        }
    }

    async fn stop(&mut self) {
        self.done = true;
        self.next_sample = None;
        self.session.feedback(false);
        if self.item.take().is_some() {
            match self.session.command(&queue::set_rate(0.0)).await {
                Ok(r) if !r.ok() => tracing::debug!("cast: setRate 0 answered {}", r.status),
                Ok(_) => {}
                Err(_) => tracing::debug!("cast: setRate 0 failed"),
            }
        }
        self.session.teardown().await;
        self.session.close();
    }

    /// The first item of a session: insert, the two non-fatal properties, `setRate 1`, feedback.
    async fn start(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        let uuid = uuid();
        let r = self.session.command(&queue::insert(self.profile, &uuid, req.url.expose(), None)).await?;
        if !r.ok() {
            // nothing more is sent (robkochman: a non-2xx stops the sequence)
            tracing::info!("cast: the TV refused the item (insertPlayQueueItem answered {})", r.status);
            return Err(CastErrorKind::ReceiverFailed);
        }
        self.begin_item(uuid.clone(), req.live);
        for cmd in [queue::date_range(&uuid), queue::action_at_item_end(1)] {
            let r = self.session.command(&cmd).await?;
            if !r.ok() {
                tracing::debug!("cast: setProperty {} answered {}; going on", queue::str_of(cmd.get("property")).unwrap_or("?"), r.status);
            }
        }
        let r = self.session.command(&queue::set_rate(1.0)).await?;
        if !r.ok() {
            tracing::info!("cast: setRate 1 answered {}", r.status);
            return Err(CastErrorKind::ReceiverFailed);
        }
        // feedback only now: never before the commands (PR 2899, foxCast)
        self.session.feedback(true);
        Ok(())
    }

    /// TEARDOWN, then the whole session again and the first-item sequence for `req`.
    async fn restart(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.session.feedback(false);
        self.session.teardown().await;
        self.session.close();
        self.item = None;
        self.next_sample = None;
        self.rearm = false;
        let (session, info, raw) = match Session::establish(self.addr, &self.creds, self.profile).await {
            Ok(s) => s,
            Err(e) => {
                self.done = true;
                return Err(e);
            }
        };
        self.session = session;
        self.info = info;
        self.raw = raw;
        self.played = false;
        self.done = false;
        self.source = Source::Untried;
        self.start(req).await
    }

    fn begin_item(&mut self, uuid: String, live: bool) {
        self.item = Some(Item { uuid, live, state: None, played: false, ended: false });
        self.estimate.restart();
        self.next_sample = None;
        self.rearm = false;
    }

    /// Before a new item: what the TV said about the old one is moot (and must not be read as the
    /// new one's), so it is taken in and dropped.
    fn forget_stale(&mut self) {
        while let Ok(raw) = self.raw.try_recv() {
            self.absorb(raw);
        }
        self.pending.clear();
    }

    fn absorb(&mut self, raw: Raw) {
        if self.done {
            return;
        }
        let now = Instant::now();
        match raw {
            Raw::Event(Event::State(state)) => self.on_state(state, now),
            Raw::Event(Event::Notification(name)) => self.on_notification(&name),
            Raw::Event(Event::Response { message_id, .. }) => tracing::debug!("cast: a late playbackInfo answer ({message_id})"),
            Raw::Event(Event::UpdateInfo | Event::Other(_)) => {}
            Raw::Closed { channel, bad } => {
                let ev = if !bad && self.played { ReceiverEvent::StoppedOnTv } else { ReceiverEvent::Lost(CastErrorKind::Unreachable) };
                tracing::info!("cast: the {} connection ended{}", channel.as_str(), if bad { " on data that can't be read" } else { "" });
                self.end(ev);
            }
        }
    }

    fn on_state(&mut self, state: PlaybackState, now: Instant) {
        let Some(item) = self.item.as_mut() else {
            tracing::debug!("cast: {state:?} before any item");
            return;
        };
        if item.state == Some(state) {
            return;
        }
        item.state = Some(state);
        let ev = match state {
            PlaybackState::Loading => {
                self.estimate.freeze(now);
                Some(ReceiverEvent::Loading)
            }
            PlaybackState::Playing => {
                self.estimate.run(now);
                if !item.played {
                    item.played = true;
                    if !item.live {
                        self.next_sample = Some(now);
                    }
                }
                self.played = true;
                Some(ReceiverEvent::Playing)
            }
            PlaybackState::Paused => {
                self.estimate.freeze(now);
                item.played.then_some(ReceiverEvent::Paused)
            }
            PlaybackState::Stopped => {
                self.estimate.freeze(now);
                if item.played && !item.ended {
                    self.end(ReceiverEvent::StoppedOnTv);
                }
                None
            }
        };
        self.pending.extend(ev);
    }

    fn on_notification(&mut self, name: &str) {
        let Some(item) = self.item.as_mut() else { return };
        match name {
            "itemPlayedToEnd" if !item.ended => {
                item.ended = true;
                self.next_sample = None;
                self.pending.push_back(ReceiverEvent::Ended);
            }
            "playbackLikelyToKeepUp" if item.played => self.pending.push_back(ReceiverEvent::Buffering(false)),
            _ => tracing::debug!("cast: notification {name}"),
        }
    }

    fn end(&mut self, ev: ReceiverEvent) {
        self.done = true;
        self.next_sample = None;
        self.pending.push_back(ev);
    }

    /// The 10 s report: an answer when the TV gives them, else the estimate. Only while the item
    /// plays (paused or loading, it stands still).
    async fn sample(&mut self) {
        self.next_sample = Some(Instant::now() + position::POLL_INTERVAL);
        if self.item.as_ref().is_none_or(|i| i.state != Some(PlaybackState::Playing)) {
            return;
        }
        let answer = match self.source {
            Source::Estimate => None,
            Source::Untried | Source::Answers => self.ask().await,
        };
        if self.done || self.item.as_ref().is_none_or(|i| i.ended) {
            return;
        }
        let (pos, dur) = match answer {
            Some(a) => (a.position, a.duration),
            None => (self.estimate.at(Instant::now()), None),
        };
        self.pending.push_back(ReceiverEvent::Position { pos, dur });
    }

    /// One `playbackInfo` request, answered within 3 s or not at all. The first one that goes
    /// unanswered switches the session to the estimate.
    async fn ask(&mut self) -> Option<position::Answer> {
        let deadline = Instant::now() + position::ANSWER_TIMEOUT;
        self.message_id += 1;
        let id = self.message_id;
        let answer = match tokio::time::timeout_at(deadline, self.session.command(&queue::playback_info(id))).await {
            Ok(Ok(r)) if r.ok() => self.await_answer(id, deadline).await,
            Ok(Ok(r)) => {
                tracing::debug!("cast: playbackInfo answered {}", r.status);
                None
            }
            _ => None,
        };
        match answer {
            Some(a) => {
                self.source = Source::Answers;
                self.estimate.anchor(a.position, Instant::now());
                Some(a)
            }
            None => {
                if self.source == Source::Untried {
                    tracing::debug!("cast: playbackInfo goes unanswered; estimating positions for this session");
                    self.source = Source::Estimate;
                }
                None
            }
        }
    }

    /// Wait for the response to `id`, taking in every other event meanwhile.
    async fn await_answer(&mut self, id: i64, deadline: Instant) -> Option<position::Answer> {
        loop {
            match tokio::time::timeout_at(deadline, self.raw.recv()).await {
                Err(_) => return None,
                Ok(Some(Raw::Event(Event::Response { message_id, value }))) if message_id == id => {
                    return value.as_ref().and_then(position::answer);
                }
                Ok(raw) => {
                    self.absorb(raw.unwrap_or_else(gone));
                    if self.done {
                        return None;
                    }
                }
            }
        }
    }
}

/// Both tasks ended without a word (the client owns the only receiver).
fn gone() -> Raw {
    Raw::Closed { channel: Channel::Control, bad: false }
}

/// One pairing (DESIGN §40 §7.2): `/pair-pin-start` and the whole pair-setup on ONE TCP
/// connection (the TV accepts the PIN only there), held open while the user types.
pub struct PairingSession {
    addr: SocketAddr,
    identity: Identity,
    /// `None` once the TV closed it, or after M6.
    conn: Option<Connection>,
    variant: PairVariant,
}

impl PairingSession {
    /// Connect and ask the TV to show a code with `variant`'s headers (200 or 453: success).
    ///
    /// HKP 3 answered with anything else (but 403, a Mac: `NotSupported`) moves to HKP 5 once
    /// (the ladder): [`PairingSession::variant`] says which one shows the code.
    pub async fn start(addr: SocketAddr, identity: &Identity, variant: PairVariant) -> Result<Self, PairError> {
        let mut s = Self { addr, identity: identity.clone(), conn: None, variant };
        s.show_code().await?;
        Ok(s)
    }

    pub fn variant(&self) -> PairVariant {
        self.variant
    }

    /// M1–M6 with `pin` (proof and signature verified, error TLVs first, unknown TLVs ignored).
    /// After `WrongPin` the TV shows a new code: the session stays usable.
    ///
    /// A 470 at M2–M4 under HKP 3 is the other step of the ladder: the session asks for a new
    /// code with HKP 5 on the same connection and answers `WrongPin` (a new code is showing;
    /// `variant()` is now HKP 5). A 470 under HKP 5 is `Refused`. The TV closing the connection
    /// while the user typed is `Expired`.
    pub async fn finish(&mut self, pin: &Pin) -> Result<PeerCredentials, PairError> {
        let mut setup = pair::PairSetup::new(&self.identity, self.variant);
        let m2 = self.setup_step(setup.m1(), true).await?;
        let m3 = setup.m3(&m2, pin)?;
        let m4 = self.setup_step(m3, true).await?;
        let m5 = setup.m5(&m4)?;
        let m6 = self.setup_step(m5, false).await?;
        let creds = setup.finish(&m6)?;
        // closed after M6
        self.conn = None;
        Ok(creds)
    }

    /// `/pair-pin-start` until the TV shows a code, climbing the ladder once.
    async fn show_code(&mut self) -> Result<(), PairError> {
        loop {
            match self.pin_start().await? {
                200 | 453 => return Ok(()),
                403 => return Err(PairError::NotSupported),
                s if self.variant == PairVariant::Hkp3 => {
                    tracing::info!("cast: pin-start answered {s} to HKP 3; trying HKP 5");
                    self.variant = PairVariant::Hkp5;
                }
                s => {
                    tracing::info!("cast: pin-start answered {s}");
                    return Err(PairError::Refused { home_only: false });
                }
            }
        }
    }

    /// One `/pair-pin-start` on the pairing connection (opened now, or again when the TV closed
    /// it): its status.
    async fn pin_start(&mut self) -> Result<u16, PairError> {
        let req = pairing_request("/pair-pin-start", self.variant, &self.identity);
        if let Some(conn) = self.conn.as_mut() {
            match conn.exchange(req.clone()).await {
                Ok(r) => return Ok(r.status),
                Err(rtsp::Error::Timeout) => return Err(PairError::Unreachable),
                Err(_) => self.conn = None,
            }
        }
        let mut conn = Connection::connect(self.addr).await.map_err(|_| PairError::Unreachable)?;
        let r = conn.exchange(req).await.map_err(|_| PairError::Unreachable)?;
        self.conn = Some(conn);
        Ok(r.status)
    }

    /// One `/pair-setup` exchange: the reply body. `ladder`: a 470 here climbs to HKP 5.
    async fn setup_step(&mut self, body: Vec<u8>, ladder: bool) -> Result<Vec<u8>, PairError> {
        let req = pairing_request("/pair-setup", self.variant, &self.identity).body(body);
        let conn = self.conn.as_mut().ok_or(PairError::Expired)?;
        let resp = match conn.exchange(req).await {
            Ok(r) => r,
            Err(rtsp::Error::Timeout) => return Err(PairError::Unreachable),
            Err(rtsp::Error::Malformed) => return Err(PairError::Failed),
            Err(rtsp::Error::Closed) => {
                self.conn = None;
                return Err(PairError::Expired);
            }
        };
        match resp.status {
            _ if resp.ok() => Ok(resp.body),
            470 if ladder && self.variant == PairVariant::Hkp3 => {
                tracing::info!("cast: pair-setup answered 470 to HKP 3; asking for a new code with HKP 5");
                self.variant = PairVariant::Hkp5;
                self.show_code().await?;
                Err(PairError::WrongPin)
            }
            470 => Err(PairError::Refused { home_only: false }),
            s => {
                tracing::info!("cast: pair-setup answered {s}");
                Err(PairError::Failed)
            }
        }
    }
}

/// `GET /info` (unauthenticated): "Add by address…" and the version line.
pub async fn probe_info(addr: SocketAddr) -> Result<features::AirPlayInfo, CastErrorKind> {
    let mut conn = Connection::connect(addr).await.map_err(|_| CastErrorKind::Unreachable)?;
    let resp = conn.exchange(Request::http("GET", "/info")).await.map_err(|_| CastErrorKind::Unreachable)?;
    if !resp.ok() {
        tracing::info!("cast: /info answered {}", resp.status);
        return Err(CastErrorKind::Unsupported);
    }
    resp.plist().map(|v| features::from_info(&v)).ok_or(CastErrorKind::Unsupported)
}

/// A pairing request (`/pair-pin-start`, `/pair-setup`, `/pair-verify`) with `variant`'s headers
/// (`pair::headers`: `X-Apple-HKP`, `X-Apple-Client-Name`, …). HKP 5 adds
/// `X-Apple-SupportedPINLengths: 4` on pin-start and `X-Apple-PD: 1` on pair-verify (foxCast,
/// facts); the body is `application/octet-stream` (pyatv `_AIRPLAY_HEADERS`).
fn pairing_request(path: &str, variant: PairVariant, identity: &Identity) -> Request {
    let req = pair::headers(variant, identity).into_iter().fold(Request::http("POST", path), |r, (k, v)| r.header(k, v));
    let req = req.header_once("Content-Type", "application/octet-stream");
    match (variant, path) {
        (PairVariant::Hkp5, "/pair-pin-start") => req.header_once("X-Apple-SupportedPINLengths", 4),
        (PairVariant::Hkp5, "/pair-verify") => req.header_once("X-Apple-PD", 1),
        _ => req,
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    aws_lc_rs::rand::fill(&mut b).expect("the system random number generator");
    b
}

fn random_u32() -> u32 {
    u32::from_le_bytes(random_bytes())
}

fn random_u64() -> u64 {
    u64::from_le_bytes(random_bytes())
}

/// A random (version 4) UUID, upper case (every UUID the references send is).
fn uuid() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// `AA:BB:CC:DD:EE:FF` (SETUP's `deviceID`/`macAddress`, the fallback `channelID`).
fn mac_string(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_shapes_the_references_send() {
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4', "version 4: {u}");
        assert!(u.chars().all(|c| c == '-' || c.is_ascii_digit() || c.is_ascii_uppercase()), "{u}");
        assert_eq!(mac_string(&[0x02, 0x1a, 0, 0xff, 0x10, 0x09]), "02:1A:00:FF:10:09");
    }
}
