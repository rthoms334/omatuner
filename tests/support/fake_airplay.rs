// Ported from pyatv tests/fake_device/airplay.py (the state / use-case split) and
// pyatv/protocols/airplay/server_auth.py (the receiver's routes) @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! A fake AirPlay 2 receiver (DESIGN §40 §14.3; CAST-3): an Apple TV (or a third-party AirPlay
//! TV) on 127.0.0.1 that pairs, verifies, takes the `/command` play queue, pushes events and
//! fetches the relay's playlist the way AVPlayer does. No open-source receiver implements the
//! play queue (foxCast's receiver and pyatv's fake only do the legacy `/play`), so this one is
//! assembled from the references' own fakes, tests, captures and notes, never from a session on
//! the user's TV [FAKE]:
//! * pair-setup (`/pair-pin-start`, M1–M6) and pair-verify (M1–M4): pyatv `server_auth.py`, made
//!   strict, through CAST-1's receiver halves (`airplay::pair::server`); a shadow SRP server with
//!   the same salt and `b` gives the fake `K`, so it can read M5 (the HKP 5 ACL) and forge M6;
//! * what strict means (scenarios only): foxCast's receiver tests — a wrong SRP proof and a
//!   forged controller signature are refused, a wrong PIN at M3 gets Error TLV 2, 453 on
//!   pin-start is a success;
//! * SETUP, RECORD, the type-130 SETUP, the `/feedback` reply `{streams: []}`, the event
//!   channel's `updateInfo` and its required `RTSP/1.0 200`, the HKDF labels: pyatv
//!   `docs/documentation/protocols.md` L1525–1979;
//! * the `/command` decoding, order and headers, "a non-2xx stops the sequence", the event shapes
//!   `{params: {data}}` and `params.playbackState`: robkochman's `test_airplay_v2_player.py`;
//! * the client-name rule: PR 2894's fake-receiver test; the quirks (`playerLoggingID` over 6
//!   characters → silent close, a request while another is unanswered → close, NTP → no events,
//!   then close): PR 2899; split and invalid event frames: foxCast `event_channel_test.go`
//!   (scenarios only); state and notification names: foxCast `docs/05`; the `playbackInfo` reply:
//!   PR 2846 with LIVI's keys (facts only); `/info`: the scrubbed LAN captures (§5.3).
//!
//! The transport is the fake's own (ChaCha20-Poly1305 frames on aws-lc-rs, HKDF labels spelt out
//! here), not the client's `hap::FrameCipher`, so the two sides don't share a misreading; it can
//! also write frames the client never does (one 8931-byte frame, a 1024 + 1259 split, a bad tag).
//!
//! Knobs are fixed at start ([`FakeConfig`], the §14.3 table plus a few quirk switches); the
//! use cases act on a running fake ([`FakeAirPlay::stop_from_remote`],
//! [`FakeAirPlay::send_bad_frame`], [`FakeAirPlay::fail_next_commands`], …). Every request,
//! fetch and event is recorded in order ([`Entry`]) so tests assert exact sequences. Waits are
//! bounded condition polls ([`FakeAirPlay::until`]).
//!
//! Include with `#[path = "support/fake_airplay.rs"] mod fake_airplay;`. It runs on its own
//! thread and runtime, so a test's runtime only drives the client.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::hkdf;
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tuner::cast::airplay::bplist::{self, Value};
use tuner::cast::airplay::pair::server::{Accessory, PairSetupServer, PairVerifyServer};
use tuner::cast::airplay::srp::SrpServer;
use tuner::cast::airplay::tlv8::{self, tag};
use tuner::cast::{Credentials, Identity, PairVariant, PeerCredentials};

/// How long [`FakeAirPlay::until`] waits before failing the test.
pub const WAIT: Duration = Duration::from_secs(15);

/// The receiver's `psi` (`/info`): the type-130 `channelID` is `<psi>-RCS-1`. Scrubbed.
pub const PSI: &str = "6F0C3A52-1B7E-4D2C-9A11-5E0D2C7B4F10";
/// The receiver's `deviceID` / `macAddress` (scrubbed).
pub const DEVICE_ID: &str = "AA:BB:CC:00:00:01";
/// The stream id the type-130 SETUP answers (the `X-Apple-StreamID` the client must send back).
pub const STREAM_ID: i64 = 1;
/// A [`Profile::ThirdParty`] fake's mDNS TXT record: the user's Hisense's (scrubbed). Its `psi`
/// and `gid` are what VioletRelay keys the `channelID` and `SETPEERSX`'s `ClockPorts` by; the
/// fake's `/info` has only `pi` (= [`DEVICE_ID`], unlike the record's), as the TV's does.
pub const HISENSE_TXT: &str = include_str!("../fixtures/cast/txt_hisense.txt");

/// A TXT fixture's `key=value` lines (comments skipped).
pub fn txt(fixture: &str) -> Vec<(String, String)> {
    fixture.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

/// One key of [`HISENSE_TXT`].
pub fn hisense_txt(key: &str) -> String {
    txt(HISENSE_TXT).into_iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or_else(|| panic!("no {key} in the Hisense TXT"))
}

// ---------------------------------------------------------------------------------------------
// Configuration (the §14.3 "Modes" table, plus quirk switches)
// ---------------------------------------------------------------------------------------------

/// Which tvOS the fake behaves as (DESIGN §40 §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tvos {
    /// The user's Apple TV today: the 26.6 `/info` (23L773, AirPlay 960.13.1).
    V26,
    /// No PIN without `X-Apple-Client-Name` (pyatv #2866, PR 2894); `sourceVersion 1005.12.1`
    /// (reported on 27.2); build 24J361 (foxCast's AppleTV11,1 on tvOS 27); an 8931-byte event
    /// frame (§7.6 row 7: claimed, so the fake sends one).
    V27,
}

impl Tvos {
    pub fn os_build(self) -> &'static str {
        match self {
            Tvos::V26 => "23L773",
            Tvos::V27 => "24J361",
        }
    }

    pub fn source_version(self) -> &'static str {
        match self {
            Tvos::V26 => "960.13.1",
            Tvos::V27 => "1005.12.1",
        }
    }

    /// AVPlayer's User-Agent on the Apple TV (`AppleCoreMedia/1.0.0.<build> (Apple TV; …)`).
    pub fn user_agent(self) -> String {
        let os = match self {
            Tvos::V26 => "26_6",
            Tvos::V27 => "27_0",
        };
        format!("AppleCoreMedia/1.0.0.{} (Apple TV; U; CPU OS {os} like Mac OS X; en_us)", self.os_build())
    }
}

/// Which pairing variants the TV accepts (DESIGN §40 §7.2, the ladder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// HKP 3 (and HKP 5) work.
    Hkp3,
    /// HKP 3 gets 470 at M3; only HKP 5 pairs.
    Hkp5Only,
}

/// The TV's Allow Access setting (DESIGN §40 §3.2, §7.6 row 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowAccess {
    /// "Anyone on the Same Network": pairing and verify work.
    SameNetwork,
    /// "Everyone in Your Home": pair-verify answers 200 with an empty body (tvOS 27.2, pyatv
    /// #2885).
    HomeOnly,
    /// Pairing refused: 470 at M3 for every variant, and 470 on pair-verify.
    Refuse470,
}

/// What a `playbackInfo` request gets (DESIGN §40 §7.3 "Position").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackInfo {
    /// 200, then the answer on the event channel: tvOS 27 as an AppleTV14,1 sent it on
    /// 2026-10-13, `{kind: response, messageID, info: {position: <CMTime>, duration: <CMTime>,
    /// rate…}}`; tvOS 26 in LIVI's form (PR 2846), `value: {position: <real>, …}`.
    Answer,
    /// HTTP 500.
    Status500,
    /// 200 and no answer, ever.
    Silent,
}

/// Whether the TV reaches the relay (the 10 s firewall check, escalation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Normal,
    /// Firewall-blocked: `loading` and nothing else.
    Never,
    /// Every fetch starts [`FakeConfig::slow_delay`] late.
    Slow,
}

/// Which receiver the fake is (DESIGN §40 §7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// An Apple TV: `X-Apple-StreamID` exactly (the hyphenated `X-Apple-Stream-ID` gets 200 but the
    /// item never starts, VioletRelay); session headers tolerated.
    AppleTv,
    /// A third-party AirPlay TV (the Hisense, VioletRelay `airplay_modern.py`): no `psi`, the
    /// type-120 SETUP before the type-130 one, `SETPEERSX` answered 501, `X-Apple-ProtocolVersion` and
    /// `X-Apple-Session-ID` refused on `/command`, the stream-id header as
    /// [`FakeConfig::third_party_header`], and no media segments without `HLS-Content-Location`.
    ThirdParty,
}

/// A `/command` stream-id header spelling (the third-party negotiation, VioletRelay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamHeader {
    /// `X-Apple-StreamID: <id>`.
    Joined,
    /// `X-Apple-Stream-ID: <id>`.
    Hyphenated,
    /// Neither.
    Absent,
}

/// A pairing reply the TV gets wrong on purpose (the client must refuse it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    /// M4's server proof has one byte flipped.
    M4Proof,
    /// M6's signature is made with another key than the LTPK it carries.
    M6Signature,
}

