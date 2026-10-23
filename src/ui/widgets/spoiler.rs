//! What the painters show of programme and episode text under the spoiler guard (DESIGN §34
//! §2.2, B4). OWNER: S-ui.
//!
//! Two levels:
//! * [`Spoilers::text`]: the text a painter shows, whatever it paints with (a `Label`, a row's
//!   galley, a tooltip). A rewritten title gets the dim glyph ("Arsenal v Chelsea 󰈉"), a hidden
//!   guide sub-title becomes a fixed-width bar ("▆▆▆▆▆▆▆▆ 󰈉": never the text's length), a
//!   hidden description or plot says why and how to see it ("󰈉 Description hidden — mentions
//!   Arsenal · until you watch it or Mon 19:30 · Ctrl+B to show"). An on-demand episode's title
//!   always shows (`guard::Field::EpisodeTitle`). Every base-B call site goes through it (the
//!   programme popover, the guide's tooltip, Live's details pane, the series and movie pages'
//!   plots, rows and labels).
//! * [`field`]: a painter of one field with the full treatment (Omarchy tokens): the bar as a
//!   rounded `separator` rect, the description block (`fill_field`, 1 px `card_border`), the
//!   revealed text with the 2 px `accent` rule and 󰈈 — a click on a bar or block reveals the
//!   item, a click on 󰈈 hides it; hover says why. [`obscured_image`] for stills and icons
//!   (`ImageKind::Obscured`: 12 px wide, stretched, scrim and 󰈉).
//!
//! Accessible names never carry hidden text ("Hidden by spoiler guard (Arsenal)").

use std::borrow::Cow;

use eframe::egui;

use crate::guard::{Field, Guard, Guarded, Policy, Reason, ReasonKind, Subject};
use crate::model::{Programme, Timestamp};
use crate::spoiler::ui::{RevealKey, RevealSet};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::glyphs;

/// 󰈉: something is hidden here.
pub const HIDDEN: &str = glyphs::HIDDEN;
/// 󰈈: hidden text shown on purpose (Ctrl+B, the pause).
pub const SHOWN: &str = "\u{f0208}";

/// The guard as a frame's painters see it (cheap: an `Arc` clone and the few reveals).
#[derive(Clone, Default)]
pub struct Spoilers {
    /// `None`: the feature is off — everything shows as is.
    guard: Option<Guard>,
    now: Timestamp,
    h24: bool,
    reveals: RevealSet,
}

impl std::fmt::Debug for Spoilers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spoilers").field("guard", &self.guard).field("now", &self.now).finish_non_exhaustive()
    }
}

impl Spoilers {
    /// This frame's view.
    pub fn of(ctx: &UiCtx<'_>) -> Self {
        if !crate::spoiler::ENABLED {
            return Self { now: ctx.now, ..Default::default() };
        }
        Self::with_guard(ctx.services.agent.guard(), ctx.now, ctx.config.interface.clock_24h)
    }

    /// A view over `guard` (tests, painters without a `UiCtx`).
    pub fn with_guard(guard: Guard, now: Timestamp, h24: bool) -> Self {
        let guard = guard.is_live().then_some(guard);
        Self { guard, now, h24, reveals: crate::spoiler::ui::reveals() }
    }

    /// What a painter caching guarded text compares to know it must load again: the guard's
    /// generation (a new snapshot, a pause) and the reveals'.
    pub fn stamp(&self) -> (u64, u64) {
        (self.guard.as_ref().map_or(0, Guard::generation), crate::spoiler::ui::reveal_generation())
    }

    /// Shown on purpose (Ctrl+B or a click on its bar).
    pub fn is_revealed(&self, subject: &Subject<'_>) -> bool {
        self.reveals.covers(subject)
    }

    /// What the guard says of `field` of `subject` (a revealed item: as is).
    pub fn guarded<'a>(&self, subject: &Subject<'_>, field: Field, text: &'a str) -> Guarded<'a> {
        let policy = if self.is_revealed(subject) { Policy::Reveal } else { Policy::Normal };
        self.guarded_with(subject, field, text, policy)
    }

    /// The guard hides or rewrites `field` of `subject` (whether or not it is shown on
    /// purpose now).
    pub fn would_guard(&self, subject: &Subject<'_>, field: Field, text: &str) -> bool {
        !text.is_empty() && !matches!(self.guarded_with(subject, field, text, Policy::Normal), Guarded::Shown(_))
    }

