//! The reminder sheet and the sleep timer menu (B6). OWNER: rui. Logic and texts in
//! `crate::reminders::ui`; both are keyboard-first (↑/↓ rows, ←/→ values, Enter, Esc) with the
//! same paths for the mouse.
//!
//! * [`ReminderSheet`]: "Switch to this when it starts…" / "Edit reminder…" on a programme:
//!   Notify me / Switch to it · how long before (notify) · Start (on time, N min in, 5 min
//!   early) · Open in (switch: pop-out, full window, where I am) · the clashes with other
//!   pending reminders · Set / Remove / Close.
//! * [`SleepTimerMenu`] (Ctrl+Shift+T, the player menu, the status-bar chip): Then stop / pause
//!   · in 15 min … 2 h · at the end of this programme · after N items · at the end of Up next ·
//!   turn off.

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim, title, with_close};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, MediaRef, OpenIn, ProgrammeId, Reminder};
use crate::msg::{DataScope, Toast};
use crate::reminders::sleep::{SleepAction, SleepWhen};
use crate::reminders::ui::{OPEN_IN, ProgrammeOn, SheetChoice, clash_text, offset_label, open_in_label, sleep_status_text};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::epg_grid::{day_label, time_range};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::glyphs;

/// `Label` above a wrapped row of choices (the selected one bold; a focus ring on the focused
/// row). Returns the clicked index.
fn choice_row(ui: &mut egui::Ui, theme: &Theme, label: &str, options: &[String], selected: usize, focused: bool) -> Option<usize> {
    let t = &theme.tokens;
    ui.add_space(theme.space(SpaceToken::Sm));
    ui.label(RichText::new(label).size(theme.font(TypeToken::BodySmall)).color(if focused { t.list_cursor_text } else { t.text_secondary }));
    let r = ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xxs);
        let mut hit = None;
        for (i, o) in options.iter().enumerate() {
            if button(ui, theme, None, o, ButtonStyle { selected: i == selected, ..Default::default() }).clicked() {
                hit = Some(i);
            }
        }
        hit
    });
    if focused {
        // the keyboard's row: an accent ring around its choices (←/→ change them)
        ui.painter().rect_stroke(r.response.rect.expand(2.0), t.rounding, egui::Stroke::new(1.0, t.list_cursor_text), egui::StrokeKind::Outside);
    }
    r.inner
}

/// Step `i` by `d` within `0..n` (no wrap).
fn step(i: usize, d: i32, n: usize) -> usize {
    (i as i64 + i64::from(d)).clamp(0, n.saturating_sub(1) as i64) as usize
}

// ------------------------------------------------------------------ reminder sheet

/// Which programme the sheet is about; `switch`: start on "Switch to it" (the menu entry's
/// promise) when it has no reminder yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderSheetSpec {
    pub programme: ProgrammeId,
    pub channel: ChannelId,
    pub switch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Mode,
    Lead,
    Offset,
    OpenIn,
    Buttons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Save,
    Remove,
    Close,
}

struct Loaded {
    /// `None`: the programme left the guide (or its channel the library).
    on: Option<ProgrammeOn>,
    /// Pending reminders around it (clash candidates; the reminder itself left out).
    around: Vec<Reminder>,
}

fn programme_len(p: &ProgrammeOn) -> i64 {
    if p.stop > p.start { p.stop - p.start } else { crate::reminders::DEFAULT_DURATION_S }
}

pub struct ReminderSheet {
    spec: ReminderSheetSpec,
    loaded: Option<Loaded>,
    choice: Option<SheetChoice>,
    field: usize,
    btn: usize,
    error: Option<String>,
}

impl ReminderSheet {
    pub fn new(spec: ReminderSheetSpec) -> Self {
        Self { spec, loaded: None, choice: None, field: 0, btn: 0, error: None }
    }

