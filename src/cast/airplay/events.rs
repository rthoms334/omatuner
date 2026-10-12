// Ported from pyatv pyatv/protocols/airplay/channels.py @ b2484099 (PR 2899; MIT, © 2020 Pierre Ståhl).
//! The event channel (DESIGN §40 §7.3 step 3, "Events"): keys `Events-Salt` swapped, every
//! RTSP-framed request acked `RTSP/1.0 200 OK` with its CSeq (else the TV closes after ~25–30 s),
//! requests reassembled across HAP frames up to Content-Length and acked once (castcli #35),
//! `playbackState` / `notification` / `playbackInfo` responses decoded. OWNER: CAST-2.
//!
//! Ported from pyatv PR 2899's `EventChannel.handle_received` / `_dispatch_event` (the ack's
//! headers, the `{params: {data: <bplist>}}` unwrapping) @ b2484099 (MIT), with robkochman's
//! `params.playbackState` form and PR 2846's `kind: response` routing (both MIT, pyatv
//! contributions); foxCast's `event_channel.go` (facts only) for the caps and for acking every
//! request whatever it carries.
//!
//! * The TV connects nothing: Tuner connects to the `eventPort` SETUP named (retried 5× 1 s apart,
//!   pyatv), so ufw's outbound policy covers it.
//! * We read with `Events-Write-Encryption-Key` and write with `Events-Read-Encryption-Key`
//!   ([`super::pair::SessionKeys::events`] already swaps them).
//! * Inbound frames may be any length the header allows (up to 65 535 B; tvOS 27 is claimed to
//!   send more than 1024); a request is reassembled from as many frames as it takes, capped at
//!   [`super::hap::MAX_MESSAGE`]; a frame that doesn't open, or a request that isn't one, ends
//!   the session (`Raw::Closed { bad: true }`).
//! * `updateInfo` and unknown types are acked and ignored (unknown types logged once at debug).

use std::collections::HashSet;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::bplist::{self, Value};
use super::hap::FrameCipher;
use super::queue;
use super::rtsp::{self, CONNECT_TIMEOUT, Message};
use super::session::{Channel, EVENTS_CONNECT_ATTEMPTS, EVENTS_RETRY_DELAY, Raw, Task};
use super::trace::Trace;
use crate::cast::CastErrorKind;
use crate::redact::Secret;

/// `{type: "playbackState", name: …}` (foxCast docs/05 names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Loading,
    Playing,
    Paused,
    Stopped,
}

/// One decoded request from the TV.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    State(PlaybackState),
    /// `{type: "notification", name}`: `currentItemChanged`, `timeJumped`,
    /// `playbackLikelyToKeepUp`, `loadedTimeRangesChanged`, `playbackBufferFull`,
    /// `itemPlayedToEnd`, …
    Notification(String),
    /// `updateInfo`: acked and ignored.
    UpdateInfo,
    /// `{kind: "response", messageID, value}`: the answer to a `playbackInfo` request (PR 2846).
    Response {
        message_id: i64,
        value: Option<Value>,
    },
    /// Any other type (or a state name nobody documents).
    Other(String),
}

/// Decode a request body (PR 2899 `_dispatch_event`): only a binary plist; `{params: {data:
/// <bplist>}}` is unwrapped, any other plist is the event itself.
pub fn decode(body: &[u8]) -> Option<Event> {
    if !body.starts_with(b"bplist00") {
        return None;
    }
    let outer = bplist::decode(body).ok()?;
    let inner = match outer.get("params").and_then(|p| p.get("data")) {
        Some(Value::Data(d)) => bplist::decode(d).ok()?,
        _ => outer,
    };
    from_plist(&inner)
}

/// The event an (unwrapped) plist carries. `kind: response` is checked first (PR 2846); a
/// `playbackState` takes its state from `params.playbackState` (robkochman's form) or `name`,
/// in any case.
pub fn from_plist(v: &Value) -> Option<Event> {
    if !matches!(v, Value::Dict(_)) {
        return None;
    }
    if queue::str_of(v.get("kind")) == Some("response") {
        return Some(Event::Response { message_id: queue::int(v.get("messageID"))?, value: v.get("value").cloned() });
    }
    Some(match queue::str_of(v.get("type"))? {
        "playbackState" => {
            let name = v.get("params").and_then(|p| queue::str_of(p.get("playbackState"))).or_else(|| queue::str_of(v.get("name")))?;
            match name.to_ascii_lowercase().as_str() {
                "loading" => Event::State(PlaybackState::Loading),
                "playing" => Event::State(PlaybackState::Playing),
                "paused" => Event::State(PlaybackState::Paused),
                "stopped" => Event::State(PlaybackState::Stopped),
                _ => Event::Other(format!("playbackState {}", short(name))),
            }
        }
        "notification" => Event::Notification(queue::str_of(v.get("name")).map(short).unwrap_or_default()),
        "updateInfo" => Event::UpdateInfo,
        other => Event::Other(short(other)),
    })
}

