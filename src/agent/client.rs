//! `tuner mcp`'s IPC v2 client (DESIGN §30.3, §30.7). Owner: MCP-bridge track.
//!
//! Blocking (the bridge is a small synchronous stdio loop; no runtime): one connection per
//! request, the request line ≤ 64 KiB, the reply line ≤ `proto::MAX_REPLY`, 6 s read timeout
//! (the server allows 5 s per connection).
//!
//! Not running (no socket / connection refused): tools with `ToolDef::launches` start Tuner
//! through the [`Launcher`] (detached: own process group, stdio to /dev/null, so it outlives
//! the MCP client and never writes into the MCP stream), then poll the socket every 100 ms for
//! up to [`LAUNCH_WAIT`]; `ErrorCode::Loading` replies (library not built yet) are retried
//! every 250 ms within [`LOADING_WAIT`]. A play that opens in the pop-out starts Tuner as the
//! pop-out only ([`Launch::PopOut`], DESIGN §30.13). `status` / `control` never launch:
//! `status` answers `{"running":false}`, `control` says Tuner isn't running.
//! A running Tuner from before v2 answers with a v1 error line (no `"v"`): "restart Tuner".

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::proto::{self, Call, ClientInfo, ErrorCode, Reply, Request};

/// Longest wait for a launched Tuner's socket.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(15);
/// Longest wait for a (just started) Tuner to finish loading its library.
pub const LOADING_WAIT: Duration = Duration::from_secs(20);
/// Client read/write timeout per request.
pub const IO_TIMEOUT: Duration = Duration::from_secs(6);
/// Largest request line the server reads (`ipc.rs`).
pub const MAX_REQUEST: usize = 64 * 1024;
/// Socket poll interval while a launched Tuner starts.
const LAUNCH_POLL: Duration = Duration::from_millis(100);
/// Retry interval while Tuner answers `loading`.
const LOADING_POLL: Duration = Duration::from_millis(250);

/// Whether and how a call may start Tuner when it isn't running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// Never (`status`, `control`).
    No,
    /// Its window, as from the app menu.
    Window,
    /// As the pop-out only, without the keyboard (`--agent-popout`): an agent's play that
    /// opens there (DESIGN §30.13).
    PopOut,
}

impl From<bool> for Launch {
    fn from(launch: bool) -> Self {
        if launch { Launch::Window } else { Launch::No }
    }
}

/// Starts Tuner (tests inject fakes that start a server on the socket).
pub trait Launcher: Send + Sync {
    fn launch(&self) -> std::io::Result<()>;

    /// Start Tuner for an agent's play in the pop-out (`--agent-popout`: pinned, without the
    /// keyboard, like a switch reminder's launch). Fakes: [`Launcher::launch`].
    fn launch_popout(&self) -> std::io::Result<()> {
        self.launch()
    }

    /// The launched process already gave up (it exited unsuccessfully before serving the
    /// socket): why, for the model. Polled while waiting for the socket; `None` = keep waiting.
    fn failed(&self) -> Option<String> {
        None
    }
}

/// The real launcher: this executable (`std::env::current_exe`) with `--profile-dir` when the
/// bridge has one, detached.
#[derive(Debug, Clone)]
pub struct ExeLauncher {
    pub exe: PathBuf,
    pub profile_dir: Option<PathBuf>,
    /// How the last launched process ended (set by its reaper thread).
    exit: Arc<Mutex<Option<std::process::ExitStatus>>>,
}

impl ExeLauncher {
    pub fn new(exe: PathBuf, profile_dir: Option<PathBuf>) -> Self {
        Self { exe, profile_dir, exit: Arc::default() }
    }

    /// This executable. After a package upgrade replaced the file under a running bridge,
    /// `/proc/self/exe` reads `/usr/bin/tuner (deleted)`: launch the new file at that path.
    pub fn current(profile_dir: Option<PathBuf>) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(path) if !exe.exists() => PathBuf::from(path),
            _ => exe,
        };
        Ok(Self::new(exe, profile_dir))
    }
}

impl ExeLauncher {
    fn spawn(&self, popout: bool) -> std::io::Result<()> {
        let mut cmd = Command::new(&self.exe);
        if let Some(dir) = &self.profile_dir {
            cmd.arg("--profile-dir").arg(dir);
        }
        if popout {
            cmd.arg("--agent-popout");
        }
        // own process group: the MCP client ending the bridge (or the terminal hanging up its
        // foreground group) doesn't take Tuner with it; stdio never touches the MCP stream
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
        // the bridge's `RUST_LOG` is for its stderr, not tuner.log; the MCP client's working
        // directory isn't Tuner's
        cmd.env_remove("RUST_LOG").current_dir("/");
        let mut child = cmd.spawn()?;
        tracing::info!("mcp: started Tuner (pid {}{})", child.id(), if popout { ", pop-out" } else { "" });
        *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let exit = self.exit.clone();
        // reap it (no zombie while the bridge lives) and remember how it ended
        std::thread::Builder::new().name("tuner-reaper".into()).spawn(move || {
            if let Ok(status) = child.wait() {
                *exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
            }
        })?;
        Ok(())
    }
}

