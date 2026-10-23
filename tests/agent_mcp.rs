//! `tuner mcp` (DESIGN §30.2, §30.7; bridge track): the MCP protocol loop over in-memory pipes,
//! `tools/call` round trips through a real `IpcServer::start_with_agent` with a fake
//! `AgentService` on a temp socket, launch-if-not-running with a fake `Launcher`, loading
//! retries, old-instance detection, and the real binary speaking nothing but JSON on stdout.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tuner::agent::client::{Client, Launcher};
use tuner::agent::mcp::{self, MAX_MESSAGE};
use tuner::agent::proto::{ClientInfo, ErrorCode, Reply, Request};
use tuner::agent::server::AgentService;
use tuner::ipc::IpcServer;
use tuner::msg::EventBus;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
}

/// `<tmp>/run/tuner.sock`, like `AppPaths::rooted(<tmp>).ipc_socket()`.
fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("run").join("tuner.sock")
}

type Answer = dyn Fn(&Request, usize) -> Reply + Send + Sync;

/// An agent service answering with `answer(request, n)` (n = requests before this one) and
/// recording every request it got.
#[derive(Clone)]
struct Fake {
    answer: Arc<Answer>,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn new(answer: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static) -> Self {
        Self { answer: Arc::new(answer), seen: Arc::default() }
    }

    /// Echoes the method and params back as the result.
    fn echo() -> Self {
        Self::new(|req, _| {
            let wire = serde_json::to_value(req).unwrap();
            Reply::ok(req.id, json!({ "method": wire["method"], "params": wire["params"] }))
        })
    }

    fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

impl AgentService for Fake {
    fn handle(&self, req: Request) -> BoxFuture<'static, Reply> {
        let mut seen = self.seen.lock().unwrap();
        let reply = (self.answer)(&req, seen.len());
        seen.push(req);
        Box::pin(async move { reply })
    }
}

fn start_server(rt: &tokio::runtime::Runtime, sock: &Path, fake: &Fake) -> IpcServer {
    let (bus, _events) = EventBus::new(None);
    IpcServer::start_with_agent(rt.handle(), sock, bus, Some(Arc::new(fake.clone()))).unwrap()
}

/// A launcher that must not be used.
struct NoLaunch;
impl Launcher for NoLaunch {
    fn launch(&self) -> std::io::Result<()> {
        panic!("this call must not launch Tuner");
    }
}

/// A launcher that "starts Tuner": serves `fake` on the socket 300 ms later (or never).
struct FakeLauncher {
    rt: tokio::runtime::Handle,
    sock: PathBuf,
    fake: Option<Fake>,
    launches: Arc<AtomicUsize>,
    server: Arc<Mutex<Option<IpcServer>>>,
}

impl Launcher for FakeLauncher {
    fn launch(&self) -> std::io::Result<()> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let Some(fake) = self.fake.clone() else { return Ok(()) };
        let (rt, sock, server) = (self.rt.clone(), self.sock.clone(), self.server.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let (bus, _events) = EventBus::new(None);
            let s = IpcServer::start_with_agent(&rt, &sock, bus, Some(Arc::new(fake))).unwrap();
            *server.lock().unwrap() = Some(s);
        });
        Ok(())
    }
}

/// Feed `messages` (one per line) through `mcp::serve`; the output lines, parsed.
fn exchange(client: &Client, messages: &[Value]) -> Vec<Value> {
    let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
    exchange_raw(client, input.as_bytes())
}

fn exchange_raw(client: &Client, input: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    mcp::serve(BufReader::new(input), &mut out, client).unwrap();
    let text = String::from_utf8(out).unwrap();
    text.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))).collect()
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
}

/// One `tools/call` → its result object.
fn tool(client: &Client, name: &str, arguments: Value) -> Value {
    let out = exchange(client, &[call(1, name, arguments)]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0]["id"], 1);
    out[0]["result"].clone()
}

