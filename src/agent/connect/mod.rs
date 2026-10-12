//! Connect an AI agent to Tuner from Tuner itself: the first-run card and Settings ▸ AI agents
//! (DESIGN §39). The package can't do it (pacman runs as root and never touches /home, and it is
//! another app's settings), so Tuner offers it once per version and the page does it on demand.
//!
//! * [`AgentId`] — the agents Tuner knows: Claude Code gets Tuner's plugin (tools + 7 skills),
//!   Codex, Gemini CLI, Copilot CLI and opencode get the plain MCP server (`tuner mcp`), Cursor is
//!   by hand (a snippet to copy).
//! * [`runner`] — running agent CLIs: argv (no shell), stdin closed, from `$HOME`, timeouts, the
//!   whole process group killed on timeout or exit; [`runner::NullRunner`] off the desktop and a
//!   scripted fake in tests, so a test never starts a real agent CLI.
//! * [`agents`] — per agent: the status read (never a command that starts every MCP server),
//!   the commands of each [`Op`], and how their output reads ("already done" counts as done).
//! * [`hub`] — the background service ([`AgentConnect`], in `Services`): detection, status,
//!   one queue of changes, toasts, the start-of-version pass and "last used".
//! * [`usage`] — when each named agent last asked Tuner something (meta `agent.used.<name>`).
//!
//! The pure decisions ([`should_check`], [`decide`], [`version_lt`]) and the copy texts
//! ([`snippet`], [`manual_disconnect`]) live here.

pub mod agents;
pub mod hub;
pub mod jsonc;
pub mod runner;
pub mod usage;

use std::path::{Path, PathBuf};

pub use hub::AgentConnect;
pub use runner::{CommandRunner, ConnectEnv};

use crate::config::AgentsConfig;
use crate::model::Timestamp;

/// The agents Tuner knows, in display and priority order (the card ticks the first one found).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentId {
    Claude,
    Codex,
    Gemini,
    Copilot,
    OpenCode,
    Cursor,
}

impl AgentId {
    pub const ALL: [AgentId; 6] = [Self::Claude, Self::Codex, Self::Gemini, Self::Copilot, Self::OpenCode, Self::Cursor];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini CLI",
            Self::Copilot => "Copilot CLI",
            Self::OpenCode => "opencode",
            Self::Cursor => "Cursor",
        }
    }

    /// The command-line program found on PATH.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor-agent",
        }
    }

    /// The name `model::AgentName` shows for this agent's MCP client — for "used 2 h ago".
    pub fn friendly(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
            Self::Copilot => "Copilot",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
        }
    }

    /// Whether Tuner runs `op` for this agent (the rest is done by hand: opencode has no
    /// remove command, Cursor no add command; only Claude's plugin has versions to update).
    pub fn can(self, op: Op) -> bool {
        matches!(
            (self, op),
            (Self::Claude, _) | (Self::Codex | Self::Gemini | Self::Copilot, Op::Connect | Op::Disconnect) | (Self::OpenCode, Op::Connect)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Connect,
    Update,
    Disconnect,
}

impl Op {
    /// "connect" | "update" | "disconnect" (toasts: "Couldn't connect Codex: …").
    pub fn verb(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Update => "update",
            Self::Disconnect => "disconnect",
        }
    }
}

/// How an agent is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// Claude Code with Tuner's plugin (tools + skills).
    Plugin,
    /// Claude Code with only the MCP server (`claude mcp add`; no skills).
    McpOnly,
    /// Every other agent: the MCP server in its settings.
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Checking,
    /// The program isn't on PATH (nor in `ConnectEnv::extra_dirs`).
    NotFound,
    NotConnected,
    Connected {
        via: Via,
        version: Option<String>,
    },
    /// Claude's plugin only: the recorded version is older than the packaged one (or Tuner's).
    UpdateAvailable {
        installed: String,
        available: String,
    },
    /// Configured but turned off in the agent.
    TurnedOff,
    /// Claude: the plugin's entry has errors (its marketplace folder is gone).
    NeedsRepair,
    /// Couldn't tell: a redacted first line, at most 120 chars.
    Unreadable(String),
}

impl Status {
    /// Tuner is in the agent's settings (perhaps turned off or out of date).
    pub fn connected(&self) -> bool {
        matches!(self, Self::Connected { .. } | Self::UpdateAvailable { .. } | Self::TurnedOff)
    }

