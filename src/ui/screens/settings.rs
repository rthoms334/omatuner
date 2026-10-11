//! Settings (DESIGN §13.9): page list (Playlists, EPG, Player, Interface, Favourites,
//! Reminders, Parental, Backup, Hidden, Orphans, About) | page content. Panel idiom; reorder
//! rows (playlists, EPG priority, favourites lists) with Alt+↑/↓ or K/J or the drag grip.
//! Playlists page: sources with status, account info (expiry, connections, formats),
//! refresh/edit/delete/reorder, "+ Add" (→ `Route::Onboarding(AddSource)`); EPG page: ordered
//! EPG sources with offsets + "EPG mapping…"; Reminders: pending list (Delete cancels);
//! About: version, paths, log viewer link. Text fields push `TextInput`. Edits go through
//! `ctx.edit_config` (applied live to the player). Owner: WP-12.
//!
//! Model: every page is a list of [`Row`]s rebuilt from the config, the library and the rows
//! read off the UI thread (sources, EPG sources, reminders, orphans, hidden VOD — re-read on
//! the matching `DataScope`). Keys (Panel idiom, `[Panel]` / `[TextInput, Panel]`):
//! ↑/↓ (j/k) move, ← (h) steps back to the page list from any row (spatial panes, DESIGN
//! §15.1), → (l) / Shift+→ (L) increase or pick the next option, Shift+← (H) decrease,
//! Enter/Space toggle or open, x/Delete the row's menu, Alt+↑/↓ (K/J) reorder, Tab switches
//! between the page list and the page (→ / Enter back into it), Esc leaves a text field, then
//! the page. A source editor with unsaved changes keeps its page: ↑/↓, Esc or a click in the
//! page list say so and return to the editor instead of dropping the changes. Config rows
//! apply through `ctx.edit_config` (saved, applied live); store rows go through the writer and
//! its `DataScope` notification. Changing parental settings needs the open PIN session once a
//! PIN exists.
//!
//! Xtream upgrade (DESIGN §27.3): a playlist that signed in with its Xtream login reads
//! "Xtream (from M3U)"; its editor (and that of a playlist kept as M3U) has "Use the Xtream
//! login", which saves `keep_m3u` and runs a forced refresh that performs the switch. The open
//! editor follows `DataScope::Sources` (a silent upgrade must not be undone by its Save), and
//! an unchanged server is saved as stored (an upgraded server may carry a path prefix).
//!
//! Category filter (DESIGN §28.5): a playlist's editor has "Categories…" (the "Choose what to
//! show" picker); Hidden lists what the filter hides per playlist and tag (Enter shows the
//! tag, → lists its categories, Enter on one shows just it), then only what the user hid
//! themselves (a filtered playlist would otherwise flood those lists).
//!
//! Several playlists (DESIGN §29.14): Interface ▸ "Same title in several playlists"
//! (Combine / Show each copy) and "Which copy plays first" (Playlist order / Best quality);
//! Hidden ▸ "Titles you separated or pinned" lists the copies taken out of their stack ("Not the
//! same title") and the "Always play from" pins — Enter undoes one.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::config::{
    Config, CopyOrder, Density, IdleInhibitMode, PlaylistOnCards, PopoutCorner, PopoutSize, SameTitle, SidebarMode, StartScreen, SubtitleAuto,
};
use crate::jobs::{JobEvent, JobKind, JobOutcome, Submitted};
use crate::keymap::{Action, KeyContext};
use crate::model::{
    AccountStatus, CategoryTag, ChannelId, EpgSource, EpgSourceId, EpgSourceKind, FavListId, Group, GroupFilter, GroupId, GroupRef, ItemKind,
    ItemRef, JobId, LiveFormat, Reminder, ReminderId, SortMode, Source, SourceColor, SourceId, SourceKind, SourceSettings,
};
use crate::msg::{AppEvent, DataScope, Toast};
use crate::platform::{PickPurpose, PlatformEvent};
use crate::store::backup::{BackupOptions, RestoreMode};
use crate::store::titles::{LinkAction, TitleLink};
use crate::store::user::Orphan;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::overlays::categories::{self, tag_name};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult, RenameSpec, RenameTarget};
use crate::ui::parental::{hash_pin, new_salt};
use crate::ui::widgets::badge;
use crate::ui::widgets::button::{ButtonStyle, back_button, icon_button};
use crate::ui::widgets::list::paint_row_state;
use crate::ui::widgets::rows::grouped;
use crate::ui::widgets::text_field::text_field;
use crate::ui::widgets::toggle::toggle;
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, section};
use crate::ui::{LayoutClass, OnboardingStart, OverlayRequest, Route, Screen, ScreenId, SettingsPage, UiCtx};

const PAGES: [(SettingsPage, &str, &str); 11] = [
    (SettingsPage::Playlists, glyphs::PLAYLIST, "Playlists"),
    (SettingsPage::Epg, glyphs::GUIDE, "EPG"),
    (SettingsPage::Player, glyphs::PLAY, "Player"),
    (SettingsPage::Interface, glyphs::DISPLAY, "Interface"),
    (SettingsPage::Favourites, glyphs::FAVOURITE, "Favourites"),
    (SettingsPage::Reminders, glyphs::REMINDER, "Reminders"),
    (SettingsPage::Parental, glyphs::LOCK, "Parental"),
    (SettingsPage::Backup, glyphs::BACKUP, "Backup"),
    (SettingsPage::Hidden, glyphs::HIDDEN, "Hidden items"),
    (SettingsPage::Orphans, glyphs::WARNING, "Orphans"),
    (SettingsPage::About, glyphs::INFO, "About & logs"),
];

// ------------------------------------------------------------------ config row tables

pub(crate) struct CfgToggle {
    pub label: &'static str,
    pub get: fn(&Config) -> bool,
    pub set: fn(&mut Config, bool),
}

pub(crate) struct CfgChoice {
    pub label: &'static str,
    pub options: &'static [&'static str],
    pub get: fn(&Config) -> usize,
    pub set: fn(&mut Config, usize),
}

pub(crate) struct CfgNumber {
    pub label: &'static str,
    /// Suffix ("s", "ms", "h"…).
    pub unit: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub get: fn(&Config) -> f64,
    pub set: fn(&mut Config, f64),
}

pub(crate) struct CfgText {
    pub label: &'static str,
    pub placeholder: &'static str,
    pub get: fn(&Config) -> String,
    pub set: fn(&mut Config, String),
}

const SORT_LIVE: [SortMode; 5] = [SortMode::Provider, SortMode::Number, SortMode::NameAsc, SortMode::NameDesc, SortMode::RecentlyAdded];
const SORT_VOD: [SortMode; 6] =
    [SortMode::RecentlyAdded, SortMode::NameAsc, SortMode::NameDesc, SortMode::Year, SortMode::Rating, SortMode::Provider];
fn pos<T: PartialEq>(all: &[T], v: &T) -> usize {
    all.iter().position(|x| x == v).unwrap_or(0)
}

macro_rules! toggle_row {
    ($label:expr, $($path:ident).+) => {
        CfgToggle { label: $label, get: |c| c.$($path).+, set: |c, v| c.$($path).+ = v }
    };
}
macro_rules! number_row {
    ($label:expr, $unit:expr, $min:expr, $max:expr, $step:expr, $ty:ty, $($path:ident).+) => {
        CfgNumber { label: $label, unit: $unit, min: $min, max: $max, step: $step, get: |c| c.$($path).+ as f64, set: |c, v| c.$($path).+ = v as $ty }
    };
}

static PLAYLIST_CFG_N: [CfgNumber; 2] = [
    number_row!("Refresh playlists every", "h (0 = manual)", 0.0, 168.0, 1.0, u32, general.playlist_refresh_h),
    number_row!("Refresh on launch when older than", "h (0 = never)", 0.0, 168.0, 1.0, u32, general.refresh_on_launch_after_h),
];
static PLAYLIST_CFG_C: CfgChoice = CfgChoice {
    label: "Default live format (Xtream)",
    options: &["Auto", "TS", "M3U8"],
    get: |c| pos(&[LiveFormat::Auto, LiveFormat::Ts, LiveFormat::M3u8], &c.general.live_format),
    set: |c, i| c.general.live_format = [LiveFormat::Auto, LiveFormat::Ts, LiveFormat::M3u8][i],
};

static EPG_N: [CfgNumber; 3] = [
    number_row!("Refresh guides every", "h (0 = manual)", 0.0, 168.0, 1.0, u32, general.epg_refresh_h),
    number_row!("Keep past programmes", "days", 0.0, 30.0, 1.0, u32, general.epg_keep_past_days),
    number_row!("Fuzzy suggestion threshold", "", 0.5, 1.0, 0.01, f64, epg.fuzzy_threshold),
];
static EPG_T: CfgToggle = toggle_row!("Match channels by name automatically", epg.auto_name_match);

static PLAYER_HWDEC: CfgChoice = CfgChoice {
    label: "Hardware decoding",
    options: &["Auto", "Auto (safe)", "Off", "VA-API", "NVDEC", "Vulkan"],
    get: |c| pos(&["auto", "auto-safe", "no", "vaapi", "nvdec", "vulkan"], &c.player.hwdec.as_str()),
    set: |c, i| c.player.hwdec = ["auto", "auto-safe", "no", "vaapi", "nvdec", "vulkan"][i].to_owned(),
};
static PLAYER_N: [CfgNumber; 10] = [
    number_row!("Buffer", "s", 1.0, 120.0, 1.0, u32, player.cache_secs),
    number_row!("Demuxer memory", "MiB", 16.0, 4096.0, 16.0, u32, player.demuxer_max_mib),
    number_row!("Network timeout", "s", 3.0, 120.0, 1.0, u32, player.network_timeout_s),
    number_row!("Zapping delay", "ms", 0.0, 3000.0, 50.0, u32, player.zap_delay_ms),
    number_row!("Skip (small)", "s", 1.0, 120.0, 1.0, u32, player.skip_small_s),
    number_row!("Skip (big)", "s", 5.0, 900.0, 5.0, u32, player.skip_large_s),
    number_row!("Subtitle size", "×", 0.5, 3.0, 0.1, f64, player.sub_scale),
    number_row!("Reconnect attempts", "", 0.0, 50.0, 1.0, u32, player.reconnect_max_attempts),
    number_row!("Offer resume after", "s", 0.0, 1800.0, 10.0, u32, player.resume_min_s),
    number_row!("Count as watched at", "%", 50.0, 100.0, 1.0, u8, player.watched_pct),
];
static PLAYER_T: [CfgToggle; 1] = [toggle_row!("Instant load (less buffering, faster zaps)", player.instant_load)];
static PLAYER_SUB: CfgChoice = CfgChoice {
    label: "Subtitles automatically",
    options: &["Off", "Forced only", "Always"],
    get: |c| pos(&[SubtitleAuto::Off, SubtitleAuto::Forced, SubtitleAuto::Always], &c.player.sub_auto),
    set: |c, i| c.player.sub_auto = [SubtitleAuto::Off, SubtitleAuto::Forced, SubtitleAuto::Always][i],
};
const POPOUT_SIZES: [PopoutSize; 2] = [PopoutSize::Small, PopoutSize::Medium];
const POPOUT_CORNERS: [PopoutCorner; 4] = [PopoutCorner::BottomRight, PopoutCorner::BottomLeft, PopoutCorner::TopRight, PopoutCorner::TopLeft];
/// Pop-out player (Hyprland, DESIGN §21.6). Changing either forgets where the last pop-out
/// was, so the choice applies to the next one.
static PLAYER_POPOUT: [CfgChoice; 2] = [
    CfgChoice {
        label: "Pop-out size",
        options: &["Small (480 × 270)", "Medium (640 × 360)"],
        get: |c| pos(&POPOUT_SIZES, &c.player.popout_size),
        set: |c, i| {
            if c.player.popout_size != POPOUT_SIZES[i] {
                c.player.popout_size = POPOUT_SIZES[i];
                c.player.popout_last = None;
            }
        },
    },
    CfgChoice {
        label: "Pop-out corner",
        options: &["Bottom right", "Bottom left", "Top right", "Top left"],
        get: |c| pos(&POPOUT_CORNERS, &c.player.popout_corner),
        set: |c, i| {
            if c.player.popout_corner != POPOUT_CORNERS[i] {
                c.player.popout_corner = POPOUT_CORNERS[i];
                c.player.popout_last = None;
            }
        },
    },
];
static PLAYER_TEXT: [CfgText; 4] = [
    CfgText {
        label: "Audio languages",
        placeholder: "eng, en",
        get: |c| c.player.audio_langs.join(", "),
        set: |c, v| c.player.audio_langs = split_list(&v),
    },
    CfgText {
        label: "Subtitle languages",
        placeholder: "eng, en",
        get: |c| c.player.sub_langs.join(", "),
        set: |c, v| c.player.sub_langs = split_list(&v),
    },
    CfgText {
        label: "Default User-Agent",
        placeholder: "VLC/3.0.21 LibVLC/3.0.21",
        get: |c| c.player.user_agent.clone(),
        set: |c, v| c.player.user_agent = v.trim().to_owned(),
    },
    CfgText {
        label: "Advanced mpv options",
        placeholder: "key=value; key=value",
        get: |c| c.player.mpv_options.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "),
        set: |c, v| c.player.mpv_options = parse_mpv_options(&v),
    },
];

static IFACE_C: [CfgChoice; 9] = [
    CfgChoice {
        label: "Start screen",
        options: &["Auto", "Home", "Live TV", "Guide", "Last used"],
        get: |c| pos(&[StartScreen::Auto, StartScreen::Home, StartScreen::Live, StartScreen::Guide, StartScreen::Last], &c.general.start_screen),
        set: |c, i| c.general.start_screen = [StartScreen::Auto, StartScreen::Home, StartScreen::Live, StartScreen::Guide, StartScreen::Last][i],
    },
    CfgChoice {
        label: "Sidebar",
        options: &["Auto", "Expanded", "Icons only"],
        get: |c| pos(&[SidebarMode::Auto, SidebarMode::Expanded, SidebarMode::Rail], &c.interface.sidebar),
        set: |c, i| c.interface.sidebar = [SidebarMode::Auto, SidebarMode::Expanded, SidebarMode::Rail][i],
    },
    CfgChoice {
        label: "Density",
        options: &["Comfortable", "Compact"],
        get: |c| pos(&[Density::Comfortable, Density::Compact], &c.interface.density),
        set: |c, i| c.interface.density = [Density::Comfortable, Density::Compact][i],
    },
    CfgChoice {
        label: "Default sort: live TV",
        options: &["Custom order", "Number", "A–Z", "Z–A", "Recently added"],
        get: |c| pos(&SORT_LIVE, &c.interface.live_sort),
        set: |c, i| c.interface.live_sort = SORT_LIVE[i],
    },
    CfgChoice {
        label: "Default sort: movies",
        options: &["Recently added", "A–Z", "Z–A", "Year", "Rating", "Custom order"],
        get: |c| pos(&SORT_VOD, &c.interface.movie_sort),
        set: |c, i| c.interface.movie_sort = SORT_VOD[i],
    },
    CfgChoice {
        label: "Default sort: series",
        options: &["Recently added", "A–Z", "Z–A", "Year", "Rating", "Custom order"],
        get: |c| pos(&SORT_VOD, &c.interface.series_sort),
        set: |c, i| c.interface.series_sort = SORT_VOD[i],
    },
    CfgChoice {
        label: "Keep the screen awake",
        options: &["While playing", "Fullscreen only", "Never"],
        get: |c| pos(&[IdleInhibitMode::WhilePlaying, IdleInhibitMode::Fullscreen, IdleInhibitMode::Never], &c.integration.idle_inhibit),
        set: |c, i| c.integration.idle_inhibit = [IdleInhibitMode::WhilePlaying, IdleInhibitMode::Fullscreen, IdleInhibitMode::Never][i],
    },
    CfgChoice {
        label: "Clock",
        options: &["24 h", "12 h"],
        get: |c| if c.interface.clock_24h { 0 } else { 1 },
        set: |c, i| c.interface.clock_24h = i == 0,
    },
    CfgChoice {
        label: "Playlist on cards",
        options: &["Auto", "Always", "Never"],
        get: |c| pos(&PLAYLIST_ON_CARDS, &c.interface.playlist_on_cards),
        set: |c, i| c.interface.playlist_on_cards = PLAYLIST_ON_CARDS[i],
    },
];
const PLAYLIST_ON_CARDS: [PlaylistOnCards; 3] = [PlaylistOnCards::Auto, PlaylistOnCards::Always, PlaylistOnCards::Never];
const SAME_TITLE: [SameTitle; 2] = [SameTitle::Combine, SameTitle::ShowEach];
const COPY_ORDER: [CopyOrder; 2] = [CopyOrder::PlaylistOrder, CopyOrder::BestQuality];
/// Copies of one title in several playlists (DESIGN §29.14).
static STACK_C: [CfgChoice; 2] = [
    CfgChoice {
        label: "Same title in several playlists",
        options: &["Combine", "Show each copy"],
        get: |c| pos(&SAME_TITLE, &c.interface.same_title),
        set: |c, i| c.interface.same_title = SAME_TITLE[i],
    },
    CfgChoice {
        label: "Which copy plays first",
        options: &["Playlist order", "Best quality"],
        get: |c| pos(&COPY_ORDER, &c.interface.copy_order),
        set: |c, i| c.interface.copy_order = COPY_ORDER[i],
    },
];
static IFACE_T: [CfgToggle; 13] = [
    toggle_row!("Channel numbers", interface.show_channel_numbers),
    toggle_row!("Channel logos", interface.show_logos),
    toggle_row!("Live preview autoplay (uses a provider connection)", interface.live_preview_autoplay),
    toggle_row!("Guide preview", interface.guide_preview),
    toggle_row!("Mini player", interface.mini_player),
    toggle_row!("Hide empty groups", interface.hide_empty_groups),
    toggle_row!("Follow the Omarchy theme", integration.follow_omarchy_theme),
    toggle_row!("Script fallback fonts (CJK, Arabic, Hebrew…)", fonts.script_fallbacks),
    toggle_row!("Reminders", reminders.enabled),
    toggle_row!("Media keys & playerctl (MPRIS)", integration.mpris),
    toggle_row!("Smart names (default for new playlists)", general.smart_names),
    toggle_row!("Debug logging", general.debug_log),
    toggle_row!("Quality badges", interface.quality_badges),
];
static IFACE_N: [CfgNumber; 4] = [
    number_row!("Preview delay", "ms", 300.0, 5000.0, 100.0, u32, interface.preview_delay_ms),
    number_row!("Hide the player controls after", "ms", 1000.0, 15000.0, 500.0, u32, interface.osd_timeout_ms),
    number_row!("UI scale (on top of Omarchy's text size)", "×", 0.75, 2.0, 0.05, f32, interface.ui_scale),
    number_row!("Remind me before", "min", 0.0, 60.0, 1.0, u32, reminders.lead_min),
];
static IFACE_TEXT: [CfgText; 2] = [
    CfgText {
        label: "Font family",
        placeholder: "monospace",
        get: |c| c.fonts.family.clone(),
        set: |c, v| c.fonts.family = if v.trim().is_empty() { "monospace".into() } else { v.trim().to_owned() },
    },
    CfgText {
        label: "Extra font files",
        placeholder: "/path/a.ttf, /path/b.otf",
        get: |c| c.fonts.extra_fallbacks.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
        set: |c, v| c.fonts.extra_fallbacks = split_list(&v).into_iter().map(PathBuf::from).collect(),
    },
];
static PARENTAL_RELOCK: CfgNumber = number_row!("Lock again after", "min (0 = until restart)", 0.0, 480.0, 5.0, u32, parental.relock_min);

