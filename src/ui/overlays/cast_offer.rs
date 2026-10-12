//! The first-run card "Watch on your TV" and the setup wizard (DESIGN §40 §3.5). OWNER: CAST-9.
//!
//! ```text
//!  󰄙  Watch on your TV
//!  Tuner found Living Room (Apple TV) on your network. Pair it once with the code the TV
//!  shows, then press C in the player to send what you're watching there.
//!
//!               [ Don't ask again ]   [ Not now ]   [ Set up Living Room ]
//!  Enter Set up Living Room · Esc Not now · Tab moves
//! ```
//! Opened by the app (`start_cast_pass`, the `CardGate`, one card at a time: the agent card
//! goes first) when the pass found an Apple-made, video-capable, unpaired TV. **Set up <TV>**
//! runs the wizard in the card's place: Pair (§3.2: the PIN overlay, `OverlayRequest::CastPair`,
//! opens over the card; the card goes on once the TV is paired) → Network (only when detection
//! says ufw blocks: the firewall panel of `cast_firewall.rs` with Allow on firewall, Show
//! command, Check again and Continue; it moves on by itself once the port is open) → Test on TV
//! (`CastHandle::test_on_tv`: a 15 s Tuner test card through the real relay, proving pairing,
//! the firewall path and the format at once; a `Firewall` failure goes back to Network), ending
//! "Ready. Press C in the player to cast." Every answer writes `cast.checked_version`; "Don't
//! ask again" also `cast.offer_setup = false` (the switch Settings ▸ Cast ▸ "Offer to set up new
//! TVs" shows).
//!
//! The card opens by itself, so for its first [`ARMING`] Enter, Space and clicks do nothing (a
//! key meant for the screen behind can't start pairing); Esc works. Keys are the dialog keys
//! (`Modal`): ←/→, ↑/↓ and Tab move along the buttons, Enter presses the focused one, Esc is Not
//! now on the card, cancels a waiting Allow, else closes the wizard.

use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::cast_firewall::{Ask, FirewallPanel, Subject, key};
use super::dialogs::{card_area, scrim, title};
use super::{Overlay, OverlayOutcome};
use crate::cast::firewall::FirewallState;
use crate::cast::{CastErrorKind, CastHandle, Pairing, ReceiverKind, SetupOffer, TestState};
use crate::config::CastConvert;
use crate::keymap::{Action, KeyContext};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::glyphs;
use crate::ui::widgets::progress::busy_bar;
use crate::ui::{OverlayRequest, UiCtx};

/// The version the card's answer is recorded for (`CastConfig::checked_version`).
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// After the card appears, Enter, Space and clicks wait this long (`agent_connect.rs`).
const ARMING: Duration = Duration::from_millis(600);

/// "Apple TV" / "AirPlay TV" (the picker's and Settings' second line).
pub(crate) fn kind_label(kind: ReceiverKind) -> &'static str {
    match kind {
        ReceiverKind::AppleTv => "Apple TV",
        ReceiverKind::AirPlayTv => "AirPlay TV",
        ReceiverKind::Chromecast => "Chromecast",
        ReceiverKind::Dlna => "DLNA TV",
    }
}

/// The one sentence of a cast failure (DESIGN §40 §3.4), shared by the alerts (toast and
/// desktop notification), the wizard and Settings. `name` is the TV's sanitized name; never
/// device or provider text beyond it. `convert_off`: `cast.convert = never` (Unsupported says
/// so).
pub(crate) fn error_sentence(kind: CastErrorKind, name: &str, convert_off: bool) -> String {
    match kind {
        CastErrorKind::Firewall => super::cast_firewall::blocked_sentence(name),
        CastErrorKind::Unsupported if convert_off => format!("{name} can't play this channel's format. Converting is off in Settings ▸ Cast."),
        CastErrorKind::Unsupported => format!("{name} can't play this channel's format."),
        CastErrorKind::CpuTooSlow => "This computer can't convert this channel fast enough.".into(),
        CastErrorKind::NeedsFfmpeg => "Casting needs ffmpeg (it's normally installed with mpv).".into(),
        CastErrorKind::Unreachable => format!("{name} isn't answering. Is it on and on the same network?"),
        CastErrorKind::PairingRequired => format!("{name} needs pairing."),
        CastErrorKind::PairingLost => format!("{name} forgot Tuner. This can happen after a TV update."),
        CastErrorKind::PairingRefused => {
            "On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network, and give it a room in the Home app.".into()
        }
        CastErrorKind::StoppedOnTv => format!("{name} stopped playing. Its remote stopped it, or another device took over."),
        CastErrorKind::ReceiverFailed => format!("{name} couldn't play it."),
    }
}

