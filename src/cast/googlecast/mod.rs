//! The Google Cast client (DESIGN §40 §15.3, Phase 2): CASTV2 to Chromecasts, Google TVs and TVs
//! with Cast built in, in-tree (not `rust_cast`: it panics in `connect` and hides its socket, so
//! it can't have timeouts [GC B.3]). The TV plays the relay's HLS in the Default Media Receiver
//! (`CC1AD845`), so the pipeline is AirPlay's with a Cast profile (`relay::plan`).
//!
//! ```text
//!  Client ──frames──▶ writer task ──TLS :8009 (self-signed; not verified)──▶ the TV
//!     ▲                  └─ PING to receiver-0 every 5 s
//!     └── inbox ◀── reader task ◀── frames (answers PING with PONG; 20 s of silence = gone)
//! ```
//! * **Connect**: TCP + TLS (5 s each), `CONNECT` to `receiver-0`, `LAUNCH` the Default Media
//!   Receiver, wait for a `RECEIVER_STATUS` naming it (its `transportId` and `sessionId`, 20 s),
//!   `CONNECT` to its transport.
//! * **Load / replace**: `LOAD` on the media namespace (the URL, `streamType` LIVE or BUFFERED,
//!   `hlsSegmentFormat`/`hlsVideoSegmentFormat` for TS or fMP4 segments, `autoplay`); its answer
//!   is the `MEDIA_STATUS` with our `requestId` (the new `mediaSessionId`), or `LOAD_FAILED`.
//!   A replace is another `LOAD` on the same session (the old media session ends `INTERRUPTED`,
//!   which is ignored).
//! * **Events** (`MEDIA_STATUS` of the current media session): `LOADING`/`BUFFERING` → `Loading`
//!   before the first picture, `Buffering(true)` after; `PLAYING` → `Playing`; `PAUSED` →
//!   `Paused`; `IDLE` + `FINISHED` → `Ended`, `CANCELLED` (the remote, another sender) →
//!   `StoppedOnTv`, `ERROR` → `Lost(ReceiverFailed)`. A `RECEIVER_STATUS` without our session
//!   (the remote's Home, another app) or a `CLOSE` from its transport → `StoppedOnTv` once
//!   something played, else `Lost(Unreachable)`; so is the connection going away.
//! * **Positions**: films and catch-up ask `GET_STATUS` every 10 s while playing; every
//!   `MEDIA_STATUS` with a `currentTime` is reported (the TV's own clock: always "reported").
//! * **Volume**: the receiver's `SET_VOLUME` (0–1).
//!
//! The certificate isn't checked (every Cast device's is self-signed by Google's device CA, which
//! senders don't verify either): what crosses the connection is the relay's one-generation LAN
//! URL, as on AirPlay (DESIGN §40 §9.2).

pub mod proto;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::CastErrorKind;
use super::receiver::{LoadReq, ReceiverEvent, ReceiverInfo};
use proto::CastMessage;

/// The Cast control port (the SRV record's port is used when discovery found one).
pub const PORT: u16 = 8009;
/// The Default Media Receiver.
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
/// Our end of every virtual connection, and the platform's.
pub const SENDER: &str = "sender-0";
pub const PLATFORM: &str = "receiver-0";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A request's answer (`GET_STATUS`, `PAUSE`…).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// `LAUNCH` until the receiver app runs, and `LOAD` until the media loaded (the TV fetches the
/// playlist and its first segments first: the controller's load budget is 30 s).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const LOAD_TIMEOUT: Duration = Duration::from_secs(25);
const HEARTBEAT: Duration = Duration::from_secs(5);
/// Nothing at all from the TV this long (it answers every PING): the connection is gone.
const SILENCE: Duration = Duration::from_secs(20);
/// Films and catch-up: a `GET_STATUS` this often while playing.
const SAMPLE: Duration = Duration::from_secs(10);

/// What the reader task hands the client.
#[derive(Debug)]
enum Inbound {
    Message(CastMessage),
    /// The connection closed, broke, or went silent.
    Closed,
}

/// The receiver app's session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct App {
    transport: String,
    session: String,
}

