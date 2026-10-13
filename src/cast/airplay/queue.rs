// Ported from pyatv pyatv/protocols/raop/protocols/airplayv2.py @ b2484099 (PR 2899; MIT, © 2020 Pierre Ståhl).
//! `/command` builders per profile (DESIGN §40 §7.3, §7.7): the item dict, `insertPlayQueueItem`,
//! `setProperty`, `setRate`, `removePlayQueueItem`, the replace sequence (VioletRelay, facts
//! only), `X-Apple-StreamID` spelt exactly so, `playerLoggingID` ≤ 6 characters, start position
//! always 0. OWNER: CAST-2. Pure (unit-tested).
//!
//! Ported from pyatv PR 2899's `send_command` (the `{params: {data: <bplist>}}` body, the command
//! headers) and `_queue_item` @ b2484099 (MIT); robkochman's `actionAtItemEnd 1` (MIT, a pyatv
//! branch); PR 2846's `playbackInfo` request (MIT). Facts only from foxCast (the item's key set
//! and values, the minimal headers that work on tvOS 27) and VioletRelay (the replace sequence,
//! the third-party item keys and the stream-id header negotiation).
//!
//! * **The item** is foxCast's key set. `playerLoggingID` is at most 6 characters: longer values
//!   make the TV close the connection silently (`P/ABCD` works, `P/ABCDE` doesn't; PR 2899).
//! * **The start position is always 0**: every playlist the TV gets starts where playback should
//!   (a `-ss` restart or a playlist view), or says where (a full-length item's `EXT-X-START`,
//!   §8.5), so nothing depends on whether tvOS honours a start key (Screenbox: tvOS 26 ignores
//!   it).
//! * **Headers** (Apple TV): `POST /command HTTP/1.1` with `Content-Type` and `X-Apple-StreamID`,
//!   spelt exactly so — the hyphenated `X-Apple-Stream-ID` gets a 200 on an Apple TV but the
//!   item never starts (VioletRelay). No `X-Apple-ProtocolVersion` or `X-Apple-Session-ID` (a
//!   Hisense rejects them on `/command`).
//! * **A third-party TV** gets `POST /command RTSP/1.0`, as VioletRelay sends it through pyatv's
//!   `RtspSession.exchange` (the mux adds CSeq, DACP-ID, Active-Remote, Client-Instance and
//!   pyatv's User-Agent), with the negotiated stream-id header (§7.7).

use super::Profile;
use super::bplist::{self, Value};
use super::rtsp::{BPLIST_CONTENT_TYPE, Request};

/// PR 2899 `COMMAND_HEADERS`.
pub const COMMAND_USER_AGENT: &str = "AirPlay/960.13.1";
pub const CLIENT_BUNDLE_ID: &str = "io.github.rthoms334.tuner";
pub const CLIENT_PROC_NAME: &str = "Tuner";
/// At most 6 characters (PR 2899).
pub const PLAYER_LOGGING_ID: &str = "TUNER";
pub const PLAYER_ITEM_LOGGING_ID: &str = "I/TUNER.01";

/// A dict in the given key order.
pub fn dict<'a>(pairs: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Dict(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

/// An integer member (a whole real counts too).
pub fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Int(i) => Some(*i),
        Value::Real(r) if r.fract() == 0.0 && r.is_finite() && r.abs() < 9.0e15 => Some(*r as i64),
        _ => None,
    }
}

/// A number member.
pub fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Int(i) => Some(*i as f64),
        Value::Real(r) => Some(*r),
        _ => None,
    }
}

pub fn str_of(v: Option<&Value>) -> Option<&str> {
    match v? {
        Value::String(s) => Some(s),
        _ => None,
    }
}

/// `Start-Position {flags 1, value 0, epoch 0, timescale}` (a CMTime of 0).
fn start_position(timescale: i64) -> Value {
    dict([("flags", Value::Int(1)), ("value", Value::Int(0)), ("epoch", Value::Int(0)), ("timescale", Value::Int(timescale))])
}

