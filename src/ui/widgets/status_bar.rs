//! Bottom status bar (bar height 26): left = background job summary ("Refreshing Provider ·
//! 45 %") or, when idle, the per-context hint line generated from the keymap
//! (`keymap::hint_line`, e.g. "Enter play · Shift+Enter fullscreen · Ctrl+F fav · ? help");
//! right = "Guide updated 2 h ago" + offline marker + (Compact, mini-player hidden) a
//! now-playing chip (click = player, ✕ = stop). No clock (the Omarchy bar has one). Hidden in
//! the fullscreen player and during onboarding. Owner: WP-10a.
//!
//! While the cursor or the pointer is on a card or a row that shows a playlist code chip, the
//! left side starts with what that item is, with the playlist's full name ("Northwind · 2019 ·
//! ★ 7.9", DESIGN §13.0 "Playlist codes"): screens [`note_focus`] it while they draw, the bar
//! shows it on the next frame.
//!
//! The right side is laid out first; the left text gets the remaining width and is elided, so
//! nothing overlaps at 360 pt.

use eframe::egui;

use super::glyphs;
use crate::jobs::JobStatus;
use crate::keymap::{self, Action};
use crate::model::Timestamp;
use crate::store::sources::{GuideHealth, GuideNote};
use crate::theme::{SpaceToken, Theme, TypeToken};

/// Guide data older than this is flagged (warning glyph + refresh hint).
pub(crate) const GUIDE_STALE_S: i64 = 2 * 86_400;

pub struct StatusInfo<'a> {
    pub jobs: &'a [JobStatus],
    pub epg_updated: Option<crate::model::Timestamp>,
    /// A failed guide update / a much older guide, said after "Guide updated …".
    pub guide_note: Option<String>,
    pub offline: bool,
    pub now: crate::model::Timestamp,
    /// Keymap hint line for the current context stack.
    pub hint: &'a str,
    /// Title of what plays, shown as a chip when the mini-player is hidden (Compact).
    pub now_playing: Option<&'a str>,
    /// The focused / hovered item with its playlist's full name ([`take_focus`]).
    pub focus: Option<&'a str>,
    /// The batch features' chips, right of the now-playing chip (the spoiler guard's
    /// "Spoilers shown · 14 min ✕"; base-B slot).
    pub chips: &'a [StatusChip],
}

/// A feature's status chip: clicking it runs `action` as a global key; its ✕ (when it has
/// one) runs `close` right away (the spoiler guard's "guard again").
#[derive(Debug, Clone, PartialEq)]
pub struct StatusChip {
    pub text: String,
    /// The accessible name.
    pub label: String,
    pub action: Action,
    pub warning: bool,
    pub close: Option<ChipClose>,
}

/// A chip's ✕: what it does (run on the UI thread when clicked) and its accessible name.
#[derive(Clone)]
pub struct ChipClose {
    pub label: String,
    run: std::sync::Arc<dyn Fn() + Send + Sync>,
}

impl ChipClose {
    pub fn new(label: impl Into<String>, run: impl Fn() + Send + Sync + 'static) -> Self {
        Self { label: label.into(), run: std::sync::Arc::new(run) }
    }
    pub fn run(&self) {
        (self.run)()
    }
}

impl std::fmt::Debug for ChipClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChipClose({})", self.label)
    }
}

impl PartialEq for ChipClose {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label && std::sync::Arc::ptr_eq(&self.run, &other.run)
    }
}

/// The focus line noted this frame: (text, from the pointer). A hovered item beats the
/// keyboard cursor's.
#[derive(Debug, Clone, Default)]
struct FocusSlot {
    pending: Option<(String, bool)>,
    shown: Option<String>,
}

fn focus_id() -> egui::Id {
    egui::Id::new("status-bar-focus")
}

/// A card or row under the cursor (`hovered`: under the pointer) says what it is, with its
/// playlist's full name, for the status bar's next frame.
pub fn note_focus(ctx: &egui::Context, text: String, hovered: bool) {
    ctx.data_mut(|d| {
        let slot = d.get_temp_mut_or_default::<FocusSlot>(focus_id());
        if !slot.pending.as_ref().is_some_and(|(_, h)| *h && !hovered) {
            slot.pending = Some((text, hovered));
        }
    });
}

