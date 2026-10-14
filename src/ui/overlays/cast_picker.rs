//! "Cast to…": the device picker (DESIGN §40 §3.1). OWNER: CAST-8.
//!
//! ```text
//!                        ┌ CAST TO ────────────────────────────────┐
//!                        │ ✓ This computer                         │
//!                        │   Living Room                           │
//!                        │   Apple TV 4K (3rd generation) · Paired │
//!                        │   Living Room TV                        │
//!                        │   Hisense 85E75LE · Pair first          │
//!                        │   OT Macbook Air                        │  ← greyed, not selectable
//!                        │   Mac · Can't cast to Macs              │
//!                        │ ━━━━━━━━━━━━━━━━━━━━━━━  Searching…     │
//!                        │   Add by address…                       │
//!                        │   Firewall help…                        │
//!                        └─────────────────────────────────────────┘
//! ```
//! A right-edge panel in OSD colours (`osd_scrim`, `osd_text`, `osd_accent`), so it is readable
//! over the video, built like `tracks.rs`: rows, a cursor, `place_cursor` and `choose`. "This
//! computer" comes first, then the devices from `CastHandle::devices()` (most recently used
//! first) with their kind and state on a second line; no address appears anywhere. The panel
//! calls `CastHandle::browse()` every frame (discovery runs only while it is open) and shows
//! "Searching…" with a busy bar while `searching()`; after [`NOTHING_FOUND_AFTER`] with no TV
//! seen: "No TVs found". "Firewall help…" opens the firewall card (CAST-9).
//!
//! Enter (or a click) on:
//! * a paired TV: `PlayerCommand::CastTo` (what plays moves there) and the panel closes;
//! * an unpaired TV: the PIN overlay (`OverlayRequest::CastPair`), which casts after pairing;
//! * "This computer" while casting: **Play here** (`StopCasting { play_here: true }`); while
//!   not casting it is where you already are;
//! * "Add by address…": a one-line IPv4 field in its place (`[TextInput, Modal]`; Enter probes
//!   `GET /info` on port 7000, Esc leaves the field).
//!
//! While casting the cursor starts on "This computer", so stopping is `C` then Enter. A Mac or
//! a TV set to "Same Home only" is listed greyed and the cursor skips it. In multi-view the
//! panel is one line: "Leave multi-view to cast". Esc, ✕ or a click outside closes.
//!
//! The same panel serves two more purposes ([`For`]):
//! * **"Cast to…" an item** (`OverlayRequest::CastItem`, DESIGN §40 §3.1): headed with the item,
//!   no "This computer" row; a TV casts that item (`ui::casting::cast_target`: as it plays here,
//!   only on the TV), an unpaired one pairs first and then casts it.
//! * **Play on** (`OverlayRequest::PlayOn`, §3.9): headed "PLAY ON", the mark on where plays go
//!   now (the TV casting, else the switch's); a row sets the switch and what plays follows it
//!   (`ui::casting::choose`), an unpaired TV pairs first. Not refused in multi-view: the switch
//!   applies once it ends.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, FontId, Rect};

use super::{Overlay, OverlayOutcome};
use crate::cast::{Device, Pairing, Usability};
use crate::keymap::{Action, KeyContext};
use crate::model::{CastErrorKind, DeviceId};
use crate::player::PlayerCommand;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::casting::{AfterPair, CastDest, ItemCast};
use crate::ui::video_host::osd_text;
use crate::ui::widgets::glyphs;
use crate::ui::widgets::text_field::text_field;
use crate::ui::{OverlayRequest, UiCtx};

/// "No TVs found" after this long without one seen (DESIGN §40 §3.1).
const NOTHING_FOUND_AFTER: Duration = Duration::from_secs(6);

/// One row of the panel.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    /// "This computer"; `here`: nothing is cast (it is the current choice).
    Here {
        here: bool,
    },
    /// A TV; `current`: it is the one casting.
    Device {
        device: Box<Device>,
        current: bool,
    },
    /// "Searching…" with the busy bar.
    Searching,
    NoneFound,
    Add,
    Help,
}

/// What choosing a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    CastTo(DeviceId),
    Pair(DeviceId),
    PlayHere,
    AddAddress,
    FirewallHelp,
    /// Already there: close the panel.
    Close,
    /// A row that isn't a choice (greyed, a status line).
    Nothing,
}

/// The inline "Add by address…" field and its progress.
#[derive(Default)]
struct Adding {
    text: String,
    error: Option<&'static str>,
    /// The probe is out; waiting for `take_add_outcome`.
    probing: bool,
    /// Enter was pressed: acted on after the field was drawn this frame (the typed text of the
    /// same frame counts).
    enter: bool,
}

/// What the panel chooses a TV for.
#[derive(Debug, Clone, PartialEq)]
enum For {
    /// `C` in the player, its Cast control and menu: what plays moves to the TV chosen.
    Playing,
    /// "Cast to…" on an item: that item goes to the TV chosen.
    Item(ItemCast),
    /// The Play on chooser: where plays go.
    PlayOn,
}

