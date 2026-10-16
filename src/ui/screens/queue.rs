//! Up next (Alt+6, the sidebar's "Up next n"; DESIGN §32 §2.2–2.3). OWNER: Q-ui.
//!
//! The list (`queue::ui::list`): "Played (n)", "Now", "Next" with ETAs and the sleep marker;
//! a detail pane for the cursor row at Wide; Compact (under 700 pt) drops the ETA column and
//! folds "Played" into one line. Above it, the card the player shows between items (the
//! countdown, a PIN wait, the end) as a banner with the same buttons, and "Carry on" after Up
//! next stepped aside.
//!
//! Keys (`[List|ListFiltering]` + item actions): Enter plays from here (what plays → the
//! player), Shift+Enter fullscreen, Alt+↑/↓ move (not while filtering), Ctrl+Shift+N play next,
//! Space / Insert mark, Delete removes (Undo in the toast), Ctrl+O pins a copy, Ctrl+I opens
//! the title, Ctrl+Enter / right-click the row menu, letters filter. Mouse: drag a row by its
//! handle, ✕ on hover removes, a click plays (as Enter).

use std::sync::Arc;

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, QueueId};
use crate::msg::Toast;
use crate::queue::ui::card::{Card, Step};
use crate::queue::ui::list::{Look, Mode, Out, QueueList};
use crate::queue::{Insert, QueueCommand, QueueSnapshot, RowState};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::{empty_state, glyphs, section, text_field};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};

/// The row menu's choices (`Action::Digit` codes; copies start at [`COPY`]).
mod code {
    pub const PLAY: u8 = 200;
    pub const FROM_START: u8 = 201;
    pub const NEXT: u8 = 202;
    pub const TOP: u8 = 203;
    pub const BOTTOM: u8 = 204;
    pub const STOP_AFTER: u8 = 205;
    pub const COPY_MENU: u8 = 206;
    pub const REMOVE: u8 = 207;
    pub const REMOVE_PLAYED: u8 = 208;
    pub const SHUFFLE: u8 = 209;
    pub const CLEAR: u8 = 210;
    pub const CARRY_ON: u8 = 211;
    pub const SAVE_LIST: u8 = 212;
}
/// "Use another copy…": copy `k` is `COPY + k`.
const COPY: u8 = 100;
/// The "Clear Up next…" confirmation.
const CLEAR_TOKEN: u64 = 0x5155_4555;
/// The detail pane's width at Wide.
const PANE_W: f32 = 360.0;

#[derive(Default)]
pub struct QueueScreen {
    list: QueueList,
    /// The row the menu or a copy choice is about.
    menu_row: Option<QueueId>,
    /// The copies the Ctrl+O menu listed.
    copies: Vec<ItemRef>,
}

impl QueueScreen {
    fn snap(&self) -> Arc<QueueSnapshot> {
        self.list.snapshot().clone()
    }

    /// Play a row: what plays now opens the player; another row plays from here, behind the
    /// Resume prompt when it has progress (`queue::ui::play_row`).
    fn play(&mut self, ctx: &mut UiCtx<'_>, id: QueueId, fullscreen: bool) {
        let s = self.snap();
        if s.driving && s.current == Some(id) {
            ctx.push(Route::Player);
            return;
        }
        if crate::queue::ui::play_row(ctx, &s, id, fullscreen) == crate::queue::ui::RowPlay::Sent && fullscreen {
            ctx.push(Route::Player);
        }
    }

    fn sends(ctx: &mut UiCtx<'_>, cmds: Vec<QueueCommand>) {
        for c in cmds {
            if !crate::queue::ui::send(ctx, c) {
                break;
            }
        }
    }

    fn out(&mut self, ctx: &mut UiCtx<'_>, out: Out) -> bool {
        match out {
            Out::None => false,
            Out::Handled | Out::Parent => true,
            Out::Play { id, fullscreen } => {
                self.play(ctx, id, fullscreen);
                true
            }
            Out::Menu(id) => {
                self.menu(ctx, id);
                true
            }
            Out::PlayList { list, entry } => {
                crate::ui::watchlists::play(ctx, list, Some(entry), false);
                true
            }
            Out::PreviewMenu(entry) => {
                self.list.open_preview_menu(ctx, entry);
                true
            }
            Out::OpenList(list) => {
                ctx.push(Route::Watchlists { list: Some(list) });
                true
            }
            Out::Send(cmds) => {
                Self::sends(ctx, cmds);
                true
            }
            Out::Remove(ids) => {
                let s = self.snap();
                crate::queue::ui::remove(ctx, &s, ids);
                true
            }
            Out::Toast(t) => {
                ctx.toast(Toast::info(t));
                true
            }
        }
    }

