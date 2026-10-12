//! The single video "screen" shown in different places (FIXED DECISION 3): Live preview pane,
//! fullscreen player, corner mini-player. Owner: WP-11 (render path implemented in the
//! skeleton from the spike).
//!
//! Per frame: screens call [`VideoHost::paint`] for the rect where video belongs (at most one
//! placement per frame; the last call wins). If no screen placed the video while something is
//! playing and the screen allows it, the app draws the mini-player (bottom-right, 16:9, width
//! 320 pt Wide / 240 pt Medium, hidden in Compact — there the status bar shows a now-playing
//! chip instead; click = open player, ✕ = stop). Toasts stack ABOVE the mini-player rect
//! ([`VideoHost::mini_rect`]); screens get it as `UiCtx::mini_rect`.
//! Without a video surface (libmpv/GL failed) every placement shows "Video unavailable —
//! libmpv failed to start (see Logs)"; browsing still works.
//! `App::logic` then calls [`VideoHost::render`] next frame with the requested physical size
//! (one-frame resize lag, invisible in practice; spike-verified resize path).
//! Overlays inside the video rect use the FIXED dark scrim (never theme colours): Loading
//! (channel logo + name + spinner), Buffering (percent), Reconnecting ("attempt n", or n/max
//! while the attempts are bounded, + countdown, and a Stop button: a channel that played keeps
//! reconnecting until stopped), Error (glyph + message + "r Retry · Backspace Last channel"),
//! Radio (logo + now playing instead of black).
//!
//! The overlay's logo is the channel logo through the image cache (screens pass it to
//! [`VideoHost::paint`]; the last texture is kept for the mini-player, which the app draws
//! without one), with the monogram tile as the fallback. The mini-player is drawn as a card
//! (theme colours for its title strip, OSD colours inside the video) and is NOT recorded as a
//! placement: the app shows it only when no screen placed the video in the previous frame, so
//! recording it would make it blink every other frame.
//!
//! Multi-view (DESIGN §35, MV-2): one surface per picture. Picture B's is made after startup
//! ([`VideoHost::make_surface`], GL current, from the `SurfaceFactory` saved at startup); until
//! then its place reads "Starting…". [`VideoHost::paint`] paints the surface of the snapshot's
//! picture (screens pass the focused one), [`VideoHost::paint_tile`] one multi-view picture
//! (only the focused one lays out recovery buttons; the other's overlay says how to choose
//! it). [`VideoHost::render`] renders each picture painted in the last frame — when none was,
//! the focused one into the sink — so a picture nobody shows stops its render clock and its
//! player suspends its video after 10 s ([`VideoHost::was_painted`] tells the app when to
//! resume it). The mini player shows picture in picture ([`VideoHost::mini_player_with`]).

use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Rect};
use eframe::glow;

use crate::images::{ImageCache, ImageKind, ImageState};
use crate::model::PlayableKind;
use crate::model::TileId;
use crate::player::render::SurfaceFactory;
use crate::player::{PlayerErrorKind, PlayerSnapshot, PlayerState, VideoSurface};
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::LayoutClass;
use crate::ui::widgets::{glyphs, logo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    None,
    Preview,
    Fullscreen,
    Mini,
}

/// What a click on the corner mini player asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniAction {
    /// The video: open the fullscreen player.
    Open,
    /// Pop the player out (small window on every workspace).
    PopOut,
    /// ✕: stop playback.
    Stop,
}

/// A recovery action offered as a button under a failed / reconnecting stream in the
/// fullscreen player (the keys `r` / Backspace do the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    Retry,
    LastChannel,
    /// "Play from TV 2 at 41:20": the title's other copy after a failure (Ctrl+O; DESIGN §29.14).
    OtherCopy,
    /// Stop the reconnects and close the player (`q`).
    Stop,
}

/// Where the player draws a [`StateAction`] button this frame (laid out by the state overlay,
/// drawn and handled by the player with its OSD buttons).
#[derive(Debug, Clone)]
pub struct StateButton {
    pub action: StateAction,
    pub rect: Rect,
    pub text: String,
    pub font: FontId,
}

/// One picture's video: its surface and what painting it needs.
#[derive(Default)]
struct TileVideo {
    surface: Option<VideoSurface>,
    last_generation: u64,
    /// Logo of what plays (url, texture), resolved through the image cache by `paint`.
    logo: Option<(String, egui::TextureHandle)>,
    /// Painted this frame (`painted`) / in the previous one (`painted_before`).
    painted: bool,
    painted_before: bool,
}

#[derive(Default)]
pub struct VideoHost {
    /// Picture A (made at startup) and picture B (made once multi-view started it).
    tiles: [TileVideo; 2],
    /// Makes picture B's surface after startup (GL current; `None` without a GL backend).
    factory: Option<SurfaceFactory>,
    placed: Placement,
    placed_this_frame: Placement,
    mini: Option<egui::Rect>,
    /// `mini_player` ran since the last `begin_frame` (a rect nobody redraws is dropped).
    mini_drawn: bool,
    /// Recovery buttons of the last fullscreen `paint` of the focused picture (empty unless
    /// failed / reconnecting).
    buttons: Vec<StateButton>,
    /// The label of the other copy a failed title can play from ([`VideoHost::set_other_copy`]).
    other_copy: Option<String>,
}