/// The focus line of the last frame (the app, before it draws the bar); starts a new frame's.
pub fn take_focus(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| {
        let slot = d.get_temp_mut_or_default::<FocusSlot>(focus_id());
        slot.shown = slot.pending.take().map(|p| p.0);
        slot.shown.clone()
    })
}

/// End of the frame (the app): the line changed since the bar drew it — draw the bar again.
pub fn settle_focus(ctx: &egui::Context) {
    let changed = ctx.data(|d| d.get_temp::<FocusSlot>(focus_id()).is_some_and(|s| s.pending.as_ref().map(|p| &p.0) != s.shown.as_ref()));
    if changed {
        ctx.request_repaint();
    }
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
    /// A feature's chip (its action).
    Chip(Action),
}

/// "Refreshing My IPTV · 45 % (+1)" for the first job of `jobs`.
fn jobs_summary(jobs: &[JobStatus]) -> Option<String> {
    let j = jobs.first()?;
    let pct = j.fraction.filter(|f| f.is_finite()).map(|f| format!(" · {:.0} %", f.clamp(0.0, 1.0) * 100.0)).unwrap_or_default();
    let more = if jobs.len() > 1 { format!(" (+{})", jobs.len() - 1) } else { String::new() };
    Some(format!("{} {}{pct}{more}", glyphs::REFRESH, j.label))
}