    fn load(&mut self, ctx: &mut UiCtx<'_>) {
        if self.loaded.is_some() {
            return;
        }
        let (db, lib, spec) = (ctx.db, ctx.lib, self.spec);
        let Some(on) = ctx.budget.run(|| crate::reminders::ui::programme_on(db, lib, spec.programme, spec.channel)) else { return };
        let around = on
            .as_ref()
            .map(|p| {
                // any offset the sheet offers keeps the window inside these bounds
                let (from, to) = (p.start - 3600, p.stop.max(p.start + crate::reminders::DEFAULT_DURATION_S) + 3600);
                crate::store::user::reminders_overlapping(db, from, to).unwrap_or_default()
            })
            .unwrap_or_default();
        let own = on.as_ref().and_then(|p| p.existing.as_ref()).map(|r| r.id);
        let around = around.into_iter().filter(|r| Some(r.id) != own).collect();
        self.choice = on.as_ref().map(|p| SheetChoice::of(p.existing.as_ref(), &ctx.config.reminders, spec.switch));
        self.loaded = Some(Loaded { on, around });
    }

    fn fields(&self) -> Vec<Field> {
        let Some(c) = &self.choice else { return vec![Field::Buttons] };
        let mut v = vec![Field::Mode];
        v.push(if c.switch { Field::OpenIn } else { Field::Lead });
        v.insert(if c.switch { 1 } else { 2 }, Field::Offset);
        v.push(Field::Buttons);
        v
    }

    fn btns(&self) -> Vec<Btn> {
        let existing = self.loaded.as_ref().and_then(|l| l.on.as_ref()).is_some_and(|p| p.existing.is_some());
        let mut v = Vec::new();
        if self.choice.is_some() {
            v.push(Btn::Save);
        }
        if existing {
            v.push(Btn::Remove);
        }
        v.push(Btn::Close);
        v
    }

    /// The programme's length (offsets past its end are not offered).
    fn programme_len(&self) -> i64 {
        self.loaded.as_ref().and_then(|l| l.on.as_ref()).map_or(i64::MAX, programme_len)
    }

    /// Move the value of `f` by `d` (or to option `to`).
    fn change(&mut self, f: Field, d: i32, to: Option<usize>) {
        let len_s = self.programme_len();
        let Some(c) = &mut self.choice else { return };
        match f {
            Field::Mode => c.switch = to.map_or(!c.switch, |i| i == 1),
            Field::Lead => {
                let leads = c.leads();
                let i = leads.iter().position(|l| *l == c.lead_min).unwrap_or(0);
                c.lead_min = leads[to.unwrap_or_else(|| step(i, d, leads.len()))];
            }
            Field::Offset => {
                let offs = c.offsets(len_s);
                let i = offs.iter().position(|o| *o == c.offset_min).unwrap_or(0);
                c.offset_min = offs[to.unwrap_or_else(|| step(i, d, offs.len()))];
            }
            Field::OpenIn => {
                let i = OPEN_IN.iter().position(|o| *o == c.open_in).unwrap_or(0);
                c.open_in = OPEN_IN[to.unwrap_or_else(|| step(i, d, OPEN_IN.len()))];
            }
            Field::Buttons => self.btn = step(self.btn, d, self.btns().len()),
        }
        // the rows change with the mode: stay on the Mode row
        if f == Field::Mode {
            self.field = 0;
        }
        self.error = None;
    }