pub struct CastPicker {
    purpose: For,
    cursor: usize,
    /// The user moved the cursor (a key or the pointer): it stays where they put it, instead of
    /// following the best row while discovery is still finding TVs.
    touched: bool,
    scroll_to_cursor: bool,
    opened: Option<Instant>,
    adding: Option<Adding>,
    /// A TV found by "Add by address…": the cursor goes to it once it is listed.
    focus: Option<DeviceId>,
}

/// What a click on the panel did.
enum Click {
    Row(usize),
    Close,
}

impl CastPicker {
    pub fn new() -> Self {
        Self { purpose: For::Playing, cursor: 0, touched: false, scroll_to_cursor: true, opened: None, adding: None, focus: None }
    }

    /// "Cast to…" on an item (DESIGN §40 §3.1).
    pub fn for_item(item: ItemCast) -> Self {
        Self { purpose: For::Item(item), ..Self::new() }
    }

    /// The Play on chooser (DESIGN §40 §3.9).
    pub fn play_on() -> Self {
        Self { purpose: For::PlayOn, ..Self::new() }
    }

    /// Multi-view can't cast: the panel is one line (the switch can still be set).
    fn refused(&self, ctx: &UiCtx<'_>) -> bool {
        ctx.multiview.on() && self.purpose != For::PlayOn
    }