/// The `C` key's label in the player (from the key table, not hard-coded).
fn cast_key() -> String {
    crate::keymap::PLAYER_COMMON.iter().find(|b| b.action == Action::CastMenu).map_or_else(|| "C".to_owned(), |b| b.chord.label())
}

/// Where the wizard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The card.
    Offer,
    /// The PIN overlay was opened over the card; back here, the TV is paired or not yet.
    Pair,
    /// Detection runs; the panel shows when ufw blocks (`forced`: after a `Firewall` test).
    Network {
        forced: bool,
    },
    /// Test on TV; `before` is the test state when it started (an older result isn't this
    /// one's), `seen` once it ran.
    Test {
        before: TestState,
        seen: bool,
    },
    /// The test failed (not the firewall): its sentence, Try again.
    Failed(CastErrorKind),
    Ready,
}

/// A button of the card's own steps (the Network step's are the firewall panel's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    SetUp,
    NotNow,
    DontAsk,
    Pair,
    TryAgain,
    Done,
    Close,
}

pub struct CastOffer {
    pub offer: SetupOffer,
    step: Step,
    focus: usize,
    firewall: FirewallPanel,
    /// When the card was first drawn (`None`: not yet).
    opened: Option<Instant>,
}

impl CastOffer {
    pub fn new(offer: SetupOffer) -> Self {
        Self { offer, step: Step::Offer, focus: 0, firewall: FirewallPanel::default(), opened: None }
    }

    /// Enter, Space and clicks count (the card has been up for [`ARMING`]).
    fn armed(&self) -> bool {
        self.opened.is_some_and(|t| t.elapsed() >= ARMING)
    }

    fn network(&self) -> bool {
        matches!(self.step, Step::Network { .. })
    }

    fn subject(&self) -> Subject {
        let failed = matches!(self.step, Step::Network { forced: true });
        Subject { name: self.offer.name.clone(), failed, retry: None, casting: false, wizard: true }
    }

    /// "Set up Living Room" (the main button) first.
    fn shown(&self) -> Vec<Btn> {
        match self.step {
            Step::Offer => vec![Btn::SetUp, Btn::NotNow, Btn::DontAsk],
            Step::Pair => vec![Btn::Pair, Btn::Close],
            Step::Failed(_) => vec![Btn::TryAgain, Btn::Close],
            Step::Ready => vec![Btn::Done],
            Step::Network { .. } | Step::Test { .. } => vec![Btn::Close],
        }
    }

    fn step_focus(&mut self, by: isize) {
        if self.network() {
            return self.firewall.buttons.step(by);
        }
        let n = self.shown().len() as isize;
        self.focus = (self.focus as isize + by).clamp(0, (n - 1).max(0)) as usize;
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        self.focus = 0;
    }

    /// Not now / Don't ask again (and, with `never` false, any other answer): this version is
    /// answered.
    fn answer(&self, ctx: &mut UiCtx<'_>, never: bool) {
        ctx.edit_config(move |c| {
            c.cast.checked_version = Some(VERSION.to_owned());
            if never {
                c.cast.offer_setup = false;
            }
        });
    }

    fn paired(&self, cast: &CastHandle) -> bool {
        cast.devices().iter().any(|d| d.id == self.offer.device && matches!(d.pairing, Pairing::Paired(_) | Pairing::NotNeeded))
    }

    fn start_test(&mut self, cast: &CastHandle) {
        let before = cast.test_state();
        cast.test_on_tv(self.offer.device);
        self.go(Step::Test { before, seen: false });
    }

    /// Pair, or straight on when the TV is paired already (Settings did it meanwhile).
    fn pair(&mut self, ctx: &mut UiCtx<'_>) {
        let cast = ctx.services.agent.cast();
        if self.paired(&cast) {
            cast.firewall_check();
            self.go(Step::Network { forced: false });
        } else {
            ctx.open(OverlayRequest::CastPair(self.offer.device));
            self.go(Step::Pair);
        }
    }

    fn press(&mut self, b: Btn, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        match b {
            Btn::SetUp => {
                self.answer(ctx, false);
                self.pair(ctx);
            }
            Btn::NotNow => {
                self.answer(ctx, false);
                return OverlayOutcome::Close;
            }
            Btn::DontAsk => {
                self.answer(ctx, true);
                return OverlayOutcome::Close;
            }
            Btn::Pair => self.pair(ctx),
            Btn::TryAgain => self.start_test(&ctx.services.agent.cast()),
            Btn::Done | Btn::Close => return OverlayOutcome::Close,
        }
        OverlayOutcome::Keep
    }