/// "eng, en ,de" → ["eng", "en", "de"].
pub(crate) fn split_list(s: &str) -> Vec<String> {
    s.split([',', ';']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()
}

/// "a=1; b = two" → {a: 1, b: two} (entries without `=` are ignored).
pub(crate) fn parse_mpv_options(s: &str) -> std::collections::BTreeMap<String, String> {
    s.split([';', '\n'])
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// "10 s", "0.92", "1.25 ×".
fn number_text(n: &CfgNumber, v: f64) -> String {
    let s = if n.step < 1.0 { format!("{v:.2}") } else { format!("{}", v.round() as i64) };
    if n.unit.is_empty() { s } else { format!("{s} {}", n.unit) }
}

// ------------------------------------------------------------------ rows

/// A row's subject. Orphans and hidden titles are keyed by their durable reference, not by
/// position: their lists are re-read whenever a refresh finishes, possibly while a menu or a
/// confirmation about one of them is open.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Item {
    Source(SourceId),
    Epg(EpgSourceId),
    List(FavListId),
    Reminder(ReminderId),
    HiddenGroup(GroupId),
    /// What a playlist's category filter hides of one tag (`None` = untagged categories).
    HiddenTag(SourceId, Option<CategoryTag>),
    /// "Categories…" / "Choose countries…": the playlist's picker.
    Categories(SourceId),
    HiddenChannel(ChannelId),
    HiddenVod(ItemRef),
    /// A copy split off its stack, or pinned in it (DESIGN §29.14).
    TitleLink(ItemRef, LinkAction),
    /// A category detected or marked as explicit content, or cleared by the user (Settings ▸
    /// Parental; DESIGN §20.3).
    Explicit(GroupId),
    Orphan(ItemRef),
    /// Index into the attach view's candidates (rebuilt only by its own filter).
    AttachTarget(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    AddSource,
    AddFreeChannels,
    AddEpgUrl,
    AddEpgFile,
    EpgMapping,
    NewList,
    SetPin,
    RemovePin,
    LockNow,
    ExportBackup,
    ImportBackup,
    OpenLogs,
    ExportLog,
    SaveSource,
    RefreshSource,
    DeleteSource,
    CloseEditor,
    CloseAttach,
    /// "Give each its own colour": playlists that look the same get the first free colours.
    DistinctColours,
}

/// Local (non-config) fields: forms and the source editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    NewEpgUrl,
    NewListName,
    PinNew,
    PinConfirm,
    BackupPasswords,
    BackupHistory,
    RestoreMode,
    /// Apply the backup's settings too (on for "Replace everything", off for "Merge").
    RestoreSettings,
    SrcName,
    SrcIcon,
    SrcColor,
    SrcUrl,
    SrcServer,
    SrcUser,
    SrcPassword,
    SrcUa,
    SrcLiveFormat,
    SrcRefresh,
    SrcTz,
    SrcProviderEpg,
    SrcSmartNames,
    SrcImportVod,
    /// "Show in All playlists" (`SourceSettings::in_all`, DESIGN §29).
    SrcInAll,
    /// "Use the Xtream login" (= `!keep_m3u`): applied at once, not on Save.
    SrcUseXtream,
    AttachFilter,
}

impl Local {
    fn is_text(self) -> bool {
        matches!(
            self,
            Local::NewEpgUrl
                | Local::NewListName
                | Local::PinNew
                | Local::PinConfirm
                | Local::SrcName
                | Local::SrcUrl
                | Local::SrcServer
                | Local::SrcUser
                | Local::SrcPassword
                | Local::SrcUa
                | Local::SrcTz
                | Local::AttachFilter
        )
    }
    fn is_toggle(self) -> bool {
        matches!(
            self,
            Local::BackupPasswords
                | Local::BackupHistory
                | Local::RestoreSettings
                | Local::SrcProviderEpg
                | Local::SrcSmartNames
                | Local::SrcImportVod
                | Local::SrcInAll
                | Local::SrcUseXtream
        )
    }
}

#[derive(Clone)]
enum Row {
    Section(String),
    Note(String, bool),
    Toggle(&'static CfgToggle),
    Choice(&'static CfgChoice),
    Number(&'static CfgNumber),
    Text(&'static CfgText),
    Act { act: Act, icon: &'static str, label: String, danger: bool },
    Item { item: Item, icon: String, color: Option<SourceColor>, label: String, detail: Option<String>, reorder: bool },
    Local { field: Local, label: &'static str },
}

impl Row {
    fn focusable(&self) -> bool {
        !matches!(self, Row::Section(_) | Row::Note(..))
    }
    fn is_text(&self) -> bool {
        matches!(self, Row::Text(_)) || matches!(self, Row::Local { field, .. } if field.is_text())
    }
}

/// The source editor's working copy. Every field is user-editable: `is_changed` compares the
/// whole struct, so transient UI state (a status, a validation message) must live outside it.
#[derive(Debug, Clone, PartialEq)]
struct SourceEditor {
    original: Source,
    name: String,
    icon: usize,
    color: usize,
    url: String,
    server: String,
    user: String,
    /// Empty = keep the stored password.
    password: String,
    ua: String,
    live_format: usize,
    refresh_h: u32,
    tz: String,
    provider_epg: bool,
    smart_names: bool,
    import_vod: bool,
    /// "Show in All playlists".
    in_all: bool,
    /// `!keep_m3u` (shown for upgraded playlists and playlists kept as M3U).
    use_xtream: bool,
}

/// Take `new` unless the user changed `mine` from `old` (a field edited in an open editor).
fn rebased<T: PartialEq + Clone>(mine: &mut T, old: &T, new: &T) {
    if mine == old {
        *mine = new.clone();
    }
}

impl SourceEditor {
    fn new(s: &Source) -> Self {
        let x = s.xtream.as_ref();
        Self {
            original: s.clone(),
            name: s.name.clone(),
            icon: glyphs::SOURCE_ICONS.iter().position(|g| *g == s.icon).unwrap_or(0),
            color: super::onboarding::COLORS.iter().position(|c| c.0 == s.color).unwrap_or(0),
            url: s.url.as_ref().map(|u| u.expose().clone()).or_else(|| s.path.as_ref().map(|p| p.display().to_string())).unwrap_or_default(),
            server: x.map(|x| x.server.clone()).unwrap_or_default(),
            user: x.map(|x| x.username.clone()).unwrap_or_default(),
            password: String::new(),
            ua: s.settings.user_agent.clone().unwrap_or_default(),
            live_format: pos(&[LiveFormat::Auto, LiveFormat::Ts, LiveFormat::M3u8], &s.settings.live_format),
            refresh_h: s.settings.refresh_h.unwrap_or(0),
            tz: s.settings.server_tz.clone().unwrap_or_default(),
            provider_epg: s.settings.use_provider_epg.unwrap_or(true),
            smart_names: s.settings.smart_names.unwrap_or(true),
            import_vod: s.settings.import_vod.unwrap_or(true),
            in_all: s.settings.in_all.unwrap_or(true),
            use_xtream: !s.settings.keep_m3u,
        }
    }

    /// The stored row changed under the open editor (a refresh upgraded the playlist, the
    /// switch toggle, a rename elsewhere): follow it, keeping what the user edited.
    fn rebase(&mut self, s: &Source) {
        let old = SourceEditor::new(&self.original);
        let new = SourceEditor::new(s);
        rebased(&mut self.name, &old.name, &new.name);
        rebased(&mut self.icon, &old.icon, &new.icon);
        rebased(&mut self.color, &old.color, &new.color);
        rebased(&mut self.url, &old.url, &new.url);
        rebased(&mut self.server, &old.server, &new.server);
        rebased(&mut self.user, &old.user, &new.user);
        rebased(&mut self.ua, &old.ua, &new.ua);
        rebased(&mut self.live_format, &old.live_format, &new.live_format);
        rebased(&mut self.refresh_h, &old.refresh_h, &new.refresh_h);
        rebased(&mut self.tz, &old.tz, &new.tz);
        rebased(&mut self.provider_epg, &old.provider_epg, &new.provider_epg);
        rebased(&mut self.smart_names, &old.smart_names, &new.smart_names);
        rebased(&mut self.import_vod, &old.import_vod, &new.import_vod);
        rebased(&mut self.in_all, &old.in_all, &new.in_all);
        self.use_xtream = new.use_xtream;
        self.original = s.clone();
    }

    /// Whether a field differs from the stored playlist (a typed password counts; "Use the
    /// Xtream login" doesn't: it is saved as it is switched).
    fn is_changed(&self) -> bool {
        let mut stored = SourceEditor::new(&self.original);
        stored.use_xtream = self.use_xtream;
        *self != stored
    }

    /// "Use the Xtream login" applies to playlists that signed in with it and to playlists
    /// the user kept as M3U (other M3U playlists upgrade on their own).
    fn offers_xtream_switch(&self) -> bool {
        let s = &self.original;
        s.m3u_origin().is_some() || (s.kind != SourceKind::Xtream && s.settings.keep_m3u)
    }

    /// The edited source, or the field that is wrong. Also: whether a refresh is needed.
    fn apply(&self) -> Result<(Source, bool), (Local, String)> {
        let mut s = self.original.clone();
        let name = self.name.trim();
        if name.is_empty() {
            return Err((Local::SrcName, "The name can't be empty".into()));
        }
        s.name = name.to_owned();
        s.icon = glyphs::SOURCE_ICONS.get(self.icon).copied().unwrap_or(glyphs::PLAYLIST).to_owned();
        s.color = super::onboarding::COLORS.get(self.color).map(|c| c.0).unwrap_or_default();
        match s.kind {
            SourceKind::Xtream => {
                // an unchanged server is kept as stored: an upgraded playlist's may carry a
                // path prefix that normalizing would strip (DESIGN §27.11)
                let stored = s.xtream.as_ref().map(|x| x.server.clone()).filter(|st| *st == self.server.trim());
                let server = stored
                    .or_else(|| crate::sources::xtream::urls::normalize_server(&self.server))
                    .ok_or((Local::SrcServer, "Enter the server address, e.g. panel.example.com:8080".to_owned()))?;
                if self.user.trim().is_empty() {
                    return Err((Local::SrcUser, "Enter the username".into()));
                }
                let old = s.xtream.clone();
                let password = if self.password.is_empty() {
                    old.as_ref().map(|x| x.password.expose().clone()).unwrap_or_default()
                } else {
                    self.password.clone()
                };
                s.xtream = Some(crate::model::XtreamCredentials {
                    server,
                    username: self.user.trim().to_owned(),
                    password: crate::redact::Secret::new(password),
                });
            }
            SourceKind::M3uUrl => {
                let u = self.url.trim();
                if u.is_empty() {
                    return Err((Local::SrcUrl, "Enter the playlist URL".into()));
                }
                s.url = Some(crate::redact::Secret::new(u.to_owned()));
            }
            SourceKind::M3uFile => {
                let p = self.url.trim();
                if p.is_empty() {
                    return Err((Local::SrcUrl, "Enter the playlist file".into()));
                }
                s.path = Some(PathBuf::from(p));
            }
        }
        s.settings.user_agent = Some(self.ua.trim().to_owned()).filter(|u| !u.is_empty());
        s.settings.live_format = [LiveFormat::Auto, LiveFormat::Ts, LiveFormat::M3u8][self.live_format.min(2)];
        s.settings.refresh_h = (self.refresh_h > 0).then_some(self.refresh_h);
        s.settings.server_tz = Some(self.tz.trim().to_owned()).filter(|t| !t.is_empty());
        // tri-state options stay unset (= default) unless the user changed them
        let tri = |old: Option<bool>, new: bool| if old.unwrap_or(true) == new { old } else { Some(new) };
        s.settings.use_provider_epg = tri(s.settings.use_provider_epg, self.provider_epg);
        s.settings.smart_names = tri(s.settings.smart_names, self.smart_names);
        s.settings.import_vod = tri(s.settings.import_vod, self.import_vod);
        s.settings.in_all = tri(s.settings.in_all, self.in_all);
        let o = &self.original;
        // "Show in All playlists" is a view setting: the library rebuild applies it, no import
        let fetched = |st: &SourceSettings| SourceSettings { in_all: None, ..st.clone() };
        let refresh = s.url != o.url
            || s.path != o.path
            || s.xtream.as_ref().map(|x| (&x.server, &x.username, x.password.expose()))
                != o.xtream.as_ref().map(|x| (&x.server, &x.username, x.password.expose()))
            || fetched(&s.settings) != fetched(&o.settings);
        Ok((s, refresh))
    }
}

/// Orphans ▸ "Attach to…": pick a library item of the same kind.
#[derive(Debug, Clone)]
struct Attach {
    orphan: Orphan,
    filter: String,
    /// (ItemRef, label) candidates for the current filter.
    candidates: Vec<(ItemRef, String)>,
}

/// What a confirm dialog / menu was opened for (echoed tokens / `Action::Digit` codes).
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    DeleteSource(SourceId),
    DeleteEpg(EpgSourceId),
    DeleteList(FavListId),
    ForgetOrphan(ItemRef),
    /// A Replace restore (and whether the backup's settings are applied too).
    Restore(PathBuf, bool),
    RemovePin,
    Menu(Item),
    /// A value change (→, Shift+←/→) on a parental row waiting for the PIN (retried as the
    /// same change).
    Adjust(i32),
    /// "Use the Xtream login" turned off: the playlist replaces the Xtream catalogue.
    UsePlaylist(SourceId),
    /// "Show explicit content" turned on without a PIN: set one first, or show anyway.
    ShowExplicit,
}

const MENU_EDIT: u8 = 1;
const MENU_REFRESH: u8 = 2;
const MENU_TOGGLE: u8 = 3;
const MENU_DELETE: u8 = 4;
const MENU_UP: u8 = 5;
const MENU_DOWN: u8 = 6;
const MENU_RENAME: u8 = 7;
const MENU_CREDENTIALS: u8 = 8;
const MENU_ATTACH: u8 = 9;
const MENU_OFFSET_MINUS: u8 = 10;
const MENU_OFFSET_PLUS: u8 = 11;
const MENU_CATEGORIES: u8 = 12;
const MENU_PIN_FIRST: u8 = 13;
const MENU_SHOW_EXPLICIT: u8 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    Pages,
    #[default]
    Content,
}

#[derive(Default)]
pub struct SettingsScreen {
    page: SettingsPage,
    pane: Pane,
    /// When a click on the page list last opened a page (egui time): at Compact the page
    /// replaces the list, and the second click of a double-click mustn't act on its rows.
    page_clicked_at: Option<f64>,
    /// Focused row (index into the page's rows; only focusable rows are chosen).
    row: usize,
    editing: bool,
    /// Text being edited for a config text row.
    edit_buf: String,
    submit: bool,
    scroll_to_row: bool,
    sources: Vec<Source>,
    sources_loader: Option<Loader<Vec<Source>>>,
    epg_sources: Vec<EpgSource>,
    /// Playlist channels each guide matches (`guide_matches`), read with the guides.
    epg_matches: HashMap<EpgSourceId, usize>,
    epg_loader: Option<Loader<GuideList>>,
    /// Channels with a guide per playlist, for the library build in `.0` (Playlists rows).
    guide_counts: (u64, HashMap<SourceId, usize>),
    reminders: Vec<Reminder>,
    reminders_loader: Option<Loader<Vec<Reminder>>>,
    orphans: Vec<Orphan>,
    orphans_loader: Option<Loader<Vec<Orphan>>>,
    hidden_items: Vec<(ItemRef, String)>,
    hidden_loader: Option<Loader<Vec<(ItemRef, String)>>>,
    /// Hidden ▸ "Titles you separated or pinned".
    title_links: Vec<TitleLink>,
    links_loader: Option<Loader<Vec<TitleLink>>>,
    editor: Option<SourceEditor>,
    attach: Option<Attach>,
    new_epg_url: String,
    new_list_name: String,
    pin_new: String,
    pin_confirm: String,
    /// The form error and its field.
    error: Option<(Local, String)>,
    backup: BackupOptions,
    restore_replace: bool,
    /// Apply the backup's settings on restore; reset to the mode's default (on for Replace,
    /// off for Merge) whenever the mode changes.
    restore_settings: bool,
    backup_job: Option<JobId>,
    backup_status: Option<(bool, String)>,
    pending: Option<Pending>,
    next_token: u64,
    /// The file dialog this screen opened (answers to other screens' dialogs are ignored).
    asked: Option<PickPurpose>,
    /// Drag & drop reorder in progress: (row of the dragged item).
    drag_from: Option<usize>,
    /// Text-field row whose egui focus must be released this frame.
    release_focus: bool,
    /// A guide offset changed with Shift+←/→ that isn't saved yet (and when it last changed).
    offset_pending: Option<(EpgSourceId, std::time::Instant)>,
    /// Hidden channels of the library generation `hidden_built` (Settings ▸ Hidden items).
    hidden_channels: Vec<(ChannelId, String)>,
    /// Hidden ▸ filtered tags whose categories are listed (→ / Shift+←).
    expanded: HashSet<(SourceId, Option<CategoryTag>)>,
    hidden_built: u64,
    /// A playlist kept as M3U that the user asked to sign in with its Xtream login: the
    /// refresh submitted once that choice was saved says whether that worked.
    login_switch: Option<LoginSwitch>,
    /// `Route::EditPlaylist`: open this playlist's editor once the playlists are read.
    edit_on_load: Option<SourceId>,
}

/// "Use the Xtream login" waiting for its refresh. Only the job that runs the refresh submitted
/// after `keep_m3u` was saved answers it: a forced refresh already running read the playlist
/// still kept, so its outcome says nothing about the login (DESIGN §27.3).
struct LoginSwitch {
    source: SourceId,
    /// Resolves once the setting is saved and the refresh submitted.
    ticket: Option<crate::store::Ticket<Submitted>>,
    submitted: Option<Submitted>,
}

impl LoginSwitch {
    /// Whether the finished refresh `id` of this source is the one that tried the login.
    fn answered_by(&mut self, id: JobId) -> bool {
        if let Some(r) = self.ticket.as_mut().and_then(crate::store::Ticket::try_take) {
            self.ticket = None;
            self.submitted = r.ok();
        }
        self.submitted.is_some_and(|s| s.ran_in(id))
    }
}

/// How long Shift+←/→ (→) on a guide's offset must rest before the change is saved and
/// re-imported.
const OFFSET_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// "channel 1" (the name the orphan had) or its key.
fn orphan_name(o: &Orphan) -> String {
    if o.fingerprint.norm_name.is_empty() { o.item.key.to_string() } else { o.fingerprint.norm_name.clone() }
}

impl SettingsScreen {
    fn reload(&mut self, ctx: &UiCtx<'_>) {
        match self.page {
            SettingsPage::Playlists => self.sources_loader = Some(Loader::start(ctx, crate::store::sources::list_sources)),
            SettingsPage::Epg => self.epg_loader = Some(Loader::start(ctx, load_guides)),
            SettingsPage::Reminders => self.reminders_loader = Some(Loader::start(ctx, crate::store::user::all_reminders)),
            SettingsPage::Orphans => self.orphans_loader = Some(Loader::start(ctx, crate::store::user::orphans)),
            SettingsPage::Hidden => {
                self.hidden_loader = Some(Loader::start(ctx, load_hidden_vod));
                self.links_loader = Some(Loader::start(ctx, crate::store::titles::links));
            }
            _ => {}
        }
    }

    /// Whether the page may change under the source editor: not while it has unsaved changes
    /// (switching the page drops the editor), which a toast explains instead.
    fn may_leave_editor(&self, ctx: &mut UiCtx<'_>) -> bool {
        if self.editor.as_ref().is_some_and(SourceEditor::is_changed) {
            ctx.toast(Toast::info("Unsaved playlist changes: Enter on Save, or Esc to discard"));
            return false;
        }
        true
    }

    fn set_page(&mut self, ctx: &mut UiCtx<'_>, page: SettingsPage) {
        if self.page != page {
            self.flush_epg_offset(ctx);
            self.page = page;
            self.row = 0;
            self.editing = false;
            self.editor = None;
            self.attach = None;
            self.error = None;
            self.scroll_to_row = true;
        }
        self.reload(ctx);
    }

    /// Settings ▸ Parental ▸ Explicit content (DESIGN §20.3): what hides it, the switch when
    /// there is no PIN, and — only while it shows — the categories treated as explicit (Enter
    /// toggles "Not explicit" / "Mark as explicit").
    fn explicit_rows(&self, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
        let item = |item: Item, label: String, detail: String| Row::Item {
            item,
            icon: glyphs::EXPLICIT.into(),
            color: None,
            label,
            detail: Some(detail),
            reorder: false,
        };
        let p = &ctx.config.parental;
        v.push(Row::Section("Explicit content".into()));
        if p.pin_active() {
            v.push(Row::Note(
                "Adult categories — named XXX, 18+, Adult, Porn… or flagged by the playlist — are hidden everywhere (lists, guide, search, Home, counts) until the PIN is entered.".into(),
                false,
            ));
        } else {
            v.push(Row::Toggle(&PARENTAL_SHOW_EXPLICIT));
            v.push(Row::Note(
                "Adult categories — named XXX, 18+, Adult, Porn… or flagged by the playlist — are hidden everywhere unless this is on. Set a PIN to keep them behind it instead.".into(),
                false,
            ));
        }
        if ctx.parental.hides_explicit(p) {
            let n = ctx.lib.explicit.groups.len();
            if n > 0 {
                let how = if p.pin_active() { "Enter the PIN to review them" } else { "Show explicit content to review them" };
                v.push(Row::Note(format!("{n} {} hidden as explicit. {how}.", if n == 1 { "category is" } else { "categories are" }), false));
            }
            return;
        }
        let several = ctx.lib.sources.iter().filter(|s| s.enabled).nth(1).is_some();
        let mut any = false;
        for g in all_groups(ctx).filter(|g| g.explicit.is_listed()) {
            any = true;
            use crate::model::Explicit;
            let why = match g.explicit {
                Explicit::Name => "its name",
                Explicit::Provider => "the playlist says so",
                Explicit::Marked => "marked by you",
                Explicit::Cleared | Explicit::No => "not explicit (your choice)",
            };
            let action = if g.explicit.is_explicit() { "Enter: not explicit" } else { "Enter: explicit again" };
            let source = if several { ctx.lib.source(g.source_id).map(|s| format!(" · {}", s.name)).unwrap_or_default() } else { String::new() };
            v.push(item(Item::Explicit(g.id), g.display_name().to_owned(), format!("{}{source} · {why} · {action}", kind_name(g.kind))));
        }
        if !any {
            v.push(Row::Note("No category is treated as explicit. Mark one from its list's menu (right-click).".into(), false));
        }
    }

    /// Enter on a category of the explicit list: toggle the user's word.
    fn toggle_explicit_item(&mut self, ctx: &mut UiCtx<'_>, id: GroupId) {
        let g = all_groups(ctx).find(|g| g.id == id).cloned();
        if let Some(g) = g {
            super::live::toggle_group_explicit(ctx, &g);
        }
    }

    /// "Show explicit content" turned on without a PIN: recommend setting one first.
    fn offer_pin_for_explicit(&mut self, ctx: &mut UiCtx<'_>) {
        let mi = |icon: &'static str, label: &str, code: u8| MenuItem {
            icon,
            label: label.into(),
            action: Action::Digit(code),
            enabled: true,
            shortcut: None,
        };
        self.pending = Some(Pending::ShowExplicit);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec {
            title: "Show explicit content?".into(),
            items: vec![
                mi(glyphs::LOCK, "Set a PIN first (recommended)", MENU_PIN_FIRST),
                mi(glyphs::EXPLICIT, "Show without a PIN", MENU_SHOW_EXPLICIT),
            ],
        }));
    }

    fn parental_locked(&self, ctx: &UiCtx<'_>) -> bool {
        ctx.config.parental.pin_hash.is_some() && !ctx.parental.is_unlocked(&ctx.config.parental) && ctx.config.parental.enabled
    }

    fn token(&mut self, p: Pending) -> u64 {
        self.next_token += 1;
        self.pending = Some(p);
        self.next_token
    }

    // -------------------------------------------------------------- page rows

    fn rows(&self, ctx: &UiCtx<'_>) -> Vec<Row> {
        if let Some(a) = &self.attach {
            return self.attach_rows(a);
        }
        if let Some(e) = &self.editor {
            return self.editor_rows(ctx, e);
        }
        let mut v = Vec::new();
        let item = |item: Item, icon: &str, label: String, detail: Option<String>| Row::Item {
            item,
            icon: icon.to_owned(),
            color: None,
            label,
            detail,
            reorder: false,
        };
        match self.page {
            SettingsPage::Playlists => {
                v.push(Row::Section("Playlists".into()));
                if self.sources.is_empty() && self.sources_loader.is_none() {
                    v.push(Row::Note("No playlists yet.".into(), false));
                }
                let jobs = ctx.services.jobs.active();
                for s in &self.sources {
                    let running = jobs.iter().find(|j| matches!(j.kind, JobKind::RefreshSource { source, .. } if source == s.id));
                    let mut detail =
                        source_detail(s, ctx.now, running.map(|j| super::onboarding::phase_text(&j.phase)), self.guide_counts.1.get(&s.id).copied());
                    if let Some(rule) = &s.settings.category_filter {
                        detail.push_str(&format!(" · Showing {}", categories::showing_text(ctx.lib, s.id, Some(rule))));
                    }
                    v.push(Row::Item {
                        item: Item::Source(s.id),
                        icon: if s.icon.is_empty() { glyphs::PLAYLIST.to_owned() } else { s.icon.clone() },
                        color: Some(s.color),
                        label: s.name.clone(),
                        detail: Some(detail),
                        reorder: true,
                    });
                }
                // playlists that look the same (glyph and colour): a one-click fix, never silent
                let looks = self.looks();
                let alike = crate::ui::scope::look_alikes(&looks, |c| ctx.theme.source_color(c));
                for g in &alike {
                    let names: Vec<&str> = g.iter().filter_map(|i| self.sources.get(*i)).map(|s| s.name.as_str()).collect();
                    let verb = if names.len() == 2 { "look the same" } else { "all look the same" };
                    v.push(Row::Note(format!("{} {verb} on cards and in lists", crate::ui::scope::names_and(&names)), false));
                }
                if !alike.is_empty() {
                    v.push(Row::Act { act: Act::DistinctColours, icon: glyphs::THEME, label: "Give each its own colour".into(), danger: false });
                }
                v.push(Row::Act { act: Act::AddSource, icon: glyphs::ADD, label: "Add playlist…".into(), danger: false });
                v.push(Row::Act { act: Act::AddFreeChannels, icon: glyphs::LIVE_TV, label: "Add free channels…".into(), danger: false });
                v.push(Row::Section("Refresh".into()));
                v.extend(PLAYLIST_CFG_N.iter().map(Row::Number));
                v.push(Row::Choice(&PLAYLIST_CFG_C));
            }
            SettingsPage::Epg => {
                v.push(Row::Section("Guides (first with data wins)".into()));
                if self.epg_sources.is_empty() && self.epg_loader.is_none() {
                    v.push(Row::Note("No guides yet. Xtream logins and playlists with url-tvg add theirs automatically.".into(), false));
                }
                for e in &self.epg_sources {
                    let kind = match &e.kind {
                        EpgSourceKind::Url { url } => crate::redact::redact_url(url.expose()),
                        EpgSourceKind::File { path } => path.display().to_string(),
                        EpgSourceKind::Provider { source_id, .. } => {
                            format!("from {}", ctx.lib.source(*source_id).map(|s| s.name.as_str()).unwrap_or("a playlist"))
                        }
                    };
                    let mut parts = vec![kind];
                    if !e.enabled {
                        parts.insert(0, "Disabled".into());
                    }
                    if e.offset_min != 0 {
                        parts.push(format!("offset {:+} min", e.offset_min));
                    }
                    // coverage first: a guide that lists 2 of 11 249 channels must say so
                    if e.last_refresh.is_some() {
                        parts.insert(
                            usize::from(!e.enabled),
                            coverage_text(e, self.epg_matches.get(&e.id).copied().unwrap_or(0), ctx.lib.live.channels.len()),
                        );
                    }
                    match (&e.last_error, e.last_refresh) {
                        (Some(err), _) => parts.push(format!("Failed: {}", crate::redact::redact(err))),
                        (None, Some(t)) => parts.push(format!("Updated {}", kit::ago(ctx.now, t))),
                        (None, None) => parts.push("Not imported yet".into()),
                    }
                    v.push(Row::Item {
                        item: Item::Epg(e.id),
                        icon: glyphs::GUIDE.to_owned(),
                        color: None,
                        label: e.name.clone(),
                        detail: Some(parts.join(" · ")),
                        reorder: true,
                    });
                }
                if let Some(note) = coverage_note(&self.epg_sources, self.guide_counts.1.values().sum(), ctx.lib.live.channels.len()) {
                    v.push(Row::Note(note, true));
                }
                v.push(Row::Local { field: Local::NewEpgUrl, label: "Add an XMLTV URL" });
                v.push(Row::Act { act: Act::AddEpgUrl, icon: glyphs::ADD, label: "Add URL".into(), danger: false });
                v.push(Row::Act { act: Act::AddEpgFile, icon: glyphs::FOLDER, label: "Add a guide file…".into(), danger: false });
                v.push(Row::Act { act: Act::EpgMapping, icon: glyphs::GUIDE, label: "EPG mapping…".into(), danger: false });
                v.push(Row::Section("Options".into()));
                v.extend(EPG_N.iter().map(Row::Number));
                v.push(Row::Toggle(&EPG_T));
            }
            SettingsPage::Player => {
                v.push(Row::Section("Playback".into()));
                v.push(Row::Choice(&PLAYER_HWDEC));
                v.extend(PLAYER_T.iter().map(Row::Toggle));
                v.extend(PLAYER_N[..4].iter().map(Row::Number));
                v.push(Row::Section("Seeking & resume".into()));
                v.extend(PLAYER_N[4..6].iter().map(Row::Number));
                v.extend(PLAYER_N[8..10].iter().map(Row::Number));
                v.push(Row::Number(&PLAYER_N[7]));
                v.push(Row::Section("Languages & subtitles".into()));
                v.extend(PLAYER_TEXT[..2].iter().map(Row::Text));
                v.push(Row::Choice(&PLAYER_SUB));
                v.push(Row::Number(&PLAYER_N[6]));
                v.push(Row::Section("Network".into()));
                v.push(Row::Text(&PLAYER_TEXT[2]));
                v.push(Row::Text(&PLAYER_TEXT[3]));
                v.push(Row::Section("Pop-out (Hyprland)".into()));
                v.extend(PLAYER_POPOUT.iter().map(Row::Choice));
                v.push(Row::Note(
                    "o in the player (Ctrl+Shift+O anywhere) pops it out into a small window on every workspace. Omarchy's Super+O works too.".into(),
                    false,
                ));
            }
            SettingsPage::Interface => {
                v.push(Row::Section("Layout".into()));
                v.extend(IFACE_C[..3].iter().map(Row::Choice));
                v.push(Row::Number(&IFACE_N[2]));
                v.extend(IFACE_T[..2].iter().map(Row::Toggle));
                v.push(Row::Toggle(&IFACE_T[12]));
                v.push(Row::Choice(&IFACE_C[8]));
                v.push(Row::Choice(&IFACE_C[7]));
                v.push(Row::Toggle(&IFACE_T[5]));
                v.push(Row::Section("Playback".into()));
                v.push(Row::Toggle(&IFACE_T[2]));
                v.push(Row::Number(&IFACE_N[0]));
                v.extend(IFACE_T[3..5].iter().map(Row::Toggle));
                v.push(Row::Number(&IFACE_N[1]));
                v.push(Row::Section("Sorting".into()));
                v.extend(IFACE_C[3..6].iter().map(Row::Choice));
                v.push(Row::Toggle(&IFACE_T[10]));
                // only for someone with two playlists of movies or series (DESIGN §29.14)
                if [ItemKind::Movie, ItemKind::Series].iter().any(|k| ctx.lib.eligible(*k).len() > 1) {
                    v.push(Row::Section("Several playlists".into()));
                    v.extend(STACK_C.iter().map(Row::Choice));
                    v.push(Row::Note(
                        "In All playlists, copies of one movie or series are one card; your audio language plays first. Space on a card shows each copy."
                            .into(),
                        false,
                    ));
                }
                v.push(Row::Section("Look".into()));
                v.push(Row::Toggle(&IFACE_T[6]));
                v.push(Row::Text(&IFACE_TEXT[0]));
                v.push(Row::Toggle(&IFACE_T[7]));
                v.push(Row::Text(&IFACE_TEXT[1]));
                v.push(Row::Section("Desktop".into()));
                v.push(Row::Toggle(&IFACE_T[8]));
                v.push(Row::Number(&IFACE_N[3]));
                v.push(Row::Toggle(&IFACE_T[9]));
                v.push(Row::Choice(&IFACE_C[6]));
            }
            SettingsPage::Favourites => {
                v.push(Row::Section("Favourites lists".into()));
                for l in &ctx.favs.lists {
                    // explicit titles parental control hides don't count (DESIGN §20.3)
                    let n = ctx.favs.items(l.id).iter().filter(|r| !ctx.lib.conceals(r)).count();
                    let detail = if l.is_default { format!("{n} items · the default list (Ctrl+F)") } else { format!("{n} items") };
                    v.push(Row::Item {
                        item: Item::List(l.id),
                        icon: if l.icon.is_empty() { glyphs::FAV_LIST.into() } else { l.icon.clone() },
                        color: None,
                        label: l.name.clone(),
                        detail: Some(detail),
                        reorder: true,
                    });
                }
                v.push(Row::Local { field: Local::NewListName, label: "New list" });
                v.push(Row::Act { act: Act::NewList, icon: glyphs::ADD, label: "Create list".into(), danger: false });
            }
            SettingsPage::Reminders => {
                v.push(Row::Section("Pending reminders".into()));
                if self.reminders.is_empty() && self.reminders_loader.is_none() {
                    v.push(Row::Note("No reminders. Press r on a future programme in the guide.".into(), false));
                }
                // explicit channels' reminders while parental control hides them (DESIGN §20.3)
                for r in self.reminders.iter().filter(|r| !ctx.lib.conceals(&r.channel)) {
                    v.push(item(
                        Item::Reminder(r.id),
                        glyphs::REMINDER,
                        r.programme_title.clone(),
                        Some(format!("{} · {} · {}", r.channel_name, kit::day_time(r.start, ctx.config.interface.clock_24h), kit::date(r.start))),
                    ));
                }
            }
            SettingsPage::Parental => {
                let has_pin = ctx.config.parental.pin_hash.is_some();
                v.push(Row::Section("Parental lock".into()));
                v.push(Row::Note("Locked groups, channels and titles ask for the PIN before they open or play, and are hidden from search while locked. A 4-digit PIN is a speed bump for children, not security.".into(), false));
                if has_pin {
                    v.push(Row::Toggle(&PARENTAL_ENABLED));
                    v.push(Row::Number(&PARENTAL_RELOCK));
                    v.push(Row::Act { act: Act::LockNow, icon: glyphs::LOCK, label: "Lock now".into(), danger: false });
                    v.push(Row::Section("Change PIN".into()));
                } else {
                    v.push(Row::Section("Set a PIN (4–8 digits)".into()));
                }
                v.push(Row::Local { field: Local::PinNew, label: "New PIN" });
                v.push(Row::Local { field: Local::PinConfirm, label: "Repeat the PIN" });
                v.push(Row::Act {
                    act: Act::SetPin,
                    icon: glyphs::LOCK,
                    label: if has_pin { "Change PIN".into() } else { "Set PIN".into() },
                    danger: false,
                });
                if has_pin {
                    v.push(Row::Act { act: Act::RemovePin, icon: glyphs::UNLOCK, label: "Remove PIN…".into(), danger: true });
                }
                v.push(Row::Note("Lock a group, channel or title from its list with Ctrl+L.".into(), false));
                self.explicit_rows(ctx, &mut v);
            }
            SettingsPage::Backup => {
                v.push(Row::Section("Export".into()));
                v.push(Row::Local { field: Local::BackupPasswords, label: "Include passwords" });
                v.push(Row::Local { field: Local::BackupHistory, label: "Include watch history" });
                v.push(Row::Act { act: Act::ExportBackup, icon: glyphs::BACKUP, label: "Export backup…".into(), danger: false });
                v.push(Row::Section("Restore".into()));
                v.push(Row::Local { field: Local::RestoreMode, label: "When restoring" });
                v.push(Row::Local { field: Local::RestoreSettings, label: "Restore settings" });
                v.push(Row::Act { act: Act::ImportBackup, icon: glyphs::FOLDER, label: "Restore a backup…".into(), danger: self.restore_replace });
                if let Some((ok, msg)) = &self.backup_status {
                    v.push(Row::Note(msg.clone(), !ok));
                }
            }
            SettingsPage::Hidden => {
                self.filtered_rows(ctx, &mut v);
                v.push(Row::Section("Hidden groups".into()));
                // only what the user hid: the category filter's are listed above
                let groups: Vec<&Group> = all_groups(ctx).filter(|g| g.user_hidden).collect();
                if groups.is_empty() {
                    v.push(Row::Note("Nothing hidden.".into(), false));
                }
                for g in groups {
                    v.push(item(
                        Item::HiddenGroup(g.id),
                        glyphs::HIDDEN,
                        g.display_name().to_owned(),
                        Some(format!("{} · Enter unhides", kind_name(g.kind))),
                    ));
                }
                v.push(Row::Section("Hidden channels".into()));
                if self.hidden_channels.is_empty() {
                    v.push(Row::Note("Nothing hidden.".into(), false));
                }
                for (id, name) in &self.hidden_channels {
                    v.push(item(Item::HiddenChannel(*id), glyphs::HIDDEN, name.clone(), Some("Enter unhides".into())));
                }
                v.push(Row::Section("Hidden movies & series".into()));
                if self.hidden_items.is_empty() && self.hidden_loader.is_none() {
                    v.push(Row::Note("Nothing hidden.".into(), false));
                }
                for (r, name) in self.hidden_items.iter().filter(|(r, _)| !ctx.lib.conceals(r)) {
                    v.push(item(Item::HiddenVod(r.clone()), glyphs::HIDDEN, name.clone(), Some("Enter unhides".into())));
                }
                // DESIGN §29.14: "Not the same title" and "Always play from" — for someone with
                // two playlists of movies or series, or with corrections to list
                let multi = [ItemKind::Movie, ItemKind::Series].iter().any(|k| ctx.lib.eligible(*k).len() > 1);
                if multi || !self.title_links.is_empty() {
                    v.push(Row::Section("Titles you separated or pinned".into()));
                }
                if multi && self.title_links.is_empty() && self.links_loader.is_none() {
                    v.push(Row::Note("Nothing changed — “Not the same title” on a copy of a movie or series shows it on its own.".into(), false));
                }
                for l in self.title_links.iter().filter(|l| !ctx.lib.conceals(&l.item)) {
                    let playlist = ctx.lib.source(l.item.source).map(|s| s.name.clone()).unwrap_or_else(|| "a removed playlist".into());
                    let name = l.name.clone().unwrap_or_else(|| l.item.key.to_string());
                    let (icon, detail) = match l.action {
                        LinkAction::Split => (glyphs::CLOSE, format!("{playlist} · shown on its own · Enter combines it again")),
                        LinkAction::Pin => (glyphs::PIN, format!("{playlist} · always plays first · Enter undoes")),
                    };
                    v.push(item(Item::TitleLink(l.item.clone(), l.action), icon, name, Some(detail)));
                }
            }
            SettingsPage::Orphans => {
                v.push(Row::Section("Saved data of items that vanished".into()));
                v.push(Row::Note("A refresh couldn't find these again. Attach them to the item they became, or forget them.".into(), false));
                if self.orphans.is_empty() && self.orphans_loader.is_none() {
                    v.push(Row::Note("No orphans.".into(), false));
                }
                // explicit titles aren't named while explicit content is hidden (DESIGN §20.3)
                let hide = ctx.parental.hides_explicit(&ctx.config.parental);
                let concealed = if hide { self.orphans.iter().filter(|o| o.explicit).count() } else { 0 };
                for o in self.orphans.iter().filter(|o| !(hide && o.explicit)) {
                    let detail = format!("{} · {} · since {}", o.item.kind.as_str(), o.holders.join(", "), kit::date(o.since));
                    v.push(item(Item::Orphan(o.item.clone()), glyphs::WARNING, orphan_name(o), Some(detail)));
                }
                if concealed > 0 {
                    let how = if ctx.config.parental.pin_active() { "Enter the PIN to review them" } else { "Show explicit content to review them" };
                    v.push(Row::Note(format!("{concealed} more hidden as explicit. {how}."), false));
                }
            }
            SettingsPage::About => {
                let p = &ctx.services.paths;
                v.push(Row::Section("Tuner".into()));
                v.push(Row::Note(format!("Version {}", env!("CARGO_PKG_VERSION")), false));
                v.push(Row::Note(format!("Settings: {}", p.config_file().display()), false));
                v.push(Row::Note(format!("Data: {}", p.data_dir.display()), false));
                v.push(Row::Note(format!("Cache: {}", p.cache_dir.display()), false));
                v.push(Row::Note(format!("Log: {}", p.log_file().display()), false));
                v.push(Row::Section("Logs".into()));
                v.push(Row::Toggle(&IFACE_T[11]));
                v.push(Row::Act { act: Act::OpenLogs, icon: glyphs::INFO, label: "Open the log viewer".into(), danger: false });
                v.push(Row::Act { act: Act::ExportLog, icon: glyphs::BACKUP, label: "Export log…".into(), danger: false });
            }
        }
        v
    }

    /// Hidden ▸ "Hidden by the category filter" (DESIGN §28.5): per filtered playlist, a row
    /// per hidden tag (biggest first; → lists its categories), then "Choose countries…"; a
    /// playlist with several tags and nothing filtered gets just "Choose countries…".
    fn filtered_rows(&self, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
        let mut by_tag: HashMap<(SourceId, Option<CategoryTag>), Vec<&Group>> = HashMap::new();
        for g in all_groups(ctx).filter(|g| g.filter == GroupFilter::Hidden) {
            by_tag.entry((g.source_id, g.tag.clone())).or_default().push(g);
        }
        let multi = categories::multi_tag_sources(ctx.lib);
        for src in &ctx.lib.sources {
            let mut tags: Vec<(&Option<CategoryTag>, &Vec<&Group>)> =
                by_tag.iter().filter(|((s, _), _)| *s == src.id).map(|((_, t), groups)| (t, groups)).collect();
            if tags.is_empty() {
                // a playlist in several countries / languages that shows them all: the way in
                if multi.contains(&src.id) {
                    v.push(Row::Section(format!("{}: countries and languages", src.name)));
                    v.push(Row::Item {
                        item: Item::Categories(src.id),
                        icon: glyphs::EARTH.to_owned(),
                        color: None,
                        label: "Choose countries…".into(),
                        detail: Some("All countries and languages are shown — hide the ones you don't watch".into()),
                        reorder: false,
                    });
                }
                continue;
            }
            let items = |gs: &[&Group]| gs.iter().map(|g| g.item_count as u64).sum::<u64>();
            tags.sort_by(|a, b| items(b.1).cmp(&items(a.1)).then_with(|| tag_name(a.0.as_ref()).cmp(&tag_name(b.0.as_ref()))));
            v.push(Row::Section(format!("{}: hidden by the category filter", src.name)));
            for (tag, groups) in tags {
                let open = self.expanded.contains(&(src.id, tag.clone()));
                v.push(Row::Item {
                    item: Item::HiddenTag(src.id, tag.clone()),
                    icon: if open { glyphs::CHEVRON_DOWN } else { glyphs::CHEVRON_RIGHT }.to_owned(),
                    color: None,
                    label: tag_name(tag.as_ref()),
                    detail: Some(format!(
                        "{} · Enter shows · {}",
                        hidden_tag_detail(groups),
                        if open { "Shift+← hides the list" } else { "→ lists them" }
                    )),
                    reorder: false,
                });
                if open {
                    // the tag leads each detail: these rows belong to the one above
                    let name = tag_name(tag.as_ref());
                    for g in groups {
                        v.push(Row::Item {
                            item: Item::HiddenGroup(g.id),
                            icon: glyphs::HIDDEN.to_owned(),
                            color: None,
                            label: g.display_name().to_owned(),
                            detail: Some(format!("{name} · {} · Enter shows just this one", kind_name(g.kind))),
                            reorder: false,
                        });
                    }
                }
            }
            v.push(Row::Item {
                item: Item::Categories(src.id),
                icon: glyphs::EARTH.to_owned(),
                color: None,
                label: "Choose countries…".into(),
                detail: None,
                reorder: false,
            });
        }
    }

    fn editor_rows(&self, ctx: &UiCtx<'_>, e: &SourceEditor) -> Vec<Row> {
        let mut v = vec![Row::Section(format!("Edit {}", e.original.name))];
        let l = |field, label| Row::Local { field, label };
        v.extend([l(Local::SrcName, "Name"), l(Local::SrcIcon, "Icon"), l(Local::SrcColor, "Colour")]);
        match e.original.kind {
            SourceKind::Xtream => {
                v.extend([l(Local::SrcServer, "Server"), l(Local::SrcUser, "Username"), l(Local::SrcPassword, "Password (empty = keep)")])
            }
            SourceKind::M3uUrl => v.push(l(Local::SrcUrl, "Playlist URL")),
            SourceKind::M3uFile => v.push(l(Local::SrcUrl, "Playlist file")),
        }
        if e.offers_xtream_switch() {
            v.push(l(Local::SrcUseXtream, "Use the Xtream login"));
            let note = if e.original.kind == SourceKind::Xtream {
                "Signed in with the Xtream login this playlist carries: movies, series, catch-up and account info. Turn off to use the playlist as it is."
            } else {
                "This playlist carries an Xtream login. Turn on to sign in with it for movies, series, catch-up and account info."
            };
            v.push(Row::Note(note.into(), false));
        }
        v.push(Row::Section("Options".into()));
        v.push(l(Local::SrcUa, "User-Agent (empty = default)"));
        if e.original.kind == SourceKind::Xtream {
            v.push(l(Local::SrcLiveFormat, "Live format"));
            v.push(l(Local::SrcTz, "Server timezone (empty = from account)"));
            v.push(l(Local::SrcImportVod, "Import movies & series"));
        }
        v.push(l(Local::SrcRefresh, "Refresh every (h, 0 = default)"));
        v.push(l(Local::SrcProviderEpg, "Use the provider's guide"));
        v.push(l(Local::SrcSmartNames, "Smart names"));
        // several playlists (DESIGN §29): off keeps e.g. free channels out of Live TV's All
        v.push(l(Local::SrcInAll, "Show in All playlists"));
        v.push(Row::Note(
            "Off: this playlist only shows on its own tab in Movies, Series, Live TV and the guide. Your favourites and history still list it."
                .into(),
            false,
        ));
        // the stored rule (the picker writes it; the editor's Save keeps it)
        let rule = self.sources.iter().find(|s| s.id == e.original.id).and_then(|s| s.settings.category_filter.as_ref());
        v.push(Row::Item {
            item: Item::Categories(e.original.id),
            icon: glyphs::EARTH.to_owned(),
            color: None,
            label: "Categories…".into(),
            detail: Some(categories::showing_text(ctx.lib, e.original.id, rule)),
            reorder: false,
        });
        if let Some(a) = &e.original.status.account {
            v.push(Row::Section("Account".into()));
            for (line, bad) in super::onboarding::account_lines(a) {
                v.push(Row::Note(line, bad));
            }
        }
        v.push(Row::Section(String::new()));
        v.push(Row::Act { act: Act::SaveSource, icon: glyphs::CHECK, label: "Save".into(), danger: false });
        v.push(Row::Act { act: Act::RefreshSource, icon: glyphs::REFRESH, label: "Refresh now".into(), danger: false });
        v.push(Row::Act { act: Act::DeleteSource, icon: glyphs::CLOSE, label: "Delete playlist…".into(), danger: true });
        v.push(Row::Act { act: Act::CloseEditor, icon: glyphs::PREVIOUS, label: "Back (discard changes)".into(), danger: false });
        v
    }

    fn attach_rows(&self, a: &Attach) -> Vec<Row> {
        let mut v = vec![Row::Section("Attach to…".into())];
        let o = &a.orphan;
        v.push(Row::Note(format!("{} ({}) — pick the item it became", orphan_name(o), o.item.kind.as_str()), false));
        v.push(Row::Local { field: Local::AttachFilter, label: "Filter" });
        for (i, (_, label)) in a.candidates.iter().enumerate().take(200) {
            v.push(Row::Item {
                item: Item::AttachTarget(i),
                icon: glyphs::CHEVRON_RIGHT.into(),
                color: None,
                label: label.clone(),
                detail: None,
                reorder: false,
            });
        }
        if a.candidates.is_empty() {
            v.push(Row::Note("No matches".into(), false));
        }
        v.push(Row::Act { act: Act::CloseAttach, icon: glyphs::PREVIOUS, label: "Cancel".into(), danger: false });
        v
    }

    fn focusable(rows: &[Row]) -> Vec<usize> {
        rows.iter().enumerate().filter(|(_, r)| r.focusable()).map(|(i, _)| i).collect()
    }

    /// Clamp `self.row` onto a focusable row.
    fn clamp_row(&mut self, rows: &[Row]) {
        let f = Self::focusable(rows);
        if f.is_empty() {
            self.row = 0;
        } else if !f.contains(&self.row) {
            self.row = f.iter().copied().find(|i| *i >= self.row).unwrap_or(*f.last().unwrap_or(&0));
        }
    }

    fn move_row(&mut self, rows: &[Row], delta: isize) {
        let f = Self::focusable(rows);
        if f.is_empty() {
            return;
        }
        let cur = f.iter().position(|i| *i == self.row).unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, f.len() as isize - 1) as usize;
        self.row = f[next];
        self.editing = false;
        self.scroll_to_row = true;
    }

    // -------------------------------------------------------------- local fields

    fn local_text(&mut self, f: Local) -> Option<&mut String> {
        Some(match f {
            Local::NewEpgUrl => &mut self.new_epg_url,
            Local::NewListName => &mut self.new_list_name,
            Local::PinNew => &mut self.pin_new,
            Local::PinConfirm => &mut self.pin_confirm,
            Local::AttachFilter => &mut self.attach.as_mut()?.filter,
            _ => {
                let e = self.editor.as_mut()?;
                match f {
                    Local::SrcName => &mut e.name,
                    Local::SrcUrl => &mut e.url,
                    Local::SrcServer => &mut e.server,
                    Local::SrcUser => &mut e.user,
                    Local::SrcPassword => &mut e.password,
                    Local::SrcUa => &mut e.ua,
                    Local::SrcTz => &mut e.tz,
                    _ => return None,
                }
            }
        })
    }

    fn local_toggle(&mut self, f: Local) -> Option<&mut bool> {
        Some(match f {
            Local::BackupPasswords => &mut self.backup.include_credentials,
            Local::BackupHistory => &mut self.backup.include_history,
            Local::RestoreSettings => &mut self.restore_settings,
            Local::SrcProviderEpg => &mut self.editor.as_mut()?.provider_epg,
            Local::SrcSmartNames => &mut self.editor.as_mut()?.smart_names,
            Local::SrcImportVod => &mut self.editor.as_mut()?.import_vod,
            Local::SrcInAll => &mut self.editor.as_mut()?.in_all,
            Local::SrcUseXtream => &mut self.editor.as_mut()?.use_xtream,
            _ => return None,
        })
    }

    /// Value text + options count of a local choice/number field.
    fn local_value(&self, f: Local) -> String {
        let e = self.editor.as_ref();
        match f {
            Local::RestoreMode => if self.restore_replace { "Replace everything" } else { "Merge with what's here" }.into(),
            Local::SrcIcon => e.and_then(|e| glyphs::SOURCE_ICONS.get(e.icon)).copied().unwrap_or("").to_owned(),
            Local::SrcColor => e.and_then(|e| super::onboarding::COLORS.get(e.color)).map(|c| c.1).unwrap_or("").to_owned(),
            Local::SrcLiveFormat => e.map(|e| ["Auto", "TS", "M3U8"][e.live_format.min(2)]).unwrap_or("").to_owned(),
            Local::SrcRefresh => e.map(|e| if e.refresh_h == 0 { "default".to_owned() } else { format!("{} h", e.refresh_h) }).unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn adjust_local(&mut self, f: Local, dir: i32) {
        let cycle = |v: &mut usize, n: usize| *v = ((*v as i32 + dir).rem_euclid(n as i32)) as usize;
        match f {
            Local::RestoreMode => {
                self.restore_replace = !self.restore_replace;
                self.restore_settings = self.restore_replace;
            }
            Local::SrcIcon => {
                if let Some(e) = &mut self.editor {
                    cycle(&mut e.icon, glyphs::SOURCE_ICONS.len());
                }
            }
            Local::SrcColor => {
                if let Some(e) = &mut self.editor {
                    cycle(&mut e.color, super::onboarding::COLORS.len());
                }
            }
            Local::SrcLiveFormat => {
                if let Some(e) = &mut self.editor {
                    cycle(&mut e.live_format, 3);
                }
            }
            Local::SrcRefresh => {
                if let Some(e) = &mut self.editor {
                    e.refresh_h = (e.refresh_h as i32 + dir).clamp(0, 168) as u32;
                }
            }
            f if f.is_toggle() => {
                if let Some(b) = self.local_toggle(f) {
                    *b = !*b;
                }
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------- actions

    /// A value change on a row: → / Shift+→ / L (`dir` 1), Shift+← / H (`dir` -1). Plain ←
    /// never comes here: it steps back to the page list.
    fn adjust(&mut self, ctx: &mut UiCtx<'_>, row: &Row, dir: i32) {
        if self.page == SettingsPage::Parental && self.parental_locked(ctx) && !matches!(row, Row::Local { .. }) {
            self.pending = Some(Pending::Adjust(dir));
            kit::ask_pin(ctx, Action::Activate, "Enter the PIN to change parental settings");
            return;
        }
        match row {
            Row::Toggle(t) if std::ptr::eq(*t, &PARENTAL_SHOW_EXPLICIT) && dir > 0 && !ctx.config.parental.show_explicit => {
                self.offer_pin_for_explicit(ctx);
            }
            Row::Toggle(t) => {
                let set = t.set;
                let on = dir > 0;
                if (t.get)(ctx.config) != on {
                    ctx.edit_config(move |c| set(c, on));
                }
            }
            // relative to the config the edit is applied to: several presses within one
            // frame (key repeat) all count
            Row::Choice(c) => {
                let (n, get, set) = (c.options.len() as i32, c.get, c.set);
                ctx.edit_config(move |cfg| {
                    let i = (get(cfg) as i32 + dir).rem_euclid(n) as usize;
                    set(cfg, i);
                });
            }
            Row::Number(n) => {
                let (get, set, step, min, max) = (n.get, n.set, n.step, n.min, n.max);
                ctx.edit_config(move |c| {
                    let v = (get(c) + step * f64::from(dir)).clamp(min, max);
                    set(c, ((v / step).round() * step).clamp(min, max));
                });
            }
            Row::Local { field: Local::SrcUseXtream, .. } => self.set_use_xtream(ctx, dir > 0),
            Row::Local { field, .. } => self.adjust_local(*field, dir),
            Row::Item { item: Item::Epg(id), .. } => self.epg_offset(ctx, *id, dir * 15),
            Row::Item { item: Item::HiddenTag(src, tag), .. } => {
                let key = (*src, tag.clone());
                if dir > 0 {
                    self.expanded.insert(key);
                } else {
                    self.expanded.remove(&key);
                }
            }
            _ => {}
        }
    }

    /// Enter/Space on a row (acted on after this frame's fields were drawn).
    fn activate(&mut self, ctx: &mut UiCtx<'_>, row: &Row) {
        match row {
            Row::Toggle(t) => {
                let cur = (t.get)(ctx.config);
                if std::ptr::eq(*t, &PARENTAL_ENABLED) && !cur && ctx.config.parental.pin_hash.is_none() {
                    ctx.toast(Toast::info("Set a PIN first"));
                    return;
                }
                self.adjust(ctx, row, if cur { -1 } else { 1 });
            }
            Row::Choice(_) | Row::Number(_) => self.adjust(ctx, row, 1),
            Row::Text(t) => {
                if self.editing {
                    self.commit_text(ctx, t);
                    self.editing = false;
                } else {
                    self.edit_buf = (t.get)(ctx.config);
                    self.editing = true;
                }
            }
            Row::Local { field, .. } if field.is_text() => {
                if self.editing {
                    self.editing = false;
                    self.commit_local(ctx, *field);
                } else {
                    self.editing = true;
                }
            }
            Row::Local { field: Local::SrcUseXtream, .. } => {
                let on = self.editor.as_ref().is_some_and(|e| e.use_xtream);
                self.set_use_xtream(ctx, !on);
            }
            Row::Local { field, .. } => self.adjust_local(*field, 1),
            Row::Act { act, .. } => self.act(ctx, *act),
            Row::Item { item, .. } => self.item_default(ctx, item.clone()),
            Row::Section(_) | Row::Note(..) => {}
        }
    }

    /// Save the text being edited into its config row (only when it changed).
    fn commit_text(&mut self, ctx: &mut UiCtx<'_>, t: &CfgText) {
        let v = std::mem::take(&mut self.edit_buf);
        if v != (t.get)(ctx.config) {
            let set = t.set;
            ctx.edit_config(move |c| set(c, v));
        }
    }

    /// Enter in a local text field: the form's natural next step.
    fn commit_local(&mut self, ctx: &mut UiCtx<'_>, f: Local) {
        match f {
            Local::NewEpgUrl => self.act(ctx, Act::AddEpgUrl),
            Local::NewListName => self.act(ctx, Act::NewList),
            Local::PinConfirm => self.act(ctx, Act::SetPin),
            Local::AttachFilter => self.refilter_attach(ctx),
            _ => {}
        }
    }

    /// (glyph, colour) of each playlist, as the marks draw them (look-alike check).
    fn looks(&self) -> Vec<(&str, SourceColor)> {
        self.sources.iter().map(|s| (badge::kind_glyph(&s.icon, s.kind), s.color)).collect()
    }

    fn act(&mut self, ctx: &mut UiCtx<'_>, act: Act) {
        match act {
            Act::DistinctColours => {
                let theme = ctx.theme;
                let fixes = crate::ui::scope::distinct_colors(&self.looks(), |c| theme.source_color(c));
                let changed: Vec<Source> = fixes
                    .into_iter()
                    .filter_map(|(i, c)| {
                        let s = self.sources.get_mut(i)?;
                        s.color = c;
                        Some(s.clone())
                    })
                    .collect();
                let n = changed.len();
                let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
                    for s in &changed {
                        crate::store::sources::update_source(c, s)?;
                    }
                    Ok(())
                });
                ctx.toast(Toast::info(if n == 1 {
                    "Gave 1 playlist its own colour".to_owned()
                } else {
                    format!("Gave {n} playlists their own colours")
                }));
            }
            Act::AddSource => ctx.push(Route::Onboarding(OnboardingStart::AddSource)),
            Act::AddFreeChannels => ctx.push(Route::Onboarding(OnboardingStart::FreeChannels)),
            Act::EpgMapping => ctx.push(Route::EpgMapping { channel: None }),
            Act::OpenLogs => ctx.push(Route::Logs),
            // the app writes the log when the dialog answers
            Act::ExportLog => kit::pick_file(ctx, PickPurpose::ExportLog),
            Act::AddEpgFile => self.ask_file(ctx, PickPurpose::OpenEpg),
            Act::AddEpgUrl => {
                let url = self.new_epg_url.trim().to_owned();
                if url.is_empty() {
                    self.error = Some((Local::NewEpgUrl, "Enter the guide URL".into()));
                    return;
                }
                let url = if url.contains("://") { url } else { format!("http://{url}") };
                let name = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_else(|| "Guide".into());
                self.add_epg(ctx, name, EpgSourceKind::Url { url: crate::redact::Secret::new(url) });
                self.new_epg_url.clear();
                self.error = None;
            }
            Act::NewList => {
                let name = self.new_list_name.trim().to_owned();
                if name.is_empty() {
                    self.error = Some((Local::NewListName, "Enter a name".into()));
                    return;
                }
                let _ = ctx
                    .services
                    .store
                    .writer()
                    .submit_notify(DataScope::Favourites, move |c| crate::store::user::create_fav_list(c, &name, glyphs::FAV_LIST).map(|_| ()));
                self.new_list_name.clear();
                self.error = None;
                ctx.toast(Toast::info("List created"));
            }
            Act::SetPin => self.set_pin(ctx),
            Act::RemovePin => {
                if self.parental_locked(ctx) {
                    kit::ask_pin(ctx, Action::Activate, "Enter the PIN to remove it");
                    return;
                }
                let token = self.token(Pending::RemovePin);
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: "Remove the PIN?".into(),
                    body: "Locked items open without a PIN until you set one again. Explicit content stays hidden unless you show it.".into(),
                    confirm_label: "Remove PIN".into(),
                    destructive: true,
                    token,
                }));
            }
            Act::LockNow => {
                ctx.parental.lock();
                ctx.toast(Toast::info("Locked"));
            }
            Act::ExportBackup => self.ask_file(ctx, PickPurpose::ExportBackup),
            Act::ImportBackup => self.ask_file(ctx, PickPurpose::ImportBackup),
            Act::SaveSource => self.save_source(ctx),
            Act::RefreshSource => {
                if let Some(e) = &self.editor {
                    ctx.services.jobs.submit(JobKind::RefreshSource { source: e.original.id, force: true });
                    ctx.toast(Toast::info(format!("Refreshing {}", e.original.name)));
                }
            }
            Act::DeleteSource => {
                if let Some(e) = &self.editor {
                    let (id, name) = (e.original.id, e.original.name.clone());
                    self.confirm_delete_source(ctx, id, &name);
                }
            }
            Act::CloseEditor => {
                self.editor = None;
                self.error = None;
                self.row = 0;
            }
            Act::CloseAttach => {
                self.attach = None;
                self.row = 0;
            }
        }
    }

    fn ask_file(&mut self, ctx: &UiCtx<'_>, purpose: PickPurpose) {
        self.asked = Some(purpose);
        kit::pick_file(ctx, purpose);
    }

    fn add_epg(&mut self, ctx: &mut UiCtx<'_>, name: String, kind: EpgSourceKind) {
        let jobs = ctx.services.jobs.clone();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| {
            let id = crate::store::sources::insert_epg_source(c, &name, &kind, 0)?;
            jobs.submit(JobKind::RefreshEpg { epg_source: id, force: true });
            Ok(())
        });
        ctx.toast(Toast::info("Guide added — importing in the background"));
    }

    fn set_pin(&mut self, ctx: &mut UiCtx<'_>) {
        if ctx.config.parental.pin_hash.is_some() && !ctx.parental.is_unlocked(&ctx.config.parental) {
            kit::ask_pin(ctx, Action::Activate, "Enter the current PIN first");
            return;
        }
        let pin = self.pin_new.trim().to_owned();
        if !(4..=8).contains(&pin.len()) || !pin.chars().all(|c| c.is_ascii_digit()) {
            self.error = Some((Local::PinNew, "Use 4–8 digits".into()));
            return;
        }
        if pin != self.pin_confirm.trim() {
            self.error = Some((Local::PinConfirm, "The PINs don't match".into()));
            self.pin_confirm.clear();
            return;
        }
        let salt = new_salt();
        let cfg = crate::config::ParentalConfig {
            enabled: true,
            pin_hash: Some(hash_pin(&pin, &salt)),
            pin_salt: Some(salt),
            relock_min: ctx.config.parental.relock_min,
            show_explicit: ctx.config.parental.show_explicit,
        };
        // the session that set the PIN stays unlocked (relock rules apply from here)
        ctx.parental.unlock(&pin, &cfg);
        ctx.edit_config(move |c| c.parental = cfg);
        self.pin_new.clear();
        self.pin_confirm.clear();
        self.error = None;
        ctx.toast(Toast::info("PIN set — parental lock is on"));
    }

    fn save_source(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(e) = &self.editor else { return };
        match e.apply() {
            Err((f, msg)) => {
                self.error = Some((f, msg));
                let rows = self.rows(ctx);
                if let Some(i) = rows.iter().position(|r| matches!(r, Row::Local { field, .. } if *field == f)) {
                    self.row = i;
                    self.editing = f.is_text();
                }
            }
            Ok((src, refresh)) => {
                let id = src.id;
                let name = src.name.clone();
                let jobs = ctx.services.jobs.clone();
                let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
                    crate::store::sources::update_source(c, &src)?;
                    if refresh {
                        jobs.submit(JobKind::RefreshSource { source: id, force: true });
                    }
                    Ok(())
                });
                ctx.toast(Toast::info(if refresh { format!("Saved {name} — refreshing") } else { format!("Saved {name}") }));
                self.editor = None;
                self.error = None;
                self.row = 0;
            }
        }
    }

    /// Editor ▸ "Use the Xtream login" (DESIGN §27.3): save `keep_m3u` now and run a forced
    /// refresh, which performs the switch (both ways). Turning it on for a playlist that is
    /// still M3U is checked when that refresh finishes; switching an Xtream source back to its
    /// playlist drops what only the login had, so it asks first.
    fn set_use_xtream(&mut self, ctx: &mut UiCtx<'_>, on: bool) {
        let Some(e) = &self.editor else { return };
        if !e.offers_xtream_switch() || e.use_xtream == on {
            return;
        }
        if !on && e.original.kind == SourceKind::Xtream {
            let token = self.token(Pending::UsePlaylist(e.original.id));
            ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                title: "Use the playlist instead of the Xtream login?".into(),
                body: "Movies and series not in the playlist will be removed. Favourites carry over where the playlist has them.".into(),
                confirm_label: "Use playlist".into(),
                destructive: false,
                token,
            }));
            return;
        }
        self.apply_use_xtream(ctx, on);
    }

    fn apply_use_xtream(&mut self, ctx: &mut UiCtx<'_>, on: bool) {
        let Some(e) = &mut self.editor else { return };
        e.use_xtream = on;
        // a later Save keeps it (and doesn't count it as a change)
        e.original.settings.keep_m3u = !on;
        let (id, name) = (e.original.id, e.original.name.clone());
        let check = on && e.original.kind != SourceKind::Xtream;
        let jobs = ctx.services.jobs.clone();
        let ticket = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
            let mut settings = crate::store::sources::get_source(c, id)?.settings;
            settings.keep_m3u = !on;
            crate::store::sources::set_source_settings(c, id, &settings)?;
            // submitted once saved (autocommit): the job that runs it reads the new setting
            Ok(jobs.submit_tracked(JobKind::RefreshSource { source: id, force: true }))
        });
        // (the refresh writes before it can finish: the ticket is resolved by then)
        self.login_switch = check.then(|| LoginSwitch { source: id, ticket: Some(ticket), submitted: None });
        ctx.toast(Toast::info(if on {
            format!("Signing in to {name} with its Xtream login…")
        } else {
            format!("Switching {name} back to its playlist…")
        }));
    }

    /// The open editor follows its stored row. Across a kind change (a silent upgrade, the
    /// switch) the fields differ: an edit in progress ends and the cursor stays on its field,
    /// else goes to the name.
    fn rebase_editor(&mut self, ctx: &UiCtx<'_>, s: &Source) {
        let Some(e) = &self.editor else { return };
        let kind_changed = s.kind != e.original.kind;
        let field = match self.editor_rows(ctx, e).get(self.row) {
            Some(Row::Local { field, .. }) => Some(*field),
            _ => None,
        };
        if let Some(e) = &mut self.editor {
            e.rebase(s);
        }
        if kind_changed && self.attach.is_none() {
            self.editing = false;
            let rows = self.editor.as_ref().map(|e| self.editor_rows(ctx, e)).unwrap_or_default();
            let at = |f: Local| rows.iter().position(|r| matches!(r, Row::Local { field, .. } if *field == f));
            self.row = field.and_then(at).or_else(|| at(Local::SrcName)).unwrap_or(0);
        }
    }

    fn confirm_delete_source(&mut self, ctx: &mut UiCtx<'_>, id: SourceId, name: &str) {
        let token = self.token(Pending::DeleteSource(id));
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: format!("Delete {name}?"),
            body: "Its channels, movies, series and guide are removed. Favourites and progress are kept as orphans in case you add it again.".into(),
            confirm_label: "Delete".into(),
            destructive: true,
            token,
        }));
    }

    /// Shift+←/→ (→) on a guide: shift its offset. Applied (saved + the guide re-imported, the
    /// offset applies at import time) once the presses stop for [`OFFSET_SETTLE`], or when the
    /// page is left — not once per press.
    fn epg_offset(&mut self, ctx: &mut UiCtx<'_>, id: EpgSourceId, delta_min: i32) {
        let Some(e) = self.epg_sources.iter_mut().find(|e| e.id == id) else { return };
        e.offset_min = (e.offset_min + delta_min).clamp(-24 * 60, 24 * 60);
        if self.offset_pending.is_some_and(|(p, _)| p != id) {
            self.flush_epg_offset(ctx);
        }
        self.offset_pending = Some((id, std::time::Instant::now()));
    }

    /// Write a pending offset change now.
    fn flush_epg_offset(&mut self, ctx: &mut UiCtx<'_>) {
        let Some((id, _)) = self.offset_pending.take() else { return };
        let Some(src) = self.epg_sources.iter().find(|e| e.id == id).cloned() else { return };
        let jobs = ctx.services.jobs.clone();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| {
            crate::store::sources::update_epg_source(c, &src)?;
            jobs.submit(JobKind::RefreshEpg { epg_source: src.id, force: true });
            Ok(())
        });
        ctx.toast(Toast::info("Guide offset saved — re-importing the guide"));
    }

    /// Enter on an item row.
    fn item_default(&mut self, ctx: &mut UiCtx<'_>, item: Item) {
        match item {
            Item::Source(id) => {
                if let Some(s) = self.sources.iter().find(|s| s.id == id) {
                    if needs_credentials(s) {
                        ctx.push(Route::Onboarding(OnboardingStart::NeedsCredentials(id)));
                    } else {
                        self.editor = Some(SourceEditor::new(s));
                        self.row = 0;
                        self.error = None;
                    }
                }
            }
            Item::Epg(id) => self.toggle_epg(ctx, id),
            Item::List(id) => self.rename_list(ctx, id),
            Item::HiddenGroup(g) => self.unhide_group(ctx, g),
            Item::HiddenTag(src, tag) => {
                self.expanded.remove(&(src, tag.clone()));
                categories::show_tag(ctx, src, tag);
            }
            Item::Categories(src) => ctx.open(OverlayRequest::Categories(src)),
            Item::HiddenChannel(c) => {
                self.unhide_item(ctx, ctx.lib.live.get(c).map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }))
            }
            Item::HiddenVod(r) => {
                self.hidden_items.retain(|(x, _)| *x != r);
                self.unhide_item(ctx, Some(r));
            }
            Item::TitleLink(r, action) => {
                self.title_links.retain(|l| !(l.item == r && l.action == action));
                let now = ctx.now;
                let write = r.clone();
                let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| match action {
                    LinkAction::Split => crate::store::titles::set_split(c, &write, false, now),
                    LinkAction::Pin => crate::store::titles::set_pin(c, &write, &[], false, now),
                });
                ctx.toast(Toast::info(match action {
                    LinkAction::Split => "Combined again",
                    LinkAction::Pin => "Plays from the best copy again",
                }));
            }
            Item::Orphan(r) => self.start_attach(ctx, &r),
            Item::AttachTarget(i) => self.attach_to(ctx, i),
            Item::Reminder(_) => self.item_menu(ctx, item),
            Item::Explicit(id) => self.toggle_explicit_item(ctx, id),
        }
    }

    fn toggle_epg(&mut self, ctx: &mut UiCtx<'_>, id: EpgSourceId) {
        let Some(e) = self.epg_sources.iter_mut().find(|e| e.id == id) else { return };
        e.enabled = !e.enabled;
        let src = e.clone();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| {
            crate::store::sources::update_epg_source(c, &src)?;
            crate::store::epg::resolve_mappings(c)
        });
    }

    fn rename_list(&mut self, ctx: &mut UiCtx<'_>, id: FavListId) {
        let name = ctx.favs.lists.iter().find(|l| l.id == id).map(|l| l.name.clone()).unwrap_or_default();
        ctx.open(OverlayRequest::Rename(RenameSpec { title: "Rename list".into(), initial: name, target: RenameTarget::FavList(id) }));
    }

    /// Show a group again; one the category filter hides becomes an exception to it
    /// (`GroupOverride::shown`, DESIGN §28.3).
    fn unhide_group(&mut self, ctx: &mut UiCtx<'_>, g: GroupId) {
        let Some(group) = all_groups(ctx).find(|x| x.id == g) else { return };
        let (r, exempt) = (GroupRef::from(group), group.filter != GroupFilter::Kept);
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::show_group(c, &r, exempt));
        ctx.toast(Toast::info("Shown again"));
    }

    fn unhide_item(&mut self, ctx: &mut UiCtx<'_>, item: Option<ItemRef>) {
        let Some(item) = item else { return };
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let mut ov = crate::store::user::item_override(c, &item)?.unwrap_or_default();
            ov.hidden = false;
            crate::store::user::set_item_override(c, &item, &ov)
        });
        ctx.toast(Toast::info("Shown again"));
    }

    fn start_attach(&mut self, ctx: &mut UiCtx<'_>, r: &ItemRef) {
        // gone meanwhile (reattached by a refresh, forgotten): nothing to attach
        let Some(o) = self.orphans.iter().find(|o| o.item == *r).cloned() else { return };
        let filter = o.fingerprint.norm_name.clone();
        self.attach = Some(Attach { orphan: o, filter, candidates: Vec::new() });
        self.refilter_attach(ctx);
        self.row = 0;
    }

    /// Library items of the orphan's kind matching the filter (best first, ≤ 200).
    fn refilter_attach(&mut self, ctx: &UiCtx<'_>) {
        let Some(a) = &mut self.attach else { return };
        let all: Vec<(ItemRef, String)> = match a.orphan.item.kind {
            ItemKind::Channel => ctx
                .lib
                .live
                .channels
                .iter()
                .map(|c| (ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }, c.name.to_string()))
                .collect(),
            ItemKind::Movie => ctx.lib.vod.movies.iter().map(|v| (super::home::vod_ref(v), v.name.to_string())).collect(),
            ItemKind::Series => ctx.lib.vod.series.iter().map(|v| (super::home::vod_ref(v), v.name.to_string())).collect(),
            ItemKind::Episode => Vec::new(),
        };
        let mut f = crate::search::ListFilter::default();
        for c in a.filter.chars() {
            f.push(c);
        }
        a.candidates = f.apply(all.iter().map(|(_, n)| n.as_str())).into_iter().take(200).map(|i| all[i as usize].clone()).collect();
    }

    fn attach_to(&mut self, ctx: &mut UiCtx<'_>, i: usize) {
        let Some(a) = &self.attach else { return };
        let Some((to, name)) = a.candidates.get(i) else { return };
        let (from, to) = (a.orphan.item.clone(), to.clone());
        let _ = ctx.services.store.writer().submit_notify(DataScope::All, move |c| crate::store::user::reassign(c, &from, &to));
        ctx.toast(Toast::info(format!("Attached to {name}")));
        self.attach = None;
        self.row = 0;
    }

    /// x / Delete / right-click: the row's menu.
    fn item_menu(&mut self, ctx: &mut UiCtx<'_>, item: Item) {
        let mi = |icon: &'static str, label: &str, code: u8| MenuItem {
            icon,
            label: label.into(),
            action: Action::Digit(code),
            enabled: true,
            shortcut: None,
        };
        let (title, items) = match item {
            Item::Source(id) => {
                let Some(s) = self.sources.iter().find(|s| s.id == id) else { return };
                let mut v = vec![mi(glyphs::SETTINGS, "Edit…", MENU_EDIT), mi(glyphs::REFRESH, "Refresh now", MENU_REFRESH)];
                if needs_credentials(s) {
                    v.insert(0, mi(glyphs::ACCOUNT, "Enter credentials…", MENU_CREDENTIALS));
                }
                v.push(mi(if s.enabled { glyphs::HIDDEN } else { glyphs::CHECK }, if s.enabled { "Disable" } else { "Enable" }, MENU_TOGGLE));
                v.push(mi(glyphs::TAG, "Rename…", MENU_RENAME));
                v.push(mi(glyphs::EARTH, "Countries & languages…", MENU_CATEGORIES));
                v.extend([
                    mi(glyphs::REORDER, "Move up", MENU_UP),
                    mi(glyphs::REORDER, "Move down", MENU_DOWN),
                    mi(glyphs::CLOSE, "Delete…", MENU_DELETE),
                ]);
                (s.name.clone(), v)
            }
            Item::Epg(id) => {
                let Some(e) = self.epg_sources.iter().find(|e| e.id == id) else { return };
                let mut v = vec![
                    mi(glyphs::REFRESH, "Refresh now", MENU_REFRESH),
                    mi(if e.enabled { glyphs::HIDDEN } else { glyphs::CHECK }, if e.enabled { "Disable" } else { "Enable" }, MENU_TOGGLE),
                    mi(glyphs::PREVIOUS, "Offset −15 min", MENU_OFFSET_MINUS),
                    mi(glyphs::NEXT, "Offset +15 min", MENU_OFFSET_PLUS),
                    mi(glyphs::REORDER, "Move up (higher priority)", MENU_UP),
                    mi(glyphs::REORDER, "Move down", MENU_DOWN),
                ];
                if !matches!(e.kind, EpgSourceKind::Provider { .. }) {
                    v.push(mi(glyphs::CLOSE, "Delete…", MENU_DELETE));
                }
                (e.name.clone(), v)
            }
            Item::List(id) => {
                let Some(l) = ctx.favs.lists.iter().find(|l| l.id == id) else { return };
                let mut v = vec![
                    mi(glyphs::TAG, "Rename…", MENU_RENAME),
                    mi(glyphs::REORDER, "Move up", MENU_UP),
                    mi(glyphs::REORDER, "Move down", MENU_DOWN),
                ];
                let mut del = mi(glyphs::CLOSE, "Delete…", MENU_DELETE);
                del.enabled = !l.is_default;
                v.push(del);
                (l.name.clone(), v)
            }
            // Delete cancels a reminder directly (DESIGN §13.9); no one-item menu
            Item::Reminder(id) => {
                self.cancel_reminder(ctx, id);
                return;
            }
            Item::Orphan(_) => {
                ("Orphan".into(), vec![mi(glyphs::CHEVRON_RIGHT, "Attach to…", MENU_ATTACH), mi(glyphs::CLOSE, "Forget…", MENU_DELETE)])
            }
            Item::HiddenGroup(_) | Item::HiddenChannel(_) | Item::HiddenVod(_) | Item::HiddenTag(..) => {
                ("Hidden".into(), vec![mi(glyphs::CHECK, "Unhide", MENU_TOGGLE)])
            }
            Item::TitleLink(..) => ("Title".into(), vec![mi(glyphs::CHECK, "Undo", MENU_TOGGLE)]),
            Item::Explicit(id) => {
                let Some(g) = all_groups(ctx).find(|g| g.id == id) else { return };
                (g.display_name().to_owned(), vec![super::live::explicit_menu_item(g, MENU_TOGGLE)])
            }
            Item::AttachTarget(_) | Item::Categories(_) => return,
        };
        self.pending = Some(Pending::Menu(item));
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    fn menu_choice(&mut self, ctx: &mut UiCtx<'_>, item: Item, code: u8) {
        match (item, code) {
            (Item::Source(id), MENU_EDIT) => self.item_default(ctx, Item::Source(id)),
            (Item::Source(id), MENU_CATEGORIES) => ctx.open(OverlayRequest::Categories(id)),
            (Item::Source(id), MENU_CREDENTIALS) => ctx.push(Route::Onboarding(OnboardingStart::NeedsCredentials(id))),
            (Item::Source(id), MENU_REFRESH) => {
                ctx.services.jobs.submit(JobKind::RefreshSource { source: id, force: true });
            }
            (Item::Source(id), MENU_TOGGLE) => {
                if let Some(s) = self.sources.iter_mut().find(|s| s.id == id) {
                    s.enabled = !s.enabled;
                    let src = s.clone();
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::update_source(c, &src));
                }
            }
            (Item::Source(id), MENU_RENAME) => {
                let name = self.sources.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
                ctx.open(OverlayRequest::Rename(RenameSpec { title: "Rename playlist".into(), initial: name, target: RenameTarget::Source(id) }));
            }
            (Item::Source(id), MENU_DELETE) => {
                let name = self.sources.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
                self.confirm_delete_source(ctx, id, &name);
            }
            (Item::Epg(id), MENU_REFRESH) => {
                ctx.services.jobs.submit(JobKind::RefreshEpg { epg_source: id, force: true });
            }
            (Item::Epg(id), MENU_TOGGLE) => self.toggle_epg(ctx, id),
            (Item::Epg(id), MENU_OFFSET_MINUS) => self.epg_offset(ctx, id, -15),
            (Item::Epg(id), MENU_OFFSET_PLUS) => self.epg_offset(ctx, id, 15),
            (Item::Epg(id), MENU_DELETE) => {
                let name = self.epg_sources.iter().find(|e| e.id == id).map(|e| e.name.clone()).unwrap_or_default();
                let token = self.token(Pending::DeleteEpg(id));
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Delete {name}?"),
                    body: "Its programmes are removed; channels fall back to the next guide.".into(),
                    confirm_label: "Delete".into(),
                    destructive: true,
                    token,
                }));
            }
            (Item::List(id), MENU_RENAME) => self.rename_list(ctx, id),
            (Item::List(id), MENU_DELETE) => {
                let name = ctx.favs.lists.iter().find(|l| l.id == id).map(|l| l.name.clone()).unwrap_or_default();
                let token = self.token(Pending::DeleteList(id));
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Delete the list {name}?"),
                    body: "The channels stay in your playlists.".into(),
                    confirm_label: "Delete".into(),
                    destructive: true,
                    token,
                }));
            }
            (item, MENU_UP) | (item, MENU_DOWN) => self.reorder(ctx, item, if code == MENU_UP { -1 } else { 1 }),
            (Item::Reminder(id), MENU_DELETE) => self.cancel_reminder(ctx, id),
            (Item::Explicit(id), MENU_TOGGLE) => self.toggle_explicit_item(ctx, id),
            (Item::Orphan(r), MENU_ATTACH) => self.start_attach(ctx, &r),
            (Item::Orphan(r), MENU_DELETE) => {
                let token = self.token(Pending::ForgetOrphan(r));
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: "Forget this item's data?".into(),
                    body: "Its favourites, progress and settings are deleted.".into(),
                    confirm_label: "Forget".into(),
                    destructive: true,
                    token,
                }));
            }
            (
                item @ (Item::HiddenGroup(_) | Item::HiddenChannel(_) | Item::HiddenVod(_) | Item::HiddenTag(..) | Item::TitleLink(..)),
                MENU_TOGGLE,
            ) => self.item_default(ctx, item),
            _ => {}
        }
    }

    fn cancel_reminder(&mut self, ctx: &mut UiCtx<'_>, id: ReminderId) {
        self.reminders.retain(|r| r.id != id);
        let _ = ctx.services.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id));
        ctx.toast(Toast::info("Reminder cancelled"));
    }

    /// Alt+↑/↓ / K/J / drag: move an item within its list and persist the order.
    fn reorder(&mut self, ctx: &mut UiCtx<'_>, item: Item, delta: isize) {
        fn moved<T: Copy + PartialEq>(order: &mut Vec<T>, x: T, delta: isize) -> bool {
            let Some(i) = order.iter().position(|y| *y == x) else { return false };
            let j = (i as isize + delta).clamp(0, order.len() as isize - 1) as usize;
            if i == j {
                return false;
            }
            let v = order.remove(i);
            order.insert(j, v);
            true
        }
        match item {
            Item::Source(id) => {
                let mut order: Vec<SourceId> = self.sources.iter().map(|s| s.id).collect();
                if moved(&mut order, id, delta) {
                    self.sources.sort_by_key(|s| order.iter().position(|x| *x == s.id));
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::reorder_sources(c, &order));
                }
            }
            Item::Epg(id) => {
                let mut order: Vec<EpgSourceId> = self.epg_sources.iter().map(|s| s.id).collect();
                if moved(&mut order, id, delta) {
                    self.epg_sources.sort_by_key(|s| order.iter().position(|x| *x == s.id));
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| {
                        crate::store::sources::reorder_epg_sources(c, &order)?;
                        crate::store::epg::resolve_mappings(c)
                    });
                }
            }
            Item::List(id) => {
                let mut order: Vec<FavListId> = ctx.favs.lists.iter().map(|l| l.id).collect();
                if moved(&mut order, id, delta) {
                    ctx.favs.lists.sort_by_key(|l| order.iter().position(|x| *x == l.id));
                    let _ =
                        ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::reorder_fav_lists(c, &order));
                }
            }
            _ => return,
        }
        // keep the focus on the moved item
        let rows = self.rows(ctx);
        if let Some(i) = rows.iter().position(|r| matches!(r, Row::Item { item: x, .. } if *x == item)) {
            self.row = i;
            self.scroll_to_row = true;
        }
    }

    fn current_row(&self, ctx: &UiCtx<'_>) -> Option<Row> {
        self.rows(ctx).get(self.row).cloned()
    }
}

