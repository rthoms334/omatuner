//! Per agent: how Tuner reads whether it's connected, the commands of each op, and how their
//! output reads. Pure (argv builders and parsers); the service runs them. Verified against the
//! CLIs in throwaway homes (DESIGN §39.4):
//!
//! | Agent | Status read | Connect | Update | Disconnect |
//! |---|---|---|---|---|
//! | Claude Code | `claude plugin list --json` (+ `~/.claude.json` `mcpServers.tuner`) | marketplace add + plugin install | marketplace update + plugin update | plugin uninstall + marketplace remove |
//! | Codex | `codex mcp get --json tuner` | `codex mcp add tuner -- tuner mcp` | — | `codex mcp remove tuner` |
//! | Gemini CLI | `~/.gemini/settings.json` | `gemini mcp add -s user tuner tuner mcp` | — | `gemini mcp remove -s user tuner` |
//! | Copilot CLI | `copilot mcp get --json tuner` | `copilot mcp add tuner -- tuner mcp` | — | `copilot mcp remove tuner` |
//! | opencode | `opencode/opencode.json[c]` | `opencode mcp add tuner -- tuner mcp` | — | by hand |
//! | Cursor | `~/.cursor/mcp.json` | by hand | — | by hand |
//!
//! Never used: `claude mcp list/get`, `gemini mcp list`, `cursor-agent mcp list`, `opencode mcp
//! list` (they start every configured server), `gemini extensions install` (a trust prompt
//! that hangs) and `--trust` (it skips Gemini's tool confirmations).

use std::path::PathBuf;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::jsonc::{self, FileText};
use super::runner::{CHANGE_TIMEOUT, Cmd, CmdOutput, ConnectEnv, RunError, STATUS_TIMEOUT};
use super::{AgentId, Op, Status, Via, version_lt};
use crate::agent::setup::{self, PLUGIN};

/// Longest message shown or logged.
const MESSAGE_MAX: usize = 120;

/// The status command; `None` = read from the settings files only (Cursor). Gemini and opencode
/// get `--version` (is the CLI alive?), then their file is read.
pub(crate) fn status_cmd(id: AgentId, _env: &ConnectEnv) -> Option<Cmd> {
    let cmd = |argv: &[&str]| Cmd::new(argv, STATUS_TIMEOUT);
    match id {
        AgentId::Claude => Some(cmd(&["claude", "plugin", "list", "--json"])),
        AgentId::Codex => Some(cmd(&["codex", "mcp", "get", "--json", "tuner"])),
        AgentId::Copilot => Some(copilot(cmd(&["copilot", "mcp", "get", "--json", "tuner"]))),
        AgentId::Gemini => Some(cmd(&["gemini", "--version"])),
        AgentId::OpenCode => Some(cmd(&["opencode", "--version"])),
        AgentId::Cursor => None,
    }
}

/// The commands of `op`, run in order until one fails; empty = not automated.
pub(crate) fn op_steps(id: AgentId, op: Op, env: &ConnectEnv) -> Vec<Cmd> {
    if !id.can(op) {
        return Vec::new();
    }
    let cmds = |list: &[&[&str]]| list.iter().map(|argv| Cmd::new(argv, CHANGE_TIMEOUT)).collect::<Vec<_>>();
    match (id, op) {
        // the same steps `tuner claude-setup` prints, with a JSON result line
        (AgentId::Claude, Op::Connect) => claude(setup::steps(env.share.as_deref())),
        (AgentId::Claude, Op::Update) => claude(setup::update_steps(true)),
        (AgentId::Claude, Op::Disconnect) => claude(setup::remove_steps()),
        (AgentId::Codex, Op::Connect) => cmds(&[&["codex", "mcp", "add", "tuner", "--", "tuner", "mcp"]]),
        (AgentId::Codex, Op::Disconnect) => cmds(&[&["codex", "mcp", "remove", "tuner"]]),
        (AgentId::Gemini, Op::Connect) => cmds(&[&["gemini", "mcp", "add", "-s", "user", "tuner", "tuner", "mcp"]]),
        (AgentId::Gemini, Op::Disconnect) => cmds(&[&["gemini", "mcp", "remove", "-s", "user", "tuner"]]),
        (AgentId::Copilot, Op::Connect) => cmds(&[&["copilot", "mcp", "add", "tuner", "--", "tuner", "mcp"]]).into_iter().map(copilot).collect(),
        (AgentId::Copilot, Op::Disconnect) => cmds(&[&["copilot", "mcp", "remove", "tuner"]]).into_iter().map(copilot).collect(),
        (AgentId::OpenCode, Op::Connect) => cmds(&[&["opencode", "mcp", "add", "tuner", "--", "tuner", "mcp"]]),
        _ => Vec::new(),
    }
}

