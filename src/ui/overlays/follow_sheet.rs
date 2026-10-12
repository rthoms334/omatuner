//! The Follow sheet (`R` in the guide, "Follow…" / "News flash this daily…" in the programme
//! popover and item menus, Settings ▸ Following; DESIGN §33 §3.5, big-features additions).
//! OWNER: F-C.
//!
//! Two views in one card:
//! * **Pick** (opened on a programme): the suggestions `follow::suggest` makes for it ("Arsenal:
//!   every match", "News flash: BBC News at One every day", "Every airing of …"), a When row
//!   and a count line from the live preview ("4 in the guide · next Sat 12:30"). ↑/↓ move
//!   between the suggestions and When, ←/→ change When, Enter follows the focused suggestion,
//!   Tab reaches the buttons. "More options…" opens the editor on that draft, unsaved. Opened
//!   from Settings ▸ Following ▸ "+ Follow…", the suggestions are the templates (Team, Show,
//!   Event, News flash, Custom), and choosing one opens the editor.
//! * **Edit** (a rule, or a draft): name, names to look for and leave out (chips: type, Enter
//!   adds; Backspace on an empty field removes the last), where to look, airings, days,
//!   channels, what it does (remind / switch / list / pop up), lead, start offset, the news
//!   flash's length and volume, spoiler-safe, the weekly limit, and "In the guide now" — the
//!   preview with what it leaves out and why. Save; for a stored rule also Delete… (cancel or
//!   keep its reminders), Duplicate and Move up / down (higher wins a clash). Esc leaves a
//!   field, then — with unsaved changes — warns once ("Unsaved rule changes: Enter on Save, or
//!   Esc to discard"), then discards; a click outside never throws an edit away.
//!
//! The preview runs on `follow::preview::PreviewService` (blocking pool, 350 ms debounce,
//! newest wins), started by the sheet with a bus of its own: its answers come back here, not
//! through the app. Explicit channels are never looked at (the preview's pool drops them).

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim_guarded};
use super::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::follow::flash::{FlashLength, FlashSpec, FlashVolume};
use crate::follow::plan::Preview;
use crate::follow::preview::PreviewService;
use crate::follow::spec::{Fields, WEEKDAYS};
use crate::follow::suggest::Suggestion;
use crate::follow::ui as fui;
use crate::follow::{Airings, ChannelScope, FollowEvent, FollowKind, FollowMode, FollowRule, FollowSpec};
use crate::keymap::{Action, KeyContext};
use crate::model::{FollowRuleId, ReminderState};
use crate::msg::{AppEvent, EventBus, Toast};
use crate::store::Ticket;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button, icon_button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::text_field::text_field;
use crate::ui::widgets::{glyphs, section};

/// What the sheet opens on.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowSheetSpec {
    /// The programme it was opened on (`None`: a blank rule, Settings ▸ Following ▸ New).
    pub programme: Option<(crate::model::ProgrammeId, crate::model::ChannelId)>,
    /// An existing rule to edit.
    pub rule: Option<crate::model::FollowRuleId>,
}

/// Days ahead the preview looks.
const PREVIEW_DAYS: u32 = 14;
/// Preview rows shown (matches; left-outs get half).
const PREVIEW_ROWS: usize = 8;
/// A click that only selects a row (the suggestions).
const SELECT: i32 = 2;
/// "Delete…" menu codes.
const DELETE_CANCEL: u8 = 1;
const DELETE_KEEP: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Pick,
    Edit,
}

/// A focusable row or button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum F {
    Suggestion(usize),
    When,
    MoreOptions,
    Follow,
    Name,
    Terms,
    Without,
    WithAny,
    LookIn,
    Shape,
    Airings,
    Days,
    Channels,
    Mode,
    Lead,
    Offset,
    Length,
    Headlines,
    Volume,
    Spoiler,
    Limit,
    Enabled,
    Delete,
    Duplicate,
    MoveUp,
    MoveDown,
    Save,
    Cancel,
}

impl F {
    fn is_button(self) -> bool {
        matches!(self, F::MoreOptions | F::Follow | F::Delete | F::Duplicate | F::MoveUp | F::MoveDown | F::Save | F::Cancel)
    }
    fn is_text(self) -> bool {
        matches!(self, F::Name | F::Terms | F::Without | F::WithAny)
    }
}

/// What the Pick view's When row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    Remind,
    Switch,
    List,
    FlashHeadlines,
    FlashWhole,
}

fn whens(kind: FollowKind) -> &'static [When] {
    if kind == FollowKind::News { &[When::FlashHeadlines, When::FlashWhole, When::Remind] } else { &[When::Remind, When::Switch, When::List] }
}

fn when_of(r: &FollowRule) -> When {
    match r.mode {
        FollowMode::Notify => When::Remind,
        FollowMode::Switch => When::Switch,
        FollowMode::List => When::List,
        FollowMode::Flash if r.spec.flash.is_some_and(|f| f.length == FlashLength::Whole) => When::FlashWhole,
        FollowMode::Flash => When::FlashHeadlines,
    }
}

fn apply_when(r: &mut FollowRule, w: When, headlines_min: u16) {
    let volume = r.spec.flash.map(|f| f.volume).unwrap_or_default();
    let flash = |length| Some(FlashSpec { length, volume });
    match w {
        When::Remind => {
            r.mode = FollowMode::Notify;
            if r.lead_s <= 0 {
                r.lead_s = 120;
            }
        }
        When::Switch => r.mode = FollowMode::Switch,
        When::List => r.mode = FollowMode::List,
        When::FlashHeadlines => {
            r.mode = FollowMode::Flash;
            let min = match r.spec.flash.map(|f| f.length) {
                Some(FlashLength::Headlines(m)) => m,
                _ => headlines_min.max(1),
            };
            r.spec.flash = flash(FlashLength::Headlines(min));
        }
        When::FlashWhole => {
            r.mode = FollowMode::Flash;
            r.spec.flash = flash(FlashLength::Whole);
        }
    }
}

/// "Remind me 2 min before", "Switch over at the start", "Pop up · first 5 min"…
fn when_text(r: &FollowRule, w: When) -> String {
    match w {
        When::Remind if r.lead_s > 0 => format!("Remind me {} min before", (r.lead_s + 59) / 60),
        When::Remind => "Remind me at the start".into(),
        When::Switch => "Switch over at the start".into(),
        When::List => "Just list it".into(),
        When::FlashHeadlines => match r.spec.flash.map(|f| f.length) {
            Some(FlashLength::Headlines(m)) => format!("Pop up · first {m} min"),
            _ => "Pop up · headlines".into(),
        },
        When::FlashWhole => "Pop up · whole programme".into(),
    }
}

/// The templates of a blank rule (Settings ▸ Following ▸ "+ Follow…").
fn templates(headlines_min: u16) -> Vec<Suggestion> {
    [
        (FollowKind::Team, "A team: every match"),
        (FollowKind::Show, "A show: new episodes"),
        (FollowKind::Event, "A competition or event: every live session"),
        (FollowKind::News, "News flash: a bulletin that pops up every day"),
        (FollowKind::Custom, "Anything with these words"),
    ]
    .into_iter()
    .map(|(kind, label)| Suggestion { label: label.into(), name: String::new(), kind, spec: FollowSpec::template(kind, Vec::new(), headlines_min) })
    .collect()
}

/// What the sheet read when it opened.
struct Loaded {
    /// The programme's title and "Sat 18 Oct 12:30 · Sky Sports Main Event".
    head: Option<(String, String)>,
    suggestions: Vec<Suggestion>,
    /// The rule being edited, as stored.
    stored: Option<FollowRule>,
    /// Every rule in list order (Move up / down).
    order: Vec<FollowRuleId>,
    /// The stored rule's pending reminders (Delete…).
    owned: usize,
}

/// The preview service this sheet started and its private bus.
struct Live {
    service: PreviewService,
    rx: crossbeam_channel::Receiver<AppEvent>,
}

