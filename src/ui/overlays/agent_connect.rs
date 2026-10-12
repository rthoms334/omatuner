//! The first-run card: "Connect Claude Code to Tuner?" (DESIGN §39). Opened by the app once
//! per Tuner version when the start-of-version pass found agents on PATH that aren't connected
//! yet (`agent::connect::decide`), and only when nothing else is going on (`app`'s
//! `agent_card_may_show`: after onboarding, never over the player, fullscreen, the pop-out,
//! multi-view, another dialog or a text field).
//!
//! ```text
//!  Connect your AI agents to Tuner?
//!  Tuner can add its MCP server to these agents …
//!
//!  󰄲 Claude Code      plugin: tools + 7 skills
//!  󰄱 Codex            MCP server
//!  󰄱 Don't ask again
//!                                         [ Not now ]  [ Connect Claude Code ]
//!  Enter Connect · Esc Not now · Tab moves
//! ```
//! Only the first agent (the primary, in `AgentId` order) starts ticked. Keys are the dialog
//! keys (`Modal`; no new bindings): ↑/↓/←/→ and Tab/Shift+Tab move along tick rows → "Don't
//! ask again" → Not now → Connect (stopping at the ends), Enter/Space toggles a tick or presses
//! the focused button, Esc is Not now. Mouse: a row or its box toggles, the buttons click, a
//! click outside is Not now. The card opens by itself, so for its first [`ARMING`] Enter, Space
//! and clicks do nothing (a key meant for the list behind can't connect an agent); Esc works.
//!
//! Answers (config through `ctx.edit_config`): Connect runs the setup in the background
//! (`AgentConnect::run`, one queue) and records `checked_version`; the card stays with a busy
//! bar and a line per agent until every ticked agent is idle, then closes (the service's toasts
//! carry the results; Esc hides it early, the work goes on). Not now records `checked_version`
//! only, so it asks again after the next upgrade; "Don't ask again" turns `offer_connect` off
//! (Settings ▸ AI agents shows that switch).

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText};

use super::dialogs::{card_area, scrim, title};
use super::{Overlay, OverlayOutcome};
use crate::agent::connect::{AgentId, AgentRow, Offer, Op, OpResult};
use crate::keymap::{Action, KeyContext};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::list::paint_row_state;
use crate::ui::widgets::progress::busy_bar;
use crate::ui::widgets::{RowState, glyphs};

/// The version the card's answer is recorded for (`AgentsConfig::checked_version`).
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// After the card appears, Enter, Space and clicks wait this long.
const ARMING: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// An agent's tick row (several agents only).
    Agent(usize),
    DontAsk,
    NotNow,
    Connect,
}

/// Connect was pressed: the agents being connected and what their rows said before, so the
/// card knows when each one is done (busy seen and gone, or a new result).
struct Running {
    ids: Vec<AgentId>,
    before: Vec<Option<OpResult>>,
    seen_busy: Vec<bool>,
}

pub struct AgentConnectCard {
    offer: Offer,
    pub(crate) ticked: Vec<bool>,
    pub(crate) dont_ask: bool,
    focus: Focus,
    running: Option<Running>,
    /// When the card was first drawn (`None`: not yet).
    opened: Option<Instant>,
}

impl AgentConnectCard {
    pub fn new(offer: Offer) -> Self {
        // only the primary (first) agent starts ticked
        let ticked = (0..offer.agents.len()).map(|i| i == 0).collect();
        Self { offer, ticked, dont_ask: false, focus: Focus::Connect, running: None, opened: None }
    }

    /// Enter, Space and clicks count (the card has been up for [`ARMING`]).
    fn armed(&self) -> bool {
        self.opened.is_some_and(|t| t.elapsed() >= ARMING)
    }

    fn several(&self) -> bool {
        self.offer.agents.len() > 1
    }

    fn chosen(&self) -> Vec<AgentId> {
        self.offer.agents.iter().zip(&self.ticked).filter(|(_, t)| **t).map(|(a, _)| *a).collect()
    }