/// `tuner claude-setup`'s steps with `--json` after the subcommand (`claude plugin install
/// --json …`): one result line each, with an outcome and a failure code.
fn claude(steps: Vec<setup::Step>) -> Vec<Cmd> {
    steps
        .into_iter()
        .map(|st| {
            let mut argv = st.argv;
            let at =
                argv.iter().position(|a| matches!(a.as_str(), "add" | "install" | "update" | "uninstall" | "remove")).map_or(argv.len(), |i| i + 1);
            argv.insert(at, "--json".into());
            Cmd { argv, env: Vec::new(), timeout: CHANGE_TIMEOUT }
        })
        .collect()
}

/// Copilot downloads a newer CLI during any call unless told not to.
fn copilot(mut c: Cmd) -> Cmd {
    c.env.push(("COPILOT_AUTO_UPDATE", "false".into()));
    c
}

/// The status from the status command's answer (and the files where the agent keeps it).
pub(crate) fn parse_status(id: AgentId, out: Result<CmdOutput, RunError>, env: &ConnectEnv) -> Status {
    let out = match out {
        Err(RunError::NotFound) => return Status::NotFound,
        Err(RunError::TimedOut) => return Status::Unreadable(format!("{} didn't answer within {} s", id.label(), STATUS_TIMEOUT.as_secs())),
        Err(RunError::Stopped) => return Status::Checking,
        Err(RunError::Spawn(e)) => return Status::Unreadable(clean(&e)),
        Ok(out) => out,
    };
    match id {
        AgentId::Claude if out.ok() => match claude_plugin(&out.stdout, env) {
            Some(s) => s,
            None => Status::Unreadable("Claude Code's plugin list didn't read as expected".into()),
        },
        AgentId::Codex if out.ok() => match json_from(&out.stdout) {
            Some(v) if v.get("enabled") == Some(&Value::Bool(false)) => Status::TurnedOff,
            // exit 0 means the server is there; the details are optional
            _ => Status::Connected { via: Via::Mcp, version: None },
        },
        AgentId::Codex if out.stderr.contains("No MCP server named") => Status::NotConnected,
        AgentId::Copilot if out.ok() => match json_from(&out.stdout) {
            Some(v) if v.get("tuner").and_then(|t| t.get("enabled")) == Some(&Value::Bool(false)) => Status::TurnedOff,
            _ => Status::Connected { via: Via::Mcp, version: None },
        },
        AgentId::Copilot if copilot_no_tuner(&out) => Status::NotConnected,
        AgentId::Gemini | AgentId::OpenCode | AgentId::Cursor if out.ok() => status_from_files(id, env),
        _ => Status::Unreadable(first_line(&out.stderr).or_else(|| first_line(&out.stdout)).unwrap_or_else(|| exit_text(&out))),
    }
}

/// Claude Code's plugin list: Tuner's plugin, or the MCP-only setup, or nothing. `None` when
/// the answer isn't the list.
fn claude_plugin(stdout: &str, env: &ConnectEnv) -> Option<Status> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        id: Option<String>,
        /// `user` | `project` | `local`; older CLIs leave it out (user).
        scope: Option<String>,
        version: Option<String>,
        enabled: Option<bool>,
        folder_version: Option<String>,
        errors: Option<Vec<IgnoredAny>>,
    }
    // a warning line may come first: the list starts at a line's `[`
    let mut at = 0;
    let starts = stdout.split_inclusive('\n').filter_map(|l| {
        let here = at;
        at += l.len();
        let lead = l.len() - l.trim_start().len();
        l.trim_start().starts_with('[').then_some(here + lead)
    });
    let list: Vec<Value> = starts.chain(stdout.find('[')).find_map(|at| {
        let mut de = serde_json::Deserializer::from_str(&stdout[at..]);
        Vec::<Value>::deserialize(&mut de).ok()
    })?;
    // Tuner connects (and updates and removes) the user scope: a project's own entry isn't it
    let entry = list
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Entry>(v).ok())
        .find(|e| e.id.as_deref() == Some(PLUGIN) && e.scope.as_deref().is_none_or(|s| s == "user"));
    let Some(e) = entry else { return Some(status_from_files(AgentId::Claude, env)) };
    Some(if e.errors.is_some_and(|v| !v.is_empty()) {
        Status::NeedsRepair
    } else if e.enabled == Some(false) {
        Status::TurnedOff
    } else {
        match (e.version, e.folder_version) {
            // the packaged folder moved on (a Tuner upgrade); it loads in place, only the record is old
            (Some(v), Some(f)) if version_lt(&v, &f) => Status::UpdateAvailable { installed: v, available: f },
            // GitHub source: older than this Tuner
            (Some(v), None) if version_lt(&v, &env.tuner_version) => Status::UpdateAvailable { installed: v, available: env.tuner_version.clone() },
            (v, _) => Status::Connected { via: Via::Plugin, version: v },
        }
    })
}

