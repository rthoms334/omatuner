//! A fake Google Cast device for `tests/cast_googlecast.rs` (DESIGN §40 Appendix E): CASTV2 over
//! TLS with a self-signed certificate (`tests/fixtures/cast/googlecast/`), the platform's
//! receiver namespace (LAUNCH, GET_STATUS, STOP, SET_VOLUME), the Default Media Receiver's media
//! namespace (LOAD, PLAY, PAUSE, GET_STATUS) and heartbeat. Built from pychromecast's,
//! go-chromecast's and node-castv2-client's message shapes (the senders that work against real
//! devices), not from a capture of one TV; a test scripts its replies and pushes the TV's own
//! statuses (the remote's stop, the end of an item, the app going away).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuner::cast::googlecast::proto::{self, CastMessage};
use tuner::cast::googlecast::{DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER, PLATFORM};

/// The app's transport and session ids.
pub const TRANSPORT: &str = "web-7";
pub const SESSION: &str = "7c1a2b3c-0000-4000-8000-00000000cafe";

/// How the fake answers.
#[derive(Debug, Clone)]
pub struct Script {
    /// LAUNCH answers LAUNCH_ERROR.
    pub launch_error: bool,
    /// A RECEIVER_STATUS without the app comes first (a TV still starting it).
    pub slow_launch: bool,
    /// LOAD answers LOAD_FAILED.
    pub load_failed: bool,
    /// After a LOAD's answer, an unsolicited PLAYING follows.
    pub play_after_load: bool,
    /// What GET_STATUS reports.
    pub position: f64,
    pub duration: Option<f64>,
}

impl Default for Script {
    fn default() -> Self {
        Self { launch_error: false, slow_launch: false, load_failed: false, play_after_load: true, position: 0.0, duration: Some(5400.0) }
    }
}

/// What the fake received, in order (heartbeats left out).
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub namespace: String,
    pub destination: String,
    pub payload: Value,
}

#[derive(Default)]
struct State {
    script: Script,
    log: Vec<Received>,
    media_session: i64,
    app: bool,
    volume: Option<f64>,
    connections: usize,
    /// Pushes to every open connection.
    pushes: Vec<mpsc::UnboundedSender<Push>>,
}

#[derive(Debug, Clone)]
enum Push {
    Message(CastMessage),
    Close,
}

/// The running fake.
pub struct FakeCast {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
}

impl Drop for FakeCast {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
    let cert = rustls::pki_types::CertificateDer::from(include_bytes!("../fixtures/cast/googlecast/cert.der").to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(include_bytes!("../fixtures/cast/googlecast/key.der").to_vec().into());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("the fixture certificate");
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

impl FakeCast {
    pub async fn start(script: Script) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(State { script, ..State::default() }));
        let stop = CancellationToken::new();
        let acceptor = tls_acceptor();
        let (st, token) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                let (tcp, _) = tokio::select! {
                    _ = token.cancelled() => return,
                    r = listener.accept() => match r {
                        Ok(c) => c,
                        Err(_) => continue,
                    },
                };
                let (acceptor, st, token) = (acceptor.clone(), st.clone(), token.clone());
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        serve(tls, st, token).await;
                    }
                });
            }
        });
        Self { addr, state, stop }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn script(&self, f: impl FnOnce(&mut Script)) {
        f(&mut self.lock().script);
    }

    /// Everything received (heartbeats aside).
    pub fn log(&self) -> Vec<Received> {
        self.lock().log.clone()
    }

    /// The `type`s received on `namespace`, in order.
    pub fn types(&self, namespace: &str) -> Vec<String> {
        self.log().iter().filter(|r| r.namespace == namespace).filter_map(|r| r.payload["type"].as_str().map(str::to_owned)).collect()
    }

    pub fn volume(&self) -> Option<f64> {
        self.lock().volume
    }

    pub fn connections(&self) -> usize {
        self.lock().connections
    }

    pub fn media_session(&self) -> i64 {
        self.lock().media_session
    }

    fn push(&self, p: Push) {
        self.lock().pushes.retain(|tx| tx.send(p.clone()).is_ok());
    }

    /// The current item's status as the TV would send it unasked.
    pub fn media_status(&self, player_state: &str, idle_reason: Option<&str>, current_time: f64) {
        let session = self.media_session();
        let mut s = json!({"mediaSessionId": session, "playerState": player_state, "currentTime": current_time});
        if let Some(r) = idle_reason {
            s["idleReason"] = json!(r);
        }
        let msg = CastMessage::json(TRANSPORT, "*", NS_MEDIA, &json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [s]}));
        self.push(Push::Message(msg));
    }

    /// The app went away (the remote's Home, another sender's app): a RECEIVER_STATUS without it.
    pub fn app_gone(&self) {
        self.lock().app = false;
        let msg = CastMessage::json(PLATFORM, "*", NS_RECEIVER, &receiver_status(false, None, 0));
        self.push(Push::Message(msg));
    }

    /// The app's transport closes our virtual connection.
    pub fn close_transport(&self) {
        let msg = CastMessage::json(TRANSPORT, "sender-0", NS_CONNECTION, &json!({"type": "CLOSE"}));
        self.push(Push::Message(msg));
    }

    /// Every TLS connection is cut.
    pub fn drop_connections(&self) {
        self.push(Push::Close);
    }
}