    fn focus_order(&self) -> Vec<Focus> {
        let mut v: Vec<Focus> = if self.several() { (0..self.offer.agents.len()).map(Focus::Agent).collect() } else { Vec::new() };
        v.extend([Focus::DontAsk, Focus::NotNow, Focus::Connect]);
        v
    }

    /// Move the focus; it stops at the ends.
    fn step(&mut self, by: isize) {
        let order = self.focus_order();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(order.len() - 1) as isize;
        self.focus = order[(at + by).clamp(0, order.len() as isize - 1) as usize];
    }

    fn toggle(&mut self, f: Focus) {
        match f {
            Focus::Agent(i) => {
                if let Some(t) = self.ticked.get_mut(i) {
                    *t = !*t;
                }
            }
            Focus::DontAsk => self.dont_ask = !self.dont_ask,
            Focus::NotNow | Focus::Connect => {}
        }
    }

    /// Not now (button, Esc, a click outside): ask again after the next upgrade — or never,
    /// with "Don't ask again" ticked.
    fn not_now(&self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let never = self.dont_ask;
        ctx.edit_config(move |c| {
            c.agents.checked_version = Some(VERSION.to_owned());
            if never {
                c.agents.offer_connect = false;
            }
        });
        OverlayOutcome::Close
    }

    fn connect(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let ids = self.chosen();
        if ids.is_empty() {
            return OverlayOutcome::Keep;
        }
        let never = self.dont_ask;
        ctx.edit_config(move |c| {
            c.agents.checked_version = Some(VERSION.to_owned());
            if never {
                c.agents.offer_connect = false;
            }
        });
        let rows = ctx.services.agent_connect.rows();
        let before = ids.iter().map(|id| row_of(&rows, *id).and_then(|r| r.last.clone())).collect();
        ctx.services.agent_connect.run(&ids, Op::Connect);
        self.running = Some(Running { seen_busy: vec![false; ids.len()], ids, before });
        OverlayOutcome::Keep
    }

    /// While connecting: note which agents were seen busy; true once every one is done.
    fn settled(&mut self, rows: &[AgentRow]) -> bool {
        let Some(r) = &mut self.running else { return false };
        let mut done = true;
        for (i, id) in r.ids.iter().enumerate() {
            match row_of(rows, *id) {
                Some(row) if row.busy.is_some() => {
                    r.seen_busy[i] = true;
                    done = false;
                }
                Some(row) => done &= r.seen_busy[i] || row.last != r.before[i],
                // gone from the list (no longer found): nothing more will happen to it
                None => {}
            }
        }
        done
    }
}

fn row_of(rows: &[AgentRow], id: AgentId) -> Option<&AgentRow> {
    rows.iter().find(|r| r.id == id)
}

/// "Connect Claude Code to Tuner?" / "Connect your AI agents to Tuner?".
fn card_title(offer: &Offer) -> String {
    match offer.agents.as_slice() {
        [one] => format!("Connect {} to Tuner?", one.label()),
        _ => "Connect your AI agents to Tuner?".to_owned(),
    }
}

/// What connecting does, in a sentence or three.
fn card_body(offer: &Offer) -> String {
    match offer.agents.as_slice() {
        [AgentId::Claude] => "Tuner runs Claude Code's own plugin commands to add Tuner's plugin to your Claude Code settings: Tuner's tools and 7 skills (/omatuner:watch, pick, series, plan, kids, fix, discover), so you can ask Claude Code to find, play and plan what to watch. Restart Claude Code afterwards. Settings ▸ AI agents undoes it.".to_owned(),
        [one] => {
            let name = one.label();
            let mut s =
                format!("It adds Tuner's MCP server (tuner mcp) to {name}'s settings, so {name} can find, play and plan what to watch for you.");
            if *one == AgentId::Gemini {
                s.push_str(" Gemini uses it in folders you trust.");
            }
            // Tuner can't take opencode's back out
            s.push_str(if *one == AgentId::OpenCode { " To remove it later, Settings ▸ AI agents shows how." } else { " Settings ▸ AI agents undoes it." });
            s
        }
        many => {
            let mut s = "Tuner can add its MCP server to these agents' settings (using each agent's own command) so they can find, play and plan what to watch for you.".to_owned();
            if many.contains(&AgentId::Claude) {
                s.push_str(" Claude Code also gets Tuner's 7 skills.");
            }
            s.push_str(if many.contains(&AgentId::OpenCode) { " Settings ▸ AI agents undoes it (opencode by hand)." } else { " Settings ▸ AI agents undoes it." });
            s
        }
    }
}