/// The knobs (fixed at start). [`FakeConfig::new`] gives the defaults for a tvOS.
#[derive(Debug, Clone)]
pub struct FakeConfig {
    pub tvos: Tvos,
    pub pairing: Pairing,
    pub allow_access: AllowAccess,
    /// M6 carries a 159-byte type-17 TLV inside its encrypted data (seen on 26.6, send-airplay2
    /// PR 8; pyatv's `server_auth.py` has the field commented out in the same place).
    pub m6_tlv17: bool,
    /// The base SETUP's `skipRecord` (`None`: not sent).
    pub skip_record: Option<bool>,
    /// What RECORD answers.
    pub record_status: u16,
    /// `updateInfo` is a 2283-byte request sent as one 1024-byte frame and one 1259-byte frame
    /// (castcli #35 on AppleTV14,1, tvOS 26.6). Wins over [`FakeConfig::big_event_frame`].
    pub split_events: bool,
    /// `updateInfo` is one 8931-byte frame (on by default for [`Tvos::V27`]).
    pub big_event_frame: bool,
    pub playback_info: PlaybackInfo,
    pub fetch: Fetch,
    pub profile: Profile,
    /// An NTP declaration in SETUP → no events at all, and the session closes 2 s later (PR 2899
    /// on 26.5, foxCast on 27: "with NTP the item plays but no events arrive").
    pub strict_ntp: bool,
    /// The first `/feedback` reply is held until the next request arrives, then written before
    /// that request's reply: a stray late CSeq (pyatv #2893).
    pub late_feedback_reply: bool,
    /// How long an event may go unacknowledged before the TV closes the session (the TV: ~25–30 s;
    /// tests use 1 s).
    pub ack_timeout: Duration,
    /// What `/pair-pin-start` answers: 200, 453 (asynchronous PIN display: a success, foxCast) or
    /// 403 (a Mac, castcli).
    pub pin_start_status: u16,
    /// The first M2 is `{State 2, Error, [BackOff]}` instead (Error 3 + BackOff: the TV wants a
    /// break; the BackOff seconds little-endian, foxCast).
    pub pair_error: Option<(u8, Option<u16>)>,
    pub forge: Option<Forge>,
    /// `playbackState` as `{type: playbackState, params: {playbackState: …}}` (robkochman's form)
    /// instead of `{type: playbackState, name: …}`.
    pub params_state: bool,
    /// The one stream-id header a [`Profile::ThirdParty`] fake accepts on `/command`.
    pub third_party_header: StreamHeader,
    /// [`Fetch::Slow`]'s delay before each fetch.
    pub slow_delay: Duration,
    /// The codes the TV shows, in turn (every pin-start and every wrong PIN shows the next).
    pub pins: Vec<&'static str>,
    /// The TV's long-term identity.
    pub accessory: Accessory,
}

impl FakeConfig {
    pub fn new(tvos: Tvos) -> Self {
        Self {
            tvos,
            pairing: Pairing::Hkp3,
            allow_access: AllowAccess::SameNetwork,
            m6_tlv17: false,
            skip_record: None,
            record_status: 200,
            split_events: false,
            big_event_frame: tvos == Tvos::V27,
            playback_info: PlaybackInfo::Answer,
            fetch: Fetch::Normal,
            profile: Profile::AppleTv,
            strict_ntp: true,
            late_feedback_reply: false,
            ack_timeout: Duration::from_secs(1),
            pin_start_status: 200,
            pair_error: None,
            forge: None,
            params_state: false,
            third_party_header: StreamHeader::Hyphenated,
            slow_delay: Duration::from_secs(2),
            pins: vec!["1111", "2222", "3333", "4444"],
            accessory: Accessory { id: "8C4E7A1D-35B2-4F60-9E0A-7D2B6C1F0A55".into(), seed: [0x5a; 32] },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------------------------

/// A decoded request body.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Empty,
    /// A pairing message.
    Tlv(Vec<(u8, Vec<u8>)>),
    Plist(Value),
    /// `/command`: the inner plist of `{params: {data: <bplist>}}`.
    Command(Value),
    Raw(Vec<u8>),
}

/// One request on a control connection, as the fake saw and answered it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The control connection (1, 2, … in accept order).
    pub conn: usize,
    pub method: String,
    pub uri: String,
    /// `RTSP/1.0` or `HTTP/1.1`.
    pub protocol: String,
    pub headers: Vec<(String, String)>,
    /// It arrived HAP-encrypted.
    pub encrypted: bool,
    pub body: Body,
    /// What the fake answered (0: nothing — it closed, or the reply is still held).
    pub status: u16,
}

impl Request {
    /// A header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn plist(&self) -> Option<&Value> {
        match &self.body {
            Body::Plist(v) | Body::Command(v) => Some(v),
            _ => None,
        }
    }

    /// The `/command`'s inner plist.
    pub fn command(&self) -> Option<&Value> {
        match &self.body {
            Body::Command(v) => Some(v),
            _ => None,
        }
    }

    pub fn tlv(&self, t: u8) -> Option<&[u8]> {
        match &self.body {
            Body::Tlv(items) => items.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice()),
            _ => None,
        }
    }

    /// A one-line name for sequence assertions: `POST /pair-verify`, `SETUP`, `SETUP 130`,
    /// `RECORD`, `GET /info`, `POST /feedback`, `/command setRate 1`,
    /// `/command setProperty actionAtItemEnd=1`, `/command insertPlayQueueItem`, `TEARDOWN`.
    pub fn step(&self) -> String {
        if let Some(cmd) = self.command() {
            return format!("/command {}", command_label(cmd));
        }
        match self.method.as_str() {
            "SETUP" => match self.plist().and_then(|p| p.get("streams")) {
                Some(Value::Array(s)) => {
                    format!("SETUP {}", s.first().and_then(|s| s.get("type")).map(show).unwrap_or_default())
                }
                _ => "SETUP".into(),
            },
            "RECORD" | "TEARDOWN" | "SETPEERSX" => self.method.clone(),
            _ => format!("{} {}", self.method, self.uri),
        }
    }
}

/// `insertPlayQueueItem`, `setRate 1`, `setProperty actionAtItemEnd=0`, `playbackInfo`, …
pub fn command_label(cmd: &Value) -> String {
    let ty = text(cmd, "type").unwrap_or("?");
    match ty {
        "setRate" => format!("setRate {}", cmd.get("rate").map(show).unwrap_or_default()),
        "setProperty" => format!("setProperty {}={}", text(cmd, "property").unwrap_or("?"), cmd.get("value").map(show).unwrap_or_default()),
        _ => ty.to_owned(),
    }
}

/// A plist scalar as text (`1` for both `Int(1)` and `Real(1.0)`).
pub fn show(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) if r.fract() == 0.0 => format!("{r:.0}"),
        Value::Real(r) => r.to_string(),
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// `dict[key]` as a string.
pub fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v.get(key) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// `dict[key]` as a number.
pub fn number(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Int(i)) => Some(*i as f64),
        Some(Value::Real(r)) => Some(*r),
        _ => None,
    }
}

/// One media request the TV made to the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The queue item it was for.
    pub item: String,
    /// The URL path.
    pub path: String,
    pub range: Option<String>,
    pub user_agent: String,
    /// 0: the request failed.
    pub status: u16,
}

/// Which connection closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Control(usize),
    Events,
}

/// Who closed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// The sender (EOF or a reset).
    Sender,
    /// The fake (TEARDOWN, a quirk, an ack timeout, the remote's Menu, shutdown).
    Receiver,
}

/// Something the sender did that a real TV punishes (the fake closes, or logs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A request arrived before the previous one was answered (PR 2899: an RTSP `/feedback` racing
    /// an HTTP `/command` drops the connection). Closed.
    Pipelined { conn: usize, during: String },
    /// A frame that didn't open (bad tag, or plaintext after pair-verify). Closed.
    BadFrame(Channel),
    /// A request the fake couldn't parse. Closed.
    Malformed(Channel),
    /// RECORD before the event channel connected (foxCast: RECORD → 500).
    RecordBeforeEvents,
    /// `/feedback` before RECORD (PR 2899: the TV drops the connection). Closed.
    FeedbackBeforeRecord,
    /// `/feedback` before the first `/command` (allowed by the TV: VioletRelay works so; Tuner
    /// sends feedback only after the commands).
    FeedbackBeforeCommands,
    /// `GET /info` before pair-verify on the session's connection (PR 2899: dropped). Closed.
    InfoBeforeVerify,
    /// `insertPlayQueueItem` with a `playerLoggingID` over 6 characters (PR 2899: the TV closes
    /// silently). Closed.
    LongPlayerLoggingId(String),
    /// An event-channel answer that isn't `RTSP/1.0 200 OK` with a CSeq.
    BadAck(String),
    /// A third-party TV's `SETPEERSX` keyed `ClockPorts` by something other than its TXT `gid`
    /// (VioletRelay's flow, the only one proven on the Hisense).
    ClockPortsKey(String),
    /// The fake's own consistency check failed (its shadow SRP disagrees with the server half).
    Fake(String),
}

/// One thing that happened, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Connected {
        conn: usize,
    },
    Request(Request),
    /// The TV showed a code (pin-start, or a new one after a wrong PIN).
    PinShown(String),
    /// The sender connected to `eventPort`.
    EventsConnected,
    /// An event-channel request the TV sent: its CSeq, what it was, and the plaintext size of each
    /// HAP frame it went out in.
    EventSent {
        cseq: u32,
        what: String,
        frames: Vec<usize>,
    },
    EventAcked {
        cseq: u32,
    },
    /// No ack within `ack_timeout`: the TV closed the session.
    AckTimeout {
        cseq: u32,
    },
    /// The fake sent a frame with a bad tag.
    BadFrameSent,
    Fetch(Fetched),
    Closed {
        channel: Channel,
        by: By,
    },
    Violation(Violation),
}

/// What the TV decrypted from an M5 (the controller's identity and, for HKP 5, its ACL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M5Seen {
    /// `X-Apple-HKP` on the request.
    pub hkp: Option<String>,
    pub identifier: Vec<u8>,
    pub ltpk: Vec<u8>,
    /// TLV 0x12.
    pub acl: Option<Vec<u8>>,
}

/// The receiver's state (read it with [`FakeAirPlay::with`] / [`FakeAirPlay::until`]).
pub struct State {
    pub log: Vec<Entry>,
    /// Controllers the TV trusts (pair-setup's M5, or [`FakeAirPlay::pre_pair`]).
    pub controllers: Vec<PeerCredentials>,
    /// The code on screen (`None`: none shown, e.g. tvOS 27 without a client name).
    pub shown_pin: Option<String>,
    pub m5: Vec<M5Seen>,
    accessory: Accessory,
    pins_shown: usize,
    verifies: u8,
    next_conn: usize,
    pair_error_used: bool,
    session: Option<Session>,
    queue: Vec<Item>,
    current: Option<String>,
    rate: f64,
    position: (f64, Option<f64>),
    command_delay: Duration,
    fail_commands: u32,
}

