//! Left navigation (Home, Live TV, Guide, Movies, Series · Search · Settings). Expanded (glyph +
//! label, 184 pt) / rail (glyph only, 48 pt) / hidden (Compact). Current item = selected fill +
//! accent text; hover = fg@0.08. Shows Movies/Series only when some source provides them.
//! Implemented (basic) in the skeleton; WP-10a owns polish (tooltips on rail, focus ring).

use eframe::egui::{self, RichText, Sense};

use super::glyphs;
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
    SidebarItem { id: ScreenId::Settings, glyph: glyphs::SETTINGS, label: "Settings" },
];

/// Draw; returns the clicked item. `search_clicked` is set when the search entry is clicked.
pub fn sidebar(ui: &mut egui::Ui, theme: &Theme, current: ScreenId, expanded: bool, has_vod: bool, search_clicked: &mut bool) -> Option<ScreenId> {
    let t = &theme.tokens;
    let mut clicked = None;
    let row_h = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Md);
    ui.add_space(theme.space(SpaceToken::PanelPadding));
    let item = |ui: &mut egui::Ui, glyph: &str, label: &str, selected: bool| -> bool {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, row_h), Sense::click());
        let fill = if selected { t.fill_selected } else if resp.hovered() { t.fill_hover } else { egui::Color32::TRANSPARENT };
        ui.painter().rect_filled(rect, t.rounding, fill);
        let color = if selected || resp.hovered() { t.list_cursor_text } else { t.fg };
        let pad = theme.space(SpaceToken::RowPaddingX);
        ui.painter().text(
            rect.left_center() + egui::vec2(pad, 0.0),
            egui::Align2::LEFT_CENTER,
            glyph,
            egui::FontId::proportional(theme.font(TypeToken::IconLarge)),
            color,
        );
        if expanded {
            ui.painter().text(
                rect.left_center() + egui::vec2(pad + theme.font(TypeToken::IconLarge) + theme.space(SpaceToken::ControlGap), 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::new(theme.font(TypeToken::Title), egui::FontFamily::Name(crate::theme::egui_style::FAMILY_MEDIUM.into())),
                color,
            );
        } else {
            resp.clone().on_hover_text(RichText::new(label).size(theme.font(TypeToken::BodySmall)));
        }
        resp.clicked()
    };
    for it in ITEMS {
        if !has_vod && matches!(it.id, ScreenId::Movies | ScreenId::Series) {
            continue;
        }
        if it.id == ScreenId::Settings {
            ui.add_space(theme.space(SpaceToken::PanelGap));
            if item(ui, glyphs::SEARCH, "Search", false) {
                *search_clicked = true;
            }
        }
        let selected = current == it.id
            || (it.id == ScreenId::Movies && current == ScreenId::MovieDetail)
            || (it.id == ScreenId::Series && current == ScreenId::SeriesDetail);
        if item(ui, it.glyph, it.label, selected) {
            clicked = Some(it.id);
        }
    }
    clicked
}
