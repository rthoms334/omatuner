//! Derived design tokens (Omarchy `Style.qml` type scale + spacing, `Color.qml` surfaces,
//! state fills; omarchy.md §2, §5; DESIGN §16.2).
//!
//! * `font(t)  = override[t] ?: max(1, round(base × mult[t]))`, base = `[font] base-size`
//!   (integer, default 12, floor 1) × `ui_scale`; pinned overrides are scaled by `ui_scale` too.
//! * `space(t) = override[t] ?: max(1, round(px[t] × scale))`,
//!   `scale = spacing.scale × (scale-with-font ? base/12 : ui_scale)`; `hairline` can't be pinned.
//! * State tints are the `[controls]` state colours (default FOREGROUND) at the Omarchy alphas
//!   (works in light themes); a state with border width 0 gets a transparent border.
//! * Surface colours use `composed(colour, alpha)`: the `*-alpha` key replaces the colour's alpha.

use eframe::egui::Color32;

use super::colors::Palette;
use super::shell::ShellConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeToken {
    Caption,
    BodySmall,
    Body,
    Subtitle,
    Title,
    Heading,
    Display,
    DisplayLarge,
    IconSmall,
    Icon,
    IconLarge,
}

impl TypeToken {
    pub const ALL: [TypeToken; 11] = [
        TypeToken::Caption,
        TypeToken::BodySmall,
        TypeToken::Body,
        TypeToken::Subtitle,
        TypeToken::Title,
        TypeToken::Heading,
        TypeToken::Display,
        TypeToken::DisplayLarge,
        TypeToken::IconSmall,
        TypeToken::Icon,
        TypeToken::IconLarge,
    ];
    pub fn mult(self) -> f32 {
        match self {
            TypeToken::Caption => 0.833,
            TypeToken::BodySmall | TypeToken::IconSmall => 0.917,
            TypeToken::Body => 1.0,
            TypeToken::Subtitle => 1.083,
            TypeToken::Title | TypeToken::Icon => 1.167,
            TypeToken::Heading => 1.333,
            TypeToken::Display => 2.0,
            TypeToken::DisplayLarge => 2.333,
            TypeToken::IconLarge => 1.5,
        }
    }
    /// Key in `[font]` for a per-token px override.
    pub fn key(self) -> &'static str {
        match self {
            TypeToken::Caption => "caption",
            TypeToken::BodySmall => "body-small",
            TypeToken::Body => "body",
            TypeToken::Subtitle => "subtitle",
            TypeToken::Title => "title",
            TypeToken::Heading => "heading",
            TypeToken::Display => "display",
            TypeToken::DisplayLarge => "display-large",
            TypeToken::IconSmall => "icon-small",
            TypeToken::Icon => "icon",
            TypeToken::IconLarge => "icon-large",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpaceToken {
    Hairline,
    Xxs,
    Xs,
    Sm,
    Md,
    Lg,
    Xl,
    Xxl,
    Xxxl,
    Huge,
    ControlGap,
    ControlPaddingX,
    ControlPaddingY,
    InputPaddingY,
    ControlHeight,
    PopupRowHeight,
    RowGap,
    RowPaddingX,
    LabelGap,
    PanelGap,
    PanelPadding,
    PopupPadding,
}

impl SpaceToken {
    pub fn base_px(self) -> f32 {
        use SpaceToken::*;
        match self {
            Hairline => 1.0,
            Xxs => 2.0,
            Xs => 3.0,
            Sm => 4.0,
            Md => 6.0,
            Lg => 8.0,
            Xl => 10.0,
            Xxl => 12.0,
            Xxxl => 14.0,
            Huge => 18.0,
            ControlGap => 8.0,
            ControlPaddingX => 10.0,
            ControlPaddingY => 6.0,
            InputPaddingY => 7.0,
            ControlHeight => 28.0,
            PopupRowHeight => 28.0,
            RowGap => 8.0,
            RowPaddingX => 12.0,
            LabelGap => 4.0,
            PanelGap => 14.0,
            PanelPadding => 18.0,
            PopupPadding => 14.0,
        }
    }
    pub fn key(self) -> &'static str {
        use SpaceToken::*;
        match self {
            Hairline => "hairline",
            Xxs => "xxs",
            Xs => "xs",
            Sm => "sm",
            Md => "md",
            Lg => "lg",
            Xl => "xl",
            Xxl => "xxl",
            Xxxl => "xxxl",
            Huge => "huge",
            ControlGap => "control-gap",
            ControlPaddingX => "control-padding-x",
            ControlPaddingY => "control-padding-y",
            InputPaddingY => "input-padding-y",
            ControlHeight => "control-height",
            PopupRowHeight => "popup-row-height",
            RowGap => "row-gap",
            RowPaddingX => "row-padding-x",
            LabelGap => "label-gap",
            PanelGap => "panel-gap",
            PanelPadding => "panel-padding",
            PopupPadding => "popup-padding",
        }
    }
}

/// Interaction states (Omarchy Button.qml paint priority: pressed > focus > hover/cursor >
/// selected > active > idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    Idle,
    /// "field" controls (inputs, dropdowns) at rest: fg @ 0.04 fill, 1 px fg @ 0.4 border.
    Field,
    Hover,
    Focus,
    Selected,
    Pressed,
}