static PARENTAL_ENABLED: CfgToggle = toggle_row!("Parental lock", parental.enabled);
static PARENTAL_SHOW_EXPLICIT: CfgToggle = toggle_row!("Show explicit content", parental.show_explicit);

/// Every group of the library (live, movie and series categories).
fn all_groups<'l>(ctx: &'l UiCtx<'_>) -> impl Iterator<Item = &'l Group> {
    ctx.lib.live.groups.iter().chain(&ctx.lib.vod.movie_groups).chain(&ctx.lib.vod.series_groups)
}

fn kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Channel => "Live TV",
        ItemKind::Movie => "Movies",
        _ => "Series",
    }
}

/// "112 categories · 4 380 channels · 12 movies" of the groups a filtered tag hides.
fn hidden_tag_detail(groups: &[&Group]) -> String {
    let count = |k: ItemKind| groups.iter().filter(|g| g.kind == k).map(|g| g.item_count as usize).sum::<usize>();
    let n = groups.len();
    let mut parts = vec![format!("{} {}", grouped(n), if n == 1 { "category" } else { "categories" })];
    for (k, one, many) in [(ItemKind::Channel, "channel", "channels"), (ItemKind::Movie, "movie", "movies"), (ItemKind::Series, "series", "series")] {
        match count(k) {
            0 => {}
            1 => parts.push(format!("1 {one}")),
            c => parts.push(format!("{} {many}", grouped(c))),
        }
    }
    parts.join(" · ")
}