    /// The row menu (Ctrl+Enter, Shift+F10, right-click).
    fn menu(&mut self, ctx: &mut UiCtx<'_>, id: QueueId) {
        let s = self.snap();
        let Some(row) = s.row(id) else { return };
        let e = &row.entry;
        let k = self.list.list.key_context();
        let mi =
            |icon: &'static str, label: &str, c: u8| MenuItem { icon, label: label.into(), action: Action::Digit(c), enabled: true, shortcut: None };
        let key = |icon: &'static str, label: &str, a: Action| crate::ui::screens::live::menu_item(icon, label, a, true, k);
        let pending = e.state == RowState::Pending;
        let mut items = vec![mi(glyphs::PLAY, "Play now", code::PLAY)];
        if s.interrupted && s.current.is_some() {
            items.push(mi(glyphs::PLAY, "Carry on with Up next", code::CARRY_ON));
        }
        if e.item.kind != ItemKind::Channel || e.programme.as_ref().is_some_and(|p| p.catchup_id.is_some()) {
            let label = if e.from_start { format!("{}  Play from the start", glyphs::CHECK) } else { "Play from the start".to_owned() };
            items.push(mi(glyphs::SKIP_BACK, &label, code::FROM_START));
        }
        if pending {
            items.push(MenuItem { action: Action::Digit(code::NEXT), ..key(glyphs::NEXT, "Play next", Action::QueuePlayNext) });
            items.push(mi(glyphs::REORDER, "Move to the top", code::TOP));
            items.push(mi(glyphs::REORDER, "Move to the bottom", code::BOTTOM));
        }
        if !e.state.is_done() {
            let label = if s.stop_after == Some(id) { format!("{}  Stop after this one", glyphs::CHECK) } else { "Stop after this one".to_owned() };
            items.push(mi(glyphs::STOP, &label, code::STOP_AFTER));
        }
        if e.item.kind != ItemKind::Channel {
            items.push(MenuItem { action: Action::Digit(code::COPY_MENU), ..key(glyphs::PIN, "Use another copy…", Action::PlayFrom) });
        }
        let marked = self.list.marks.len();
        let remove = if marked > 1 { format!("Remove {marked} marked") } else { "Remove".to_owned() };
        items.push(MenuItem { action: Action::Digit(code::REMOVE), ..key(glyphs::CLOSE, &remove, Action::Remove) });
        if s.rows.iter().any(|r| r.entry.state.is_done()) {
            items.push(mi(glyphs::CHECK, "Remove played", code::REMOVE_PLAYED));
        }
        if s.rows.iter().filter(|r| r.entry.state == RowState::Pending).count() > 2 {
            items.push(mi(glyphs::SYNC, "Shuffle the rest", code::SHUFFLE));
        }
        items.push(mi(glyphs::FAV_LIST, "Save as a watchlist…", code::SAVE_LIST));
        items.push(mi(glyphs::CLOSE, "Clear Up next…", code::CLEAR));
        self.menu_row = Some(id);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: crate::queue::ui::row_title(e), items }));
    }

    /// A menu choice about `id` (or the marked rows).
    fn choose(&mut self, ctx: &mut UiCtx<'_>, c: u8) {
        let s = self.snap();
        let Some(id) = self.menu_row.or(self.list.cursor_id()) else { return };
        let Some(row) = s.row(id) else { return };
        let ids = if self.list.marks.contains(&id) { self.list.targets() } else { vec![id] };
        let cmd = match c {
            code::SAVE_LIST => {
                let mut items = Vec::new();
                let mut omitted = (0, 0);
                for row in s.rows.iter().filter(|r| r.entry.state == crate::queue::RowState::Pending) {
                    if row.entry.programme.is_some() || row.entry.item.kind == crate::model::ItemKind::Channel {
                        omitted.0 += 1;
                    } else if row.entry.orphaned {
                        omitted.1 += 1;
                    } else {
                        items.push(row.entry.item.clone());
                    }
                }
                ctx.open(OverlayRequest::WatchlistPicker(crate::ui::overlays::watchlists::PickerSpec {
                    omitted,
                    items,
                    title: "Save Up next".into(),
                    edit: None,
                    create: true,
                }));
                return;
            }
            code::PLAY => return self.play(ctx, id, false),
            code::CARRY_ON => QueueCommand::Resume,
            code::FROM_START => QueueCommand::FromStart(id, !row.entry.from_start),
            code::NEXT => QueueCommand::Move { ids, to: Insert::Next },
            code::TOP => QueueCommand::Move { ids, to: Insert::Index(0) },
            code::BOTTOM => QueueCommand::Move { ids, to: Insert::End },
            code::STOP_AFTER => QueueCommand::StopAfter((s.stop_after != Some(id)).then_some(id)),
            code::COPY_MENU => return self.copy_menu(ctx, id),
            code::REMOVE => {
                self.list.marks.clear();
                return crate::queue::ui::remove(ctx, &s, ids);
            }
            code::REMOVE_PLAYED => QueueCommand::Clear { played_only: true },
            code::SHUFFLE => return Self::sends(ctx, shuffled(&s)),
            code::CLEAR => {
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: "Clear Up next?".into(),
                    body: "Everything in Up next goes, played or not.".into(),
                    confirm_label: "Clear".into(),
                    destructive: true,
                    token: CLEAR_TOKEN,
                }));
                return;
            }
            _ => return,
        };
        if matches!(cmd, QueueCommand::Move { .. }) {
            self.list.marks.clear();
        }
        crate::queue::ui::send(ctx, cmd);
    }

    /// Ctrl+O: the row's copies; the choice pins it ("Use another copy…").
    fn copy_menu(&mut self, ctx: &mut UiCtx<'_>, id: QueueId) {
        let s = self.snap();
        let Some(row) = s.row(id) else { return };
        match crate::queue::ui::copy_choices(ctx, &row.entry, COPY) {
            Ok((items, copies)) => {
                self.copies = copies;
                self.menu_row = Some(id);
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Use another copy".into(), items }));
            }
            Err(why) => ctx.toast(Toast::info(why)),
        }
    }

    /// Ctrl+I: the title's page (a programme: the guide at its channel).
    fn details(&self, ctx: &mut UiCtx<'_>) {
        let Some(row) = self.list.cursor_row() else { return };
        let item = &row.entry.item;
        match item.kind {
            ItemKind::Movie => {
                if let Some(id) = crate::store::vod::resolve_movie(ctx.db, item).ok().flatten() {
                    ctx.push(Route::MovieDetail(id));
                }
            }
            ItemKind::Episode | ItemKind::Series => {
                let ep = crate::store::vod::resolve_episode(ctx.db, item).ok().flatten().and_then(|id| crate::store::vod::episode(ctx.db, id).ok());
                if let Some(e) = ep {
                    ctx.push(Route::SeriesDetail(e.series_id));
                }
            }
            ItemKind::Channel => {
                if let Some(ch) = ctx.lib.live.by_ref(item) {
                    ctx.push(Route::Guide { collection: None, focus: Some(ch) });
                }
            }
        }
    }

    /// The card banner (same card and buttons as the player's); the PIN comes back as
    /// `Unlocked(Digit(PIN_CODE))`.
    fn banner(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, s: &QueueSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let h24 = ctx.config.interface.clock_24h;
        let card = Card::of(s, ctx.now);
        let stepped_aside = card.is_none() && s.interrupted && s.current.is_some();
        if card.is_none() && !stepped_aside {
            return;
        }
        let (head, line, buttons) = match &card {
            Some(c) => {
                let (h, l) = c.lines(ctx.now, h24);
                (h, l, c.buttons().into_iter().filter(|b| *b != crate::queue::ui::card::Button::List).map(Some).collect::<Vec<_>>())
            }
            None => (
                "Up next stepped aside".to_owned(),
                s.current.and_then(|c| s.row(c)).map(|r| format!("It carries on with {}", crate::queue::ui::row_title(&r.entry))),
                vec![None],
            ),
        };
        let mut pressed = None;
        egui::Frame::NONE
            .fill(t.card_bg)
            .stroke(egui::Stroke::new(1.0, t.popup_border))
            .corner_radius(t.rounding)
            .inner_margin(theme.space(SpaceToken::Lg))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&head).size(theme.font(TypeToken::Title)).color(t.accent));
                        if let Some(l) = &line {
                            ui.add(egui::Label::new(egui::RichText::new(l).size(theme.font(TypeToken::Body)).color(t.fg)).truncate());
                        }
                    });
                    for b in &buttons {
                        let (icon, label) = match b {
                            Some(b) => {
                                let (g, l, _) = b.parts();
                                (g, l)
                            }
                            None => (glyphs::PLAY, "Carry on"),
                        };
                        if button(ui, theme, Some(icon), label, ButtonStyle { bordered: true, ..Default::default() }).clicked() {
                            pressed = Some(*b);
                        }
                    }
                });
            });
        ui.add_space(theme.space(SpaceToken::Md));
        if card.as_ref().is_some_and(Card::counts_down) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        match (pressed, card) {
            (Some(Some(b)), Some(c)) => self.step(ctx, c.press(b)),
            (Some(None), _) => {
                crate::queue::ui::send(ctx, QueueCommand::Resume);
            }
            _ => {}
        }
    }

    fn step(&mut self, ctx: &mut UiCtx<'_>, step: Step) {
        match step {
            Step::Send(c) => {
                crate::queue::ui::send(ctx, c);
            }
            Step::Pin(reason) => {
                ctx.open(OverlayRequest::Pin { then: Box::new(Action::Digit(crate::queue::ui::PIN_CODE)), reason });
            }
            Step::List => {}
            Step::PlayEpisode(id) => {
                crate::ui::screens::home::kit::play_media(ctx, crate::model::MediaRef::Episode(id), true, crate::model::PlayOrigin::Fullscreen);
                crate::queue::ui::send(ctx, QueueCommand::Cancel);
            }
        }
    }

    /// The detail pane (Wide): the cursor row's title, where it plays from and how it starts.
    fn pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(row) = self.list.cursor_row().cloned() else { return };
        let e = &row.entry;
        let s = self.snap();
        let h24 = ctx.config.interface.clock_24h;
        ui.add(egui::Label::new(egui::RichText::new(&e.title).size(theme.font(TypeToken::Title)).color(t.fg)).wrap());
        if let Some(sub) = &e.subtitle {
            ui.label(egui::RichText::new(sub).size(theme.font(TypeToken::Body)).color(t.text_secondary));
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let playlist = ctx.lib.sources.iter().find(|x| x.id == e.item.source).map(|x| x.name.clone()).unwrap_or_default();
        let plays = if e.programme.is_some() {
            playlist
        } else if e.any_copy {
            "The best copy when its turn comes (Ctrl+O chooses)".to_owned()
        } else {
            format!("{playlist} (chosen)")
        };
        let state = match e.state {
            RowState::Current if s.driving => "Playing now",
            RowState::Current => "Where Up next stopped",
            RowState::Pending => "Still to play",
            RowState::Played => "Played",
            RowState::Skipped => "Skipped",
            RowState::Failed => "Couldn't play",
        };
        let mut facts = vec![
            ("Length", crate::queue::ui::duration(e.duration_s)),
            ("Plays from", plays),
            ("Starts", if e.from_start { "From the start".into() } else { "Where you left off".into() }),
            ("State", row.note.clone().map_or_else(|| state.to_owned(), |n| format!("{state} · {n}"))),
        ];
        if let Some(at) = row.eta {
            facts.push(("Expected", crate::queue::ui::eta(at, h24)));
        }
        if row.locked {
            facts.push(("Lock", "Needs the PIN when its turn comes".into()));
        }
        egui::Grid::new("queue-facts").num_columns(2).spacing([theme.space(SpaceToken::Lg), theme.space(SpaceToken::Sm)]).show(ui, |ui| {
            for (k, v) in facts {
                ui.label(egui::RichText::new(k).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                ui.add(egui::Label::new(egui::RichText::new(v).size(theme.font(TypeToken::Body)).color(t.fg)).wrap());
                ui.end_row();
            }
        });
        ui.add_space(theme.space(SpaceToken::Lg));
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            let b = ButtonStyle { bordered: true, ..Default::default() };
            if button(ui, theme, Some(glyphs::PLAY), "Play now", ButtonStyle { primary: true, ..b }).clicked() {
                chosen = Some(code::PLAY);
            }
            if e.state == RowState::Pending && button(ui, theme, Some(glyphs::NEXT), "Play next", b).clicked() {
                chosen = Some(code::NEXT);
            }
            if button(ui, theme, Some(glyphs::CLOSE), "Remove", ButtonStyle { danger: true, ..b }).clicked() {
                chosen = Some(code::REMOVE);
            }
        });
        if let Some(c) = chosen {
            self.menu_row = Some(e.id);
            self.choose(ctx, c);
            self.menu_row = None;
        }
    }
}

