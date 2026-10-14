//! Audio / subtitle track menu over the video (fixed dark scrim). Audio also has lip sync
//! (−/+ 50 ms) and Reset; subtitles have delay (−/+ 100 ms) and size rows. Owner: WP-11.
//!
//! A panel at the right edge of the window, vertically centred (clear of the OSD bars and the
//! controls row it is opened from), in OSD colours (`osd_scrim`, `osd_text`, `osd_accent`)
//! so it stays legible over any video and in light themes. Enter picks a track (and closes);
//! ←/→ on the delay and size rows adjust them (the menu stays open); Tab switches between the
//! audio and subtitle menus; Esc or a click outside closes. Track data comes from the player
//! snapshot every frame, so the selection mark follows mpv.
//!
//! Mouse: the AUDIO / SUBTITLES tabs switch menus, ✕ closes, a track row picks it, the ‹ ›
//! buttons of the delay and size rows step them and a click on the value resets it. A long
//! track list scrolls; the delay and size rows stay below it.

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
    LipSync(i32),
    LipSyncReset,
    Size(f64),
}

pub struct TracksOverlay {
    pub audio: bool,
    cursor: usize,
    /// The cursor was placed on the selected track once.
    placed: bool,
    /// The cursor moved by key: scroll the track list to it.
    scroll_to_cursor: bool,
}

/// What a click on the panel did.
enum Click {
    Row(usize),
    Step(usize, bool),
    Tab(bool),
    Close,
}

impl TracksOverlay {
    pub fn new(audio: bool) -> Self {
        Self { audio, cursor: 0, placed: false, scroll_to_cursor: true }
    }

