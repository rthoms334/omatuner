//! PIN pairing with a TV (DESIGN §40 §3.2). OWNER: CAST-8.
//!
//! ```text
//!  Pair with Living Room
//!  Enter the code shown on the TV.
//!
//!               ┌───┐  ┌───┐  ┌───┐  ┌───┐
//!               │ 4 │  │ 7 │  │ _ │  │   │
//!               └───┘  └───┘  └───┘  └───┘
//!  No code on the TV? On the Apple TV open Settings ▸ AirPlay and HomeKit ▸ Allow
//!  Access ▸ Anyone on the Same Network.
//!                              [ Show a new code ]  [ Cancel ]  [ Pair ]
//! ```
//! The overlay starts the job with `CastHandle::pair_start` and polls `pair_state` every frame:
//! the connection stays open while the user types (the Apple TV takes the PIN only on the
//! connection that asked for it) and the overlay waits if the user leaves its workspace; when
//! the TV drops the code meanwhile, the state says so ("The code expired.") and **Show a new
//! code** (`pair_new_code`: the next pairing variant) asks again. Each `PairError` has its own
//! copy ([`failure_copy`]).
//!
//! Keys are `[TextInput, Modal]` and no new context: digits and Backspace edit the code (read
//! from the frame's events, so no text field takes focus), Enter pairs once 4 digits are in (or
//! presses the focused button), Tab moves between the buttons, Esc cancels. Like the first-run
//! card it opens by itself from a click or Enter in the picker, so for its first [`ARMING`]
//! digits, Enter and clicks do nothing (a key meant for the screen behind can't be typed
//! into the TV's code); Esc works.
//!
//! The PIN goes to the service once (`pair_submit`, a [`Pin`]) and is dropped: it is never
//! logged, stored or shown in `Debug` output, and the digit buffer is zeroed when it is sent or
//! cleared. On success: the toast "Paired with <TV>", and when opened to cast (the picker, the
//! casting card's Pair) with something playing, the cast starts (`PlayerCommand::CastTo`, what
//! plays moves to the TV); from Settings ▸ Cast or the setup wizard it only pairs.

use std::fmt;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim, title};
use super::{Overlay, OverlayOutcome};
use crate::cast::{CastHandle, PairError, PairJob, PairState, Pin};
use crate::keymap::{Action, KeyContext};
use crate::model::DeviceId;
use crate::msg::Toast;
use crate::player::PlayerCommand;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::progress::busy_bar;

/// After the overlay appears, digits, Enter and clicks wait this long (the same guard as
/// `agent_connect.rs`).
const ARMING: Duration = Duration::from_millis(600);

/// The code's length.
const DIGITS: usize = 4;

/// What Enter acts on (Tab moves it along).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Code,
    NewCode,
    Cancel,
    Pair,
}

/// The typed digits: at most [`DIGITS`], zeroed whenever they are cleared or sent.
#[derive(Default)]
struct Code {
    digits: [u8; DIGITS],
    len: usize,
}

impl Code {
    fn push(&mut self, c: char) {
        if c.is_ascii_digit() && self.len < DIGITS {
            self.digits[self.len] = c as u8;
            self.len += 1;
        }
    }

    fn pop(&mut self) {
        if self.len > 0 {
            self.len -= 1;
            self.digits[self.len] = 0;
        }
    }

    fn clear(&mut self) {
        self.digits = [0; DIGITS];
        self.len = 0;
        std::hint::black_box(&self.digits);
    }

    fn full(&self) -> bool {
        self.len == DIGITS
    }

    /// The PIN to send, and the buffer emptied.
    fn take(&mut self) -> Option<Pin> {
        let pin = std::str::from_utf8(&self.digits[..self.len]).ok().and_then(Pin::new);
        self.clear();
        pin
    }

    fn digit(&self, i: usize) -> Option<char> {
        (i < self.len).then(|| self.digits[i] as char)
    }
}

pub struct CastPair {
    pub device: DeviceId,
    /// Cast once paired (opened from the picker or the casting card).
    then_cast: bool,
    /// The job and the service that runs it (cancelled when the overlay goes, unless it ended).
    job: Option<(CastHandle, PairJob)>,
    code: Code,
    focus: Focus,
    /// When the overlay was first drawn (`None`: not yet).
    opened: Option<Instant>,
    /// The last failure the TV or the network reported (its copy shows until the user types).
    failure: Option<Failure>,
    /// Enter was pressed: acted on at the end of `ui`, after this frame's digits were read.
    enter: bool,
    /// The job ended (paired, or failed for good): nothing to cancel.
    ended: bool,
}