    fn press_focused(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if self.network() {
            let s = self.subject();
            return match self.firewall.buttons.focused().map(|b| self.firewall.press(b, ctx, &s)) {
                Some(Ask::Continue) => {
                    self.start_test(&ctx.services.agent.cast());
                    OverlayOutcome::Keep
                }
                Some(Ask::Close) => OverlayOutcome::Close,
                Some(Ask::Nothing) | None => OverlayOutcome::Keep,
            };
        }
        match self.shown().get(self.focus).copied() {
            Some(b) => self.press(b, ctx),
            None => OverlayOutcome::Keep,
        }
    }

    /// Esc: Not now on the card; a waiting Allow is cancelled; else the wizard closes (what is
    /// done stays done: the pairing, the rule).
    fn back(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        match self.step {
            Step::Offer => self.press(Btn::NotNow, ctx),
            Step::Network { .. } if self.firewall.back(&ctx.services.agent.cast()) => OverlayOutcome::Keep,
            _ => OverlayOutcome::Close,
        }
    }

    /// Move on by itself where nothing needs the user: a paired TV leaves Pair, an open port
    /// leaves Network, a finished test leaves Test.
    fn advance(&mut self, cast: &CastHandle) {
        match self.step {
            Step::Pair if self.paired(cast) => {
                cast.firewall_check();
                self.go(Step::Network { forced: false });
            }
            // an Allow that worked tests at once; otherwise the port open (detection) moves on,
            // except after the TV couldn't fetch: what the files say isn't what is loaded, so
            // the user continues
            Step::Network { forced } => {
                let open = cast.firewall().is_some_and(|d| !matches!(d.state, FirewallState::Blocked { .. }));
                if self.firewall.take_allowed() || (open && !forced) {
                    self.start_test(cast);
                }
            }
            Step::Test { before, seen } => match cast.test_state() {
                TestState::Running => self.step = Step::Test { before, seen: true },
                s if !seen && s == before => {}
                TestState::Passed => self.go(Step::Ready),
                TestState::Failed(CastErrorKind::Firewall) => {
                    cast.firewall_check();
                    self.go(Step::Network { forced: true });
                }
                TestState::Failed(kind) => self.go(Step::Failed(kind)),
                TestState::Idle => {}
            },
            _ => {}
        }
    }

    /// The step's title and lines.
    fn text(&self, ctx: &UiCtx<'_>) -> (String, String) {
        let name = &self.offer.name;
        match self.step {
            Step::Offer => (
                "Watch on your TV".into(),
                format!(
                    "Tuner found {name} ({}) on your network. Pair it once with the code the TV shows, then press {} in the player to send what you're watching there.",
                    kind_label(self.offer.kind),
                    cast_key()
                ),
            ),
            Step::Pair => (format!("Set up {name}"), format!("{name} isn't paired yet. Pair shows the code on the TV again.")),
            Step::Network { forced: true } => (format!("Set up {name}"), super::cast_firewall::blocked_sentence(name)),
            Step::Network { forced: false } => (
                format!("Set up {name}"),
                format!(
                    "{name} fetches the picture from Tuner on this computer, port {} (TCP). Your firewall blocks it.",
                    ctx.config.cast.media_port
                ),
            ),
            Step::Test { .. } => (format!("Set up {name}"), format!("Playing Tuner's test card on {name} for 15 seconds…")),
            Step::Failed(kind) => (format!("Set up {name}"), error_sentence(kind, name, ctx.config.cast.convert == CastConvert::Never)),
            Step::Ready => (format!("{name} is ready"), format!("Ready. Press {} in the player to cast.", cast_key())),
        }
    }

    fn label(&self, b: Btn) -> String {
        match b {
            Btn::SetUp => format!("Set up {}", self.offer.name),
            Btn::NotNow => "Not now".into(),
            Btn::DontAsk => "Don't ask again".into(),
            Btn::Pair => "Pair".into(),
            Btn::TryAgain => "Try again".into(),
            Btn::Done => "Done".into(),
            Btn::Close => "Close".into(),
        }
    }
}

