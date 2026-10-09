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

use std::sync::Mutex;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText};

use super::{ConfirmSpec, ContextMenuSpec, ListPickerSpec, OverlayResult, Overlay, OverlayOutcome, RenameSpec, RenameTarget};
use crate::keymap::{Action, KeyContext};
use crate::model::{FavListId, FavouriteList};
use crate::msg::DataScope;
use crate::store::{Store, Ticket};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::{CardKind, card_frame};
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state};
use crate::ui::widgets::text_field::{search_header, text_field};
use crate::ui::widgets::{RowState, glyphs};

// ------------------------------------------------------------------ shared overlay drawing

/// Full-window scrim (bg @ 0.5) below the overlay card. Returns true when it was clicked.
pub(super) fn scrim(egui_ctx: &egui::Context, theme: &Theme, id: &str) -> bool {
    let screen = egui_ctx.content_rect();
    egui::Area::new(egui::Id::new((id, "scrim")))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(egui_ctx, |ui| {
            let r = ui.allocate_rect(screen, egui::Sense::click());
            ui.painter().rect_filled(screen, 0.0, theme.tokens.scrim);
            r.clicked()
        })
        .inner
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
    let screen = egui_ctx.content_rect();
    let gap = theme.space(SpaceToken::PanelPadding);
    let frame = card_frame(theme, kind);
    let chrome = frame.total_margin().sum().x;
    let width = width.min(screen.width() - 2.0 * gap).max(120.0);
    let area = egui::Area::new(egui::Id::new((id, "card"))).order(egui::Order::Tooltip);
    let area = match top {
        Some(y) => area.pivot(Align2::CENTER_TOP).fixed_pos(egui::pos2(screen.center().x, y)),
        None => area.anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO),
    };
    let r = area.show(egui_ctx, |ui| {
        frame.show(ui, |ui| {
            ui.set_width(width - chrome);
            ui.set_max_height(screen.height() - 2.0 * gap - frame.total_margin().sum().y);
            add(ui)
        })
    });
    egui::InnerResponse::new(r.inner.inner, r.response)
}

/// Dialog title + optional secondary line.
fn title(ui: &mut egui::Ui, theme: &Theme, title: &str, detail: Option<&str>) {
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
fn buttons(ui: &mut egui::Ui, theme: &Theme, confirm: &str, style: ButtonStyle, focus_confirm: Option<bool>) -> (bool, bool) {
    ui.add_space(theme.space(SpaceToken::Lg));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
        let ok = button(ui, theme, None, confirm, ButtonStyle { focused: focus_confirm == Some(true), ..style }).clicked();
        let cancel = button(ui, theme, None, "Cancel", ButtonStyle { bordered: true, focused: focus_confirm == Some(false), ..Default::default() }).clicked();
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

/// A user-visible error message (errors are redacted by contract; redact again defensively).
fn message(e: &crate::error::Error) -> String { crate::redact::redact(&e.to_string()).into_owned() }

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
        Self { spec, list, rows, top: None, last_top: None, scroll_to_cursor: false }
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
    fn key_contexts(&self) -> Vec<KeyContext> { vec![self.list.key_context(), KeyContext::Modal] }

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
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => OverlayOutcome::Keep,
                Action::Activate | Action::ActivateAlt | Action::Left | Action::Right => OverlayOutcome::Keep,
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
        let r = card_area(egui_ctx, theme, "context-menu", CardKind::Menu, width, self.top, |ui| {
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
                    if resp.clicked() {
                        self.list.select(row);
                        chosen = Some(row);
                    }
                    paint_menu_row(ui.painter(), rect, theme, it.icon, &it.label, it.shortcut.as_deref(), it.enabled, cursor);
                }
            });
        });
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
        p.text(egui::pos2(rect.left() + inset + icon_col / 2.0, rect.center().y), Align2::CENTER_CENTER, icon, egui::FontId::proportional(theme.font(TypeToken::Icon)), color);
    }
    let mut right = rect.right() - inset;
    if let Some(s) = shortcut {
        let r = p.text(egui::pos2(right, rect.center().y), Align2::RIGHT_CENTER, s, egui::FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_secondary);
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
    pub fn new(spec: ConfirmSpec) -> Self { let f = !spec.destructive; Self { spec, focus_confirm: f } }

    fn result(&self, confirm: bool) -> OverlayOutcome {
        OverlayOutcome::CloseWith(if confirm { OverlayResult::Confirmed(self.spec.token) } else { OverlayResult::Cancelled(self.spec.token) })
    }
}

impl Overlay for ConfirmOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }

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

/// Failed attempts and cooldown are process-wide, so closing and reopening the prompt doesn't
/// reset them (it's still only a speed bump — see DESIGN §20.3).
struct PinGuard {
    failures: u32,
    until: Option<Instant>,
}

static PIN_GUARD: Mutex<PinGuard> = Mutex::new(PinGuard { failures: 0, until: None });

fn pin_guard() -> std::sync::MutexGuard<'static, PinGuard> { PIN_GUARD.lock().unwrap_or_else(|e| e.into_inner()) }

