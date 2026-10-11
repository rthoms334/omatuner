//! `tuner mcp`'s IPC v2 client (DESIGN §30.3, §30.7). Owner: MCP-bridge track.
//!
//! Blocking (the bridge is a small synchronous stdio loop; no runtime): one connection per
//! request, the request line ≤ 64 KiB, the reply line ≤ `proto::MAX_REPLY`, 6 s read timeout
//! (the server allows 5 s per connection).
//!
//! Not running (no socket / connection refused): tools with `ToolDef::launches` start Tuner
//! through the [`Launcher`] (detached: own process group, stdio to /dev/null, so it outlives
//! Claude and never writes into the MCP stream), then poll the socket every 100 ms for up to
//! [`LAUNCH_WAIT`]; `ErrorCode::Loading` replies (library not built yet) are retried every
//! 250 ms within [`LOADING_WAIT`]. `status` / `control` never launch: `status` answers
//! `{"running":false}`, `control` says Tuner isn't running.
//! A running Tuner from before v2 answers with a v1 error line (no `"v"`): "restart Tuner".

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::proto::{Call, Reply};

/// Longest wait for a launched Tuner's socket.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(15);
/// Longest wait for a (just started) Tuner to finish loading its library.
pub const LOADING_WAIT: Duration = Duration::from_secs(20);
/// Client read/write timeout per request.
pub const IO_TIMEOUT: Duration = Duration::from_secs(6);

/// Starts Tuner (tests inject fakes that start a server on the socket).
pub trait Launcher: Send + Sync {
    fn launch(&self) -> std::io::Result<()>;
}

/// The real launcher: this executable (`std::env::current_exe`) with `--profile-dir` when the
/// bridge has one, detached.
#[derive(Debug, Clone)]
pub struct ExeLauncher {
    pub exe: PathBuf,
    pub profile_dir: Option<PathBuf>,
}

impl Launcher for ExeLauncher {
    fn launch(&self) -> std::io::Result<()> {
        let _ = (&self.exe, &self.profile_dir);
        todo!("MCP-bridge")
    }
}

/// Why a call got no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Nothing listens and the call may not launch (`status`, `control`).
    NotRunning,
    /// Launched, but no socket within [`LAUNCH_WAIT`] (or the launch failed): the reason.
    DidNotStart(String),
    /// Still loading after [`LOADING_WAIT`].
    StillLoading,
    /// The running Tuner predates IPC v2.
    TooOld,
    /// I/O or a malformed reply (redacted).
    Failed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "Tuner isn't running"),
            ClientError::DidNotStart(why) => write!(f, "Tuner didn't start: {why}"),
            ClientError::StillLoading => write!(f, "Tuner is still loading the playlists; try again in a moment"),
            ClientError::TooOld => write!(f, "the running Tuner is older than this command; restart Tuner to update it"),
            ClientError::Failed(why) => write!(f, "couldn't reach Tuner: {why}"),
        }
    }
}

/// The bridge's connection to the running Tuner.
pub struct Client {
    socket: PathBuf,
    launcher: Box<dyn Launcher>,
}

impl Client {
    pub fn new(socket: &Path, launcher: Box<dyn Launcher>) -> Self {
        Self { socket: socket.to_path_buf(), launcher }
    }

    /// Send `call`; `launch`: start Tuner first when nothing answers (see module docs).
    pub fn call(&self, call: Call, launch: bool) -> Result<Reply, ClientError> {
        let _ = (&self.socket, &self.launcher, call, launch);
        todo!("MCP-bridge")
    }
}
