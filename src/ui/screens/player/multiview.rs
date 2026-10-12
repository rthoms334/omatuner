//! The multi-view part of the player screen (DESIGN §35): both pictures in their layout, the
//! focus ring and corner letters, each picture's hover chrome (sound here, swap, close), the
//! info strips (side by side; the small picture of big + small) and a caption on the other
//! layouts' small picture, the multi-view group (Layout · Sound here · Swap · Close), its keys
//! and its mouse paths (click = focus, drag a picture onto the other = swap, drag the inset =
//! snap to a corner). OWNER: MV-2.
//!
//! Single view draws nothing here. Everything over the video uses the fixed OSD colours.

use super::*;
use crate::model::TileId;
use crate::ui::multiview::{self as mv, Dir, Layout, MvRequest, Placed, glyph};

/// The player's multi-view state between frames.
#[derive(Default)]
pub(super) struct MvScreen {
    /// Where each picture went last frame (Ctrl+arrows, drops).
    placed: Vec<Placed>,
    area: Option<Rect>,
    /// Captions of the small picture show until then (after a zap or a focus change).
    caption_until: Option<Instant>,
    /// What the pictures played last frame (a change shows the caption).
    seen: [Option<(TileId, u64)>; 2],
}

/// Rows of the info strip.
struct StripText {
    head: String,
    now: Option<(String, String, f32)>,
    next: Option<String>,
}

impl PlayerScreen {
    /// Multi-view on: paint both pictures in the layout and handle their mouse paths. Returns
    /// the response over the whole video area (clicks, double-clicks, right-clicks and hover of
    /// any picture), or `None` in single view.
    pub(super) fn paint_multiview(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) -> Option<egui::Response> {
        let frame = ctx.multiview.clone();
        if !frame.on() {
            self.mv.placed.clear();
            return None;
        }
        let theme = ctx.theme;
        let view = frame.view;
        let layout = Layout::effective(ctx.config.player.multiview.layout, rect.width(), theme.rem(), ctx.video_only);
        let corner = ctx.config.player.multiview.pip_corner;
        let placed = mv::geometry(layout, rect, view.slots, corner, theme.rem());
        // the whole area first: pictures drawn over it take their own clicks
        let bg = ui.interact(rect, ui.id().with("mv-area"), egui::Sense::click());
        ui.painter_at(rect).rect_filled(rect, 0.0, Color32::BLACK);
        let mut resp = bg;
        let sense = if ctx.video_only { egui::Sense::click() } else { egui::Sense::click_and_drag() };
        for p in &placed {
            let focused = p.tile == view.focus;
            let r = match frame.snap(p.tile) {
                Some(snap) if !(frame.starting && p.tile != TileId::A && snap.state == PlayerState::Idle) => {
                    let snap = snap.clone();
                    ctx.video.paint_tile(ui, p.video, Placement::Fullscreen, theme, ctx.images, &snap, focused)
                }
                _ => {
                    // picture B's player is starting
                    let r = ui.allocate_rect(p.video, egui::Sense::click());
                    let painter = ui.painter_at(p.video);
                    painter.rect_filled(p.video, 0.0, Color32::BLACK);
                    let font = FontId::proportional(theme.font(TypeToken::Body));
                    osd_text(
                        &painter,
                        p.video.center(),
                        Align2::CENTER_CENTER,
                        &format!("Starting picture {}…", p.tile),
                        font,
                        theme.tokens.osd_text,
                        p.video.width(),
                    );
                    r
                }
            };
            let r = ui.interact(p.video, ui.id().with(("mv-tile", p.tile.0)), sense).union(r);
            if r.clicked() || r.secondary_clicked() {
                // a click chooses the picture (never the sound); the menu is that picture's
                ctx.mv_request(MvRequest::Focus(p.tile));
            }
            if r.drag_stopped()
                && let Some(at) = ui.input(|i| i.pointer.interact_pos())
            {
                self.drop_picture(ctx, &placed, p, layout, rect, at);
            }
            if r.dragged() {
                // the drop target lights up
                if let Some(target) =
                    placed.iter().find(|q| q.tile != p.tile && ui.input(|i| i.pointer.hover_pos()).is_some_and(|h| q.video.contains(h)))
                {
                    ui.painter().rect_stroke(target.video, 0.0, egui::Stroke::new(3.0, theme.tokens.osd_accent), egui::StrokeKind::Inside);
                }
            }
            resp = resp.union(r);
        }
        // a zap or a focus change shows the small picture's caption for a while
        let seen = [view.focus, view.unfocused()].map(|t| frame.snap(t).map(|s| (t, s.load_generation)));
        if seen != self.mv.seen {
            self.mv.seen = seen;
            self.mv.caption_until = Some(Instant::now() + Duration::from_millis(3500));
        }
        self.mv.placed = placed;
        self.mv.area = Some(rect);
        Some(resp)
    }