/// Remaining cooldown, if any.
fn pin_cooldown() -> Option<Duration> {
    let mut g = pin_guard();
    match g.until {
        Some(u) if u > Instant::now() => Some(u - Instant::now()),
        Some(_) => {
            g.until = None;
            None
        }
        None => None,
    }
}

/// 4–8 digit PIN entry (dots), 3 wrong attempts → 30 s cooldown. Verifies with
/// `ctx.parental.unlock` and replies `OverlayResult::Unlocked(then)`.
pub struct PinOverlay {
    pub then: Action,
    pub reason: String,
    entered: String,
    error: Option<String>,
}

impl PinOverlay {
    pub fn new(then: Action, reason: String) -> Self { Self { then, reason, entered: String::new(), error: None } }

    fn submit(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if pin_cooldown().is_some() {
            return OverlayOutcome::Keep;
        }
        if !(4..=8).contains(&self.entered.len()) {
            self.error = Some("Enter 4–8 digits".into());
            return OverlayOutcome::Keep;
        }
        if ctx.parental.unlock(&self.entered, &ctx.config.parental) {
            *pin_guard() = PinGuard { failures: 0, until: None };
            return OverlayOutcome::CloseWith(OverlayResult::Unlocked(self.then));
        }
        self.entered.clear();
        let mut g = pin_guard();
        g.failures += 1;
        if g.failures >= PIN_MAX_FAILURES {
            g.failures = 0;
            g.until = Some(Instant::now() + PIN_COOLDOWN);
            self.error = None;
        } else {
            self.error = Some(format!("Wrong PIN ({} of {PIN_MAX_FAILURES} attempts)", g.failures));
        }
        OverlayOutcome::Keep
    }
}

impl Overlay for PinOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        // no text field while cooling down: nothing to type into
        if pin_cooldown().is_some() { vec![KeyContext::Modal] } else { vec![KeyContext::TextInput, KeyContext::Modal] }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Activate => self.submit(ctx),
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
        if ok {
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
}

impl RenameOverlay {
    pub fn new(spec: RenameSpec) -> Self { let t = spec.initial.clone(); Self { spec, text: t, pending: None, error: None } }

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
        self.pending = Some(save_rename(&ctx.services.store, &self.spec.target, name));
        OverlayOutcome::Keep
    }
}

impl Overlay for RenameOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::TextInput, KeyContext::Modal] }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Activate => self.save(ctx),
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
        if scrim(egui_ctx, theme, "rename") {
            return OverlayOutcome::Close;
        }
        let saving = self.pending.is_some();
        let (cancel, ok) = card_area(egui_ctx, theme, "rename", CardKind::Popup, 420.0 * theme.rem(), None, |ui| {
            let hint = match self.spec.target {
                RenameTarget::Item { .. } | RenameTarget::Group { .. } => Some("Leave empty to restore the provider's name."),
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
        if ok {
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
pub struct ListPickerOverlay {
    pub spec: ListPickerSpec,
    cursor: usize,
    /// `Some(name)` while the "New list…" field is open.
    naming: Option<String>,
    creating: Option<Ticket<FavListId>>,
    error: Option<String>,
}

impl ListPickerOverlay {
    pub fn new(spec: ListPickerSpec) -> Self { Self { spec, cursor: 0, naming: None, creating: None, error: None } }

    fn toggle(&mut self, ctx: &mut UiCtx<'_>, list: FavListId) {
        let item = self.spec.item.clone();
        let on = !ctx.favs.lists_of(&item).contains(&list);
        let now = ctx.now;
        let write_item = item.clone();
        // fire-and-forget: DataScope::Favourites makes the app reload `favs` from the store
        drop(ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| {
            crate::store::user::set_favourite(c, list, &write_item, on, now)
        }));
        ctx.favs.apply_toggle(list, &item, on);
    }

    fn create(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(name) = self.naming.as_ref().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) else {
            self.error = Some("The name can't be empty".into());
            return;
        };
        if self.creating.is_some() {
            return;
        }
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
        let name = self.naming.take().unwrap_or_default().trim().to_owned();
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

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        if self.naming.is_some() {
            return match action {
                Action::Activate => {
                    self.create(ctx);
                    OverlayOutcome::Keep
                }
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
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "list-picker") {
            return OverlayOutcome::Close;
        }
        self.cursor = self.cursor.min(ctx.favs.lists.len());
        let row_h = theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg);
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let member: Vec<FavListId> = ctx.favs.lists_of(&self.spec.item).to_vec();
        let lists: Vec<(FavListId, String, String)> = ctx.favs.lists.iter().map(|l| (l.id, l.icon.clone(), l.name.clone())).collect();
        let mut clicked = None;
        let mut close = false;
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
                if resp.clicked() {
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
                    if resp.clicked() {
                        self.cursor = new_row;
                        self.naming = Some(String::new());
                    }
                    paint_menu_row(ui.painter(), rect, theme, glyphs::ADD, "New list…", None, true, self.cursor == new_row);
                }
            }
            error_line(ui, theme, self.error.as_deref());
            ui.add_space(theme.space(SpaceToken::Md));
            ui.horizontal(|ui| {
                ui.label(RichText::new("Enter toggle · Esc done").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = button(ui, theme, None, "Done", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                });
            });
        });
        if let Some(id) = clicked {
            self.toggle(ctx, id);
        }
        if close { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}