fn error_text(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

fn offline(dir: &tempfile::TempDir) -> Client {
    Client::new(&socket_in(dir), Box::new(NoLaunch))
}

/// `initialize` echoes a protocol version we speak (newest, middle, oldest) and offers our
/// newest for one we don't; the server describes itself and its tools capability;
/// notifications get no answer.
#[test]
fn initialize_negotiates_the_protocol_version() {
    let dir = tempfile::tempdir().unwrap();
    let client = offline(&dir);
    let init = |id: u64, v: &str| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
        "protocolVersion": v, "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } })
    };
    let out = exchange(
        &client,
        &[
            init(1, "2025-11-25"),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            init(2, "2025-06-18"),
            init(3, "2024-11-05"),
            init(4, "2099-01-01"),
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 9 } }),
        ],
    );
    assert_eq!(out.len(), 4, "notifications are not answered: {out:?}");
    let versions: Vec<_> = out.iter().map(|r| r["result"]["protocolVersion"].as_str().unwrap()).collect();
    assert_eq!(versions, ["2025-11-25", "2025-06-18", "2024-11-05", "2025-11-25"]);
    let r = &out[0]["result"];
    assert_eq!(out[0]["jsonrpc"], "2.0");
    assert_eq!(r["capabilities"], json!({ "tools": { "listChanged": false } }));
    assert_eq!(r["serverInfo"]["name"], "tuner");
    assert_eq!(r["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    let instructions = r["instructions"].as_str().unwrap();
    assert!(instructions.contains("guide_search") && instructions.contains("PIN"), "{instructions}");
}

/// `tools/list`: the tools on in this build, with MCP-shaped entries and closed object schemas.
#[test]
fn tools_list_has_every_tool() {
    let dir = tempfile::tempdir().unwrap();
    let out = exchange(&offline(&dir), &[json!({ "jsonrpc": "2.0", "id": "a", "method": "tools/list", "params": {} })]);
    assert_eq!(out[0]["id"], "a", "string ids are echoed");
    let tools = out[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "guide_search",
            "whats_on",
            "search",
            "details",
            "categories",
            "browse",
            "continue_watching",
            "favourites",
            "status",
            // the TVs to cast to (DESIGN §40 §12; next to status, before play's `device`)
            "cast_devices",
            "play",
            "schedule_switch",
            "cancel_switch",
            "control",
            // the feature tools (each listed while its feature is on)
            "downloads",
            "new_episodes",
            "queue",
            "watchlist",
            "follow",
            "spoiler_guard",
            "guide_fix",
            "multiview",
            "reminders",
            "sleep_timer",
            // the library and health tools
            "match_titles",
            "history",
            "episodes",
            "favourites_edit",
            "parental_status",
            "lock",
            "diagnose",
            "sources"
        ]
    );
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{t}");
        assert!(!t["description"].as_str().unwrap().is_empty());
        assert!(t["annotations"]["readOnlyHint"].is_boolean());
    }
}

/// JSON-RPC errors: garbage (−32700, id null), unknown method (−32601), not JSON-RPC 2.0 or a
/// batch (−32600), unknown tool (−32602); a line over 1 MiB is refused and the next one still
/// answered; responses from the client are ignored; `ping` → `{}`.
#[test]
fn protocol_errors_and_ping() {
    let dir = tempfile::tempdir().unwrap();
    let client = offline(&dir);
    let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\",\"params\":{{\"pad\":\"{}\"}}}}", "x".repeat(MAX_MESSAGE));
    let input = [
        "this is not json".to_owned(),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }).to_string(),
        json!({ "id": 2, "method": "ping" }).to_string(),
        json!([{ "jsonrpc": "2.0", "id": 3, "method": "ping" }]).to_string(),
        call(4, "record_tv", json!({})).to_string(),
        String::new(),
        big,
        json!({ "jsonrpc": "2.0", "id": 5, "result": {} }).to_string(),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "ping" }).to_string(),
    ]
    .join("\n");
    let out = exchange_raw(&client, input.as_bytes());
    let got: Vec<_> = out.iter().map(|r| (r["id"].clone(), r["error"]["code"].as_i64())).collect();
    assert_eq!(
        got,
        [
            (Value::Null, Some(-32700)),
            (json!(1), Some(-32601)),
            (json!(2), Some(-32600)),
            (Value::Null, Some(-32600)),
            (json!(4), Some(-32602)),
            (Value::Null, Some(-32600)),
            (json!(6), None),
        ],
        "{out:?}"
    );
    assert_eq!(out[6]["result"], json!({}));
}

