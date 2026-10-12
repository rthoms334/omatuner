// Ported from pyatv pyatv/support/rtsp.py and pyatv/support/http.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! HTTP/1.1 + RTSP/1.0 on one TCP connection (DESIGN §40 §7.1 "Transport"). OWNER: CAST-2. Port
//! from pyatv `support/rtsp.py` (`RtspSession.exchange`: the RTSP headers, CSeq from 0) and
//! `support/http.py` (`_format_message`, `_parse_http_message`, `HttpConnection`'s in-order reply
//! matching) @ b277a4c8 (MIT).
//!
//! * Exactly one request is in flight: [`Connection::exchange`] takes `&mut self`, and the
//!   session's control task (`super::session::Control`) serializes everything else through it
//!   (PR 2899, foxCast: an RTSP `/feedback` racing an HTTP `/command` makes the TV drop the
//!   connection).
//! * RTSP replies are matched by `CSeq`, HTTP replies in order. A reply whose CSeq matches nothing
//!   pending — a `/feedback` answer that arrives after its request timed out, pyatv #2893 — is
//!   dropped and logged at debug, and so is the late reply of a timed-out HTTP request: neither
//!   ever shifts the matching onto the next request.
//! * RTSP requests carry `CSeq`, `DACP-ID`, `Active-Remote`, `Client-Instance` and `User-Agent:
//!   AirPlay/550.10` (pyatv); HTTP requests only what the caller sets (the pairing headers, the
//!   `/command` headers) plus a `User-Agent`.
//! * Encryption ([`super::hap::FrameCipher`]) is switched on right after pair-verify (foxCast: the
//!   TV drops the connection ~1 ms after a plaintext request). Timeouts: connect 3 s, request 5 s.
//! * Replies are untrusted: the head is capped at [`MAX_HEAD`], the body at
//!   [`super::hap::MAX_MESSAGE`]; anything else is [`Error::Malformed`] and ends the connection.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::bplist;
use super::hap::{FrameCipher, MAX_MESSAGE};
use super::trace::Trace;

/// pyatv `rtsp.USER_AGENT`; also the HTTP requests that set none of their own.
pub const USER_AGENT: &str = "AirPlay/550.10";
pub const BPLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest request or status line plus headers (foxCast's event-channel cap, used for both).
pub const MAX_HEAD: usize = 16 * 1024;

/// The request line's protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Rtsp,
    Http,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Rtsp => "RTSP/1.0",
            Proto::Http => "HTTP/1.1",
        }
    }
}

/// Why an exchange failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The TV closed the connection, it broke, or it couldn't be opened.
    Closed,
    /// No reply within [`REQUEST_TIMEOUT`] (or no connection within [`CONNECT_TIMEOUT`]). The
    /// connection stays usable: the late reply is dropped when it comes.
    Timeout,
    /// A frame that doesn't open (a bad tag, over a cap) or a message that isn't HTTP/RTSP: the
    /// connection is unusable.
    Malformed,
}

impl Error {
    /// The connection can't carry another request.
    pub fn fatal(self) -> bool {
        self != Error::Timeout
    }
}