    fn since_open(&self) -> Duration {
        self.opened.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Where the cursor starts: "This computer" while casting; else the configured default TV,
    /// else the first one that can be chosen, else "This computer".
    fn best(rows: &[Row], casting: bool, default: Option<&str>) -> usize {
        if casting {
            return 0;
        }
        let device = |r: &Row| match r {
            Row::Device { device, .. } if device.selectable() => Some(device.id),
            _ => None,
        };
        let by_default = default.and_then(|d| d.parse::<DeviceId>().ok()).and_then(|id| rows.iter().position(|r| device(r) == Some(id)));
        by_default.or_else(|| rows.iter().position(|r| device(r).is_some())).unwrap_or(0)
    }

    /// Move to the next row that can be chosen in direction `by`; the cursor stops at the ends.
    fn step(&mut self, rows: &[Row], by: isize) {
        let mut i = self.cursor as isize + by;
        while (0..rows.len() as isize).contains(&i) {
            if row_enabled(&rows[i as usize]) {
                self.cursor = i as usize;
                break;
            }
            i += by;
        }
        self.touched = true;
        self.scroll_to_cursor = true;
    }

    /// Enter / click on a row.
    fn choose(&mut self, ctx: &mut UiCtx<'_>, row: &Row) -> OverlayOutcome {
        match pick(row) {
            Pick::CastTo(device) => {
                match &self.purpose {
                    For::Playing => ctx.play(PlayerCommand::CastTo { device, play: None }),
                    For::Item(item) => {
                        let tv = match row {
                            Row::Device { device, .. } => device.name.clone(),
                            _ => String::new(),
                        };
                        crate::ui::casting::cast_target(ctx, CastDest { device, tv, title: item.title.clone() }, &item.target);
                    }
                    For::PlayOn => crate::ui::casting::choose(ctx, Some(device)),
                }
                OverlayOutcome::Close
            }
            Pick::Pair(device) => {
                let then = match &self.purpose {
                    For::Playing => AfterPair::CastPlaying,
                    For::Item(item) => AfterPair::Item(item.clone()),
                    For::PlayOn => AfterPair::PlayOn,
                };
                ctx.open(match then {
                    AfterPair::CastPlaying => OverlayRequest::CastPair { device, then_cast: true },
                    then => OverlayRequest::CastPairThen { device, then },
                });
                OverlayOutcome::Close
            }
            Pick::PlayHere => {
                match self.purpose {
                    For::PlayOn => crate::ui::casting::choose(ctx, None),
                    _ => ctx.play(PlayerCommand::StopCasting { play_here: true }),
                }
                OverlayOutcome::Close
            }
            Pick::AddAddress => {
                self.adding = Some(Adding::default());
                OverlayOutcome::Keep
            }
            Pick::FirewallHelp => {
                ctx.open(OverlayRequest::CastFirewall);
                OverlayOutcome::Close
            }
            // Play on: the TV casting now (a one-off) becomes where plays go from now on
            Pick::Close => {
                if let (For::PlayOn, Row::Device { device, .. }) = (&self.purpose, row)
                    && ctx.services.tiles.a().handle().snapshot().play_on != Some(device.id)
                {
                    crate::ui::casting::choose(ctx, Some(device.id));
                }
                OverlayOutcome::Close
            }
            Pick::Nothing => OverlayOutcome::Keep,
        }
    }

    /// The rows now: the TVs known, whether a browse runs and how long the panel has been up.
    /// The mark: the TV casting (the player's picker), where plays go (Play on), none (an item).
    fn rows(&self, ctx: &UiCtx<'_>, devices: Vec<Device>, searching: bool) -> Vec<Row> {
        let (current, here) = match self.purpose {
            For::Playing => (ctx.player.cast.as_ref().map(|c| c.device), true),
            For::Item(_) => (None, false),
            For::PlayOn => (crate::ui::casting::plays_on(&ctx.services.tiles.a().handle().snapshot()), true),
        };
        rows_of(devices, current, here, searching, self.since_open())
    }

    /// Where the cursor starts ([`Self::best`]; Play on: the marked row).
    fn start(&self, rows: &[Row], casting: bool, default: Option<&str>) -> usize {
        match self.purpose {
            For::PlayOn => rows.iter().position(|r| matches!(r, Row::Here { here: true } | Row::Device { current: true, .. })).unwrap_or(0),
            For::Item(_) => Self::best(rows, false, default),
            For::Playing => Self::best(rows, casting, default),
        }
    }

    /// The panel's heading: "CAST TO", "CAST “Dune” TO", "PLAY ON".
    fn heading(&self) -> String {
        match &self.purpose {
            For::Playing => "CAST TO".into(),
            For::Item(item) => format!("CAST “{}” TO", item.title),
            For::PlayOn => "PLAY ON".into(),
        }
    }

    /// Enter in the address field: probe it.
    fn probe(&mut self, ctx: &UiCtx<'_>) {
        let Some(add) = &mut self.adding else { return };
        if add.probing {
            return;
        }
        match add.text.trim().parse::<Ipv4Addr>() {
            Ok(ip) => {
                add.error = None;
                add.probing = true;
                ctx.services.agent.cast().add_by_address(ip);
            }
            Err(_) => add.error = Some("Enter an IPv4 address like 192.168.1.20"),
        }
    }

    /// The probe's answer: the TV joins the list (the cursor goes to it), or why not.
    fn poll_probe(&mut self, ctx: &UiCtx<'_>) {
        let Some(add) = &mut self.adding else { return };
        if !add.probing {
            return;
        }
        match ctx.services.agent.cast().take_add_outcome() {
            Some(Ok(id)) => {
                self.focus = Some(id);
                self.touched = false;
                self.adding = None;
            }
            Some(Err(kind)) => {
                add.probing = false;
                add.error = Some(add_error(kind));
            }
            None => {}
        }
    }
}

impl Default for CastPicker {
    fn default() -> Self {
        Self::new()
    }
}

/// The rows for a state (pure: `casting` is the TV marked — casting now, or where plays go —
/// `here` whether "This computer" is offered, `age` how long the panel has been open).
fn rows_of(devices: Vec<Device>, casting: Option<DeviceId>, here: bool, searching: bool, age: Duration) -> Vec<Row> {
    let seen = devices.iter().any(|d| d.available);
    let mut rows = if here { vec![Row::Here { here: casting.is_none() }] } else { Vec::new() };
    rows.extend(devices.into_iter().map(|d| Row::Device { current: casting == Some(d.id), device: Box::new(d) }));
    if searching {
        rows.push(Row::Searching);
    }
    if !seen && age >= NOTHING_FOUND_AFTER {
        rows.push(Row::NoneFound);
    }
    rows.push(Row::Add);
    rows.push(Row::Help);
    rows
}

/// The cursor can rest on it.
fn row_enabled(row: &Row) -> bool {
    match row {
        Row::Device { device, .. } => device.selectable(),
        Row::Searching | Row::NoneFound => false,
        Row::Here { .. } | Row::Add | Row::Help => true,
    }
}

/// What choosing `row` does.
fn pick(row: &Row) -> Pick {
    match row {
        Row::Here { here: true } => Pick::Close,
        Row::Here { here: false } => Pick::PlayHere,
        Row::Device { device, .. } if !device.selectable() => Pick::Nothing,
        Row::Device { current: true, .. } => Pick::Close,
        Row::Device { device, .. } if device.pairing == Pairing::Required => Pick::Pair(device.id),
        Row::Device { device, .. } => Pick::CastTo(device.id),
        Row::Add => Pick::AddAddress,
        Row::Help => Pick::FirewallHelp,
        Row::Searching | Row::NoneFound => Pick::Nothing,
    }
}

/// The second line of a TV: what it is and its state ("Hisense 85E75LE · Paired", "Apple TV
/// 4K (3rd generation) · Paired"; [`Device::make`]). Never an address, and every string is the
/// sanitized one; the OS version lives in Settings only.
fn device_line(d: &Device) -> String {
    let kind = match d.usability {
        Usability::Mac => "Mac".to_owned(),
        _ => d.make(),
    };
    let mut parts = vec![kind.as_str()];
    match d.usability {
        Usability::Mac => parts.push("Can't cast to Macs"),
        Usability::HomeOnly => parts.push("Same Home only"),
        Usability::Usable => match d.pairing {
            Pairing::Required => parts.push("Pair first"),
            Pairing::Paired(_) => parts.push("Paired"),
            Pairing::NotNeeded => {}
        },
    }
    if !d.available {
        parts.push("Not found now");
    }
    parts.join(" · ")
}

/// What a greyed row's tooltip says about the setting that would let it through.
fn device_hint(d: &Device) -> Option<&'static str> {
    match d.usability {
        Usability::Usable => None,
        Usability::Mac => Some("Tuner can't cast to a Mac."),
        Usability::HomeOnly => Some("On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network."),
    }
}