/// A tool call goes to the running Tuner as an IPC v2 request whose params ARE the arguments,
/// and comes back as `structuredContent` + the same JSON as text; an error reply is a tool
/// error `"<code>: <message>"`; bad arguments are a tool error without any IPC request;
/// `status` says `running: true`; a result carrying an address is withheld; the client's name
/// from `initialize` goes with the session's calls.
#[test]
fn tool_calls_round_trip_through_the_ipc_socket() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let fake = Fake::new(|req, _| {
        let wire = serde_json::to_value(req).unwrap();
        match req.call.name() {
            "details" => Reply::err(req.id, ErrorCode::NotFound, "no such item"),
            "status" => Reply::ok(req.id, json!({ "state": "idle", "paused": false, "volume": 80.0, "muted": false, "parental_locked": true })),
            "whats_on" => Reply::ok(req.id, json!({ "programmes": [{ "logo": "http://h.tv/l.png" }] })),
            _ => Reply::ok(req.id, json!({ "method": wire["method"], "params": wire["params"] })),
        }
    });
    let _server = start_server(&rt, &sock, &fake);
    let client = Client::new(&sock, Box::new(NoLaunch));

    let args = json!({ "query": "touring car", "from": "now", "limit": 5 });
    let r = tool(&client, "guide_search", args.clone());
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"], json!({ "method": "guide_search", "params": args }));
    let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, r["structuredContent"]);
    assert_eq!(r["content"].as_array().unwrap().len(), 1, "no 'started' note: Tuner was running");

    // no arguments at all = {}
    let out = exchange(&client, &[json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "continue_watching" } })]);
    assert_eq!(out[0]["result"]["structuredContent"]["method"], "continue_watching");

    assert_eq!(error_text(&tool(&client, "details", json!({ "ref": "movie:3:xt:vod:1" }))), "not_found: no such item");

    let before = fake.seen().len();
    let t = error_text(&tool(&client, "search", json!({ "query": "x", "pin": "1234" }))).to_owned();
    assert!(t.starts_with("bad_request: ") && t.contains("pin"), "{t}");
    assert!(error_text(&tool(&client, "play", json!({}))).starts_with("bad_request: "));
    assert!(error_text(&tool(&client, "search", json!("dune"))).starts_with("bad_request: "));
    assert_eq!(fake.seen().len(), before, "bad arguments never reach Tuner");

    let r = tool(&client, "status", json!({}));
    assert_eq!((r["structuredContent"]["running"].as_bool(), r["structuredContent"]["parental_locked"].as_bool()), (Some(true), Some(true)));

    let t = error_text(&tool(&client, "whats_on", json!({}))).to_owned();
    assert!(t.starts_with("internal: ") && !t.contains("h.tv"), "{t}");

    let methods: Vec<_> = fake.seen().iter().map(|r| r.call.name()).collect();
    assert_eq!(methods, ["guide_search", "continue_watching", "details", "status", "whats_on"]);

    let hello = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "codex-mcp-client", "title": "Codex", "version": "1" } });
    let out = exchange(
        &client,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": hello }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "continue_watching" } }),
        ],
    );
    assert_eq!(out[1]["result"]["isError"], false, "{out:?}");
    let seen = fake.seen();
    assert!(seen[..seen.len() - 1].iter().all(|r| r.agent.is_none()), "no initialize, no name");
    assert_eq!(seen.last().unwrap().agent, Some(ClientInfo { name: "codex-mcp-client".into(), title: Some("Codex".into()) }));
}

