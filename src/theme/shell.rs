//! `shell.toml` (+ `~/.config/omarchy/shell.toml` override, + `shell.<section>.toml` section
//! files) with reference resolution (`"hyprland.active-border"`), roles (`foreground`, `accent`,
//! `urgent`, `muted`, `background`, `transparent`), gradients (first stop) and alphas.
//! Skeleton implements lookup/merge; WP-01 owns section files, border-width parsing and tests.

use eframe::egui::Color32;

use super::colors::{Palette, parse_color};

/// Merged shell configuration (theme file with user override applied key by key).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellConfig {
    table: toml::Table,
}

impl ShellConfig {
    /// Parse theme `shell.toml` text and merge `user` (override) on top, key by key.
    pub fn parse(theme: &str, user: Option<&str>) -> Result<Self, String> {
        let mut table: toml::Table = if theme.trim().is_empty() {
            toml::Table::new()
        } else {
            theme.parse().map_err(|e: toml::de::Error| e.to_string())?
        };
        if let Some(u) = user {
            let ut: toml::Table = u.parse().map_err(|e: toml::de::Error| e.to_string())?;
            merge(&mut table, ut);
        }
        Ok(Self { table })
    }

    /// Merge a whole override file on top (key by key) — the user's
    /// `~/.config/omarchy/shell.toml`, applied LAST (after section files).
    pub fn merge_override(&mut self, text: &str) -> Result<(), String> {
        let t: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        merge(&mut self.table, t);
        Ok(())
    }

    /// Merge a `shell.<section>.toml` file (replaces that section's keys).
    pub fn merge_section(&mut self, section: &str, text: &str) -> Result<(), String> {
        let t: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
        let mut wrapper = toml::Table::new();
        wrapper.insert(section.to_owned(), toml::Value::Table(t));
        merge(&mut self.table, wrapper);
        Ok(())
    }

    fn raw(&self, section: &str, key: &str) -> Option<&toml::Value> {
        self.table.get(section)?.as_table()?.get(key)
    }

    pub fn f32(&self, section: &str, key: &str) -> Option<f32> {
        match self.raw(section, key)? {
            toml::Value::Float(f) => Some(*f as f32),
            toml::Value::Integer(i) => Some(*i as f32),
            toml::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn bool(&self, section: &str, key: &str) -> Option<bool> { self.raw(section, key)?.as_bool() }

    /// Resolve a colour value: hex / role / `section.key` reference (max depth 4) / gradient.
    pub fn color(&self, section: &str, key: &str, pal: &Palette) -> Option<Color32> {
        let v = self.raw(section, key)?.as_str()?.to_owned();
        self.resolve_color(&v, pal, 0)
    }

    fn resolve_color(&self, v: &str, pal: &Palette, depth: u8) -> Option<Color32> {
        let v = v.trim();
        match v {
            "foreground" | "text" => return Some(pal.foreground),
            "accent" => return Some(pal.accent),
            "urgent" => return Some(pal.urgent),
            "muted" => return Some(pal.muted),
            "background" => return Some(pal.background),
            "transparent" => return Some(Color32::TRANSPARENT),
            _ => {}
        }
        if let Some(c) = parse_color(v) {
            return Some(c);
        }
        if depth < 4
            && let Some((s, k)) = v.split_once('.') {
                let inner = self.raw(s, k)?.as_str()?.to_owned();
                return self.resolve_color(&inner, pal, depth + 1);
            }
        None
    }
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}