/// "Shuffle the rest": the pending rows in a random order, in their places (one write).
fn shuffled(s: &QueueSnapshot) -> Vec<QueueCommand> {
    let mut ids: Vec<QueueId> = s.rows.iter().filter(|r| r.entry.state == RowState::Pending).map(|r| r.entry.id).collect();
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
    for i in (1..ids.len()).rev() {
        // xorshift: plenty for an order nobody chose
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ids.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    vec![QueueCommand::Reorder(ids)]
}

impl Screen for QueueScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Queue
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.list.key_context()]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, _route: &Route) {
        let s = crate::queue::ui::snapshot(ctx);
        self.list.sync(s.clone(), self.mode(ctx), None);
        // the cursor on what plays (or plays next)
        if let Some(id) = s.current.or(s.next) {
            self.list.select(id);
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let s = crate::queue::ui::snapshot(ctx);
        self.list.sync(s, self.mode(ctx), crate::queue::ui::sleep_mark(ctx));
        match action {
            Action::Back if self.list.cancel_drag() => return true,
            Action::Undo(n) if crate::queue::ui::is_undo(n) => return crate::queue::ui::undo(ctx, n),
            Action::PlayFrom => {
                if let Some(id) = self.list.cursor_id() {
                    self.copy_menu(ctx, id);
                }
                return true;
            }
            Action::ShowDetails => {
                self.details(ctx);
                return true;
            }
            _ => {}
        }
        self.menu_row = None;
        let out = self.list.handle(action);
        self.out(ctx, out)
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        if let OverlayResult::Chosen(action) = &result
            && let Some(out) = self.list.preview_choice(ctx, *action)
        {
            self.out(ctx, out);
            return;
        }
        match result {
            OverlayResult::Chosen(Action::Digit(d)) if (COPY..code::PLAY).contains(&d) => {
                if let (Some(id), Some(copy)) = (self.menu_row, self.copies.get(usize::from(d - COPY)).cloned()) {
                    crate::queue::ui::send(ctx, QueueCommand::PinCopy(id, copy));
                }
                self.copies.clear();
            }
            OverlayResult::Chosen(Action::Digit(d)) if d >= code::PLAY && d != crate::queue::ui::PIN_CODE => self.choose(ctx, d),
            OverlayResult::Unlocked(Action::Digit(crate::queue::ui::PIN_CODE)) => {
                // the engine re-reads the parental state at play time: publish the unlock first
                ctx.services.agent.set_unlocked_at(ctx.parental.unlocked_at());
                crate::queue::ui::send(ctx, QueueCommand::Proceed { unlocked: true });
            }
            OverlayResult::Confirmed(CLEAR_TOKEN) => {
                crate::queue::ui::send(ctx, QueueCommand::Clear { played_only: false });
            }
            _ => {}
        }
    }

    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        vec![(Action::MoveUp, "move"), (Action::Remove, "remove"), (Action::PlayFrom, "copy")]
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let s = crate::queue::ui::snapshot(ctx);
        let mode = self.mode(ctx);
        self.list.sync(s.clone(), mode, crate::queue::ui::sleep_mark(ctx));
        let h24 = ctx.config.interface.clock_24h;
        let summary = crate::queue::ui::summary(&s, ctx.now, h24);
        let title = s.watchlist.as_ref().map_or_else(|| "Up next".to_owned(), |w| format!("Up next · {}", w.name));
        section::page_header(ui, ctx, &title, |ui| {
            if let Some(text) = &summary {
                ui.label(egui::RichText::new(text).size(theme.font(TypeToken::Body)).color(theme.tokens.text_secondary));
            }
        });
        ui.add_space(theme.space(SpaceToken::Md));
        self.banner(ui, ctx, &s);
        if s.rows.is_empty() && s.watchlist.is_none() {
            empty_state::empty_state(
                ui,
                theme,
                glyphs::UP_NEXT,
                "Up next is empty",
                Some("Ctrl+N on a film, an episode or a programme adds it; Ctrl+Shift+N plays it next."),
                None,
            );
            return;
        }
        let q = self.list.list.filter.query().to_owned();
        if !q.is_empty() {
            text_field::search_header(ui, theme, &q, "type to filter", Some(glyphs::SEARCH));
        }
        let row_h = crate::ui::widgets::rows::row_height(theme, ctx.config.interface.density, false);
        let rect = ui.available_rect_before_wrap();
        let wide = ctx.layout == LayoutClass::Wide && rect.width() >= 2.0 * PANE_W * theme.rem();
        let pane_w = if wide { PANE_W * theme.rem() } else { 0.0 };
        let gap = theme.space(SpaceToken::PanelGap);
        let list_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - if wide { pane_w + gap } else { 0.0 }, rect.bottom()));
        let out = ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| self.list.ui(ui, ctx, &Look::theme(theme), row_h)).inner;
        if wide {
            let pane = egui::Rect::from_min_max(egui::pos2(list_rect.right() + gap, rect.top()), rect.max);
            section::pane_divider(ui.painter(), list_rect.right() + gap / 2.0, rect.y_range(), theme);
            ui.scope_builder(egui::UiBuilder::new().max_rect(pane.shrink2(egui::vec2(theme.space(SpaceToken::Lg), 0.0))), |ui| self.pane(ui, ctx));
        }
        ui.advance_cursor_after_rect(rect);
        self.out(ctx, out);
        if s.driving {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}

