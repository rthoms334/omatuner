//! Cards over the video: end of playback (next episode), the error card's other copy, the
//! idle card. OWNER (batch): Q-ui — the Up next card replaces the next-episode card here
//! (DESIGN §32).
//!
//! With Up next on, the engine runs the card between items and the empty-queue next-episode
//! countdown (so it also runs in the mini player, the pop-out and a hidden window); this file
//! draws [`crate::queue::ui::card::Card`] from its snapshot and sends the keys and buttons as
//! commands. Esc ladder: the side panel / number entry first (above), then the card's Esc
//! cancels, then the OSD steps; popped out, the card's Esc comes before the pop-in.

use super::*;
use crate::queue::ui::card::{Button as CardButton, Card, Step};
use crate::queue::{QueueCommand, ui as qui};

pub(super) struct NextEpisode {
    pub(super) id: EpisodeId,
    pub(super) label: String,
    pub(super) at: Instant,
}

pub(super) const NEXT_EPISODE_DELAY: Duration = Duration::from_secs(10);

/// The player menu's "Send as is", "Play here" and "Convert this channel" (codes above typed
/// digits and the Ctrl+O copies, below multi-view's `CLOSE_OTHER`).
pub(super) const CAST_AS_IS: u8 = 247;
pub(super) const CAST_PLAY_HERE: u8 = 248;
pub(super) const CAST_CONVERT: u8 = 249;