/// "plugin: tools + 7 skills" / "MCP server" next to an agent's tick.
fn what_it_gets(id: AgentId) -> &'static str {
    if id == AgentId::Claude { "plugin: tools + 7 skills" } else { "MCP server" }
}

/// "Connect Claude Code" with one ticked of several, "Connect 2 agents", "Connect" for one.
fn connect_label(offer: &Offer, chosen: &[AgentId]) -> String {
    match chosen {
        _ if offer.agents.len() == 1 => "Connect".to_owned(),
        [one] => format!("Connect {}", one.label()),
        [] => "Connect".to_owned(),
        many => format!("Connect {} agents", many.len()),
    }
}

/// "Claude Code — connecting…" / "✓ connected" / "✗ <message>" while Connect runs.
fn progress_line(id: AgentId, row: Option<&AgentRow>, before: &Option<OpResult>) -> (String, bool) {
    let name = id.label();
    match row {
        Some(r) if r.busy.is_some() => (format!("{name} — connecting…"), false),
        Some(AgentRow { last: Some(l), .. }) if Some(l) != before.as_ref() => {
            if l.ok {
                (format!("{name} — ✓ connected"), false)
            } else {
                (format!("{name} — ✗ {}", l.message), true)
            }
        }
        _ => (format!("{name} — waiting…"), false),
    }
}

/// The dialog keys for the hint line (from the `Modal` table, not hard-coded).
fn key(action: Action) -> String {
    crate::keymap::MODAL.iter().find(|b| b.action == action).map(|b| b.chord.label()).unwrap_or_default()
}

/// A tick row: box, label and (optional) secondary text; the focus ring when focused.
fn tick_row(ui: &mut egui::Ui, theme: &Theme, ticked: bool, label: &str, detail: Option<&str>, focused: bool) -> egui::Response {
    let t = &theme.tokens;
    let h = theme.space(SpaceToken::ControlHeight);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, ticked, label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    paint_row_state(p, rect, theme, if focused || resp.hovered() { RowState::Cursor } else { RowState::Idle });
    if focused {
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, t.border_hover), egui::StrokeKind::Inside);
    }
    let inset = theme.space(SpaceToken::Md);
    let y = rect.center().y;
    let (tick, tick_color) = if ticked { (glyphs::CHECKBOX_ON, t.accent) } else { (glyphs::CHECKBOX_OFF, t.text_secondary) };
    let tick_r =
        p.text(egui::pos2(rect.left() + inset, y), Align2::LEFT_CENTER, tick, egui::FontId::proportional(theme.font(TypeToken::Icon)), tick_color);
    let left = tick_r.right() + theme.space(SpaceToken::Md);
    let color = if focused { t.list_cursor_text } else { t.fg };
    let label_r = p.text(egui::pos2(left, y), Align2::LEFT_CENTER, label, egui::FontId::proportional(theme.font(TypeToken::Body)), color);
    if let Some(d) = detail {
        // a fixed column keeps the agents' details aligned
        let col = left.max(label_r.right() + theme.space(SpaceToken::Lg)).max(left + (150.0 * theme.rem()).round());
        p.text(egui::pos2(col, y), Align2::LEFT_CENTER, d, egui::FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_secondary);
    }
    resp
}