/// A restored Xtream source without its password.
fn needs_credentials(s: &Source) -> bool {
    s.xtream.as_ref().is_some_and(|x| x.password.expose().is_empty())
}

/// The guides with the playlist channels each one matches.
pub(crate) type GuideList = (Vec<EpgSource>, HashMap<EpgSourceId, usize>);

/// Settings ▸ EPG / onboarding read (off the UI thread): the guides and their matches.
pub(crate) fn load_guides(c: &rusqlite::Connection) -> crate::error::Result<GuideList> {
    Ok((crate::store::sources::list_epg_sources(c)?, guide_matches(c)?))
}

/// How many channels of enabled playlists each guide matches (automatic or manual mappings to
/// a channel of its current data). A guide that lists 2 channels of a 11 249-channel playlist
/// imports fine; this is what tells the user it covers almost nothing.
pub(crate) fn guide_matches(c: &rusqlite::Connection) -> crate::error::Result<HashMap<EpgSourceId, usize>> {
    let mut stmt = c.prepare_cached(
        "SELECT m.epg_source_id, count(*) FROM channel_epg_map m
         JOIN channel ch ON ch.source_id = m.source_id AND ch.key = m.channel_key
         JOIN source s ON s.id = ch.source_id AND s.enabled = 1
         JOIN epg_source es ON es.id = m.epg_source_id
         JOIN epg_channel ec ON ec.epg_source_id = m.epg_source_id AND ec.xmltv_id = m.xmltv_id AND ec.seen_gen >= es.active_gen
         WHERE m.orphaned_since IS NULL
         GROUP BY m.epg_source_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, EpgSourceId>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, n) = row?;
        out.insert(id, usize::try_from(n).unwrap_or(0));
    }
    Ok(out)
}

