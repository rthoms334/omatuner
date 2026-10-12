//! Generic dialogs: context menu, confirm, PIN prompt, rename, favourites-list picker.
//! Owner: WP-10a. All keyboard-first (Modal / TextInput contexts), Omarchy menu card visuals.
//!
//! Also home of the drawing helpers every WP-10a overlay shares ([`scrim`], [`card_area`]):
//! a bg @ 0.5 scrim whose click cancels, and a card centred on the window (Menu card for
//! menus/help/jobs, Popup card with the accent border for dialogs).
//!
//! Writes (rename, favourites) go through the store writer and never block the UI thread: the
//! overlay keeps the `Ticket` and polls it every frame, closing on success and showing the
//! (already redacted) error otherwise.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText};

use super::{ConfirmSpec, ContextMenuSpec, ListPickerSpec, Overlay, OverlayOutcome, OverlayResult, RenameSpec, RenameTarget};
use crate::keymap::{Action, KeyContext};
use crate::model::{FavListId, FavouriteList};
use crate::msg::DataScope;
use crate::store::{Store, Ticket};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button, icon_button};
use crate::ui::widgets::card::{CardKind, card_frame};
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state};
use crate::ui::widgets::text_field::{search_header, text_field};
use crate::ui::widgets::{RowState, glyphs};

// ------------------------------------------------------------------ shared overlay drawing

/// Full-window scrim (bg @ 0.5) below the overlay card. Returns true when it was clicked
/// (either button: a right-click outside a menu closes it too).
pub(super) fn scrim(egui_ctx: &egui::Context, theme: &Theme, id: &str) -> bool {
    scrim_guarded(egui_ctx, theme, id, false)
}

/// How long the card border flashes when a click outside can't close a dirty overlay.
const FLASH_S: f64 = 0.3;
/// How long after that flash the pointer's tooltip says why.
const FLASH_HINT_S: f64 = 3.0;

fn flash_id(id: &str) -> egui::Id {
    egui::Id::new((id, "flash"))
}

/// [`scrim`] of an overlay that may hold an unsaved edit (`Overlay::is_dirty`): while `dirty`,
/// a click outside never closes it (one stray click must not throw the edit away) and the
/// card's border flashes instead; Esc and Cancel still close.
pub(super) fn scrim_guarded(egui_ctx: &egui::Context, theme: &Theme, id: &str, dirty: bool) -> bool {
    let screen = egui_ctx.content_rect();
    let clicked = egui::Area::new(egui::Id::new((id, "scrim")))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(egui_ctx, |ui| {
            let r = ui.allocate_rect(screen, egui::Sense::click());
            ui.painter().rect_filled(screen, 0.0, theme.tokens.scrim);
            // after a flash, the pointer says why the click outside did nothing
            let flashed = ui.data(|d| d.get_temp::<f64>(flash_id(id))).is_some_and(|at| ui.input(|i| i.time) - at < FLASH_HINT_S);
            let r = if dirty && flashed {
                r.on_hover_text(RichText::new("Unsaved changes — Cancel or Esc to discard").size(theme.font(TypeToken::BodySmall)))
            } else {
                r
            };
            r.clicked() || r.secondary_clicked()
        })
        .inner;
    if clicked && dirty {
        let now = egui_ctx.input(|i| i.time);
        egui_ctx.data_mut(|d| d.insert_temp(flash_id(id), now));
        return false;
    }
    clicked
}

/// A card of `width` centred on the window (or hanging from `top` once a launcher froze its
/// top edge), above the scrim.
pub(super) fn card_area<R>(
    egui_ctx: &egui::Context,
    theme: &Theme,
    id: &str,
    kind: CardKind,
    width: f32,
    top: Option<f32>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let place = match top {
        Some(y) => Place::Top(y),
        None => Place::Centre,
    };
    card_in(egui_ctx, theme, CardSpec { id, kind, width, place }, add)
}

/// [`card_area`] with its top-left corner at `at` (a menu opened by a right-click), kept on
/// the window.
pub(super) fn card_area_at<R>(
    egui_ctx: &egui::Context,
    theme: &Theme,
    id: &str,
    kind: CardKind,
    width: f32,
    at: egui::Pos2,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    card_in(egui_ctx, theme, CardSpec { id, kind, width, place: Place::At(at) }, add)
}

enum Place {
    Centre,
    Top(f32),
    At(egui::Pos2),
}

struct CardSpec<'a> {
    id: &'a str,
    kind: CardKind,
    width: f32,
    place: Place,
}