pub struct FollowSheet {
    spec: FollowSheetSpec,
    loaded: Option<Result<Loaded, String>>,
    view: View,
    /// The focused suggestion (Pick).
    pick: usize,
    cursor: usize,
    draft: Option<FollowRule>,
    /// The text field being typed in, its chip text, and whether egui focus must move to it.
    editing: Option<F>,
    buf: String,
    focus_field: bool,
    /// Enter in a chip field: add the chip after this frame's typing landed.
    commit: bool,
    scroll_to_cursor: bool,
    live: Option<Live>,
    rev: u64,
    asked: Option<FollowRule>,
    preview: Option<(u64, Preview)>,
    pending: Option<Ticket<FollowRuleId>>,
    error: Option<String>,
    /// Esc was pressed once with unsaved changes.
    warned: bool,
    save_requested: bool,
    /// The buttons' height on the last frame (the scrolling part leaves room for them).
    footer_h: Option<f32>,
}

impl FollowSheet {
    pub fn new(spec: FollowSheetSpec) -> Self {
        Self {
            spec,
            loaded: None,
            view: View::Pick,
            pick: 0,
            cursor: 0,
            draft: None,
            editing: None,
            buf: String::new(),
            focus_field: false,
            commit: false,
            scroll_to_cursor: false,
            live: None,
            rev: 0,
            asked: None,
            preview: None,
            pending: None,
            error: None,
            warned: false,
            save_requested: false,
            footer_h: None,
        }
    }

    fn load(&mut self, ctx: &mut UiCtx<'_>) {
        if self.loaded.is_some() {
            return;
        }
        let (db, lib, spec) = (ctx.db, ctx.lib.clone(), self.spec.clone());
        let h24 = ctx.config.interface.clock_24h;
        let headlines = ctx.config.follow.news_flash.headlines_min;
        let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
        let Some(r) = ctx.budget.run(|| load(db, &lib, &spec, h24, headlines, &spoilers)) else { return };
        self.loaded = Some(r.map_err(|e| crate::redact::redact(&e.to_string()).into_owned()));
        let Some(Ok(l)) = &self.loaded else { return };
        if let Some(stored) = &l.stored {
            self.draft = Some(stored.clone());
            self.view = View::Edit;
        } else if l.head.is_none() {
            // a blank rule: pick a template
            self.draft = None;
        } else {
            self.pick_suggestion(ctx, 0, None);
        }
    }

    fn loaded(&self) -> Option<&Loaded> {
        self.loaded.as_ref().and_then(|l| l.as_ref().ok())
    }

    fn stored(&self) -> Option<&FollowRule> {
        self.loaded().and_then(|l| l.stored.as_ref())
    }

    /// Focus suggestion `i`: the draft becomes its rule, keeping `when` if it fits its kind.
    fn pick_suggestion(&mut self, ctx: &UiCtx<'_>, i: usize, when: Option<When>) {
        let Some(s) = self.loaded().and_then(|l| l.suggestions.get(i)).cloned() else { return };
        self.pick = i;
        let cfg = &ctx.config.follow;
        let mut r = FollowRule::draft(s.name.clone(), s.kind, s.spec.terms.clone(), cfg, ctx.now);
        r.spec = s.spec.clone();
        if let Some(w) = when.filter(|w| whens(s.kind).contains(w)) {
            apply_when(&mut r, w, cfg.news_flash.headlines_min);
        }
        self.draft = Some(r);
    }

    /// The focusable rows of the current view, in order.
    fn fields(&self) -> Vec<F> {
        let mut v = Vec::new();
        match self.view {
            View::Pick => {
                let n = self.loaded().map_or(0, |l| l.suggestions.len());
                v.extend((0..n).map(F::Suggestion));
                if self.loaded().is_some_and(|l| l.head.is_some()) {
                    v.extend([F::When, F::MoreOptions, F::Cancel, F::Follow]);
                } else {
                    v.push(F::Cancel);
                }
            }
            View::Edit => {
                let Some(r) = &self.draft else { return vec![F::Cancel] };
                let news = r.kind == FollowKind::News;
                v.extend([F::Name, F::Terms, F::Without]);
                if matches!(r.kind, FollowKind::Event | FollowKind::Custom) || !r.spec.with_any.is_empty() {
                    v.push(F::WithAny);
                }
                if !news {
                    v.push(F::LookIn);
                }
                v.push(F::Shape);
                if !news {
                    v.push(F::Airings);
                }
                v.extend([F::Days, F::Channels, F::Mode]);
                match r.mode {
                    FollowMode::Notify => v.push(F::Lead),
                    FollowMode::Switch => v.push(F::Offset),
                    FollowMode::Flash => {
                        v.push(F::Length);
                        if matches!(r.spec.flash.map(|f| f.length), Some(FlashLength::Headlines(_))) {
                            v.push(F::Headlines);
                        }
                        v.extend([F::Volume, F::Offset]);
                    }
                    FollowMode::List => {}
                }
                v.extend([F::Spoiler, F::Limit]);
                if r.id.0 != 0 {
                    v.extend([F::Enabled, F::Delete, F::Duplicate, F::MoveUp, F::MoveDown]);
                }
                v.extend([F::Cancel, F::Save]);
            }
        }
        v
    }

    fn field(&self) -> Option<F> {
        let f = self.fields();
        f.get(self.cursor.min(f.len().saturating_sub(1))).copied()
    }

    fn focus(&mut self, f: F) {
        if let Some(i) = self.fields().iter().position(|x| *x == f) {
            self.cursor = i;
            self.scroll_to_cursor = true;
        }
    }

    fn move_cursor(&mut self, ctx: &UiCtx<'_>, delta: isize) {
        let n = self.fields().len();
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        self.scroll_to_cursor = true;
        self.after_move(ctx);
    }

    /// Moving onto a suggestion makes it the draft (keeping the When choice).
    fn after_move(&mut self, ctx: &UiCtx<'_>) {
        if let (View::Pick, Some(F::Suggestion(i))) = (self.view, self.field())
            && i != self.pick
            && self.loaded().is_some_and(|l| l.head.is_some())
        {
            let when = self.draft.as_ref().map(when_of);
            self.pick_suggestion(ctx, i, when);
        }
    }

    /// Unsaved: the editor holds a draft that differs from the stored rule (a new rule counts
    /// once it has a name to look for). The Pick view holds nothing typed.
    fn dirty(&self) -> bool {
        if self.view != View::Edit {
            return false;
        }
        match (&self.draft, self.stored()) {
            (Some(d), Some(s)) => d != s,
            (Some(d), None) => d.spec.terms.iter().any(|t| !t.trim().is_empty()) || !self.buf.trim().is_empty(),
            (None, _) => false,
        }
    }

    fn start_editing(&mut self, f: F) {
        self.editing = Some(f);
        self.focus_field = true;
        self.buf.clear();
    }

    fn stop_editing(&mut self) {
        self.add_chip();
        self.editing = None;
    }

    /// The chip being typed joins its field (names, leave out, also needs).
    fn add_chip(&mut self) {
        let text = std::mem::take(&mut self.buf);
        let text = text.trim();
        let (Some(f), Some(r)) = (self.editing, self.draft.as_mut()) else { return };
        if text.is_empty() || f == F::Name {
            return;
        }
        let list = match f {
            F::Terms => &mut r.spec.terms,
            F::Without => &mut r.spec.without,
            F::WithAny => &mut r.spec.with_any,
            _ => return,
        };
        if !list.iter().any(|x| x.eq_ignore_ascii_case(text)) {
            list.push(text.to_owned());
        }
        if f == F::Terms && r.name.trim().is_empty() {
            r.name = text.to_owned();
        }
    }

    fn remove_chip(&mut self, f: F, i: Option<usize>) {
        let Some(r) = self.draft.as_mut() else { return };
        let list = match f {
            F::Terms => &mut r.spec.terms,
            F::Without => &mut r.spec.without,
            F::WithAny => &mut r.spec.with_any,
            _ => return,
        };
        match i {
            Some(i) if i < list.len() => {
                list.remove(i);
            }
            None => {
                list.pop();
            }
            _ => {}
        }
    }