/// "Matches 3 of 11 249 channels · guide lists 2 channels, 312 programmes".
pub(crate) fn coverage_text(e: &EpgSource, matched: usize, total: usize) -> String {
    let n = |v: u32| grouped(v as usize);
    format!(
        "Matches {} of {} channels · guide lists {} channel{}, {} programme{}",
        grouped(matched.min(total)),
        grouped(total),
        n(e.channel_count),
        if e.channel_count == 1 { "" } else { "s" },
        n(e.programme_count),
        if e.programme_count == 1 { "" } else { "s" }
    )
}

/// A note under the guides when they cover less than half of the `total` channels, with the
/// way forward (both rows are on this page).
fn coverage_note(guides: &[EpgSource], covered: usize, total: usize) -> Option<String> {
    if total == 0 || !guides.iter().any(|g| g.enabled && g.last_refresh.is_some()) {
        return None;
    }
    (covered * 2 < total).then(|| {
        format!(
            "Your guides cover {} of {} channels. Add an XMLTV guide that lists your channels, or map them in EPG mapping (below).",
            grouped(covered),
            grouped(total)
        )
    })
}

/// "Xtream · 9 812 ch (3 with guide) · 48 211 movies · Updated 2 h ago · Active, expires
/// 2027-01-01".
fn source_detail(s: &Source, now: crate::model::Timestamp, running: Option<String>, with_guide: Option<usize>) -> String {
    let kind = match s.kind {
        SourceKind::Xtream if s.m3u_origin().is_some() => "Xtream (from M3U)",
        SourceKind::Xtream => "Xtream",
        SourceKind::M3uUrl => "M3U URL",
        SourceKind::M3uFile => "M3U file",
    };
    let mut parts = vec![kind.to_owned()];
    if !s.enabled {
        parts.insert(0, "Disabled".into());
    }
    if needs_credentials(s) {
        parts.insert(0, "Needs credentials".into());
    }
    let c = s.status.counts;
    let channels = c.channels as usize;
    match with_guide {
        Some(g) if channels > 0 => parts.push(format!("{} ch ({} with guide)", grouped(channels), grouped(g.min(channels)))),
        _ => parts.push(format!("{} ch", grouped(channels))),
    }
    if c.movies > 0 {
        parts.push(format!("{} movies", grouped(c.movies as usize)));
    }
    if c.series > 0 {
        parts.push(format!("{} series", grouped(c.series as usize)));
    }
    match (running, &s.status.last_error, s.status.last_refresh) {
        (Some(p), _, _) => parts.push(format!("Refreshing: {p}")),
        (None, Some(e), _) => {
            parts.push(format!("Failed {}: {}", s.status.last_attempt.map(|t| kit::ago(now, t)).unwrap_or_default(), crate::redact::redact(e)))
        }
        (None, None, Some(t)) => parts.push(format!("Updated {}", kit::ago(now, t))),
        (None, None, None) => parts.push("Never refreshed".into()),
    }
    if let Some(a) = &s.status.account {
        match a.status {
            AccountStatus::Expired => parts.push(a.exp_date.map(|d| format!("Expired on {}", kit::date(d))).unwrap_or_else(|| "Expired".into())),
            AccountStatus::Banned | AccountStatus::Disabled => parts.push("Account unusable".into()),
            _ => {
                if let Some(d) = a.exp_date {
                    parts.push(format!("expires {}", kit::date(d)));
                }
                if let Some(m) = a.max_connections {
                    parts.push(format!("{}/{m} connections", a.active_cons.unwrap_or(0)));
                }
            }
        }
    }
    parts.join(" · ")
}

/// Hidden movies / series (item overrides with `hidden`), named from the catalogue.
fn load_hidden_vod(c: &rusqlite::Connection) -> crate::error::Result<Vec<(ItemRef, String)>> {
    let mut out = Vec::new();
    for r in
        crate::store::user::item_overrides(c, None)?.into_iter().filter(|r| r.ov.hidden && matches!(r.item.kind, ItemKind::Movie | ItemKind::Series))
    {
        let name = match r.item.kind {
            ItemKind::Movie => crate::store::vod::resolve_movie(c, &r.item)?
                .map(|id| crate::store::vod::movie(c, id))
                .transpose()?
                .map(|m| m.display_name().to_owned()),
            _ => None,
        };
        let name = name
            .or(r.ov.rename.clone())
            .unwrap_or_else(|| if r.fingerprint.norm_name.is_empty() { r.item.key.to_string() } else { r.fingerprint.norm_name.clone() });
        out.push((r.item, name));
    }
    Ok(out)
}