impl State {
    pub fn requests(&self) -> impl Iterator<Item = &Request> {
        self.log.iter().filter_map(|e| match e {
            Entry::Request(r) => Some(r),
            _ => None,
        })
    }

    pub fn fetches(&self) -> impl Iterator<Item = &Fetched> {
        self.log.iter().filter_map(|e| match e {
            Entry::Fetch(f) => Some(f),
            _ => None,
        })
    }

    pub fn violations(&self) -> Vec<Violation> {
        self.log
            .iter()
            .filter_map(|e| match e {
                Entry::Violation(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }

    /// The control connection closed (by either side).
    pub fn closed(&self, channel: Channel) -> bool {
        self.log.iter().any(|e| matches!(e, Entry::Closed { channel: c, .. } if *c == channel))
    }

    /// The uuid of the item playing.
    pub fn current_item(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------
// The handle (use cases)
// ---------------------------------------------------------------------------------------------

/// A running fake TV. Dropping it shuts it down.
pub struct FakeAirPlay {
    inner: Arc<Inner>,
    addr: SocketAddr,
    shutdown: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeAirPlay {
    pub fn start(cfg: FakeConfig) -> Self {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        std_listener.set_nonblocking(true).expect("nonblocking listener");
        let addr = std_listener.local_addr().expect("local addr");
        let shutdown = CancellationToken::new();
        let state = State {
            log: Vec::new(),
            controllers: Vec::new(),
            shown_pin: None,
            m5: Vec::new(),
            accessory: cfg.accessory.clone(),
            pins_shown: 0,
            verifies: 0,
            next_conn: 1,
            pair_error_used: false,
            session: None,
            queue: Vec::new(),
            current: None,
            rate: 0.0,
            position: (0.0, None),
            command_delay: Duration::ZERO,
            fail_commands: 0,
        };
        let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().expect("http client");
        let inner = Arc::new(Inner { cfg, state: Mutex::new(state), http, shutdown: shutdown.clone() });
        let (run, stop) = (inner.clone(), shutdown.clone());
        let thread = std::thread::Builder::new()
            .name("fake-airplay".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("fake runtime");
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    loop {
                        tokio::select! {
                            accepted = listener.accept() => {
                                let Ok((stream, _)) = accepted else { continue };
                                let id = {
                                    let mut st = run.lock();
                                    let id = st.next_conn;
                                    st.next_conn += 1;
                                    st.log.push(Entry::Connected { conn: id });
                                    id
                                };
                                tokio::spawn(serve_control(run.clone(), id, stream));
                            }
                            _ = stop.cancelled() => break,
                        }
                    }
                });
            })
            .expect("spawn fake AirPlay");
        Self { inner, addr, shutdown, thread: Some(thread) }
    }

    /// Where the TV listens (its `:7000`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn config(&self) -> &FakeConfig {
        &self.inner.cfg
    }

    /// Read the state (`f` runs under its lock: it must not call back into the fake).
    pub fn with<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.inner.lock())
    }

    pub fn log(&self) -> Vec<Entry> {
        self.with(|s| s.log.clone())
    }

    pub fn requests(&self) -> Vec<Request> {
        self.with(|s| s.requests().cloned().collect())
    }

    /// The inner plists of every `/command`, in order.
    pub fn commands(&self) -> Vec<Value> {
        self.with(|s| s.requests().filter_map(|r| r.command().cloned()).collect())
    }

    pub fn violations(&self) -> Vec<Violation> {
        self.with(State::violations)
    }

    /// The code on the TV's screen.
    pub fn shown_pin(&self) -> Option<String> {
        self.with(|s| s.shown_pin.clone())
    }

    /// The TV's pairing id and long-term public key as a paired controller stores them.
    pub fn accessory(&self) -> (String, [u8; 32]) {
        self.with(|s| (s.accessory.id.clone(), ed25519_public(&s.accessory.seed)))
    }

    pub fn controllers(&self) -> Vec<PeerCredentials> {
        self.with(|s| s.controllers.clone())
    }

    /// Pair `identity` without the PIN dance (the session tests): the TV trusts its key, and the
    /// returned credentials are what a pairing with `variant` would have stored.
    pub fn pre_pair(&self, identity: &Identity, variant: PairVariant) -> Credentials {
        let mut st = self.inner.lock();
        st.controllers.push(PeerCredentials { peer_id: identity.pairing_id.clone(), peer_ltpk: ed25519_public(identity.seed.expose()), variant });
        let peer = PeerCredentials { peer_id: st.accessory.id.clone(), peer_ltpk: ed25519_public(&st.accessory.seed), variant };
        Credentials { identity: identity.clone(), peer }
    }

    /// The TV was reset (or re-paired elsewhere): a new long-term key, and it forgets every
    /// controller. Pair-verify with old credentials then fails the signature check.
    pub fn reset_accessory(&self) {
        let mut st = self.inner.lock();
        st.accessory.seed = [0xc3; 32];
        st.controllers.clear();
    }

    /// The position `playbackInfo` reports (relative to the item's playlist).
    pub fn set_position(&self, position: f64, duration: Option<f64>) {
        self.inner.lock().position = (position, duration);
    }

    /// Hold every `/command` reply this long (a request that arrives meanwhile is pipelining).
    pub fn set_command_delay(&self, delay: Duration) {
        self.inner.lock().command_delay = delay;
    }

    /// The next `n` `/command`s answer 500 (pyatv's injectable failure counters).
    pub fn fail_next_commands(&self, n: u32) {
        self.inner.lock().fail_commands = n;
    }

    /// The remote's Menu: the TV closes the session (control connection and event channel) and
    /// sends no `stopped` (foxCast `waitQueue`).
    pub fn stop_from_remote(&self) {
        if let Some(s) = &self.inner.lock().session {
            s.close.cancel();
        }
    }

    /// Write one frame on the event channel whose tag doesn't verify.
    pub fn send_bad_frame(&self) {
        self.inner.push(EventOut::BadFrame);
    }

    /// Push `{type: playbackState, …}` (`loading`, `playing`, `paused`, `stopped`).
    pub fn push_state(&self, name: &str) {
        self.inner.state_event(name);
    }

    /// Push `{type: notification, name}`.
    pub fn push_notification(&self, name: &str) {
        self.inner.notification(name);
    }

    /// The TV's playhead jumped (its own remote scrubbed): `timeJumped` for the current item,
    /// with the position as tvOS 27 sends it (a CMTime), or without one.
    pub fn jump(&self, position: Option<f64>) {
        let uuid = self.inner.lock().current.clone().unwrap_or_default();
        let mut d = vec![("type", string("notification")), ("name", string("timeJumped")), ("item", dict([("uuid", string(&uuid))]))];
        if let Some(p) = position {
            d.push(("position", cmtime((p * 90_000.0).round() as i64, 90_000, 1)));
        }
        self.inner.push_wrapped("notification timeJumped".into(), Value::Dict(d.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()));
    }

    /// Wait (bounded, polling) until `f` gives a value; fails the test with the log otherwise.
    /// `f` runs under the state's lock: it must not call back into the fake.
    pub async fn until<T>(&self, what: &str, mut f: impl FnMut(&State) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(v) = f(&self.inner.lock()) {
                return v;
            }
            assert!(Instant::now() < deadline, "fake AirPlay: no {what} within {WAIT:?}\n{}", self.dump());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The log, one line per entry (failure messages).
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for e in self.with(|s| s.log.clone()) {
            let _ = match e {
                Entry::Request(r) => writeln!(out, "  #{} {} → {}", r.conn, r.step(), r.status),
                other => writeln!(out, "  {other:?}"),
            };
        }
        out
    }
}

impl Drop for FakeAirPlay {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

struct Inner {
    cfg: FakeConfig,
    state: Mutex<State>,
    http: reqwest::Client,
    shutdown: CancellationToken,
}

/// The sender's session (one at a time: a new SETUP takes over, as on the TV).
struct Session {
    conn: usize,
    events: mpsc::UnboundedSender<EventOut>,
    /// Closes the control connection and the event channel.
    close: CancellationToken,
    events_connected: bool,
    recorded: bool,
    update_sent: bool,
    /// Declared NTP and `strict_ntp`: no events.
    mute: bool,
    commands: bool,
    has_120: bool,
    rcs: bool,
    /// An Apple TV got `X-Apple-Stream-ID`: 200, but the item never starts (VioletRelay).
    hyphenated: bool,
    late_feedback_done: bool,
}

struct Item {
    uuid: String,
    url: String,
    hls: bool,
    started: bool,
    cancel: CancellationToken,
}

enum EventOut {
    Push { what: String, body: EventBody },
    BadFrame,
    Close,
}

enum EventBody {
    Plain(Vec<u8>),
    UpdateInfo,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, e: Entry) {
        self.lock().log.push(e);
    }

    fn push(&self, out: EventOut) {
        let st = self.lock();
        if let Some(s) = st.session.as_ref().filter(|s| !s.mute) {
            let _ = s.events.send(out);
        }
    }

    /// `{params: {data: <bplist of inner>}}` (PR 2899, robkochman, foxCast docs/05).
    fn push_wrapped(&self, what: String, inner: Value) {
        let body = bplist::encode(&dict([("params", dict([("data", Value::Data(bplist::encode(&inner)))]))]));
        self.push(EventOut::Push { what, body: EventBody::Plain(body) });
    }

    fn state_event(&self, name: &str) {
        let inner = if self.cfg.params_state {
            dict([("type", string("playbackState")), ("params", dict([("playbackState", string(name))]))])
        } else {
            dict([("type", string("playbackState")), ("name", string(name))])
        };
        self.push_wrapped(format!("playbackState {name}"), inner);
    }

    fn notification(&self, name: &str) {
        self.push_wrapped(format!("notification {name}"), dict([("type", string("notification")), ("name", string(name))]));
    }

    fn rate(&self) -> f64 {
        self.lock().rate
    }

