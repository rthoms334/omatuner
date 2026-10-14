//! `tuner mcp`: the MCP server over stdio (DESIGN §30.2). Owner: MCP-bridge track.
//!
//! Hand-rolled JSON-RPC 2.0, one message per line (MCP stdio transport): `initialize` (protocol
//! version negotiation over `tools::PROTOCOL_VERSIONS`, `capabilities: {tools: {listChanged:
//! false}}`, `serverInfo {name: "tuner", version}`, `instructions`), `notifications/initialized`
//! and other notifications (ignored), `ping`, `tools/list`, `tools/call`. Anything else with an
//! id → `-32601`; unparsable line → `-32700` (id null); a request line over 1 MiB is refused
//! (`-32600`) without being buffered whole. Requests are handled in order.
//!
//! `tools/call` → `proto::Call::from_tool(name, arguments)` (bad arguments → a tool error
//! result, not a JSON-RPC error, so the model can correct itself) → `client::Client::call` →
//! result `{content: [{type: "text", text: <result JSON>}], structuredContent: <result>,
//! isError: false}`; an error reply or a `ClientError` → `{content: [{type: "text", text:
//! "<code>: <message>"}], isError: true}`. An unknown tool name is a JSON-RPC `-32602`, as the
//! MCP spec shows it.
//!
//! `initialize` records the client's `clientInfo` (name, title), sent with every call so Tuner can
//! name the agent that made a change (DESIGN §30.12).
//!
//! stdout carries only protocol lines; logging goes to stderr (MCP clients such as Claude Code
//! keep it in their MCP log) at WARN, or DEBUG with `--debug` — never to `tuner.log`, which the
//! running app owns.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use super::client::{Client, ClientError, ExeLauncher, Launch};
use super::proto::{Call, ClientInfo, ErrorCode};
use super::tools::{self, PROTOCOL_VERSIONS};

/// Largest JSON-RPC message line read from the client.
pub const MAX_MESSAGE: usize = 1 << 20;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Longest client name or title passed on (Tuner sanitises it again and keeps 32 chars).
const MAX_CLIENT_TEXT: usize = 64;

/// What one stdio session knows about its client.
#[derive(Debug, Default)]
struct Session {
    /// `initialize.clientInfo`; `None` before it, or when it was malformed.
    agent: Option<ClientInfo>,
    /// Tuner's `config.toml`, read for "Agents play in" when a play has to start Tuner
    /// (`None`: the default, the pop-out).
    config: Option<std::path::PathBuf>,
}

/// `tuner mcp [--profile-dir DIR] [--debug]`: serve stdin/stdout until stdin closes.
pub fn main(paths: &crate::paths::AppPaths, profile_dir: Option<&std::path::Path>, debug: bool) -> anyhow::Result<()> {
    init_stderr_logging(debug);
    // the launched Tuner gets the same profile; make it absolute in case its cwd differs
    let profile_dir = profile_dir.map(std::path::absolute).transpose()?;
    // `TUNER_MCP_NO_LAUNCH=1`: never start Tuner (tests driving the real binary against a
    // headless instance must never open a window if that instance went away)
    let launcher: Box<dyn super::client::Launcher> = if std::env::var_os("TUNER_MCP_NO_LAUNCH").is_some_and(|v| v == "1") {
        Box::new(NoLaunch)
    } else {
        Box::new(ExeLauncher::current(profile_dir)?)
    };
    let client = Client::new(&paths.ipc_socket(), launcher);
    tracing::debug!("mcp: serving on stdio, socket {}", paths.ipc_socket().display());
    serve_with(std::io::stdin().lock(), std::io::stdout().lock(), &client, Some(paths.config_file()))?;
    tracing::debug!("mcp: stdin closed");
    Ok(())
}

/// The launcher of `TUNER_MCP_NO_LAUNCH=1`.
struct NoLaunch;

impl super::client::Launcher for NoLaunch {
    fn launch(&self) -> std::io::Result<()> {
        Err(std::io::Error::other("starting Tuner is turned off (TUNER_MCP_NO_LAUNCH)"))
    }
}

/// stderr only (no ANSI, no `tuner.log`); `RUST_LOG` overrides.
fn init_stderr_logging(debug: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(if debug { "warn,tuner=debug" } else { "warn" }));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
}

