// Ported from pyatv pyatv/protocols/raop/protocols/airplayv2.py @ b2484099 (PR 2899; MIT, © 2020 Pierre Ståhl).
//! The session sequence (DESIGN §40 §7.3): pair-verify → SETUP (declared PTP, no timing port) →
//! event channel → RECORD (skipped on `skipRecord`; errors non-fatal) → `GET /info` → type-130
//! SETUP → `/command` → `/feedback` every 2 s after the commands, never in flight with a
//! `/command`; TEARDOWN. OWNER: CAST-2. Ported from pyatv PR 2899 (`AirPlayV2.play_url`,
//! `_setup_video_session`, `_setup_remote_control_session`, `_feedback_task_loop`) @ b2484099
//! (MIT), with foxCast's order and RECORD headers as the cross-check (facts only), and
//! VioletRelay's third-party extras (facts only, §7.7).
//!
//! * **Timing: declared PTP only.** No clock, no inbound UDP, no `timingPort`: PR 2899 measured
//!   NTP on tvOS 26.5 (zero events, the picture gone by ~27 s) and foxCast saw the same on 27.
//! * **Order: RECORD, then `GET /info`** (PR 2899, foxCast, castcli). Reading `/info` before
//!   pair-verify, or feedback before RECORD, drops the connection (PR 2899).
//! * **One request in flight.** After the handshake the connection moves into [`Control`], a task
//!   that runs every request — the commands, `/feedback`, TEARDOWN — one after the other, reads
//!   while idle (so a close by the TV is seen at once and stray replies are dropped), and reports
//!   the end of the connection as [`Raw::Closed`].
//! * **The sender identity is foxCast's** (castcli sends the same model and OS): an iPhone, as
//!   every working sender presents itself, named [`super::CLIENT_NAME`], with
//!   `cast_identity.sender_mac` as `deviceID` and `macAddress`.
//! * **A third-party TV** (§7.7) gets VioletRelay's flow, the only one tested on such a TV (a
//!   Hisense): its TXT identifiers for the `channelID` and the `ClockPorts` key, `SETPEERSX`,
//!   the type-120 SETUP, and [`stream_headers`] on the type-120 and type-130 SETUPs.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::bplist::Value;
use super::events::{self, Event};
use super::features::{self, AirPlayInfo, ReceiverIds};
use super::hap::FrameCipher;
use super::pair::{PairVerify, SessionKeys};
use super::queue::{self, Spelling};
use super::rtsp::{self, Connection, Request, Response};
use super::trace::Trace;
use super::{CLIENT_NAME, Profile};
use crate::cast::receiver::ReceiverInfo;
use crate::cast::{CastErrorKind, Credentials, PairError, PairVariant};

/// pyatv `FEEDBACK_INTERVAL` ("This is what iOS uses").
pub const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// The "UrlFling" client type of the type-130 remote-control stream (PR 2899, foxCast; LIVI and
/// DiPlay confirm it). Not the MRP tunnel (`controlType 2`, `1910A70F-…`).
pub const RCS_CLIENT_TYPE_UUID: &str = "A6B27562-B43A-4F2D-B75F-82391E250194";
/// The event channel's connect is retried 5× 1 s apart (pyatv `_setup_base`).
pub const EVENTS_CONNECT_ATTEMPTS: u32 = 5;
pub const EVENTS_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The sender identity in SETUP (foxCast `playback.go`; PR 2899 sends the same).
pub const SENDER_MODEL: &str = "iPhone14,3";
pub const SENDER_OS_NAME: &str = "iPhone OS";
pub const SENDER_OS_VERSION: &str = "16.5";
pub const SENDER_OS_BUILD: &str = "20F66";
pub const SENDER_SOURCE_VERSION: &str = "690.7.1";
/// SETPEERSX's clock port (VioletRelay, third-party TVs only).
pub const PEER_CLOCK_PORT: i64 = 32768;

/// Third-party TVs: VioletRelay's `_stream_headers` on the type-120 and type-130 SETUPs
/// (`airplay_modern.py` L285–289, L350–353, L401; facts only), the session id being the base
/// SETUP's `sessionUUID` in lower case. Never on `/command`: the Hisense SDK rejects the
/// session and protocol headers there (VioletRelay; `queue::Spelling` negotiates the stream id).
pub fn stream_headers(req: Request, session_uuid: &str) -> Request {
    req.header("X-Apple-ProtocolVersion", 1).header("X-Apple-Session-ID", session_uuid.to_ascii_lowercase()).header("X-Apple-Stream-ID", 1)
}