fn card_in<R>(egui_ctx: &egui::Context, theme: &Theme, spec: CardSpec<'_>, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    let screen = egui_ctx.content_rect();
    let gap = theme.space(SpaceToken::PanelPadding);
    let frame = card_frame(theme, spec.kind);
    let chrome = frame.total_margin().sum().x;
    let width = spec.width.min(screen.width() - 2.0 * gap).max(120.0);
    let area = egui::Area::new(egui::Id::new((spec.id, "card"))).order(egui::Order::Tooltip);
    let area = match spec.place {
        Place::Top(y) => area.pivot(Align2::CENTER_TOP).fixed_pos(egui::pos2(screen.center().x, y)),
        Place::Centre => area.anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO),
        // egui keeps an area on the window (`constrain`); the x is clamped here so a menu at
        // the right edge opens leftwards of the pointer rather than shifted under it
        Place::At(p) => area.pivot(Align2::LEFT_TOP).fixed_pos(egui::pos2(p.x.min(screen.right() - width - gap).max(screen.left() + gap), p.y)),
    };
    let since_flash = egui_ctx.data(|d| d.get_temp::<f64>(flash_id(spec.id))).map(|at| egui_ctx.input(|i| i.time) - at);
    let r = area.show(egui_ctx, |ui| {
        let r = frame.show(ui, |ui| {
            ui.set_width(width - chrome);
            ui.set_max_height(screen.height() - 2.0 * gap - frame.total_margin().sum().y);
            add(ui)
        });
        if since_flash.is_some_and(|dt| dt < FLASH_S) {
            // a border pulse twice the usual width: finish or cancel the edit first
            let w = 2.0 * theme.tokens.card_border_width.max(2.0);
            ui.painter().rect_stroke(r.response.rect, theme.tokens.rounding, egui::Stroke::new(w, theme.tokens.accent), egui::StrokeKind::Inside);
            egui_ctx.request_repaint();
        }
        r
    });
    egui::InnerResponse::new(r.inner.inner, r.response)
}

/// A row with the card's close button (✕ "Close (Esc)") at its right end and `add` filling the
/// rest: a card's header line. Returns whether the button was clicked.
pub(super) fn with_close(ui: &mut egui::Ui, theme: &Theme, add: impl FnOnce(&mut egui::Ui)) -> bool {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let close = close_button(ui, theme);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), add);
            close
        })
        .inner
    })
    .inner
}

/// The ✕ of a card: does what Esc does on it.
pub(super) fn close_button(ui: &mut egui::Ui, theme: &Theme) -> bool {
    icon_button(ui, theme, glyphs::CLOSE, "Close (Esc)", ButtonStyle::default()).clicked()
}

/// Dialog title + optional secondary line.
pub(super) fn title(ui: &mut egui::Ui, theme: &Theme, title: &str, detail: Option<&str>) {
    ui.label(RichText::new(title).size(theme.font(TypeToken::Heading)).color(theme.tokens.fg));
    if let Some(d) = detail.filter(|d| !d.is_empty()) {
        ui.add_space(theme.space(SpaceToken::Xs));
        ui.add(egui::Label::new(RichText::new(d).size(theme.font(TypeToken::Body)).color(theme.tokens.text_secondary)).wrap());
    }
}

/// Error line (urgent).
fn error_line(ui: &mut egui::Ui, theme: &Theme, error: Option<&str>) {
    if let Some(e) = error {
        ui.add_space(theme.space(SpaceToken::Sm));
        ui.add(egui::Label::new(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(theme.tokens.urgent)).wrap());
    }
}

/// Right-aligned button row; returns (cancel clicked, confirm clicked).
pub(super) fn buttons(ui: &mut egui::Ui, theme: &Theme, confirm: &str, style: ButtonStyle, focus_confirm: Option<bool>) -> (bool, bool) {
    ui.add_space(theme.space(SpaceToken::Lg));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
        let ok = button(ui, theme, None, confirm, ButtonStyle { focused: focus_confirm == Some(true), ..style }).clicked();
        let cancel =
            button(ui, theme, None, "Cancel", ButtonStyle { bordered: true, focused: focus_confirm == Some(false), ..Default::default() }).clicked();
        (cancel, ok)
    })
    .inner
}

/// Drive a pending write: `Some(result)` once the writer answered.
fn poll<R>(ticket: &mut Option<Ticket<R>>, egui_ctx: &egui::Context) -> Option<crate::error::Result<R>> {
    let r = ticket.as_mut()?.try_take();
    if r.is_some() {
        *ticket = None;
    } else {
        egui_ctx.request_repaint_after(Duration::from_millis(30));
    }
    r
}

/// Enter in a dialog with a text field. Actions are dispatched at the start of the frame, before
/// the field takes that frame's `Text` events (DESIGN §15.1): acting in `on_action` would miss a
/// digit typed in the same frame as Enter. So `on_action` only records the request and `ui`
/// acts on it after drawing the field.
#[derive(Default)]
struct Submit(bool);

impl Submit {
    fn request(&mut self) -> OverlayOutcome {
        self.0 = true;
        OverlayOutcome::Keep
    }
    /// True once per request. Asks for another frame: the outcome (an error line, "Saving…")
    /// is acted on after this frame's widgets were drawn.
    fn take(&mut self, egui_ctx: &egui::Context) -> bool {
        let requested = std::mem::take(&mut self.0);
        if requested {
            egui_ctx.request_repaint();
        }
        requested
    }
}