/// Device text for a debug line: sanitized and short.
fn short(s: &str) -> String {
    crate::cast::sanitize_name(&s.chars().take(48).collect::<String>(), "?")
}

/// Reassembles the TV's requests from the opened frames: a request may span several frames
/// (castcli #35: one `/command` arrived as 1024 + 1259 bytes on an AppleTV14,1 with 26.6) and a
/// frame may hold several requests.
#[derive(Debug, Default)]
pub struct Assembler {
    buf: Vec<u8>,
}

impl Assembler {
    pub fn push(&mut self, plain: &[u8]) {
        self.buf.extend_from_slice(plain);
    }

    /// The next complete request; `Ok(None)` until it is all here. A message that isn't a request
    /// (`METHOD PATH PROTO`) or breaks a cap is `Malformed`.
    pub fn take(&mut self) -> Result<Option<Message>, rtsp::Error> {
        let Some(m) = rtsp::take_message(&mut self.buf)? else { return Ok(None) };
        let parts: Vec<&str> = m.start.split(' ').collect();
        if parts.len() != 3 || !parts[2].contains('/') {
            return Err(rtsp::Error::Malformed);
        }
        Ok(Some(m))
    }
}

/// The ack (PR 2899 `handle_received`): `200 OK` in the request's protocol with
/// `Content-Length: 0`, `Audio-Latency: 0`, and the request's `Server` and `CSeq` echoed.
pub fn ack(req: &Message) -> Vec<u8> {
    let proto = req.start.rsplit(' ').next().unwrap_or("RTSP/1.0");
    let mut out = format!("{proto} 200 OK\r\nContent-Length: 0\r\nAudio-Latency: 0\r\n");
    if let Some(server) = req.header("Server") {
        out.push_str(&format!("Server: {server}\r\n"));
    }
    if let Some(cseq) = req.header("CSeq") {
        out.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Connect to the event port (5 attempts, 1 s apart, pyatv) and serve the channel on a task:
/// `keys` is `(write, read)` for us, already swapped.
pub async fn open(
    addr: SocketAddr,
    keys: (Secret<[u8; 32]>, Secret<[u8; 32]>),
    raw: mpsc::UnboundedSender<Raw>,
    trace: Option<Trace>,
) -> Result<Task, CastErrorKind> {
    let mut attempt = 1;
    let stream = loop {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => break s,
            _ if attempt < EVENTS_CONNECT_ATTEMPTS => {
                tracing::debug!("cast: the event channel didn't connect (attempt {attempt}); retrying");
                attempt += 1;
                tokio::time::sleep(EVENTS_RETRY_DELAY).await;
            }
            _ => {
                tracing::info!("cast: the event channel didn't connect");
                return Err(CastErrorKind::Unreachable);
            }
        }
    };
    let _ = stream.set_nodelay(true);
    let cipher = FrameCipher::new(&keys.0, &keys.1);
    Ok(Task::spawn(serve(stream, cipher, raw, trace)))
}

/// Read, open, reassemble, ack, decode, forward — until the TV closes the channel or sends what
/// can't be read.
async fn serve(mut stream: TcpStream, mut cipher: FrameCipher, raw: mpsc::UnboundedSender<Raw>, trace: Option<Trace>) {
    let mut sealed = Vec::new();
    let mut requests = Assembler::default();
    let mut seen = HashSet::new();
    let mut buf = vec![0u8; 16 * 1024];
    let bad = 'serve: loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break false,
            Ok(n) => n,
        };
        sealed.extend_from_slice(&buf[..n]);
        // the frames before a bad one still count: their requests are acked and delivered first
        let mut broken = false;
        loop {
            match cipher.open(&mut sealed) {
                Ok(Some(plain)) => requests.push(&plain),
                Ok(None) => break,
                Err(e) => {
                    tracing::info!("cast: an event frame didn't open ({e:?})");
                    broken = true;
                    break;
                }
            }
        }
        loop {
            let req = match requests.take() {
                Ok(Some(req)) => req,
                Ok(None) => break,
                Err(_) => {
                    tracing::info!("cast: the TV sent an event that isn't a request");
                    break 'serve true;
                }
            };
            if let Some(t) = &trace {
                t.incoming(Channel::Events, &req);
            }
            let acked = tokio::time::timeout(rtsp::REQUEST_TIMEOUT, stream.write_all(&cipher.seal(&ack(&req)))).await;
            if !matches!(acked, Ok(Ok(()))) {
                break 'serve false;
            }
            match decode(&req.body) {
                Some(Event::UpdateInfo) | None => {}
                Some(Event::Other(kind)) => {
                    if seen.insert(kind.clone()) {
                        tracing::debug!("cast: ignoring the TV's {kind} events");
                    }
                }
                Some(ev) => {
                    match &ev {
                        Event::Response { message_id, .. } => tracing::debug!("cast: the answer to request {message_id}"),
                        ev => tracing::debug!("cast: event {ev:?}"),
                    }
                    let _ = raw.send(Raw::Event(ev));
                }
            }
        }
        if broken {
            break true;
        }
    };
    let _ = raw.send(Raw::Closed { channel: Channel::Events, bad });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, Value)]) -> Value {
        queue::dict(pairs.iter().cloned())
    }

    /// castcli #35: one request split 1024 + 1259 bytes over two frames is one request, acked
    /// once with its CSeq; two requests in one frame are two acks.
    #[test]
    fn requests_are_reassembled_and_acked_once() {
        let body = vec![b'x'; 2283 - 80];
        let mut wire =
            format!("POST /command RTSP/1.0\r\nCSeq: 41\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n", body.len())
                .into_bytes();
        wire.extend_from_slice(&body);
        let mut asm = Assembler::default();
        asm.push(&wire[..1024]);
        assert_eq!(asm.take(), Ok(None), "half a request is not a request");
        asm.push(&wire[1024..]);
        let req = asm.take().unwrap().unwrap();
        assert_eq!(req.body.len(), body.len());
        assert_eq!(asm.take(), Ok(None));
        let ack = String::from_utf8(ack(&req)).unwrap();
        assert!(ack.starts_with("RTSP/1.0 200 OK\r\n"), "{ack}");
        assert!(ack.contains("\r\nCSeq: 41\r\n") && ack.contains("Content-Length: 0\r\n") && ack.ends_with("\r\n\r\n"), "{ack}");

        asm.push(b"POST /command RTSP/1.0\r\nCSeq: 1\r\nServer: AirTunes/960.13.1\r\n\r\nPOST /command RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        let first = asm.take().unwrap().unwrap();
        assert!(String::from_utf8(super::ack(&first)).unwrap().contains("Server: AirTunes/960.13.1\r\nCSeq: 1\r\n"));
        assert_eq!(asm.take().unwrap().unwrap().header("CSeq"), Some("2"));

        asm.push(b"RTSP/1.0 200 OK\r\n\r\n");
        assert_eq!(asm.take(), Err(rtsp::Error::Malformed), "a reply is not a request");
    }

    #[test]
    fn events_decode_in_every_documented_form() {
        let state = |name: &str| dict(&[("type", queue::text("playbackState")), ("name", queue::text(name))]);
        assert_eq!(from_plist(&state("playing")), Some(Event::State(PlaybackState::Playing)));
        assert_eq!(from_plist(&state("Loading")), Some(Event::State(PlaybackState::Loading)));
        assert_eq!(from_plist(&state("stopped")), Some(Event::State(PlaybackState::Stopped)));
        // robkochman: the state in params.playbackState
        let params = dict(&[("type", queue::text("playbackState")), ("params", dict(&[("playbackState", queue::text("paused"))]))]);
        assert_eq!(from_plist(&params), Some(Event::State(PlaybackState::Paused)));
        let end = dict(&[("type", queue::text("notification")), ("name", queue::text("itemPlayedToEnd"))]);
        assert_eq!(from_plist(&end), Some(Event::Notification("itemPlayedToEnd".into())));
        assert_eq!(from_plist(&dict(&[("type", queue::text("updateInfo"))])), Some(Event::UpdateInfo));
        assert_eq!(from_plist(&dict(&[("type", queue::text("mystery"))])), Some(Event::Other("mystery".into())));
        // PR 2846: a response, whatever its type
        let value = dict(&[("position", Value::Real(12.5))]);
        let resp =
            dict(&[("kind", queue::text("response")), ("type", queue::text("playbackInfo")), ("messageID", Value::Int(3)), ("value", value.clone())]);
        assert_eq!(from_plist(&resp), Some(Event::Response { message_id: 3, value: Some(value) }));
        assert_eq!(from_plist(&Value::Array(vec![])), None);
        assert_eq!(decode(b"not a plist"), None);
    }
}
