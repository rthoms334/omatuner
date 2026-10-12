//! Settings ▸ AI agents (DESIGN §39): connect the AI agents on this computer to Tuner, see how
//! they stand, and the first-run card's switch.
//!
//! ```text
//! AI AGENTS
//! Let an AI agent find, play and plan what to watch in Tuner. Last used by Claude, 2 h ago.
//! Offer to connect agents after updates                              [ On  ]
//! Keep Claude Code's plugin up to date after Tuner updates           [ On  ]
//! Agents play in                                            ‹ Pop-out ›
//! AGENTS ON THIS COMPUTER
//! ↻ Check again
//! 󰚩 Claude Code   Connected · plugin 0.2.2 · used 2 h ago
//! 󰚩 Codex         Not connected
//! 󰚩 Gemini CLI    Connecting…
//! Not found on this computer: Copilot CLI.
//! OTHER AGENTS
//! VS Code, Zed and any other MCP agent: add the command tuner mcp in its MCP settings.
//!  Copy "tuner mcp" ·  Copy the VS Code command ·  Copy the README link
//! ```
//! Enter (or a click, `x`, the gear) on an agent opens its menu: Connect / Repair, Update,
//! Disconnect… (a destructive confirm), and the Copy actions (the setup command, Cursor's JSON
//! and its file, how to remove it by hand, the README link). A busy row has no menu. Every
//! change runs in the background (`AgentConnect::run`) and toasts its result; the rows are the
//! service's snapshot, read each frame. Copy puts the text on the clipboard in the next `ui`.

use super::*;
use crate::agent::connect::{self, AgentId, AgentRow, Op, Status, Via};

/// `Act::Feature(Feature::Agents, n)`.
const ACT_CHECK: u16 = 0;
const ACT_COPY_GENERIC: u16 = 1;
const ACT_COPY_VSCODE: u16 = 2;
const ACT_COPY_README: u16 = 3;

/// The agent menu's codes (`Action::Digit`; kept clear of the page's own `MENU_*`).
const MENU_CONNECT: u8 = 40;
const MENU_UPDATE: u8 = 41;
const MENU_DISCONNECT: u8 = 42;
const MENU_COPY_SETUP: u8 = 43;
const MENU_COPY_FILE: u8 = 44;
const MENU_COPY_REMOVE: u8 = 45;
const MENU_COPY_README: u8 = 46;

/// The page's state, kept in `SettingsScreen`.
#[derive(Default)]
pub(super) struct State {
    /// Text to put on the clipboard in the next `ui` (`egui::Context::copy_text`).
    pub(super) copy: Option<String>,
}

static TOGGLES: [CfgToggle; 2] = [
    CfgToggle { label: "Offer to connect agents after updates", get: |c| c.agents.offer_connect, set: |c, v| c.agents.offer_connect = v },
    CfgToggle {
        label: "Keep Claude Code's plugin up to date after Tuner updates",
        get: |c| c.agents.keep_in_sync,
        set: |c, v| c.agents.keep_in_sync = v,
    },
];

/// Where an agent's play opens (DESIGN §30.13): "Pop-out" — Tuner's window when it's on
/// screen, the pop-out when it's out of sight — or always Tuner's window. A play's own
/// `open_in` wins ("put it on the TV"); a hand-written `where-i-am` acts as, and shows as, the
/// pop-out.
static PLAY_IN: CfgChoice = CfgChoice {
    label: "Agents play in",
    options: &["Pop-out", "Tuner window"],
    get: |c| usize::from(c.agents.play_in == crate::model::OpenIn::FullWindow),
    set: |c, i| c.agents.play_in = [crate::model::OpenIn::PopOut, crate::model::OpenIn::FullWindow][i],
};

/// A row's id is the agent's place in `AgentId::ALL`.
fn row_id(id: AgentId) -> i64 {
    AgentId::ALL.iter().position(|a| *a == id).unwrap_or(0) as i64
}

fn agent_of(id: i64) -> Option<AgentId> {
    usize::try_from(id).ok().and_then(|i| AgentId::ALL.get(i)).copied()
}

fn row_for(ctx: &UiCtx<'_>, id: AgentId) -> Option<AgentRow> {
    ctx.services.agent_connect.rows().into_iter().find(|r| r.id == id)
}

/// "Connected · plugin 0.2.2 · used 2 h ago".
fn detail(r: &AgentRow, now: crate::model::Timestamp) -> String {
    let mut s = r.status_text();
    if let Some(at) = r.last_used {
        s.push_str(&format!(" · used {}", super::super::live::age_label(now, at)));
    }
    s
}

