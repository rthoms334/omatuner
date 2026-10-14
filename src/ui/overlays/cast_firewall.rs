//! The firewall card (DESIGN §40 §3.4, §9.4). OWNER: CAST-9.
//!
//! ```text
//!  Firewall
//!  Living Room couldn't connect back to Tuner. Your firewall is blocking it.
//!
//!  Waiting for your password…                    ← while polkit's dialog is up
//!  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//!  sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'
//!                    [ Close ] [ Play here ] [ Try again ] [ Show command ] [ Allow on firewall ]
//!  Enter Allow on firewall · Esc Close · Tab moves
//! ```
//! "Living Room couldn't connect back to Tuner. Your firewall is blocking it." with **Allow on
//! firewall** (ufw with pkexec, when ufw blocks or a cast just failed with `Firewall`:
//! [`allow_shows`]), **Show command** (`firewall::copy_text`, the only remedy for firewalld,
//! nftables, `NoAgent` and `NotAuthorized`), **Check again**, and — after a failed cast — **Try
//! again** (`CastTo { play: None }`) and **Play here**. While polkit's dialog is up
//! (`FirewallJob::Waiting`): "Waiting for your password…" with Cancel
//! (`CastHandle::firewall_cancel`, which kills pkexec and so closes the dialog). Each
//! `AllowOutcome` has its copy ([`outcome_copy`]); ufw's redacted message shows only here.
//! Opened from the casting card, a cast alert, and Settings ▸ Cast ▸ Network (whose Allow and
//! Remove start the job first: a card that opens on a waiting job adopts it).
//!
//! The panel ([`FirewallPanel`]) is shared with the setup wizard's Network step
//! (`cast_offer.rs`), which shows it with Continue in place of Try again and Play here. The
//! command it shows names this computer's own interface and subnet, never the TV's address.
//! Keys are the dialog keys (`Modal`): ←/→, ↑/↓ and Tab move along the buttons, Enter presses
//! the focused one, Esc cancels a waiting Allow, else closes.

use eframe::egui::{self, RichText};

use super::dialogs::{button_row, card_area, scrim, title};
use super::{Overlay, OverlayOutcome};
use crate::cast::firewall::{self, AllowOutcome, Detection, FirewallState, Rule, UnknownWhy, Verb};
use crate::cast::{CastErrorKind, CastHandle, FirewallJob};
use crate::keymap::{Action, KeyContext};
use crate::player::{PlayerCommand, PlayerErrorKind};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::progress::busy_bar;

/// "Living Room couldn't connect back to Tuner. Your firewall is blocking it." (§3.4).
pub(crate) fn blocked_sentence(name: &str) -> String {
    format!("{name} couldn't connect back to Tuner. Your firewall is blocking it.")
}

/// What detection concluded, as Settings ▸ Cast ▸ Network's Firewall row says it (§9.4).
pub(crate) fn state_text(state: FirewallState) -> &'static str {
    match state {
        FirewallState::NoFirewall => "No firewall",
        FirewallState::Allowed { ours: false } => "Allowed",
        FirewallState::Allowed { ours: true } => "Allowed (Tuner's rule)",
        FirewallState::Blocked { shadowed: false } => "Blocked (ufw)",
        FirewallState::Blocked { shadowed: true } => "Blocked by an earlier rule",
        FirewallState::Unknown { why: UnknownWhy::Firewalld } => "Can't tell (firewalld)",
        FirewallState::Unknown { why: UnknownWhy::Nftables } => "Can't tell (nftables)",
        FirewallState::Unknown { why: UnknownWhy::Unreadable } => "Can't tell (the ufw files can't be read)",
        FirewallState::Unknown { why: UnknownWhy::NoTv } => "Can't tell until Tuner finds a TV",
    }
}