    fn rows(&self, snap: &PlayerSnapshot) -> Vec<Row> {
        let mut v = Vec::new();
        if self.audio {
            for t in &snap.tracks.audio {
                v.push(Row::Track { id: t.id, label: t.label(), selected: t.selected });
            }
            if snap.cast.is_none() {
                v.push(Row::LipSync(snap.lip_sync_ms));
                v.push(Row::LipSyncReset);
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
    fn choose(&self, ctx: &mut UiCtx<'_>, row: &Row) -> OverlayOutcome {
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
            Row::LipSync(_) | Row::LipSyncReset => {
                ctx.edit_lip_sync(ctx.player.audio_output.clone(), None);
                OverlayOutcome::Keep
            }
        }
    }

    /// ←/→ on the delay / size rows.
    fn adjust(&self, ctx: &mut UiCtx<'_>, row: &Row, up: bool) {
        let sign = if up { 1.0 } else { -1.0 };
        match row {
            Row::Delay(_) => ctx.play(PlayerCommand::AdjustSubDelay(0.1 * sign)),
            Row::Size(_) => ctx.play(PlayerCommand::AdjustSubScale(0.1 * sign)),
            Row::LipSync(_) => ctx.edit_lip_sync(ctx.player.audio_output.clone(), Some(if up { 1 } else { -1 })),
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
        Row::LipSync(ms) => (format!("Lip sync: {}", crate::player::audio_output::offset_text(*ms)), false),
        Row::LipSyncReset => ("Reset lip sync".into(), false),
    }
}

/// A stepped row's name and value (drawn with ‹ › buttons around the value).
fn stepped(row: &Row) -> Option<(&'static str, String)> {
    match row {
        Row::Delay(d) => Some(("Delay", format!("{:+.1} s", d))),
        Row::Size(s) => Some(("Size", format!("{:.0} %", s * 100.0))),
        Row::LipSync(ms) => Some(("Lip sync", format!("{ms:+} ms"))),
        _ => None,
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
                self.scroll_to_cursor = true;
                OverlayOutcome::Keep
            }
            Action::Down => {
                self.cursor = (self.cursor + 1).min(rows.len().saturating_sub(1));
                self.scroll_to_cursor = true;
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
        // a click outside (either button) closes (no theme scrim over the video)
        egui::Area::new(egui::Id::new("tracks-catcher")).movable(false).order(egui::Order::Foreground).fixed_pos(screen.min).show(egui_ctx, |ui| {
            let r = ui.allocate_rect(screen, egui::Sense::click());
            if r.clicked() || r.secondary_clicked() {
                outcome = OverlayOutcome::Close;
            }
        });
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let look = Look {
            theme,
            font: FontId::proportional((theme.font(TypeToken::Title) * s).round()),
            caption: FontId::proportional((theme.font(TypeToken::Caption) * s).round()),
            pad,
            row_h,
            value_w: 80.0 * theme.rem() * s,
        };
        let fixed = rows.iter().filter(|r| stepped(r).is_some() || matches!(r, Row::LipSyncReset)).count();
        let mut click = None;
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let audio = self.audio;
        let cursor = &mut self.cursor;
        egui::Area::new(egui::Id::new("tracks-panel")).movable(false).order(egui::Order::Tooltip).fixed_pos(panel.min).show(egui_ctx, |ui| {
            ui.set_min_size(panel.size());
            ui.set_max_size(panel.size());
            ui.painter().rect_filled(panel, t.rounding, t.osd_scrim);
            let inner = panel.shrink2(egui::vec2(pad / 2.0, pad));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            if let Some(c) = look.header(&mut ui, audio, title_h) {
                click = Some(c);
            }
            if rows.is_empty() {
                let (r, _) = ui.allocate_exact_size(egui::vec2(inner.width(), row_h), egui::Sense::hover());
                let muted = with_alpha(t.osd_text, 0.7);
                osd_text(
                    ui.painter(),
                    r.left_center() + egui::vec2(pad / 2.0, 0.0),
                    Align2::LEFT_CENTER,
                    "No audio tracks",
                    look.font.clone(),
                    muted,
                    r.width() - pad,
                );
            }
            // the track rows scroll; the delay and size rows stay below them
            let list_h = (ui.available_height() - fixed as f32 * row_h).max(row_h);
            let split = rows.len() - fixed;
            let mut row = |ui: &mut egui::Ui, i: usize| {
                if pointer_moved && ui.rect_contains_pointer(egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), row_h))) {
                    *cursor = i;
                }
                if let Some(c) = look.row(ui, i, &rows[i], i == *cursor, scroll) {
                    click = Some(c);
                }
            };
            egui::ScrollArea::vertical().id_salt("tracks-list").max_height(list_h).auto_shrink([false, true]).show(&mut ui, |ui| {
                for i in 0..split {
                    row(ui, i);
                }
            });
            for i in split..rows.len() {
                row(&mut ui, i);
            }
        });
        match click {
            Some(Click::Row(i)) => {
                self.cursor = i;
                outcome = self.choose(ctx, &rows[i]);
            }
            Some(Click::Step(i, up)) => {
                self.cursor = i;
                self.adjust(ctx, &rows[i], up);
                outcome = OverlayOutcome::Keep;
            }
            Some(Click::Tab(audio)) => {
                if audio != self.audio {
                    self.audio = audio;
                    self.placed = false;
                    self.scroll_to_cursor = true;
                }
                outcome = OverlayOutcome::Keep;
            }
            Some(Click::Close) => outcome = OverlayOutcome::Close,
            None => {}
        }
        outcome
    }
}

/// Sizes and fonts of the panel at this OSD scale (drawing helpers for its parts).
struct Look<'a> {
    theme: &'a Theme,
    font: FontId,
    caption: FontId,
    pad: f32,
    row_h: f32,
    value_w: f32,
}