/// A failure to show: the error, or a job the service no longer knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Pair(PairError),
    Interrupted,
}

impl CastPair {
    pub fn new(device: DeviceId, then_cast: bool) -> Self {
        Self { device, then_cast, job: None, code: Code::default(), focus: Focus::Code, opened: None, failure: None, enter: false, ended: false }
    }

    fn armed(&self) -> bool {
        self.opened.is_some_and(|t| t.elapsed() >= ARMING)
    }

    fn cancel(&mut self) -> OverlayOutcome {
        if let Some((cast, job)) = self.job.take()
            && !self.ended
        {
            cast.pair_cancel(job);
        }
        self.code.clear();
        OverlayOutcome::Close
    }

    /// Tab / Shift+Tab along the buttons.
    fn step(&mut self, by: isize) {
        const ORDER: [Focus; 4] = [Focus::Code, Focus::NewCode, Focus::Cancel, Focus::Pair];
        let at = ORDER.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = ORDER[(at + by).rem_euclid(ORDER.len() as isize) as usize];
    }

    /// Send the code (once 4 digits are in and the TV is waiting for one).
    fn submit(&mut self, state: PairState) {
        if !(self.armed() && self.code.full() && matches!(state, PairState::EnterCode { .. })) {
            return;
        }
        let (Some((cast, job)), Some(pin)) = (&self.job, self.code.take()) else { return };
        cast.pair_submit(*job, pin);
    }

    /// **Show a new code**: the next variant on the same job, or a fresh job when the service
    /// lost it.
    fn new_code(&mut self, interrupted: bool) {
        self.code.clear();
        self.failure = None;
        self.ended = false;
        if let Some((cast, job)) = &mut self.job {
            if interrupted {
                *job = cast.pair_start(self.device);
            } else {
                cast.pair_new_code(*job);
            }
        }
    }

    /// The digits typed this frame (only while the TV waits for a code).
    fn read_keys(&mut self, egui_ctx: &egui::Context, state: PairState) {
        if !(self.armed() && matches!(state, PairState::EnterCode { .. }) && self.focus == Focus::Code) {
            return;
        }
        let events = egui_ctx.input(|i| i.events.clone());
        for ev in events {
            match ev {
                egui::Event::Text(t) => {
                    for c in t.chars().filter(char::is_ascii_digit) {
                        self.failure = None;
                        self.code.push(c);
                    }
                }
                egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. } => self.code.pop(),
                _ => {}
            }
        }
    }
}

impl fmt::Debug for CastPair {
    /// Never the digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastPair").field("device", &self.device).field("typed", &self.code.len).finish_non_exhaustive()
    }
}

impl Drop for CastPair {
    fn drop(&mut self) {
        if let Some((cast, job)) = self.job.take()
            && !self.ended
        {
            cast.pair_cancel(job);
        }
        self.code.clear();
    }
}

/// What the user reads for each way pairing stops (DESIGN §40 §3.2 "Replies and their copy").
fn failure_copy(f: Failure, name: &str) -> String {
    match f {
        Failure::Interrupted => "Pairing was interrupted.".to_owned(),
        Failure::Pair(e) => match e {
            PairError::WrongPin => "That code didn't match. The TV is showing a new one.".to_owned(),
            PairError::Backoff { secs } => {
                let wait = match secs {
                    Some(s) if s > 90 => format!("about {} minutes", s.div_ceil(60)),
                    _ => "a minute".to_owned(),
                };
                format!("{name} wants a short break after several tries. Try again in {wait}.")
            }
            PairError::Refused { home_only: true } => {
                "On the Apple TV set Allow Access to Anyone on the Same Network. 'Everyone in your Home' blocks Tuner.".to_owned()
            }
            PairError::Refused { home_only: false } => {
                "On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network, and give it a room in the Home app."
                    .to_owned()
            }
            PairError::Expired => "The code expired.".to_owned(),
            PairError::Unreachable => format!("{name} isn't answering. Is it on?"),
            PairError::NotSupported => format!("Tuner can't cast to {name}."),
            PairError::Failed => format!("Pairing with {name} didn't work. Try again."),
        },
    }
}