    fn guarded_with<'a>(&self, subject: &Subject<'_>, field: Field, text: &'a str, policy: Policy) -> Guarded<'a> {
        match &self.guard {
            Some(g) => g.text(subject, field, text, policy, self.now),
            None => Guarded::Shown(Cow::Borrowed(text)),
        }
    }

    /// The text to paint for `field`: as is, rewritten with the dim glyph, or what stands in
    /// for hidden text (a fixed-width bar, "Description hidden — …"). `None` only for empty
    /// input.
    pub fn text<'a>(&self, subject: &Subject<'_>, field: Field, text: &'a str) -> Option<Cow<'a, str>> {
        if text.is_empty() {
            return None;
        }
        Some(display(self.guarded(subject, field, text), field, self.now, self.h24))
    }

    /// Whether an image of `subject` (a still, a programme icon) is shown obscured.
    pub fn obscures_image(&self, subject: &Subject<'_>) -> bool {
        matches!(self.guarded(subject, Field::Image, ""), Guarded::Hidden { .. })
    }
}

/// The subject of a full guide programme.
pub fn programme_subject(p: &Programme) -> Subject<'_> {
    Subject::Programme {
        title: &p.title,
        sub_title: p.sub_title.as_deref(),
        categories: &p.categories,
        season_episode: crate::spoiler::programme_se(p),
        start: p.start,
        is_new: p.is_new,
    }
}

/// The subject of a grid programme (no categories in the slim row; its S/E and "new" flag
/// are there).
pub fn slim_subject(p: &crate::model::ProgrammeSlim) -> Subject<'_> {
    Subject::Programme {
        title: &p.title,
        sub_title: p.sub_title.as_deref(),
        categories: &[],
        season_episode: p.episode.and_then(crate::spoiler::episode_se),
        start: p.start,
        is_new: p.flags.is_new,
    }
}

impl Spoilers {
    /// A guide programme's title as a row, cell or search hit shows it ("Arsenal v Chelsea 󰈉").
    pub fn programme_title<'p>(&self, p: &'p crate::model::ProgrammeSlim) -> Cow<'p, str> {
        if self.guard.is_none() {
            return Cow::Borrowed(&p.title);
        }
        self.text(&slim_subject(p), Field::Title, &p.title).unwrap_or(Cow::Borrowed(&p.title))
    }
}

/// Guide titles as the guard shows them, judged once per programme per minute (activity moves
/// with time) and per guard change (`Spoilers::stamp`): the grid paints hundreds of cells a
/// frame.
#[derive(Default)]
pub struct TitleCache {
    key: ((u64, u64), Timestamp),
    /// `None`: shown as is.
    titles: std::collections::HashMap<crate::model::ProgrammeId, Option<std::sync::Arc<str>>>,
}

impl TitleCache {
    const MAX: usize = 4000;

    /// `p`'s title under `sp`.
    pub fn title<'p>(&mut self, sp: &Spoilers, p: &'p crate::model::ProgrammeSlim) -> Cow<'p, str> {
        if sp.guard.is_none() {
            return Cow::Borrowed(&p.title);
        }
        let key = (sp.stamp(), sp.now.div_euclid(60));
        if key != self.key || self.titles.len() >= Self::MAX {
            self.titles.clear();
            self.key = key;
        }
        let shown = self.titles.entry(p.id).or_insert_with(|| match sp.programme_title(p) {
            Cow::Owned(s) => Some(s.into()),
            Cow::Borrowed(_) => None,
        });
        match shown {
            Some(s) => Cow::Owned(s.to_string()),
            None => Cow::Borrowed(&p.title),
        }
    }
}

// ------------------------------------------------------------------ text forms

/// A bar's length in blocks: fixed, so its width never tells the text's.
const BAR_BLOCKS: usize = 8;

/// "▆▆▆▆▆▆▆▆ 󰈉".
fn bar_text() -> String {
    format!("{} {HIDDEN}", "▆".repeat(BAR_BLOCKS))
}