/// **Allow on firewall** shows (§9.4 "When the button shows"): ufw is active and `pkexec` is
/// there, and ufw blocks — or a cast just failed with `Firewall` (the files show what is
/// configured, not what is loaded). Never for firewalld, nftables or an unreadable ufw: those
/// are copy-only.
pub(crate) fn allow_shows(d: &Detection, failed: bool) -> bool {
    d.ufw && d.pkexec && (matches!(d.state, FirewallState::Blocked { .. }) || (failed && !matches!(d.state, FirewallState::Unknown { .. })))
}

/// Tuner's tagged rules for another network than this one ("Allowed on another network
/// (eth0)", with Remove).
pub(crate) fn other_networks(d: &Detection) -> Vec<&Rule> {
    let here = d.net.as_ref().map(|n| n.rule(d.port));
    d.tagged.iter().filter(|r| here.as_ref().is_none_or(|h| r.iface != h.iface || r.net != h.net || r.prefix != h.prefix)).collect()
}

/// The commands to copy (**Show command**): the allow for this network, the removes of Tuner's
/// rules, the firewalld form when firewalld runs. Empty when the network isn't known yet.
pub(crate) fn command_lines(d: &Detection, verb: Option<Verb>) -> Vec<String> {
    if verb == Some(Verb::Remove) {
        return d.tagged.iter().map(|r| firewall::copy_text(r).remove).collect();
    }
    let Some(net) = &d.net else { return Vec::new() };
    let text = firewall::copy_text(&net.rule(d.port));
    match d.state {
        FirewallState::Unknown { why: UnknownWhy::Firewalld } => vec![text.firewalld],
        _ => vec![text.allow],
    }
}

/// The card's line for a finished Allow / Remove (§9.4 "Outcomes"): the text, whether the
/// command follows it, whether it is bad news. `None`: nothing new (the dialog was dismissed
/// or cancelled; the command stays visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutcomeCopy {
    pub text: String,
    pub command: bool,
    pub bad: bool,
}

pub(crate) fn outcome_copy(verb: Verb, outcome: &AllowOutcome, name: &str) -> Option<OutcomeCopy> {
    let line = |text: String, command: bool, bad: bool| Some(OutcomeCopy { text, command, bad });
    let remove = verb == Verb::Remove;
    match outcome {
        AllowOutcome::Added if remove => line("Tuner's firewall rule is removed.".into(), false, false),
        AllowOutcome::AddedUnverified if remove => line("Removed, but Tuner can still see the rule.".into(), false, true),
        AllowOutcome::Added => line(format!("Allowed. Checking with {name}…"), false, false),
        AllowOutcome::AddedUnverified => line(format!("Added, but Tuner can't see the rule. Checking with {name}…"), false, false),
        AllowOutcome::Cancelled => None,
        AllowOutcome::NoAgent => line("No password dialog is available. Run this in a terminal:".into(), true, true),
        AllowOutcome::NotAuthorized => line("Not allowed (needs an administrator). Run this in a terminal:".into(), true, true),
        AllowOutcome::Failed { message } => {
            let message = crate::redact::redact(message.trim());
            let text = if message.is_empty() { "The firewall refused.".to_owned() } else { format!("The firewall refused: {message}") };
            line(text, true, true)
        }
        AllowOutcome::TimedOut => line("Timed out.".into(), false, true),
    }
}

/// A button of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Btn {
    Allow,
    /// Cancel a waiting Allow / Remove.
    CancelJob,
    ShowCommand,
    CopyCommand,
    CheckAgain,
    TryAgain,
    PlayHere,
    /// The wizard: on to Test on TV.
    Continue,
    Close,
}

impl Btn {
    fn label(self) -> &'static str {
        match self {
            Btn::Allow => "Allow on firewall",
            Btn::CancelJob => "Cancel",
            Btn::ShowCommand => "Show command",
            Btn::CopyCommand => "Copy command",
            Btn::CheckAgain => "Check again",
            Btn::TryAgain => "Try again",
            Btn::PlayHere => "Play here",
            Btn::Continue => "Continue",
            Btn::Close => "Close",
        }
    }
}