/// "Remove …" → "remove …" (inside a sentence).
fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// "Claude Code, Codex and Gemini CLI".
fn and_list(names: &[&str]) -> String {
    joined(names, "and")
}

/// "Claude Code, Codex or Gemini CLI".
fn or_list(names: &[&str]) -> String {
    joined(names, "or")
}

fn joined(names: &[&str], word: &str) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} {word} {last}", init.join(", ")),
    }
}

/// The AI agents page's rows.
pub(super) fn rows(_s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    let svc = &ctx.services.agent_connect;
    let now = ctx.now;
    v.push(Row::Section("AI agents".into()));
    let mut intro = "Let an AI agent find, play and plan what to watch in Tuner. Connect adds Tuner's MCP server (tuner mcp) to the agent's own settings, using the agent's own command; Claude Code gets Tuner's plugin with its skills.".to_owned();
    if let Some((name, at)) = svc.last_used() {
        intro.push_str(&format!(" Last used by {name}, {}.", super::super::live::age_label(now, at)));
    }
    v.push(Row::Note(intro, false));
    let missing = svc.missing();
    // the plugin switch only matters with Claude Code here
    let toggles = if missing.contains(&AgentId::Claude) { &TOGGLES[..1] } else { &TOGGLES[..] };
    v.extend(toggles.iter().map(Row::Toggle));
    v.push(Row::Choice(&PLAY_IN));
    if !crate::platform::hypr::is_hyprland() {
        v.push(Row::Note("The pop-out needs Hyprland: agents play in Tuner's window here.".into(), false));
    }

    let rows = svc.rows();
    let env = svc.env();
    v.push(Row::Section(if svc.refreshing() { "Agents on this computer · checking…".into() } else { "Agents on this computer".into() }));
    v.push(Row::Act { act: Act::Feature(Feature::Agents, ACT_CHECK), icon: glyphs::REFRESH, label: "Check again".into(), danger: false });
    for r in &rows {
        v.push(Row::Item {
            item: Item::Feature(Feature::Agents, row_id(r.id)),
            icon: glyphs::AGENT.to_owned(),
            color: None,
            label: r.id.label().to_owned(),
            detail: Some(detail(r, now)),
            reorder: false,
        });
        // a failure stays under its row until the agent's next change; an unreadable status
        // in full (the row's line is cut at the width)
        if r.busy.is_none() {
            if let Some(l) = r.last.as_ref().filter(|l| !l.ok) {
                v.push(Row::Note(format!("Couldn't {} {}: {}", l.op.verb(), r.id.label(), l.message), true));
            } else if let Status::Unreadable(why) = &r.status {
                v.push(Row::Note(format!("Couldn't check {}: {why}", r.id.label()), true));
            }
        }
        // what's done by hand, and that the row's menu copies it
        let by_hand = match (r.id, r.status.connected()) {
            (AgentId::Cursor, false) if r.status == Status::NotConnected => {
                let file = connect::snippet(r.id, &env).file.map(|f| connect::tilde(&f, &env.home)).unwrap_or_default();
                Some(format!("Cursor: add Tuner to {file}, then approve it in Cursor. The row's menu copies the setup."))
            }
            (AgentId::OpenCode | AgentId::Cursor, true) => {
                svc.manual_disconnect(r.id, &env).map(|s| format!("To disconnect it: {} — the row's menu copies it.", lower_first(&s.what)))
            }
            _ => None,
        };
        if let Some(what) = by_hand {
            v.push(Row::Note(what, false));
        }
    }
    if rows.is_empty() && svc.checked() {
        let all: Vec<&str> = AgentId::ALL.iter().map(|a| a.label()).collect();
        v.push(Row::Note(
            format!("Tuner didn't find {} on this computer. Install one, then Check again, or set up another agent below.", or_list(&all)),
            false,
        ));
    } else if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|a| a.label()).collect();
        v.push(Row::Note(format!("Not found on this computer: {}.", and_list(&names)), false));
    }

    v.push(Row::Section("Other agents".into()));
    v.push(Row::Note(
        "VS Code, Zed and any other MCP agent: add the command tuner mcp in its MCP settings. The README shows the setup for each agent.".into(),
        false,
    ));
    let copy = |n: u16, label: &str| Row::Act { act: Act::Feature(Feature::Agents, n), icon: glyphs::COPY, label: label.into(), danger: false };
    v.push(copy(ACT_COPY_GENERIC, "Copy \u{201c}tuner mcp\u{201d}"));
    v.push(copy(ACT_COPY_VSCODE, "Copy the VS Code command"));
    v.push(copy(ACT_COPY_README, "Copy the README link"));
}