/// What a field shows as text.
pub fn display<'a>(g: Guarded<'a>, field: Field, now: Timestamp, h24: bool) -> Cow<'a, str> {
    match g {
        Guarded::Shown(t) => t,
        Guarded::Rewritten(t) => Cow::Owned(format!("{t} {HIDDEN}")),
        Guarded::Hidden { reason } => Cow::Owned(match field {
            Field::Title => match &reason {
                Some(r) if !starting(r) => format!("{} · title hidden {HIDDEN}", r.rule),
                _ => format!("Title hidden {HIDDEN}"),
            },
            Field::SubTitle | Field::EpisodeTitle | Field::Image => bar_text(),
            Field::Description | Field::Plot => hidden_line(field, reason.as_ref(), now, h24, &crate::spoiler::ui::reveal_key_label()),
        }),
    }
}

/// The fail-closed placeholder's reason (before the first snapshot).
fn starting(r: &Reason) -> bool {
    r.rule == "Spoiler guard"
}

/// Why it is hidden and until when ("mentions Arsenal · until you watch it or Mon 19:30").
pub fn why(reason: Option<&Reason>, now: Timestamp, h24: bool) -> String {
    let Some(r) = reason.filter(|r| !starting(r)) else { return "spoiler guard".into() };
    let when = |t: Timestamp| if t - now < 20 * 3600 { crate::ui::widgets::time::hhmm(t, h24) } else { crate::ui::widgets::time::day_time(t, h24) };
    match r.kind {
        ReasonKind::Embargo => match r.until {
            Some(t) => format!("mentions {} · until you watch it or {}", r.rule, when(t)),
            None => format!("mentions {} · until you watch it", r.rule),
        },
        ReasonKind::Blackout => match r.until {
            Some(t) => format!("you'll watch {} later · until {}", r.rule, when(t)),
            None => format!("you'll watch {} later", r.rule),
        },
        ReasonKind::Series => format!("{} · after the episode you're on", r.rule),
        ReasonKind::SpoilerSafe => "spoiler-safe".into(),
    }
}

/// "󰈉 Description hidden — mentions Arsenal · until … · Ctrl+B to show".
fn hidden_line(field: Field, reason: Option<&Reason>, now: Timestamp, h24: bool, key: &str) -> String {
    let what = if field == Field::Plot { "Plot hidden" } else { "Description hidden" };
    let show = if key.is_empty() { String::new() } else { format!(" · {key} to show") };
    format!("{HIDDEN} {what} — {}{show}", why(reason, now, h24))
}

/// The accessible name of hidden text: never the text.
pub fn accessible_name(reason: Option<&Reason>) -> String {
    match reason.filter(|r| !starting(r)) {
        Some(r) => format!("Hidden by spoiler guard ({})", r.rule),
        None => "Hidden by spoiler guard".into(),
    }
}

// ------------------------------------------------------------------ painters

/// What a click on a painted field asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldClick {
    #[default]
    None,
    /// A bar or block: the item is shown now (the row under it must not act on this click).
    Revealed,
    /// 󰈈: hidden again.
    Hidden,
}

/// Paint `field` of `subject` with the full treatment at `size` in `color` (wrapping in the
/// available width). The click on a bar or block reveals the item ([`FieldClick::Revealed`]):
/// the caller's row ignores that click.
pub fn field(
    ui: &mut egui::Ui,
    theme: &Theme,
    sp: &Spoilers,
    subject: &Subject<'_>,
    field: Field,
    text: &str,
    size: f32,
    color: egui::Color32,
) -> FieldClick {
    let t = &theme.tokens;
    if text.is_empty() {
        return FieldClick::None;
    }
    let normal = sp.guarded_with(subject, field, text, Policy::Normal);
    let revealed = sp.is_revealed(subject) && !matches!(normal, Guarded::Shown(_));
    if revealed {
        // the real text, marked as peeked: a 2 px accent rule on its left and 󰈈 (click: hide)
        let mut click = FieldClick::None;
        let r = ui.horizontal(|ui| {
            ui.add_space(theme.space(SpaceToken::Sm));
            ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).wrap());
            let eye = ui
                .add(egui::Label::new(egui::RichText::new(SHOWN).size(size).color(t.accent)).sense(egui::Sense::click()))
                .on_hover_text(format!("Shown on purpose — click or {} to hide", crate::spoiler::ui::reveal_key_label()))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            eye.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Hide spoilers again"));
            if super::fresh_click(&eye) {
                crate::spoiler::ui::hide_all();
                click = FieldClick::Hidden;
            }
        });
        let rr = r.response.rect;
        ui.painter().line_segment([rr.left_top(), rr.left_bottom()], egui::Stroke::new(2.0, t.accent));
        return click;
    }
    match normal {
        Guarded::Shown(s) => {
            ui.add(egui::Label::new(egui::RichText::new(s.as_ref()).size(size).color(color)).wrap());
            FieldClick::None
        }
        Guarded::Rewritten(s) => {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(s.as_ref()).size(size).color(color)).wrap());
                ui.label(egui::RichText::new(HIDDEN).size(size).color(t.text_disabled)).on_hover_text("Score hidden by the spoiler guard");
            });
            FieldClick::None
        }
        Guarded::Hidden { reason } => {
            let tip = format!("Hidden by the spoiler guard: {}", why(reason.as_ref(), sp.now, sp.h24));
            let resp = match field {
                Field::Description | Field::Plot => hidden_block(ui, theme, field, reason.as_ref(), sp.now, sp.h24),
                Field::Title => {
                    let label = display(Guarded::Hidden { reason: reason.clone() }, field, sp.now, sp.h24);
                    ui.add(egui::Label::new(egui::RichText::new(label.as_ref()).size(size).color(t.text_secondary)).sense(egui::Sense::click()))
                }
                _ => bar(ui, theme, size),
            };
            let resp = resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, accessible_name(reason.as_ref())));
            if super::fresh_click(&resp)
                && let Some(k) = RevealKey::of(subject)
            {
                crate::spoiler::ui::reveal_key(k);
                return FieldClick::Revealed;
            }
            FieldClick::None
        }
    }
}