/// The item the TV has now.
#[derive(Debug, Clone, PartialEq)]
struct Item {
    live: bool,
    media_session: Option<i64>,
    /// The last `playerState` reported (repeats are dropped).
    state: Option<String>,
    played: bool,
    ended: bool,
}

/// One session with a Cast device (the app stays up across zaps).
pub struct Client(Box<State>);

struct State {
    addr: SocketAddr,
    info: ReceiverInfo,
    out: mpsc::UnboundedSender<CastMessage>,
    inbox: mpsc::UnboundedReceiver<Inbound>,
    tasks: [JoinHandle<()>; 2],
    app: Option<App>,
    item: Option<Item>,
    /// Media sessions of items replaced in this session (their late statuses are ignored).
    stale: Vec<i64>,
    pending: VecDeque<ReceiverEvent>,
    next_id: i64,
    /// Something played in this session: losing the app is a stop from the TV, not a failure.
    played: bool,
    done: bool,
    next_sample: Option<Instant>,
}

impl Drop for State {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Client {
    /// TLS, `CONNECT`, `LAUNCH` the Default Media Receiver, `CONNECT` to it. `model` is the
    /// device's own (`md`), for the session's log line.
    pub async fn connect(addr: SocketAddr, model: Option<String>) -> Result<Self, CastErrorKind> {
        State::connect(addr, ReceiverInfo { model, ..ReceiverInfo::default() }).await.map(|s| Self(Box::new(s)))
    }

    /// `LOAD` (a replace when an item plays: the same session, a new media session).
    pub async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.load(req).await
    }

    pub async fn replace(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.load(req).await
    }

    /// `PAUSE` / `PLAY`.
    pub async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        self.0.set_paused(paused).await
    }

    /// The receiver's `SET_VOLUME` (0–1).
    pub async fn set_volume(&mut self, level: f64) -> Option<()> {
        self.0.set_volume(level).await
    }

    /// `GET_STATUS` now: the TV's `currentTime`.
    pub async fn position(&mut self) -> Option<f64> {
        self.0.position().await
    }

    /// Cancel-safe.
    pub async fn next_event(&mut self) -> ReceiverEvent {
        self.0.next_event().await
    }

    /// `STOP` the app (the TV goes back to its home screen) and close.
    pub async fn stop(&mut self) {
        self.0.stop().await
    }

    pub fn info(&self) -> ReceiverInfo {
        self.0.info.clone()
    }
}

/// A TLS client that takes any certificate (module docs).
fn tls() -> Result<tokio_rustls::TlsConnector, CastErrorKind> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| CastErrorKind::Unreachable)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// Every Cast device's certificate is self-signed: the handshake's signatures are still checked
/// (by the provider's algorithms), the chain is not.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Open the TLS connection and start its two tasks.
async fn open(
    addr: SocketAddr,
) -> Result<(mpsc::UnboundedSender<CastMessage>, mpsc::UnboundedReceiver<Inbound>, [JoinHandle<()>; 2]), CastErrorKind> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.map_err(|_| CastErrorKind::Unreachable)?.map_err(|e| {
        tracing::debug!("cast: Google Cast connect: {e}");
        CastErrorKind::Unreachable
    })?;
    let _ = tcp.set_nodelay(true);
    let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, tls()?.connect(name, tcp)).await.map_err(|_| CastErrorKind::Unreachable)?.map_err(|e| {
        tracing::debug!("cast: Google Cast TLS: {e}");
        CastErrorKind::Unreachable
    })?;
    let (mut rd, mut wr) = tokio::io::split(tls);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<CastMessage>();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(async move {
        let ping = CastMessage::json(SENDER, PLATFORM, NS_HEARTBEAT, &json!({"type": "PING"}));
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.tick().await;
        loop {
            let msg = tokio::select! {
                m = out_rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                _ = beat.tick() => ping.clone(),
            };
            if wr.write_all(&msg.frame()).await.is_err() || wr.flush().await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    let pong_tx = out_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let mut head = [0u8; 4];
            let msg = tokio::time::timeout(SILENCE, async {
                rd.read_exact(&mut head).await.ok()?;
                let mut body = vec![0u8; proto::frame_len(head).ok()?];
                rd.read_exact(&mut body).await.ok()?;
                CastMessage::decode(&body).ok()
            })
            .await;
            let Ok(Some(msg)) = msg else { break };
            if msg.namespace == NS_HEARTBEAT {
                if msg.value()["type"] == "PING" {
                    let _ = pong_tx.send(CastMessage::json(SENDER, &msg.source, NS_HEARTBEAT, &json!({"type": "PONG"})));
                }
                continue;
            }
            if in_tx.send(Inbound::Message(msg)).is_err() {
                return;
            }
        }
        let _ = in_tx.send(Inbound::Closed);
    });
    Ok((out_tx, in_rx, [writer, reader]))
}

