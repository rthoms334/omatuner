//! Live Omarchy theme. Owner: WP-01. DESIGN §16.
//!
//! Sources (in order): `~/.local/state/omarchy/current/theme/colors.toml`, `shell.toml`
//! (+ `shell.<section>.toml`), `~/.config/omarchy/shell.toml` (user override, key by key),
//! `hyprctl -j getoption decoration:rounding` (default 0 = square). Missing files → built-in
//! fallback (`Palette::fallback`), never an error at startup.
//! Live reload: [`watch::ThemeWatcher`] (notify, non-recursive) sends `ThemeEvent::Changed`;
//! the app swaps `Arc<Theme>` and re-applies egui style in the same frame.

pub mod colors;
pub mod egui_style;
pub mod shell;
pub mod tokens;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::Color32;

pub use colors::{Mode, Palette};
pub use shell::ShellConfig;
pub use tokens::{Interaction, SpaceToken, Tokens, TypeToken, with_alpha};

use crate::model::SourceColor;
use crate::paths::AppPaths;

#[derive(Debug, Clone)]
pub enum ThemeEvent {
    /// Theme files changed and were re-read successfully.
    Changed(Arc<Theme>),
    /// `~/.config/fontconfig/fonts.conf` changed; the watcher already resolved AND read the
    /// font files off the UI thread — the app only installs them.
    FontsChanged(Arc<crate::fonts::LoadedFonts>),
}

/// Resolved theme: palette + shell config + derived tokens.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Contents of `theme.name` (e.g. "theme-liftoff"), or "fallback".
    pub name: String,
    pub palette: Palette,
    pub shell: ShellConfig,
    pub tokens: Tokens,
    fonts: HashMap<TypeToken, f32>,
    spaces: HashMap<SpaceToken, f32>,
}

impl Theme {
    /// Build from parts (tests, fallback).
    pub fn from_parts(name: String, palette: Palette, shell: ShellConfig, rounding: f32, ui_scale: f32) -> Self {
        let tokens = Tokens::compute(&palette, &shell, rounding, ui_scale);
        let fonts = TypeToken::ALL.iter().map(|t| (*t, tokens.font(&shell, *t))).collect();
        use SpaceToken::*;
        let all = [
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
        ];
        let spaces = all.iter().map(|t| (*t, tokens.space(&shell, *t))).collect();
        Self { name, palette, shell, tokens, fonts, spaces }
    }

    pub fn fallback() -> Self {
        Self::from_parts("fallback".into(), Palette::fallback(), ShellConfig::default(), 0.0, 1.0)
    }

