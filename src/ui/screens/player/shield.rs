//! The zap shield card (DESIGN §34 §2.3, S4). OWNER: S-ui.
//!
//! Zapping (↑/↓, the channel panel, a number) onto a live channel whose programme may show a
//! guarded result — it mentions an active topic and isn't the "until seen" event itself, or a
//! blackout covers it (`Guard::shield`) — covers the picture and asks:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ 󰈉  230 Sky Sports News                                │
//! │ This channel may show the Arsenal result.            │
//! │ [ Watch anyway ↵ ]  [ Next channel ↓ ]  [ Back ⌫ ]   │
//! └──────────────────────────────────────────────────────┘
//! ```
//! Enter (and a click) watches anyway — for this channel, this session; ↑/↓ zap on (the next
//! channel is judged again), Backspace goes back to the last channel, Esc leaves the player as
//! usual. Only zaps inside the player are shielded: a channel chosen in Live or the guide is a
//! choice, not a zap. Never while the guard is paused, off, or before its first build.

use std::cell::RefCell;
use std::collections::HashSet;

use super::*;
use crate::guard::Reason;

#[derive(Default)]
struct Shield {
    /// The live channel of the last frame (a different one now is a zap).
    last: Option<ChannelId>,
    /// The card is up over this channel.
    card: Option<(ChannelId, Reason)>,
    /// "Watch anyway" (this session).
    allowed: HashSet<ChannelId>,
    /// The egui pass the player last drew in.
    drawn_pass: Option<u64>,
}

thread_local! {
    static SHIELD: RefCell<Shield> = RefCell::default();
}

/// The live channel playing (`None`: catch-up, a film, nothing).
fn live_channel(snap: &PlayerSnapshot) -> Option<ChannelId> {
    match (snap.playable.as_ref().map(|p| (&p.media, &p.kind)), &snap.state) {
        (Some((MediaRef::Channel(id), PlayableKind::Live)), s) if *s != PlayerState::Idle => Some(*id),
        _ => None,
    }
}

impl PlayerScreen {
    /// Draw the shield card over `rect` when the channel zapped onto is shielded; `true` when
    /// drawn (the banner then stays hidden).
    pub(super) fn draw_shield(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) -> bool {
        if !crate::spoiler::ENABLED {
            return false;
        }
        let guard = ctx.services.agent.guard();
        let now = ctx.now;
        let channel = live_channel(snap);
        let pass = ui.ctx().cumulative_pass_nr();
        let card = SHIELD.with(|s| {
            let mut s = s.borrow_mut();
            // the player wasn't drawn last pass (Live, the guide…): what plays now was chosen,
            // not zapped onto
            if s.drawn_pass.is_none_or(|p| p + 1 < pass) {
                s.last = None;
                s.card = None;
            }
            s.drawn_pass = Some(pass);
            let zapped = channel.is_some() && s.last.is_some() && s.last != channel;
            s.last = channel;
            // another channel, or its programme no longer shielded (it ended, the topic lifted)
            if s.card.as_ref().is_some_and(|(c, _)| Some(*c) != channel || guard.shield(*c, now).is_none()) {
                s.card = None;
            }
            if zapped
                && let Some(ch) = channel
                && !s.allowed.contains(&ch)
                && let Some(reason) = guard.shield(ch, now)
            {
                s.card = Some((ch, reason));
            }
            // Enter (controls focus) on the card: watch anyway
            if s.card.is_some() && self.osd_focus {
                self.osd_focus = false;
                if let Some((ch, _)) = s.card.take() {
                    s.allowed.insert(ch);
                }
            }
            s.card.clone()
        });
        let Some((ch, reason)) = card else { return false };
        let choice = self.paint_shield(ui, ctx, rect, ch, &reason);
        match choice {
            Some(Action::Activate) => SHIELD.with(|s| {
                let mut s = s.borrow_mut();
                s.card = None;
                s.allowed.insert(ch);
            }),
            Some(a) => {
                self.on_action(ctx, a);
            }
            None => {}
        }
        // the card stays until the programme changes or its topic lifts
        ui.ctx().request_repaint_after(Duration::from_secs(30));
        true
    }

    /// The card: black over the picture, the reason, three buttons. Returns the button's
    /// meaning.
    fn paint_shield(&self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, ch: ChannelId, reason: &Reason) -> Option<Action> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::BLACK);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let body = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let w = (540.0 * s).min(rect.width() - 2.0 * pad);
        let button_h = (body.size * 2.0).round();
        let h = font.size * 1.6 + body.size * 1.5 + button_h + 3.0 * pad;
        let r = Rect::from_center_size(rect.center(), egui::vec2(w, h));
        p.rect_filled(r, t.rounding, t.osd_scrim);
        p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
        let name = ctx.lib.live.get(ch).map(|c| match c.number {
            Some(n) if ctx.config.interface.show_channel_numbers => format!("{n} {}", c.name),
            _ => c.name.to_string(),
        });
        let heading = format!("{}  {}", crate::ui::widgets::spoiler::HIDDEN, name.unwrap_or_default());
        osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad), Align2::LEFT_TOP, &heading, font.clone(), t.osd_accent, w - 2.0 * pad);
        let why = match reason.kind {
            crate::guard::ReasonKind::Blackout => format!("You said you'd watch {} later.", reason.rule),
            _ => format!("This channel may show the {} result.", reason.rule),
        };
        osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad + font.size * 1.6), Align2::LEFT_TOP, &why, body.clone(), t.osd_text, w - 2.0 * pad);
        let mut x = r.left() + pad / 2.0;
        let y = r.bottom() - pad / 2.0 - button_h;
        let mut chosen = None;
        let buttons: [(String, &str, Action); 3] = [
            (format!("{}  Watch anyway · Enter", glyphs::PLAY), "Watch anyway", Action::Activate),
            (format!("{}  Next channel · ↓", glyphs::CHEVRON_DOWN), "Next channel", Action::ZapNext),
            (format!("{}  Back · ⌫", glyphs::ARROW_LEFT), "Back to the last channel", Action::LastChannel),
        ];
        for (i, (text, label, action)) in buttons.into_iter().enumerate() {
            let bw = p.layout_no_wrap(text.clone(), body.clone(), t.osd_text).size().x + pad;
            let br = Rect::from_min_size(egui::pos2(x, y), egui::vec2(bw, button_h));
            x = br.right() + pad / 2.0;
            if osd_button(ui, &p, theme, br, ui.id().with(("spoiler-shield", i)), &text, body.clone(), label, None, i == 0).clicked() {
                chosen = Some(action);
            }
        }
        chosen
    }
}