    /// The row's status line ("Connected · plugin 0.2.2", "Not connected", …).
    pub fn text(&self, id: AgentId) -> String {
        match self {
            Self::Checking => "Checking…".into(),
            Self::NotFound => "Not found".into(),
            Self::NotConnected if id == AgentId::Cursor => "Not connected · add it by hand".into(),
            Self::NotConnected => "Not connected".into(),
            Self::Connected { via: Via::Plugin, version: Some(v) } => format!("Connected · plugin {v}"),
            Self::Connected { via: Via::Plugin, version: None } => "Connected · plugin".into(),
            Self::Connected { via: Via::McpOnly, .. } => "Connected · MCP server only (no skills)".into(),
            Self::Connected { via: Via::Mcp, .. } if id == AgentId::OpenCode => "Connected · disconnect by hand".into(),
            Self::Connected { via: Via::Mcp, .. } => "Connected".into(),
            Self::UpdateAvailable { installed, available } => format!("Update available · plugin {installed} → {available}"),
            Self::TurnedOff => format!("Turned off in {}", id.label()),
            Self::NeedsRepair => "Needs repair · Tuner's plugin folder is missing".into(),
            Self::Unreadable(why) => format!("Couldn't check: {why}"),
        }
    }
}

/// What the last op on a row did (the card shows it per agent; the page under the row until the
/// next refresh).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpResult {
    pub op: Op,
    pub ok: bool,
    /// Redacted, one line.
    pub message: String,
}

/// One agent as the UI shows it (a snapshot; cheap to clone).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: AgentId,
    pub status: Status,
    /// An op is running (or queued) on this row.
    pub busy: Option<Op>,
    pub last: Option<OpResult>,
    /// Last MCP request from this agent (meta `agent.used.<friendly>`), unix seconds.
    pub last_used: Option<Timestamp>,
}

impl AgentRow {
    /// The automated ops that make sense now, main one first. Empty while busy, for
    /// `NotFound` / `Checking`, and for agents done by hand (Cursor).
    pub fn ops(&self) -> Vec<Op> {
        if self.busy.is_some() {
            return Vec::new();
        }
        let ops: &[Op] = match &self.status {
            Status::Checking | Status::NotFound => &[],
            // Unreadable: connecting again is harmless (every Connect is idempotent)
            Status::NotConnected | Status::Unreadable(_) => &[Op::Connect],
            // Claude with only the MCP server: Connect adds the plugin (the skills)
            Status::Connected { via: Via::McpOnly, .. } => &[Op::Connect],
            Status::Connected { .. } | Status::TurnedOff => &[Op::Disconnect],
            Status::UpdateAvailable { .. } => &[Op::Update, Op::Disconnect],
            // "Repair" = connect again
            Status::NeedsRepair => &[Op::Connect, Op::Disconnect],
        };
        ops.iter().copied().filter(|op| self.id.can(*op)).collect()
    }

    /// "Connecting…" / "Updating…" / "Disconnecting…" while busy, else the status line.
    pub fn status_text(&self) -> String {
        match self.busy {
            Some(Op::Connect) => "Connecting…".into(),
            Some(Op::Update) => "Updating…".into(),
            Some(Op::Disconnect) => "Disconnecting…".into(),
            None => self.status.text(self.id),
        }
    }
}

/// The card's offer: agents found, automatable and not connected, in [`AgentId`] order. The
/// first is the primary (ticked); the others start unticked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub agents: Vec<AgentId>,
}

/// How the start-of-version pass ended (the app polls it once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassOutcome {
    /// Show the card when the gate allows; record `checked_version` once it's answered.
    Offer(Offer),
    /// Nothing to offer (agents found but all connected, unreadable or by hand, or
    /// `offer_connect` is off): record `checked_version` now.
    Settled,
    /// No agent found at all: don't record (the next start looks again — it's free).
    Retry,
}

/// Copyable setup text for a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// One line for the menu / note: "Add this to ~/.cursor/mcp.json, then approve Tuner in Cursor".
    pub what: String,
    /// The text copied (a command line or a JSON block).
    pub text: String,
    /// The file it goes in, when there is one.
    pub file: Option<PathBuf>,
}

pub const README_URL: &str = "https://github.com/rthoms334/omatuner#use-tuner-from-an-ai-agent";
/// Copy texts for the "Other agents" note.
pub const GENERIC_COMMAND: &str = "tuner mcp";
pub const VSCODE_COMMAND: &str = r#"code --add-mcp '{"name":"tuner","command":"tuner","args":["mcp"]}'"#;
/// Cursor's MCP settings with Tuner in them (the snippet).
const CURSOR_JSON: &str = "{\n  \"mcpServers\": {\n    \"tuner\": { \"command\": \"tuner\", \"args\": [\"mcp\"] }\n  }\n}";

/// The start-of-version pass is due: one of the switches is on and this version wasn't checked.
pub fn should_check(cfg: &AgentsConfig, version: &str) -> bool {
    (cfg.offer_connect || cfg.keep_in_sync) && cfg.checked_version.as_deref() != Some(version)
}

