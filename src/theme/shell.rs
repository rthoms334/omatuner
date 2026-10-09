//! `shell.toml` (+ `shell.<section>.toml` section files, + `~/.config/omarchy/shell.toml`
//! override) with reference resolution (`"hyprland.active-border"`), roles (`foreground`,
//! `accent`, `urgent`, `muted`, `background`, `transparent`), gradients (first stop), alphas and
//! CSS-style border widths.
//!
//! Mirrors the Omarchy shell (`Commons/Color.qml` `parseShell` / `flatColor`, `Style.qml`):
//! values are read line by line into a flat `section.key → raw string` map rather than as strict
//! TOML, so a theme file the shell accepts never fails here either. Cascade (DESIGN §16.1):
//! theme `shell.toml` → each `shell.<section>.toml` REPLACES its whole section (what
//! `omarchy-theme-set-templates` splices in; idempotent on the already-spliced live file) →
//! the user override, merged LAST key by key.

use std::collections::HashMap;

use eframe::egui::Color32;

use super::colors::{Palette, is_angle, parse_rgba};

/// Maximum `section.key` reference chain (guards against cycles).
const MAX_REF_DEPTH: u8 = 8;

/// Merged shell configuration (theme file with user override applied key by key).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellConfig {
    /// `"section.key"` → raw value (quotes stripped, numbers kept as text).
    values: HashMap<String, String>,
}

impl ShellConfig {
    /// Parse theme `shell.toml` text and merge `user` (override) on top, key by key.
    /// Lenient like the shell: unparsable lines are skipped, so this never fails today.
    pub fn parse(theme: &str, user: Option<&str>) -> Result<Self, String> {
        let mut values = HashMap::new();
        parse_lines(theme, None, &mut values);
        let mut cfg = Self { values };
        if let Some(u) = user {
            cfg.merge_override(u)?;
        }
        Ok(cfg)
    }

    /// Merge a whole override file on top (key by key) — the user's
    /// `~/.config/omarchy/shell.toml`, applied LAST (after section files).
    pub fn merge_override(&mut self, text: &str) -> Result<(), String> {
        parse_lines(text, None, &mut self.values);
        Ok(())
    }

    /// Apply a `shell.<section>.toml` file: its keys REPLACE that whole section (a leading
    /// `[section]` header in the file is optional, other headers start their own sections —
    /// exactly what pasting it into `shell.toml` does).
    pub fn merge_section(&mut self, section: &str, text: &str) -> Result<(), String> {
        if section.is_empty() || !section.chars().all(is_name_char) {
            return Err(format!("invalid section name {section:?}"));
        }
        let prefix = format!("{section}.");
        self.values.retain(|k, _| !k.starts_with(&prefix));
        parse_lines(text, Some(section), &mut self.values);
        Ok(())
    }

    fn raw(&self, section: &str, key: &str) -> Option<&str> {
        self.values.get(&format!("{section}.{key}")).map(|v| v.trim()).filter(|v| !v.is_empty())
    }

