//! `?` / F1 help overlay, GENERATED from the keymap tables (never hand-written). Owner: WP-10a.
//!
//! Shows the contexts relevant to where the user is first (`with_contexts`, passed by the app),
//! then all others, `Global` last; every context of `KeyContext::ALL` appears, so the overlay is
//! complete by construction. Two columns from 760 pt of card width. Keys: ↑/↓ (j/k) scroll,
//! PgUp/PgDn page, Esc, `?`, F1 or Enter close. Uses the `Panel` key context (no type-to-filter
//! here, so `?` and the vim keys are free).

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{self, Action, KeyContext};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::card::CardKind;

/// Card width from which the contexts flow into two columns.
const TWO_COLUMNS_FROM: f32 = 760.0;

pub struct HelpOverlay {
    first: Vec<KeyContext>,
    scroll: f32,
    /// Viewport height and maximum offset of the last frame (page step, clamping).
    viewport: f32,
    max_scroll: f32,
}

impl HelpOverlay {
    pub fn new() -> Self {
        Self::with_contexts(Vec::new())
    }
    /// Put the given contexts (current screen's stack) at the top.
    pub fn with_contexts(contexts: Vec<KeyContext>) -> Self {
        Self { first: contexts, scroll: 0.0, viewport: 0.0, max_scroll: f32::INFINITY }
    }

    /// Every context exactly once: the current ones first (innermost first), then the rest in
    /// documentation order, `Global` last.
    fn ordered(&self) -> Vec<KeyContext> {
        let mut v: Vec<KeyContext> = Vec::with_capacity(KeyContext::ALL.len());
        let rest = KeyContext::ALL.into_iter().filter(|c| *c != KeyContext::Global).chain([KeyContext::Global]);
        for c in self.first.iter().copied().chain(rest) {
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll = (self.scroll + dy).clamp(0.0, self.max_scroll.max(0.0));
    }
}

impl Default for HelpOverlay {
    fn default() -> Self {
        Self::new()
    }
}

/// Split contexts into `cols` reading-order columns of roughly equal row counts.
fn split_columns(contexts: &[(KeyContext, Vec<(String, &'static str)>)], cols: usize) -> Vec<Vec<usize>> {
    let total: usize = contexts.iter().map(|(_, r)| r.len() + 2).sum();
    let per = total.div_ceil(cols.max(1));
    let mut out = vec![Vec::new(); cols.max(1)];
    let (mut col, mut acc) = (0, 0);
    for (i, (_, rows)) in contexts.iter().enumerate() {
        let h = rows.len() + 2;
        // next column once this block would end up more past the column's share than not
        if acc > 0 && acc + h / 2 >= per && col + 1 < out.len() {
            col += 1;
            acc = 0;
        }
        out[col].push(i);
        acc += h;
    }
    out
}

fn context_block(ui: &mut egui::Ui, theme: &Theme, ctx: KeyContext, rows: &[(String, &'static str)], keys_w: f32) {
    let t = &theme.tokens;
    ui.add_space(theme.space(SpaceToken::Lg));
    crate::ui::widgets::section::section_header(ui, theme, ctx.title());
    egui::Grid::new(("help-grid", format!("{ctx:?}")))
        .num_columns(2)
        .min_col_width(keys_w)
        .spacing([theme.space(SpaceToken::Lg), theme.space(SpaceToken::Xs)])
        .show(ui, |ui| {
            for (keys, help) in rows {
                ui.label(RichText::new(keys).size(theme.font(TypeToken::Body)).color(t.accent));
                ui.add(egui::Label::new(RichText::new(*help).size(theme.font(TypeToken::Body)).color(t.fg)).wrap());
                ui.end_row();
            }
        });
}

impl Overlay for HelpOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Panel]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let line = ctx.theme.font(TypeToken::Body) * 3.0;
        let page = (self.viewport * 0.85).max(line);
        match action {
            Action::Back | Action::ToggleHelp | Action::Activate => return OverlayOutcome::Close,
            Action::Down => self.scroll_by(line),
            Action::Up => self.scroll_by(-line),
            Action::PageDown | Action::FocusNext => self.scroll_by(page),
            Action::PageUp | Action::FocusPrev => self.scroll_by(-page),
            Action::Home => self.scroll = 0.0,
            Action::End => self.scroll_by(f32::INFINITY),
            // the rest of the Panel table (h/l, x, reorder) does nothing here
            Action::Left | Action::Right | Action::MoveUp | Action::MoveDown | Action::ContextMenu => {}
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "help") {
            return OverlayOutcome::Close;
        }
        let screen = egui_ctx.content_rect();
        let contexts: Vec<_> = self.ordered().into_iter().map(|c| (c, keymap::help_rows(c))).collect();
        let r = card_area(egui_ctx, theme, "help", CardKind::Menu, 1040.0 * theme.rem(), None, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Keyboard").size(theme.font(TypeToken::Heading)).color(t.fg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("↑/↓ scroll · Esc close").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                });
            });
            ui.add_space(theme.space(SpaceToken::Md));
            let width = ui.available_width();
            let cols = if width >= TWO_COLUMNS_FROM { 2 } else { 1 };
            let keys_w = (150.0 * theme.rem()).min(width / cols as f32 * 0.4);
            egui::ScrollArea::vertical().max_height(screen.height() * 0.8).vertical_scroll_offset(self.scroll).auto_shrink([false, true]).show(
                ui,
                |ui| {
                    let split = split_columns(&contexts, cols);
                    ui.columns(cols, |columns| {
                        for (col, idx) in columns.iter_mut().zip(&split) {
                            for &i in idx {
                                context_block(col, theme, contexts[i].0, &contexts[i].1, keys_w);
                            }
                        }
                    });
                },
            )
        });
        let out = r.inner;
        self.viewport = out.inner_rect.height();
        self.max_scroll = (out.content_size.y - out.inner_rect.height()).max(0.0);
        // follow wheel scrolling too
        self.scroll = out.state.offset.y.min(self.max_scroll);
        OverlayOutcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_context_once_current_first_global_last() {
        let h = HelpOverlay::with_contexts(vec![KeyContext::PlayerOsd, KeyContext::PlayerLive, KeyContext::PlayerOsd]);
        let v = h.ordered();
        assert_eq!(v.len(), KeyContext::ALL.len());
        assert_eq!(&v[..2], &[KeyContext::PlayerOsd, KeyContext::PlayerLive]);
        assert_eq!(*v.last().unwrap(), KeyContext::Global);
        for c in KeyContext::ALL {
            assert!(v.contains(&c), "{c:?} missing");
        }
    }

    #[test]
    fn columns_keep_reading_order_and_balance() {
        let contexts: Vec<_> = KeyContext::ALL.into_iter().map(|c| (c, keymap::help_rows(c))).collect();
        let split = split_columns(&contexts, 2);
        assert_eq!(split.len(), 2);
        let flat: Vec<usize> = split.concat();
        assert_eq!(flat, (0..contexts.len()).collect::<Vec<_>>());
        let rows = |col: &Vec<usize>| col.iter().map(|&i| contexts[i].1.len() + 2).sum::<usize>();
        let (a, b) = (rows(&split[0]), rows(&split[1]));
        assert!(a.abs_diff(b) <= 30, "{a} vs {b}");
        assert_eq!(split_columns(&contexts, 1)[0].len(), contexts.len());
    }
}
