//! Row renderers (Omarchy menu rows: 50 px, 58 with detail; compact density 36 px).
//! Owner: WP-10a.
//!
//! Every row allocates the full available width, paints its own state background
//! ([`list::paint_row_state`](super::list::paint_row_state)) and text colour (accent under the
//! cursor), elides text that doesn't fit with "…", and reports a `Button` widget info labelled
//! with its main text. Inside a fixed slot (a `virtual_list` row) a row never grows taller than
//! the slot.

use eframe::egui::{self, Align2, Color32, Rect};

use super::list::paint_row_state;
use super::progress::paint_progress;
use super::{RowState, glyphs, paint};
use crate::config::Density;
use crate::images::ImageCache;
use crate::model::{ChannelSummary, Episode, Group, NowNext, ProgrammeSlim, Timestamp};
use crate::store::library::SourceMeta;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};

/// Row width from which the channel detail line shows everything (title · time · next).
const DETAIL_FULL_W: f32 = 520.0;
/// Row width from which the channel detail line still shows the next programme.
const DETAIL_NEXT_W: f32 = 380.0;

/// Row height for a density (+ detail line).
pub fn row_height(theme: &Theme, density: Density, with_detail: bool) -> f32 {
    let rem = theme.rem().max(0.5);
    match density {
        Density::Compact => (36.0 * rem).round(),
        Density::Comfortable => {
            let base = (50.0 * rem).max(theme.font(TypeToken::Body) + 2.0 * theme.space(SpaceToken::RowPaddingX));
            if with_detail { (base + 8.0 * rem).round() } else { base.round() }
        }
    }
}

/// Allocate a full-width clickable row and paint its state background.
fn allocate_row(ui: &mut egui::Ui, theme: &Theme, natural_h: f32, state: RowState, label: &str) -> (Rect, egui::Response) {
    let h = paint::fit_height(ui, natural_h);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        paint_row_state(ui.painter(), rect, theme, state);
    }
    (rect, resp)
}

/// Thin progress bar height (programme / watch progress inside rows).
fn bar_h(theme: &Theme) -> f32 {
    (3.0 * theme.rem()).round().max(2.0)
}

/// Icon + label (+ detail at body-small @0.52, + chevron @0.36).
pub fn menu_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: Option<&str>,
    label: &str,
    detail: Option<&str>,
    chevron: bool,
    state: RowState,
) -> egui::Response {
    let t = &theme.tokens;
    let (rect, resp) = allocate_row(ui, theme, row_height(theme, Density::Comfortable, detail.is_some()), state, label);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let color = paint::row_text(theme, state);
    let inset = theme.space(SpaceToken::Lg);
    let mut left = rect.left() + inset;
    let mut right = rect.right() - inset;
    if let Some(icon) = icon {
        let col = (36.0 * theme.rem()).round();
        p.text(egui::pos2(left + col / 2.0, rect.center().y), Align2::CENTER_CENTER, icon, paint::font(theme, TypeToken::IconLarge), color);
        left += col + theme.space(SpaceToken::Sm);
    } else {
        left += theme.space(SpaceToken::Sm);
    }
    if chevron {
        let r = paint::text(
            p,
            egui::pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            glyphs::CHEVRON_RIGHT,
            paint::font(theme, TypeToken::Icon),
            t.text_disabled,
            f32::INFINITY,
        );
        right = r.left() - theme.space(SpaceToken::Lg);
    }
    let max_w = (right - left).max(0.0);
    let label_g = paint::galley(p, label, paint::medium(ui.ctx(), theme, TypeToken::Heading), color, max_w);
    match detail {
        Some(d) => {
            let detail_g = paint::galley(p, d, paint::font(theme, TypeToken::BodySmall), t.text_secondary, max_w);
            let gap = theme.space(SpaceToken::Xxs);
            let top = rect.center().y - (label_g.size().y + gap + detail_g.size().y) / 2.0;
            let detail_y = top + label_g.size().y + gap;
            p.galley(egui::pos2(left, top), label_g, color);
            p.galley(egui::pos2(left, detail_y), detail_g, t.text_secondary);
        }
        None => {
            p.galley(egui::pos2(left, rect.center().y - label_g.size().y / 2.0), label_g, color);
        }
    }
    resp
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelRowOpts<'a> {
    pub density: Density,
    pub show_number: bool,
    pub show_logo: bool,
    /// Show a source badge (multiple sources / search results).
    pub source: Option<&'a SourceMeta>,
    pub favourite: bool,
    /// Dim + warning glyph (recent failures).
    pub failed: bool,
    /// The playing channel: accent 󰐊 glyph left of the name (§13.0).
    pub playing: bool,
    /// Times on the 12-hour clock ("9:00 PM"; Settings ▸ Interface ▸ Clock). Default: 24 h.
    pub twelve_hour: bool,
    /// Replaces "No guide data" on the detail line, e.g. "Loading guide… 45 %" while the first
    /// guide import runs (DESIGN §13.12).
    pub no_guide_label: Option<&'a str>,
}

