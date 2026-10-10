//! Test media generated with ffmpeg (scripts/gen-test-media.sh) — never committed. Owner: WP-07.
//! Tests that need media call `ensure_media()` and SKIP (return early with a note) when ffmpeg
//! is unavailable, so `cargo test` stays green on minimal machines.
//!
//! [`TestServer`] is a tiny HTTP/1.1 file server (one request per connection, `Connection:
//! close`) bound to 127.0.0.1 on an ephemeral port, running on its own thread and runtime so
//! blocking tests can use it. It records every request (path, headers, open/close time),
//! supports `Range`, and injects faults: forced statuses per path, connections cut after N
//! bytes, 404 for every Nth HLS segment, throttling, and "kill"/"revive" of the whole server
//! (listener closed, open connections reset). `/live/index.m3u8` is a sliding-window live
//! playlist over the `long/` segments (one new 1 s segment per second, no `ENDLIST`).

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Bump when the script's outputs change (forces regeneration of an existing directory).
const MEDIA_VERSION: &str = "2";
const STAMP: &str = ".complete";

/// Directory with `testsrc.mp4` (10 s 640×360 25 fps + 1 kHz sine), `bars.ts` (MPEG-TS),
/// `hls/index.m3u8` (6 × 2 s segments) — generated on first use into `target/test-media`.
/// Also `long/index.m3u8` (60 × 1 s segments; see the script).
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
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
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

/// One HTTP request as seen by the server.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    /// Lower-cased names, values as sent.
    pub headers: Vec<(String, String)>,
    pub status: u16,
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
    throttle_bps: Mutex<Option<u64>>,
}

enum Ctl {
    Kill,
    Revive,
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
    pub fn url(&self, path: &str) -> String { format!("{}/{}", self.base, path.trim_start_matches('/')) }

    /// Answer every request for `path` (e.g. `/a.ts`) with `status` (0 = back to normal).
    pub fn set_status(&self, path: &str, status: u16) {
        let mut m = self.shared.status.lock().unwrap();
        if status == 0 {
            m.remove(path);
        } else {
            m.insert(path.to_owned(), status);
        }
    }

    /// Limit every response body to `bps` bytes per second (`None` = unlimited).
    pub fn set_throttle(&self, bps: Option<u64>) { *self.shared.throttle_bps.lock().unwrap() = bps; }

    /// Close the listener (connection refused) and reset every open connection.
    pub fn kill(&self) { let _ = self.ctl.send(Ctl::Kill); }

    /// Listen again on the same port.
    pub fn revive(&self) { let _ = self.ctl.send(Ctl::Revive); }

    pub fn requests(&self) -> Vec<Request> { self.shared.requests.lock().unwrap().clone() }

    pub fn requests_for(&self, path: &str) -> Vec<Request> { self.requests().into_iter().filter(|r| r.path == path).collect() }

    /// Highest number of simultaneously open connections so far.
    pub fn max_concurrent(&self) -> usize { self.shared.max_active.load(Ordering::SeqCst) }

    pub fn active(&self) -> usize { self.shared.active.load(Ordering::SeqCst) }
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
                Some(Ctl::Revive) => {
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
                }
                Some(Ctl::Shutdown) | None => {
                    conns.abort_all();
                    return;
                }
            },
            a = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                if let Ok((stream, _)) = a {
                    conns.spawn(connection(stream, ctx.clone()));
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// Decrements the active count and stamps the close time even when the task is aborted.
struct ConnGuard {
    shared: Arc<Shared>,
    index: Option<usize>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.shared.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(i) = self.index {
            self.shared.requests.lock().unwrap()[i].closed = Some(Instant::now());
        }
    }
}

async fn connection(mut stream: TcpStream, ctx: Arc<Ctx>) {
    let shared = ctx.shared.clone();
    let n = shared.active.fetch_add(1, Ordering::SeqCst) + 1;
    shared.max_active.fetch_max(n, Ordering::SeqCst);
    let mut guard = ConnGuard { shared: shared.clone(), index: None };

    let Some((method, path, headers)) = read_head(&mut stream).await else { return };
    let index = {
        let mut reqs = shared.requests.lock().unwrap();
        reqs.push(Request { path: path.clone(), headers: headers.clone(), status: 0, opened: Instant::now(), closed: None });
        reqs.len() - 1
    };
    guard.index = Some(index);
    let set_status = |s: u16| shared.requests.lock().unwrap()[index].status = s;

    let forced = shared.status.lock().unwrap().get(&path).copied();
    if let Some(code) = forced {
        set_status(code);
        let _ = respond_error(&mut stream, code).await;
        return;
    }
    let is_segment = path.rsplit('/').next().is_some_and(|f| f.starts_with("seg") && f.ends_with(".ts"));
    if is_segment && let Some(every) = ctx.faults.segment_404_every {
        let k = shared.segment_count.fetch_add(1, Ordering::SeqCst) + 1;
        if every > 0 && k.is_multiple_of(every as usize) {
            set_status(404);
            let _ = respond_error(&mut stream, 404).await;
            return;
        }
    }

    let body = if path == "/live/index.m3u8" {
        Some(live_playlist(ctx.epoch.elapsed()).into_bytes())
    } else {
        let rel = path.strip_prefix("/live/").map(|f| format!("long/{f}")).unwrap_or_else(|| path.trim_start_matches('/').to_owned());
        if rel.split('/').any(|c| c == ".." || c.is_empty()) {
            None
        } else {
            tokio::fs::read(ctx.dir.join(rel)).await.ok()
        }
    };
    let Some(body) = body else {
        set_status(404);
        let _ = respond_error(&mut stream, 404).await;
        return;
    };

    let total = body.len() as u64;
    let range = headers.iter().find(|(k, _)| k == "range").and_then(|(_, v)| parse_range(v, total));
    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    let status = if range.is_some() { 206 } else { 200 };
    set_status(status);
    let len = if total == 0 { 0 } else { end - start + 1 };
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
        reason(status),
        content_type(&path)
    );
    if range.is_some() {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() || method == "HEAD" {
        return;
    }

    let first = shared.first_seen.lock().unwrap().insert(path.clone());
    let cut = ctx.faults.drop_after_bytes.filter(|_| first);
    let (mut rd, mut wr) = stream.into_split();
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
            return;
        }
        if wr.write_all(chunk).await.is_err() {
            return;
        }
        sent += chunk.len() as u64;
        let bps = *shared.throttle_bps.lock().unwrap();
        if let Some(bps) = bps.filter(|b| *b > 0) {
            // notice a client that hangs up while we pace (stop-before-start ordering checks
            // compare these close times)
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64(chunk.len() as f64 / bps as f64)) => {}
                r = rd.read(&mut probe) => if matches!(r, Ok(0) | Err(_)) { return },
            }
        }
    }
    let _ = wr.flush().await;
    let _ = wr.shutdown().await;
    // wait for the client to close (or 2 s) so `closed` reflects when the client let go
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(n) = rd.read(&mut probe).await
            && n > 0
        {}
    })
    .await;
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
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason(code),
        body.len()
    );
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