/// Which connection something happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Port 7000: requests and their replies.
    Control,
    /// The TV's `eventPort`: its requests and our acks.
    Events,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Events => "events",
        }
    }
}

/// What the session's tasks hand to the client (`Client::next_event` turns them into
/// `ReceiverEvent`s).
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Event(Event),
    /// A connection ended: the TV closed it (`bad == false`), or it carried a frame or message
    /// that can't be read (`bad`).
    Closed {
        channel: Channel,
        bad: bool,
    },
}

/// The SETUP body of a video session (DESIGN §40 §7.3 step 2; PR 2899 `_setup_video_session`,
/// in its key order). `correlation` is both the timing peer's `ID` and
/// `sessionCorrelationUUID` (PR 2899, foxCast).
pub fn setup_body(local_ip: &str, session_uuid: &str, correlation: &str, sender_mac: &str) -> Value {
    let peer = timing_peer(local_ip, correlation);
    queue::dict([
        ("timingProtocol", queue::text("PTP")),
        ("timingPeerInfo", peer.clone()),
        ("timingPeerList", Value::Array(vec![peer])),
        ("sessionUUID", queue::text(session_uuid)),
        ("sessionCorrelationUUID", queue::text(correlation)),
        ("updateSessionRequest", Value::Bool(false)),
        ("statsCollectionEnabled", Value::Bool(false)),
        ("isMultiSelectAirPlay", Value::Bool(false)),
        ("deviceID", queue::text(sender_mac)),
        ("macAddress", queue::text(sender_mac)),
        ("model", queue::text(SENDER_MODEL)),
        ("name", queue::text(CLIENT_NAME)),
        ("osName", queue::text(SENDER_OS_NAME)),
        ("osVersion", queue::text(SENDER_OS_VERSION)),
        ("osBuildVersion", queue::text(SENDER_OS_BUILD)),
        ("sourceVersion", queue::text(SENDER_SOURCE_VERSION)),
    ])
}

fn timing_peer(local_ip: &str, correlation: &str) -> Value {
    queue::dict([
        ("ID", queue::text(correlation)),
        ("Addresses", Value::Array(vec![queue::text(local_ip)])),
        ("DeviceType", Value::Int(0)),
        ("SupportsClockPortMatchingOverride", Value::Bool(true)),
    ])
}

/// The type-130 SETUP that registers the remote-control stream `/command` needs (step 6; PR
/// 2899 `_setup_remote_control_session`). No `dataPort` comes back and nothing connects.
pub fn rcs_body(channel_id: &str, client_uuid: &str) -> Value {
    let stream = queue::dict([
        ("type", Value::Int(130)),
        ("controlType", Value::Int(1)),
        ("channelID", queue::text(channel_id)),
        ("clientUUID", queue::text(client_uuid)),
        ("clientTypeUUID", queue::text(RCS_CLIENT_TYPE_UUID)),
    ]);
    queue::dict([("streams", Value::Array(vec![stream]))])
}

/// `channelID`: `<receiver id>-RCS-1` — an Apple TV's `/info` `psi` (PR 2899, foxCast), a
/// third-party TV's TXT `psi`, else its `pi` (VioletRelay, [`ReceiverIds::receiver_id`]);
/// without one, `<sender MAC>-RCS-1` (castcli's form; both work on 26.x).
pub fn channel_id(receiver_id: Option<&str>, sender_mac: &str) -> String {
    format!("{}-RCS-1", receiver_id.filter(|p| !p.is_empty()).unwrap_or(sender_mac))
}

/// Third-party TVs (DESIGN §40 §7.7, VioletRelay `build_playback_stream_body`): a playback stream
/// before the type-130 one, so the TV allocates its full-screen video surface.
pub fn playback_stream_body() -> Value {
    queue::dict([("streams", Value::Array(vec![queue::dict([("type", Value::Int(120))])]))])
}