/// The status from the agent's settings file (Gemini, opencode, Cursor; Claude's MCP-only
/// setup). Reads only Tuner's entry; the file's content is never logged.
pub(crate) fn status_from_files(id: AgentId, env: &ConnectEnv) -> Status {
    let (path, key) = match id {
        AgentId::Claude => (claude_json(env), "mcpServers"),
        AgentId::Gemini => (env.home.join(".gemini/settings.json"), "mcpServers"),
        AgentId::OpenCode => (opencode_file(env), "mcp"),
        AgentId::Cursor => (cursor_file(env), "mcpServers"),
        AgentId::Codex | AgentId::Copilot => return Status::Unreadable("read by its CLI".into()),
    };
    let text = match jsonc::read(&path) {
        FileText::Missing => return Status::NotConnected,
        FileText::Unreadable => return Status::Unreadable(format!("can't read {}", super::tilde(&path, &env.home))),
        FileText::Text(t) => t,
    };
    #[derive(Deserialize)]
    struct Servers {
        #[serde(rename = "mcpServers")]
        mcp_servers: Option<serde_json::Map<String, Value>>,
        mcp: Option<serde_json::Map<String, Value>>,
    }
    let unparsed = || Status::Unreadable(format!("{} doesn't parse", super::tilde(&path, &env.home)));
    if id == AgentId::Claude {
        return match serde_json::from_str::<ClaudeJson>(&text) {
            Ok(c) => {
                // a project's own (local scope) server counts too: adding the plugin would give
                // that project two `tuner` servers
                let local = c.projects.into_iter().flat_map(|p| p.into_values()).find_map(|p| p.mcp_servers?.remove("tuner"));
                entry_status(id, c.mcp_servers.and_then(|mut t| t.remove("tuner")).or(local).as_ref())
            }
            Err(_) => unparsed(),
        };
    }
    let Some(servers) = jsonc::parse(&text).and_then(|v| serde_json::from_value::<Servers>(v).ok()) else { return unparsed() };
    let mut table = if key == "mcp" { servers.mcp } else { servers.mcp_servers };
    entry_status(id, table.as_mut().and_then(|t| t.remove("tuner")).as_ref())
}

/// `~/.claude.json`'s servers: the user scope's and each project's (only these keys are kept;
/// the file holds account data).
#[derive(Deserialize)]
struct ClaudeJson {
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<serde_json::Map<String, Value>>,
    projects: Option<std::collections::BTreeMap<String, ClaudeProject>>,
}

#[derive(Deserialize)]
struct ClaudeProject {
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<serde_json::Map<String, Value>>,
}

/// The status from Tuner's entry in a settings file (`None`: not there).
fn entry_status(id: AgentId, entry: Option<&Value>) -> Status {
    match entry {
        None => Status::NotConnected,
        Some(t) if t.get("enabled") == Some(&Value::Bool(false)) || t.get("disabled") == Some(&Value::Bool(true)) => Status::TurnedOff,
        Some(_) => Status::Connected { via: if id == AgentId::Claude { Via::McpOnly } else { Via::Mcp }, version: None },
    }
}

/// `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`.
fn claude_json(env: &ConnectEnv) -> PathBuf {
    env.claude_config_dir.as_ref().unwrap_or(&env.home).join(".claude.json")
}

/// opencode's global settings: `opencode.json`, else `opencode.jsonc` (what `opencode mcp add`
/// creates when neither exists).
pub(crate) fn opencode_file(env: &ConnectEnv) -> PathBuf {
    let dir = env.xdg_config.join("opencode");
    let json = dir.join("opencode.json");
    if json.is_file() { json } else { dir.join("opencode.jsonc") }
}

/// Cursor's MCP settings (shared by the editor and `cursor-agent`).
pub(crate) fn cursor_file(env: &ConnectEnv) -> PathBuf {
    env.home.join(".cursor/mcp.json")
}

