//! Single-instance IPC over a Unix socket in `$XDG_RUNTIME_DIR/tuner/tuner.sock`. Owner: WP-08.
//!
//! Protocol: one JSON line per connection (`IpcRequest`), server replies `{"ok":true}`
//! (or `{"ok":false,"error":"…"}` for a malformed request).
//! Startup: `try_handoff` connects; if a server answers, the request is delivered and the new
//! process exits 0. Otherwise (no socket / ECONNREFUSED → stale file removed) this process
//! becomes the server. Requests arrive in the UI as `AppEvent::Ipc` and also trigger
//! `hypr::focus_self` right away — the UI may not be running `logic` while the window sits on
//! another workspace, so raising cannot wait for it.
//!
//! Security: the socket is 0600 inside the 0700 runtime dir, and the server only accepts peers
//! with the socket owner's uid (`SO_PEERCRED`). Requests are capped at 64 KiB and 5 s.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::cli::ViewArg;
use crate::error::{Error, Result};
use crate::msg::{AppEvent, EventBus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IpcRequest {
    pub view: Option<ViewArg>,
    pub play: Option<String>,
    pub inputs: Vec<String>,
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

/// Try to deliver `req` to a running instance. `Ok(true)` = delivered (caller exits).
///
/// `Ok(false)` = become the primary instance: no socket, a stale socket (connection refused;
/// the file is removed), or a server that does not answer properly (logged — starting a
/// second window beats starting nothing).
pub fn try_handoff(socket: &Path, req: &IpcRequest) -> Result<bool> {
    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            remove_stale(socket);
            return Ok(false);
        }
        Err(e) => {
            tracing::warn!("IPC: cannot reach {}: {e}", socket.display());
            return Ok(false);
        }
    };
    let mut line = serde_json::to_string(req).map_err(Error::other)?;
    line.push('\n');
    match exchange(stream, &line) {
        Ok(reply) if reply.ok => Ok(true),
        Ok(reply) => {
            tracing::warn!("IPC: running instance rejected the request: {}", reply.error.unwrap_or_default());
            Ok(false)
        }
        Err(e) => {
            tracing::warn!("IPC: running instance did not answer: {e}");
            Ok(false)
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
    /// without single-instance handoff) or the socket cannot be created.
    pub fn start(rt: &tokio::runtime::Handle, socket: &Path, bus: EventBus) -> Result<Self> {
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
        let task = rt.spawn(serve(listener, meta.uid(), bus));
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

async fn serve(listener: tokio::net::UnixListener, owner: u32, bus: EventBus) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let bus = bus.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(IO_TIMEOUT, handle(stream, owner, &bus)).await {
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

async fn handle(stream: tokio::net::UnixStream, owner: u32, bus: &EventBus) -> std::io::Result<()> {
    let uid = stream.peer_cred()?.uid();
    if uid != owner {
        tracing::warn!("IPC: rejected connection from uid {uid}");
        return Ok(());
    }
    let (rd, mut wr) = stream.into_split();
    let mut line = String::new();
    tokio::io::BufReader::new(rd.take(MAX_REQUEST)).read_line(&mut line).await?;
    let reply = match serde_json::from_str::<IpcRequest>(line.trim()) {
        Ok(req) => {
            tracing::info!("IPC request: view={:?} play={} inputs={}", req.view, req.play.is_some(), req.inputs.len());
            bus.send(AppEvent::Ipc(req));
            crate::platform::hypr::focus_self();
            IpcReply { ok: true, error: None }
        }
        Err(e) => IpcReply { ok: false, error: Some(format!("bad request: {e}")) },
    };
    let mut out = serde_json::to_string(&reply).map_err(std::io::Error::other)?;
    out.push('\n');
    wr.write_all(out.as_bytes()).await?;
    wr.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reply_wire_format() {
        let req = IpcRequest { view: Some(ViewArg::Guide), play: Some("2:xt:live:1".into()), inputs: vec!["a.m3u".into()] };
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"view":"guide","play":"2:xt:live:1","inputs":["a.m3u"]}"#);
        assert_eq!(serde_json::from_str::<IpcRequest>(&s).unwrap(), req);
        assert_eq!(serde_json::to_string(&IpcReply { ok: true, error: None }).unwrap(), r#"{"ok":true}"#);
        assert!(!serde_json::from_str::<IpcReply>(r#"{"ok":false,"error":"x"}"#).unwrap().ok);
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
