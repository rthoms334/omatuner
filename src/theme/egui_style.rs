//! Apply a [`Theme`] to egui's global style/visuals. Owner: WP-01.
//!
//! Mapping (DESIGN §16.3): applied to BOTH egui styles and the theme preference pinned to the
//! Omarchy mode (we decide, not the OS); square corners (rounding from Hyprland, default 0),
//! no shadows, panel fill = background, window fill = menu background, extreme bg =
//! darker_background, widget states = the Omarchy state fills, selection = fg @ 0.35,
//! hyperlinks = accent, text = foreground, item spacing control-gap × xs, button padding 10×6,
//! interact height 28, popup padding 14, tooltips after 400 ms, text styles from the type scale
//! (heading uses the Medium family).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Visuals};

use super::{SpaceToken, Theme, TypeToken};

/// Named font families registered by `fonts::install` (Medium / Bold faces).
pub const FAMILY_MEDIUM: &str = "medium";
pub const FAMILY_BOLD: &str = "bold";

pub fn apply(ctx: &egui::Context, theme: &Theme) {
    let v = visuals(theme);
    ctx.set_theme(if theme.is_light() { egui::Theme::Light } else { egui::Theme::Dark });
    let text_styles: std::collections::BTreeMap<TextStyle, FontId> = [
        (TextStyle::Small, FontId::new(theme.font(TypeToken::BodySmall), FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(theme.font(TypeToken::Body), FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(theme.font(TypeToken::Body), FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(theme.font(TypeToken::Heading), FontFamily::Name(FAMILY_MEDIUM.into()))),
        (TextStyle::Monospace, FontId::new(theme.font(TypeToken::Body), FontFamily::Monospace)),
    ]
    .into();
    let popup_padding = margin(theme.space(SpaceToken::PopupPadding));
    ctx.all_styles_mut(|s| {
        s.visuals = v.clone();
        s.spacing.item_spacing = egui::vec2(theme.space(SpaceToken::ControlGap), theme.space(SpaceToken::Xs));
        s.spacing.button_padding = egui::vec2(theme.space(SpaceToken::ControlPaddingX), theme.space(SpaceToken::ControlPaddingY));
        s.spacing.interact_size.y = theme.space(SpaceToken::ControlHeight);
        s.spacing.window_margin = popup_padding;
        s.spacing.menu_margin = margin(theme.space(SpaceToken::Md));
        s.interaction.tooltip_delay = 0.4;
        s.text_styles = text_styles.clone();
    });
}

/// The visuals for `theme` (pure; tested headless).
fn visuals(theme: &Theme) -> Visuals {
    let t = &theme.tokens;
    let r = CornerRadius::same(t.rounding.round().clamp(0.0, 255.0) as u8);
    let mut v = if theme.is_light() { Visuals::light() } else { Visuals::dark() };
    v.dark_mode = !theme.is_light();
    v.override_text_color = Some(t.fg);
    v.weak_text_color = Some(t.text_secondary);
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
    v
}

fn margin(px: f32) -> Margin {
    Margin::same(px.round().clamp(0.0, 127.0) as i8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Palette, ShellConfig};

    fn theme(colors: &str, rounding: f32) -> Theme {
        Theme::from_parts("t".into(), Palette::parse(colors, false).unwrap(), ShellConfig::default(), rounding, 1.0)
    }

    #[test]
    fn applies_to_both_styles_and_pins_the_mode() {
        let ctx = egui::Context::default();
        let light = theme("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\naccent = \"#1e66f5\"\n", 0.0);
        apply(&ctx, &light);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        for which in [egui::Theme::Light, egui::Theme::Dark] {
            let s = ctx.style_of(which);
            assert_eq!(s.visuals.panel_fill, light.palette.background);
            assert_eq!(s.visuals.override_text_color, Some(light.palette.foreground));
            assert_eq!(s.visuals.hyperlink_color, light.palette.accent);
            assert!(!s.visuals.dark_mode);
            assert_eq!(s.spacing.interact_size.y, 28.0);
            assert_eq!(s.spacing.button_padding, egui::vec2(10.0, 6.0));
            assert_eq!(s.spacing.item_spacing, egui::vec2(8.0, 3.0));
            assert_eq!(s.text_styles[&TextStyle::Heading], FontId::new(16.0, FontFamily::Name(FAMILY_MEDIUM.into())));
            assert_eq!(s.text_styles[&TextStyle::Body].size, 12.0);
        }
        let dark = theme("background = \"#151E26\"\nforeground = \"#ECE5D8\"\n", 0.0);
        apply(&ctx, &dark);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert!(ctx.style_of(egui::Theme::Light).visuals.dark_mode, "the OS preference doesn't matter");
    }

    #[test]
    fn rounding_shadows_and_selection() {
        let t = theme("background = \"#151E26\"\nforeground = \"#ECE5D8\"\n", 6.4);
        let v = visuals(&t);
        assert_eq!(v.window_corner_radius, CornerRadius::same(6));
        assert_eq!(v.widgets.inactive.corner_radius, CornerRadius::same(6));
        assert_eq!(v.window_shadow, Shadow::NONE);
        assert_eq!(v.popup_shadow, Shadow::NONE);
        assert_eq!(v.selection.bg_fill, crate::theme::with_alpha(t.palette.foreground, 0.35));
        assert_eq!(v.extreme_bg_color, t.palette.darker_background);
        assert_eq!(visuals(&theme("background = \"#000000\"\n", 0.0)).window_corner_radius, CornerRadius::ZERO);
    }
}
