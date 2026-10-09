//! Keyboard model: per-context keymap tables (the SINGLE source of truth — the `?` help overlay,
//! the status-bar hint line and DESIGN §15 are generated from these tables). Owner: WP-10a.
//!
//! Resolution (DESIGN §15.1): the app builds a context stack each frame, innermost first
//! (e.g. `[Modal]`, `[ListFiltering]`, `[PlayerOsd, PlayerLive]`, `[TextInput, Modal]`), then
//! appends `Global`. Resolution happens in `App::raw_input_hook`, BEFORE egui sees the input;
//! every event that resolves to an action is REMOVED from the input so egui widgets never
//! handle it a second time (Enter would otherwise also click the focused button).
//!
//! Rules:
//! * For each event the first matching binding wins, innermost context first.
//! * Filterable contexts (`List`, `ListFiltering`, `PosterGrid`, `PosterGridFiltering`,
//!   `Search`): an unbound printable character (no Ctrl/Alt) becomes [`Action::FilterInput`] —
//!   the launcher idiom: in filterable lists letters type, so there is NO j/k there; vim keys
//!   live in the guide grid, the player and settings panels.
//! * `TextInput` owns the keyboard: after its own table, only Ctrl/Alt chords and F-keys may
//!   reach `Global`; every other event (Space, ←/→, Delete, Backspace…) returns `None` so the
//!   focused egui text widget edits text. Contexts below `TextInput` are never consulted.
//! * `Modal` swallows printable text that it does not bind.
//! * Key auto-repeat never triggers `Parent`/`Back` (holding Backspace to clear a filter must
//!   not continue into "leave the pane").
//! * `Global` contains only Ctrl/Alt/F-key chords so it never steals typing.
//! * No screen reads raw keys: every key a screen reacts to is in a table (so help is complete).
//!
//! Printable symbols (`?`, `/`, `[`, `F`) are matched against `egui::Event::Text` (layout
//! independent, shift already applied); named keys and Ctrl/Alt chords against `Event::Key`.

use eframe::egui::{self, Key};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyContext {
    /// Always active, lowest priority. Ctrl/Alt/F-key chords only.
    Global,
    /// A filterable list with an empty filter (launcher idiom).
    List,
    /// A filterable list with a non-empty filter: printable keys edit the filter.
    ListFiltering,
    /// Poster grids and Home rails: 2-D arrows; typing starts a filter (grids only).
    PosterGrid,
    /// Poster grid with a non-empty filter.
    PosterGridFiltering,
    /// Movie / series detail pages (buttons, seasons, episodes).
    Detail,
    /// EPG grid: no filter; vim keys; digits jump to a channel number.
    Guide,
    /// Fullscreen player, live content. The passive info banner does NOT change the context.
    PlayerLive,
    /// Fullscreen player, seekable content (VOD / catch-up).
    PlayerVod,
    /// Controls focus mode (entered with Enter / `i` / Tab / click on a control): arrows move
    /// between OSD controls; unbound keys fall through to PlayerLive/Vod.
    PlayerOsd,
    /// Typing a channel number (player, guide): digits, Enter, Backspace, Esc.
    NumericEntry,
    /// EPG mapping screen extras (assign / clear), stacked above a list context.
    EpgMapping,
    /// Global search overlay (always typing).
    Search,
    /// Dialogs, context menus, popovers, pickers, onboarding choice cards.
    Modal,
    /// A focused text field (settings, onboarding forms, rename, PIN).
    TextInput,
    /// Settings panes (panel idiom: h/j/k/l allowed because there is no filter).
    Panel,
}

impl KeyContext {
    /// Every context, in help-overlay / documentation order.
    pub const ALL: [KeyContext; 16] = [
        KeyContext::Global,
        KeyContext::List,
        KeyContext::ListFiltering,
        KeyContext::PosterGrid,
        KeyContext::PosterGridFiltering,
        KeyContext::Detail,
        KeyContext::Guide,
        KeyContext::PlayerLive,
        KeyContext::PlayerVod,
        KeyContext::PlayerOsd,
        KeyContext::NumericEntry,
        KeyContext::EpgMapping,
        KeyContext::Search,
        KeyContext::Modal,
        KeyContext::TextInput,
        KeyContext::Panel,
    ];