fn receiver_status(app: bool, volume: Option<f64>, request: i64) -> Value {
    let apps = if app {
        json!([{"appId": DEFAULT_MEDIA_RECEIVER, "displayName": "Default Media Receiver", "sessionId": SESSION, "transportId": TRANSPORT, "statusText": "Ready To Cast"}])
    } else {
        json!([])
    };
    json!({"type": "RECEIVER_STATUS", "requestId": request, "status": {"applications": apps, "volume": {"level": volume.unwrap_or(0.5), "muted": false}}})
}

async fn serve(tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>, st: Arc<Mutex<State>>, stop: CancellationToken) {
    let (mut rd, mut wr) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
        s.connections += 1;
        s.pushes.push(tx.clone());
    }
    let reader = async {
        loop {
            let mut head = [0u8; 4];
            if rd.read_exact(&mut head).await.is_err() {
                return;
            }
            let Ok(len) = proto::frame_len(head) else { return };
            let mut body = vec![0u8; len];
            if rd.read_exact(&mut body).await.is_err() {
                return;
            }
            let Ok(msg) = CastMessage::decode(&body) else { return };
            for reply in answer(&st, &msg) {
                let _ = tx.send(Push::Message(reply));
            }
        }
    };
    let writer = async {
        while let Some(p) = rx.recv().await {
            match p {
                Push::Message(m) => {
                    if wr.write_all(&m.frame()).await.is_err() {
                        return;
                    }
                }
                Push::Close => {
                    let _ = wr.shutdown().await;
                    return;
                }
            }
        }
    };
    tokio::select! {
        _ = stop.cancelled() => {}
        () = reader => {}
        () = writer => {}
    }
}

/// The fake's replies to one message.
fn answer(st: &Mutex<State>, msg: &CastMessage) -> Vec<CastMessage> {
    let v = msg.value();
    let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
    let id = v["requestId"].as_i64().unwrap_or(0);
    let from = |src: &str, ns: &str, payload: Value| CastMessage::json(src, &msg.source, ns, &payload);
    if msg.namespace == NS_HEARTBEAT {
        return if v["type"] == "PING" { vec![from(&msg.destination, NS_HEARTBEAT, json!({"type": "PONG"}))] } else { Vec::new() };
    }
    s.log.push(Received { namespace: msg.namespace.clone(), destination: msg.destination.clone(), payload: v.clone() });
    let script = s.script.clone();
    match (msg.namespace.as_str(), v["type"].as_str().unwrap_or_default()) {
        (NS_RECEIVER, "LAUNCH") if script.launch_error => {
            vec![from(PLATFORM, NS_RECEIVER, json!({"type": "LAUNCH_ERROR", "requestId": id, "reason": "NOT_FOUND"}))]
        }
        (NS_RECEIVER, "LAUNCH") => {
            s.app = true;
            let mut out = Vec::new();
            if script.slow_launch {
                out.push(from(PLATFORM, NS_RECEIVER, receiver_status(false, s.volume, 0)));
            }
            out.push(from(PLATFORM, NS_RECEIVER, receiver_status(true, s.volume, id)));
            out
        }
        (NS_RECEIVER, "GET_STATUS") => vec![from(PLATFORM, NS_RECEIVER, receiver_status(s.app, s.volume, id))],
        (NS_RECEIVER, "STOP") => {
            s.app = false;
            vec![from(PLATFORM, NS_RECEIVER, receiver_status(false, s.volume, id))]
        }
        (NS_RECEIVER, "SET_VOLUME") => {
            s.volume = v["volume"]["level"].as_f64();
            vec![from(PLATFORM, NS_RECEIVER, receiver_status(s.app, s.volume, id))]
        }
        (NS_MEDIA, "LOAD") if script.load_failed => {
            vec![from(TRANSPORT, NS_MEDIA, json!({"type": "LOAD_FAILED", "requestId": id, "detailedErrorCode": 311}))]
        }
        (NS_MEDIA, "LOAD") => {
            let mut out = Vec::new();
            if s.media_session > 0 {
                // the item it replaces ends INTERRUPTED (the client ignores it)
                let old = json!({"mediaSessionId": s.media_session, "playerState": "IDLE", "idleReason": "INTERRUPTED"});
                out.push(from(TRANSPORT, NS_MEDIA, json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [old]})));
            }
            s.media_session += 1;
            let status =
                json!({"mediaSessionId": s.media_session, "playerState": "BUFFERING", "currentTime": 0.0, "media": {"duration": script.duration}});
            out.push(from(TRANSPORT, NS_MEDIA, json!({"type": "MEDIA_STATUS", "requestId": id, "status": [status]})));
            if script.play_after_load {
                let playing = json!({"mediaSessionId": s.media_session, "playerState": "PLAYING", "currentTime": 0.0});
                out.push(from(TRANSPORT, NS_MEDIA, json!({"type": "MEDIA_STATUS", "requestId": 0, "status": [playing]})));
            }
            out
        }
        (NS_MEDIA, "GET_STATUS") => {
            let status = json!({"mediaSessionId": s.media_session, "playerState": "PLAYING", "currentTime": script.position, "media": {"duration": script.duration}});
            vec![from(TRANSPORT, NS_MEDIA, json!({"type": "MEDIA_STATUS", "requestId": id, "status": [status]}))]
        }
        (NS_MEDIA, kind @ ("PAUSE" | "PLAY")) => {
            let state = if kind == "PAUSE" { "PAUSED" } else { "PLAYING" };
            let status = json!({"mediaSessionId": s.media_session, "playerState": state, "currentTime": script.position});
            vec![from(TRANSPORT, NS_MEDIA, json!({"type": "MEDIA_STATUS", "requestId": id, "status": [status]}))]
        }
        _ => Vec::new(),
    }
}
