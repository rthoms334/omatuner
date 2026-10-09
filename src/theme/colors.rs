//! `colors.toml` palette with Omarchy's fallback cascade, mirroring
//! `/usr/share/omarchy/bin/omarchy-theme-color` step by step (omarchy.md §1.2) so every value
//! equals what Omarchy's own templates and tools resolve. Tests compare every theme in
//! `tests/fixtures/theme` against golden `omarchy-theme-color --all` output.
//!
//! The file is read the way the script reads it (line by line, `key = "value"`, last key wins,
//! inline comments ignored) rather than as strict TOML, so a third-party theme with a duplicate
//! key or a stray line still loads instead of dropping to the fallback palette.
//!
//! Deviations (documented): `accent` = `accent` → `color4` (the shell's `Color.qml`) → `blue` →
//! foreground instead of the shell's grey `#cacccc`; colours a theme leaves undefined fall back
//! to the foreground (or the shell's hard fallbacks) instead of an empty string.

use std::collections::HashMap;

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

/// Omarchy shell hard fallbacks (`shell/Commons/Color.qml`).
const FALLBACK_FOREGROUND: &str = "#cacccc";
const FALLBACK_BACKGROUND: &str = "#101315";
const FALLBACK_URGENT: &str = "#a55555";
const FALLBACK_MUTED: &str = "#707880";

/// Canonical name ← legacy short name.
const LEGACY_PALETTE_ALIAS: [(&str, &str); 8] = [
    ("background", "bg"),
    ("dark_background", "dark_bg"),
    ("darker_background", "darker_bg"),
    ("lighter_background", "lighter_bg"),
    ("foreground", "fg"),
    ("dark_foreground", "dark_fg"),
    ("light_foreground", "light_fg"),
    ("bright_foreground", "bright_fg"),
];

/// Semantic name ← ANSI name.
const LEGACY_ANSI_ALIAS: [(&str, &str); 12] = [
    ("red", "color1"),
    ("green", "color2"),
    ("yellow", "color3"),
    ("blue", "color4"),
    ("magenta", "color5"),
    ("cyan", "color6"),
    ("bright_red", "color9"),
    ("bright_green", "color10"),
    ("bright_yellow", "color11"),
    ("bright_blue", "color12"),
    ("bright_magenta", "color13"),
    ("bright_cyan", "color14"),
];

/// Raw `key → value` strings as `omarchy-theme-color` holds them while resolving.
#[derive(Default)]
struct RawPalette(HashMap<String, String>);

impl RawPalette {
    fn has(&self, k: &str) -> bool { self.0.get(k).is_some_and(|v| !v.is_empty()) }
    fn get(&self, k: &str) -> Option<&str> { self.0.get(k).map(String::as_str).filter(|v| !v.is_empty()) }
    fn set(&mut self, k: &str, v: impl Into<String>) { self.0.insert(k.to_owned(), v.into()); }

    /// `alias_theme_color key fallback`: copy `fallback` into an unset `key`.
    fn alias(&mut self, key: &str, fallback: &str) {
        if !self.has(key)
            && let Some(v) = self.get(fallback)
        {
            let v = v.to_owned();
            self.set(key, v);
        }
    }

    /// `[[ key ]] || key = first set of candidates`.
    fn default_from(&mut self, key: &str, candidates: &[&str]) {
        if self.has(key) {
            return;
        }
        if let Some(v) = candidates.iter().find_map(|c| self.get(c)) {
            let v = v.to_owned();
            self.set(key, v);
        }
    }

    /// `[[ key ]] || key = mix(from, with, t)`. The awk mixer reads an empty/undefined `from` as
    /// `#000000` (an undefined `yellow` gives `bright_yellow = #333333`); we mirror that. Other
    /// non-`#rrggbb` inputs give the script garbage, so they leave `key` unset instead (it then
    /// falls back like any undefined colour).
    fn default_mix(&mut self, key: &str, from: &str, with: Color32, t: f32) {
        if self.has(key) {
            return;
        }
        let from = match self.get(from) {
            None => Some(Color32::BLACK),
            Some(v) => strict_hex6(v),
        };
        if let Some(c) = from {
            self.set(key, to_hex(mix(c, with, t)));
        }
    }