    fn run(&mut self, ctx: &mut UiCtx<'_>, b: Btn) -> OverlayOutcome {
        match b {
            Btn::Close => OverlayOutcome::Close,
            Btn::Save => self.save(ctx),
            Btn::Remove => {
                let Some(r) = self.loaded.as_ref().and_then(|l| l.on.as_ref()).and_then(|p| p.existing.clone()) else { return OverlayOutcome::Close };
                let id = r.id;
                let _ = ctx
                    .services
                    .store
                    .writer()
                    .submit_notify(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id, crate::model::RemoveWhy::User));
                ctx.toast(Toast::info(format!("Reminder cancelled: {}", r.programme_title)));
                OverlayOutcome::Close
            }
        }
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let (Some(p), Some(c)) = (self.loaded.as_ref().and_then(|l| l.on.as_ref()), self.choice) else { return OverlayOutcome::Close };
        let default = ctx.config.reminders.open_in;
        let draft = p.draft(&c, default);
        // a switch happens at start + offset; a note goes out `lead` before it
        let due = if c.switch { crate::reminders::switch_at(&draft) } else { crate::reminders::notify_at(&draft, 0) };
        if crate::reminders::target_at(&draft) <= ctx.now || due <= ctx.now {
            self.error = Some("That time has passed — watch it now instead".into());
            return OverlayOutcome::Keep;
        }
        let writer = ctx.services.store.writer();
        let _ = match p.existing.as_ref().map(|r| r.id) {
            Some(id) => {
                let edit = c.edit(default);
                writer.submit_notify(DataScope::Reminders, move |conn| crate::store::user::edit_reminder(conn, id, &edit).map(|_| ()))
            }
            None => writer.submit_notify(DataScope::Reminders, move |conn| crate::store::user::add_reminder(conn, &draft).map(|_| ())),
        };
        let h24 = ctx.config.interface.clock_24h;
        let at = crate::ui::widgets::time::hhmm(crate::reminders::target_at(&p.draft(&c, default)), h24);
        let text = if c.switch {
            let place = match c.open_in {
                OpenIn::PopOut => "in the pop-out",
                OpenIn::FullWindow => "in the full window",
                OpenIn::WhereIAm => "where you are",
            };
            format!("Switching to {} at {at} ({place}) · {}", p.channel.name, p.title)
        } else {
            format!("Reminder set: {} min before {at} · {} on {}", c.lead_min, p.title, p.channel.name)
        };
        ctx.toast(Toast::info(text));
        OverlayOutcome::Close
    }
}

