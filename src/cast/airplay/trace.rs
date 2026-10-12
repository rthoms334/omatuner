//! `TUNER_CAST_TRACE=1` (dev only; DESIGN §40 §7.3 "Trace"): a redacted JSON transcript of a session
//! in `AppPaths::cast_trace_dir()` (0600): request lines, statuses, decoded plists, event bodies;
//! tokens, URL paths, addresses, MACs and ids replaced by placeholders. Off by default. OWNER:
//! CAST-2.
//!
//! One file per session, one JSON object per line (`cast-<unix time>-<4 hex>.jsonl`, so a crash
//! leaves every line before it readable): `{"t": seconds, "chan": "control"|"events", "dir":
//! "out"|"in", "line", "status", "headers", "body"}`. §18 runs the first real session with it and
//! turns the transcript into fixtures. Bodies: binary plists decoded (nested `data` plists too),
//! text redacted, anything else (the TLV8 of pair-verify) only its size.
//!
//! Redaction, before anything is written: registered secrets and credential patterns
//! ([`crate::redact::redact`]: the relay token is a registered secret), then every URL becomes
//! `<scheme>://<addr>/<path>`, IPv4 addresses `<addr>`, MAC-like ids `<mac>`, UUIDs `<uuid>`; the
//! id headers (`DACP-ID`, `Active-Remote`, `Client-Instance`, `Session`) and the plist keys that
//! hold device ids (`deviceID`, `pi`, `psi`, `pk`, `gid`, `serialNumber`, …) become `<id>`. The
//! directory is the default profile's (dev only).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Value as Json, json};

use super::bplist::{self, Value};
use super::rtsp::{Message, Request, Response};
use super::session::Channel;

/// The switch (`1` turns it on).
pub const ENV: &str = "TUNER_CAST_TRACE";

/// Plist keys whose values identify a device, a sender or a session.
const ID_KEYS: &[&str] = &[
    "deviceID",
    "macAddress",
    "pi",
    "psi",
    "pk",
    "gid",
    "serialNumber",
    "btaddr",
    "senderAddress",
    "sessionUUID",
    "sessionCorrelationUUID",
    "clientUUID",
    "uuid",
    "ID",
    "channelID",
    "ClockID",
    "ClockPorts",
    "SenderMACAddress",
];
/// Headers whose values identify the sender or the session.
const ID_HEADERS: &[&str] = &["dacp-id", "active-remote", "client-instance", "session", "x-apple-session-id", "x-apple-client-id"];

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\b([a-z][a-z0-9+.-]*)://[^\s"'<>]*"#).unwrap());
static IPV4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").unwrap());
static MAC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}\b").unwrap());
static UUID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\b").unwrap());

/// A session's transcript (cheap to clone: the control and event tasks share one file).
#[derive(Clone)]
pub struct Trace(Arc<Mutex<Writer>>);

struct Writer {
    file: File,
    start: Instant,
}

impl Trace {
    /// A new transcript when `TUNER_CAST_TRACE=1`, else `None` (also when the file can't be made:
    /// a warning, and the session goes on untraced).
    pub fn start() -> Option<Self> {
        if std::env::var_os(ENV).is_none_or(|v| v != "1") {
            return None;
        }
        let dir = crate::paths::AppPaths::from_env().ok()?.cast_trace_dir();
        match Self::create(&dir) {
            Ok((trace, path)) => {
                tracing::info!("cast: tracing this session to {}", path.display());
                Some(trace)
            }
            Err(e) => {
                tracing::warn!("cast: can't write a trace in {}: {e}", dir.display());
                None
            }
        }
    }

    /// A new 0600 file in `dir` (created 0700).
    pub fn create(dir: &Path) -> std::io::Result<(Self, PathBuf)> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let path = dir.join(format!("cast-{secs}-{:04x}.jsonl", super::random_u32() & 0xffff));
        let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        Ok((Self(Arc::new(Mutex::new(Writer { file, start: Instant::now() }))), path))
    }

    /// A request we send.
    pub fn request(&self, chan: Channel, req: &Request) {
        let line = text(&format!("{} {} {}", req.method, req.uri, req.proto.as_str()));
        self.write(chan, "out", json!({ "line": line, "headers": headers(&req.headers), "body": body(&req.headers, &req.body) }));
    }

    /// A reply we got.
    pub fn response(&self, chan: Channel, resp: &Response) {
        self.write(chan, "in", json!({ "status": resp.status, "headers": headers(&resp.headers), "body": body(&resp.headers, &resp.body) }));
    }

    /// A request the TV sent (the event channel).
    pub fn incoming(&self, chan: Channel, msg: &Message) {
        self.write(chan, "in", json!({ "line": text(&msg.start), "headers": headers(&msg.headers), "body": body(&msg.headers, &msg.body) }));
    }

    fn write(&self, chan: Channel, dir: &str, mut entry: Json) {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Json::Object(m) = &mut entry {
            m.insert("t".into(), json!((w.start.elapsed().as_secs_f64() * 1000.0).round() / 1000.0));
            m.insert("chan".into(), json!(chan.as_str()));
            m.insert("dir".into(), json!(dir));
        }
        let mut line = entry.to_string();
        line.push('\n');
        if let Err(e) = w.file.write_all(line.as_bytes()) {
            tracing::debug!("cast: trace write failed: {e}");
        }
    }
}