/// Every colour/size the widgets use. Computed once per theme load.
#[derive(Debug, Clone, PartialEq)]
pub struct Tokens {
    pub base_font: f32,
    pub scale: f32,
    pub rounding: f32,
    // surfaces
    pub bg: Color32,
    pub card_bg: Color32,
    pub raised_bg: Color32,
    pub fg: Color32,
    pub accent: Color32,
    pub urgent: Color32,
    pub success: Color32,
    pub warning: Color32,
    // text
    pub text_secondary: Color32,  // fg @ 0.52
    pub text_placeholder: Color32, // fg @ 0.58
    pub text_section: Color32,    // darker(fg, 1.4)
    pub text_disabled: Color32,   // fg @ 0.36
    // lines
    pub separator: Color32,       // fg @ 0.12
    pub pane_divider: Color32,    // fg @ 0.28
    pub group_divider: Color32,   // fg @ 0.20
    pub card_border: Color32,     // menu.border (fg) @ menu.border-alpha
    pub popup_border: Color32,    // popups.border (accent) @ popups.border-alpha
    pub card_border_width: f32,   // popups/menu border-width, else max(1, space(2)) = 2 px
    // state fills ([controls] state colour, default fg, @ alpha)
    pub fill_field: Color32,      // 0.04
    pub fill_hover: Color32,      // 0.08
    pub fill_selected: Color32,   // 0.18
    pub fill_pressed: Color32,    // 0.22
    pub fill_text_selection: Color32, // 0.35
    pub border_field: Color32,    // fg @ 0.4
    pub border_hover: Color32,    // fg @ 0.25
    // lists (menu idiom)
    pub list_cursor_bg: Color32,  // menu.selected-background @ alpha (fg @ 0.08)
    pub list_cursor_text: Color32, // menu.selected-text (accent)
    pub list_cursor_border: Color32, // menu.selected-border @ alpha (template: fg @ 0.25; none: 0)
    pub scrim: Color32,           // menu.scrim @ alpha (bg @ 0.5)
    /// Fixed dark scrim for video OSD regardless of theme mode (FIXED DECISION 6).
    pub osd_scrim: Color32,
    pub osd_text: Color32,
    /// Accent used ON the fixed dark OSD scrim (progress, seek bar, focus): the theme accent
    /// when its contrast against the scrim composited over mid-grey video (`#2b2b2b`) is
    /// ≥ 3:1, else near-white `#f2f2f2` (e.g. the `white` theme's `#6e6e6e` accent, or
    /// catppuccin-latte's dark blue).
    pub osd_accent: Color32,
}

/// `c` with its alpha REPLACED by `a` (0..=1). `Color32` is premultiplied, so the RGB is
/// unmultiplied first (exact for opaque colours, which is what the palette holds).
pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Qt `darker(c, factor)`: HSV value ÷ factor (opaque result).
pub fn darker(c: Color32, factor: f32) -> Color32 {
    let f = 1.0 / factor.max(1e-3);
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Color32::from_rgb((r as f32 * f) as u8, (g as f32 * f) as u8, (b as f32 * f) as u8)
}

/// The fixed OSD scrim (`rgba(0,0,0,170)`) over mid-grey (#808080) video.
pub const OSD_EFFECTIVE_BG: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x2b);

/// Near-white OSD text, also the OSD accent when the theme accent is too dark for the scrim.
const OSD_TEXT: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);

/// `[font] base-size` with Style.qml semantics: `parseInt`, 1 px floor, default 12.
fn base_size(shell: &ShellConfig) -> f32 { shell.f32("font", "base-size").map(f32::trunc).unwrap_or(12.0).max(1.0) }