fn slot(tile: TileId) -> usize {
    usize::from(tile.0).min(1)
}

impl VideoHost {
    pub fn new(surface: Option<VideoSurface>) -> Self {
        let mut h = Self::default();
        h.tiles[0].surface = surface;
        h
    }

    /// What makes the other pictures' surfaces later (saved from the creation context).
    pub fn set_factory(&mut self, factory: Option<SurfaceFactory>) {
        self.factory = factory;
    }

    /// Make `tile`'s surface for its player (`Player::render_core`); GL must be current. Without
    /// a factory (no GL backend: tests) the picture shows "Video unavailable".
    pub fn make_surface(&mut self, tile: TileId, core: crate::player::RenderCore) -> Result<(), String> {
        let Some(f) = &self.factory else { return Err("no GL backend".into()) };
        let s = f.make(core).map_err(|e| e.to_string())?;
        self.tiles[slot(tile)].surface = Some(s);
        Ok(())
    }

    /// `tile`'s surface exists (B: once made).
    pub fn has_surface(&self, tile: TileId) -> bool {
        self.tiles[slot(tile)].surface.is_some()
    }

    /// `tile` was painted in the last frame (the app resumes a suspended picture only then).
    pub fn was_painted(&self, tile: TileId) -> bool {
        let t = &self.tiles[slot(tile)];
        t.painted || t.painted_before
    }

    /// Start of frame bookkeeping (app).
    pub fn begin_frame(&mut self) {
        self.placed = self.placed_this_frame;
        self.placed_this_frame = Placement::None;
        for t in &mut self.tiles {
            t.painted_before = std::mem::take(&mut t.painted);
        }
        // the mini-player wasn't drawn last frame (a screen placed the video, playback
        // stopped, Compact…): toasts and the guide must stop avoiding its old rect
        if !self.mini_drawn {
            self.mini = None;
        }
        self.mini_drawn = false;
    }

    /// Where the video was shown last frame.
    pub fn placement(&self) -> Placement {
        self.placed
    }

    /// False when libmpv/GL failed at startup (no surface).
    pub fn available(&self) -> bool {
        self.tiles[0].surface.is_some()
    }

    /// The mini-player rect drawn this frame or the previous one (toasts and the guide avoid
    /// it); `None` once a frame passed without the mini-player.
    pub fn mini_rect(&self) -> Option<egui::Rect> {
        self.mini
    }

    /// Render mpv into the FBOs (from `App::logic`, GL current). Clears to black on new loads.
    /// Each picture painted in the last frame renders; when none was, only the focused picture
    /// renders, into the sink (frames must be consumed). A picture that isn't rendered stops
    /// its render clock, and its player suspends its video after 10 s (DESIGN §35 §4.6).
    pub fn render(&mut self, gl: &glow::Context, frame: &mut eframe::Frame, snaps: [Option<&PlayerSnapshot>; 2], focus: TileId) {
        let any = self.tiles.iter().any(|t| t.painted);
        for (i, t) in self.tiles.iter_mut().enumerate() {
            let (Some(s), Some(snap)) = (&mut t.surface, snaps[i]) else { continue };
            if !(t.painted || (!any && i == slot(focus))) {
                continue;
            }
            if snap.load_generation != t.last_generation {
                t.last_generation = snap.load_generation;
                s.reset_for_new_file();
            }
            s.render(gl, frame);
        }
    }

    /// Paint the video into `rect` (mpv draws black bars), plus the state overlay (with the
    /// channel logo from `images`). Returns the response over the whole rect (click /
    /// double-click handling by the caller). A fullscreen failed / reconnecting
    /// stream also lays out its recovery buttons ([`VideoHost::state_buttons`]).
    pub fn paint(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        placement: Placement,
        theme: &Theme,
        images: &mut ImageCache,
        snap: &PlayerSnapshot,
    ) -> egui::Response {
        self.update_logo(images, snap);
        self.paint_video(ui, rect, placement, theme, snap, true)
    }

    /// [`paint`](Self::paint) of one multi-view picture: `focused` = keys act on it (only its
    /// recovery buttons are laid out; the others' overlays say how to get to them).
    #[allow(clippy::too_many_arguments)]
    pub fn paint_tile(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        placement: Placement,
        theme: &Theme,
        images: &mut ImageCache,
        snap: &PlayerSnapshot,
        focused: bool,
    ) -> egui::Response {
        self.update_logo(images, snap);
        self.paint_video(ui, rect, placement, theme, snap, focused)
    }

