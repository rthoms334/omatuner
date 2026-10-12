//! The zap shield card (DESIGN §34 §2.3, S4). OWNER: S-ui.
//!
//! Zapping (↑/↓, the channel panel, a number) onto a live channel whose programme may show a
//! guarded result — it mentions an active topic and isn't the "until seen" event itself, or a
//! blackout covers it (`Guard::shield`) — doesn't tune it: the picture under the card is paused
//! (no sound, the account's one connection not spent on the new channel) and the card asks:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ 󰈉  230 Sky Sports News                                │
//! │ This channel may show the Arsenal result.            │
//! │ [ Watch anyway ↵ ]  [ Next channel ↓ ]  [ Back ⌫ ]   │
//! └──────────────────────────────────────────────────────┘
//! ```
//! Enter (and a click) tunes it anyway — for this channel while the same rule shields it; ↑/↓
//! zap on from the shielded channel (the next one is judged again), Backspace / ← drop the card
//! and resume the channel underneath, Esc leaves the player as usual. Only zaps inside the
//! player are shielded: a channel chosen in Live or the guide is a choice, not a zap. Never
//! while the guard is paused, off, or before its first build.
//!
//! Multi-view (DESIGN §35): the card covers only the picture that zapped — that picture pauses,
//! the other plays on. Its keys act while that picture has the focus; a click on the card of
//! the other picture focuses it first. One card at a time: a shielded zap in the other picture
//! moves the card there (the first picture plays its channel again).

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;
use crate::guard::Reason;
use crate::model::TileId;

#[derive(Default)]
struct Shield {
    /// The card is up over this channel (not tuned).
    card: Option<(ChannelId, Reason)>,
    /// Multi-view: the picture the card covers (`None`: single view).
    tile: Option<TileId>,
    /// The live channel under the card (paused), when the card went up.
    under: Option<ChannelId>,
    /// The card paused the picture under it (resumed when the card goes without tuning).
    paused_it: bool,
    /// "Watch anyway": the channel stays unshielded while the same reason shields it (a later
    /// event, another rule, shields it again).
    allowed: HashMap<ChannelId, Reason>,
    /// The egui pass the player last drew in.
    drawn_pass: Option<u64>,
}

thread_local! {
    static SHIELD: RefCell<Shield> = RefCell::default();
}

impl PlayerScreen {
    #[cfg(test)]
    /// The channel the shield card is up over (not tuned), whichever picture it covers.
    pub(super) fn shield_card(&self) -> Option<ChannelId> {
        SHIELD.with(|s| s.borrow().card.as_ref().map(|c| c.0))
    }

    #[cfg(test)]
    /// The picture the card covers (`None`: single view, or no card).
    pub(super) fn shield_tile(&self) -> Option<TileId> {
        SHIELD.with(|s| s.borrow().card.as_ref().and(s.borrow().tile))
    }

    /// The card over the picture that has the focus (its keys act): in single view, the card.
    fn card_here(ctx: &UiCtx<'_>) -> Option<ChannelId> {
        let here = Self::focused_tile(ctx);
        SHIELD.with(|s| {
            let s = s.borrow();
            s.card.as_ref().filter(|_| s.tile == here).map(|c| c.0)
        })
    }

    /// The picture keys act on (`None`: single view).
    fn focused_tile(ctx: &UiCtx<'_>) -> Option<TileId> {
        ctx.multiview.on().then_some(ctx.multiview.view.focus)
    }

    /// Pause or play the picture a card covers.
    /// (A single-view card's picture is A, also once multi-view has opened under it.)
    fn pause_picture(ctx: &UiCtx<'_>, tile: Option<TileId>, pause: bool) {
        match tile {
            Some(t) => ctx.play_in(t, PlayerCommand::SetPause(pause)),
            None if ctx.multiview.on() => ctx.play_in(TileId::A, PlayerCommand::SetPause(pause)),
            None => ctx.play(PlayerCommand::SetPause(pause)),
        }
    }

    /// Why `target` must not be tuned by a zap now (`None`: tune it).
    fn shield_reason(ctx: &UiCtx<'_>, target: ChannelId) -> Option<Reason> {
        if !crate::spoiler::ENABLED {
            return None;
        }
        let reason = ctx.services.agent.guard().shield(target, ctx.now)?;
        let allowed = SHIELD.with(|s| s.borrow().allowed.get(&target) == Some(&reason));
        (!allowed).then_some(reason)
    }