impl State {
    async fn connect(addr: SocketAddr, info: ReceiverInfo) -> Result<Self, CastErrorKind> {
        let (out, inbox, tasks) = open(addr).await?;
        let mut s = Self {
            addr,
            info,
            out,
            inbox,
            tasks,
            app: None,
            item: None,
            stale: Vec::new(),
            pending: VecDeque::new(),
            next_id: 0,
            played: false,
            done: false,
            next_sample: None,
        };
        s.launch().await?;
        Ok(s)
    }

    fn send(&self, to: &str, ns: &str, payload: Value) {
        let _ = self.out.send(CastMessage::json(SENDER, to, ns, &payload));
    }

    /// `CONNECT` to the platform, `LAUNCH` the Default Media Receiver, `CONNECT` to it.
    async fn launch(&mut self) -> Result<(), CastErrorKind> {
        self.send(PLATFORM, NS_CONNECTION, connect_payload());
        let id = self.id();
        self.send(PLATFORM, NS_RECEIVER, json!({"type": "LAUNCH", "requestId": id, "appId": DEFAULT_MEDIA_RECEIVER}));
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            let msg = match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Err(_) => {
                    tracing::info!("cast: the Cast device didn't start its media receiver in time");
                    return Err(CastErrorKind::Unreachable);
                }
                Ok(None | Some(Inbound::Closed)) => return Err(CastErrorKind::Unreachable),
                Ok(Some(Inbound::Message(m))) => m,
            };
            let v = msg.value();
            if msg.namespace != NS_RECEIVER {
                continue;
            }
            match v["type"].as_str() {
                Some("LAUNCH_ERROR") | Some("INVALID_REQUEST") if v["requestId"].as_i64() == Some(id) => {
                    tracing::info!("cast: the Cast device refused to start its media receiver ({})", v["reason"].as_str().unwrap_or("?"));
                    return Err(CastErrorKind::Unsupported);
                }
                Some("RECEIVER_STATUS") => {
                    if let Some(app) = app_of(&v) {
                        self.send(&app.transport, NS_CONNECTION, connect_payload());
                        self.app = Some(app);
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }

    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// Send a request and wait for the reply carrying its `requestId`; everything else that
    /// comes meanwhile is absorbed as events. `Err` when the connection closes or time runs out.
    async fn request(&mut self, to: &str, ns: &str, mut payload: Value, within: Duration) -> Result<Value, CastErrorKind> {
        let id = self.id();
        payload["requestId"] = json!(id);
        self.send(to, ns, payload);
        let deadline = Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Err(_) => return Err(CastErrorKind::Unreachable),
                Ok(None | Some(Inbound::Closed)) => {
                    self.closed();
                    return Err(CastErrorKind::Unreachable);
                }
                Ok(Some(Inbound::Message(m))) => {
                    let v = m.value();
                    if v["requestId"].as_i64() == Some(id) {
                        // a MEDIA_STATUS answer counts as an event too (the state it reports)
                        if m.namespace == NS_MEDIA && v["type"] == "MEDIA_STATUS" {
                            self.absorb(m);
                        }
                        return Ok(v);
                    }
                    self.absorb(m);
                }
            }
        }
    }