    /// Where each picture went last frame (empty in single view).
    pub(super) fn mv_placed(&self) -> &[Placed] {
        &self.mv.placed
    }

    /// A picture dragged and dropped at `at`: onto the other picture swaps places; the inset
    /// dropped elsewhere snaps to the nearest corner (saved).
    fn drop_picture(&mut self, ctx: &mut UiCtx<'_>, placed: &[Placed], p: &Placed, layout: Layout, area: Rect, at: egui::Pos2) {
        let other = placed.iter().find(|q| q.tile != p.tile);
        let inset = layout == Layout::Pip && placed.last().is_some_and(|l| l.tile == p.tile);
        if inset {
            let big = placed[0].video;
            let centre_third = Rect::from_center_size(big.center(), big.size() / 3.0);
            if centre_third.contains(at) {
                ctx.mv_request(MvRequest::Swap);
            } else {
                ctx.mv_request(MvRequest::SetCorner(mv::nearest_corner(area, at)));
            }
        } else if other.is_some_and(|o| o.video.contains(at)) {
            ctx.mv_request(MvRequest::Swap);
        }
    }

    /// Draw the multi-view chrome over the pictures; nothing in single view.
    pub(super) fn draw_multiview(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, _rect: Rect) {
        if !ctx.multiview.on() || self.mv.placed.is_empty() {
            return;
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        let view = ctx.multiview.view;
        let banner = self.banner_visible(ctx.player);
        let caption = banner || self.mv.caption_until.is_some_and(|u| Instant::now() < u);
        if let Some(u) = self.mv.caption_until {
            ui.ctx().request_repaint_after(u.saturating_duration_since(Instant::now()));
        }
        let placed = self.mv.placed.clone();
        let letter_font = FontId::proportional(theme.font(TypeToken::Title));
        let icon_font = FontId::proportional(theme.font(TypeToken::Icon));
        for p in &placed {
            let painter = ui.painter_at(p.video.expand(3.0));
            let focused = p.tile == view.focus;
            // the focus ring: keys act on this picture (dimmer once the banner hides)
            if focused {
                let alpha = if banner { 1.0 } else { 0.4 };
                painter.rect_stroke(p.video, 0.0, egui::Stroke::new(2.0, with_alpha(t.osd_accent, alpha)), egui::StrokeKind::Inside);
            }
            // the corner letter, with the speaker on the audible picture
            let letter = if p.tile == view.audible { format!("{} {}", p.tile, glyph::SOUND) } else { p.tile.to_string() };
            let g = painter.layout_no_wrap(letter, letter_font.clone(), t.osd_text);
            let pad = theme.space(SpaceToken::Sm);
            let lr = Rect::from_min_size(p.video.min + egui::vec2(pad, pad), g.size() + egui::vec2(2.0 * pad, pad));
            painter.rect_filled(lr, t.rounding, t.osd_scrim);
            painter.galley(lr.min + egui::vec2(pad, pad / 2.0), g, t.osd_text);
            // hover chrome: sound here, swap, close (top right)
            if ui.rect_contains_pointer(p.video) {
                let side = (theme.font(TypeToken::Icon) * 1.8).round();
                let mut x = p.video.right() - pad - side;
                let buttons = [
                    (glyph::CLOSE, "Close this picture", "Close this picture (q)", MvRequest::Close(Some(p.tile))),
                    (glyph::SWAP, "Swap places", "Swap places (W)", MvRequest::Swap),
                    (glyph::SOUND, "Sound from this picture", "Sound from this picture (M)", MvRequest::Sound(Some(p.tile))),
                ];
                for (i, (g, label, tip, req)) in buttons.into_iter().enumerate() {
                    if req == MvRequest::Sound(Some(p.tile)) && p.tile == view.audible {
                        continue;
                    }
                    let r = Rect::from_min_size(egui::pos2(x, p.video.top() + pad), egui::vec2(side, side));
                    painter.rect_filled(r, t.rounding, t.osd_scrim);
                    if osd_button(ui, &painter, theme, r, ui.id().with(("mv-chrome", p.tile.0, i)), g, icon_font.clone(), label, Some(tip), false)
                        .clicked()
                    {
                        ctx.mv_request(req);
                    }
                    x -= side + pad / 2.0;
                }
            }
            // what plays: the strip under the picture, else a caption on the small picture
            let Some(snap) = ctx.multiview.snap(p.tile).cloned() else { continue };
            match p.strip {
                Some(strip) => self.draw_strip(ui, ctx, strip, &snap, focused),
                None if caption && !focused => {
                    let text = strip_text(ctx, &snap);
                    let font = FontId::proportional(theme.font(TypeToken::BodySmall));
                    let h = font.size + 2.0 * pad;
                    let r = Rect::from_min_max(egui::pos2(p.video.left(), p.video.bottom() - h), p.video.max);
                    painter.rect_filled(r, 0.0, t.osd_scrim);
                    let line = match &text.now {
                        Some((title, _, _)) => format!("{} · {title}", text.head),
                        None => text.head,
                    };
                    osd_text(&painter, egui::pos2(r.left() + pad, r.center().y), Align2::LEFT_CENTER, &line, font, t.osd_text, r.width() - 2.0 * pad);
                }
                None => {}
            }
        }
    }

    /// One picture's info strip: number, name, playlist · programme, time and progress · next.
    fn draw_strip(&self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, strip: Rect, snap: &PlayerSnapshot, focused: bool) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let painter = ui.painter_at(strip);
        let text = strip_text(ctx, snap);
        let pad = theme.space(SpaceToken::Sm);
        let w = strip.width() - 2.0 * pad;
        let head_font = FontId::proportional(theme.font(TypeToken::Body));
        let small = FontId::proportional(theme.font(TypeToken::BodySmall));
        let line = (strip.height() - 2.0 * pad) / 3.0;
        let mut y = strip.top() + pad + line / 2.0;
        let color = if focused { t.osd_text } else { with_alpha(t.osd_text, 0.8) };
        osd_text(&painter, egui::pos2(strip.left() + pad, y), Align2::LEFT_CENTER, &text.head, head_font, color, w);
        y += line;
        if let Some((title, range, progress)) = &text.now {
            let bar_w = (w * 0.25).clamp(40.0, 160.0);
            let r = osd_text(
                &painter,
                egui::pos2(strip.right() - pad, y),
                Align2::RIGHT_CENTER,
                range,
                small.clone(),
                with_alpha(t.osd_text, 0.7),
                w / 3.0,
            );
            let bar = Rect::from_min_size(egui::pos2(r.left() - pad - bar_w, y - 2.0), egui::vec2(bar_w, 4.0));
            painter.rect_filled(bar, 2.0, with_alpha(t.osd_text, 0.2));
            painter.rect_filled(Rect::from_min_size(bar.min, egui::vec2(bar_w * progress.clamp(0.0, 1.0), 4.0)), 2.0, t.osd_accent);
            osd_text(
                &painter,
                egui::pos2(strip.left() + pad, y),
                Align2::LEFT_CENTER,
                title,
                small.clone(),
                color,
                (bar.left() - strip.left() - 2.0 * pad).max(0.0),
            );
        }
        y += line;
        if let Some(next) = &text.next {
            osd_text(&painter, egui::pos2(strip.left() + pad, y), Align2::LEFT_CENTER, next, small, with_alpha(t.osd_text, 0.6), w);
        }
    }