    /// Put the card up over `target` (nothing tuned) on the focused picture, which pauses; a
    /// card over the other picture goes (that picture plays on).
    fn raise_shield(ctx: &mut UiCtx<'_>, target: ChannelId, reason: Reason) {
        let tile = Self::focused_tile(ctx);
        let (pause, moved) = SHIELD.with(|s| {
            let mut s = s.borrow_mut();
            let moved = (s.card.is_some() && s.tile != tile).then(|| {
                s.card = None;
                s.under = None;
                (s.tile, std::mem::take(&mut s.paused_it))
            });
            let first = s.card.is_none();
            s.card = Some((target, reason));
            if first {
                s.tile = tile;
                s.under = playing_channel(ctx.player);
                s.paused_it = !ctx.player.paused && s.under.is_some();
            }
            (first && s.paused_it, moved)
        });
        if let Some((other, true)) = moved {
            Self::pause_picture(ctx, other, false);
        }
        if pause {
            Self::pause_picture(ctx, tile, true);
        }
    }

    /// Take the card down without tuning; `resume`: play the channel under it again.
    fn drop_shield(ctx: &mut UiCtx<'_>, resume: bool) {
        let (unpause, tile) = SHIELD.with(|s| {
            let mut s = s.borrow_mut();
            s.card = None;
            s.under = None;
            (std::mem::take(&mut s.paused_it), s.tile.take())
        });
        if unpause && resume {
            Self::pause_picture(ctx, tile, false);
        }
    }

    /// A zap (↑/↓) while the shield may act: `true` when it took the zap. With the card up the
    /// zap goes on from the shielded channel; otherwise a shielded neighbour gets the card
    /// instead of a tune.
    pub(super) fn shield_zap(&mut self, ctx: &mut UiCtx<'_>, forward: bool) -> bool {
        if !crate::spoiler::ENABLED {
            return false;
        }
        let card = Self::card_here(ctx);
        if card.is_none() && !ctx.services.agent.guard().on() {
            return false;
        }
        let from = card.or_else(|| playing_channel(ctx.player));
        let (Some(from), Some(zap)) = (from, ctx.player.zap.as_ref()) else { return card.is_some() };
        let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
        let hides_explicit = ctx.config.parental.hides_explicit(unlocked);
        let failing = super::live::failing_set(ctx).unwrap_or_default();
        let lib = ctx.lib.clone();
        let skip = |c: ChannelId| {
            lib.live.get(c).is_none_or(|x| (x.locked && !unlocked) || x.unsupported || failing.contains(&live::channel_ref(x)))
                || (hides_explicit && lib.explicit.channels.contains(&c))
        };
        let Some(next) = LiveIndex::neighbour(&zap.channels, from, forward, &skip) else { return card.is_some() };
        if let Some(reason) = Self::shield_reason(ctx, next) {
            Self::raise_shield(ctx, next, reason);
            return true;
        }
        if card.is_none() {
            return false; // the player's own zap
        }
        // from the card onto an unshielded channel
        if Some(next) == SHIELD.with(|s| s.borrow().under) {
            Self::drop_shield(ctx, true);
        } else {
            Self::drop_shield(ctx, false);
            ctx.play(PlayerCommand::ZapTo { id: next, unlocked });
        }
        true
    }

    /// A channel tuned from a panel or a number: `true` when the shield put its card up instead.
    pub(super) fn shield_tune(&mut self, ctx: &mut UiCtx<'_>, id: ChannelId) -> bool {
        match Self::shield_reason(ctx, id) {
            Some(reason) => {
                Self::raise_shield(ctx, id, reason);
                true
            }
            None => {
                // tuned on purpose: whatever card was up goes (the load plays)
                if Self::card_here(ctx).is_some() {
                    Self::drop_shield(ctx, Some(id) == SHIELD.with(|s| s.borrow().under));
                }
                false
            }
        }
    }