    /// `/info` (and the `updateInfo` value), from the scrubbed LAN captures (§5.3): ids replaced,
    /// no addresses.
    fn info(&self, txt_pad: usize) -> Value {
        let st = self.lock();
        let pk = Value::Data(ed25519_public(&st.accessory.seed).to_vec());
        match self.cfg.profile {
            Profile::AppleTv => {
                let mut d = vec![
                    ("psi", string(PSI)),
                    ("vv", Value::Int(1)),
                    (
                        "playbackCapabilities",
                        dict([
                            ("supportsInterstitials", Value::Bool(true)),
                            ("supportsUIForAudioOnlyContent", Value::Bool(true)),
                            ("supportsAirPlayVideoWithSharePlay", Value::Bool(true)),
                            ("supportsOfflineHLS", Value::Bool(false)),
                        ]),
                    ),
                    ("canRecordScreenStream", Value::Bool(false)),
                    ("statusFlags", Value::Int(0x11_8644)),
                    ("name", string("Living Room")),
                    ("protocolVersion", string("1.1")),
                    ("keepAliveSendStatsAsBody", Value::Bool(true)),
                    ("volumeControlType", Value::Int(4)),
                    ("deviceID", string(DEVICE_ID)),
                    ("pi", string("2b3c4d5e-0000-4000-8000-00000000a1a1")),
                    ("screenDemoMode", Value::Bool(false)),
                    ("initialVolume", Value::Real(0.0)),
                    ("featuresEx", string("1d9/St5/Fzw4oY58")),
                    ("sourceVersion", string(self.cfg.tvos.source_version())),
                    ("hasUDPMirroringSupport", Value::Bool(true)),
                    ("model", string("AppleTV14,1")),
                    ("pk", pk),
                    ("macAddress", string(DEVICE_ID)),
                    ("osBuildVersion", string(self.cfg.tvos.os_build())),
                    ("receiverHDRCapability", string("4k60")),
                    ("features", Value::Int(4_330_070_159_449_382_869)),
                ];
                if txt_pad > 0 {
                    d.push(("txtAirPlay", Value::Data(vec![b'x'; txt_pad])));
                }
                Value::Dict(d.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
            }
            Profile::ThirdParty => {
                let mut d = vec![
                    ("sdk", string("AirPlay;3.6.0.126")),
                    ("pi", string(DEVICE_ID)),
                    ("manufacturer", string("Hisense")),
                    ("model", string("85E75LE")),
                    ("operatingSystem", string("Linux")),
                    ("playbackCapabilities", dict([("supportsUIForAudioOnlyContent", Value::Bool(false))])),
                    ("keepAliveSendStatsAsBody", Value::Bool(true)),
                    ("featuresEx", string("0Ip/AEbPiwNABA")),
                    ("statusFlags", Value::Int(580)),
                    ("deviceID", string(DEVICE_ID)),
                    ("PTPInfo", string("AirPlay;3.6.0.126")),
                    ("protocolVersion", string("1.1")),
                    ("sourceVersion", string("377.40.00")),
                    ("features", Value::Int(255_525_703_439_583_952)),
                    ("volumeControlType", Value::Int(2)),
                    ("name", string("Living Room TV")),
                ];
                if txt_pad > 0 {
                    d.push(("txtAirPlay", Value::Data(vec![b'x'; txt_pad])));
                }
                Value::Dict(d.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
            }
        }
    }

    /// The session belongs to control connection `conn`.
    fn session_mut(st: &mut State, conn: usize) -> Option<&mut Session> {
        st.session.as_mut().filter(|s| s.conn == conn)
    }

    /// Send `updateInfo` once the event channel is up and RECORD came (or `skipRecord`).
    fn maybe_update_info(&self, st: &mut State) {
        let skip = self.cfg.skip_record == Some(true);
        if let Some(s) = st.session.as_mut()
            && s.events_connected
            && (s.recorded || skip)
            && !s.update_sent
        {
            s.update_sent = true;
            if !s.mute {
                let _ = s.events.send(EventOut::Push { what: "updateInfo".into(), body: EventBody::UpdateInfo });
            }
        }
    }

    /// End the session (TEARDOWN, the control connection closing): the event channel closes and
    /// the queue empties.
    fn end_session(&self, st: &mut State, conn: usize) {
        if st.session.as_ref().is_some_and(|s| s.conn == conn) {
            let s = st.session.take().expect("checked");
            let _ = s.events.send(EventOut::Close);
            for item in st.queue.drain(..) {
                item.cancel.cancel();
            }
            st.current = None;
            st.rate = 0.0;
        }
    }
}

// ---- control connection -----------------------------------------------------------------------

/// What answering a request does to the connection afterwards.
enum After {
    Nothing,
    /// Switch to HAP framing with these control keys (right after pair-verify's M4).
    Encrypt(Box<FakeCipher>),
    /// Close without answering (a quirk).
    CloseSilently,
    /// Answer, then hold back: the reply is written before the next request's (late feedback).
    Hold,
}

struct Reply {
    status: u16,
    content_type: Option<&'static str>,
    body: Vec<u8>,
    after: After,
    delay: Duration,
    /// An event pushed once the reply is written (a `playbackInfo` answer follows the HTTP 200:
    /// PR 2846's sender only listens for it after `/command` returned).
    then: Option<(String, Value)>,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self { status, content_type: None, body: Vec::new(), after: After::Nothing, delay: Duration::ZERO, then: None }
    }

    fn tlv(body: Vec<u8>) -> Self {
        Self { content_type: Some("application/octet-stream"), body, ..Self::status(200) }
    }

    fn plist(v: &Value) -> Self {
        Self { content_type: Some("application/x-apple-binary-plist"), body: bplist::encode(v), ..Self::status(200) }
    }

    fn close() -> Self {
        Self { after: After::CloseSilently, ..Self::status(0) }
    }
}

/// Pair-setup in progress on one connection: CAST-1's strict server half plus a shadow SRP
/// server with the same salt and `b` (same `B`, same `K`) so the fake can read M5 and build M6.
struct SetupArm {
    server: PairSetupServer,
    shadow: SrpServer,
    key: Option<Vec<u8>>,
}

struct Conn {
    id: usize,
    close: CancellationToken,
    /// pin-start showed a code on this connection (castcli: pair-setup is authorised only on
    /// the socket that called pin-start).
    setup: Option<SetupArm>,
    verify: Option<PairVerifyServer>,
    verified: bool,
    /// The pair-verify shared secret (the event channel's keys derive from it).
    shared: Option<[u8; 32]>,
    info_before_verify: bool,
}

async fn serve_control(inner: Arc<Inner>, id: usize, stream: TcpStream) {
    let (mut rd, mut wr) = stream.into_split();
    let mut conn =
        Conn { id, close: inner.shutdown.child_token(), setup: None, verify: None, verified: false, shared: None, info_before_verify: false };
    let mut cipher: Option<FakeCipher> = None;
    let mut raw = Vec::new();
    let mut plain = Vec::new();
    let mut held: Option<Vec<u8>> = None;
    let mut buf = vec![0u8; 16 * 1024];
    let by = 'conn: loop {
        match &mut cipher {
            Some(c) => loop {
                match c.open(&mut raw) {
                    Ok(Some(p)) => plain.extend_from_slice(&p),
                    Ok(None) => break,
                    Err(()) => {
                        inner.log(Entry::Violation(Violation::BadFrame(Channel::Control(id))));
                        break 'conn By::Receiver;
                    }
                }
            },
            None => plain.append(&mut raw),
        }
        match parse_message(&mut plain) {
            Err(()) => {
                inner.log(Entry::Violation(Violation::Malformed(Channel::Control(id))));
                break By::Receiver;
            }
            Ok(Some(msg)) => {
                let req = msg;
                // One request in flight: anything already behind it was sent before its answer.
                if !plain.is_empty() || !raw.is_empty() {
                    let during = format!("{} {}", req.method, req.uri);
                    inner.log(Entry::Violation(Violation::Pipelined { conn: id, during }));
                    break By::Receiver;
                }
                if let Some(late) = held.take()
                    && write(&mut wr, &mut cipher, &late).await.is_err()
                {
                    break By::Sender;
                }
                if req.method == "RECORD" {
                    wait_for_events(&inner, id).await;
                }
                let encrypted = cipher.is_some();
                let index = {
                    let mut st = inner.lock();
                    st.log.push(Entry::Request(Request {
                        conn: id,
                        method: req.method.clone(),
                        uri: req.uri.clone(),
                        protocol: req.protocol.clone(),
                        headers: req.headers.clone(),
                        encrypted,
                        body: decode_body(&req),
                        status: 0,
                    }));
                    st.log.len() - 1
                };
                let reply = handle(&inner, &mut conn, &req, encrypted);
                if let Some(Entry::Request(r)) = inner.lock().log.get_mut(index) {
                    r.status = reply.status;
                }
                if !reply.delay.is_zero() {
                    tokio::select! {
                        _ = tokio::time::sleep(reply.delay) => {}
                        n = rd.read(&mut buf) => {
                            if let Ok(n @ 1..) = n {
                                raw.extend_from_slice(&buf[..n]);
                                let during = format!("{} {}", req.method, req.uri);
                                inner.log(Entry::Violation(Violation::Pipelined { conn: id, during }));
                                break By::Receiver;
                            }
                            break By::Sender;
                        }
                    }
                }
                let bytes = response(&req, &reply, inner.cfg.tvos.source_version());
                let then = reply.then;
                match reply.after {
                    After::CloseSilently => break By::Receiver,
                    After::Hold => held = Some(bytes),
                    After::Nothing => {
                        if write(&mut wr, &mut cipher, &bytes).await.is_err() {
                            break By::Sender;
                        }
                    }
                    After::Encrypt(c) => {
                        if write(&mut wr, &mut cipher, &bytes).await.is_err() {
                            break By::Sender;
                        }
                        cipher = Some(*c);
                    }
                }
                if let Some((what, event)) = then {
                    inner.push_wrapped(what, event);
                }
                continue;
            }
            Ok(None) => {}
        }
        tokio::select! {
            n = rd.read(&mut buf) => match n {
                Ok(n @ 1..) => raw.extend_from_slice(&buf[..n]),
                _ => break By::Sender,
            },
            _ = conn.close.cancelled() => break By::Receiver,
        }
    };
    let _ = wr.shutdown().await;
    let mut st = inner.lock();
    st.log.push(Entry::Closed { channel: Channel::Control(id), by });
    inner.end_session(&mut st, id);
}