/// One request (pyatv's `exchange` / `send_and_receive` arguments).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: &'static str,
    pub uri: String,
    pub proto: Proto,
    /// In wire order (the connection puts the RTSP headers in front).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn rtsp(method: &'static str, uri: impl Into<String>) -> Self {
        Self { method, uri: uri.into(), proto: Proto::Rtsp, headers: Vec::new(), body: Vec::new() }
    }

    pub fn http(method: &'static str, path: impl Into<String>) -> Self {
        Self { method, uri: path.into(), proto: Proto::Http, headers: Vec::new(), body: Vec::new() }
    }

    pub fn header(mut self, name: &str, value: impl ToString) -> Self {
        self.headers.push((name.to_owned(), value.to_string()));
        self
    }

    /// `header` unless one of that name is already set (case-insensitive).
    pub fn header_once(self, name: &str, value: impl ToString) -> Self {
        if self.has_header(name) { self } else { self.header(name, value) }
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// A binary-plist body (pyatv: a dict body is sent as a bplist with its Content-Type).
    pub fn plist(self, v: &bplist::Value) -> Self {
        self.header_once("Content-Type", BPLIST_CONTENT_TYPE).body(bplist::encode(v))
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// The wire bytes (pyatv `_format_message`): the request line, a `User-Agent` unless the
    /// caller set one, `Content-Length` when there is a body, the headers in order, a blank line
    /// and the body.
    pub fn encode(&self) -> Vec<u8> {
        let mut head = format!("{} {} {}\r\n", self.method, self.uri, self.proto.as_str());
        if !self.has_header("User-Agent") {
            head.push_str(&format!("User-Agent: {USER_AGENT}\r\n"));
        }
        if !self.body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// One complete HTTP/RTSP message: its first line, headers and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub start: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    /// The first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

/// A reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// "RTSP/1.0" or "HTTP/1.1".
    pub proto: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A status line `PROTO/VERSION CODE [REASON]`, else `None`.
    pub fn from_message(m: Message) -> Option<Self> {
        let mut parts = m.start.splitn(3, ' ');
        let proto = parts.next().filter(|p| p.contains('/'))?.to_owned();
        let status = parts.next()?.parse().ok()?;
        let reason = parts.next().unwrap_or_default().to_owned();
        Some(Self { proto, status, reason, headers: m.headers, body: m.body })
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    /// The body as a binary plist (`None` when empty or not one).
    pub fn plist(&self) -> Option<bplist::Value> {
        if self.body.is_empty() { None } else { bplist::decode(&self.body).ok() }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Take one complete message from the front of `buf` (pyatv `_parse_http_message`); `Ok(None)`:
/// more bytes are needed. A head over [`MAX_HEAD`], a header line without a colon, or a
/// `Content-Length` that isn't a number or is over [`MAX_MESSAGE`] is `Malformed`.
pub fn take_message(buf: &mut Vec<u8>) -> Result<Option<Message>, Error> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD { Err(Error::Malformed) } else { Ok(None) };
    };
    if end > MAX_HEAD {
        return Err(Error::Malformed);
    }
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| Error::Malformed)?;
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    for line in lines {
        let (k, v) = line.split_once(':').ok_or(Error::Malformed)?;
        headers.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    let len = match header(&headers, "Content-Length") {
        Some(v) => v.parse::<usize>().map_err(|_| Error::Malformed)?,
        None => 0,
    };
    if len > MAX_MESSAGE {
        return Err(Error::Malformed);
    }
    let body_at = end + 4;
    if buf.len() < body_at + len {
        return Ok(None);
    }
    let body = buf[body_at..body_at + len].to_vec();
    buf.drain(..body_at + len);
    Ok(Some(Message { start, headers, body }))
}

/// The request a reply may answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlight {
    Nothing,
    Http,
    Rtsp(u32),
}

/// What the connection runs over: a `TcpStream`, or an in-memory pipe in tests.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// The control connection to the TV's port 7000 (pyatv `HttpConnection` + `RtspSession`).
pub struct Connection {
    io: Box<dyn Io>,
    local: IpAddr,
    remote: IpAddr,
    cipher: Option<FrameCipher>,
    /// Bytes read but not opened yet (encrypted), and opened bytes not parsed yet.
    sealed: Vec<u8>,
    plain: Vec<u8>,
    /// The next RTSP CSeq (pyatv starts at 0).
    cseq: u32,
    /// pyatv `RtspSession`: `session_id = randrange(2**32)`, `dacp_id = f"{randrange(2**64):X}"`,
    /// `active_remote = randrange(2**32)`.
    session_id: u32,
    dacp_id: String,
    active_remote: String,
    /// HTTP requests that timed out: their replies, when they come, are dropped in order.
    late_http: usize,
    trace: Option<Trace>,
}

impl Connection {
    /// Connect within [`CONNECT_TIMEOUT`] (`Timeout`; refused or unroutable: `Closed`).
    pub async fn connect(addr: SocketAddr) -> Result<Self, Error> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.map_err(|_| Error::Timeout)?.map_err(|_| Error::Closed)?;
        let _ = stream.set_nodelay(true);
        let local = stream.local_addr().map_err(|_| Error::Closed)?.ip();
        Ok(Self::over(Box::new(stream), local, addr.ip()))
    }

    /// A connection over any byte stream (tests use `tokio::io::duplex`).
    pub fn over(io: Box<dyn Io>, local: IpAddr, remote: IpAddr) -> Self {
        Self {
            io,
            local,
            remote,
            cipher: None,
            sealed: Vec::new(),
            plain: Vec::new(),
            cseq: 0,
            session_id: super::random_u32(),
            dacp_id: format!("{:X}", super::random_u64()),
            active_remote: super::random_u32().to_string(),
            late_http: 0,
            trace: None,
        }
    }

    /// This side's address toward the TV (SETUP's `Addresses`, the session URI).
    pub fn local_ip(&self) -> IpAddr {
        self.local
    }

    /// The TV's address (the event channel connects there).
    pub fn remote_ip(&self) -> IpAddr {
        self.remote
    }

    /// pyatv `RtspSession.uri`: `rtsp://<local ip>/<session id>`.
    pub fn session_uri(&self) -> String {
        match self.local {
            IpAddr::V4(a) => format!("rtsp://{a}/{}", self.session_id),
            IpAddr::V6(a) => format!("rtsp://[{a}]/{}", self.session_id),
        }
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// Everything from now on is HAP-framed (right after pair-verify).
    pub fn encrypt(&mut self, cipher: FrameCipher) {
        self.cipher = Some(cipher);
    }

    /// Record the transcript (`TUNER_CAST_TRACE`).
    pub fn set_trace(&mut self, trace: Option<Trace>) {
        self.trace = trace;
    }

    /// Send `req` and wait for its reply (pyatv `RtspSession.exchange` /
    /// `HttpConnection.send_and_receive`). Any status is returned: the caller decides what an
    /// error status means.
    pub async fn exchange(&mut self, mut req: Request) -> Result<Response, Error> {
        let cseq = (req.proto == Proto::Rtsp).then(|| {
            let cseq = self.cseq;
            self.cseq = self.cseq.wrapping_add(1);
            let rtsp = [
                ("CSeq".to_owned(), cseq.to_string()),
                ("DACP-ID".to_owned(), self.dacp_id.clone()),
                ("Active-Remote".to_owned(), self.active_remote.clone()),
                ("Client-Instance".to_owned(), self.dacp_id.clone()),
            ];
            req.headers.splice(0..0, rtsp);
            cseq
        });
        if let Some(t) = &self.trace {
            t.request(super::session::Channel::Control, &req);
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let wire = req.encode();
        let wire = match &mut self.cipher {
            Some(c) => c.seal(&wire),
            None => wire,
        };
        // A write that can't finish leaves half a request on the wire: the connection is done.
        match tokio::time::timeout_at(deadline, self.io.write_all(&wire)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return Err(Error::Closed),
        }
        let waiting = match cseq {
            Some(c) => InFlight::Rtsp(c),
            None => InFlight::Http,
        };
        loop {
            while let Some(msg) = take_message(&mut self.plain)? {
                let resp = Response::from_message(msg).ok_or(Error::Malformed)?;
                if self.claim(&resp, waiting) {
                    if let Some(t) = &self.trace {
                        t.response(super::session::Channel::Control, &resp);
                    }
                    return Ok(resp);
                }
            }
            match tokio::time::timeout_at(deadline, self.fill()).await {
                Ok(r) => r?,
                Err(_) => {
                    if cseq.is_none() {
                        self.late_http += 1;
                    }
                    tracing::debug!("cast: no reply to {} {} within {:?}", req.method, path_of(&req.uri), REQUEST_TIMEOUT);
                    return Err(Error::Timeout);
                }
            }
        }
    }

    /// Wait for bytes while nothing is in flight (cancel-safe: the control task selects on it).
    /// Whatever arrives now is a late reply and is dropped; `Err` once the TV closed.
    pub async fn idle(&mut self) -> Result<(), Error> {
        self.fill().await?;
        while let Some(msg) = take_message(&mut self.plain)? {
            let resp = Response::from_message(msg).ok_or(Error::Malformed)?;
            self.claim(&resp, InFlight::Nothing);
        }
        Ok(())
    }

    /// Whether `resp` answers the request in flight. A reply with a CSeq answers the RTSP request
    /// with that CSeq (or, being an HTTP reply, the HTTP request in flight); anything else with a
    /// CSeq is a stray. A reply without one answers the oldest request still owed a reply — a
    /// timed-out HTTP request first, whose reply is dropped (pyatv's HTTP matching is in order).
    fn claim(&mut self, resp: &Response, waiting: InFlight) -> bool {
        if let Some(theirs) = resp.header("CSeq") {
            match waiting {
                InFlight::Rtsp(ours) if theirs.parse() == Ok(ours) => return true,
                InFlight::Http if resp.proto.starts_with("HTTP/") && self.late_http == 0 => return true,
                _ => {
                    tracing::debug!("cast: dropping a stray reply (CSeq {}, status {})", theirs.chars().take(10).collect::<String>(), resp.status);
                    return false;
                }
            }
        }
        if self.late_http > 0 {
            self.late_http -= 1;
            tracing::debug!("cast: dropping the late reply of a timed-out request (status {})", resp.status);
            return false;
        }
        if waiting == InFlight::Nothing {
            tracing::debug!("cast: dropping a reply nothing asked for (status {})", resp.status);
            return false;
        }
        true
    }

    /// Read more bytes (cancel-safe) and open the complete frames among them.
    async fn fill(&mut self) -> Result<(), Error> {
        let mut buf = [0u8; 16 * 1024];
        let n = self.io.read(&mut buf).await.map_err(|_| Error::Closed)?;
        if n == 0 {
            return Err(Error::Closed);
        }
        match &mut self.cipher {
            None => self.plain.extend_from_slice(&buf[..n]),
            Some(c) => {
                self.sealed.extend_from_slice(&buf[..n]);
                while let Some(plain) = c.open(&mut self.sealed).map_err(|_| Error::Malformed)? {
                    self.plain.extend_from_slice(&plain);
                }
            }
        }
        Ok(())
    }
}

/// A request URI for logs: the path only (an `rtsp://<address>/…` URI loses its address).
pub fn path_of(uri: &str) -> &str {
    match uri.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => uri,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    use super::*;

    fn pipe() -> (Connection, DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        (Connection::over(Box::new(ours), ip, ip), theirs)
    }

    /// Read one request from the TV's side of the pipe.
    async fn next_request(tv: &mut DuplexStream, buf: &mut Vec<u8>) -> Message {
        loop {
            if let Some(m) = take_message(buf).unwrap() {
                return m;
            }
            let mut chunk = [0u8; 4096];
            let n = tv.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    #[test]
    fn requests_are_formatted_like_pyatv() {
        let req = Request::rtsp("SETUP", "rtsp://10.0.0.2/77").header("CSeq", 0).body(b"xy".to_vec());
        assert_eq!(req.encode(), b"SETUP rtsp://10.0.0.2/77 RTSP/1.0\r\nUser-Agent: AirPlay/550.10\r\nContent-Length: 2\r\nCSeq: 0\r\n\r\nxy");
        // the caller's User-Agent wins; no body, no Content-Length
        let req = Request::http("GET", "/info").header("User-Agent", "AirPlay/960.13.1");
        assert_eq!(req.encode(), b"GET /info HTTP/1.1\r\nUser-Agent: AirPlay/960.13.1\r\n\r\n");
        assert_eq!(path_of("rtsp://10.0.0.2/77"), "/77");
        assert_eq!(path_of("/feedback"), "/feedback");
    }

    #[test]
    fn messages_are_split_on_content_length_and_capped() {
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 2\r\n\r\nabHTTP/1.1 500 Internal".to_vec();
        let m = take_message(&mut buf).unwrap().unwrap();
        let r = Response::from_message(m).unwrap();
        assert_eq!((r.proto.as_str(), r.status, r.header("cseq"), r.body.as_slice()), ("RTSP/1.0", 200, Some("3"), &b"ab"[..]));
        assert_eq!(take_message(&mut buf), Ok(None), "the second reply is incomplete");
        assert_eq!(buf, b"HTTP/1.1 500 Internal");
        let mut big = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_MESSAGE + 1).into_bytes();
        assert_eq!(take_message(&mut big), Err(Error::Malformed));
        let mut endless = vec![b'a'; MAX_HEAD + 1];
        assert_eq!(take_message(&mut endless), Err(Error::Malformed));
        let mut no_colon = b"HTTP/1.1 200 OK\r\nbogus\r\n\r\n".to_vec();
        assert_eq!(take_message(&mut no_colon), Err(Error::Malformed));
    }

    #[tokio::test]
    async fn rtsp_requests_carry_cseq_from_zero_and_the_dacp_headers() {
        let (mut conn, mut tv) = pipe();
        let client = tokio::spawn(async move {
            let a = conn.exchange(Request::rtsp("RECORD", "rtsp://127.0.0.1/1")).await.unwrap();
            let b = conn.exchange(Request::rtsp("POST", "/feedback")).await.unwrap();
            (a.status, b.status)
        });
        let mut buf = Vec::new();
        for want in ["0", "1"] {
            let req = next_request(&mut tv, &mut buf).await;
            assert_eq!(req.header("CSeq"), Some(want));
            assert_eq!(req.header("User-Agent"), Some(USER_AGENT));
            let dacp = req.header("DACP-ID").unwrap();
            assert!(!dacp.is_empty() && dacp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
            assert_eq!(req.header("Client-Instance"), Some(dacp));
            assert!(req.header("Active-Remote").unwrap().parse::<u32>().is_ok());
            tv.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {want}\r\n\r\n").as_bytes()).await.unwrap();
        }
        assert_eq!(client.await.unwrap(), (200, 200));
    }

    /// pyatv #2893: a `/feedback` reply that comes after its request timed out lands while the
    /// next request (an HTTP `/command`) waits. It is dropped by its CSeq, and the HTTP reply
    /// behind it is the command's. A late HTTP reply is dropped in order too.
    #[tokio::test(start_paused = true)]
    async fn stray_late_replies_never_shift_the_matching() {
        let (mut conn, mut tv) = pipe();
        let mut buf = Vec::new();

        // 1. /feedback (CSeq 0) times out: the TV answers too late.
        let feedback = conn.exchange(Request::rtsp("POST", "/feedback")).await;
        assert_eq!(feedback, Err(Error::Timeout));
        let req = next_request(&mut tv, &mut buf).await;
        assert_eq!(req.start, "POST /feedback RTSP/1.0");

        // 2. /command (HTTP): the late feedback reply arrives first, then the command's.
        tv.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 0\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        tv.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        let resp = conn.exchange(Request::http("POST", "/command")).await.unwrap();
        assert_eq!((resp.proto.as_str(), resp.status), ("HTTP/1.1", 500));

        // 3. An HTTP request times out; its reply comes during the next RTSP exchange and is
        //    dropped (no CSeq), the RTSP reply is matched by its CSeq.
        assert_eq!(conn.exchange(Request::http("GET", "/info")).await, Err(Error::Timeout));
        tv.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlate").await.unwrap();
        tv.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n").await.unwrap(); // matches nothing
        tv.write_all(b"RTSP/1.0 204 No Content\r\nCSeq: 1\r\n\r\n").await.unwrap();
        let resp = conn.exchange(Request::rtsp("POST", "/feedback")).await.unwrap();
        assert_eq!(resp.status, 204);
    }

    #[tokio::test]
    async fn a_closed_connection_is_reported() {
        let (mut conn, tv) = pipe();
        drop(tv);
        assert_eq!(conn.exchange(Request::http("GET", "/info")).await, Err(Error::Closed));
        assert!(conn.idle().await.is_err());
    }
}