    /// Resolve the logo of what plays (only requested while a video rect is painted).
    fn update_logo(&mut self, images: &mut ImageCache, snap: &PlayerSnapshot) {
        let t = &mut self.tiles[slot(snap.tile)];
        let Some(url) = snap.playable.as_ref().and_then(|p| p.logo.as_deref()).map(str::trim).filter(|u| !u.is_empty()) else {
            t.logo = None;
            return;
        };
        match images.get(url, ImageKind::Logo) {
            ImageState::Ready(tex) => t.logo = Some((url.to_owned(), tex)),
            // keep a texture of the same url while it reloads; a new channel starts without
            ImageState::Loading => t.logo = t.logo.take().filter(|(u, _)| u == url),
            ImageState::Failed => t.logo = None,
        }
    }

    /// The logo texture for what plays now (`None` → monogram).
    fn logo_for(&self, snap: &PlayerSnapshot) -> Option<&egui::TextureHandle> {
        let url = snap.playable.as_ref().and_then(|p| p.logo.as_deref()).map(str::trim)?;
        self.tiles[slot(snap.tile)].logo.as_ref().filter(|(u, _)| u == url).map(|(_, t)| t)
    }

    /// Offer another copy of the failed title on the error card ("Play from TV 2 at 41:20");
    /// the fullscreen player sets it before every `paint` (`None`: nothing to offer).
    pub fn set_other_copy(&mut self, label: Option<String>) {
        self.other_copy = label;
    }

    /// The recovery buttons laid out by the last fullscreen [`paint`](Self::paint).
    pub fn state_buttons(&self) -> &[StateButton] {
        &self.buttons
    }

    fn paint_video(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        placement: Placement,
        theme: &Theme,
        snap: &PlayerSnapshot,
        focused: bool,
    ) -> egui::Response {
        self.placed_this_frame = placement;
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
        let ppp = ui.ctx().pixels_per_point();
        // The FBO covers the whole rect: mpv letterboxes itself (keepaspect), so aspect
        // override / panscan / zoom (`v`, `V`) work without app-side geometry.
        let target = rect;
        let starting = snap.tile != TileId::A && self.tiles[slot(snap.tile)].surface.is_none() && self.tiles[0].surface.is_some();
        let tile = &mut self.tiles[slot(snap.tile)];
        tile.painted = true;
        if starting {
            // picture B's player is up; its surface comes with the next GL frame
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Starting…",
                egui::FontId::proportional(theme.font(crate::theme::TypeToken::Body)),
                theme.tokens.osd_text,
            );
        } else if let Some(s) = &mut tile.surface {
            s.request_size((target.width() * ppp).round() as i32, (target.height() * ppp).round() as i32);
            if let (Some((tex, _)), true) = (s.texture(), s.has_frame()) {
                painter.image(tex, target, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Video unavailable — libmpv failed to start (see Logs)",
                egui::FontId::proportional(theme.font(crate::theme::TypeToken::Body)),
                theme.tokens.osd_text,
            );
        }
        let buttons = self.paint_state_overlay(ui, rect, placement, theme, snap, focused);
        if focused {
            self.buttons = buttons;
        }
        resp
    }