/// The redaction bar: a rounded `separator` rect one glyph high and of a fixed width, then the
/// dim 󰈉. Clickable.
pub fn bar(ui: &mut egui::Ui, theme: &Theme, size: f32) -> egui::Response {
    let t = &theme.tokens;
    let w = (BAR_BLOCKS as f32 * size * 0.6).round();
    let gap = theme.space(SpaceToken::Sm);
    let font = egui::FontId::proportional(size);
    let glyph = ui.painter().layout_no_wrap(HIDDEN.to_owned(), font, t.text_disabled);
    let h = (size * 1.3).round();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w + gap + glyph.size().x, h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bar = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - size * 0.4), egui::vec2(w, (size * 0.8).round()));
        let fill = if resp.hovered() { t.fill_hover } else { t.separator };
        ui.painter().rect_filled(bar, t.rounding, fill);
        ui.painter().galley(egui::pos2(bar.right() + gap, rect.center().y - glyph.size().y / 2.0), glyph, t.text_disabled);
    }
    resp
}

/// The "Description hidden" block: `fill_field` with a 1 px `card_border` outline, saying why
/// and how to see it. Clickable.
pub fn hidden_block(ui: &mut egui::Ui, theme: &Theme, field: Field, reason: Option<&Reason>, now: Timestamp, h24: bool) -> egui::Response {
    let t = &theme.tokens;
    let key = crate::spoiler::ui::reveal_key_label();
    let text = hidden_line(field, reason, now, h24, &key).replacen(" to show", " or click to show", 1);
    let frame = egui::Frame::new()
        .fill(t.fill_field)
        .stroke(egui::Stroke::new(1.0, t.card_border))
        .corner_radius(t.rounding)
        .inner_margin(egui::Margin::same(theme.space(SpaceToken::Md) as i8));
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(egui::RichText::new(text).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
    });
    ui.interact(r.response.rect, r.response.id.with("reveal"), egui::Sense::click())
}

