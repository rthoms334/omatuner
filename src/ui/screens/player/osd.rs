//! The OSD: controls, bars, volume, badges (split from the player screen; owner WP-11).

use std::sync::Arc;

use super::*;

/// OSD controls (controls focus mode moves between them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Prev,
    PlayPause,
    Next,
    SkipBack,
    SkipForward,
    BackToLive,
    AudioSubtitles,
    Delay,
    Settings,
    Channels,
    Guide,
    History,
    Info,
    /// Up next's panel (`u`; DESIGN §32): with Up next on.
    UpNext,
    /// Up next's next item (`n`): while Up next has one and multi-view is off.
    QueueNext,
    /// Cast to a TV (`C`; DESIGN §40 §3.1): before Fullscreen in the wide layout; while casting
    /// it shows the connected glyph and the TV's name.
    Cast,
    Fullscreen,
    PopOut,
}

/// Which optional controls the OSD row and cog offer: Up next's (DESIGN §32, up-next-design
/// §2.5 Mouse) and Cast (DESIGN §40 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct OptionalButtons {
    pub(super) panel: bool,
    pub(super) next: bool,
    pub(super) cast: bool,
}

impl OptionalButtons {
    /// `Up next` with Up next on; `Next` while it has a next item and multi-view is off (Up
    /// next waits in multi-view). `Cast` with casting on (`cast::on`: built in and not switched
    /// off in Settings), or while a cast runs (its way off); not in multi-view, which can't cast.
    pub(super) fn of(ctx: &UiCtx<'_>) -> Self {
        let cast = (crate::cast::on(ctx.config) || ctx.player.cast.is_some()) && !ctx.multiview.on();
        if !crate::queue::ENABLED {
            return Self { cast, ..Self::default() };
        }
        Self { panel: true, next: !ctx.multiview.on() && ctx.services.agent.queue().next.is_some(), cast }
    }
}