impl PlayerScreen {
    /// Up next's player keys (`u` panel, `n` / `N`; DESIGN §32): `None` when not one of them
    /// or Up next is off. Q-ui.
    pub(super) fn queue_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if !crate::queue::ENABLED {
            return None;
        }
        match action {
            Action::Undo(n) if qui::is_undo(n) => return Some(qui::undo(ctx, n)),
            // the PIN card's PIN was typed: publish the unlock (the engine re-reads it), proceed
            Action::Digit(qui::PIN_CODE) => {
                if ctx.parental.is_unlocked(&ctx.config.parental) {
                    ctx.services.agent.set_unlocked_at(ctx.parental.unlocked_at());
                    qui::send(ctx, QueueCommand::Proceed { unlocked: true });
                }
                return Some(true);
            }
            _ => {}
        }
        let s = qui::snapshot(ctx);
        // the card shows (and takes its keys) in single view only
        if let Some(card) = Card::of(&s, ctx.now).filter(|_| !ctx.multiview.on()) {
            // popped out, Esc is the pop-in key: the card has it first
            let key = if ctx.video_only && action == Action::TogglePopOut { Action::Back } else { action };
            if let Some(step) = card.key(key) {
                self.card_step(ctx, step);
                return Some(true);
            }
        }
        match action {
            Action::QueuePanel => self.open_panel(ctx, PanelKind::Queue),
            Action::QueueNext | Action::QueuePrev => {
                let text = if ctx.services.agent.tiles().on {
                    Some("Up next waits while multi-view is on")
                } else if action == Action::QueueNext && s.next.is_none() {
                    Some("Up next is empty")
                } else if action == Action::QueuePrev && !s.rows.iter().any(|r| r.entry.state.is_done()) {
                    Some("Nothing before this in Up next")
                } else {
                    None
                };
                match text {
                    Some(t) => self.flash_text = Some((t.into(), Instant::now())),
                    None => {
                        qui::send(ctx, if action == Action::QueueNext { QueueCommand::Next } else { QueueCommand::Prev });
                    }
                }
            }
            _ => return None,
        }
        Some(true)
    }

    /// A card key or button.
    fn card_step(&mut self, ctx: &mut UiCtx<'_>, step: Step) {
        match step {
            Step::Send(c) => {
                qui::send(ctx, c);
            }
            Step::Pin(reason) => ctx.open(OverlayRequest::Pin { then: Box::new(Action::Digit(qui::PIN_CODE)), reason }),
            Step::List => {
                if self.panel.as_ref().is_none_or(|p| p.kind != PanelKind::Queue) {
                    self.open_panel(ctx, PanelKind::Queue);
                }
            }
            Step::PlayEpisode(id) => {
                ctx.play(PlayerCommand::Play(PlayRequest {
                    media: MediaRef::Episode(id),
                    zap: None,
                    origin: PlayOrigin::Fullscreen,
                    resume: true,
                    immediate: true,
                }));
                qui::send(ctx, QueueCommand::Cancel);
                self.show_banner(ctx);
            }
        }
    }

    /// The Up next card over the video (above the bottom bar while it shows); popped out, a
    /// strip along the bottom edge with glyph buttons. Single view only: in multi-view Up next
    /// waits, and `n` says so.
    pub(super) fn draw_queue_card(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        if !crate::queue::ENABLED || ctx.multiview.on() {
            return;
        }
        let s = qui::snapshot(ctx);
        let Some(card) = Card::of(&s, ctx.now) else { return };
        let theme = ctx.theme;
        let t = &theme.tokens;
        let sc = scale_of(ui);
        let p = ui.painter_at(rect);
        let pad = (theme.space(SpaceToken::PopupPadding) * sc).round();
        let (head, line) = card.lines(ctx.now, ctx.config.interface.clock_24h);
        let buttons = card.buttons();
        let small = FontId::proportional((theme.font(TypeToken::Body) * sc).round());
        let mut pressed = None;
        if ctx.video_only {
            let h = (small.size * 2.2).round();
            let r = Rect::from_min_size(egui::pos2(rect.left() + pad, rect.bottom() - pad - h), egui::vec2(rect.width() - 2.0 * pad, h));
            p.rect_filled(r, t.rounding, t.osd_scrim);
            let mut right = r.right();
            for (i, b) in buttons.iter().enumerate().rev() {
                let (glyph, label, key) = b.parts();
                let br = Rect::from_min_size(egui::pos2(right - h, r.top()), egui::vec2(h, h));
                right = br.left();
                let tip = format!("{label} ({key})");
                if osd_button(ui, &p, theme, br, ui.id().with(("queue-strip", i)), glyph, small.clone(), label, Some(&tip), false).clicked() {
                    pressed = Some(*b);
                }
            }
            osd_text(&p, egui::pos2(r.left() + pad, r.center().y), Align2::LEFT_CENTER, &head, small, t.osd_text, right - r.left() - 2.0 * pad);
        } else {
            let font = FontId::proportional((theme.font(TypeToken::Title) * sc).round());
            let w = (460.0 * sc).min(rect.width() - 2.0 * pad);
            let button_h = (small.size * 2.0).round();
            let lines_h = if line.is_some() { font.size * 2.6 } else { font.size * 1.4 };
            let h = lines_h + button_h + 2.5 * pad;
            // above the bottom OSD bar while it shows, else near the bottom edge
            let bottom = rect.bottom() - self.bottom_bar.unwrap_or(0.0) - 2.0 * pad;
            let r = Rect::from_min_size(egui::pos2(rect.right() - w - pad, (bottom - h).max(rect.top() + pad)), egui::vec2(w, h));
            // the card takes the clicks that miss a button (they would reach the video)
            let _ = ui.interact(r, ui.id().with("queue-card"), egui::Sense::click());
            p.rect_filled(r, t.rounding, t.osd_scrim);
            p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
            osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad), Align2::LEFT_TOP, &head, font.clone(), t.osd_accent, w - 2.0 * pad);
            if let Some(l) = &line {
                osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad + font.size * 1.3), Align2::LEFT_TOP, l, font, t.osd_text, w - 2.0 * pad);
            }
            let mut x = r.left() + pad / 2.0;
            for (i, b) in buttons.iter().enumerate() {
                let (glyph, label, key) = b.parts();
                let label = match (b, &card) {
                    (CardButton::PlayOffer, Card::Finished { offer: Some((_, l)), .. }) => l.split(" · ").next().unwrap_or(label).to_owned(),
                    _ => label.to_owned(),
                };
                let text = format!("{glyph}  {label} · {key}");
                let bw = p.layout_no_wrap(text.clone(), small.clone(), t.osd_text).size().x + pad;
                if x + bw > r.right() {
                    break;
                }
                let br = Rect::from_min_size(egui::pos2(x, r.bottom() - pad / 2.0 - button_h), egui::vec2(bw, button_h));
                x = br.right() + pad / 2.0;
                let tip = format!("{label} ({key})");
                if osd_button(ui, &p, theme, br, ui.id().with(("queue-card-button", i)), &text, small.clone(), &label, Some(&tip), false).clicked() {
                    pressed = Some(*b);
                }
            }
        }
        if card.counts_down() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        if let Some(b) = pressed {
            let step = card.press(b);
            self.card_step(ctx, step);
        }
    }

    pub(super) fn end_of_playback(&mut self, ctx: &mut UiCtx<'_>) {
        // Up next's engine runs the next-episode countdown (any placement, a hidden window);
        // without it (it never started) the screen's own card is the fallback
        if crate::queue::ENABLED && ctx.services.agent.queue_handle().is_some() {
            return;
        }
        // a finished movie goes back to its page from `movie_detail` (one owner for that
        // navigation); an episode gets the up-next card here, over the video
        if let Some(MediaRef::Episode(id)) = ctx.player.playable.as_ref().map(|p| &p.media) {
            self.next_lookup = Some(*id);
            self.find_next_episode(ctx);
        }
    }

    /// The episode after the one that ended (two indexed reads, inside the frame budget;
    /// retried next frame when the budget is spent).
    pub(super) fn find_next_episode(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(id) = self.next_lookup else { return };
        let db = ctx.db;
        let Some(next) = ctx.budget.run(|| {
            crate::store::vod::episode(db, id).and_then(|e| {
                let eps = crate::store::vod::episodes(db, e.series_id, None)?;
                Ok(eps.iter().position(|x| x.id == id).and_then(|i| eps.get(i + 1).cloned()))
            })
        }) else {
            return;
        };
        self.next_lookup = None;
        match next {
            Ok(Some(e)) => {
                self.next_episode = Some(NextEpisode {
                    id: e.id,
                    label: format!("S{:02}E{:02} · {}", e.season, e.episode, crate::ui::widgets::rows::episode_title(&e)),
                    at: Instant::now() + NEXT_EPISODE_DELAY,
                })
            }
            Ok(None) => {}
            Err(err) => tracing::debug!("next episode: {err}"),
        }
    }

    pub(super) fn play_next_episode(&mut self, ctx: &mut UiCtx<'_>) {
        if let Some(n) = self.next_episode.take() {
            ctx.play(PlayerCommand::Play(PlayRequest {
                media: MediaRef::Episode(n.id),
                zap: None,
                origin: PlayOrigin::Fullscreen,
                resume: true,
                immediate: true,
            }));
            self.show_banner(ctx);
        }
    }

    /// The other copy offered for the current failure, if any.
    pub(super) fn offered(&self, snap: &PlayerSnapshot) -> Option<&crate::ui::copies::OtherCopy> {
        let (generation, other) = self.other_copy.as_ref()?;
        if *generation != snap.load_generation || !matches!(snap.state, PlayerState::Error(_)) {
            return None;
        }
        other.as_ref()
    }

    /// Look the failed title's other copy up once per failure (a few indexed reads, within the
    /// frame budget) and hand its label to the error card.
    pub(super) fn update_other_copy(&mut self, ctx: &mut UiCtx<'_>, snap: &PlayerSnapshot) {
        let vod = snap.playable.as_ref().is_some_and(|p| matches!(p.media, MediaRef::Movie(_) | MediaRef::Episode(_)));
        if !(vod && matches!(snap.state, PlayerState::Error(_))) {
            ctx.video.set_other_copy(None);
            return;
        }
        if self.other_copy.as_ref().is_none_or(|(g, _)| *g != snap.load_generation) {
            let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
            let (db, lib, order, now) = (ctx.db, ctx.lib, ctx.config.interface.copy_order, ctx.now);
            let found = ctx.budget.run(|| {
                let plan_of = |movies: bool, item: u32| crate::store::copies::plan(db, &lib.vod, movies, item, unlocked, order, now).ok();
                crate::ui::copies::other_copy(db, lib, plan_of, snap)
            });
            if let Some(other) = found {
                self.other_copy = Some((snap.load_generation, other));
            }
        }
        ctx.video.set_other_copy(self.offered(snap).map(|o| o.label.clone()));
    }

    // ------------------------------------------------------------------ casting (DESIGN §40)

    /// `C`, the OSD's Cast control and the menu's "Cast to a TV…": the device picker.
    pub(super) fn cast_menu(&mut self, ctx: &mut UiCtx<'_>) {
        ctx.open(OverlayRequest::CastPicker);
    }

    /// The player menu's casting items: "Cast to a TV…" (also to switch TV), and while casting
    /// Stop casting, Play here and Convert this channel (L3 for this item on this TV). When the
    /// TV is fed L3 only because it was forced, a remembered choice or failure (`as_is`), that
    /// last slot is **Send as is**, which forgets it (DESIGN §40 §3.3, §8.3); a format that
    /// needs converting leaves Convert this channel greyed.
    pub(super) fn cast_menu_items(&self, ctx: &UiCtx<'_>, k: KeyContext) -> Vec<crate::ui::overlays::MenuItem> {
        let Some(view) = ctx.player.cast.as_ref() else {
            return if crate::cast::on(ctx.config) {
                vec![menu_item(glyphs::CAST, "Cast to a TV…", Action::CastMenu, true, k)]
            } else {
                Vec::new()
            };
        };
        let converted = view.level == Some(crate::model::CastLevel::Converted);
        vec![
            menu_item(glyphs::CAST_CONNECTED, "Cast to a TV…", Action::CastMenu, true, k),
            menu_item(glyphs::STOP, "Stop casting", Action::StopCasting, true, k),
            menu_item(glyphs::PLAY, "Play here", Action::Digit(CAST_PLAY_HERE), true, k),
            if converted && view.as_is {
                menu_item(glyphs::SYNC, "Send as is", Action::Digit(CAST_AS_IS), true, k)
            } else {
                menu_item(glyphs::SYNC, "Convert this channel", Action::Digit(CAST_CONVERT), !converted, k)
            },
        ]
    }

    /// What the casting card adds to the snapshot: the programme now through the spoiler guard,
    /// like the OSD's, and whether converting is off (set before each paint, DESIGN §40 §3.3).
    pub(super) fn update_cast_context(&mut self, ctx: &mut UiCtx<'_>, snap: &PlayerSnapshot) {
        if snap.cast.is_none() {
            return;
        }
        let now = snap
            .playable
            .as_ref()
            .filter(|p| matches!(p.kind, PlayableKind::Live))
            .and_then(|p| match p.media {
                MediaRef::Channel(id) => ctx.lib.live.get(id).cloned(),
                _ => None,
            })
            .and_then(|c| live::now_next_of(ctx, &c))
            .and_then(|n| n.now)
            .map(|now| {
                let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
                crate::ui::video_host::CastNow {
                    line: self.banner_cats.shown(ctx, &now).line(&spoilers),
                    range: time_range(now.start, now.stop, ctx.config.interface.clock_24h),
                    progress: now.progress(ctx.now),
                }
            });
        let convert_off = ctx.config.cast.convert == crate::config::CastConvert::Never;
        ctx.video.set_cast_context(crate::ui::video_host::CastContext { now, convert_off });
    }

    /// A casting card button (DESIGN §40 §3.3, §3.4).
    pub(super) fn cast_state_action(&mut self, ctx: &mut UiCtx<'_>, action: StateAction, snap: &PlayerSnapshot) {
        let Some(view) = snap.cast.as_ref() else { return };
        let cast = ctx.services.agent.cast();
        match action {
            StateAction::StopCasting => ctx.play(PlayerCommand::StopCasting { play_here: false }),
            StateAction::PlayHere => ctx.play(PlayerCommand::StopCasting { play_here: true }),
            // nothing is replayed from here: with nothing playing the controller takes the last item
            StateAction::TryAgain | StateAction::CastAgain => ctx.play(PlayerCommand::CastTo { device: view.device, play: None }),
            StateAction::ConvertChannel => cast.convert_current(),
            // pkexec's dialog is Omarchy's; the firewall card shows "Waiting for your password…"
            StateAction::AllowFirewall => {
                if cast.firewall().is_some_and(|d| d.can_allow()) {
                    cast.firewall_allow();
                }
                ctx.open(OverlayRequest::CastFirewall);
            }
            StateAction::ShowCommand => ctx.open(OverlayRequest::CastFirewall),
            StateAction::Pair => ctx.open(OverlayRequest::CastPair { device: view.device, then_cast: true }),
            StateAction::Retry | StateAction::LastChannel | StateAction::OtherCopy | StateAction::Stop => {}
        }
        self.show_banner(ctx);
    }

    /// Nothing playing: what to do about it and a button for it. In the full window a click
    /// anywhere goes back; popped out, the button (or a double-click) pops the window back in.
    pub(super) fn idle_ui(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::BLACK);
        let (title, hint, title_font, hint_font) = if ctx.video_only {
            ("Nothing playing", "o · Esc", TypeToken::Subtitle, TypeToken::BodySmall)
        } else {
            ("Nothing is playing", "Esc goes back", TypeToken::Title, TypeToken::Body)
        };
        let background = ui.interact(rect, ui.id().with("player-idle"), egui::Sense::click());
        osd_text(&p, rect.center(), Align2::CENTER_BOTTOM, title, FontId::proportional(theme.font(title_font)), t.osd_text, rect.width());
        let font = FontId::proportional(theme.font(hint_font));
        let hr =
            osd_text(&p, rect.center() + egui::vec2(0.0, 8.0), Align2::CENTER_TOP, hint, font.clone(), with_alpha(t.osd_text, 0.7), rect.width());
        let (text, label) = if ctx.video_only {
            (format!("{}  Back to the full window", glyphs::POP_IN), "Back to the full window".to_owned())
        } else {
            // like every other back button: says where it goes
            let label = ctx.back.map_or_else(|| "Back".to_owned(), |b| format!("Back to {}", b.label));
            (format!("{}  {label}", glyphs::CHEVRON_LEFT), label)
        };
        let pad = theme.space(SpaceToken::Md);
        let size = p.layout_no_wrap(text.clone(), font.clone(), t.osd_text).size() + egui::vec2(2.0 * pad, pad);
        let br = Rect::from_center_size(egui::pos2(rect.center().x, hr.bottom() + pad + size.y / 2.0), size);
        p.rect_stroke(br, t.rounding, egui::Stroke::new(1.0, with_alpha(t.osd_text, 0.6)), egui::StrokeKind::Inside);
        let button = osd_button(ui, &p, theme, br, ui.id().with("player-idle-back"), &text, font, &label, None, false).clicked();
        if ctx.video_only {
            if button || background.double_clicked() {
                ctx.navigate(NavRequest::TogglePopOut);
            }
        } else if button || background.clicked() {
            ctx.navigate(NavRequest::Back);
        }
    }

    pub(super) fn draw_overlays(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let p = ui.painter_at(rect);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        // numeric entry
        if let Some((digits, _)) = &self.numeric {
            let font = FontId::proportional((theme.font(TypeToken::DisplayLarge) * s * 1.4).round());
            let name = digits
                .parse::<u32>()
                .ok()
                .and_then(|n| ctx.lib.live.by_number(n))
                .and_then(|id| ctx.lib.live.get(id))
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let g = p.layout_no_wrap(format!("{digits}_"), font, t.osd_accent);
            let r = Rect::from_min_size(
                egui::pos2(rect.right() - g.size().x - 3.0 * pad, rect.top() + pad * 6.0),
                g.size() + egui::vec2(2.0 * pad, 2.0 * pad),
            );
            let name_font = FontId::proportional((theme.font(TypeToken::Body) * s).round());
            let extra = if name.is_empty() { 0.0 } else { (name_font.size * 2.0).round() };
            p.rect_filled(r.union(Rect::from_min_size(r.left_bottom(), egui::vec2(r.width(), extra))), t.rounding, t.osd_scrim);
            p.galley(r.min + egui::vec2(pad, pad), g, t.osd_accent);
            if !name.is_empty() {
                osd_text(&p, egui::pos2(r.center().x, r.bottom() + extra / 2.0), Align2::CENTER_CENTER, &name, name_font, t.osd_text, r.width());
            }
        }
        // flash message (track / delay / volume changes)
        let text = self
            .flash
            .filter(|(_, at)| at.elapsed() < FLASH)
            .map(|(f, _)| flash_text(f, snap, &self.zoom))
            .or_else(|| self.flash_text.as_ref().filter(|(_, at)| at.elapsed() < FLASH).map(|(t, _)| t.clone()));
        if let Some(text) = text {
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let g = p.layout_no_wrap(text, font, t.osd_text);
            let r = Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + pad * 9.0), g.size() + egui::vec2(2.0 * pad, pad));
            p.rect_filled(r, t.rounding, t.osd_scrim);
            p.galley(r.min + egui::vec2(pad, pad / 2.0), g, t.osd_text);
        }
        // next episode card: a click on it plays now; its buttons play now / cancel
        if let Some((at, label)) = self.next_episode.as_ref().map(|n| (n.at, n.label.clone())) {
            let left = at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
            let w = (420.0 * s).min(rect.width() - 2.0 * pad);
            let button_h = (small.size * 2.0).round();
            let h = font.size * 2.6 + button_h + 2.5 * pad;
            // above the bottom OSD bar while it shows, else near the bottom edge
            let bottom = rect.bottom() - self.bottom_bar.unwrap_or(0.0) - 2.0 * pad;
            let r = Rect::from_min_size(egui::pos2(rect.right() - w - pad, (bottom - h).max(rect.top() + pad)), egui::vec2(w, h));
            let card = ui.interact(r, ui.id().with("next-episode"), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            p.rect_filled(r, t.rounding, t.osd_scrim);
            p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
            osd_text(
                &p,
                egui::pos2(r.left() + pad, r.top() + pad),
                Align2::LEFT_TOP,
                &format!("Next episode in {left} s"),
                font.clone(),
                t.osd_accent,
                w - 2.0 * pad,
            );
            osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad + font.size * 1.3), Align2::LEFT_TOP, &label, font, t.osd_text, w - 2.0 * pad);
            let mut x = r.left() + pad / 2.0;
            let mut button = |text: String, label: &str, tip: &str, i: u8| {
                let bw = p.layout_no_wrap(text.clone(), small.clone(), t.osd_text).size().x + pad;
                let br = Rect::from_min_size(egui::pos2(x, r.bottom() - pad / 2.0 - button_h), egui::vec2(bw, button_h));
                x = br.right() + pad / 2.0;
                osd_button(ui, &p, theme, br, ui.id().with(("next-episode-button", i)), &text, small.clone(), label, Some(tip), false).clicked()
            };
            // the keys stay on the buttons (the card used to be keys only)
            let play = button(format!("{}  Play now · Enter", glyphs::PLAY), "Play now", "Play now (Enter)", 0);
            let cancel = button(format!("{}  Cancel · Esc", glyphs::CLOSE), "Cancel", "Cancel (Esc)", 1);
            if cancel {
                self.next_episode = None;
            } else if play || card.clicked() {
                self.play_next_episode(ctx);
            }
        }
        self.draw_queue_card(ui, ctx, rect);
    }
}