/// A user-visible error message (errors are redacted by contract; redact again defensively).
fn message(e: &crate::error::Error) -> String {
    crate::redact::redact(&e.to_string()).into_owned()
}

// ------------------------------------------------------------------ context menu

/// Launcher-style menu: rows (icon, label, shortcut hint), type-to-filter, Enter chooses.
pub struct ContextMenuOverlay {
    pub spec: ContextMenuSpec,
    /// Cursor + filter over the displayed rows.
    list: ListState,
    /// Displayed rows (indices into `spec.items`) for the current filter.
    rows: Vec<usize>,
    /// Card top edge, frozen at the first keystroke (the card then grows downward only).
    top: Option<f32>,
    last_top: Option<f32>,
    scroll_to_cursor: bool,
    /// Opened by a right-click: the card's top-left corner (it then grows downward only).
    at: Option<egui::Pos2>,
}

impl ContextMenuOverlay {
    pub fn new(spec: ContextMenuSpec) -> Self {
        let mut list = ListState::default();
        list.set_len(spec.items.len());
        // start on the first enabled item
        if let Some(i) = spec.items.iter().position(|it| it.enabled) {
            list.select(i);
        }
        let rows = (0..spec.items.len()).collect();
        Self { spec, list, rows, top: None, last_top: None, scroll_to_cursor: false, at: None }
    }

    fn refilter(&mut self) {
        self.rows = self.list.filter.apply(self.spec.items.iter().map(|it| it.label.as_str())).into_iter().map(|i| i as usize).collect();
        self.list.set_len(self.rows.len());
        if self.list.filter.is_active() && self.top.is_none() {
            self.top = self.last_top;
        }
    }

    fn choose(&self, row: usize) -> OverlayOutcome {
        match self.rows.get(row).map(|&i| &self.spec.items[i]) {
            Some(it) if it.enabled => OverlayOutcome::CloseWith(OverlayResult::Chosen(it.action)),
            _ => OverlayOutcome::Keep,
        }
    }
}

impl Overlay for ContextMenuOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.key_context(), KeyContext::Modal]
    }

    fn anchor_at(&mut self, pos: egui::Pos2) {
        self.at = Some(pos);
    }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match self.list.handle(action) {
            ListOutcome::Moved => {
                self.scroll_to_cursor = true;
                OverlayOutcome::Keep
            }
            ListOutcome::FilterChanged => {
                self.refilter();
                OverlayOutcome::Keep
            }
            ListOutcome::Activated(i) | ListOutcome::ActivatedAlt(i) => self.choose(i),
            ListOutcome::Parent => OverlayOutcome::Close,
            ListOutcome::ContextMenu(_) => OverlayOutcome::Keep,
            ListOutcome::None => match action {
                Action::Back => OverlayOutcome::Close,
                // keys the list absorbed without effect (ends of the list, a leading space)
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => {
                    OverlayOutcome::Keep
                }
                Action::Activate | Action::ActivateAlt | Action::Right => OverlayOutcome::Keep,
                _ => OverlayOutcome::Ignored,
            },
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "context-menu") {
            return OverlayOutcome::Close;
        }
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg);
        let mut chosen = None;
        let width = 340.0 * theme.rem();
        let (at, top) = (self.at, self.top);
        let add = |ui: &mut egui::Ui| {
            search_header(ui, theme, self.list.filter.query(), &self.spec.title, None);
            ui.add_space(theme.space(SpaceToken::Md));
            if self.rows.is_empty() {
                let q = self.list.filter.query();
                ui.label(RichText::new(format!("No matches for “{q}”")).size(theme.font(TypeToken::Title)).color(t.text_secondary));
                return;
            }
            egui::ScrollArea::vertical().max_height(egui_ctx.content_rect().height() * 0.6).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xs);
                for (row, &i) in self.rows.iter().enumerate() {
                    let it = &self.spec.items[i];
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, it.enabled, &it.label));
                    let cursor = row == self.list.cursor;
                    if cursor && self.scroll_to_cursor {
                        ui.scroll_to_rect(rect, None);
                        self.scroll_to_cursor = false;
                    }
                    if pointer_moved && resp.hovered() && !cursor {
                        self.list.select(row);
                    }
                    if crate::ui::widgets::fresh_click(&resp) {
                        self.list.select(row);
                        chosen = Some(row);
                    }
                    paint_menu_row(ui.painter(), rect, theme, it.icon, &it.label, it.shortcut.as_deref(), it.enabled, cursor);
                }
            });
        };
        let r = match at {
            Some(at) => card_area_at(egui_ctx, theme, "context-menu", CardKind::Menu, width, at, add),
            None => card_area(egui_ctx, theme, "context-menu", CardKind::Menu, width, top, add),
        };
        self.last_top = Some(r.response.rect.top());
        chosen.map_or(OverlayOutcome::Keep, |row| self.choose(row))
    }
}