    /// Numeric value (`parseFloat` semantics: a leading number, e.g. `"2 4"` → 2).
    pub fn f32(&self, section: &str, key: &str) -> Option<f32> {
        let v = self.raw(section, key)?;
        let end = v
            .char_indices()
            .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '-' || c == '+'))))
            .map_or(v.len(), |(i, _)| i);
        v[..end].parse::<f32>().ok().filter(|f| f.is_finite())
    }

    /// `true/1/yes/on` / `false/0/no/off` (Style.qml `boolToken`).
    pub fn bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.raw(section, key)?.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    /// Resolve a colour value: `section.key` reference / role / hex / rgb(a) / gradient (first
    /// stop). `None` when the key is missing or the value is not a colour.
    pub fn color(&self, section: &str, key: &str, pal: &Palette) -> Option<Color32> {
        let [r, g, b, a] = self.color_rgba(section, key, pal)?;
        Some(Color32::from_rgba_unmultiplied(r, g, b, a))
    }

    /// [`Self::color`] as unmultiplied `[r, g, b, a]`, for callers that replace the alpha
    /// (`Color32` is premultiplied, so round-tripping a translucent gradient stop through it
    /// would darken the RGB).
    pub(super) fn color_rgba(&self, section: &str, key: &str, pal: &Palette) -> Option<[u8; 4]> {
        let v = self.raw(section, key)?;
        self.resolve_color(v, pal, 0)
    }

    /// `flatColor`: first non-angle token; a token naming another key is followed; then roles;
    /// then a literal colour.
    fn resolve_color(&self, v: &str, pal: &Palette, depth: u8) -> Option<[u8; 4]> {
        let token = v.split_whitespace().find(|t| !is_angle(t)).unwrap_or(v.trim());
        let role = token.to_ascii_lowercase();
        if let Some(target) = self.values.get(&role)
            && target.trim() != token
        {
            return if depth < MAX_REF_DEPTH { self.resolve_color(target, pal, depth + 1) } else { None };
        }
        let opaque = |c: Color32| Some(c.to_srgba_unmultiplied());
        match role.as_str() {
            "foreground" | "text" => opaque(pal.foreground),
            "accent" => opaque(pal.accent),
            "urgent" => opaque(pal.urgent),
            "muted" => opaque(pal.muted),
            "background" => opaque(pal.background),
            "transparent" => Some([0, 0, 0, 0]),
            _ => parse_rgba(v),
        }
    }

    /// CSS-style border width of `section.key` (`N`, `"Y X"`, `"T X B"`, `"T R B L"`, plus
    /// `<key>-top/-right/-bottom/-left` overrides), reduced to one uniform width (the widest
    /// side; egui strokes are uniform). `None` when nothing is set.
    pub(super) fn border_width(&self, section: &str, key: &str) -> Option<f32> {
        let nums: Vec<f32> = self
            .raw(section, key)
            .map(|v| v.split_whitespace().filter_map(|n| n.parse::<f32>().ok()).take(4).map(|n| if n.is_finite() && n > 0.0 { n } else { 0.0 }).collect())
            .unwrap_or_default();
        let mut sides: [Option<f32>; 4] = match nums.as_slice() {
            [] => [None; 4],
            [a] => [Some(*a); 4],
            [y, x] => [Some(*y), Some(*x), Some(*y), Some(*x)],
            [t, x, b] => [Some(*t), Some(*x), Some(*b), Some(*x)],
            [t, r, b, l, ..] => [Some(*t), Some(*r), Some(*b), Some(*l)],
        };
        for (i, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
            if let Some(w) = self.f32(section, &format!("{key}-{side}")) {
                sides[i] = Some(w.max(0.0));
            }
        }
        sides.iter().flatten().copied().reduce(f32::max)
    }
}

fn is_name_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '_' || c == '-' }

/// `parseShell`: `[section]` headers; `key = "string"` / `'string'`, numbers, width lists
/// (`1 2`), bare words (`foreground`, `true`); inline `#` comments; keys outside a section are
/// ignored. `section` presets the current section (section files).
fn parse_lines(text: &str, section: Option<&str>, out: &mut HashMap<String, String>) {
    let mut current = section.map(str::to_owned);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            if let Some((name, tail)) = rest.split_once(']')
                && !name.is_empty()
                && name.chars().all(is_name_char)
                && (tail.trim().is_empty() || tail.trim_start().starts_with('#'))
            {
                current = Some(name.to_owned());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(is_name_char) {
            continue;
        }
        let Some(value) = parse_value(value.trim()) else { continue };
        if let Some(s) = &current {
            out.insert(format!("{s}.{key}"), value);
        }
    }
}