/// A row of buttons, right to left in the card, with a keyboard focus that moves along them
/// (stopping at the ends) and lands on the first (the main one) when the set changes.
#[derive(Debug, Default)]
pub(super) struct Buttons {
    shown: Vec<Btn>,
    focus: usize,
}

impl Buttons {
    /// This frame's buttons, the main one first.
    pub(super) fn set(&mut self, shown: Vec<Btn>) {
        if shown != self.shown {
            let keep = self.shown.get(self.focus).and_then(|b| shown.iter().position(|s| s == b));
            self.focus = keep.unwrap_or(0);
            self.shown = shown;
        }
    }

    pub(super) fn step(&mut self, by: isize) {
        if !self.shown.is_empty() {
            self.focus = (self.focus as isize + by).clamp(0, self.shown.len() as isize - 1) as usize;
        }
    }

    pub(super) fn focused(&self) -> Option<Btn> {
        self.shown.get(self.focus).copied()
    }

    /// Draw them; the one clicked.
    pub(super) fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Option<Btn> {
        let mut clicked = None;
        button_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
            for (i, b) in self.shown.iter().enumerate() {
                let style = ButtonStyle { primary: i == 0, bordered: true, focused: i == self.focus, ..Default::default() };
                if button(ui, theme, None, b.label(), style).clicked() {
                    clicked = Some(*b);
                }
            }
        });
        if let Some(b) = clicked {
            self.focus = self.shown.iter().position(|s| *s == b).unwrap_or(0);
        }
        clicked
    }

    /// "Enter Allow on firewall · Esc Close · Tab moves".
    pub(super) fn hint(&self, esc: &str) -> String {
        let enter = self.focused().map_or("", Btn::label);
        format!("{} {enter} · {} {esc} · {} moves", key(Action::Activate), key(Action::Back), key(Action::FocusNext))
    }
}

/// The dialog keys for the hint line (from the `Modal` table, not hard-coded).
pub(super) fn key(action: Action) -> String {
    crate::keymap::MODAL.iter().find(|b| b.action == action).map(|b| b.chord.label()).unwrap_or_default()
}

/// Where the panel's Allow / Remove is: only the job this panel started (or found waiting when
/// it opened) is its own; an older finished job's outcome isn't shown.
#[derive(Debug, Default)]
struct Watch {
    /// Started here (or adopted), and the job state when it was started.
    asked: Option<FirewallJob>,
    seen_waiting: bool,
}

impl Watch {
    fn start(&mut self, before: FirewallJob) {
        self.asked = Some(before);
        self.seen_waiting = false;
    }

    /// The job as far as it is this panel's.
    fn ours(&mut self, job: FirewallJob) -> FirewallJob {
        if matches!(job, FirewallJob::Waiting { .. }) {
            // a job already waiting when the panel opened (Settings started it) is adopted
            self.seen_waiting = true;
            self.asked.get_or_insert(FirewallJob::Idle);
            return job;
        }
        match &self.asked {
            Some(before) if self.seen_waiting || job != *before => job,
            _ => FirewallJob::Idle,
        }
    }
}

/// The firewall card's content (the card and the wizard's Network step).
#[derive(Debug, Default)]
pub(crate) struct FirewallPanel {
    show_command: bool,
    commands: Vec<String>,
    copy: Option<String>,
    watch: Watch,
    /// The outcome already acted on (an Allow that worked re-checks once).
    handled: Option<FirewallJob>,
    /// An Allow of this panel worked (the wizard tests on the TV next).
    allowed: bool,
    pub(super) buttons: Buttons,
}

/// What a panel press asks of its card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ask {
    Nothing,
    Close,
    Continue,
}

/// What the panel knows of the cast this card is about.
#[derive(Debug, Clone)]
pub(super) struct Subject {
    /// The TV's sanitized name ("Living Room"; "Your TV" when none is known).
    pub name: String,
    /// A cast (or Test on TV) just failed with `Firewall`: Allow shows while ufw runs.
    pub failed: bool,
    /// The TV to try again on (a cast that failed with `Firewall`).
    pub retry: Option<crate::model::DeviceId>,
    /// A cast is on (Play here).
    pub casting: bool,
    /// The wizard (Continue; no Try again / Play here).
    pub wizard: bool,
}