    pub fn title(self) -> &'static str {
        match self {
            KeyContext::Global => "Everywhere",
            KeyContext::List => "Lists",
            KeyContext::ListFiltering => "Lists while filtering",
            KeyContext::PosterGrid => "Movies & series grid",
            KeyContext::PosterGridFiltering => "Movies & series grid while filtering",
            KeyContext::Detail => "Movie & series pages",
            KeyContext::Guide => "TV guide",
            KeyContext::PlayerLive => "Player — live TV",
            KeyContext::PlayerVod => "Player — movies, episodes, catch-up",
            KeyContext::PlayerOsd => "Player — on-screen controls",
            KeyContext::NumericEntry => "Channel number entry",
            KeyContext::EpgMapping => "EPG mapping",
            KeyContext::Search => "Search",
            KeyContext::Modal => "Dialogs & menus",
            KeyContext::TextInput => "Text fields",
            KeyContext::Panel => "Settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // ---- navigation
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    /// Enter: see DESIGN §13 "Enter rule" (play here / open / confirm).
    Activate,
    /// Shift+Enter: always fullscreen (lists) / secondary confirm.
    ActivateAlt,
    /// Esc / Alt+← / mouse back: step out one layer (DESIGN §15.1 Esc ladder).
    Back,
    /// Left/Backspace on an empty filter: go to the parent pane.
    Parent,
    FocusNext,
    FocusPrev,
    /// `[` / `]` on detail pages: previous / next season (tab strip).
    TabPrev,
    TabNext,
    ContextMenu,
    // ---- filter editing
    FilterInput(char),
    FilterBackspace,
    FilterDeleteWord,
    FilterClear,
    // ---- global
    OpenSearch,
    ToggleHelp,
    GoHome,
    GoLive,
    GoGuide,
    GoMovies,
    GoSeries,
    GoSettings,
    Quit,
    ToggleWindowFullscreen,
    /// Expand the mini player to the fullscreen player.
    OpenPlayer,
    StopPlayback,
    GlobalPlayPause,
    RefreshSource,
    ShowJobs,
    // ---- items
    ToggleFavourite,
    AddToList,
    ToggleHidden,
    Rename,
    ToggleLock,
    MoveUp,
    MoveDown,
    /// Ctrl+S: small sort & view menu (launcher card) — never a blind cycle.
    SortMenu,
    OpenInGuide,
    ShowDetails,
    AssignEpg,
    /// Delete in the EPG mapping screen: clear the manual assignment.
    ClearEpg,
    MarkWatched,
    /// Delete: remove from a favourites list / cancel a job / forget an orphan / cancel a
    /// reminder (whatever "remove" means for the focused row).
    Remove,
    // ---- guide
    GuidePrevDay,
    GuideNextDay,
    GuideNow,
    GuideBackHours,
    GuideForwardHours,
    GuideCategory,
    /// Toggle a reminder for the focused (future) programme.
    GuideRemind,
    GuideWatch,
    GuideCatchup,
    // ---- player
    PlayPause,
    Mute,
    VolumeUp,
    VolumeDown,
    ZapPrev,
    ZapNext,
    LastChannel,
    ChannelHistory,
    ChannelList,
    MiniGuide,
    SeekBack,
    SeekForward,
    SeekBackBig,
    SeekForwardBig,
    JumpToLive,
    /// Enter / `i` / Tab: enter OSD controls focus mode (pushes `PlayerOsd`).
    FocusOsdControls,
    /// Esc in controls focus mode: leave it (the info banner stays until its timeout).
    LeaveOsdControls,
    StreamInfo,
    AudioMenu,
    SubtitleMenu,
    CycleAudio,
    CycleSubtitle,
    SubDelayMinus,
    SubDelayPlus,
    SubDelayReset,
    SubSizeMinus,
    SubSizePlus,
    CycleAspect,
    CycleZoom,
    Minimize,
    StopAndClose,
    Retry,
    Digit(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Key(Key),
    /// Matched against `Event::Text` (shift applied by the layout).
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub trigger: Trigger,
    pub ctrl: bool,
    pub alt: bool,
    /// Only meaningful for `Trigger::Key`.
    pub shift: bool,
}

impl KeyChord {
    pub const fn key(k: Key) -> Self { Self { trigger: Trigger::Key(k), ctrl: false, alt: false, shift: false } }
    pub const fn ch(c: char) -> Self { Self { trigger: Trigger::Char(c), ctrl: false, alt: false, shift: false } }
    pub const fn ctrl(k: Key) -> Self { Self { trigger: Trigger::Key(k), ctrl: true, alt: false, shift: false } }
    pub const fn alt(k: Key) -> Self { Self { trigger: Trigger::Key(k), ctrl: false, alt: true, shift: false } }
    pub const fn shift(k: Key) -> Self { Self { trigger: Trigger::Key(k), ctrl: false, alt: false, shift: true } }
    pub const fn ctrl_shift(k: Key) -> Self { Self { trigger: Trigger::Key(k), ctrl: true, alt: false, shift: true } }

    /// Human label: "Ctrl+K", "Shift+Enter", "?", "↑".
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        match self.trigger {
            Trigger::Char(' ') => s.push_str("Space"),
            Trigger::Char(c) => s.push(c),
            Trigger::Key(k) => s.push_str(match k {
                Key::ArrowUp => "↑",
                Key::ArrowDown => "↓",
                Key::ArrowLeft => "←",
                Key::ArrowRight => "→",
                Key::Enter => "Enter",
                Key::Escape => "Esc",
                Key::Backspace => "Backspace",
                Key::Space => "Space",
                Key::PageUp => "PgUp",
                Key::PageDown => "PgDn",
                other => other.name(),
            }),
        }
        s
    }

    fn matches(&self, ev: &egui::Event) -> bool {
        match (self.trigger, ev) {
            (Trigger::Key(k), egui::Event::Key { key, pressed: true, modifiers, .. }) => {
                *key == k && modifiers.ctrl == self.ctrl && modifiers.alt == self.alt && modifiers.shift == self.shift
            }
            (Trigger::Char(c), egui::Event::Text(t)) => !self.ctrl && !self.alt && t.chars().eq(std::iter::once(c)),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub chord: KeyChord,
    pub action: Action,
    /// Help overlay text (bindings with equal text are merged into one row: "↑ / k").
    pub help: &'static str,
}

const fn b(chord: KeyChord, action: Action, help: &'static str) -> Binding { Binding { chord, action, help } }

use Action as A;
use KeyChord as C;

pub static GLOBAL: &[Binding] = &[
    b(C::ctrl(Key::K), A::OpenSearch, "Search everything"),
    b(C::ctrl(Key::Q), A::Quit, "Quit"),
    b(C::alt(Key::Num1), A::GoHome, "Home"),
    b(C::alt(Key::Num2), A::GoLive, "Live TV"),
    b(C::alt(Key::Num3), A::GoGuide, "TV guide"),
    b(C::alt(Key::Num4), A::GoMovies, "Movies"),
    b(C::alt(Key::Num5), A::GoSeries, "Series"),
    b(C::ctrl(Key::Comma), A::GoSettings, "Settings"),
    b(C::alt(Key::Num0), A::GoSettings, "Settings"),
    b(C::alt(Key::ArrowLeft), A::Back, "Back (also the mouse back button)"),
    b(C::key(Key::F11), A::ToggleWindowFullscreen, "Toggle window fullscreen"),
    b(C::ctrl(Key::P), A::OpenPlayer, "Open the player (from the mini player)"),
    // not Ctrl+Space: that is fcitx5's input-method toggle on Omarchy
    b(C::ctrl_shift(Key::P), A::GlobalPlayPause, "Play / pause"),
    b(C::ctrl_shift(Key::S), A::StopPlayback, "Stop playback"),
    b(C::ctrl(Key::R), A::RefreshSource, "Refresh playlists & guide"),
    b(C::key(Key::F5), A::RefreshSource, "Refresh playlists & guide"),
    b(C::ctrl(Key::J), A::ShowJobs, "Show background tasks"),
    b(C::key(Key::F1), A::ToggleHelp, "Keyboard help"),
];

/// Shared by List / ListFiltering / PosterGrid(Filtering) / Detail / Search: item actions on
/// Ctrl chords and Delete.
pub static ITEM_ACTIONS: &[Binding] = &[
    b(C::ctrl(Key::F), A::ToggleFavourite, "Add to / remove from Favourites"),
    b(C::ctrl_shift(Key::F), A::AddToList, "Favourites lists… (add, new list)"),
    b(C::ctrl(Key::H), A::ToggleHidden, "Hide / unhide"),
    b(C::key(Key::F2), A::Rename, "Rename"),
    b(C::ctrl(Key::L), A::ToggleLock, "Lock with PIN / unlock"),
    b(C::alt(Key::ArrowUp), A::MoveUp, "Move up (reorder)"),
    b(C::alt(Key::ArrowDown), A::MoveDown, "Move down (reorder)"),
    b(C::ctrl(Key::S), A::SortMenu, "Sort & view options…"),
    b(C::ctrl(Key::G), A::OpenInGuide, "Show in TV guide"),
    b(C::ctrl(Key::I), A::ShowDetails, "Details / edit information"),
    b(C::ctrl(Key::E), A::AssignEpg, "Assign EPG channel"),
    b(C::ctrl(Key::W), A::MarkWatched, "Mark watched / unwatched"),
    b(C::key(Key::Delete), A::Remove, "Remove (from list / cancel task / forget)"),
    b(C::shift(Key::F10), A::ContextMenu, "Context menu"),
    b(C::ctrl(Key::Enter), A::ContextMenu, "Context menu"),
];

pub static LIST: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Home), A::Home, "First / last"),
    b(C::key(Key::End), A::End, "First / last"),
    b(C::key(Key::Enter), A::Activate, "Play here / open (playing → fullscreen)"),
    b(C::key(Key::ArrowRight), A::Activate, "Play here / open (playing → fullscreen)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play fullscreen"),
    b(C::key(Key::ArrowLeft), A::Parent, "Back to the previous pane"),
    b(C::key(Key::Backspace), A::Parent, "Back to the previous pane"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous pane"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous pane"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::ch('/'), A::OpenSearch, "Search everything"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
    // any other printable key starts filtering (FilterInput)
];

pub static LIST_FILTERING: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Enter), A::Activate, "Play here / open (playing → fullscreen)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play fullscreen"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
    b(C::key(Key::Escape), A::FilterClear, "Clear filter"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous pane"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous pane"),
    // printable keys → FilterInput
];