/// The detail line of a channel row for a row `width`: "title · 20:00–21:00 · next: X",
/// dropping the time range, then the next programme, as the row gets narrower. `None` while
/// now/next isn't known yet (no "No guide data" flicker while the cache loads).
fn channel_detail(ch: &ChannelSummary, now_next: Option<&NowNext>, width: f32, rem: f32, opts: &ChannelRowOpts<'_>) -> Option<(String, bool)> {
    let h24 = !opts.twelve_hour;
    let no_data = || Some((opts.no_guide_label.unwrap_or("No guide data").to_owned(), false));
    if ch.epg_channel_id.is_none() {
        return no_data();
    }
    let nn = now_next?;
    let next = nn.next.as_ref().map(|n| format!("next: {}", n.title));
    let line = match &nn.now {
        Some(p) => {
            let mut parts = vec![p.title.to_string()];
            if width >= DETAIL_FULL_W * rem {
                parts.push(super::time::span(p.start, p.stop, h24));
            }
            if width >= DETAIL_NEXT_W * rem
                && let Some(n) = next
            {
                parts.push(n);
            }
            parts.join(" · ")
        }
        None => match nn.next.as_ref() {
            Some(n) => format!("{} {}", super::time::hhmm(n.start, h24), n.title),
            None => return no_data(),
        },
    };
    Some((line, true))
}