impl Overlay for AgentConnectCard {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        if self.running.is_some() {
            // connecting: Enter (Hide) and Esc close the card, the work goes on
            return match action {
                Action::Activate | Action::Back => OverlayOutcome::Close,
                Action::Up | Action::Down | Action::Left | Action::Right | Action::FocusNext | Action::FocusPrev => OverlayOutcome::Keep,
                _ => OverlayOutcome::Ignored,
            };
        }
        match action {
            Action::Up | Action::Left | Action::FocusPrev => {
                self.step(-1);
                OverlayOutcome::Keep
            }
            Action::Down | Action::Right | Action::FocusNext => {
                self.step(1);
                OverlayOutcome::Keep
            }
            // too soon: a key meant for what was there before the card
            Action::Activate if !self.armed() => OverlayOutcome::Keep,
            Action::Activate => match self.focus {
                f @ (Focus::Agent(_) | Focus::DontAsk) => {
                    self.toggle(f);
                    OverlayOutcome::Keep
                }
                Focus::NotNow => self.not_now(ctx),
                Focus::Connect => self.connect(ctx),
            },
            Action::Back => self.not_now(ctx),
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let opened = *self.opened.get_or_insert_with(Instant::now);
        let armed = self.armed();
        if !armed {
            egui_ctx.request_repaint_after(ARMING.saturating_sub(opened.elapsed()));
        }
        let rows = if self.running.is_some() { ctx.services.agent_connect.rows() } else { Vec::new() };
        if self.running.is_some() {
            if self.settled(&rows) {
                return OverlayOutcome::Close;
            }
            egui_ctx.request_repaint_after(Duration::from_millis(100));
        }
        if scrim(egui_ctx, theme, "agent-connect") && armed {
            return if self.running.is_some() { OverlayOutcome::Close } else { self.not_now(ctx) };
        }
        let chosen = self.chosen();
        let several = self.several();
        let mut clicked_tick = None;
        let (mut not_now, mut connect, mut hide) = (false, false, false);
        card_area(egui_ctx, theme, "agent-connect", CardKind::Popup, 560.0 * theme.rem(), None, |ui| {
            title(ui, theme, &card_title(&self.offer), Some(&card_body(&self.offer)));
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xxs);
            if let Some(r) = &self.running {
                for (id, before) in r.ids.iter().zip(&r.before) {
                    let (line, bad) = progress_line(*id, row_of(&rows, *id), before);
                    ui.label(RichText::new(line).size(theme.font(TypeToken::Body)).color(if bad { t.urgent } else { t.fg }));
                }
                ui.add_space(theme.space(SpaceToken::Sm));
                busy_bar(ui, theme, 3.0);
                ui.add_space(theme.space(SpaceToken::Lg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    hide = button(ui, theme, None, "Hide", ButtonStyle { bordered: true, focused: true, ..Default::default() }).clicked();
                });
                ui.add_space(theme.space(SpaceToken::Sm));
                let hint = format!("{} or {} hide · it goes on in the background", key(Action::Activate), key(Action::Back));
                ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                return;
            }
            if several {
                for (i, id) in self.offer.agents.iter().enumerate() {
                    if tick_row(ui, theme, self.ticked[i], id.label(), Some(what_it_gets(*id)), self.focus == Focus::Agent(i)).clicked() {
                        clicked_tick = Some(Focus::Agent(i));
                    }
                }
            }
            if tick_row(ui, theme, self.dont_ask, "Don't ask again", None, self.focus == Focus::DontAsk).clicked() {
                clicked_tick = Some(Focus::DontAsk);
            }
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                let style = ButtonStyle { primary: true, bordered: true, focused: self.focus == Focus::Connect, ..Default::default() };
                connect =
                    ui.add_enabled_ui(!chosen.is_empty(), |ui| button(ui, theme, None, &connect_label(&self.offer, &chosen), style)).inner.clicked();
                let style = ButtonStyle { bordered: true, focused: self.focus == Focus::NotNow, ..Default::default() };
                not_now = button(ui, theme, None, "Not now", style).clicked();
            });
            ui.add_space(theme.space(SpaceToken::Sm));
            let enter_does = match self.focus {
                Focus::Agent(_) | Focus::DontAsk => "toggle".to_owned(),
                Focus::NotNow => "Not now".to_owned(),
                Focus::Connect => connect_label(&self.offer, &chosen),
            };
            let hint = format!("{} {enter_does} · {} Not now · {} moves", key(Action::Activate), key(Action::Back), key(Action::FocusNext));
            ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        if !armed {
            return OverlayOutcome::Keep;
        }
        if hide {
            return OverlayOutcome::Close;
        }
        if let Some(f) = clicked_tick {
            self.focus = f;
            self.toggle(f);
        }
        if connect {
            self.focus = Focus::Connect;
            return self.connect(ctx);
        }
        if not_now {
            return self.not_now(ctx);
        }
        OverlayOutcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui_kittest::kittest::{NodeT, Queryable};