/// Tuner not running: `status` answers `running: false` and `control` a "not running" error,
/// neither launching; any other tool launches Tuner, waits for its socket and answers (with
/// a note that Tuner was started); later calls find it running.
#[test]
fn launches_tuner_when_it_is_not_running() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let launches = Arc::new(AtomicUsize::new(0));
    let server = Arc::new(Mutex::new(None));
    let launcher =
        FakeLauncher { rt: rt.handle().clone(), sock: sock.clone(), fake: Some(Fake::echo()), launches: launches.clone(), server: server.clone() };
    let client = Client::new(&sock, Box::new(launcher));

    let r = tool(&client, "status", json!({}));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"], json!({ "running": false }));
    let t = error_text(&tool(&client, "control", json!({ "action": "pause" }))).to_owned();
    assert!(t.starts_with("not_running: "), "{t}");
    assert_eq!(launches.load(Ordering::SeqCst), 0, "status and control never launch");

    let t0 = Instant::now();
    let r = tool(&client, "search", json!({ "query": "dune" }));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["method"], "search");
    assert!(r["content"][1]["text"].as_str().unwrap().contains("started"), "{r}");
    assert!(t0.elapsed() >= Duration::from_millis(300));
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert!(server.lock().unwrap().is_some());

    let r = tool(&client, "control", json!({ "action": "volume", "value": 40 }));
    assert_eq!(r["structuredContent"]["params"], json!({ "action": "volume", "value": 40.0 }));
    assert_eq!(r["content"].as_array().unwrap().len(), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

/// A launch that never serves the socket ends with `did_not_start` after the wait (not a hang).
#[test]
fn a_tuner_that_never_answers_did_not_start() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let launches = Arc::new(AtomicUsize::new(0));
    let launcher = FakeLauncher { rt: rt.handle().clone(), sock: sock.clone(), fake: None, launches: launches.clone(), server: Arc::default() };
    let client = Client::new(&sock, Box::new(launcher)).with_waits(Duration::from_millis(500), Duration::from_secs(1));
    let t0 = Instant::now();
    let t = error_text(&tool(&client, "browse", json!({ "kind": "movie" }))).to_owned();
    assert!(t.starts_with("did_not_start: ") && t.contains("didn't answer"), "{t}");
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert_eq!(launches.load(Ordering::SeqCst), 1, "launched once, not once per poll");
}

/// `loading` replies (library not built yet) are retried until Tuner answers; one that stays
/// loading past the wait is a `loading` tool error.
#[test]
fn loading_is_retried() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let fake = Fake::new(|req, n| match n {
        0..3 => Reply::err(req.id, ErrorCode::Loading, "loading"),
        _ if req.call.name() == "favourites" => Reply::err(req.id, ErrorCode::Loading, "loading"),
        _ => Reply::ok(req.id, json!({ "items": [] })),
    });
    let _server = start_server(&rt, &sock, &fake);
    let client = Client::new(&sock, Box::new(NoLaunch)).with_waits(Duration::from_secs(1), Duration::from_millis(800));
    let r = tool(&client, "continue_watching", json!({}));
    assert_eq!(r["structuredContent"], json!({ "items": [] }), "{r}");
    assert_eq!(fake.seen().len(), 4);
    let t0 = Instant::now();
    let t = error_text(&tool(&client, "favourites", json!({}))).to_owned();
    assert!(t.starts_with("loading: "), "{t}");
    assert!(t0.elapsed() < Duration::from_secs(3));
}

/// A Tuner from before IPC v2 answers a v2 line with a v1 error: "restart Tuner".
#[test]
fn an_old_tuner_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    // private, like every Tuner's runtime directory (the bridge refuses others)
    std::fs::set_permissions(sock.parent().unwrap(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            (&stream).write_all(b"{\"ok\":false,\"error\":\"bad request: missing field `inputs`\"}\n").unwrap();
        }
    });
    let client = Client::new(&sock, Box::new(NoLaunch));
    let t = error_text(&tool(&client, "search", json!({ "query": "news" }))).to_owned();
    assert!(t.starts_with("too_old: ") && t.contains("restart Tuner"), "{t}");
}

/// A Tuner from before `open_in` (0.2.4 still running after an upgrade) refuses the field: the
/// bridge asks again without it, so "full screen" still plays (in that Tuner's window), and the
/// result says to restart Tuner (DESIGN §30.13).
#[test]
fn a_tuner_before_open_in_still_plays() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let sock = socket_in(&dir);
    let fake = Fake::new(|req, _| {
        if serde_json::to_value(req).unwrap()["params"].get("open_in").is_some() {
            return Reply::err(req.id, ErrorCode::BadRequest, "unknown field `open_in`, expected `ref` or `from_start`");
        }
        Reply::ok(req.id, json!({ "outcome": "playing", "message": "Playing BBC One." }))
    });
    let _server = start_server(&rt, &sock, &fake);
    let client = offline(&dir);
    let r = tool(&client, "play", json!({ "ref": "channel:1:m3u:bbc1", "open_in": "full_window" }));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["outcome"], "playing");
    assert!(r["structuredContent"]["note"].as_str().unwrap().contains("Restart Tuner"), "{r}");
    let seen: Vec<Value> = fake.seen().iter().map(|q| serde_json::to_value(q).unwrap()["params"].clone()).collect();
    assert_eq!(seen, [json!({ "ref": "channel:1:m3u:bbc1", "open_in": "full_window" }), json!({ "ref": "channel:1:m3u:bbc1" })]);
    // without open_in nothing is retried or noted
    let r = tool(&client, "play", json!({ "ref": "channel:1:m3u:bbc1" }));
    assert!(r["structuredContent"].get("note").is_none() && fake.seen().len() == 3, "{r}");
}