impl Overlay for ReminderSheet {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.load(ctx);
        let fields = self.fields();
        self.field = self.field.min(fields.len() - 1);
        let f = fields[self.field];
        match action {
            Action::Up | Action::FocusPrev => self.field = step(self.field, -1, fields.len()),
            Action::Down | Action::FocusNext => self.field = step(self.field, 1, fields.len()),
            Action::Left | Action::Decrease => self.change(f, -1, None),
            Action::Right | Action::Increase => self.change(f, 1, None),
            Action::Activate if f == Field::Buttons => {
                let btns = self.btns();
                return self.run(ctx, btns[self.btn.min(btns.len() - 1)]);
            }
            // Enter on a value row sets the reminder as shown
            Action::Activate => return self.save(ctx),
            Action::Back => return OverlayOutcome::Close,
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.load(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "reminder-sheet") {
            return OverlayOutcome::Close;
        }
        let (now, h24) = (ctx.now, ctx.config.interface.clock_24h);
        let guard = ctx.services.agent.guard();
        let fields = self.fields();
        self.field = self.field.min(fields.len() - 1);
        let btns = self.btns();
        self.btn = self.btn.min(btns.len() - 1);
        let mut clicked: Option<(Field, usize)> = None;
        let mut closed = false;
        card_area(egui_ctx, theme, "reminder-sheet", CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            let Some(l) = &self.loaded else {
                ui.label(RichText::new("Loading…").size(theme.font(TypeToken::Body)).color(t.text_secondary));
                return;
            };
            let heading = if l.on.as_ref().is_some_and(|p| p.existing.is_some()) { "Edit reminder" } else { "Remind me" };
            closed = with_close(ui, theme, |ui| {
                ui.label(RichText::new(heading).size(theme.font(TypeToken::Heading)).color(t.fg));
            });
            let Some(p) = &l.on else {
                ui.label(RichText::new("This programme is no longer in the guide").size(theme.font(TypeToken::Title)).color(t.text_secondary));
                return;
            };
            let shown = guard.outside_title(&p.title, now);
            title(ui, theme, &shown, Some(&format!("{} · {} · {}", day_label(p.start), time_range(p.start, p.stop, h24), p.channel.name)));
            let Some(c) = self.choice else { return };
            let len_s = programme_len(p);
            for (i, f) in fields.iter().enumerate() {
                let focused = i == self.field;
                let hit = match f {
                    Field::Mode => {
                        choice_row(ui, theme, "When it starts", &["Notify me".into(), "Switch to it".into()], usize::from(c.switch), focused)
                    }
                    Field::Lead => {
                        let leads = c.leads();
                        let opts: Vec<String> = leads.iter().map(|m| format!("{m} min")).collect();
                        let sel = leads.iter().position(|m| *m == c.lead_min).unwrap_or(0);
                        choice_row(ui, theme, "Notify me before", &opts, sel, focused)
                    }
                    Field::Offset => {
                        let offs = c.offsets(len_s);
                        let opts: Vec<String> = offs.iter().map(|m| offset_label(*m)).collect();
                        let sel = offs.iter().position(|m| *m == c.offset_min).unwrap_or(0);
                        let label = if c.switch { "Switch (skip the build-up)" } else { "From" };
                        choice_row(ui, theme, label, &opts, sel, focused)
                    }
                    Field::OpenIn => {
                        let default = ctx.config.reminders.open_in;
                        let opts: Vec<String> = OPEN_IN
                            .iter()
                            .map(|o| if *o == default { format!("{} (setting)", open_in_label(*o)) } else { open_in_label(*o).to_owned() })
                            .collect();
                        let sel = OPEN_IN.iter().position(|o| *o == c.open_in).unwrap_or(0);
                        choice_row(ui, theme, "Open in", &opts, sel, focused)
                    }
                    Field::Buttons => None,
                };
                if let Some(o) = hit {
                    clicked = Some((*f, o));
                }
            }
            // what it clashes with (the same programme on another reminder never does)
            let draft = p.draft(&c, ctx.config.reminders.open_in);
            let clashes = crate::reminders::clashes_with(&draft, &l.around);
            if !clashes.is_empty() {
                ui.add_space(theme.space(SpaceToken::Md));
                for (other, kind) in clashes.iter().take(3) {
                    let title = guard.outside_title(&other.programme_title, now);
                    let text = format!("{} {}", glyphs::WARNING, clash_text(other, *kind, &title, h24));
                    ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(t.warning)).wrap());
                }
                if clashes.len() > 3 {
                    ui.label(RichText::new(format!("…and {} more", clashes.len() - 3)).size(theme.font(TypeToken::BodySmall)).color(t.warning));
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.add(egui::Label::new(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(t.urgent)).wrap());
            }
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
                let on_buttons = fields[self.field] == Field::Buttons;
                for (i, b) in btns.iter().enumerate() {
                    let (label, style) = match b {
                        Btn::Save if p.existing.is_some() => ("Save", ButtonStyle { primary: true, ..Default::default() }),
                        Btn::Save => ("Set reminder", ButtonStyle { primary: true, ..Default::default() }),
                        Btn::Remove => ("Cancel reminder", ButtonStyle { danger: true, bordered: true, ..Default::default() }),
                        Btn::Close => ("Close", ButtonStyle { bordered: true, ..Default::default() }),
                    };
                    if button(ui, theme, None, label, ButtonStyle { focused: on_buttons && i == self.btn, ..style }).clicked() {
                        clicked = Some((Field::Buttons, i));
                    }
                }
            });
        });
        if closed {
            return OverlayOutcome::Close;
        }
        match clicked {
            Some((Field::Buttons, i)) => self.run(ctx, btns[i]),
            Some((f, i)) => {
                self.field = self.fields().iter().position(|x| *x == f).unwrap_or(0);
                self.change(f, 0, Some(i));
                OverlayOutcome::Keep
            }
            None => OverlayOutcome::Keep,
        }
    }
}

// ------------------------------------------------------------------ sleep timer menu

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepRow {
    /// Then: stop / pause (←/→, Enter toggles).
    Then,
    In(u32),
    EndOfProgramme,
    /// After N items (←/→ change N).
    AfterItems,
    EndOfQueue,
    Off,
}