    /// ←/→ (and Enter) on a value row.
    fn adjust(&mut self, ctx: &UiCtx<'_>, f: F, dir: i32) {
        let headlines = ctx.config.follow.news_flash.headlines_min;
        let channel_opts = channel_options(ctx, self.draft.as_ref().map(|r| &r.spec.channels));
        let Some(r) = self.draft.as_mut() else { return };
        let cycle = |n: usize, i: usize| ((i as i32 + dir).rem_euclid(n as i32)) as usize;
        match f {
            F::When | F::Mode => {
                let mut opts: Vec<When> = whens(r.kind).to_vec();
                if f == F::Mode && r.kind != FollowKind::News {
                    opts = vec![When::Remind, When::Switch, When::List];
                }
                let i = opts.iter().position(|w| *w == when_of(r)).unwrap_or(0);
                apply_when(r, opts[cycle(opts.len(), i)], headlines);
            }
            F::LookIn => {
                let opts = look_in_options();
                let i = opts.iter().position(|(fl, _)| *fl == r.spec.fields).unwrap_or(0);
                r.spec.fields = opts[cycle(opts.len(), i)].0;
            }
            F::Shape => {
                let i = fui::SHAPES.iter().position(|s| *s == r.spec.shape).unwrap_or(0);
                r.spec.shape = fui::SHAPES[cycle(fui::SHAPES.len(), i)].clone();
            }
            F::Airings => {
                const A: [Airings; 4] = [Airings::LiveOnly, Airings::LiveOrNew, Airings::NewOnly, Airings::Any];
                let i = A.iter().position(|a| *a == r.spec.airings).unwrap_or(0);
                r.spec.airings = A[cycle(A.len(), i)];
            }
            F::Days => {
                const D: [u8; 3] = [0, WEEKDAYS, fui::WEEKENDS];
                let i = D.iter().position(|d| *d == r.spec.days).unwrap_or(0);
                r.spec.days = D[cycle(D.len(), i)];
            }
            F::Channels => {
                let i = channel_opts.iter().position(|(c, _)| *c == r.spec.channels).unwrap_or(0);
                r.spec.channels = channel_opts[cycle(channel_opts.len(), i)].0.clone();
            }
            F::Lead => r.lead_s = (r.lead_s / 60 + i64::from(dir)).clamp(0, 60) * 60,
            F::Offset => r.offset_s = (r.offset_s / 60 + 5 * i64::from(dir)).clamp(0, 120) * 60,
            F::Length => {
                let w = if when_of(r) == When::FlashWhole { When::FlashHeadlines } else { When::FlashWhole };
                apply_when(r, w, headlines);
            }
            F::Headlines => {
                if let Some(FlashSpec { length: FlashLength::Headlines(m), volume }) = r.spec.flash {
                    let m = (i32::from(m) + dir).clamp(1, 30) as u16;
                    r.spec.flash = Some(FlashSpec { length: FlashLength::Headlines(m), volume });
                }
            }
            F::Volume => {
                const V: [FlashVolume; 2] = [FlashVolume::Normal, FlashVolume::Mute];
                let mut fl = r.spec.flash.unwrap_or_default();
                let i = V.iter().position(|v| *v == fl.volume).unwrap_or(0);
                fl.volume = V[cycle(V.len(), i)];
                r.spec.flash = Some(fl);
            }
            F::Spoiler => r.spoiler_safe = !r.spoiler_safe,
            F::Enabled => r.enabled = !r.enabled,
            F::Limit => r.max_per_week = (i64::from(r.max_per_week) + i64::from(dir)).clamp(1, 50) as u32,
            _ => {}
        }
    }

    /// Enter / a click on a row or button.
    fn activate(&mut self, ctx: &mut UiCtx<'_>, f: F) -> OverlayOutcome {
        match f {
            F::Suggestion(i) => {
                let template = self.loaded().is_some_and(|l| l.head.is_none());
                if template {
                    self.pick_suggestion(ctx, i, None);
                    self.open_editor(F::Terms);
                    return OverlayOutcome::Keep;
                }
                if i != self.pick {
                    let when = self.draft.as_ref().map(when_of);
                    self.pick_suggestion(ctx, i, when);
                }
                self.save_requested = true;
            }
            // Enter on the When row follows the focused suggestion
            F::When | F::Follow | F::Save => self.save_requested = true,
            F::MoreOptions => self.open_editor(F::Name),
            F::Cancel => return OverlayOutcome::Close,
            F::Name | F::Terms | F::Without | F::WithAny => self.start_editing(f),
            F::Delete => self.ask_delete(ctx),
            F::Duplicate => {
                if let Some(r) = self.draft.as_mut() {
                    r.id = FollowRuleId(0);
                    r.name = format!("{} (copy)", r.name.trim());
                    r.priority = i64::MAX;
                    r.by_agent = false;
                    r.enabled = true;
                }
                if let Some(Ok(l)) = self.loaded.as_mut() {
                    l.stored = None;
                }
                self.focus(F::Name);
                self.start_editing(F::Name);
            }
            F::MoveUp | F::MoveDown => self.reorder(ctx, if f == F::MoveUp { -1 } else { 1 }),
            _ => self.adjust(ctx, f, 1),
        }
        OverlayOutcome::Keep
    }

    fn open_editor(&mut self, focus: F) {
        self.view = View::Edit;
        self.warned = false;
        self.focus(focus);
        if focus.is_text() {
            self.start_editing(focus);
        }
    }

    fn ask_delete(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(l) = self.loaded() else { return };
        let Some(r) = &l.stored else { return };
        let n = l.owned;
        let mi = |label: String, code| MenuItem { icon: glyphs::CLOSE, label, action: Action::Digit(code), enabled: true, shortcut: None };
        let mut items = vec![mi(
            if n == 0 { "Delete the rule".into() } else { format!("Delete it and cancel its {n} reminder{}", if n == 1 { "" } else { "s" }) },
            DELETE_CANCEL,
        )];
        if n > 0 {
            items.push(mi(format!("Delete it, keep the reminder{} as my own", if n == 1 { "" } else { "s" }), DELETE_KEEP));
        }
        ctx.open(crate::ui::OverlayRequest::ContextMenu(ContextMenuSpec { title: format!("Stop following {}?", r.name), items }));
    }

    fn delete(&mut self, ctx: &mut UiCtx<'_>, keep: bool) -> OverlayOutcome {
        let Some(r) = self.stored().cloned() else { return OverlayOutcome::Keep };
        fui::delete_rule(ctx.services, r.id, r.name, keep);
        OverlayOutcome::Close
    }

    /// Move the stored rule up or down the list now (higher wins a clash).
    fn reorder(&mut self, ctx: &mut UiCtx<'_>, delta: isize) {
        let Some(id) = self.stored().map(|r| r.id) else { return };
        let Some(Ok(l)) = self.loaded.as_mut() else { return };
        let Some(i) = l.order.iter().position(|x| *x == id) else { return };
        let j = (i as isize + delta).clamp(0, l.order.len() as isize - 1) as usize;
        if i == j {
            ctx.toast(Toast::info(if delta < 0 { "Already first: it wins every clash" } else { "Already last" }));
            return;
        }
        let v = l.order.remove(i);
        l.order.insert(j, v);
        fui::reorder_rules(ctx.services, l.order.clone(), ctx.now);
        ctx.toast(Toast::info(format!("Moved to {} of {}", j + 1, l.order.len())));
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) {
        if self.pending.is_some() {
            return;
        }
        let Some(mut r) = self.draft.clone() else { return };
        if r.name.trim().is_empty() {
            r.name = r.spec.terms.first().cloned().unwrap_or_default();
        }
        if r.name.trim().is_empty() || r.spec.problem().is_some() {
            self.error = Some("Add a name to look for first (type it, then Enter)".into());
            if self.view == View::Pick {
                self.open_editor(F::Terms);
            } else {
                self.focus(F::Terms);
            }
            return;
        }
        if let Some(why) = r.spec.problem() {
            self.error = Some(why.to_owned());
            return;
        }
        self.error = None;
        self.pending = Some(fui::save_rule(ctx, r));
    }