/// The protocol loop over any reader/writer (tests drive it with in-memory pipes).
pub fn serve(input: impl BufRead, output: impl Write, client: &Client) -> std::io::Result<()> {
    serve_with(input, output, client, None)
}

/// [`serve`], reading "Agents play in" from `config` when a play starts Tuner.
pub fn serve_with(mut input: impl BufRead, mut output: impl Write, client: &Client, config: Option<std::path::PathBuf>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut session = Session { config, ..Default::default() };
    loop {
        let line = match read_message(&mut input, &mut buf)? {
            None => return Ok(()),
            Some(Line::TooLong) => {
                tracing::warn!("mcp: refused a message over {MAX_MESSAGE} bytes");
                Some(error(Value::Null, INVALID_REQUEST, "message too large"))
            }
            Some(Line::Text) => handle_line(&buf, client, &mut session),
        };
        if let Some(reply) = line {
            let mut out = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
            out.push(b'\n');
            output.write_all(&out)?;
            output.flush()?;
        }
    }
}

enum Line {
    /// `buf` holds the line (without its newline).
    Text,
    /// Over [`MAX_MESSAGE`]; the rest was discarded up to its newline.
    TooLong,
}

/// The next line into `buf`, never holding more than [`MAX_MESSAGE`] bytes of it. `None`: end of
/// input.
fn read_message(input: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Option<Line>> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = match input.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            // EOF: a last line without a newline still counts
            return Ok(any.then_some(if too_long { Line::TooLong } else { Line::Text }));
        }
        any = true;
        let (part, done) = match memchr::memchr(b'\n', chunk) {
            Some(i) => (&chunk[..i], Some(i + 1)),
            None => (chunk, None),
        };
        if !too_long {
            if buf.len() + part.len() > MAX_MESSAGE {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(part);
            }
        }
        let used = done.unwrap_or(chunk.len());
        input.consume(used);
        if done.is_some() {
            return Ok(Some(if too_long { Line::TooLong } else { Line::Text }));
        }
    }
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// One message → its response (`None` for notifications, responses and blank lines).
fn handle_line(line: &[u8], client: &Client, session: &mut Session) -> Option<Value> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let msg: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(_) => return Some(error(Value::Null, PARSE_ERROR, "parse error")),
    };
    let Value::Object(msg) = msg else {
        // batches (2025-03-26 only, removed since) and scalars
        return Some(error(Value::Null, INVALID_REQUEST, "expected one JSON-RPC message object"));
    };
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // a response to a request we never send, or junk: answer only what has an id
        if msg.contains_key("result") || msg.contains_key("error") {
            return None;
        }
        return id.map(|id| error(id, INVALID_REQUEST, "missing method"));
    };
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return id.map(|id| error(id, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
    }
    // notifications (initialized, cancelled, roots/list_changed…) need no answer
    let id = id?;
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(error(Value::Null, INVALID_REQUEST, "id must be a string or an integer"));
    }
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => response(id, initialize(&params, session)),
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({ "tools": tools::tools().iter().map(tools::ToolDef::to_json).collect::<Vec<_>>() })),
        "tools/call" => match tools_call(&params, client, session) {
            Ok(result) => response(id, result),
            Err(message) => error(id, INVALID_PARAMS, &message),
        },
        _ => error(id, METHOD_NOT_FOUND, "method not found"),
    })
}

/// `initialize`: the client's protocol version when we speak it, else our newest; records who
/// the client says it is (a second `initialize` replaces it).
fn initialize(params: &Value, session: &mut Session) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked.and_then(|v| PROTOCOL_VERSIONS.iter().find(|&&ours| ours == v)).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    let info = params.get("clientInfo");
    let text = |key: &str| info.and_then(|i| i.get(key)).and_then(Value::as_str).map(|s| s.chars().take(MAX_CLIENT_TEXT).collect::<String>());
    session.agent = text("name").map(|name| ClientInfo { name, title: text("title") });
    tracing::debug!("mcp: initialize (client asked {asked:?}, using {version}; client {:?})", session.agent);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "tuner", "title": "Tuner", "version": env!("CARGO_PKG_VERSION") },
        "instructions": tools::INSTRUCTIONS,
    })
}