    /// Keys while the card is up: Enter watches anyway, Backspace / ← go back to the channel
    /// under the card. ↑/↓ reach [`Self::shield_zap`] through the player's zap.
    pub(super) fn shield_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        Self::card_here(ctx)?;
        match action {
            Action::FocusOsdControls | Action::Activate => {
                self.watch_anyway(ctx);
                Some(true)
            }
            Action::LastChannel => {
                Self::drop_shield(ctx, true);
                Some(true)
            }
            _ => None,
        }
    }

    /// "Watch anyway": tune the shielded channel; it stays unshielded while the same reason
    /// shields it.
    fn watch_anyway(&mut self, ctx: &mut UiCtx<'_>) {
        let Some((ch, reason)) = SHIELD.with(|s| s.borrow().card.clone()) else { return };
        SHIELD.with(|s| s.borrow_mut().allowed.insert(ch, reason));
        Self::drop_shield(ctx, false);
        self.tune(ctx, ch, Action::Activate);
    }

    /// Draw the shield card over `rect` while it is up; `true` when drawn.
    pub(super) fn draw_shield(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) -> bool {
        if !crate::spoiler::ENABLED {
            return false;
        }
        let pass = ui.ctx().cumulative_pass_nr();
        let here = Self::focused_tile(ctx);
        let (card, tile, gone, unpause) = SHIELD.with(|s| {
            let mut s = s.borrow_mut();
            // the player wasn't drawn last pass (Live, the guide…): the card went with it
            let left = s.drawn_pass.is_none_or(|p| p + 1 < pass);
            s.drawn_pass = Some(pass);
            // multi-view opened or closed under the card: it goes, its picture plays again
            let mode = s.card.is_some() && s.tile.is_some() != ctx.multiview.on();
            // something else plays now in its picture (a choice in Live, an agent, a reminder)
            let under_now = match s.tile {
                Some(t) => ctx.multiview.snap(t).and_then(|p| playing_channel(p)),
                None => playing_channel(snap),
            };
            let replaced = s.card.is_some() && !mode && under_now != s.under;
            let mut unpause = None;
            if left || replaced || mode {
                if mode && s.paused_it {
                    unpause = Some(s.tile);
                }
                s.card = None;
                s.under = None;
                s.paused_it = false;
            }
            // the programme no longer shielded (it ended, the topic lifted): tune what was zapped
            // to — in multi-view once its picture has the focus again
            let gone =
                s.card.as_ref().filter(|_| s.tile == here).filter(|(c, _)| ctx.services.agent.guard().shield(*c, ctx.now).is_none()).map(|c| c.0);
            (s.card.clone(), s.tile, gone, unpause)
        });
        if let Some(t) = unpause {
            Self::pause_picture(ctx, t, false);
        }
        if let Some(ch) = gone {
            Self::drop_shield(ctx, false);
            self.tune(ctx, ch, Action::Activate);
            return false;
        }
        let Some((ch, reason)) = card else { return false };
        // multi-view: over its own picture only
        let area = tile.and_then(|t| self.shield_rect(t)).unwrap_or(rect);
        match self.paint_shield(ui, ctx, area, ch, &reason) {
            // the card of the other picture: a click chooses that picture first
            Some(_) if tile != here => {
                if let Some(t) = tile {
                    ctx.mv_request(crate::ui::multiview::MvRequest::Focus(t));
                }
            }
            Some(Action::Activate) => self.watch_anyway(ctx),
            Some(Action::ZapNext) => self.zap(ctx, true),
            Some(Action::LastChannel) => Self::drop_shield(ctx, true),
            _ => {}
        }
        // the card stays until the programme changes or its topic lifts
        ui.ctx().request_repaint_after(Duration::from_secs(30));
        true
    }

    /// Where picture `tile` is (multi-view, last frame's layout).
    fn shield_rect(&self, tile: TileId) -> Option<Rect> {
        self.mv_placed().iter().find(|p| p.tile == tile).map(|p| p.video)
    }

    /// The card: black over the picture, the reason, three buttons (their keys alone when the
    /// picture is too small for the words). Returns the button's meaning.
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
        let mut buttons: [(String, &str, Action); 3] = [
            (format!("{}  Watch anyway · Enter", glyphs::PLAY), "Watch anyway", Action::Activate),
            (format!("{}  Next channel · ↓", glyphs::CHEVRON_DOWN), "Next channel", Action::ZapNext),
            (format!("{}  Back · ⌫", glyphs::ARROW_LEFT), "Back to the last channel", Action::LastChannel),
        ];
        let width = |b: &[(String, &str, Action); 3]| {
            b.iter().map(|(text, ..)| p.layout_no_wrap(text.clone(), body.clone(), t.osd_text).size().x + pad * 1.5).sum::<f32>()
        };
        if width(&buttons) > w {
            for (b, short) in buttons.iter_mut().zip([
                format!("{} Enter", glyphs::PLAY),
                format!("{} ↓", glyphs::CHEVRON_DOWN),
                format!("{} ⌫", glyphs::ARROW_LEFT),
            ]) {
                b.0 = short;
            }
        }
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