impl Launcher for ExeLauncher {
    fn launch(&self) -> std::io::Result<()> {
        self.spawn(false)
    }

    fn launch_popout(&self) -> std::io::Result<()> {
        self.spawn(true)
    }

    fn failed(&self) -> Option<String> {
        // exit 0 = it handed off to an instance that was starting at the same time: keep
        // waiting for that one's socket
        let status = (*self.exit.lock().unwrap_or_else(|e| e.into_inner()))?;
        (!status.success()).then(|| {
            format!(
                "it exited ({status}) before it was ready; see tuner.log (Settings ▸ About & logs) — a background `tuner --refresh` may be holding the profile"
            )
        })
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

impl ClientError {
    /// The code a tool error shows (`"<code>: <message>"`), next to `proto::ErrorCode`'s.
    pub fn code(&self) -> &'static str {
        match self {
            ClientError::NotRunning => "not_running",
            ClientError::DidNotStart(_) => "did_not_start",
            ClientError::StillLoading => "loading",
            ClientError::TooOld => "too_old",
            ClientError::Failed(_) => "connection_failed",
        }
    }
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

impl std::error::Error for ClientError {}

/// A reply and how it was obtained.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub reply: Reply,
    /// Tuner wasn't running and this call started it.
    pub started: bool,
}

/// One attempt's outcome.
enum Attempt {
    /// Nobody listens (no socket file, or a stale one refusing connections).
    NotListening,
    Reply(Reply),
}

/// The bridge's connection to the running Tuner.
pub struct Client {
    socket: PathBuf,
    launcher: Box<dyn Launcher>,
    next_id: AtomicU64,
    launch_wait: Duration,
    loading_wait: Duration,
}

impl Client {
    pub fn new(socket: &Path, launcher: Box<dyn Launcher>) -> Self {
        Self { socket: socket.to_path_buf(), launcher, next_id: AtomicU64::new(1), launch_wait: LAUNCH_WAIT, loading_wait: LOADING_WAIT }
    }

    /// Shorter waits than [`LAUNCH_WAIT`] / [`LOADING_WAIT`] (tests).
    pub fn with_waits(mut self, launch: Duration, loading: Duration) -> Self {
        self.launch_wait = launch;
        self.loading_wait = loading;
        self
    }

    /// Send `call`; `launch`: start Tuner first when nothing answers (see module docs).
    pub fn call(&self, call: Call, launch: bool) -> Result<Reply, ClientError> {
        self.send(call, launch).map(|s| s.reply)
    }

    /// [`Client::call`], also saying whether it started Tuner.
    pub fn send(&self, call: Call, launch: bool) -> Result<Sent, ClientError> {
        self.send_as(call, launch.into(), None)
    }

    /// [`Client::send`] on behalf of an MCP client (`initialize.clientInfo`), so Tuner can say
    /// which agent made a change; `launch`: how it starts Tuner when nothing answers.
    pub fn send_as(&self, call: Call, launch: Launch, agent: Option<ClientInfo>) -> Result<Sent, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(id, call).with_agent(agent);
        let mut line = serde_json::to_string(&req).map_err(|e| ClientError::Failed(e.to_string()))?;
        line.push('\n');
        if line.len() > MAX_REQUEST {
            return Ok(Sent { reply: Reply::err(id, ErrorCode::BadRequest, "the arguments are too large"), started: false });
        }
        let mut started: Option<Instant> = None;
        let mut loading_since: Option<Instant> = None;
        loop {
            let reply = match self.attempt(id, &line)? {
                Attempt::Reply(r) => r,
                Attempt::NotListening if launch == Launch::No => return Err(ClientError::NotRunning),
                Attempt::NotListening => {
                    match started {
                        None => {
                            let launched = if launch == Launch::PopOut { self.launcher.launch_popout() } else { self.launcher.launch() };
                            launched.map_err(|e| ClientError::DidNotStart(format!("couldn't run it: {e}")))?;
                            started = Some(Instant::now());
                        }
                        Some(at) => {
                            if let Some(why) = self.launcher.failed() {
                                return Err(ClientError::DidNotStart(why));
                            }
                            if at.elapsed() >= self.launch_wait {
                                return Err(ClientError::DidNotStart(format!("it didn't answer within {} s", self.launch_wait.as_secs())));
                            }
                        }
                    }
                    std::thread::sleep(LAUNCH_POLL);
                    continue;
                }
            };
            if reply.error.as_ref().is_some_and(|e| e.code == ErrorCode::Loading) {
                let since = *loading_since.get_or_insert_with(Instant::now);
                if since.elapsed() + LOADING_POLL > self.loading_wait {
                    return Err(ClientError::StillLoading);
                }
                std::thread::sleep(LOADING_POLL);
                continue;
            }
            return Ok(Sent { reply, started: started.is_some() });
        }
    }