/// Why "Add by address…" found no TV (fixed sentences: nothing the device said).
fn add_error(kind: CastErrorKind) -> &'static str {
    match kind {
        CastErrorKind::Unsupported => "That isn't a TV Tuner can cast to.",
        _ => "Nothing answered at that address.",
    }
}

fn row_text(row: &Row) -> (String, Option<String>) {
    match row {
        Row::Here { .. } => ("This computer".into(), None),
        Row::Device { device, .. } => (device.name.clone(), Some(device_line(device))),
        Row::Searching => ("Searching…".into(), None),
        Row::NoneFound => ("No TVs found".into(), None),
        Row::Add => ("Add by address…".into(), None),
        Row::Help => ("Firewall help…".into(), None),
    }
}

impl Overlay for CastPicker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        // the address field owns the keyboard while it is open
        if self.adding.is_some() { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        if let Some(add) = &mut self.adding {
            return match action {
                Action::Activate => {
                    add.enter = true;
                    OverlayOutcome::Keep
                }
                Action::Back => {
                    self.adding = None;
                    OverlayOutcome::Keep
                }
                _ => OverlayOutcome::Ignored,
            };
        }
        if self.refused(ctx) {
            return match action {
                Action::Activate | Action::Back => OverlayOutcome::Close,
                _ => OverlayOutcome::Ignored,
            };
        }
        let cast = ctx.services.agent.cast();
        let rows = self.rows(ctx, cast.devices(), cast.searching());
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        match action {
            Action::Up => {
                self.step(&rows, -1);
                OverlayOutcome::Keep
            }
            Action::Down => {
                self.step(&rows, 1);
                OverlayOutcome::Keep
            }
            Action::Activate => {
                let row = rows[self.cursor].clone();
                self.choose(ctx, &row)
            }
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme: &Theme = ctx.theme;
        let t = &theme.tokens;
        self.opened.get_or_insert_with(Instant::now);
        let screen = egui_ctx.content_rect();
        let s = crate::ui::layout::osd_scale(screen.width());
        let row_h = (theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg)) * s;
        let pad = theme.space(SpaceToken::PopupPadding) * s;
        let width = (360.0 * theme.rem() * s).min(screen.width() - 2.0 * pad);
        let title_h = theme.font(TypeToken::Caption) * s * 2.0;
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let caption = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let multiview = self.refused(ctx);

        // discovery runs while the panel is up, and only then (DESIGN §40 §6)
        let cast = ctx.services.agent.cast();
        if !multiview {
            cast.browse();
        }
        self.poll_probe(ctx);
        let searching = cast.searching();
        let devices = cast.devices();
        let default = ctx.config.cast.default_device.clone();
        let casting = ctx.player.cast.is_some();
        let rows = if multiview { Vec::new() } else { self.rows(ctx, devices, searching) };
        if let Some(id) = self.focus.take_if(|id| rows.iter().any(|r| matches!(r, Row::Device { device, .. } if device.id == *id))) {
            self.cursor = rows.iter().position(|r| matches!(r, Row::Device { device, .. } if device.id == id)).unwrap_or(self.cursor);
            self.touched = true;
        }
        if !self.touched && !rows.is_empty() {
            self.cursor = self.start(&rows, casting, default.as_deref());
        }
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        // the clock for "No TVs found" and the busy bar
        egui_ctx.request_repaint_after(if searching { Duration::from_millis(33) } else { Duration::from_millis(250) });

        let heights: Vec<f32> = rows.iter().map(|r| row_height(r, row_h)).collect();
        let body_h = if multiview {
            row_h
        } else {
            heights.iter().sum::<f32>() + self.adding.as_ref().map_or(0.0, |a| if a.error.is_some() { row_h * 0.7 } else { 0.0 })
        };
        let height = (title_h + body_h + 2.0 * pad).min(screen.height() - 2.0 * pad);
        let panel = Rect::from_min_size(
            egui::pos2(screen.right() - width - pad, screen.center().y - height / 2.0).max(screen.min),
            egui::vec2(width, height),
        );
        let mut outcome = OverlayOutcome::Keep;
        // a click outside (either button) closes (no theme scrim over the video)
        egui::Area::new(egui::Id::new("cast-picker-catcher")).movable(false).order(egui::Order::Foreground).fixed_pos(screen.min).show(
            egui_ctx,
            |ui| {
                let r = ui.allocate_rect(screen, egui::Sense::click());
                if r.clicked() || r.secondary_clicked() {
                    outcome = OverlayOutcome::Close;
                }
            },
        );
        let pointer_moved = egui_ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let look = Look { theme, font, caption, pad, row_h, heading: self.heading() };
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let mut click = None;
        let mut picked_hover = None;
        let time = egui_ctx.input(|i| i.time);
        egui::Area::new(egui::Id::new("cast-picker-panel")).movable(false).order(egui::Order::Tooltip).fixed_pos(panel.min).show(egui_ctx, |ui| {
            ui.set_min_size(panel.size());
            ui.set_max_size(panel.size());
            ui.painter().rect_filled(panel, t.rounding, t.osd_scrim);
            let inner = panel.shrink2(egui::vec2(pad / 2.0, pad));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            if let Some(c) = look.header(&mut ui, title_h) {
                click = Some(c);
            }
            if multiview {
                let (r, _) = ui.allocate_exact_size(egui::vec2(inner.width(), row_h), egui::Sense::hover());
                osd_text(
                    ui.painter(),
                    r.left_center() + egui::vec2(pad / 2.0, 0.0),
                    Align2::LEFT_CENTER,
                    "Leave multi-view to cast",
                    look.font.clone(),
                    with_alpha(t.osd_text, 0.7),
                    r.width() - pad,
                );
                return;
            }
            egui::ScrollArea::vertical().id_salt("cast-picker-list").auto_shrink([false, true]).show(&mut ui, |ui| {
                for (i, row) in rows.iter().enumerate() {
                    if self.adding.is_some() && matches!(row, Row::Add) {
                        if let Some(add) = &mut self.adding {
                            look.address_row(ui, add);
                        }
                        continue;
                    }
                    let h = heights[i];
                    let enabled = row_enabled(row);
                    let at = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), h));
                    if pointer_moved && enabled && ui.rect_contains_pointer(at) {
                        picked_hover = Some(i);
                    }
                    if let Some(c) = look.row(ui, i, row, h, i == self.cursor, scroll, time) {
                        click = Some(c);
                    }
                }
            });
        });
        if let Some(i) = picked_hover {
            self.cursor = i;
            self.touched = true;
        }
        // Enter in the address field acts after the field drew this frame's typing
        if self.adding.as_mut().is_some_and(|a| std::mem::take(&mut a.enter)) {
            self.probe(ctx);
        }
        match click {
            Some(Click::Row(i)) => {
                self.cursor = i;
                self.touched = true;
                outcome = self.choose(ctx, &rows[i]);
            }
            Some(Click::Close) => outcome = OverlayOutcome::Close,
            None => {}
        }
        outcome
    }
}