/// Cursor is there: `cursor-agent` on PATH (checked by the service) or the editor's folder.
pub(crate) fn cursor_present(env: &ConnectEnv) -> bool {
    env.home.join(".cursor").is_dir()
}

/// A step's outcome: `Ok(message)` or `Err(message)` (redacted, one line). "Already done"
/// counts as done: Copilot's "already exists" on add and "not found" on remove, Claude's
/// `not_installed` / `not_configured` on disconnect.
pub(crate) fn step_result(id: AgentId, op: Op, out: Result<CmdOutput, RunError>) -> Result<String, String> {
    let out = match out {
        Ok(o) => o,
        Err(RunError::NotFound) => return Err(format!("`{}` isn't on your PATH any more", id.program())),
        Err(RunError::TimedOut) => return Err(format!("it didn't answer within {} minutes", CHANGE_TIMEOUT.as_secs() / 60)),
        Err(RunError::Stopped) => return Err("Tuner closed before it finished".into()),
        Err(RunError::Spawn(e)) => return Err(clean(&e)),
    };
    if id == AgentId::Claude
        && let Some(r) = claude_result(&out.stdout)
    {
        let message = r.message.as_deref().and_then(first_line);
        let done = r.outcome == "ok" || (op == Op::Disconnect && matches!(r.failure_code.as_deref(), Some("not_installed" | "not_configured")));
        return if done {
            Ok(message.unwrap_or_else(|| "done".into()))
        } else {
            Err(message.or_else(|| first_line(&out.stderr)).unwrap_or_else(|| exit_text(&out)))
        };
    }
    let said = || first_line(&out.stderr).or_else(|| first_line(&out.stdout));
    let already = match (id, op) {
        (AgentId::Copilot, Op::Connect) => out.stderr.contains("already exists"),
        (AgentId::Copilot, Op::Disconnect) => copilot_no_tuner(&out),
        _ => false,
    };
    if out.ok() || already {
        // Gemini answers on stderr; opencode draws a box around its line
        Ok(last_line(&out.stdout).or_else(|| last_line(&out.stderr)).unwrap_or_else(|| "done".into()))
    } else {
        Err(said().unwrap_or_else(|| exit_text(&out)))
    }
}

/// Copilot's own "no such server" (not a wrapper's `sh: node: not found`).
fn copilot_no_tuner(out: &CmdOutput) -> bool {
    out.stderr.contains("Server \"tuner\" not found")
}

/// Claude's `--json` result line: the last stdout line that is an object with `outcome`.
pub(crate) struct ClaudeResult {
    pub outcome: String,
    pub message: Option<String>,
    pub failure_code: Option<String>,
    /// `plugin update`: `updated` | `up_to_date`.
    pub update_outcome: Option<String>,
    pub new_version: Option<String>,
}

pub(crate) fn claude_result(stdout: &str) -> Option<ClaudeResult> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Line {
        outcome: String,
        message: Option<String>,
        failure_code: Option<String>,
        update_outcome: Option<String>,
        new_version: Option<String>,
    }
    stdout.lines().rev().filter(|l| l.trim_start().starts_with('{')).find_map(|l| serde_json::from_str::<Line>(l.trim()).ok()).map(|l| ClaudeResult {
        outcome: l.outcome,
        message: l.message,
        failure_code: l.failure_code,
        update_outcome: l.update_outcome,
        new_version: l.new_version,
    })
}

/// The first JSON object in `s` (a warning line may come before it).
fn json_from(s: &str) -> Option<Value> {
    let at = s.find('{')?;
    let mut de = serde_json::Deserializer::from_str(&s[at..]);
    Value::deserialize(&mut de).ok()
}

/// The first meaningful line (not a WARNING, not blank), cleaned, without a leading "Error:"
/// (the toast already says what failed).
fn first_line(s: &str) -> Option<String> {
    let line = s.lines().map(trim_line).find(|l| !l.is_empty() && !l.starts_with("WARNING"))?;
    let line = ["Error:", "error:", "ERROR:"].iter().find_map(|p| line.strip_prefix(p)).map_or(line, str::trim_start);
    Some(clean(line))
}

/// The last meaningful line, cleaned (success messages come last).
fn last_line(s: &str) -> Option<String> {
    s.lines().rev().map(trim_line).find(|l| !l.is_empty() && !l.starts_with("WARNING")).map(clean)
}

/// A line without box drawing and status marks ("◆  MCP server …", "✘ …", "│").
fn trim_line(l: &str) -> &str {
    l.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '◆' | '◇' | '┌' | '└' | '✘' | '✔' | '✓' | '✗' | '●'))
}