    use super::*;
    use crate::agent::connect::ConnectEnv;
    use crate::agent::connect::runner::fake::FakeRunner;
    use crate::config::Config;
    use crate::msg::AppEvent;
    use crate::ui::screens::home::testkit::{OvApp, has_label, overlay_harness, rig, until_ov};

    type H = egui_kittest::Harness<'static, OvApp<AgentConnectCard>>;

    /// Claude Code and Codex on PATH, neither connected; both connect without a fuss.
    fn fake() -> Arc<FakeRunner> {
        Arc::new(
            FakeRunner::default()
                .found(&["claude", "codex", "gemini"])
                .on_ok(&["claude", "plugin", "list"], "[]")
                .on_ok(
                    &["claude", "plugin", "marketplace", "add"],
                    r#"{"command":"marketplace-add","outcome":"ok","message":"Successfully added marketplace: omatuner"}"#,
                )
                .on_ok(
                    &["claude", "plugin", "install"],
                    r#"{"command":"install","outcome":"ok","message":"Successfully installed plugin: omatuner@omatuner (scope: user)"}"#,
                )
                .on_fail(&["codex", "mcp", "get"], 1, "", "Error: No MCP server named 'tuner' found.")
                .on_ok(&["codex", "mcp", "add"], "Added global MCP server 'tuner'."),
        )
    }

    fn card(agents: &[AgentId]) -> (H, Arc<FakeRunner>, tempfile::TempDir) {
        let (t, one) = rig();
        let home = tempfile::tempdir().unwrap();
        let fake = fake();
        t.services().agent_connect.with_fake(fake.clone(), ConnectEnv::rooted(home.path()));
        let mut h = overlay_harness(t, one, AgentConnectCard::new(Offer { agents: agents.to_vec() }), egui::vec2(1200.0, 800.0));
        h.state_mut().o.opened = Some(Instant::now() - ARMING); // the user has seen it
        (h, fake, home)
    }

    /// The config after the card's answers.
    fn answered(h: &mut H) -> Config {
        let mut c = Config::default();
        for e in h.state_mut().t.config_edits.drain(..) {
            e(&mut c);
        }
        c
    }

    fn toasts(h: &H) -> Vec<String> {
        h.state().t.events.try_iter().filter_map(|e| if let AppEvent::Toast(t) = e { Some(t.text) } else { None }).collect()
    }

    /// The card opens by itself: an Enter (or a click outside) in its first moment was meant
    /// for the screen behind, and does nothing; Esc still works.
    #[test]
    fn a_key_or_click_right_as_the_card_appears_does_nothing() {
        let (mut h, fake, _home) = card(&[AgentId::Claude]);
        h.state_mut().o.opened = None;
        h.run_steps(1);
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        let pos = egui::pos2(4.0, 4.0);
        h.hover_at(pos);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        h.run_steps(2);
        assert_ne!(h.state().last, "Close", "a stray click isn't Not now");
        assert!(h.state().o.running.is_none() && fake.calls().is_empty() && answered(&mut h).agents.checked_version.is_none());
        assert_eq!(h.state_mut().act(Action::Back), "Close");
    }

