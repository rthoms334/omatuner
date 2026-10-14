//! IPC v2: the request/response protocol between `tuner mcp` and the running Tuner (DESIGN
//! §30.3). SHARED by both MCP tracks — implemented in the skeleton; a change needs both tracks'
//! agreement (adding a field to a result struct is free: the bridge passes results through as
//! JSON without decoding them).
//!
//! One JSON line per connection on the existing socket (`ipc.rs`: same-uid peers only, 64 KiB
//! request, 5 s per connection):
//!
//! ```text
//! → {"v":2,"id":7,"agent":{"name":"codex-mcp-client","title":"Codex"},"method":"guide_search","params":{"query":"touring car"}}
//! ← {"v":2,"id":7,"ok":true,"result":{"programmes":[…],"truncated":false}}
//! ← {"v":2,"id":7,"ok":false,"error":{"code":"not_found","message":"…"}}
//! ```
//! A line without `"v"` (or `"v":1`) is a v1 [`crate::ipc::IpcRequest`] (`--view`/`--play`
//! handoff). `agent` (optional) is the MCP client that asked, as its `initialize` reported it;
//! Tuner from before 0.2.2 ignores it, bridges before 0.2.2 don't send it, and a malformed one is
//! read as absent (never a refused call). A Tuner from before v2 answers a v2 line with a v1 error
//! (`{"ok":false,"error":"bad request: …"}`): the client reports "restart Tuner to update it".
//!
//! Tool arguments ARE the params: `tuner mcp` forwards a `tools/call`'s `arguments` unchanged as
//! `params` (`{}` when absent), so the JSON schemas in `agent::tools` and the param structs here
//! describe the same objects (`tests` pin that every schema example parses).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use super::params::*;
use crate::model::{CastPhase, ItemKey, ItemKind, ItemRef, SourceId, Timestamp};

/// Protocol version of this build.
pub const VERSION: u32 = 2;
/// Largest reply line a client reads (results are bounded by their `limit`s far below it).
pub const MAX_REPLY: usize = 1 << 20;
/// Longest free-text parameter (queries, names, times); longer ones are refused.
pub const MAX_TEXT: usize = 200;

/// `{"v":2,"id":…,"agent":…,"method":…,"params":…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    /// The MCP client that asked; `None` from bridges before 0.2.2 (DESIGN §30.12) or when
    /// malformed: it only names the agent on screen, so it never fails the call.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_agent")]
    pub agent: Option<ClientInfo>,
    #[serde(flatten)]
    pub call: Call,
}

/// The MCP client as `initialize.clientInfo` reported it. Client-supplied: Tuner names it with
/// [`crate::model::AgentName::from_client`] (sanitised, display only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `agent` as a [`ClientInfo`] when it is one, else `None`.
fn lenient_agent<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ClientInfo>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}

impl Request {
    pub fn new(id: u64, call: Call) -> Self {
        Self { v: VERSION, id, agent: None, call }
    }

    /// The same request, saying which MCP client asked.
    pub fn with_agent(mut self, agent: Option<ClientInfo>) -> Self {
        self.agent = agent;
        self
    }

    /// Who asked, as Tuner names it ("your agent" when the bridge didn't say).
    pub fn agent_name(&self) -> crate::model::AgentName {
        self.agent.as_ref().map_or_else(Default::default, |c| crate::model::AgentName::from_client(&c.name, c.title.as_deref()))
    }

    /// The protocol version of a request line: `Some(v)` for a JSON object with a numeric `v`,
    /// `None` for anything else (a v1 request has no `v`).
    pub fn peek_version(line: &str) -> Option<u64> {
        serde_json::from_str::<serde_json::Value>(line).ok()?.get("v")?.as_u64()
    }

    /// The `id` of a request line that may not parse as a [`Request`] (for the error reply).
    pub fn peek_id(line: &str) -> u64 {
        serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0)
    }
}

/// One method per MCP tool (same names).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    GuideSearch(GuideSearch),
    WhatsOn(WhatsOn),
    Search(Search),
    Details(Details),
    Categories(Categories),
    Browse(Browse),
    ContinueWatching(ContinueWatching),
    Favourites(Favourites),
    Status(Empty),
    CastDevices(Empty),
    Play(Play),
    ScheduleSwitch(ScheduleSwitch),
    CancelSwitch(CancelSwitch),
    Control(Control),
    // ---- the batch (DESIGN §31–§35, the skill plan): action-shaped feature tools…
    Queue(Queue),
    Watchlist(Watchlist),
    Follow(Follow),
    SpoilerGuard(SpoilerGuard),
    GuideFix(GuideFix),
    Multiview(MultiView),
    Reminders(Reminders),
    SleepTimer(SleepTimer),
    // …and library / health tools
    MatchTitles(MatchTitles),
    History(History),
    Episodes(Episodes),
    FavouritesEdit(FavouritesEdit),
    ParentalStatus(Empty),
    Lock(Lock),
    Diagnose(Diagnose),
    Sources(Empty),
}

