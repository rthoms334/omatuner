//! "Resume from 41:20" / "Start over" prompt (Resume focused first). Owner: WP-12.
//! Also used for the end-of-episode card "Next episode in 10 s" (Enter = now, Esc = cancel).
//!
//! The overlay starts playback itself (fullscreen, `PlayerCommand::Play` + `Route::Player`),
//! so it works the same whoever opened it (detail pages, Home, search, the player).
//! A spec with `position_s <= 0` (nothing to resume) is the up-next card: it counts down
//! [`UP_NEXT`] and then plays; Enter plays at once, Esc cancels (the player stays on the
//! ended episode). Key context `[Modal]`: ←/→/↑/↓/Tab move between the two buttons.

use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim, with_close};
use super::{Overlay, OverlayOutcome, ResumeSpec};
use crate::keymap::{Action, KeyContext};
use crate::model::PlayOrigin;
use crate::player::{PlayRequest, PlayerCommand};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::glyphs;
use crate::ui::{Route, UiCtx};

/// Countdown of the "Next episode" card.
pub const UP_NEXT: Duration = Duration::from_secs(10);

pub struct ResumeOverlay {
    pub spec: ResumeSpec,
    resume_focused: bool,
    /// Up-next mode: when the countdown started (first frame).
    started: Option<Instant>,
}

impl ResumeOverlay {
    pub fn new(spec: ResumeSpec) -> Self {
        Self { spec, resume_focused: true, started: None }
    }

    /// The up-next card (no position to resume from).
    fn up_next(&self) -> bool {
        self.spec.position_s <= 0.0
    }

    fn start(&self, ctx: &mut UiCtx<'_>, resume: bool) -> OverlayOutcome {
        if let Some(w) = self.spec.watchlist {
            crate::queue::ui::send(ctx, crate::queue::QueueCommand::PlayListFrom { list: w.list, from: w.from, just_one: w.just_one, resume });
            return OverlayOutcome::Close;
        }
        if let Some(q) = self.spec.queue {
            let sent = crate::queue::ui::send(ctx, crate::queue::QueueCommand::PlayAtFrom { id: q.id, resume });
            if sent && q.fullscreen {
                ctx.push(Route::Player);
            }
            return OverlayOutcome::Close;
        }
        let req = PlayRequest { media: self.spec.media.clone(), zap: None, origin: PlayOrigin::Fullscreen, resume, immediate: true };
        match &self.spec.copy {
            // a combined title: its fallback, and (resuming) the latest progress of any copy
            Some(copy) => {
                let start_at = if resume { copy.start_at } else { None };
                ctx.play(PlayerCommand::PlayCopy(crate::player::CopyPlay {
                    req,
                    fallback: copy.fallback.clone(),
                    elsewhere: copy.elsewhere.clone(),
                    start_at,
                }));
            }
            None => ctx.play(PlayerCommand::Play(req)),
        }
        ctx.push(Route::Player);
        OverlayOutcome::Close
    }

    /// Enter on the focused button.
    fn confirm(&self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if self.up_next() {
            // the focused button is "Play now" or "Cancel"
            return if self.resume_focused { self.start(ctx, false) } else { OverlayOutcome::Close };
        }
        self.start(ctx, self.resume_focused)
    }

    fn remaining(&self) -> Duration {
        self.started.map_or(UP_NEXT, |s| UP_NEXT.saturating_sub(s.elapsed()))
    }
}

