//! Audio / subtitle track menu over the video (fixed dark scrim). Subtitle menu also has delay
//! (−/+ 100 ms) and size rows. Owner: WP-11.
//!
//! A panel at the right edge of the window, vertically centred (clear of the OSD bars and the
//! controls row it is opened from), in OSD colours (`osd_scrim`, `osd_text`, `osd_accent`)
//! so it stays legible over any video and in light themes. Enter picks a track (and closes);
//! ←/→ on the delay and size rows adjust them (the menu stays open); Tab switches between the
//! audio and subtitle menus; Esc or a click outside closes. Track data comes from the player
//! snapshot every frame, so the selection mark follows mpv.

use eframe::egui::{self, Align2, FontId, Rect};

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::player::{PlayerCommand, PlayerSnapshot};
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::UiCtx;
use crate::ui::video_host::osd_text;
use crate::ui::widgets::glyphs;

/// One row of the menu.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Track { id: i64, label: String, selected: bool },
    SubsOff { selected: bool },
    Delay(f64),
    Size(f64),
}

pub struct TracksOverlay {
    pub audio: bool,
    cursor: usize,
    /// The cursor was placed on the selected track once.
    placed: bool,
}

impl TracksOverlay {
    pub fn new(audio: bool) -> Self {
        Self { audio, cursor: 0, placed: false }
    }

    fn rows(&self, snap: &PlayerSnapshot) -> Vec<Row> {
        let mut v = Vec::new();
        if self.audio {
            for t in &snap.tracks.audio {
                v.push(Row::Track { id: t.id, label: t.label(), selected: t.selected });
            }
        } else {
            v.push(Row::SubsOff { selected: snap.tracks.selected_sub().is_none() });
            for t in &snap.tracks.subs {
                v.push(Row::Track { id: t.id, label: t.label(), selected: t.selected });
            }
            v.push(Row::Delay(snap.sub_delay));
            v.push(Row::Size(snap.sub_scale));
        }
        v
    }

    fn place_cursor(&mut self, rows: &[Row]) {
        if !self.placed {
            self.placed = true;
            self.cursor = rows.iter().position(|r| matches!(r, Row::Track { selected: true, .. } | Row::SubsOff { selected: true })).unwrap_or(0);
        }
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
    }

    /// Enter / click on a row.
    fn choose(&self, ctx: &UiCtx<'_>, row: &Row) -> OverlayOutcome {
        match row {
            Row::Track { id, .. } => {
                ctx.play(if self.audio { PlayerCommand::SetAudioTrack(Some(*id)) } else { PlayerCommand::SetSubtitleTrack(Some(*id)) });
                OverlayOutcome::Close
            }
            Row::SubsOff { .. } => {
                ctx.play(PlayerCommand::SetSubtitleTrack(None));
                OverlayOutcome::Close
            }
            Row::Delay(_) => {
                ctx.play(PlayerCommand::ResetSubDelay);
                OverlayOutcome::Keep
            }
            Row::Size(s) => {
                ctx.play(PlayerCommand::AdjustSubScale(1.0 - s));
                OverlayOutcome::Keep
            }
        }
    }

    /// ←/→ on the delay / size rows.
    fn adjust(&self, ctx: &UiCtx<'_>, row: &Row, up: bool) {
        let sign = if up { 1.0 } else { -1.0 };
        match row {
            Row::Delay(_) => ctx.play(PlayerCommand::AdjustSubDelay(0.1 * sign)),
            Row::Size(_) => ctx.play(PlayerCommand::AdjustSubScale(0.1 * sign)),
            _ => {}
        }
    }
}

fn row_label(row: &Row) -> (String, bool) {
    match row {
        Row::Track { label, selected, .. } => (label.clone(), *selected),
        Row::SubsOff { selected } => ("Off".into(), *selected),
        Row::Delay(d) => (format!("Delay  ‹ {:+.1} s ›", d), false),
        Row::Size(s) => (format!("Size  ‹ {:.0} % ›", s * 100.0), false),
    }
}

