//! Left navigation ([Play on] · Search · Home, Live TV, Guide, Movies, Series, Up next · Settings).
//! Expanded (glyph + label, 184 pt) / rail (glyph only, 48 pt) / hidden (Compact). Current item =
//! selected fill + accent text; hover = fg@0.08. Shows Movies/Series only when some source
//! provides them. Owner: WP-10a.
//!
//! **Play on** (DESIGN §40 §3.9) heads the list while casting is on and a TV is ready: where plays
//! go ("This computer" / the TV's name, the TV in the accent colour); a click opens the same
//! chooser as the status bar's chip and Alt+T. It is a control, not a screen: never the lit tab.
//!
//! **Top glyph strip** (Compact below 480·rem, where `layout::sidebar_width` is 0): call
//! [`sidebar`] inside a horizontal layout (e.g. `ui.horizontal(|ui| sidebar(ui, …))` in a top
//! panel of height `ControlHeight + Md`); it then draws the same entries as a row of glyph
//! buttons with tooltips, so Compact keeps mouse navigation.

use eframe::egui::{self, Align2, RichText, Sense};

use super::list::paint_row_state;
use super::{RowState, glyphs, paint};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::ScreenId;

pub struct SidebarItem {
    pub id: ScreenId,
    pub glyph: &'static str,
    pub label: &'static str,
}

pub const ITEMS: &[SidebarItem] = &[
    SidebarItem { id: ScreenId::Home, glyph: glyphs::HOME, label: "Home" },
    SidebarItem { id: ScreenId::Live, glyph: glyphs::LIVE_TV, label: "Live TV" },
    SidebarItem { id: ScreenId::Guide, glyph: glyphs::GUIDE, label: "Guide" },
    SidebarItem { id: ScreenId::Movies, glyph: glyphs::MOVIES, label: "Movies" },
    SidebarItem { id: ScreenId::Series, glyph: glyphs::SERIES, label: "Series" },
    // only while Up next has items to go, with their count (DESIGN §32)
    SidebarItem { id: ScreenId::Queue, glyph: glyphs::UP_NEXT, label: "Up next" },
    SidebarItem { id: ScreenId::Settings, glyph: glyphs::SETTINGS, label: "Settings" },
];

/// Pseudo-entry for the search button (drawn above Home).
const SEARCH: SidebarItem = SidebarItem { id: ScreenId::Home, glyph: glyphs::SEARCH, label: "Search" };

/// Whether `current` highlights entry `id` (detail pages belong to their grid, Logs and EPG
/// mapping to Settings).
pub fn is_current(id: ScreenId, current: ScreenId) -> bool {
    current == id
        || (id == ScreenId::Movies && current == ScreenId::MovieDetail)
        || (id == ScreenId::Series && current == ScreenId::SeriesDetail)
        || (id == ScreenId::Settings && matches!(current, ScreenId::Logs | ScreenId::EpgMapping))
}

/// What a sidebar row is: the Play on control, the search button or a screen.
#[derive(Clone, Copy)]
enum Entry {
    PlayOn,
    Search,
    Screen(&'static SidebarItem),
}

/// Where plays go, for the Play on entry (`None` from the app: the entry is hidden).
#[derive(Clone, Copy)]
pub struct PlayOn<'a> {
    /// The TV's name; `None`: this computer.
    pub tv: Option<&'a str>,
    /// The key that opens the chooser (for the rail's tooltip).
    pub key: &'a str,
}

/// The visible entries in order. `play_on`: the Play on control is shown; `up_next`: Up next's
/// pending items (`None`: the entry is hidden).
fn entries(play_on: bool, has_vod: bool, up_next: Option<usize>) -> impl Iterator<Item = Entry> {
    play_on.then_some(Entry::PlayOn).into_iter().chain(std::iter::once(Entry::Search)).chain(
        ITEMS
            .iter()
            .filter(move |it| has_vod || !matches!(it.id, ScreenId::Movies | ScreenId::Series))
            .filter(move |it| up_next.is_some() || it.id != ScreenId::Queue)
            .map(Entry::Screen),
    )
}

/// What one row shows: glyph, label, and the name / tooltip when that differs from the label.
struct Row<'a> {
    glyph: &'a str,
    label: &'a str,
    /// The accessible name and the rail's tooltip (default: the label, with `count`).
    name: Option<String>,
    /// Drawn in the accent colour (Play on while a TV is the target).
    accent: bool,
}