    /// Read the live Omarchy theme. Never fails: any unreadable piece falls back with a warning
    /// (startup). The watcher uses [`Theme::try_load`] instead.
    pub fn load(paths: &AppPaths, ui_scale: f32) -> Self {
        match Self::try_load(paths, ui_scale) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("theme: {e}; using the fallback palette");
                let mut t = Self::fallback();
                if ui_scale != 1.0 {
                    t = Self::from_parts(t.name, t.palette, t.shell, t.tokens.rounding, ui_scale);
                }
                t
            }
        }
    }

    /// Strict variant for live reload: `Err` when `colors.toml` is missing/unparsable (e.g. the
    /// reload raced `omarchy-theme-set`'s `rm -rf current/theme` / `mv next-theme theme`) — the
    /// caller keeps the current theme and retries. Cascade: theme `shell.toml` → theme
    /// `shell.<section>.toml` files → user `~/.config/omarchy/shell.toml` (last, key by key).
    pub fn try_load(paths: &AppPaths, ui_scale: f32) -> Result<Self, String> {
        let fixed = fixed_theme_dir();
        let dir = fixed.clone().unwrap_or_else(|| paths.omarchy_theme_dir());
        let name = match &fixed {
            Some(d) => d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "fixed".into()),
            None => {
                std::fs::read_to_string(paths.omarchy_current().join("theme.name")).map(|s| s.trim().to_owned()).unwrap_or_else(|_| "fallback".into())
            }
        };
        let colors = std::fs::read_to_string(dir.join("colors.toml")).map_err(|e| format!("colors.toml: {e}"))?;
        let palette = Palette::parse(&colors, dir.join("light.mode").exists()).map_err(|e| format!("colors.toml: {e}"))?;
        let theme_shell = std::fs::read_to_string(dir.join("shell.toml")).unwrap_or_default();
        let mut shell = ShellConfig::parse(&theme_shell, None).unwrap_or_else(|e| {
            tracing::warn!("shell.toml: {e}");
            ShellConfig::default()
        });
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = rd.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
            files.sort();
            for (f, path) in files {
                if let Some(section) = f.strip_prefix("shell.").and_then(|s| s.strip_suffix(".toml"))
                    && let Ok(t) = std::fs::read_to_string(&path)
                    && let Err(e) = shell.merge_section(section, &t)
                {
                    tracing::warn!("{f}: {e}");
                }
            }
        }
        if fixed.is_none()
            && let Ok(u) = std::fs::read_to_string(paths.omarchy_config().join("shell.toml"))
            && let Err(e) = shell.merge_override(&u)
        {
            tracing::warn!("~/.config/omarchy/shell.toml: {e}");
        }
        // The directory may have been swapped while we read it (`rm -rf theme` → `mv`): a
        // missing colors.toml now means the pieces may come from two themes — retry instead.
        if !dir.join("colors.toml").is_file() {
            return Err("theme directory replaced while loading".into());
        }
        // a fixed theme ignores the compositor's rounding too (Omarchy's default is square)
        let rounding = if fixed.is_some() { 0.0 } else { query_rounding().unwrap_or(0.0) };
        Ok(Self::from_parts(name, palette, shell, rounding, ui_scale))
    }

    /// Layout unit (see `Tokens::rem`).
    pub fn rem(&self) -> f32 {
        self.tokens.rem()
    }

    pub fn font(&self, t: TypeToken) -> f32 {
        self.fonts.get(&t).copied().unwrap_or(12.0)
    }
    pub fn space(&self, t: SpaceToken) -> f32 {
        self.spaces.get(&t).copied().unwrap_or(8.0)
    }
    pub fn is_light(&self) -> bool {
        self.palette.mode == Mode::Light
    }

    /// Badge colour for a source, from the live palette.
    pub fn source_color(&self, c: SourceColor) -> Color32 {
        let p = &self.palette;
        match c {
            SourceColor::Accent => p.accent,
            SourceColor::Red => p.red,
            SourceColor::Yellow => p.yellow,
            SourceColor::Orange => p.orange,
            SourceColor::Green => p.green,
            SourceColor::Cyan => p.cyan,
            SourceColor::Blue => p.blue,
            SourceColor::Magenta => p.magenta,
            SourceColor::Brown => p.brown,
            SourceColor::Muted => p.muted,
        }
    }
}

/// Developer override (screenshots, DESIGN §23.1): an Omarchy theme directory (`colors.toml`,
/// optional `shell*.toml`, `light.mode`) used instead of the live theme. Nothing of the user's
/// desktop then reaches the look — no `~/.config/omarchy/shell.toml`, no Hyprland rounding —
/// and the theme watcher stays off.
pub const THEME_DIR_ENV: &str = "TUNER_THEME_DIR";

/// The directory named by [`THEME_DIR_ENV`], if set.
pub fn fixed_theme_dir() -> Option<std::path::PathBuf> {
    std::env::var_os(THEME_DIR_ENV).filter(|d| !d.is_empty()).map(std::path::PathBuf::from)
}

/// `hyprctl -j getoption decoration:rounding` → `.int`. `None` if hyprctl is unavailable.
/// WP-01: call off the UI thread on reload (it's a subprocess, ~5 ms).
pub fn query_rounding() -> Option<f32> {
    let out = std::process::Command::new("hyprctl").args(["-j", "getoption", "decoration:rounding"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("int").and_then(|i| i.as_f64()).map(|f| f as f32)
}
