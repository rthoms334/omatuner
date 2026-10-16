//! Single-instance IPC over a Unix socket in `$XDG_RUNTIME_DIR/tuner/tuner.sock`. Owner: WP-08.
//!
//! Protocol: one JSON line per connection (`IpcRequest`), server replies `{"ok":true}`
//! (or `{"ok":false,"error":"…"}` for a malformed request).
//! Startup: [`handoff`] connects; if a server answers, the request is delivered and the new
//! process exits 0. Otherwise (no socket / ECONNREFUSED → stale file removed) this process
//! becomes the server — once it holds the instance lock (`run/tuner.lock`, taken by `main`
//! before the store opens): the socket is only bound once the window is up, so a second quick
//! launch finds the lock held and keeps retrying the handoff (removing nothing: the socket may
//! be the starting instance's) instead of opening the database.
//! Requests arrive in the UI as `AppEvent::Ipc` and also trigger `hypr::focus_self` right
//! away — the UI may not be running `logic` while the window sits on another workspace, so
//! raising cannot wait for it. A reminder's timer (`--reminder`) and a pop-out play
//! (`--popout`) never raise it.
//!
//! Security: the socket is 0600 inside the 0700 runtime dir, and the server only accepts peers
//! with the socket owner's uid (`SO_PEERCRED`). Requests are capped at 64 KiB and 5 s.
//!
//! IPC v2 (DESIGN §30.3, `agent::proto`): a line with `"v":2` is an agent request
//! (`tuner mcp`). It is answered by the [`AgentService`] the server was started with — with
//! the reply line (≤ `proto::MAX_REPLY`) instead of `{"ok":true}` — and never raises the window
//! by itself (`play` does, in the handler). Without a service, or for another version, the reply
//! is a v2 error.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::agent::proto::{self, ErrorCode, Reply, Request};
use crate::agent::server::AgentService;
use crate::cli::ViewArg;
use crate::error::{Error, Result};
use crate::msg::{AppEvent, EventBus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IpcRequest {
    pub view: Option<ViewArg>,
    pub play: Option<String>,
    pub inputs: Vec<String>,
    /// `--popout` with `--play`: open in the pop-out without taking focus. Also set by `main`
    /// for its own window when a scheduled switch starts Tuner for the pop-out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub popout: bool,
    /// `--reminder <id>`: a reminder timer fired (B6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<i64>,
    /// A recording timer wakes the scheduler without raising a window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcReply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Largest request line the server reads.
const MAX_REQUEST: u64 = 64 * 1024;
/// Per-connection limit on the server; client read/write timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of [`handoff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handoff {
    /// A running instance took the request (the caller exits).
    Delivered,
    /// Nobody answered: no socket, a stale one (connection refused), or a server that did not
    /// answer properly (logged).
    NoServer,
    /// A running instance answered and refused the request (its reason; e.g. over 64 KiB).
    Rejected(String),
}

/// Try to deliver `req` to a running instance. `remove_stale`: a socket file that refuses the
/// connection is removed. Only pass `true` while holding the instance lock (or without one) —
/// otherwise the file may be one a starting instance has bound but not listened on yet.
pub fn handoff(socket: &Path, req: &IpcRequest, remove_stale: bool) -> Result<Handoff> {
    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Handoff::NoServer),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            if remove_stale {
                self::remove_stale(socket);
            }
            return Ok(Handoff::NoServer);
        }
        Err(e) => {
            tracing::warn!("IPC: cannot reach {}: {e}", socket.display());
            return Ok(Handoff::NoServer);
        }
    };
    let mut line = serde_json::to_string(req).map_err(Error::other)?;
    line.push('\n');
    match exchange(stream, &line) {
        Ok(reply) if reply.ok => Ok(Handoff::Delivered),
        Ok(reply) => Ok(Handoff::Rejected(reply.error.unwrap_or_default())),
        Err(e) => {
            tracing::warn!("IPC: running instance did not answer: {e}");
            Ok(Handoff::NoServer)
        }
    }
}

fn exchange(stream: UnixStream, line: &str) -> std::io::Result<IpcReply> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    (&stream).write_all(line.as_bytes())?;
    (&stream).flush()?;
    let mut reply = String::new();
    BufReader::new(std::io::Read::take(&stream, 4096)).read_line(&mut reply)?;
    serde_json::from_str(reply.trim()).map_err(std::io::Error::other)
}

/// Remove a socket file nobody listens on (never a regular file someone put there).
fn remove_stale(socket: &Path) {
    let is_socket = std::fs::symlink_metadata(socket).map(|m| m.file_type().is_socket()).unwrap_or(false);
    if is_socket {
        match std::fs::remove_file(socket) {
            Ok(()) => tracing::info!("IPC: removed stale socket {}", socket.display()),
            Err(e) => tracing::warn!("IPC: cannot remove stale socket {}: {e}", socket.display()),
        }
    }
}