    /// The multi-view group of the controls (with the banner): Layout · Sound here · Swap ·
    /// Close, centred above the bottom bar.
    pub(super) fn draw_multiview_bar(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        if !ctx.multiview.on() || !self.banner_visible(ctx.player) || ctx.video_only && !self.osd_focus {
            return;
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        let view = ctx.multiview.view;
        let s = scale_of(ui);
        let font = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let narrow = Self::narrow(rect.width(), theme);
        let items = [
            (glyph::LAYOUT, "Layout", "Layout: side by side, big + small, picture in picture (t)", MvRequest::CycleLayout),
            (glyph::SOUND, "Sound here", "Sound from this picture (M)", MvRequest::Sound(None)),
            (glyph::SWAP, "Swap", "Swap places (W)", MvRequest::Swap),
            (glyph::CLOSE, "Close", "Close this picture; the other carries on (q)", MvRequest::Close(None)),
        ];
        let pad = (theme.space(SpaceToken::Md) * s).round();
        let h = font.size + 2.0 * pad;
        let p = ui.painter_at(rect);
        let texts: Vec<String> = items.iter().map(|(g, l, ..)| if narrow { (*g).to_owned() } else { format!("{g} {l}") }).collect();
        let widths: Vec<f32> = texts.iter().map(|s| p.layout_no_wrap(s.clone(), font.clone(), t.osd_text).size().x + 2.0 * pad).collect();
        let total = widths.iter().sum::<f32>();
        let bottom = rect.bottom() - self.bottom_bar.unwrap_or(0.0) - pad;
        let bar = Rect::from_min_size(egui::pos2(rect.center().x - total / 2.0, bottom - h), egui::vec2(total, h));
        p.rect_filled(bar, t.rounding, t.osd_scrim);
        self.bars.push(bar);
        let mut x = bar.left();
        for (i, ((_, label, tip, req), w)) in items.into_iter().zip(widths).enumerate() {
            let r = Rect::from_min_size(egui::pos2(x, bar.top()), egui::vec2(w, h));
            x += w;
            // "Sound here" on the picture that has it does nothing: drawn dimmed
            let disabled = req == MvRequest::Sound(None) && view.audible == view.focus;
            let text = &texts[i];
            let mut painter = p.clone();
            if disabled {
                painter.multiply_opacity(0.45);
            }
            let tip = if disabled { "This picture has the sound" } else { tip };
            if osd_button(ui, &painter, theme, r, ui.id().with(("mv-bar", i)), text, font.clone(), label, Some(tip), false).clicked() && !disabled {
                ctx.mv_request(req);
                self.show_banner(ctx);
            }
        }
    }

    /// A multi-view key (`KeyContext::MultiView`, and Ctrl+M "Watch something alongside…" in
    /// single view); `None` when not one.
    pub(super) fn multiview_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if !mv::ENABLED {
            return None;
        }
        let view = ctx.multiview.view;
        if action == Action::WatchAlongside {
            let snap = ctx.player;
            let live = snap.state != PlayerState::Idle
                && snap.playable.as_ref().is_some_and(|p| !matches!(p.kind, PlayableKind::Movie | PlayableKind::Episode { .. }));
            if live || view.on {
                ctx.open(OverlayRequest::WatchAlongside);
            } else {
                ctx.toast(Toast::info(crate::playback::route::MULTIVIEW_LIVE_ONLY));
            }
            return Some(true);
        }
        if !view.on {
            return None;
        }
        let flash = |s: &mut Self, text: String| s.flash_text = Some((text, Instant::now()));
        match action {
            Action::NextPicture => ctx.mv_request(MvRequest::FocusNext),
            Action::FocusPictureLeft | Action::FocusPictureRight | Action::FocusPictureUp | Action::FocusPictureDown => {
                let dir = match action {
                    Action::FocusPictureLeft => Dir::Left,
                    Action::FocusPictureRight => Dir::Right,
                    Action::FocusPictureUp => Dir::Up,
                    _ => Dir::Down,
                };
                if let Some(t) = mv::neighbour(&self.mv.placed, view.focus, dir) {
                    ctx.mv_request(MvRequest::Focus(t));
                }
            }
            Action::SoundHere => {
                ctx.mv_request(MvRequest::Sound(None));
                flash(self, format!("{} Sound from picture {}", glyph::SOUND, view.focus));
            }
            Action::SwapPictures => ctx.mv_request(MvRequest::Swap),
            Action::CycleLayout => {
                let area = self.mv.area.map_or(1600.0, |r| r.width());
                let next = Layout::effective(ctx.config.player.multiview.layout, area, ctx.theme.rem(), ctx.video_only).next();
                ctx.mv_request(MvRequest::CycleLayout);
                flash(self, format!("{} {}", glyph::LAYOUT, next.label()));
            }
            Action::ClosePicture => ctx.mv_request(MvRequest::Close(None)),
            _ => return None,
        }
        self.show_banner(ctx);
        Some(true)
    }

