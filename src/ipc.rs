//! Single-instance IPC over a Unix socket in `$XDG_RUNTIME_DIR/tuner/tuner.sock`. Owner: WP-08.
//!
//! Protocol: one JSON line per connection (`IpcRequest`), server replies `{"ok":true}`.
//! Startup: `try_handoff` connects; if a server answers, the request is delivered and the new
//! process exits 0. Otherwise (no socket / ECONNREFUSED → stale file removed) this process
//! becomes the server. Requests arrive in the UI as `AppEvent::Ipc` and also trigger
//! `hypr::focus_self`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::ViewArg;
use crate::error::Result;
use crate::msg::EventBus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IpcRequest {
    pub view: Option<ViewArg>,
    pub play: Option<String>,
    pub inputs: Vec<String>,
}

/// Try to deliver `req` to a running instance. `Ok(true)` = delivered (caller exits).
/// SKELETON SAFE DEFAULT: `Ok(false)` (always become the primary instance).
pub fn try_handoff(socket: &Path, req: &IpcRequest) -> Result<bool> { Ok(false) }

pub struct IpcServer {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl IpcServer {
    /// Bind (mode 0600) and serve on the runtime, forwarding requests to the bus.
    /// SKELETON SAFE DEFAULT: `Err` (logged; no single-instance handoff).
    pub fn start(rt: &tokio::runtime::Handle, socket: &Path, bus: EventBus) -> Result<Self> {
        Err(crate::error::Error::other("IPC not implemented yet (WP-08)"))
    }
}
