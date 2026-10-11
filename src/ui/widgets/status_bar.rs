//! Bottom status bar (bar height 26): left = background job summary ("Refreshing Provider ·
//! 45 %") or, when idle, the per-context hint line generated from the keymap
//! (`keymap::hint_line`, e.g. "Enter play · Shift+Enter fullscreen · Ctrl+F fav · ? help");
//! right = "Guide updated 2 h ago" + offline marker + (Compact, mini-player hidden) a
//! now-playing chip (click = player, ✕ = stop). No clock (the Omarchy bar has one). Hidden in
//! the fullscreen player and during onboarding. Owner: WP-10a.
//!
//! The right side is laid out first; the left text gets the remaining width and is elided, so
//! nothing overlaps at 360 pt.

use eframe::egui;

use super::glyphs;
use crate::jobs::JobStatus;
use crate::keymap::{self, Action};
use crate::model::Timestamp;
use crate::theme::{SpaceToken, Theme, TypeToken};

/// Guide data older than this is flagged (warning glyph + refresh hint).
const GUIDE_STALE_S: i64 = 2 * 86_400;

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

/// "Refreshing My IPTV · 45 % (+1)" for the first job of `jobs`.
fn jobs_summary(jobs: &[JobStatus]) -> Option<String> {
    let j = jobs.first()?;
    let pct = j.fraction.filter(|f| f.is_finite()).map(|f| format!(" · {:.0} %", f.clamp(0.0, 1.0) * 100.0)).unwrap_or_default();
    let more = if jobs.len() > 1 { format!(" (+{})", jobs.len() - 1) } else { String::new() };
    Some(format!("{} {}{pct}{more}", glyphs::REFRESH, j.label))
}

/// "Guide updated 2 h ago" (+ stale flag) for the last EPG import at `at`.
fn guide_freshness(now: Timestamp, at: Timestamp) -> (String, bool) {
    let age = (now - at).max(0);
    let ago = match age {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} min ago", age / 60),
        3_600..=172_799 => format!("{} h ago", age / 3_600),
        _ => format!("{} d ago", age / 86_400),
    };
    (format!("Guide updated {ago}"), age >= GUIDE_STALE_S)
}

/// Clickable status text (sized to its text, elided at `max_w`).
fn chip(ui: &mut egui::Ui, text: impl Into<String>, color: egui::Color32, size: f32, label: &str) -> egui::Response {
    let resp = ui
        .add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).sense(egui::Sense::click()).truncate())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    resp
}

pub fn status_bar(ui: &mut egui::Ui, theme: &Theme, info: &StatusInfo<'_>) -> StatusClick {
    let t = &theme.tokens;
    let size = theme.font(TypeToken::BodySmall);
    let gap = theme.space(SpaceToken::Xl);
    let mut click = StatusClick::None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = gap;
        if let Some(title) = info.now_playing {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
            if chip(ui, glyphs::CLOSE, t.text_secondary, size, "Stop playback").clicked() {
                click = StatusClick::StopPlayback;
            }
            let max_w = (ui.available_width() * 0.4).max(80.0);
            ui.scope(|ui| {
                ui.set_max_width(max_w);
                if chip(ui, format!("{} {title}", glyphs::PLAY), t.accent, size, &format!("Now playing: {title}")).clicked() {
                    click = StatusClick::OpenPlayer;
                }
            });
            ui.spacing_mut().item_spacing.x = gap;
        }
        if info.offline {
            ui.label(egui::RichText::new(format!("{} Offline — showing saved data", glyphs::OFFLINE)).size(size).color(t.warning));
        }
        if let Some(at) = info.epg_updated {
            let (text, stale) = guide_freshness(info.now, at);
            if stale {
                let key = keymap::GLOBAL.iter().find(|b| b.action == Action::RefreshSource).map(|b| b.chord.label()).unwrap_or_default();
                ui.label(egui::RichText::new(format!("{} {text} · {key} refresh", glyphs::WARNING)).size(size).color(t.warning));
            } else {
                ui.label(egui::RichText::new(text).size(size).color(t.text_secondary));
            }
        }
        // the left side takes whatever is left
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| match jobs_summary(info.jobs) {
            Some(text) => {
                if chip(ui, text, t.text_secondary, size, "Background tasks").clicked() {
                    click = StatusClick::Jobs;
                }
            }
            None => {
                ui.add(egui::Label::new(egui::RichText::new(info.hint).size(size).color(t.text_secondary)).truncate());
            }
        });
    });
    click
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_copy_and_staleness() {
        assert_eq!(guide_freshness(1000, 990), ("Guide updated just now".into(), false));
        assert_eq!(guide_freshness(10_000, 10_000 - 600), ("Guide updated 10 min ago".into(), false));
        assert_eq!(guide_freshness(100_000, 100_000 - 7_200), ("Guide updated 2 h ago".into(), false));
        assert_eq!(guide_freshness(1_000_000, 1_000_000 - 3 * 86_400), ("Guide updated 3 d ago".into(), true));
        // clock skew never shows negative ages
        assert_eq!(guide_freshness(0, 50), ("Guide updated just now".into(), false));
    }

    #[test]
    fn job_summary_counts_and_percent() {
        use crate::jobs::{JobKind, JobPhase};
        let job = |label: &str, fraction| JobStatus {
            id: crate::model::JobId(1),
            kind: JobKind::RefreshStale,
            label: label.into(),
            phase: JobPhase::Connecting,
            started_at: 0,
            fraction,
        };
        assert_eq!(jobs_summary(&[]), None);
        let one = jobs_summary(&[job("Refreshing My IPTV", Some(0.454))]).unwrap();
        assert!(one.ends_with("Refreshing My IPTV · 45 %"), "{one}");
        let two = jobs_summary(&[job("A", None), job("B", None)]).unwrap();
        assert!(two.ends_with("A (+1)"), "{two}");
    }
}