impl Screen for SettingsScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Settings
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.editing { vec![KeyContext::TextInput, KeyContext::Panel] } else { vec![KeyContext::Panel] }
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let page = match route {
            Route::Settings(p) => *p,
            Route::EditPlaylist(_) => SettingsPage::Playlists,
            _ => self.page,
        };
        self.flush_epg_offset(ctx);
        self.set_page(ctx, page);
        self.editing = false;
        // a scope tab's "Edit playlist…" (DESIGN §29): its editor, once the list is read
        if let Route::EditPlaylist(id) = route {
            self.editor = None;
            self.attach = None;
            self.edit_on_load = Some(*id);
        }
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(scope) => {
                let relevant = match scope {
                    DataScope::Sources | DataScope::Live | DataScope::Vod => self.page == SettingsPage::Playlists,
                    DataScope::Epg => self.page == SettingsPage::Epg,
                    DataScope::Reminders => self.page == SettingsPage::Reminders,
                    DataScope::Overrides => matches!(self.page, SettingsPage::Hidden | SettingsPage::Orphans),
                    DataScope::All => true,
                    _ => false,
                };
                // an open source editor follows its row (a silent upgrade, the switch toggle)
                if relevant && (self.editor.is_none() || matches!(scope, DataScope::Sources | DataScope::All)) {
                    self.reload(ctx);
                }
            }
            // the forced refresh after "Use the Xtream login" was turned on (DESIGN §27.3)
            AppEvent::Job(JobEvent::Finished { id, kind: JobKind::RefreshSource { source, force: true }, outcome, .. })
                if self.login_switch.as_mut().is_some_and(|w| w.source == *source && w.answered_by(*id)) =>
            {
                self.login_switch = None;
                if matches!(outcome, Ok(JobOutcome::SourceRefreshed { upgraded: false, .. })) {
                    ctx.toast(Toast::error("Couldn't sign in with this playlist's Xtream login — still using the playlist"));
                }
                // still a playlist: it stays kept (no upgrade attempts behind the user's back) and
                // the toggle stays offered
                if !matches!(outcome, Ok(JobOutcome::SourceRefreshed { upgraded: true, .. })) {
                    let id = *source;
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
                        let s = crate::store::sources::get_source(c, id)?;
                        if s.kind != SourceKind::Xtream && !s.settings.keep_m3u {
                            crate::store::sources::set_source_settings(c, id, &crate::model::SourceSettings { keep_m3u: true, ..s.settings })?;
                        }
                        Ok(())
                    });
                }
                if self.editor.is_none() {
                    self.reload(ctx);
                }
            }
            AppEvent::Job(JobEvent::Finished { kind: JobKind::RefreshSource { .. } | JobKind::RefreshEpg { .. }, .. }) => {
                if self.editor.is_none() {
                    self.reload(ctx);
                }
            }
            AppEvent::Job(JobEvent::Finished { id, outcome, .. }) if Some(*id) == self.backup_job => {
                self.backup_job = None;
                self.backup_status = Some(match outcome {
                    Ok(JobOutcome::BackupWritten(p)) => (true, format!("Backup saved to {}", p.display())),
                    Ok(JobOutcome::Restored(r)) => {
                        let mut s =
                            format!("Restored: {} playlists added, {} matched, {} user rows", r.sources_added, r.sources_matched, r.user_rows);
                        if !r.needs_credentials.is_empty() {
                            s.push_str(&format!(" · needs credentials: {}", r.needs_credentials.join(", ")));
                        }
                        // the app applies them (`TunerApp::on_job_finished`)
                        if r.settings.is_some() {
                            s.push_str(" · settings restored");
                        }
                        (true, s)
                    }
                    Ok(_) => (true, "Done".into()),
                    Err(e) => (false, format!("Failed: {}", crate::redact::redact(e))),
                });
            }
            AppEvent::Platform(PlatformEvent::FilePicked { purpose, path }) if self.asked == Some(*purpose) => match (purpose, path) {
                (_, None) => self.asked = None,
                (PickPurpose::OpenEpg, Some(path)) => {
                    self.asked = None;
                    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Guide".into());
                    self.add_epg(ctx, name, EpgSourceKind::File { path: path.clone() });
                }
                (PickPurpose::ExportBackup, Some(path)) => {
                    self.asked = None;
                    self.backup_job = Some(ctx.services.jobs.submit(JobKind::ExportBackup { path: path.clone(), opts: self.backup }));
                    self.backup_status = Some((true, "Exporting…".into()));
                }
                (PickPurpose::ImportBackup, Some(path)) => {
                    self.asked = None;
                    if self.restore_replace {
                        let token = self.token(Pending::Restore(path.clone(), self.restore_settings));
                        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                            title: "Replace everything?".into(),
                            body: "All playlists, guides, favourites and progress are replaced by the backup.".into(),
                            confirm_label: "Replace".into(),
                            destructive: true,
                            token,
                        }));
                    } else {
                        let settings = self.restore_settings;
                        self.backup_job =
                            Some(ctx.services.jobs.submit(JobKind::ImportBackup { path: path.clone(), mode: RestoreMode::Merge, settings }));
                        self.backup_status = Some((true, "Restoring…".into()));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if self.pane == Pane::Pages {
            let i = PAGES.iter().position(|p| p.0 == self.page).unwrap_or(0);
            match action {
                Action::Up | Action::Down => {
                    let j = if action == Action::Up { i.saturating_sub(1) } else { (i + 1).min(PAGES.len() - 1) };
                    // blocked: back to the editor, where Esc (as the toast says) discards
                    if j != i && !self.may_leave_editor(ctx) {
                        self.pane = Pane::Content;
                        return true;
                    }
                    self.set_page(ctx, PAGES[j].0);
                }
                Action::Activate | Action::Right | Action::FocusNext | Action::FocusPrev => self.pane = Pane::Content,
                // leaving Settings would keep unsaved editor changes hidden behind a later visit
                Action::Back if !self.may_leave_editor(ctx) => self.pane = Pane::Content,
                Action::Back | Action::Left => return false,
                Action::ToggleHelp => return false,
                _ => {}
            }
            return true;
        }
        let rows = self.rows(ctx);
        self.clamp_row(&rows);
        let row = rows.get(self.row).cloned();
        if self.editing {
            match action {
                Action::Activate => {
                    // acted on after this frame's text events reached the field
                    self.submit = true;
                }
                Action::Back => {
                    // leaving a field keeps what was typed (as Tab does, and as the form
                    // fields keep their text)
                    if let Some(Row::Text(t)) = &row {
                        self.commit_text(ctx, t);
                    }
                    self.editing = false;
                    self.release_focus = true;
                    if let Some(Row::Local { field: Local::AttachFilter, .. }) = row {
                        self.refilter_attach(ctx);
                    }
                }
                Action::FocusNext | Action::FocusPrev => {
                    if let Some(Row::Text(t)) = &row {
                        self.commit_text(ctx, t);
                    }
                    self.move_row(&rows, if action == Action::FocusNext { 1 } else { -1 });
                    self.editing = rows.get(self.row).is_some_and(Row::is_text) && matches!(rows.get(self.row), Some(Row::Local { .. }));
                }
                _ => return false,
            }
            return true;
        }
        match action {
            Action::Up => self.move_row(&rows, -1),
            Action::Down => self.move_row(&rows, 1),
            Action::PageUp | Action::Home => self.move_row(&rows, -1000),
            Action::PageDown | Action::End => self.move_row(&rows, 1000),
            // spatial panes (DESIGN §15.1): ← is the page list from every row, the source editor
            // and the attach view included (both stay open); values change with → / Shift+←/→
            Action::Left => self.pane = Pane::Pages,
            Action::Right | Action::Increase | Action::Decrease => {
                if let Some(r) = &row {
                    self.adjust(ctx, r, if action == Action::Decrease { -1 } else { 1 });
                }
            }
            Action::Activate => self.submit = true,
            Action::ContextMenu => {
                if let Some(Row::Item { item, .. }) = row {
                    self.item_menu(ctx, item);
                }
            }
            Action::MoveUp | Action::MoveDown => {
                if let Some(Row::Item { item, reorder: true, .. }) = row {
                    self.reorder(ctx, item, if action == Action::MoveUp { -1 } else { 1 });
                }
            }
            Action::FocusNext | Action::FocusPrev => self.pane = Pane::Pages,
            Action::Back => {
                if self.attach.is_some() {
                    self.attach = None;
                    self.row = 0;
                } else if self.editor.is_some() {
                    self.editor = None;
                    self.error = None;
                    self.row = 0;
                } else {
                    self.pane = Pane::Pages;
                }
            }
            _ => return false,
        }
        true
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        let pending = self.pending.take();
        match (result, pending) {
            (OverlayResult::Chosen(Action::Digit(code)), Some(Pending::Menu(item))) => self.menu_choice(ctx, item, code),
            (OverlayResult::Chosen(Action::Digit(MENU_SHOW_EXPLICIT)), Some(Pending::ShowExplicit)) => {
                ctx.edit_config(|c| c.parental.show_explicit = true);
                ctx.toast(Toast::info("Explicit content is shown — anyone using Tuner can see it"));
            }
            (OverlayResult::Chosen(Action::Digit(MENU_PIN_FIRST)), Some(Pending::ShowExplicit)) => {
                // to the New PIN field: explicit content then shows while the PIN is entered
                if let Some(i) = self.rows(ctx).iter().position(|r| matches!(r, Row::Local { field: Local::PinNew, .. })) {
                    self.row = i;
                    self.scroll_to_row = true;
                    self.editing = true;
                }
                ctx.toast(Toast::info("Set a PIN — explicit content then shows after the PIN is entered"));
            }
            (OverlayResult::Confirmed(_), Some(p)) => match p {
                Pending::DeleteSource(id) => {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::delete_source(c, id));
                    self.sources.retain(|s| s.id != id);
                    self.editor = None;
                    self.row = 0;
                    ctx.toast(Toast::info("Playlist deleted"));
                }
                Pending::DeleteEpg(id) => {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| crate::store::sources::delete_epg_source(c, id));
                    self.epg_sources.retain(|e| e.id != id);
                }
                Pending::DeleteList(id) => {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::delete_fav_list(c, id));
                }
                Pending::ForgetOrphan(r) => {
                    // only what the dialog named, and only while it is still an orphan
                    if self.orphans.iter().any(|o| o.item == r) {
                        self.orphans.retain(|o| o.item != r);
                        let _ = ctx.services.store.writer().submit_notify(DataScope::All, move |c| crate::store::user::forget_orphan(c, &r));
                    }
                }
                Pending::Restore(path, settings) => {
                    self.backup_job = Some(ctx.services.jobs.submit(JobKind::ImportBackup { path, mode: RestoreMode::Replace, settings }));
                    self.backup_status = Some((true, "Restoring…".into()));
                }
                Pending::RemovePin => {
                    ctx.edit_config(|c| {
                        c.parental.pin_hash = None;
                        c.parental.pin_salt = None;
                        c.parental.enabled = false;
                        // explicit content goes back to hidden (DESIGN §20.3)
                        c.parental.show_explicit = false;
                    });
                    ctx.toast(Toast::info("PIN removed"));
                }
                Pending::UsePlaylist(id) => {
                    if self.editor.as_ref().is_some_and(|e| e.original.id == id) {
                        self.apply_use_xtream(ctx, false);
                    }
                }
                Pending::Menu(_) | Pending::Adjust(_) | Pending::ShowExplicit => {}
            },
            // the PIN was entered: retry what needed it (the same value change, else Enter)
            (OverlayResult::Unlocked(_), pending) => {
                if let Some(r) = self.current_row(ctx) {
                    match pending {
                        Some(Pending::Adjust(dir)) => self.adjust(ctx, &r, dir),
                        _ => self.activate(ctx, &r),
                    }
                }
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.poll_loaders(ctx);
        let theme = ctx.theme;
        let compact = ctx.layout == LayoutClass::Compact;
        let full = ui.available_rect_before_wrap();
        let gap = theme.space(SpaceToken::PanelPadding);
        let pages_w = if compact { full.width() } else { (if ctx.layout == LayoutClass::Wide { 220.0 } else { 180.0 } * theme.rem()).round() };
        let pages_rect = egui::Rect::from_min_size(full.min, egui::vec2(pages_w.min(full.width()), full.height()));
        let content_rect = if compact { full } else { egui::Rect::from_min_max(egui::pos2(pages_rect.right() + gap, full.top()), full.max) };
        if !compact || self.pane == Pane::Pages {
            let mut pui =
                ui.new_child(egui::UiBuilder::new().id_salt("settings-pages").max_rect(pages_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            self.pages_ui(&mut pui, ctx);
            if !compact {
                section::pane_divider(ui.painter(), pages_rect.right() + gap / 2.0, full.y_range(), theme);
            }
        }
        if !compact || self.pane == Pane::Content {
            let mut cui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("settings-content", self.page as u8))
                    .max_rect(content_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.content_ui(&mut cui, ctx);
        }
        ui.allocate_rect(full, egui::Sense::hover());
    }
}

impl SettingsScreen {
    fn poll_loaders(&mut self, ctx: &mut UiCtx<'_>) {
        // `generation`, not `built_at`: two builds within a second (hide, then unhide) share it
        if self.page == SettingsPage::Hidden && self.hidden_built != ctx.lib.generation {
            // one pass per library build, not one per `rows()` call
            self.hidden_built = ctx.lib.generation;
            let live = &ctx.lib.live;
            // hidden by their own override: a channel whose groups are all hidden (by the user or
            // the category filter) shows with its group instead (DESIGN §28.5)
            let hidden_groups: HashSet<GroupId> = live.groups.iter().filter(|g| g.hidden).map(|g| g.id).collect();
            self.hidden_channels = live
                .channels
                .iter()
                .filter(|c| live.hidden.contains(&c.id) && (c.group_ids.is_empty() || c.group_ids.iter().any(|g| !hidden_groups.contains(g))))
                .take(500)
                .map(|c| (c.id, c.name.to_string()))
                .collect();
        }
        if self.offset_pending.is_some_and(|(_, at)| at.elapsed() >= OFFSET_SETTLE) {
            self.flush_epg_offset(ctx);
        } else if self.offset_pending.is_some() {
            ctx.services.bus.request_repaint();
        }
        if let Some(r) = kit::poll(&mut self.sources_loader) {
            match r {
                Ok(v) => {
                    let changed = self.editor.as_ref().and_then(|e| v.iter().find(|s| s.id == e.original.id && **s != e.original).cloned());
                    if let Some(s) = changed {
                        self.rebase_editor(ctx, &s);
                    }
                    self.sources = v;
                    if let Some(id) = self.edit_on_load.take() {
                        self.item_default(ctx, Item::Source(id));
                    }
                }
                Err(e) => kit::error_toast(ctx, "Couldn't read the playlists", &e),
            }
        }
        if let Some(r) = kit::poll(&mut self.epg_loader) {
            match r {
                Ok((v, m)) => {
                    self.epg_sources = v;
                    self.epg_matches = m;
                }
                Err(e) => kit::error_toast(ctx, "Couldn't read the guides", &e),
            }
        }
        // channels with a guide per playlist (one pass per library build)
        let built = ctx.lib.generation;
        if matches!(self.page, SettingsPage::Playlists | SettingsPage::Epg) && self.guide_counts.0 != built {
            let mut m: HashMap<SourceId, usize> = HashMap::new();
            for c in ctx.lib.live.channels.iter().filter(|c| c.epg_channel_id.is_some()) {
                *m.entry(c.source_id).or_default() += 1;
            }
            self.guide_counts = (built, m);
        }
        if let Some(r) = kit::poll(&mut self.reminders_loader) {
            match r {
                Ok(v) => self.reminders = v,
                Err(e) => kit::error_toast(ctx, "Couldn't read the reminders", &e),
            }
        }
        if let Some(r) = kit::poll(&mut self.orphans_loader) {
            match r {
                Ok(v) => self.orphans = v,
                Err(e) => kit::error_toast(ctx, "Couldn't read the orphans", &e),
            }
        }
        if let Some(r) = kit::poll(&mut self.hidden_loader) {
            match r {
                Ok(v) => self.hidden_items = v,
                Err(e) => kit::error_toast(ctx, "Couldn't read hidden items", &e),
            }
        }
        if let Some(r) = kit::poll(&mut self.links_loader) {
            match r {
                Ok(v) => self.title_links = v,
                Err(e) => kit::error_toast(ctx, "Couldn't read the titles you split", &e),
            }
        }
    }

    fn pages_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        // pushed over another screen (Movies ▸ Hidden…): the way back to it
        if ctx.back.is_some() {
            section::page_header(ui, ctx, "", |_| {});
            ui.add_space(theme.space(SpaceToken::Sm));
        }
        section::section_header(ui, theme, "Settings");
        ui.add_space(theme.space(SpaceToken::Md));
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("settings-pages-scroll").auto_shrink([false, false]).show(ui, |ui| {
            for (page, icon, label) in PAGES {
                let selected = page == self.page;
                // hover lights a page only while the keyboard cursor is in the content (one
                // cursor at a time; moving it here would switch the page under the pointer)
                let state = RowState::from_flags(selected && self.pane == Pane::Pages, selected);
                let state = if self.pane == Pane::Pages { state } else { rows::with_hover(ui, theme, false, state) };
                if rows::menu_row(ui, theme, Some(icon), label, None, false, state).clicked() {
                    clicked = Some(page);
                }
            }
        });
        if let Some(p) = clicked {
            if p != self.page && !self.may_leave_editor(ctx) {
                self.pane = Pane::Content;
                return;
            }
            self.set_page(ctx, p);
            self.pane = Pane::Content;
            self.page_clicked_at = Some(ui.input(|i| i.time));
        }
    }

    fn content_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let rows = self.rows(ctx);
        self.clamp_row(&rows);
        let focused_pane = self.pane == Pane::Content;
        let row_h = theme.space(SpaceToken::ControlHeight) + 2.0 * theme.space(SpaceToken::Md);
        let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let scroll = std::mem::take(&mut self.scroll_to_row);
        let release = std::mem::take(&mut self.release_focus);
        let mut clicked: Option<(usize, i32)> = None;
        let mut hovered = None;
        let mut text_clicked = None;
        let mut drag_target: Option<usize> = None;
        let mut drag_started = None;
        let mut drag_released = false;
        let mut item_rects: Vec<(usize, egui::Rect)> = Vec::new();
        // the way out of a sub-view (Esc): the source editor or attach view to their page, and
        // in Compact (one pane at a time) the page to the page list
        let page_label = PAGES.iter().find(|p| p.0 == self.page).map_or("Settings", |p| p.2);
        let back_to = if self.attach.is_some() || self.editor.is_some() {
            Some(page_label)
        } else if ctx.layout == LayoutClass::Compact {
            Some("Settings")
        } else {
            None
        };
        if let Some(label) = back_to {
            if back_button(ui, theme, label).clicked() {
                // exactly what Esc does from the page: a field being edited is left first
                // (keeping what was typed), then the sub-view / the page
                self.pane = Pane::Content;
                if self.editing {
                    self.on_action(ctx, Action::Back);
                }
                self.on_action(ctx, Action::Back);
            }
            ui.add_space(theme.space(SpaceToken::Xs));
        }
        egui::ScrollArea::vertical().id_salt(("settings-content-scroll", self.page as u8)).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Xxs);
            for (i, row) in rows.iter().enumerate() {
                let focused = focused_pane && i == self.row;
                match row {
                    Row::Section(s) => {
                        ui.add_space(theme.space(SpaceToken::Lg));
                        if !s.is_empty() {
                            section::section_header(ui, theme, s);
                        }
                        ui.add_space(theme.space(SpaceToken::Xs));
                        continue;
                    }
                    Row::Note(s, bad) => {
                        ui.add(
                            egui::Label::new(RichText::new(s).size(theme.font(TypeToken::BodySmall)).color(if *bad {
                                t.urgent
                            } else {
                                t.text_secondary
                            }))
                            .wrap(),
                        );
                        continue;
                    }
                    _ => {}
                }
                let is_text = row.is_text();
                let item_h = if matches!(row, Row::Item { detail: Some(_), .. }) {
                    rows::row_height(theme, ctx.config.interface.density, true)
                } else {
                    row_h
                };
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), if is_text { row_h + theme.font(TypeToken::Body) } else { item_h }),
                    egui::Sense::click(),
                );
                if focused && scroll {
                    ui.scroll_to_rect(rect, None);
                }
                paint_row_state(ui.painter(), rect, theme, if focused || resp.hovered() { RowState::Cursor } else { RowState::Idle });
                if focused {
                    ui.painter().rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, t.border_hover), egui::StrokeKind::Inside);
                }
                if resp.clicked() {
                    clicked = Some((i, 0));
                } else if resp.secondary_clicked() {
                    clicked = Some((i, 2));
                } else if pointer_moved && resp.hovered() && i != self.row && !self.editing {
                    hovered = Some(i);
                }
                let mut inner = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt(("row", i))
                        .max_rect(rect.shrink2(egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Xs))))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                let label_color = if focused { t.list_cursor_text } else { t.fg };
                match row {
                    Row::Toggle(tg) => {
                        let mut v = (tg.get)(ctx.config);
                        if toggle_row(&mut inner, theme, &mut v, tg.label, label_color) {
                            clicked = Some((i, if v { 1 } else { -1 }));
                        }
                    }
                    Row::Choice(c) => {
                        let value = c.options.get((c.get)(ctx.config)).copied().unwrap_or("");
                        if let Some(d) = value_row(&mut inner, theme, c.label, value, label_color) {
                            clicked = Some((i, d));
                        }
                    }
                    Row::Number(n) => {
                        let value = number_text(n, (n.get)(ctx.config));
                        if let Some(d) = value_row(&mut inner, theme, n.label, &value, label_color) {
                            clicked = Some((i, d));
                        }
                    }
                    Row::Text(tx) => {
                        let editing = focused && self.editing;
                        let mut shown = if editing { std::mem::take(&mut self.edit_buf) } else { (tx.get)(ctx.config) };
                        let (hit, _) = text_row(&mut inner, theme, tx.label, &mut shown, tx.placeholder, false, editing, release, label_color, None);
                        if editing {
                            self.edit_buf = shown;
                        }
                        if hit && !editing {
                            text_clicked = Some(i);
                        }
                    }
                    Row::Local { field, label } => {
                        let field = *field;
                        let err = self.error.as_ref().filter(|(f, _)| *f == field).map(|(_, m)| m.clone());
                        if field.is_text() {
                            let editing = focused && self.editing;
                            let password = matches!(field, Local::PinNew | Local::PinConfirm | Local::SrcPassword);
                            let placeholder = match field {
                                Local::NewEpgUrl => "http://example.com/guide.xml.gz",
                                Local::PinNew | Local::PinConfirm => "4–8 digits",
                                Local::SrcServer => "panel.example.com:8080",
                                Local::AttachFilter => "type to filter",
                                _ => "",
                            };
                            let mut buf = self.local_text(field).map(std::mem::take).unwrap_or_default();
                            let (hit, changed) =
                                text_row(&mut inner, theme, label, &mut buf, placeholder, password, editing, release, label_color, err.as_deref());
                            if matches!(field, Local::PinNew | Local::PinConfirm) {
                                buf.retain(|c| c.is_ascii_digit());
                                buf.truncate(8);
                            }
                            if let Some(slot) = self.local_text(field) {
                                *slot = buf;
                            }
                            if changed && field == Local::AttachFilter {
                                self.refilter_attach(ctx);
                            }
                            if hit && !editing {
                                text_clicked = Some(i);
                            }
                        } else if field.is_toggle() {
                            let mut v = self.local_toggle(field).map(|b| *b).unwrap_or(false);
                            if toggle_row(&mut inner, theme, &mut v, label, label_color) {
                                // the new state, as Shift+←/→ would set it
                                clicked = Some((i, if v { 1 } else { -1 }));
                            }
                        } else {
                            let value = self.local_value(field);
                            if let Some(d) = value_row(&mut inner, theme, label, &value, label_color) {
                                clicked = Some((i, d));
                            }
                        }
                    }
                    Row::Act { icon, label, danger, .. } => {
                        inner.label(RichText::new(*icon).size(theme.font(TypeToken::Icon)).color(if *danger { t.urgent } else { t.accent }));
                        inner.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(if *danger { t.urgent } else { label_color }));
                    }
                    Row::Item { icon, color, label, detail, reorder, item } => {
                        if *reorder {
                            let g = inner.add(
                                egui::Label::new(RichText::new(glyphs::REORDER).size(theme.font(TypeToken::Icon)).color(t.text_disabled))
                                    .sense(egui::Sense::drag()),
                            );
                            if g.drag_started() {
                                drag_started = Some(i);
                            }
                            if g.drag_stopped() {
                                drag_released = true;
                            }
                            item_rects.push((i, rect));
                        }
                        let c = color.map(|c| theme.source_color(c)).unwrap_or(t.text_secondary);
                        if let Item::HiddenTag(..) = item {
                            // the chevron lists / hides the tag's categories (→ / Shift+←); the row
                            // itself shows them all (Enter)
                            let open = icon == glyphs::CHEVRON_DOWN;
                            let tip = if open { "Hide the list (Shift+←)" } else { "List them (→)" };
                            if icon_button(&mut inner, theme, icon, tip, ButtonStyle::default()).clicked() {
                                clicked = Some((i, if open { -1 } else { 1 }));
                            }
                        } else {
                            inner.label(RichText::new(icon).size(theme.font(TypeToken::IconLarge)).color(c));
                        }
                        let more = matches!(item, Item::Source(_) | Item::Epg(_) | Item::List(_) | Item::Orphan(_) | Item::Reminder(_));
                        // the button is laid out first so long names and errors end before it
                        inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if more {
                                let (glyph, tip) = if matches!(item, Item::Reminder(_)) {
                                    (glyphs::CLOSE, "Cancel reminder (Delete)")
                                } else {
                                    (glyphs::SETTINGS, "More (x)")
                                };
                                if icon_button(ui, theme, glyph, tip, ButtonStyle::default()).clicked() {
                                    clicked = Some((i, 2));
                                }
                            }
                            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                let w = ui.available_width().max(0.0);
                                let line = |text: &str, size: f32, color: egui::Color32| {
                                    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), egui::FontId::proportional(size), color);
                                    job.wrap = egui::text::TextWrapping::truncate_at_width(w);
                                    job
                                };
                                ui.label(line(label, theme.font(TypeToken::Title), label_color));
                                if let Some(d) = detail {
                                    ui.label(line(d, theme.font(TypeToken::BodySmall), t.text_secondary));
                                }
                            });
                        });
                    }
                    Row::Section(_) | Row::Note(..) => {}
                }
            }
            if self.drag_from.is_some()
                && let Some(p) = ui.ctx().pointer_interact_pos()
            {
                drag_target = item_rects.iter().find(|(_, r)| r.y_range().contains(p.y)).map(|(i, _)| *i);
                if let Some((_, r)) = item_rects.iter().find(|(i, _)| Some(*i) == drag_target) {
                    ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(2.0, t.accent));
                }
            }
        });
        if rows.iter().all(|r| !r.focusable()) && rows.is_empty() {
            empty_state::empty_state(ui, theme, glyphs::SETTINGS, "Nothing here", None, None);
        }
        // drag & drop reorder (grip)
        if let Some(i) = drag_started {
            self.drag_from = Some(i);
        }
        if drag_released {
            if let (Some(from), Some(to)) = (self.drag_from.take(), drag_target)
                && let (Some(Row::Item { item, .. }), true) = (rows.get(from), from != to)
            {
                let delta = rows[from.min(to)..=from.max(to)].iter().filter(|r| matches!(r, Row::Item { reorder: true, .. })).count() as isize - 1;
                self.reorder(ctx, item.clone(), if to > from { delta } else { -delta });
            }
            self.drag_from = None;
        }
        if let Some(i) = hovered.filter(|_| !self.editing) {
            self.row = i;
        }
        if crate::ui::widgets::second_click_after(ui, self.page_clicked_at) {
            text_clicked = None;
            clicked = None;
        }
        if let Some(i) = text_clicked {
            self.pane = Pane::Content;
            self.row = i;
            if let Some(Row::Text(tx)) = rows.get(i) {
                self.edit_buf = (tx.get)(ctx.config);
            }
            self.editing = true;
        }
        if let Some((i, how)) = clicked {
            self.pane = Pane::Content;
            self.row = i;
            self.editing = false;
            if let Some(r) = rows.get(i).cloned() {
                match how {
                    0 => self.activate(ctx, &r),
                    2 => {
                        if let Row::Item { item, .. } = r {
                            self.item_menu(ctx, item);
                        }
                    }
                    d => self.adjust(ctx, &r, d),
                }
            }
        }
        if std::mem::take(&mut self.submit) {
            let rows = self.rows(ctx);
            if let Some(r) = rows.get(self.row).cloned() {
                self.activate(ctx, &r);
            }
        }
        if self.sources_loader.is_some()
            || self.epg_loader.is_some()
            || ctx.services.jobs.active().iter().any(|j| matches!(j.kind, JobKind::RefreshSource { .. }))
        {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// `Label                    [−] value [+]`; returns −1 / +1 when a side was clicked.
fn value_row(ui: &mut egui::Ui, theme: &Theme, label: &str, value: &str, color: egui::Color32) -> Option<i32> {
    let t = &theme.tokens;
    ui.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(color));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let mut r = None;
        if icon_button(ui, theme, glyphs::CHEVRON_RIGHT, "Increase", ButtonStyle::default()).clicked() {
            r = Some(1);
        }
        ui.label(RichText::new(value).size(theme.font(TypeToken::Body)).color(t.fg));
        if icon_button(ui, theme, glyphs::CHEVRON_LEFT, "Decrease", ButtonStyle::default()).clicked() {
            r = Some(-1);
        }
        r
    })
    .inner
}

/// `Label                       [switch]` with the label in the row's colour (accent when
/// focused, like the other rows). True when the switch was clicked.
fn toggle_row(ui: &mut egui::Ui, theme: &Theme, on: &mut bool, label: &str, color: egui::Color32) -> bool {
    ui.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(color));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| toggle(ui, theme, on, "", false).changed()).inner
}