/// Put `text` on the clipboard (in the next `ui`) and say so.
fn copy(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, text: String, what: &str) {
    s.agents.copy = Some(text);
    ctx.toast(Toast::info(format!("Copied {what}")));
}

/// Its `Act::Feature(Feature::Agents, n)` buttons.
pub(super) fn act(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    match n {
        ACT_CHECK => ctx.services.agent_connect.refresh(),
        ACT_COPY_GENERIC => copy(s, ctx, connect::GENERIC_COMMAND.into(), "\u{201c}tuner mcp\u{201d}"),
        ACT_COPY_VSCODE => copy(s, ctx, connect::VSCODE_COMMAND.into(), "the VS Code command"),
        ACT_COPY_README => copy(s, ctx, connect::README_URL.into(), "the README link"),
        _ => {}
    }
}

/// Enter on an agent's row: its menu (none while it's busy).
pub(super) fn item(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) {
    s.item_menu(ctx, Item::Feature(Feature::Agents, id));
}

/// An agent's menu (title, items): its automated ops, then the Copy actions.
pub(super) fn menu(ctx: &UiCtx<'_>, id: i64) -> Option<(String, Vec<MenuItem>)> {
    let r = row_for(ctx, agent_of(id)?)?;
    if r.busy.is_some() {
        return None;
    }
    let mi = |icon: &'static str, label: String, code: u8| MenuItem { icon, label, action: Action::Digit(code), enabled: true, shortcut: None };
    let mut items = Vec::new();
    for op in r.ops() {
        items.push(match (op, &r.status) {
            (Op::Connect, Status::NeedsRepair) => mi(glyphs::REFRESH, "Repair (connect again)".into(), MENU_CONNECT),
            (Op::Connect, Status::Connected { via: Via::McpOnly, .. }) => mi(glyphs::ADD, "Add Tuner's plugin (the skills)".into(), MENU_CONNECT),
            (Op::Connect, _) => mi(glyphs::ADD, "Connect".into(), MENU_CONNECT),
            (Op::Update, Status::UpdateAvailable { available, .. }) => mi(glyphs::REFRESH, format!("Update to {available}"), MENU_UPDATE),
            (Op::Update, _) => mi(glyphs::REFRESH, "Update".into(), MENU_UPDATE),
            (Op::Disconnect, _) => mi(glyphs::CLOSE, "Disconnect…".into(), MENU_DISCONNECT),
        });
    }
    let env = ctx.services.agent_connect.env();
    if r.id == AgentId::Cursor {
        items.push(mi(glyphs::COPY, "Copy the setup (JSON)".into(), MENU_COPY_SETUP));
        if connect::snippet(r.id, &env).file.is_some() {
            items.push(mi(glyphs::COPY, "Copy the file's path".into(), MENU_COPY_FILE));
        }
    } else {
        let label = if r.id == AgentId::Claude { "Copy the setup commands" } else { "Copy the setup command" };
        items.push(mi(glyphs::COPY, label.into(), MENU_COPY_SETUP));
    }
    match (r.id, &r.status) {
        (AgentId::Claude, Status::Connected { via: Via::McpOnly, .. }) => {
            items.push(mi(glyphs::COPY, "Copy the command that removes it".into(), MENU_COPY_REMOVE));
        }
        (AgentId::OpenCode | AgentId::Cursor, st) if st.connected() => items.push(mi(glyphs::COPY, "Copy how to remove it".into(), MENU_COPY_REMOVE)),
        _ => {}
    }
    items.push(mi(glyphs::COPY, "Copy the README link".into(), MENU_COPY_README));
    Some((r.id.label().to_owned(), items))
}

/// A choice in an agent's menu.
pub(super) fn menu_choice(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64, code: u8) {
    let Some(agent) = agent_of(id) else { return };
    let env = ctx.services.agent_connect.env();
    match code {
        MENU_CONNECT => ctx.services.agent_connect.run(&[agent], Op::Connect),
        MENU_UPDATE => ctx.services.agent_connect.run(&[agent], Op::Update),
        MENU_DISCONNECT => {
            let name = agent.label();
            let body = if agent == AgentId::Claude {
                "Claude Code loses Tuner's tools and the omatuner skills (Tuner's plugin and its marketplace are removed from Claude Code). Restart Claude Code afterwards. You can connect it again here.".to_owned()
            } else {
                format!("{name} loses Tuner's tools (the tuner server is removed from its settings). You can connect it again here.")
            };
            let token = s.token(Pending::Feature(Feature::Agents, id));
            ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                title: format!("Disconnect {name}?"),
                body,
                confirm_label: "Disconnect".into(),
                destructive: true,
                token,
            }));
        }
        MENU_COPY_SETUP => {
            let what = match agent {
                AgentId::Cursor => "Cursor's setup",
                AgentId::Claude => "the setup commands",
                _ => "the setup command",
            };
            copy(s, ctx, connect::snippet(agent, &env).text, what);
        }
        MENU_COPY_FILE => {
            if let Some(f) = connect::snippet(agent, &env).file {
                copy(s, ctx, f.display().to_string(), "the file's path");
            }
        }
        MENU_COPY_REMOVE => {
            if let Some(sn) = ctx.services.agent_connect.manual_disconnect(agent, &env) {
                copy(s, ctx, sn.text, "how to remove it");
            }
        }
        MENU_COPY_README => copy(s, ctx, connect::README_URL.into(), "the README link"),
        _ => {}
    }
}