/// The play-queue item (DESIGN §40 §7.3 "The item"). A third-party TV (§7.7) also gets
/// `HLS-Content-Location` (the same URL: "the receiver uses this key — not the URL suffix — to
/// select its HLS/MSE player", VioletRelay), `isAudioOnly false`, `streamType 1`, and a
/// `Start-Position` with timescale 1000.
pub fn item(profile: Profile, uuid: &str, url: &str) -> Value {
    let mut d = vec![
        ("uuid", text(uuid)),
        ("Content-Location", text(url)),
        ("mediaType", text("file")),
        ("IsTLSEnabled", Value::Bool(url.starts_with("https://"))),
        ("playbackRestrictions", Value::Int(0)),
        ("referenceRestrictions", Value::Int(2)),
        ("supportsIntegratedTimeline", Value::Bool(false)),
        ("snapTimeToPausePlayback", Value::Bool(false)),
        ("clientBundleID", text(CLIENT_BUNDLE_ID)),
        ("clientProcName", text(CLIENT_PROC_NAME)),
        ("playerLoggingID", text(PLAYER_LOGGING_ID)),
        ("playerItemLoggingID", text(PLAYER_ITEM_LOGGING_ID)),
    ];
    match profile {
        Profile::AppleTv => d.push(("Start-Position", start_position(1))),
        Profile::ThirdParty => d.extend([
            ("HLS-Content-Location", text(url)),
            ("isAudioOnly", Value::Bool(false)),
            ("streamType", Value::Int(1)),
            ("Start-Position", start_position(1000)),
        ]),
    }
    dict(d)
}

/// `insertPlayQueueItem` (fatal on non-2xx); `after`: the item it goes behind (a replace).
pub fn insert(profile: Profile, uuid: &str, url: &str, after: Option<&str>) -> Value {
    let mut d = vec![("type", text("insertPlayQueueItem")), ("item", item(profile, uuid, url))];
    if let Some(old) = after {
        d.push(("itemAfter", dict([("uuid", text(old))])));
    }
    dict(d)
}

/// `setProperty isInterestedInDateRange true` on the item (non-fatal; castcli and foxCast send
/// it, VioletRelay doesn't).
pub fn date_range(uuid: &str) -> Value {
    dict([
        ("type", text("setProperty")),
        ("property", text("isInterestedInDateRange")),
        ("value", Value::Bool(true)),
        ("item", dict([("uuid", text(uuid))])),
    ])
}

/// `setProperty actionAtItemEnd` (robkochman, castcli: 1; VioletRelay: required for URL playback
/// to start on its 26.x hardware; 0 while a replace swaps the items).
pub fn action_at_item_end(value: i64) -> Value {
    dict([("type", text("setProperty")), ("property", text("actionAtItemEnd")), ("value", Value::Int(value))])
}

/// `setRate` (1.0 plays — the item loads paused without it —, 0.0 pauses).
pub fn set_rate(rate: f64) -> Value {
    dict([("type", text("setRate")), ("rate", Value::Real(rate))])
}

pub fn remove(uuid: &str) -> Value {
    dict([("type", text("removePlayQueueItem")), ("item", dict([("uuid", text(uuid))]))])
}

/// A `playbackInfo` request (PR 2846), answered on the event channel by `{kind: "response",
/// messageID}`.
pub fn playback_info(message_id: i64) -> Value {
    dict([("type", text("playbackInfo")), ("kind", text("request")), ("messageID", Value::Int(message_id))])
}

/// The replace (zap, seek, live reconnect; VioletRelay `replace`, hardware-tested on 26.5 with an
/// Apple TV and a Hisense): `setRate 0` → `actionAtItemEnd 0` → insert the new item after the
/// old one → remove the old one → `setRate 1`. `actionAtItemEnd 1` follows once the TV fetched
/// the new item (`Client::fetched`).
pub fn replace(profile: Profile, old: &str, new: &str, url: &str) -> [Value; 5] {
    [set_rate(0.0), action_at_item_end(0), insert(profile, new, url, Some(old)), remove(old), set_rate(1.0)]
}

