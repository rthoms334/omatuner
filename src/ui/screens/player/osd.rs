//! The OSD: controls, bars, volume, badges (split from the player screen; owner WP-11).

use super::*;

/// OSD controls (controls focus mode moves between them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Prev,
    PlayPause,
    Next,
    SkipBack,
    SkipForward,
    Audio,
    Subtitles,
    Channels,
    Guide,
    History,
    Info,
    /// Up next's panel (`u`; DESIGN §32): with Up next on.
    UpNext,
    /// Up next's next item (`n`): while Up next has one and multi-view is off.
    QueueNext,
    Fullscreen,
    PopOut,
}

/// Which of Up next's OSD buttons the controls row has (DESIGN §32, up-next-design §2.5 Mouse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct QueueButtons {
    pub(super) panel: bool,
    pub(super) next: bool,
}

impl QueueButtons {
    /// `Up next` with Up next on; `Next` while it has a next item and multi-view is off (Up
    /// next waits in multi-view).
    pub(super) fn of(ctx: &UiCtx<'_>) -> Self {
        if !crate::queue::ENABLED {
            return Self::default();
        }
        Self { panel: true, next: !ctx.multiview.on() && ctx.services.agent.queue().next.is_some() }
    }
}

impl Control {
    pub(super) fn glyph(self, paused: bool) -> &'static str {
        match self {
            Control::Prev => glyphs::PREVIOUS,
            Control::PlayPause => {
                if paused {
                    glyphs::PLAY
                } else {
                    glyphs::PAUSE
                }
            }
            Control::Next => glyphs::NEXT,
            Control::SkipBack => glyphs::SKIP_BACK,
            Control::SkipForward => glyphs::SKIP_FORWARD,
            Control::Audio => glyphs::AUDIO_TRACK,
            Control::Subtitles => glyphs::SUBTITLES,
            Control::Channels => glyphs::PLAYLIST,
            Control::Guide => glyphs::GUIDE,
            Control::History => glyphs::HISTORY,
            Control::Info => glyphs::INFO,
            Control::UpNext => glyphs::UP_NEXT,
            Control::QueueNext => glyphs::QUEUE_NEXT,
            Control::Fullscreen => glyphs::FULLSCREEN,
            Control::PopOut => glyphs::POP_OUT,
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            Control::Prev => "Previous channel",
            Control::PlayPause => "Play / pause",
            Control::Next => "Next channel",
            Control::SkipBack => "Back",
            Control::SkipForward => "Forward",
            Control::Audio => "Audio",
            Control::Subtitles => "Subtitles",
            Control::Channels => "Channels",
            Control::Guide => "Guide",
            Control::History => "History",
            Control::Info => "Info",
            Control::UpNext => "Up next",
            Control::QueueNext => "Next in Up next",
            Control::Fullscreen => "Fullscreen",
            Control::PopOut => "Pop out",
        }
    }
    /// Drawn as a glyph only (DESIGN §13.8 wireframe); the label stays the accessible name and
    /// the hover text.
    pub(super) fn glyph_only(self) -> bool {
        matches!(
            self,
            Control::Prev | Control::PlayPause | Control::Next | Control::SkipBack | Control::SkipForward | Control::Fullscreen | Control::PopOut
        )
    }
    /// The hover text of a control that always has one (its key), whether labelled or not.
    pub(super) fn key_tip(self) -> Option<&'static str> {
        match self {
            Control::UpNext => Some("Up next (u)"),
            Control::QueueNext => Some("Play the next item in Up next (n)"),
            _ => None,
        }
    }
}

/// Which OSD row has the controls focus (VOD has a seek bar row above the buttons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum OsdRow {
    Seek,
    #[default]
    Buttons,
}

/// One OSD button (controls row, ←, volume, panel ✕, next-episode card, pop-out strip, the
/// failed stream's recovery buttons): OSD colours, hover fill, accent focus ring, pointing
/// hand. `label` is the accessible name; `tooltip` the hover text.
pub(super) fn osd_button(
    ui: &egui::Ui,
    p: &egui::Painter,
    theme: &Theme,
    rect: Rect,
    id: egui::Id,
    text: &str,
    font: FontId,
    label: &str,
    tooltip: Option<&str>,
    focused: bool,
) -> egui::Response {
    let t = &theme.tokens;
    let resp = ui.interact(rect, id, egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let resp = match tooltip {
        Some(tip) => resp.on_hover_text(tip),
        None => resp,
    };
    if resp.hovered() || focused {
        p.rect_filled(rect, t.rounding, with_alpha(t.osd_text, 0.12));
    }
    if focused {
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
    }
    let color = if focused { t.osd_accent } else { t.osd_text };
    let g = p.layout_no_wrap(text.to_owned(), font, color);
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp
}

/// The speaker glyph for a level (0–130 %).
pub(super) fn volume_glyph(muted: bool, level: f32) -> &'static str {
    match level {
        _ if muted => glyphs::MUTED,
        l if l < 34.0 => glyphs::VOLUME_LOW,
        l if l < 67.0 => glyphs::VOLUME_MEDIUM,
        _ => glyphs::VOLUME,
    }
}

