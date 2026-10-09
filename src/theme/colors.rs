//! `colors.toml` palette with Omarchy's fallback cascade (mirrors
//! `/usr/share/omarchy/bin/omarchy-theme-color`; omarchy.md §1.2). Skeleton implements it;
//! WP-01 owns tests against every theme in /usr/share/omarchy/themes.

use eframe::egui::Color32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub mode: Mode,
    pub accent: Color32,
    pub urgent: Color32,
    pub selection: Color32,
    pub selection_foreground: Color32,
    pub muted: Color32,
    pub background: Color32,
    pub dark_background: Color32,
    pub darker_background: Color32,
    pub lighter_background: Color32,
    pub foreground: Color32,
    pub dark_foreground: Color32,
    pub light_foreground: Color32,
    pub bright_foreground: Color32,
    pub red: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub green: Color32,
    pub cyan: Color32,
    pub blue: Color32,
    pub magenta: Color32,
    pub brown: Color32,
    pub bright_red: Color32,
    pub bright_yellow: Color32,
    pub bright_green: Color32,
    pub bright_cyan: Color32,
    pub bright_blue: Color32,
    pub bright_magenta: Color32,
}

impl Palette {
    /// Omarchy shell hard fallback (`Commons/Color.qml`) — used when no theme exists.
    pub fn fallback() -> Self {
        let fg = rgb(0xca, 0xcc, 0xcc);
        let bg = rgb(0x10, 0x13, 0x15);
        Self::derive(Mode::Dark, fg, bg, fg, rgb(0xa5, 0x55, 0x55), &|_| None)
    }

    fn derive(mode: Mode, fg: Color32, bg: Color32, accent: Color32, red: Color32, get: &dyn Fn(&str) -> Option<Color32>) -> Self {
        let black = Color32::BLACK;
        let white = Color32::WHITE;
        let g = |k: &str, d: Color32| get(k).unwrap_or(d);
        let yellow = g("yellow", g("color3", fg));
        let green = g("green", g("color2", fg));
        let blue = g("blue", g("color4", accent));
        let magenta = g("magenta", g("purple", g("color5", fg)));
        let cyan = g("cyan", g("color6", fg));
        let orange = g("orange", yellow);
        let muted = g("muted", g("color8", g("dark_foreground", g("dark_fg", fg))));
        Self {
            mode,
            accent,
            urgent: red,
            selection: g("selection", g("selection_background", g("color8", bg))),
            selection_foreground: g("selection_foreground", g("bright_foreground", g("bright_fg", fg))),
            muted,
            background: bg,
            dark_background: g("dark_background", g("dark_bg", mix(bg, black, 0.25))),
            darker_background: g("darker_background", g("darker_bg", mix(bg, black, 0.5))),
            lighter_background: g("lighter_background", g("lighter_bg", g("color0", bg))),
            foreground: fg,
            dark_foreground: g("dark_foreground", g("dark_fg", g("color8", fg))),
            light_foreground: g("light_foreground", g("light_fg", fg)),
            bright_foreground: g("bright_foreground", g("bright_fg", fg)),
            red,
            yellow,
            orange,
            green,
            cyan,
            blue,
            magenta,
            brown: g("brown", mix(orange, black, 0.5)),
            bright_red: g("bright_red", g("color9", mix(red, white, 0.2))),
            bright_yellow: g("bright_yellow", g("color11", mix(yellow, white, 0.2))),
            bright_green: g("bright_green", g("color10", mix(green, white, 0.2))),
            bright_cyan: g("bright_cyan", g("color14", mix(cyan, white, 0.2))),
            bright_blue: g("bright_blue", g("color12", mix(blue, white, 0.2))),
            bright_magenta: g("bright_magenta", g("color13", mix(magenta, white, 0.2))),
        }
    }

    /// Parse `colors.toml` text. `light_mode_file` = existence of `light.mode` beside it.
    pub fn parse(toml_text: &str, light_mode_file: bool) -> Result<Self, String> {
        let table: toml::Table = toml_text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let get = |k: &str| table.get(k).and_then(|v| v.as_str()).and_then(parse_color);
        let fg = get("foreground").or_else(|| get("fg")).or_else(|| get("color7")).unwrap_or(rgb(0xca, 0xcc, 0xcc));
        let bg = get("background").or_else(|| get("bg")).or_else(|| get("color0")).unwrap_or(rgb(0x10, 0x13, 0x15));
        let accent = get("accent").or_else(|| get("color4")).unwrap_or(fg);
        let red = get("red").or_else(|| get("color1")).unwrap_or(rgb(0xa5, 0x55, 0x55));
        let mode = match table.get("mode").or_else(|| table.get("theme_type")).and_then(|v| v.as_str()) {
            Some("light") => Mode::Light,
            Some("dark") => Mode::Dark,
            _ if light_mode_file => Mode::Light,
            _ => {
                if bg.r() as u32 + bg.g() as u32 + bg.b() as u32 > 382 { Mode::Light } else { Mode::Dark }
            }
        };
        Ok(Self::derive(mode, fg, bg, accent, red, &get))
    }
}

pub const fn rgb(r: u8, g: u8, b: u8) -> Color32 { Color32::from_rgb(r, g, b) }

/// Linear mix in sRGB space (what Omarchy's templates do): `t` = weight of `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(rrggbb)`, `rgba(rrggbbaa)`, or a Hyprland gradient
/// (`rgba(..) rgba(..) 45deg` → first stop).
pub fn parse_color(s: &str) -> Option<Color32> {
    let s = s.trim();
    let first = s.split_whitespace().next()?;
    let hex = if let Some(h) = first.strip_prefix('#') {
        h
    } else if let Some(inner) = first.strip_prefix("rgba(").or_else(|| first.strip_prefix("rgb(")) {
        inner.strip_suffix(')')?
    } else if first.len() == 6 || first.len() == 8 {
        first
    } else {
        return None;
    };
    let p = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
            Some(Color32::from_rgb(d(0)?, d(1)?, d(2)?))
        }
        6 => Some(Color32::from_rgb(p(0)?, p(2)?, p(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(p(0)?, p(2)?, p(4)?, p(6)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_liftoff_subset() {
        let p = Palette::parse("mode = \"dark\"\naccent = \"#F3B77B\"\nbackground = \"#151E26\"\nforeground = \"#ECE5D8\"\n", false).unwrap();
        assert_eq!(p.accent, rgb(0xF3, 0xB7, 0x7B));
        assert_eq!(p.mode, Mode::Dark);
        assert_eq!(p.dark_background, mix(rgb(0x15, 0x1E, 0x26), Color32::BLACK, 0.25));
    }

    #[test]
    fn gradient_takes_first_stop() {
        assert_eq!(parse_color("rgba(26a269ee) rgba(2ec27eee) 45deg"), Some(Color32::from_rgba_unmultiplied(0x26, 0xa2, 0x69, 0xee)));
    }

    #[test]
    fn luminance_detects_light() {
        let p = Palette::parse("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\n", false).unwrap();
        assert_eq!(p.mode, Mode::Light);
    }
}