/// `[logo] 101  BBC One                       ▕████░░▏ 20:00–21:00 News at Eight · next: …`
/// Medium/compact layouts drop the progress text then the next programme.
pub fn channel_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    ch: &ChannelSummary,
    now_next: Option<&NowNext>,
    now: Timestamp,
    opts: ChannelRowOpts<'_>,
    state: RowState,
) -> egui::Response {
    let t = &theme.tokens;
    let comfortable = opts.density == Density::Comfortable;
    let (rect, resp) = allocate_row(ui, theme, row_height(theme, opts.density, comfortable), state, &ch.name);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let dim = opts.failed || ch.unsupported;
    let color = if dim { t.text_disabled } else { paint::row_text(theme, state) };
    let inset = theme.space(SpaceToken::Lg);
    let gap = theme.space(SpaceToken::Lg);
    let mut left = rect.left() + inset;
    let mut right = rect.right() - inset;

    if opts.show_logo {
        let size = (rect.height() - 2.0 * theme.space(SpaceToken::Md)).min(40.0 * theme.rem()).max(8.0);
        let logo_rect = Rect::from_min_size(egui::pos2(left, rect.center().y - size / 2.0), egui::vec2(size, size));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(logo_rect));
        super::logo::logo(&mut child, theme, images, ch.logo.as_deref(), &ch.name, size);
        left += size + gap;
    }

    let p = ui.painter().clone();
    let line_font = paint::medium(ui.ctx(), theme, TypeToken::Heading);
    let detail = if comfortable { channel_detail(ch, now_next, rect.width(), theme.rem(), &opts) } else { None };
    let line_y = if detail.is_some() { rect.top() + rect.height() * 0.36 } else { rect.center().y };

    // right side, from the edge inwards: progress bar, favourite, lock, source badge
    if let Some(prog) = now_next.and_then(|nn| nn.now.as_ref()).filter(|p| p.contains(now)) {
        let w = (48.0 * theme.rem()).round();
        let bar = Rect::from_center_size(egui::pos2(right - w / 2.0, line_y), egui::vec2(w, bar_h(theme)));
        paint_progress(&p, bar, theme, prog.progress(now), false);
        right -= w + gap;
    }
    let mut glyph = |g: &str, c: Color32| {
        let r = paint::text(&p, egui::pos2(right, line_y), Align2::RIGHT_CENTER, g, paint::font(theme, TypeToken::Icon), c, f32::INFINITY);
        right = r.left() - theme.space(SpaceToken::Md);
    };
    if opts.favourite {
        glyph(glyphs::FAVOURITE, t.accent);
    }
    if ch.locked {
        glyph(glyphs::LOCK, t.text_secondary);
    }
    if let Some(src) = opts.source {
        glyph(super::badge::source_glyph(src), theme.source_color(src.color));
    }

    if opts.show_number {
        let num_font = paint::font(theme, TypeToken::Body);
        let col = paint::galley(&p, "9999", num_font.clone(), t.text_secondary, f32::INFINITY).size().x;
        if let Some(n) = ch.number {
            paint::text(&p, egui::pos2(left + col, line_y), Align2::RIGHT_CENTER, n.to_string(), num_font, t.text_secondary, col);
        }
        left += col + gap;
    }
    if opts.playing {
        let r = paint::text(
            &p,
            egui::pos2(left, line_y),
            Align2::LEFT_CENTER,
            glyphs::PLAY,
            paint::font(theme, TypeToken::Icon),
            t.accent,
            f32::INFINITY,
        );
        left = r.right() + theme.space(SpaceToken::Md);
    }
    let mut name_right = right;
    let mut suffix = String::new();
    if opts.failed {
        suffix.push_str(glyphs::WARNING);
    }
    if ch.unsupported {
        suffix.push_str(" DRM");
    } else if ch.radio {
        suffix.push_str(glyphs::RADIO);
    }
    if !suffix.is_empty() {
        let sg = paint::galley(
            &p,
            suffix.trim(),
            paint::font(theme, TypeToken::BodySmall),
            if opts.failed { t.warning } else { t.text_secondary },
            f32::INFINITY,
        );
        name_right -= sg.size().x + theme.space(SpaceToken::Md);
        let name_g = paint::galley(&p, ch.name.to_string(), line_font.clone(), color, (name_right - left).max(0.0));
        let sx = left + name_g.size().x + theme.space(SpaceToken::Md);
        p.galley(egui::pos2(sx, line_y - sg.size().y / 2.0), sg, color);
        p.galley(egui::pos2(left, line_y - name_g.size().y / 2.0), name_g, color);
    } else {
        paint::text(&p, egui::pos2(left, line_y), Align2::LEFT_CENTER, ch.name.to_string(), line_font, color, (name_right - left).max(0.0));
    }
    if let Some((line, has_data)) = detail {
        let c = if has_data { t.text_secondary } else { t.text_disabled };
        let y = rect.top() + rect.height() * 0.70;
        paint::text(
            &p,
            egui::pos2(left, y),
            Align2::LEFT_CENTER,
            line,
            paint::font(theme, TypeToken::BodySmall),
            c,
            (rect.right() - inset - left).max(0.0),
        );
    }
    resp
}

/// Extra marks of a group row (DESIGN §28.7).
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupDecor<'a> {
    /// Marked for a bulk hide: a check glyph and the accent tint.
    pub marked: bool,
    /// Short code of the provider name's country / language tag ("UK"), shown when the
    /// playlist has several (smart names drop the "UK |" prefix, so "UK | Sports" and
    /// "US | Sports" would both read "Sports").
    pub tag: Option<&'a str>,
}