    /// The saved rule's toast: "Following Arsenal · 4 in the guide".
    fn saved_toast(&self) -> Toast {
        let Some(r) = &self.draft else { return Toast::info("Saved") };
        if self.stored().is_some() {
            return Toast::info(format!("Saved {}", r.name.trim()));
        }
        let name = if r.name.trim().is_empty() { r.spec.terms.first().map_or("", String::as_str) } else { r.name.trim() };
        let found = self.current_preview().map(|p| p.matches.len());
        let tail = match found {
            Some(0) | None => " — Tuner checks the guide after every refresh".to_owned(),
            Some(n) => format!(" · {n} in the guide"),
        };
        let what = if r.kind == FollowKind::News { "News flash" } else { "Following" };
        Toast::info(format!("{what}: {name}{tail}"))
    }

    /// Ask for a preview of the draft when it changed (debounced by the service).
    fn ask_preview(&mut self, egui_ctx: &egui::Context, ctx: &UiCtx<'_>) {
        if !crate::follow::ENABLED {
            return;
        }
        let Some(r) = &self.draft else { return };
        if r.spec.problem().is_some() || self.asked.as_ref() == Some(r) {
            return;
        }
        let live = self.live.get_or_insert_with(|| {
            let (bus, rx) = EventBus::new(Some(egui_ctx.clone()));
            let s = ctx.services;
            Live { service: PreviewService::start(s.rt.handle(), s.store.clone(), s.agent.clone(), bus, s.config.clone()), rx }
        });
        self.rev += 1;
        live.service.request(self.rev, r.clone(), PREVIEW_DAYS);
        self.asked = Some(r.clone());
    }

    fn poll_preview(&mut self) {
        let Some(live) = &self.live else { return };
        while let Ok(ev) = live.rx.try_recv() {
            if let AppEvent::Follow(FollowEvent::PreviewReady { rev, preview }) = ev
                && self.preview.as_ref().is_none_or(|(r, _)| *r < rev)
            {
                self.preview = Some((rev, preview));
            }
        }
    }

    /// The preview of the draft as it is now (`None` while it is being counted).
    fn current_preview(&self) -> Option<&Preview> {
        let (rev, p) = self.preview.as_ref()?;
        (*rev == self.rev && self.asked.is_some() && self.asked == self.draft).then_some(p)
    }

    /// "4 in the guide · next Sat 12:30 · spoiler-safe on", or "counting…".
    fn count_line(&self, ctx: &UiCtx<'_>) -> String {
        let Some(r) = &self.draft else { return String::new() };
        let safe = if r.spoiler_safe { " · spoiler-safe on" } else { "" };
        match self.current_preview() {
            Some(p) => match p.matches.first() {
                None => format!("Nothing in the next {PREVIEW_DAYS} days yet — checked after every guide refresh{safe}"),
                Some(first) => {
                    let n = p.matches.len();
                    let more = if n >= 50 { "50+".to_owned() } else { n.to_string() };
                    format!("{more} in the guide · next {}{safe}", crate::ui::widgets::time::day_time(first.start, ctx.config.interface.clock_24h))
                }
            },
            None if self.asked.is_none() => String::new(),
            None => "counting…".into(),
        }
    }
}

/// Read the programme (and its suggestions) or the rule the sheet opens on. The header shows
/// the programme as the spoiler guard lets it (the suggestions only name the sides).
fn load(
    db: &rusqlite::Connection,
    lib: &crate::store::library::Library,
    spec: &FollowSheetSpec,
    h24: bool,
    headlines: u16,
    spoilers: &crate::ui::widgets::spoiler::Spoilers,
) -> crate::error::Result<Loaded> {
    let rules = crate::store::follow::rules(db)?;
    let order = rules.iter().map(|r| r.id).collect();
    if let Some(id) = spec.rule {
        let stored = rules.into_iter().find(|r| r.id == id).ok_or_else(|| crate::error::Error::other("That rule no longer exists"))?;
        let owned = crate::store::user::all_reminders(db)?.iter().filter(|m| m.rule_id == Some(id) && m.state == ReminderState::Pending).count();
        return Ok(Loaded { head: None, suggestions: Vec::new(), stored: Some(stored), order, owned });
    }
    let Some((pid, ch)) = spec.programme else {
        return Ok(Loaded { head: None, suggestions: templates(headlines), stored: None, order, owned: 0 });
    };
    let p = crate::store::epg::programme(db, pid)?;
    let channel = lib.live.get(ch);
    let shift = channel.map_or(0, |c| i64::from(c.tvg_shift_min) * 60);
    let subject = crate::ui::widgets::spoiler::programme_subject(&p);
    let shown = spoilers.text(&subject, crate::guard::Field::Title, &p.title).map(|t| t.into_owned()).unwrap_or_default();
    let sub = p.sub_title.as_deref().filter(|s| !s.trim().is_empty()).and_then(|s| spoilers.text(&subject, crate::guard::Field::SubTitle, s));
    let title = match sub {
        Some(sub) => format!("{shown} · {sub}"),
        None => shown,
    };
    let when = crate::ui::widgets::time::local(p.start + shift, if h24 { "%a %-d %b %H:%M" } else { "%a %-d %b %-I:%M %p" }, "");
    let detail = match channel {
        Some(c) => format!("{when} · {}", c.name),
        None => when,
    };
    let suggestions = crate::follow::suggest::suggest(&p.title, p.sub_title.as_deref(), &p.categories, p.episode.is_some(), headlines);
    Ok(Loaded { head: Some((title, detail)), suggestions, stored: None, order, owned: 0 })
}