/// RECORD may reach the fake before its event listener's `accept()` ran (the kernel accepted the
/// sender's connection already): give that task a moment before judging the order.
async fn wait_for_events(inner: &Inner, conn: usize) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if inner.lock().session.as_ref().is_none_or(|s| s.conn != conn || s.events_connected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn write(wr: &mut OwnedWriteHalf, cipher: &mut Option<FakeCipher>, bytes: &[u8]) -> std::io::Result<()> {
    match cipher {
        Some(c) => {
            let mut out = Vec::new();
            for chunk in bytes.chunks(1024) {
                out.extend_from_slice(&c.seal(chunk));
            }
            wr.write_all(&out).await
        }
        None => wr.write_all(bytes).await,
    }
}

fn handle(inner: &Arc<Inner>, conn: &mut Conn, req: &Message, encrypted: bool) -> Reply {
    let path = req.uri.split('?').next().unwrap_or_default();
    match (req.method.as_str(), path) {
        ("POST", "/pair-pin-start") if !encrypted => pin_start(inner, conn, req),
        ("POST", "/pair-setup") if !encrypted => pair_setup(inner, conn, req),
        ("POST", "/pair-verify") if !encrypted => pair_verify(inner, conn, req),
        ("GET", "/info") => {
            if !conn.verified {
                conn.info_before_verify = true;
            }
            Reply::plist(&inner.info(0))
        }
        // §7.6 row 10: dead on 26.x and 27.0.
        ("GET", "/playback-info") | ("POST", "/play") => Reply::status(500),
        _ if !encrypted => Reply::status(403),
        ("SETUP", _) => setup(inner, conn, req),
        ("RECORD", _) => record(inner, conn),
        ("POST", "/feedback") => feedback(inner, conn),
        ("POST", "/command") => command(inner, conn, req),
        ("SETPEERSX", _) => match inner.cfg.profile {
            Profile::AppleTv => Reply::status(200),
            Profile::ThirdParty => {
                let key = match bplist::decode(&req.body) {
                    Ok(Value::Array(peers)) => peers.first().and_then(|p| match p.get("ClockPorts") {
                        Some(Value::Dict(d)) => d.first().map(|(k, _)| k.clone()),
                        _ => None,
                    }),
                    _ => None,
                };
                let key = key.unwrap_or_default();
                if key != hisense_txt("gid") {
                    inner.log(Entry::Violation(Violation::ClockPortsKey(key)));
                }
                Reply::status(501)
            }
        },
        ("TEARDOWN", _) => {
            inner.end_session(&mut inner.lock(), conn.id);
            Reply::status(200)
        }
        _ => Reply::status(404),
    }
}

fn pin_start(inner: &Arc<Inner>, conn: &mut Conn, req: &Message) -> Reply {
    let status = inner.cfg.pin_start_status;
    if status == 403 {
        return Reply::status(403);
    }
    let named = req.header("X-Apple-Client-Name").is_some_and(|n| !n.trim().is_empty());
    if inner.cfg.tvos == Tvos::V27 && !named {
        // "passcodeString is empty or clientName is nil. Bailing." (pyatv #2866): no code.
        conn.setup = None;
        inner.lock().shown_pin = None;
        return Reply::status(200);
    }
    arm(inner, conn);
    Reply::status(status)
}

/// Show the next code and get a pair-setup ready for it on this connection.
fn arm(inner: &Arc<Inner>, conn: &mut Conn) {
    let mut st = inner.lock();
    let k = st.pins_shown;
    st.pins_shown += 1;
    let pin = inner.cfg.pins[k % inner.cfg.pins.len()];
    let salt = [0x51u8.wrapping_add(k as u8); 16];
    let b = [0x2bu8.wrapping_add(k as u8); 32];
    let server = PairSetupServer::new(&st.accessory, pin, &salt, &b);
    let shadow = SrpServer::new(&salt, pin, &b);
    st.shown_pin = Some(pin.to_owned());
    st.log.push(Entry::PinShown(pin.to_owned()));
    conn.setup = Some(SetupArm { server, shadow, key: None });
}

fn pair_setup(inner: &Arc<Inner>, conn: &mut Conn, req: &Message) -> Reply {
    let Ok(tlv) = tlv8::decode(&req.body) else { return Reply::status(400) };
    let state_no = tlv.get(tag::STATE).and_then(|v| v.first().copied());
    let Some(arm_now) = conn.setup.as_mut() else {
        // No code shown on this connection.
        return Reply::status(470);
    };
    let hkp = req.header("X-Apple-HKP").map(str::to_owned);
    match state_no {
        Some(1) => {
            {
                let mut st = inner.lock();
                if let Some((error, backoff)) = inner.cfg.pair_error
                    && !st.pair_error_used
                {
                    st.pair_error_used = true;
                    let mut items: Vec<(u8, Vec<u8>)> = vec![(tag::STATE, vec![2]), (tag::ERROR, vec![error])];
                    if let Some(secs) = backoff {
                        items.push((tag::BACK_OFF, secs.to_le_bytes().to_vec()));
                    }
                    return Reply::tlv(encode_tlv(&items));
                }
            }
            let m2 = arm_now.server.handle(&req.body);
            let b = tlv8::decode(&m2).ok().and_then(|t| t.get(tag::PUBLIC_KEY).map(<[u8]>::to_vec));
            if b.is_some_and(|b| b != arm_now.shadow.public_b()) {
                inner.log(Entry::Violation(Violation::Fake("shadow SRP B differs from PairSetupServer's".into())));
            }
            Reply::tlv(m2)
        }
        Some(3) => {
            let refuse = match inner.cfg.allow_access {
                AllowAccess::Refuse470 => true,
                _ => inner.cfg.pairing == Pairing::Hkp5Only && hkp.as_deref() != Some("5"),
            };
            if refuse {
                return Reply::status(470);
            }
            if let (Some(a), Some(proof)) = (tlv.get(tag::PUBLIC_KEY), tlv.get(tag::PROOF)) {
                arm_now.key = arm_now.shadow.verify(a, proof).ok().map(|(_, k)| k.expose().clone());
            }
            let mut m4 = arm_now.server.handle(&req.body);
            let failed = tlv8::decode(&m4).map(|t| t.get(tag::ERROR).is_some()).unwrap_or(true);
            if failed {
                // A wrong code: the TV shows a new one.
                arm(inner, conn);
                return Reply::tlv(m4);
            }
            if inner.cfg.forge == Some(Forge::M4Proof) {
                m4 = map_tlv(&m4, |t, v| {
                    if t == tag::PROOF {
                        v[0] ^= 0x01;
                    }
                });
            }
            Reply::tlv(m4)
        }
        Some(5) => {
            let key = arm_now.key.clone();
            if let (Some(k), Some(sealed)) = (&key, tlv.get(tag::ENCRYPTED_DATA)) {
                let ek = hkdf32(k, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
                if let Some(plain) = open_once(&ek, b"PS-Msg05", sealed)
                    && let Ok(inner_tlv) = tlv8::decode(&plain)
                {
                    inner.lock().m5.push(M5Seen {
                        hkp: hkp.clone(),
                        identifier: inner_tlv.get(tag::IDENTIFIER).unwrap_or_default().to_vec(),
                        ltpk: inner_tlv.get(tag::PUBLIC_KEY).unwrap_or_default().to_vec(),
                        acl: inner_tlv.get(tag::ACL).map(<[u8]>::to_vec),
                    });
                }
            }
            let mut m6 = arm_now.server.handle(&req.body);
            let paired = arm_now.server.paired();
            let mut st = inner.lock();
            if let Some(p) = paired {
                st.controllers.retain(|c| c.peer_id != p.peer_id);
                st.controllers.push(p);
                let forge = inner.cfg.forge == Some(Forge::M6Signature);
                if let Some(k) = key.as_deref()
                    && (forge || inner.cfg.m6_tlv17)
                {
                    m6 = build_m6(&st.accessory, k, forge, inner.cfg.m6_tlv17);
                }
            }
            drop(st);
            conn.setup = None;
            Reply::tlv(m6)
        }
        _ => Reply::status(400),
    }
}

/// M6 built by the fake (pyatv `server_auth.py` `_m5_setup`'s layout): `{State 6,
/// EncryptedData{Identifier, PublicKey, Signature[, 17]}}`, the signature over
/// `HKDF(K, "Pair-Setup-Accessory-Sign-…") ‖ id ‖ LTPK` — made with a stranger's key when
/// `forge`.
fn build_m6(acc: &Accessory, k: &[u8], forge: bool, tlv17: bool) -> Vec<u8> {
    let ek = hkdf32(k, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
    let x = hkdf32(k, b"Pair-Setup-Accessory-Sign-Salt", b"Pair-Setup-Accessory-Sign-Info");
    let key = Ed25519KeyPair::from_seed_unchecked(&acc.seed).expect("accessory seed");
    let ltpk = key.public_key().as_ref().to_vec();
    let mut info = x.to_vec();
    info.extend_from_slice(acc.id.as_bytes());
    info.extend_from_slice(&ltpk);
    let signature = if forge {
        Ed25519KeyPair::from_seed_unchecked(&[0x66; 32]).expect("stranger seed").sign(&info).as_ref().to_vec()
    } else {
        key.sign(&info).as_ref().to_vec()
    };
    let mut items = vec![(tag::IDENTIFIER, acc.id.as_bytes().to_vec()), (tag::PUBLIC_KEY, ltpk), (tag::SIGNATURE, signature)];
    if tlv17 {
        items.push((17, (0..159u8).collect()));
    }
    let sealed = seal_once(&ek, b"PS-Msg06", &encode_tlv(&items));
    encode_tlv(&[(tag::STATE, vec![6]), (tag::ENCRYPTED_DATA, sealed)])
}

fn pair_verify(inner: &Arc<Inner>, conn: &mut Conn, req: &Message) -> Reply {
    match inner.cfg.allow_access {
        AllowAccess::HomeOnly => return Reply::status(200),
        AllowAccess::Refuse470 => return Reply::status(470),
        AllowAccess::SameNetwork => {}
    }
    if conn.info_before_verify {
        inner.log(Entry::Violation(Violation::InfoBeforeVerify));
        return Reply::close();
    }
    let Ok(tlv) = tlv8::decode(&req.body) else { return Reply::status(400) };
    match tlv.get(tag::STATE).and_then(|v| v.first().copied()) {
        Some(1) => {
            let mut st = inner.lock();
            st.verifies = st.verifies.wrapping_add(1);
            let ephemeral = [0x40u8.wrapping_add(st.verifies); 32];
            // The controller it trusts (a reset TV trusts none: M3 then fails strictly).
            let controller = st.controllers.last().cloned().unwrap_or(PeerCredentials {
                peer_id: "unknown-controller".into(),
                peer_ltpk: [0; 32],
                variant: PairVariant::Hkp3,
            });
            let mut server = PairVerifyServer::new(&st.accessory, &controller, &ephemeral);
            drop(st);
            let m2 = server.handle(&req.body);
            conn.verify = Some(server);
            Reply::tlv(m2)
        }
        Some(3) => {
            let Some(server) = conn.verify.as_mut() else { return Reply::status(400) };
            let m4 = server.handle(&req.body);
            let mut reply = Reply::tlv(m4);
            if let Some(keys) = server.keys() {
                // The receiver reads with the controller's write key and writes with its read key
                // (pyatv `server_auth.py`: input = Control-Write, output = Control-Read).
                let shared = keys.shared.expose();
                let read = hkdf32(shared, b"Control-Salt", b"Control-Write-Encryption-Key");
                let write = hkdf32(shared, b"Control-Salt", b"Control-Read-Encryption-Key");
                reply.after = After::Encrypt(Box::new(FakeCipher::new(&write, &read)));
                conn.verified = true;
                conn.verify = None;
                conn.shared = Some(*shared);
            }
            reply
        }
        _ => Reply::status(400),
    }
}

fn setup(inner: &Arc<Inner>, conn: &mut Conn, req: &Message) -> Reply {
    let Ok(body) = bplist::decode(&req.body) else { return Reply::status(400) };
    if let Some(Value::Array(streams)) = body.get("streams") {
        let ty = streams.first().and_then(|s| number(s, "type")).unwrap_or(0.0) as i64;
        let mut st = inner.lock();
        let Some(s) = Inner::session_mut(&mut st, conn.id) else { return Reply::status(500) };
        return match ty {
            120 => {
                s.has_120 = true;
                Reply::plist(&dict([("streams", Value::Array(vec![dict([("type", Value::Int(120)), ("streamID", Value::Int(2))])]))]))
            }
            130 => {
                if inner.cfg.profile == Profile::ThirdParty && !s.has_120 {
                    return Reply::status(500);
                }
                s.rcs = true;
                // No dataPort: nothing connects, the stream only registers (PR 2899, foxCast).
                Reply::plist(&dict([("streams", Value::Array(vec![dict([("type", Value::Int(130)), ("streamID", Value::Int(STREAM_ID))])]))]))
            }
            _ => Reply::status(400),
        };
    }
    // The base SETUP: a new session (it takes over from any other sender).
    let ntp = text(&body, "timingProtocol") == Some("NTP");
    let Some(shared) = conn.shared else { return Reply::status(403) };
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind event port");
    std_listener.set_nonblocking(true).expect("nonblocking event listener");
    let port = std_listener.local_addr().expect("event port").port();
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio event listener");
    let (tx, rx) = mpsc::unbounded_channel();
    let mute = ntp && inner.cfg.strict_ntp;
    {
        let mut st = inner.lock();
        if let Some(old) = st.session.take() {
            if old.conn == conn.id {
                let _ = old.events.send(EventOut::Close);
            } else {
                // Another sender took over: the TV drops the old one.
                old.close.cancel();
            }
        }
        st.session = Some(Session {
            conn: conn.id,
            events: tx,
            close: conn.close.clone(),
            events_connected: false,
            recorded: false,
            update_sent: false,
            mute,
            commands: false,
            has_120: false,
            rcs: false,
            hyphenated: false,
            late_feedback_done: false,
        });
    }
    tokio::spawn(serve_events(inner.clone(), listener, shared, rx, conn.close.clone()));
    if mute {
        // PR 2899: "NTP delivers zero events and is gone by 27s" — compressed to 2 s.
        let close = conn.close.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => close.cancel(),
                _ = close.cancelled() => {}
            }
        });
    }
    let mut reply = vec![("eventPort", Value::Int(i64::from(port)))];
    if let Some(skip) = inner.cfg.skip_record {
        reply.push(("skipRecord", Value::Bool(skip)));
    }
    Reply::plist(&Value::Dict(reply.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()))
}