pub static POSTER_GRID: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::ArrowLeft), A::Left, "Move"),
    b(C::key(Key::ArrowRight), A::Right, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Home), A::Home, "First / last"),
    b(C::key(Key::End), A::End, "First / last"),
    b(C::key(Key::Enter), A::Activate, "Open details (channels: play)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play / resume fullscreen"),
    b(C::key(Key::Backspace), A::Parent, "Back to categories"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous pane"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous pane"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::ch('/'), A::OpenSearch, "Search everything"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
    // any other printable key starts filtering (FilterInput)
];

pub static POSTER_GRID_FILTERING: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::ArrowLeft), A::Left, "Move"),
    b(C::key(Key::ArrowRight), A::Right, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Enter), A::Activate, "Open details (channels: play)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play / resume fullscreen"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
    b(C::key(Key::Escape), A::FilterClear, "Clear filter"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous pane"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous pane"),
];

pub static DETAIL: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::ArrowLeft), A::Left, "Move"),
    b(C::key(Key::ArrowRight), A::Right, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::ch('['), A::TabPrev, "Previous / next season"),
    b(C::ch(']'), A::TabNext, "Previous / next season"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous section (buttons, seasons, episodes)"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous section (buttons, seasons, episodes)"),
    b(C::key(Key::Enter), A::Activate, "Play / resume (or the focused button)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play from the start"),
    b(C::key(Key::Backspace), A::Back, "Back"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::ch('/'), A::OpenSearch, "Search everything"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
];