    /// Loading / buffering / reconnecting / error overlay (fixed dark scrim). WP-11. Returns
    /// the recovery buttons to draw under a failed / reconnecting stream (fullscreen only).
    fn paint_state_overlay(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        placement: Placement,
        theme: &Theme,
        snap: &PlayerSnapshot,
        focused: bool,
    ) -> Vec<StateButton> {
        let t = &theme.tokens;
        let p = ui.painter_at(rect);
        let k = overlay_scale(rect);
        let title_font = FontId::proportional((theme.font(TypeToken::Title) * k).round());
        let body_font = FontId::proportional((theme.font(TypeToken::Body) * k).round());
        let small_font = FontId::proportional((theme.font(TypeToken::BodySmall) * k).round());
        let gap = theme.space(SpaceToken::Md) * k;
        let title = snap.playable.as_ref().map(|p| p.title.as_str()).unwrap_or("");
        let live = snap.playable.as_ref().is_none_or(|p| p.kind.is_live());
        let time = ui.input(|i| i.time);
        // recovery buttons centred under the text (the fullscreen player handles them)
        let buttons = |bottom: f32, reconnecting: bool| -> Vec<StateButton> {
            // keys act on the focused picture; inside one narrower than 360 pt only the hint
            // says what to press
            if placement != Placement::Fullscreen || !focused || rect.width() < 360.0 {
                return Vec::new();
            }
            let font = FontId::proportional((theme.font(TypeToken::Body) * k).round());
            let retry = if reconnecting { "Retry now" } else { "Retry" };
            let mut list = Vec::new();
            // after the reconnects gave up, the title's other copy comes first
            if let Some(label) = self.other_copy.as_ref().filter(|_| !reconnecting) {
                list.push((StateAction::OtherCopy, format!("{}  {label}", glyphs::PLAY)));
            }
            list.push((StateAction::Retry, format!("{}  {retry}", glyphs::REFRESH)));
            if live && snap.previous_live.is_some() {
                list.push((StateAction::LastChannel, format!("{}  Last channel", glyphs::HISTORY)));
            }
            if reconnecting {
                list.push((StateAction::Stop, format!("{}  Stop", glyphs::STOP)));
            }
            let pad = egui::vec2(gap, gap / 2.0);
            let sizes: Vec<egui::Vec2> = list.iter().map(|(_, s)| p.layout_no_wrap(s.clone(), font.clone(), t.osd_text).size() + 2.0 * pad).collect();
            let total = sizes.iter().map(|s| s.x).sum::<f32>() + gap * (sizes.len() as f32 - 1.0);
            let mut x = rect.center().x - total / 2.0;
            list.into_iter()
                .zip(sizes)
                .map(|((action, text), size)| {
                    let r = Rect::from_min_size(egui::pos2(x, bottom + gap), size);
                    x = r.right() + gap;
                    StateButton { action, rect: r, text, font: font.clone() }
                })
                .collect()
        };
        match &snap.state {
            PlayerState::Idle => {}
            PlayerState::Zapping { .. } | PlayerState::Loading { .. } => {
                p.rect_filled(rect, 0.0, t.osd_scrim);
                let detail = match &snap.state {
                    PlayerState::Loading { attempt, .. } if *attempt > 0 => format!("Connecting… (attempt {})", attempt + 1),
                    _ => "Connecting…".to_owned(),
                };
                let tile = 56.0 * k;
                let block_h = tile + gap + title_font.size + gap + body_font.size + gap + 18.0 * k;
                let mut y = rect.center().y - block_h / 2.0;
                logo_tile(
                    &p,
                    theme,
                    Rect::from_center_size(egui::pos2(rect.center().x, y + tile / 2.0), egui::vec2(tile, tile)),
                    title,
                    self.logo_for(snap),
                );
                y += tile + gap;
                osd_text(&p, egui::pos2(rect.center().x, y), Align2::CENTER_TOP, title, title_font.clone(), t.osd_text, rect.width() - 2.0 * gap);
                y += title_font.size + gap;
                osd_text(
                    &p,
                    egui::pos2(rect.center().x, y),
                    Align2::CENTER_TOP,
                    &detail,
                    body_font,
                    with_alpha(t.osd_text, 0.7),
                    rect.width() - 2.0 * gap,
                );
                y += body_font_size(theme, k) + gap;
                spinner(&p, egui::pos2(rect.center().x, y + 9.0 * k), 9.0 * k, t.osd_accent, time);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
            PlayerState::Buffering { percent, .. } => {
                let label = match percent {
                    Some(pc) => format!("Buffering… {pc} %"),
                    None => "Buffering…".to_owned(),
                };
                let pill = pill_rect(&p, rect, &label, body_font.clone(), theme, k);
                p.rect_filled(pill, t.rounding, t.osd_scrim);
                spinner(&p, egui::pos2(pill.left() + pill.height() / 2.0, pill.center().y), pill.height() * 0.25, t.osd_accent, time);
                osd_text(
                    &p,
                    egui::pos2(pill.left() + pill.height(), pill.center().y),
                    Align2::LEFT_CENTER,
                    &label,
                    body_font,
                    t.osd_text,
                    f32::INFINITY,
                );
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
            PlayerState::Reconnecting { attempt, max, at, reason } => {
                p.rect_filled(rect, 0.0, t.osd_scrim);
                let secs = at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
                let head = reconnecting_head(*attempt, *max);
                let detail = if secs > 0 {
                    format!("{} · retrying in {secs} s", reason_label(*reason))
                } else {
                    format!("{} · retrying now", reason_label(*reason))
                };
                let hint = if focused { recovery_hint(placement, live, snap.previous_live.is_some(), true) } else { UNFOCUSED_HINT };
                let lines = [
                    (head.as_str(), title_font, t.osd_text),
                    (detail.as_str(), body_font, with_alpha(t.osd_text, 0.7)),
                    (hint, small_font, t.osd_accent),
                ];
                let bottom = stacked(&p, rect, &lines, gap);
                spinner(&p, egui::pos2(rect.center().x, rect.center().y - 3.0 * title_font_size(theme, k)), 10.0 * k, t.osd_accent, time);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                return buttons(bottom, true);
            }
            PlayerState::Error(e) => {
                p.rect_filled(rect, 0.0, t.osd_scrim);
                let (head, detail) = error_copy(e.kind, &e.message);
                let hint = if focused { recovery_hint(placement, live, snap.previous_live.is_some(), false) } else { UNFOCUSED_HINT };
                let glyph_font = FontId::proportional((theme.font(TypeToken::DisplayLarge) * k).round());
                let lines = [
                    (glyphs::WARNING, glyph_font, t.osd_accent),
                    (head.as_str(), title_font, t.osd_text),
                    (detail.as_str(), body_font, with_alpha(t.osd_text, 0.7)),
                    (hint, small_font, t.osd_accent),
                ];
                let bottom = stacked(&p, rect, &lines, gap);
                return buttons(bottom, false);
            }
            PlayerState::Ended => {
                p.rect_filled(rect, 0.0, t.osd_scrim);
                let lines = [(title, title_font, t.osd_text), ("Finished", body_font, with_alpha(t.osd_text, 0.7))];
                stacked(&p, rect, &lines, gap);
            }
            PlayerState::Playing | PlayerState::Paused => {
                // audio-only (radio) streams: logo + now playing instead of black
                let audio_only = snap.video_size.is_none() && snap.position.is_some_and(|s| s >= 1.0) && snap.tracks.video.is_empty();
                if audio_only {
                    let tile = 72.0 * k;
                    let mut y = rect.center().y - (tile + gap + 2.0 * title_font.size) / 2.0;
                    logo_tile(
                        &p,
                        theme,
                        Rect::from_center_size(egui::pos2(rect.center().x, y + tile / 2.0), egui::vec2(tile, tile)),
                        title,
                        self.logo_for(snap),
                    );
                    y += tile + gap;
                    osd_text(&p, egui::pos2(rect.center().x, y), Align2::CENTER_TOP, title, title_font.clone(), t.osd_text, rect.width() - 2.0 * gap);
                    if let Some(sub) = snap.playable.as_ref().and_then(|p| p.subtitle.as_deref()) {
                        osd_text(
                            &p,
                            egui::pos2(rect.center().x, y + title_font.size + gap),
                            Align2::CENTER_TOP,
                            sub,
                            body_font,
                            with_alpha(t.osd_text, 0.7),
                            rect.width() - 2.0 * gap,
                        );
                    }
                }
                if snap.state == PlayerState::Paused || snap.paused {
                    let s = (36.0 * k).round();
                    let r = Rect::from_center_size(rect.center(), egui::vec2(s * 1.6, s * 1.6));
                    if !audio_only {
                        p.rect_filled(r, t.rounding, t.osd_scrim);
                        p.text(r.center(), Align2::CENTER_CENTER, glyphs::PAUSE, FontId::proportional(s), t.osd_text);
                    }
                }
            }
        }
        Vec::new()
    }

    /// Corner mini-player (app calls when nothing else placed the video, the screen allows it,
    /// the layout isn't Compact and `interface.mini_player` is on). Returns what was clicked:
    /// the video opens the fullscreen player, the strip pops the player out or stops it.
    /// Records [`VideoHost::mini_rect`].
    /// `queue`: Up next's snapshot, for its chip over the video (`queue::ui::mini_chip`).
    pub fn mini_player(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        snap: &PlayerSnapshot,
        queue: &crate::queue::QueueSnapshot,
    ) -> Option<MiniAction> {
        self.mini_player_with(ctx, theme, snap, queue, None)
    }

    /// [`mini_player`](Self::mini_player) with multi-view's other picture (DESIGN §35):
    /// picture in picture, the focused one big and `inset` a third of the width at the top
    /// right, outlined; ✕ stops both.
    pub fn mini_player_with(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        snap: &PlayerSnapshot,
        queue: &crate::queue::QueueSnapshot,
        inset: Option<&PlayerSnapshot>,
    ) -> Option<MiniAction> {
        self.mini = None;
        if snap.state == PlayerState::Idle {
            return None;
        }
        let total = mini_rect_for(ctx.content_rect(), theme)?;
        let t = &theme.tokens;
        let strip_h = mini_strip_height(theme);
        let mut result = None;
        let restore = self.placed_this_frame;
        egui::Area::new(egui::Id::new("mini-player")).movable(false).order(egui::Order::Foreground).fixed_pos(total.min).show(ctx, |ui| {
            ui.set_min_size(total.size());
            let video = Rect::from_min_size(total.min, egui::vec2(total.width(), total.height() - strip_h));
            let resp = self.paint_video(ui, video, Placement::Mini, theme, snap, true);
            if resp.on_hover_text("Open the player").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                result = Some(MiniAction::Open);
            }
            if let Some(other) = inset {
                let w = (video.width() / 3.0).round();
                let m = theme.space(SpaceToken::Sm);
                let r = Rect::from_min_size(egui::pos2(video.right() - m - w, video.top() + m), egui::vec2(w, (w * 9.0 / 16.0).round()));
                let resp = self.paint_video(ui, r, Placement::Mini, theme, other, false);
                ui.painter().rect_stroke(r, 0.0, egui::Stroke::new(1.0, t.osd_text), egui::StrokeKind::Outside);
                if resp.on_hover_text("Open the player").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    result = Some(MiniAction::Open);
                }
            }
            crate::queue::ui::mini_chip(ui, theme, queue, video);
            let strip = Rect::from_min_max(egui::pos2(total.left(), video.bottom()), total.max);
            let p = ui.painter();
            p.rect_filled(strip, 0.0, t.card_bg);
            let pad = theme.space(SpaceToken::Lg);
            let close = Rect::from_center_size(egui::pos2(strip.right() - strip_h / 2.0, strip.center().y), egui::vec2(strip_h, strip_h));
            let pop = close.translate(egui::vec2(-strip_h, 0.0));
            let title = snap.playable.as_ref().map(|p| p.title.as_str()).unwrap_or("");
            let mut label = title.to_owned();
            if let Some(n) = snap.playable.as_ref().and_then(|p| p.number) {
                label = format!("{title} · {n}");
            }
            osd_text(
                p,
                egui::pos2(strip.left() + pad, strip.center().y),
                Align2::LEFT_CENTER,
                &label,
                FontId::proportional(theme.font(TypeToken::BodySmall)),
                t.fg,
                (pop.left() - strip.left() - 2.0 * pad).max(0.0),
            );
            // strip buttons: pop out, stop
            let strip_button = |ui: &egui::Ui, r: Rect, id: &str, glyph: &str, label: &str, tip: &str| {
                let resp = ui.interact(r, ui.id().with(id), egui::Sense::click()).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
                let p = ui.painter();
                if resp.hovered() {
                    p.rect_filled(r.shrink(2.0), t.rounding, t.fill_hover);
                }
                let color = if resp.hovered() { t.accent } else { t.text_secondary };
                p.text(r.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(theme.font(TypeToken::Icon)), color);
                resp.clicked()
            };
            if strip_button(ui, pop, "mini-pop-out", glyphs::POP_OUT, "Pop out", "Pop out (Ctrl+Shift+O)") {
                result = Some(MiniAction::PopOut);
            }
            let stop = if inset.is_some() { "Stop both pictures" } else { "Stop playback" };
            if strip_button(ui, close, "mini-close", glyphs::CLOSE, stop, stop) {
                result = Some(MiniAction::Stop);
            }
            let p = ui.painter();
            p.rect_stroke(total, t.rounding, egui::Stroke::new(t.card_border_width.min(2.0), t.card_border), egui::StrokeKind::Outside);
        });
        // not a placement (see module docs)
        self.placed_this_frame = restore;
        self.mini = Some(total);
        self.mini_drawn = true;
        result
    }