fn record(inner: &Arc<Inner>, conn: &Conn) -> Reply {
    let mut st = inner.lock();
    let Some(s) = Inner::session_mut(&mut st, conn.id) else { return Reply::status(500) };
    if !s.events_connected {
        st.log.push(Entry::Violation(Violation::RecordBeforeEvents));
        return Reply::status(500);
    }
    s.recorded = true;
    inner.maybe_update_info(&mut st);
    Reply::status(inner.cfg.record_status)
}

fn feedback(inner: &Arc<Inner>, conn: &Conn) -> Reply {
    let mut st = inner.lock();
    let skip = inner.cfg.skip_record == Some(true);
    let Some(s) = Inner::session_mut(&mut st, conn.id) else { return Reply::status(500) };
    if !s.recorded && !skip {
        st.log.push(Entry::Violation(Violation::FeedbackBeforeRecord));
        return Reply::close();
    }
    let early = !s.commands;
    let hold = inner.cfg.late_feedback_reply && !s.late_feedback_done;
    if hold {
        s.late_feedback_done = true;
    }
    if early {
        st.log.push(Entry::Violation(Violation::FeedbackBeforeCommands));
    }
    let mut reply = Reply::plist(&dict([("streams", Value::Array(Vec::new()))]));
    if hold {
        reply.after = After::Hold;
    }
    reply
}