pub static GUIDE: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Previous / next channel"),
    b(C::ch('k'), A::Up, "Previous / next channel"),
    b(C::key(Key::ArrowDown), A::Down, "Previous / next channel"),
    b(C::ch('j'), A::Down, "Previous / next channel"),
    b(C::key(Key::ArrowLeft), A::Left, "Previous / next programme"),
    b(C::ch('h'), A::Left, "Previous / next programme"),
    b(C::key(Key::ArrowRight), A::Right, "Previous / next programme"),
    b(C::ch('l'), A::Right, "Previous / next programme"),
    b(C::key(Key::PageUp), A::PageUp, "Page channels"),
    b(C::key(Key::PageDown), A::PageDown, "Page channels"),
    b(C::shift(Key::ArrowLeft), A::GuideBackHours, "Scroll time −/+ 3 h"),
    b(C::shift(Key::ArrowRight), A::GuideForwardHours, "Scroll time −/+ 3 h"),
    b(C::ch('H'), A::GuideBackHours, "Scroll time −/+ 3 h"),
    b(C::ch('L'), A::GuideForwardHours, "Scroll time −/+ 3 h"),
    b(C::ch('['), A::GuidePrevDay, "Previous / next day"),
    b(C::ch(']'), A::GuideNextDay, "Previous / next day"),
    b(C::ch('n'), A::GuideNow, "Jump to now"),
    b(C::key(Key::Home), A::GuideNow, "Jump to now"),
    b(C::ch('g'), A::Home, "First / last channel"),
    b(C::ch('G'), A::End, "First / last channel"),
    b(C::key(Key::Enter), A::Activate, "Programme details (Watch / Catch up / Remind)"),
    b(C::ch('w'), A::GuideWatch, "Watch channel"),
    b(C::shift(Key::Enter), A::GuideWatch, "Watch channel"),
    b(C::ch('c'), A::GuideCatchup, "Catch up (past programme)"),
    b(C::ch('r'), A::GuideRemind, "Remind me / cancel reminder"),
    b(C::ch('f'), A::GuideCategory, "Collection: favourites lists, groups, all"),
    b(C::ch('0'), A::Digit(0), "Jump to channel number"),
    b(C::ch('1'), A::Digit(1), "Jump to channel number"),
    b(C::ch('2'), A::Digit(2), "Jump to channel number"),
    b(C::ch('3'), A::Digit(3), "Jump to channel number"),
    b(C::ch('4'), A::Digit(4), "Jump to channel number"),
    b(C::ch('5'), A::Digit(5), "Jump to channel number"),
    b(C::ch('6'), A::Digit(6), "Jump to channel number"),
    b(C::ch('7'), A::Digit(7), "Jump to channel number"),
    b(C::ch('8'), A::Digit(8), "Jump to channel number"),
    b(C::ch('9'), A::Digit(9), "Jump to channel number"),
    b(C::ctrl(Key::F), A::ToggleFavourite, "Add channel to Favourites"),
    b(C::ctrl(Key::E), A::AssignEpg, "Assign EPG channel"),
    b(C::ch('/'), A::OpenSearch, "Search everything"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
    b(C::key(Key::Escape), A::Back, "Back"),
];