/// The Disconnect confirm answered Disconnect.
pub(super) fn confirmed(ctx: &mut UiCtx<'_>, id: i64) {
    if let Some(agent) = agent_of(id) {
        ctx.services.agent_connect.run(&[agent], Op::Disconnect);
    }
}

/// The page became current: look again unless that was a moment ago.
pub(super) fn enter(_s: &mut SettingsScreen, ctx: &UiCtx<'_>) {
    ctx.services.agent_connect.refresh_if_stale();
}

/// Every frame the screen draws: hand a Copy to the clipboard.
pub(super) fn frame(s: &mut SettingsScreen, ui: &egui::Ui) {
    if let Some(text) = s.agents.copy.take() {
        ui.ctx().copy_text(text);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use egui_kittest::Harness;

    use super::*;
    use crate::agent::connect::ConnectEnv;
    use crate::agent::connect::runner::CmdOutput;
    use crate::agent::connect::runner::fake::FakeRunner;
    use crate::ui::screens::home::testkit::{App, harness, has_label, has_label_containing, rig, until};

    const CLAUDE_LIST: &str = r#"[{"id":"omatuner@omatuner","version":"0.2.2","scope":"user","enabled":true,"readFromFolder":"/usr/share/tuner/claude/plugins/omatuner","folderVersion":"0.2.2","mcpServers":{"tuner":{"command":"tuner","args":["mcp"]}}}]"#;

    fn codex_get(connected: bool) -> Result<CmdOutput, crate::agent::connect::runner::RunError> {
        Ok(if connected {
            CmdOutput {
                code: Some(0),
                stdout: r#"{"name":"tuner","enabled":true,"transport":{"type":"stdio","command":"tuner","args":["mcp"]}}"#.into(),
                stderr: String::new(),
            }
        } else {
            CmdOutput { code: Some(1), stdout: String::new(), stderr: "Error: No MCP server named 'tuner' found.".into() }
        })
    }

    /// Enter on an agent's row; the menu it opened.
    fn menu_of(h: &mut Harness<'static, App<SettingsScreen>>, label: &str) -> ContextMenuSpec {
        let app = h.state_mut();
        let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
        app.s.row = rows.iter().position(|r| matches!(r, Row::Item { label: l, .. } if l == label)).unwrap();
        app.s.pane = Pane::Content;
        app.act(Action::Activate);
        h.run_steps(1);
        let nav = h.state_mut().nav();
        nav.into_iter()
            .find_map(|n| match n {
                crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(m)) => Some(m),
                _ => None,
            })
            .expect("the row's menu")
    }

    /// Choose `label` in the menu `m` (the page opened it, so the answer comes back to it).
    fn choose(h: &mut Harness<'static, App<SettingsScreen>>, m: &ContextMenuSpec, label: &str) {
        let it = m.items.iter().find(|i| i.label == label).unwrap_or_else(|| panic!("{label} in {:?}", m.items));
        h.state_mut().result(OverlayResult::Chosen(it.action));
    }

    /// The token of the confirm the page opened.
    fn confirm(h: &mut Harness<'static, App<SettingsScreen>>) -> ConfirmSpec {
        h.state_mut()
            .nav()
            .into_iter()
            .find_map(|n| match n {
                crate::ui::NavRequest::Open(OverlayRequest::Confirm(c)) => Some(c),
                _ => None,
            })
            .expect("a confirm")
    }

    /// The page lists the agents found with their status and last use, the missing ones in a
    /// note; Codex connects from its menu, Disconnect asks first (Cancel runs nothing); Cursor's
    /// setup is copied to the clipboard.
    #[test]
    fn agents_are_listed_and_codex_connects_and_disconnects_from_its_menu() {
        let (t, one) = rig();
        let home = tempfile::tempdir().unwrap();
        let fake = Arc::new(
            FakeRunner::default()
                .found(&["claude", "codex", "cursor-agent"])
                .on_ok(&["claude", "plugin", "list"], CLAUDE_LIST)
                .on(&["codex", "mcp", "get"], codex_get(false))
                .on_ok(&["codex", "mcp", "add"], "Added global MCP server 'tuner'.")
                .on_ok(&["codex", "mcp", "remove"], "Removed global MCP server 'tuner'."),
        );
        let svc = t.services().agent_connect.clone();
        svc.with_fake(fake.clone(), ConnectEnv::rooted(home.path()));
        let used = t.now - 2 * 3600;
        t.services().store.writer().blocking(move |c| crate::agent::connect::usage::write(c, "Claude", used)).unwrap();
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1600.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Agents));
        until(&mut h, "the agents", |h| has_label(h, "Not connected"));
        assert!(has_label(&h, "Connected · plugin 0.2.2 · used 2 h ago"), "Claude Code");
        assert!(has_label(&h, "Not connected · add it by hand"), "Cursor");
        assert!(has_label_containing(&h, "Last used by Claude, 2 h ago."));
        assert!(has_label(&h, "Not found on this computer: Gemini CLI, Copilot CLI and opencode."));
        // the page fits at 790 and 1600 pt with 12 and 16 px text
        for (w, scale) in [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)] {
            use egui_kittest::kittest::Queryable;
            let base = crate::theme::Theme::fallback();
            let theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.state_mut().t.layout = crate::ui::LayoutClass::from_width(w, theme.rem());
            h.state_mut().t.theme = theme;
            h.set_size(egui::vec2(w, 1000.0));
            h.run_steps(3);
            for node in h.query_all_by_label_contains("") {
                let r = node.rect();
                assert!(r.left() >= -1.0 && r.right() <= w + 1.0, "at {w} pt × {scale}: {node:?}");
            }
        }
        h.state_mut().t.layout = crate::ui::LayoutClass::Wide;
        h.set_size(egui::vec2(1600.0, 1000.0));
        h.run_steps(2);

        // Connect from Codex's menu
        let m = menu_of(&mut h, "Codex");
        assert_eq!(m.items[0].label, "Connect");
        fake.set(&["codex", "mcp", "get"], codex_get(true));
        choose(&mut h, &m, "Connect");
        assert!(svc.wait_idle(Duration::from_secs(10)));
        until(&mut h, "Codex connected", |h| has_label(h, "Connected"));
        assert!(fake.lines().iter().any(|l| l == "codex mcp add tuner -- tuner mcp"), "{:?}", fake.lines());
        assert!(h.state().t.toasts.iter().any(|t| t.text.starts_with("Codex is connected to Tuner")), "{:?}", h.state().t.toasts);

        // Disconnect… asks first; Cancel runs nothing
        let m = menu_of(&mut h, "Codex");
        choose(&mut h, &m, "Disconnect…");
        let c = confirm(&mut h);
        assert_eq!(c.title, "Disconnect Codex?");
        assert!(c.destructive);
        h.state_mut().result(OverlayResult::Cancelled(c.token));
        assert!(svc.wait_idle(Duration::from_secs(10)));
        assert!(!fake.lines().iter().any(|l| l.contains("remove")));
        // … and Disconnect removes it
        let m = menu_of(&mut h, "Codex");
        choose(&mut h, &m, "Disconnect…");
        let c = confirm(&mut h);
        fake.set(&["codex", "mcp", "get"], codex_get(false));
        h.state_mut().result(OverlayResult::Confirmed(c.token));
        assert!(svc.wait_idle(Duration::from_secs(10)));
        until(&mut h, "Codex disconnected", |h| has_label(h, "Not connected"));
        assert!(fake.lines().iter().any(|l| l == "codex mcp remove tuner"), "{:?}", fake.lines());

        // Cursor is by hand: its JSON goes to the clipboard
        let m = menu_of(&mut h, "Cursor");
        assert!(!m.items.iter().any(|i| i.label == "Connect"), "{:?}", m.items);
        choose(&mut h, &m, "Copy the setup (JSON)");
        h.run_steps(1);
        let copied: Vec<String> = h
            .output()
            .platform_output
            .commands
            .iter()
            .filter_map(|c| if let egui::OutputCommand::CopyText(t) = c { Some(t.clone()) } else { None })
            .collect();
        assert!(copied.iter().any(|t| t.contains("\"mcpServers\"") && t.contains("\"tuner\"")), "{copied:?}");
    }
}