/// "update failed, retrying in 10 min" / "TV guide 5 d old" for the guides' health, `None` when
/// all is well. A failing guide that is also the old one gets one note: "TV guide 5 d old,
/// update failed, retrying in 6 h". Short, the status bar truncates.
pub(crate) fn guide_note(now: Timestamp, h: &GuideHealth) -> Option<String> {
    let retry = |n: &GuideNote| {
        let wait = n.retry_at.map_or(0, |t| t - now);
        match wait {
            ..=59 => "retrying soon".to_owned(),
            60..=3_599 => format!("retrying in {} min", wait / 60),
            3_600..=172_799 => format!("retrying in {} h", wait / 3_600),
            _ => format!("retrying in {} d", wait / 86_400),
        }
    };
    let age = |t: Timestamp| match (now - t).max(0) {
        a @ 0..=3_599 => format!("{} min", a / 60),
        a @ 3_600..=172_799 => format!("{} h", a / 3_600),
        a => format!("{} d", a / 86_400),
    };
    let failed = |n: &GuideNote| format!("update failed, {}", retry(n));
    match (&h.old, &h.failing) {
        (Some(o), Some(f)) if o.name == f.name => Some(format!("{} guide {} old, {}", o.name, age(o.last_refresh?), failed(f))),
        (Some(o), f) => {
            let old = format!("{} guide {} old", o.name, age(o.last_refresh?));
            Some(f.as_ref().map_or(old.clone(), |f| format!("{old} · {} {}", f.name, failed(f))))
        }
        (None, Some(f)) if h.several => Some(format!("{} {}", f.name, failed(f))),
        (None, Some(f)) => Some(failed(f)),
        (None, None) => None,
    }
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
        for c in info.chips {
            // right to left: the ✕ first, close to its chip
            if let Some(close) = &c.close {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
                if chip(ui, glyphs::CLOSE, t.text_secondary, size, &close.label).on_hover_text(&close.label).clicked() {
                    close.run();
                    ui.ctx().request_repaint();
                }
            }
            if chip(ui, c.text.clone(), if c.warning { t.warning } else { t.accent }, size, &c.label).clicked() {
                click = StatusClick::Chip(c.action);
            }
            ui.spacing_mut().item_spacing.x = gap;
        }
        if !info.chips.is_empty() {
            // a chip's text counts down ("14 min") and goes when its state ends
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(20));
        }
        if info.offline {
            ui.label(egui::RichText::new(format!("{} Offline — showing saved data", glyphs::OFFLINE)).size(size).color(t.warning));
        }
        if info.epg_updated.is_none()
            && let Some(note) = &info.guide_note
        {
            // nothing imported yet, and the first update failed
            ui.label(egui::RichText::new(format!("Guide {note}")).size(size).color(t.warning));
        }
        if let Some(at) = info.epg_updated {
            let (mut text, stale) = guide_freshness(info.now, at);
            if let Some(note) = &info.guide_note {
                text = format!("{text} · {note}");
            }
            if stale {
                let key = keymap::GLOBAL.iter().find(|b| b.action == Action::RefreshSource).map(|b| b.chord.label()).unwrap_or_default();
                ui.label(egui::RichText::new(format!("{} {text} · {key} refresh", glyphs::WARNING)).size(size).color(t.warning));
            } else {
                let color = if info.guide_note.is_some() { t.warning } else { t.text_secondary };
                ui.label(egui::RichText::new(text).size(size).color(color));
            }
        }
        // the left side takes whatever is left
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let jobs = jobs_summary(info.jobs);
            match (info.focus, jobs.is_some()) {
                // the item, then the hints in one line (elided together)
                (Some(f), false) => {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(f, 0.0, egui::TextFormat::simple(egui::FontId::proportional(size), t.fg));
                    job.append(info.hint, gap, egui::TextFormat::simple(egui::FontId::proportional(size), t.text_secondary));
                    ui.add(egui::Label::new(job).truncate());
                }
                (Some(f), true) => {
                    ui.scope(|ui| {
                        ui.set_max_width((ui.available_width() * 0.5).max(80.0));
                        ui.add(egui::Label::new(egui::RichText::new(f).size(size).color(t.fg)).truncate());
                    });
                }
                (None, _) => {}
            }
            match jobs {
                Some(text) => {
                    if chip(ui, text, t.text_secondary, size, "Background tasks").clicked() {
                        click = StatusClick::Jobs;
                    }
                }
                None if info.focus.is_none() => {
                    ui.add(egui::Label::new(egui::RichText::new(info.hint).size(size).color(t.text_secondary)).truncate());
                }
                None => {}
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

    fn note(name: &str, last_refresh: Option<Timestamp>, retry_at: Option<Timestamp>) -> GuideNote {
        GuideNote { name: name.into(), last_refresh, retry_at }
    }

    #[test]
    fn guide_notes_say_what_failed_and_which_guide_is_old() {
        let now = 1_000_000;
        let health = |several, failing, old| GuideHealth { newest: Some(now - 3_600), several, failing, old };
        assert_eq!(guide_note(now, &health(false, None, None)), None);
        // the report: one guide, its launch refresh failed
        let f = note("TV 2", Some(now - 23 * 3_600), Some(now + 600));
        assert_eq!(guide_note(now, &health(false, Some(f.clone()), None)).as_deref(), Some("update failed, retrying in 10 min"));
        assert_eq!(guide_note(now, &health(true, Some(f), None)).as_deref(), Some("TV 2 update failed, retrying in 10 min"));
        // due already: the scheduler ticks every 15 min
        let due = note("TV", Some(now - 100), Some(now - 5));
        assert_eq!(guide_note(now, &health(false, Some(due), None)).as_deref(), Some("update failed, retrying soon"));
        // an old guide, failing too: one note
        let old = note("TV", Some(now - 5 * 86_400), Some(now + 6 * 3_600 + 60));
        assert_eq!(
            guide_note(now, &health(true, Some(old.clone()), Some(old.clone()))).as_deref(),
            Some("TV guide 5 d old, update failed, retrying in 6 h")
        );
        // old but not failing; another failing
        let other = note("Main", Some(now - 3_600), Some(now + 1_200));
        assert_eq!(guide_note(now, &health(true, None, Some(note("TV", old.last_refresh, None)))).as_deref(), Some("TV guide 5 d old"));
        assert_eq!(
            guide_note(now, &health(true, Some(other), Some(note("TV", old.last_refresh, None)))).as_deref(),
            Some("TV guide 5 d old · Main update failed, retrying in 20 min")
        );
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