/// Redacted, without control characters, at most [`MESSAGE_MAX`] chars.
pub(crate) fn clean(s: &str) -> String {
    let red = crate::redact::redact(s);
    let one: String = red.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let one = one.trim();
    if one.chars().count() <= MESSAGE_MAX { one.to_owned() } else { format!("{}…", one.chars().take(MESSAGE_MAX - 1).collect::<String>()) }
}

fn exit_text(out: &CmdOutput) -> String {
    out.code.map_or_else(|| "it was stopped".into(), |c| format!("it ended with exit code {c}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const REAL_022: &str = r#"[{"id":"omatuner@omatuner","version":"0.2.2","scope":"user","enabled":true,"installPath":"/h/.claude/plugins/cache/omatuner/omatuner/0.2.2","readFromFolder":"/usr/share/tuner/claude/plugins/omatuner","folderVersion":"0.2.2","installedAt":"2026-10-10T10:00:00Z","lastUpdated":"2026-10-10T10:00:00Z","mcpServers":{"tuner":{"command":"tuner","args":["mcp"]}},"projectEnabled":false}]"#;

    fn ok(stdout: &str) -> Result<CmdOutput, RunError> {
        Ok(CmdOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
    }

    fn fail(code: i32, stdout: &str, stderr: &str) -> Result<CmdOutput, RunError> {
        Ok(CmdOutput { code: Some(code), stdout: stdout.into(), stderr: stderr.into() })
    }

    fn home() -> (tempfile::TempDir, ConnectEnv) {
        let dir = tempfile::tempdir().unwrap();
        let mut env = ConnectEnv::rooted(dir.path());
        env.tuner_version = "0.2.3".into();
        (dir, env)
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn claude_plugin_list_reads_every_shape() {
        let (dir, env) = home();
        let st = |s: &str| parse_status(AgentId::Claude, ok(s), &env);
        assert_eq!(st(REAL_022), Status::Connected { via: Via::Plugin, version: Some("0.2.2".into()) });
        // the package moved on: the folder is newer than the record
        let newer = REAL_022.replace(r#""folderVersion":"0.2.2""#, r#""folderVersion":"0.2.3""#);
        assert_eq!(st(&newer), Status::UpdateAvailable { installed: "0.2.2".into(), available: "0.2.3".into() });
        // GitHub source (no folder): compared with this Tuner
        let github = r#"[{"id":"omatuner@omatuner","version":"0.2.2","scope":"user","enabled":true}]"#;
        assert_eq!(st(github), Status::UpdateAvailable { installed: "0.2.2".into(), available: "0.2.3".into() });
        assert_eq!(st(&github.replace("0.2.2", "0.2.3")), Status::Connected { via: Via::Plugin, version: Some("0.2.3".into()) });
        // a downgrade (the record newer than the folder) isn't an update
        assert_eq!(
            st(&REAL_022.replace(r#""version":"0.2.2""#, r#""version":"0.2.3""#)),
            Status::Connected { via: Via::Plugin, version: Some("0.2.3".into()) }
        );
        assert_eq!(st(&REAL_022.replace(r#""enabled":true"#, r#""enabled":false"#)), Status::TurnedOff);
        // only the user scope is Tuner's; `"errors": null` is no error
        let project_only = REAL_022.replace(r#""scope":"user""#, r#""scope":"project""#);
        assert_eq!(st(&project_only), Status::NotConnected);
        let both = format!("[{},{}]", project_only[1..project_only.len() - 1].replace(r#""enabled":true"#, r#""enabled":false"#), &REAL_022[1..]);
        assert_eq!(st(&both), Status::Connected { via: Via::Plugin, version: Some("0.2.2".into()) });
        assert_eq!(
            st(&REAL_022.replace(r#""projectEnabled""#, r#""errors":null,"projectEnabled""#)),
            Status::Connected { via: Via::Plugin, version: Some("0.2.2".into()) }
        );
        let broken = r#"[{"id":"omatuner@omatuner","version":"0.2.2","enabled":true,"errors":["Marketplace omatuner failed to load: cache-miss"],"errorDetails":[{"type":"marketplace-load-failed"}]}]"#;
        assert_eq!(st(broken), Status::NeedsRepair);
        // a warning line first, other plugins, unknown fields, missing optional ones
        let noisy = "Warning: something odd [ignored]\n[{\"id\":\"other@x\",\"weird\":[1,2]},{\"id\":\"omatuner@omatuner\",\"future\":{}}]\n";
        assert_eq!(st(noisy), Status::Connected { via: Via::Plugin, version: None });
        assert!(matches!(st("garbage"), Status::Unreadable(_)));
        assert!(matches!(st("[{\"id\": 3"), Status::Unreadable(_)));
        // no plugin: the MCP-only setup in ~/.claude.json, or nothing
        assert_eq!(st("[]"), Status::NotConnected);
        put(
            &dir.path().join(".claude.json"),
            r#"{"numStartups":3,"oauthAccount":{"x":1},"mcpServers":{"tuner":{"type":"stdio","command":"tuner","args":["mcp"]}}}"#,
        );
        assert_eq!(st("[]"), Status::Connected { via: Via::McpOnly, version: None });
        // a project's own (local scope) server
        put(&dir.path().join(".claude.json"), r#"{"projects":{"/w/a":{"allowedTools":[]},"/w/b":{"mcpServers":{"tuner":{"command":"tuner"}}}}}"#);
        assert_eq!(st("[]"), Status::Connected { via: Via::McpOnly, version: None });
        // $CLAUDE_CONFIG_DIR wins over the home
        let mut moved = env.clone();
        moved.claude_config_dir = Some(dir.path().join("cc"));
        assert_eq!(parse_status(AgentId::Claude, ok("[]"), &moved), Status::NotConnected);
        put(&dir.path().join("cc/.claude.json"), r#"{"mcpServers":{"tuner":{}}}"#);
        assert_eq!(parse_status(AgentId::Claude, ok("[]"), &moved), Status::Connected { via: Via::McpOnly, version: None });
        // the CLI itself: missing, hanging, broken (a mise shim without a version)
        assert_eq!(parse_status(AgentId::Claude, Err(RunError::NotFound), &env), Status::NotFound);
        assert!(matches!(parse_status(AgentId::Claude, Err(RunError::TimedOut), &env), Status::Unreadable(m) if m.contains("15 s")));
        assert_eq!(
            parse_status(AgentId::Claude, fail(1, "", "mise ERROR No version is set for shim: claude\n"), &env),
            Status::Unreadable("mise ERROR No version is set for shim: claude".into())
        );
    }

    #[test]
    fn claude_results_read_the_json_line() {
        let r = |op, out| step_result(AgentId::Claude, op, out);
        let added = r#"{"command":"marketplace-add","outcome":"ok","marketplace":"omatuner","message":"Successfully added marketplace: omatuner (declared in user settings)"}"#;
        assert_eq!(r(Op::Connect, ok(added)), Ok("Successfully added marketplace: omatuner (declared in user settings)".into()));
        // already uninstalled / no marketplace: a disconnect is done
        let gone = r#"{"command":"uninstall","outcome":"failed","failureCode":"not_installed","message":"Plugin omatuner is not installed"}"#;
        assert!(r(Op::Disconnect, fail(1, gone, "")).is_ok());
        assert!(r(Op::Disconnect, fail(1, r#"{"outcome":"failed","failureCode":"not_configured"}"#, "")).is_ok());
        assert!(r(Op::Connect, fail(1, gone, "")).is_err(), "only a disconnect treats it as done");
        // a failed clone: the first line of git's message, redacted and capped
        let clone = serde_json::json!({"outcome":"failed","failureCode":"error_not_found","message":format!("Failed to clone https://user:hunter2@github.com/x/y.git\nfatal: {}\nmore", "x".repeat(300))}).to_string();
        let e = r(Op::Connect, fail(1, &format!("progress…\n{clone}\n"), "")).unwrap_err();
        assert!(!e.contains("hunter2") && !e.contains('\n') && e.starts_with("Failed to clone"), "{e}");
        assert_eq!(r(Op::Connect, fail(1, "", &format!("✘ {}", "y".repeat(400)))).unwrap_err().chars().count(), MESSAGE_MAX);
        // not JSON at all (an older CLI): stderr's line
        assert_eq!(r(Op::Connect, fail(1, "Usage: claude plugin …", "✘ error: unknown option '--json'")), Err("unknown option '--json'".into()));
        assert_eq!(r(Op::Update, Err(RunError::TimedOut)), Err("it didn't answer within 2 minutes".into()));
        assert_eq!(r(Op::Connect, Err(RunError::NotFound)), Err("`claude` isn't on your PATH any more".into()));
        let up =
            claude_result(r#"{"command":"update","outcome":"ok","updateOutcome":"up_to_date","oldVersion":"0.2.4","newVersion":"0.2.4"}"#).unwrap();
        assert_eq!((up.update_outcome.as_deref(), up.new_version.as_deref()), (Some("up_to_date"), Some("0.2.4")));
    }

    #[test]
    fn codex_and_copilot_statuses_and_already_done() {
        let env = ConnectEnv::rooted(Path::new("/nonexistent"));
        let codex = |out| parse_status(AgentId::Codex, out, &env);
        let get = r#"{"name":"tuner","enabled":true,"transport":{"type":"stdio","command":"tuner","args":["mcp"]}}"#;
        assert_eq!(
            codex(Ok(CmdOutput { code: Some(0), stdout: get.into(), stderr: "WARNING: helper binary missing\n".into() })),
            Status::Connected { via: Via::Mcp, version: None }
        );
        assert_eq!(codex(ok(&get.replace("true", "false"))), Status::TurnedOff);
        assert_eq!(codex(fail(1, "", "Error: No MCP server named 'tuner' found.\n")), Status::NotConnected);
        assert_eq!(
            codex(fail(2, "", "error: unexpected argument '--json' found\n")),
            Status::Unreadable("unexpected argument '--json' found".into())
        );
        let copilot = |out| parse_status(AgentId::Copilot, out, &env);
        assert_eq!(
            copilot(ok(r#"{"tuner":{"tools":["*"],"type":"local","command":"tuner","args":["mcp"],"source":"user","enabled":true}}"#)),
            Status::Connected { via: Via::Mcp, version: None }
        );
        assert_eq!(copilot(ok(r#"{"tuner":{"enabled":false}}"#)), Status::TurnedOff);
        assert_eq!(copilot(fail(1, "", "Error: Server \"tuner\" not found.\n")), Status::NotConnected);
        // a wrapper's own "not found" is a broken CLI, not "not connected"
        assert_eq!(copilot(fail(127, "", "sh: node: not found\n")), Status::Unreadable("sh: node: not found".into()));
        assert!(step_result(AgentId::Copilot, Op::Disconnect, fail(127, "", "sh: node: not found\n")).is_err());
        // already there / already gone: done
        assert!(
            step_result(AgentId::Copilot, Op::Connect, fail(1, "", "Error: Server \"tuner\" already exists. To update it, remove it first:\n"))
                .is_ok()
        );
        assert!(step_result(AgentId::Copilot, Op::Disconnect, fail(1, "", "Error: Server \"tuner\" not found.\n")).is_ok());
        assert_eq!(step_result(AgentId::Copilot, Op::Connect, fail(1, "", "Error: not logged in\n")), Err("not logged in".into()));
        assert_eq!(step_result(AgentId::Codex, Op::Connect, ok("Added global MCP server 'tuner'.\n")), Ok("Added global MCP server 'tuner'.".into()));
        assert_eq!(
            step_result(
                AgentId::Gemini,
                Op::Connect,
                Ok(CmdOutput { code: Some(0), stdout: String::new(), stderr: "MCP server \"tuner\" added to user settings. (stdio)\n".into() })
            ),
            Ok("MCP server \"tuner\" added to user settings. (stdio)".into())
        );
        assert_eq!(
            step_result(AgentId::OpenCode, Op::Connect, ok("│\n◆  MCP server \"tuner\" added to /h/.config/opencode/opencode.jsonc\n")),
            Ok("MCP server \"tuner\" added to /h/.config/opencode/opencode.jsonc".into())
        );
    }

    #[test]
    fn file_statuses_read_only_tuners_entry() {
        let (dir, env) = home();
        let alive = || ok("0.63.0\n");
        let gemini = || parse_status(AgentId::Gemini, alive(), &env);
        assert_eq!(gemini(), Status::NotConnected, "no settings file yet");
        put(
            &dir.path().join(".gemini/settings.json"),
            "{ // mine\n \"theme\": \"x\",\n \"mcpServers\": { \"tuner\": { \"command\": \"tuner\", \"args\": [\"mcp\"] }, },\n}",
        );
        assert_eq!(gemini(), Status::Connected { via: Via::Mcp, version: None });
        put(&dir.path().join(".gemini/settings.json"), "{ \"mcpServers\": ");
        assert_eq!(gemini(), Status::Unreadable("~/.gemini/settings.json doesn't parse".into()));
        assert!(matches!(parse_status(AgentId::Gemini, fail(1, "", "boom"), &env), Status::Unreadable(_)), "a broken CLI");

        let oc = |out| parse_status(AgentId::OpenCode, out, &env);
        put(
            &dir.path().join(".config/opencode/opencode.jsonc"),
            "{ /* c */ \"mcp\": { \"tuner\": { \"type\": \"local\", \"command\": [\"tuner\", \"mcp\"] } } }",
        );
        assert_eq!(oc(alive()), Status::Connected { via: Via::Mcp, version: None }, ".jsonc alone");
        put(&dir.path().join(".config/opencode/opencode.json"), "{ \"$schema\": \"https://opencode.ai/config.json\" }");
        assert_eq!(oc(alive()), Status::NotConnected, ".json comes first");
        put(&dir.path().join(".config/opencode/opencode.json"), "{ \"mcp\": { \"tuner\": { \"enabled\": false } } }");
        assert_eq!(oc(alive()), Status::TurnedOff);

        assert!(!cursor_present(&env));
        assert_eq!(status_from_files(AgentId::Cursor, &env), Status::NotConnected);
        put(
            &dir.path().join(".cursor/mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x","env":{"TOKEN":"secret"}},"tuner":{"command":"tuner","args":["mcp"]}}}"#,
        );
        assert!(cursor_present(&env));
        assert_eq!(status_from_files(AgentId::Cursor, &env), Status::Connected { via: Via::Mcp, version: None });
    }

    #[test]
    fn each_agent_gets_its_exact_commands() {
        let mut env = ConnectEnv::rooted(Path::new("/h"));
        let argv = |id, op, env: &ConnectEnv| op_steps(id, op, env).iter().map(|c| c.argv.join(" ")).collect::<Vec<_>>();
        assert_eq!(
            argv(AgentId::Claude, Op::Connect, &env),
            ["claude plugin marketplace add --json rthoms334/omatuner", "claude plugin install --json omatuner@omatuner --scope user"]
        );
        env.share = Some("/usr/share/tuner/claude".into());
        assert_eq!(argv(AgentId::Claude, Op::Connect, &env)[0], "claude plugin marketplace add --json /usr/share/tuner/claude");
        assert_eq!(
            argv(AgentId::Claude, Op::Update, &env),
            ["claude plugin marketplace update --json omatuner", "claude plugin update --json omatuner@omatuner"]
        );
        assert_eq!(
            argv(AgentId::Claude, Op::Disconnect, &env),
            ["claude plugin uninstall --json omatuner@omatuner --scope user", "claude plugin marketplace remove --json omatuner"]
        );
        assert_eq!(argv(AgentId::Codex, Op::Connect, &env), ["codex mcp add tuner -- tuner mcp"]);
        assert_eq!(argv(AgentId::Codex, Op::Disconnect, &env), ["codex mcp remove tuner"]);
        assert_eq!(argv(AgentId::Gemini, Op::Connect, &env), ["gemini mcp add -s user tuner tuner mcp"]);
        assert_eq!(argv(AgentId::Gemini, Op::Disconnect, &env), ["gemini mcp remove -s user tuner"]);
        assert_eq!(argv(AgentId::Copilot, Op::Connect, &env), ["copilot mcp add tuner -- tuner mcp"]);
        assert_eq!(argv(AgentId::OpenCode, Op::Connect, &env), ["opencode mcp add tuner -- tuner mcp"]);
        for (id, op) in
            [(AgentId::OpenCode, Op::Disconnect), (AgentId::Codex, Op::Update), (AgentId::Cursor, Op::Connect), (AgentId::Cursor, Op::Disconnect)]
        {
            assert!(op_steps(id, op, &env).is_empty(), "{id:?} {op:?} is done by hand");
        }
        let copilot = op_steps(AgentId::Copilot, Op::Disconnect, &env);
        assert_eq!(copilot[0].env, [("COPILOT_AUTO_UPDATE", "false".to_owned())]);
        assert_eq!(status_cmd(AgentId::Copilot, &env).unwrap().env, copilot[0].env);
        assert!(op_steps(AgentId::Claude, Op::Connect, &env).iter().all(|c| c.timeout == CHANGE_TIMEOUT));
        assert_eq!(
            status_cmd(AgentId::Claude, &env).map(|c| (c.argv.join(" "), c.timeout)),
            Some(("claude plugin list --json".into(), STATUS_TIMEOUT))
        );
        assert_eq!(status_cmd(AgentId::Codex, &env).unwrap().argv.join(" "), "codex mcp get --json tuner");
        assert!(status_cmd(AgentId::Cursor, &env).is_none());
        // no status read ever starts the configured servers
        for id in AgentId::ALL {
            if let Some(c) = status_cmd(id, &env) {
                assert!(!c.argv.iter().any(|a| a == "list") || id == AgentId::Claude, "{:?}", c.argv);
                assert!(!(c.argv.contains(&"mcp".to_owned()) && c.argv.contains(&"list".to_owned())));
            }
        }
    }
}