impl Call {
    /// The method / tool name.
    pub fn name(&self) -> &'static str {
        match self {
            Call::GuideSearch(_) => "guide_search",
            Call::WhatsOn(_) => "whats_on",
            Call::Search(_) => "search",
            Call::Details(_) => "details",
            Call::Categories(_) => "categories",
            Call::Browse(_) => "browse",
            Call::ContinueWatching(_) => "continue_watching",
            Call::Favourites(_) => "favourites",
            Call::Status(_) => "status",
            Call::CastDevices(_) => "cast_devices",
            Call::Play(_) => "play",
            Call::ScheduleSwitch(_) => "schedule_switch",
            Call::CancelSwitch(_) => "cancel_switch",
            Call::Control(_) => "control",
            Call::Queue(_) => "queue",
            Call::Watchlist(_) => "watchlist",
            Call::Follow(_) => "follow",
            Call::SpoilerGuard(_) => "spoiler_guard",
            Call::GuideFix(_) => "guide_fix",
            Call::Multiview(_) => "multiview",
            Call::Reminders(_) => "reminders",
            Call::SleepTimer(_) => "sleep_timer",
            Call::MatchTitles(_) => "match_titles",
            Call::History(_) => "history",
            Call::Episodes(_) => "episodes",
            Call::FavouritesEdit(_) => "favourites_edit",
            Call::ParentalStatus(_) => "parental_status",
            Call::Lock(_) => "lock",
            Call::Diagnose(_) => "diagnose",
            Call::Sources(_) => "sources",
        }
    }

    /// Changes something (playback, reminders, rules, guide data, locks): rate-limited
    /// separately, logged at INFO. Action-shaped tools decide per verb.
    pub fn is_action(&self) -> bool {
        use super::actions::ToolAction;
        match self {
            Call::Play(_) | Call::ScheduleSwitch(_) | Call::CancelSwitch(_) | Call::Control(_) => true,
            Call::FavouritesEdit(_) | Call::Lock(_) => true,
            Call::Queue(p) => p.action.mutates(),
            Call::Watchlist(p) => p.action.mutates(),
            Call::Follow(p) => p.action.mutates(),
            Call::SpoilerGuard(p) => p.action.mutates(),
            Call::GuideFix(p) => p.action.mutates(),
            Call::Multiview(p) => p.action.mutates(),
            Call::Reminders(p) => p.action.mutates(),
            Call::SleepTimer(p) => p.action.mutates(),
            _ => false,
        }
    }

    /// How the spoiler guard treats this call's result (B4): `spoiler_safe=true` strips
    /// sub-titles, descriptions and plots of everything; the refs in `reveal_spoilers` (the skill
    /// passes them only when the user asked to see them in that turn) come back unguarded.
    pub fn guard_opts(&self) -> (crate::guard::Policy, Vec<String>) {
        let (safe, reveal) = match self {
            Call::GuideSearch(p) => (p.spoiler_safe, p.reveal_spoilers.clone()),
            Call::WhatsOn(p) => (p.spoiler_safe, p.reveal_spoilers.clone()),
            Call::Details(p) => (p.spoiler_safe, p.reveal_spoilers.clone()),
            Call::Search(p) => (p.spoiler_safe, None),
            Call::Browse(p) => (p.spoiler_safe, None),
            Call::ContinueWatching(p) => (p.spoiler_safe, None),
            Call::Episodes(p) => (p.spoiler_safe, p.reveal_spoilers.clone()),
            Call::Follow(p) => (p.spoiler_safe, None),
            _ => (None, None),
        };
        let policy = if safe == Some(true) { crate::guard::Policy::SpoilerSafe } else { crate::guard::Policy::Normal };
        (policy, reveal.unwrap_or_default().into_iter().map(|r| r.trim().to_owned()).collect())
    }

    /// Build a call from a tool name and its arguments (`tools/call`). `Err`: unknown tool, or
    /// arguments that don't match its schema (the serde message, for the model to correct).
    pub fn from_tool(name: &str, arguments: serde_json::Value) -> Result<Call, String> {
        let wire = serde_json::json!({ "method": name, "params": arguments });
        serde_json::from_value(wire).map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------------ params

/// The kinds of catalogue item the tools speak of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Channel,
    Movie,
    Series,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuideSearch {
    /// Words of a programme title / episode title (FTS, last word a prefix). Without it the
    /// search needs a window (`from` + `to`, at most 3 days) and `categories` or `channel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Only programmes overlapping [`from`, `to`) ([`parse_when`] formats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Include programmes that already ended (catch-up). Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_past: Option<bool>,
    /// Guide categories, any of (words, case-insensitive: "football", "news").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    /// Only on this channel (name words or number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Only channels of this playlist (name words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    /// Only favourite channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites_only: Option<bool>,
    /// Only programmes the guide marks live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_only: Option<bool>,
    /// Only programmes the guide marks new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_only: Option<bool>,
    /// Leave out programmes the guide marks as shown before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_repeats: Option<bool>,
    /// Only programmes the archive can play now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catchup_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_spoilers: Option<Vec<String>>,
    /// 1–50, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhatsOn {
    /// Default now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Channel name (words, case-insensitive) or number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Channel group / category name (words, case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favourites_only: Option<bool>,
    /// The programme's guide categories, any of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_spoilers: Option<Vec<String>>,
    /// 1–100, default 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub query: String,
    /// Default all three.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<Kind>>,
    /// Also say which of the user's own settings hide matching titles (a hidden category, the
    /// category filter, a playlist kept out of All, title languages). Never parental control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain_filtered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    /// 1–50, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Details {
    /// An [`AgentRef`] string.
    #[serde(rename = "ref")]
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_spoilers: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Categories {
    pub kind: Kind,
    /// Playlist name (words); default every playlist in "All playlists".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    /// Only categories about this (read from their names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<CategoryKind>,
}

/// What a category is about, read from its name (`categories tag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryKind {
    Kids,
    Family,
    News,
    Sport,
    Music,
    Doc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowseSort {
    /// Recently added first (default).
    Recent,
    Rating,
    Year,
    Name,
    /// A random sample (mood picks): a fresh one every call.
    Random,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Browse {
    pub kind: Kind,
    /// Category name (words, case-insensitive; `categories` lists them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    /// Only titles never started (movies) / with no episode watched (series).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unwatched: Option<bool>,
    /// Movies only: runtime at most this; titles of unknown runtime come after the known ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
    /// 0–10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rating: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_from: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_to: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<BrowseSort>,
    /// Only titles added since ([`parse_when`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_since: Option<String>,
    /// Movies / series: a cast member or director (name words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    /// Movies / series: genre words ("thriller").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    /// 1–50, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Paging (not with `random`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueWatching {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Favourites {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// Favourites list name (default: every list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// Items per list, 1–100, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// Channel, movie, series, episode or programme ref.
    #[serde(rename = "ref")]
    pub item: String,
    /// Movies / episodes: ignore saved progress. Live programme: start over from the archive
    /// when the channel has catch-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_start: Option<bool>,
    /// Where it plays: `popout`, `full_window` or `where_i_am` (`agent::act::parse_open_in`);
    /// default: the user's setting (DESIGN §30.13). Older clients don't send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_in: Option<String>,
    /// Play it on this TV (DESIGN §40 §12): a `device:<hex>` ref from `cast_devices`, or the
    /// TV's exact name. The window stays where it is, whatever `open_in` says. A Tuner from
    /// before casting refuses the field and the bridge never retries without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSwitch {
    /// A programme ref (switch at its start), or a channel ref with `at`.
    #[serde(rename = "ref")]
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelSwitch {
    /// `SwitchOut::id` (or `StatusOut::scheduled[].id`).
    pub id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Pause,
    Resume,
    TogglePause,
    Stop,
    /// `value` = 0–130 (percent).
    Volume,
    Mute,
    Unmute,
    /// Back to the channel watched before (the PIN rules apply).
    PreviousChannel,
    /// Move what plays to the pop-out (the small pinned window; it never takes the keyboard).
    Popout,
    /// Move what plays to Tuner's window, fullscreen and raised ("put it on the big screen",
    /// DESIGN §30.13): out of the pop-out without starting it again.
    Window,
    /// End the cast: the TV stops and nothing plays here (DESIGN §40 §12).
    StopCasting,
    /// End the cast and carry on here, at the TV's position (live: at the live point).
    PlayHere,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub action: ControlAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Multi-view: one picture (`a` or `b`). Default: pause / resume / stop both, volume and
    /// mute the one with the sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile: Option<TileName>,
}

/// A picture of multi-view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileName {
    A,
    B,
}

impl TileName {
    pub fn id(self) -> crate::model::TileId {
        match self {
            TileName::A => crate::model::TileId::A,
            TileName::B => crate::model::TileId::B,
        }
    }

    pub fn of(t: crate::model::TileId) -> Self {
        if t == crate::model::TileId::A { TileName::A } else { TileName::B }
    }
}

// ------------------------------------------------------------------ reply

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub v: u32,
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
}

impl Reply {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self { v: VERSION, id, ok: true, result: Some(result), error: None }
    }

    /// A typed result (every result type is a JSON object).
    pub fn of<T: Serialize>(id: u64, result: &T) -> Self {
        match serde_json::to_value(result) {
            Ok(v) => Self::ok(id, v),
            Err(e) => Self::err(id, ErrorCode::Internal, e.to_string()),
        }
    }

    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { v: VERSION, id, ok: false, result: None, error: Some(AgentError { code, message: message.into() }) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentError {
    pub code: ErrorCode,
    /// Plain English for the model; never a title of concealed content, a URL or a login.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Params don't parse / are out of range (message says which).
    BadRequest,
    /// No such item — or one parental control hides (indistinguishable on purpose).
    NotFound,
    /// Exists but can't be done: an ended programme without catch-up, nothing playing…
    Unavailable,
    /// A programme that hasn't started: use `schedule_switch`.
    NotStarted,
    /// The library hasn't loaded yet (right after start): retry shortly.
    Loading,
    RateLimited,
    /// Too many agent requests at once.
    Busy,
    /// The method isn't available in this build / instance.
    Unsupported,
    Internal,
}

// ------------------------------------------------------------------ results

/// Air time relative to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Airing {
    Past,
    Live,
    Upcoming,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// Playlist (source) name.
    pub playlist: String,
    /// Primary group's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `SD` / `HD` / `FHD` / `4K` (`4K?` doubtful), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Catch-up archive in days (0 = none).
    pub catchup_days: u32,
    pub favourite: bool,
    /// Needs the parental PIN (shown on screen by `play`).
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgrammeOut {
    /// `programme:` ref on `channel`.
    #[serde(rename = "ref")]
    pub item: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Local time with offset ([`fmt_time`]); the channel's tvg-shift applied.
    pub start: String,
    pub end: String,
    pub airing: Airing,
    /// Playable from the archive (past, or live and restartable) on `channel`.
    pub catchup: bool,
    /// The best channel showing it (favourite, unlocked, best quality first).
    pub channel: ChannelOut,
    /// Other channels with the same programme (each `ref` is that channel's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_on: Vec<ChannelOut>,
    /// Guide categories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// The guide marks it live / new / shown before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_live: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_new: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
    /// The guide's age rating ("15", "PG").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    /// Catch-up: when the archive drops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catchup_until: Option<String>,
    /// Live: minutes left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_min: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressOut {
    pub position_min: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_min: Option<u32>,
    pub finished: bool,
    /// When it was last watched ([`fmt_time`]).
    pub watched: String,
}

/// A movie or series (one per title: copies in several playlists are one entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleOut {
    /// The copy that would play (stack top).
    #[serde(rename = "ref")]
    pub item: String,
    pub kind: Kind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    /// 0–10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_min: Option<u32>,
    pub playlist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Copies in other playlists (0 = only this one).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub other_copies: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressOut>,
    pub favourite: bool,
    pub locked: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// `search` / `browse` / `favourites` rows: `{"kind":"channel",…}` or a [`TitleOut`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemOut {
    Channel(ChannelItemOut),
    Title(TitleOut),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelItemOut {
    /// Always [`Kind::Channel`].
    pub kind: Kind,
    #[serde(flatten)]
    pub channel: ChannelOut,
    /// Title of the programme on now, when the guide knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideSearchOut {
    pub programmes: Vec<ProgrammeOut>,
    /// More matched than `limit`.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhatsOnOut {
    pub at: String,
    pub programmes: Vec<ProgrammeOut>,
    /// Not every channel was looked at (the scan is capped; narrow with `channel` / `group`).
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchOut {
    pub results: Vec<ItemOut>,
    /// `explain_filtered`: matching titles one of the user's own settings hides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filtered: Vec<FilteredOut>,
}

/// A title the user's own settings hide (`search explain_filtered`); never one parental
/// control hides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilteredOut {
    pub name: String,
    pub kind: Kind,
    pub playlist: String,
    /// `hidden_category` (with `category`), `category_filter` (with `category`),
    /// `hidden_by_you`, `not_in_all`, `title_language`.
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeasonOut {
    pub number: u32,
    pub episodes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressOut>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DetailsOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub kind: String,
    pub name: String,
    /// While the session is locked a locked item shows only its name and `locked`.
    pub locked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seasons: Vec<SeasonOut>,
    /// Series: what `play` would start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_episode: Option<EpisodeOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressOut>,
    /// Programme: the channel; channel: itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelOut>,
    /// Channel: now, next and a few upcoming; programme: itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<ProgrammeOut>,
    /// Programme: description and guide categories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// Programme numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// Programme: the guide's age rating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    /// Channel / programme: the channel and the same channel in other playlists; movie /
    /// series: its copies. The best first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<CopyOut>,
    /// Details are still being fetched from the provider: ask again in a moment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_pending: bool,
}

/// One copy of a channel or title (`details copies`): its `ref` plays that copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub playlist: String,
    /// Measured or labelled quality (`SD` … `4K`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Stream failures in the last 7 days.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures_7d: u32,
    /// Channels: has guide data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_guide: Option<bool>,
    /// Channels: catch-up days (0 = none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catchup_days: Option<u32>,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryOut {
    pub name: String,
    pub playlist: String,
    /// Visible items.
    pub count: u32,
    pub locked: bool,
    /// What it is about, from its name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<CategoryKind>,
    /// Its country or language tag ("GB", "lang:fr").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoriesOut {
    pub categories: Vec<CategoryOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowseOut {
    pub items: Vec<ItemOut>,
    /// Matches before `limit` / `offset`.
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContinueOut {
    /// Movies and series (with `progress`; series name their next episode).
    pub items: Vec<ContinueItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContinueItem {
    #[serde(flatten)]
    pub title: TitleOut,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_episode: Option<EpisodeOut>,
    /// Minutes left of the movie, or of the episode in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_min: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavListOut {
    pub name: String,
    pub items: Vec<ItemOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavouritesOut {
    pub lists: Vec<FavListOut>,
    /// Films and series live in Watchlists (DESIGN §42 §4.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_watchlist: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStateOut {
    Idle,
    Loading,
    Playing,
    Paused,
    Buffering,
    Reconnecting,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlayingOut {
    /// Channel / movie / episode ref (`None` for a file or URL).
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub title: String,
    /// Programme on now (live) or `S02E05 · Title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// `live`, `catchup`, `movie`, `episode`, `file`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchOut {
    /// For `cancel_switch`.
    pub id: i64,
    pub channel: ChannelOut,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub start: String,
    /// When Tuner shows the "Switching in 10 s · Cancel" toast.
    pub warn_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusOut {
    pub state: PlayerStateOut,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_playing: Option<NowPlayingOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    /// The error kind (`network`, `http-forbidden`, …), never the provider's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A parental PIN is set and the session is locked (locked items need it on screen).
    pub parental_locked: bool,
    /// Pending automatic switches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled: Vec<SwitchOut>,
    /// Up next, while it has items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueStatusOut>,
    /// The list following Up next (DESIGN §42 §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchlist: Option<WatchlistStatusOut>,
    /// Multi-view, while two pictures are on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiview: Option<MultiviewStatusOut>,
    /// Streams per playlist: the account's limit and the pictures using it now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<StreamsOut>,
    /// The sleep timer, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_timer: Option<SleepOut>,
    /// The picture plays on a TV, or a cast just ended in a failure the user may not have seen
    /// (DESIGN §40 §12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub casting: Option<CastingOut>,
    /// Tuner's Play on switch names a TV (DESIGN §40 §3.9): every play goes there, a `play`
    /// without `device` too (`opened_in: "device"`). Absent: plays are on this computer (always
    /// with two pictures: multi-view is this computer's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_on: Option<PlayOnOut>,
}

/// `status.play_on`: the TV plays go to. No address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayOnOut {
    /// The TV's `cast_devices` ref.
    #[serde(rename = "ref")]
    pub device_ref: String,
    /// The TV's name (sanitised).
    pub device: String,
}

/// Where a cast is (`status.casting`). No address: the TV is named and referred to by its
/// `device:` ref.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastingOut {
    /// The TV's `cast_devices` ref.
    #[serde(rename = "ref")]
    pub device_ref: String,
    /// The TV's name (sanitised).
    pub device: String,
    /// `apple_tv`, `airplay_tv`, `chromecast` or `dlna` (`act::kind_name`).
    pub kind: String,
    pub phase: CastPhase,
    /// Live: roughly how far the TV runs behind the live point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_live_s: Option<u16>,
    /// The typed reason of the failure (`cast-firewall`, `cast-stopped-on-tv`, …), never a
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `cast_devices`: the TVs Tuner knows (DESIGN §40 §12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastDevicesOut {
    pub devices: Vec<CastDeviceOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastDeviceOut {
    /// `device:<32 hex>`; `play {device}` takes it.
    #[serde(rename = "ref")]
    pub device_ref: String,
    pub name: String,
    /// `apple_tv`, `airplay_tv`, `chromecast` or `dlna` (`act::kind_name`).
    pub kind: String,
    /// Ready to cast: paired, or a TV that needs no pairing. `false`: pair it in Tuner first.
    pub paired: bool,
    /// Seen on the network by the latest discovery (else remembered).
    pub available: bool,
    /// Tuner casts to it now.
    pub casting: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchlistStatusOut {
    pub name: String,
    pub position: u32,
    pub total: u32,
    pub playing_through: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueStatusOut {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_id: Option<i64>,
    /// What plays next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<QueueNextOut>,
    /// Items still to play.
    pub left: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_end: Option<String>,
    /// Up next drives the picture now.
    pub playing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueNextOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TileOut {
    pub tile: TileName,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    pub state: PlayerStateOut,
    pub sound: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiviewStatusOut {
    pub on: bool,
    pub tiles: Vec<TileOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamsOut {
    pub playlist: String,
    /// The account's connection limit, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Tuner's pictures using it now.
    pub in_use: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SleepOut {
    /// `at`, `end_of_programme`, `after_items` or `end_of_queue`.
    pub when: String,
    /// `stop` or `pause`.
    pub then: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_left: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayOutcome {
    /// Started (live, from saved progress, or from the start).
    Playing,
    /// A past (or restarted) programme from the archive.
    CatchUp,
    /// Tuner is showing the parental PIN prompt; it plays once the user types it there.
    PinRequired,
    /// `device`: Tuner is connecting to the TV (it can take longer than a request may); `status`
    /// says how it went.
    CastingStarted,
    /// `device`: the TV isn't paired; the user does it in Tuner (Settings ▸ Cast). Nothing was
    /// sent.
    PairingRequired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayOut {
    pub outcome: PlayOutcome,
    /// What plays (for a series: the episode).
    #[serde(rename = "ref")]
    pub item: String,
    pub title: String,
    /// Movies / episodes: where it resumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at_min: Option<u32>,
    /// One sentence for the user.
    pub message: String,
    /// Where it plays (DESIGN §30.13; the handler fills it in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_in: Option<OpenedIn>,
    /// `play {device}`: the TV's name (DESIGN §40 §12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// Where an agent's play shows (`PlayOut::opened_in`, `queue play`'s `opened_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenedIn {
    /// The pop-out: the small player pinned on every workspace.
    Popout,
    /// Tuner's window.
    Window,
    /// A TV: Tuner's window stayed where it is (DESIGN §40 §3.8).
    Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOut {
    pub cancelled: bool,
}

// ------------------------------------------------------------------ refs

/// What tools hand out and take back: a stable catalogue reference with its kind, or a
/// programme as (channel, start). Strings (`Display` / `FromStr`):
///
/// * `channel:2:xt:live:1234`, `movie:3:m3u:0123456789abcdef`, `series:2:xt:series:9`,
///   `episode:2:xt:ep:77` — `<kind>:<source id>:<item key>` (the key may contain `:`).
/// * `programme:2:xt:live:1234@1791748800` — the channel's `<source id>:<item key>` and the
///   programme's start as shown on that channel (UTC seconds, tvg-shift applied). Stable across
///   EPG re-imports, unlike `programme.id`.
/// * A bare `2:xt:live:1234` (what `--play` takes) reads as a channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentRef {
    Item(ItemRef),
    Programme { channel: ItemRef, start: Timestamp },
}

impl AgentRef {
    pub fn programme(channel: ItemRef, start: Timestamp) -> Self {
        AgentRef::Programme { channel, start }
    }
}

impl fmt::Display for AgentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentRef::Item(r) => write!(f, "{}:{}:{}", r.kind.as_str(), r.source.0, r.key),
            AgentRef::Programme { channel, start } => write!(f, "programme:{}:{}@{start}", channel.source.0, channel.key),
        }
    }
}

impl FromStr for AgentRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("not a Tuner ref: {s:?} (pass back a `ref` a tool returned)");
        let s = s.trim();
        if s.len() > MAX_TEXT {
            return Err(bad());
        }
        let source_key = |rest: &str, kind: ItemKind| -> Result<ItemRef, String> {
            let (source, key) = rest.split_once(':').ok_or_else(bad)?;
            let source = source.parse::<i64>().map_err(|_| bad())?;
            if key.is_empty() || key.chars().any(char::is_whitespace) {
                return Err(bad());
            }
            Ok(ItemRef { source: SourceId(source), kind, key: ItemKey(key.to_owned()) })
        };
        let (head, rest) = s.split_once(':').ok_or_else(bad)?;
        if head == "programme" {
            let (chan, start) = rest.rsplit_once('@').ok_or_else(bad)?;
            let start = start.parse::<Timestamp>().map_err(|_| bad())?;
            return Ok(AgentRef::Programme { channel: source_key(chan, ItemKind::Channel)?, start });
        }
        if let Some(kind) = ItemKind::parse(head) {
            return source_key(rest, kind).map(AgentRef::Item);
        }
        // `--play` form: `<source>:xt:live:<id>` / `<source>:m3u:<hash>`
        if head.parse::<i64>().is_ok() && (rest.starts_with("xt:live:") || rest.starts_with("m3u:")) {
            return source_key(s, ItemKind::Channel).map(AgentRef::Item);
        }
        Err(bad())
    }
}

/// `AgentRef::Item(r).to_string()`.
pub fn item_ref(r: &ItemRef) -> String {
    AgentRef::Item(r.clone()).to_string()
}

// ------------------------------------------------------------------ times

/// A time parameter → UTC seconds. Accepted (local = the system time zone):
/// `now`; unix seconds; RFC 3339 with offset (`2026-10-11T21:00:00+01:00`, `…Z`); local
/// `2026-10-11T21:00` / `2026-10-11 21:00`; local `21:00` / `9:30` = that time today.
pub fn parse_when(s: &str, now: Timestamp) -> Result<Timestamp, String> {
    parse_when_in(s, now, &jiff::tz::TimeZone::system())
}

/// [`parse_when`] in `tz` (tests).
pub fn parse_when_in(s: &str, now: Timestamp, tz: &jiff::tz::TimeZone) -> Result<Timestamp, String> {
    let s = s.trim();
    let bad = || format!("unreadable time {s:?}: use HH:MM (today), YYYY-MM-DDTHH:MM (local) or RFC 3339");
    if s.len() > MAX_TEXT {
        return Err(bad());
    }
    if s.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<Timestamp>().map_err(|_| bad());
    }
    if let Ok(t) = jiff::Timestamp::from_str(s) {
        return Ok(t.as_second());
    }
    if let Ok(dt) = jiff::civil::DateTime::from_str(s) {
        return dt.to_zoned(tz.clone()).map(|z| z.timestamp().as_second()).map_err(|_| bad());
    }
    let padded = if s.len() == 4 && s.as_bytes()[1] == b':' { format!("0{s}") } else { s.to_owned() };
    if let Ok(t) = jiff::civil::Time::from_str(&padded) {
        let today = jiff::Timestamp::from_second(now).map_err(|_| bad())?.to_zoned(tz.clone()).date();
        return today.to_datetime(t).to_zoned(tz.clone()).map(|z| z.timestamp().as_second()).map_err(|_| bad());
    }
    Err(bad())
}

/// UTC seconds → local `2026-10-11T21:00+01:00` (what every result carries).
pub fn fmt_time(t: Timestamp) -> String {
    fmt_time_in(t, &jiff::tz::TimeZone::system())
}

/// [`fmt_time`] in `tz` (tests).
pub fn fmt_time_in(t: Timestamp, tz: &jiff::tz::TimeZone) -> String {
    match jiff::Timestamp::from_second(t) {
        Ok(ts) => ts.to_zoned(tz.clone()).strftime("%Y-%m-%dT%H:%M%:z").to_string(),
        Err(_) => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_on_the_wire() {
        let req = Request::new(7, Call::GuideSearch(GuideSearch { query: Some("touring car".into()), limit: Some(5), ..Default::default() }));
        let line = serde_json::to_string(&req).unwrap();
        assert_eq!(line, r#"{"v":2,"id":7,"method":"guide_search","params":{"query":"touring car","limit":5}}"#);
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        assert_eq!(Request::peek_version(&line), Some(2));
        assert_eq!(Request::peek_version(r#"{"view":"guide","play":null,"inputs":[]}"#), None);
        assert_eq!(Request::peek_id(r#"{"v":2,"id":9,"method":"nope"}"#), 9);

        let status = serde_json::from_str::<Request>(r#"{"v":2,"id":1,"method":"status","params":{}}"#).unwrap();
        assert_eq!(status.call, Call::Status(Empty {}));
        // unknown params are refused, so a schema typo can't be silently ignored
        assert!(serde_json::from_str::<Request>(r#"{"v":2,"id":1,"method":"search","params":{"query":"x","colour":1}}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"v":2,"id":1,"method":"launch_missiles","params":{}}"#).is_err());

        // the client's name rides at the top level, beside (not in) the params; an unknown
        // top-level key is ignored, which is what lets a 0.2.1 Tuner read a newer bridge's lines
        let named = req.clone().with_agent(Some(ClientInfo { name: "codex-mcp-client".into(), title: Some("Codex".into()) }));
        let line = serde_json::to_string(&named).unwrap();
        assert!(line.starts_with(r#"{"v":2,"id":7,"agent":{"name":"codex-mcp-client","title":"Codex"},"method""#), "{line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), named);
        assert_eq!(named.agent_name().name(), "Codex");
        let newer = serde_json::from_str::<Request>(r#"{"v":2,"id":1,"agent":{"name":"x"},"later":true,"method":"status","params":{}}"#).unwrap();
        assert_eq!(newer.call, Call::Status(Empty {}));
        // a malformed `agent` only loses the name, never the call
        for agent in [r#"{"title":"Codex"}"#, "{}", "null", r#""Codex""#, r#"{"name":5}"#] {
            let line = format!(r#"{{"v":2,"id":1,"agent":{agent},"method":"status","params":{{}}}}"#);
            let req = serde_json::from_str::<Request>(&line).unwrap_or_else(|e| panic!("{agent}: {e}"));
            assert_eq!(req.call, Call::Status(Empty {}), "{agent}");
        }
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"v":2,"id":1,"agent":{"title":"Codex"},"method":"status","params":{}}"#).unwrap().agent_name().name(),
            "Codex"
        );
    }

    #[test]
    fn tool_arguments_are_the_params() {
        let call = Call::from_tool("play", serde_json::json!({"ref": "channel:2:xt:live:1", "from_start": true})).unwrap();
        // an older client: no open_in
        assert_eq!(call, Call::Play(Play { item: "channel:2:xt:live:1".into(), from_start: Some(true), open_in: None, device: None }));
        assert!(call.is_action() && call.name() == "play");
        let tv = Call::from_tool("play", serde_json::json!({"ref": "movie:2:xt:vod:1", "open_in": "full_window"})).unwrap();
        assert_eq!(tv, Call::Play(Play { item: "movie:2:xt:vod:1".into(), from_start: None, open_in: Some("full_window".into()), device: None }));
        let cast =
            Call::from_tool("play", serde_json::json!({"ref": "movie:2:xt:vod:1", "device": "device:00112233445566778899aabbccddeeff"})).unwrap();
        assert_eq!(cast.name(), "play");
        assert!(matches!(&cast, Call::Play(p) if p.device.as_deref() == Some("device:00112233445566778899aabbccddeeff")));
        assert!(Call::from_tool("browse", serde_json::json!({"kind": "film"})).is_err());
        assert!(Call::from_tool("nope", serde_json::json!({})).is_err());
    }

    #[test]
    fn replies_carry_a_result_or_an_error() {
        let ok = Reply::of(3, &CancelOut { cancelled: true });
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"v":2,"id":3,"ok":true,"result":{"cancelled":true}}"#);
        let err = Reply::err(4, ErrorCode::NotStarted, "starts at 21:00");
        let s = serde_json::to_string(&err).unwrap();
        assert_eq!(s, r#"{"v":2,"id":4,"ok":false,"error":{"code":"not_started","message":"starts at 21:00"}}"#);
        assert_eq!(serde_json::from_str::<Reply>(&s).unwrap(), err);
    }

    #[test]
    fn refs_round_trip() {
        let ch = ItemRef { source: SourceId(2), kind: ItemKind::Channel, key: ItemKey("xt:live:1234".into()) };
        let movie = ItemRef { source: SourceId(3), kind: ItemKind::Movie, key: ItemKey("m3u:0123456789abcdef#2".into()) };
        for r in [AgentRef::Item(ch.clone()), AgentRef::Item(movie), AgentRef::programme(ch.clone(), 1_791_748_800)] {
            assert_eq!(r.to_string().parse::<AgentRef>().unwrap(), r, "{r}");
        }
        assert_eq!(AgentRef::programme(ch.clone(), 5).to_string(), "programme:2:xt:live:1234@5");
        assert_eq!("2:xt:live:1234".parse::<AgentRef>().unwrap(), AgentRef::Item(ch));
        for bad in ["", "movie", "movie:x:y", "programme:2:xt:live:1", "programme:2:xt:live:1@soon", "2:xt:vod:1", "channel:2:", "http://x/y"] {
            assert!(bad.parse::<AgentRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn times_parse_in_the_local_zone_and_format_with_the_offset() {
        let tz = jiff::tz::TimeZone::get("Europe/London").unwrap();
        // 2026-10-11 12:00 BST
        let now = parse_when_in("2026-10-11T12:00:00+01:00", 0, &tz).unwrap();
        let nine_pm = parse_when_in("2026-10-11T21:00+01:00", now, &tz).unwrap();
        assert_eq!(parse_when_in("21:00", now, &tz), Ok(nine_pm));
        assert_eq!(parse_when_in("2026-10-11T21:00", now, &tz), Ok(nine_pm));
        assert_eq!(parse_when_in("2026-10-11 21:00", now, &tz), Ok(nine_pm));
        assert_eq!(parse_when_in("2026-10-11T20:00:00Z", now, &tz), Ok(nine_pm));
        assert_eq!(parse_when_in(&nine_pm.to_string(), now, &tz), Ok(nine_pm));
        assert_eq!(parse_when_in("9:30", now, &tz), Ok(nine_pm - 11 * 3600 - 1800));
        assert_eq!(parse_when_in("now", now, &tz), Ok(now));
        assert!(parse_when_in("tonight", now, &tz).is_err());
        assert_eq!(fmt_time_in(nine_pm, &tz), "2026-10-11T21:00+01:00");
    }
}
