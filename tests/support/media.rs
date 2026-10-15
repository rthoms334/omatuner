//! Test media generated with ffmpeg (scripts/gen-test-media.sh) — never committed. Owner: WP-07.
//! Tests that need media call `ensure_media()` and SKIP (return early with a note) when ffmpeg
//! is unavailable, so `cargo test` stays green on minimal machines.
//!
//! [`TestServer`] is a tiny HTTP/1.1 file server (by default one request per connection,
//! `Connection: close`) bound to 127.0.0.1 on an ephemeral port, running on its own thread and
//! runtime so blocking tests can use it. It records every request (path, headers, open/close
//! time, connection and its n-th request), supports `Range`, and injects faults: forced
//! statuses per path (or `prefix*`), connections cut after N bytes, 404 for every Nth HLS
//! segment, throttling, "kill"/"revive" of the whole server (listener closed, open connections
//! reset), and the provider behaviours of the 2026-10-11 field report: keep-alive connections
//! the server closes when idle or abandons (a reused request is never answered), a text body
//! where a playlist should be ("Cannot read …"), and a connection limit whose sessions linger
//! after their connections closed. `/live/index.m3u8` is a sliding-window live playlist over
//! the `long/` segments (one new 1 s segment per second, no `ENDLIST`).

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Bump when the script's outputs change (forces regeneration of an existing directory).
const MEDIA_VERSION: &str = "11";
const STAMP: &str = ".complete";

/// Directory with `testsrc.mp4` (10 s 640×360 25 fps + 1 kHz sine), `bars.ts` (MPEG-TS),
/// `hls/index.m3u8` (6 × 2 s segments) — generated on first use into `target/test-media`.
/// Also `long/index.m3u8` (60 × 1 s segments), the 5.1 `surround*` films and the `avsync.mkv`
/// marker film (see the script).
pub fn ensure_media() -> Option<PathBuf> {
    static MEDIA: OnceLock<Option<PathBuf>> = OnceLock::new();
    MEDIA
        .get_or_init(|| match generate() {
            Ok(dir) => Some(dir),
            Err(e) => {
                eprintln!("note: skipping media test ({e})");
                None
            }
        })
        .clone()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
}

fn is_complete(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(STAMP)).is_ok_and(|s| s.trim() == MEDIA_VERSION)
}

fn generate() -> Result<PathBuf, String> {
    let target = target_dir();
    let dir = target.join("test-media");
    if is_complete(&dir) {
        return Ok(dir);
    }
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(o) if o.status.success() => {}
        _ => return Err("ffmpeg not found".into()),
    }
    std::fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
    // Test binaries run concurrently: one generates, the others wait on the lock.
    let lock = std::fs::File::create(target.join("test-media.lock")).map_err(|e| format!("lock file: {e}"))?;
    lock.lock().map_err(|e| format!("lock: {e}"))?;
    if is_complete(&dir) {
        return Ok(dir);
    }
    let tmp = target.join(format!("test-media.tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/gen-test-media.sh");
    let out = Command::new("bash").arg(&script).arg(&tmp).output().map_err(|e| format!("run {}: {e}", script.display()))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("gen-test-media.sh failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    std::fs::write(tmp.join(STAMP), MEDIA_VERSION).map_err(|e| format!("stamp: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(|e| format!("rename media dir: {e}"))?;
    Ok(dir)
}

/// Serve `dir` over HTTP on an ephemeral port with optional fault injection (drop the
/// connection after N bytes, 404 every Nth segment) — reconnect tests. Returns the base URL.
/// The server lives until the process exits; use [`TestServer::start`] for control and logs.
pub async fn serve(dir: PathBuf, faults: Faults) -> String {
    let server = TestServer::start(dir, faults);
    let base = server.base.clone();
    std::mem::forget(server);
    base
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Faults {
    /// Cut the FIRST response of every path after this many body bytes (later requests for the
    /// same path, i.e. reconnects, are served in full).
    pub drop_after_bytes: Option<u64>,
    /// Answer every Nth request for an HLS segment (`segNNN.ts`) with 404.
    pub segment_404_every: Option<u32>,
}

/// Keep-alive behaviour (see [`TestServer::set_keep_alive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepAlive {
    /// `Connection: close` after every response.
    #[default]
    Off,
    /// Persistent connections, closed silently (FIN) after this long without a request — a
    /// provider closing idle connections between segment fetches.
    IdleClose(Duration),
    /// Persistent connections whose second request is read but never answered — a half-dead
    /// connection (the client waits for its network timeout).
    Abandon,
}

/// A provider's connection limit (see [`TestServer::set_session_limit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimit {
    /// Streams that may be open at once.
    pub max: usize,
    /// How long a stream still counts after its last connection closed.
    pub linger: Duration,
}