    /// Free GL resources (from `App::on_exit`, before the players shut down): picture B's
    /// surface first, then A's.
    pub fn destroy(&mut self, gl: Option<&glow::Context>) {
        for t in self.tiles.iter_mut().rev() {
            if let Some(s) = &mut t.surface {
                s.destroy(gl);
            }
            t.surface = None;
        }
    }
}

/// Fit the video's aspect inside `rect` (centred). Unknown size → whole rect. (Used for the
/// mini-player window shape; the main surface lets mpv letterbox.)
pub fn letterbox(rect: egui::Rect, video: Option<(i64, i64)>) -> egui::Rect {
    let Some((w, h)) = video.filter(|(w, h)| *w > 0 && *h > 0) else { return rect };
    let ar = w as f32 / h as f32;
    let (rw, rh) = (rect.width(), rect.height());
    let (tw, th) = if rw / rh > ar { (rh * ar, rh) } else { (rw, rw / ar) };
    egui::Rect::from_center_size(rect.center(), egui::vec2(tw, th))
}

/// Height of the mini-player's title strip.
fn mini_strip_height(theme: &Theme) -> f32 {
    theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Sm)
}

/// Where the mini-player goes in a window: bottom-right above the status bar, 16:9, 320 pt
/// (Wide) / 240 pt (Medium) × rem; none in Compact.
pub(crate) fn mini_rect_for(screen: Rect, theme: &Theme) -> Option<Rect> {
    let rem = theme.rem();
    let w = match LayoutClass::from_width(screen.width(), rem) {
        LayoutClass::Wide => 320.0,
        LayoutClass::Medium => 240.0,
        LayoutClass::Compact => return None,
    } * rem;
    let h = (w * 9.0 / 16.0).round() + mini_strip_height(theme);
    let margin = theme.space(SpaceToken::PanelPadding);
    let status = (26.0 * rem).round();
    let max = egui::pos2(screen.right() - margin, screen.bottom() - status - margin);
    Some(Rect::from_min_max(egui::pos2(max.x - w.round(), max.y - h), max))
}