/// Third-party TVs: `SETPEERSX`'s peer list (VioletRelay, facts). The clock-port key is the TV's
/// TXT `gid`, else its `psi`, else its `pi` ([`ReceiverIds::clock_key`]).
pub fn peers_body(local_ip: &str, correlation: &str, clock_id: i64, receiver_key: &str) -> Value {
    Value::Array(vec![queue::dict([
        ("ID", queue::text(correlation)),
        ("Addresses", Value::Array(vec![queue::text(local_ip)])),
        ("ClockID", Value::Int(clock_id)),
        ("DeviceType", Value::Int(0)),
        ("ClockPorts", queue::dict([(receiver_key, Value::Int(PEER_CLOCK_PORT))])),
        ("SupportsClockPortMatchingOverride", Value::Bool(true)),
    ])])
}

/// What the base SETUP answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetupReply {
    pub event_port: Option<u16>,
    /// Seen only in mirroring replies so far (foxCast `mirror.go`, doubletake): honoured.
    pub skip_record: bool,
}

impl SetupReply {
    pub fn from_plist(v: Option<&Value>) -> Self {
        let event_port = v.and_then(|v| queue::int(v.get("eventPort"))).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0);
        let skip_record = matches!(v.and_then(|v| v.get("skipRecord")), Some(Value::Bool(true)));
        Self { event_port, skip_record }
    }
}

/// A task that is aborted when dropped.
pub struct Task(JoinHandle<()>);