/// One entry: `size` rect, glyph (+ label when `expanded`), tooltip otherwise; `count` (Up
/// next's items to go) right-aligned when expanded, on the glyph's corner otherwise. Returns
/// clicked.
fn entry(ui: &mut egui::Ui, theme: &Theme, it: &Row<'_>, size: egui::Vec2, expanded: bool, selected: bool, count: Option<usize>) -> bool {
    let t = &theme.tokens;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = match (&it.name, count) {
        (Some(name), _) => name.clone(),
        (None, Some(n)) => format!("{} {n}", it.label),
        (None, None) => it.label.to_owned(),
    };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, &label));
    if ui.is_rect_visible(rect) {
        paint_row_state(ui.painter(), rect, theme, RowState::from_flags(resp.hovered(), selected));
        let color = if selected || resp.hovered() {
            t.list_cursor_text
        } else if it.accent {
            t.accent
        } else {
            t.fg
        };
        let icon_font = paint::font(theme, TypeToken::IconLarge);
        let count_color = if selected || resp.hovered() { t.list_cursor_text } else { t.text_secondary };
        if expanded {
            let pad = theme.space(SpaceToken::RowPaddingX);
            ui.painter().text(rect.left_center() + egui::vec2(pad, 0.0), Align2::LEFT_CENTER, it.glyph, icon_font, color);
            let x = pad + theme.font(TypeToken::IconLarge) + theme.space(SpaceToken::ControlGap);
            let mut right = rect.right() - pad;
            if let Some(n) = count {
                let r = paint::text(
                    ui.painter(),
                    egui::pos2(right, rect.center().y),
                    Align2::RIGHT_CENTER,
                    n.to_string(),
                    paint::font(theme, TypeToken::Body),
                    count_color,
                    f32::INFINITY,
                );
                right = r.left() - theme.space(SpaceToken::Md);
            }
            paint::text(
                ui.painter(),
                rect.left_center() + egui::vec2(x, 0.0),
                Align2::LEFT_CENTER,
                it.label,
                paint::medium(ui.ctx(), theme, TypeToken::Title),
                color,
                (right - rect.left() - x).max(0.0),
            );
        } else {
            let g = ui.painter().text(rect.center(), Align2::CENTER_CENTER, it.glyph, icon_font, color);
            if let Some(n) = count {
                ui.painter().text(g.right_top(), Align2::LEFT_TOP, n.to_string(), paint::font(theme, TypeToken::Caption), count_color);
            }
        }
    }
    let resp = if expanded { resp } else { resp.on_hover_text(RichText::new(&label).size(theme.font(TypeToken::BodySmall))) };
    resp.clicked()
}

/// Draw; returns the clicked item. `search_clicked` is set when the search entry is clicked,
/// `play_on_clicked` when the Play on control (shown for `Some`) is.
#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    ui: &mut egui::Ui,
    theme: &Theme,
    current: ScreenId,
    expanded: bool,
    has_vod: bool,
    up_next: Option<usize>,
    play_on: Option<PlayOn<'_>>,
    search_clicked: &mut bool,
    play_on_clicked: &mut bool,
) -> Option<ScreenId> {
    let side = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Md);
    let strip = ui.layout().main_dir().is_horizontal();
    let mut clicked = None;
    if !strip {
        ui.add_space(theme.space(SpaceToken::PanelPadding));
    }
    let mut gap_before = false;
    for e in entries(play_on.is_some(), has_vod, up_next) {
        // Play on and Search stand apart from the screens, Settings from those between
        if gap_before || matches!(e, Entry::Screen(it) if it.id == ScreenId::Settings) {
            ui.add_space(theme.space(if strip { SpaceToken::Lg } else { SpaceToken::PanelGap }));
        }
        gap_before = matches!(e, Entry::PlayOn | Entry::Search);
        let row = match (e, play_on) {
            (Entry::Screen(it), _) => Row { glyph: it.glyph, label: it.label, name: None, accent: false },
            (Entry::PlayOn, Some(p)) => {
                let target = p.tv.unwrap_or("This computer");
                let glyph = if p.tv.is_some() { glyphs::CAST_CONNECTED } else { glyphs::CAST };
                Row { glyph, label: target, name: Some(format!("Play on: {target} ({})", p.key)), accent: p.tv.is_some() }
            }
            _ => Row { glyph: SEARCH.glyph, label: SEARCH.label, name: None, accent: false },
        };
        let size = if strip { egui::vec2(side, side) } else { egui::vec2(ui.available_width(), side) };
        let selected = matches!(e, Entry::Screen(it) if is_current(it.id, current));
        let count = up_next.filter(|_| matches!(e, Entry::Screen(it) if it.id == ScreenId::Queue));
        if entry(ui, theme, &row, size, expanded && !strip, selected, count) {
            match e {
                Entry::Screen(it) => clicked = Some(it.id),
                Entry::Search => *search_clicked = true,
                Entry::PlayOn => *play_on_clicked = true,
            }
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vod_entries_hide_without_vod_and_search_comes_first() {
        let labels = |play_on, vod, up_next| {
            entries(play_on, vod, up_next)
                .map(|e| match e {
                    Entry::PlayOn => "Play on",
                    Entry::Search => "Search",
                    Entry::Screen(it) => it.label,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(false, true, None), ["Search", "Home", "Live TV", "Guide", "Movies", "Series", "Settings"]);
        assert_eq!(labels(false, false, None), ["Search", "Home", "Live TV", "Guide", "Settings"]);
        assert_eq!(labels(false, true, Some(2)), ["Search", "Home", "Live TV", "Guide", "Movies", "Series", "Up next", "Settings"]);
        assert_eq!(labels(false, false, Some(2)), ["Search", "Home", "Live TV", "Guide", "Up next", "Settings"]);
        // Play on heads the list when shown, and is no screen
        assert_eq!(labels(true, false, None), ["Play on", "Search", "Home", "Live TV", "Guide", "Settings"]);
        assert_eq!(labels(true, true, Some(2)), ["Play on", "Search", "Home", "Live TV", "Guide", "Movies", "Series", "Up next", "Settings"]);
        assert!(is_current(ScreenId::Movies, ScreenId::MovieDetail));
        assert!(!is_current(ScreenId::Live, ScreenId::Guide));
    }
}
