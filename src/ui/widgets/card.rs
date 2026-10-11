//! Cards: menu card (2 px fg border), popup card (2 px accent border), tooltip (1 px).
//! Owner: WP-10a.

use eframe::egui;

use super::paint;
use crate::theme::{SpaceToken, Theme, with_alpha};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Menu,
    Popup,
    Tooltip,
    /// Pane surface inside a screen (no border, raised bg).
    Pane,
}

pub fn card<R>(ui: &mut egui::Ui, theme: &Theme, kind: CardKind, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    card_frame(theme, kind).show(ui, add)
}

/// The egui Frame for a card kind (for `Area`s / overlays).
pub fn card_frame(theme: &Theme, kind: CardKind) -> egui::Frame {
    let t = &theme.tokens;
    let frame = egui::Frame::NONE.corner_radius(t.rounding);
    match kind {
        CardKind::Menu => {
            let pad = theme.space(SpaceToken::PanelPadding);
            frame.fill(t.card_bg).stroke(egui::Stroke::new(t.card_border_width, t.card_border)).inner_margin(paint::margin(pad, pad))
        }
        CardKind::Popup => {
            let pad = theme.space(SpaceToken::PopupPadding);
            frame.fill(t.card_bg).stroke(egui::Stroke::new(t.card_border_width, t.popup_border)).inner_margin(paint::margin(pad, pad))
        }
        CardKind::Tooltip => frame
            .fill(with_alpha(t.card_bg, 0.97))
            .stroke(egui::Stroke::new(1.0, t.fg))
            .inner_margin(paint::margin(theme.space(SpaceToken::ControlPaddingX), theme.space(SpaceToken::ControlPaddingY))),
        CardKind::Pane => {
            let pad = theme.space(SpaceToken::Xxl);
            frame.fill(t.raised_bg).inner_margin(paint::margin(pad, pad))
        }
    }
}