/// One HTTP request as seen by the server.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    /// Lower-cased names, values as sent.
    pub headers: Vec<(String, String)>,
    pub status: u16,
    /// The connection it came on (numbered from 1) and its position there (1 = first).
    pub conn: usize,
    pub nth: usize,
    pub opened: Instant,
    /// When the connection ended (response done, cut, or reset by the client/kill).
    pub closed: Option<Instant>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Vec<Request>>,
    status: Mutex<HashMap<String, u16>>,
    first_seen: Mutex<HashSet<String>>,
    segment_count: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    /// Open connections by number.
    open: Mutex<HashMap<usize, OpenConn>>,
    /// Accepts that found other connections still open on the client's side.
    overlaps: Mutex<Vec<String>>,
    throttle_bps: Mutex<Option<u64>>,
    /// Requests are not recorded (long soaks: the log would grow with every stream load).
    unlogged: AtomicBool,
    connections: AtomicUsize,
    keep_alive: Mutex<KeepAlive>,
    /// Paths answered 200 with this text.
    bodies: Mutex<HashMap<String, String>>,
    limit: Mutex<Option<SessionLimit>>,
    /// `Range` is ignored: every answer is the whole file, without `Accept-Ranges`.
    no_ranges: AtomicBool,
    /// Stream key → (open connections, last close).
    sessions: Mutex<HashMap<String, (usize, Option<Instant>)>>,
    /// `(path, from, status, times)`: the next `times` requests for `path` whose `Range` starts at
    /// or after `from` are answered `status`.
    range_faults: Mutex<Vec<(String, u64, u16, u32)>>,
}

impl Shared {
    /// A [`TestServer::fail_ranges_from`] answer for this request (counted), if one is due.
    fn range_fault(&self, path: &str, headers: &[(String, String)]) -> Option<u16> {
        let from = headers
            .iter()
            .find(|(k, _)| k == "range")
            .and_then(|(_, v)| v.trim().strip_prefix("bytes=")?.split('-').next()?.trim().parse::<u64>().ok())?;
        let mut faults = self.range_faults.lock().unwrap();
        let (_, _, status, times) = faults.iter_mut().find(|(p, at, _, times)| p == path && from >= *at && *times > 0)?;
        *times -= 1;
        Some(*status)
    }

    /// The forced status of `path`: its own, else the longest matching `prefix*`.
    fn forced_status(&self, path: &str) -> Option<u16> {
        let m = self.status.lock().unwrap();
        m.get(path).copied().or_else(|| {
            m.iter()
                .filter_map(|(k, v)| Some((k.strip_suffix('*')?, *v)))
                .filter(|(prefix, _)| path.starts_with(prefix))
                .max_by_key(|(prefix, _)| prefix.len())
                .map(|(_, v)| v)
        })
    }

    /// Open a session of `key` unless the limit is reached by OTHER streams (the same stream
    /// shares its session, like a provider recognising its client). `false` = refused.
    fn open_session(&self, key: &str) -> bool {
        let limit = *self.limit.lock().unwrap();
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(l) = limit {
            let now = Instant::now();
            let alive = sessions
                .iter()
                .filter(|(k, (open, closed))| *k != key && (*open > 0 || closed.is_some_and(|c| now.duration_since(c) < l.linger)))
                .count();
            if alive >= l.max {
                return false;
            }
        }
        sessions.entry(key.to_owned()).or_default().0 += 1;
        true
    }

    fn close_session(&self, key: &str) {
        if let Some(s) = self.sessions.lock().unwrap().get_mut(key) {
            s.0 = s.0.saturating_sub(1);
            s.1 = Some(Instant::now());
        }
    }
}

/// The stream a request belongs to: its directory (`/live/seg001.ts` → `/live`), or the file
/// itself at the top level (`/bars.ts`).
fn stream_key(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => path,
        Some(i) => &path[..i],
    }
}