/// The minutes the menu offers.
const SLEEP_MINUTES: [u32; 6] = [15, 30, 45, 60, 90, 120];

pub struct SleepTimerMenu {
    cursor: usize,
    pause: bool,
    /// After N items.
    items: u32,
    error: Option<String>,
}

impl Default for SleepTimerMenu {
    /// On the first time ("In 15 minutes"): Enter sets it; "Then" is one row up.
    fn default() -> Self {
        Self { cursor: 1, pause: false, items: 1, error: None }
    }
}

impl SleepTimerMenu {
    fn rows(ctx: &UiCtx<'_>) -> Vec<SleepRow> {
        let playing = !matches!(ctx.player.state, crate::player::PlayerState::Idle) && ctx.player.playable.is_some();
        let live = ctx.player.playable.as_ref().is_some_and(|p| p.kind.is_live());
        let q = ctx.services.agent.queue();
        let driving = q.driving && q.current.is_some();
        let mut v = vec![SleepRow::Then];
        v.extend(SLEEP_MINUTES.iter().map(|m| SleepRow::In(*m)));
        if playing {
            v.push(SleepRow::EndOfProgramme);
        }
        if driving || (playing && !live) {
            v.push(SleepRow::AfterItems);
        }
        if driving {
            v.push(SleepRow::EndOfQueue);
        }
        if ctx.services.agent.sleep().and_then(|h| h.status()).is_some() {
            v.push(SleepRow::Off);
        }
        v
    }

    fn items(&self) -> u32 {
        self.items.max(1)
    }

    fn label(&self, row: SleepRow, ctx: &UiCtx<'_>) -> String {
        let up_next = {
            let q = ctx.services.agent.queue();
            q.driving && q.current.is_some()
        };
        match row {
            SleepRow::Then => format!("Then: {}", if self.pause { "pause" } else { "stop playback" }),
            SleepRow::In(m) if m < 60 => format!("In {m} minutes"),
            SleepRow::In(60) => "In 1 hour".into(),
            SleepRow::In(90) => "In 1½ hours".into(),
            SleepRow::In(m) => format!("In {} hours", m / 60),
            SleepRow::EndOfProgramme => "At the end of this programme".into(),
            SleepRow::AfterItems => match (self.items(), up_next) {
                (1, _) => "After this one".into(),
                (n, true) => format!("After {n} Up next items"),
                (n, false) => format!("After {n} episodes or films"),
            },
            SleepRow::EndOfQueue => "At the end of Up next".into(),
            SleepRow::Off => "Turn off the sleep timer".into(),
        }
    }

    fn adjust(&mut self, row: SleepRow, d: i32) {
        match row {
            SleepRow::Then => self.pause = !self.pause,
            SleepRow::AfterItems => {
                self.items = (i64::from(self.items()) + i64::from(d)).clamp(1, i64::from(crate::reminders::sleep::MAX_ITEMS)) as u32;
            }
            _ => {}
        }
    }

