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

/// Settings ▸ Interface ▸ Playlist on cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PlaylistOnCards {
    /// Only while two or more playlists have that kind of item and a screen shows them all
    /// (DESIGN §29: inside one playlist, or with only one, the mark is noise).
    #[default]
    Auto,
    Always,
    Never,
}

/// Settings ▸ Interface ▸ "Same title in several playlists" (DESIGN §29.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SameTitle {
    /// "All playlists" shows the copies of one movie or series as one stack.
    #[default]
    Combine,
    /// Every copy is its own card, as inside one playlist.
    ShowEach,
}

/// Which copy of a stacked title plays first, after the pin, your progress, troubled copies
/// and your audio language (DESIGN §29.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CopyOrder {
    /// The playlist order of Settings ▸ Playlists.
    #[default]
    PlaylistOrder,
    BestQuality,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceConfig {
    pub sidebar: SidebarMode,
    pub density: Density,
    pub show_channel_numbers: bool,
    pub show_logos: bool,
    /// SD / HD / FHD / 4K badges next to channel names (DESIGN §13.0).
    pub quality_badges: bool,
    /// The playlist an item comes from on cards, rails, rows and detail pages (DESIGN §13.0).
    pub playlist_on_cards: PlaylistOnCards,
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
    /// The playlist each screen shows (DESIGN §29); `None` = All playlists.
    pub playlist_scope: PlaylistScopes,
    /// Copies of one title in "All playlists": one stack, or each its own card (DESIGN §29.11).
    pub same_title: SameTitle,
    /// Which copy of a stack plays first after the stronger rules (DESIGN §29.12).
    pub copy_order: CopyOrder,
}

/// The playlist Movies, Series and Live TV each show (the guide follows Live TV); `None` = All
/// playlists. A playlist that is gone, disabled or empty for that screen falls back to All.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PlaylistScopes {
    pub movies: Option<crate::model::SourceId>,
    pub series: Option<crate::model::SourceId>,
    pub live: Option<crate::model::SourceId>,
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            sidebar: SidebarMode::Auto,
            density: Density::Comfortable,
            show_channel_numbers: true,
            show_logos: true,
            quality_badges: true,
            playlist_on_cards: PlaylistOnCards::Auto,
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
            playlist_scope: PlaylistScopes::default(),
            same_title: SameTitle::Combine,
            copy_order: CopyOrder::PlaylistOrder,
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
    /// Reconnect attempts before giving up on a live channel that has not played yet (one that
    /// played keeps reconnecting until stopped, DESIGN §11.4).
    pub reconnect_max_attempts: u32,
    /// Default User-Agent when neither the playlist nor the source sets one.
    pub user_agent: String,
    /// VOD resume: only offer resume when position > this many seconds.
    pub resume_min_s: u32,
    /// VOD counts as watched at this percentage.
    pub watched_pct: u8,
    /// Extra raw mpv options (power users). Applied after ours at init.
    pub mpv_options: BTreeMap<String, String>,
    /// Pop-out player (Hyprland, DESIGN §21.6): size of a new pop-out…
    pub popout_size: PopoutSize,
    /// …and the monitor corner it goes to.
    pub popout_corner: PopoutCorner,
    /// Where the last pop-out was when it was popped back in, if the user had moved or resized
    /// it (`[x, y, w, h]`, logical px, x/y relative to its monitor's top-left): the next one
    /// comes back there. Cleared by an unmoved pop-out and when the size or corner is changed in
    /// Settings.
    pub popout_last: Option<[i32; 4]>,
}

/// 16:9 pop-out sizes (logical px).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PopoutSize {
    /// 480 × 270.
    #[default]
    Small,
    /// 640 × 360.
    Medium,
}

