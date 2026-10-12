//! `tuner mcp` end to end (DESIGN §30): the real binary, spoken to over stdin/stdout like Claude
//! Code does, bridged over the real IPC socket to the real app — `TunerApp` with its services on
//! the demo profile (`examples/demo_profile.rs`), headless (null video/audio, no window), driven
//! frame by frame while the bridge waits. "Sports is on" → guide_search → play the live
//! programme → status → schedule_switch for a later one → control pause.
//!
//! `TUNER_MCP_NO_LAUNCH=1`: if the app weren't serving, the bridge must not open a window.
//! The transcript is printed (`cargo test --test agent_e2e -- --nocapture`).

#[path = "../examples/demo_profile.rs"]
#[allow(dead_code)]
mod demo;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::App as _;
use eframe::egui;
use serde_json::{Value, json};
use tuner::app::TunerApp;
use tuner::config::Config;
use tuner::ipc::IpcRequest;
use tuner::paths::AppPaths;
use tuner::services::Services;

/// The programme titles the demo's sports channels rotate through.
const SPORTS: &[&str] =
    &["Inside the Paddock", "Matchday Live", "Goal Rush", "Courtside Tonight", "Sports Desk", "Classic Finals", "Training Ground", "Extra Time"];

struct Headless {
    app: TunerApp,
    ctx: egui::Context,
    frame: eframe::Frame,
}

impl Headless {
    fn start(root: &std::path::Path) -> Self {
        let mut paths = AppPaths::rooted(root);
        paths.home = root.join("home"); // never the user's theme or fonts
        let mut cfg = Config::load(&paths.config_file()).expect("demo config");
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        cfg.integration.mpris = false;
        cfg.integration.follow_omarchy_theme = false;
        cfg.general.refresh_on_launch_after_h = 0;
        let (bus, events) = tuner::msg::EventBus::new(None);
        let services = Services::start(paths, cfg, tuner::logging::LogBuffer::new(100), bus).expect("services");
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let theme = Arc::new(tuner::theme::Theme::fallback());
        let app = TunerApp::new(&cc, services, events, theme, tuner::fonts::LoadedFonts::default(), IpcRequest::default());
        let mut h = Self { app, ctx, frame: eframe::Frame::_new_kittest() };
        h.frame();
        h
    }

    /// One eframe frame (`logic` + `ui`), like the window would run.
    fn frame(&mut self) {
        let mut raw =
            egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..Default::default() };
        let Headless { app, ctx, frame } = self;
        app.raw_input_hook(ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx(), frame);
            app.ui(ui, frame);
        });
        out.textures_delta.clear();
    }

    fn frames_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.frame();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        self.app.on_exit(None);
    }
}

/// `tuner mcp` as Claude Code runs it, plus the transcript.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next: u64,
    transcript: Vec<String>,
}

impl Bridge {
    fn start(profile: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tuner"))
            .args(["mcp", "--profile-dir"])
            .arg(profile)
            .env("TUNER_MCP_NO_LAUNCH", "1")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("tuner mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self { child, stdin, lines, next: 1, transcript: Vec::new() }
    }

    fn send(&mut self, msg: &Value) {
        let line = msg.to_string();
        self.transcript.push(format!("→ {line}"));
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// A request; frames run while the bridge (and the app's handler) work on it.
    fn request(&mut self, app: &mut Headless, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            assert!(Instant::now() < deadline, "no answer to {method}");
            app.frame();
            match self.lines.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout must be JSON-RPC only ({e}): {line}"));
                    assert!(!line.contains("://"), "an address on the wire: {line}");
                    let shown = if method == "tools/list" {
                        let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
                        format!("← {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{} tools: {}]}}}}", names.len(), names.join(", "))
                    } else {
                        format!("← {line}")
                    };
                    self.transcript.push(shown);
                    assert_eq!(v["id"], id, "{line}");
                    return v;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("the bridge went away: {e}"),
            }
        }
    }

    /// A tool's structured result (it must succeed).
    fn tool(&mut self, app: &mut Headless, name: &str, args: Value) -> Value {
        let v = self.request(app, "tools/call", json!({"name": name, "arguments": args}));
        let r = &v["result"];
        assert_eq!(r["isError"], false, "{name}: {v}");
        r["structuredContent"].clone()
    }
}

/// The first programme of the demo's sports titles airing `airing` (live / upcoming).
fn find(bridge: &mut Bridge, app: &mut Headless, airing: &str) -> Value {
    for title in SPORTS {
        let r = bridge.tool(app, "guide_search", json!({"query": title, "include_past": false, "limit": 10}));
        if let Some(p) = r["programmes"].as_array().unwrap().iter().find(|p| p["airing"] == airing) {
            return p.clone();
        }
    }
    panic!("no {airing} sports programme in the demo guide");
}

#[test]
fn claude_finds_the_live_sport_plays_it_and_schedules_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("p");
    // the live channels' "stream": a still image, which mpv plays (looped) like a video
    let media = dir.path().join("live.png");
    image::RgbImage::from_pixel(64, 36, image::Rgb([40, 90, 160])).save(&media).unwrap();
    demo::build(&profile, Some(&media), None, true).expect("demo profile");
    let profile = std::fs::canonicalize(&profile).unwrap();
    let mut app = Headless::start(&profile);
    // the app serves its socket (bound by the services at start)
    let socket = AppPaths::rooted(&profile).ipc_socket();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "the app never served {}", socket.display());
        app.frame();
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut bridge = Bridge::start(&profile);

    let init = bridge.request(
        &mut app,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "e2e"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "tuner");
    bridge.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = bridge.request(&mut app, "tools/list", json!({}));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), tuner::agent::tools::tools().len());

    // "sports is on": the live programme, played
    let live = find(&mut bridge, &mut app, "live");
    let channel = live["channel"]["ref"].as_str().unwrap().to_owned();
    let played = bridge.tool(&mut app, "play", json!({"ref": live["ref"]}));
    assert_eq!(played["outcome"], "playing", "{played}");
    assert_eq!(played["ref"], channel.as_str());
    app.frames_for(Duration::from_millis(1500));
    let status = bridge.tool(&mut app, "status", json!({}));
    assert_eq!(status["running"], true);
    assert_eq!(status["now_playing"]["ref"], channel.as_str(), "{status}");
    assert_eq!(status["now_playing"]["kind"], "live");

    // a later one: switch to it by itself at the start
    let later = find(&mut bridge, &mut app, "upcoming");
    let sw = bridge.tool(&mut app, "schedule_switch", json!({"ref": later["ref"]}));
    assert_eq!(sw["start"], later["start"]);
    assert_eq!(sw["channel"]["ref"], later["channel"]["ref"]);
    let status = bridge.tool(&mut app, "status", json!({}));
    assert!(status["scheduled"].as_array().unwrap().iter().any(|s| s["id"] == sw["id"]), "{status}");

    let paused = bridge.tool(&mut app, "control", json!({"action": "pause"}));
    assert_eq!(paused["paused"], true, "{paused}");

    drop(bridge.stdin.take());
    let exit = bridge.child.wait().unwrap();
    assert!(exit.success());
    println!("\n----- MCP transcript (tuner mcp ⇄ headless Tuner on the demo profile) -----");
    for line in &bridge.transcript {
        println!("{line}");
    }
}
