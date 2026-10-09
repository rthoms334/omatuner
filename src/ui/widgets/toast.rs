//! Toast stack (bottom-right, above the status bar AND above the mini-player rect): popup card
//! with kind glyph, auto-expire, optional action button ("Undo"). Owner: WP-10a.
//! SKELETON: minimal working body (the app calls `show` every frame); WP-10a restyles it.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::keymap::Action;
use crate::msg::{Toast, ToastKind};
use crate::theme::{Theme, TypeToken};

#[derive(Default)]
pub struct Toasts {
    items: Vec<(Toast, Instant)>,
}

impl Toasts {
    pub fn push(&mut self, t: Toast) { self.items.push((t, Instant::now())); }

    /// Draw + expire. `avoid` = the mini-player rect (toasts stack above it); `bottom` = y of
    /// the status bar top. Returns an action if a toast button was pressed.
    pub fn show(&mut self, ctx: &egui::Context, theme: &Theme, avoid: Option<egui::Rect>, bottom: f32) -> Option<Action> {
        self.items.retain(|(t, at)| at.elapsed() < Duration::from_millis(t.ttl_ms as u64));
        if self.items.is_empty() {
            return None;
        }
        let tk = &theme.tokens;
        let pad = 12.0;
        let screen = ctx.content_rect();
        let base_y = avoid.map(|r| r.top()).unwrap_or(bottom).min(bottom) - pad;
        let mut out = None;
        egui::Area::new(egui::Id::new("toasts"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::RIGHT_BOTTOM)
            .fixed_pos(egui::pos2(screen.right() - pad, base_y))
            .show(ctx, |ui| {
                for (t, _) in self.items.iter().rev() {
                    let (glyph, color) = match t.kind {
                        ToastKind::Info => ("󰋽", tk.fg),
                        ToastKind::Success => ("󰄬", tk.success),
                        ToastKind::Warning => ("󰀦", tk.warning),
                        ToastKind::Error => ("󰅚", tk.urgent),
                    };
                    egui::Frame::NONE.fill(tk.card_bg).stroke(egui::Stroke::new(tk.card_border_width, tk.popup_border)).inner_margin(10.0).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(glyph).color(color).size(theme.font(TypeToken::Body)));
                            ui.label(egui::RichText::new(&t.text).color(tk.fg).size(theme.font(TypeToken::Body)));
                            if let Some((label, action)) = &t.action
                                && ui.button(label).clicked() {
                                    out = Some(*action);
                                }
                        });
                    });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(250));
        out
    }

    pub fn is_empty(&self) -> bool { self.items.is_empty() }
}