    /// The player menu's multi-view section (on top), for the focused picture.
    pub(super) fn multiview_menu(&self, ctx: &UiCtx<'_>) -> Vec<crate::ui::overlays::MenuItem> {
        let view = ctx.multiview.view;
        if !view.on {
            return Vec::new();
        }
        let k = KeyContext::MultiView;
        vec![
            menu_item(glyph::SOUND, "Sound from this picture", Action::SoundHere, view.audible != view.focus, k),
            menu_item(glyph::SWAP, "Swap places", Action::SwapPictures, true, k),
            menu_item(glyph::LAYOUT, "Next layout", Action::CycleLayout, true, k),
            menu_item(glyph::CLOSE, format!("Close picture {}", view.focus), Action::ClosePicture, true, k),
            menu_item(glyph::CLOSE, format!("Close picture {}", view.unfocused()), Action::Digit(CLOSE_OTHER), true, k),
        ]
    }
}

/// The player menu's "Close the other picture" (a code above typed digits and copies).
pub(super) const CLOSE_OTHER: u8 = 250;

/// What a picture's strip / caption says (the spoiler guard decides the programme's title).
fn strip_text(ctx: &mut UiCtx<'_>, snap: &PlayerSnapshot) -> StripText {
    let h24 = ctx.config.interface.clock_24h;
    let ch = live::playing_channel(snap).and_then(|id| ctx.lib.live.get(id).cloned());
    let Some(ch) = ch else {
        let title = snap.playable.as_ref().map(|p| p.title.clone()).unwrap_or_default();
        return StripText { head: title, now: None, next: None };
    };
    let mut head = match ch.number {
        Some(n) => format!("{n} {}", ch.name),
        None => ch.name.to_string(),
    };
    if let Some(src) = ctx.lib.source(ch.source_id) {
        head.push_str(&format!("  {} {}", crate::ui::widgets::glyphs::DOT, src.name));
    }
    let catchup = matches!(snap.playable.as_ref().map(|p| &p.kind), Some(PlayableKind::Catchup { .. }));
    let nn = if catchup { None } else { live::now_next_of(ctx, &ch) };
    let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
    let title_of = |p: &crate::model::ProgrammeSlim| {
        spoilers.text(&crate::ui::widgets::spoiler::slim_subject(p), crate::guard::Field::Title, &p.title).map(|c| c.into_owned()).unwrap_or_default()
    };
    let now = nn.as_ref().and_then(|n| n.now.clone()).map(|p| (title_of(&p), time_range(p.start, p.stop, h24), p.progress(ctx.now)));
    let next = nn.as_ref().and_then(|n| n.next.clone()).map(|p| format!("Next {} {}", hhmm(p.start, h24), title_of(&p)));
    let now = now.or_else(|| {
        snap.playable.as_ref().filter(|_| catchup).map(|p| {
            let pos = snap.position.zip(snap.duration).map_or(0.0, |(a, b)| (a / b.max(1.0)) as f32);
            // a catch-up's sub-title is its programme's title: the spoiler guard's, as the guide's
            let start = match p.kind {
                PlayableKind::Catchup { start, .. } => start,
                _ => 0,
            };
            let title = p.subtitle.as_deref().map_or_else(
                || "Catch-up".to_owned(),
                |t| {
                    let subject =
                        crate::guard::Subject::Programme { title: t, sub_title: None, categories: &[], season_episode: None, start, is_new: false };
                    spoilers.text(&subject, crate::guard::Field::Title, t).map(|c| c.into_owned()).unwrap_or_default()
                },
            );
            (title, "Catch-up".into(), pos)
        })
    });
    StripText { head, now, next }
}