/// What the pass offers, from fresh rows.
pub fn decide(rows: &[AgentRow], offer_connect: bool) -> PassOutcome {
    if !rows.iter().any(|r| !matches!(r.status, Status::NotFound | Status::Checking)) {
        return PassOutcome::Retry;
    }
    let mut agents: Vec<AgentId> = rows.iter().filter(|r| r.status == Status::NotConnected && r.id.can(Op::Connect)).map(|r| r.id).collect();
    agents.sort();
    agents.dedup();
    if !offer_connect || agents.is_empty() { PassOutcome::Settled } else { PassOutcome::Offer(Offer { agents }) }
}

/// The setup an agent needs, to copy ("Copy the setup command"; Cursor's JSON).
pub fn snippet(id: AgentId, env: &ConnectEnv) -> Snippet {
    let run = |text: String| Snippet { what: "Run this in a terminal".into(), text, file: None };
    match id {
        AgentId::Claude => Snippet {
            what: "Run these in a terminal".into(),
            text: crate::agent::setup::steps(env.share.as_deref()).iter().map(|s| s.shell()).collect::<Vec<_>>().join("\n"),
            file: None,
        },
        AgentId::Cursor => {
            let file = agents::cursor_file(env);
            Snippet {
                what: format!("Add this to {}, then approve Tuner in Cursor", tilde(&file, &env.home)),
                text: CURSOR_JSON.into(),
                file: Some(file),
            }
        }
        _ => run(agents::op_steps(id, Op::Connect, env).first().map(|c| shell_line(&c.argv)).unwrap_or_default()),
    }
}

/// How to remove Tuner by hand where Tuner doesn't: opencode (no remove command), Cursor, and a
/// Claude Code setup with only the MCP server (`claude mcp add`; the UI offers it for
/// `Via::McpOnly` only). `None` when Disconnect is automated.
pub fn manual_disconnect(id: AgentId, env: &ConnectEnv) -> Option<Snippet> {
    manual_disconnect_in(id, env, || agents::opencode_file(env))
}

/// [`manual_disconnect`] with opencode's settings file given (it depends on which file exists).
pub(crate) fn manual_disconnect_in(id: AgentId, env: &ConnectEnv, opencode_file: impl FnOnce() -> PathBuf) -> Option<Snippet> {
    let by_hand = |key: &str, file: PathBuf| {
        let what = format!("Remove \"tuner\" under \"{key}\" in {}", tilde(&file, &env.home));
        Snippet { text: what.clone(), what, file: Some(file) }
    };
    match id {
        AgentId::OpenCode => Some(by_hand("mcp", opencode_file())),
        AgentId::Cursor => Some(by_hand("mcpServers", agents::cursor_file(env))),
        AgentId::Claude => Some(Snippet {
            what: "Remove the MCP server Claude Code has without the plugin: run this in a terminal".into(),
            text: "claude mcp remove --scope user tuner".into(),
            file: None,
        }),
        _ => None,
    }
}

/// Dotted version compare: numeric parts as numbers ("0.2.10" > "0.2.9"), others as text, a
/// missing part as 0 ("0.2" = "0.2.0"). Build suffixes after `-`/`+` are ignored.
pub fn version_lt(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    let parts = |v: &str| -> Vec<String> {
        let core = v.trim().trim_start_matches('v').split(['-', '+']).next().unwrap_or("");
        core.split('.').map(str::to_owned).collect()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).map_or("0", String::as_str), b.get(i).map_or("0", String::as_str));
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord == Ordering::Less;
        }
    }
    false
}

