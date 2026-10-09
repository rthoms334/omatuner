//! Cards: menu card (2 px fg border), popup card (2 px accent border), tooltip (1 px).
//! Owner: WP-10a.

use eframe::egui;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Menu,
    Popup,
    Tooltip,
    /// Pane surface inside a screen (no border, raised bg).
    Pane,
}

pub fn card<R>(ui: &mut egui::Ui, theme: &Theme, kind: CardKind, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> { todo!("WP-10a") }

/// The egui Frame for a card kind (for `Area`s / overlays).
pub fn card_frame(theme: &Theme, kind: CardKind) -> egui::Frame { todo!("WP-10a") }