/// `tools/call`: `Err` only for a malformed call or an unknown tool (JSON-RPC errors); every
/// other failure is a tool result with `isError`.
fn tools_call(params: &Value, client: &Client, session: &Session) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).ok_or("params.name must be a tool name")?;
    let tool = tools::tool(name).ok_or_else(|| format!("unknown tool: {}", name.chars().take(64).collect::<String>()))?;
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(a @ Value::Object(_)) => a.clone(),
        Some(_) => return Ok(tool_error("bad_request", "arguments must be an object")),
    };
    let call = match Call::from_tool(tool.name, arguments) {
        Ok(c) => c,
        Err(e) => return Ok(tool_error("bad_request", &crate::redact::redact(&e))),
    };
    tracing::debug!("mcp: tools/call {}", tool.name);
    let hyprland = crate::platform::hypr::is_hyprland() && !crate::platform::off_desktop();
    let setting = || session.config.as_deref().and_then(|p| crate::config::Config::load(p).ok()).map(|c| c.agents.play_in).unwrap_or_default();
    let launch = launch_for(&tool, &call, hyprland, setting);
    let plain = without_open_in(&call);
    let casts = matches!(&call, Call::Play(p) if p.device.is_some());
    let mut reply = client.send_as(call, launch, session.agent.clone());
    // a Tuner from before casting refuses `device`: never play it here instead (DESIGN §40 §12)
    if casts
        && let Ok(sent) = &reply
        && cannot_cast(&sent.reply)
    {
        tracing::info!("mcp: {}: the running Tuner doesn't know device; not playing it here", tool.name);
        return Ok(tool_error("unsupported", CANNOT_CAST));
    }
    // a Tuner from before `open_in` (0.2.4, still running after an upgrade) refuses the field:
    // ask again without it, as that Tuner's own play
    let mut older = false;
    if let (Ok(first), Some(plain)) = (&reply, plain)
        && first.reply.error.as_ref().is_some_and(|e| e.code == ErrorCode::BadRequest && e.message.contains("open_in"))
    {
        tracing::info!("mcp: {}: the running Tuner doesn't know open_in; asking again without it", tool.name);
        let started = first.started;
        reply = client.send_as(plain, Launch::No, session.agent.clone()).map(|mut s| {
            s.started |= started;
            s
        });
        older = true;
    }
    Ok(match reply {
        Ok(sent) if sent.reply.ok => {
            let mut result = sent.reply.result.unwrap_or_else(|| json!({}));
            if !result.is_object() {
                result = json!({ "result": result });
            }
            if older && let Some(obj) = result.as_object_mut() {
                obj.insert("note".into(), Value::String(OLDER_TUNER.into()));
            }
            if tool.name == "status"
                && let Some(obj) = result.as_object_mut()
            {
                obj.entry("running").or_insert(Value::Bool(true));
            }
            // the server checks this already (§30.6); a second look costs nothing
            if result.to_string().contains("://") {
                tracing::warn!("mcp: {} → a result with an address was withheld", tool.name);
                return Ok(tool_error("internal", "Tuner's answer was withheld (it contained an address)"));
            }
            tool_result(result, sent.started)
        }
        Ok(sent) => {
            let (code, message) = match sent.reply.error {
                Some(e) => (error_code(e.code), e.message),
                None => ("internal", "Tuner reported an error without a message".to_owned()),
            };
            tracing::debug!("mcp: {} → {code}", tool.name);
            tool_error(code, &message)
        }
        Err(ClientError::NotRunning) if tool.name == "status" => tool_result(json!({ "running": false }), false),
        Err(e) => {
            tracing::warn!("mcp: {} → {}", tool.name, e.code());
            let mut message = e.to_string();
            if e == ClientError::NotRunning {
                message.push_str(" (this tool doesn't start it; ask for something to watch and Tuner opens)");
            }
            tool_error(e.code(), &message)
        }
    })
}

/// The note on a play a Tuner from before `open_in` made (`tools_call`).
const OLDER_TUNER: &str = "The running Tuner is older than this bridge: it played this its own way (in its window), without open_in. \
Restart Tuner to update it.";

/// What an agent is told when the running Tuner is older than casting.
const CANNOT_CAST: &str = "The running Tuner can't cast; update Tuner";