fn command(inner: &Arc<Inner>, conn: &Conn, req: &Message) -> Reply {
    let id = STREAM_ID.to_string();
    let joined = req.header("X-Apple-StreamID");
    let hyphenated = req.header("X-Apple-Stream-ID");
    let mut st = inner.lock();
    let Some(s) = Inner::session_mut(&mut st, conn.id) else { return Reply::status(500) };
    if !s.rcs {
        // "/command answers 500 until it exists" (the type-130 SETUP).
        return Reply::status(500);
    }
    match inner.cfg.profile {
        Profile::AppleTv => {
            if joined == Some(id.as_str()) {
            } else if hyphenated.is_some() {
                s.hyphenated = true;
            } else {
                return Reply::status(400);
            }
        }
        Profile::ThirdParty => {
            if req.header("X-Apple-ProtocolVersion").is_some() || req.header("X-Apple-Session-ID").is_some() {
                return Reply::status(400);
            }
            let ok = match inner.cfg.third_party_header {
                StreamHeader::Joined => joined == Some(id.as_str()) && hyphenated.is_none(),
                StreamHeader::Hyphenated => hyphenated == Some(id.as_str()) && joined.is_none(),
                StreamHeader::Absent => joined.is_none() && hyphenated.is_none(),
            };
            if !ok {
                return Reply::status(400);
            }
        }
    }
    s.commands = true;
    let never_starts = s.hyphenated;
    if st.fail_commands > 0 {
        st.fail_commands -= 1;
        return Reply::status(500);
    }
    let delay = st.command_delay;
    let Some(cmd) = decode_command(&req.body) else { return Reply::status(400) };
    let mut reply = Reply { delay, ..Reply::status(200) };
    match text(&cmd, "type").unwrap_or_default() {
        "insertPlayQueueItem" => {
            let Some(item) = cmd.get("item") else { return Reply::status(400) };
            if let Some(id) = text(item, "playerLoggingID")
                && id.chars().count() > 6
            {
                st.log.push(Entry::Violation(Violation::LongPlayerLoggingId(id.to_owned())));
                return Reply::close();
            }
            let (Some(uuid), Some(url)) = (text(item, "uuid"), text(item, "Content-Location")) else {
                return Reply::status(400);
            };
            if text(item, "mediaType") != Some("file") {
                return Reply::status(400);
            }
            let new = Item {
                uuid: uuid.to_owned(),
                url: url.to_owned(),
                hls: item.get("HLS-Content-Location").is_some(),
                started: false,
                cancel: inner.shutdown.child_token(),
            };
            let after = cmd.get("itemAfter").and_then(|a| text(a, "uuid")).map(str::to_owned);
            match after.and_then(|a| st.queue.iter().position(|i| i.uuid == a)) {
                Some(i) => st.queue.insert(i + 1, new),
                None => st.queue.push(new),
            }
            if st.current.is_none() {
                st.current = Some(uuid.to_owned());
            }
        }
        "removePlayQueueItem" => {
            let uuid = cmd.get("item").and_then(|i| text(i, "uuid")).unwrap_or_default().to_owned();
            if let Some(i) = st.queue.iter().position(|it| it.uuid == uuid) {
                let gone = st.queue.remove(i);
                gone.cancel.cancel();
                if st.current.as_deref() == Some(uuid.as_str()) {
                    // Removing the current item advances to the one queued after it (VioletRelay).
                    st.current = st.queue.get(i).map(|it| it.uuid.clone());
                    drop(st);
                    inner.state_event("loading");
                    return reply;
                }
            }
        }
        "setRate" => {
            let rate = number(&cmd, "rate").unwrap_or(0.0);
            st.rate = rate;
            let current = st.current.clone();
            let item = current.as_deref().and_then(|c| st.queue.iter_mut().find(|i| i.uuid == c));
            match item {
                Some(item) if rate > 0.0 && !item.started && !never_starts => {
                    item.started = true;
                    let job = (item.uuid.clone(), item.url.clone(), item.hls, item.cancel.clone());
                    drop(st);
                    tokio::spawn(play_item(inner.clone(), job.0, job.1, job.2, job.3));
                }
                Some(item) if item.started => {
                    drop(st);
                    inner.state_event(if rate > 0.0 { "playing" } else { "paused" });
                }
                _ => {}
            }
        }
        "playbackInfo" => match inner.cfg.playback_info {
            PlaybackInfo::Status500 => return Reply::status(500),
            PlaybackInfo::Silent => {}
            PlaybackInfo::Answer => {
                let (position, duration) = st.position;
                let rate = st.rate;
                drop(st);
                let tvos27 = inner.cfg.tvos == Tvos::V27;
                // tvOS 27: CMTimes (a growing playlist's duration is indefinite, flags 17)
                let time = |t: Option<f64>| match (tvos27, t) {
                    (true, Some(t)) => cmtime((t * 1e9).round() as i64, 1_000_000_000, 1),
                    (true, None) => cmtime(0, 0, 17),
                    (false, t) => Value::Real(t.unwrap_or(f64::INFINITY)),
                };
                let mut value = vec![("position", time(Some(position))), ("rate", Value::Real(rate)), ("readyToPlay", Value::Bool(true))];
                if duration.is_some() || tvos27 {
                    value.push(("duration", time(duration)));
                }
                value.push(("playbackLikelyToKeepUp", Value::Bool(true)));
                let mut answer = vec![("kind", string("response")), ("type", string("playbackInfo"))];
                if let Some(id) = cmd.get("messageID") {
                    answer.push(("messageID", id.clone()));
                }
                let key = if tvos27 { "info" } else { "value" };
                answer.push((key, Value::Dict(value.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())));
                let answer = Value::Dict(answer.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
                reply.then = Some(("playbackInfo response".into(), answer));
            }
        },
        _ => {}
    }
    reply
}

// ---- the TV playing an item -----------------------------------------------------------------------

/// AVPlayer-style: `Range: bytes=0-1` on the playlist, the playlist, then the segments; `loading`
/// first, `playing` with the first segment, `itemPlayedToEnd` after the last one of an ENDLIST
/// playlist; a live playlist is fetched again every target duration.
async fn play_item(inner: Arc<Inner>, uuid: String, url: String, hls: bool, cancel: CancellationToken) {
    inner.notification("currentItemChanged");
    inner.state_event("loading");
    match inner.cfg.fetch {
        Fetch::Never => return,
        Fetch::Slow => {
            tokio::select! {
                _ = tokio::time::sleep(inner.cfg.slow_delay) => {}
                _ = cancel.cancelled() => return,
            }
        }
        Fetch::Normal => {}
    }
    let Ok(base) = url::Url::parse(&url) else { return };
    let apple = inner.cfg.profile == Profile::AppleTv;
    let ua = if apple { inner.cfg.tvos.user_agent() } else { "VIDAA hls.js/4.0.16".into() };
    if apple {
        get(&inner, &uuid, &base, Some("bytes=0-1"), &ua).await;
    }
    let mut last: Option<u64> = None;
    let mut started = false;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let Some(text) = get(&inner, &uuid, &base, None, &ua).await else { return };
        let pl = Playlist::parse(&text);
        if !apple && !hls {
            // VioletRelay: without HLS-Content-Location the TV's web player never asks for segments.
            return;
        }
        for (seq, uri) in &pl.segments {
            if last.is_some_and(|l| *seq <= l) {
                continue;
            }
            let Ok(seg) = base.join(uri) else { continue };
            if cancel.is_cancelled() {
                return;
            }
            if get(&inner, &uuid, &seg, None, &ua).await.is_none() {
                break;
            }
            last = Some(*seq);
            if !started {
                started = true;
                inner.notification("playbackLikelyToKeepUp");
                if inner.rate() > 0.0 {
                    inner.state_event("playing");
                }
            }
        }
        if pl.endlist && last == pl.segments.last().map(|(s, _)| *s) {
            if started {
                inner.notification("itemPlayedToEnd");
            }
            return;
        }
        let wait = Duration::from_secs_f64(pl.target.clamp(0.2, 2.0));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

/// One TV request to the relay (recorded); the body on a 2xx.
async fn get(inner: &Inner, item: &str, url: &url::Url, range: Option<&str>, ua: &str) -> Option<String> {
    let mut rq = inner.http.get(url.clone()).header("User-Agent", ua);
    if let Some(r) = range {
        rq = rq.header("Range", r);
    }
    let (status, body) = match rq.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            (status, resp.text().await.ok())
        }
        Err(_) => (0, None),
    };
    inner.log(Entry::Fetch(Fetched {
        item: item.to_owned(),
        path: url.path().to_owned(),
        range: range.map(str::to_owned),
        user_agent: ua.to_owned(),
        status,
    }));
    if (200..300).contains(&status) { body } else { None }
}

/// The bits of a media playlist the TV needs.
struct Playlist {
    target: f64,
    segments: Vec<(u64, String)>,
    endlist: bool,
}

impl Playlist {
    fn parse(text: &str) -> Self {
        let mut pl = Playlist { target: 1.0, segments: Vec::new(), endlist: false };
        let mut seq = 0u64;
        for line in text.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                pl.target = v.parse().unwrap_or(1.0);
            } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                seq = v.parse().unwrap_or(0);
            } else if line == "#EXT-X-ENDLIST" {
                pl.endlist = true;
            } else if !line.is_empty() && !line.starts_with('#') {
                pl.segments.push((seq, line.to_owned()));
                seq += 1;
            }
        }
        pl
    }
}

// ---- the event channel -------------------------------------------------------------------------

/// The TV's side of the event channel: keys `Events-Salt` (it writes with
/// `Events-Write-Encryption-Key`, reads with `Events-Read-Encryption-Key`; foxCast, pyatv
/// protocols.md), requests `POST /command RTSP/1.0` from CSeq 0, each to be answered
/// `RTSP/1.0 200 OK` with its CSeq within `ack_timeout` or the session closes.
async fn serve_events(
    inner: Arc<Inner>,
    listener: tokio::net::TcpListener,
    shared: [u8; 32],
    mut rx: mpsc::UnboundedReceiver<EventOut>,
    close: CancellationToken,
) {
    let stream = tokio::select! {
        accepted = listener.accept() => match accepted {
            Ok((stream, _)) => stream,
            Err(_) => return,
        },
        _ = close.cancelled() => return,
    };
    drop(listener);
    {
        let mut st = inner.lock();
        st.log.push(Entry::EventsConnected);
        if let Some(s) = st.session.as_mut() {
            s.events_connected = true;
        }
        inner.maybe_update_info(&mut st);
    }
    let write_key = hkdf32(&shared, b"Events-Salt", b"Events-Write-Encryption-Key");
    let read_key = hkdf32(&shared, b"Events-Salt", b"Events-Read-Encryption-Key");
    let mut cipher = FakeCipher::new(&write_key, &read_key);
    let (mut rd, mut wr): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    let mut cseq = 0u32;
    let mut pending: Vec<(u32, tokio::time::Instant)> = Vec::new();
    let (mut raw, mut plain) = (Vec::new(), Vec::new());
    let mut buf = vec![0u8; 16 * 1024];
    let by = 'events: loop {
        let deadline = pending.iter().map(|(_, d)| *d).min();
        tokio::select! {
            out = rx.recv() => match out {
                None | Some(EventOut::Close) => break By::Receiver,
                Some(EventOut::BadFrame) => {
                    let mut frame = 16u16.to_le_bytes().to_vec();
                    frame.extend_from_slice(&[0u8; 32]);
                    inner.log(Entry::BadFrameSent);
                    if wr.write_all(&frame).await.is_err() {
                        break By::Sender;
                    }
                }
                Some(EventOut::Push { what, body }) => {
                    let (bytes, layout) = match body {
                        EventBody::Plain(body) => (event_request(cseq, &body), Layout::Hap),
                        EventBody::UpdateInfo => update_info(&inner, cseq),
                    };
                    let frames = layout.frames(bytes.len());
                    let mut out = Vec::new();
                    let mut at = 0;
                    for n in &frames {
                        out.extend_from_slice(&cipher.seal(&bytes[at..at + n]));
                        at += n;
                    }
                    inner.log(Entry::EventSent { cseq, what, frames });
                    if wr.write_all(&out).await.is_err() {
                        break By::Sender;
                    }
                    pending.push((cseq, tokio::time::Instant::now() + inner.cfg.ack_timeout));
                    cseq += 1;
                }
            },
            n = rd.read(&mut buf) => {
                let Ok(n @ 1..) = n else { break By::Sender };
                raw.extend_from_slice(&buf[..n]);
                loop {
                    match cipher.open(&mut raw) {
                        Ok(Some(p)) => plain.extend_from_slice(&p),
                        Ok(None) => break,
                        Err(()) => {
                            inner.log(Entry::Violation(Violation::BadFrame(Channel::Events)));
                            break 'events By::Receiver;
                        }
                    }
                }
                loop {
                    match parse_message(&mut plain) {
                        Ok(Some(msg)) => {
                            let acked = msg.first_line == "RTSP/1.0 200 OK";
                            match msg.header("CSeq").and_then(|c| c.trim().parse::<u32>().ok()) {
                                Some(c) if acked => {
                                    pending.retain(|(p, _)| *p != c);
                                    inner.log(Entry::EventAcked { cseq: c });
                                }
                                _ => inner.log(Entry::Violation(Violation::BadAck(msg.first_line.clone()))),
                            }
                        }
                        Ok(None) => break,
                        Err(()) => {
                            inner.log(Entry::Violation(Violation::Malformed(Channel::Events)));
                            break 'events By::Receiver;
                        }
                    }
                }
            }
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                let (c, _) = pending.iter().min_by_key(|(_, d)| *d).copied().expect("a pending ack");
                inner.log(Entry::AckTimeout { cseq: c });
                close.cancel();
                break By::Receiver;
            }
            _ = close.cancelled() => break By::Receiver,
        }
    };
    let _ = wr.shutdown().await;
    inner.log(Entry::Closed { channel: Channel::Events, by });
}