/// `[icon] Label                Ctrl+F` in a menu card.
fn paint_menu_row(p: &egui::Painter, rect: egui::Rect, theme: &Theme, icon: &str, label: &str, shortcut: Option<&str>, enabled: bool, cursor: bool) {
    let t = &theme.tokens;
    paint_row_state(p, rect, theme, if cursor { RowState::Cursor } else { RowState::Idle });
    let color = if !enabled {
        t.text_disabled
    } else if cursor {
        t.list_cursor_text
    } else {
        t.fg
    };
    let inset = theme.space(SpaceToken::Lg);
    let icon_col = (28.0 * theme.rem()).round();
    if !icon.is_empty() {
        p.text(
            egui::pos2(rect.left() + inset + icon_col / 2.0, rect.center().y),
            Align2::CENTER_CENTER,
            icon,
            egui::FontId::proportional(theme.font(TypeToken::Icon)),
            color,
        );
    }
    let mut right = rect.right() - inset;
    if let Some(s) = shortcut {
        let r = p.text(
            egui::pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            s,
            egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
            t.text_secondary,
        );
        right = r.left() - theme.space(SpaceToken::Lg);
    }
    let left = rect.left() + inset + icon_col + theme.space(SpaceToken::Md);
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), egui::FontId::proportional(theme.font(TypeToken::Title)), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(0.0));
    let g = p.layout_job(job);
    p.galley(egui::pos2(left, rect.center().y - g.size().y / 2.0), g, color);
}

// ------------------------------------------------------------------ confirm

/// Yes/No dialog; destructive variant focuses Cancel and paints confirm in `urgent`.
pub struct ConfirmOverlay {
    pub spec: ConfirmSpec,
    focus_confirm: bool,
}

impl ConfirmOverlay {
    pub fn new(spec: ConfirmSpec) -> Self {
        let f = !spec.destructive;
        Self { spec, focus_confirm: f }
    }

    fn result(&self, confirm: bool) -> OverlayOutcome {
        OverlayOutcome::CloseWith(if confirm { OverlayResult::Confirmed(self.spec.token) } else { OverlayResult::Cancelled(self.spec.token) })
    }
}

impl Overlay for ConfirmOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Left | Action::Right | Action::Up | Action::Down | Action::FocusNext | Action::FocusPrev => {
                self.focus_confirm = !self.focus_confirm;
                OverlayOutcome::Keep
            }
            Action::Activate => self.result(self.focus_confirm),
            Action::Back => self.result(false),
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        if scrim(egui_ctx, theme, "confirm") {
            return self.result(false);
        }
        let style = ButtonStyle { danger: self.spec.destructive, primary: !self.spec.destructive, bordered: true, ..Default::default() };
        let (cancel, ok) = card_area(egui_ctx, theme, "confirm", CardKind::Popup, 420.0 * theme.rem(), None, |ui| {
            title(ui, theme, &self.spec.title, Some(&self.spec.body));
            buttons(ui, theme, &self.spec.confirm_label, style, Some(self.focus_confirm))
        })
        .inner;
        if ok {
            self.result(true)
        } else if cancel {
            self.result(false)
        } else {
            OverlayOutcome::Keep
        }
    }
}

// ------------------------------------------------------------------ PIN

/// Wrong attempts before the cooldown, and its length (DESIGN §20.3).
const PIN_MAX_FAILURES: u32 = 3;
const PIN_COOLDOWN: Duration = Duration::from_secs(30);

/// Failed attempts and cooldown live as long as the UI thread, so closing and reopening the
/// prompt doesn't reset them (it's still only a speed bump — see DESIGN §20.3). Thread-local
/// rather than a process-wide static: the app has one UI thread, and tests (one thread each)
/// don't put each other into the cooldown.
#[derive(Default)]
struct PinGuard {
    failures: u32,
    until: Option<Instant>,
}

thread_local! {
    static PIN_GUARD: RefCell<PinGuard> = RefCell::default();
}

/// Remaining cooldown, if any.
fn pin_cooldown() -> Option<Duration> {
    PIN_GUARD.with_borrow_mut(|g| {
        let now = Instant::now();
        match g.until {
            Some(u) if u > now => Some(u - now),
            Some(_) => {
                g.until = None;
                None
            }
            None => None,
        }
    })
}

/// 4–8 digit PIN entry (dots), 3 wrong attempts → 30 s cooldown. Verifies with
/// `ctx.parental.unlock` and replies `OverlayResult::Unlocked(then)`.
pub struct PinOverlay {
    pub then: Action,
    pub reason: String,
    entered: String,
    error: Option<String>,
    /// Enter was pressed: checked at the end of `ui`, see [`Submit`].
    enter: Submit,
}

impl PinOverlay {
    pub fn new(then: Action, reason: String) -> Self {
        Self { then, reason, entered: String::new(), error: None, enter: Submit::default() }
    }