/// Overlay type scale for a video rect: small in the mini-player, larger fullscreen.
fn overlay_scale(rect: Rect) -> f32 {
    (rect.width() / 900.0).clamp(0.75, 1.6)
}

fn body_font_size(theme: &Theme, k: f32) -> f32 {
    (theme.font(TypeToken::Body) * k).round()
}
fn title_font_size(theme: &Theme, k: f32) -> f32 {
    (theme.font(TypeToken::Title) * k).round()
}

/// User-facing copy for a playback failure (DESIGN §11.4, §13.12): headline + detail.
pub(crate) fn error_copy(kind: PlayerErrorKind, message: &str) -> (String, String) {
    let detail = crate::redact::redact(message).into_owned();
    let head = match kind {
        PlayerErrorKind::HttpForbidden => "The provider refused the stream (account expired or too many connections)",
        PlayerErrorKind::HttpNotFound => "Channel offline",
        PlayerErrorKind::Drm => "DRM-protected — can't be played",
        PlayerErrorKind::Timeout => "The stream didn't start in time",
        PlayerErrorKind::Network => "Network error",
        PlayerErrorKind::Stalled => "The stream stalled",
        PlayerErrorKind::StreamEnded => "The stream ended",
        PlayerErrorKind::Unsupported => "This stream's format isn't supported",
        PlayerErrorKind::Resolve => "Couldn't open this item",
        PlayerErrorKind::Budget => "Connection limit reached for this playlist",
        PlayerErrorKind::ProviderError => "The provider can't deliver this channel right now",
        PlayerErrorKind::HttpOther(_) | PlayerErrorKind::Mpv => "Playback failed",
        // CAST-8: the casting card's own copy and actions (DESIGN §40 §3.4)
        PlayerErrorKind::Cast(c) => crate::player::state::describe_cast(c),
    };
    let detail = if detail.is_empty() || detail == head { String::new() } else { detail };
    (head.to_owned(), detail)
}