/// Free text with every credential, URL path, address, MAC and UUID replaced.
pub fn text(s: &str) -> String {
    let s = crate::redact::redact(s);
    let s = URL.replace_all(&s, "${1}://<addr>/<path>");
    let s = IPV4.replace_all(&s, "<addr>");
    let s = MAC.replace_all(&s, "<mac>");
    UUID.replace_all(&s, "<uuid>").into_owned()
}

fn headers(h: &[(String, String)]) -> Json {
    Json::Array(
        h.iter()
            .map(|(k, v)| {
                let v = if ID_HEADERS.contains(&k.to_ascii_lowercase().as_str()) { "<id>".to_owned() } else { text(v) };
                json!([k, v])
            })
            .collect(),
    )
}

fn body(headers: &[(String, String)], b: &[u8]) -> Json {
    if b.is_empty() {
        return Json::Null;
    }
    if b.starts_with(b"bplist00") {
        return match bplist::decode(b) {
            Ok(v) => plist(&v),
            Err(_) => json!(format!("<{} bytes>", b.len())),
        };
    }
    let textual = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("Content-Type") && (v.starts_with("text/") || v.contains("parameters")));
    match std::str::from_utf8(b) {
        Ok(s) if textual => json!(text(s)),
        _ => json!(format!("<{} bytes>", b.len())),
    }
}

/// A plist as JSON, redacted: id keys become `<id>`, strings go through [`text`], `data` that is
/// itself a plist (the `/command` and event bodies) is decoded, other data only sized.
pub fn plist(v: &Value) -> Json {
    match v {
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Real(r) if r.is_finite() => json!(r),
        Value::Real(r) => json!(r.to_string()),
        Value::String(s) => json!(text(s)),
        Value::Data(d) => match d.starts_with(b"bplist00").then(|| bplist::decode(d).ok()).flatten() {
            Some(inner) => plist(&inner),
            None => json!(format!("<{} bytes>", d.len())),
        },
        Value::Date(d) => json!(d),
        Value::Array(a) => Json::Array(a.iter().map(plist).collect()),
        Value::Dict(d) => {
            Json::Object(d.iter().map(|(k, v)| (k.clone(), if ID_KEYS.contains(&k.as_str()) { json!("<id>") } else { plist(v) })).collect())
        }
        Value::Uid(u) => json!(u),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::airplay::{Profile, queue, session};

    /// The transcript is shared to make fixtures: no address, token, MAC or id may reach it.
    #[test]
    fn nothing_identifying_reaches_the_transcript() {
        let token = "q3k7m2x9w4p8z5c1v6b0n3h7je";
        let url = format!("http://192.168.1.88:47800/c/{token}/index.m3u8");
        let insert = queue::insert(Profile::ThirdParty, "6F1C2D3E-4A5B-4C6D-8E7F-901234567890", &url, Some("0A1B2C3D-4E5F-4061-8273-948596A7B8C9"));
        let setup =
            session::setup_body("192.168.1.88", "11111111-2222-4333-8444-555555555555", "AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE", "02:1A:2B:3C:4D:5E");
        let info = queue::dict([
            ("deviceID", queue::text("A8:51:AB:01:02:03")),
            ("psi", queue::text("PSI-SECRET-ID")),
            ("pk", Value::Data(vec![7; 32])),
            ("serialNumber", queue::text("C07XYZ")),
            ("senderAddress", queue::text("192.168.1.88:50123")),
            ("model", queue::text("AppleTV14,1")),
        ]);
        let out = format!("{}{}{}", plist(&insert), plist(&setup), plist(&info));
        for leak in [token, "192.168", "02:1A", "A8:51", "6F1C2D3E", "0A1B2C3D", "AAAAAAAA", "PSI-SECRET", "C07XYZ", "/c/"] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(out.contains("AppleTV14,1") && out.contains("<addr>") && out.contains("insertPlayQueueItem"), "{out}");
        let line = text("SETUP rtsp://192.168.1.88/3735928559 RTSP/1.0");
        assert_eq!(line, "SETUP rtsp://<addr>/<path> RTSP/1.0");
        let h = headers(&[("DACP-ID".into(), "A1B2C3D4E5F60718".into()), ("CSeq".into(), "3".into())]).to_string();
        assert!(!h.contains("A1B2C3D4") && h.contains("\"3\""), "{h}");
    }

    #[test]
    fn a_trace_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (trace, path) = Trace::create(&dir.path().join("cast-trace")).unwrap();
        trace.response(Channel::Control, &Response { proto: "RTSP/1.0".into(), status: 200, reason: "OK".into(), headers: vec![], body: vec![] });
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let line: Json = serde_json::from_str(std::fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!((line["status"].as_u64(), line["chan"].as_str(), line["dir"].as_str()), (Some(200), Some("control"), Some("in")));
    }
}