/// "Look in" presets.
fn look_in_options() -> [(Fields, &'static str); 4] {
    let f = |title, sub_title, description, categories| Fields { title, sub_title, description, categories };
    [
        (f(true, false, false, false), "title"),
        (f(true, true, false, false), "title, episode title"),
        (f(true, true, true, false), "title, episode title, description"),
        (f(true, true, true, true), "everything, categories too"),
    ]
}

/// The channel scopes the editor offers: any, favourites, each playlist — and the rule's own
/// when it is something else (a list or channels Claude chose).
fn channel_options(ctx: &UiCtx<'_>, current: Option<&ChannelScope>) -> Vec<(ChannelScope, String)> {
    let mut v = vec![(ChannelScope::Any, "Any I can watch".to_owned()), (ChannelScope::Favourites, "My favourites".to_owned())];
    v.extend(ctx.lib.sources.iter().filter(|s| s.enabled).map(|s| (ChannelScope::Playlist(s.id), format!("Only {}", s.name))));
    if let Some(c) = current
        && !v.iter().any(|(x, _)| x == c)
    {
        let label = match c {
            ChannelScope::List(id) => {
                ctx.favs.lists.iter().find(|l| l.id == *id).map_or("A favourites list".into(), |l| format!("The list {}", l.name))
            }
            ChannelScope::Channels(chs) => format!("{} chosen channel{}", chs.len(), if chs.len() == 1 { "" } else { "s" }),
            _ => "As set".into(),
        };
        v.push((c.clone(), label));
    }
    v
}

impl Overlay for FollowSheet {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.editing.is_some() { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    fn is_dirty(&self) -> bool {
        self.dirty()
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.load(ctx);
        if self.editing.is_some() {
            return match action {
                // Enter is acted on in `ui`, after this frame's typing landed (keys are dispatched
                // before the field takes the frame's text): a chip field adds the chip and stays,
                // an empty chip field or the name is left
                Action::Activate => {
                    self.commit = true;
                    OverlayOutcome::Keep
                }
                Action::Back => {
                    self.buf.clear();
                    self.stop_editing();
                    OverlayOutcome::Keep
                }
                Action::FocusNext | Action::FocusPrev => {
                    self.stop_editing();
                    self.move_cursor(ctx, if action == Action::FocusNext { 1 } else { -1 });
                    OverlayOutcome::Keep
                }
                _ => OverlayOutcome::Ignored,
            };
        }
        if action != Action::Back {
            self.warned = false;
        }
        let Some(f) = self.field() else {
            return if action == Action::Back { OverlayOutcome::Close } else { OverlayOutcome::Ignored };
        };
        match action {
            Action::Up => self.move_cursor(ctx, -1),
            Action::Down => self.move_cursor(ctx, 1),
            // Tab: from the suggestions to the buttons, then along them
            Action::FocusNext if self.view == View::Pick && !f.is_button() => {
                if let Some(i) = self.fields().iter().position(|x| x.is_button()) {
                    self.cursor = i;
                }
            }
            Action::FocusNext => self.move_cursor(ctx, 1),
            Action::FocusPrev => self.move_cursor(ctx, -1),
            Action::Left | Action::Right if f.is_button() => self.move_cursor(ctx, if action == Action::Left { -1 } else { 1 }),
            Action::Left | Action::Right => {
                let dir = if action == Action::Left { -1 } else { 1 };
                match f {
                    F::Suggestion(_) => {}
                    F::Name | F::Terms | F::Without | F::WithAny => {}
                    _ => self.adjust(ctx, f, dir),
                }
            }
            Action::Activate => return self.activate(ctx, f),
            Action::Back => {
                if self.dirty() && !self.warned {
                    self.warned = true;
                    self.focus(F::Save);
                    return OverlayOutcome::Keep;
                }
                return OverlayOutcome::Close;
            }
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    /// The Delete… menu's answer.
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) -> OverlayOutcome {
        match result {
            OverlayResult::Chosen(Action::Digit(DELETE_CANCEL)) => self.delete(ctx, false),
            OverlayResult::Chosen(Action::Digit(DELETE_KEEP)) => self.delete(ctx, true),
            _ => OverlayOutcome::Keep,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.load(ctx);
        if let Some(r) = self.pending.as_mut().and_then(Ticket::try_take) {
            self.pending = None;
            match r {
                Ok(id) => {
                    let mut toast = self.saved_toast();
                    // a new rule can be taken back at once (its reminders go with it)
                    if self.stored().is_none() {
                        toast.action = Some(("Undo".into(), Action::UndoFollow(id)));
                    }
                    ctx.toast(toast);
                    return OverlayOutcome::Close;
                }
                Err(e) => self.error = Some(format!("Couldn't save: {}", crate::redact::redact(&e.to_string()))),
            }
        } else if self.pending.is_some() {
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        self.poll_preview();
        self.ask_preview(egui_ctx, ctx);
        let theme = ctx.theme;
        if scrim_guarded(egui_ctx, theme, "follow-sheet", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        let fields = self.fields();
        self.cursor = self.cursor.min(fields.len().saturating_sub(1));
        let cur = fields.get(self.cursor).copied();
        let width = if self.view == View::Edit { 680.0 } else { 560.0 } * theme.rem();
        let mut clicked: Option<(F, i32)> = None;
        let mut outcome = OverlayOutcome::Keep;
        card_area(egui_ctx, theme, "follow-sheet", CardKind::Popup, width, None, |ui| {
            match &self.loaded {
                None => {
                    ui.label(RichText::new("Loading…").size(theme.font(TypeToken::Body)).color(theme.tokens.text_secondary));
                    return;
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e.as_str()).size(theme.font(TypeToken::Body)).color(theme.tokens.urgent));
                    if button(ui, theme, None, "Close", ButtonStyle { bordered: true, focused: true, ..Default::default() }).clicked() {
                        outcome = OverlayOutcome::Close;
                    }
                    return;
                }
                Some(Ok(_)) => {}
            }
            // what the scrolling part leaves below it: the buttons, a warning and an error line
            // (measured on the last frame; the first frame estimates it)
            let line = theme.font(TypeToken::BodySmall) * 1.5;
            let footer = self.footer_h.unwrap_or(2.0 * theme.space(SpaceToken::ControlHeight) + 2.0 * theme.space(SpaceToken::Lg) + 2.0 * line);
            match self.view {
                View::Pick => self.draw_pick(ui, ctx, cur, &mut clicked, footer),
                View::Edit => self.draw_edit(ui, ctx, cur, &mut clicked, footer),
            }
            self.draw_footer(ui, theme, &fields, cur, &mut clicked);
        });
        self.scroll_to_cursor = false;
        if std::mem::take(&mut self.commit) {
            if self.editing.is_some_and(|f| f != F::Name) && !self.buf.trim().is_empty() {
                self.add_chip();
            } else {
                self.stop_editing();
            }
        }
        if let Some((f, dir)) = clicked {
            self.warned = false;
            if let Some(i) = fields.iter().position(|x| *x == f) {
                self.cursor = i;
            }
            outcome = if dir == SELECT {
                self.after_move(ctx);
                OverlayOutcome::Keep
            } else if dir == 0 {
                self.activate(ctx, f)
            } else {
                self.adjust(ctx, f, dir);
                OverlayOutcome::Keep
            };
        }
        if std::mem::take(&mut self.save_requested) {
            self.save(ctx);
        }
        outcome
    }
}

// ------------------------------------------------------------------ drawing

impl FollowSheet {
    fn draw_pick(&mut self, ui: &mut egui::Ui, ctx: &UiCtx<'_>, cur: Option<F>, clicked: &mut Option<(F, i32)>, footer: f32) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(l) = self.loaded() else { return };
        match &l.head {
            Some((title, detail)) => {
                ui.label(RichText::new("Follow").size(theme.font(TypeToken::Caption)).strong().color(t.text_section));
                ui.add(egui::Label::new(RichText::new(title).size(theme.font(TypeToken::Heading)).color(t.fg)).wrap());
                ui.label(RichText::new(detail).size(theme.font(TypeToken::Body)).color(t.text_secondary));
            }
            None => {
                ui.label(RichText::new("Follow…").size(theme.font(TypeToken::Heading)).color(t.fg));
                ui.add(
                    egui::Label::new(
                        RichText::new("What should Tuner look for in the guide after every refresh?")
                            .size(theme.font(TypeToken::Body))
                            .color(t.text_secondary),
                    )
                    .wrap(),
                );
            }
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let suggestions: Vec<(String, FollowKind)> = l.suggestions.iter().map(|s| (s.label.clone(), s.kind)).collect();
        let has_head = l.head.is_some();
        // below the list: the When row and two lines of counts / warnings
        let after = if has_head {
            theme.space(SpaceToken::ControlHeight) + 2.0 * theme.space(SpaceToken::Sm) + 3.0 * theme.font(TypeToken::BodySmall) * 1.5
        } else {
            0.0
        };
        let max_h = (ui.available_height() - footer - after).max(2.0 * theme.space(SpaceToken::ControlHeight));
        egui::ScrollArea::vertical().id_salt("follow-pick").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            for (i, (label, kind)) in suggestions.iter().enumerate() {
                let focused = cur == Some(F::Suggestion(i));
                let chosen = has_head && i == self.pick;
                let r = row(ui, theme, focused, self.scroll_to_cursor, |ui| {
                    let mark = if chosen { glyphs::CHEVRON_RIGHT } else { "" };
                    ui.label(RichText::new(mark).size(theme.font(TypeToken::Icon)).color(t.accent));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(fui::kind_text(*kind)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let color = if focused || chosen { t.list_cursor_text } else { t.fg };
                            ui.add(egui::Label::new(RichText::new(label).size(theme.font(TypeToken::Body)).color(color)).truncate());
                        });
                    });
                });
                // a click selects a suggestion (Follow then follows it); a template opens the editor
                if r.clicked() {
                    *clicked = Some((F::Suggestion(i), if has_head { SELECT } else { 0 }));
                }
            }
        });
        if !has_head {
            return;
        }
        section::separator(ui, theme);
        ui.add_space(theme.space(SpaceToken::Sm));
        if let Some(r) = self.draft.clone() {
            let focused = cur == Some(F::When);
            value_row(ui, theme, "When", &when_text(&r, when_of(&r)), focused, self.scroll_to_cursor, F::When, clicked);
            ui.add_space(theme.space(SpaceToken::Sm));
            ui.add(egui::Label::new(RichText::new(self.count_line(ctx)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).wrap());
            if let Some(w) = self.current_preview().and_then(|p| p.warnings.first()) {
                ui.add(egui::Label::new(RichText::new(w).size(theme.font(TypeToken::BodySmall)).color(t.warning)).wrap());
            }
        }
    }

    fn draw_edit(&mut self, ui: &mut egui::Ui, ctx: &UiCtx<'_>, cur: Option<F>, clicked: &mut Option<(F, i32)>, footer: f32) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(r) = self.draft.clone() else { return };
        let new = r.id.0 == 0;
        let heading = if new {
            if r.kind == FollowKind::News { "New news flash".to_owned() } else { format!("Follow · {}", fui::kind_text(r.kind)) }
        } else {
            r.name.clone()
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(heading).size(theme.font(TypeToken::Heading)).color(t.fg));
            if r.by_agent {
                ui.label(RichText::new("added by Claude").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            }
        });
        ui.add_space(theme.space(SpaceToken::Md));
        let scroll = self.scroll_to_cursor;
        let fields = self.fields();
        let channel_opts = channel_options(ctx, Some(&r.spec.channels));
        let max_h = (ui.available_height() - footer).max(3.0 * theme.space(SpaceToken::ControlHeight));
        egui::ScrollArea::vertical().id_salt("follow-edit").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            for f in fields.iter().copied().filter(|f| !f.is_button()) {
                let focused = cur == Some(f);
                match f {
                    F::Name => self.draw_name(ui, theme, focused, scroll, clicked),
                    F::Terms => self.draw_chips(ui, theme, f, "Names", "+ a name or alias, Enter", &r.spec.terms, focused, scroll, clicked),
                    F::Without => self.draw_chips(ui, theme, f, "Leave out", "+ a word, Enter", &r.spec.without, focused, scroll, clicked),
                    F::WithAny => self.draw_chips(ui, theme, f, "Also needs", "+ e.g. Qualifying, Enter", &r.spec.with_any, focused, scroll, clicked),
                    F::LookIn => {
                        let v = look_in_options().iter().find(|(fl, _)| *fl == r.spec.fields).map_or("custom", |(_, s)| s);
                        value_row(ui, theme, "Look in", v, focused, scroll, f, clicked);
                    }
                    F::Shape => value_row(ui, theme, "Match", &fui::shape_text(&r.spec.shape, r.name.trim()), focused, scroll, f, clicked),
                    F::Airings => {
                        let v = match r.spec.airings {
                            Airings::LiveOnly => "Live only",
                            Airings::LiveOrNew => "Live or new",
                            Airings::NewOnly => "New only",
                            Airings::Any => "Every airing",
                        };
                        value_row(ui, theme, "Airings", v, focused, scroll, f, clicked);
                    }
                    F::Days => {
                        let v = match r.spec.days {
                            0 => "Every day",
                            WEEKDAYS => "Weekdays",
                            fui::WEEKENDS => "Weekends",
                            _ => "Some days",
                        };
                        value_row(ui, theme, "Days", v, focused, scroll, f, clicked);
                    }
                    F::Channels => {
                        let v = channel_opts.iter().find(|(c, _)| *c == r.spec.channels).map_or("Any I can watch", |(_, s)| s.as_str());
                        value_row(ui, theme, "Channels", v, focused, scroll, f, clicked);
                    }
                    F::Mode => value_row(ui, theme, "When", &when_text(&r, when_of(&r)), focused, scroll, f, clicked),
                    F::Lead => value_row(ui, theme, "Before", &format!("{} min", r.lead_s / 60), focused, scroll, f, clicked),
                    F::Offset => {
                        let v = if r.offset_s == 0 { "At the start".to_owned() } else { format!("{} min in (skip the build-up)", r.offset_s / 60) };
                        value_row(ui, theme, "Start", &v, focused, scroll, f, clicked);
                    }
                    F::Length => {
                        let v = if when_of(&r) == When::FlashWhole { "Whole programme" } else { "Headlines only" };
                        value_row(ui, theme, "Length", v, focused, scroll, f, clicked);
                    }
                    F::Headlines => {
                        let m = match r.spec.flash.map(|x| x.length) {
                            Some(FlashLength::Headlines(m)) => m,
                            _ => 0,
                        };
                        value_row(ui, theme, "Headlines", &format!("first {m} min, then close"), focused, scroll, f, clicked);
                    }
                    F::Volume => {
                        value_row(ui, theme, "Sound", fui::volume_text(r.spec.flash.unwrap_or_default().volume), focused, scroll, f, clicked)
                    }
                    F::Spoiler => {
                        let v = if r.spoiler_safe { "On: no episode titles, descriptions or results; late = from the start" } else { "Off" };
                        value_row(ui, theme, "Spoiler-safe", v, focused, scroll, f, clicked);
                    }
                    F::Limit => value_row(ui, theme, "Limit", &format!("{} a week", r.max_per_week), focused, scroll, f, clicked),
                    F::Enabled => value_row(ui, theme, "Rule", if r.enabled { "On" } else { "Paused" }, focused, scroll, f, clicked),
                    _ => {}
                }
            }
            ui.add_space(theme.space(SpaceToken::Md));
            self.draw_preview(ui, ctx);
        });
    }

    fn draw_name(&mut self, ui: &mut egui::Ui, theme: &Theme, focused: bool, scroll: bool, clicked: &mut Option<(F, i32)>) {
        let t = &theme.tokens;
        let editing = self.editing == Some(F::Name);
        let focus_now = editing && std::mem::take(&mut self.focus_field);
        let mut stop = false;
        let r = row(ui, theme, focused && !editing, scroll, |ui| {
            label_cell(ui, theme, "Name", focused);
            let Some(d) = self.draft.as_mut() else { return };
            if editing {
                let id = egui::Id::new("follow-name");
                let resp = ui.scope_builder(egui::UiBuilder::new().id(id), |ui| text_field(ui, theme, &mut d.name, "Arsenal", false)).inner;
                if focus_now {
                    resp.request_focus();
                } else if !resp.has_focus() {
                    stop = true;
                }
            } else {
                let shown = if d.name.trim().is_empty() { "(the first name below)" } else { d.name.as_str() };
                ui.label(RichText::new(shown).size(theme.font(TypeToken::Body)).color(if focused { t.list_cursor_text } else { t.fg }));
            }
        });
        if stop {
            self.editing = None;
        } else if r.clicked() && !editing {
            *clicked = Some((F::Name, 0));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chips(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        f: F,
        label: &str,
        hint: &str,
        chips: &[String],
        focused: bool,
        scroll: bool,
        clicked: &mut Option<(F, i32)>,
    ) {
        let t = &theme.tokens;
        let editing = self.editing == Some(f);
        let focus_now = editing && std::mem::take(&mut self.focus_field);
        let mut remove: Option<usize> = None;
        let mut stop = false;
        let mut backspace_empty = false;
        let r = row(ui, theme, focused && !editing, scroll, |ui| {
            label_cell(ui, theme, label, focused);
            ui.horizontal_wrapped(|ui| {
                for (i, c) in chips.iter().enumerate() {
                    if button(ui, theme, Some(glyphs::CLOSE), c, ButtonStyle { bordered: true, ..Default::default() })
                        .on_hover_text(format!("Remove {c}"))
                        .clicked()
                    {
                        remove = Some(i);
                    }
                }
                if editing {
                    let empty_before = self.buf.is_empty();
                    ui.allocate_ui(
                        egui::vec2((220.0 * theme.rem()).min(ui.available_width().max(80.0)), theme.space(SpaceToken::ControlHeight)),
                        |ui| {
                            // a stable id: the chips before it come and go while it has focus
                            let id = egui::Id::new(("follow-chip", label));
                            let resp = ui.scope_builder(egui::UiBuilder::new().id(id), |ui| text_field(ui, theme, &mut self.buf, hint, false)).inner;
                            if focus_now {
                                resp.request_focus();
                            } else if !resp.has_focus() {
                                stop = true;
                            }
                            backspace_empty = empty_before && resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Backspace));
                        },
                    );
                } else if chips.is_empty() || focused {
                    ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_disabled));
                }
            });
        });
        if let Some(i) = remove {
            self.remove_chip(f, Some(i));
        } else if backspace_empty {
            self.remove_chip(f, None);
        } else if stop {
            self.stop_editing();
        } else if r.clicked() && !editing {
            *clicked = Some((f, 0));
        }
    }

    fn draw_preview(&self, ui: &mut egui::Ui, ctx: &UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let h24 = ctx.config.interface.clock_24h;
        let current = self.current_preview().is_some();
        let nothing_to_look_for = self.draft.as_ref().is_none_or(|r| r.spec.problem().is_some());
        let p = match &self.preview {
            _ if nothing_to_look_for => {
                section::section_header(ui, theme, "In the guide now · add a name to look for");
                return;
            }
            None => {
                section::section_header(ui, theme, "In the guide now · counting…");
                return;
            }
            Some((_, p)) => p,
        };
        let mut head = format!("In the guide now · {} match", p.matches.len());
        if !p.left_out.is_empty() {
            head.push_str(&format!(" · {} left out", p.left_out.len()));
        }
        if !current {
            head.push_str(" · updating…");
        }
        section::section_header(ui, theme, &head);
        for w in &p.warnings {
            ui.add(egui::Label::new(RichText::new(w).size(theme.font(TypeToken::BodySmall)).color(t.warning)).wrap());
        }
        let wide = ctx.layout == crate::ui::LayoutClass::Wide;
        let line = |ui: &mut egui::Ui, u: &crate::follow::plan::Upcoming, ok: bool| {
            ui.horizontal(|ui| {
                let when =
                    format!("{} {}", crate::ui::widgets::time::local(u.start, "%a %-d %b", ""), crate::ui::widgets::time::span(u.start, u.stop, h24));
                ui.label(RichText::new(when).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (g, c) = if ok { (glyphs::CHECK, t.success) } else { (glyphs::CLOSE, t.text_disabled) };
                    ui.label(RichText::new(g).size(theme.font(TypeToken::Icon)).color(c));
                    let mut where_ = u.channel_name.clone();
                    if wide && !u.alternates.is_empty() {
                        where_.push_str(&format!(" · also on {}", u.alternates.len()));
                    }
                    let state = fui::state_text(u);
                    let right = if ok { where_ } else { state };
                    ui.label(RichText::new(right).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let title = match u.sub_title.as_deref().filter(|s| !s.is_empty()) {
                            Some(s) => format!("{} · {s}", u.title),
                            None => u.title.clone(),
                        };
                        ui.add(
                            egui::Label::new(RichText::new(title).size(theme.font(TypeToken::Body)).color(if ok { t.fg } else { t.text_disabled }))
                                .truncate(),
                        );
                    });
                });
            });
        };
        for u in p.matches.iter().take(PREVIEW_ROWS) {
            let ok = !matches!(u.state, crate::follow::plan::MatchState::OverCap | crate::follow::plan::MatchState::Skipped);
            line(ui, u, ok);
        }
        if p.matches.len() > PREVIEW_ROWS {
            ui.label(
                RichText::new(format!("+{} more", p.matches.len() - PREVIEW_ROWS)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary),
            );
        }
        for u in p.left_out.iter().take(PREVIEW_ROWS / 2) {
            line(ui, u, false);
        }
    }

    fn draw_footer(&mut self, ui: &mut egui::Ui, theme: &Theme, fields: &[F], cur: Option<F>, clicked: &mut Option<(F, i32)>) {
        let t = &theme.tokens;
        if self.warned {
            ui.add_space(theme.space(SpaceToken::Sm));
            ui.label(RichText::new("Unsaved rule changes: Enter on Save, or Esc to discard").size(theme.font(TypeToken::BodySmall)).color(t.warning));
        }
        if let Some(e) = &self.error {
            ui.add_space(theme.space(SpaceToken::Sm));
            ui.add(egui::Label::new(RichText::new(e.as_str()).size(theme.font(TypeToken::BodySmall)).color(t.urgent)).wrap());
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let top = ui.cursor().top();
        self.draw_buttons(ui, theme, fields, cur, clicked);
        // what the scrolling part leaves for this next frame
        self.footer_h = Some(ui.min_rect().bottom() - top + theme.space(SpaceToken::Lg) + 2.0 * theme.font(TypeToken::BodySmall) * 1.5);
    }

    fn draw_buttons(&mut self, ui: &mut egui::Ui, theme: &Theme, fields: &[F], cur: Option<F>, clicked: &mut Option<(F, i32)>) {
        let saving = self.pending.is_some();
        let new = self.draft.as_ref().is_none_or(|r| r.id.0 == 0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
            // left: the stored rule's own buttons
            for f in fields.iter().copied().filter(|f| matches!(f, F::Delete | F::Duplicate | F::MoveUp | F::MoveDown)) {
                let focused = cur == Some(f);
                let resp = match f {
                    F::Delete => button(ui, theme, None, "Delete…", ButtonStyle { bordered: true, danger: true, focused, ..Default::default() }),
                    F::Duplicate => button(ui, theme, None, "Duplicate", ButtonStyle { bordered: true, focused, ..Default::default() }),
                    F::MoveUp => icon_button(
                        ui,
                        theme,
                        "↑",
                        "Move up: wins clashes over the rules below (↑ ↓ Enter)",
                        ButtonStyle { bordered: true, focused, ..Default::default() },
                    ),
                    _ => icon_button(ui, theme, "↓", "Move down", ButtonStyle { bordered: true, focused, ..Default::default() }),
                };
                if resp.clicked() {
                    *clicked = Some((f, 0));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                for f in fields.iter().rev().copied().filter(|f| matches!(f, F::Follow | F::Save | F::Cancel | F::MoreOptions)) {
                    let focused = cur == Some(f);
                    let resp = match f {
                        F::Follow | F::Save => {
                            let label = if saving {
                                "Saving…"
                            } else if f == F::Save && !new {
                                "Save"
                            } else {
                                "Follow"
                            };
                            button(ui, theme, None, label, ButtonStyle { primary: true, focused, ..Default::default() })
                        }
                        F::Cancel => button(ui, theme, None, "Cancel", ButtonStyle { bordered: true, focused, ..Default::default() }),
                        _ => button(ui, theme, None, "More options…", ButtonStyle { bordered: true, focused, ..Default::default() }),
                    };
                    if resp.clicked() {
                        *clicked = Some((f, 0));
                    }
                }
            });
        });
    }
}