/// Channel logo on the OSD: the image when loaded, else a monogram tile in OSD colours (the
/// theme's logo placeholder would be illegible on the dark scrim in light themes).
pub(super) fn osd_logo(p: &egui::Painter, theme: &Theme, images: &mut crate::images::ImageCache, url: Option<&str>, name: &str, r: Rect) {
    let t = &theme.tokens;
    if let Some(crate::images::ImageState::Ready(tex)) = url.filter(|u| !u.is_empty()).map(|u| images.get(u, crate::images::ImageKind::Logo)) {
        let sz = tex.size_vec2();
        let k = (r.width() / sz.x.max(1.0)).min(r.height() / sz.y.max(1.0));
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        p.image(tex.id(), Rect::from_center_size(r.center(), sz * k), uv, Color32::WHITE);
        return;
    }
    p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
    p.text(r.center(), Align2::CENTER_CENTER, logo::monogram(name), FontId::proportional((r.height() * 0.4).round().max(6.0)), t.osd_text);
}

/// "41:20" / "1:02:13".
pub(super) fn clock(seconds: f64) -> String {
    let s = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Control label with the live value (track names).
pub(super) fn control_text(c: Control, snap: &PlayerSnapshot) -> String {
    match c {
        Control::Audio => snap.tracks.selected_audio().map_or_else(|| "Audio".into(), |a| a.label()),
        Control::Subtitles => snap.tracks.selected_sub().map_or_else(|| "Off".into(), |a| a.label()),
        Control::QueueNext => "Next".into(),
        other => other.label().into(),
    }
}

impl PlayerScreen {
    /// The controls focus mode walks: those drawn last frame (a prefix of `controls`).
    pub(super) fn active_controls(&self, snap: &PlayerSnapshot, narrow: bool, video_only: bool, queue: QueueButtons) -> Vec<Control> {
        let all = Self::controls(snap, narrow, video_only, queue);
        let drawn: Vec<Control> = all.iter().copied().filter(|c| self.visible.contains(c)).collect();
        if drawn.is_empty() { all } else { drawn }
    }

    /// `controls`' index of the control focused last frame: a button that came or went since
    /// (Up next's `Next`) doesn't move the focus onto its neighbour.
    pub(super) fn focused_index(&self, controls: &[Control]) -> usize {
        self.visible
            .get(self.control)
            .and_then(|c| controls.iter().position(|x| x == c))
            .unwrap_or_else(|| self.control.min(controls.len().saturating_sub(1)))
    }

    /// Below 700 pt (× rem) the controls row collapses (DESIGN §13.8).
    pub(super) fn narrow(width: f32, theme: &Theme) -> bool {
        width < 700.0 * theme.rem()
    }

    /// The controls row; narrow keeps zapping (the only mouse path to it), play / tracks /
    /// info and Up next's `Next`. "Pop out" isn't offered in the popped-out window (its strip
    /// has the way back). Up next's buttons sit before Info (up-next-design §2.4).
    pub(super) fn controls(snap: &PlayerSnapshot, narrow: bool, video_only: bool, queue: QueueButtons) -> Vec<Control> {
        use Control as C;
        let live = is_live(snap);
        let up_next = [queue.panel.then_some(C::UpNext), queue.next.then_some(C::QueueNext)];
        if narrow {
            let mut v = if live { vec![C::Prev, C::PlayPause, C::Next] } else { vec![C::PlayPause] };
            v.extend([C::Audio, C::Subtitles]);
            v.extend(queue.next.then_some(C::QueueNext));
            v.push(C::Info);
            return v;
        }
        let mut v = if live {
            vec![C::Prev, C::PlayPause, C::Next, C::Audio, C::Subtitles, C::Channels, C::Guide, C::History]
        } else {
            vec![C::SkipBack, C::PlayPause, C::SkipForward, C::Audio, C::Subtitles]
        };
        v.extend(up_next.into_iter().flatten());
        v.extend([C::Info, C::Fullscreen]);
        if !video_only {
            v.push(C::PopOut);
        }
        v
    }

    /// The volume to show: what the user just set until mpv reports it, else mpv's.
    pub(super) fn shown_volume(&mut self, snap: &PlayerSnapshot) -> f32 {
        let mpv = snap.volume as f32;
        match self.volume_local {
            Some((v, at)) if (v - mpv).abs() >= 0.5 && at.elapsed() < VOLUME_ECHO => v,
            _ => {
                self.volume_local = None;
                mpv
            }
        }
    }

    /// Set the volume from the slider / wheel; a muted player is unmuted first (once).
    pub(super) fn set_volume(&mut self, ctx: &mut UiCtx<'_>, v: f32) {
        if ctx.player.muted && self.unmuting.is_none_or(|t| t.elapsed() >= VOLUME_ECHO) {
            ctx.play(PlayerCommand::ToggleMute);
            self.unmuting = Some(Instant::now());
        }
        ctx.play(PlayerCommand::SetVolume(f64::from(v)));
        self.volume_local = Some((v, Instant::now()));
        // the pop-out strip's slider has no level readout next to it
        if ctx.video_only {
            self.flash = Some((Flash::Volume, Instant::now()));
        }
        self.show_banner(ctx);
    }

    /// Speaker button, and with `slider` the volume slider and its level label (`label`: the
    /// label's rect). The wheel over either steps the volume.
    pub(super) fn draw_volume(
        &mut self,
        ui: &egui::Ui,
        ctx: &mut UiCtx<'_>,
        p: &egui::Painter,
        speaker: Rect,
        slider: Option<(Rect, Option<Rect>)>,
        font: FontId,
        snap: &PlayerSnapshot,
    ) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if !snap.muted {
            self.unmuting = None;
        }
        let level = self.shown_volume(snap);
        let muted = snap.muted;
        let tip = match (slider.is_some(), muted) {
            (true, false) => "Mute (m)".to_owned(),
            (true, true) => "Unmute (m)".to_owned(),
            (false, false) => format!("Volume {level:.0} % · wheel to change · click to mute (m)"),
            (false, true) => "Muted · click to unmute (m) · wheel to change".to_owned(),
        };
        let label = if muted { "Unmute" } else { "Mute" };
        let resp = osd_button(ui, p, theme, speaker, ui.id().with("osd-speaker"), volume_glyph(muted, level), font.clone(), label, Some(&tip), false);
        if resp.clicked() {
            ctx.play(PlayerCommand::ToggleMute);
            if slider.is_none() {
                self.flash = Some((Flash::Volume, Instant::now()));
            }
            self.show_banner(ctx);
        }
        if resp.hovered() {
            let notches = slider::wheel_notches(ui, resp.id);
            if notches != 0.0 {
                self.set_volume(ctx, (level + notches * VOLUME_WHEEL).clamp(0.0, VOLUME_MAX));
                if slider.is_none() {
                    self.flash = Some((Flash::Volume, Instant::now()));
                }
            }
        }
        let Some((sr, label_rect)) = slider else { return };
        let mut look = SliderLook::osd(t);
        if muted {
            look.fill = with_alpha(t.osd_text, 0.4);
            look.knob = look.fill;
        }
        let spec = SliderSpec { range: 0.0..=VOLUME_MAX, step: 1.0, marks: &[100.0], wheel_step: Some(VOLUME_WHEEL), label: "Volume", look };
        let mut v = level;
        let r = slider::slider_in(ui, sr, ui.id().with("osd-volume"), theme, &mut v, false, &spec);
        if r.changed() {
            self.set_volume(ctx, v);
        } else if muted && (r.clicked() || r.drag_started()) && self.unmuting.is_none() {
            // a click on the knob itself sets nothing new, but still means "unmute"
            ctx.play(PlayerCommand::ToggleMute);
            self.unmuting = Some(Instant::now());
            self.show_banner(ctx);
        }
        if let Some(lr) = label_rect {
            let text = if muted { "Muted".to_owned() } else { format!("{v:.0} %") };
            osd_text(p, lr.left_center(), Align2::LEFT_CENTER, &text, font, t.osd_text, f32::INFINITY);
        }
    }

    pub(super) fn activate_control(&mut self, ctx: &mut UiCtx<'_>, c: Control) {
        let cfg = &ctx.config.player;
        match c {
            Control::Prev => self.zap(ctx, false),
            Control::Next => self.zap(ctx, true),
            Control::PlayPause => ctx.play(PlayerCommand::TogglePause),
            Control::SkipBack => ctx.play(PlayerCommand::SeekBy(-f64::from(cfg.skip_small_s))),
            Control::SkipForward => ctx.play(PlayerCommand::SeekBy(f64::from(cfg.skip_small_s))),
            Control::Audio => ctx.open(OverlayRequest::Tracks { audio: true }),
            Control::Subtitles => ctx.open(OverlayRequest::Tracks { audio: false }),
            Control::Channels => self.open_panel(ctx, PanelKind::Channels),
            Control::Guide => self.open_panel(ctx, PanelKind::MiniGuide),
            Control::History => self.open_panel(ctx, PanelKind::History),
            Control::Info => self.toggle_info(ctx),
            // the keys' own paths: a card showing takes them first, `n` flashes when it can't act
            Control::UpNext => {
                self.queue_action(ctx, Action::QueuePanel);
            }
            Control::QueueNext => {
                self.queue_action(ctx, Action::QueueNext);
            }
            Control::Fullscreen => self.toggle_fullscreen = true,
            Control::PopOut => ctx.navigate(NavRequest::TogglePopOut),
        }
    }

    /// Controls focus mode (`PlayerOsd`). Back (Alt+←, the mouse back button) leaves it like
    /// Esc; only what it handles shows the banner (an unhandled Back used to show it and then
    /// hide it again, forever).
    pub(super) fn osd_action(&mut self, ctx: &mut UiCtx<'_>, action: Action, narrow: bool) -> Option<bool> {
        if !self.osd_focus {
            return None;
        }
        let snap = ctx.player.clone();
        let controls = self.active_controls(&snap, narrow, ctx.video_only, QueueButtons::of(ctx));
        let n = controls.len();
        self.control = self.focused_index(&controls);
        // the index below is into this list from now on
        self.visible.clone_from(&controls);
        let seekable = !is_live(&snap) && snap.seekable;
        match action {
            Action::LeaveOsdControls | Action::Back => self.osd_focus = false,
            Action::Left | Action::Right if self.row == OsdRow::Seek && seekable => {
                let s = f64::from(ctx.config.player.skip_small_s);
                ctx.play(PlayerCommand::SeekBy(if action == Action::Left { -s } else { s }));
            }
            Action::Left | Action::FocusPrev => self.control = (self.control + n - 1) % n,
            Action::Right | Action::FocusNext => self.control = (self.control + 1) % n,
            Action::Up if seekable => self.row = OsdRow::Seek,
            Action::Down => self.row = OsdRow::Buttons,
            Action::Up => {}
            Action::Activate => {
                if self.row == OsdRow::Seek {
                    ctx.play(PlayerCommand::TogglePause);
                } else if let Some(c) = controls.get(self.control.min(n - 1)) {
                    self.activate_control(ctx, *c);
                }
            }
            _ => return None,
        }
        self.show_banner(ctx);
        Some(true)
    }

    pub(super) fn draw_osd(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let narrow = Self::narrow(rect.width(), theme);
        let pad = (theme.space(SpaceToken::PanelPadding) * s).round();
        let title_font = FontId::proportional((theme.font(TypeToken::Display) * s).round());
        let body_font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small_font = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let badge_font = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let p = ui.painter_at(rect);
        let playable = snap.playable.clone();
        let h24 = ctx.config.interface.clock_24h;
        let live = is_live(snap);

        // ---- top bar: ← · logo · number · title ……… clock (· volume when the bottom bar has
        // no level readout)
        let top_h = title_font.size + 2.0 * pad;
        let top = Rect::from_min_size(rect.min, egui::vec2(rect.width(), top_h));
        p.rect_filled(top, 0.0, t.osd_scrim);
        self.bars = vec![top];
        let back_side = top_h - pad;
        let back = Rect::from_min_size(egui::pos2(top.left() + pad / 2.0, top.center().y - back_side / 2.0), egui::vec2(back_side, back_side));
        let back_tip = if ctx.video_only {
            "Back to the full window · keeps playing in the mini player (p)"
        } else {
            "Back · keeps playing in the mini player (p)"
        };
        if osd_button(ui, &p, theme, back, ui.id().with("osd-back"), glyphs::ARROW_LEFT, body_font.clone(), "Leave the player", Some(back_tip), false)
            .clicked()
        {
            Self::leave(ctx);
        }
        let mut x = back.right() + pad / 2.0;
        let logo_size = (title_font.size * 1.2).round();
        if let Some(pl) = &playable {
            let lr = Rect::from_min_size(egui::pos2(x, top.center().y - logo_size / 2.0), egui::vec2(logo_size, logo_size));
            osd_logo(&p, theme, ctx.images, pl.logo.as_deref(), &pl.title, lr);
            x = lr.right() + pad / 2.0;
            if let Some(n) = pl.number {
                let r = osd_text(
                    &p,
                    egui::pos2(x, top.center().y),
                    Align2::LEFT_CENTER,
                    &n.to_string(),
                    body_font.clone(),
                    with_alpha(t.osd_text, 0.7),
                    f32::INFINITY,
                );
                x = r.right() + pad / 2.0;
            }
        }
        // narrow: the bottom bar has only the speaker, so the level stays up here
        let clock_right = if narrow {
            let vol = if snap.muted { format!("{} muted", glyphs::MUTED) } else { format!("{} {:.0} %", glyphs::VOLUME, snap.volume) };
            osd_text(&p, egui::pos2(top.right() - pad, top.center().y), Align2::RIGHT_CENTER, &vol, small_font.clone(), t.osd_text, f32::INFINITY)
                .left()
                - pad
        } else {
            top.right() - pad
        };
        let cr = osd_text(
            &p,
            egui::pos2(clock_right, top.center().y),
            Align2::RIGHT_CENTER,
            &hhmm(ctx.now, h24),
            small_font.clone(),
            t.osd_text,
            f32::INFINITY,
        );
        let ch = playable.as_ref().and_then(|p| match p.media {
            MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(id).cloned(),
            _ => None,
        });
        let title = playable.as_ref().map(|p| p.title.as_str()).unwrap_or("");
        // a zap shows which of the SD / HD / 4K duplicates this is before the picture does;
        // once it plays, the decoded badge on the right says it
        let decoded = snap.state == PlayerState::Playing && snap.video_size.is_some();
        let quality = ch.as_ref().filter(|_| live && !decoded).and_then(|c| ctx.quality(c));
        let badge = quality.map(|q| InlineBadge::quality(&p, theme, q, t.osd_text, s));
        // which playlist, left of the clock, whenever two or more have this kind (DESIGN §29)
        let mut right = cr.left() - pad;
        if let Some(src) =
            playable.as_ref().and_then(|pl| Some((playable_item_kind(&pl.kind)?, pl.source_id?))).and_then(|(k, id)| ctx.source_of(k, id))
        {
            let color = theme.source_color(src.color);
            let name_color = badge::legible_on(color, t.osd_scrim.to_opaque(), t.osd_text);
            let r = osd_text(
                &p,
                egui::pos2(right, top.center().y),
                Align2::RIGHT_CENTER,
                &src.name,
                small_font.clone(),
                name_color,
                (rect.width() * 0.25).round(),
            );
            let d = osd_text(
                &p,
                egui::pos2(r.left() - pad / 3.0, top.center().y),
                Align2::RIGHT_CENTER,
                glyphs::DOT,
                small_font.clone(),
                color,
                f32::INFINITY,
            );
            right = d.left() - pad;
        }
        let max_w = (right - x).max(0.0);
        name_with_quality(&p, theme, egui::pos2(x, top.center().y), title, title_font.clone(), t.osd_text, badge, max_w);

        // ---- bottom bar
        let nn = ch.as_ref().filter(|_| live).and_then(|c| live::now_next_of(ctx, c));
        let controls = Self::controls(snap, narrow, ctx.video_only, QueueButtons::of(ctx));
        let ctrl_h = (theme.space(SpaceToken::ControlHeight) * s * 1.2).round();
        let line = body_font.size * 1.4;
        let lines = 3.0;
        let bottom_h = pad * 2.0 + line * lines + ctrl_h + pad / 2.0;
        let bottom = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - bottom_h), rect.max);
        p.rect_filled(bottom, 0.0, t.osd_scrim);
        self.bottom_bar = Some(bottom_h);
        self.bars.push(bottom);
        let inner = bottom.shrink2(egui::vec2(pad, pad));
        let mut y = inner.top();
        if live {
            match nn.as_ref().and_then(|n| n.now.clone()) {
                Some(now) => {
                    let tr = osd_text(
                        &p,
                        egui::pos2(inner.right(), y),
                        Align2::RIGHT_TOP,
                        &time_range(now.start, now.stop, h24),
                        small_font.clone(),
                        with_alpha(t.osd_text, 0.8),
                        f32::INFINITY,
                    );
                    osd_text(
                        &p,
                        egui::pos2(inner.left(), y),
                        Align2::LEFT_TOP,
                        &now.title,
                        body_font.clone(),
                        t.osd_text,
                        (tr.left() - pad - inner.left()).max(0.0),
                    );
                    y += line;
                    let left = osd_text(
                        &p,
                        egui::pos2(inner.right(), y + line / 2.0),
                        Align2::RIGHT_CENTER,
                        &time_left(now.stop - ctx.now),
                        small_font.clone(),
                        with_alpha(t.osd_text, 0.8),
                        f32::INFINITY,
                    );
                    let bar = Rect::from_min_max(
                        egui::pos2(inner.left(), y + line / 2.0 - 2.0 * s),
                        egui::pos2(left.left() - pad, y + line / 2.0 + 2.0 * s),
                    );
                    p.rect_filled(bar, t.rounding, with_alpha(t.osd_text, 0.25));
                    let fill = Rect::from_min_max(bar.min, egui::pos2(bar.left() + bar.width() * now.progress(ctx.now), bar.bottom()));
                    p.rect_filled(fill, t.rounding, t.osd_accent);
                }
                None => {
                    let msg = if ch.as_ref().is_some_and(|c| c.epg_channel_id.is_none()) {
                        self.guide.loading_label(ctx).unwrap_or_else(|| "No guide data".to_owned())
                    } else {
                        "No information".to_owned()
                    };
                    osd_text(&p, egui::pos2(inner.left(), y), Align2::LEFT_TOP, &msg, body_font.clone(), with_alpha(t.osd_text, 0.7), inner.width());
                    y += line;
                }
            }
            y += line;
            let next = nn.as_ref().and_then(|n| n.next.clone()).map(|n| format!("Next  {}  {}", hhmm(n.start, h24), n.title)).unwrap_or_default();
            let br = self.badges(
                &p,
                theme,
                snap,
                ch.as_ref().is_some_and(|c| c.has_catchup),
                egui::pos2(inner.right(), y + line / 2.0),
                badge_font.clone(),
                s,
            );
            osd_text(
                &p,
                egui::pos2(inner.left(), y + line / 2.0),
                Align2::LEFT_CENTER,
                &next,
                small_font.clone(),
                with_alpha(t.osd_text, 0.8),
                (br.left() - pad - inner.left()).max(0.0),
            );
            y += line;
        } else {
            let sub = playable.as_ref().and_then(|p| p.subtitle.clone()).unwrap_or_default();
            let kind = playable.as_ref().map(|p| p.kind.clone());
            let head = match kind {
                Some(PlayableKind::Catchup { start, stop }) => format!("{} · {}", glyphs::CATCHUP, time_range(start, stop, h24)),
                Some(PlayableKind::Episode { season, episode }) => format!("S{season:02}E{episode:02}"),
                _ => String::new(),
            };
            let line1 = [head, sub].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            osd_text(&p, egui::pos2(inner.left(), y), Align2::LEFT_TOP, &line1, body_font.clone(), t.osd_text, inner.width());
            y += line;
            let pos = snap.position.unwrap_or(0.0);
            let dur = snap.duration.or_else(|| playable.as_ref().and_then(|p| p.duration_hint)).unwrap_or(0.0);
            let times = format!("{} / {}", clock(pos), clock(dur));
            let tr =
                osd_text(&p, egui::pos2(inner.right(), y + line / 2.0), Align2::RIGHT_CENTER, &times, small_font.clone(), t.osd_text, f32::INFINITY);
            let seek_rect = Rect::from_min_max(egui::pos2(inner.left(), y), egui::pos2(tr.left() - pad, y + line));
            let focused = self.osd_focus && self.row == OsdRow::Seek;
            let cache_end = snap.cache_secs.map(|c| pos + c);
            let req =
                ui.scope_builder(egui::UiBuilder::new().max_rect(seek_rect), |ui| slider::seek_bar(ui, theme, pos, dur, cache_end, focused)).inner;
            if let Some(to) = req {
                ctx.play(PlayerCommand::SeekTo(to));
                self.show_banner(ctx);
            }
            y += line;
            let br = self.badges(&p, theme, snap, false, egui::pos2(inner.right(), y + line / 2.0), badge_font.clone(), s);
            let hint = format!("−{} / +{} s", ctx.config.player.skip_small_s, ctx.config.player.skip_small_s);
            osd_text(
                &p,
                egui::pos2(inner.left(), y + line / 2.0),
                Align2::LEFT_CENTER,
                &hint,
                small_font.clone(),
                with_alpha(t.osd_text, 0.7),
                (br.left() - pad - inner.left()).max(0.0),
            );
            y += line;
        }

        // volume at the right end of the controls row: its room is reserved first, so the
        // controls give up space before it does (narrow: the speaker alone)
        let cy = y + pad / 2.0;
        let speaker_w = [glyphs::VOLUME, glyphs::MUTED]
            .iter()
            .map(|g| p.layout_no_wrap((*g).to_owned(), small_font.clone(), t.osd_text).size().x)
            .fold(0.0, f32::max)
            + pad;
        let speaker_x = |right: f32| Rect::from_min_size(egui::pos2(right - speaker_w, cy), egui::vec2(speaker_w, ctrl_h));
        let controls_right = if narrow {
            let speaker = speaker_x(inner.right());
            self.draw_volume(ui, ctx, &p, speaker, None, small_font.clone(), snap);
            speaker.left() - pad
        } else {
            let label_w =
                ["130 %", "Muted"].iter().map(|s| p.layout_no_wrap((*s).to_owned(), small_font.clone(), t.osd_text).size().x).fold(0.0, f32::max);
            let slider_w = (if rect.width() >= 1200.0 * theme.rem() { 112.0 } else { 80.0 }) * s;
            let label = Rect::from_min_size(egui::pos2(inner.right() - label_w, cy), egui::vec2(label_w, ctrl_h));
            let slider = Rect::from_min_size(egui::pos2(label.left() - pad / 2.0 - slider_w, cy), egui::vec2(slider_w, ctrl_h));
            let speaker = speaker_x(slider.left() - pad / 4.0);
            self.draw_volume(ui, ctx, &p, speaker, Some((slider, Some(label))), small_font.clone(), snap);
            speaker.left() - pad
        };

        // controls row: labelled where the design labels them; all glyphs when that doesn't
        // fit; whatever still doesn't fit isn't drawn (and focus mode skips it)
        let mut cx = inner.left();
        let mut clicked = None;
        let text_of = |c: Control, glyphs_only: bool| {
            if glyphs_only || c.glyph_only() {
                c.glyph(snap.paused).to_owned()
            } else {
                format!("{}  {}", c.glyph(snap.paused), control_text(c, snap))
            }
        };
        let row_width = |glyphs_only: bool| -> f32 {
            controls.iter().map(|c| p.layout_no_wrap(text_of(*c, glyphs_only), small_font.clone(), t.osd_text).size().x + pad * 1.5).sum()
        };
        let glyphs_only = narrow || row_width(false) > controls_right - inner.left();
        self.control = self.focused_index(&controls);
        let mut drawn = Vec::with_capacity(controls.len());
        for (i, c) in controls.iter().enumerate() {
            let text = text_of(*c, glyphs_only);
            let w = p.layout_no_wrap(text.clone(), small_font.clone(), t.osd_text).size().x;
            let r = Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(w + pad, ctrl_h));
            if r.right() > controls_right {
                break;
            }
            drawn.push(*c);
            let focused = self.osd_focus && self.row == OsdRow::Buttons && i == self.control.min(controls.len() - 1);
            let tip = match c.key_tip() {
                Some(k) => Some(k.to_owned()),
                None => (glyphs_only || c.glyph_only()).then(|| control_text(*c, snap)),
            };
            let resp = osd_button(ui, &p, theme, r, ui.id().with(("osd-control", i)), &text, small_font.clone(), c.label(), tip.as_deref(), focused);
            if resp.clicked() {
                clicked = Some((i, *c));
            }
            cx = r.right() + pad / 2.0;
        }
        self.control = self.control.min(drawn.len().saturating_sub(1));
        self.visible = drawn;
        if let Some((i, c)) = clicked {
            // a click activates the control but doesn't enter controls focus mode (DESIGN
            // §13.8): the arrows keep zapping / seeking afterwards
            self.row = OsdRow::Buttons;
            self.control = i;
            self.show_banner(ctx);
            self.activate_control(ctx, c);
        }
    }

    /// The video-only window's banner: one bottom strip — title and volume, then what's on
    /// (live) or the position (VOD), and the way back when there is room. With the pointer in
    /// the window (or paused) the right side holds buttons instead of the volume and the hint:
    /// previous / play / next (live) or skips (VOD), speaker (+ slider from 360 px), pop in,
    /// stop.
    pub(super) fn draw_compact_osd(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let pad = theme.space(SpaceToken::Sm);
        let title_font = FontId::proportional(theme.font(TypeToken::Body));
        let small_font = FontId::proportional(theme.font(TypeToken::Caption));
        let line = title_font.size * 1.35;
        let strip = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 2.0 * pad - line - small_font.size * 1.35), rect.max);
        let p = ui.painter_at(rect);
        p.rect_filled(strip, 0.0, t.osd_scrim);
        self.bottom_bar = Some(strip.height());
        self.bars = vec![strip];
        let inner = strip.shrink(pad);
        let buttons = snap.paused || ui.ctx().pointer_hover_pos().is_some_and(|pos| rect.contains(pos));
        let text_right = if buttons {
            self.draw_strip_buttons(ui, ctx, &p, inner, title_font.clone(), snap) - pad
        } else {
            let vol = if snap.muted { format!("{} muted", glyphs::MUTED) } else { format!("{} {:.0} %", glyphs::VOLUME, snap.volume) };
            osd_text(&p, inner.right_top(), Align2::RIGHT_TOP, &vol, small_font.clone(), t.osd_text, f32::INFINITY).left() - pad
        };
        let playable = snap.playable.as_ref();
        let mut title = playable.map(|p| p.title.clone()).unwrap_or_default();
        if snap.paused {
            title = format!("{} {title}", glyphs::PAUSE);
        }
        osd_text(&p, inner.left_top(), Align2::LEFT_TOP, &title, title_font, t.osd_text, (text_right - inner.left()).max(0.0));
        let second = if is_live(snap) {
            let ch = playable.and_then(|p| match p.media {
                MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(id).cloned(),
                _ => None,
            });
            ch.and_then(|c| live::now_next_of(ctx, &c)).and_then(|n| n.now).map(|now| now.title.to_string()).unwrap_or_default()
        } else {
            let dur = snap.duration.or_else(|| playable.and_then(|p| p.duration_hint)).unwrap_or(0.0);
            format!("{} / {}", clock(snap.position.unwrap_or(0.0)), clock(dur))
        };
        let second_pos = egui::pos2(inner.left(), inner.top() + line);
        let mut second_w = inner.width();
        if buttons {
            second_w = (text_right - inner.left()).max(0.0);
        } else if strip.width() >= 360.0 {
            let hint = osd_text(
                &p,
                egui::pos2(inner.right(), second_pos.y),
                Align2::RIGHT_TOP,
                "o · Esc  back",
                small_font.clone(),
                with_alpha(t.osd_text, 0.5),
                f32::INFINITY,
            );
            second_w = (hint.left() - pad - inner.left()).max(0.0);
        }
        osd_text(&p, second_pos, Align2::LEFT_TOP, &second, small_font, with_alpha(t.osd_text, 0.8), second_w);
    }

    /// The pop-out strip's buttons, right-aligned in `inner` (the strip's two text lines high);
    /// returns their left edge. The text keeps room: the slider only from 560 pt, previous /
    /// next only from 420 pt (the wheel over the speaker still steps the volume).
    pub(super) fn draw_strip_buttons(
        &mut self,
        ui: &egui::Ui,
        ctx: &mut UiCtx<'_>,
        p: &egui::Painter,
        inner: Rect,
        font: FontId,
        snap: &PlayerSnapshot,
    ) -> f32 {
        #[derive(Clone, Copy)]
        enum B {
            Prev,
            PlayPause,
            Next,
            PopIn,
            Stop,
        }
        let theme = ctx.theme;
        let live = is_live(snap);
        let side = inner.height();
        let gap = theme.space(SpaceToken::Xxs);
        let square = |right: f32| Rect::from_min_size(egui::pos2(right - side, inner.top()), egui::vec2(side, side));
        let rem = theme.rem();
        let wide = inner.width() >= 560.0 * rem;
        let skips = inner.width() >= 420.0 * rem;
        // right to left: stop, pop in, volume, next, play, previous
        let stop = square(inner.right());
        let pop_in = square(stop.left() - gap);
        let slider_w = 64.0 * rem;
        let slider = wide.then(|| Rect::from_min_size(egui::pos2(pop_in.left() - gap - slider_w, inner.top()), egui::vec2(slider_w, side)));
        let speaker = square(slider.map_or(pop_in.left(), |s| s.left()) - gap);
        let next = square(speaker.left() - gap);
        let play = if skips { square(next.left() - gap) } else { next };
        let prev = square(play.left() - gap);
        self.draw_volume(ui, ctx, p, speaker, slider.map(|s| (s, None)), font.clone(), snap);
        let (prev_label, next_label) =
            if live { ("Previous channel", "Next channel") } else { (Control::SkipBack.label(), Control::SkipForward.label()) };
        let (prev_glyph, next_glyph) = if live { (glyphs::PREVIOUS, glyphs::NEXT) } else { (glyphs::SKIP_BACK, glyphs::SKIP_FORWARD) };
        let buttons = [
            (B::Prev, prev, prev_glyph, prev_label, prev_label),
            (B::PlayPause, play, if snap.paused { glyphs::PLAY } else { glyphs::PAUSE }, "Play / pause", "Play / pause (Space)"),
            (B::Next, next, next_glyph, next_label, next_label),
            (B::PopIn, pop_in, glyphs::POP_IN, "Back to the full window", "Back to the full window (o · Esc · double-click)"),
            (B::Stop, stop, glyphs::CLOSE, "Stop and close", "Stop and close (q)"),
        ];
        let mut clicked = None;
        for (i, (b, r, glyph, label, tip)) in buttons.into_iter().enumerate() {
            if !skips && matches!(b, B::Prev | B::Next) {
                continue;
            }
            if osd_button(ui, p, theme, r, ui.id().with(("strip-button", i)), glyph, font.clone(), label, Some(tip), false).clicked() {
                clicked = Some(b);
            }
        }
        let skip = f64::from(ctx.config.player.skip_small_s);
        match clicked {
            Some(B::Prev) if live => self.zap(ctx, false),
            Some(B::Next) if live => self.zap(ctx, true),
            Some(B::Prev) => ctx.play(PlayerCommand::SeekBy(-skip)),
            Some(B::Next) => ctx.play(PlayerCommand::SeekBy(skip)),
            Some(B::PlayPause) => ctx.play(PlayerCommand::TogglePause),
            Some(B::PopIn) => ctx.navigate(NavRequest::TogglePopOut),
            Some(B::Stop) => Self::stop_and_leave(ctx),
            None => {}
        }
        if clicked.is_some() {
            self.show_banner(ctx);
        }
        if skips { prev.left() } else { play.left() }
    }

    /// `[HD] [5.1] [CC] [⟲]` right-aligned at `right_center`; returns their extent.
    pub(super) fn badges(
        &self,
        p: &egui::Painter,
        theme: &Theme,
        snap: &PlayerSnapshot,
        catchup: bool,
        right_center: egui::Pos2,
        font: FontId,
        s: f32,
    ) -> Rect {
        let t = &theme.tokens;
        let mut labels: Vec<String> = Vec::new();
        if let Some(q) = snap.video_size.and_then(|(w, h)| Quality::of_video(w, h)) {
            labels.push(q.label().into());
        }
        if snap.stream_info.as_ref().is_some_and(|i| i.is_hdr()) {
            labels.push("HDR".into());
        }
        if let Some(chs) = snap.tracks.selected_audio().and_then(|a| a.channels) {
            labels.push(match chs {
                1 => "Mono".into(),
                2 => "Stereo".into(),
                6 => "5.1".into(),
                8 => "7.1".into(),
                n => format!("{n} ch"),
            });
        }
        if !snap.tracks.subs.is_empty() {
            labels.push("CC".into());
        }
        if catchup {
            labels.push(glyphs::CATCHUP.into());
        }
        let mut right = right_center.x;
        let mut extent = Rect::from_center_size(right_center, egui::Vec2::ZERO);
        let gap = theme.space(SpaceToken::Sm) * s;
        for l in labels.iter().rev() {
            let g = p.layout_no_wrap(l.clone(), font.clone(), t.osd_text);
            let r = Rect::from_min_size(
                egui::pos2(right - g.size().x - 2.0 * gap, right_center.y - g.size().y / 2.0 - gap / 2.0),
                g.size() + egui::vec2(2.0 * gap, gap),
            );
            p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, with_alpha(t.osd_text, 0.6)), egui::StrokeKind::Inside);
            p.galley(r.min + egui::vec2(gap, gap / 2.0), g, t.osd_text);
            extent = extent.union(r);
            right = r.left() - gap;
        }
        extent
    }
}