    fn choose(&mut self, ctx: &mut UiCtx<'_>, row: SleepRow) -> OverlayOutcome {
        let Some(h) = ctx.services.agent.sleep() else {
            self.error = Some("The sleep timer isn't running".into());
            return OverlayOutcome::Keep;
        };
        let action = if self.pause { SleepAction::Pause } else { SleepAction::Stop };
        let now = ctx.now;
        let (when, stop) = match row {
            SleepRow::Then => {
                self.adjust(row, 1);
                return OverlayOutcome::Keep;
            }
            SleepRow::Off => {
                h.cancel();
                ctx.toast(Toast::info("Sleep timer off"));
                return OverlayOutcome::Close;
            }
            SleepRow::In(m) => (SleepWhen::At(now + i64::from(m) * 60), None),
            SleepRow::EndOfProgramme => {
                // live: the guide's end of the programme on now (one indexed read)
                let stop = match ctx.player.playable.as_ref().map(|p| &p.media) {
                    Some(MediaRef::Channel(ch)) => {
                        let (db, lib, ch) = (ctx.db, ctx.lib, *ch);
                        ctx.budget.run(|| crate::reminders::sleep::live_programme_stop(db, lib, ch, now)).and_then(|r| r.ok().flatten())
                    }
                    _ => None,
                };
                (SleepWhen::EndOfProgramme, stop)
            }
            SleepRow::AfterItems => (SleepWhen::AfterItems(self.items()), None),
            SleepRow::EndOfQueue => (SleepWhen::EndOfQueue, None),
        };
        match h.set(when, action, ctx.config.reminders.sleep_warn_s, stop) {
            Ok(st) => {
                ctx.toast(Toast::info(format!("Sleep timer: {}", sleep_status_text(&st, now, ctx.config.interface.clock_24h).to_lowercase())));
                OverlayOutcome::Close
            }
            Err(e) => {
                self.error = Some(e.message().into());
                OverlayOutcome::Keep
            }
        }
    }
}