/// A command as a user would type it (no shell runs it; for the log and the Copy texts).
pub(crate) fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| if a.is_empty() || a.contains([' ', '\'', '"', '$', '&', ';', '|']) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.clone() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `~/…` for a path under the home directory (on screen; the full path is what's copied).
pub(crate) fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: AgentId, status: Status) -> AgentRow {
        AgentRow { id, status, busy: None, last: None, last_used: None }
    }

    #[test]
    fn the_pass_runs_once_per_version_while_a_switch_is_on() {
        let mut cfg = AgentsConfig::default();
        assert!(should_check(&cfg, "0.2.3"), "new install or upgraded from 0.2.2");
        cfg.checked_version = Some("0.2.3".into());
        assert!(!should_check(&cfg, "0.2.3"));
        assert!(should_check(&cfg, "0.2.4"), "the next upgrade checks again");
        cfg.offer_connect = false;
        assert!(should_check(&cfg, "0.2.4"), "keep-in-sync alone still syncs the plugin");
        cfg.keep_in_sync = false;
        assert!(!should_check(&cfg, "0.2.4"));
    }

    #[test]
    fn the_offer_holds_found_automatable_unconnected_agents_in_order() {
        use Status::*;
        let rows = [
            row(AgentId::Gemini, NotConnected),
            row(AgentId::Claude, Connected { via: Via::Plugin, version: Some("0.2.2".into()) }),
            row(AgentId::Codex, NotConnected),
            row(AgentId::Copilot, NotFound),
            row(AgentId::Cursor, NotConnected),
            row(AgentId::OpenCode, Unreadable("x".into())),
        ];
        assert_eq!(decide(&rows, true), PassOutcome::Offer(Offer { agents: vec![AgentId::Codex, AgentId::Gemini] }));
        assert_eq!(decide(&rows, false), PassOutcome::Settled, "Don't ask again");
        // all connected, or only by-hand / unreadable agents: nothing to ask, recorded
        assert_eq!(decide(&rows[1..2], true), PassOutcome::Settled);
        assert_eq!(decide(&[row(AgentId::Cursor, NotConnected), row(AgentId::Codex, Unreadable("x".into()))], true), PassOutcome::Settled);
        // nothing found: look again next start
        assert_eq!(decide(&[row(AgentId::Claude, NotFound), row(AgentId::Codex, NotFound)], true), PassOutcome::Retry);
        assert_eq!(decide(&[], true), PassOutcome::Retry);
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(version_lt("0.2.9", "0.2.10"));
        assert!(!version_lt("0.2.10", "0.2.9"));
        assert!(!version_lt("0.3", "0.2.9"));
        assert!(version_lt("0.2", "0.2.1"));
        assert!(!version_lt("0.2.2", "0.2.2") && !version_lt("0.2", "0.2.0"));
        assert!(version_lt("0.2.2-rc1", "0.2.3"));
        assert!(!version_lt("garbage", "garbage"));
    }

    #[test]
    fn row_ops_follow_the_status() {
        use Status::*;
        let ops = |id, s| row(id, s).ops();
        assert_eq!(ops(AgentId::Claude, NotConnected), [Op::Connect]);
        assert_eq!(ops(AgentId::Claude, Connected { via: Via::Plugin, version: None }), [Op::Disconnect]);
        assert_eq!(ops(AgentId::Claude, Connected { via: Via::McpOnly, version: None }), [Op::Connect]);
        assert_eq!(ops(AgentId::Claude, UpdateAvailable { installed: "0.2.2".into(), available: "0.2.3".into() }), [Op::Update, Op::Disconnect]);
        assert_eq!(ops(AgentId::Claude, NeedsRepair), [Op::Connect, Op::Disconnect]);
        assert_eq!(ops(AgentId::Codex, Connected { via: Via::Mcp, version: None }), [Op::Disconnect]);
        assert_eq!(ops(AgentId::Codex, TurnedOff), [Op::Disconnect]);
        assert!(ops(AgentId::OpenCode, Connected { via: Via::Mcp, version: None }).is_empty(), "removed by hand");
        assert_eq!(ops(AgentId::OpenCode, NotConnected), [Op::Connect]);
        assert!(ops(AgentId::Cursor, NotConnected).is_empty());
        assert!(ops(AgentId::Codex, NotFound).is_empty() && ops(AgentId::Codex, Checking).is_empty());
        let busy = AgentRow { busy: Some(Op::Connect), ..row(AgentId::Codex, NotConnected) };
        assert!(busy.ops().is_empty());
        assert_eq!(busy.status_text(), "Connecting…");
    }

    #[test]
    fn copy_texts_name_the_exact_setup() {
        let env = ConnectEnv::rooted(Path::new("/home/u"));
        let claude = snippet(AgentId::Claude, &env).text;
        assert!(claude.starts_with("claude plugin marketplace add rthoms334/omatuner\n"), "{claude}");
        let cursor = snippet(AgentId::Cursor, &env);
        assert_eq!(cursor.file.as_deref(), Some(Path::new("/home/u/.cursor/mcp.json")));
        assert_eq!(cursor.what, "Add this to ~/.cursor/mcp.json, then approve Tuner in Cursor");
        let v: serde_json::Value = serde_json::from_str(&cursor.text).unwrap();
        assert_eq!(v["mcpServers"]["tuner"]["args"][0], "mcp");
        let oc = manual_disconnect(AgentId::OpenCode, &env).unwrap();
        assert_eq!(oc.what, "Remove \"tuner\" under \"mcp\" in ~/.config/opencode/opencode.jsonc");
        assert!(manual_disconnect(AgentId::Codex, &env).is_none());
    }
}
