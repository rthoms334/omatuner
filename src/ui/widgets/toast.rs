//! Toast stack (bottom-right, above the status bar AND above the mini-player rect): popup card
//! with kind glyph, auto-expire, optional action button ("Undo"). Owner: WP-10a.
//!
//! Newest toast at the bottom (next to the anchor), at most [`MAX_VISIBLE`] at once (older ones
//! wait their turn, and their timer only starts once shown); hovering a toast restarts its
//! timer; pressing its action dismisses it.

use std::time::{Duration, Instant};

use eframe::egui;

use super::button::{ButtonStyle, button};
use super::card::{CardKind, card_frame};
use super::{glyphs, paint};
use crate::keymap::Action;
use crate::msg::{Toast, ToastKind};
use crate::theme::{SpaceToken, Theme, TypeToken};

/// Toasts shown at once.
const MAX_VISIBLE: usize = 4;
/// Card width cap (× rem).
const MAX_WIDTH: f32 = 360.0;

#[derive(Default)]
pub struct Toasts {
    items: Vec<(Toast, Instant)>,
}

impl Toasts {
    pub fn push(&mut self, t: Toast) {
        self.items.push((t, Instant::now()));
    }

    /// Draw + expire. `avoid` = the mini-player rect (toasts stack above it); `bottom` = y of
    /// the status bar top. Returns an action if a toast button was pressed.
    pub fn show(&mut self, ctx: &egui::Context, theme: &Theme, avoid: Option<egui::Rect>, bottom: f32) -> Option<Action> {
        let now = Instant::now();
        // queued toasts don't age (their timer starts once they're shown); done before expiring
        // so one that moves up into the stack this frame starts with its full time
        let queued = self.items.len().saturating_sub(MAX_VISIBLE);
        for (_, at) in &mut self.items[..queued] {
            *at = now;
        }
        self.items.retain(|(t, at)| now.duration_since(*at) < ttl(t));
        if self.items.is_empty() {
            return None;
        }
        let first = self.items.len().saturating_sub(MAX_VISIBLE);
        let tk = &theme.tokens;
        let gap = theme.space(SpaceToken::PanelGap);
        let width = (MAX_WIDTH * theme.rem()).min(ctx.content_rect().width() - 2.0 * gap).max(120.0);
        let screen = ctx.content_rect();
        let anchor = stack_anchor(screen, width, gap, avoid, bottom);
        let mut out = None;
        let mut dismiss = None;
        let mut hovered = Vec::new();
        egui::Area::new(egui::Id::new("toasts")).order(egui::Order::Foreground).pivot(egui::Align2::RIGHT_BOTTOM).fixed_pos(anchor).show(ctx, |ui| {
            ui.set_max_width(width);
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Md);
            for (i, (t, _)) in self.items.iter().enumerate().skip(first) {
                let (glyph, color) = match t.kind {
                    ToastKind::Info => (glyphs::INFO, tk.fg),
                    ToastKind::Success => (glyphs::CHECK, tk.success),
                    ToastKind::Warning => (glyphs::WARNING, tk.warning),
                    ToastKind::Error => (glyphs::CLOSE, tk.urgent),
                };
                let card = card_frame(theme, CardKind::Popup).show(ui, |ui| {
                    ui.set_width(width - 2.0 * theme.space(SpaceToken::PopupPadding) - 2.0 * tk.card_border_width);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                        ui.label(egui::RichText::new(glyph).font(paint::font(theme, TypeToken::Icon)).color(color));
                        if let Some((label, action)) = &t.action {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if button(ui, theme, None, label, ButtonStyle { primary: true, ..Default::default() }).clicked() {
                                    out = Some(*action);
                                    dismiss = Some(i);
                                }
                                ui.add(egui::Label::new(egui::RichText::new(&t.text).color(tk.fg).size(theme.font(TypeToken::Body))).wrap());
                            });
                        } else {
                            ui.add(egui::Label::new(egui::RichText::new(&t.text).color(tk.fg).size(theme.font(TypeToken::Body))).wrap());
                        }
                    });
                });
                if card.response.contains_pointer() {
                    hovered.push(i);
                }
            }
        });
        for i in hovered {
            self.items[i].1 = now;
        }
        if let Some(i) = dismiss {
            self.items.remove(i);
        }
        // wake up for the next expiry (and keep hover restarts responsive)
        let next = self.items.iter().map(|(t, at)| ttl(t).saturating_sub(now.duration_since(*at))).min().unwrap_or_default();
        ctx.request_repaint_after(next.clamp(Duration::from_millis(50), Duration::from_millis(500)));
        out
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn ttl(t: &Toast) -> Duration {
    Duration::from_millis(t.ttl_ms as u64)
}

/// Bottom-right corner of the stack: `gap` inside the screen's right edge, above the status bar
/// top (`bottom`) and above `avoid` when it shares the stack's column (the mini player).
fn stack_anchor(screen: egui::Rect, width: f32, gap: f32, avoid: Option<egui::Rect>, bottom: f32) -> egui::Pos2 {
    let right = screen.right() - gap;
    let column = egui::Rangef::new(right - width, right);
    let mut y = bottom.min(screen.bottom());
    if let Some(r) = avoid.filter(|r| r.x_range().intersects(column) && r.top() < y) {
        y = r.top();
    }
    egui::pos2(right, y - gap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_avoids_the_mini_player_column_only() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0));
        let mini = egui::Rect::from_min_max(egui::pos2(1260.0, 760.0), egui::pos2(1590.0, 960.0));
        assert_eq!(stack_anchor(screen, 360.0, 14.0, Some(mini), 974.0), egui::pos2(1586.0, 746.0));
        assert_eq!(stack_anchor(screen, 360.0, 14.0, None, 974.0), egui::pos2(1586.0, 960.0));
        // a rect on the left doesn't push the stack up
        let left = egui::Rect::from_min_max(egui::pos2(0.0, 700.0), egui::pos2(300.0, 960.0));
        assert_eq!(stack_anchor(screen, 360.0, 14.0, Some(left), 974.0), egui::pos2(1586.0, 960.0));
    }
}