/// Closes a session when its response ends (however the task ends).
struct SessionGuard {
    shared: Arc<Shared>,
    key: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.shared.close_session(&self.key);
    }
}

enum Ctl {
    Kill,
    /// Answered once the listener is bound again.
    Revive(std::sync::mpsc::Sender<()>),
    Shutdown,
}

/// Handle to a running test server (see module docs). Dropping it shuts the server down.
pub struct TestServer {
    /// `http://127.0.0.1:<port>` (no trailing slash).
    pub base: String,
    shared: Arc<Shared>,
    ctl: mpsc::UnboundedSender<Ctl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn start(dir: PathBuf, faults: Faults) -> Self {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        std_listener.set_nonblocking(true).expect("nonblocking listener");
        let addr = std_listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared::default());
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let ctx = Ctx { dir, faults, shared: shared.clone(), epoch: Instant::now() };
        let thread = std::thread::Builder::new()
            .name("test-http".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("test server runtime");
                rt.block_on(run(std_listener, addr, Arc::new(ctx), ctl_rx));
            })
            .expect("spawn test server");
        Self { base: format!("http://{addr}"), shared, ctl, thread: Some(thread) }
    }

    /// `base` + `/path`.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// Answer every request for `path` (e.g. `/a.ts`, or `/live/seg*` for every path with that
    /// prefix) with `status` (0 = back to normal).
    pub fn set_status(&self, path: &str, status: u16) {
        let mut m = self.shared.status.lock().unwrap();
        if status == 0 {
            m.remove(path);
        } else {
            m.insert(path.to_owned(), status);
        }
    }

    /// Answer the next `times` requests for `path` whose `Range` starts at or after byte `from`
    /// with `status`: a provider refusing a connection made right after the previous one closed,
    /// for a read of the file's end (a keyframe index).
    pub fn fail_ranges_from(&self, path: &str, from: u64, status: u16, times: u32) {
        self.shared.range_faults.lock().unwrap().push((path.to_owned(), from, status, times));
    }

    /// Keep connections open between requests ([`KeepAlive`]); applies to new requests.
    pub fn set_keep_alive(&self, k: KeepAlive) {
        *self.shared.keep_alive.lock().unwrap() = k;
    }

    /// Answer `path` with 200 and `text` (`None` = back to normal): the restreamer's "Cannot
    /// read …" where a playlist should be.
    pub fn set_body(&self, path: &str, text: Option<&str>) {
        let mut m = self.shared.bodies.lock().unwrap();
        match text {
            Some(t) => m.insert(path.to_owned(), t.to_owned()),
            None => m.remove(path),
        };
    }

    /// Refuse (403) a stream while `max` OTHER streams are open or closed less than `linger`
    /// ago (`None` = no limit). A stream is a directory (`/live`) or a top-level file.
    pub fn set_session_limit(&self, limit: Option<SessionLimit>) {
        *self.shared.limit.lock().unwrap() = limit;
    }

    /// Answer `Range` requests (`true`, the default) or ignore them, as some providers do.
    pub fn set_ranges(&self, on: bool) {
        self.shared.no_ranges.store(!on, Ordering::SeqCst);
    }

    /// Limit every response body to `bps` bytes per second (`None` = unlimited).
    pub fn set_throttle(&self, bps: Option<u64>) {
        *self.shared.throttle_bps.lock().unwrap() = bps;
    }

    /// Close the listener (connection refused) and reset every open connection.
    pub fn kill(&self) {
        let _ = self.ctl.send(Ctl::Kill);
    }

    /// Listen again on the same port (returns once it does, so a connect right after succeeds).
    pub fn revive(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.ctl.send(Ctl::Revive(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    /// Stop (`false`) or resume recording requests for [`TestServer::requests`].
    pub fn set_request_log(&self, on: bool) {
        self.shared.unlogged.store(!on, Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.shared.requests.lock().unwrap().clone()
    }

    pub fn requests_for(&self, path: &str) -> Vec<Request> {
        self.requests().into_iter().filter(|r| r.path == path).collect()
    }

    /// Highest number of connections the client held open at once, counted at each accept: the
    /// new one, plus every earlier one the client hasn't hung up yet by its socket's state in
    /// the kernel ([`count_accept`]). Not the server's own view: a task busy answering (reading
    /// the file) notices a hang-up only later, and so may the server's socket under load (the
    /// client's FIN still on its way), so a strict client would show two connections. A client
    /// that connects again before closing its previous socket shows.
    pub fn max_concurrent(&self) -> usize {
        self.shared.max_active.load(Ordering::SeqCst)
    }

    /// Each accept that found earlier connections still open (for assertion messages): which,
    /// their request, and their TCP state.
    pub fn overlaps(&self) -> Vec<String> {
        self.shared.overlaps.lock().unwrap().clone()
    }

    /// Connections the server hasn't finished with (its own view: it may lag the client's close).
    pub fn active(&self) -> usize {
        self.shared.active.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Ctx {
    dir: PathBuf,
    faults: Faults,
    shared: Arc<Shared>,
    /// Start of the simulated live stream.
    epoch: Instant,
}

async fn run(std_listener: std::net::TcpListener, addr: SocketAddr, ctx: Arc<Ctx>, mut ctl: mpsc::UnboundedReceiver<Ctl>) {
    let mut listener = Some(TcpListener::from_std(std_listener).expect("tokio listener"));
    let mut conns = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            c = ctl.recv() => match c {
                Some(Ctl::Kill) => {
                    listener = None;
                    conns.abort_all();
                }
                Some(Ctl::Revive(done)) => {
                    if listener.is_none() {
                        // the old socket is closed; SO_REUSEADDR (tokio default) lets us rebind
                        for _ in 0..50 {
                            match TcpListener::bind(addr).await {
                                Ok(l) => {
                                    listener = Some(l);
                                    break;
                                }
                                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                            }
                        }
                    }
                    let _ = done.send(());
                }
                Some(Ctl::Shutdown) | None => {
                    conns.abort_all();
                    return;
                }
            },
            a = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                if let Ok((stream, _)) = a {
                    // counted here, as soon as possible after the client connected
                    let guard = ConnGuard::accepted(&ctx.shared, &stream);
                    conns.spawn(connection(stream, guard, ctx.clone()));
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

// The kernel's view of one TCP socket of this host, by an exact `sock_diag` lookup (what `ss`
// reads). Closing a socket doesn't close the connection while a child forked before that
// (another test's ffmpeg starting) still holds a copy of its descriptor, until its `exec`.

/// `TCP_ESTABLISHED` (`include/net/tcp_states.h`).
const ESTABLISHED: u8 = 1;
const SYN_SENT: u8 = 2;
const CLOSE_WAIT: u8 = 8;
/// `SOCK_DIAG_BY_FAMILY` (`linux/sock_diag.h`).
const SOCK_DIAG_BY_FAMILY: u16 = 20;
/// `struct nlmsghdr` + `struct inet_diag_req_v2` (with its `struct inet_diag_sockid`).
const REQUEST_LEN: usize = 16 + 8 + 48;

/// The state (`ESTABLISHED` = 1 …, as `include/net/tcp_states.h` numbers them) of this host's
/// TCP socket from `local` to `remote`; `None` when there is none any more.
pub fn tcp_state(local: SocketAddr, remote: SocketAddr) -> std::io::Result<Option<u8>> {
    let (family, src, dst) = match (local.ip(), remote.ip()) {
        (IpAddr::V4(l), IpAddr::V4(r)) => (libc::AF_INET, pad(&l.octets()), pad(&r.octets())),
        (l, r) => (libc::AF_INET6, v6(l), v6(r)),
    };
    let mut req = [0u8; REQUEST_LEN];
    // struct nlmsghdr: len, type, flags, seq, pid
    req[0..4].copy_from_slice(&(REQUEST_LEN as u32).to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&(libc::NLM_F_REQUEST as u16).to_ne_bytes());
    // struct inet_diag_req_v2: family, protocol, ext, pad, states
    req[16] = family as u8;
    req[17] = libc::IPPROTO_TCP as u8;
    req[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
    // struct inet_diag_sockid: sport, dport (big-endian), src[16], dst[16], if, cookie[2]
    req[24..26].copy_from_slice(&local.port().to_be_bytes());
    req[26..28].copy_from_slice(&remote.port().to_be_bytes());
    req[28..44].copy_from_slice(&src);
    req[44..60].copy_from_slice(&dst);
    req[64..72].fill(0xff); // INET_DIAG_NOCOOKIE: any socket with these addresses
    let mut answer = [0u8; 512];
    // SAFETY: plain syscalls on a socket owned here; each buffer is valid for the length given.
    let n = unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut kernel: libc::sockaddr_nl = std::mem::zeroed();
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let to = (&raw const kernel).cast::<libc::sockaddr>();
        if libc::sendto(fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0, to, size_of::<libc::sockaddr_nl>() as libc::socklen_t) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // the kernel answers within `sendto`
        let n = libc::recv(fd.as_raw_fd(), answer.as_mut_ptr().cast(), answer.len(), libc::MSG_DONTWAIT);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        n as usize
    };
    if n < 20 {
        return Err(std::io::Error::other("short sock_diag answer"));
    }
    match u16::from_ne_bytes([answer[4], answer[5]]) {
        // struct inet_diag_msg: family, state, …
        SOCK_DIAG_BY_FAMILY => Ok(Some(answer[17])),
        // struct nlmsgerr: a negative errno, ENOENT for no such socket
        t if t == libc::NLMSG_ERROR as u16 => match -i32::from_ne_bytes([answer[16], answer[17], answer[18], answer[19]]) {
            libc::ENOENT => Ok(None),
            e => Err(std::io::Error::from_raw_os_error(e)),
        },
        t => Err(std::io::Error::other(format!("sock_diag answered message type {t}"))),
    }
}

/// The connection from `local` to `remote` is still open on this host's side: no FIN has gone
/// out (`ESTABLISHED`, `SYN_SENT`, or `CLOSE_WAIT` after the peer's FIN). One closed, or
/// half-closed (`shutdown`, as hyper does before letting go), is winding down (`FIN_WAIT1` …)
/// or gone; the peer has seen it end.
pub fn tcp_open(local: SocketAddr, remote: SocketAddr) -> std::io::Result<bool> {
    Ok(tcp_state(local, remote)?.is_some_and(is_open))
}

fn is_open(state: u8) -> bool {
    matches!(state, ESTABLISHED | SYN_SENT | CLOSE_WAIT)
}

fn pad(v4: &[u8; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(v4);
    out
}

fn v6(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// An accepted connection, until its task ends.
struct OpenConn {
    /// The client's end.
    peer: SocketAddr,
    /// Its current request.
    path: String,
}

/// Count a new connection: itself, plus every open one whose client socket is still open in the
/// kernel now that the new one has connected ([`tcp_open`]: no FIN from the client yet, which a
/// child it forked can delay until its exec). Not the server's view of it: its task may be busy
/// answering (reading the file), and its socket may not have the client's FIN yet.
fn count_accept(shared: &Shared, conn: usize, stream: &TcpStream) {
    let (Ok(server), Ok(peer)) = (stream.local_addr(), stream.peer_addr()) else {
        // reset before the accept: a connection for a moment, nothing to watch
        shared.max_active.fetch_max(1, Ordering::SeqCst);
        return;
    };
    let mut open = shared.open.lock().unwrap();
    let held: Vec<String> = open
        .iter()
        .filter_map(|(c, o)| match tcp_state(o.peer, server) {
            Ok(Some(state)) if is_open(state) => Some(format!("connection {c} ({}, client TCP state {state})", o.path)),
            Ok(_) => None,
            Err(e) => Some(format!("connection {c} ({}, client TCP state unknown: {e})", o.path)),
        })
        .collect();
    shared.max_active.fetch_max(held.len() + 1, Ordering::SeqCst);
    if !held.is_empty() {
        shared.overlaps.lock().unwrap().push(format!("connection {conn} accepted while the client held {}", held.join(", ")));
    }
    open.insert(conn, OpenConn { peer, path: String::new() });
}

/// Decrements the active count, forgets the connection and stamps the close time even when the
/// task is aborted.
struct ConnGuard {
    shared: Arc<Shared>,
    index: Option<usize>,
    conn: usize,
}

impl ConnGuard {
    /// Number and count a new connection ([`count_accept`]).
    fn accepted(shared: &Arc<Shared>, stream: &TcpStream) -> Self {
        shared.active.fetch_add(1, Ordering::SeqCst);
        let conn = shared.connections.fetch_add(1, Ordering::SeqCst) + 1;
        count_accept(shared, conn, stream);
        Self { shared: shared.clone(), index: None, conn }
    }

    fn stamp_closed(&mut self) {
        if let Some(i) = self.index.take() {
            self.shared.requests.lock().unwrap()[i].closed = Some(Instant::now());
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.shared.open.lock().unwrap().remove(&self.conn);
        self.shared.active.fetch_sub(1, Ordering::SeqCst);
        self.stamp_closed();
    }
}

/// What a served request leaves of its connection.
enum After {
    /// Nothing: closed, cut or broken.
    Closed,
    /// Response complete on a persistent connection: read the next request.
    Next,
}

async fn connection(mut stream: TcpStream, mut guard: ConnGuard, ctx: Arc<Ctx>) {
    let shared = ctx.shared.clone();
    let conn = guard.conn;
    for nth in 1.. {
        let keep = *shared.keep_alive.lock().unwrap();
        let head = match keep {
            KeepAlive::IdleClose(idle) if nth > 1 => match tokio::time::timeout(idle, read_head(&mut stream)).await {
                Ok(h) => h,
                Err(_) => return, // idle: close without a word
            },
            _ => read_head(&mut stream).await,
        };
        let Some((method, path, headers)) = head else { return };
        if let Some(c) = shared.open.lock().unwrap().get_mut(&conn) {
            c.path.clone_from(&path);
        }
        let index = (!shared.unlogged.load(Ordering::SeqCst)).then(|| {
            let mut reqs = shared.requests.lock().unwrap();
            reqs.push(Request { path: path.clone(), headers: headers.clone(), status: 0, conn, nth, opened: Instant::now(), closed: None });
            reqs.len() - 1
        });
        guard.index = index;
        if keep == KeepAlive::Abandon && nth > 1 {
            // half-dead: never answered; hold the socket until the client gives up
            let mut probe = [0u8; 512];
            let _ = tokio::time::timeout(Duration::from_secs(120), async {
                while let Ok(n) = stream.read(&mut probe).await
                    && n > 0
                {}
            })
            .await;
            return;
        }
        let persist = keep != KeepAlive::Off && method == "GET";
        match respond(&mut stream, &ctx, &method, &path, &headers, index, persist).await {
            After::Next => guard.stamp_closed(),
            After::Closed => return,
        }
    }
}

/// Answer one request. `persist`: keep the connection open afterwards (`Connection:
/// keep-alive`) unless the response is an error or was cut.
async fn respond(
    stream: &mut TcpStream,
    ctx: &Ctx,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    index: Option<usize>,
    persist: bool,
) -> After {
    let shared = &ctx.shared;
    let set_status = |s: u16| {
        if let Some(i) = index {
            shared.requests.lock().unwrap()[i].status = s;
        }
    };

    if let Some(code) = shared.forced_status(path).or_else(|| shared.range_fault(path, headers)) {
        set_status(code);
        let _ = respond_error(stream, code).await;
        return After::Closed;
    }
    let is_segment = path.rsplit('/').next().is_some_and(|f| f.starts_with("seg") && f.ends_with(".ts"));
    if is_segment && let Some(every) = ctx.faults.segment_404_every {
        let k = shared.segment_count.fetch_add(1, Ordering::SeqCst) + 1;
        if every > 0 && k.is_multiple_of(every as usize) {
            set_status(404);
            let _ = respond_error(stream, 404).await;
            return After::Closed;
        }
    }
    let key = stream_key(path).to_owned();
    if !shared.open_session(&key) {
        set_status(403);
        let _ = respond_error(stream, 403).await;
        return After::Closed;
    }
    let _session = SessionGuard { shared: shared.clone(), key };

    let text = shared.bodies.lock().unwrap().get(path).cloned();
    let body = if let Some(t) = text {
        Some(t.into_bytes())
    } else if path == "/live/index.m3u8" {
        Some(live_playlist(ctx.epoch.elapsed()).into_bytes())
    } else {
        let rel = path.strip_prefix("/live/").map(|f| format!("long/{f}")).unwrap_or_else(|| path.trim_start_matches('/').to_owned());
        if rel.split('/').any(|c| c == ".." || c.is_empty()) { None } else { tokio::fs::read(ctx.dir.join(rel)).await.ok() }
    };
    let Some(body) = body else {
        set_status(404);
        let _ = respond_error(stream, 404).await;
        return After::Closed;
    };

    let total = body.len() as u64;
    let ranges = !shared.no_ranges.load(Ordering::SeqCst);
    let range = headers.iter().find(|(k, _)| k == "range").filter(|_| ranges).and_then(|(_, v)| parse_range(v, total));
    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    let status = if range.is_some() { 206 } else { 200 };
    set_status(status);
    let len = if total == 0 { 0 } else { end - start + 1 };
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {}\r\nContent-Length: {len}\r\n{}Connection: {}\r\n",
        reason(status),
        content_type(path),
        if ranges { "Accept-Ranges: bytes\r\n" } else { "" },
        if persist { "keep-alive" } else { "close" }
    );
    if range.is_some() {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return After::Closed;
    }
    if method == "HEAD" {
        return if persist { After::Next } else { After::Closed };
    }

    let first = shared.first_seen.lock().unwrap().insert(path.to_owned());
    let cut = ctx.faults.drop_after_bytes.filter(|_| first);
    let (mut rd, mut wr) = stream.split();
    let mut probe = [0u8; 512];
    let mut sent = 0u64;
    let data = &body[start as usize..(start + len) as usize];
    for chunk in data.chunks(16 * 1024) {
        if let Some(limit) = cut
            && sent + chunk.len() as u64 >= limit
        {
            let _ = wr.write_all(&chunk[..(limit - sent) as usize]).await;
            // reset instead of a clean FIN so the client sees a broken transfer
            let _ = wr.as_ref().set_zero_linger();
            return After::Closed;
        }
        if wr.write_all(chunk).await.is_err() {
            return After::Closed;
        }
        sent += chunk.len() as u64;
        let bps = *shared.throttle_bps.lock().unwrap();
        if let Some(bps) = bps.filter(|b| *b > 0) {
            // notice a client that hangs up while we pace (stop-before-start ordering checks
            // compare these close times)
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64(chunk.len() as f64 / bps as f64)) => {}
                r = rd.read(&mut probe) => if matches!(r, Ok(0) | Err(_)) { return After::Closed },
            }
        }
    }
    let _ = wr.flush().await;
    if persist {
        return After::Next;
    }
    let _ = wr.shutdown().await;
    // wait for the client to close (or 2 s) so `closed` reflects when the client let go
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(n) = rd.read(&mut probe).await
            && n > 0
        {}
    })
    .await;
    After::Closed
}

async fn read_head(stream: &mut TcpStream) -> Option<(String, String, Vec<(String, String)>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let read = async {
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            if buf.len() > 32 * 1024 {
                return false;
            }
            match stream.read(&mut tmp).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        true
    };
    if !matches!(tokio::time::timeout(Duration::from_secs(10), read).await, Ok(true)) {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?;
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let headers = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Some((method, path, headers))
}

/// `bytes=a-` / `bytes=a-b` → inclusive (start, end); `None` for anything else or out of range.
fn parse_range(v: &str, total: u64) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = if b.trim().is_empty() { total.checked_sub(1)? } else { b.trim().parse::<u64>().ok()?.min(total.checked_sub(1)?) };
    (start <= end).then_some((start, end))
}

async fn respond_error(stream: &mut TcpStream, code: u16) -> std::io::Result<()> {
    let body = format!("{code} {}\n", reason(code));
    let head =
        format!("HTTP/1.1 {code} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reason(code), body.len());
    stream.write_all(head.as_bytes()).await?;
    stream.shutdown().await.or_else(|e| if e.kind() == ErrorKind::NotConnected { Ok(()) } else { Err(e) })
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("ts") => "video/mp2t",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Number of 1 s segments in `long/`.
const LIVE_SEGMENTS: u64 = 60;
const LIVE_WINDOW: u64 = 4;

/// Sliding-window live playlist: the newest segment is the one "recorded" in the current
/// second; wraps around after 60 s with a discontinuity.
fn live_playlist(elapsed: Duration) -> String {
    let newest = elapsed.as_secs() + LIVE_WINDOW - 1;
    let first = newest + 1 - LIVE_WINDOW;
    let mut s = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    s.push_str(&format!("#EXT-X-DISCONTINUITY-SEQUENCE:{}\n", first / LIVE_SEGMENTS));
    for seq in first..=newest {
        if seq > first && seq % LIVE_SEGMENTS == 0 {
            s.push_str("#EXT-X-DISCONTINUITY\n");
        }
        s.push_str(&format!("#EXTINF:1.000000,\nseg{:03}.ts\n", seq % LIVE_SEGMENTS));
    }
    s
}
