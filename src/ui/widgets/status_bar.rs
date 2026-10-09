//! Bottom status bar (bar height 26): left = background job summary ("Refreshing Provider ·
//! 45 %") or, when idle, the per-context hint line generated from the keymap
//! (`keymap::hint_line`, e.g. "Enter play · Shift+Enter fullscreen · Ctrl+F fav · ? help");
//! right = "Guide updated 2 h ago" + offline marker + (Compact, mini-player hidden) a
//! now-playing chip (click = player, ✕ = stop). No clock (the Omarchy bar has one). Hidden in
//! the fullscreen player and during onboarding. Owner: WP-10a.
//! SKELETON: minimal working body so the app can call it every frame; WP-10a restyles it.

use eframe::egui;

use crate::jobs::JobStatus;
use crate::theme::{Theme, TypeToken};

pub struct StatusInfo<'a> {
    pub jobs: &'a [JobStatus],
    pub epg_updated: Option<crate::model::Timestamp>,
    pub offline: bool,
    pub now: crate::model::Timestamp,
    /// Keymap hint line for the current context stack.
    pub hint: &'a str,
    /// Title of what plays, shown as a chip when the mini-player is hidden (Compact).
    pub now_playing: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClick {
    None,
    /// Jobs area → Jobs overlay.
    Jobs,
    /// Now-playing chip → fullscreen player.
    OpenPlayer,
    /// ✕ on the chip.
    StopPlayback,
}

pub fn status_bar(ui: &mut egui::Ui, theme: &Theme, info: &StatusInfo<'_>) -> StatusClick {
    let t = &theme.tokens;
    let size = theme.font(TypeToken::BodySmall);
    let mut click = StatusClick::None;
    ui.horizontal_centered(|ui| {
        let left = match info.jobs.first() {
            Some(j) => {
                let pct = j.fraction.map(|f| format!(" · {:.0} %", f * 100.0)).unwrap_or_default();
                let more = if info.jobs.len() > 1 { format!(" (+{})", info.jobs.len() - 1) } else { String::new() };
                format!("󰑐 {}{pct}{more}", j.label)
            }
            None => info.hint.to_owned(),
        };
        let r = ui.add(egui::Label::new(egui::RichText::new(left).size(size).color(t.text_secondary)).sense(egui::Sense::click()));
        if r.clicked() && !info.jobs.is_empty() {
            click = StatusClick::Jobs;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(title) = info.now_playing {
                if ui.add(egui::Label::new(egui::RichText::new("✕").size(size).color(t.text_secondary)).sense(egui::Sense::click())).clicked() {
                    click = StatusClick::StopPlayback;
                }
                if ui.add(egui::Label::new(egui::RichText::new(format!("󰐊 {title}")).size(size).color(t.accent)).sense(egui::Sense::click())).clicked() {
                    click = StatusClick::OpenPlayer;
                }
            }
            if info.offline {
                ui.label(egui::RichText::new("󰖪 Offline — showing saved data").size(size).color(t.warning));
            }
            if let Some(at) = info.epg_updated {
                let h = (info.now - at).max(0) / 3600;
                ui.label(egui::RichText::new(format!("Guide updated {h} h ago")).size(size).color(t.text_secondary));
            }
        });
    });
    click
}