impl PopoutSize {
    pub fn logical(self) -> (i32, i32) {
        match self {
            PopoutSize::Small => (480, 270),
            PopoutSize::Medium => (640, 360),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PopoutCorner {
    #[default]
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
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
            popout_size: PopoutSize::Small,
            popout_corner: PopoutCorner::BottomRight,
            popout_last: None,
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
    /// Without a PIN: show explicit (adult) content anyway (DESIGN §20.3). With a PIN it is
    /// shown while the session is unlocked, whatever this says.
    pub show_explicit: bool,
}

impl ParentalConfig {
    /// The PIN guards locked items: parental lock on and a PIN set.
    pub fn pin_active(&self) -> bool {
        self.enabled && self.pin_hash.is_some()
    }

    /// Explicit content is hidden (DESIGN §20.3): with a PIN, while the session is locked
    /// (`unlocked` false); without one, unless "Show explicit content" is on.
    pub fn hides_explicit(&self, unlocked: bool) -> bool {
        if self.pin_active() { !unlocked } else { !self.show_explicit }
    }
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

    /// The settings of a restored backup (`raw` = its `config.toml`) for this machine: the
    /// backup's preferences, except what belongs to this machine and stays `local` — fonts,
    /// hardware decoding, raw mpv options, UI scale, debug logging, the last screen. The
    /// parental settings (PIN) come from the backup only while no PIN is set here: a restore
    /// must not remove or replace an existing PIN.
    pub fn restored(raw: &str, local: &Config) -> Result<Config> {
        let mut c: Config = toml::from_str(raw).map_err(|e| Error::Config(format!("the backup's settings: {e}")))?;
        c.fonts = local.fonts.clone();
        c.player.hwdec.clone_from(&local.player.hwdec);
        c.player.mpv_options.clone_from(&local.player.mpv_options);
        c.interface.ui_scale = local.interface.ui_scale;
        c.interface.last_screen.clone_from(&local.interface.last_screen);
        c.general.debug_log = local.general.debug_log;
        if local.parental.pin_hash.is_some() {
            c.parental = local.parental.clone();
        }
        Ok(c)
    }

    /// Move a config file that [`Config::load`] rejected aside (`config.toml.bad`, or `.bad.1`…
    /// when that exists), so the first Settings change — which saves the defaults the app then
    /// runs on — can't overwrite it (it may hold the parental PIN). The primary instance does
    /// this at startup.
    pub fn set_aside(path: &Path, error: Error) -> SetAside {
        let moved_to = (0..100)
            .map(|i| path.with_extension(if i == 0 { "toml.bad".to_owned() } else { format!("toml.bad.{i}") }))
            .find(|p| !p.exists())
            .ok_or_else(|| std::io::Error::other("too many set-aside copies"))
            .and_then(|bad| std::fs::rename(path, &bad).map(|()| bad));
        SetAside { error, moved_to }
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

/// A config file [`Config::load`] rejected, after [`Config::set_aside`].
#[derive(Debug)]
pub struct SetAside {
    pub error: Error,
    /// Where the file went (`Err`: it couldn't be moved; a save may then replace it).
    pub moved_to: std::io::Result<std::path::PathBuf>,
}

impl SetAside {
    /// For the log: the parse/read error and what happened to the file.
    pub fn log_line(&self) -> String {
        match &self.moved_to {
            Ok(p) => format!("{}; using defaults (your file was moved to {})", self.error, p.display()),
            Err(e) => format!("{}; using defaults (couldn't move your file aside: {e})", self.error),
        }
    }

    /// For a toast: short, the details are in the log.
    pub fn toast_text(&self) -> String {
        match &self.moved_to {
            Ok(p) => format!(
                "Your settings file has an error — using defaults. It was kept as {}",
                p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            ),
            Err(_) => "Your settings file has an error — using defaults (see Logs)".to_owned(),
        }
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

    /// Audit fix: restoring a backup ignored its settings. They come back except what belongs
    /// to this machine, and never replace a PIN that is set here.
    #[test]
    fn restored_settings_keep_machine_specific_ones_and_an_existing_pin() {
        let mut backup = Config::default();
        backup.player.zap_delay_ms = 120;
        backup.player.hwdec = "vaapi".into();
        backup.interface.ui_scale = 1.5;
        backup.fonts.family = "Other Mono".into();
        backup.parental =
            ParentalConfig { show_explicit: false, enabled: true, pin_hash: Some("backup".into()), pin_salt: Some("s1".into()), relock_min: 5 };
        let raw = toml::to_string_pretty(&backup).unwrap();
        let mut local = Config::default();
        local.player.hwdec = "no".into();
        local.player.mpv_options.insert("vo".into(), "gpu-next".into());
        local.interface.ui_scale = 1.1;

        let c = Config::restored(&raw, &local).unwrap();
        assert_eq!(c.player.zap_delay_ms, 120);
        assert_eq!(c.parental, backup.parental, "no PIN here: the backup's applies");
        assert_eq!((c.player.hwdec.as_str(), c.interface.ui_scale), ("no", 1.1));
        assert_eq!(c.player.mpv_options, local.player.mpv_options);
        assert_eq!(c.fonts, local.fonts);

        local.parental =
            ParentalConfig { show_explicit: false, enabled: true, pin_hash: Some("local".into()), pin_salt: Some("s2".into()), relock_min: 0 };
        let mut no_pin = backup.clone();
        no_pin.parental = ParentalConfig::default();
        assert_eq!(Config::restored(&toml::to_string_pretty(&no_pin).unwrap(), &local).unwrap().parental, local.parental);
        assert!(Config::restored("general = 3", &local).is_err());
    }

    /// Audit fix: a typo in config.toml made Tuner start on defaults and the first Settings
    /// change saved them over the file, losing e.g. the parental PIN.
    #[test]
    fn a_broken_file_is_set_aside_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let broken = "[parental]\npin_hash = \"abc\"\nenabled = tru\n";
        let load = || Config::load(&path).map_err(|e| Config::set_aside(&path, e));
        std::fs::write(&path, broken).unwrap();
        let bad = load().expect_err("malformed");
        assert_eq!(bad.moved_to.as_deref().unwrap(), dir.path().join("config.toml.bad"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.bad")).unwrap(), broken);
        // the first save no longer touches it; a second broken file doesn't replace the first
        Config::default().save(&path).unwrap();
        std::fs::write(&path, "nonsense").unwrap();
        assert_eq!(load().expect_err("malformed").moved_to.unwrap(), dir.path().join("config.toml.bad.1"));
        assert_eq!(std::fs::read_to_string(dir.path().join("config.toml.bad")).unwrap(), broken);
    }
}