    async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        if self.done || self.app.is_none() {
            self.restart().await?;
        }
        let app = self.app.clone().ok_or(CastErrorKind::Unreachable)?;
        // what the TV says about the item being replaced isn't the new one's
        if let Some(old) = self.item.as_ref().and_then(|i| i.media_session) {
            self.stale.push(old);
        }
        self.item = Some(Item { live: req.live, media_session: None, state: None, played: false, ended: false });
        self.next_sample = None;
        let payload = load_payload(&app.session, req);
        let v = match self.request(&app.transport, NS_MEDIA, payload, LOAD_TIMEOUT).await {
            Ok(v) => v,
            Err(e) => {
                tracing::info!("cast: the Cast device didn't answer LOAD");
                return Err(e);
            }
        };
        match v["type"].as_str() {
            Some("MEDIA_STATUS") => {
                let session = v["status"].as_array().and_then(|s| s.first()).and_then(|s| s["mediaSessionId"].as_i64());
                if let Some(item) = self.item.as_mut() {
                    item.media_session = session.or(item.media_session);
                }
                if !req.live {
                    self.next_sample = Some(Instant::now() + SAMPLE);
                }
                Ok(())
            }
            other => {
                tracing::info!("cast: the Cast device refused the item ({})", other.unwrap_or("?"));
                Err(CastErrorKind::ReceiverFailed)
            }
        }
    }

    async fn restart(&mut self) -> Result<(), CastErrorKind> {
        for t in &self.tasks {
            t.abort();
        }
        let (out, inbox, tasks) = open(self.addr).await.inspect_err(|_| self.done = true)?;
        (self.out, self.inbox, self.tasks) = (out, inbox, tasks);
        (self.app, self.item, self.played, self.done, self.next_sample) = (None, None, false, false, None);
        self.stale.clear();
        self.pending.clear();
        self.launch().await.inspect_err(|_| self.done = true)
    }

    fn media_session(&self) -> Option<(App, i64)> {
        Some((self.app.clone()?, self.item.as_ref()?.media_session?))
    }

    async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        let (app, media) = self.media_session().ok_or(CastErrorKind::ReceiverFailed)?;
        let kind = if paused { "PAUSE" } else { "PLAY" };
        let v = self.request(&app.transport, NS_MEDIA, json!({"type": kind, "mediaSessionId": media}), REQUEST_TIMEOUT).await?;
        if v["type"] == "MEDIA_STATUS" {
            Ok(())
        } else {
            tracing::info!("cast: {kind} answered {}", v["type"].as_str().unwrap_or("?"));
            Err(CastErrorKind::ReceiverFailed)
        }
    }

    async fn set_volume(&mut self, level: f64) -> Option<()> {
        if self.done {
            return None;
        }
        let payload = json!({"type": "SET_VOLUME", "volume": {"level": level.clamp(0.0, 1.0)}});
        let v = self.request(PLATFORM, NS_RECEIVER, payload, REQUEST_TIMEOUT).await.ok()?;
        (v["type"] == "RECEIVER_STATUS").then_some(())
    }

    async fn position(&mut self) -> Option<f64> {
        let (app, media) = self.media_session()?;
        if self.done {
            return None;
        }
        let v = self.request(&app.transport, NS_MEDIA, json!({"type": "GET_STATUS", "mediaSessionId": media}), REQUEST_TIMEOUT).await.ok()?;
        v["status"].as_array()?.iter().find(|s| s["mediaSessionId"].as_i64() == Some(media))?["currentTime"].as_f64()
    }

    async fn next_event(&mut self) -> ReceiverEvent {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return ev;
            }
            if self.done {
                return std::future::pending().await;
            }
            let sample = self.next_sample;
            tokio::select! {
                m = self.inbox.recv() => match m {
                    Some(Inbound::Message(m)) => self.absorb(m),
                    None | Some(Inbound::Closed) => self.closed(),
                },
                () = sleep_until(sample) => self.sample(),
            }
        }
    }

    /// The 10 s position request (its answer arrives as a `MEDIA_STATUS`, reported then).
    fn sample(&mut self) {
        self.next_sample = Some(Instant::now() + SAMPLE);
        let Some((app, media)) = self.media_session() else { return };
        if self.item.as_ref().is_some_and(|i| i.played && !i.ended) {
            let id = self.id();
            self.send(&app.transport, NS_MEDIA, json!({"type": "GET_STATUS", "requestId": id, "mediaSessionId": media}));
        }
    }

    async fn stop(&mut self) {
        self.next_sample = None;
        if let Some(app) = self.app.take()
            && !self.done
        {
            let _ = self.request(PLATFORM, NS_RECEIVER, json!({"type": "STOP", "sessionId": app.session}), Duration::from_secs(1)).await;
            self.send(&app.transport, NS_CONNECTION, json!({"type": "CLOSE"}));
        }
        self.send(PLATFORM, NS_CONNECTION, json!({"type": "CLOSE"}));
        self.done = true;
        self.item = None;
        // the writer sends what's queued, then closes (the reader's PONG sender goes first)
        self.tasks[1].abort();
        let (out, _) = mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.out, out));
        let _ = tokio::time::timeout(Duration::from_millis(300), &mut self.tasks[0]).await;
    }

    /// The connection is gone.
    fn closed(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.next_sample = None;
        tracing::info!("cast: the Cast device closed the connection");
        self.pending.push_back(ReceiverEvent::Lost(CastErrorKind::Unreachable));
    }

    /// The app this session runs went away (the remote's Home, another sender's app, a `CLOSE`).
    fn app_gone(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.next_sample = None;
        self.app = None;
        let ev = if self.played { ReceiverEvent::StoppedOnTv } else { ReceiverEvent::Lost(CastErrorKind::Unreachable) };
        self.pending.push_back(ev);
    }

    fn absorb(&mut self, m: CastMessage) {
        let v = m.value();
        match (m.namespace.as_str(), v["type"].as_str()) {
            (NS_CONNECTION, Some("CLOSE")) if self.app.as_ref().is_some_and(|a| a.transport == m.source) => self.app_gone(),
            (NS_RECEIVER, Some("RECEIVER_STATUS")) => {
                let ours = self.app.as_ref().map(|a| a.session.clone());
                let running =
                    v["status"]["applications"].as_array().is_some_and(|apps| apps.iter().any(|a| a["sessionId"].as_str() == ours.as_deref()));
                if ours.is_some() && !running {
                    tracing::info!("cast: the media receiver isn't running on the Cast device any more");
                    self.app_gone();
                }
            }
            (NS_MEDIA, Some("MEDIA_STATUS")) => {
                for s in v["status"].as_array().into_iter().flatten() {
                    self.media_status(s);
                }
            }
            (NS_MEDIA, Some("LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "INVALID_PLAYER_STATE"))
                if self.item.as_ref().is_some_and(|i| !i.played && i.media_session.is_none()) =>
            {
                tracing::info!("cast: the Cast device couldn't load the item ({})", v["type"].as_str().unwrap_or("?"));
                self.pending.push_back(ReceiverEvent::Lost(CastErrorKind::ReceiverFailed));
            }
            _ => {}
        }
    }

    /// One media session's status (only the current item's counts).
    fn media_status(&mut self, s: &Value) {
        let session = s["mediaSessionId"].as_i64();
        if session.is_some_and(|id| self.stale.contains(&id)) {
            return;
        }
        let Some(item) = self.item.as_mut() else { return };
        match (item.media_session, session) {
            (Some(ours), Some(theirs)) if ours != theirs => return,
            (None, Some(theirs)) => item.media_session = Some(theirs),
            _ => {}
        }
        let mut out = Vec::new();
        if let Some(state) = s["playerState"].as_str()
            && item.state.as_deref() != Some(state)
        {
            item.state = Some(state.to_owned());
            match state {
                "LOADING" | "BUFFERING" if !item.played => out.push(ReceiverEvent::Loading),
                "BUFFERING" => out.push(ReceiverEvent::Buffering(true)),
                "PLAYING" => {
                    item.played = true;
                    out.push(ReceiverEvent::Playing);
                }
                "PAUSED" if item.played => out.push(ReceiverEvent::Paused),
                "IDLE" => match s["idleReason"].as_str() {
                    Some("FINISHED") if !item.ended => {
                        item.ended = true;
                        out.push(ReceiverEvent::Ended);
                    }
                    Some("CANCELLED") => out.push(ReceiverEvent::StoppedOnTv),
                    Some("ERROR") => {
                        tracing::info!("cast: the Cast device's player failed");
                        out.push(ReceiverEvent::Lost(CastErrorKind::ReceiverFailed));
                    }
                    // INTERRUPTED: replaced (by our next LOAD); none: not loaded yet
                    _ => {}
                },
                _ => {}
            }
        }
        if !item.live
            && item.played
            && let Some(pos) = s["currentTime"].as_f64()
        {
            let dur = s["media"]["duration"].as_f64().filter(|d| d.is_finite() && *d > 0.0);
            out.push(ReceiverEvent::Position { pos, dur });
        }
        if item.played {
            self.played = true;
        }
        let finished = out.iter().any(|e| matches!(e, ReceiverEvent::StoppedOnTv | ReceiverEvent::Lost(_)));
        self.pending.extend(out);
        if finished {
            self.done = true;
            self.next_sample = None;
        }
    }
}

/// `CONNECT` as pychromecast and go-chromecast send it.
fn connect_payload() -> Value {
    json!({"type": "CONNECT", "origin": {}, "userAgent": "Tuner", "senderInfo": {"sdkType": 2, "version": "15.605.1.3", "browserVersion": "44.0.2403.30", "platform": 4, "systemVersion": "Linux", "connectionType": 1}})
}

/// The Default Media Receiver's app in a `RECEIVER_STATUS`.
fn app_of(v: &Value) -> Option<App> {
    let a = v["status"]["applications"].as_array()?.iter().find(|a| a["appId"] == DEFAULT_MEDIA_RECEIVER)?;
    Some(App { transport: a["transportId"].as_str()?.to_owned(), session: a["sessionId"].as_str()?.to_owned() })
}

/// The `LOAD` of the relay's playlist (the URL is a bearer token: it goes only to the TV).
pub fn load_payload(session: &str, req: &LoadReq) -> Value {
    let url = req.url.expose();
    let (segments, video) = if req.fmp4 { ("fmp4", "fmp4") } else { ("ts", "mpeg2_ts") };
    json!({
        "type": "LOAD",
        "sessionId": session,
        "media": {
            "contentId": url,
            "contentUrl": url,
            "contentType": "application/vnd.apple.mpegurl",
            "streamType": if req.live { "LIVE" } else { "BUFFERED" },
            "hlsSegmentFormat": segments,
            "hlsVideoSegmentFormat": video,
            "metadata": {"metadataType": 0, "title": "Tuner"},
        },
        "autoplay": true,
    })
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::Secret;

    #[test]
    fn load_names_the_segments_and_the_stream_type() {
        let req = LoadReq { url: Secret::new("http://192.0.2.1:47800/c/tok/index.m3u8".into()), live: true, offset_s: 0.0, fmp4: false };
        let v = load_payload("sess", &req);
        assert_eq!(v["media"]["streamType"], "LIVE");
        assert_eq!((v["media"]["hlsSegmentFormat"].as_str(), v["media"]["hlsVideoSegmentFormat"].as_str()), (Some("ts"), Some("mpeg2_ts")));
        assert_eq!(v["media"]["contentId"], "http://192.0.2.1:47800/c/tok/index.m3u8");
        let v = load_payload("sess", &LoadReq { live: false, fmp4: true, ..req });
        assert_eq!((v["media"]["streamType"].as_str(), v["media"]["hlsVideoSegmentFormat"].as_str()), (Some("BUFFERED"), Some("fmp4")));
    }

    #[test]
    fn the_app_is_found_in_a_receiver_status() {
        let v = json!({"type": "RECEIVER_STATUS", "status": {"applications": [
            {"appId": "E8C28D3C", "transportId": "web-1", "sessionId": "other"},
            {"appId": "CC1AD845", "transportId": "web-5", "sessionId": "abc"},
        ]}});
        assert_eq!(app_of(&v), Some(App { transport: "web-5".into(), session: "abc".into() }));
        assert_eq!(app_of(&json!({"status": {"applications": []}})), None);
        assert_eq!(app_of(&json!({"status": {}})), None);
    }
}