impl Overlay for TracksOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let rows = self.rows(ctx.player);
        self.place_cursor(&rows);
        match action {
            Action::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                OverlayOutcome::Keep
            }
            Action::Down => {
                self.cursor = (self.cursor + 1).min(rows.len().saturating_sub(1));
                OverlayOutcome::Keep
            }
            Action::Left | Action::Right => {
                if let Some(r) = rows.get(self.cursor) {
                    self.adjust(ctx, r, action == Action::Right);
                }
                OverlayOutcome::Keep
            }
            Action::FocusNext | Action::FocusPrev => {
                self.audio = !self.audio;
                self.placed = false;
                OverlayOutcome::Keep
            }
            Action::Activate => match rows.get(self.cursor) {
                Some(r) => self.choose(ctx, r),
                None => OverlayOutcome::Close,
            },
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme: &Theme = ctx.theme;
        let t = &theme.tokens;
        let snap = ctx.player.clone();
        let rows = self.rows(&snap);
        self.place_cursor(&rows);
        let screen = egui_ctx.content_rect();
        let s = crate::ui::layout::osd_scale(screen.width());
        let row_h = (theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg)) * s;
        let pad = theme.space(SpaceToken::PopupPadding) * s;
        let width = (360.0 * theme.rem() * s).min(screen.width() - 2.0 * pad);
        let title_h = theme.font(TypeToken::Caption) * s * 2.0;
        let body_h = row_h * rows.len().max(1) as f32;
        let height = (title_h + body_h + 2.0 * pad).min(screen.height() - 2.0 * pad);
        let panel = Rect::from_min_size(
            egui::pos2(screen.right() - width - pad, screen.center().y - height / 2.0).max(screen.min),
            egui::vec2(width, height),
        );
        let mut outcome = OverlayOutcome::Keep;
        // click outside closes (no theme scrim over the video)
        egui::Area::new(egui::Id::new("tracks-catcher")).movable(false).order(egui::Order::Foreground).fixed_pos(screen.min).show(egui_ctx, |ui| {
            if ui.allocate_rect(screen, egui::Sense::click()).clicked() {
                outcome = OverlayOutcome::Close;
            }
        });
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        egui::Area::new(egui::Id::new("tracks-panel")).movable(false).order(egui::Order::Tooltip).fixed_pos(panel.min).show(egui_ctx, |ui| {
            ui.set_min_size(panel.size());
            let p = ui.painter();
            p.rect_filled(panel, t.rounding, t.osd_scrim);
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let caption = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
            let title = if self.audio { "AUDIO  ·  Tab: subtitles" } else { "SUBTITLES  ·  Tab: audio" };
            osd_text(
                p,
                egui::pos2(panel.left() + pad, panel.top() + pad),
                Align2::LEFT_TOP,
                title,
                caption,
                with_alpha(t.osd_text, 0.7),
                width - 2.0 * pad,
            );
            let top = panel.top() + pad + title_h;
            if rows.is_empty() {
                osd_text(
                    p,
                    egui::pos2(panel.left() + pad, top + row_h / 2.0),
                    Align2::LEFT_CENTER,
                    "No audio tracks",
                    font.clone(),
                    with_alpha(t.osd_text, 0.7),
                    width - 2.0 * pad,
                );
            }
            for (i, row) in rows.iter().enumerate() {
                let r = Rect::from_min_size(egui::pos2(panel.left() + pad / 2.0, top + i as f32 * row_h), egui::vec2(width - pad, row_h));
                if r.bottom() > panel.bottom() {
                    break;
                }
                let resp = ui.interact(r, ui.id().with(("track", i)), egui::Sense::click());
                let (label, selected) = row_label(row);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                if pointer_moved && resp.hovered() {
                    self.cursor = i;
                }
                let cursor = i == self.cursor;
                if cursor {
                    p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
                }
                let color = if cursor { t.osd_accent } else { t.osd_text };
                if selected {
                    osd_text(
                        p,
                        egui::pos2(r.left() + pad / 2.0, r.center().y),
                        Align2::LEFT_CENTER,
                        glyphs::CHECK,
                        font.clone(),
                        t.osd_accent,
                        f32::INFINITY,
                    );
                }
                osd_text(p, egui::pos2(r.left() + pad * 2.0, r.center().y), Align2::LEFT_CENTER, &label, font.clone(), color, r.width() - pad * 2.5);
                if resp.clicked() {
                    self.cursor = i;
                    outcome = match row {
                        Row::Delay(_) | Row::Size(_) => {
                            // click on the left/right half adjusts down/up
                            let up = resp.interact_pointer_pos().is_some_and(|p| p.x > r.center().x);
                            self.adjust(ctx, row, up);
                            OverlayOutcome::Keep
                        }
                        _ => self.choose(ctx, row),
                    };
                }
            }
        });
        outcome
    }
}