/// A Tuner from before casting refuses `play`'s `device` as an unknown field (it never reaches a
/// handler that could answer): unlike `open_in`, the call is not asked again without it, because
/// playing on this computer instead of the TV would be wrong.
fn cannot_cast(reply: &super::proto::Reply) -> bool {
    reply.error.as_ref().is_some_and(|e| e.code == ErrorCode::BadRequest && e.message.contains("unknown field `device`"))
}

/// `call` without its `open_in`, when it has one (`play`, `queue`): what a Tuner from before
/// the field accepts.
fn without_open_in(call: &Call) -> Option<Call> {
    match call {
        Call::Play(p) if p.open_in.is_some() => Some(Call::Play(super::proto::Play { open_in: None, ..p.clone() })),
        Call::Queue(q) if q.open_in.is_some() => Some(Call::Queue(super::params::Queue { open_in: None, ..q.clone() })),
        _ => None,
    }
}

/// How a call starts Tuner when it isn't running: a play that opens in the pop-out (DESIGN
/// §30.13) starts it there, pinned and without the keyboard (like a switch reminder), the rest
/// in its window. Unasked, a play starts the window too: Hyprland opens it on the workspace
/// the agent's terminal shows, i.e. on screen, where the play fills it; only `open_in:
/// popout` starts the pop-out; a play with a `device` always starts the window (the picture is on
/// the TV, and the pop-out would be an empty player pinned on every workspace).
/// `setting`: "Agents play in", read only for plays.
fn launch_for(tool: &tools::ToolDef, call: &Call, hyprland: bool, setting: impl FnOnce() -> crate::model::OpenIn) -> Launch {
    use super::params::QueueAction;
    if !tool.launches {
        return Launch::No;
    }
    let asked = match call {
        // a cast is the window, never the pop-out: the picture is on the TV (DESIGN §40 §12)
        Call::Play(p) if p.device.is_some() => return Launch::Window,
        Call::Play(p) => p.open_in.as_deref(),
        Call::Queue(q) if matches!(q.action, QueueAction::Play | QueueAction::Skip) => q.open_in.as_deref(),
        Call::Watchlist(p) if p.action == super::params::WatchlistAction::Play => p.open_in.as_deref(),
        _ => return Launch::Window,
    };
    // a bad open_in is refused by Tuner anyway
    let Ok(asked) = super::act::parse_open_in(asked) else { return Launch::Window };
    // a Tuner that isn't running opens its window on the current workspace: on screen
    let w = crate::reminders::ui::WindowNow { can_pop: hyprland, visible: true, ..Default::default() };
    match super::act::agent_show(asked, setting(), w, false).place {
        crate::reminders::ui::Placement::PopOut => Launch::PopOut,
        _ => Launch::Window,
    }
}

/// The `ErrorCode` as its wire name (`not_found`, …).
fn error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::NotStarted => "not_started",
        ErrorCode::Loading => "loading",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::Busy => "busy",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Internal => "internal",
    }
}

fn tool_result(result: Value, started: bool) -> Value {
    let mut content = vec![json!({ "type": "text", "text": result.to_string() })];
    if started {
        content.push(json!({ "type": "text", "text": "Tuner wasn't running, so it was started." }));
    }
    json!({ "content": content, "structuredContent": result, "isError": false })
}