/// Up next over the video (kittest over `TestUi` with the real engine; the card comes from a
/// published snapshot, so it shows without anything playing to its end).
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eframe::egui;

    use super::*;
    use crate::model::{Playable, QueueId};
    use crate::queue::{Countdown, CountdownWhy, QueueRow, QueueSnapshot, RowState};
    use crate::ui::screens::home::testkit::{App, harness, has_label, painted, rig, until};

    type H = egui_kittest::Harness<'static, App<PlayerScreen>>;

    /// A film playing, the player entered, the engine running.
    fn player(video_only: bool, size: egui::Vec2) -> H {
        let (t, one) = rig();
        player_on(t, one, video_only, size).0
    }

    /// [`player`] on a rig the test seeded; also the main picture's commands.
    fn player_on(
        mut t: crate::ui::testing::TestUi,
        one: std::sync::MutexGuard<'static, ()>,
        video_only: bool,
        size: egui::Vec2,
    ) -> (H, Arc<crate::ui::screens::queue::tests::Rec>) {
        t.video_only = video_only;
        let rec = crate::ui::screens::queue::tests::start_engine(&t);
        let mut s = (*t.services().tiles.focused().snapshot()).clone();
        s.state = PlayerState::Playing;
        s.playable = Some(Arc::new(Playable {
            media: MediaRef::Movie(crate::model::MovieId(1)),
            kind: PlayableKind::Movie,
            item: None,
            source_id: None,
            title: "Goldfinger".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: crate::redact::Secret::new("http://127.0.0.1/m.mkv".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        }));
        t.snapshot = Arc::new(s);
        let mut h = harness(t, one, PlayerScreen::default(), size);
        h.state_mut().enter(Route::Player);
        // the engine's first (empty) snapshot is out before ours goes in
        std::thread::sleep(Duration::from_millis(150));
        h.run_steps(2);
        (h, rec)
    }

    /// Moonraker was played; Thunderball plays in 9 s.
    fn countdown(h: &mut H) {
        let rows: Vec<QueueRow> = [(1, RowState::Played, "Moonraker"), (2, RowState::Pending, "Thunderball"), (3, RowState::Pending, "Dr. No")]
            .map(|(id, st, title)| {
                let mut e = crate::queue::ui::tests::entry(id, st);
                e.title = title.into();
                QueueRow { entry: e, locked: false, eta: None, note: None }
            })
            .to_vec();
        let s = QueueSnapshot {
            rows: rows.into(),
            next: Some(QueueId(2)),
            countdown: Some(Countdown { id: Some(QueueId(2)), at: Some(crate::model::now() + 9), why: CountdownWhy::Ended }),
            ..Default::default()
        };
        h.state().t.services().agent.publish_queue(Arc::new(s));
        h.run_steps(2);
    }

    fn card_shown(h: &H) -> bool {
        painted(h).iter().any(|t| t.starts_with("Up next in"))
    }

    /// Esc ladder: in controls focus mode the card's Esc cancels it (the engine answers with its
    /// own snapshot) before Esc leaves the controls.
    #[test]
    fn esc_cancels_the_card_before_it_leaves_the_controls() {
        let mut h = player(false, egui::vec2(1600.0, 900.0));
        assert!(h.state_mut().act(Action::FocusOsdControls));
        countdown(&mut h);
        assert!(card_shown(&h), "{:?}", painted(&h));
        assert!(has_label(&h, "Play now") && has_label(&h, "List") && has_label(&h, "Cancel"));
        assert!(h.state_mut().act(Action::LeaveOsdControls));
        until(&mut h, "the card cancelled", |h| !card_shown(h));
        assert!(h.state().s.key_contexts().contains(&KeyContext::PlayerOsd), "the controls keep the focus");
        assert!(h.state_mut().act(Action::LeaveOsdControls));
        assert!(!h.state().s.key_contexts().contains(&KeyContext::PlayerOsd));
    }

    /// Popped out: a strip with glyph buttons; Esc (the pop-in key) cancels it first. `u` opens
    /// the Up next panel with what is still to play; it filters like the other panels and `u`
    /// closes it.
    #[test]
    fn popped_out_the_strip_takes_esc_first_and_u_opens_the_panel() {
        let mut h = player(true, egui::vec2(640.0, 360.0));
        countdown(&mut h);
        assert!(card_shown(&h), "{:?}", painted(&h));
        assert!(has_label(&h, "Cancel"));
        h.state_mut().nav();
        assert!(h.state_mut().act(Action::TogglePopOut), "Esc goes to the card");
        until(&mut h, "the strip cancelled", |h| !card_shown(h));
        assert!(!h.state_mut().nav().contains(&NavRequest::TogglePopOut), "and doesn't pop in");
        countdown(&mut h);
        assert!(h.state_mut().act(Action::QueuePanel));
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts()[0], KeyContext::List);
        let text = painted(&h);
        assert!(text.iter().any(|t| t == "Thunderball") && !text.iter().any(|t| t == "Moonraker"), "{text:?}");
        assert!(h.state_mut().act(Action::FilterInput('o')));
        assert_eq!(h.state().s.key_contexts()[0], KeyContext::ListFiltering, "the panel's filter drives the keys");
        h.run_steps(2);
        let text = painted(&h);
        assert!(text.iter().any(|t| t == "Dr. No") && !text.iter().any(|t| t == "Moonraker" || t == "Thunderball"), "{text:?}");
        assert!(h.state_mut().act(Action::FilterClear));
        assert!(h.state_mut().act(Action::QueuePanel));
        assert!(h.state().s.panel.is_none());
    }

    /// The focused OSD control.
    fn focused(h: &H) -> Option<Control> {
        let s = &h.state().s;
        s.osd_focus.then(|| s.visible.get(s.control).copied()).flatten()
    }

    /// The OSD row has `Up next` (the panel) and, once Up next has a next item, `Next`, for
    /// the mouse and the controls focus alike: the button appearing doesn't move the focus,
    /// Enter on `Next` plays the next item and a click on `Up next` opens the panel.
    #[test]
    fn the_osd_has_up_next_and_next_buttons() {
        use crate::ui::item_menu::Target;
        use crate::ui::screens::home::testkit::{movies, seed, source, until};
        use egui_kittest::kittest::Queryable;
        let (mut t, one) = rig();
        let ms = seed(&mut t, |c| {
            let a = source(c, "TV")?;
            movies(c, a, 2)
        });
        let (mut h, rec) = player_on(t, one, false, egui::vec2(1600.0, 900.0));
        assert!(h.state_mut().act(Action::FocusOsdControls));
        h.run_steps(2);
        assert!(has_label(&h, "Up next") && !has_label(&h, "Next in Up next"), "Up next is empty: no Next");
        // the focus on Info, then Up next gets two films: Next comes in before Info
        while focused(&h) != Some(Control::Info) {
            assert!(h.state_mut().act(Action::Right));
            h.run_steps(1);
        }
        for m in &ms {
            assert!(h.state_mut().t.with_ctx(|ctx| crate::queue::ui::on_action(ctx, Action::QueueAdd, &Target::Movie(*m))));
        }
        until(&mut h, "the Next button", |h| has_label(h, "Next in Up next"));
        h.run_steps(1);
        assert_eq!(focused(&h), Some(Control::Info), "the focus stays on Info");
        assert!(h.state_mut().act(Action::Left));
        h.run_steps(1);
        assert_eq!(focused(&h), Some(Control::QueueNext));
        assert!(h.state_mut().act(Action::Activate));
        until(&mut h, "Up next plays its first film", |_| {
            rec.0.lock().unwrap().iter().any(|c| matches!(c, PlayerCommand::PlayCopy(c) if c.req.media == MediaRef::Movie(ms[0])))
        });
        assert!(h.state().s.panel.is_none());
        h.get_by_label("Up next").click();
        h.run_steps(2);
        assert_eq!(h.state().s.panel.as_ref().map(|p| p.kind), Some(PanelKind::Queue), "the button opens the panel");
    }
}
