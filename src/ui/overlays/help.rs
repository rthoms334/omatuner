//! `?` / F1 help overlay, GENERATED from the keymap tables (never hand-written). Implemented
//! in the skeleton; owner for polish: WP-10a.
//!
//! Shows the contexts relevant to where the user is first (passed by the app), then all
//! others. Up/Down/PgUp/PgDn scroll; Esc, `?`, F1 close.

use eframe::egui::{self, Align2, RichText};

use super::{Overlay, OverlayOutcome};
use crate::keymap::{self, Action, KeyContext};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::UiCtx;



pub struct HelpOverlay {
    first: Vec<KeyContext>,
    scroll: f32,
}

impl HelpOverlay {
    pub fn new() -> Self { Self { first: Vec::new(), scroll: 0.0 } }
    /// Put the given contexts (current screen's stack) at the top.
    pub fn with_contexts(contexts: Vec<KeyContext>) -> Self { Self { first: contexts, scroll: 0.0 } }

    fn ordered(&self) -> Vec<KeyContext> {
        let mut v: Vec<KeyContext> = self.first.iter().copied().filter(|c| *c != KeyContext::TextInput).collect();
        // context-specific tables first, `Global` last
        for c in KeyContext::ALL.into_iter().filter(|c| *c != KeyContext::Global).chain([KeyContext::Global]) {
            if !v.contains(&c) && c != KeyContext::TextInput {
                v.push(c);
            }
        }
        v
    }
}

impl Default for HelpOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for HelpOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Modal] }

    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Back | Action::ToggleHelp | Action::Activate => OverlayOutcome::Close,
            Action::Down => {
                self.scroll += 40.0;
                OverlayOutcome::Keep
            }
            Action::Up => {
                self.scroll = (self.scroll - 40.0).max(0.0);
                OverlayOutcome::Keep
            }
            Action::PageDown => {
                self.scroll += 400.0;
                OverlayOutcome::Keep
            }
            Action::PageUp => {
                self.scroll = (self.scroll - 400.0).max(0.0);
                OverlayOutcome::Keep
            }
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let screen = egui_ctx.content_rect();
        let mut close = false;
        // scrim
        egui::Area::new(egui::Id::new("help-scrim")).order(egui::Order::Foreground).fixed_pos(screen.min).show(egui_ctx, |ui| {
            let r = ui.allocate_rect(screen, egui::Sense::click());
            ui.painter().rect_filled(screen, 0.0, t.scrim);
            if r.clicked() {
                close = true;
            }
        });
        let width = (screen.width() - 2.0 * theme.space(SpaceToken::PanelPadding)).min(980.0);
        let height = (screen.height() * 0.85).max(200.0);
        egui::Area::new(egui::Id::new("help-card"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(egui_ctx, |ui| {
                egui::Frame::NONE
                    .fill(t.card_bg)
                    .stroke(egui::Stroke::new(t.card_border_width, t.popup_border))
                    .inner_margin(theme.space(SpaceToken::PanelPadding))
                    .show(ui, |ui| {
                        ui.set_width(width);
                        ui.set_max_height(height);
                        ui.label(RichText::new("Keyboard").size(theme.font(TypeToken::Heading)).color(t.fg));
                        ui.add_space(theme.space(SpaceToken::Md));
                        egui::ScrollArea::vertical().vertical_scroll_offset(self.scroll).show(ui, |ui| {
                            let cols = if width > 760.0 { 2 } else { 1 };
                            let contexts = self.ordered();
                            ui.columns(cols, |columns| {
                                for (i, c) in contexts.iter().enumerate() {
                                    let col = &mut columns[i % cols];
                                    col.add_space(theme.space(SpaceToken::Lg));
                                    col.label(
                                        RichText::new(c.title().to_uppercase())
                                            .size(theme.font(TypeToken::Caption))
                                            .color(t.text_section)
                                            .strong(),
                                    );
                                    for (keys, help) in keymap::help_rows(*c) {
                                        col.horizontal(|ui| {
                                            ui.add_sized(
                                                [150.0, theme.font(TypeToken::Body) + 4.0],
                                                egui::Label::new(RichText::new(keys).color(t.accent).size(theme.font(TypeToken::Body))),
                                            );
                                            ui.label(RichText::new(help).color(t.fg).size(theme.font(TypeToken::Body)));
                                        });
                                    }
                                }
                            });
                        });
                    });
            });
        if close { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}
