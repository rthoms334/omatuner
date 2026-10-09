//! Derived design tokens (Omarchy `Style.qml` type scale + spacing, state fills). Skeleton
//! implements the formulas (omarchy.md §2, §5); WP-01 owns tests.
//!
//! * `font(t)  = override[t] ?: round(base × mult[t])`, base = `[font] base-size` (default 12)
//! * `space(t) = override[t] ?: max(1, round(px[t] × scale))`,
//!   `scale = spacing.scale × (scale-with-font ? base/12 : 1)`
//! * All state tints are FOREGROUND at an alpha (works in light themes).

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
    pub card_border: Color32,     // menu.border (fg) — 2 px
    pub popup_border: Color32,    // popups.border (accent) — 2 px
    pub card_border_width: f32,
    // state fills (fg @ alpha)
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
    pub list_cursor_border: Color32, // fg @ 0.25 (width 0 by default)
    pub scrim: Color32,           // bg @ 0.5
    /// Fixed dark scrim for video OSD regardless of theme mode (FIXED DECISION 6).
    pub osd_scrim: Color32,
    pub osd_text: Color32,
    /// Accent used ON the fixed dark OSD scrim (progress, seek bar, focus): the theme accent
    /// when its contrast against the scrim composited over mid-grey video (`#2b2b2b`) is
    /// ≥ 3:1, else near-white `#f2f2f2` (e.g. the `white` theme's `#6e6e6e` accent, or
    /// catppuccin-latte's dark blue).
    pub osd_accent: Color32,
}

pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Qt `darker(c, factor)`: HSV value ÷ factor.
pub fn darker(c: Color32, factor: f32) -> Color32 {
    let f = 1.0 / factor.max(1e-3);
    Color32::from_rgb((c.r() as f32 * f) as u8, (c.g() as f32 * f) as u8, (c.b() as f32 * f) as u8)
}

/// The fixed OSD scrim (`rgba(0,0,0,170)`) over mid-grey (#808080) video.
pub const OSD_EFFECTIVE_BG: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x2b);

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

    pub fn compute(pal: &Palette, shell: &ShellConfig, rounding: f32, ui_scale: f32) -> Self {
        let fg = pal.foreground;
        let a = |s: &str, k: &str, d: f32| shell.f32(s, k).unwrap_or(d);
        let base_font = shell.f32("font", "base-size").unwrap_or(12.0).max(1.0) * ui_scale;
        let scale_with_font = shell.bool("spacing", "scale-with-font").unwrap_or(true);
        let scale = a("spacing", "scale", 1.0) * if scale_with_font { base_font / 12.0 } else { ui_scale };
        let menu_sel = shell.color("menu", "selected-background", pal).unwrap_or(fg);
        Self {
            base_font,
            scale,
            rounding,
            bg: pal.background,
            card_bg: shell.color("menu", "background", pal).unwrap_or(pal.background),
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
            card_border: with_alpha(shell.color("menu", "border", pal).unwrap_or(fg), a("menu", "border-alpha", 1.0)),
            popup_border: with_alpha(shell.color("popups", "border", pal).unwrap_or(pal.accent), a("popups", "border-alpha", 1.0)),
            card_border_width: 2.0,
            fill_field: with_alpha(fg, a("controls", "normal-fill-alpha", 0.04)),
            fill_hover: with_alpha(fg, a("controls", "hover-cursor-fill-alpha", 0.08)),
            fill_selected: with_alpha(fg, a("controls", "selected-fill-alpha", 0.18)),
            fill_pressed: with_alpha(fg, a("controls", "pressed-fill-alpha", 0.22)),
            fill_text_selection: with_alpha(fg, a("controls", "selection-fill-alpha", 0.35)),
            border_field: with_alpha(fg, a("controls", "normal-border-alpha", 0.4)),
            border_hover: with_alpha(fg, a("controls", "hover-cursor-border-alpha", 0.25)),
            list_cursor_bg: with_alpha(menu_sel, a("menu", "selected-background-alpha", 0.08)),
            list_cursor_text: shell.color("menu", "selected-text", pal).unwrap_or(pal.accent),
            list_cursor_border: with_alpha(fg, a("menu", "selected-border-alpha", 0.25)),
            scrim: with_alpha(shell.color("menu", "scrim", pal).unwrap_or(pal.background), a("menu", "scrim-alpha", 0.5)),
            osd_scrim: Color32::from_rgba_unmultiplied(0, 0, 0, 170),
            osd_text: Color32::from_rgb(0xf2, 0xf2, 0xf2),
            osd_accent: if contrast_ratio(pal.accent, OSD_EFFECTIVE_BG) >= 3.0 { pal.accent } else { Color32::from_rgb(0xf2, 0xf2, 0xf2) },
        }
    }

    pub fn font(&self, shell: &ShellConfig, t: TypeToken) -> f32 {
        shell.f32("font", t.key()).unwrap_or_else(|| (self.base_font * t.mult()).round())
    }

    pub fn space(&self, shell: &ShellConfig, t: SpaceToken) -> f32 {
        shell.f32("spacing", t.key()).unwrap_or_else(|| (t.base_px() * self.scale).round().max(1.0))
    }

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
}