impl Look<'_> {
    /// A clickable OSD target: hover fill and the pointing hand; returns the response.
    fn target(&self, ui: &egui::Ui, rect: Rect, id: impl std::hash::Hash + std::fmt::Debug, label: &str, selected: Option<bool>) -> egui::Response {
        let t = &self.theme.tokens;
        let resp = ui.interact(rect, ui.id().with(id), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.widget_info(|| match selected {
            Some(on) => egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, label),
            None => egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label),
        });
        if resp.hovered() {
            ui.painter().rect_filled(rect, t.rounding, with_alpha(t.osd_text, 0.12));
        }
        resp
    }

    /// `AUDIO  SUBTITLES                ✕`: the menu tabs and the close button.
    fn header(&self, ui: &mut egui::Ui, audio: bool, h: f32) -> Option<Click> {
        let t = &self.theme.tokens;
        let (head, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let mut click = None;
        let mut x = head.left() + self.pad / 2.0;
        for (tab, label) in [(true, "AUDIO"), (false, "SUBTITLES")] {
            let color = if tab == audio { t.osd_accent } else { with_alpha(t.osd_text, 0.7) };
            let g = ui.painter().layout_no_wrap(label.into(), self.caption.clone(), color);
            let r = Rect::from_min_size(egui::pos2(x - self.pad / 4.0, head.top()), egui::vec2(g.size().x + self.pad / 2.0, head.height()));
            if self.target(ui, r, ("tab", tab), label, Some(tab == audio)).on_hover_text("Tab switches").clicked() {
                click = Some(Click::Tab(tab));
            }
            // the open tab is underlined, so the two read as tabs, not as a title
            if tab == audio {
                let y = head.bottom() - 1.0;
                ui.painter().line_segment([egui::pos2(x, y), egui::pos2(x + g.size().x, y)], egui::Stroke::new(2.0, t.osd_accent));
            }
            ui.painter().galley(egui::pos2(x, head.center().y - g.size().y / 2.0), g, color);
            x = r.right() + self.pad / 2.0;
        }
        let close = Rect::from_min_size(egui::pos2(head.right() - head.height(), head.top()), egui::vec2(head.height(), head.height()));
        if self.target(ui, close, "close", "Close (Esc)", None).on_hover_text("Close (Esc)").clicked() {
            click = Some(Click::Close);
        }
        osd_text(ui.painter(), close.center(), Align2::CENTER_CENTER, glyphs::CLOSE, self.font.clone(), t.osd_text, f32::INFINITY);
        click
    }

    /// One row: a track (click picks it) or a stepped value (`Delay   [‹] +0.1 s [›]`).
    fn row(&self, ui: &mut egui::Ui, i: usize, row: &Row, cursor: bool, scroll: bool) -> Option<Click> {
        let t = &self.theme.tokens;
        let pad = self.pad;
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), self.row_h), egui::Sense::hover());
        // the whole row's name ("Delay  ‹ +0.1 s ›"; a track row is named by its click target)
        if stepped(row).is_some() {
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, row_label(row).0));
        }
        if scroll && cursor {
            ui.scroll_to_rect(r, None);
        }
        if cursor {
            ui.painter().rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
        }
        let color = if cursor { t.osd_accent } else { t.osd_text };
        let Some((name, value)) = stepped(row) else {
            let (label, selected) = row_label(row);
            let clicked = self.target(ui, r, ("track", i), &label, None).clicked();
            if selected {
                osd_text(
                    ui.painter(),
                    egui::pos2(r.left() + pad / 2.0, r.center().y),
                    Align2::LEFT_CENTER,
                    glyphs::CHECK,
                    self.font.clone(),
                    t.osd_accent,
                    f32::INFINITY,
                );
            }
            osd_text(
                ui.painter(),
                egui::pos2(r.left() + pad * 2.0, r.center().y),
                Align2::LEFT_CENTER,
                &label,
                self.font.clone(),
                color,
                r.width() - pad * 2.5,
            );
            return clicked.then_some(Click::Row(i));
        };
        osd_text(ui.painter(), egui::pos2(r.left() + pad * 2.0, r.center().y), Align2::LEFT_CENTER, name, self.font.clone(), color, r.width() / 2.0);
        let side = r.height();
        let more = Rect::from_min_size(egui::pos2(r.right() - side, r.top()), egui::vec2(side, side));
        let val = Rect::from_min_size(egui::pos2(more.left() - self.value_w, r.top()), egui::vec2(self.value_w, side));
        let less = Rect::from_min_size(egui::pos2(val.left() - side, r.top()), egui::vec2(side, side));
        let delay = matches!(row, Row::Delay(_) | Row::LipSync(_));
        let mut click = None;
        for (slot, (rect, text, tip, step)) in [
            (less, glyphs::CHEVRON_LEFT, if delay { "Earlier (←)" } else { "Smaller (←)" }, Some(false)),
            (val, value.as_str(), "Reset (Enter)", None),
            (more, glyphs::CHEVRON_RIGHT, if delay { "Later (→)" } else { "Larger (→)" }, Some(true)),
        ]
        .into_iter()
        .enumerate()
        {
            if self.target(ui, rect, ("step", i, slot), &format!("{name}: {tip}"), None).on_hover_text(tip).clicked() {
                click = Some(step.map_or(Click::Row(i), |up| Click::Step(i, up)));
            }
            osd_text(ui.painter(), rect.center(), Align2::CENTER_CENTER, text, self.font.clone(), color, rect.width());
        }
        click
    }
}