impl Overlay for CastOffer {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            // the buttons sit right to left, the main one rightmost
            Action::Left | Action::Up | Action::FocusPrev => self.step_focus(1),
            Action::Right | Action::Down | Action::FocusNext => self.step_focus(-1),
            // too soon: a key meant for what was there before the card
            Action::Activate if !self.armed() => {}
            Action::Activate => return self.press_focused(ctx),
            Action::Back => return self.back(ctx),
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let opened = *self.opened.get_or_insert_with(Instant::now);
        let armed = self.armed();
        if !armed {
            egui_ctx.request_repaint_after(ARMING.saturating_sub(opened.elapsed()));
        }
        let cast = ctx.services.agent.cast();
        self.advance(&cast);
        if !matches!(self.step, Step::Offer | Step::Ready | Step::Failed(_)) {
            egui_ctx.request_repaint_after(Duration::from_millis(200));
        }
        if scrim(egui_ctx, theme, "cast-offer") && armed {
            return self.back(ctx);
        }
        let (heading, body) = self.text(ctx);
        let shown = self.shown();
        self.focus = self.focus.min(shown.len().saturating_sub(1));
        let s = self.subject();
        let (mut clicked, mut fw_clicked) = (None, None);
        card_area(egui_ctx, theme, "cast-offer", CardKind::Popup, 600.0 * theme.rem(), None, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(glyphs::CAST).size(theme.font(TypeToken::Icon)).color(t.accent));
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.vertical(|ui| title(ui, theme, &heading, None));
            });
            ui.add_space(theme.space(SpaceToken::Xs));
            ui.add(egui::Label::new(RichText::new(&body).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
            ui.add_space(theme.space(SpaceToken::Md));
            if self.network() {
                fw_clicked = self.firewall.ui(ui, ctx, &s);
                ui.add_space(theme.space(SpaceToken::Sm));
                let hint = self.firewall.buttons.hint("Close");
                ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                return;
            }
            if matches!(self.step, Step::Test { .. }) {
                busy_bar(ui, theme, 3.0);
                ui.add_space(theme.space(SpaceToken::Lg));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                for (i, b) in shown.iter().enumerate() {
                    let style = ButtonStyle { primary: i == 0, bordered: true, focused: i == self.focus, ..Default::default() };
                    if button(ui, theme, None, &self.label(*b), style).clicked() {
                        clicked = Some((i, *b));
                    }
                }
            });
            ui.add_space(theme.space(SpaceToken::Sm));
            let esc = if self.step == Step::Offer { "Not now" } else { "Close" };
            let enter = shown.get(self.focus).map(|b| self.label(*b)).unwrap_or_default();
            let hint = format!("{} {enter} · {} {esc} · {} moves", key(Action::Activate), key(Action::Back), key(Action::FocusNext));
            ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        if !armed {
            return OverlayOutcome::Keep;
        }
        if let Some(b) = fw_clicked {
            return match self.firewall.press(b, ctx, &s) {
                Ask::Continue => {
                    self.start_test(&cast);
                    OverlayOutcome::Keep
                }
                Ask::Close => OverlayOutcome::Close,
                Ask::Nothing => OverlayOutcome::Keep,
            };
        }
        if let Some((i, b)) = clicked {
            self.focus = i;
            return self.press(b, ctx);
        }
        OverlayOutcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cast::firewall::{Detection, LanNet};
    use crate::cast::testing::{FakeCall, FakeCast};
    use crate::cast::{Device, PairVariant, Protocol};
    use crate::config::Config;
    use crate::model::DeviceId;
    use crate::ui::screens::home::testkit::{OvApp, has_label, overlay_harness, rig};

    type H = egui_kittest::Harness<'static, OvApp<CastOffer>>;

    fn living_room() -> DeviceId {
        DeviceId::derive("airplay", "6C:4A:85:00:00:01")
    }

    fn card() -> (H, Arc<FakeCast>) {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.devices = vec![Device::new(living_room(), "Living Room", ReceiverKind::AppleTv, Protocol::AirPlay)]);
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let offer = SetupOffer { device: living_room(), name: "Living Room".into(), kind: ReceiverKind::AppleTv };
        let mut h = overlay_harness(t, one, CastOffer::new(offer), egui::vec2(1200.0, 800.0));
        h.state_mut().o.opened = Some(Instant::now() - ARMING); // the user has seen it
        (h, fake)
    }

    /// The config after the card's answers.
    fn answered(h: &mut H) -> Config {
        let mut c = Config::default();
        for e in h.state_mut().t.config_edits.drain(..) {
            e(&mut c);
        }
        c
    }

    fn blocked() -> Detection {
        Detection {
            state: FirewallState::Blocked { shadowed: false },
            ufw: true,
            pkexec: true,
            net: Some(LanNet { iface: "wlan0".into(), addr: std::net::Ipv4Addr::new(192, 168, 1, 88), prefix: 24 }),
            tagged: Vec::new(),
            port: 47800,
        }
    }

    /// Not now answers this version only; "Don't ask again" (← to it, Enter) also turns the
    /// offer off; neither pairs anything.
    #[test]
    fn not_now_and_dont_ask_again_answer_the_version() {
        let (mut h, fake) = card();
        assert!(has_label(&h, "Watch on your TV"));
        assert!(has_label(&h, "Set up Living Room"));
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        let c = answered(&mut h);
        assert_eq!(c.cast.checked_version.as_deref(), Some(VERSION));
        assert!(c.cast.offer_setup);
        for a in [Action::Left, Action::Left, Action::Activate] {
            h.state_mut().act(a);
        }
        let c = answered(&mut h);
        assert!(!c.cast.offer_setup && c.cast.checked_version.as_deref() == Some(VERSION));
        assert!(fake.take_calls().is_empty());
    }

    /// Set up: the PIN overlay over the card; once paired, Network (ufw blocks: Allow on
    /// firewall), then — the port open — Test on TV by itself, then Ready.
    #[test]
    fn set_up_pairs_opens_the_firewall_and_tests_on_the_tv() {
        let (mut h, fake) = card();
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [crate::ui::NavRequest::Open(OverlayRequest::CastPair(d))] if *d == living_room()), "{nav:?}");
        assert_eq!(answered(&mut h).cast.checked_version.as_deref(), Some(VERSION));
        // back from the PIN overlay without pairing: say so, Pair again
        h.run_steps(2);
        assert!(has_label(&h, "Living Room isn't paired yet. Pair shows the code on the TV again."));
        fake.script(|s| {
            s.devices[0].pairing = Pairing::Paired(PairVariant::Hkp3);
            s.firewall = Some(blocked());
        });
        h.run_steps(2);
        assert!(has_label(&h, "Allow on firewall"), "ufw blocks the port");
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        assert_eq!(fake.take_calls(), [FakeCall::FirewallCheck, FakeCall::FirewallAllow]);
        fake.script(|s| s.firewall = Some(Detection { state: FirewallState::Allowed { ours: true }, ..blocked() }));
        h.run_steps(2);
        assert_eq!(fake.take_calls(), [FakeCall::TestOnTv(living_room())]);
        assert!(has_label(&h, "Playing Tuner's test card on Living Room for 15 seconds…"));
        fake.script(|s| s.test_state = TestState::Running);
        h.run_steps(2);
        fake.script(|s| s.test_state = TestState::Passed);
        h.run_steps(2);
        assert!(has_label(&h, &format!("Ready. Press {} in the player to cast.", cast_key())));
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
    }

    /// A test the TV couldn't fetch goes back to Network with Allow on firewall; another
    /// failure says its sentence and offers Try again.
    #[test]
    fn a_failed_test_says_why() {
        let (mut h, fake) = card();
        fake.script(|s| {
            s.devices[0].pairing = Pairing::Paired(PairVariant::Hkp5);
            s.firewall = Some(Detection { state: FirewallState::NoFirewall, ufw: false, pkexec: true, ..blocked() });
        });
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(fake.take_calls(), [FakeCall::FirewallCheck, FakeCall::TestOnTv(living_room())], "no firewall: straight to the test");
        fake.script(|s| s.test_state = TestState::Failed(CastErrorKind::Unreachable));
        h.run_steps(2);
        assert!(has_label(&h, "Living Room isn't answering. Is it on and on the same network?"));
        h.state_mut().act(Action::Activate);
        assert_eq!(fake.take_calls(), [FakeCall::TestOnTv(living_room())], "Try again");
        fake.script(|s| {
            s.test_state = TestState::Failed(CastErrorKind::Firewall);
            s.firewall = Some(Detection { state: FirewallState::Allowed { ours: false }, ..blocked() });
        });
        h.run_steps(2);
        assert!(has_label(&h, "Living Room couldn't connect back to Tuner. Your firewall is blocking it."));
        assert!(has_label(&h, "Allow on firewall") && has_label(&h, "Continue") && has_label(&h, "Check again"));
        h.run_steps(2);
        assert_eq!(fake.take_calls(), [FakeCall::FirewallCheck], "detection again, then it waits for the user");
    }
}