/// The dialog keys for the hint line (from the `Modal` table).
fn key(action: Action) -> String {
    crate::keymap::MODAL.iter().find(|b| b.action == action).map(|b| b.chord.label()).unwrap_or_default()
}

/// The four boxes of the code (typed digits, then `_` where the next goes).
fn code_boxes(ui: &mut egui::Ui, theme: &Theme, code: &Code, active: bool) {
    let t = &theme.tokens;
    let side = (theme.space(SpaceToken::ControlHeight) * 1.5).round();
    let gap = theme.space(SpaceToken::Lg);
    let total = side * DIGITS as f32 + gap * (DIGITS - 1) as f32;
    let (area, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), side), egui::Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("Code, {} of {DIGITS} digits", code.len)));
    let mut x = area.center().x - total / 2.0;
    for i in 0..DIGITS {
        let r = egui::Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(side, side));
        x = r.right() + gap;
        let here = active && i == code.len;
        let p = ui.painter();
        p.rect_filled(r, t.rounding, t.fill_field);
        p.rect_stroke(
            r,
            t.rounding,
            egui::Stroke::new(if here { 2.0 } else { 1.0 }, if here { t.accent } else { t.border_field }),
            egui::StrokeKind::Inside,
        );
        let (text, color) = match code.digit(i) {
            Some(d) => (d.to_string(), t.fg),
            None if here => ("_".to_owned(), t.accent),
            None => (String::new(), t.fg),
        };
        p.text(r.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(theme.font(TypeToken::Heading)), color);
    }
}