impl Control {
    pub(super) fn glyph(self, snap: &PlayerSnapshot) -> &'static str {
        match self {
            Control::Prev => glyphs::PREVIOUS,
            Control::PlayPause => {
                if snap.paused {
                    glyphs::PLAY
                } else {
                    glyphs::PAUSE
                }
            }
            Control::Next => glyphs::NEXT,
            Control::SkipBack => glyphs::SKIP_BACK,
            Control::SkipForward => glyphs::SKIP_FORWARD,
            Control::BackToLive => glyphs::NEXT,
            Control::AudioSubtitles => glyphs::AUDIO_TRACK,
            Control::Delay | Control::Settings => glyphs::SETTINGS,
            Control::Channels => glyphs::PLAYLIST,
            Control::Guide => glyphs::GUIDE,
            Control::History => glyphs::HISTORY,
            Control::Info => glyphs::INFO,
            Control::UpNext => glyphs::UP_NEXT,
            Control::QueueNext => glyphs::QUEUE_NEXT,
            Control::Cast if snap.cast.is_some() => glyphs::CAST_CONNECTED,
            Control::Cast => glyphs::CAST,
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
            Control::BackToLive => "Back to live",
            Control::AudioSubtitles => "Audio & subtitles",
            Control::Delay => "Delay",
            Control::Settings => "Player settings",
            Control::Channels => "Channels",
            Control::Guide => "Guide",
            Control::History => "History",
            Control::Info => "Info",
            Control::UpNext => "Up next",
            Control::QueueNext => "Next in Up next",
            Control::Cast => "Cast to a TV",
            Control::Fullscreen => "Fullscreen",
            Control::PopOut => "Pop out",
        }
    }
    /// Drawn as a glyph only (DESIGN §13.8 wireframe); the label stays the accessible name and
    /// the hover text.
    pub(super) fn glyph_only(self) -> bool {
        matches!(
            self,
            Control::Prev
                | Control::PlayPause
                | Control::Next
                | Control::SkipBack
                | Control::SkipForward
                | Control::Fullscreen
                | Control::PopOut
                | Control::AudioSubtitles
                | Control::Settings
        )
    }
    /// The hover text of a control that always has one (its key), whether labelled or not.
    pub(super) fn key_tip(self) -> Option<&'static str> {
        match self {
            Control::Settings => Some("Player settings (O)"),
            Control::AudioSubtitles => Some("Audio & subtitles (A / S)"),
            Control::Fullscreen => Some("Fullscreen (f)"),
            Control::UpNext => Some("Up next (u)"),
            Control::QueueNext => Some("Play the next item in Up next (n)"),
            Control::Cast => Some("Cast to a TV (C)"),
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

/// Soft glyph shadow on the picture: a few faint copies, with no button background.
pub(super) fn shadowed_galley(p: &egui::Painter, centre: egui::Pos2, galley: Arc<egui::Galley>, color: Color32, s: f32) {
    let pos = centre - galley.mesh_bounds.center().to_vec2();
    let shadow = Color32::from_black_alpha((38.0 * f32::from(color.a()) / 255.0).round() as u8);
    for offset in [egui::vec2(-1.0, 1.0), egui::vec2(1.0, 1.0), egui::vec2(0.0, -0.5), egui::vec2(0.0, 2.0)] {
        p.galley_with_override_text_color(pos + offset * s, galley.clone(), shadow);
    }
    p.galley_with_override_text_color(pos, galley, color);
}

/// Size the ink, not the font's line box: Nerd glyphs have different side bearings/heights.
fn centre_glyph(p: &egui::Painter, text: &str, height: f32, color: Color32) -> Arc<egui::Galley> {
    let galley = p.layout_no_wrap(text.to_owned(), FontId::proportional(height), color);
    let size = height * height / galley.mesh_bounds.height().max(1.0);
    p.layout_no_wrap(text.to_owned(), FontId::proportional(size), color)
}

/// The three picture controls keep large targets independently of their smaller glyphs.
pub(super) struct CentreTransport {
    pub(super) centre: egui::Pos2,
    pub(super) skip: f32,
    hit: f32,
    spacing: f32,
}

impl CentreTransport {
    pub(super) fn new(rect: Rect, top: f32, bottom: f32, s: f32) -> Self {
        let hit = (80.0 * s).min(rect.width() / 5.0).min((bottom - top).max(36.0) * 0.65);
        let skip = hit * 0.5;
        let centre = egui::pos2(rect.center().x, rect.center().y.clamp(top + hit / 2.0, (bottom - hit / 2.0).max(top + hit / 2.0)));
        // About 1.9 skip widths, with enough room that the invisible hit targets never overlap.
        Self { centre, skip, hit, spacing: (skip * 1.9).max(hit + 4.0 * hit / 80.0) }
    }

    pub(super) fn at(&self, c: Control) -> egui::Pos2 {
        let offset = match c {
            Control::SkipBack => -self.spacing,
            Control::SkipForward => self.spacing,
            _ => 0.0,
        };
        self.centre + egui::vec2(offset, 0.0)
    }

    fn rect(&self, c: Control) -> Rect {
        Rect::from_center_size(self.at(c), egui::vec2(self.hit, self.hit))
    }
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

/// Control label with the live value (the connected TV).
pub(super) fn control_text(c: Control, snap: &PlayerSnapshot) -> String {
    match c {
        Control::QueueNext => "Next".into(),
        // the TV's name while casting, else what the button does
        Control::Cast => snap.cast.as_ref().map_or_else(|| "Cast".into(), |c| c.name.clone()),
        other => other.label().into(),
    }
}

impl PlayerScreen {
    /// The controls focus mode walks those drawn last frame.
    pub(super) fn active_controls(&self, snap: &PlayerSnapshot, narrow: bool, video_only: bool, queue: OptionalButtons) -> Vec<Control> {
        let all = Self::focus_controls(snap, narrow, video_only, queue);
        let drawn: Vec<Control> = all.iter().copied().filter(|c| self.visible.contains(c)).collect();
        if drawn.is_empty() { all } else { drawn }
    }

    /// Picture controls first, then the bottom row from left to right. In the pop-out the
    /// transport is already at the start of the row.
    fn focus_controls(snap: &PlayerSnapshot, narrow: bool, video_only: bool, queue: OptionalButtons) -> Vec<Control> {
        let mut controls = if video_only { Vec::new() } else { Self::transport_controls(snap) };
        controls.extend(Self::controls(snap, narrow, video_only, queue));
        controls
    }

    /// `controls`' index of the control focused last frame: a button that came or went since
    /// (Up next's `Next`) doesn't move the focus onto its neighbour.
    pub(super) fn focused_index(&self, controls: &[Control]) -> usize {
        self.visible
            .get(self.control)
            .and_then(|c| controls.iter().position(|x| x == c))
            .unwrap_or_else(|| controls.iter().position(|c| *c == Control::PlayPause).unwrap_or(0))
    }

    /// Below 700 pt (× rem) the controls row collapses (DESIGN §13.8).
    pub(super) fn narrow(width: f32, theme: &Theme) -> bool {
        width < 700.0 * theme.rem()
    }

    fn transport_controls(snap: &PlayerSnapshot) -> Vec<Control> {
        use Control as C;
        let mut v = Vec::new();
        if snap.seekable {
            v.push(C::SkipBack);
        }
        v.push(C::PlayPause);
        if snap.seekable {
            v.push(C::SkipForward);
        }
        v
    }

    /// The bottom row's essentials (DESIGN §13.8), with volume before the right group.
    /// Only the popped-out window keeps transport here; the full window has it in the picture.
    pub(super) fn controls(snap: &PlayerSnapshot, narrow: bool, video_only: bool, queue: OptionalButtons) -> Vec<Control> {
        use Control as C;
        let mut v = if video_only { Self::transport_controls(snap) } else { Vec::new() };
        if is_live(snap) {
            if snap.live_buffer.is_some_and(|r| r.behind(snap.position.unwrap_or(r.end))) {
                v.push(C::BackToLive);
            }
            v.extend([C::Prev, C::Next]);
        }
        v.extend(queue.next.then_some(C::QueueNext));
        if snap.cast.is_none() {
            v.push(C::AudioSubtitles);
        }
        if !narrow {
            v.extend(queue.cast.then_some(C::Cast));
            v.push(C::Fullscreen);
        }
        v.push(C::Settings);
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
        // AirPlay has no volume to set from here: one "TV remote" button (DESIGN §40 §3.3)
        if tv_volume(snap) {
            let full = slider.map_or(speaker, |(s, label)| speaker.union(s).union(label.unwrap_or(s)));
            let text = if slider.is_some() { format!("{}  TV remote", glyphs::VOLUME) } else { glyphs::VOLUME.to_owned() };
            let id = ui.id().with("osd-speaker");
            if osd_button(ui, p, theme, full, id, &text, font, "Volume: use the TV remote", Some(TV_VOLUME), false).clicked() {
                self.cast_notice(ctx, TV_VOLUME);
            }
            return;
        }
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
            Control::BackToLive => ctx.play(PlayerCommand::JumpToLive),
            Control::AudioSubtitles => self.open_settings(ctx, SettingsPage::Tracks),
            Control::Delay => self.open_settings(ctx, SettingsPage::Delay),
            Control::Settings => self.open_settings(ctx, SettingsPage::Main),
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
            Control::Cast => self.cast_menu(ctx),
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
        let controls = self.active_controls(&snap, narrow, ctx.video_only, OptionalButtons::of(ctx));
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
            let vol = if tv_volume(snap) {
                format!("{} TV remote", glyphs::VOLUME)
            } else if snap.muted {
                format!("{} muted", glyphs::MUTED)
            } else {
                format!("{} {:.0} %", glyphs::VOLUME, snap.volume)
            };
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
        // the spoiler guard: what the banner may say of the programme, and Ctrl+B's hint
        let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
        let shown = self.banner_programmes_with(ctx, nn.as_ref());
        let spoiler_hint = spoiler_hint(&shown, &spoilers);
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
                        &self.banner_cats.shown(ctx, &now).line(&spoilers),
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
            let next = nn
                .as_ref()
                .and_then(|n| n.next.clone())
                .map(|n| format!("Next  {}  {}", hhmm(n.start, h24), self.banner_cats.shown(ctx, &n).title(&spoilers)))
                .unwrap_or_default();
            let br = self.badges(
                &p,
                theme,
                snap,
                ch.as_ref().is_some_and(|c| c.has_catchup),
                egui::pos2(inner.right(), y + line / 2.0),
                badge_font.clone(),
                s,
            );
            // "Ctrl+B show spoilers" left of the badges while the banner hides something
            let mut next_right = br.left() - pad;
            if let Some(h) = &spoiler_hint {
                let hr = osd_text(
                    &p,
                    egui::pos2(next_right, y + line / 2.0),
                    Align2::RIGHT_CENTER,
                    h,
                    small_font.clone(),
                    with_alpha(t.osd_text, 0.7),
                    f32::INFINITY,
                );
                next_right = hr.left() - pad;
            }
            osd_text(
                &p,
                egui::pos2(inner.left(), y + line / 2.0),
                Align2::LEFT_CENTER,
                &next,
                small_font.clone(),
                with_alpha(t.osd_text, 0.8),
                (next_right - inner.left()).max(0.0),
            );
            y += line;
        } else {
            let kind = playable.as_ref().map(|p| p.kind.clone());
            // a catch-up's sub-title is its programme's title: the guard's, as the guide's
            let sub = match (&kind, shown.first()) {
                (Some(PlayableKind::Catchup { .. }), Some(c)) => c.title(&spoilers),
                _ => playable.as_ref().and_then(|p| p.subtitle.clone()).unwrap_or_default(),
            };
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
            let mut hint = format!("−{} / +{} s", ctx.config.player.skip_small_s, ctx.config.player.skip_small_s);
            if let Some(h) = &spoiler_hint {
                hint = format!("{hint}  ·  {h}");
            }
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

        let row = Rect::from_min_max(egui::pos2(inner.left(), y + pad / 2.0), inner.max);
        self.draw_controls(ui, ctx, row, small_font, ctrl_h, snap);
        if !ctx.video_only {
            self.draw_centre_controls(ui, ctx, rect, top.bottom(), bottom.top(), snap);
        }
    }

    /// Picture transport starts the focus walk; the pop-out keeps transport in its row.
    fn draw_centre_controls(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, top: f32, bottom: f32, snap: &PlayerSnapshot) {
        let t = &ctx.theme.tokens;
        let layout = CentreTransport::new(rect, top, bottom, scale_of(ui));
        let p = ui.painter_at(rect);
        for c in [Control::SkipBack, Control::PlayPause, Control::SkipForward] {
            let Some(i) = self.visible.iter().position(|control| *control == c) else { continue };
            let r = layout.rect(c);
            let label = match c {
                Control::SkipBack => "Skip back",
                Control::SkipForward => "Skip forward",
                _ => "Centre play / pause",
            };
            let resp = ui.interact(r, ui.id().with(("centre-control", i)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
            let resp = resp.on_hover_text(c.label());
            let focused = self.settings.is_none() && ((self.osd_focus && self.row == OsdRow::Buttons && i == self.control) || resp.has_focus());
            let color = with_alpha(t.osd_text, if resp.hovered() || focused { 1.0 } else { 0.88 });
            let pressed = if resp.is_pointer_button_down_on() { 0.92 } else { 1.0 };
            let size = layout.skip * if c == Control::PlayPause { 1.4 } else { 1.0 } * pressed;
            let glyph = match c {
                Control::SkipBack => glyphs::SKIP_BACK_CIRCULAR,
                Control::SkipForward => glyphs::SKIP_FORWARD_CIRCULAR,
                _ if snap.paused => glyphs::PLAY_OUTLINE,
                _ => glyphs::PAUSE,
            };
            if focused {
                p.circle_stroke(r.center(), layout.hit * 0.48, egui::Stroke::new(1.25, with_alpha(t.osd_accent, 0.6)));
            }
            shadowed_galley(&p, r.center(), centre_glyph(&p, glyph, size, color), color, layout.skip / 40.0);
            if c != Control::PlayPause {
                let text = ctx.config.player.skip_small_s.to_string();
                // Medium once `fonts::install` has bound it; egui panics on an unknown family
                // (the first frames and headless harnesses have none), so Regular until then.
                let medium = egui::FontFamily::Name(crate::theme::egui_style::FAMILY_MEDIUM.into());
                let bound = ui.ctx().fonts(|f| f.definitions().families.contains_key(&medium));
                let font = FontId::new(size * 0.32, if bound { medium } else { egui::FontFamily::Proportional });
                let mut number = p.layout_no_wrap(text.clone(), font.clone(), color);
                let fit = (size * 0.46 / number.mesh_bounds.width().max(1.0)).min(1.0);
                if fit < 1.0 {
                    number = p.layout_no_wrap(text, FontId::new(font.size * fit, font.family), color);
                }
                // The arrowhead projects beyond the ring: centre the number within the ring.
                let offset = if c == Control::SkipBack { 0.06 } else { -0.06 };
                shadowed_galley(&p, r.center() + egui::vec2(size * offset, 0.0), number, color, layout.skip / 40.0);
            }
            self.bars.push(r);
            if resp.clicked() {
                self.row = OsdRow::Buttons;
                self.control = i;
                self.show_banner(ctx);
                self.activate_control(ctx, c);
            }
        }
    }

    /// The row keeps its right-aligned cog. Collapse labels before shrinking the spacing,
    /// so even a small live-rewind pop-out with transport keeps every essential.
    fn draw_controls(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, font: FontId, height: f32, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let narrow = Self::narrow(ui.available_rect_before_wrap().width(), theme);
        self.controls_narrow = narrow;
        let optional = OptionalButtons::of(ctx);
        let controls = Self::controls(snap, narrow, ctx.video_only, optional);
        let focus_controls = Self::focus_controls(snap, narrow, ctx.video_only, optional);
        let focus_offset = focus_controls.len() - controls.len();
        let split = controls
            .iter()
            .position(|c| matches!(c, Control::QueueNext | Control::AudioSubtitles | Control::Cast | Control::Fullscreen | Control::Settings))
            .unwrap();
        let p = ui.painter_at(rect);
        let pad = (theme.space(SpaceToken::PanelPadding) * s).round();
        let mut gap = pad / 2.0;
        let mut button_pad = pad;
        let text_of = |c: Control, only: bool| {
            if only || c.glyph_only() { c.glyph(snap).to_owned() } else { format!("{}  {}", c.glyph(snap), control_text(c, snap)) }
        };
        let width_of = |text: String| p.layout_no_wrap(text, font.clone(), t.osd_text).size().x;
        let slider_w = if narrow { 0.0 } else { (if rect.width() >= 1200.0 * theme.rem() { 112.0 } else { 80.0 }) * s };
        let label_w = if narrow { 0.0 } else { width_of("130 %".into()).max(width_of("Muted".into())) };
        let speaker_w = width_of(glyphs::VOLUME.into()).max(width_of(glyphs::MUTED.into()));
        let volume_w = |gap: f32, padding: f32| speaker_w + padding + slider_w + label_w + if narrow { 0.0 } else { gap * 2.0 };
        let total = |only: bool| controls.iter().map(|c| width_of(text_of(*c, only)) + button_pad + gap).sum::<f32>() + volume_w(gap, button_pad);
        let only = narrow || total(false) > rect.width();
        let fixed = controls.iter().map(|c| width_of(text_of(*c, only))).sum::<f32>() + speaker_w + slider_w + label_w;
        let spacing = button_pad * (controls.len() + 1) as f32 + gap * (controls.len() + if narrow { 0 } else { 2 }) as f32;
        let fit = ((rect.width() - fixed) / spacing).clamp(0.0, 1.0);
        gap *= fit;
        button_pad *= fit;
        let right_w = controls[split..].iter().map(|c| width_of(text_of(*c, only)) + button_pad + gap).sum::<f32>() - gap;
        let right_x = rect.right() - right_w;
        let mut x = rect.left();
        let mut clicked = None;
        self.control = self.focused_index(&focus_controls);
        for (row_index, c) in controls.iter().copied().enumerate() {
            let i = row_index + focus_offset;
            if row_index == split {
                let speaker = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(speaker_w + button_pad, height));
                let slider = (!narrow).then(|| {
                    let slider = Rect::from_min_size(egui::pos2(speaker.right() + gap, rect.top()), egui::vec2(slider_w, height));
                    let label = Rect::from_min_size(egui::pos2(slider.right() + gap, rect.top()), egui::vec2(label_w, height));
                    (slider, Some(label))
                });
                self.draw_volume(ui, ctx, &p, speaker, slider, font.clone(), snap);
                x = right_x;
            }
            let text = text_of(c, only);
            let r = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(width_of(text.clone()) + button_pad, height));
            if c == Control::Settings {
                self.settings_anchor = Some(r);
            }
            let focused = self.settings.is_none() && self.osd_focus && self.row == OsdRow::Buttons && i == self.control;
            let tip = c.key_tip().unwrap_or(c.label());
            if osd_button(ui, &p, theme, r, ui.id().with(("osd-control", i)), &text, font.clone(), c.label(), Some(tip), focused).clicked() {
                clicked = Some((i, c));
            }
            x = r.right() + gap;
        }
        self.visible = focus_controls;
        if let Some((i, c)) = clicked {
            self.row = OsdRow::Buttons;
            self.control = i;
            self.show_banner(ctx);
            self.activate_control(ctx, c);
        }
    }

    /// The popped-out window's compact banner: title / return, programme / position, then
    /// the essential controls including transport (DESIGN §13.8).
    pub(super) fn draw_compact_osd(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let pad = theme.space(SpaceToken::Sm);
        let font = FontId::proportional(theme.font(TypeToken::Body));
        let small = FontId::proportional(theme.font(TypeToken::Caption));
        let line = font.size * 1.4;
        let ctrl_h = theme.space(SpaceToken::ControlHeight);
        let strip = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 2.0 * pad - line * 2.0 - ctrl_h), rect.max);
        let p = ui.painter_at(rect);
        p.rect_filled(strip, 0.0, t.osd_scrim);
        self.bottom_bar = Some(strip.height());
        self.bars = vec![strip];
        let inner = strip.shrink(pad);
        let back = Rect::from_min_size(egui::pos2(inner.right() - ctrl_h, inner.top()), egui::vec2(ctrl_h, line));
        if osd_button(
            ui,
            &p,
            theme,
            back,
            ui.id().with("strip-back"),
            glyphs::POP_IN,
            font.clone(),
            "Back to the full window",
            Some("Back to the full window (o · Esc · double-click)"),
            false,
        )
        .clicked()
        {
            ctx.navigate(NavRequest::TogglePopOut);
        }
        let playable = snap.playable.as_ref();
        let title = playable.map(|p| p.title.clone()).unwrap_or_default();
        osd_text(&p, inner.left_top(), Align2::LEFT_TOP, &title, font.clone(), t.osd_text, (back.left() - pad - inner.left()).max(0.0));
        let second = if is_live(snap) {
            let ch = playing_channel(snap).and_then(|id| ctx.lib.live.get(id)).cloned();
            let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
            let programme = ch
                .and_then(|c| live::now_next_of(ctx, &c))
                .and_then(|n| n.now)
                .map(|now| self.banner_cats.shown(ctx, &now).title(&spoilers))
                .unwrap_or_default();
            format!("{} · {programme}", live_label(snap))
        } else {
            let dur = snap.duration.or_else(|| playable.and_then(|p| p.duration_hint)).unwrap_or(0.0);
            format!("{} / {}", clock(snap.position.unwrap_or(0.0)), clock(dur))
        };
        osd_text(&p, inner.left_top() + egui::vec2(0.0, line), Align2::LEFT_TOP, &second, small, with_alpha(t.osd_text, 0.8), inner.width());
        let row = Rect::from_min_max(inner.left_top() + egui::vec2(0.0, line * 2.0), inner.max);
        self.draw_controls(ui, ctx, row, font, ctrl_h, snap);
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
        if snap.playable.as_ref().and_then(|p| p.item.as_ref()).is_some_and(|i| crate::ui::downloads::recording(p.ctx(), i)) {
            labels.push("⏺ Recording".into());
        }
        if is_live(snap) {
            labels.push(live_label(snap));
        }
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