/// Label above a text field that follows the settings focus. Returns (clicked, changed).
#[allow(clippy::too_many_arguments)]
fn text_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    text: &mut String,
    placeholder: &str,
    password: bool,
    editing: bool,
    release: bool,
    color: egui::Color32,
    error: Option<&str>,
) -> (bool, bool) {
    let t = &theme.tokens;
    let mut out = (false, false);
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(theme.font(TypeToken::BodySmall)).color(color));
            if let Some(e) = error {
                ui.label(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(t.urgent));
            }
        });
        let resp = text_field(ui, theme, text, placeholder, password);
        if editing && !resp.has_focus() {
            resp.request_focus();
        } else if (!editing || release) && resp.has_focus() {
            ui.memory_mut(|m| m.surrender_focus(resp.id));
        }
        out = (resp.clicked() || resp.gained_focus(), resp.changed());
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use egui_kittest::kittest::Queryable;

    use crate::model::ItemKey;
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed, until};

    type H = egui_kittest::Harness<'static, testkit::App<SettingsScreen>>;

    /// Focus the content row labelled `label` (by moving with ↓ from the top).
    fn focus(h: &mut H, label: &str) {
        let app = h.state_mut();
        let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
        let i = rows
            .iter()
            .position(|r| match r {
                Row::Toggle(t) => t.label == label,
                Row::Choice(c) => c.label == label,
                Row::Number(n) => n.label == label,
                Row::Text(t) => t.label == label,
                Row::Local { label: l, .. } => *l == label,
                Row::Act { label: l, .. } => l == label,
                Row::Item { label: l, .. } => l == label,
                _ => false,
            })
            .unwrap_or_else(|| panic!("no row {label}"));
        app.s.pane = Pane::Content;
        app.s.row = i;
        app.s.editing = false;
    }

    fn type_text(h: &mut H, text: &str) {
        h.run_steps(2); // the field takes focus
        h.event(egui::Event::Text(text.into()));
        h.run_steps(1);
    }

    fn group_ov(h: &H, key: &str) -> Option<crate::model::GroupOverride> {
        crate::store::user::group_overrides(&h.state().t.services().store.reader().unwrap())
            .unwrap()
            .into_iter()
            .find(|r| r.key.0 == key)
            .map(|r| r.ov)
    }

    fn sources(h: &H) -> Vec<Source> {
        crate::store::sources::list_sources(&h.state().t.services().store.reader().unwrap()).unwrap()
    }

    /// Telling playlists apart (DESIGN §29): two playlists with the same glyph and colour get a
    /// notice and a one-click fix — nothing changes until it is pressed — which keeps the first
    /// and gives the other the first free colour; a scope tab's "Edit playlist…" opens that
    /// playlist's editor, whose "Show in All playlists" is saved with the playlist.
    #[test]
    fn look_alike_playlists_get_a_fix_and_a_tab_opens_the_editor() {
        let (mut t, one) = rig();
        let tv2 = seed(&mut t, |c| {
            testkit::source(c, "TV")?;
            let tv2 = testkit::source(c, "TV 2")?;
            let radio = testkit::source(c, "Radio")?;
            c.execute("UPDATE source SET color = 'red' WHERE id = ?1", [radio])?;
            Ok(tv2)
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the sources", |h| has_label(h, "Radio"));
        assert!(has_label(&h, "TV and TV 2 look the same on cards and in lists"));
        let colors = |h: &H| sources(h).iter().map(|s| s.color).collect::<Vec<_>>();
        assert_eq!(colors(&h), [SourceColor::Accent, SourceColor::Accent, SourceColor::Red], "never silently");
        focus(&mut h, "Give each its own colour");
        h.state_mut().act(Action::Activate);
        until(&mut h, "the new colour", |h| colors(h)[1] != SourceColor::Accent);
        // as the theme draws them: TV 2 now differs from TV and from Radio; the first keeps its own
        let drawn: Vec<_> = colors(&h).iter().map(|c| h.state().t.theme.source_color(*c)).collect();
        assert_eq!(colors(&h)[0], SourceColor::Accent);
        assert!(drawn[1] != drawn[0] && drawn[1] != drawn[2], "{:?}", colors(&h));
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the notice to go", |h| !has_label(h, "Give each its own colour"));

        h.state_mut().enter(Route::EditPlaylist(tv2));
        until(&mut h, "the editor", |h| has_label(h, "EDIT TV 2"));
        focus(&mut h, "Show in All playlists");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        focus(&mut h, "Save");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some("Saved TV 2"), "a view setting: no refresh");
        until(&mut h, "the setting", |h| sources(h).iter().any(|s| s.id == tv2 && s.settings.in_all == Some(false)));
    }

    #[test]
    fn config_rows_edit_and_apply_and_pages_switch() {
        let (t, one) = rig();
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Player));
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Panel]);
        assert!(has_label(&h, "Hardware decoding") && has_label(&h, "Zapping delay"));
        // Enter toggles; → / Shift+←/→ adjust numbers and choices; everything goes through
        // edit_config
        focus(&mut h, "Instant load (less buffering, faster zaps)");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        focus(&mut h, "Zapping delay");
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Right);
        focus(&mut h, "Subtitles automatically");
        h.state_mut().act(Action::Decrease);
        h.state_mut().apply_config();
        let c = h.state().t.config.clone();
        assert!(c.player.instant_load);
        assert_eq!(c.player.zap_delay_ms, 400);
        assert_eq!(c.player.sub_auto, SubtitleAuto::Off);
        // a text row: Enter edits ([TextInput, Panel]), typing, Enter commits
        focus(&mut h, "Advanced mpv options");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Panel]);
        type_text(&mut h, "; profile=fast"); // appended to the headless "ao=null; vo=null"
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.player.mpv_options.get("profile").map(String::as_str), Some("fast"));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Panel]);
        // Esc leaves a field keeping what was typed (like Tab, and like the form fields)
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        type_text(&mut h, "; cache=no");
        h.state_mut().act(Action::Back);
        h.run_steps(1);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Panel]);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.player.mpv_options.get("cache").map(String::as_str), Some("no"), "Esc keeps the edit");
        // leaving a field unchanged queues nothing
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        h.state_mut().act(Action::Back);
        assert!(h.state().t.config_edits.is_empty());
        // Tab to the page list, ↓ switches pages; Esc there leaves Settings (the app's Back)
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::Down);
        h.run_steps(1);
        assert_eq!(h.state().s.page, SettingsPage::Interface);
        assert!(has_label(&h, "Start screen") && has_label(&h, "UI scale (on top of Omarchy's text size)"));
        assert!(!h.state_mut().act(Action::Back));
        // About: the log viewer and the export dialog (the app writes the file)
        h.state_mut().enter(Route::Settings(SettingsPage::About));
        h.run_steps(1);
        focus(&mut h, "Open the log viewer");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Logs)]);
        focus(&mut h, "Export log…");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(kit::PICKED.with_borrow_mut(std::mem::take), vec![PickPurpose::ExportLog]);
    }

    #[test]
    fn playlists_reorder_edit_delete_and_epg_priority_by_keyboard_and_drag() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            testkit::source(c, "Beta")?;
            let ch = testkit::channels(c, a, 3)?;
            testkit::epg(c, &ch, 0, 1, true)?;
            c.execute("UPDATE epg_source SET priority = 0 WHERE name = 'Seed EPG'", [])?;
            c.execute(
                "INSERT INTO epg_source(name, kind, url, active_gen, priority) VALUES ('Second guide','url','http://127.0.0.1:9/b.xml',0,1)",
                [],
            )?;
            c.execute(
                "INSERT INTO epg_source(name, kind, url, active_gen, priority) VALUES ('Third guide','url','http://127.0.0.1:9/c.xml',0,2)",
                [],
            )?;
            Ok(())
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the sources", |h| has_label(h, "Beta"));
        // Alt+↓ / J moves Alpha below Beta
        focus(&mut h, "Alpha");
        h.state_mut().act(Action::MoveDown);
        until(&mut h, "the new order", |h| sources(h).iter().map(|s| s.name.as_str()).collect::<Vec<_>>() == ["Beta", "Alpha"]);
        // Enter opens the editor; rename + Save writes it
        focus(&mut h, "Alpha");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(has_label(&h, "EDIT ALPHA"));
        focus(&mut h, "Name");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        type_text(&mut h, " One");
        h.state_mut().act(Action::Back);
        focus(&mut h, "Save");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        until(&mut h, "the rename", |h| sources(h).iter().any(|s| s.name == "Alpha One"));
        // x / Delete: the row menu → Delete… → Confirm
        until(&mut h, "the list", |h| has_label(h, "Alpha One"));
        focus(&mut h, "Beta");
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.label == "Delete…")),
            "{nav:?}"
        );
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_DELETE)));
        let nav = h.state_mut().nav();
        let token = match nav.as_slice() {
            [NavRequest::Open(OverlayRequest::Confirm(c))] => {
                assert!(c.destructive);
                c.token
            }
            other => panic!("{other:?}"),
        };
        h.state_mut().result(OverlayResult::Confirmed(token));
        until(&mut h, "the deletion", |h| sources(h).len() == 1);
        // "+ Add playlist…" opens the onboarding card at Type
        focus(&mut h, "Add playlist…");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Onboarding(OnboardingStart::AddSource))]);

        // EPG priority: keyboard, then drag & drop on the grip
        let names = |h: &H| {
            crate::store::sources::list_epg_sources(&h.state().t.services().store.reader().unwrap())
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect::<Vec<_>>()
        };
        h.state_mut().enter(Route::Settings(SettingsPage::Epg));
        until(&mut h, "the guides", |h| has_label(h, "Third guide"));
        assert_eq!(names(&h), ["Seed EPG", "Second guide", "Third guide"]);
        focus(&mut h, "Third guide");
        h.state_mut().act(Action::MoveUp);
        until(&mut h, "keyboard reorder", |h| names(h) == ["Seed EPG", "Third guide", "Second guide"]);
        h.state_mut().enter(Route::Settings(SettingsPage::Epg));
        until(&mut h, "reloaded order", |h| {
            h.state().s.epg_sources.first().is_some_and(|e| e.name == "Seed EPG")
                && h.state().s.epg_sources.get(1).is_some_and(|e| e.name == "Third guide")
        });
        h.run_steps(2);
        let grip = |h: &H, name: &str| {
            let row = h.query_all_by_label(name).next().expect("row").rect();
            let grips: Vec<egui::Rect> = h.query_all_by_label(glyphs::REORDER).map(|n| n.rect()).collect();
            grips.into_iter().find(|g| g.center().y >= row.top() - 12.0 && g.center().y <= row.bottom() + 24.0).expect("grip of the row")
        };
        let from = grip(&h, "Second guide").center();
        let to = h.query_all_by_label("Seed EPG").next().expect("first guide").rect().center();
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(egui::pos2(from.x, (from.y + to.y) / 2.0));
        h.run_steps(1);
        h.hover_at(egui::pos2(from.x, to.y));
        h.run_steps(1);
        h.drop_at(egui::pos2(from.x, to.y));
        h.run_steps(2);
        until(&mut h, "drag reorder", |h| names(h).first().map(String::as_str) == Some("Second guide"));
    }

    #[test]
    fn favourites_reminders_hidden_and_orphans_pages() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)?;
            let r = |i: usize| ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
            crate::store::user::set_item_override(c, &r(2), &crate::model::ItemOverride { hidden: true, ..Default::default() })?;
            crate::store::user::add_reminder(
                c,
                &Reminder {
                    id: ReminderId(0),
                    channel: r(0),
                    channel_name: "Channel 0".into(),
                    programme_title: "Late Show".into(),
                    start: 4_000_000_000,
                    stop: 4_000_003_600,
                    lead_s: 120,
                    state: crate::model::ReminderState::Pending,
                },
            )?;
            // an orphaned favourite of a channel that vanished
            let list = crate::store::user::default_fav_list(c)?;
            c.execute(
                "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp, orphaned_since) VALUES (?1, ?2, 'channel', 'm3u:gone', 0, 1, '{\"norm_name\":\"channel 1\"}', 5)",
                rusqlite::params![list, s],
            )?;
            // a category filter that keeps the UK only
            let rule = crate::model::CategoryFilter { keep: [CategoryTag("GB".into())].into(), ..Default::default() };
            crate::store::sources::set_category_filter(c, s, Some(&rule))?;
            Ok(())
        });
        // ... and "News" is an Arabic category the filter hides (as the library build marks it)
        let arabic = CategoryTag("lang:ar".into());
        let mut lib = crate::store::library::load(&t.services().store.reader().unwrap()).unwrap();
        for g in std::sync::Arc::get_mut(&mut lib.live).unwrap().groups.iter_mut().filter(|g| g.key.0 == "channel:news") {
            (g.tag, g.filter, g.hidden) = (Some(arabic.clone()), GroupFilter::Hidden, true);
        }
        lib.sources[0].category_filter = Some(crate::model::CategoryFilter { keep: [CategoryTag("GB".into())].into(), ..Default::default() });
        t.set_library(lib);
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        // favourites: create a list by typing its name
        h.state_mut().enter(Route::Settings(SettingsPage::Favourites));
        h.run_steps(1);
        assert!(has_label(&h, "Favourites"));
        focus(&mut h, "New list");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        type_text(&mut h, "Sport");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let lists = |h: &H| crate::store::user::fav_lists(&h.state().t.services().store.reader().unwrap()).unwrap();
        until(&mut h, "the new list", |h| lists(h).iter().any(|l| l.name == "Sport"));
        // reminders: the pending one, cancelled from its menu
        h.state_mut().enter(Route::Settings(SettingsPage::Reminders));
        until(&mut h, "the reminder", |h| has_label(h, "Late Show"));
        focus(&mut h, "Late Show");
        h.state_mut().act(Action::ContextMenu);
        h.state_mut().nav();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_DELETE)));
        until(&mut h, "the cancellation", |h| crate::store::user::all_reminders(&h.state().t.services().store.reader().unwrap()).unwrap().is_empty());
        // hidden items: what the category filter hides, per tag (not among the user's groups)
        h.state_mut().enter(Route::Settings(SettingsPage::Hidden));
        h.run_steps(1);
        let tag_row = tag_name(Some(&arabic));
        assert!(has_label(&h, &tag_row) && has_label(&h, "Choose countries…"));
        assert!(!has_label(&h, "News"), "filtered groups aren't in Hidden groups");
        // → lists the tag's categories; Enter on one shows just it (an exception to the rule)
        focus(&mut h, &tag_row);
        h.state_mut().act(Action::Right);
        h.run_steps(1);
        focus(&mut h, "News");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        until(&mut h, "News shown", |h| group_ov(h, "channel:news").is_some_and(|o| o.shown && !o.hidden));
        // Enter on the tag row shows the whole tag: the rule keeps it
        focus(&mut h, &tag_row);
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let rule =
            |h: &H| crate::store::sources::get_source(&h.state().t.services().store.reader().unwrap(), SourceId(1)).unwrap().settings.category_filter;
        until(&mut h, "the tag kept", |h| rule(h).is_some_and(|r| r.keep.contains(&arabic) && r.keep.contains(&CategoryTag("GB".into()))));
        // the user's own hidden channel: Enter unhides
        assert!(has_label(&h, "Channel 2"));
        focus(&mut h, "Channel 2");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 2)) };
        until(&mut h, "unhidden", |h| {
            crate::store::user::item_override(&h.state().t.services().store.reader().unwrap(), &item).unwrap().is_none_or(|o| !o.hidden)
        });
        // orphans: attach to a library item (the filter starts with the old name)
        h.state_mut().enter(Route::Settings(SettingsPage::Orphans));
        until(&mut h, "the orphan", |h| has_label(h, "channel 1"));
        focus(&mut h, "channel 1");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(has_label(&h, "ATTACH TO…"));
        focus(&mut h, "Channel 1");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let target = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 1)) };
        until(&mut h, "reattached", |h| {
            let r = h.state().t.services().store.reader().unwrap();
            crate::store::user::lists_containing(&r, &target).unwrap().len() == 1
        });
    }

    #[test]
    fn backup_export_and_replace_restore_run_their_jobs() {
        let (mut t, one) = rig();
        seed(&mut t, |c| testkit::source(c, "My IPTV"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuner-backup.json");
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Backup));
        h.run_steps(1);
        assert!(has(&h, "Include passwords") && has(&h, "Include watch history"));
        focus(&mut h, "Export backup…");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(kit::PICKED.with_borrow_mut(std::mem::take), vec![PickPurpose::ExportBackup]);
        // the dialog answers: the export job runs
        h.state()
            .t
            .services()
            .bus
            .send(AppEvent::Platform(PlatformEvent::FilePicked { purpose: PickPurpose::ExportBackup, path: Some(path.clone()) }));
        until(&mut h, "the export", |h| testkit::has_label_containing(h, "Backup saved to"));
        assert!(path.exists());
        // restore, replacing everything: confirmed first
        focus(&mut h, "When restoring");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(h.state().s.restore_replace && h.state().s.restore_settings, "Replace restores the settings by default");
        focus(&mut h, "Restore a backup…");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(kit::PICKED.with_borrow_mut(std::mem::take), vec![PickPurpose::ImportBackup]);
        h.state()
            .t
            .services()
            .bus
            .send(AppEvent::Platform(PlatformEvent::FilePicked { purpose: PickPurpose::ImportBackup, path: Some(path.clone()) }));
        h.run_steps(2);
        let token = match h.state_mut().nav().as_slice() {
            [NavRequest::Open(OverlayRequest::Confirm(c))] if c.destructive => c.token,
            other => panic!("{other:?}"),
        };
        h.state_mut().result(OverlayResult::Confirmed(token));
        until(&mut h, "the restore", |h| testkit::has_label_containing(h, "Restored:"));
    }

    fn has(h: &H, label: &str) -> bool {
        has_label(h, label)
    }

    /// A Forget confirmed after the orphans were re-read in another order (a refresh finished
    /// while the dialog was open) forgets the orphan the dialog named; Delete on a reminder
    /// cancels it at once; a page opened by route drops another page's source editor.
    #[test]
    fn orphans_are_keyed_by_item_reminders_cancel_on_delete_and_routes_reset_the_editor() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 2)?;
            let list = crate::store::user::default_fav_list(c)?;
            for (i, name) in ["alpha one", "beta two"].iter().enumerate() {
                c.execute(
                    "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp, orphaned_since) VALUES (?1, ?2, 'channel', ?3, ?4, 1, ?5, 5)",
                    rusqlite::params![list, s, format!("m3u:gone{i}"), i as i64, format!("{{\"norm_name\":\"{name}\"}}")],
                )?;
            }
            let channel = ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 0)) };
            let r = Reminder {
                id: ReminderId(0),
                channel,
                channel_name: "Channel 0".into(),
                programme_title: "Late Show".into(),
                start: 4_000_000_000,
                stop: 4_000_003_600,
                lead_s: 120,
                state: crate::model::ReminderState::Pending,
            };
            crate::store::user::add_reminder(c, &r).map(|_| ())
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        let orphans = |h: &H| {
            crate::store::user::orphans(&h.state().t.services().store.reader().unwrap())
                .unwrap()
                .into_iter()
                .map(|o| o.fingerprint.norm_name)
                .collect::<Vec<_>>()
        };
        h.state_mut().enter(Route::Settings(SettingsPage::Orphans));
        until(&mut h, "the orphans", |h| has_label(h, "alpha one") && has_label(h, "beta two"));
        focus(&mut h, "beta two");
        h.state_mut().act(Action::ContextMenu);
        h.state_mut().nav();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_DELETE)));
        let token = match h.state_mut().nav().as_slice() {
            [NavRequest::Open(OverlayRequest::Confirm(c))] if c.destructive => c.token,
            other => panic!("{other:?}"),
        };
        // a refresh re-read the orphans in another order while the dialog was open
        h.state_mut().s.orphans.reverse();
        h.state_mut().result(OverlayResult::Confirmed(token));
        until(&mut h, "only the named orphan forgotten", |h| orphans(h) == ["alpha one"]);

        // Delete (x) on a reminder cancels it directly: no one-item menu
        h.state_mut().enter(Route::Settings(SettingsPage::Reminders));
        until(&mut h, "the reminder", |h| has_label(h, "Late Show"));
        focus(&mut h, "Late Show");
        h.state_mut().act(Action::ContextMenu);
        assert!(h.state_mut().nav().is_empty(), "no menu");
        until(&mut h, "cancelled", |h| crate::store::user::all_reminders(&h.state().t.services().store.reader().unwrap()).unwrap().is_empty());

        // the source editor of Playlists doesn't leak into Movies ▸ "Show hidden"
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the playlist", |h| has_label(h, "My IPTV"));
        focus(&mut h, "My IPTV");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(has_label(&h, "EDIT MY IPTV"));
        h.state_mut().enter(Route::Settings(SettingsPage::Hidden));
        h.run_steps(2);
        assert!(h.state().s.editor.is_none() && !has_label(&h, "EDIT MY IPTV"));
        assert!(has_label(&h, "HIDDEN GROUPS"));
    }

    /// Spatial panes (DESIGN §15.1): ← steps from any content row back to the page list (a
    /// choice or a toggle keeps its value), ↓ there switches the page and → / Enter return;
    /// values change with → and Shift+←/→ (H/L); Tab and Esc are unchanged. The source editor
    /// stays open under ←, and a page switch never drops its unsaved changes.
    #[test]
    fn left_steps_back_to_the_page_list_and_shift_arrows_change_values() {
        let (mut t, one) = rig();
        seed(&mut t, |c| testkit::source(c, "My IPTV").map(|_| ()));
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Interface));
        h.run_steps(2);
        let density = h.state().t.config.interface.density;
        let numbers = h.state().t.config.interface.show_channel_numbers;
        // ← from a choice row and from a toggle row: the page list, the value untouched
        for label in ["Density", "Channel numbers"] {
            focus(&mut h, label);
            assert!(h.state_mut().act(Action::Left));
            assert_eq!(h.state().s.pane, Pane::Pages, "{label}");
            assert!(h.state().t.config_edits.is_empty(), "{label}: ← changes nothing");
        }
        // the page list: ← stays, ↓ switches the page, → returns to the page
        h.state_mut().act(Action::Left);
        assert_eq!((h.state().s.pane, h.state().s.page), (Pane::Pages, SettingsPage::Interface));
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().s.page, SettingsPage::Favourites);
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::Right);
        assert_eq!((h.state().s.pane, h.state().s.page), (Pane::Content, SettingsPage::Interface));
        // Shift+→ / Shift+← (L / H) change a choice (wrapping), a toggle and a number; → still
        // increases
        focus(&mut h, "Density");
        h.state_mut().act(Action::Increase);
        h.state_mut().apply_config();
        assert_ne!(h.state().t.config.interface.density, density);
        h.state_mut().act(Action::Decrease);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.density, density);
        focus(&mut h, "Channel numbers");
        h.state_mut().act(if numbers { Action::Decrease } else { Action::Increase });
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.show_channel_numbers, !numbers);
        focus(&mut h, "Preview delay");
        let delay = h.state().t.config.interface.preview_delay_ms;
        h.state_mut().act(Action::Increase);
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Decrease);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.preview_delay_ms, delay + 100);
        assert_eq!(h.state().s.pane, Pane::Content, "value keys never leave the page");
        // Tab and Esc go to the page list as before
        h.state_mut().act(Action::FocusNext);
        assert_eq!(h.state().s.pane, Pane::Pages);
        h.state_mut().act(Action::Activate);
        h.state_mut().act(Action::Back);
        assert_eq!(h.state().s.pane, Pane::Pages);
        // the source editor: ← keeps it open; with a change, ↓ in the page list keeps the page
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the playlist", |h| has_label(h, "My IPTV"));
        focus(&mut h, "My IPTV");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(has_label(&h, "EDIT MY IPTV"));
        focus(&mut h, "Icon");
        h.state_mut().act(Action::Left);
        assert_eq!(h.state().s.pane, Pane::Pages);
        assert!(h.state().s.editor.is_some(), "← keeps the editor");
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Increase);
        let icon = h.state().s.editor.as_ref().map(|e| e.icon);
        assert_ne!(icon, Some(SourceEditor::new(&h.state().s.editor.as_ref().unwrap().original).icon));
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().s.page, SettingsPage::Playlists, "unsaved changes keep the page");
        assert_eq!(h.state().s.editor.as_ref().map(|e| e.icon), icon, "and the changes");
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some(UNSAVED));
        assert_eq!(h.state().s.pane, Pane::Content, "back in the editor, where Esc discards");
        // so a second (or held) ↓ moves through the editor instead of piling up toasts
        let toasts = h.state().t.toasts.len();
        h.state_mut().act(Action::Down);
        assert_eq!((h.state().t.toasts.len(), h.state().s.pane), (toasts, Pane::Content));
        // Esc in the page list doesn't leave Settings with the changes either
        h.state_mut().act(Action::Left);
        assert!(h.state_mut().act(Action::Back), "Settings stays");
        assert_eq!(h.state().s.pane, Pane::Content);
        assert_eq!(h.state().s.editor.as_ref().map(|e| e.icon), icon);
        // one Esc in the editor discards them; then ← ↓ switches the page
        h.state_mut().act(Action::Back);
        assert!(h.state().s.editor.is_none());
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().s.page, SettingsPage::Epg);
    }

    const UNSAVED: &str = "Unsaved playlist changes: Enter on Save, or Esc to discard";

    /// The page-list guard holds only real changes: an untouched editor lets ↓ switch the page.
    /// With a change, a click on another page keeps the page, the change and the editor's
    /// highlight.
    #[test]
    fn the_page_list_guard_holds_only_unsaved_changes_and_covers_clicks() {
        let (mut t, one) = rig();
        seed(&mut t, |c| testkit::source(c, "My IPTV").map(|_| ()));
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        let open = |h: &mut H| {
            h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
            until(h, "the playlist", |h| has_label(h, "My IPTV"));
            focus(h, "My IPTV");
            h.state_mut().act(Action::Activate);
            h.run_steps(2);
            assert!(has_label(h, "EDIT MY IPTV"));
        };
        open(&mut h);
        let toasts = h.state().t.toasts.len();
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state().s.page, SettingsPage::Epg, "nothing to lose");
        assert_eq!(h.state().t.toasts.len(), toasts, "no toast");
        open(&mut h);
        focus(&mut h, "Icon");
        h.state_mut().act(Action::Increase);
        let icon = h.state().s.editor.as_ref().map(|e| e.icon);
        h.state_mut().act(Action::Left);
        h.run_steps(1);
        h.get_by_label("Interface").click();
        h.run_steps(2);
        assert_eq!((h.state().s.page, h.state().s.pane), (SettingsPage::Playlists, Pane::Content));
        assert_eq!(h.state().s.editor.as_ref().map(|e| e.icon), icon, "the change is kept");
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some(UNSAVED));
    }

    /// Compact (one pane at a time): ← from a row shows the page list alone, → brings back the
    /// page on the same row.
    #[test]
    fn compact_left_shows_the_page_list_and_right_returns_to_the_row() {
        let (mut t, one) = rig();
        t.layout = LayoutClass::Compact;
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(420.0, 900.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Interface));
        h.run_steps(2);
        focus(&mut h, "Density");
        let row = h.state().s.row;
        h.state_mut().act(Action::Left);
        h.run_steps(2);
        assert!(has_label(&h, "Hidden items") && !has_label(&h, "Density"), "the page list alone");
        h.state_mut().act(Action::Right);
        h.run_steps(2);
        assert!(has_label(&h, "Density") && !has_label(&h, "Hidden items"), "the page again");
        assert_eq!((h.state().s.page, h.state().s.row), (SettingsPage::Interface, row));
    }

    /// Shift+← on a parental row that needed the PIN is retried as the same Shift+← (not as
    /// Enter), the relock time is editable from its row, and "Lock now" locks the session.
    #[test]
    fn parental_rows_retry_the_same_adjustment_after_the_pin_and_lock_now_locks() {
        let (mut t, one) = rig();
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(hash_pin("1234", "s")),
            relock_min: 30,
        };
        t.config = std::sync::Arc::new(cfg);
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1200.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Parental));
        h.run_steps(1);
        focus(&mut h, "Lock again after");
        h.state_mut().act(Action::Decrease);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]), "{nav:?}");
        // the PIN overlay unlocks and answers Unlocked(Activate)
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        app.result(OverlayResult::Unlocked(Action::Activate));
        app.apply_config();
        assert_eq!(h.state().t.config.parental.relock_min, 25, "the Shift+← was retried (Enter would have added 5)");
        // the session is open now: → applies directly
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Right);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.parental.relock_min, 35);
        assert!(h.state().t.parental.is_unlocked(&h.state().t.config.parental));
        // "Lock now" (Enter on the row) closes the session
        focus(&mut h, "Lock now");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let app = h.state();
        assert!(!app.t.parental.is_unlocked(&app.t.config.parental), "locked again");
    }

    /// DESIGN §20.3, Settings ▸ Parental without a PIN: hidden explicit categories are only
    /// counted; "Show explicit content" first offers a PIN (which focuses the New PIN field) or
    /// shows without one; once shown, the categories are listed and Enter says "Not explicit",
    /// which the store keeps.
    #[test]
    fn show_explicit_content_offers_a_pin_then_lists_the_categories() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            c.execute(
                "INSERT INTO grp(source_id, kind, key, name, raw_name, provider_order, item_count, seen_gen, explicit)
                 VALUES (?1, 'channel', 'channel:xxx', 'XXX', 'XXX', 0, 0, 1, 1)",
                [src],
            )?;
            Ok(())
        });
        let shown = crate::store::library::load(&t.services().store.reader().unwrap()).unwrap();
        t.set_library(shown.hiding_explicit());
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1200.0, 1600.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Parental));
        h.run_steps(1);
        let rows = |h: &mut H| {
            let app = h.state_mut();
            app.t.with_ctx(|ctx| app.s.rows(ctx))
        };
        let notes = |rows: &[Row]| rows.iter().filter_map(|r| if let Row::Note(n, _) = r { Some(n.clone()) } else { None }).collect::<Vec<_>>();
        let r = rows(&mut h);
        assert!(notes(&r).iter().any(|n| n == "1 category is hidden as explicit. Show explicit content to review them."), "{:?}", notes(&r));
        assert!(!r.iter().any(|r| matches!(r, Row::Item { item: Item::Explicit(_), .. })), "no names while hidden");

        focus(&mut h, "Show explicit content");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::ContextMenu(menu))] = nav.as_slice() else { panic!("{nav:?}") };
        assert_eq!(menu.title, "Show explicit content?");
        assert_eq!(menu.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Set a PIN first (recommended)", "Show without a PIN"]);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_PIN_FIRST)));
        h.state_mut().apply_config();
        assert!(!h.state().t.config.parental.show_explicit, "a PIN first: nothing shown yet");
        assert!(matches!(rows(&mut h).get(h.state().s.row), Some(Row::Local { field: Local::PinNew, .. })), "on the New PIN field");

        focus(&mut h, "Show explicit content");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        h.state_mut().nav();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SHOW_EXPLICIT)));
        h.state_mut().apply_config();
        assert!(h.state().t.config.parental.show_explicit);
        // the app's rebuild shows it: the categories are listed
        h.state_mut().t.set_library(shown);
        h.run_steps(1);
        focus(&mut h, "XXX");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        until(&mut h, "the override", |h| {
            let c = h.state().t.services().store.reader().unwrap();
            c.query_row("SELECT explicit FROM group_override WHERE group_key = 'channel:xxx'", [], |r| r.get::<_, Option<i64>>(0)).ok().flatten()
                == Some(0)
        });
    }

    /// Orphans of explicit categories aren't named while explicit content is hidden: a note
    /// counts them; shown, they are listed.
    #[test]
    fn orphans_of_explicit_categories_stay_unnamed_while_hidden() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "TV")?;
            let list = crate::store::user::default_fav_list(c)?;
            for (i, (name, group)) in [("blue film", "xxx live"), ("news hour", "uk news")].iter().enumerate() {
                c.execute(
                    "INSERT INTO fav_item(list_id, source_id, kind, item_key, position, added_at, fp, orphaned_since) VALUES (?1, ?2, 'channel', ?3, ?4, 1, ?5, 5)",
                    rusqlite::params![list, s, format!("m3u:gone{i}"), i as i64, format!("{{\"norm_name\":\"{name}\",\"norm_group\":\"{group}\"}}")],
                )?;
            }
            Ok(())
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Orphans));
        until(&mut h, "the orphans", |h| has_label(h, "news hour"));
        assert!(!has_label(&h, "blue film"), "no PIN, switch off: hidden by default");
        assert!(has_label(&h, "1 more hidden as explicit. Show explicit content to review them."));

        std::sync::Arc::make_mut(&mut h.state_mut().t.config).parental.show_explicit = true;
        h.run_steps(2);
        assert!(has_label(&h, "blue film") && has_label(&h, "news hour"), "shown: listed");
        assert!(!has_label(&h, "1 more hidden as explicit. Show explicit content to review them."));
    }

    /// While a text field is being edited, pointer motion over other rows doesn't move the
    /// focus — the typed text lands in the field that has it (UX review M1).
    #[test]
    fn hover_never_moves_the_focus_while_a_field_is_edited() {
        let (t, one) = rig();
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1200.0, 1600.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Player));
        h.run_steps(2);
        focus(&mut h, "Subtitle languages");
        let row = h.state().s.row;
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        // the empty right end of the user-agent row (labels and fields take the hover themselves)
        let y = h.query_all_by_label("Default User-Agent").next().expect("the user-agent row").rect().center().y;
        for x in [1100.0, 1106.0, 1112.0] {
            h.hover_at(egui::pos2(x, y));
            h.run_steps(1);
        }
        assert_eq!(h.state().s.row, row, "the focus stays on the edited field");
        type_text(&mut h, ", de");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        h.state_mut().apply_config();
        let c = h.state().t.config.clone();
        assert!(c.player.sub_langs.iter().any(|l| l == "de"), "{:?}", c.player.sub_langs);
        assert!(!c.player.user_agent.contains("de"), "{}", c.player.user_agent);
    }

    /// Compact shows one pane at a time: `‹ Settings` above a page leads back to the page list
    /// (the mouse twin of Esc there), and a page from the list opens it again.
    #[test]
    fn compact_pages_are_reached_by_the_back_button() {
        let (mut t, one) = rig();
        t.layout = LayoutClass::Compact;
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(420.0, 900.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Player));
        h.run_steps(2);
        assert!(has_label(&h, "Hardware decoding") && !has_label(&h, "Hidden items"), "the page, alone");
        // left mid-edit, like Esc: what was typed is kept
        focus(&mut h, "Subtitle languages");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        type_text(&mut h, ", de");
        h.get_by_label("Back to Settings").click();
        h.run_steps(2);
        assert!(has_label(&h, "Hidden items") && !has_label(&h, "Hardware decoding"), "the page list");
        h.state_mut().apply_config();
        assert!(h.state().t.config.player.sub_langs.iter().any(|l| l == "de"), "{:?}", h.state().t.config.player.sub_langs);
        h.get_by_label("Interface").click();
        h.run_steps(2);
        assert_eq!(h.state().s.page, SettingsPage::Interface);
        assert!(has_label(&h, "Back to Settings"));
    }

    /// A sub-view's `‹ {page}` button closes it like Esc: the source editor goes, the page's
    /// rows are back.
    #[test]
    fn the_source_editor_back_button_closes_it_like_esc() {
        let (mut t, one) = rig();
        seed(&mut t, |c| testkit::source(c, "Alpha").map(|_| ()));
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the source", |h| has_label(h, "Alpha"));
        focus(&mut h, "Alpha");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(has_label(&h, "EDIT ALPHA"));
        h.get_by_label("Back to Playlists").click();
        h.run_steps(2);
        assert!(!has_label(&h, "EDIT ALPHA") && has_label(&h, "Alpha"), "the editor closed");
    }

    /// ←/→ on a guide's offset shows at once but is saved (and the guide re-imported) once the
    /// presses settle, not per press.
    #[test]
    fn epg_offset_presses_are_saved_once_they_settle() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ch = testkit::channels(c, s, 2)?;
            testkit::epg(c, &ch, 0, 1, true)
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1200.0, 900.0));
        let stored = |h: &H| crate::store::sources::list_epg_sources(&h.state().t.services().store.reader().unwrap()).unwrap()[0].offset_min;
        h.state_mut().enter(Route::Settings(SettingsPage::Epg));
        until(&mut h, "the guide", |h| has_label(h, "Seed EPG"));
        focus(&mut h, "Seed EPG");
        for _ in 0..3 {
            h.state_mut().act(Action::Right);
        }
        assert_eq!(h.state().s.epg_sources[0].offset_min, 45, "shown at once");
        h.run_steps(1);
        assert_eq!(stored(&h), 0, "not written per press");
        until(&mut h, "saved after the presses settle", |h| stored(h) == 45);
    }

    /// DESIGN §29.14: Hidden ▸ "Titles you separated or pinned" lists the copies taken out of
    /// their stack and the pinned ones; Enter undoes one (stored at once). Interface ▸ "Same
    /// title in several playlists" switches between Combine and Show each copy.
    #[test]
    fn split_and_pinned_titles_are_listed_with_undo_and_the_combine_setting() {
        use crate::store::titles::{LinkAction, links, set_pin, set_split};
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("EN ★ Heat - 1995", "Heat")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune (Bravo)"), ("EN - Heat (1995)", "Heat (Bravo)")])?;
            let r = |s: SourceId, i: usize| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) };
            set_split(c, &r(b, 0), true, 5)?;
            set_pin(c, &r(a, 1), &[r(b, 1)], true, 6)
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Hidden));
        until(&mut h, "the titles", |h| has_label(h, "Dune (Bravo)") && has_label(h, "Heat"));
        assert!(has_label(&h, "TITLES YOU SEPARATED OR PINNED"));
        assert!(testkit::has_label_containing(&h, "Bravo · shown on its own") && testkit::has_label_containing(&h, "Alpha · always plays first"));
        focus(&mut h, "Dune (Bravo)");
        h.state_mut().act(Action::Activate);
        let stored = |h: &H| links(&h.state().t.services().store.reader().unwrap()).unwrap();
        until(&mut h, "combined again", |h| stored(h).iter().all(|l| l.action != LinkAction::Split));
        assert_eq!(stored(&h).len(), 1, "the pin stays");
        // Interface: Combine → Show each copy
        h.state_mut().enter(Route::Settings(SettingsPage::Interface));
        h.run_steps(1);
        focus(&mut h, "Same title in several playlists");
        h.state_mut().act(Action::Right);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.same_title, crate::config::SameTitle::ShowEach);
        focus(&mut h, "Which copy plays first");
        h.state_mut().act(Action::Right);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.interface.copy_order, crate::config::CopyOrder::BestQuality);
    }

    #[test]
    fn config_rows_round_trip_through_their_setters() {
        let mut c = Config::default();
        (PLAYER_N[0].set)(&mut c, 25.0);
        assert_eq!(c.player.cache_secs, 25);
        assert_eq!((PLAYER_N[0].get)(&c), 25.0);
        (IFACE_N[2].set)(&mut c, 1.25);
        assert_eq!(c.interface.ui_scale, 1.25);
        (IFACE_C[0].set)(&mut c, 4);
        assert_eq!(c.general.start_screen, StartScreen::Last);
        assert_eq!((IFACE_C[0].get)(&c), 4);
        (PLAYER_TEXT[3].set)(&mut c, "profile=fast; cache = yes; junk".into());
        assert_eq!(c.player.mpv_options.get("cache").map(String::as_str), Some("yes"));
        assert_eq!(c.player.mpv_options.len(), 2);
        assert_eq!((PLAYER_TEXT[3].get)(&c), "cache=yes; profile=fast");
        (PLAYER_TEXT[0].set)(&mut c, "de, en ;fr".into());
        assert_eq!(c.player.audio_langs, vec!["de", "en", "fr"]);
        (PLAYER_HWDEC.set)(&mut c, 2);
        assert_eq!(c.player.hwdec, "no");
        assert_eq!(number_text(&EPG_N[2], 0.92), "0.92");
        assert_eq!(number_text(&PLAYER_N[0], 10.0), "10 s");
    }

    #[test]
    fn every_config_section_of_the_design_table_has_rows() {
        // DESIGN §13.9: every setting is editable — spot-check the less obvious ones
        let mut c = Config::default();
        (IFACE_T[2].set)(&mut c, true);
        assert!(c.interface.live_preview_autoplay);
        (IFACE_C[6].set)(&mut c, 2);
        assert_eq!(c.integration.idle_inhibit, IdleInhibitMode::Never);
        (EPG_N[1].set)(&mut c, 7.0);
        assert_eq!(c.general.epg_keep_past_days, 7);
        (PLAYLIST_CFG_C.set)(&mut c, 2);
        assert_eq!(c.general.live_format, LiveFormat::M3u8);
        (IFACE_TEXT[1].set)(&mut c, "/a.ttf, /b.otf".into());
        assert_eq!(c.fonts.extra_fallbacks.len(), 2);
        (PARENTAL_RELOCK.set)(&mut c, 30.0);
        assert_eq!(c.parental.relock_min, 30);
        (IFACE_C[4].set)(&mut c, 3);
        assert_eq!(c.interface.movie_sort, SortMode::Year);
    }

    #[test]
    fn source_editor_validates_and_detects_refresh() {
        let src = Source {
            id: SourceId(1),
            name: "My IPTV".into(),
            kind: SourceKind::Xtream,
            icon: glyphs::PLAYLIST.into(),
            color: SourceColor::Blue,
            url: None,
            path: None,
            xtream: Some(crate::model::XtreamCredentials {
                server: "http://p.example:8080".into(),
                username: "u".into(),
                password: crate::redact::Secret::new("pw".into()),
            }),
            settings: Default::default(),
            enabled: true,
            sort_index: 0,
            created_at: 0,
            status: Default::default(),
        };
        let mut e = SourceEditor::new(&src);
        assert_eq!(e.color, 6);
        e.name = "Renamed".into();
        let (s, refresh) = e.apply().unwrap();
        assert_eq!(s.name, "Renamed");
        assert_eq!(s.xtream.as_ref().unwrap().password.expose(), "pw", "an empty password field keeps the stored one");
        assert!(!refresh, "a rename needs no refresh");
        e.password = "new".into();
        assert!(e.apply().unwrap().1, "new credentials refresh");
        e.server = "".into();
        assert_eq!(e.apply().unwrap_err().0, Local::SrcServer);
        assert!(!needs_credentials(&src));
        let detail = source_detail(&src, 1000, None, Some(0));
        assert!(detail.starts_with("Xtream · 0 ch") && detail.ends_with("Never refreshed"), "{detail}");
    }

    /// DESIGN §27.3/§27.11: an editor opened on an M3U playlist that a refresh then upgrades
    /// follows the row (Save must not drop the new login), keeps what the user typed, offers
    /// the switch back, and saves the panel's path-prefixed server untouched.
    #[test]
    fn source_editor_follows_a_silent_upgrade() {
        let url = "http://p.example/sub/get.php?username=u&password=pw";
        let m3u = Source {
            id: SourceId(1),
            name: "Provider".into(),
            kind: SourceKind::M3uUrl,
            icon: glyphs::PLAYLIST.into(),
            color: SourceColor::Blue,
            url: Some(crate::redact::Secret::new(url.into())),
            path: None,
            xtream: None,
            settings: Default::default(),
            enabled: true,
            sort_index: 0,
            created_at: 0,
            status: Default::default(),
        };
        let mut e = SourceEditor::new(&m3u);
        assert!(!e.offers_xtream_switch(), "an M3U playlist upgrades on its own");
        e.name = "Renamed".into();
        let upgraded = Source {
            kind: SourceKind::Xtream,
            xtream: Some(crate::model::XtreamCredentials {
                server: "http://p.example/sub".into(),
                username: "u".into(),
                password: crate::redact::Secret::new("pw".into()),
            }),
            ..m3u.clone()
        };
        e.rebase(&upgraded);
        assert!(e.offers_xtream_switch() && e.use_xtream);
        let (s, refresh) = e.apply().unwrap();
        assert_eq!(s.name, "Renamed", "the user's edit is kept");
        assert_eq!(s.kind, SourceKind::Xtream);
        assert_eq!(s.xtream.as_ref().map(|x| x.server.as_str()), Some("http://p.example/sub"), "an unchanged server isn't normalized");
        assert_eq!(s.url.as_ref().map(|u| u.expose().as_str()), Some(url), "the playlist stays for switching back");
        assert!(!refresh);
        assert!(source_detail(&upgraded, 1000, None, None).starts_with("Xtream (from M3U) · "));
        // a playlist the user kept as M3U offers the login
        let kept = Source { settings: crate::model::SourceSettings { keep_m3u: true, ..Default::default() }, ..m3u };
        let e = SourceEditor::new(&kept);
        assert!(e.offers_xtream_switch() && !e.use_xtream);
    }

    /// DESIGN §27.3: "Use the Xtream login" on a playlist kept as M3U saves the choice and runs
    /// a forced refresh; while the playlist is still M3U afterwards it stays kept (no upgrade
    /// attempts the user didn't ask for) and the open editor follows the row, still offering
    /// the toggle. A sign-in that didn't happen says so.
    #[test]
    fn use_the_xtream_login_reports_a_failed_sign_in_and_stays_kept() {
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let id = testkit::source(c, "Kept")?;
            crate::store::sources::set_source_settings(c, id, &crate::model::SourceSettings { keep_m3u: true, ..Default::default() })?;
            Ok(id)
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Playlists));
        until(&mut h, "the source", |h| has_label(h, "Kept"));
        focus(&mut h, "Kept");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        let use_xtream = |h: &H| h.state().s.editor.as_ref().is_some_and(|e| e.use_xtream);
        let sign_in_failed = |h: &H| {
            h.state().t.toasts.iter().filter(|t| t.text == "Couldn't sign in with this playlist's Xtream login — still using the playlist").count()
        };
        // the real refresh fails (nothing listens on the playlist's port)
        focus(&mut h, "Use the Xtream login");
        h.state_mut().act(Action::Activate);
        h.run_steps(1); // acted on in the frame
        assert!(use_xtream(&h) && h.state().s.login_switch.as_ref().is_some_and(|w| w.source == id));
        until(&mut h, "the refresh, then the playlist kept again", |h| {
            h.state().s.login_switch.is_none() && sources(h)[0].settings.keep_m3u && h.state().s.editor.as_ref().is_some_and(|e| !e.use_xtream)
        });
        assert!(h.state().s.editor.as_ref().is_some_and(SourceEditor::offers_xtream_switch));
        // the refresh answered without signing in
        let finished = |job: JobId, upgraded| {
            AppEvent::Job(JobEvent::Finished {
                id: job,
                kind: JobKind::RefreshSource { source: id, force: true },
                outcome: Ok(JobOutcome::SourceRefreshed { source: id, summary: Default::default(), not_modified: false, account: None, upgraded }),
                failure: None,
            })
        };
        let deliver = |h: &mut H, ev: AppEvent| {
            let app = h.state_mut();
            app.t.with_ctx(|ctx| app.s.on_event(ctx, &ev));
        };
        focus(&mut h, "Use the Xtream login");
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        // the job that runs this request, once the setting is saved (before the next frame
        // pumps the real answer)
        let ours = {
            let w = h.state_mut().s.login_switch.as_mut().expect("waiting for the refresh");
            let s = w.ticket.take().expect("submitted with the setting").wait().unwrap();
            w.submitted = Some(s);
            match s {
                Submitted::In(j) => j,
                Submitted::After(j) => JobId(j.0 + 1),
            }
        };
        // a forced refresh that was already running read the playlist still kept: its result
        // is not the toggle's (no error, and the user's choice is not reverted)
        deliver(&mut h, finished(JobId(0), false));
        assert_eq!(sign_in_failed(&h), 0);
        assert!(h.state().s.login_switch.is_some() && use_xtream(&h));
        deliver(&mut h, finished(ours, false));
        assert_eq!(sign_in_failed(&h), 1);
        assert!(h.state().s.login_switch.is_none());
        h.state_mut().s.login_switch = Some(LoginSwitch { source: id, ticket: None, submitted: Some(Submitted::In(ours)) });
        deliver(&mut h, finished(ours, true));
        assert_eq!(sign_in_failed(&h), 1, "an upgrade is the app's toast");
    }

    /// The iptv-org case: a guide that lists 2 channels matching 3 of 11 249 says so.
    #[test]
    fn guide_rows_say_how_much_they_cover() {
        let e = EpgSource {
            id: EpgSourceId(1),
            name: "iptv-org".into(),
            kind: EpgSourceKind::Url { url: crate::redact::Secret::new("http://example.com/guide.xml".into()) },
            priority: 0,
            offset_min: 0,
            enabled: true,
            refresh_h: None,
            active_generation: 1,
            last_refresh: Some(1),
            last_error: None,
            etag: None,
            last_modified: None,
            channel_count: 2,
            programme_count: 312,
            max_stop: Some(10),
        };
        assert_eq!(coverage_text(&e, 3, 11_249), "Matches 3 of 11\u{2009}249 channels · guide lists 2 channels, 312 programmes");
        let note = coverage_note(std::slice::from_ref(&e), 3, 11_249).expect("a note for poor coverage");
        assert!(note.starts_with("Your guides cover 3 of 11\u{2009}249 channels") && note.contains("EPG mapping"), "{note}");
        assert_eq!(coverage_note(std::slice::from_ref(&e), 9_000, 11_249), None, "good coverage needs no note");
        assert_eq!(coverage_note(&[], 0, 11_249), None, "no guide: the page says so already");
    }

    /// The "Matches N" count takes only live mappings: channels of enabled playlists, mapped
    /// to the guide's current data, not orphaned.
    #[test]
    fn guide_matches_counts_live_mappings_only() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::migrations::migrate(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO source(id, name, kind, created_at, enabled) VALUES (1, 'on', 'm3u-url', 0, 1), (2, 'off', 'm3u-url', 0, 0);
             INSERT INTO channel(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen) VALUES
               (1, 'a', 'A', 'A', 'a', '{}', 0, 1), (1, 'b', 'B', 'B', 'b', '{}', 1, 1), (1, 'c', 'C', 'C', 'c', '{}', 2, 1),
               (1, 'd', 'D', 'D', 'd', '{}', 3, 1), (2, 'e', 'E', 'E', 'e', '{}', 0, 1);
             INSERT INTO epg_source(id, name, kind, active_gen) VALUES (1, 'g', 'url', 2);
             INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (1, 'x', 2), (1, 'y', 2), (1, 'old', 1);
             INSERT INTO channel_epg_map(source_id, channel_key, epg_source_id, xmltv_id, method, orphaned_since) VALUES
               (1, 'a', 1, 'x', 'tvg-id', NULL),   -- automatic
               (1, 'b', 1, 'y', 'manual', NULL),   -- manual
               (1, 'c', 1, 'x', 'manual', 5),      -- orphaned
               (1, 'd', 1, 'old', 'tvg-id', NULL), -- the guide's previous data
               (2, 'e', 1, 'x', 'tvg-id', NULL);   -- a disabled playlist",
        )
        .unwrap();
        assert_eq!(guide_matches(&c).unwrap(), HashMap::from([(EpgSourceId(1), 2)]));
    }

    #[test]
    fn helpers() {
        assert_eq!(split_list(" a, b;;c "), vec!["a", "b", "c"]);
        assert!(parse_mpv_options("").is_empty());
    }
}