impl Overlay for CastPair {
    fn key_contexts(&self) -> Vec<KeyContext> {
        // the digits and Backspace are read from the frame's events (no text field)
        vec![KeyContext::TextInput, KeyContext::Modal]
    }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::FocusNext => {
                self.step(1);
                OverlayOutcome::Keep
            }
            Action::FocusPrev => {
                self.step(-1);
                OverlayOutcome::Keep
            }
            // too soon: a key meant for what was there before the overlay
            Action::Activate if !self.armed() => OverlayOutcome::Keep,
            // acted on in `ui`, after this frame's typing was read
            Action::Activate => {
                self.enter = true;
                OverlayOutcome::Keep
            }
            Action::Back => self.cancel(),
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
        let cast = ctx.services.agent.cast();
        let name = cast.devices().into_iter().find(|d| d.id == self.device).map_or_else(|| "the TV".to_owned(), |d| d.name);
        if self.job.is_none() {
            let job = cast.pair_start(self.device);
            self.job = Some((cast.clone(), job));
        }
        let job = self.job.as_ref().map(|(_, j)| *j).unwrap_or(PairJob(0));
        let state = cast.pair_state(job);
        match state {
            PairState::Paired => {
                self.ended = true;
                ctx.toast(Toast::info(format!("Paired with {name}")));
                // opened to cast, what plays moves to the TV; Settings and the wizard only pair
                if self.then_cast && ctx.player.playable.is_some() {
                    ctx.play(PlayerCommand::CastTo { device: self.device, play: None });
                }
                return self.cancel();
            }
            PairState::Failed(e) => {
                if self.failure != Some(Failure::Pair(e)) {
                    self.code.clear();
                }
                self.failure = Some(Failure::Pair(e));
            }
            PairState::Gone => {
                if self.failure.is_none() {
                    self.code.clear();
                }
                self.failure = self.failure.or(Some(Failure::Interrupted));
            }
            PairState::Starting | PairState::Checking => egui_ctx.request_repaint_after(Duration::from_millis(100)),
            PairState::EnterCode { .. } => {}
        }
        self.read_keys(egui_ctx, state);
        if scrim(egui_ctx, theme, "cast-pair") && armed {
            return self.cancel();
        }
        let waiting = matches!(state, PairState::Starting | PairState::Checking);
        let entering = matches!(state, PairState::EnterCode { .. });
        let interrupted = matches!(self.failure, Some(Failure::Interrupted));
        let (mut cancel, mut pair, mut new_code) = (false, false, false);
        card_area(egui_ctx, theme, "cast-pair", CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            let lead = match state {
                PairState::Starting => format!("Asking {name} to show a code…"),
                PairState::Checking => "Checking the code…".to_owned(),
                _ => "Enter the code shown on the TV.".to_owned(),
            };
            title(ui, theme, &format!("Pair with {name}"), Some(&lead));
            ui.add_space(theme.space(SpaceToken::Lg));
            code_boxes(ui, theme, &self.code, entering && self.focus == Focus::Code);
            ui.add_space(theme.space(SpaceToken::Md));
            if waiting {
                busy_bar(ui, theme, 3.0);
            } else if let Some(f) = self.failure {
                ui.add(egui::Label::new(RichText::new(failure_copy(f, &name)).size(theme.font(TypeToken::Body)).color(t.urgent)).wrap());
            }
            ui.add_space(theme.space(SpaceToken::Sm));
            let hint = "No code on the TV? On the Apple TV open Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network.";
            ui.add(egui::Label::new(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).wrap());
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                let style = |f: Focus, primary: bool| ButtonStyle { primary, bordered: true, focused: self.focus == f, ..Default::default() };
                pair =
                    ui.add_enabled_ui(entering && self.code.full(), |ui| button(ui, theme, None, "Pair", style(Focus::Pair, true))).inner.clicked();
                cancel = button(ui, theme, None, "Cancel", style(Focus::Cancel, false)).clicked();
                new_code = ui.add_enabled_ui(!waiting, |ui| button(ui, theme, None, "Show a new code", style(Focus::NewCode, false))).inner.clicked();
            });
            ui.add_space(theme.space(SpaceToken::Sm));
            let hint = format!("{} pairs · {} moves · {} cancels", key(Action::Activate), key(Action::FocusNext), key(Action::Back));
            ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        // clicks count once the overlay has been up for a moment
        if armed {
            if cancel {
                return self.cancel();
            }
            if new_code {
                self.focus = Focus::Code;
                self.new_code(interrupted);
            } else if pair {
                self.submit(state);
            }
        }
        if std::mem::take(&mut self.enter) {
            match self.focus {
                Focus::Code | Focus::Pair => self.submit(state),
                Focus::NewCode if !waiting => self.new_code(interrupted),
                Focus::NewCode => {}
                Focus::Cancel => return self.cancel(),
            }
        }
        OverlayOutcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::cast::testing::{FakeCall, FakeCast};
    use crate::cast::{PairVariant, Protocol};
    use crate::model::ReceiverKind;
    use crate::ui::screens::home::testkit::{OvApp, has_label, overlay_harness, painted, rig};

    type H = egui_kittest::Harness<'static, OvApp<CastPair>>;

    const TV: DeviceId = DeviceId::from_bytes([1; 16]);

    /// The overlay for "Living Room", armed (the user has seen it), the TV showing a code.
    fn pairing() -> (H, Arc<FakeCast>) {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| {
            s.devices = vec![crate::cast::Device::new(TV, "Living Room", ReceiverKind::AppleTv, Protocol::AirPlay)];
            s.pair_state = Some(PairState::EnterCode { variant: PairVariant::Hkp3 });
        });
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastPair::new(TV, true), egui::vec2(1200.0, 800.0));
        h.state_mut().o.opened = Some(Instant::now() - ARMING);
        h.run_steps(2);
        (h, fake)
    }

    fn type_digits(h: &mut H, digits: &str) {
        h.event(egui::Event::Text(digits.to_owned()));
        h.run_steps(2);
    }

    fn backspace(h: &mut H) {
        h.event(egui::Event::Key { key: egui::Key::Backspace, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
    }

    #[test]
    fn each_way_pairing_stops_has_its_copy() {
        let tv = "Living Room";
        let copy = |e| failure_copy(Failure::Pair(e), tv);
        assert_eq!(copy(PairError::WrongPin), "That code didn't match. The TV is showing a new one.");
        assert_eq!(copy(PairError::Backoff { secs: None }), "Living Room wants a short break after several tries. Try again in a minute.");
        assert_eq!(copy(PairError::Backoff { secs: Some(60) }), "Living Room wants a short break after several tries. Try again in a minute.");
        assert_eq!(
            copy(PairError::Backoff { secs: Some(300) }),
            "Living Room wants a short break after several tries. Try again in about 5 minutes."
        );
        assert!(copy(PairError::Refused { home_only: false }).starts_with("On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access"));
        assert!(copy(PairError::Refused { home_only: true }).contains("'Everyone in your Home' blocks Tuner"));
        assert_eq!(copy(PairError::Expired), "The code expired.");
        assert_eq!(copy(PairError::Unreachable), "Living Room isn't answering. Is it on?");
        assert_eq!(failure_copy(Failure::Interrupted, tv), "Pairing was interrupted.");
    }

    #[test]
    fn the_typed_code_is_four_digits_zeroed_when_sent_and_never_printed() {
        let mut code = Code::default();
        for c in "47x11".chars() {
            code.push(c);
        }
        assert!(code.full());
        assert_eq!((code.digit(0), code.digit(3)), (Some('4'), Some('1')));
        code.pop();
        assert!(!code.full() && code.digit(3).is_none());
        code.push('9');
        let pin = code.take().expect("four digits");
        assert_eq!(pin.expose(), "4719");
        assert!(code.digits == [0; DIGITS] && code.len == 0, "zeroed once sent");
        assert!(code.take().is_none(), "nothing left to send");
        let mut o = CastPair::new(TV, true);
        "4711".chars().for_each(|c| o.code.push(c));
        let shown = format!("{o:?}");
        assert!(!shown.contains("4711") && shown.contains("typed: 4"), "{shown}");
    }

    /// The overlay arms after 600 ms; the digits go to the service once, on Enter, and are
    /// gone from the screen; **Show a new code** asks again; the TV is told when it closes.
    #[test]
    fn typing_the_code_and_pressing_enter_sends_it_once() {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.pair_state = Some(PairState::EnterCode { variant: PairVariant::Hkp3 }));
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastPair::new(TV, true), egui::vec2(1200.0, 800.0));
        // too soon: a digit or Enter was meant for the screen behind
        type_digits(&mut h, "4711");
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        h.run_steps(2);
        assert!(!fake.take_calls().iter().any(|c| matches!(c, FakeCall::PairSubmit(_))));
        assert_eq!(h.state().o.code.len, 0);
        h.state_mut().o.opened = Some(Instant::now() - ARMING);
        h.run_steps(1);
        assert!(has_label(&h, "Code, 0 of 4 digits"));
        type_digits(&mut h, "47");
        backspace(&mut h);
        type_digits(&mut h, "811");
        assert!(has_label(&h, "Code, 4 of 4 digits"));
        assert!(painted(&h).iter().any(|t| t == "8"), "{:?}", painted(&h));
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        h.run_steps(2);
        let calls = fake.take_calls();
        assert_eq!(calls.iter().filter(|c| matches!(c, FakeCall::PairSubmit(_))).count(), 1, "{calls:?}");
        assert!(!painted(&h).iter().any(|t| t == "8"), "the digits are gone");
        // Show a new code asks the same job again; Esc cancels it and closes
        h.get_by_label("Show a new code").click();
        h.run_steps(2);
        assert!(fake.take_calls().contains(&FakeCall::PairNewCode(PairJob(1))));
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        assert!(fake.take_calls().contains(&FakeCall::PairCancel(PairJob(1))));
    }

    /// Enter before four digits pairs nothing; a wrong code says so and clears the boxes; the
    /// code expiring while the user was away says "The code expired."
    #[test]
    fn a_short_code_a_wrong_code_and_an_expired_code() {
        let (mut h, fake) = pairing();
        type_digits(&mut h, "471");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(!fake.take_calls().iter().any(|c| matches!(c, FakeCall::PairSubmit(_))), "Enter needs four digits");
        type_digits(&mut h, "1");
        fake.script(|s| s.pair_state = Some(PairState::Failed(PairError::WrongPin)));
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "That code didn't match. The TV is showing a new one."));
        assert_eq!(h.state().o.code.len, 0, "the boxes are cleared");
        // the service asked again: the TV shows a new code; typing clears the notice
        fake.script(|s| s.pair_state = Some(PairState::EnterCode { variant: PairVariant::Hkp5 }));
        h.run_steps(2);
        type_digits(&mut h, "2");
        assert!(!painted(&h).iter().any(|t| t.starts_with("That code didn't match")));
        fake.script(|s| s.pair_state = Some(PairState::Failed(PairError::Expired)));
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "The code expired."));
        assert!(has_label(&h, "Show a new code"));
    }

    /// Success: the toast and the overlay closes; the finished job isn't cancelled.
    #[test]
    fn pairing_done_toasts_and_closes_without_cancelling() {
        let (mut h, fake) = pairing();
        fake.script(|s| s.pair_state = Some(PairState::Paired));
        h.run_steps(2);
        assert_eq!(h.state().last, "Close");
        assert!(h.state().t.toasts.iter().any(|t| t.text == "Paired with Living Room"), "{:?}", h.state().t.toasts);
        assert!(!fake.take_calls().contains(&FakeCall::PairCancel(PairJob(1))));
    }
}