/// How an event request is cut into HAP frames.
enum Layout {
    /// 1024-byte frames (pyatv `HAPSession`).
    Hap,
    /// One 1024-byte frame and the rest in one (castcli #35).
    Split,
    /// One frame.
    Single,
}

impl Layout {
    fn frames(&self, len: usize) -> Vec<usize> {
        match self {
            Layout::Hap => {
                let mut v = vec![1024; len / 1024];
                if !len.is_multiple_of(1024) {
                    v.push(len % 1024);
                }
                v
            }
            Layout::Split if len > 1024 => vec![1024, len - 1024],
            Layout::Split | Layout::Single => vec![len],
        }
    }
}

/// `{type: updateInfo, value: <the /info dict>}`, not wrapped in `params.data` (pyatv
/// protocols.md's capture), padded (with `txtAirPlay`, as the real one carries) to the frame
/// size the mode needs: 2283 bytes split 1024 + 1259, or 8931 bytes in one frame.
fn update_info(inner: &Inner, cseq: u32) -> (Vec<u8>, Layout) {
    let build = |pad: usize| {
        let body = bplist::encode(&dict([("type", string("updateInfo")), ("value", inner.info(pad))]));
        event_request(cseq, &body)
    };
    let (target, layout) = if inner.cfg.split_events {
        (Some(2283usize), Layout::Split)
    } else if inner.cfg.big_event_frame {
        (Some(8931), Layout::Single)
    } else {
        (None, Layout::Hap)
    };
    let Some(target) = target else { return (build(0), layout) };
    // Once the padding is over 255 bytes the request grows one byte per padding byte (until the
    // Content-Length gains a digit): aim, then step up to the size.
    let at = build(256).len();
    let mut pad = (target + 256).saturating_sub(at).max(256);
    let mut bytes = build(pad);
    while bytes.len() < target && pad < target {
        pad += 1;
        bytes = build(pad);
    }
    (bytes, layout)
}

fn event_request(cseq: u32, body: &[u8]) -> Vec<u8> {
    let mut out =
        format!("POST /command RTSP/1.0\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n", body.len())
            .into_bytes();
    out.extend_from_slice(body);
    out
}

// ---- wire helpers ------------------------------------------------------------------------------

/// An HTTP/RTSP request or response.
struct Message {
    first_line: String,
    method: String,
    uri: String,
    protocol: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Message {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// One complete message from the front of `buf` (consumed); `Ok(None)`: need more bytes.
fn parse_message(buf: &mut Vec<u8>) -> Result<Option<Message>, ()> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > 64 * 1024 { Err(()) } else { Ok(None) };
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| ())?;
    let mut lines = head.split("\r\n");
    let first_line = lines.next().ok_or(())?.to_owned();
    let mut parts = first_line.splitn(3, ' ');
    let (a, b, c) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .map(|(_, v)| v.parse::<usize>().map_err(|_| ()))
        .transpose()?
        .unwrap_or(0);
    if len > 1 << 20 {
        return Err(());
    }
    if buf.len() < end + 4 + len {
        return Ok(None);
    }
    let body = buf[end + 4..end + 4 + len].to_vec();
    buf.drain(..end + 4 + len);
    let msg = Message { method: a.to_owned(), uri: b.to_owned(), protocol: c.to_owned(), first_line: first_line.clone(), headers, body };
    Ok(Some(msg))
}

fn decode_body(req: &Message) -> Body {
    if req.body.is_empty() {
        return Body::Empty;
    }
    let path = req.uri.split('?').next().unwrap_or_default();
    if path.starts_with("/pair-") {
        return match tlv8::decode(&req.body) {
            Ok(t) => Body::Tlv(t.0),
            Err(_) => Body::Raw(req.body.clone()),
        };
    }
    if path == "/command" {
        return decode_command(&req.body).map(Body::Command).unwrap_or_else(|| Body::Raw(req.body.clone()));
    }
    match bplist::decode(&req.body) {
        Ok(v) => Body::Plist(v),
        Err(_) => Body::Raw(req.body.clone()),
    }
}

/// `{params: {data: <bplist>}}` → the inner plist.
fn decode_command(body: &[u8]) -> Option<Value> {
    let outer = bplist::decode(body).ok()?;
    match outer.get("params")?.get("data")? {
        Value::Data(d) => bplist::decode(d).ok(),
        _ => None,
    }
}

fn response(req: &Message, reply: &Reply, source_version: &str) -> Vec<u8> {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        453 => "Not Enough Bandwidth",
        470 => "Connection Authorization Required",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Status",
    };
    let protocol = if req.protocol.is_empty() { "HTTP/1.1" } else { req.protocol.as_str() };
    let mut head = format!("{protocol} {} {reason}\r\nServer: AirTunes/{source_version}\r\n", reply.status);
    if let Some(cseq) = req.header("CSeq") {
        let _ = write!(head, "CSeq: {cseq}\r\n");
    }
    if let Some(ct) = reply.content_type {
        let _ = write!(head, "Content-Type: {ct}\r\n");
    }
    let _ = write!(head, "Content-Length: {}\r\n\r\n", reply.body.len());
    let mut out = head.into_bytes();
    out.extend_from_slice(&reply.body);
    out
}

fn encode_tlv(items: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let refs: Vec<(u8, &[u8])> = items.iter().map(|(t, v)| (*t, v.as_slice())).collect();
    tlv8::encode(&refs)
}

/// Re-encode a TLV8 message with `f` applied to each value.
fn map_tlv(msg: &[u8], mut f: impl FnMut(u8, &mut Vec<u8>)) -> Vec<u8> {
    let Ok(t) = tlv8::decode(msg) else { return msg.to_vec() };
    let mut items = t.0;
    for (k, v) in &mut items {
        f(*k, v);
    }
    encode_tlv(&items)
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Dict(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// A CMTime dict as tvOS sends times (`flags` 1 valid, 17 valid + indefinite).
fn cmtime(value: i64, timescale: i64, flags: i64) -> Value {
    dict([("epoch", Value::Int(0)), ("flags", Value::Int(flags)), ("timescale", Value::Int(timescale)), ("value", Value::Int(value))])
}

fn string(v: &str) -> Value {
    Value::String(v.to_owned())
}

// ---- crypto (the fake's own, on aws-lc-rs) ------------------------------------------------------

struct Len32;

impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

/// `HKDF-SHA512(ikm, salt, info)` → 32 bytes (pyatv `hkdf_expand`).
pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA512, salt).extract(ikm);
    let info = [info];
    let okm = prk.expand(&info, Len32).expect("HKDF length");
    let mut out = [0u8; 32];
    okm.fill(&mut out).expect("HKDF fill");
    out
}

/// The Ed25519 public key of a seed.
pub fn ed25519_public(seed: &[u8; 32]) -> [u8; 32] {
    let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("Ed25519 seed");
    key.public_key().as_ref().try_into().expect("32-byte key")
}

fn chacha(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("ChaCha20-Poly1305 key"))
}

/// A pairing nonce: 4 zero bytes and the 8-byte label (`PS-Msg05`; pyatv pads short nonces).
fn label_nonce(label: &[u8; 8]) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(label);
    Nonce::assume_unique_for_key(n)
}

fn seal_once(key: &[u8; 32], label: &[u8; 8], plain: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    chacha(key).seal_in_place_append_tag(label_nonce(label), Aad::empty(), &mut out).expect("seal");
    out
}

fn open_once(key: &[u8; 32], label: &[u8; 8], sealed: &[u8]) -> Option<Vec<u8>> {
    let mut buf = sealed.to_vec();
    let plain = chacha(key).open_in_place(label_nonce(label), Aad::empty(), &mut buf).ok()?;
    Some(plain.to_vec())
}

/// HAP framing, one direction each way: `[len u16 LE][ciphertext][tag 16]`, the length as AAD,
/// nonce = 4 zero bytes + a little-endian u64 counter (pyatv `hap_session.py`, `chacha20.py`).
/// Frames of any length the header allows are opened and sealed.
struct FakeCipher {
    seal: LessSafeKey,
    open: LessSafeKey,
    out: u64,
    inn: u64,
}

impl FakeCipher {
    fn new(write: &[u8; 32], read: &[u8; 32]) -> Self {
        Self { seal: chacha(write), open: chacha(read), out: 0, inn: 0 }
    }

    fn nonce(n: u64) -> Nonce {
        let mut b = [0u8; 12];
        b[4..].copy_from_slice(&n.to_le_bytes());
        Nonce::assume_unique_for_key(b)
    }

    /// One frame holding all of `plain`.
    fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let len = u16::try_from(plain.len()).expect("frame fits u16").to_le_bytes();
        let mut body = plain.to_vec();
        self.seal.seal_in_place_append_tag(Self::nonce(self.out), Aad::from(len), &mut body).expect("seal frame");
        self.out += 1;
        let mut out = len.to_vec();
        out.extend_from_slice(&body);
        out
    }

    /// The first complete frame of `raw` (consumed); `Err` when its tag doesn't verify.
    fn open(&mut self, raw: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ()> {
        if raw.len() < 2 {
            return Ok(None);
        }
        let len = usize::from(u16::from_le_bytes([raw[0], raw[1]]));
        if raw.len() < 2 + len + 16 {
            return Ok(None);
        }
        let aad = [raw[0], raw[1]];
        let mut body = raw[2..2 + len + 16].to_vec();
        raw.drain(..2 + len + 16);
        let plain = self.open.open_in_place(Self::nonce(self.inn), Aad::from(aad), &mut body).map_err(|_| ())?;
        self.inn += 1;
        Ok(Some(plain.to_vec()))
    }
}