/// The `/command` body: the command as a binary plist inside `{params: {data}}`.
pub fn wrap(cmd: &Value) -> Value {
    dict([("params", dict([("data", Value::Data(bplist::encode(cmd)))]))])
}

/// How a `/command` names the remote-control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamHeader {
    /// `X-Apple-StreamID` (the Apple TV's only form).
    StreamId,
    /// `X-Apple-Stream-ID` (some third-party firmware).
    Hyphenated,
    /// No stream header.
    Omitted,
}

impl StreamHeader {
    pub fn name(self) -> Option<&'static str> {
        match self {
            StreamHeader::StreamId => Some("X-Apple-StreamID"),
            StreamHeader::Hyphenated => Some("X-Apple-Stream-ID"),
            StreamHeader::Omitted => None,
        }
    }
}

/// The stream-header spelling a session uses: the Apple TV's is fixed; a third-party TV's is
/// negotiated in the order `X-Apple-Stream-ID`, none, `X-Apple-StreamID` until one is answered
/// 2xx, then kept (DESIGN §40 §7.7, VioletRelay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spelling {
    profile: Profile,
    confirmed: Option<StreamHeader>,
}

impl Spelling {
    pub fn new(profile: Profile) -> Self {
        Self { profile, confirmed: None }
    }

    /// The spellings to try for the next command, in order.
    pub fn candidates(&self) -> Vec<StreamHeader> {
        match (self.profile, self.confirmed) {
            (Profile::AppleTv, _) => vec![StreamHeader::StreamId],
            (Profile::ThirdParty, Some(h)) => vec![h],
            (Profile::ThirdParty, None) => vec![StreamHeader::Hyphenated, StreamHeader::Omitted, StreamHeader::StreamId],
        }
    }

    pub fn confirm(&mut self, h: StreamHeader) {
        self.confirmed = Some(h);
    }
}