impl Task {
    pub fn spawn(fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(tokio::spawn(fut))
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Op {
    Exchange(Request, oneshot::Sender<Result<Response, rtsp::Error>>),
    Feedback(bool),
}

/// The control connection once the handshake is done: one task runs every request in order
/// (exactly one in flight), sends `/feedback` every 2 s while it is on (best effort: an error
/// status or a timeout is logged at debug, a broken connection ends the session), and reads
/// while idle.
pub struct Control {
    ops: mpsc::UnboundedSender<Op>,
    _task: Task,
}

impl Control {
    pub fn spawn(conn: Connection, raw: mpsc::UnboundedSender<Raw>) -> Self {
        let (ops, rx) = mpsc::unbounded_channel();
        Self { ops, _task: Task::spawn(run_control(conn, rx, raw)) }
    }

    /// Run `req` after whatever is queued (cancelling the caller doesn't cancel the request: it
    /// still completes, so nothing is ever left half-sent).
    pub async fn exchange(&self, req: Request) -> Result<Response, rtsp::Error> {
        let (tx, rx) = oneshot::channel();
        self.ops.send(Op::Exchange(req, tx)).map_err(|_| rtsp::Error::Closed)?;
        rx.await.unwrap_or(Err(rtsp::Error::Closed))
    }

    /// Feedback on (the first one goes out at once, as pyatv's loop does) or off.
    pub fn feedback(&self, on: bool) {
        let _ = self.ops.send(Op::Feedback(on));
    }
}

async fn run_control(mut conn: Connection, mut ops: mpsc::UnboundedReceiver<Op>, raw: mpsc::UnboundedSender<Raw>) {
    let mut tick = tokio::time::interval(FEEDBACK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut feedback = false;
    let error = loop {
        let failed = tokio::select! {
            biased;
            op = ops.recv() => match op {
                // the client closed the session
                None => return,
                Some(Op::Exchange(req, reply)) => {
                    let r = conn.exchange(req).await;
                    let fatal = r.as_ref().err().copied().filter(|e| e.fatal());
                    let _ = reply.send(r);
                    fatal
                }
                Some(Op::Feedback(on)) => {
                    if on && !feedback {
                        tick.reset_immediately();
                    }
                    feedback = on;
                    None
                }
            },
            _ = tick.tick(), if feedback => match conn.exchange(Request::rtsp("POST", "/feedback")).await {
                Ok(r) if !r.ok() => {
                    tracing::debug!("cast: /feedback answered {}", r.status);
                    None
                }
                Ok(_) => None,
                Err(e) if e.fatal() => Some(e),
                Err(_) => None,
            },
            r = conn.idle() => r.err(),
        };
        if let Some(e) = failed {
            break e;
        }
    };
    tracing::debug!("cast: the control connection ended ({error:?})");
    let _ = raw.send(Raw::Closed { channel: Channel::Control, bad: error == rtsp::Error::Malformed });
}

/// An established session (steps 1–6): what `/command` and TEARDOWN need.
pub struct Session {
    control: Option<Control>,
    events: Option<Task>,
    uri: String,
    session_id: u32,
    stream_id: i64,
    profile: Profile,
    spelling: Spelling,
}

impl Session {
    /// Steps 1–6 of DESIGN §40 §7.3 on a new connection to `addr`. Returns the session, what it
    /// learned about the TV, and the stream of raw events (the client owns the only receiver:
    /// once both tasks end it yields `None`). `txt` are the receiver's TXT identifiers (a
    /// third-party TV's; an Apple TV's `channelID` comes from `/info`).
    pub async fn establish(
        addr: SocketAddr,
        creds: &Credentials,
        profile: Profile,
        txt: &ReceiverIds,
    ) -> Result<(Self, ReceiverInfo, mpsc::UnboundedReceiver<Raw>), CastErrorKind> {
        let mut conn = Connection::connect(addr).await.map_err(|_| CastErrorKind::Unreachable)?;
        let trace = Trace::start();
        conn.set_trace(trace.clone());

        // 1. pair-verify; encryption on at once (the shared secret lives only as long as it takes
        //    to derive the control and event keys)
        let (control_keys, event_keys) = {
            let keys = verify(&mut conn, creds).await?;
            (keys.control(), keys.events())
        };
        conn.encrypt(FrameCipher::new(&control_keys.0, &control_keys.1));

        // 2. SETUP: a video session with declared PTP
        let local = conn.local_ip().to_string();
        let mac = super::mac_string(&creds.identity.sender_mac);
        let correlation = super::uuid();
        let session_uuid = super::uuid();
        let uri = conn.session_uri();
        let body = setup_body(&local, &session_uuid, &correlation, &mac);
        let resp = conn.exchange(Request::rtsp("SETUP", uri.clone()).plist(&body)).await.map_err(unreachable)?;
        if !resp.ok() {
            tracing::info!("cast: the TV refused the video session (SETUP answered {})", resp.status);
            return Err(CastErrorKind::ReceiverFailed);
        }
        let reply = SetupReply::from_plist(resp.plist().as_ref());

        // 3. the event channel (keys swapped: we read with the TV's write key)
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let events = match reply.event_port {
            Some(port) => Some(events::open(SocketAddr::new(conn.remote_ip(), port), event_keys, raw_tx.clone(), trace.clone()).await?),
            None => {
                tracing::debug!("cast: SETUP named no event port");
                None
            }
        };

        // 4. RECORD, unless the TV said skipRecord; an error is logged and the session goes on
        if reply.skip_record {
            tracing::debug!("cast: SETUP said skipRecord");
        } else {
            let record = Request::rtsp("RECORD", uri.clone()).header("Range", "npt=0-").header("RTP-Info", "seq=0;rtptime=0");
            match tolerate(conn.exchange(record).await)? {
                Some(r) if r.ok() => {}
                Some(r) => tracing::debug!("cast: RECORD answered {}; going on", r.status),
                None => tracing::debug!("cast: RECORD went unanswered; going on"),
            }
        }

        // 5. GET /info: psi (an Apple TV's channel id), the TV's model and versions; without it
        //    the channel id comes from the sender MAC
        let info_plist = match tolerate(conn.exchange(Request::http("GET", "/info")).await)? {
            Some(r) if r.ok() => r.plist(),
            r => {
                tracing::debug!("cast: /info answered {:?}", r.map(|r| r.status));
                None
            }
        };
        let ap = info_plist.as_ref().map(features::from_info).unwrap_or_default();

        // third-party TVs (§7.7): the TXT record's identifiers, as VioletRelay reads them; `/info`'s
        // only when the record had none (a TV added by address)
        let ids = match profile {
            Profile::AppleTv => ap.ids(),
            Profile::ThirdParty if txt.is_empty() => ap.ids(),
            Profile::ThirdParty => txt.clone(),
        };
        let mut rcs_setup = Request::rtsp("SETUP", uri.clone());

        // third-party TVs: SETPEERSX (404/501 tolerated), then a type-120 playback stream
        if profile == Profile::ThirdParty {
            let key = ids.clock_key().unwrap_or_default();
            let clock_id = i64::try_from(super::random_u64() >> 1).unwrap_or(1).max(1);
            let body = super::bplist::encode(&peers_body(&local, &correlation, clock_id, key));
            let peers = Request::rtsp("SETPEERSX", uri.clone()).header("Content-Type", "/peer-list-changed-x").body(body);
            let resp = conn.exchange(peers).await.map_err(unreachable)?;
            match resp.status {
                _ if resp.ok() => {}
                404 | 501 => tracing::debug!("cast: the TV skipped SETPEERSX ({})", resp.status),
                s => {
                    tracing::info!("cast: the TV refused SETPEERSX ({s})");
                    return Err(CastErrorKind::ReceiverFailed);
                }
            }
            let playback = stream_headers(Request::rtsp("SETUP", uri.clone()), &session_uuid).plist(&playback_stream_body());
            let resp = conn.exchange(playback).await.map_err(unreachable)?;
            if !resp.ok() {
                tracing::info!("cast: the TV refused the playback stream (SETUP answered {})", resp.status);
                return Err(CastErrorKind::ReceiverFailed);
            }
            rcs_setup = stream_headers(rcs_setup, &session_uuid);
        }

        // 6. the type-130 remote-control stream: /command answers 500 until it exists. An Apple
        //    TV's channel id is its /info psi (PR 2899, foxCast)
        let receiver_id = match profile {
            Profile::AppleTv => ids.psi.as_deref(),
            Profile::ThirdParty => ids.receiver_id(),
        };
        let rcs = rcs_body(&channel_id(receiver_id, &mac), &super::uuid());
        let resp = conn.exchange(rcs_setup.plist(&rcs)).await.map_err(unreachable)?;
        let streams = resp.plist().and_then(|v| match v.get("streams") {
            Some(Value::Array(a)) => a.first().cloned(),
            _ => None,
        });
        let Some(stream) = streams.filter(|_| resp.ok()) else {
            tracing::info!("cast: the TV refused the remote-control stream (SETUP answered {})", resp.status);
            return Err(CastErrorKind::ReceiverFailed);
        };
        let stream_id = queue::int(stream.get("streamID")).unwrap_or(1);

        let info = receiver_info(&ap, creds.peer.variant);
        let session_id = conn.session_id();
        let session =
            Self { control: Some(Control::spawn(conn, raw_tx)), events, uri, session_id, stream_id, profile, spelling: Spelling::new(profile) };
        Ok((session, info, raw_rx))
    }

    /// `POST /command` with `cmd` (DESIGN §40 §7.3 step 7). A third-party TV gets it over RTSP and
    /// the stream-id header spellings in turn until one is answered 2xx (§7.7); after that, that
    /// one. Any status comes back (the caller decides what is fatal); a broken connection is
    /// `Unreachable`.
    pub async fn command(&mut self, cmd: &Value) -> Result<Response, CastErrorKind> {
        let control = self.control.as_ref().ok_or(CastErrorKind::Unreachable)?;
        let body = queue::wrap(cmd);
        let mut last = None;
        for header in self.spelling.candidates() {
            let resp = control.exchange(queue::request(self.profile, &body, header, self.stream_id)).await.map_err(unreachable)?;
            if resp.ok() {
                self.spelling.confirm(header);
                return Ok(resp);
            }
            last = Some(resp);
        }
        last.ok_or(CastErrorKind::Unreachable)
    }

    pub fn feedback(&self, on: bool) {
        if let Some(c) = &self.control {
            c.feedback(on);
        }
    }

    /// `TEARDOWN` (pyatv `RtspSession.teardown`: the session URI with a `Session` header), best
    /// effort.
    pub async fn teardown(&self) {
        let Some(control) = &self.control else { return };
        match control.exchange(Request::rtsp("TEARDOWN", self.uri.clone()).header("Session", self.session_id)).await {
            Ok(r) if !r.ok() => tracing::debug!("cast: TEARDOWN answered {}", r.status),
            Ok(_) => {}
            Err(e) => tracing::debug!("cast: TEARDOWN failed ({e:?})"),
        }
    }

    /// Close the event channel and the control connection (their tasks end; the TV sees both
    /// close).
    pub fn close(&mut self) {
        self.events = None;
        self.control = None;
    }
}

/// Pair-verify on `conn` (DESIGN §40 §7.2 "Pair-verify"; pyatv `AirPlayHapPairVerifyProcedure`,
/// with the M4 status checked). Replies: an empty 200 → `PairingRefused` (the "Anyone on the Same
/// Network" hint); an id or signature mismatch or an error TLV → `PairingLost`; 470 →
/// `PairingRefused`; refused or timed out → `Unreachable`.
async fn verify(conn: &mut Connection, creds: &Credentials) -> Result<SessionKeys, CastErrorKind> {
    let mut pv = PairVerify::new(&creds.identity, &creds.peer, &super::random_bytes::<32>());
    let m2 = verify_step(conn, creds, pv.m1()).await?;
    let m3 = pv.m3(&m2).map_err(verify_error)?;
    let m4 = verify_step(conn, creds, m3).await?;
    pv.finish(&m4).map_err(verify_error)
}

async fn verify_step(conn: &mut Connection, creds: &Credentials, body: Vec<u8>) -> Result<Vec<u8>, CastErrorKind> {
    let req = super::pairing_request("/pair-verify", creds.peer.variant, &creds.identity).body(body);
    let resp = conn.exchange(req).await.map_err(unreachable)?;
    match resp.status {
        _ if resp.ok() => Ok(resp.body),
        470 => Err(CastErrorKind::PairingRefused),
        s => {
            tracing::info!("cast: pair-verify answered {s}");
            Err(CastErrorKind::PairingLost)
        }
    }
}

fn verify_error(e: PairError) -> CastErrorKind {
    match e {
        PairError::Refused { home_only: true } => CastErrorKind::PairingRefused,
        PairError::Unreachable | PairError::Expired => CastErrorKind::Unreachable,
        PairError::NotSupported => CastErrorKind::Unsupported,
        PairError::Refused { home_only: false } | PairError::WrongPin | PairError::Backoff { .. } | PairError::Failed => CastErrorKind::PairingLost,
    }
}

fn unreachable(_: rtsp::Error) -> CastErrorKind {
    CastErrorKind::Unreachable
}

/// A step whose failure the session survives: a timeout is `None` (its late reply is dropped
/// when it comes); a broken connection still ends the session.
fn tolerate(r: Result<Response, rtsp::Error>) -> Result<Option<Response>, CastErrorKind> {
    match r {
        Ok(r) => Ok(Some(r)),
        Err(rtsp::Error::Timeout) => Ok(None),
        Err(_) => Err(CastErrorKind::Unreachable),
    }
}

/// A device string made safe to show and log (`None` when nothing is left).
fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref().map(|s| crate::cast::sanitize_name(s, "")).filter(|s| !s.is_empty())
}

/// What the session learned (device strings sanitized: they are shown in Settings and logged).
/// The Apple TV's `/info` carries only the build ("(23L773)"); the registry joins it with the TXT
/// version (`merge_os_version`) and the session logs the joined line (DESIGN §40 §7.6).
pub fn receiver_info(ap: &AirPlayInfo, variant: PairVariant) -> ReceiverInfo {
    let os_version = crate::cast::os_version_text(clean(&ap.os_version), clean(&ap.os_build));
    ReceiverInfo { model: clean(&ap.model), os_version, source_version: clean(&ap.source_version), pairing: Some(variant) }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn strings(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
            Value::Dict(d) => d.iter().for_each(|(k, v)| {
                out.push(k.clone());
                strings(v, out)
            }),
            _ => {}
        }
    }

    /// DESIGN §40 §7.6 row 9: Tuner never declares NTP and never names a timing port.
    #[test]
    fn the_video_session_declares_ptp_and_nothing_else() {
        let body = setup_body("192.168.1.88", "S-UUID", "C-UUID", "02:11:22:33:44:55");
        assert_eq!(body.get("timingProtocol"), Some(&queue::text("PTP")));
        let mut all = Vec::new();
        strings(&body, &mut all);
        assert!(!all.iter().any(|s| s.contains("NTP") || s == "timingPort"), "{all:?}");
        let peer = body.get("timingPeerInfo").unwrap();
        assert_eq!(peer.get("ID"), Some(&queue::text("C-UUID")));
        assert_eq!(peer.get("Addresses"), Some(&Value::Array(vec![queue::text("192.168.1.88")])));
        assert_eq!(body.get("timingPeerList"), Some(&Value::Array(vec![peer.clone()])));
        assert_eq!(body.get("sessionCorrelationUUID"), Some(&queue::text("C-UUID")));
        assert_eq!(body.get("deviceID"), body.get("macAddress"));
        assert_eq!(body.get("name"), Some(&queue::text("Tuner")));
        assert_eq!(body.get("isMultiSelectAirPlay"), Some(&Value::Bool(false)));
    }

    #[test]
    fn the_remote_control_stream_and_its_channel_id() {
        assert_eq!(channel_id(Some("ABC-PSI"), "02:11:22:33:44:55"), "ABC-PSI-RCS-1");
        assert_eq!(channel_id(None, "02:11:22:33:44:55"), "02:11:22:33:44:55-RCS-1");
        let body = rcs_body("ABC-PSI-RCS-1", "CLIENT");
        let Some(Value::Array(streams)) = body.get("streams") else { panic!("no streams") };
        let s = &streams[0];
        assert_eq!((s.get("type"), s.get("controlType")), (Some(&Value::Int(130)), Some(&Value::Int(1))));
        assert_eq!(s.get("clientTypeUUID"), Some(&queue::text(RCS_CLIENT_TYPE_UUID)));
        assert!(s.get("dataPort").is_none());
        let reply = queue::dict([("eventPort", Value::Int(51000)), ("skipRecord", Value::Bool(true))]);
        assert_eq!(SetupReply::from_plist(Some(&reply)), SetupReply { event_port: Some(51000), skip_record: true });
        assert_eq!(SetupReply::from_plist(None), SetupReply::default());
    }

    async fn read_one(tv: &mut tokio::io::DuplexStream, buf: &mut Vec<u8>) -> rtsp::Message {
        loop {
            if let Some(m) = rtsp::take_message(buf).unwrap() {
                return m;
            }
            let mut chunk = [0u8; 4096];
            let n = tv.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Two callers and the feedback timer share the connection: the TV never sees a second
    /// request before it answered the first (PR 2899: `/feedback` racing `/command` drops the
    /// connection), and feedback starts only when switched on.
    #[tokio::test(start_paused = true)]
    async fn one_request_in_flight_with_feedback_between() {
        let (ours, mut tv) = tokio::io::duplex(64 * 1024);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
        let control = std::sync::Arc::new(Control::spawn(Connection::over(Box::new(ours), ip, ip), raw_tx));
        let a = tokio::spawn({
            let c = control.clone();
            async move { c.exchange(Request::http("POST", "/command")).await.map(|r| r.status) }
        });
        let b = tokio::spawn({
            let c = control.clone();
            async move { c.exchange(Request::http("POST", "/command")).await.map(|r| r.status) }
        });
        let mut buf = Vec::new();
        let first = read_one(&mut tv, &mut buf).await;
        assert_eq!(first.start, "POST /command HTTP/1.1");
        // nothing else is written while the first waits (time runs: the request timeout is 5 s)
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut peek = [0u8; 16];
        assert!(tokio::time::timeout(Duration::from_millis(10), tv.read(&mut peek)).await.is_err(), "a second request went out early");
        tv.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let second = read_one(&mut tv, &mut buf).await;
        assert_eq!(second.start, "POST /command HTTP/1.1");
        tv.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        assert_eq!((a.await.unwrap(), b.await.unwrap()), (Ok(200), Ok(200)));

        control.feedback(true);
        let fb = read_one(&mut tv, &mut buf).await;
        assert_eq!(fb.start, "POST /feedback RTSP/1.0");
        // a command asked for now waits for the feedback's reply
        let c = tokio::spawn({
            let c = control.clone();
            async move { c.exchange(Request::http("POST", "/command")).await.map(|r| r.status) }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(tokio::time::timeout(Duration::from_millis(10), tv.read(&mut peek)).await.is_err(), "a command overlapped /feedback");
        tv.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", fb.header("CSeq").unwrap()).as_bytes()).await.unwrap();
        let cmd = read_one(&mut tv, &mut buf).await;
        assert_eq!(cmd.start, "POST /command HTTP/1.1");
        tv.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        assert_eq!(c.await.unwrap(), Ok(200));

        // the TV closes: the session hears it
        drop(tv);
        assert_eq!(raw_rx.recv().await, Some(Raw::Closed { channel: Channel::Control, bad: false }));
    }
}