pub struct IpcServer {
    task: Option<tokio::task::JoinHandle<()>>,
    socket: SocketGuard,
}

/// The socket file we bound: path + (dev, ino). Removing it only removes it while it is still
/// ours (another instance may have replaced a stale one).
#[derive(Debug, Clone)]
pub struct SocketGuard {
    path: PathBuf,
    ident: (u64, u64),
}

impl SocketGuard {
    /// Remove the socket file if it is still the one we bound (idempotent).
    pub fn remove(&self) {
        let ours = std::fs::symlink_metadata(&self.path).map(|m| (m.dev(), m.ino()) == self.ident).unwrap_or(false);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl IpcServer {
    /// Bind (mode 0600) and serve on the runtime, forwarding requests to the bus.
    /// Fails when another live instance already serves `socket` (the caller logs and runs
    /// without single-instance handoff) or the socket cannot be created. v2 (agent) requests
    /// are refused; see [`IpcServer::start_with_agent`].
    pub fn start(rt: &tokio::runtime::Handle, socket: &Path, bus: EventBus) -> Result<Self> {
        Self::start_with_agent(rt, socket, bus, None)
    }

    /// [`IpcServer::start`], answering v2 requests with `agent`.
    pub fn start_with_agent(rt: &tokio::runtime::Handle, socket: &Path, bus: EventBus, agent: Option<Arc<dyn AgentService>>) -> Result<Self> {
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(dir, e))?;
        }
        match UnixStream::connect(socket) {
            Ok(_) => return Err(Error::other(format!("another instance is serving {}", socket.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => remove_stale(socket),
            Err(_) => {}
        }
        let listener = std::os::unix::net::UnixListener::bind(socket).map_err(|e| Error::io(socket, e))?;
        // The 0700 parent dir already keeps others out; tighten the socket itself too.
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| Error::io(socket, e))?;
        let meta = std::fs::metadata(socket).map_err(|e| Error::io(socket, e))?;
        listener.set_nonblocking(true).map_err(|e| Error::io(socket, e))?;
        let listener = {
            let _rt = rt.enter();
            tokio::net::UnixListener::from_std(listener).map_err(|e| Error::io(socket, e))?
        };
        let task = rt.spawn(serve(listener, meta.uid(), bus, agent));
        Ok(Self { task: Some(task), socket: SocketGuard { path: socket.to_path_buf(), ident: (meta.dev(), meta.ino()) } })
    }

    /// The bound socket, for an exit path that can't drop the server (signal handling).
    pub fn socket(&self) -> SocketGuard {
        self.socket.clone()
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.socket.remove();
    }
}

async fn serve(listener: tokio::net::UnixListener, owner: u32, bus: EventBus, agent: Option<Arc<dyn AgentService>>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (bus, agent) = (bus.clone(), agent.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(IO_TIMEOUT, handle(stream, owner, &bus, agent)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::debug!("IPC connection: {e}"),
                        Err(_) => tracing::debug!("IPC connection timed out"),
                    }
                });
            }
            Err(e) => {
                // e.g. EMFILE: back off instead of spinning
                tracing::warn!("IPC accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(stream: tokio::net::UnixStream, owner: u32, bus: &EventBus, agent: Option<Arc<dyn AgentService>>) -> std::io::Result<()> {
    let uid = stream.peer_cred()?.uid();
    if uid != owner {
        tracing::warn!("IPC: rejected connection from uid {uid}");
        return Ok(());
    }
    let (rd, mut wr) = stream.into_split();
    let mut line = String::new();
    tokio::io::BufReader::new(rd.take(MAX_REQUEST)).read_line(&mut line).await?;
    let mut out = match Request::peek_version(line.trim()) {
        Some(v) if v >= 2 => agent_reply(line.trim(), v, agent).await,
        _ => v1_reply(line.trim(), bus)?,
    };
    out.push('\n');
    wr.write_all(out.as_bytes()).await?;
    wr.shutdown().await
}

/// A v1 request (`--view` / `--play` / inputs): forwarded to the UI, which raises the window.
fn v1_reply(line: &str, bus: &EventBus) -> std::io::Result<String> {
    let reply = match serde_json::from_str::<IpcRequest>(line) {
        Ok(req) => {
            tracing::info!("IPC request: view={:?} play={} inputs={}", req.view, req.play.is_some(), req.inputs.len());
            // a reminder's timer and a pop-out play never take focus (the app raises the
            // window itself for "Open in: Full window")
            let raise = req.reminder.is_none() && req.record.is_none() && !req.popout;
            bus.send(AppEvent::Ipc(req));
            if raise {
                crate::platform::hypr::focus_self();
            }
            IpcReply { ok: true, error: None }
        }
        Err(e) => IpcReply { ok: false, error: Some(format!("bad request: {e}")) },
    };
    serde_json::to_string(&reply).map_err(std::io::Error::other)
}

/// A v2 (agent) request line → its reply line (without the newline).
async fn agent_reply(line: &str, version: u64, agent: Option<Arc<dyn AgentService>>) -> String {
    let id = Request::peek_id(line);
    let reply = if version != u64::from(proto::VERSION) {
        Reply::err(id, ErrorCode::Unsupported, format!("this Tuner speaks IPC v{}, not v{version}", proto::VERSION))
    } else {
        match (serde_json::from_str::<Request>(line), agent) {
            (Err(e), _) => Reply::err(id, ErrorCode::BadRequest, crate::redact::redact(&e.to_string())),
            (Ok(_), None) => Reply::err(id, ErrorCode::Unsupported, "agent control isn't available in this Tuner"),
            (Ok(req), Some(agent)) => agent.handle(req).await,
        }
    };
    let out = serde_json::to_string(&reply).unwrap_or_default();
    if out.len() > proto::MAX_REPLY {
        let too_big = Reply::err(id, ErrorCode::Internal, "the answer was too large; ask for fewer results");
        return serde_json::to_string(&too_big).unwrap_or_default();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reply_wire_format() {
        let req = IpcRequest { view: Some(ViewArg::Guide), play: Some("2:xt:live:1".into()), inputs: vec!["a.m3u".into()], ..Default::default() };
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"view":"guide","play":"2:xt:live:1","inputs":["a.m3u"]}"#);
        assert_eq!(serde_json::from_str::<IpcRequest>(&s).unwrap(), req);
        assert_eq!(serde_json::to_string(&IpcReply { ok: true, error: None }).unwrap(), r#"{"ok":true}"#);
        assert!(!serde_json::from_str::<IpcReply>(r#"{"ok":false,"error":"x"}"#).unwrap().ok);
    }

    /// v2 lines go to the agent service and come back as v2 replies; without a service, for
    /// another version or a malformed request the reply is a v2 error; v1 still works and v2
    /// never reaches the UI as a handoff.
    #[test]
    fn v2_requests_are_answered_by_the_agent_service() {
        struct Echo;
        impl AgentService for Echo {
            fn handle(&self, req: Request) -> futures_util::future::BoxFuture<'static, Reply> {
                let reply = Reply::ok(req.id, serde_json::json!({ "method": req.call.name() }));
                Box::pin(async move { reply })
            }
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ask = |sock: &Path, line: &str| -> serde_json::Value {
            let stream = UnixStream::connect(sock).unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
            let mut reply = String::new();
            BufReader::new(&stream).read_line(&mut reply).unwrap();
            serde_json::from_str(&reply).unwrap()
        };
        let (bus, events) = EventBus::new(None);
        let sock = dir.path().join("a").join("tuner.sock");
        let server = IpcServer::start_with_agent(rt.handle(), &sock, bus.clone(), Some(Arc::new(Echo))).unwrap();
        let r = ask(&sock, r#"{"v":2,"id":5,"method":"status","params":{}}"#);
        assert_eq!(r, serde_json::json!({"v":2,"id":5,"ok":true,"result":{"method":"status"}}));
        let r = ask(&sock, r#"{"v":2,"id":6,"method":"search","params":{}}"#);
        assert_eq!((r["id"].as_u64(), r["error"]["code"].as_str()), (Some(6), Some("bad_request")), "{r}");
        let r = ask(&sock, r#"{"v":3,"id":7,"method":"status","params":{}}"#);
        assert_eq!(r["error"]["code"], "unsupported");
        assert!(events.try_iter().next().is_none(), "agent requests never reach the UI as a handoff");
        let r = ask(&sock, r#"{"view":"guide","play":null,"inputs":[]}"#);
        assert_eq!(r, serde_json::json!({"ok":true}));
        drop(server);

        let sock = dir.path().join("b").join("tuner.sock");
        let _server = IpcServer::start(rt.handle(), &sock, bus).unwrap();
        let r = ask(&sock, r#"{"v":2,"id":8,"method":"status","params":{}}"#);
        assert_eq!(r["error"]["code"], "unsupported");
    }

    #[test]
    fn remove_stale_leaves_regular_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("tuner.sock");
        std::fs::write(&f, "not a socket").unwrap();
        remove_stale(&f);
        assert!(f.exists());
    }
}