/// `POST /command` with the stream header: an Apple TV's over HTTP/1.1 with PR 2899's headers
/// (User-Agent, Content-Type); a third-party TV's over RTSP/1.0 with the Content-Type, as
/// VioletRelay's `send` (pyatv `RtspSession.exchange`, its default User-Agent).
pub fn request(profile: Profile, body: &Value, header: StreamHeader, stream_id: i64) -> Request {
    let req = match profile {
        Profile::AppleTv => Request::http("POST", "/command").header("User-Agent", COMMAND_USER_AGENT),
        Profile::ThirdParty => Request::rtsp("POST", "/command"),
    };
    let req = req.header("Content-Type", BPLIST_CONTENT_TYPE);
    let req = match header.name() {
        Some(name) => req.header(name, stream_id),
        None => req,
    };
    req.body(bplist::encode(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://192.168.1.88:47800/c/tok/index.m3u8";

    fn keys(v: &Value) -> Vec<&str> {
        match v {
            Value::Dict(d) => d.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn the_apple_tv_item_is_foxcasts_key_set_starting_at_zero() {
        let it = item(Profile::AppleTv, "A-UUID", URL);
        assert_eq!(
            keys(&it),
            [
                "uuid",
                "Content-Location",
                "mediaType",
                "IsTLSEnabled",
                "playbackRestrictions",
                "referenceRestrictions",
                "supportsIntegratedTimeline",
                "snapTimeToPausePlayback",
                "clientBundleID",
                "clientProcName",
                "playerLoggingID",
                "playerItemLoggingID",
                "Start-Position"
            ]
        );
        assert_eq!(it.get("Content-Location"), Some(&text(URL)));
        assert_eq!(it.get("IsTLSEnabled"), Some(&Value::Bool(false)));
        assert_eq!(it.get("referenceRestrictions"), Some(&Value::Int(2)));
        let start = it.get("Start-Position").unwrap();
        assert_eq!(
            (start.get("value"), start.get("timescale"), start.get("flags")),
            (Some(&Value::Int(0)), Some(&Value::Int(1)), Some(&Value::Int(1)))
        );
        assert!(it.get("HLS-Content-Location").is_none() && it.get("streamType").is_none());
        // PR 2899: longer than 6 characters and the TV closes the connection silently
        assert!(PLAYER_LOGGING_ID.chars().count() <= 6);
    }

    #[test]
    fn a_third_party_item_adds_violetrelays_keys() {
        let it = item(Profile::ThirdParty, "A-UUID", URL);
        assert_eq!(it.get("HLS-Content-Location"), Some(&text(URL)));
        assert_eq!(it.get("isAudioOnly"), Some(&Value::Bool(false)));
        assert_eq!(it.get("streamType"), Some(&Value::Int(1)));
        let start = it.get("Start-Position").unwrap();
        assert_eq!((start.get("value"), start.get("timescale")), (Some(&Value::Int(0)), Some(&Value::Int(1000))));
        assert_eq!(it.get("playerLoggingID"), Some(&text(PLAYER_LOGGING_ID)));
    }

    #[test]
    fn the_replace_sequence_is_violetrelays() {
        let steps = replace(Profile::AppleTv, "OLD", "NEW", URL);
        let types: Vec<_> = steps.iter().map(|c| str_of(c.get("type")).unwrap()).collect();
        assert_eq!(types, ["setRate", "setProperty", "insertPlayQueueItem", "removePlayQueueItem", "setRate"]);
        assert_eq!(steps[0].get("rate"), Some(&Value::Real(0.0)));
        assert_eq!((steps[1].get("property"), steps[1].get("value")), (Some(&text("actionAtItemEnd")), Some(&Value::Int(0))));
        assert_eq!(steps[2].get("itemAfter"), Some(&dict([("uuid", text("OLD"))])));
        assert_eq!(steps[2].get("item").and_then(|i| i.get("uuid")), Some(&text("NEW")));
        assert_eq!(steps[3].get("item"), Some(&dict([("uuid", text("OLD"))])));
        assert_eq!(steps[4].get("rate"), Some(&Value::Real(1.0)));
        assert!(insert(Profile::AppleTv, "NEW", URL, None).get("itemAfter").is_none());
    }

    #[test]
    fn the_stream_header_is_spelt_exactly_and_negotiated_for_third_parties() {
        let body = wrap(&set_rate(1.0));
        let req = request(Profile::AppleTv, &body, StreamHeader::StreamId, 7);
        assert_eq!((req.method, req.uri.as_str(), req.proto.as_str()), ("POST", "/command", "HTTP/1.1"));
        assert!(req.headers.contains(&("X-Apple-StreamID".to_owned(), "7".to_owned())));
        assert!(req.headers.contains(&("Content-Type".to_owned(), BPLIST_CONTENT_TYPE.to_owned())));
        for never in ["X-Apple-Stream-ID", "X-Apple-ProtocolVersion", "X-Apple-Session-ID"] {
            assert!(!req.has_header(never), "{never}");
        }
        // a third-party TV: RTSP (VioletRelay), the stream header as negotiated, nothing else
        let other = request(Profile::ThirdParty, &body, StreamHeader::Omitted, 7);
        assert_eq!((other.uri.as_str(), other.proto.as_str()), ("/command", "RTSP/1.0"));
        assert!(!other.headers.iter().any(|(k, _)| k.starts_with("X-Apple")));

        let mut apple = Spelling::new(Profile::AppleTv);
        assert_eq!(apple.candidates(), [StreamHeader::StreamId]);
        apple.confirm(StreamHeader::StreamId);
        assert_eq!(apple.candidates(), [StreamHeader::StreamId]);
        let mut other = Spelling::new(Profile::ThirdParty);
        assert_eq!(other.candidates(), [StreamHeader::Hyphenated, StreamHeader::Omitted, StreamHeader::StreamId]);
        other.confirm(StreamHeader::Omitted);
        assert_eq!(other.candidates(), [StreamHeader::Omitted]);
    }
}