    /// One connection: send `line`, read the reply line.
    fn attempt(&self, id: u64, line: &str) -> Result<Attempt, ClientError> {
        if !socket_is_ours(&self.socket)? {
            return Ok(Attempt::NotListening);
        }
        let stream = match UnixStream::connect(&self.socket) {
            Ok(s) => s,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Ok(Attempt::NotListening),
            Err(e) => return Err(ClientError::Failed(e.to_string())),
        };
        let io = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => ClientError::Failed("Tuner didn't answer in time".into()),
            _ => ClientError::Failed(e.to_string()),
        };
        stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        (&stream).write_all(line.as_bytes()).map_err(io)?;
        (&stream).flush().map_err(io)?;
        let mut buf = Vec::new();
        BufReader::new((&stream).take(proto::MAX_REPLY as u64 + 1)).read_until(b'\n', &mut buf).map_err(io)?;
        if buf.len() > proto::MAX_REPLY {
            return Err(ClientError::Failed("the answer was too large".into()));
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            return Err(ClientError::Failed("Tuner closed the connection without answering".into()));
        }
        let value: serde_json::Value = serde_json::from_slice(&buf).map_err(|_| ClientError::Failed("Tuner sent a malformed answer".into()))?;
        if value.get("v").is_none() {
            // a v1 server: `{"ok":false,"error":"bad request: …"}`
            return Err(ClientError::TooOld);
        }
        let reply: Reply = serde_json::from_value(value).map_err(|_| ClientError::Failed("Tuner sent a malformed answer".into()))?;
        if reply.id != id {
            return Err(ClientError::Failed("Tuner answered another request".into()));
        }
        Ok(Attempt::Reply(reply))
    }
}

/// The socket belongs to this user, in a directory only this user can enter (Tuner's own
/// runtime directory, mode 0700). Without `XDG_RUNTIME_DIR` the path is under the shared temp
/// directory, where another user could create it first and answer as "Tuner" (feeding made-up
/// results to the model). `false`: no socket (nothing listens).
fn socket_is_ours(socket: &Path) -> Result<bool, ClientError> {
    let not_ours = |what: &str| Err(ClientError::Failed(format!("the Tuner socket's {what} isn't yours alone; not connecting")));
    let me = std::fs::metadata("/proc/self").map_err(|e| ClientError::Failed(format!("can't tell who I am: {e}")))?.uid();
    let meta = match std::fs::symlink_metadata(socket) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ClientError::Failed(e.to_string())),
    };
    if meta.uid() != me {
        return not_ours("owner");
    }
    let dir = socket.parent().unwrap_or(Path::new("/"));
    match std::fs::metadata(dir) {
        Ok(d) if d.uid() == me && d.permissions().mode() & 0o077 == 0 => Ok(true),
        Ok(_) => not_ours("directory"),
        Err(e) => Err(ClientError::Failed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket in a directory others can enter is refused (another user may have made it);
    /// the same socket in a private directory is used.
    #[test]
    fn a_socket_in_a_shared_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tuner.sock");
        assert_eq!(socket_is_ours(&sock), Ok(false), "no socket: not listening");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(socket_is_ours(&sock), Ok(true));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        match Client::new(&sock, Box::new(ExeLauncher::new("/bin/false".into(), None))).call(Call::Status(Default::default()), true) {
            Err(ClientError::Failed(why)) => assert!(why.contains("isn't yours"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn client_errors_have_codes_and_plain_messages() {
        assert_eq!(ClientError::NotRunning.code(), "not_running");
        assert!(ClientError::TooOld.to_string().contains("restart Tuner"));
        assert!(ClientError::DidNotStart("x".into()).to_string().ends_with(": x"));
    }

    /// No socket and no launch: `NotRunning` without calling the launcher; an unrunnable
    /// executable is `DidNotStart` with the reason.
    #[test]
    fn no_socket_without_launch_is_not_running() {
        struct Never;
        impl Launcher for Never {
            fn launch(&self) -> std::io::Result<()> {
                panic!("must not launch");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let client = Client::new(&dir.path().join("tuner.sock"), Box::new(Never));
        assert_eq!(client.call(Call::Status(Default::default()), false), Err(ClientError::NotRunning));

        let exe = ExeLauncher::new(dir.path().join("no-such-tuner"), None);
        let client = Client::new(&dir.path().join("tuner.sock"), Box::new(exe));
        match client.call(Call::Status(Default::default()), true) {
            Err(ClientError::DidNotStart(why)) => assert!(why.contains("couldn't run it"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// A launched process that exits unsuccessfully is reported at once, not after 15 s.
    #[test]
    fn a_launch_that_exits_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::new(&dir.path().join("tuner.sock"), Box::new(ExeLauncher::new("/bin/false".into(), None)));
        let t = Instant::now();
        match client.call(Call::Status(Default::default()), true) {
            Err(ClientError::DidNotStart(why)) => assert!(why.contains("exited"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