/// An image under the guard: the `ImageKind::Obscured` texture (12 px wide) stretched over
/// `rect` with linear filtering, the `scrim` and a centred 󰈉. Clickable (reveal).
pub fn obscured_image(ui: &mut egui::Ui, theme: &Theme, images: &mut crate::images::ImageCache, url: &str, size: egui::Vec2) -> egui::Response {
    let t = &theme.tokens;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter_at(rect);
    match images.get(url, crate::images::ImageKind::Obscured) {
        crate::images::ImageState::Ready(tex) => {
            p.image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        _ => {
            p.rect_filled(rect, t.rounding, t.fill_field);
        }
    }
    p.rect_filled(rect, t.rounding, t.scrim);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, HIDDEN, egui::FontId::proportional(theme.font(TypeToken::Icon)), t.fg);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, accessible_name(None)));
    resp
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use egui_kittest::kittest::Queryable;

    use crate::spoiler::snapshot::{GuardSnapshot, SeriesMark, Settings};

    /// Severance, next up S2E4: later episodes are guarded.
    fn severance() -> Guard {
        let mut series = HashMap::new();
        series.insert("severance".to_owned(), SeriesMark { label: "Severance".into(), next_up: (2, 4), automatic: true, ..Default::default() });
        Guard::with_snapshot(GuardSnapshot::from_parts(Settings::default(), Vec::new(), series, HashMap::new()))
    }

    #[test]
    fn an_episode_title_always_shows_and_a_hidden_plot_says_why_never_the_text() {
        crate::spoiler::ui::hide_all();
        let sp = Spoilers::with_guard(severance(), 0, true);
        let ep4 = Subject::Episode { series_norm: "severance", season: 2, episode: 4 };
        assert_eq!(sp.text(&ep4, Field::Plot, "Mark goes to the retreat").as_deref(), Some("Mark goes to the retreat"), "the next-up episode");
        let ep5 = Subject::Episode { series_norm: "severance", season: 2, episode: 5 };
        assert_eq!(sp.text(&ep5, Field::EpisodeTitle, "Trojan's Horse").as_deref(), Some("Trojan's Horse"));
        let plot = sp.text(&ep5, Field::Plot, "What happens in Trojan's Horse").unwrap();
        assert!(plot.contains("Plot hidden") && plot.contains("Severance") && !plot.contains("Trojan"), "{plot}");
        // the identity guard (feature off) shows everything
        let off = Spoilers::default();
        assert_eq!(off.text(&ep5, Field::Plot, "What happens").as_deref(), Some("What happens"));
    }

    #[test]
    fn a_guide_airings_hidden_sub_title_is_a_fixed_bar() {
        let sp = Spoilers::with_guard(severance(), 0, true);
        let cats = vec!["Drama".to_owned()];
        let airing = |sub| Subject::Programme {
            title: "Severance",
            sub_title: Some(sub),
            categories: &cats,
            season_episode: Some((2, 5)),
            start: 0,
            is_new: false,
        };
        let short = sp.text(&airing("Trojan's Horse"), Field::SubTitle, "Trojan's Horse").unwrap().into_owned();
        let long = "Attila the Hun and the Very Long Episode Title";
        assert_eq!(sp.text(&airing(long), Field::SubTitle, long).unwrap(), short, "a bar's width never tells the title's length");
        assert!(short.starts_with('▆') && !short.contains("Trojan"));
    }

    #[test]
    fn a_reveal_shows_that_item_only_until_it_is_hidden_again() {
        crate::spoiler::ui::hide_all();
        let ep5 = Subject::Episode { series_norm: "severance", season: 2, episode: 5 };
        crate::spoiler::ui::reveal_key(RevealKey::of(&ep5).unwrap());
        let sp = Spoilers::with_guard(severance(), 0, true);
        assert_eq!(sp.text(&ep5, Field::Plot, "Mark escapes").as_deref(), Some("Mark escapes"));
        let ep6 = Subject::Episode { series_norm: "severance", season: 2, episode: 6 };
        assert_ne!(sp.text(&ep6, Field::Plot, "Helly lies").as_deref(), Some("Helly lies"));
        crate::spoiler::ui::hide_all();
        let sp = Spoilers::with_guard(severance(), 0, true);
        assert_ne!(sp.text(&ep5, Field::Plot, "Mark escapes").as_deref(), Some("Mark escapes"));
    }

    #[test]
    fn a_click_on_a_hidden_block_reveals_the_item_and_a_click_on_the_eye_hides_it() {
        crate::spoiler::ui::hide_all();
        let ep5 = Subject::Episode { series_norm: "severance", season: 2, episode: 5 };
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let sp = Spoilers::with_guard(severance(), 0, true);
            field(ui, &Theme::fallback(), &sp, &ep5, Field::Plot, "Mark escapes", 16.0, ui.visuals().text_color());
        });
        harness.run();
        harness.get_by_label("Hidden by spoiler guard (Severance)").click();
        harness.run();
        assert!(crate::spoiler::ui::reveals().covers(&ep5));
        harness.get_by_label("Mark escapes");
        harness.get_by_label("Hide spoilers again").click();
        harness.run();
        assert!(crate::spoiler::ui::reveals().is_empty());
    }
}