/// A full-width row: the keyboard cursor's fill when `focused`; the response senses clicks.
fn row(ui: &mut egui::Ui, theme: &Theme, focused: bool, scroll: bool, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let t = &theme.tokens;
    let pad = theme.space(SpaceToken::RowPaddingX);
    let frame = egui::Frame::NONE.corner_radius(t.rounding).inner_margin(egui::Margin::symmetric(pad as i8, (pad / 2.0) as i8));
    let frame = if focused { frame.fill(t.list_cursor_bg).stroke(egui::Stroke::new(1.0, t.list_cursor_border)) } else { frame };
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(add);
    });
    let resp = r.response.interact(egui::Sense::click());
    if focused && scroll {
        resp.scroll_to_me(None);
    }
    resp
}

/// The label column of an editor row.
fn label_cell(ui: &mut egui::Ui, theme: &Theme, label: &str, focused: bool) {
    let t = &theme.tokens;
    let w = 110.0 * theme.rem();
    ui.allocate_ui_with_layout(egui::vec2(w, theme.space(SpaceToken::ControlHeight)), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(if focused { t.list_cursor_text } else { t.text_secondary }));
    });
}

/// "Label   ◂ value ▸": ←/→ (or the arrows) change it, Enter or a click on the value goes on.
#[allow(clippy::too_many_arguments)]
fn value_row(ui: &mut egui::Ui, theme: &Theme, label: &str, value: &str, focused: bool, scroll: bool, f: F, clicked: &mut Option<(F, i32)>) {
    let t = &theme.tokens;
    let mut dir = None;
    let r = row(ui, theme, focused, scroll, |ui| {
        label_cell(ui, theme, label, focused);
        if icon_button(ui, theme, glyphs::CHEVRON_LEFT, "Previous (←)", ButtonStyle::default()).clicked() {
            dir = Some(-1);
        }
        ui.add(
            egui::Label::new(RichText::new(value).size(theme.font(TypeToken::Body)).color(if focused { t.list_cursor_text } else { t.fg }))
                .truncate(),
        );
        if icon_button(ui, theme, glyphs::CHEVRON_RIGHT, "Next (→)", ButtonStyle::default()).clicked() {
            dir = Some(1);
        }
    });
    match dir {
        Some(d) => *clicked = Some((f, d)),
        None if r.clicked() => *clicked = Some((f, 1)),
        None => {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use eframe::egui;
    use rusqlite::params;

    use super::*;
    use crate::model::{ChannelId, ProgrammeId, Timestamp};
    use crate::ui::screens::home::testkit::{self, has_label, overlay_harness, rig, seed, until_ov};
    use crate::ui::testing::TestUi;

    type H = egui_kittest::Harness<'static, testkit::OvApp<FollowSheet>>;

    /// One playlist channel with a guide, and on it, in an hour, a live "Premier League
    /// Football · Arsenal v Chelsea" (Sport, Football); the library is published for the
    /// preview. Returns (programme, channel, its start).
    pub(crate) fn fixture(t: &mut TestUi) -> (ProgrammeId, ChannelId, Timestamp) {
        let start = (crate::model::now() / 1800 + 2) * 1800;
        let (ch, pid) = seed(t, move |c| {
            let s = testkit::source(c, "TV")?;
            let ch = testkit::channels(c, s, 1)?;
            testkit::epg(c, &ch, start - 3600, 1, true)?;
            let (src, eid): (i64, i64) =
                c.query_row("SELECT epg_source_id, id FROM epg_channel WHERE xmltv_id = 'ch0.tv'", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, categories, flags)
                 VALUES (?1, ?2, 1, ?3, ?4, 'Premier League Football', 'Arsenal v Chelsea', 'Sport\x1fFootball', 2)",
                params![src, eid, start, start + 7200],
            )?;
            Ok((ch[0], ProgrammeId(c.last_insert_rowid())))
        });
        let _ = t.services().agent.publish_library(&t.lib);
        (pid, ch, start)
    }

    fn rules(t: &TestUi) -> Vec<FollowRule> {
        crate::store::follow::rules(&t.services().store.reader().unwrap()).unwrap()
    }

    fn count_line(h: &mut H) -> String {
        let app = h.state_mut();
        app.t.with_ctx(|ctx| app.o.count_line(ctx))
    }

    /// `R` on "Arsenal v Chelsea": the sheet offers both sides, counts the focused one in the
    /// guide, and Enter follows the focused suggestion — here the second, Chelsea — as a team
    /// rule, with a toast that says how many it found.
    #[test]
    fn enter_follows_the_focused_suggestion() {
        let (mut t, one) = rig();
        let (pid, ch, _) = fixture(&mut t);
        let sheet = FollowSheet::new(FollowSheetSpec { programme: Some((pid, ch)), rule: None });
        let mut h = overlay_harness(t, one, sheet, egui::vec2(1600.0, 1000.0));
        until_ov(&mut h, "the preview", |h| count_line(h).starts_with("1 in the guide"));
        assert!(has_label(&h, "Arsenal: every match") && has_label(&h, "Chelsea: every match"));
        assert!(has_label(&h, "Remind me 2 min before"));
        h.state_mut().act(Action::Down);
        until_ov(&mut h, "Chelsea's preview", |h| {
            h.state().o.draft.as_ref().is_some_and(|r| r.name == "Chelsea") && count_line(h).starts_with("1 in the guide")
        });
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        until_ov(&mut h, "saved", |h| h.state().last == "Close");
        let saved = rules(&h.state().t);
        let got: Vec<_> = saved.iter().map(|r| (r.name.as_str(), r.kind, r.mode)).collect();
        assert_eq!(got, [("Chelsea", FollowKind::Team, FollowMode::Notify)]);
        assert_eq!(saved[0].spec.shape, crate::follow::Shape::Fixture);
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some("Following: Chelsea · 1 in the guide"));
    }

    /// The sheet fits a tiled half (790 pt) with 16-pt text and a full window with 12-pt text:
    /// the suggestions' Follow and the longest editor's (a news flash's) Save stay on screen.
    #[test]
    fn the_sheet_fits_small_windows_and_large_text() {
        use egui_kittest::kittest::Queryable;
        for (size, text_pt) in [(egui::vec2(790.0, 700.0), 16.0), (egui::vec2(1600.0, 1000.0), 12.0)] {
            let (mut t, one) = rig();
            let p = Theme::fallback();
            t.theme = Theme::from_parts(p.name, p.palette, p.shell, 0.0, text_pt / 12.0);
            let (pid, ch, _) = fixture(&mut t);
            let id = seed(&mut t, |c| {
                let r = FollowRule::draft("BBC News at One", FollowKind::News, vec!["BBC News at One".into()], &Default::default(), 0);
                crate::store::follow::save_rule(c, &r, 100, 0)
            });
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let sheet = FollowSheet::new(FollowSheetSpec { programme: Some((pid, ch)), rule: None });
            let mut h = overlay_harness(t, one, sheet, size);
            until_ov(&mut h, "the suggestions", |h| has_label(h, "Follow"));
            h.run_steps(3); // the card settles where its size puts it
            assert!(screen.contains_rect(h.get_by_role_and_label(egui::accesskit::Role::Button, "Follow").rect()), "{size:?}: Follow");
            h.state_mut().o = FollowSheet::new(FollowSheetSpec { programme: None, rule: Some(id) });
            until_ov(&mut h, "the editor", |h| has_label(h, "Save"));
            h.run_steps(3);
            assert!(screen.contains_rect(h.get_by_label("Save").rect()), "{size:?}: Save at {:?}", h.get_by_label("Save").rect());
            assert!(screen.contains_rect(h.get_by_label("Delete…").rect()), "{size:?}: Delete…");
        }
    }

    /// The editor's Esc ladder: an unchanged rule closes at once; a changed one leaves the field
    /// first, then warns once ("Unsaved rule changes"), then discards — and a stray click
    /// outside never throws the change away. Names are chips: Enter adds one, Backspace on the
    /// empty field takes the last off.
    #[test]
    fn a_changed_rule_warns_once_on_esc_then_discards() {
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let r = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &crate::config::FollowConfig::default(), 0);
            crate::store::follow::save_rule(c, &r, 100, 0)
        });
        let spec = FollowSheetSpec { programme: None, rule: Some(id) };
        let mut h = overlay_harness(t, one, FollowSheet::new(spec.clone()), egui::vec2(900.0, 800.0));
        until_ov(&mut h, "the editor", |h| h.state().o.view == View::Edit);
        assert_eq!(h.state_mut().act(Action::Back), "Close", "unchanged: Esc closes");

        h.state_mut().o = FollowSheet::new(spec);
        h.state_mut().last.clear();
        until_ov(&mut h, "the editor", |h| h.state().o.view == View::Edit);
        let terms = |h: &H| h.state().o.draft.as_ref().map(|r| r.spec.terms.clone()).unwrap_or_default();
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        h.event(egui::Event::Text("Gunners".into()));
        h.run_steps(1);
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(terms(&h), ["Arsenal", "Gunners"]);
        h.key_press(egui::Key::Backspace);
        h.run_steps(1);
        assert_eq!(terms(&h), ["Arsenal"], "Backspace on the empty field");
        h.event(egui::Event::Text("Gooners".into()));
        h.run_steps(1);
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(terms(&h), ["Arsenal", "Gooners"]);
        assert_eq!(h.state_mut().act(Action::Back), "Keep", "leaves the field");
        assert_eq!(h.state().o.editing, None);

        let pos = egui::pos2(4.0, 4.0);
        h.hover_at(pos);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        }
        h.run_steps(2);
        assert_ne!(h.state().last, "Close", "a stray click keeps the change");
        assert_eq!(h.state_mut().act(Action::Back), "Keep", "warns");
        h.run_steps(1);
        assert!(has_label(&h, "Unsaved rule changes: Enter on Save, or Esc to discard"));
        assert_eq!(h.state_mut().act(Action::Back), "Close", "discards");
        assert_eq!(rules(&h.state().t)[0].spec.terms, ["Arsenal"]);
    }
}