/// Group/category row: name, count, lock glyph.
pub fn group_row(ui: &mut egui::Ui, theme: &Theme, g: &Group, state: RowState) -> egui::Response {
    group_row_decor(ui, theme, g, state, GroupDecor::default())
}

/// [`group_row`] with the bulk-hide mark and the tag badge.
pub fn group_row_decor(ui: &mut egui::Ui, theme: &Theme, g: &Group, state: RowState, decor: GroupDecor<'_>) -> egui::Response {
    let t = &theme.tokens;
    let name = g.display_name();
    let (rect, resp) = allocate_row(ui, theme, row_height(theme, Density::Comfortable, false), state, name);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    if decor.marked {
        p.rect_filled(rect, t.rounding, with_alpha(t.accent, 0.14));
    }
    let color = if g.hidden { t.text_disabled } else { paint::row_text(theme, state) };
    let inset = theme.space(SpaceToken::Lg);
    let left = rect.left() + inset + theme.space(SpaceToken::Sm);
    let mut right = rect.right() - inset;
    let y = rect.center().y;
    let count = paint::text(
        p,
        egui::pos2(right, y),
        Align2::RIGHT_CENTER,
        grouped(g.item_count as usize),
        paint::font(theme, TypeToken::Body),
        t.text_secondary,
        f32::INFINITY,
    );
    right = count.left() - theme.space(SpaceToken::Lg);
    if g.locked {
        let r = paint::text(
            p,
            egui::pos2(right, y),
            Align2::RIGHT_CENTER,
            glyphs::LOCK,
            paint::font(theme, TypeToken::Icon),
            t.text_secondary,
            f32::INFINITY,
        );
        right = r.left() - theme.space(SpaceToken::Md);
    }
    if g.hidden {
        let r = paint::text(
            p,
            egui::pos2(right, y),
            Align2::RIGHT_CENTER,
            glyphs::HIDDEN,
            paint::font(theme, TypeToken::Icon),
            t.text_disabled,
            f32::INFINITY,
        );
        right = r.left() - theme.space(SpaceToken::Md);
    }
    if let Some(code) = decor.tag {
        let r = code_badge(p, theme, egui::pos2(right, y), Align2::RIGHT_CENTER, code, t.text_secondary);
        right = r.left() - theme.space(SpaceToken::Md);
    }
    if decor.marked {
        let r =
            paint::text(p, egui::pos2(right, y), Align2::RIGHT_CENTER, glyphs::CHECK, paint::font(theme, TypeToken::Icon), t.accent, f32::INFINITY);
        right = r.left() - theme.space(SpaceToken::Md);
    }
    paint::text(
        p,
        egui::pos2(left, y),
        Align2::LEFT_CENTER,
        name,
        paint::medium(ui.ctx(), theme, TypeToken::Heading),
        color,
        (right - left).max(0.0),
    );
    resp
}

/// A short code in a 1 px outlined badge ("UK", "EX-YU"), anchored at `pos`; returns its rect.
fn code_badge(p: &egui::Painter, theme: &Theme, pos: egui::Pos2, anchor: Align2, code: &str, color: Color32) -> Rect {
    let g = paint::galley(p, code, paint::font(theme, TypeToken::Caption), color, f32::INFINITY);
    let pad = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xxs));
    let rect = anchor.anchor_size(pos, g.size() + 2.0 * pad);
    p.rect_stroke(rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(color, 0.6)), egui::StrokeKind::Inside);
    p.galley(rect.center() - g.size() / 2.0, g, color);
    rect
}