    fn submit(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if pin_cooldown().is_some() {
            return OverlayOutcome::Keep;
        }
        if !(4..=8).contains(&self.entered.len()) {
            self.error = Some("Enter 4–8 digits".into());
            return OverlayOutcome::Keep;
        }
        if ctx.parental.unlock(&self.entered, &ctx.config.parental) {
            PIN_GUARD.set(PinGuard::default());
            return OverlayOutcome::CloseWith(OverlayResult::Unlocked(self.then));
        }
        self.entered.clear();
        self.error = PIN_GUARD.with_borrow_mut(|g| {
            g.failures += 1;
            if g.failures >= PIN_MAX_FAILURES {
                *g = PinGuard { failures: 0, until: Some(Instant::now() + PIN_COOLDOWN) };
                None
            } else {
                Some(format!("Wrong PIN ({} of {PIN_MAX_FAILURES} attempts)", g.failures))
            }
        });
        OverlayOutcome::Keep
    }
}

impl Overlay for PinOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        // no text field while cooling down: nothing to type into
        if pin_cooldown().is_some() { vec![KeyContext::Modal] } else { vec![KeyContext::TextInput, KeyContext::Modal] }
    }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Activate => self.enter.request(),
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        if scrim(egui_ctx, theme, "pin") {
            return OverlayOutcome::Close;
        }
        let cooldown = pin_cooldown();
        let (cancel, ok) = card_area(egui_ctx, theme, "pin", CardKind::Popup, 340.0 * theme.rem(), None, |ui| {
            title(ui, theme, &format!("{}  Enter PIN", glyphs::LOCK), Some(&self.reason));
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.add_enabled_ui(cooldown.is_none(), |ui| {
                let resp = text_field(ui, theme, &mut self.entered, "PIN", true);
                if resp.changed() {
                    self.entered.retain(|c| c.is_ascii_digit());
                    self.entered.truncate(8);
                    self.error = None;
                }
                if cooldown.is_none() && !resp.has_focus() {
                    resp.request_focus();
                }
            });
            match cooldown {
                Some(left) => {
                    let msg = format!("Too many wrong attempts — try again in {} s", left.as_secs() + 1);
                    error_line(ui, theme, Some(&msg));
                    egui_ctx.request_repaint_after(Duration::from_millis(250));
                }
                None => error_line(ui, theme, self.error.as_deref()),
            }
            buttons(ui, theme, "Unlock", ButtonStyle { primary: true, ..Default::default() }, None)
        })
        .inner;
        if self.enter.take(egui_ctx) || ok {
            self.submit(ctx)
        } else if cancel {
            OverlayOutcome::Close
        } else {
            OverlayOutcome::Keep
        }
    }
}

// ------------------------------------------------------------------ rename

/// Submit the rename write for `target`. Empty names remove an item/group rename; lists and
/// sources must keep a name (checked by the caller).
fn save_rename(store: &Store, target: &RenameTarget, name: String) -> Ticket<()> {
    use crate::store::{sources, user};
    let w = store.writer();
    let rename = (!name.is_empty()).then_some(name.clone());
    match target.clone() {
        RenameTarget::Item { item } => w.submit_notify(DataScope::Overrides, move |c| {
            let mut ov = user::item_override(c, &item)?.unwrap_or_default();
            ov.rename = rename;
            user::set_item_override(c, &item, &ov)
        }),
        RenameTarget::Group { source, kind, key } => w.submit_notify(DataScope::Overrides, move |c| {
            let mut ov = user::group_overrides(c)?
                .into_iter()
                .find(|r| r.source == source && r.kind == kind && r.key == key)
                .map(|r| r.ov)
                .unwrap_or_default();
            ov.rename = rename;
            user::set_group_override(c, source, kind, &key, &ov)
        }),
        // written by `RenameOverlay::save` (`ui::copies::rename`)
        RenameTarget::Copies { .. } => w.submit(|_| Ok(())),
        RenameTarget::FavList(id) => w.submit_notify(DataScope::Favourites, move |c| {
            let icon = user::fav_lists(c)?.into_iter().find(|l| l.id == id).map(|l| l.icon).unwrap_or_else(|| glyphs::FAV_LIST.into());
            user::rename_fav_list(c, id, &name, &icon)
        }),
        RenameTarget::Source(id) => w.submit_notify(DataScope::Sources, move |c| {
            let mut s = sources::get_source(c, id)?;
            s.name = name;
            sources::update_source(c, &s)
        }),
    }
}

/// Single text field; Enter saves via the store writer (empty = remove the rename).
pub struct RenameOverlay {
    pub spec: RenameSpec,
    text: String,
    pending: Option<Ticket<()>>,
    error: Option<String>,
    enter: Submit,
    /// Copies of a combined title with their own name: the name the user confirmed renaming
    /// them to (Enter again).
    overwrite: Option<String>,
}