    /// `resolve_theme_colors` minus the reverse aliases (they only add duplicate keys).
    fn resolve(&mut self) {
        for (k, short) in LEGACY_PALETTE_ALIAS {
            self.alias(k, short);
        }
        // ANSI-only themes: background/foreground from color0/color7, then the reverse.
        self.alias("background", "color0");
        self.alias("foreground", "color7");
        if let Some(bg) = self.get("background").map(str::to_owned) {
            self.set("color0", bg);
        }
        if let Some(fg) = self.get("foreground").map(str::to_owned) {
            self.set("color7", fg);
        }
        for (k, ansi) in LEGACY_ANSI_ALIAS {
            self.alias(k, ansi);
        }
        self.alias("magenta", "purple");
        self.alias("bright_magenta", "bright_purple");

        self.default_from("light_foreground", &["color7", "foreground"]);
        self.default_from("bright_foreground", &["color15", "foreground"]);
        self.default_from("lighter_background", &["color0", "background"]);
        self.default_from("dark_foreground", &["color8", "foreground"]);
        self.default_from("muted", &["color8", "dark_foreground"]);
        self.default_from("selection", &["selection_background", "color8", "color0", "background"]);
        self.default_from("selection_foreground", &["bright_foreground"]);
        self.default_from("orange", &["yellow"]);
        self.default_mix("brown", "orange", Color32::BLACK, 0.5);
        self.default_mix("dark_background", "background", Color32::BLACK, 0.25);
        self.default_mix("darker_background", "background", Color32::BLACK, 0.5);
        for c in ["red", "yellow", "green", "cyan", "blue", "magenta"] {
            self.default_mix(&format!("bright_{c}"), c, Color32::WHITE, 0.2);
        }
    }

    /// `resolve_theme_mode`: `mode` → `theme_type` → `light.mode` → luminance of a `#rrggbb`
    /// background (r+g+b > 382) → dark.
    fn mode(&self, light_mode_file: bool) -> Mode {
        match self.get("mode").or_else(|| self.get("theme_type")) {
            Some("light") => return Mode::Light,
            Some(_) => return Mode::Dark,
            None => {}
        }
        if light_mode_file {
            return Mode::Light;
        }
        match self.get("background").and_then(strict_hex6) {
            Some(bg) if bg.r() as u32 + bg.g() as u32 + bg.b() as u32 > 382 => Mode::Light,
            _ => Mode::Dark,
        }
    }

    /// Palette colours are opaque: an `#rrggbbaa` value keeps its RGB and drops the alpha (the
    /// script's templates use the value as an opaque colour too).
    fn color(&self, k: &str) -> Option<Color32> {
        let [r, g, b, _] = self.get(k).and_then(parse_rgba)?;
        Some(Color32::from_rgb(r, g, b))
    }

    fn into_palette(self, mode: Mode, accent: Option<Color32>) -> Palette {
        let hard = |s: &str| parse_color(s).unwrap_or(Color32::GRAY);
        let fg = self.color("foreground").unwrap_or_else(|| hard(FALLBACK_FOREGROUND));
        let bg = self.color("background").unwrap_or_else(|| hard(FALLBACK_BACKGROUND));
        let c = |k: &str| self.color(k).unwrap_or(fg);
        let red = self.color("red").unwrap_or_else(|| hard(FALLBACK_URGENT));
        Palette {
            mode,
            accent: accent.or_else(|| self.color("blue")).unwrap_or(fg),
            urgent: red,
            selection: self.color("selection").unwrap_or(bg),
            selection_foreground: c("selection_foreground"),
            muted: self.color("muted").unwrap_or_else(|| hard(FALLBACK_MUTED)),
            background: bg,
            dark_background: self.color("dark_background").unwrap_or_else(|| mix(bg, Color32::BLACK, 0.25)),
            darker_background: self.color("darker_background").unwrap_or_else(|| mix(bg, Color32::BLACK, 0.5)),
            lighter_background: self.color("lighter_background").unwrap_or(bg),
            foreground: fg,
            dark_foreground: c("dark_foreground"),
            light_foreground: c("light_foreground"),
            bright_foreground: c("bright_foreground"),
            red,
            yellow: c("yellow"),
            orange: c("orange"),
            green: c("green"),
            cyan: c("cyan"),
            blue: c("blue"),
            magenta: c("magenta"),
            brown: c("brown"),
            bright_red: c("bright_red"),
            bright_yellow: c("bright_yellow"),
            bright_green: c("bright_green"),
            bright_cyan: c("bright_cyan"),
            bright_blue: c("bright_blue"),
            bright_magenta: c("bright_magenta"),
        }
    }
}

impl Palette {
    /// Omarchy shell hard fallback (`Commons/Color.qml`) — used when no theme exists.
    pub fn fallback() -> Self {
        let mut raw = RawPalette::default();
        raw.set("foreground", FALLBACK_FOREGROUND);
        raw.set("background", FALLBACK_BACKGROUND);
        raw.set("red", FALLBACK_URGENT);
        raw.set("muted", FALLBACK_MUTED);
        raw.resolve();
        let accent = raw.color("foreground");
        raw.into_palette(Mode::Dark, accent)
    }

