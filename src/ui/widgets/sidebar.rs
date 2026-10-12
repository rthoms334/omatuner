//! Left navigation (Home, Live TV, Guide, Movies, Series · Search · Settings). Expanded (glyph +
//! label, 184 pt) / rail (glyph only, 48 pt) / hidden (Compact). Current item = selected fill +
//! accent text; hover = fg@0.08. Shows Movies/Series only when some source provides them.
//! Owner: WP-10a.
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

/// Pseudo-entry for the search button (drawn before Settings).
const SEARCH: SidebarItem = SidebarItem { id: ScreenId::Home, glyph: glyphs::SEARCH, label: "Search" };

/// Whether `current` highlights entry `id` (detail pages belong to their grid, Logs and EPG
/// mapping to Settings).
pub fn is_current(id: ScreenId, current: ScreenId) -> bool {
    current == id
        || (id == ScreenId::Movies && current == ScreenId::MovieDetail)
        || (id == ScreenId::Series && current == ScreenId::SeriesDetail)
        || (id == ScreenId::Settings && matches!(current, ScreenId::Logs | ScreenId::EpgMapping))
}

/// The visible entries in order; `None` marks the search button. `up_next`: Up next's pending
/// items (`None`: the entry is hidden).
fn entries(has_vod: bool, up_next: Option<usize>) -> impl Iterator<Item = Option<&'static SidebarItem>> {
    ITEMS
        .iter()
        .filter(move |it| has_vod || !matches!(it.id, ScreenId::Movies | ScreenId::Series))
        .filter(move |it| up_next.is_some() || it.id != ScreenId::Queue)
        .flat_map(|it| (it.id == ScreenId::Settings).then_some(None).into_iter().chain([Some(it)]))
}

/// One entry: `size` rect, glyph (+ label when `expanded`), tooltip otherwise; `count` (Up
/// next's items to go) right-aligned when expanded, on the glyph's corner otherwise. Returns
/// clicked.
fn entry(ui: &mut egui::Ui, theme: &Theme, it: &SidebarItem, size: egui::Vec2, expanded: bool, selected: bool, count: Option<usize>) -> bool {
    let t = &theme.tokens;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = match count {
        Some(n) => format!("{} {n}", it.label),
        None => it.label.to_owned(),
    };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, &label));
    if ui.is_rect_visible(rect) {
        paint_row_state(ui.painter(), rect, theme, RowState::from_flags(resp.hovered(), selected));
        let color = if selected || resp.hovered() { t.list_cursor_text } else { t.fg };
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

/// Draw; returns the clicked item. `search_clicked` is set when the search entry is clicked.
pub fn sidebar(
    ui: &mut egui::Ui,
    theme: &Theme,
    current: ScreenId,
    expanded: bool,
    has_vod: bool,
    up_next: Option<usize>,
    search_clicked: &mut bool,
) -> Option<ScreenId> {
    let side = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Md);
    let strip = ui.layout().main_dir().is_horizontal();
    let mut clicked = None;
    if !strip {
        ui.add_space(theme.space(SpaceToken::PanelPadding));
    }
    for e in entries(has_vod, up_next) {
        let it = e.unwrap_or(&SEARCH);
        if strip {
            if e.is_none() {
                ui.add_space(theme.space(SpaceToken::Lg));
            }
        } else if e.is_none() {
            ui.add_space(theme.space(SpaceToken::PanelGap));
        }
        let size = if strip { egui::vec2(side, side) } else { egui::vec2(ui.available_width(), side) };
        let selected = e.is_some_and(|it| is_current(it.id, current));
        let count = up_next.filter(|_| e.is_some_and(|it| it.id == ScreenId::Queue));
        if entry(ui, theme, it, size, expanded && !strip, selected, count) {
            match e {
                Some(it) => clicked = Some(it.id),
                None => *search_clicked = true,
            }
        }
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vod_entries_hide_without_vod_and_search_precedes_settings() {
        let labels = |vod, up_next| entries(vod, up_next).map(|e| e.map_or("Search", |it| it.label)).collect::<Vec<_>>();
        assert_eq!(labels(true, None), ["Home", "Live TV", "Guide", "Movies", "Series", "Search", "Settings"]);
        assert_eq!(labels(false, None), ["Home", "Live TV", "Guide", "Search", "Settings"]);
        assert_eq!(labels(false, Some(2)), ["Home", "Live TV", "Guide", "Up next", "Search", "Settings"]);
        assert!(is_current(ScreenId::Movies, ScreenId::MovieDetail));
        assert!(!is_current(ScreenId::Live, ScreenId::Guide));
    }
}