fn row_height(row: &Row, row_h: f32) -> f32 {
    match row {
        Row::Device { .. } => row_h * 1.7,
        Row::Searching => row_h * 0.8,
        _ => row_h,
    }
}

/// Sizes and fonts of the panel at this OSD scale (drawing helpers for its parts).
struct Look<'a> {
    theme: &'a Theme,
    font: FontId,
    caption: FontId,
    pad: f32,
    row_h: f32,
    heading: String,
}

impl Look<'_> {
    /// A clickable OSD target: hover fill and the pointing hand.
    fn target(&self, ui: &egui::Ui, rect: Rect, id: impl std::hash::Hash + std::fmt::Debug, label: &str, hover_text: Option<&str>) -> egui::Response {
        let t = &self.theme.tokens;
        let resp = ui.interact(rect, ui.id().with(id), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
        if resp.hovered() {
            ui.painter().rect_filled(rect, t.rounding, with_alpha(t.osd_text, 0.12));
        }
        match hover_text {
            Some(tip) => resp.on_hover_text(tip),
            None => resp,
        }
    }

    /// `CAST TO                      ✕` (or the item's, or `PLAY ON`).
    fn header(&self, ui: &mut egui::Ui, h: f32) -> Option<Click> {
        let t = &self.theme.tokens;
        let (head, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        osd_text(
            ui.painter(),
            egui::pos2(head.left() + self.pad / 2.0, head.center().y),
            Align2::LEFT_CENTER,
            &self.heading,
            self.caption.clone(),
            t.osd_accent,
            head.width() - head.height() - self.pad,
        );
        let close = Rect::from_min_size(egui::pos2(head.right() - head.height(), head.top()), egui::vec2(head.height(), head.height()));
        let clicked = self.target(ui, close, "close", "Close (Esc)", Some("Close (Esc)")).clicked();
        osd_text(ui.painter(), close.center(), Align2::CENTER_CENTER, glyphs::CLOSE, self.font.clone(), t.osd_text, f32::INFINITY);
        clicked.then_some(Click::Close)
    }

    /// One row: a mark for the current choice, the name and (TVs) the state line.
    #[allow(clippy::too_many_arguments)]
    fn row(&self, ui: &mut egui::Ui, i: usize, row: &Row, h: f32, cursor: bool, scroll: bool, time: f64) -> Option<Click> {
        let t = &self.theme.tokens;
        let pad = self.pad;
        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        if scroll && cursor {
            ui.scroll_to_rect(r, None);
        }
        let enabled = row_enabled(row);
        let (name, line) = row_text(row);
        if matches!(row, Row::Searching) {
            // a moving block in OSD colours (the theme's busy bar would vanish on the scrim)
            let bar = Rect::from_min_size(egui::pos2(r.left() + pad * 2.0, r.center().y - 2.0), egui::vec2(r.width() * 0.4, 4.0));
            ui.painter().rect_filled(bar, t.rounding, with_alpha(t.osd_text, 0.25));
            let block = bar.width() * 0.3;
            let x = bar.left() - block + (bar.width() + block) * (time % 1.0) as f32;
            let blk = Rect::from_min_max(egui::pos2(x.max(bar.left()), bar.top()), egui::pos2((x + block).min(bar.right()), bar.bottom()));
            if blk.width() > 0.0 {
                ui.painter().rect_filled(blk, t.rounding, t.osd_accent);
            }
            osd_text(
                ui.painter(),
                egui::pos2(bar.right() + pad / 2.0, r.center().y),
                Align2::LEFT_CENTER,
                &name,
                self.caption.clone(),
                with_alpha(t.osd_text, 0.7),
                f32::INFINITY,
            );
            return None;
        }
        let label = match &line {
            Some(l) => format!("{name}, {l}"),
            None => name.clone(),
        };
        let tip = match row {
            Row::Device { device, .. } => device_hint(device),
            _ => None,
        };
        if cursor {
            ui.painter().rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
        }
        let clicked = enabled && self.target(ui, r, ("row", i), &label, tip).clicked();
        if !enabled {
            // greyed rows are named for the accessibility tree and explain themselves on hover
            let resp = ui.interact(r, ui.id().with(("row", i)), egui::Sense::hover());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label.clone()));
            if let Some(tip) = tip {
                resp.on_hover_text(tip);
            }
        }
        let current = matches!(row, Row::Here { here: true } | Row::Device { current: true, .. });
        if current {
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
        let color = match (enabled, cursor) {
            (false, _) => with_alpha(t.osd_text, 0.45),
            (true, true) => t.osd_accent,
            (true, false) => t.osd_text,
        };
        let x = r.left() + pad * 2.0;
        let w = r.width() - pad * 2.5;
        match line {
            Some(l) => {
                osd_text(ui.painter(), egui::pos2(x, r.top() + h * 0.34), Align2::LEFT_CENTER, &name, self.font.clone(), color, w);
                osd_text(ui.painter(), egui::pos2(x, r.top() + h * 0.74), Align2::LEFT_CENTER, &l, self.caption.clone(), with_alpha(color, 0.75), w);
            }
            None => {
                osd_text(ui.painter(), egui::pos2(x, r.center().y), Align2::LEFT_CENTER, &name, self.font.clone(), color, w);
            }
        }
        clicked.then_some(Click::Row(i))
    }

    /// The "Add by address…" row while its field is open.
    fn address_row(&self, ui: &mut egui::Ui, add: &mut Adding) {
        let t = &self.theme.tokens;
        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), self.row_h), egui::Sense::hover());
        let mut field = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(egui::vec2(self.pad / 2.0, self.row_h * 0.1))));
        field.add_enabled_ui(!add.probing, |ui| {
            let resp = text_field(ui, self.theme, &mut add.text, "192.168.1.20", false);
            if resp.changed() {
                add.error = None;
            }
            if !add.probing && !resp.has_focus() {
                resp.request_focus();
            }
        });
        let note = if add.probing { Some(("Looking for a TV…", with_alpha(t.osd_text, 0.7))) } else { add.error.map(|e| (e, t.urgent)) };
        if let Some((text, color)) = note {
            let (nr, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), self.row_h * 0.7), egui::Sense::hover());
            osd_text(
                ui.painter(),
                egui::pos2(nr.left() + self.pad * 2.0, nr.center().y),
                Align2::LEFT_CENTER,
                text,
                self.caption.clone(),
                color,
                nr.width() - self.pad * 2.5,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::cast::testing::{FakeCall, FakeCast};
    use crate::cast::{CastHandle, PairVariant, Protocol};
    use crate::model::{CastPhase, CastView, ReceiverKind};
    use crate::ui::screens::home::testkit::{OvApp, has_label, overlay_harness, painted, rig, until_ov};
    use crate::ui::{NavRequest, OverlayRequest};

    type H = egui_kittest::Harness<'static, OvApp<CastPicker>>;

    fn id(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 16])
    }

    fn tv(n: u8, name: &str, kind: ReceiverKind, pairing: Pairing) -> Device {
        Device { pairing, ..Device::new(id(n), name, kind, Protocol::AirPlay) }
    }

    /// Living Room (paired Apple TV), Living Room TV (a Hisense: pair first), a Mac; the models
    /// and maker are the user's real registry rows.
    fn devices() -> Vec<Device> {
        vec![
            Device { model: Some("AppleTV14,1".into()), ..tv(1, "Living Room", ReceiverKind::AppleTv, Pairing::Paired(PairVariant::Hkp3)) },
            Device {
                model: Some("85E75LE".into()),
                manufacturer: Some("Hisense".into()),
                ..tv(2, "Living Room TV", ReceiverKind::AirPlayTv, Pairing::Required)
            },
            Device { usability: Usability::Mac, ..tv(3, "OT Macbook Air", ReceiverKind::AppleTv, Pairing::Required) },
        ]
    }

    fn picker(devices: Vec<Device>) -> (H, Arc<FakeCast>) {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.devices = devices);
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let h = overlay_harness(t, one, CastPicker::new(), egui::vec2(1200.0, 800.0));
        (h, fake)
    }

    fn row(device: Device) -> Row {
        Row::Device { device: Box::new(device), current: false }
    }

    /// The second line says what the TV is (a third-party TV by maker and model, so the Hisense
    /// isn't taken for a generic entry; an Apple TV by its model's name when known) and its
    /// state, never an address.
    #[test]
    fn the_second_line_names_the_tv_and_the_state_without_an_address() {
        let ds = devices();
        assert_eq!(device_line(&ds[0]), "Apple TV 4K (3rd generation) · Paired");
        assert_eq!(device_line(&ds[1]), "Hisense 85E75LE · Pair first");
        assert_eq!(device_line(&ds[2]), "Mac · Can't cast to Macs");
        let unnamed = tv(7, "Den TV", ReceiverKind::AirPlayTv, Pairing::Required);
        assert_eq!(device_line(&unnamed), "AirPlay TV · Pair first", "remembered, not announced this run");
        let home = Device { usability: Usability::HomeOnly, ..tv(4, "Bedroom", ReceiverKind::AppleTv, Pairing::Required) };
        assert_eq!(device_line(&home), "Apple TV · Same Home only");
        assert!(device_hint(&home).is_some_and(|h| h.contains("Anyone on the Same Network")));
        let gone = Device { available: false, ..tv(5, "Den", ReceiverKind::AppleTv, Pairing::Paired(PairVariant::Hkp3)) };
        assert_eq!(device_line(&gone), "Apple TV · Paired · Not found now");
        let addressed = Device::new(id(6), "Study", ReceiverKind::AppleTv, Protocol::AirPlay).at("192.168.1.77:7000".parse().unwrap());
        assert!(!device_line(&addressed).contains("192.168"));
    }

    #[test]
    fn rows_follow_what_discovery_has_found() {
        let none = |age| rows_of(Vec::new(), None, true, true, Duration::from_secs(age));
        assert_eq!(none(0), vec![Row::Here { here: true }, Row::Searching, Row::Add, Row::Help]);
        assert_eq!(none(7), vec![Row::Here { here: true }, Row::Searching, Row::NoneFound, Row::Add, Row::Help]);
        // a TV seen: never "No TVs found"; a remembered one that isn't on the network doesn't count
        let rows = rows_of(devices(), Some(id(1)), true, false, Duration::from_secs(30));
        assert_eq!(rows.len(), 1 + 3 + 2);
        assert!(matches!(&rows[0], Row::Here { here: false }));
        assert!(matches!(&rows[1], Row::Device { current: true, .. }) && matches!(&rows[2], Row::Device { current: false, .. }));
        let remembered = Device { available: false, ..devices().remove(0) };
        assert!(rows_of(vec![remembered], None, true, false, Duration::from_secs(7)).contains(&Row::NoneFound));
    }

    #[test]
    fn enter_on_a_row_does_what_the_design_says() {
        let [paired, unpaired, mac]: [Device; 3] = devices().try_into().unwrap();
        assert_eq!(pick(&row(paired.clone())), Pick::CastTo(id(1)));
        assert_eq!(pick(&row(unpaired)), Pick::Pair(id(2)));
        assert_eq!(pick(&row(mac)), Pick::Nothing, "a Mac is greyed out");
        assert_eq!(pick(&Row::Device { device: Box::new(paired), current: true }), Pick::Close);
        assert_eq!(pick(&Row::Here { here: false }), Pick::PlayHere, "This computer while casting");
        assert_eq!(pick(&Row::Here { here: true }), Pick::Close);
        assert_eq!((pick(&Row::Add), pick(&Row::Help), pick(&Row::NoneFound)), (Pick::AddAddress, Pick::FirewallHelp, Pick::Nothing));
    }

    #[test]
    fn the_cursor_starts_on_the_default_or_first_usable_tv_and_on_this_computer_while_casting() {
        let rows = rows_of(devices(), None, true, false, Duration::ZERO);
        assert_eq!(CastPicker::best(&rows, false, None), 1, "the first TV (most recent first)");
        assert_eq!(CastPicker::best(&rows, false, Some(&id(2).to_string())), 2, "cast.default_device");
        assert_eq!(CastPicker::best(&rows, false, Some(&id(3).to_string())), 1, "a Mac can't be the default");
        assert_eq!(CastPicker::best(&rows, true, None), 0, "C then Enter stops casting");
        assert_eq!(CastPicker::best(&rows_of(Vec::new(), None, true, true, Duration::ZERO), false, None), 0);
    }

    /// The panel's height is its rows' (computed, not the window's) in a tall window too.
    #[test]
    fn the_panel_sizes_to_its_rows_in_a_tall_window() {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.devices = devices());
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = overlay_harness(t, one, CastPicker::new(), egui::vec2(1280.0, 1400.0));
        h.run_steps(3);
        let height = h.ctx.memory(|m| m.area_rect(egui::Id::new("cast-picker-panel"))).map_or(0.0, |r| r.height());
        assert!(height > 100.0 && height < 700.0, "{height} px tall in a 1400 px window");
    }

    /// The panel lists the TVs with their state lines and starts discovery; ↓ skips the Mac;
    /// an unpaired TV opens the PIN overlay; "Firewall help…" opens the firewall card.
    #[test]
    fn the_picker_lists_tvs_browses_and_opens_pairing_or_firewall_help() {
        let (mut h, fake) = picker(devices());
        h.run_steps(2);
        let texts = painted(&h);
        for want in [
            "This computer",
            "Living Room",
            "Apple TV 4K (3rd generation) · Paired",
            "Living Room TV",
            "Hisense 85E75LE · Pair first",
            "Mac · Can't cast to Macs",
        ] {
            assert!(texts.iter().any(|t| t == want), "{want}: {texts:?}");
        }
        assert!(has_label(&h, "Add by address…") && has_label(&h, "Firewall help…"));
        assert!(fake.take_calls().contains(&FakeCall::Browse));
        // the cursor starts on the first TV; ↓ to the Hisense, Enter pairs it first
        assert_eq!(h.state().o.cursor, 1);
        assert_eq!(h.state_mut().act(Action::Down), "Keep");
        h.state_mut().nav();
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastPair { device: id(2), then_cast: true })]);
        drop(h);
        // ↓ again skips the Mac and lands on "Add by address…"
        let (mut h, _fake) = picker(devices());
        h.run_steps(2);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().o.cursor, 4);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().o.cursor, 5, "Firewall help…");
        h.state_mut().nav();
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastFirewall)]);
    }

    /// "Cast to…" an item (DESIGN §40 §3.1): the panel names it and offers no "This computer";
    /// the cursor starts on the first TV ready, and an unpaired TV pairs first, then casts that
    /// item. Play on (§3.9): "PLAY ON", the cursor on where plays go (this computer here).
    #[test]
    fn the_item_and_play_on_pickers() {
        use crate::ui::casting::{AfterPair, ItemCast};
        let item = ItemCast { target: crate::ui::item_menu::Target::Channel(crate::model::ChannelId(1)), title: "BBC One".into() };
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| s.devices = devices());
        t.services().agent.set_cast(Some(CastHandle::new(fake)));
        let mut h = overlay_harness(t, one, CastPicker::for_item(item.clone()), egui::vec2(1200.0, 800.0));
        h.run_steps(2);
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t == "CAST “BBC One” TO") && !texts.iter().any(|t| t == "This computer"), "{texts:?}");
        assert_eq!(h.state().o.cursor, 0, "Living Room");
        h.state_mut().act(Action::Down);
        h.state_mut().nav();
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastPairThen { device: id(2), then: AfterPair::Item(item) })]);
        drop(h);
        let (mut h, _fake) = picker(devices());
        h.state_mut().o = CastPicker::play_on();
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "PLAY ON"));
        assert_eq!(h.state().o.cursor, 0, "this computer, marked");
    }

    /// "Add by address…" probes the typed IPv4 address; a bad one says what is expected, and
    /// the TV it finds takes the cursor.
    #[test]
    fn add_by_address_probes_an_ipv4_address() {
        let (mut h, fake) = picker(devices());
        h.run_steps(2);
        h.get_by_label("Add by address…").click();
        h.run_steps(2);
        assert!(h.state().o.adding.is_some());
        assert_eq!(h.state().o.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
        h.state_mut().o.adding.as_mut().unwrap().text = "not an address".into();
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "Enter an IPv4 address like 192.168.1.20"));
        assert!(!fake.take_calls().iter().any(|c| matches!(c, FakeCall::AddByAddress(_))));
        h.state_mut().o.adding.as_mut().unwrap().text = " 192.168.1.9 ".into();
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(fake.take_calls().contains(&FakeCall::AddByAddress("192.168.1.9".parse().unwrap())));
        // the probe found a TV: the field goes, the cursor goes to it
        let found = tv(9, "Study", ReceiverKind::AppleTv, Pairing::Required);
        fake.script(|s| {
            s.devices.push(found);
            s.add_outcome = Some(Ok(id(9)));
        });
        until_ov(&mut h, "the cursor on the new TV", |h| h.state().o.adding.is_none() && h.state().o.cursor == 4);
        // Esc leaves the field before it closes the panel
        h.get_by_label("Add by address…").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().act(Action::Back), "Keep");
        assert!(h.state().o.adding.is_none());
        assert_eq!(h.state_mut().act(Action::Back), "Close");
    }

    /// While casting the cursor starts on "This computer"; with no TV seen for 6 s the panel
    /// says so; in multi-view it is one line and discovery doesn't run.
    #[test]
    fn the_picker_while_casting_after_a_wait_and_in_multi_view() {
        let (mut h, _fake) = picker(devices());
        let view = CastView {
            device: id(1),
            name: "Living Room".into(),
            kind: ReceiverKind::AppleTv,
            phase: CastPhase::Playing,
            level: None,
            as_is: false,
            behind_live_s: None,
            can_seek: false,
            volume: false,
        };
        h.state_mut().t.snapshot = Arc::new(crate::player::PlayerSnapshot { cast: Some(view), ..Default::default() });
        h.run_steps(2);
        assert_eq!(h.state().o.cursor, 0);
        drop(h);
        let (mut h, _fake) = picker(Vec::new());
        h.state_mut().o.opened = Some(Instant::now() - Duration::from_secs(7));
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "No TVs found"));
        drop(h);
        let (mut h, fake) = picker(devices());
        h.state_mut().t.multiview.view.on = true;
        fake.take_calls();
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "Leave multi-view to cast"));
        assert!(!fake.take_calls().contains(&FakeCall::Browse));
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
    }
}
