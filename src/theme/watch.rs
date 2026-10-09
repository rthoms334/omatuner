//! Live reload via inotify (`notify` 8, non-recursive watches). Owner: WP-01.
//!
//! Watches (omarchy.md §1.4):
//! * `~/.local/state/omarchy/current/` — `theme.name` close-write (LAST write of
//!   `omarchy-theme-set`) or `theme` moved-in/created → reload. Ignore `next-theme`,
//!   `background`.
//! * `~/.config/omarchy/` — `shell.toml` create/modify/delete (text size override).
//! * `~/.config/fontconfig/` — `fonts.conf` → `ThemeEvent::FontsChanged`.
//! * Hyprland config (`~/.config/hypr/*.lua`) → re-query rounding 200 ms later (optional).
//!
//! Debounce 180 ms; reload on a background thread with `Theme::try_load` — on error (e.g. the
//! reload landed between `rm -rf current/theme` and `mv next-theme theme`) keep the current
//! theme and retry after 300 ms (never flash the fallback palette); send
//! `ThemeEvent::Changed(Arc<Theme>)`. Font changes: resolve + read the font files on the
//! background thread too and send `ThemeEvent::FontsChanged(Arc<LoadedFonts>)` — the UI only
//! installs them.
//! If a watch can't be set up (dir missing), poll `theme.name` mtime every 5 s instead.

use crate::error::Result;
use crate::msg::EventBus;
use crate::paths::AppPaths;

/// Keeps the watchers alive; dropping stops them.
pub struct ThemeWatcher {
    _watchers: Vec<notify::RecommendedWatcher>,
    _poller: Option<std::thread::JoinHandle<()>>,
}

impl ThemeWatcher {
    /// Start watching. `ui_scale` from config is applied to reloaded themes.
    /// SKELETON SAFE DEFAULT: `Err` (the app logs it and runs without live reload).
    pub fn start(paths: AppPaths, ui_scale: f32, bus: EventBus) -> Result<Self> {
        Err(crate::error::Error::other("theme live reload not implemented yet (WP-01)"))
    }
}