/// Bindings shared by both player contexts. The passive info banner (shown on zap, any key,
/// mouse move) never changes the context — only `FocusOsdControls` pushes `PlayerOsd`.
pub static PLAYER_COMMON: &[Binding] = &[
    b(C::key(Key::Space), A::PlayPause, "Play / pause"),
    b(C::ch('m'), A::Mute, "Mute"),
    b(C::ch('+'), A::VolumeUp, "Volume up / down"),
    b(C::ch('='), A::VolumeUp, "Volume up / down"),
    b(C::ch('-'), A::VolumeDown, "Volume up / down"),
    b(C::key(Key::Enter), A::FocusOsdControls, "On-screen controls"),
    b(C::ch('i'), A::FocusOsdControls, "On-screen controls"),
    b(C::key(Key::Tab), A::FocusOsdControls, "On-screen controls"),
    b(C::ch('I'), A::StreamInfo, "Stream info (codec, resolution, bitrate)"),
    b(C::ch('a'), A::CycleAudio, "Next audio track"),
    b(C::ch('A'), A::AudioMenu, "Audio tracks…"),
    b(C::ch('s'), A::CycleSubtitle, "Next subtitle track"),
    b(C::ch('S'), A::SubtitleMenu, "Subtitles…"),
    b(C::ch('z'), A::SubDelayMinus, "Subtitle delay −/+ 100 ms"),
    b(C::ch('x'), A::SubDelayPlus, "Subtitle delay −/+ 100 ms"),
    b(C::ch('Z'), A::SubDelayReset, "Reset subtitle delay"),
    b(C::ch('{'), A::SubSizeMinus, "Subtitle size −/+"),
    b(C::ch('}'), A::SubSizePlus, "Subtitle size −/+"),
    b(C::ch('v'), A::CycleAspect, "Aspect ratio (auto, 16:9, 4:3, 21:9, fill, stretch)"),
    b(C::ch('V'), A::CycleZoom, "Zoom"),
    b(C::ch('f'), A::ToggleWindowFullscreen, "Toggle window fullscreen"),
    b(C::ch('p'), A::Minimize, "Keep playing in the mini player"),
    b(C::ch('q'), A::StopAndClose, "Stop and close"),
    b(C::ch('r'), A::Retry, "Retry (after an error)"),
    b(C::ctrl(Key::F), A::ToggleFavourite, "Add to / remove from Favourites"),
    b(C::key(Key::Escape), A::Back, "Back (hide banner, then leave; keeps playing in the mini player)"),
    b(C::ch('/'), A::OpenSearch, "Search everything"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
];

pub static PLAYER_LIVE: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::ZapPrev, "Previous / next channel (in this list)"),
    b(C::ch('k'), A::ZapPrev, "Previous / next channel (in this list)"),
    b(C::key(Key::ArrowDown), A::ZapNext, "Previous / next channel (in this list)"),
    b(C::ch('j'), A::ZapNext, "Previous / next channel (in this list)"),
    b(C::key(Key::PageUp), A::ZapPrev, "Previous / next channel (in this list)"),
    b(C::key(Key::PageDown), A::ZapNext, "Previous / next channel (in this list)"),
    b(C::key(Key::Backspace), A::LastChannel, "Last channel"),
    b(C::key(Key::ArrowLeft), A::LastChannel, "Last channel"),
    b(C::key(Key::ArrowRight), A::ChannelHistory, "Channel history"),
    b(C::ch('h'), A::ChannelHistory, "Channel history"),
    b(C::ch('c'), A::ChannelList, "Channel list"),
    b(C::ch('g'), A::MiniGuide, "Mini guide (now / next)"),
    b(C::ch('0'), A::Digit(0), "Type a channel number"),
    b(C::ch('1'), A::Digit(1), "Type a channel number"),
    b(C::ch('2'), A::Digit(2), "Type a channel number"),
    b(C::ch('3'), A::Digit(3), "Type a channel number"),
    b(C::ch('4'), A::Digit(4), "Type a channel number"),
    b(C::ch('5'), A::Digit(5), "Type a channel number"),
    b(C::ch('6'), A::Digit(6), "Type a channel number"),
    b(C::ch('7'), A::Digit(7), "Type a channel number"),
    b(C::ch('8'), A::Digit(8), "Type a channel number"),
    b(C::ch('9'), A::Digit(9), "Type a channel number"),
];

pub static PLAYER_VOD: &[Binding] = &[
    b(C::key(Key::ArrowLeft), A::SeekBack, "Skip back / forward (small step)"),
    b(C::ch('h'), A::SeekBack, "Skip back / forward (small step)"),
    b(C::key(Key::ArrowRight), A::SeekForward, "Skip back / forward (small step)"),
    b(C::ch('l'), A::SeekForward, "Skip back / forward (small step)"),
    b(C::shift(Key::ArrowLeft), A::SeekBackBig, "Skip back / forward (big step)"),
    b(C::shift(Key::ArrowRight), A::SeekForwardBig, "Skip back / forward (big step)"),
    b(C::key(Key::ArrowUp), A::SeekForwardBig, "Skip back / forward (big step)"),
    b(C::key(Key::ArrowDown), A::SeekBackBig, "Skip back / forward (big step)"),
    b(C::ch('L'), A::JumpToLive, "Jump to live (from catch-up)"),
    b(C::key(Key::Home), A::Home, "Go to start"),
];

/// Controls focus mode only (never pushed by the passive banner).
pub static PLAYER_OSD: &[Binding] = &[
    b(C::key(Key::ArrowLeft), A::Left, "Move between controls / scrub"),
    b(C::key(Key::ArrowRight), A::Right, "Move between controls / scrub"),
    b(C::key(Key::ArrowUp), A::Up, "Move between control rows"),
    b(C::key(Key::ArrowDown), A::Down, "Move between control rows"),
    b(C::key(Key::Enter), A::Activate, "Activate control"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous control"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous control"),
    b(C::key(Key::Escape), A::LeaveOsdControls, "Leave the controls"),
];