/// Headline of the reconnect overlay: "attempt n" when the player keeps trying (`max` 0: a
/// channel that played, the connection-limit race), "n/max" while the attempts are bounded.
pub(crate) fn reconnecting_head(attempt: u32, max: u32) -> String {
    if max == 0 { format!("Reconnecting… attempt {attempt}") } else { format!("Reconnecting… ({attempt}/{max})") }
}

/// Short reason for the reconnect line.
fn reason_label(kind: PlayerErrorKind) -> &'static str {
    match kind {
        PlayerErrorKind::ProviderError => "The provider can't deliver this channel",
        PlayerErrorKind::HttpForbidden => "Refused by the provider",
        PlayerErrorKind::HttpNotFound => "Not found",
        PlayerErrorKind::Timeout => "Timed out",
        PlayerErrorKind::Stalled => "Stalled",
        PlayerErrorKind::StreamEnded => "Stream ended",
        _ => "Connection lost",
    }
}

/// The hint on a multi-view picture keys don't go to.
const UNFOCUSED_HINT: &str = "Click it or press w to choose this picture";

/// The key hint under a failed / reconnecting stream. The player's keys (`r`, Backspace) only
/// work in the fullscreen player: in the Live list `r` types into the filter and Backspace
/// goes back a pane, so the preview and the mini-player name what works there.
pub(crate) fn recovery_hint(placement: Placement, live: bool, has_last_channel: bool, reconnecting: bool) -> &'static str {
    match placement {
        Placement::Mini => "Click to open the player",
        // Enter on a failed channel plays it again (`playing_channel` ignores the error);
        // while reconnecting the player retries by itself (until stopped)
        Placement::Preview if reconnecting => "Shift+Enter fullscreen · Ctrl+Shift+S Stop",
        Placement::Preview => "Enter retries · Shift+Enter fullscreen",
        Placement::Fullscreen | Placement::None => match (reconnecting, live && has_last_channel) {
            (true, true) => "r Retry now · Backspace Last channel · q Stop",
            (true, false) => "r Retry now · q Stop",
            (false, true) => "r Retry · Backspace Last channel",
            (false, false) => "r Retry",
        },
    }
}

/// One OSD text line, elided.
pub(crate) fn osd_text(p: &egui::Painter, pos: egui::Pos2, anchor: Align2, s: &str, font: FontId, color: Color32, max_w: f32) -> Rect {
    let mut job = egui::text::LayoutJob::simple_singleline(s.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(0.0));
    let g: Arc<egui::Galley> = p.layout_job(job);
    let r = anchor.anchor_size(pos, g.size());
    p.galley(r.min, g, color);
    r
}

/// Lines stacked and centred in `rect`; each wraps to at most two rows (a long error
/// headline keeps its tail — "…or too many connections" is the part users need). Returns the
/// bottom of the last line.
fn stacked(p: &egui::Painter, rect: Rect, lines: &[(&str, FontId, Color32)], gap: f32) -> f32 {
    let max_w = (rect.width() - 2.0 * gap).max(0.0);
    let galleys: Vec<Arc<egui::Galley>> = lines
        .iter()
        .filter(|(s, _, _)| !s.is_empty())
        .map(|(s, f, c)| {
            let mut job = egui::text::LayoutJob::simple((*s).to_owned(), f.clone(), *c, max_w);
            job.halign = egui::Align::Center;
            job.wrap.max_rows = 2;
            p.layout_job(job)
        })
        .collect();
    let h: f32 = galleys.iter().map(|g| g.size().y).sum::<f32>() + gap * galleys.len().saturating_sub(1) as f32;
    let mut y = rect.center().y - h / 2.0;
    let mut bottom = y;
    for g in galleys {
        let gh = g.size().y;
        // a centre-aligned galley is laid out around x = 0
        p.galley(egui::pos2(rect.center().x, y), g, Color32::PLACEHOLDER);
        bottom = y + gh;
        y += gh + gap;
    }
    bottom
}