impl RenameOverlay {
    pub fn new(spec: RenameSpec) -> Self {
        let text = spec.initial.clone();
        Self { spec, text, pending: None, error: None, enter: Submit::default(), overwrite: None }
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if self.pending.is_some() {
            return OverlayOutcome::Keep;
        }
        let name = self.text.trim().to_owned();
        if name.is_empty() && matches!(self.spec.target, RenameTarget::FavList(_) | RenameTarget::Source(_)) {
            self.error = Some("The name can't be empty".into());
            return OverlayOutcome::Keep;
        }
        if name == self.spec.initial.trim() {
            return OverlayOutcome::Close;
        }
        if let RenameTarget::Copies { items } = &self.spec.target {
            // copies with a name of their own keep it unless Enter is pressed again
            let overwrite = self.overwrite.as_deref() == Some(name.as_str());
            if !overwrite {
                let db = ctx.db;
                let own = crate::store::copies::rename_conflicts(db, items, &name).unwrap_or_default();
                if let Some((_, first)) = own.first() {
                    let n = own.len();
                    let which = if n == 1 { format!("A copy is named “{first}”") } else { format!("{n} copies have names of their own") };
                    self.error = Some(format!("{which} — Enter again renames {} too", if n == 1 { "it" } else { "them" }));
                    self.overwrite = Some(name);
                    return OverlayOutcome::Keep;
                }
            }
            crate::ui::copies::rename(ctx, items.clone(), &name, overwrite);
            return OverlayOutcome::Close;
        }
        self.pending = Some(save_rename(&ctx.services.store, &self.spec.target, name));
        OverlayOutcome::Keep
    }
}

impl Overlay for RenameOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::TextInput, KeyContext::Modal]
    }

    fn is_dirty(&self) -> bool {
        self.text != self.spec.initial
    }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Activate => self.enter.request(),
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        match poll(&mut self.pending, egui_ctx) {
            Some(Ok(())) => return OverlayOutcome::Close,
            Some(Err(e)) => self.error = Some(format!("Couldn't rename: {}", message(&e))),
            None => {}
        }
        let theme = ctx.theme;
        if scrim_guarded(egui_ctx, theme, "rename", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        let saving = self.pending.is_some();
        let (cancel, ok) = card_area(egui_ctx, theme, "rename", CardKind::Popup, 420.0 * theme.rem(), None, |ui| {
            let hint = match self.spec.target {
                RenameTarget::Item { .. } | RenameTarget::Group { .. } | RenameTarget::Copies { .. } => {
                    Some("Leave empty to restore the provider's name.")
                }
                _ => None,
            };
            title(ui, theme, &self.spec.title, hint);
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.add_enabled_ui(!saving, |ui| {
                let resp = text_field(ui, theme, &mut self.text, "Name", false);
                if resp.changed() {
                    self.error = None;
                }
                if !saving && !resp.has_focus() {
                    resp.request_focus();
                }
            });
            error_line(ui, theme, self.error.as_deref());
            buttons(ui, theme, if saving { "Saving…" } else { "Save" }, ButtonStyle { primary: true, ..Default::default() }, None)
        })
        .inner;
        if self.enter.take(egui_ctx) || ok {
            self.save(ctx)
        } else if cancel {
            OverlayOutcome::Close
        } else {
            OverlayOutcome::Keep
        }
    }
}

// ------------------------------------------------------------------ favourites-list picker

/// Checklist of favourites lists (toggle membership with Enter/Space) + a "New list…" row
/// (opens an inline name field: `[TextInput, Modal]` while typing).
///
/// Lists and memberships come from the app-owned `ctx.favs` (no DB read on the UI thread);
/// toggles are written through the store writer and patched into `ctx.favs` optimistically.
/// Every write ends with `DataScope::Favourites`, failed ones too, so the app re-reads the
/// real memberships even after the picker closed; while it is open, a failure also restores
/// the list's last stored state at once (with an error line).
pub struct ListPickerOverlay {
    pub spec: ListPickerSpec,
    cursor: usize,
    /// `Some(name)` while the "New list…" field is open.
    naming: Option<String>,
    creating: Option<Ticket<FavListId>>,
    /// Name the pending create was submitted with (the field may be closed meanwhile).
    new_name: String,
    /// Membership writes not answered yet.
    toggles: Vec<PendingToggle>,
    /// Per list toggled here: its stored membership as far as the answered writes tell.
    stored: HashMap<FavListId, bool>,
    error: Option<String>,
    enter: Submit,
}

/// An optimistic membership change waiting for the writer.
struct PendingToggle {
    ticket: Ticket<()>,
    list: FavListId,
    on: bool,
}

impl ListPickerOverlay {
    pub fn new(spec: ListPickerSpec) -> Self {
        Self {
            spec,
            cursor: 0,
            naming: None,
            creating: None,
            new_name: String::new(),
            toggles: Vec::new(),
            stored: HashMap::new(),
            error: None,
            enter: Submit::default(),
        }
    }

    fn toggle(&mut self, ctx: &mut UiCtx<'_>, list: FavListId) {
        let item = self.spec.item.clone();
        let was = ctx.favs.lists_of(&item).contains(&list);
        let on = !was;
        let now = ctx.now;
        let write_item = item.clone();
        let bus = ctx.services.bus.clone();
        // DataScope::Favourites makes the app reload `favs` from the store, also after a
        // failed write: that undoes the optimistic patch even once the picker is closed
        let ticket = ctx.services.store.writer().submit(move |c| {
            let r = crate::store::user::set_favourite(c, list, &write_item, on, now);
            bus.data_changed(DataScope::Favourites);
            r
        });
        self.stored.entry(list).or_insert(was);
        self.toggles.push(PendingToggle { ticket, list, on });
        ctx.favs.apply_toggle(list, &item, on);
    }