/// The real binary: `tuner mcp --profile-dir <tmp>` with no Tuner running answers initialize,
/// tools/list, status (`running: false`) and control (not running) with nothing but JSON-RPC
/// on stdout, exits when stdin closes, and touches neither the database nor `tuner.log`.
#[test]
fn the_real_binary_speaks_only_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tuner"))
        .args(["mcp", "--profile-dir"])
        .arg(dir.path())
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let messages = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        call(3, "status", json!({})),
        call(4, "control", json!({ "action": "stop" })),
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for m in &messages {
            writeln!(stdin, "{m}").unwrap();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "tuner mcp did not exit when stdin closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let replies: Vec<Value> = stdout.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))).collect();
    let ids: Vec<_> = replies.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3, 4], "{stdout}");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), tuner::agent::tools::tools().len());
    assert_eq!(replies[2]["result"]["structuredContent"], json!({ "running": false }));
    assert!(error_text(&replies[3]["result"]).starts_with("not_running: "));
    assert!(!dir.path().join("data").join("tuner.db").exists(), "the bridge never opens the database");
    assert!(!dir.path().join("state").join("tuner.log").exists(), "the bridge never writes tuner.log");
    assert!(!socket_in(&dir).exists(), "status and control never launch Tuner");
}

/// The Claude Code plugin (`plugins/omatuner`) talks to this server: its `.mcp.json` names the
/// server `tuner` (`tuner mcp`), the marketplace entry matches the plugin, and every MCP tool a
/// skill allows (`mcp__plugin_omatuner_tuner__<tool>`) is a tool this build knows — a renamed
/// tool would otherwise leave a skill calling nothing. Every skill points at the shared rules.
#[test]
fn the_plugin_names_real_tools() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    let mcp: Value = serde_json::from_str(&read("plugins/omatuner/.mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"]["tuner"], json!({ "command": "tuner", "args": ["mcp"] }));
    let plugin: Value = serde_json::from_str(&read("plugins/omatuner/.claude-plugin/plugin.json")).unwrap();
    let market: Value = serde_json::from_str(&read(".claude-plugin/marketplace.json")).unwrap();
    assert_eq!(market["plugins"][0]["name"], plugin["name"]);
    assert_eq!(market["plugins"][0]["source"], "./plugins/omatuner");
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"), "bump the plugin with the app (installs are cached by version)");
    let known: Vec<&str> = tuner::agent::tools::tools().iter().map(|t| t.name).collect();
    for skill in ["watch", "pick", "series", "plan", "kids", "fix", "discover"] {
        let text = read(&format!("plugins/omatuner/skills/{skill}/SKILL.md"));
        let front = text.split("---").nth(1).unwrap_or_else(|| panic!("{skill}: no frontmatter"));
        assert!(front.contains(&format!("name: {skill}\n")), "{skill}");
        let allowed = front.lines().find_map(|l| l.strip_prefix("allowed-tools: ")).unwrap_or_else(|| panic!("{skill}: allowed-tools"));
        let tools: Vec<&str> = allowed.split_whitespace().filter_map(|t| t.strip_prefix("mcp__plugin_omatuner_tuner__")).collect();
        assert!(!tools.is_empty(), "{skill}");
        for t in tools {
            assert!(known.contains(&t), "{skill} allows {t}, which isn't a tool");
        }
        assert!(text.contains("`${CLAUDE_SKILL_DIR}/../../RULES.md`"), "{skill} points at the shared rules");
        assert!(text.lines().count() < 500, "{skill}: keep skills short");
    }
}