    /// One agent: Enter connects it (the plugin's two steps, in order), records this version
    /// but keeps offering after later upgrades, and the card closes by itself when it's done,
    /// leaving the result to the toast.
    #[test]
    fn enter_connects_the_one_agent_and_the_card_closes_when_done() {
        let (mut h, fake, _home) = card(&[AgentId::Claude]);
        assert!(has_label(&h, "Connect Claude Code to Tuner?"));
        assert_eq!(h.state_mut().act(Action::Activate), "Keep", "stays open while connecting");
        until_ov(&mut h, "the card to close", |h| h.state().last == "Close");
        assert_eq!(
            fake.lines(),
            [
                "claude plugin marketplace add --json rthoms334/omatuner",
                "claude plugin install --json omatuner@omatuner --scope user",
                "claude plugin list --json"
            ]
        );
        let c = answered(&mut h);
        assert_eq!(c.agents.checked_version.as_deref(), Some(VERSION));
        assert!(c.agents.offer_connect, "Connect isn't \"don't ask again\"");
        assert!(toasts(&h).iter().any(|t| t.starts_with("Claude Code is connected to Tuner")), "{:?}", toasts(&h));
    }

    /// Esc is Not now: this version is answered, nothing runs. With "Don't ask again" ticked
    /// (keyboard to the box, or a click on it), the offer is switched off for good.
    #[test]
    fn not_now_runs_nothing_and_dont_ask_again_switches_the_offer_off() {
        let (mut h, fake, _home) = card(&[AgentId::Codex]);
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        let c = answered(&mut h);
        assert_eq!(c.agents.checked_version.as_deref(), Some(VERSION));
        assert!(c.agents.offer_connect);
        // ↑↑ from Connect: Not now, then the box; Enter ticks it; Esc
        for a in [Action::Up, Action::Up, Action::Activate] {
            assert_eq!(h.state_mut().act(a), "Keep");
        }
        assert!(h.state().o.dont_ask);
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        assert!(!answered(&mut h).agents.offer_connect);
        // the mouse: untick by clicking the row, then the Not now button
        h.get_by_label("Don't ask again").click();
        h.run_steps(2);
        assert!(!h.state().o.dont_ask);
        h.get_by_label("Not now").click();
        h.run_steps(2);
        assert_eq!(h.state().last, "Close");
        assert!(answered(&mut h).agents.offer_connect);
        assert!(fake.calls().is_empty(), "{:?}", fake.lines());
    }

    /// Several agents: only the first is ticked; Space ticks another; Connect runs them in
    /// order on one queue. Nothing ticked: Connect is off.
    #[test]
    fn several_agents_tick_the_primary_and_connect_the_ticked_in_order() {
        let (mut h, fake, _home) = card(&[AgentId::Claude, AgentId::Codex, AgentId::Gemini]);
        assert!(has_label(&h, "Connect your AI agents to Tuner?"));
        assert_eq!(h.state().o.ticked, [true, false, false]);
        assert!(has_label(&h, "Connect Claude Code"));
        // ↑ from Connect: Not now, Don't ask again, Gemini, Codex
        for _ in 0..4 {
            h.state_mut().act(Action::Up);
        }
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state().o.ticked, [true, true, false]);
        h.run_steps(1);
        assert!(has_label(&h, "Connect 2 agents"));
        // nothing ticked: the button is off and Enter on it does nothing
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::Activate);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state().o.ticked, [false, false, false]);
        // ↓ to Connect: Gemini, Don't ask again, Not now, Connect
        for _ in 0..4 {
            h.state_mut().act(Action::Down);
        }
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        h.run_steps(1);
        assert!(h.get_by_label("Connect").accesskit_node().is_disabled());
        assert!(fake.calls().is_empty());
        // tick Claude and Codex again with the mouse, then Connect
        h.get_by_label("Claude Code").click();
        h.run_steps(2);
        h.get_by_label("Codex").click();
        h.run_steps(2);
        assert_eq!(h.state().o.ticked, [true, true, false]);
        h.get_by_label("Connect 2 agents").click();
        h.run_steps(1);
        until_ov(&mut h, "the card to close", |h| h.state().last == "Close");
        let changes: Vec<String> = fake.lines().into_iter().filter(|l| !l.contains(" list ") && !l.contains(" get ")).collect();
        assert_eq!(
            changes,
            [
                "claude plugin marketplace add --json rthoms334/omatuner",
                "claude plugin install --json omatuner@omatuner --scope user",
                "codex mcp add tuner -- tuner mcp"
            ]
        );
    }
}