    /// Settle answered membership writes (the writer answers in order). A failure restores the
    /// list's last stored state once no later write for it is pending, and is reported.
    fn poll_toggles(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) {
        let mut failed = None;
        let mut settled = Vec::new();
        self.toggles.retain_mut(|p| match p.ticket.try_take() {
            None => true,
            Some(r) => {
                settled.push((p.list, p.on, r.is_ok()));
                if let Err(e) = r {
                    failed = Some((p.list, e));
                }
                false
            }
        });
        for (list, on, ok) in settled {
            if ok {
                self.stored.insert(list, on);
            } else if !self.toggles.iter().any(|p| p.list == list)
                && let Some(stored) = self.stored.get(&list).copied()
            {
                ctx.favs.apply_toggle(list, &self.spec.item, stored);
            }
        }
        if let Some((list, e)) = failed {
            let name = ctx.favs.lists.iter().find(|l| l.id == list).map(|l| l.name.as_str()).unwrap_or("the list");
            self.error = Some(format!("Couldn't update “{name}”: {}", message(&e)));
        }
        if !self.toggles.is_empty() {
            egui_ctx.request_repaint_after(Duration::from_millis(30));
        }
    }

    fn create(&mut self, ctx: &mut UiCtx<'_>) {
        if self.creating.is_some() {
            return;
        }
        let Some(name) = self.naming.as_ref().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) else {
            self.error = Some("The name can't be empty".into());
            return;
        };
        self.new_name = name.clone();
        let item = self.spec.item.clone();
        let now = ctx.now;
        self.creating = Some(ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| {
            let id = crate::store::user::create_fav_list(c, &name, glyphs::FAV_LIST)?;
            crate::store::user::set_favourite(c, id, &item, true, now)?;
            Ok(id)
        }));
    }

    /// The new list was stored: show it at once (the app's reload follows).
    fn created(&mut self, ctx: &mut UiCtx<'_>, id: FavListId) {
        self.naming = None;
        let name = std::mem::take(&mut self.new_name);
        if !ctx.favs.lists.iter().any(|l| l.id == id) {
            let sort_index = ctx.favs.lists.iter().map(|l| l.sort_index).max().unwrap_or(0) + 1;
            ctx.favs.lists.push(FavouriteList { id, name, icon: glyphs::FAV_LIST.into(), sort_index, is_default: false });
        }
        ctx.favs.apply_toggle(id, &self.spec.item, true);
        self.cursor = ctx.favs.lists.iter().position(|l| l.id == id).unwrap_or(0);
    }
}