pub static NUMERIC_ENTRY: &[Binding] = &[
    b(C::ch('0'), A::Digit(0), "Type the channel number"),
    b(C::ch('1'), A::Digit(1), "Type the channel number"),
    b(C::ch('2'), A::Digit(2), "Type the channel number"),
    b(C::ch('3'), A::Digit(3), "Type the channel number"),
    b(C::ch('4'), A::Digit(4), "Type the channel number"),
    b(C::ch('5'), A::Digit(5), "Type the channel number"),
    b(C::ch('6'), A::Digit(6), "Type the channel number"),
    b(C::ch('7'), A::Digit(7), "Type the channel number"),
    b(C::ch('8'), A::Digit(8), "Type the channel number"),
    b(C::ch('9'), A::Digit(9), "Type the channel number"),
    b(C::key(Key::Enter), A::Activate, "Go now (otherwise after 1.5 s)"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete digit"),
    b(C::key(Key::Escape), A::Back, "Cancel"),
];

/// Stacked above `List`/`ListFiltering` on the EPG mapping screen.
pub static EPG_MAPPING: &[Binding] = &[
    b(C::key(Key::Enter), A::Activate, "Assign the highlighted EPG channel"),
    b(C::key(Key::Delete), A::ClearEpg, "Clear the manual assignment"),
];

pub static SEARCH: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous section (All, Live, Movies, Series, Guide)"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous section (All, Live, Movies, Series, Guide)"),
    b(C::key(Key::Enter), A::Activate, "Play fullscreen / open"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play fullscreen / open details"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear"),
    b(C::key(Key::Escape), A::Back, "Clear, then close"),
];

pub static MODAL: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::ArrowLeft), A::Left, "Move"),
    b(C::key(Key::ArrowRight), A::Right, "Move"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous field"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous field"),
    b(C::key(Key::Enter), A::Activate, "Confirm"),
    b(C::key(Key::Space), A::Activate, "Confirm"),
    b(C::key(Key::Escape), A::Back, "Cancel"),
];

pub static TEXT_INPUT: &[Binding] = &[
    b(C::key(Key::Tab), A::FocusNext, "Next / previous field"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous field"),
    b(C::key(Key::Enter), A::Activate, "Confirm"),
    b(C::key(Key::Escape), A::Back, "Leave the field"),
];

pub static PANEL: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::ch('k'), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::ch('j'), A::Down, "Move"),
    b(C::key(Key::ArrowLeft), A::Left, "Decrease / previous option"),
    b(C::ch('h'), A::Left, "Decrease / previous option"),
    b(C::key(Key::ArrowRight), A::Right, "Increase / next option"),
    b(C::ch('l'), A::Right, "Increase / next option"),
    b(C::alt(Key::ArrowUp), A::MoveUp, "Move up / down (reorder: EPG priority, playlists, lists)"),
    b(C::ch('K'), A::MoveUp, "Move up / down (reorder: EPG priority, playlists, lists)"),
    b(C::alt(Key::ArrowDown), A::MoveDown, "Move up / down (reorder: EPG priority, playlists, lists)"),
    b(C::ch('J'), A::MoveDown, "Move up / down (reorder: EPG priority, playlists, lists)"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous section"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous section"),
    b(C::key(Key::Enter), A::Activate, "Toggle / open"),
    b(C::key(Key::Space), A::Activate, "Toggle / open"),
    b(C::ch('x'), A::ContextMenu, "Delete / more actions"),
    b(C::key(Key::Delete), A::ContextMenu, "Delete / more actions"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
];

/// The bindings of one context, in priority order (context-specific first).
pub fn tables(ctx: KeyContext) -> Vec<&'static [Binding]> {
    match ctx {
        KeyContext::Global => vec![GLOBAL],
        KeyContext::List => vec![LIST, ITEM_ACTIONS],
        KeyContext::ListFiltering => vec![LIST_FILTERING, ITEM_ACTIONS],
        KeyContext::PosterGrid => vec![POSTER_GRID, ITEM_ACTIONS],
        KeyContext::PosterGridFiltering => vec![POSTER_GRID_FILTERING, ITEM_ACTIONS],
        KeyContext::Detail => vec![DETAIL, ITEM_ACTIONS],
        KeyContext::Guide => vec![GUIDE],
        KeyContext::PlayerLive => vec![PLAYER_LIVE, PLAYER_COMMON],
        KeyContext::PlayerVod => vec![PLAYER_VOD, PLAYER_COMMON],
        KeyContext::PlayerOsd => vec![PLAYER_OSD],
        KeyContext::NumericEntry => vec![NUMERIC_ENTRY],
        KeyContext::EpgMapping => vec![EPG_MAPPING],
        KeyContext::Search => vec![SEARCH, ITEM_ACTIONS],
        KeyContext::Modal => vec![MODAL],
        KeyContext::TextInput => vec![TEXT_INPUT],
        KeyContext::Panel => vec![PANEL],
    }
}

/// Contexts whose unbound printable characters become `FilterInput`.
pub fn is_filtering(ctx: KeyContext) -> bool {
    matches!(
        ctx,
        KeyContext::List | KeyContext::ListFiltering | KeyContext::PosterGrid | KeyContext::PosterGridFiltering | KeyContext::Search
    )
}

