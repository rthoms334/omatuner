//! Ctrl+I "Edit information" (UHF 1.100.1): rename, per-channel catch-up offset (± minutes,
//! stored as `item_override.catchup_offset_s`), EPG assignment (opens the picker with
//! `epg_match::suggest` candidates), lock, hide. Writes through `store::user::set_item_override`
//! / `store::epg::set_manual_mapping` (fingerprints computed by the store). Owner: WP-11.
//! Key contexts: `[Modal]`, `[TextInput, Modal]` while a field is focused.
//!
//! Rows: Name (text field; Enter on it or a click starts editing, Tab/Esc leave it) · Catch-up
//! offset (channels with an archive; ←/→ ±1 min) · EPG channel (opens the EPG mapping screen
//! for the channel, which ranks `epg_match::suggest` candidates and writes the manual mapping)
//! · Locked · Hidden (Enter/Space/←/→ toggle) · Save / Cancel. Changing the lock needs an
//! unlocked parental session. Everything is read once when the overlay opens (two indexed
//! reads) and saved in one writer job (`DataScope::Overrides`).

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, ItemKind, ItemOverride, ItemRef};
use crate::msg::DataScope;
use crate::store::Ticket;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::screens::live;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::{text_field::text_field, toggle::toggle};
use crate::ui::{Route, UiCtx};

/// Catch-up offset limits (minutes).
const OFFSET_LIMIT_MIN: i64 = 24 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Offset,
    Epg,
    Locked,
    Hidden,
    Save,
    Cancel,
}

/// The item as loaded.
struct Loaded {
    /// The provider's (cleaned) name: an equal or empty name removes the rename.
    provider_name: String,
    channel: Option<ChannelId>,
    has_catchup: bool,
    epg_label: Option<String>,
    ov: ItemOverride,
}

pub struct EditItemOverlay {
    pub item: ItemRef,
    cursor: usize,
    loaded: Option<Result<Loaded, String>>,
    name: String,
    offset_min: i64,
    locked: bool,
    hidden: bool,
    editing: bool,
    /// Ask the text field for egui focus on the next frame.
    focus_field: bool,
    pending: Option<Ticket<()>>,
    error: Option<String>,
    save_requested: bool,
}

impl EditItemOverlay {
    pub fn new(item: ItemRef) -> Self {
        Self {
            item,
            cursor: 0,
            loaded: None,
            name: String::new(),
            offset_min: 0,
            locked: false,
            hidden: false,
            editing: false,
            focus_field: false,
            pending: None,
            error: None,
            save_requested: false,
        }
    }

    fn load(&mut self, ctx: &mut UiCtx<'_>) {
        if self.loaded.is_some() {
            return;
        }
        let (db, item) = (ctx.db, self.item.clone());
        let Some(r) = ctx.budget.run(|| load_item(db, &item)) else { return };
        self.loaded = Some(
            r.map_err(|e| crate::redact::redact(&e.to_string()).into_owned())
                .and_then(|l| l.ok_or_else(|| "This item is no longer in your playlists".to_owned())),
        );
        if let Some(Ok(l)) = &self.loaded {
            self.name = l.ov.rename.clone().unwrap_or_else(|| l.provider_name.clone());
            self.offset_min = l.ov.catchup_offset_s.unwrap_or(0) / 60;
            self.locked = l.ov.locked;
            self.hidden = l.ov.hidden;
        }
    }

    fn fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Name];
        if let Some(Ok(l)) = &self.loaded {
            if l.channel.is_some() && l.has_catchup {
                v.push(Field::Offset);
            }
            if l.channel.is_some() {
                v.push(Field::Epg);
            }
        }
        v.extend([Field::Locked, Field::Hidden, Field::Save, Field::Cancel]);
        v
    }

    fn field(&self) -> Field {
        let f = self.fields();
        f[self.cursor.min(f.len() - 1)]
    }

    fn lock_allowed(&self, ctx: &UiCtx<'_>) -> bool {
        ctx.parental.is_unlocked(&ctx.config.parental)
    }

    fn activate(&mut self, ctx: &mut UiCtx<'_>, f: Field) -> OverlayOutcome {
        match f {
            Field::Name => {
                self.editing = true;
                self.focus_field = true;
            }
            Field::Offset => {}
            Field::Epg => {
                if let Some(Ok(Loaded { channel: Some(ch), .. })) = &self.loaded {
                    ctx.push(Route::EpgMapping { channel: Some(*ch) });
                    return OverlayOutcome::Close;
                }
            }
            Field::Locked => {
                if self.lock_allowed(ctx) {
                    self.locked = !self.locked;
                } else {
                    // (not "Ctrl+L in a list": that would change that item's lock after the PIN)
                    self.error = Some("Changing locks needs the parental PIN — enter it on any locked channel or group first".into());
                }
            }
            Field::Hidden => self.hidden = !self.hidden,
            Field::Save => self.save_requested = true,
            Field::Cancel => return OverlayOutcome::Close,
        }
        OverlayOutcome::Keep
    }

    fn adjust(&mut self, ctx: &mut UiCtx<'_>, f: Field, up: bool) {
        match f {
            Field::Offset => self.offset_min = (self.offset_min + if up { 1 } else { -1 }).clamp(-OFFSET_LIMIT_MIN, OFFSET_LIMIT_MIN),
            Field::Locked | Field::Hidden => {
                self.activate(ctx, f);
            }
            _ => {}
        }
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if self.pending.is_some() {
            return OverlayOutcome::Keep;
        }
        let Some(Ok(l)) = &self.loaded else { return OverlayOutcome::Close };
        let name = self.name.trim();
        let mut ov = l.ov.clone();
        ov.rename = (!name.is_empty() && name != l.provider_name).then(|| name.to_owned());
        ov.catchup_offset_s = (self.offset_min != 0).then_some(self.offset_min * 60);
        ov.locked = self.locked;
        ov.hidden = self.hidden;
        if ov == l.ov {
            return OverlayOutcome::Close;
        }
        let item = self.item.clone();
        self.pending =
            Some(ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::set_item_override(c, &item, &ov)));
        OverlayOutcome::Keep
    }
}