impl Overlay for ListPickerOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.naming.is_some() { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    /// A new list's name being typed (memberships are saved as they're toggled).
    fn is_dirty(&self) -> bool {
        self.naming.as_ref().is_some_and(|n| !n.trim().is_empty())
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        if self.naming.is_some() {
            return match action {
                Action::Activate => self.enter.request(),
                Action::Back => {
                    self.naming = None;
                    self.error = None;
                    OverlayOutcome::Keep
                }
                _ => OverlayOutcome::Ignored,
            };
        }
        let rows = ctx.favs.lists.len() + 1;
        match action {
            Action::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                OverlayOutcome::Keep
            }
            Action::Down => {
                self.cursor = (self.cursor + 1).min(rows - 1);
                OverlayOutcome::Keep
            }
            Action::Activate => {
                match ctx.favs.lists.get(self.cursor).map(|l| l.id) {
                    Some(id) => self.toggle(ctx, id),
                    None => self.naming = Some(String::new()),
                }
                OverlayOutcome::Keep
            }
            Action::Back => OverlayOutcome::Close,
            Action::Left | Action::Right | Action::FocusNext | Action::FocusPrev => OverlayOutcome::Keep,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        match poll(&mut self.creating, egui_ctx) {
            Some(Ok(id)) => self.created(ctx, id),
            Some(Err(e)) => self.error = Some(format!("Couldn't create the list: {}", message(&e))),
            None => {}
        }
        self.poll_toggles(egui_ctx, ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim_guarded(egui_ctx, theme, "list-picker", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        self.cursor = self.cursor.min(ctx.favs.lists.len());
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg);
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let member: Vec<FavListId> = ctx.favs.lists_of(&self.spec.item).to_vec();
        let lists: Vec<(FavListId, String, String)> = ctx.favs.lists.iter().map(|l| (l.id, l.icon.clone(), l.name.clone())).collect();
        let mut clicked = None;
        let mut close = false;
        let (mut create, mut cancel_naming) = (false, false);
        card_area(egui_ctx, theme, "list-picker", CardKind::Menu, 360.0 * theme.rem(), None, |ui| {
            title(ui, theme, &self.spec.title, None);
            ui.add_space(theme.space(SpaceToken::Md));
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xs);
            for (row, (id, icon, name)) in lists.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let on = member.contains(id);
                resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, on, name));
                if pointer_moved && resp.hovered() {
                    self.cursor = row;
                }
                // a tick: a double-click doesn't untick it
                if crate::ui::widgets::fresh_click(&resp) {
                    self.cursor = row;
                    clicked = Some(*id);
                }
                let icon = if icon.is_empty() { glyphs::FAV_LIST } else { icon.as_str() };
                let check = on.then_some(glyphs::CHECK);
                paint_menu_row(ui.painter(), rect, theme, icon, name, check, true, row == self.cursor);
            }
            let new_row = lists.len();
            match &mut self.naming {
                Some(name) => {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    let resp = text_field(ui, theme, name, "New list name", false);
                    if resp.changed() {
                        self.error = None;
                    }
                    if self.creating.is_none() && !resp.has_focus() {
                        resp.request_focus();
                    }
                }
                None => {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "New list…"));
                    if pointer_moved && resp.hovered() {
                        self.cursor = new_row;
                    }
                    if crate::ui::widgets::fresh_click(&resp) {
                        self.cursor = new_row;
                        self.naming = Some(String::new());
                    }
                    paint_menu_row(ui.painter(), rect, theme, glyphs::ADD, "New list…", None, true, self.cursor == new_row);
                }
            }
            error_line(ui, theme, self.error.as_deref());
            ui.add_space(theme.space(SpaceToken::Md));
            let naming = self.naming.is_some();
            ui.horizontal(|ui| {
                let hint = if naming { "Enter create · Esc cancel" } else { "Enter toggle · Esc done" };
                ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if naming {
                        // the name field's own buttons (Enter / Esc)
                        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                        create = button(ui, theme, Some(glyphs::CHECK), "Create", ButtonStyle { primary: true, ..Default::default() }).clicked();
                        cancel_naming = button(ui, theme, None, "Cancel", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                    } else {
                        close = button(ui, theme, None, "Done", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                    }
                });
            });
        });
        if let Some(id) = clicked {
            self.toggle(ctx, id);
        }
        if cancel_naming {
            self.naming = None;
            self.error = None;
        }
        // Enter in the name field, after the field took this frame's text
        if (self.enter.take(egui_ctx) || create) && self.naming.is_some() {
            self.create(ctx);
        }
        if close { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_cooldown_is_per_thread() {
        PIN_GUARD.set(PinGuard { failures: 0, until: Some(Instant::now() + PIN_COOLDOWN) });
        assert!(pin_cooldown().is_some());
        assert!(std::thread::spawn(pin_cooldown).join().expect("thread").is_none(), "another thread isn't locked out");
        PIN_GUARD.set(PinGuard { failures: 0, until: Some(Instant::now()) });
        assert!(pin_cooldown().is_none(), "an elapsed cooldown ends");
    }

    /// F2 on a combined title (DESIGN §29.15): copies without a name of their own take the new
    /// one; a copy with its own name keeps it until Enter is pressed again, after the dialog
    /// said so — then every copy is renamed, with an Undo toast.
    #[test]
    fn renaming_every_copy_asks_before_overwriting_a_copys_own_name() {
        use crate::model::{ItemKey, ItemKind, ItemOverride, ItemRef, SourceId};
        use crate::ui::screens::home::testkit::{overlay_harness, rig, seed, until_ov};
        let (mut t, one) = rig();
        let (a, b) = seed(&mut t, |c| {
            let a = crate::ui::screens::home::testkit::source(c, "Alpha")?;
            let b = crate::ui::screens::home::testkit::source(c, "Bravo")?;
            crate::ui::screens::home::testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune")])?;
            crate::ui::screens::home::testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?;
            Ok((a, b))
        });
        let r = |s: SourceId| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) };
        let own = r(b);
        seed(&mut t, move |c| {
            crate::store::user::set_item_override(c, &own, &ItemOverride { rename: Some("Dune (Bravo)".into()), ..Default::default() })
        });
        let spec = RenameSpec { title: "Rename every copy".into(), initial: "Dune".into(), target: RenameTarget::Copies { items: vec![r(a), r(b)] } };
        let mut h = overlay_harness(t, one, RenameOverlay::new(spec), egui::vec2(1000.0, 700.0));
        h.state_mut().o.text = "Dune: Part One".into();
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state().o.error.as_deref(), Some("A copy is named “Dune (Bravo)” — Enter again renames it too"), "asks first");
        assert_ne!(h.state().last, "Close");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state().last, "Close");
        let name = |h: &egui_kittest::Harness<'_, crate::ui::screens::home::testkit::OvApp<RenameOverlay>>, s: SourceId| {
            crate::store::user::item_override(&h.state().t.services().store.reader().unwrap(), &r(s)).unwrap().and_then(|o| o.rename)
        };
        until_ov(&mut h, "both renamed", |h| name(h, a).as_deref() == Some("Dune: Part One") && name(h, b).as_deref() == Some("Dune: Part One"));
    }
}