/// Config `ui_scale` guard: non-finite or non-positive values mean 1.0.
fn sanitize_ui_scale(s: f32) -> f32 { if s.is_finite() && s > 0.0 { s } else { 1.0 } }

/// WCAG relative luminance of an sRGB colour.
pub fn relative_luminance(c: Color32) -> f32 {
    let lin = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio (1..=21).
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

impl Tokens {
    /// Layout unit relative to Omarchy's default text size: `base_font / 12` (base-size × UI
    /// scale). Breakpoints, sidebar widths and guide geometry scale with it so
    /// `omarchy display text size 16` doesn't cramp the Medium layout.
    pub fn rem(&self) -> f32 { self.base_font / 12.0 }

    /// Derive every token from the palette and the merged shell config (Omarchy `Style.qml` /
    /// `Color.qml` semantics; see the module docs). `ui_scale` multiplies the base font size and
    /// with it every derived size.
    pub fn compute(pal: &Palette, shell: &ShellConfig, rounding: f32, ui_scale: f32) -> Self {
        let fg = pal.foreground;
        let ui_scale = sanitize_ui_scale(ui_scale);
        let base_font = base_size(shell) * ui_scale;
        let scale_with_font = shell.bool("spacing", "scale-with-font").unwrap_or(true);
        let spacing_scale = shell.f32("spacing", "scale").filter(|s| *s >= 0.0).unwrap_or(1.0);
        let scale = spacing_scale * if scale_with_font { (base_font / 12.0).max(1.0 / 12.0) } else { ui_scale };

        // A colour whose alpha is about to be replaced: its unmultiplied RGB, opaque (a
        // translucent gradient stop like `rgba(26a269ee)` keeps its exact RGB).
        let rgb_of = |section: &str, key: &str| shell.color_rgba(section, key, pal).map(|[r, g, b, _]| Color32::from_rgb(r, g, b));
        // `composed(colorKey, alphaKey, colorFallback, alphaFallback)`: the alpha key REPLACES the
        // colour's own alpha (Util.alpha).
        let composed = |section: &str, key: &str, fallback: Color32, alpha: f32| {
            let c = rgb_of(section, key).unwrap_or(fallback);
            with_alpha(c, shell.f32(section, &format!("{key}-alpha")).unwrap_or(alpha))
        };
        // `[controls]` (legacy name `[style]`): state colour tokens and alphas.
        let ctl_f32 = |k: &str| shell.f32("controls", k).or_else(|| shell.f32("style", k));
        let ctl_color = |k: &str| rgb_of("controls", k).or_else(|| rgb_of("style", k));
        let alpha = |k: &str, d: f32| ctl_f32(k).unwrap_or(d).clamp(0.0, 1.0);
        let normal = ctl_color("normal-color").unwrap_or(fg);
        let hover = ctl_color("hover-cursor-color").unwrap_or(fg);
        let selected = ctl_color("selected-color").unwrap_or(fg);
        let pressed = ctl_color("pressed-color").unwrap_or(hover);
        let selection = ctl_color("selection-color").unwrap_or(fg);
        let normal_border_w = ctl_f32("normal-border-width").unwrap_or(1.0).round().max(0.0);
        let hover_border_w = ctl_f32("hover-cursor-border-width").map_or(normal_border_w, |w| w.round().max(0.0));
        // A state whose border width is 0 drops its border: keep the 1 px stroke geometry the
        // widgets reserve, but paint it transparent.
        let border = |c: Color32, a: f32, width: f32| if width > 0.0 { with_alpha(c, a) } else { Color32::TRANSPARENT };
        let hover_fill = alpha("hover-cursor-fill-alpha", 0.08);
        let hover_border = alpha("hover-cursor-border-alpha", 0.25);

        let space2 = (2.0 * scale).round().max(1.0);
        let card_border_width = shell
            .border_width("popups", "border-width")
            .or_else(|| shell.border_width("menu", "border-width"))
            .unwrap_or(space2);
        Self {
            base_font,
            scale,
            rounding: rounding.max(0.0),
            bg: pal.background,
            card_bg: composed("menu", "background", pal.background, 1.0),
            raised_bg: pal.lighter_background,
            fg,
            accent: pal.accent,
            urgent: pal.urgent,
            success: pal.green,
            warning: pal.yellow,
            text_secondary: with_alpha(fg, 0.52),
            text_placeholder: with_alpha(fg, 0.58),
            text_section: darker(fg, 1.4),
            text_disabled: with_alpha(fg, 0.36),
            separator: with_alpha(fg, 0.12),
            pane_divider: with_alpha(fg, 0.28),
            group_divider: with_alpha(fg, 0.20),
            card_border: composed("menu", "border", fg, 1.0),
            popup_border: composed("popups", "border", pal.accent, 1.0),
            card_border_width,
            fill_field: with_alpha(normal, alpha("normal-fill-alpha", 0.04)),
            fill_hover: with_alpha(hover, hover_fill),
            fill_selected: with_alpha(selected, alpha("selected-fill-alpha", 0.18)),
            fill_pressed: with_alpha(pressed, alpha("pressed-fill-alpha", 0.22)),
            fill_text_selection: with_alpha(selection, alpha("selection-fill-alpha", 0.35)),
            border_field: border(normal, alpha("normal-border-alpha", 0.4), normal_border_w),
            border_hover: border(hover, hover_border, hover_border_w),
            list_cursor_bg: composed("menu", "selected-background", fg, 0.08),
            list_cursor_text: shell.color("menu", "selected-text", pal).unwrap_or(pal.accent),
            list_cursor_border: composed("menu", "selected-border", fg, 0.0),
            scrim: composed("menu", "scrim", pal.background, 0.5),
            osd_scrim: Color32::from_rgba_unmultiplied(0, 0, 0, 170),
            osd_text: OSD_TEXT,
            osd_accent: if contrast_ratio(pal.accent, OSD_EFFECTIVE_BG) >= 3.0 { pal.accent } else { OSD_TEXT },
        }
    }

    /// `[font] <token>` override (integer px > 0, × UI scale) or `max(1, round(base × mult))`.
    pub fn font(&self, shell: &ShellConfig, t: TypeToken) -> f32 {
        match shell.f32("font", t.key()).map(f32::trunc).filter(|px| *px > 0.0) {
            Some(px) => (px * self.ui_scale(shell)).round().max(1.0),
            None => (self.base_font * t.mult()).round().max(1.0),
        }
    }

    /// `[spacing] <token>` override (px ≥ 0, × UI scale) or `max(1, round(px × scale))`.
    /// `hairline` is never overridden (Style.qml).
    pub fn space(&self, shell: &ShellConfig, t: SpaceToken) -> f32 {
        let computed = (t.base_px() * self.scale).round().max(1.0);
        if t == SpaceToken::Hairline {
            return computed;
        }
        match shell.f32("spacing", t.key()).filter(|px| *px >= 0.0) {
            Some(px) => (px * self.ui_scale(shell)).round(),
            None => computed,
        }
    }

    /// The UI scale baked into `base_font` (pinned token overrides scale with it too).
    fn ui_scale(&self, shell: &ShellConfig) -> f32 { self.base_font / base_size(shell) }

    pub fn fill(&self, i: Interaction) -> Color32 {
        match i {
            Interaction::Idle => Color32::TRANSPARENT,
            Interaction::Field => self.fill_field,
            Interaction::Hover | Interaction::Focus => self.fill_hover,
            Interaction::Selected => self.fill_selected,
            Interaction::Pressed => self.fill_pressed,
        }
    }

    pub fn border(&self, i: Interaction) -> Option<(f32, Color32)> {
        match i {
            Interaction::Field => Some((1.0, self.border_field)),
            Interaction::Hover | Interaction::Focus => Some((1.0, self.border_hover)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod osd_tests {
    use super::*;

    #[test]
    fn contrast() {
        assert!(contrast_ratio(Color32::WHITE, Color32::BLACK) > 20.0);
        assert!(contrast_ratio(Color32::from_rgb(0x30, 0x30, 0x30), Color32::BLACK) < 3.0);
        // `white` theme accent → falls back; tokyo-night blue stays
        assert!(contrast_ratio(Color32::from_rgb(0x6e, 0x6e, 0x6e), OSD_EFFECTIVE_BG) < 3.0);
        assert!(contrast_ratio(Color32::from_rgb(0x7a, 0xa2, 0xf7), OSD_EFFECTIVE_BG) >= 3.0);
    }

    #[test]
    fn osd_accent_choice() {
        let shell = ShellConfig::default();
        let pal = |accent: &str| Palette::parse(&format!("background = \"#ffffff\"\nforeground = \"#222222\"\naccent = \"{accent}\"\n"), false).unwrap();
        assert_eq!(Tokens::compute(&pal("#6e6e6e"), &shell, 0.0, 1.0).osd_accent, OSD_TEXT);
        assert_eq!(Tokens::compute(&pal("#7aa2f7"), &shell, 0.0, 1.0).osd_accent, Color32::from_rgb(0x7a, 0xa2, 0xf7));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> Palette { Palette::parse("accent = \"#F3B77B\"\nbackground = \"#151E26\"\nforeground = \"#ECE5D8\"\n", false).unwrap() }

    #[test]
    fn with_alpha_replaces_alpha_of_translucent_colours() {
        let c = Color32::from_rgba_unmultiplied(0x26, 0xa2, 0x69, 0xee);
        assert_eq!(with_alpha(c, 1.0), Color32::from_rgb(0x26, 0xa2, 0x69));
        assert_eq!(with_alpha(Color32::from_rgb(10, 20, 30), 0.5), Color32::from_rgba_unmultiplied(10, 20, 30, 128));
        assert_eq!(with_alpha(Color32::WHITE, 7.0).a(), 255, "clamped");
        assert_eq!(darker(Color32::from_rgb(140, 70, 14), 1.4), Color32::from_rgb(100, 50, 10));
    }

    #[test]
    fn composed_keeps_exact_rgb_of_translucent_gradient_stops() {
        let shell = ShellConfig::parse(
            "[hyprland]\nactive-border = \"rgba(26a26911) rgba(2ec27eee) 45deg\"\n[popups]\nborder = \"hyprland.active-border\"\n[menu]\nscrim = \"rgba(10203040)\"\nscrim-alpha = 0.25\n",
            None,
        )
        .unwrap();
        let t = Tokens::compute(&pal(), &shell, 0.0, 1.0);
        assert_eq!(t.popup_border, Color32::from_rgb(0x26, 0xa2, 0x69), "alpha 0x11 → border-alpha default 1.0");
        assert_eq!(t.scrim, with_alpha(Color32::from_rgb(0x10, 0x20, 0x30), 0.25));
    }

    #[test]
    fn controls_section_and_zero_width_borders() {
        let shell = ShellConfig::parse(
            "[style]\nnormal-color = \"#102030\"\nnormal-fill-alpha = 0.5\n[controls]\nhover-cursor-color = accent\nhover-cursor-border-width = 0\nnormal-border-width = 0\n",
            None,
        )
        .unwrap();
        let p = pal();
        let t = Tokens::compute(&p, &shell, 3.0, 1.0);
        assert_eq!(t.fill_field, with_alpha(Color32::from_rgb(0x10, 0x20, 0x30), 0.5), "legacy [style] section");
        assert_eq!(t.fill_hover, with_alpha(p.accent, 0.08));
        assert_eq!(t.fill_pressed, with_alpha(p.accent, 0.22), "pressed defaults to the hover colour");
        assert_eq!(t.border_field, Color32::TRANSPARENT);
        assert_eq!(t.border_hover, Color32::TRANSPARENT);
        assert_eq!(t.rounding, 3.0);
    }

    #[test]
    fn spacing_scale_and_bad_ui_scale() {
        let shell = ShellConfig::parse("[spacing]\nscale-with-font = false\nscale = 1.5\n[font]\nbase-size = 18\n", None).unwrap();
        let t = Tokens::compute(&pal(), &shell, 0.0, 1.0);
        assert_eq!(t.space(&shell, SpaceToken::ControlHeight), 42.0, "scale only (font size ignored)");
        assert_eq!(t.font(&shell, TypeToken::Body), 18.0);
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let t = Tokens::compute(&pal(), &ShellConfig::default(), 0.0, bad);
            assert_eq!(t.base_font, 12.0, "{bad}");
        }
        let shell = ShellConfig::parse("[font]\nbase-size = 0\n", None).unwrap();
        assert_eq!(Tokens::compute(&pal(), &shell, -2.0, 1.0).base_font, 1.0, "1 px floor");
        assert_eq!(Tokens::compute(&pal(), &shell, -2.0, 1.0).rounding, 0.0);
    }

    #[test]
    fn card_border_width_from_popups_or_menu() {
        let p = pal();
        let w = |s: &str| {
            let shell = ShellConfig::parse(s, None).unwrap();
            Tokens::compute(&p, &shell, 0.0, 1.0).card_border_width
        };
        assert_eq!(w(""), 2.0);
        assert_eq!(w("[menu]\nborder-width = 3\n"), 3.0);
        assert_eq!(w("[popups]\nborder-width = \"1 4\"\n[menu]\nborder-width = 3\n"), 4.0);
        assert_eq!(w("[font]\nbase-size = 24\n"), 4.0, "space(2) scales with the font");
    }
}