/// The channel logo (aspect kept, fit into `r`) on the OSD scrim, or its monogram tile.
fn logo_tile(p: &egui::Painter, theme: &Theme, r: Rect, name: &str, logo: Option<&egui::TextureHandle>) {
    let t = &theme.tokens;
    if let Some(tex) = logo {
        let s = tex.size_vec2();
        let scale = (r.width() / s.x.max(1.0)).min(r.height() / s.y.max(1.0));
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        p.image(tex.id(), Rect::from_center_size(r.center(), s * scale), uv, Color32::WHITE);
        return;
    }
    p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
    p.text(r.center(), Align2::CENTER_CENTER, logo::monogram(name), FontId::proportional((r.height() * 0.4).round().max(6.0)), t.osd_text);
}

/// Rotating arc (OSD colours).
fn spinner(p: &egui::Painter, c: egui::Pos2, r: f32, color: Color32, time: f64) {
    let start = (time * 4.0) as f32;
    let pts: Vec<egui::Pos2> =
        (0..=24).map(|i| start + i as f32 / 24.0 * std::f32::consts::PI * 1.5).map(|a| c + r * egui::vec2(a.cos(), a.sin())).collect();
    p.add(egui::Shape::line(pts, egui::Stroke::new((r * 0.22).max(2.0), color)));
}

/// A pill at the bottom centre of `rect` sized for `label`.
fn pill_rect(p: &egui::Painter, rect: Rect, label: &str, font: FontId, theme: &Theme, k: f32) -> Rect {
    let g = p.layout_no_wrap(label.to_owned(), font, Color32::WHITE);
    let h = g.size().y + 2.0 * theme.space(SpaceToken::Md) * k;
    let w = g.size().x + h * 1.5;
    Rect::from_center_size(egui::pos2(rect.center().x, rect.bottom() - h - theme.space(SpaceToken::Huge) * k), egui::vec2(w, h))
}

/// Is `kind` live TV (zapping applies)?
pub(crate) fn is_live(kind: Option<&PlayableKind>) -> bool {
    kind.is_none_or(PlayableKind::is_live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini_rect_sits_bottom_right_above_the_status_bar() {
        let th = Theme::fallback();
        let wide = mini_rect_for(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0)), &th).unwrap();
        assert_eq!(wide.width(), 320.0);
        assert!(wide.right() < 1600.0 && wide.bottom() <= 900.0 - 26.0);
        let medium = mini_rect_for(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(790.0, 600.0)), &th).unwrap();
        assert_eq!(medium.width(), 240.0);
        assert!(mini_rect_for(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 600.0)), &th).is_none());
    }

    #[test]
    fn error_copy_per_kind() {
        let (h, d) = error_copy(PlayerErrorKind::HttpForbidden, "HTTP 403");
        assert!(h.contains("too many connections"));
        assert_eq!(d, "HTTP 403");
        assert_eq!(error_copy(PlayerErrorKind::HttpNotFound, "").0, "Channel offline");
        assert!(error_copy(PlayerErrorKind::Drm, "x").0.starts_with("DRM"));
        let (h, d) = error_copy(PlayerErrorKind::ProviderError, "The provider said: “Cannot read /home/x/streams/1_.m3u8”");
        assert_eq!(h, "The provider can't deliver this channel right now");
        assert!(d.contains("Cannot read"), "the provider's own words: {d}");
    }

    /// Field fix: an unbounded reconnect reads "attempt n", never "(n/0)".
    #[test]
    fn reconnecting_head_never_shows_a_zero_maximum() {
        assert_eq!(reconnecting_head(12, 0), "Reconnecting… attempt 12");
        assert_eq!(reconnecting_head(2, 8), "Reconnecting… (2/8)");
    }

    #[test]
    fn recovery_hints_name_keys_that_work_where_the_video_is() {
        assert_eq!(recovery_hint(Placement::Fullscreen, true, true, false), "r Retry · Backspace Last channel");
        assert_eq!(recovery_hint(Placement::Fullscreen, true, false, false), "r Retry", "no last channel to go back to");
        assert_eq!(recovery_hint(Placement::Fullscreen, false, true, true), "r Retry now · q Stop");
        // the Live list: `r` types into the filter, Backspace goes back a pane
        for reconnecting in [false, true] {
            let h = recovery_hint(Placement::Preview, true, true, reconnecting);
            assert!(!h.contains("r Retry") && !h.contains("Backspace"), "{h}");
        }
        assert_eq!(recovery_hint(Placement::Mini, true, true, false), "Click to open the player");
    }

    #[test]
    fn letterbox_fits() {
        let r = letterbox(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 400.0)), Some((1920, 1080)));
        assert_eq!(r.width(), 400.0);
        assert_eq!(r.height(), 225.0);
        assert_eq!(letterbox(r, None), r);
    }
}