impl Overlay for ResumeOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Left | Action::Right | Action::Up | Action::Down | Action::FocusNext | Action::FocusPrev => {
                self.resume_focused = !self.resume_focused;
                OverlayOutcome::Keep
            }
            Action::Activate => self.confirm(ctx),
            // Shift+Enter: start over / play now
            Action::ActivateAlt => self.start(ctx, false),
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let up_next = self.up_next();
        if up_next && self.started.is_none() {
            self.started = Some(Instant::now());
        }
        if up_next && self.remaining().is_zero() {
            return self.start(ctx, false);
        }
        if scrim(egui_ctx, theme, "resume") {
            return OverlayOutcome::Close;
        }
        let (title, detail, primary, secondary) = if up_next {
            let secs = self.remaining().as_secs_f32().ceil() as u32;
            (format!("Next episode in {secs} s"), self.spec.title.clone(), "Play now".to_owned(), "Cancel".to_owned())
        } else {
            let of = self.spec.duration_s.filter(|d| *d > 0.0).map(|d| format!(" of {}", clock(d))).unwrap_or_default();
            (
                self.spec.title.clone(),
                format!("Stopped at {}{of}", clock(self.spec.position_s)),
                format!("Resume from {}", clock(self.spec.position_s)),
                "Start over".to_owned(),
            )
        };
        let mut close = false;
        let (a, b) = card_area(egui_ctx, theme, "resume", CardKind::Popup, 420.0 * theme.rem(), None, |ui| {
            close = with_close(ui, theme, |ui| {
                ui.add(egui::Label::new(RichText::new(&title).size(theme.font(TypeToken::Heading)).color(t.fg)).wrap());
            });
            ui.add_space(theme.space(SpaceToken::Xs));
            ui.add(egui::Label::new(RichText::new(&detail).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
            if up_next {
                ui.add_space(theme.space(SpaceToken::Md));
                let f = 1.0 - self.remaining().as_secs_f32() / UP_NEXT.as_secs_f32();
                crate::ui::widgets::progress::progress_bar(ui, theme, f, (3.0 * theme.rem()).round().max(2.0));
            }
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                let a = button(
                    ui,
                    theme,
                    Some(glyphs::PLAY),
                    &primary,
                    ButtonStyle { primary: true, focused: self.resume_focused, ..Default::default() },
                )
                .clicked();
                let icon = if up_next { glyphs::CLOSE } else { glyphs::SKIP_BACK };
                let b =
                    button(ui, theme, Some(icon), &secondary, ButtonStyle { bordered: true, focused: !self.resume_focused, ..Default::default() })
                        .clicked();
                (a, b)
            })
            .inner
        })
        .inner;
        if up_next {
            egui_ctx.request_repaint_after(Duration::from_millis(200));
        }
        if close {
            return OverlayOutcome::Close;
        }
        match (a, b, up_next) {
            (true, _, true) => self.start(ctx, false),
            (true, _, false) => self.start(ctx, true),
            (_, true, true) => OverlayOutcome::Close,
            (_, true, false) => self.start(ctx, false),
            _ => OverlayOutcome::Keep,
        }
    }
}

/// "41:20" / "1:02:13".
fn clock(seconds: f64) -> String {
    crate::ui::screens::home::kit::clock(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaRef;

    fn spec(pos: f64) -> ResumeSpec {
        ResumeSpec {
            media: MediaRef::Url("x".into()),
            title: "T".into(),
            position_s: pos,
            duration_s: Some(100.0),
            copy: None,
            queue: None,
            watchlist: None,
        }
    }

    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{has_label, overlay_harness, rig};

    #[test]
    fn resume_start_over_and_the_up_next_card() {
        let (t, one) = rig();
        let mut h = overlay_harness(t, one, ResumeOverlay::new(spec(2480.0)), egui::vec2(1000.0, 700.0));
        assert!(has_label(&h, "Resume from 41:20") && has_label(&h, "Start over"));
        assert_eq!(h.state().o.key_contexts(), vec![KeyContext::Modal]);
        // Enter on the focused Resume: plays (fullscreen) with the saved position
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        // → moves to Start over
        h.state_mut().o = ResumeOverlay::new(spec(2480.0));
        assert_eq!(h.state_mut().act(Action::Right), "Keep");
        assert!(!h.state().o.resume_focused);
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().act(Action::Quit), "Ignored", "globals keep working");
        // up next: countdown copy, Esc cancels (stays on the ended episode)
        h.state_mut().o = ResumeOverlay::new(spec(0.0));
        h.run_steps(2);
        assert!(has_label(&h, "Next episode in 10 s") && has_label(&h, "Play now") && has_label(&h, "Cancel"));
        assert_eq!(h.state_mut().act(Action::Back), "Close");
        h.state_mut().nav();
        // the countdown runs out: it plays
        h.state_mut().o = ResumeOverlay::new(spec(0.0));
        h.state_mut().o.started = Instant::now().checked_sub(UP_NEXT);
        h.run_steps(1);
        assert_eq!(h.state().last, "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
    }

    #[test]
    fn modes_and_countdown() {
        let o = ResumeOverlay::new(spec(41.0));
        assert!(!o.up_next());
        assert!(o.resume_focused, "Resume is focused first");
        let mut n = ResumeOverlay::new(spec(0.0));
        assert!(n.up_next());
        assert_eq!(n.remaining(), UP_NEXT);
        n.started = Instant::now().checked_sub(Duration::from_secs(11));
        assert!(n.remaining().is_zero());
    }
}