/// The value grammar of `parseShell` (string | number | width list | bare word, each with an
/// optional trailing `# comment`).
fn parse_value(v: &str) -> Option<String> {
    let rest_ok = |rest: &str| {
        let r = rest.trim_start();
        r.is_empty() || r.starts_with('#')
    };
    if v.starts_with(['"', '\'']) {
        let inner = &v[1..];
        let end = inner.find(['"', '\''])?;
        return (end > 0 && rest_ok(&inner[end + 1..])).then(|| inner[..end].to_owned());
    }
    let body = v.split_once('#').map_or(v, |(b, _)| b).trim();
    if body.is_empty() {
        return None;
    }
    let is_num = |t: &str| {
        let t = t.strip_prefix('-').unwrap_or(t);
        !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || b == b'.') && t.parse::<f64>().is_ok()
    };
    let parts: Vec<&str> = body.split_whitespace().collect();
    if parts.len() <= 4 && parts.iter().all(|p| is_num(p)) {
        return Some(parts.join(" "));
    }
    let word = parts.first().filter(|_| parts.len() == 1)?;
    (word.starts_with(|c: char| c.is_ascii_alphabetic()) && word.chars().all(is_name_char)).then(|| (*word).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::colors::rgb;

    fn pal() -> Palette { Palette::parse("accent = \"#F3B77B\"\nbackground = \"#151E26\"\nforeground = \"#ECE5D8\"\nred = \"#EF8D80\"\nmuted = \"#88979F\"\n", false).unwrap() }

    #[test]
    fn value_grammar() {
        let s = ShellConfig::parse(
            "top = \"x\"\n[a]\ns = \"#112233\" # c\nq = 'accent'\nn = 0.25\ni = -3\nw = 1 2 3\nb = true # yes\nbad = \"open\nalso = 1 2 3 4 5\n[b] # hdr\nk = \"v\"\n",
            None,
        )
        .unwrap();
        assert_eq!(s.raw("a", "s"), Some("#112233"));
        assert_eq!(s.raw("a", "q"), Some("accent"));
        assert_eq!(s.f32("a", "n"), Some(0.25));
        assert_eq!(s.f32("a", "i"), Some(-3.0));
        assert_eq!(s.raw("a", "w"), Some("1 2 3"));
        assert_eq!(s.f32("a", "w"), Some(1.0));
        assert_eq!(s.bool("a", "b"), Some(true));
        assert_eq!(s.raw("a", "bad"), None);
        assert_eq!(s.raw("a", "also"), None);
        assert_eq!(s.raw("b", "k"), Some("v"));
        assert!(!s.values.contains_key(".top") && !s.values.contains_key("top"), "keys before a section are ignored");
    }

    #[test]
    fn colors_roles_refs_gradients() {
        let s = ShellConfig::parse(
            "[hyprland]\nactive-border = \"rgba(26a269ee) rgba(2ec27eee) 45deg\"\nfg = \"hyprland.active-border\"\n[menu]\nborder = \"hyprland.fg\"\nsel = accent\ntext = \"text\"\nloop = \"menu.loop2\"\nloop2 = \"menu.loop\"\nnone = \"transparent\"\nshort = \"#abc\"\nbad = \"nonsense\"\n",
            None,
        )
        .unwrap();
        let p = pal();
        let first_stop = Color32::from_rgba_unmultiplied(0x26, 0xa2, 0x69, 0xee);
        assert_eq!(s.color("hyprland", "active-border", &p), Some(first_stop));
        assert_eq!(s.color("menu", "border", &p), Some(first_stop), "two-level reference to a gradient");
        assert_eq!(s.color("menu", "sel", &p), Some(p.accent));
        assert_eq!(s.color("menu", "text", &p), Some(p.foreground));
        assert_eq!(s.color("menu", "loop", &p), None, "reference cycles terminate");
        assert_eq!(s.color("menu", "none", &p), Some(Color32::TRANSPARENT));
        assert_eq!(s.color("menu", "short", &p), Some(rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(s.color("menu", "bad", &p), None);
        assert_eq!(s.color("menu", "missing", &p), None);
    }

    #[test]
    fn section_file_replaces_section_then_user_wins() {
        let mut s = ShellConfig::parse("[lock]\na = 1\nb = 2\n[menu]\nc = 3\n", None).unwrap();
        s.merge_section("lock", "[lock]\na = 10\n").unwrap();
        assert_eq!(s.f32("lock", "a"), Some(10.0));
        assert_eq!(s.f32("lock", "b"), None, "section files replace the whole section");
        assert_eq!(s.f32("menu", "c"), Some(3.0));
        s.merge_section("menu", "d = 4\n").unwrap();
        assert_eq!((s.f32("menu", "c"), s.f32("menu", "d")), (None, Some(4.0)), "header optional");
        s.merge_override("[lock]\nb = 20\n[font]\nbase-size = 16\n").unwrap();
        assert_eq!((s.f32("lock", "a"), s.f32("lock", "b")), (Some(10.0), Some(20.0)), "override merges key by key");
        assert_eq!(s.f32("font", "base-size"), Some(16.0));
        assert!(s.merge_section("../x", "").is_err());
    }

    #[test]
    fn parse_with_user_override() {
        let s = ShellConfig::parse("[font]\nbase-size = 12\nbody = 13\n", Some("[font]\nbase-size = 14\n")).unwrap();
        assert_eq!(s.f32("font", "base-size"), Some(14.0));
        assert_eq!(s.f32("font", "body"), Some(13.0));
    }

    #[test]
    fn border_widths() {
        let s = ShellConfig::parse(
            "[a]\none = 2\ntwo = \"1 3\"\nthree = 1 2 4\nfour = \"1 2 3 5\"\nneg = -2\nside = 1\nside-left = 6\nonly-top = 0\nonly-top-top = 3\n",
            None,
        )
        .unwrap();
        assert_eq!(s.border_width("a", "one"), Some(2.0));
        assert_eq!(s.border_width("a", "two"), Some(3.0));
        assert_eq!(s.border_width("a", "three"), Some(4.0));
        assert_eq!(s.border_width("a", "four"), Some(5.0));
        assert_eq!(s.border_width("a", "neg"), Some(0.0));
        assert_eq!(s.border_width("a", "side"), Some(6.0));
        assert_eq!(s.border_width("a", "only-top"), Some(3.0));
        assert_eq!(s.border_width("a", "missing"), None);
    }

    #[test]
    fn bools() {
        let s = ShellConfig::parse("[s]\na = \"yes\"\nb = off\nc = 0\nd = maybe\n", None).unwrap();
        assert_eq!((s.bool("s", "a"), s.bool("s", "b"), s.bool("s", "c"), s.bool("s", "d")), (Some(true), Some(false), Some(false), None));
    }
}