    /// Parse `colors.toml` text. `light_mode_file` = existence of `light.mode` beside it.
    ///
    /// `Err` only when the text defines neither a foreground nor a background under any name
    /// (empty or foreign file) — the live-reload path then keeps the current theme.
    pub fn parse(toml_text: &str, light_mode_file: bool) -> Result<Self, String> {
        let mut raw = read_colors_file(toml_text);
        if !["foreground", "fg", "color7", "background", "bg", "color0"].iter().any(|k| raw.has(k)) {
            return Err("no foreground or background colour defined".into());
        }
        // Color.qml: the explicit accent wins; color4 only as written in the file.
        let accent = raw.color("accent").or_else(|| raw.color("color4"));
        raw.resolve();
        let mode = raw.mode(light_mode_file);
        Ok(raw.into_palette(mode, accent))
    }
}

/// `parse_colors_file`: `key = value` lines; the key loses quotes and spaces; a quoted value is
/// the text between the first pair of quotes (inline comments ignored), an unquoted one is
/// trimmed. Keys and values outside the script's character sets are skipped.
fn read_colors_file(text: &str) -> RawPalette {
    let mut raw = RawPalette::default();
    for line in text.lines() {
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        let key: String = key.chars().filter(|c| !matches!(c, '"' | '\'' | ' ')).collect();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let value = match value.find(['"', '\'']) {
            Some(i) => {
                let rest = &value[i + 1..];
                &rest[..rest.find(['"', '\'']).unwrap_or(rest.len())]
            }
            None => value.trim(),
        };
        let allowed = |c: char| c.is_ascii_alphanumeric() || "#(),._+/% -".contains(c);
        if !value.chars().all(allowed) {
            continue;
        }
        raw.set(&key, value);
    }
    raw
}

/// `^#[0-9A-Fa-f]{6}$` only.
fn strict_hex6(s: &str) -> Option<Color32> {
    let h = s.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some(Color32::from_rgb(p(0)?, p(2)?, p(4)?))
}

fn to_hex(c: Color32) -> String { format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b()) }

pub const fn rgb(r: u8, g: u8, b: u8) -> Color32 { Color32::from_rgb(r, g, b) }

/// Linear mix in sRGB space (what Omarchy's templates do): `t` = weight of `b`. Rounds half up
/// like the awk mixer (`int(x + 0.5)`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0) as f64;
    let l = |x: u8, y: u8| (x as f64 * (1.0 - t) + y as f64 * t + 0.5).floor() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Omarchy's `canonicalColor` (shell `BorderGeometry.js`) on the first non-angle token of a
/// value: `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(rrggbb)`, `rgba(rrggbbaa)`, `rgb(r,g,b)`,
/// `rgba(r,g,b,a)` (a = 0..1), `0xaarrggbb`. A Hyprland gradient
/// (`rgba(..) rgba(..) 45deg`) yields its first stop.
pub fn parse_color(s: &str) -> Option<Color32> {
    let [r, g, b, a] = parse_rgba(s)?;
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

/// [`parse_color`] as UNMULTIPLIED `[r, g, b, a]`. `Color32` stores premultiplied channels, so
/// callers that replace the alpha (`composed` in tokens) need the original RGB losslessly.
pub(super) fn parse_rgba(s: &str) -> Option<[u8; 4]> {
    let token = s.split_whitespace().find(|t| !is_angle(t))?;
    let hex = |h: &str| -> Option<(u8, u8, u8, u8)> {
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        match h.len() {
            3 => {
                let d = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
                Some((d(0)?, d(1)?, d(2)?, 255))
            }
            6 => Some((p(0)?, p(2)?, p(4)?, 255)),
            8 => Some((p(0)?, p(2)?, p(4)?, p(6)?)),
            _ => None,
        }
    };
    let lower = token.to_ascii_lowercase();
    let (r, g, b, a) = if let Some(h) = lower.strip_prefix('#') {
        hex(h)?
    } else if let Some(h) = lower.strip_prefix("0x") {
        // Hyprland's legacy 0xAARRGGBB
        if h.len() != 8 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let (r, g, b, _) = hex(&h[2..])?;
        (r, g, b, u8::from_str_radix(&h[..2], 16).ok()?)
    } else if let Some(inner) = lower.strip_prefix("rgba(").and_then(|s| s.strip_suffix(')')) {
        if inner.contains(',') {
            let parts: Vec<&str> = inner.split(',').collect();
            let [r, g, b, a] = parts.as_slice() else { return None };
            let alpha: f32 = a.trim().parse().ok()?;
            (dec(r)?, dec(g)?, dec(b)?, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
        } else if inner.len() == 8 {
            hex(inner)?
        } else {
            return None;
        }
    } else {
        let inner = lower.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')'))?;
        if inner.contains(',') {
            let parts: Vec<&str> = inner.split(',').collect();
            let [r, g, b] = parts.as_slice() else { return None };
            (dec(r)?, dec(g)?, dec(b)?, 255)
        } else if inner.len() == 6 {
            hex(inner)?
        } else {
            return None;
        }
    };
    Some([r, g, b, a])
}