impl Overlay for SleepTimerMenu {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let rows = Self::rows(ctx);
        self.cursor = self.cursor.min(rows.len() - 1);
        let row = rows[self.cursor];
        match action {
            Action::Up | Action::FocusPrev => self.cursor = step(self.cursor, -1, rows.len()),
            Action::Down | Action::FocusNext => self.cursor = step(self.cursor, 1, rows.len()),
            Action::Left | Action::Decrease => self.adjust(row, -1),
            Action::Right | Action::Increase => self.adjust(row, 1),
            Action::Activate => return self.choose(ctx, row),
            Action::Back => return OverlayOutcome::Close,
            _ => return OverlayOutcome::Ignored,
        }
        self.error = None;
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "sleep-timer") {
            return OverlayOutcome::Close;
        }
        let rows = Self::rows(ctx);
        self.cursor = self.cursor.min(rows.len() - 1);
        let status = ctx.services.agent.sleep().and_then(|h| h.status());
        let (now, h24) = (ctx.now, ctx.config.interface.clock_24h);
        let labels: Vec<String> = rows.iter().map(|r| self.label(*r, ctx)).collect();
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg);
        let mut clicked = None;
        let mut closed = false;
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        card_area(egui_ctx, theme, "sleep-timer", CardKind::Menu, 380.0 * theme.rem(), None, |ui| {
            closed = with_close(ui, theme, |ui| {
                ui.label(RichText::new(format!("{} Sleep timer", glyphs::SLEEP)).size(theme.font(TypeToken::Heading)).color(t.fg));
            });
            let line = match &status {
                Some(s) => sleep_status_text(s, now, h24),
                None => "Off".into(),
            };
            ui.label(RichText::new(line).size(theme.font(TypeToken::Body)).color(t.text_secondary));
            ui.add_space(theme.space(SpaceToken::Md));
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xs);
            for (i, (row, label)) in rows.iter().zip(&labels).enumerate() {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
                let cursor = i == self.cursor;
                if pointer_moved && resp.hovered() && !cursor {
                    self.cursor = i;
                }
                if crate::ui::widgets::fresh_click(&resp) {
                    clicked = Some(i);
                }
                let p = ui.painter();
                crate::ui::widgets::list::paint_row_state(
                    p,
                    rect,
                    theme,
                    if cursor { crate::ui::widgets::RowState::Cursor } else { crate::ui::widgets::RowState::Idle },
                );
                let color = if cursor { t.list_cursor_text } else { t.fg };
                let inset = theme.space(SpaceToken::Lg);
                let adjustable = matches!(row, SleepRow::Then | SleepRow::AfterItems);
                let text = if adjustable { format!("‹ {label} ›") } else { label.clone() };
                p.text(
                    egui::pos2(rect.left() + inset, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    text,
                    egui::FontId::proportional(theme.font(TypeToken::Title)),
                    color,
                );
            }
            if let Some(e) = &self.error {
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.add(egui::Label::new(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(t.urgent)).wrap());
            }
            ui.add_space(theme.space(SpaceToken::Sm));
            ui.label(RichText::new("↑↓ choose · ←→ change · Enter set · Esc close").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        });
        if closed {
            return OverlayOutcome::Close;
        }
        match clicked {
            Some(i) => {
                self.cursor = i;
                self.choose(ctx, rows[i])
            }
            None => OverlayOutcome::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui;
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::reminders::ui::{Countdown, ReminderUi};
    use crate::ui::screens::home::testkit::{self, overlay_harness, rig, seed};

    const SIZES: [(f32, f32); 4] = [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)];

    fn theme(scale: f32) -> crate::theme::Theme {
        let base = crate::theme::Theme::fallback();
        crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale)
    }

    /// Nothing `h` laid out leaves a window `w` wide.
    fn fits<T>(h: &egui_kittest::Harness<'_, T>, w: f32, what: &str) {
        let mut n = 0;
        for node in h.query_all_by_label_contains("") {
            let r = node.rect();
            assert!(r.left() >= -1.0 && r.right() <= w + 1.0, "{what} at {w} pt: {node:?}");
            n += 1;
        }
        assert!(n > 3, "{what} drew nothing");
    }

    /// The reminder sheet, the sleep timer menu and the strip fit a tiled half (790 pt) and a
    /// full screen (1600 pt) at Omarchy text size 12 and 16; the strip also fits the 480 × 270
    /// pop-out.
    #[test]
    fn reminder_ui_fits_790_and_1600_pt_at_text_size_12_and_16() {
        let (mut t, one) = rig();
        let start = crate::model::now() / 1800 * 1800 + 3600;
        let (programme, channel) = seed(&mut t, move |c| {
            let s = testkit::source(c, "My IPTV with a rather long playlist name")?;
            let ch = testkit::channels(c, s, 2)?;
            testkit::epg(c, &ch, start, 2, true)?;
            let p: i64 = c.query_row("SELECT id FROM programme ORDER BY id LIMIT 1", [], |r| r.get(0))?;
            Ok((ProgrammeId(p), ch[0]))
        });
        let mut h = overlay_harness(t, one, ReminderSheet::new(ReminderSheetSpec { programme, channel, switch: true }), egui::vec2(1600.0, 900.0));
        for (w, scale) in SIZES {
            h.state_mut().t.theme = theme(scale);
            h.set_size(egui::vec2(w, 900.0));
            h.run_steps(3);
            assert!(h.query_all_by_label("Set reminder").next().is_some(), "the sheet loaded");
            fits(&h, w, "the reminder sheet");
        }
        drop(h);
        let (t, one) = rig();
        let mut h = overlay_harness(t, one, SleepTimerMenu::default(), egui::vec2(1600.0, 900.0));
        for (w, scale) in SIZES {
            h.state_mut().t.theme = theme(scale);
            h.set_size(egui::vec2(w, 900.0));
            h.run_steps(3);
            fits(&h, w, "the sleep timer menu");
        }
        drop(h);
        let mut strip = ReminderUi { sleep_warn: Some(crate::model::now() + 100), ..Default::default() };
        strip.add_countdown(Countdown {
            reminder: crate::model::ReminderId(1),
            channel_name: "A channel with a rather long name HD".into(),
            title: "The final of the championship, live from the stadium".into(),
            at: crate::model::now() + 10,
            open_in: OpenIn::PopOut,
        });
        for (w, h_, scale, video_only) in [(790.0, 900.0, 1.0, false), (1600.0, 900.0, 4.0 / 3.0, false), (480.0, 270.0, 1.0, true)] {
            let th = theme(scale);
            let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(w, h_)).build_ui(|ui| {
                strip.draw(ui.ctx(), &th, crate::model::now(), video_only, true, None);
            });
            h.run_steps(2);
            assert!(h.query_all_by_label("Open full").next().is_some());
            fits(&h, w, "the strip");
        }
    }
}