/// A tick-list row ("Choose what to show", DESIGN §28.6): tick box, short-code badge, label,
/// detail line and a "new" badge. Reports a `Checkbox` widget labelled with `label`.
#[allow(clippy::too_many_arguments)]
pub fn check_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    ticked: bool,
    code: Option<&str>,
    label: &str,
    detail: Option<&str>,
    new: bool,
    state: RowState,
) -> egui::Response {
    let t = &theme.tokens;
    let h = paint::fit_height(ui, row_height(theme, Density::Comfortable, detail.is_some()));
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, ticked, label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    paint_row_state(p, rect, theme, state);
    let color = paint::row_text(theme, state);
    let inset = theme.space(SpaceToken::Lg);
    let y = rect.center().y;
    let col = (36.0 * theme.rem()).round();
    let (tick, tick_color) = if ticked { (glyphs::CHECKBOX_ON, t.accent) } else { (glyphs::CHECKBOX_OFF, t.text_secondary) };
    p.text(egui::pos2(rect.left() + inset + col / 2.0, y), Align2::CENTER_CENTER, tick, paint::font(theme, TypeToken::IconLarge), tick_color);
    let mut left = rect.left() + inset + col + theme.space(SpaceToken::Sm);
    if let Some(code) = code {
        code_badge(p, theme, egui::pos2(left, y), Align2::LEFT_CENTER, code, if ticked { t.fg } else { t.text_secondary });
    }
    // a fixed code column keeps the labels aligned
    left += (64.0 * theme.rem()).round();
    let mut right = rect.right() - inset;
    if new {
        let r = code_badge(p, theme, egui::pos2(right, y), Align2::RIGHT_CENTER, "new", t.accent);
        right = r.left() - theme.space(SpaceToken::Lg);
    }
    let max_w = (right - left).max(0.0);
    let label_g = paint::galley(p, label, paint::medium(ui.ctx(), theme, TypeToken::Heading), color, max_w);
    match detail {
        Some(d) => {
            let detail_g = paint::galley(p, d, paint::font(theme, TypeToken::BodySmall), t.text_secondary, max_w);
            let gap = theme.space(SpaceToken::Xxs);
            let top = y - (label_g.size().y + gap + detail_g.size().y) / 2.0;
            let detail_y = top + label_g.size().y + gap;
            p.galley(egui::pos2(left, top), label_g, color);
            p.galley(egui::pos2(left, detail_y), detail_g, t.text_secondary);
        }
        None => {
            p.galley(egui::pos2(left, y - label_g.size().y / 2.0), label_g, color);
        }
    }
    resp
}

/// "9 812" — thin-space grouped counts like the design mocks.
pub(crate) fn grouped(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// `20:00  News at Eight` (+ catch-up glyph when available, + live progress).
pub fn programme_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    p: &ProgrammeSlim,
    now: Timestamp,
    catchup: bool,
    h24: bool,
    state: RowState,
) -> egui::Response {
    let t = &theme.tokens;
    let (rect, resp) = allocate_row(ui, theme, row_height(theme, Density::Comfortable, false), state, &p.title);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter();
    let airing = p.contains(now);
    let past = p.stop <= now;
    let color = if state.is_cursor() {
        t.list_cursor_text
    } else if past {
        t.text_secondary
    } else {
        t.fg
    };
    let inset = theme.space(SpaceToken::Lg);
    let gap = theme.space(SpaceToken::Lg);
    let y = rect.center().y;
    let mut left = rect.left() + inset + theme.space(SpaceToken::Sm);
    let mut right = rect.right() - inset;
    let time_font = paint::font(theme, TypeToken::Body);
    let col = paint::galley(painter, if h24 { "00:00" } else { "00:00 PM" }, time_font.clone(), t.text_secondary, f32::INFINITY).size().x;
    paint::text(
        painter,
        egui::pos2(left, y),
        Align2::LEFT_CENTER,
        super::time::hhmm(p.start, h24),
        time_font,
        if airing { t.fg } else { t.text_secondary },
        col,
    );
    left += col + gap;
    if catchup && p.start <= now {
        let r = paint::text(
            painter,
            egui::pos2(right, y),
            Align2::RIGHT_CENTER,
            glyphs::CATCHUP,
            paint::font(theme, TypeToken::Icon),
            t.text_secondary,
            f32::INFINITY,
        );
        right = r.left() - theme.space(SpaceToken::Md);
    }
    if p.flags.is_live {
        let r =
            paint::text(painter, egui::pos2(right, y), Align2::RIGHT_CENTER, "LIVE", paint::font(theme, TypeToken::Caption), t.urgent, f32::INFINITY);
        right = r.left() - theme.space(SpaceToken::Md);
    }
    let title = match &p.sub_title {
        Some(s) if !s.is_empty() => format!("{} · {s}", p.title),
        _ => p.title.to_string(),
    };
    paint::text(painter, egui::pos2(left, y), Align2::LEFT_CENTER, title, paint::font(theme, TypeToken::Title), color, (right - left).max(0.0));
    if airing {
        let bar = Rect::from_min_max(
            egui::pos2(left, rect.bottom() - theme.space(SpaceToken::Md) - bar_h(theme)),
            egui::pos2(rect.right() - inset, rect.bottom() - theme.space(SpaceToken::Md)),
        );
        paint_progress(painter, bar, theme, p.progress(now), false);
    }
    resp
}

