//! `~/.config/tuner/config.toml` — user preferences. Implemented in the skeleton.
//!
//! Contract:
//! * Every field has a serde default, so a partial (or empty) file is valid and new fields can be
//!   added without migrations. Unknown keys are ignored (forward compatible).
//! * [`Config::save`] writes atomically (tmp file + rename) and is called from the UI thread only
//!   when the Settings screen commits a change; it is cheap (< 1 ms).
//! * Per-source and per-channel settings live in the database, NOT here (see `model::source`).
//! * Credentials are NEVER stored here.
//!
//! Ownership: struct shape frozen by the skeleton; the Settings screen (WP-12) renders it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{LiveFormat, SortMode};

/// The live config shared by services and jobs. Readers clone the inner `Arc` (cheap, no
/// struct clone per frame); the app swaps in a new `Arc<Config>` after Settings edits.
pub type SharedConfig = std::sync::Arc<std::sync::RwLock<std::sync::Arc<Config>>>;

/// Current value of a [`SharedConfig`] (poison-tolerant).
pub fn current(c: &SharedConfig) -> std::sync::Arc<Config> {
    c.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub interface: InterfaceConfig,
    pub player: PlayerConfig,
    pub epg: EpgConfig,
    pub reminders: ReminderConfig,
    pub parental: ParentalConfig,
    pub integration: IntegrationConfig,
    pub fonts: FontConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StartScreen {
    /// Live TV until the user has favourites or watch history, then Home (a new M3U user would
    /// otherwise land on an empty Home). No sources at all → onboarding regardless.
    #[default]
    Auto,
    Home,
    Live,
    Guide,
    /// Whatever screen was open when the app last quit.
    Last,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub start_screen: StartScreen,
    /// Default for sources whose own `smart_names` is unset (UHF "Smart Playlists").
    pub smart_names: bool,
    /// Default live format for Xtream sources whose own setting is `Auto`.
    pub live_format: LiveFormat,
    /// Refresh playlists on launch when the last refresh is older than this (hours, 0 = never).
    pub refresh_on_launch_after_h: u32,
    /// Background playlist refresh interval while running (hours, 0 = manual only).
    pub playlist_refresh_h: u32,
    /// Background EPG refresh interval while running (hours, 0 = manual only).
    pub epg_refresh_h: u32,
    /// Keep past programmes this many days (raised automatically to the max catch-up window).
    pub epg_keep_past_days: u32,
    /// Verbose (debug-level) logging into the in-app log viewer and log file.
    pub debug_log: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            start_screen: StartScreen::Auto,
            smart_names: true,
            live_format: LiveFormat::Auto,
            refresh_on_launch_after_h: 12,
            playlist_refresh_h: 24,
            epg_refresh_h: 12,
            epg_keep_past_days: 3,
            debug_log: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarMode {
    /// Expanded (icon + label) at Wide, icon rail at Medium, hidden at Compact.
    #[default]
    Auto,
    Expanded,
    Rail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Density {
    /// Omarchy menu rows (50 px, 58 with detail line).
    #[default]
    Comfortable,
    /// 36 px rows, no detail line.
    Compact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceConfig {
    pub sidebar: SidebarMode,
    pub density: Density,
    pub show_channel_numbers: bool,
    pub show_logos: bool,
    pub clock_24h: bool,
    /// Live TV: resting the keyboard cursor on a channel for `preview_delay_ms` plays it in the
    /// preview pane. OFF by default (each autoplay opens a provider connection; Xtream accounts
    /// with one connection get banned for rapid re-tunes; software decode on Asahi). Never
    /// triggered by mouse hover, and never replaces something the user started explicitly
    /// (only plays when idle or when the current stream was itself started by autoplay).
    pub live_preview_autoplay: bool,
    /// Cursor rest time before preview autoplay (ms). Separate from the player's zap delay.
    pub preview_delay_ms: u32,
    /// Guide: focusing a programme row tunes the preview (costs the single stream connection).
    pub guide_preview: bool,
    /// Keep playing in a corner mini-player when leaving the player / Live screen.
    pub mini_player: bool,
    /// OSD auto-hide delay in ms.
    pub osd_timeout_ms: u32,
    /// Extra UI scale applied on top of Omarchy's `[font] base-size` (1.0 = follow Omarchy).
    pub ui_scale: f32,
    pub hide_empty_groups: bool,
    pub live_sort: SortMode,
    pub movie_sort: SortMode,
    pub series_sort: SortMode,
    /// Remember last screen etc. (used by StartScreen::Last).
    pub last_screen: Option<String>,
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            sidebar: SidebarMode::Auto,
            density: Density::Comfortable,
            show_channel_numbers: true,
            show_logos: true,
            clock_24h: true,
            live_preview_autoplay: false,
            preview_delay_ms: 1200,
            guide_preview: false,
            mini_player: true,
            osd_timeout_ms: 3500,
            ui_scale: 1.0,
            hide_empty_groups: true,
            live_sort: SortMode::Provider,
            movie_sort: SortMode::RecentlyAdded,
            series_sort: SortMode::RecentlyAdded,
            last_screen: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubtitleAuto {
    /// Never auto-select subtitles.
    Off,
    /// Only forced tracks in the preferred language.
    #[default]
    Forced,
    /// Always select the first subtitle in a preferred language.
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    /// `"auto"` = `no` on Apple Silicon/Asahi (no HW decoder), `auto-safe` elsewhere.
    /// Any other value is passed to mpv's `hwdec` verbatim.
    pub hwdec: String,
    /// Network read-ahead in seconds (`demuxer-readahead-secs`, `cache-secs`).
    pub cache_secs: u32,
    /// UHF "instant load": minimal buffering for fast zapping (needs a good connection).
    pub instant_load: bool,
    /// `demuxer-max-bytes` in MiB.
    pub demuxer_max_mib: u32,
    /// `network-timeout` in seconds; also the load timeout of the state machine.
    pub network_timeout_s: u32,
    /// Delay before a zap actually connects; repeated zaps inside the window coalesce.
    pub zap_delay_ms: u32,
    pub skip_small_s: u32,
    pub skip_large_s: u32,
    pub volume: f64,
    pub muted: bool,
    /// ISO-639 codes in preference order (`alang`).
    pub audio_langs: Vec<String>,
    /// ISO-639 codes in preference order (`slang`).
    pub sub_langs: Vec<String>,
    pub sub_auto: SubtitleAuto,
    pub sub_scale: f64,
    /// Reconnect attempts before giving up (live).
    pub reconnect_max_attempts: u32,
    /// Default User-Agent when neither the playlist nor the source sets one.
    pub user_agent: String,
    /// VOD resume: only offer resume when position > this many seconds.
    pub resume_min_s: u32,
    /// VOD counts as watched at this percentage.
    pub watched_pct: u8,
    /// Extra raw mpv options (power users). Applied after ours at init.
    pub mpv_options: BTreeMap<String, String>,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            hwdec: "auto".into(),
            cache_secs: 10,
            instant_load: false,
            demuxer_max_mib: 150,
            network_timeout_s: 15,
            zap_delay_ms: 300,
            skip_small_s: 10,
            skip_large_s: 60,
            volume: 100.0,
            muted: false,
            audio_langs: vec!["eng".into(), "en".into()],
            sub_langs: vec!["eng".into(), "en".into()],
            sub_auto: SubtitleAuto::Forced,
            sub_scale: 1.0,
            reconnect_max_attempts: 8,
            user_agent: "VLC/3.0.21 LibVLC/3.0.21".into(),
            resume_min_s: 60,
            watched_pct: 92,
            mpv_options: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpgConfig {
    /// Jaro-Winkler threshold for fuzzy *suggestions* (never auto-applied).
    pub fuzzy_threshold: f64,
    /// Accept normalized-name matches automatically (step 4 of the matcher).
    pub auto_name_match: bool,
}

impl Default for EpgConfig {
    fn default() -> Self {
        Self { fuzzy_threshold: 0.92, auto_name_match: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReminderConfig {
    pub enabled: bool,
    /// Notify this many minutes before the programme starts.
    pub lead_min: u32,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self { enabled: true, lead_min: 2 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ParentalConfig {
    pub enabled: bool,
    /// hex(blake3(salt || pin)); `None` = no PIN set.
    pub pin_hash: Option<String>,
    /// hex salt (16 random bytes).
    pub pin_salt: Option<String>,
    /// Re-lock after this many minutes of the unlocked session (0 = until restart).
    pub relock_min: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IdleInhibitMode {
    /// Inhibit idle whenever video is playing and the window is visible.
    #[default]
    WhilePlaying,
    /// Only while the player is fullscreen.
    Fullscreen,
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationConfig {
    pub mpris: bool,
    pub idle_inhibit: IdleInhibitMode,
    /// Follow the live Omarchy theme. When false, uses the built-in fallback palette.
    pub follow_omarchy_theme: bool,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self { mpris: true, idle_inhibit: IdleInhibitMode::WhilePlaying, follow_omarchy_theme: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// fontconfig pattern for the UI font (Omarchy uses the `monospace` alias).
    pub family: String,
    /// Load script fallbacks (CJK, Arabic, Hebrew, Thai, Devanagari...) found via fontconfig.
    pub script_fallbacks: bool,
    /// Extra font files appended to the fallback chain.
    pub extra_fallbacks: Vec<std::path::PathBuf>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self { family: "monospace".into(), script_fallbacks: true, extra_fallbacks: vec![] }
    }
}

impl Config {
    /// Load from `path`. A missing file yields defaults; a malformed file is an error (the caller
    /// shows it and falls back to defaults without overwriting the user's file).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Atomic save (write `config.toml.tmp`, fsync, rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let s = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        let tmp = path.with_extension("toml.tmp");
        let mut f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        f.write_all(s.as_bytes()).map_err(|e| Error::io(&tmp, e))?;
        f.sync_all().map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
    }

    /// Resolve `player.hwdec = "auto"`: `no` on Apple Silicon running Asahi (no usable HW
    /// decoder; avoids failed Vulkan/VDPAU probes), `auto-safe` elsewhere.
    pub fn effective_hwdec(&self) -> String {
        if self.player.hwdec != "auto" {
            return self.player.hwdec.clone();
        }
        if is_asahi() { "no".into() } else { "auto-safe".into() }
    }
}

/// Detect Apple Silicon under Asahi Linux: aarch64 + device-tree compatible contains "apple,".
pub fn is_asahi() -> bool {
    if std::env::consts::ARCH != "aarch64" {
        return false;
    }
    std::fs::read("/proc/device-tree/compatible").map(|b| String::from_utf8_lossy(&b).contains("apple,")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }
}