impl Subject {
    /// The card over the player: the cast in the snapshot, if any.
    fn from_player(ctx: &UiCtx<'_>) -> Self {
        let cast = ctx.player.cast.as_ref();
        let failed = matches!(ctx.player.last_error.as_ref().map(|e| e.kind), Some(PlayerErrorKind::Cast(CastErrorKind::Firewall)));
        Self {
            name: cast.map_or_else(|| "Your TV".to_owned(), |c| c.name.clone()),
            failed,
            retry: cast.filter(|_| failed).map(|c| c.device),
            casting: cast.is_some(),
            wizard: false,
        }
    }
}

impl FirewallPanel {
    /// The buttons for `job` and `d` (the main one first).
    fn shown(&self, d: Option<&Detection>, job: &FirewallJob, s: &Subject) -> Vec<Btn> {
        if matches!(job, FirewallJob::Waiting { .. }) {
            return vec![Btn::CancelJob];
        }
        let mut v = Vec::new();
        let blocked = d.is_some_and(|d| !matches!(d.state, FirewallState::NoFirewall | FirewallState::Allowed { .. }));
        if d.is_some_and(|d| allow_shows(d, s.failed)) {
            v.push(Btn::Allow);
        }
        if s.wizard {
            v.push(Btn::Continue);
        } else if s.retry.is_some() {
            v.push(Btn::TryAgain);
        }
        if d.is_some_and(|d| !d.tagged.is_empty() || d.net.is_some()) && blocked && !self.show_command {
            v.push(Btn::ShowCommand);
        }
        if !self.commands.is_empty() {
            v.push(Btn::CopyCommand);
        }
        v.push(Btn::CheckAgain);
        if s.casting && !s.wizard {
            v.push(Btn::PlayHere);
        }
        if !s.wizard {
            v.push(Btn::Close);
        }
        v
    }

    /// A button pressed (click or Enter).
    pub(super) fn press(&mut self, b: Btn, ctx: &mut UiCtx<'_>, s: &Subject) -> Ask {
        let cast = ctx.services.agent.cast();
        match b {
            Btn::Allow => {
                self.watch.start(cast.firewall_job());
                self.handled = None;
                cast.firewall_allow();
            }
            Btn::CancelJob => cast.firewall_cancel(),
            Btn::ShowCommand => self.show_command = true,
            Btn::CopyCommand => self.copy = Some(self.commands.join("\n")),
            Btn::CheckAgain => cast.firewall_check(),
            Btn::TryAgain => {
                if let Some(device) = s.retry {
                    ctx.play(PlayerCommand::CastTo { device, play: None });
                }
                return Ask::Close;
            }
            Btn::PlayHere => {
                ctx.play(PlayerCommand::StopCasting { play_here: true });
                return Ask::Close;
            }
            Btn::Continue => return Ask::Continue,
            Btn::Close => return Ask::Close,
        }
        Ask::Nothing
    }

    /// An Allow of this panel worked since the last call.
    pub(super) fn take_allowed(&mut self) -> bool {
        std::mem::take(&mut self.allowed)
    }

    /// Esc: cancel a waiting Allow, else the card's own answer.
    pub(super) fn back(&mut self, cast: &CastHandle) -> bool {
        if matches!(self.watch.ours(cast.firewall_job()), FirewallJob::Waiting { .. }) {
            cast.firewall_cancel();
            return true;
        }
        false
    }