/// Decimal channel, clamped to 255 (like the template renderer).
fn dec(s: &str) -> Option<u8> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.parse::<u32>().unwrap_or(u32::MAX).min(255) as u8)
}

/// Gradient angle token (`45deg`, `-90.5deg`).
pub(super) fn is_angle(t: &str) -> bool {
    t.strip_suffix("deg")
        .map(|n| n.strip_prefix('-').unwrap_or(n))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit() || b == b'.') && n.parse::<f64>().is_ok())
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
        assert_eq!(parse_color("45deg #112233"), Some(rgb(0x11, 0x22, 0x33)));
    }

    #[test]
    fn canonical_color_forms() {
        assert_eq!(parse_color("#abc"), Some(rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_color("#A1B2C3"), Some(rgb(0xa1, 0xb2, 0xc3)));
        assert_eq!(parse_color("#a1b2c380"), Some(Color32::from_rgba_unmultiplied(0xa1, 0xb2, 0xc3, 0x80)));
        assert_eq!(parse_color("rgb(1e1e1e)"), Some(rgb(0x1e, 0x1e, 0x1e)));
        assert_eq!(parse_color("RGBA(584e51aa)"), Some(Color32::from_rgba_unmultiplied(0x58, 0x4e, 0x51, 0xaa)));
        assert_eq!(parse_color("rgb(10,20,300)"), Some(rgb(10, 20, 255)));
        assert_eq!(parse_color("rgba(10,20,30,0.5)"), Some(Color32::from_rgba_unmultiplied(10, 20, 30, 128)));
        assert_eq!(parse_color("0xee33ccff"), Some(Color32::from_rgba_unmultiplied(0x33, 0xcc, 0xff, 0xee)));
        for bad in ["", "accent", "beaded", "#12", "#gggggg", "rgb(12345)", "rgba(1,2,3)", "0x123", "45deg"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn luminance_detects_light() {
        let p = Palette::parse("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\n", false).unwrap();
        assert_eq!(p.mode, Mode::Light);
        let p = Palette::parse("background = \"#101010\"\nforeground = \"#eeeeee\"\n", true).unwrap();
        assert_eq!(p.mode, Mode::Light, "light.mode file wins over luminance");
        let p = Palette::parse("theme_type = \"dark\"\nbackground = \"#ffffff\"\n", true).unwrap();
        assert_eq!(p.mode, Mode::Dark, "legacy theme_type wins over light.mode");
        let p = Palette::parse("background = \"rgb(ffffff)\"\n", false).unwrap();
        assert_eq!(p.mode, Mode::Dark, "luminance only for #rrggbb backgrounds");
    }

    #[test]
    fn lenient_like_the_script() {
        let text = "# comment\n[section]\nforeground = '#111111' # inline\nforeground = \"#222222\"\nbackground=#333333\nbad key = \"#444444\"\nred = \"#55;555\"\n";
        let p = Palette::parse(text, false).unwrap();
        assert_eq!(p.foreground, rgb(0x22, 0x22, 0x22), "last duplicate wins");
        assert_eq!(p.background, rgb(0x33, 0x33, 0x33), "unquoted value");
        assert_eq!(p.red, rgb(0xa5, 0x55, 0x55), "rejected value → hard fallback");
    }

    #[test]
    fn empty_file_is_an_error() {
        assert!(Palette::parse("", false).is_err());
        assert!(Palette::parse("# nothing\naccent = \"#ffffff\"\n", false).is_err());
    }

    #[test]
    fn fallback_matches_color_qml() {
        let p = Palette::fallback();
        assert_eq!(p.foreground, rgb(0xca, 0xcc, 0xcc));
        assert_eq!(p.background, rgb(0x10, 0x13, 0x15));
        assert_eq!(p.accent, rgb(0xca, 0xcc, 0xcc));
        assert_eq!(p.urgent, rgb(0xa5, 0x55, 0x55));
        assert_eq!(p.muted, rgb(0x70, 0x78, 0x80));
        assert_eq!(p.mode, Mode::Dark);
    }

    #[test]
    fn mix_rounds_half_up() {
        // awk: int(0x15 * 0.75 + 0.5) = int(16.25) = 16
        assert_eq!(mix(rgb(0x15, 0x01, 0xff), Color32::BLACK, 0.25), rgb(16, 1, 191));
        assert_eq!(mix(rgb(0x01, 0x03, 0x05), Color32::BLACK, 0.5), rgb(1, 2, 3));
    }
}