/// An episode's title; a playlist episode without one reads "Episode 2" (DESIGN §27.7).
pub fn episode_title(e: &Episode) -> std::borrow::Cow<'_, str> {
    if e.title.trim().is_empty() { format!("Episode {}", e.episode).into() } else { e.title.as_str().into() }
}

/// `E05  Title · 42 min` with watched check / progress bar; `playing` puts the accent 󰐊
/// marker left of the title, like channel rows (DESIGN §13.0).
pub fn episode_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    e: &Episode,
    progress: Option<f32>,
    watched: bool,
    playing: bool,
    state: RowState,
) -> egui::Response {
    let t = &theme.tokens;
    let name = episode_title(e);
    let (rect, resp) = allocate_row(ui, theme, row_height(theme, Density::Comfortable, false), state, &name);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter();
    let color = paint::row_text(theme, state);
    let inset = theme.space(SpaceToken::Lg);
    let gap = theme.space(SpaceToken::Lg);
    let y = rect.center().y;
    let mut left = rect.left() + inset + theme.space(SpaceToken::Sm);
    let mut right = rect.right() - inset;
    let num_font = paint::font(theme, TypeToken::Body);
    let col = paint::galley(painter, "E000", num_font.clone(), t.text_secondary, f32::INFINITY).size().x;
    paint::text(painter, egui::pos2(left, y), Align2::LEFT_CENTER, format!("E{:02}", e.episode), num_font, t.text_secondary, col);
    left += col + gap;
    if playing {
        let r = paint::text(
            painter,
            egui::pos2(left, y),
            Align2::LEFT_CENTER,
            glyphs::PLAY,
            paint::font(theme, TypeToken::Icon),
            t.accent,
            f32::INFINITY,
        );
        left = r.right() + theme.space(SpaceToken::Md);
    }
    if watched {
        let r = paint::text(
            painter,
            egui::pos2(right, y),
            Align2::RIGHT_CENTER,
            glyphs::CHECK,
            paint::font(theme, TypeToken::Icon),
            t.accent,
            f32::INFINITY,
        );
        right = r.left() - theme.space(SpaceToken::Md);
    }
    let title = match e.duration_s {
        Some(d) if d > 0 => format!("{name} · {} min", (d + 30) / 60),
        _ => name.into_owned(),
    };
    paint::text(painter, egui::pos2(left, y), Align2::LEFT_CENTER, title, paint::font(theme, TypeToken::Title), color, (right - left).max(0.0));
    if let Some(f) = progress.filter(|f| !watched && *f > 0.0 && *f < 1.0) {
        let bar = Rect::from_min_max(
            egui::pos2(left, rect.bottom() - theme.space(SpaceToken::Md) - bar_h(theme)),
            egui::pos2(rect.right() - inset, rect.bottom() - theme.space(SpaceToken::Md)),
        );
        paint_progress(painter, bar, theme, f, true);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heights_follow_omarchy_menu_rows() {
        let th = Theme::fallback();
        assert_eq!(row_height(&th, Density::Comfortable, false), 50.0);
        assert_eq!(row_height(&th, Density::Comfortable, true), 58.0);
        assert_eq!(row_height(&th, Density::Compact, false), 36.0);
        assert_eq!(row_height(&th, Density::Compact, true), 36.0, "compact rows have no detail line");
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(42), "42");
        assert_eq!(grouped(9812), "9\u{2009}812");
        assert_eq!(grouped(1_234_567), "1\u{2009}234\u{2009}567");
    }

    fn chan(epg: bool) -> ChannelSummary {
        ChannelSummary {
            id: crate::model::ChannelId(1),
            source_id: crate::model::SourceId(1),
            key: crate::model::ItemKey("k".into()),
            name: "BBC One".into(),
            number: Some(101),
            logo: None,
            group_ids: vec![],
            epg_channel_id: epg.then_some(crate::model::EpgChannelId(1)),
            tvg_shift_min: 0,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: 0,
            user_order: None,
            added_at: None,
        }
    }

    fn prog(title: &str, start: i64, stop: i64) -> ProgrammeSlim {
        ProgrammeSlim { id: crate::model::ProgrammeId(1), start, stop, title: title.into(), sub_title: None, flags: Default::default() }
    }

    #[test]
    fn channel_detail_degrades_with_width() {
        let o = ChannelRowOpts::default();
        let nn = NowNext { now: Some(prog("News", 0, 3600)), next: Some(prog("Drama", 3600, 7200)) };
        let wide = channel_detail(&chan(true), Some(&nn), 800.0, 1.0, &o).unwrap().0;
        assert!(wide.starts_with("News · ") && wide.ends_with(" · next: Drama"), "{wide}");
        assert_eq!(channel_detail(&chan(true), Some(&nn), 400.0, 1.0, &o).unwrap().0, "News · next: Drama");
        assert_eq!(channel_detail(&chan(true), Some(&nn), 300.0, 1.0, &o).unwrap().0, "News");
        // unknown yet → nothing (no flicker); no mapping or no data → "No guide data"
        assert!(channel_detail(&chan(true), None, 800.0, 1.0, &o).is_none());
        assert_eq!(channel_detail(&chan(false), None, 800.0, 1.0, &o), Some(("No guide data".into(), false)));
        assert_eq!(channel_detail(&chan(true), Some(&NowNext::default()), 800.0, 1.0, &o), Some(("No guide data".into(), false)));
    }

    /// The first guide import shows its progress instead of "No guide data" (DESIGN §13.12).
    #[test]
    fn channel_detail_says_the_guide_is_loading() {
        let o = ChannelRowOpts { no_guide_label: Some("Loading guide… 45 %"), ..Default::default() };
        assert_eq!(channel_detail(&chan(false), None, 800.0, 1.0, &o), Some(("Loading guide… 45 %".into(), false)));
        assert_eq!(channel_detail(&chan(true), Some(&NowNext::default()), 800.0, 1.0, &o), Some(("Loading guide… 45 %".into(), false)));
    }

    /// Settings ▸ Interface ▸ Clock applies to row times too.
    #[test]
    fn channel_detail_follows_the_clock_setting() {
        let nn = NowNext { now: Some(prog("News", 0, 3600)), next: None };
        let twelve = channel_detail(&chan(true), Some(&nn), 800.0, 1.0, &ChannelRowOpts { twelve_hour: true, ..Default::default() }).unwrap().0;
        assert!(twelve.contains("M–") && (twelve.ends_with("AM") || twelve.ends_with("PM")), "{twelve}");
        let h24 = channel_detail(&chan(true), Some(&nn), 800.0, 1.0, &ChannelRowOpts::default()).unwrap().0;
        assert!(!h24.contains('M'), "{h24}");
        let soon = NowNext { now: None, next: Some(prog("Drama", 3600, 7200)) };
        let line = channel_detail(&chan(true), Some(&soon), 800.0, 1.0, &ChannelRowOpts { twelve_hour: true, ..Default::default() }).unwrap().0;
        assert!(line.ends_with("M Drama"), "{line}");
    }
}