fn find(tables: &[&'static [Binding]], ev: &egui::Event) -> Option<Action> {
    tables.iter().find_map(|t| t.iter().find(|bd| bd.chord.matches(ev)).map(|bd| bd.action))
}

/// Ctrl/Alt chord or F-key: the only events a focused text field lets through (to `Global`).
fn is_chord_or_fkey(ev: &egui::Event) -> bool {
    match ev {
        egui::Event::Key { key, modifiers, .. } => {
            modifiers.ctrl || modifiers.alt || modifiers.command || key.name().starts_with('F') && key.name().len() > 1 && key.name()[1..].chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// Resolve one input event against a context stack (innermost first; `Global` is appended
/// automatically). Returns `None` for unbound events (they flow to egui widgets).
pub fn resolve(stack: &[KeyContext], ev: &egui::Event) -> Option<Action> {
    let action = resolve_inner(stack, ev)?;
    // Auto-repeat never steps out of a pane/layer (holding Backspace to clear a filter).
    if matches!(action, Action::Parent | Action::Back) && matches!(ev, egui::Event::Key { repeat: true, .. }) {
        return None;
    }
    Some(action)
}

fn resolve_inner(stack: &[KeyContext], ev: &egui::Event) -> Option<Action> {
    for ctx in stack.iter().copied() {
        if let Some(a) = find(&tables(ctx), ev) {
            return Some(a);
        }
        match ctx {
            // A focused text field owns the keyboard: nothing below it is consulted; only
            // Ctrl/Alt chords and F-keys may still reach Global.
            KeyContext::TextInput => {
                return if is_chord_or_fkey(ev) { find(&tables(KeyContext::Global), ev) } else { None };
            }
            KeyContext::Modal => {
                if let egui::Event::Text(_) = ev {
                    return None;
                }
            }
            c if is_filtering(c) => {
                if let egui::Event::Text(t) = ev {
                    let mut it = t.chars();
                    if let (Some(c), None) = (it.next(), it.next())
                        && !c.is_control() {
                            return Some(Action::FilterInput(c));
                        }
                }
            }
            // PlayerOsd / NumericEntry / EpgMapping / others fall through to the next context.
            _ => {}
        }
    }
    find(&tables(KeyContext::Global), ev)
}

/// Rows for the help overlay of one context: (merged key labels, description).
pub fn help_rows(ctx: KeyContext) -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for table in tables(ctx) {
        for bd in table.iter() {
            let label = bd.chord.label();
            if let Some(r) = rows.iter_mut().find(|r| r.1 == bd.help) {
                if !r.0.split(" / ").any(|l| l == label) {
                    r.0.push_str(" / ");
                    r.0.push_str(&label);
                }
            } else {
                rows.push((label, bd.help));
            }
        }
    }
    if is_filtering(ctx) && ctx != KeyContext::Search {
        rows.push(("a–z, 0–9 …".into(), "Type to filter"));
    }
    rows
}

/// Curated short verbs per context for the status-bar hint line; the KEYS are looked up in the
/// tables, so the line can never disagree with the keymap.
const HINTS: &[(KeyContext, &[(Action, &str)])] = &[
    (
        KeyContext::List,
        &[(A::Activate, "play"), (A::ActivateAlt, "fullscreen"), (A::ToggleFavourite, "fav"), (A::ContextMenu, "more"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::ListFiltering, &[(A::Activate, "play"), (A::FilterClear, "clear"), (A::ContextMenu, "more")]),
    (
        KeyContext::PosterGrid,
        &[(A::Activate, "details"), (A::ActivateAlt, "play"), (A::SortMenu, "sort"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::PosterGridFiltering, &[(A::Activate, "details"), (A::FilterClear, "clear")]),
    (KeyContext::Detail, &[(A::Activate, "play"), (A::TabNext, "season"), (A::MarkWatched, "watched"), (A::Back, "back")]),
    (
        KeyContext::Guide,
        &[(A::Activate, "details"), (A::GuideWatch, "watch"), (A::GuideRemind, "remind"), (A::GuideNow, "now"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::Panel, &[(A::Activate, "toggle"), (A::MoveUp, "reorder"), (A::ContextMenu, "more"), (A::ToggleHelp, "help")]),
    (KeyContext::EpgMapping, &[(A::Activate, "assign"), (A::ClearEpg, "clear")]),
];

/// Status-bar hint line for the innermost context that has hints, e.g.
/// "Enter play · Shift+Enter fullscreen · Ctrl+F fav · Shift+F10 more · ? help".
pub fn hint_line(stack: &[KeyContext]) -> String {
    for ctx in stack {
        if let Some((_, items)) = HINTS.iter().find(|(c, _)| c == ctx) {
            let t = tables(*ctx);
            let parts: Vec<String> = items
                .iter()
                .filter_map(|(a, verb)| {
                    t.iter().chain(std::iter::once(&GLOBAL)).find_map(|tb| tb.iter().find(|bd| bd.action == *a)).map(|bd| format!("{} {verb}", bd.chord.label()))
                })
                .collect();
            return parts.join(" · ");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_ev(k: Key, ctrl: bool, alt: bool, shift: bool, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers: egui::Modifiers { ctrl, alt, shift, command: ctrl, ..Default::default() },
        }
    }
    fn key(k: Key, ctrl: bool) -> egui::Event { key_ev(k, ctrl, false, false, false) }
    use KeyContext as K;

    #[test]
    fn letters_filter_in_lists_but_zap_in_player() {
        let j = egui::Event::Text("j".into());
        assert_eq!(resolve(&[K::List], &j), Some(Action::FilterInput('j')));
        assert_eq!(resolve(&[K::PlayerLive], &j), Some(Action::ZapNext));
        assert_eq!(resolve(&[K::Guide], &j), Some(Action::Down));
    }

    #[test]
    fn slash_searches_only_when_not_filtering() {
        let s = egui::Event::Text("/".into());
        assert_eq!(resolve(&[K::List], &s), Some(Action::OpenSearch));
        assert_eq!(resolve(&[K::ListFiltering], &s), Some(Action::FilterInput('/')));
        assert_eq!(resolve(&[K::PosterGrid], &s), Some(Action::OpenSearch));
    }

    #[test]
    fn global_ctrl_k_everywhere_and_no_printable_globals() {
        assert_eq!(resolve(&[K::ListFiltering], &key(Key::K, true)), Some(Action::OpenSearch));
        assert!(GLOBAL.iter().all(|b| !matches!(b.chord.trigger, Trigger::Char(_))));
    }

    #[test]
    fn help_rows_merge_aliases() {
        let rows = help_rows(K::PlayerLive);
        assert!(rows.iter().any(|(k, h)| h.starts_with("Previous / next channel") && k.contains("↑") && k.contains("k")));
    }

    // UX review B1: the info banner never changes the stack, so arrows keep zapping.
    #[test]
    fn arrows_zap_while_banner_visible_and_controls_need_explicit_focus() {
        assert_eq!(resolve(&[K::PlayerLive], &key(Key::ArrowDown, false)), Some(Action::ZapNext));
        assert_eq!(resolve(&[K::PlayerLive], &key(Key::Enter, false)), Some(Action::FocusOsdControls));
        assert_eq!(resolve(&[K::PlayerOsd, K::PlayerLive], &key(Key::ArrowDown, false)), Some(Action::Down));
        assert_eq!(resolve(&[K::PlayerOsd, K::PlayerLive], &key(Key::Escape, false)), Some(Action::LeaveOsdControls));
        // unbound keys fall through to the player context
        assert_eq!(resolve(&[K::PlayerOsd, K::PlayerLive], &key(Key::Space, false)), Some(Action::PlayPause));
    }

    // UX review B2: text fields own named keys.
    #[test]
    fn text_input_swallows_named_keys() {
        assert_eq!(resolve(&[K::TextInput, K::Modal], &key(Key::Space, false)), None);
        assert_eq!(resolve(&[K::TextInput, K::Panel], &key(Key::Delete, false)), None);
        assert_eq!(resolve(&[K::TextInput, K::Panel], &key(Key::ArrowLeft, false)), None);
        assert_eq!(resolve(&[K::TextInput, K::Modal], &egui::Event::Text(" ".into())), None);
        // chords still reach Global, but never the contexts below the field
        assert_eq!(resolve(&[K::TextInput, K::List], &key(Key::Q, true)), Some(Action::Quit));
        assert_eq!(resolve(&[K::TextInput, K::List], &key(Key::F, true)), None);
        assert_eq!(resolve(&[K::TextInput, K::Modal], &key(Key::Enter, false)), Some(Action::Activate));
    }

    #[test]
    fn repeat_never_steps_out() {
        assert_eq!(resolve(&[K::List], &key_ev(Key::Backspace, false, false, false, true)), None);
        assert_eq!(resolve(&[K::List], &key_ev(Key::Backspace, false, false, false, false)), Some(Action::Parent));
        assert_eq!(resolve(&[K::ListFiltering], &key_ev(Key::Backspace, false, false, false, true)), Some(Action::FilterBackspace));
    }

    #[test]
    fn grids_filter_and_detail_has_seasons() {
        assert_eq!(resolve(&[K::PosterGrid], &egui::Event::Text("a".into())), Some(Action::FilterInput('a')));
        assert_eq!(resolve(&[K::PosterGridFiltering], &key(Key::Backspace, false)), Some(Action::FilterBackspace));
        assert_eq!(resolve(&[K::Detail], &egui::Event::Text("]".into())), Some(Action::TabNext));
        assert_eq!(resolve(&[K::Detail], &key(Key::W, true)), Some(Action::MarkWatched));
        assert!(help_rows(K::PosterGrid).iter().any(|(_, h)| *h == "Type to filter"));
    }

    #[test]
    fn panel_reorders_and_mapping_clears() {
        assert_eq!(resolve(&[K::Panel], &key_ev(Key::ArrowUp, false, true, false, false)), Some(Action::MoveUp));
        assert_eq!(resolve(&[K::Panel], &egui::Event::Text("J".into())), Some(Action::MoveDown));
        assert_eq!(resolve(&[K::EpgMapping, K::List], &key(Key::Delete, false)), Some(Action::ClearEpg));
        assert_eq!(resolve(&[K::List], &key(Key::Delete, false)), Some(Action::Remove));
        assert_eq!(resolve(&[K::NumericEntry, K::PlayerLive], &key(Key::Backspace, false)), Some(Action::FilterBackspace));
        assert_eq!(resolve(&[K::Guide], &egui::Event::Text("7".into())), Some(Action::Digit(7)));
    }

    #[test]
    fn alt_left_is_back_and_hints_come_from_tables() {
        assert_eq!(resolve(&[K::Guide], &key_ev(Key::ArrowLeft, false, true, false, false)), Some(Action::Back));
        let h = hint_line(&[K::List]);
        assert!(h.starts_with("Enter play"), "{h}");
        assert!(h.contains("Ctrl+F fav"), "{h}");
    }

    #[test]
    fn every_context_has_a_title_and_rows() {
        for c in KeyContext::ALL {
            assert!(!c.title().is_empty());
            assert!(!help_rows(c).is_empty(), "{c:?}");
        }
    }
}