/// Read what the dialog shows (`Ok(None)` = the item vanished).
fn load_item(db: &rusqlite::Connection, item: &ItemRef) -> crate::error::Result<Option<Loaded>> {
    let ov = crate::store::user::item_override(db, item)?.unwrap_or_default();
    let loaded = match item.kind {
        ItemKind::Channel => crate::store::live::channel_by_ref(db, item)?.map(|c| {
            let epg_label = c
                .epg_channel_id
                .and_then(|id| crate::store::epg::epg_channel(db, id).ok())
                .map(|e| e.display_names.first().cloned().unwrap_or(e.xmltv_id));
            Loaded { provider_name: c.clean_name.clone(), channel: Some(c.id), has_catchup: c.catchup.is_some(), epg_label, ov: ov.clone() }
        }),
        ItemKind::Movie => crate::store::vod::resolve_movie(db, item)?.map(|id| crate::store::vod::movie(db, id)).transpose()?.map(|m| Loaded {
            provider_name: m.clean_name,
            channel: None,
            has_catchup: false,
            epg_label: None,
            ov: ov.clone(),
        }),
        ItemKind::Series => db
            .query_row("SELECT clean_name FROM series WHERE source_id = ?1 AND key = ?2", rusqlite::params![item.source, item.key.as_str()], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .map(|n| Loaded { provider_name: n, channel: None, has_catchup: false, epg_label: None, ov: ov.clone() }),
        ItemKind::Episode => crate::store::vod::resolve_episode(db, item)?
            .map(|id| crate::store::vod::episode(db, id))
            .transpose()?
            .map(|e| Loaded { provider_name: e.title, channel: None, has_catchup: false, epg_label: None, ov: ov.clone() }),
    };
    Ok(loaded)
}

impl Overlay for EditItemOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.editing { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.load(ctx);
        if self.editing {
            return match action {
                Action::Back | Action::FocusNext | Action::FocusPrev => {
                    self.editing = false;
                    if action == Action::FocusNext {
                        self.cursor += 1;
                    }
                    OverlayOutcome::Keep
                }
                // Enter in the name field saves (after this frame's typing landed, see `ui`)
                Action::Activate => {
                    self.editing = false;
                    self.save_requested = true;
                    OverlayOutcome::Keep
                }
                _ => OverlayOutcome::Ignored,
            };
        }
        let n = self.fields().len();
        match action {
            Action::Up | Action::FocusPrev => {
                self.cursor = self.cursor.saturating_sub(1);
                OverlayOutcome::Keep
            }
            Action::Down | Action::FocusNext => {
                self.cursor = (self.cursor + 1).min(n - 1);
                OverlayOutcome::Keep
            }
            Action::Left | Action::Right => {
                let f = self.field();
                if matches!(f, Field::Save | Field::Cancel) {
                    self.cursor = if action == Action::Left { n - 2 } else { n - 1 };
                } else {
                    self.adjust(ctx, f, action == Action::Right);
                }
                OverlayOutcome::Keep
            }
            Action::Activate => {
                let f = self.field();
                self.activate(ctx, f)
            }
            Action::Back => OverlayOutcome::Close,
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.load(ctx);
        if let Some(r) = self.pending.as_mut().and_then(Ticket::try_take) {
            self.pending = None;
            match r {
                Ok(()) => return OverlayOutcome::Close,
                Err(e) => self.error = Some(format!("Couldn't save: {}", crate::redact::redact(&e.to_string()))),
            }
        } else if self.pending.is_some() {
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "edit-item") {
            return OverlayOutcome::Close;
        }
        let fields = self.fields();
        self.cursor = self.cursor.min(fields.len() - 1);
        let cur = fields[self.cursor];
        let lock_ok = self.lock_allowed(ctx);
        let lock_label =
            if live::pin_hint(ctx).is_empty() { "Locked (needs the parental PIN)".to_owned() } else { format!("Locked{}", live::pin_hint(ctx)) };
        let mut clicked: Option<Field> = None;
        let mut outcome = OverlayOutcome::Keep;
        let saving = self.pending.is_some();
        card_area(egui_ctx, theme, "edit-item", CardKind::Popup, 460.0 * theme.rem(), None, |ui| {
            ui.label(RichText::new("Edit information").size(theme.font(TypeToken::Heading)).color(t.fg));
            ui.add_space(theme.space(SpaceToken::Lg));
            let loaded = match &self.loaded {
                None => {
                    ui.label(RichText::new("Loading…").color(t.text_secondary));
                    return;
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e.as_str()).size(theme.font(TypeToken::Body)).color(t.urgent));
                    if button(ui, theme, None, "Close", ButtonStyle { bordered: true, focused: true, ..Default::default() }).clicked() {
                        outcome = OverlayOutcome::Close;
                    }
                    return;
                }
                Some(Ok(l)) => l,
            };
            let label = |ui: &mut egui::Ui, s: &str, focused: bool| {
                ui.label(RichText::new(s).size(theme.font(TypeToken::Body)).color(if focused { t.list_cursor_text } else { t.text_secondary }));
            };
            for f in &fields {
                let focused = *f == cur;
                match f {
                    Field::Name => {
                        label(ui, "Name", focused);
                        let resp = text_field(ui, theme, &mut self.name, &loaded.provider_name, false);
                        if self.focus_field {
                            resp.request_focus();
                            self.focus_field = false;
                        } else if resp.clicked() {
                            self.editing = true;
                            self.cursor = 0;
                        } else if self.editing && !resp.has_focus() {
                            // focus went elsewhere (a click outside the field)
                            self.editing = false;
                        }
                        if !self.editing && resp.has_focus() {
                            resp.surrender_focus();
                        }
                        if focused && !self.editing {
                            ui.painter().rect_stroke(resp.rect, t.rounding, egui::Stroke::new(1.0, t.list_cursor_border), egui::StrokeKind::Outside);
                        }
                        ui.label(
                            RichText::new("Leave empty to restore the provider's name.")
                                .size(theme.font(TypeToken::BodySmall))
                                .color(t.text_secondary),
                        );
                    }
                    Field::Offset => {
                        ui.add_space(theme.space(SpaceToken::Md));
                        ui.horizontal(|ui| {
                            label(ui, "Catch-up offset", focused);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if button(ui, theme, None, "+", Default::default()).clicked() {
                                    self.offset_min = (self.offset_min + 1).min(OFFSET_LIMIT_MIN);
                                }
                                let v = format!("{:+} min", self.offset_min);
                                ui.label(RichText::new(v).size(theme.font(TypeToken::Body)).color(if focused { t.list_cursor_text } else { t.fg }));
                                if button(ui, theme, None, "−", Default::default()).clicked() {
                                    self.offset_min = (self.offset_min - 1).max(-OFFSET_LIMIT_MIN);
                                }
                            });
                        });
                    }
                    Field::Epg => {
                        ui.add_space(theme.space(SpaceToken::Md));
                        ui.horizontal(|ui| {
                            label(ui, "EPG channel", focused);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if button(ui, theme, None, "Assign…", ButtonStyle { focused, ..Default::default() }).clicked() {
                                    clicked = Some(Field::Epg);
                                }
                                let current = loaded.epg_label.clone().unwrap_or_else(|| "None".into());
                                ui.add(egui::Label::new(RichText::new(current).size(theme.font(TypeToken::Body)).color(t.fg)).truncate());
                            });
                        });
                    }
                    Field::Locked => {
                        ui.add_space(theme.space(SpaceToken::Md));
                        ui.add_enabled_ui(lock_ok, |ui| {
                            let mut v = self.locked;
                            if toggle(ui, theme, &mut v, &lock_label, focused).changed() {
                                self.locked = v;
                            }
                        });
                    }
                    Field::Hidden => {
                        let mut v = self.hidden;
                        if toggle(ui, theme, &mut v, "Hidden", focused).changed() {
                            self.hidden = v;
                        }
                    }
                    Field::Save | Field::Cancel => {}
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.add(egui::Label::new(RichText::new(e.as_str()).size(theme.font(TypeToken::BodySmall)).color(t.urgent)).wrap());
            }
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                let save = button(
                    ui,
                    theme,
                    None,
                    if saving { "Saving…" } else { "Save" },
                    ButtonStyle { primary: true, focused: cur == Field::Save, ..Default::default() },
                );
                if save.clicked() {
                    clicked = Some(Field::Save);
                }
                if button(ui, theme, None, "Cancel", ButtonStyle { bordered: true, focused: cur == Field::Cancel, ..Default::default() }).clicked() {
                    clicked = Some(Field::Cancel);
                }
            });
        });
        if let Some(f) = clicked {
            outcome = self.activate(ctx, f);
        }
        if std::mem::take(&mut self.save_requested) {
            outcome = self.save(ctx);
        }
        outcome
    }
}