fn tool_error(code: &str, message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": format!("{code}: {message}") }], "isError": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_match_their_wire_names() {
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::NotFound,
            ErrorCode::Unavailable,
            ErrorCode::NotStarted,
            ErrorCode::Loading,
            ErrorCode::RateLimited,
            ErrorCode::Busy,
            ErrorCode::Unsupported,
            ErrorCode::Internal,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), error_code(code));
        }
    }

    /// A play asked for in the pop-out starts Tuner there (DESIGN §30.13); an unasked play
    /// (its window opens on the terminal's workspace, on screen: the play fills it), a play for
    /// the window, the setting "Full window", no Hyprland or any other tool start its window;
    /// `status` never starts it.
    #[test]
    fn plays_for_the_pop_out_start_tuner_there() {
        use crate::model::OpenIn;
        let launch = |name: &str, args: Value, hyprland: bool, setting: OpenIn| {
            let tool = tools::tool(name).unwrap();
            launch_for(&tool, &Call::from_tool(name, args).unwrap(), hyprland, || setting)
        };
        let film = || json!({ "ref": "movie:2:xt:vod:1" });
        assert_eq!(launch("play", film(), true, OpenIn::PopOut), Launch::Window, "unasked: the window, on screen");
        assert_eq!(launch("play", json!({ "ref": "movie:2:xt:vod:1", "open_in": "full_window" }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("play", json!({ "ref": "movie:2:xt:vod:1", "open_in": "popout" }), true, OpenIn::FullWindow), Launch::PopOut);
        assert_eq!(launch("play", film(), true, OpenIn::FullWindow), Launch::Window);
        assert_eq!(launch("play", film(), false, OpenIn::PopOut), Launch::Window, "no Hyprland: no pop-out");
        assert_eq!(launch("queue", json!({ "action": "play" }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("queue", json!({ "action": "play", "open_in": "popout" }), true, OpenIn::PopOut), Launch::PopOut);
        assert_eq!(launch("queue", json!({ "action": "add", "refs": ["movie:2:xt:vod:1"] }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("watchlist", json!({ "action": "play", "list": "My list", "open_in": "popout" }), true, OpenIn::PopOut), Launch::PopOut);
        assert_eq!(launch("watchlist", json!({ "action": "next", "list": "My list" }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("search", json!({ "query": "dune" }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("status", json!({}), true, OpenIn::PopOut), Launch::No);
        // a cast never starts the pop-out, whatever is asked or set
        let tv = "device:00112233445566778899aabbccddeeff";
        assert_eq!(launch("play", json!({ "ref": "movie:2:xt:vod:1", "device": tv }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("play", json!({ "ref": "movie:2:xt:vod:1", "device": tv, "open_in": "popout" }), true, OpenIn::PopOut), Launch::Window);
        assert_eq!(launch("cast_devices", json!({}), true, OpenIn::PopOut), Launch::Window);
    }

    /// A Tuner from before casting says `unknown field `device``: the bridge tells the agent to
    /// update it, and does not play the item here. Any other refusal is passed on as it is.
    #[test]
    fn an_older_tuner_is_not_asked_to_play_a_cast_here() {
        use crate::agent::proto::Reply;
        // what serde says in Tuner 0.2.5, whose `Play` knows `ref`, `from_start` and `open_in`
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct OlderPlay {
            r#ref: String,
            from_start: Option<bool>,
            open_in: Option<String>,
        }
        let says = serde_json::from_value::<OlderPlay>(json!({ "ref": "movie:2:xt:vod:1", "device": "x" })).unwrap_err().to_string();
        assert!(cannot_cast(&Reply::err(1, ErrorCode::BadRequest, crate::redact::redact(&says))), "{says}");
        assert!(!cannot_cast(&Reply::err(1, ErrorCode::BadRequest, "not a device ref: pass back a `ref` cast_devices returned")));
        assert!(!cannot_cast(&Reply::err(1, ErrorCode::NotFound, "unknown field `device`")));
        assert!(!cannot_cast(&Reply::ok(1, json!({}))));
    }

    /// Lines are read whole up to the cap; a longer one is skipped to its newline without
    /// being kept, and the next line reads normally; a last line without newline counts.
    #[test]
    fn read_message_caps_lines() {
        let big = "x".repeat(4 * MAX_MESSAGE);
        let data = format!("{{\"a\":1}}\n{big}\nnext\nlast");
        let mut input = std::io::BufReader::with_capacity(4096, data.as_bytes());
        let mut buf = Vec::new();
        assert!(matches!(read_message(&mut input, &mut buf).unwrap(), Some(Line::Text)));
        assert_eq!(buf, b"{\"a\":1}");
        assert!(matches!(read_message(&mut input, &mut buf).unwrap(), Some(Line::TooLong)));
        assert!(buf.capacity() <= 2 * MAX_MESSAGE, "the long line was not buffered whole");
        assert!(matches!(read_message(&mut input, &mut buf).unwrap(), Some(Line::Text)));
        assert_eq!(buf, b"next");
        assert!(matches!(read_message(&mut input, &mut buf).unwrap(), Some(Line::Text)));
        assert_eq!(buf, b"last");
        assert!(read_message(&mut input, &mut buf).unwrap().is_none());
    }
}
