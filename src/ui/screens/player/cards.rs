//! Cards over the video: end of playback (next episode), the error card's other copy, the
//! idle card. OWNER (batch): Q-ui — the Up next card replaces the next-episode card here
//! (DESIGN §32).

use super::*;

pub(super) struct NextEpisode {
    pub(super) id: EpisodeId,
    pub(super) label: String,
    pub(super) at: Instant,
}

pub(super) const NEXT_EPISODE_DELAY: Duration = Duration::from_secs(10);

impl PlayerScreen {
    /// Up next's player keys (`u` panel, `n` / `N`; DESIGN §32): `None` when not one of them
    /// or Up next is off. Q-ui.
    pub(super) fn queue_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if !crate::queue::ENABLED {
            return None;
        }
        matches!(action, Action::QueuePanel | Action::QueueNext | Action::QueuePrev).then_some(false)
    }

    pub(super) fn end_of_playback(&mut self, ctx: &mut UiCtx<'_>) {
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
    }
}
