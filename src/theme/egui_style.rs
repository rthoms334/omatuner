//! Apply a [`Theme`] to egui's global style/visuals. Skeleton implements the essentials;
//! WP-01 refines (text styles per token, selection, scroll bars, tooltips).
//!
//! Mapping (DESIGN §16.3): square corners (rounding from Hyprland, default 0), no shadows,
//! panel fill = background, extreme bg = darker_background, widget states = foreground at the
//! Omarchy alphas, selection = fg @ 0.35, hyperlinks = accent, text = foreground.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals};

use super::{Theme, TypeToken};

/// Named font families registered by `fonts::install` (Medium / Bold faces).
pub const FAMILY_MEDIUM: &str = "medium";
pub const FAMILY_BOLD: &str = "bold";

pub fn apply(ctx: &egui::Context, theme: &Theme) {
    let t = &theme.tokens;
    let r = CornerRadius::same(t.rounding.round().clamp(0.0, 255.0) as u8);
    let mut v = if theme.is_light() { Visuals::light() } else { Visuals::dark() };
    v.dark_mode = !theme.is_light();
    v.override_text_color = Some(t.fg);
    v.hyperlink_color = t.accent;
    v.panel_fill = t.bg;
    v.window_fill = t.card_bg;
    v.extreme_bg_color = theme.palette.darker_background;
    v.faint_bg_color = t.fill_field;
    v.code_bg_color = t.fill_field;
    v.warn_fg_color = t.warning;
    v.error_fg_color = t.urgent;
    v.window_corner_radius = r;
    v.menu_corner_radius = r;
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.window_stroke = Stroke::new(t.card_border_width, t.popup_border);
    v.selection.bg_fill = t.fill_text_selection;
    v.selection.stroke = Stroke::new(1.0, t.fg);
    v.text_cursor.stroke = Stroke::new(2.0, t.accent);

    let w = &mut v.widgets;
    for (ws, fill, stroke) in [
        (&mut w.noninteractive, Color32::TRANSPARENT, Stroke::new(1.0, t.separator)),
        (&mut w.inactive, t.fill_field, Stroke::NONE),
        (&mut w.hovered, t.fill_hover, Stroke::new(1.0, t.border_hover)),
        (&mut w.active, t.fill_pressed, Stroke::new(1.0, t.border_hover)),
        (&mut w.open, t.fill_selected, Stroke::NONE),
    ] {
        ws.bg_fill = fill;
        ws.weak_bg_fill = fill;
        ws.bg_stroke = stroke;
        ws.corner_radius = r;
        ws.fg_stroke = Stroke::new(1.0, t.fg);
        ws.expansion = 0.0;
    }

    ctx.all_styles_mut(|s| {
        s.visuals = v.clone();
        s.spacing.item_spacing = egui::vec2(theme.space(super::SpaceToken::ControlGap), theme.space(super::SpaceToken::Xs));
        s.spacing.button_padding = egui::vec2(theme.space(super::SpaceToken::ControlPaddingX), theme.space(super::SpaceToken::ControlPaddingY));
        s.spacing.interact_size.y = theme.space(super::SpaceToken::ControlHeight);
        s.text_styles = [
            (TextStyle::Small, FontId::new(theme.font(TypeToken::BodySmall), FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(theme.font(TypeToken::Body), FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(theme.font(TypeToken::Body), FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(theme.font(TypeToken::Heading), FontFamily::Name(FAMILY_MEDIUM.into()))),
            (TextStyle::Monospace, FontId::new(theme.font(TypeToken::Body), FontFamily::Monospace)),
        ]
        .into();
    });
}