    /// Draw the panel's lines and buttons under the card's title; the button clicked.
    pub(super) fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, s: &Subject) -> Option<Btn> {
        if let Some(text) = self.copy.take() {
            ui.ctx().copy_text(text);
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        let cast = ctx.services.agent.cast();
        let d = cast.firewall();
        let job = self.watch.ours(cast.firewall_job());
        // an Allow that worked: detection again, and the failed cast tries again (the 10 s
        // fetch check is the truth)
        if let FirewallJob::Done { verb: Verb::Allow { .. }, outcome: AllowOutcome::Added | AllowOutcome::AddedUnverified } = &job
            && self.handled.as_ref() != Some(&job)
        {
            self.handled = Some(job.clone());
            self.allowed = true;
            cast.firewall_check();
            if let Some(device) = s.retry.filter(|_| !s.wizard) {
                ctx.play(PlayerCommand::CastTo { device, play: None });
            }
        }
        let small = |text: String, color| RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(color);
        let line = |ui: &mut egui::Ui, text: String, color| {
            ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::Body)).color(color)).wrap());
        };
        match &d {
            None => {
                line(ui, "Checking the firewall…".into(), t.text_secondary);
                ui.add_space(theme.space(SpaceToken::Xs));
                busy_bar(ui, theme, 3.0);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
            Some(d) => {
                let mut status = format!("Firewall: {}", state_text(d.state));
                if let Some(n) = &d.net {
                    status.push_str(&format!(" · this computer ({}), port {}", n.iface, d.port));
                }
                ui.label(small(status, t.text_secondary));
                for r in other_networks(d) {
                    ui.label(small(format!("Allowed on another network ({})", r.iface), t.text_secondary));
                }
            }
        }
        let mut verb = None;
        match &job {
            FirewallJob::Waiting { verb: v } => {
                verb = Some(*v);
                ui.add_space(theme.space(SpaceToken::Sm));
                line(ui, "Waiting for your password…".into(), t.fg);
                ui.add_space(theme.space(SpaceToken::Xs));
                busy_bar(ui, theme, 3.0);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
            FirewallJob::Done { verb: v, outcome } => {
                verb = Some(*v);
                if let Some(c) = outcome_copy(*v, outcome, &s.name) {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    line(ui, c.text, if c.bad { t.urgent } else { t.fg });
                    self.show_command |= c.command;
                }
            }
            FirewallJob::Idle => {}
        }
        if job == FirewallJob::Idle {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
        self.commands.clear();
        if self.show_command
            && let Some(d) = &d
        {
            self.commands = command_lines(d, verb);
            ui.add_space(theme.space(SpaceToken::Sm));
            if self.commands.is_empty() {
                line(ui, "Tuner can't tell which network the TV is on yet: Check again.".into(), t.text_secondary);
            }
            for l in &self.commands {
                ui.add(egui::Label::new(RichText::new(l).monospace().size(theme.font(TypeToken::BodySmall)).color(t.fg)).wrap());
            }
        }
        self.buttons.set(self.shown(d.as_ref(), &job, s));
        ui.add_space(theme.space(SpaceToken::Lg));
        self.buttons.ui(ui, theme)
    }
}

/// The firewall card.
#[derive(Debug, Default)]
pub struct CastFirewall {
    panel: FirewallPanel,
}

impl CastFirewall {
    pub fn new() -> Self {
        Self::default()
    }

    fn answer(ask: Ask) -> OverlayOutcome {
        match ask {
            Ask::Nothing => OverlayOutcome::Keep,
            Ask::Close | Ask::Continue => OverlayOutcome::Close,
        }
    }
}

impl Overlay for CastFirewall {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Left | Action::Up | Action::FocusPrev => self.panel.buttons.step(1),
            Action::Right | Action::Down | Action::FocusNext => self.panel.buttons.step(-1),
            Action::Activate => {
                let s = Subject::from_player(ctx);
                if let Some(b) = self.panel.buttons.focused() {
                    return Self::answer(self.panel.press(b, ctx, &s));
                }
            }
            Action::Back => {
                if !self.panel.back(&ctx.services.agent.cast()) {
                    return OverlayOutcome::Close;
                }
            }
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let s = Subject::from_player(ctx);
        if scrim(egui_ctx, theme, "cast-firewall") && !self.panel.back(&ctx.services.agent.cast()) {
            return OverlayOutcome::Close;
        }
        let body = if s.failed {
            blocked_sentence(&s.name)
        } else {
            let port = ctx.config.cast.media_port;
            format!(
                "Your TV fetches the picture from Tuner on this computer, port {port} (TCP). Tuner never runs sudo: Allow on firewall asks for your password in Omarchy's dialog."
            )
        };
        let clicked = card_area(egui_ctx, theme, "cast-firewall", CardKind::Popup, 600.0 * theme.rem(), None, |ui| {
            title(ui, theme, "Firewall", Some(&body));
            ui.add_space(theme.space(SpaceToken::Md));
            let clicked = self.panel.ui(ui, ctx, &s);
            ui.add_space(theme.space(SpaceToken::Sm));
            let hint = self.panel.buttons.hint("Close");
            ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(theme.tokens.text_secondary));
            clicked
        })
        .inner;
        match clicked {
            Some(b) => Self::answer(self.panel.press(b, ctx, &s)),
            None => OverlayOutcome::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use super::*;
    use crate::cast::firewall::LanNet;
    use crate::cast::testing::{FakeCall, FakeCast};
    use crate::ui::screens::home::testkit::{has_label, overlay_harness, rig};

    fn detection(state: FirewallState) -> Detection {
        Detection {
            state,
            toward: Ipv4Addr::new(192, 168, 1, 5),
            ufw: true,
            pkexec: true,
            net: Some(LanNet { iface: "wlan0".into(), addr: Ipv4Addr::new(192, 168, 1, 88), prefix: 24 }),
            tagged: Vec::new(),
            port: 47800,
        }
    }

    #[test]
    fn firewall_command_text_can_be_copied() {
        use crate::ui::testing::clicks;
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.firewall = Some(detection(FirewallState::Blocked { shadowed: false })));
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastFirewall::new(), egui::vec2(1200.0, 800.0));
        clicks::click_text(&mut h, "Show command");
        h.run_steps(2);
        clicks::click_text(&mut h, "Copy command");
        h.run_steps(1);
        assert_eq!(
            clicks::copied_text(&h),
            Some("sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'")
        );
        assert!(fake.take_calls().is_empty(), "copying does not run the command");
    }

    /// The button shows for ufw with pkexec when ufw blocks, or right after a cast failed with
    /// `Firewall` while ufw runs; never for what only a copied command can fix.
    #[test]
    fn allow_on_firewall_shows_only_where_pkexec_and_ufw_can_fix_it() {
        let blocked = detection(FirewallState::Blocked { shadowed: false });
        assert!(allow_shows(&blocked, false));
        assert!(!allow_shows(&Detection { pkexec: false, ..blocked.clone() }, true));
        assert!(!allow_shows(&Detection { ufw: false, ..blocked.clone() }, true));
        let allowed = detection(FirewallState::Allowed { ours: false });
        assert!(!allow_shows(&allowed, false) && allow_shows(&allowed, true), "configured isn't loaded: the fetch check is the truth");
        let firewalld = Detection { ufw: false, ..detection(FirewallState::Unknown { why: UnknownWhy::Firewalld }) };
        assert!(!allow_shows(&firewalld, true));
        assert_eq!(command_lines(&firewalld, None)[0].split_whitespace().nth(1), Some("firewall-cmd"));
        assert_eq!(
            command_lines(&blocked, None),
            ["sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'"]
        );
    }

    /// Every outcome has its copy (§9.4); the command follows where only it can help, and
    /// ufw's message is redacted.
    #[test]
    fn each_outcome_has_its_copy() {
        let allow = Verb::Allow { first: false };
        let copy = |o: AllowOutcome| outcome_copy(allow, &o, "Living Room");
        assert_eq!(copy(AllowOutcome::Added).unwrap().text, "Allowed. Checking with Living Room…");
        assert!(copy(AllowOutcome::AddedUnverified).unwrap().text.starts_with("Added, but Tuner can't see the rule."));
        assert_eq!(copy(AllowOutcome::Cancelled), None, "nothing new");
        for (o, text) in [
            (AllowOutcome::NoAgent, "No password dialog is available. Run this in a terminal:"),
            (AllowOutcome::NotAuthorized, "Not allowed (needs an administrator). Run this in a terminal:"),
        ] {
            assert_eq!(copy(o), Some(OutcomeCopy { text: text.into(), command: true, bad: true }));
        }
        crate::redact::register_secret("s3cr3t-token");
        let failed = copy(AllowOutcome::Failed { message: "ERROR: bad rule s3cr3t-token\n".into() }).unwrap();
        assert!(failed.command && failed.text.starts_with("The firewall refused: ERROR: bad rule") && !failed.text.contains("s3cr3t"), "{failed:?}");
        assert_eq!(copy(AllowOutcome::TimedOut).unwrap().text, "Timed out.");
        assert_eq!(outcome_copy(Verb::Remove, &AllowOutcome::Added, "TV").unwrap().text, "Tuner's firewall rule is removed.");
    }

    /// Allow on firewall waits for polkit's dialog with Cancel (which kills pkexec), then says
    /// how it went; a job finished before the card asked isn't its outcome.
    /// The card is as tall as what it shows, not the window (`dialogs::button_row`): blocked
    /// with its buttons, and waiting for the password.
    #[test]
    fn the_card_sizes_to_its_content_in_a_tall_window() {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.firewall = Some(detection(FirewallState::Blocked { shadowed: false })));
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastFirewall::new(), egui::vec2(1280.0, 1400.0));
        for job in [FirewallJob::Idle, FirewallJob::Waiting { verb: Verb::Allow { first: false } }] {
            fake.script(|s| s.firewall_job = job.clone());
            h.run_steps(3);
            let height = crate::ui::overlays::dialogs::card_height(&h.ctx, "cast-firewall");
            assert!(height > 100.0 && height < 700.0, "{job:?}: {height} px tall in a 1400 px window");
        }
    }

    #[test]
    fn allow_waits_for_the_password_and_says_how_it_went() {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| {
            s.firewall = Some(detection(FirewallState::Blocked { shadowed: false }));
            s.firewall_job = FirewallJob::Done { verb: Verb::Allow { first: false }, outcome: AllowOutcome::NoAgent };
        });
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastFirewall::new(), egui::vec2(1200.0, 800.0));
        assert!(has_label(&h, "Firewall: Blocked (ufw) · this computer (wlan0), port 47800"));
        assert!(!has_label(&h, "No password dialog is available. Run this in a terminal:"), "an older job's outcome");
        assert_eq!(h.state().o.panel.buttons.focused(), Some(Btn::Allow));
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        assert_eq!(fake.take_calls(), [FakeCall::FirewallAllow]);
        fake.script(|s| s.firewall_job = FirewallJob::Waiting { verb: Verb::Allow { first: false } });
        h.run_steps(2);
        assert!(has_label(&h, "Waiting for your password…") && has_label(&h, "Cancel"));
        assert_eq!(h.state_mut().act(Action::Back), "Keep", "Esc cancels the dialog first");
        assert_eq!(fake.take_calls(), [FakeCall::FirewallCancel]);
        fake.script(|s| s.firewall_job = FirewallJob::Done { verb: Verb::Allow { first: false }, outcome: AllowOutcome::NotAuthorized });
        h.run_steps(2);
        assert!(has_label(&h, "Not allowed (needs an administrator). Run this in a terminal:"));
        assert!(has_label(&h, "sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'"));
        fake.script(|s| {
            s.firewall_job = FirewallJob::Done { verb: Verb::Allow { first: false }, outcome: AllowOutcome::Added };
            s.firewall = Some(detection(FirewallState::Allowed { ours: true }));
        });
        h.run_steps(2);
        assert!(has_label(&h, "Allowed. Checking with Your TV…"));
        assert_eq!(fake.take_calls(), [FakeCall::FirewallCheck], "detection runs again once");
        assert_eq!(h.state_mut().act(Action::Back), "Close");
    }
}