impl QueueScreen {
    fn mode(&self, ctx: &UiCtx<'_>) -> Mode {
        Mode::Screen { compact: ctx.layout == LayoutClass::Compact }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::queue::ui::tests::entry;
    use crate::queue::{Countdown, CountdownWhy, QueueRow};
    use crate::ui::screens::home::testkit::{harness, has_label, painted, rig};
    use egui_kittest::kittest::Queryable;

    fn publish(t: &crate::ui::testing::TestUi, rows: &[(i64, RowState)], f: impl FnOnce(&mut QueueSnapshot)) {
        let rows: Vec<QueueRow> = rows.iter().map(|(id, s)| QueueRow { entry: entry(*id, *s), locked: false, eta: None, note: None }).collect();
        let mut s = QueueSnapshot { rows: rows.into(), ..Default::default() };
        f(&mut s);
        t.services().agent.publish_queue(Arc::new(s));
    }

    /// Full screen (1600 pt, with the detail pane), the tiled half (790 pt) and the half with
    /// 16 pt text (Compact): the sections, the countdown banner with its buttons and the rows;
    /// Compact folds "Played" into one line.
    #[test]
    fn the_screen_lists_sections_and_the_card_at_every_width() {
        for (w, scale) in [(1600.0, 1.0), (790.0, 1.0), (790.0, 16.0 / 12.0)] {
            let (mut t, one) = rig();
            crate::ui::screens::home::testkit::seed(&mut t, |c| {
                let source = crate::ui::screens::home::testkit::source(c, "TV")?;
                crate::ui::screens::home::testkit::movies(c, source, 3)?;
                c.execute("UPDATE movie SET key='m' || id", [])?;
                Ok(())
            });
            let base = crate::theme::Theme::fallback();
            t.theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            let layout = LayoutClass::from_width(w, t.theme.rem());
            t.layout = layout;
            let size = egui::vec2(w, 700.0);
            publish(&t, &[(1, RowState::Played), (2, RowState::Current), (3, RowState::Pending)], |s| {
                s.current = Some(QueueId(2));
                s.countdown = Some(Countdown { id: Some(QueueId(3)), at: Some(crate::model::now() + 9), why: CountdownWhy::Ended });
            });
            let mut h = harness(t, one, QueueScreen::default(), size);
            h.state_mut().enter(Route::Queue);
            h.run_steps(3);
            let text = painted(&h);
            assert!(text.iter().any(|x| x == "Now") && text.iter().any(|x| x == "Next"), "{text:?}");
            if layout == LayoutClass::Compact {
                assert!(text.iter().any(|x| x == "✓ 1 played"), "{text:?}");
            } else {
                assert!(text.iter().any(|x| x == "Played (1)"), "{text:?}");
            }
            assert!(has_label(&h, "Cancel"), "the card's buttons");
            assert!(h.get_all_by_label("Movie 3").next().is_some());
            // the detail pane only at full width: its facts about the cursor row
            assert_eq!(text.iter().any(|x| x == "Plays from"), layout == LayoutClass::Wide, "{w} × {scale}");
        }
    }

    /// The main picture's commands (nothing plays in these tests).
    #[derive(Default)]
    pub(crate) struct Rec(pub(crate) std::sync::Mutex<Vec<crate::player::PlayerCommand>>);
    impl crate::queue::PlayerPort for Rec {
        fn send(&self, c: crate::player::PlayerCommand) {
            self.0.lock().unwrap().push(c);
        }
    }

    /// The real engine on the rig's store, library and shared state.
    pub(crate) fn start_engine(t: &crate::ui::testing::TestUi) -> Arc<Rec> {
        let s = t.services();
        let _ = s.agent.publish_library(&t.lib);
        let rec = Arc::new(Rec::default());
        let deps = crate::queue::EngineDeps {
            store: s.store.clone(),
            config: s.config.clone(),
            player: rec.clone(),
            bus: s.bus.clone(),
            jobs: Some(s.jobs.clone()),
        };
        s.agent.set_queue_handle(Some(crate::queue::service::spawn(s.rt.handle(), s.agent.clone(), deps)));
        rec
    }

    type H = egui_kittest::Harness<'static, crate::ui::screens::home::testkit::App<QueueScreen>>;

    pub(crate) fn seed_watchlist(
        t: &mut crate::ui::testing::TestUi,
        name: &'static str,
        order: crate::watchlist::PlayOrder,
        count: usize,
    ) -> (crate::model::WatchlistId, Vec<crate::model::WatchlistItemId>) {
        use crate::ui::screens::home::testkit::{movies, seed, source};
        seed(t, move |c| {
            let src = source(c, "TV")?;
            movies(c, src, count)?;
            let lib = crate::store::library::load(c)?;
            let list = crate::store::watchlist::create(c, name, None, order, "app", 1)?;
            let entries = (0..count)
                .map(|n| {
                    crate::store::watchlist::new_entry(
                        c,
                        &ItemRef { source: src, kind: ItemKind::Movie, key: crate::model::ItemKey(format!("m3u:movie{n:010}")) },
                    )
                })
                .collect::<crate::error::Result<Vec<_>>>()?;
            let added = crate::store::watchlist::add(c, &lib, list, &entries, crate::watchlist::Insert::End, 1)?;
            if let Some(first) = added.added.first() {
                crate::store::watchlist::make_next(c, list, *first)?;
            }
            Ok((list, added.added))
        })
    }

    #[test]
    fn watchlist_previews_on_the_screen_play_skip_open_and_stop() {
        use crate::ui::screens::home::testkit::until;
        let (mut t, one) = rig();
        let (list, entries) = seed_watchlist(&mut t, "Star Wars", crate::watchlist::PlayOrder::Ordered, 3);
        start_engine(&t);
        let mut h = harness(t, one, QueueScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Queue);
        h.state_mut().t.with_ctx(|ctx| crate::queue::ui::send(ctx, QueueCommand::PlayList { list, from: None, just_one: false }));
        until(&mut h, "watchlist previews", |h| has_label(h, "Then from Star Wars · 1 of 3") && has_label(h, "Movie 2"));
        assert!(has_label(&h, "Movie 0 (1980)") && has_label(&h, "Movie 1"));
        assert!(painted(&h).iter().any(|t| t == "Up next · Star Wars"));
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let Some(crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(menu))) = nav.last() else { panic!("{nav:?}") };
        assert_eq!(menu.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Play from here", "Skip in this list", "Open the list"]);
        h.state_mut().result(OverlayResult::Chosen(menu.items[1].action));
        h.state().t.services().store.writer().blocking(|_| Ok(())).unwrap();
        assert!(crate::store::watchlist::entries(&h.state().t.db, list).unwrap()[1].skipped);
        // Deliver the app's refresh after the watchlist write.
        h.state_mut().t.with_ctx(|ctx| crate::queue::ui::send(ctx, QueueCommand::RefreshLists));
        until(&mut h, "skipped preview gone", |h| !has_label(h, "Movie 1"));
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let Some(crate::ui::NavRequest::Open(OverlayRequest::ContextMenu(menu))) = nav.last() else { panic!("{nav:?}") };
        h.state_mut().result(OverlayResult::Chosen(menu.items[2].action));
        assert!(h.state_mut().nav().iter().any(|n| matches!(n, crate::ui::NavRequest::Push(Route::Watchlists { list: Some(id) }) if *id == list)));
        h.get_by_label("Movie 2").click();
        until(&mut h, "clicked preview plays", |h| {
            let s = h.state().t.services().agent.queue();
            s.current.and_then(|id| s.row(id)).is_some_and(|r| r.entry.watchlist_item_id == Some(entries[2]))
        });
        h.run_steps(2);
        h.get_by_label("Stop the list").click();
        until(&mut h, "list stopped", |h| h.state().t.services().agent.queue().context.is_none());
    }

    #[test]
    fn watchlist_context_without_queue_rows_still_shows_on_the_screen() {
        let (mut t, one) = rig();
        let (list, entries) = seed_watchlist(&mut t, "Christmas films", crate::watchlist::PlayOrder::Any, 6);
        publish(&t, &[], |s| {
            s.context = Some(list);
            s.watchlist = Some(crate::queue::ListContext {
                id: list,
                name: "Christmas films".into(),
                any_order: true,
                position: 1,
                total: 6,
                playing_through: false,
                then: entries[1..4]
                    .iter()
                    .enumerate()
                    .map(|(n, id)| crate::queue::ListPreview {
                        id: *id,
                        kind: ItemKind::Movie,
                        title: format!("Movie {}", n + 1),
                        series: None,
                        season: None,
                        episode: None,
                        locked: n == 0,
                    })
                    .collect(),
            });
        });
        let mut h = harness(t, one, QueueScreen::default(), egui::vec2(800.0, 700.0));
        h.state_mut().enter(Route::Queue);
        crate::ui::screens::home::testkit::until(&mut h, "context-only previews", |h| has_label(h, "Another from Christmas films · 1 of 6"));
        assert!(has_label(&h, "󰌾 Movie 1") && has_label(&h, "and 2 more") && has_label(&h, "Stop the list"));
        assert!(!has_label(&h, "Up next is empty"));
    }

    /// The stored order, by movie number.
    fn order(h: &H) -> Vec<String> {
        h.state().t.services().agent.queue().rows.iter().map(|r| r.entry.title.split(" (").next().unwrap_or_default().to_owned()).collect()
    }

    fn title(h: &H, movie: &str) -> String {
        let s = h.state().t.services().agent.queue();
        s.rows.iter().find(|r| r.entry.title.starts_with(movie)).map(|r| crate::queue::ui::row_title(&r.entry)).unwrap()
    }

    /// Ctrl+N / Ctrl+Shift+N add (a channel without a guide is refused with a toast); Alt+↓ and
    /// a drag by the handle both reorder the stored list, and a drag cancelled with Esc changes
    /// nothing; Delete removes and the toast's Undo puts the row back in its place.
    #[test]
    fn adds_reorders_and_undo_reach_the_engine() {
        use crate::ui::item_menu::Target;
        use crate::ui::screens::home::testkit::{channels, movies, seed, source, until};
        let (mut t, one) = rig();
        let (ms, ch) = seed(&mut t, |c| {
            let a = source(c, "TV")?;
            Ok((movies(c, a, 3)?, channels(c, a, 1)?[0]))
        });
        start_engine(&t);
        let mut h = harness(t, one, QueueScreen::default(), egui::vec2(1600.0, 800.0));
        let add = |h: &mut H, a: Action, target: Target| h.state_mut().t.with_ctx(|ctx| crate::queue::ui::on_action(ctx, a, &target));
        assert!(add(&mut h, Action::QueueAdd, Target::Movie(ms[0])));
        assert!(add(&mut h, Action::QueueAdd, Target::Movie(ms[1])));
        assert!(add(&mut h, Action::QueuePlayNext, Target::Movie(ms[2])));
        until(&mut h, "three rows, the last one next", |h| order(h) == ["Movie 2", "Movie 0", "Movie 1"]);
        assert!(add(&mut h, Action::QueueAdd, Target::Channel(ch)));
        assert!(h.state().t.toasts.iter().any(|x| x.text == "Live channels join Up next by programme; this one has no guide"));
        h.state_mut().enter(Route::Queue);
        h.run_steps(3);
        // Alt+↓ on the next row (the cursor starts there)
        assert!(h.state_mut().act(Action::MoveDown));
        until(&mut h, "Movie 2 one down", |h| order(h) == ["Movie 0", "Movie 2", "Movie 1"]);
        h.run_steps(2);
        // drag Movie 2 by its handle above Movie 0
        let drag_to_top = |h: &mut H, cancel: bool| {
            let row = h.get_by_role_and_label(egui::accesskit::Role::Button, &title(h, "Movie 2")).rect();
            h.hover_at(row.center());
            h.run_steps(2);
            let handle = h.get_by_label(&format!("Drag {}", title(h, "Movie 2"))).rect().center();
            let top = h.get_by_role_and_label(egui::accesskit::Role::Button, &title(h, "Movie 0")).rect().top() + 2.0;
            h.hover_at(handle);
            h.drag_at(handle);
            h.run_steps(1);
            h.hover_at(egui::pos2(handle.x, top + 10.0));
            h.run_steps(1);
            h.hover_at(egui::pos2(handle.x, top));
            h.run_steps(2);
            if cancel {
                assert!(h.state_mut().act(Action::Back), "Esc cancels the drag first");
            }
            h.drop_at(egui::pos2(handle.x, top));
            h.run_steps(2);
        };
        drag_to_top(&mut h, true);
        std::thread::sleep(std::time::Duration::from_millis(200));
        h.run_steps(2);
        assert_eq!(order(&h), ["Movie 0", "Movie 2", "Movie 1"], "a cancelled drag moves nothing");
        drag_to_top(&mut h, false);
        until(&mut h, "Movie 2 dragged back to the top", |h| order(h) == ["Movie 2", "Movie 0", "Movie 1"]);
        // Delete on Movie 0, then the toast's Undo
        let zero = h.state().t.services().agent.queue().rows[1].entry.id;
        h.state_mut().s.list.select(zero);
        h.state_mut().t.toasts.clear();
        assert!(h.state_mut().act(Action::Remove));
        until(&mut h, "Movie 0 removed", |h| order(h) == ["Movie 2", "Movie 1"]);
        let undo = h.state().t.toasts.iter().find_map(|x| x.action.as_ref().map(|a| a.1));
        let Some(Action::Undo(n)) = undo else { panic!("no Undo: {:?}", h.state().t.toasts) };
        // the app hands a toast's action to the screen first (`TunerApp::dispatch_actions`)
        let app = h.state_mut();
        let s = &mut app.s;
        assert!(app.t.with_ctx(|ctx| s.on_action(ctx, Action::Undo(n))));
        until(&mut h, "Movie 0 back in its place", |h| order(h) == ["Movie 2", "Movie 0", "Movie 1"]);
    }

    /// The film commands the engine sent to the main picture: (film, resume, start at).
    fn film_plays(rec: &Rec) -> Vec<(crate::model::MediaRef, bool)> {
        rec.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                crate::player::PlayerCommand::PlayCopy(c) => Some((c.req.media.clone(), c.req.resume)),
                _ => None,
            })
            .collect()
    }

    /// Enter on a row with progress asks Resume / Start over like everywhere else (the engine
    /// then plays it as Up next's: "Start over" from 0, "Resume" from where it was; Shift+Enter
    /// opens the player once answered); a row with nothing to resume plays straight away.
    #[test]
    fn enter_on_a_row_with_progress_asks_resume_or_start_over() {
        use crate::model::{ItemKey, MediaRef, WatchProgress};
        use crate::ui::NavRequest;
        use crate::ui::item_menu::Target;
        use crate::ui::overlays::{Overlay, QueueResume};
        use crate::ui::screens::home::testkit::{movies, seed, source, until};
        let (mut t, one) = rig();
        let ms = seed(&mut t, |c| {
            let a = source(c, "TV")?;
            let ms = movies(c, a, 2)?;
            let item = ItemRef { source: a, kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000000".into()) };
            crate::store::user::save_progress(
                c,
                &WatchProgress {
                    item,
                    position_s: 600.0,
                    duration_s: Some(3600.0),
                    updated_at: 5,
                    finished: false,
                    title: "Movie 0".into(),
                    fingerprint: Default::default(),
                },
            )?;
            Ok(ms)
        });
        let rec = start_engine(&t);
        let mut h = harness(t, one, QueueScreen::default(), egui::vec2(1600.0, 800.0));
        for m in &ms {
            assert!(h.state_mut().t.with_ctx(|ctx| crate::queue::ui::on_action(ctx, Action::QueueAdd, &Target::Movie(*m))));
        }
        until(&mut h, "both rows", |h| order(h) == ["Movie 0", "Movie 1"]);
        h.state_mut().enter(Route::Queue);
        h.run_steps(3);
        let ids: Vec<QueueId> = h.state().t.services().agent.queue().rows.iter().map(|r| r.entry.id).collect();
        h.state_mut().nav();

        // Enter on Movie 0: the prompt, nothing played yet
        h.state_mut().s.list.select(ids[0]);
        assert!(h.state_mut().act(Action::Activate));
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::Resume(spec))] = nav.as_slice() else { panic!("{nav:?}") };
        assert_eq!((spec.position_s, spec.duration_s), (600.0, Some(3600.0)));
        assert_eq!(spec.queue, Some(QueueResume { id: ids[0], fullscreen: false }));
        assert!(spec.title.starts_with("Movie 0"), "{}", spec.title);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(film_plays(&rec).is_empty(), "the prompt comes first");

        // "Start over" (Shift+Enter in the prompt): the engine plays it from 0; the list stays
        let mut prompt = crate::ui::overlays::resume::ResumeOverlay::new(spec.clone());
        h.state_mut().t.with_ctx(|ctx| prompt.on_action(ctx, Action::ActivateAlt));
        until(&mut h, "Movie 0 from the start", |_| film_plays(&rec) == [(MediaRef::Movie(ms[0]), false)]);
        assert!(!h.state_mut().nav().contains(&NavRequest::Push(Route::Player)));
        until(&mut h, "Movie 0 is Up next's", |h| h.state().t.services().agent.queue().current == Some(ids[0]));

        // Shift+Enter on it again (it isn't what plays: nothing is loaded in these tests), then
        // "Resume": from where it was, and the player opens
        h.state_mut().s.list.select(ids[0]);
        assert!(h.state_mut().act(Action::ActivateAlt));
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::Resume(spec))] = nav.as_slice() else { panic!("{nav:?}") };
        assert_eq!(spec.queue, Some(QueueResume { id: ids[0], fullscreen: true }));
        let mut prompt = crate::ui::overlays::resume::ResumeOverlay::new(spec.clone());
        h.state_mut().t.with_ctx(|ctx| prompt.on_action(ctx, Action::Activate));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        until(&mut h, "Movie 0 resumed", |_| film_plays(&rec).get(1) == Some(&(MediaRef::Movie(ms[0]), true)));

        // Movie 1 has nothing to resume: Enter plays it at once
        h.state_mut().s.list.select(ids[1]);
        assert!(h.state_mut().act(Action::Activate));
        assert!(h.state_mut().nav().is_empty(), "no prompt");
        until(&mut h, "Movie 1 playing", |_| film_plays(&rec).get(2) == Some(&(MediaRef::Movie(ms[1]), true)));
    }
}
