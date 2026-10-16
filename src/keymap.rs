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
//! * Filterable contexts (`List`, `ListFiltering`, `PosterGrid`, `PosterGridFiltering`, `Checklist`,
//!   `Search`, `Help`): an unbound printable character (no Ctrl/Alt) becomes
//!   [`Action::FilterInput`] — the launcher idiom: in filterable lists letters type, so there is
//!   NO j/k there; vim keys live in the guide grid, the player and value panels.
//! * `TextInput` owns the keyboard: after its own table, only Ctrl/Alt chords and F-keys may
//!   reach `Global`; every other event (Space, ←/→, Delete, Backspace…) returns `None` so the
//!   focused egui text widget edits text. Contexts below `TextInput` are never consulted.
//! * `Modal` swallows printable text that it does not bind.
//! * Spatial panes (DESIGN §15.1): ← at the left edge of the focused pane steps to the pane on
//!   its left (lists: `Parent`, or `Left` while filtering; Settings: `Left`; grids: `Left` on
//!   the first column); → / Enter go right. Where ←/→ mean something inside the pane (a grid,
//!   the guide, the player, a text field) they keep it; Settings values change with → and
//!   Shift+←/→.
//! * Key auto-repeat never triggers `Parent`/`Back`, a filtered list's ← or `LastChannel`
//!   (holding Backspace to clear a filter must not continue into "leave the pane"; holding the
//!   ← that closed a player side panel must not zap).
//! * `Global` contains only Ctrl/Alt/F-key chords so it never steals typing.
//! * No screen reads raw keys: every key a screen reacts to is in a table (so help is complete).
//!
//! Printable symbols (`?`, `/`, `[`, `F`) are matched against `egui::Event::Text` (layout
//! independent, shift already applied); named keys and Ctrl/Alt chords against `Event::Key`.
//!
//! Caller contract (`App::raw_input_hook`): egui-winit suppresses the `Event::Text` of Ctrl/Cmd
//! chords but NOT of Alt chords, so Alt+1 arrives as `Key{Num1, alt}` (→ `GoHome`) followed by
//! `Text("1")` (→ `Digit(1)` in the guide, `FilterInput('1')` in a list). When a Key event with
//! `alt` resolves to an action, the hook must also drop the `Text` event that immediately
//! follows it. `resolve` is per-event and cannot see that pairing.

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
    /// Controls focus mode (entered with Enter / `i` / Tab): arrows move
    /// between OSD controls; unbound keys fall through to PlayerLive/Vod.
    PlayerOsd,
    /// The player cog pop-up and its sub-pages (DESIGN §13.8).
    PlayerSettings,
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
    /// Value panels, including EPG mapping adjustments.
    Panel,
    /// Settings panes: arrows change values; letters start a filter.
    Settings,
    /// The video-only player of a popped-out window, stacked right above `PlayerLive` /
    /// `PlayerVod` (Esc pops back in instead of leaving the player).
    PopOut,
    /// A tick list with type-to-filter ("Choose what to show"), innermost above `Modal`: it
    /// binds its own navigation and filter keys, so → never activates and Backspace never
    /// leaves (as `List` would).
    Checklist,
    /// The keyboard help overlay: typing filters the shortcuts (Omarchy's keybindings menu).
    Help,
    /// Multi-view (DESIGN §35), stacked right above `PlayerLive` / `PlayerVod` of the FOCUSED
    /// picture (`[MultiView, PlayerLive]`, `[PlayerOsd, MultiView, PlayerLive]`, `[List,
    /// MultiView, PlayerLive]`, `[PopOut, MultiView, PlayerLive]`); unbound keys fall through.
    MultiView,
    /// Watchlist entries and list controls (DESIGN §42 §2.4).
    Watchlists,
}

impl KeyContext {
    /// Every context, in help-overlay / documentation order.
    pub const ALL: [KeyContext; 23] = [
        KeyContext::Global,
        KeyContext::Watchlists,
        KeyContext::List,
        KeyContext::ListFiltering,
        KeyContext::Checklist,
        KeyContext::PosterGrid,
        KeyContext::PosterGridFiltering,
        KeyContext::Detail,
        KeyContext::Guide,
        KeyContext::PlayerLive,
        KeyContext::PlayerVod,
        KeyContext::PlayerOsd,
        KeyContext::PlayerSettings,
        KeyContext::MultiView,
        KeyContext::PopOut,
        KeyContext::NumericEntry,
        KeyContext::EpgMapping,
        KeyContext::Search,
        KeyContext::Modal,
        KeyContext::TextInput,
        KeyContext::Panel,
        KeyContext::Settings,
        KeyContext::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            KeyContext::Global => "Everywhere",
            KeyContext::Watchlists => "Watchlists",
            KeyContext::List => "Lists",
            KeyContext::ListFiltering => "Lists while filtering",
            KeyContext::PosterGrid => "Movies & series grid",
            KeyContext::PosterGridFiltering => "Movies & series grid while filtering",
            KeyContext::Detail => "Movie & series pages",
            KeyContext::Guide => "TV guide",
            KeyContext::PlayerLive => "Player — live TV",
            KeyContext::PlayerVod => "Player — movies, episodes, catch-up",
            KeyContext::PlayerOsd => "Player — on-screen controls",
            KeyContext::PlayerSettings => "Player — settings menu",
            KeyContext::PopOut => "Pop-out player",
            KeyContext::NumericEntry => "Channel number entry",
            KeyContext::EpgMapping => "Guide fixes",
            KeyContext::Search => "Search",
            KeyContext::Modal => "Dialogs & menus",
            KeyContext::TextInput => "Text fields",
            KeyContext::Panel => "Panel",
            KeyContext::Settings => "Settings",
            KeyContext::Checklist => "Tick lists",
            KeyContext::Help => "Keyboard help",
            KeyContext::MultiView => "Player — multi-view",
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
    /// ← / Backspace in a list (Backspace only on an empty filter), ← on a grid's first
    /// column, ← in Settings: step to the pane on the left (DESIGN §15.1 "Spatial panes").
    Parent,
    /// Settings value rows (Shift+←): previous option, smaller number, switch off.
    Decrease,
    /// Settings value rows (→ / Shift+→): next option, larger number, switch on.
    Increase,
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
    /// Alt+P on Movies, Series, Live TV and the guide: pick the playlist shown (DESIGN §29).
    PlaylistPicker,
    /// Ctrl+PgDn / Ctrl+PgUp there: the next / previous playlist (All included).
    NextPlaylist,
    PrevPlaylist,
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
    /// Space / Insert: mark the focused group for a bulk hide; tick a row of a tick list.
    ToggleMark,
    /// Ctrl+A in a tick list: tick all shown rows, or none when all are ticked.
    MarkAll,
    /// Ctrl+E in the category picker: just the user's country + English.
    MarkDefaults,
    /// Ctrl+Shift+H while a group pane is filtered: hide the provider groups it shows whose
    /// names have a word starting with each word typed (not every fuzzy match).
    HideShown,
    /// Delete: remove from a favourites list / cancel a job / forget an orphan / cancel a
    /// reminder (whatever "remove" means for the focused row).
    Remove,
    /// Space on a stack card in Movies / Series: fan the copies out in the grid, or fold them
    /// back (DESIGN §29.14).
    FanOut,
    /// Ctrl+O on a title with several copies (grid card, detail page, the player): "Play
    /// from…" a chosen copy (DESIGN §29.14, §29.15).
    PlayFrom,
    /// A toast's Undo button (never bound to a key): the app puts back what a combined action
    /// on a title's copies wrote (`ui::copies`; the number names the action).
    Undo(u16),
    /// A toast's Cancel button (never bound to a key): call off an automatic switch that is
    /// about to happen (DESIGN §30.8).
    CancelSwitch(crate::model::ReminderId),
    /// A toast's Undo for a Following rule just added (never bound to a key): delete it again.
    UndoFollow(crate::model::FollowRuleId),
    /// The Open button of Following's summary toast (never bound to a key): Settings ▸
    /// Following.
    OpenFollowing,
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
    PlayerSettings,
    StreamInfo,
    AudioMenu,
    SubtitleMenu,
    CycleAudio,
    CycleSubtitle,
    LipSyncEarlier,
    LipSyncLater,
    SubDelayMinus,
    SubDelayPlus,
    SubDelayReset,
    SubSizeMinus,
    SubSizePlus,
    CycleAspect,
    CycleZoom,
    Minimize,
    /// `o` / Ctrl+Shift+O: pop the player out into a small pinned window on every workspace
    /// (Hyprland), or back in (DESIGN §21.6).
    TogglePopOut,
    StopAndClose,
    Retry,
    Digit(u8),
    // ---- Up next (DESIGN §32)
    /// Ctrl+N: add the focused item (or programme) to the end of Up next.
    QueueAdd,
    /// Ctrl+Shift+N: play it next (after the current item).
    QueuePlayNext,
    /// Alt+6: the Up next screen.
    GoQueue,
    GoWatchlists,
    WatchlistOne,
    WatchlistSkip,
    WatchlistOrder,
    WatchlistNew,
    /// `u` in the player: the Up next panel.
    QueuePanel,
    /// `n` / `N` in the player: next / previous Up next item (multi-view: "Up next waits").
    QueueNext,
    QueuePrev,
    // ---- Following (DESIGN §33)
    /// `R` in the guide: Follow… (every time it's on).
    FollowProgramme,
    // ---- spoiler guard (DESIGN §34)
    /// Ctrl+B: show / hide the guarded text of the focused item.
    RevealSpoilers,
    /// Ctrl+Shift+B: the spoiler-guard menu (pause, blackout, guard a topic, settings).
    SpoilerMenu,
    // ---- Guide fixes (DESIGN §31)
    /// Shift+Enter: accept the proposed fix (every marked channel when some are marked).
    AcceptFix,
    /// Ctrl+Shift+→ / ←: time shift +1 h / −1 h.
    ShiftLater,
    ShiftEarlier,
    /// Ctrl+Z (`None` = the last batch) or a toast's Undo (`Some(batch)`).
    UndoGuideFix(Option<crate::model::GuideFixLogId>),
    // ---- multi-view (DESIGN §35)
    /// `w`: focus the next picture.
    NextPicture,
    /// Ctrl+←/→/↑/↓: focus the picture in that direction.
    FocusPictureLeft,
    FocusPictureRight,
    FocusPictureUp,
    FocusPictureDown,
    /// `M`: the sound comes from the focused picture.
    SoundHere,
    /// `W`: swap the pictures' places.
    SwapPictures,
    /// `t`: side by side → big + small → picture in picture.
    CycleLayout,
    /// `q` in multi-view: close the focused picture (the other carries on).
    ClosePicture,
    /// Ctrl+M: watch in the other picture (opens multi-view from a list, the guide, search;
    /// in the player, choose the channel for the other picture).
    WatchAlongside,
    // ---- Tuner's updates (DESIGN §41): toasts and the status-bar chip, never bound to a key
    /// "Install" on the toast of an update that waits: download it if need be, then install.
    InstallUpdate,
    /// "Restart now" on the update's toast and chip: restart into the installed version.
    RestartToUpdate,
    // ---- reminders, sleep timer, news flash (B6, big-features additions)
    /// Ctrl+Shift+T: the sleep timer menu (in N min, end of programme, after N items, end of
    /// Up next, off).
    SleepTimer,
    /// Delete in the pop-out during a news flash: end it now and put back what was playing
    /// (or, while a switch's countdown shows, call that switch off).
    EndNewsFlash,
    /// A menu entry, never bound to a key: "Switch to this when it starts…" / "Edit
    /// reminder…" on a programme — the reminder sheet (notify or switch, lead, start offset,
    /// where it opens; `reminders::ui`).
    ReminderOptions,
    // ---- casting (DESIGN §40; the key tables and `feature_on` are CAST-8's)
    /// `C` in the player: "Cast to a TV…" (the picker; while casting it opens with "This
    /// computer" focused, so stopping is `C` then Enter).
    CastMenu,
    /// "Stop casting": the player menu, the casting card, the status chip and the picker. No
    /// default key.
    StopCasting,
    /// "Play here" on the alert after the TV stopped the cast (§3.4 "Play here · Cast again"):
    /// the last item plays here, from where the TV was. No default key.
    PlayHere,
    /// "Cast to…" on the focused item (DESIGN §40 §3.1): a channel row, a card, a programme, an
    /// episode, a title's page. `C` where typing doesn't filter (the guide, detail pages),
    /// Alt+C everywhere an item is focused (lists and grids filter on letters). One paired TV:
    /// straight there; else the picker for that item (`ui::casting`).
    CastItem,
    /// Alt+T and the status bar's "Play on" chip (DESIGN §40 §3.9): the chooser of where plays
    /// go — this computer or a TV.
    PlayOnMenu,
    /// The PIN prompt's answer for a locked item that "Cast to…" casts (never bound): the app
    /// casts it once unlocked (`ui::casting::unlocked`).
    CastUnlocked,
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
    pub const fn key(k: Key) -> Self {
        Self { trigger: Trigger::Key(k), ctrl: false, alt: false, shift: false }
    }
    pub const fn ch(c: char) -> Self {
        Self { trigger: Trigger::Char(c), ctrl: false, alt: false, shift: false }
    }
    pub const fn ctrl(k: Key) -> Self {
        Self { trigger: Trigger::Key(k), ctrl: true, alt: false, shift: false }
    }
    pub const fn alt(k: Key) -> Self {
        Self { trigger: Trigger::Key(k), ctrl: false, alt: true, shift: false }
    }
    pub const fn shift(k: Key) -> Self {
        Self { trigger: Trigger::Key(k), ctrl: false, alt: false, shift: true }
    }
    pub const fn ctrl_shift(k: Key) -> Self {
        Self { trigger: Trigger::Key(k), ctrl: true, alt: false, shift: true }
    }

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

const fn b(chord: KeyChord, action: Action, help: &'static str) -> Binding {
    Binding { chord, action, help }
}

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
    b(C::alt(Key::Num6), A::GoQueue, "Up next"),
    b(C::alt(Key::Num7), A::GoWatchlists, "Watchlists"),
    b(C::ctrl(Key::Comma), A::GoSettings, "Settings"),
    b(C::alt(Key::Num0), A::GoSettings, "Settings"),
    b(C::alt(Key::ArrowLeft), A::Back, "Back"),
    b(C::key(Key::F11), A::ToggleWindowFullscreen, "Toggle window fullscreen"),
    b(C::ctrl(Key::P), A::OpenPlayer, "Open the player (from the mini player)"),
    // not Ctrl+Space: that is fcitx5's input-method toggle on Omarchy
    b(C::ctrl_shift(Key::P), A::GlobalPlayPause, "Play / pause"),
    b(C::ctrl_shift(Key::S), A::StopPlayback, "Stop playback"),
    b(C::ctrl_shift(Key::O), A::TogglePopOut, "Pop the player out (small, on every workspace) / back in"),
    b(C::ctrl(Key::R), A::RefreshSource, "Refresh playlists & guide"),
    b(C::key(Key::F5), A::RefreshSource, "Refresh playlists & guide"),
    b(C::ctrl(Key::J), A::ShowJobs, "Show background tasks"),
    b(C::key(Key::F1), A::ToggleHelp, "Keyboard help"),
    // Ctrl chords, so they work in filtered lists and text fields too
    b(C::ctrl(Key::B), A::RevealSpoilers, "Show / hide spoilers of the focused item"),
    b(C::ctrl_shift(Key::B), A::SpoilerMenu, "Spoiler guard… (pause, I'll watch it later, guard a topic)"),
    b(C::ctrl_shift(Key::T), A::SleepTimer, "Sleep timer…"),
    // not Ctrl+0 (egui's zoom reset) nor Alt+[ / Alt+] (AltGr chords on German / French keyboards)
    b(C::alt(Key::P), A::PlaylistPicker, "Choose the playlist shown (Movies, Series, Live TV, guide)"),
    // casting (DESIGN §40 §3.9): where plays go; T for TV (Alt+C casts the focused item)
    b(C::alt(Key::T), A::PlayOnMenu, "Play on… (this computer or a TV)"),
    b(C::ctrl(Key::PageDown), A::NextPlaylist, "Next / previous playlist (Movies, Series, Live TV, guide)"),
    b(C::ctrl(Key::PageUp), A::PrevPlaylist, "Next / previous playlist (Movies, Series, Live TV, guide)"),
];

/// Shared by List / ListFiltering / PosterGrid(Filtering) / Detail: item actions on Ctrl
/// chords and Delete (Search has its own subset, [`SEARCH_ITEM_ACTIONS`]).
pub static ITEM_ACTIONS: &[Binding] = &[
    b(C::ctrl(Key::F), A::ToggleFavourite, "Save to / remove from My list (channels: Favourites)"),
    b(C::ctrl_shift(Key::F), A::AddToList, "Watchlists… (channels: Favourites lists…)"),
    b(C::ctrl(Key::H), A::ToggleHidden, "Hide / unhide"),
    b(C::key(Key::F2), A::Rename, "Rename"),
    b(C::ctrl(Key::L), A::ToggleLock, "Lock with PIN / unlock"),
    b(C::alt(Key::ArrowUp), A::MoveUp, "Move up (reorder)"),
    b(C::alt(Key::ArrowDown), A::MoveDown, "Move down (reorder)"),
    b(C::ctrl(Key::S), A::SortMenu, "Sort & view options…"),
    b(C::ctrl(Key::G), A::OpenInGuide, "Show in TV guide"),
    b(C::ctrl(Key::I), A::ShowDetails, "Details / edit information"),
    b(C::ctrl(Key::E), A::AssignEpg, "Assign guide channel"),
    b(C::ctrl(Key::W), A::MarkWatched, "Mark watched / unwatched"),
    b(C::key(Key::Delete), A::Remove, "Remove (from list / cancel task / forget)"),
    // not Global: a text field must not pass them on (Ctrl chords reach Global from there)
    b(C::ctrl(Key::N), A::QueueAdd, "Add to Up next"),
    b(C::ctrl_shift(Key::N), A::QueuePlayNext, "Play next (Up next)"),
    b(C::ctrl(Key::M), A::WatchAlongside, "Watch alongside"),
    // casting (DESIGN §40 §3.1): a chord, so it works while a filter is typed (plain `C` would
    // start or extend the filter here)
    b(C::alt(Key::C), A::CastItem, "Cast to a TV…"),
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
    // the Text event, never Key::Space: the space is then never also typed into the filter
    // (and a leading space doesn't filter anyway)
    b(C::ch(' '), A::ToggleMark, "Mark (groups: Ctrl+H then hides all marked)"),
    b(C::key(Key::Insert), A::ToggleMark, "Mark (groups: Ctrl+H then hides all marked)"),
    // Live TV: the same channel in another playlist ("Also on", DESIGN §29.16)
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another playlist (Also on)…"),
    // any other printable key starts filtering (FilterInput)
];

pub static LIST_FILTERING: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Enter), A::Activate, "Play here / open (playing → fullscreen)"),
    b(C::shift(Key::Enter), A::ActivateAlt, "Play fullscreen"),
    // `Left`, not `Parent`: the list steps out whatever the filter (Backspace edits it), and
    // the filter is kept for when the pane is focused again
    b(C::key(Key::ArrowLeft), A::Left, "Back to the previous pane"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
    b(C::key(Key::Escape), A::FilterClear, "Clear filter"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous pane"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous pane"),
    // Space types here (filters have spaces); Insert still marks
    b(C::key(Key::Insert), A::ToggleMark, "Mark (groups: Ctrl+H then hides all marked)"),
    b(C::ctrl_shift(Key::H), A::HideShown, "Hide the groups with the words typed"),
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another playlist (Also on)…"),
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
    // the Text event, like the group lists' marks (a leading space never filters anyway);
    // only a stack card reacts (DESIGN §29.14)
    b(C::ch(' '), A::FanOut, "Show each copy / fold them (stack card)"),
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another copy…"),
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
    // Space types here (filters have spaces)
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another copy…"),
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
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another copy…"),
    // no filter on a title's page: `C` as in the player (Alt+C from `ITEM_ACTIONS` too)
    b(C::ch('C'), A::CastItem, "Cast to a TV…"),
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
    b(C::ch('R'), A::FollowProgramme, "Follow… (remind me every time it's on)"),
    b(C::ctrl(Key::W), A::MarkWatched, "Mark seen (lifts the spoiler guard)"),
    b(C::ctrl(Key::N), A::QueueAdd, "Add to Up next"),
    b(C::ctrl_shift(Key::N), A::QueuePlayNext, "Play next (Up next)"),
    b(C::ctrl(Key::M), A::WatchAlongside, "Watch alongside"),
    // casting (DESIGN §40 §3.1): the channel airing, or a past programme's catch-up
    b(C::ch('C'), A::CastItem, "Cast to a TV…"),
    b(C::alt(Key::C), A::CastItem, "Cast to a TV…"),
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
    b(C::ctrl_shift(Key::F), A::AddToList, "Favourites lists…"),
    b(C::ctrl(Key::E), A::AssignEpg, "Assign guide channel"),
    b(C::ctrl(Key::I), A::ShowDetails, "Edit channel information"),
    b(C::key(Key::F2), A::Rename, "Rename channel"),
    b(C::ctrl(Key::H), A::ToggleHidden, "Hide channel"),
    b(C::ctrl(Key::L), A::ToggleLock, "Lock channel with PIN / unlock"),
    b(C::shift(Key::F10), A::ContextMenu, "Channel menu"),
    b(C::ctrl(Key::Enter), A::ContextMenu, "Channel menu"),
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
    b(C::ch('O'), A::PlayerSettings, "Player settings…"),
    b(C::ch('I'), A::StreamInfo, "Stream info (codec, resolution, bitrate)"),
    b(C::ch('a'), A::CycleAudio, "Next audio track"),
    b(C::ch('A'), A::AudioMenu, "Audio tracks…"),
    b(C::ch('s'), A::CycleSubtitle, "Next subtitle track"),
    b(C::ch('S'), A::SubtitleMenu, "Subtitles…"),
    b(C::ch('['), A::LipSyncEarlier, "Delay: sound earlier / later (50 ms)"),
    b(C::ch(']'), A::LipSyncLater, "Delay: sound earlier / later (50 ms)"),
    b(C::ch('z'), A::SubDelayMinus, "Subtitle delay −/+ 100 ms"),
    b(C::ch('x'), A::SubDelayPlus, "Subtitle delay −/+ 100 ms"),
    b(C::ch('Z'), A::SubDelayReset, "Reset subtitle delay"),
    b(C::ch('{'), A::SubSizeMinus, "Subtitle size −/+"),
    b(C::ch('}'), A::SubSizePlus, "Subtitle size −/+"),
    b(C::ch('v'), A::CycleAspect, "Aspect ratio (auto, 16:9, 4:3, 21:9, fill, stretch)"),
    b(C::ch('V'), A::CycleZoom, "Zoom"),
    b(C::ch('f'), A::ToggleWindowFullscreen, "Toggle window fullscreen"),
    b(C::ch('p'), A::Minimize, "Keep playing in the mini player"),
    b(C::ch('o'), A::TogglePopOut, "Pop out: small window on every workspace (Hyprland)"),
    b(C::ch('q'), A::StopAndClose, "Stop and close"),
    b(C::ch('r'), A::Retry, "Retry (after an error)"),
    // casting (DESIGN §40 §3.7): while casting it opens the picker on "This computer", so
    // stopping is `C` then Enter; no global chord (Ctrl+Shift+C is the terminal's copy)
    b(C::ch('C'), A::CastMenu, "Cast to a TV…"),
    b(C::ch('u'), A::QueuePanel, "Up next"),
    b(C::ch('n'), A::QueueNext, "Next / previous in Up next"),
    b(C::ch('N'), A::QueuePrev, "Next / previous in Up next"),
    b(C::ctrl(Key::M), A::WatchAlongside, "Watch alongside… (choose the channel)"),
    b(C::ctrl(Key::F), A::ToggleFavourite, "Save to / remove from My list (channels: Favourites)"),
    b(C::ctrl_shift(Key::F), A::AddToList, "Watchlists… (channels: Favourites lists…)"),
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
    b(C::key(Key::ArrowLeft), A::SeekBack, "Skip back / forward (small step)"),
    b(C::key(Key::ArrowRight), A::SeekForward, "Skip back / forward (small step)"),
    b(C::shift(Key::ArrowLeft), A::SeekBackBig, "Skip back / forward (big step)"),
    b(C::shift(Key::ArrowRight), A::SeekForwardBig, "Skip back / forward (big step)"),
    b(C::ch('H'), A::ChannelHistory, "Channel history"),
    b(C::ch('L'), A::JumpToLive, "Back to live"),
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
    b(C::ctrl(Key::O), A::PlayFrom, "Play from another copy…"),
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

/// The cog keeps navigation inside the pop-up; other player shortcuts still work.
pub static PLAYER_SETTINGS: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Previous / next menu row"),
    b(C::key(Key::ArrowDown), A::Down, "Previous / next menu row"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous menu row"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous menu row"),
    b(C::key(Key::Enter), A::Activate, "Open / choose / reset value"),
    b(C::key(Key::Space), A::Activate, "Open / choose / reset value"),
    b(C::key(Key::ArrowRight), A::Right, "Open / choose / increase value"),
    b(C::key(Key::ArrowLeft), A::Back, "Back a page, then close"),
    b(C::key(Key::Backspace), A::Back, "Back a page, then close"),
    b(C::key(Key::Escape), A::Back, "Back a page, then close"),
];

/// The popped-out (video-only) player, above `PlayerLive` / `PlayerVod`: every player key
/// works; Esc and `o` (also a double-click) put the window back, `f` puts it back fullscreen;
/// `p` and `q` leave the player as usual and put the window back too.
pub static POP_OUT: &[Binding] = &[
    b(C::key(Key::Escape), A::TogglePopOut, "Back to the full window (also double-click)"),
    b(C::ch('o'), A::TogglePopOut, "Back to the full window (also double-click)"),
    b(C::ch('f'), A::ToggleWindowFullscreen, "Back to the full window, fullscreen"),
    b(C::key(Key::Delete), A::EndNewsFlash, "Cancel the coming switch / end the news flash (back to what you were watching)"),
];

/// Multi-view (DESIGN §35), above the focused picture's player context. `q` shadows "Stop and
/// close" only here; Esc keeps its ladder (it never closes a picture).
pub static MULTI_VIEW: &[Binding] = &[
    b(C::ch('w'), A::NextPicture, "Next picture"),
    b(C::ctrl(Key::ArrowLeft), A::FocusPictureLeft, "Picture in that direction"),
    b(C::ctrl(Key::ArrowRight), A::FocusPictureRight, "Picture in that direction"),
    b(C::ctrl(Key::ArrowUp), A::FocusPictureUp, "Picture in that direction"),
    b(C::ctrl(Key::ArrowDown), A::FocusPictureDown, "Picture in that direction"),
    b(C::ch('M'), A::SoundHere, "Sound from this picture"),
    b(C::ch('W'), A::SwapPictures, "Swap the pictures' places"),
    b(C::ch('t'), A::CycleLayout, "Layout: side by side, big + small, picture in picture"),
    b(C::ch('q'), A::ClosePicture, "Close this picture (the other carries on)"),
    b(C::ctrl(Key::M), A::WatchAlongside, "Watch alongside… (choose the channel)"),
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
    b(C::key(Key::Enter), A::Activate, "Issues: show its guide channels · Guide channels: assign"),
    b(C::key(Key::Delete), A::ClearEpg, "Reset to automatic (guide channel and time shift)"),
    // Guide fixes (DESIGN §31); Shift+Enter shadows the list's "Play fullscreen" here only
    b(C::shift(Key::Enter), A::AcceptFix, "Accept the proposed fix (every marked channel)"),
    b(C::ctrl(Key::A), A::MarkAll, "Mark all shown / none"),
    b(C::ctrl(Key::Z), A::UndoGuideFix(None), "Undo the last guide fix"),
    b(C::ctrl_shift(Key::ArrowRight), A::ShiftLater, "Time shift +1 h / −1 h"),
    b(C::ctrl_shift(Key::ArrowLeft), A::ShiftEarlier, "Time shift +1 h / −1 h"),
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

/// The item shortcuts the search overlay acts on (no reorder, sort or remove there: a hit
/// list has no order of its own and nothing to remove from).
pub static SEARCH_ITEM_ACTIONS: &[Binding] = &[
    b(C::ctrl(Key::F), A::ToggleFavourite, "Save to / remove from My list (channels: Favourites)"),
    b(C::ctrl_shift(Key::F), A::AddToList, "Watchlists… (channels: Favourites lists…)"),
    b(C::ctrl(Key::H), A::ToggleHidden, "Hide / unhide"),
    b(C::key(Key::F2), A::Rename, "Rename"),
    b(C::ctrl(Key::L), A::ToggleLock, "Lock with PIN / unlock"),
    b(C::ctrl(Key::G), A::OpenInGuide, "Show in TV guide"),
    b(C::ctrl(Key::I), A::ShowDetails, "Details / edit information"),
    b(C::ctrl(Key::E), A::AssignEpg, "Assign guide channel"),
    b(C::ctrl(Key::W), A::MarkWatched, "Mark watched / unwatched"),
    b(C::ctrl(Key::N), A::QueueAdd, "Add to Up next"),
    b(C::ctrl_shift(Key::N), A::QueuePlayNext, "Play next (Up next)"),
    b(C::ctrl(Key::M), A::WatchAlongside, "Watch alongside"),
    b(C::alt(Key::C), A::CastItem, "Cast to a TV…"),
    b(C::shift(Key::F10), A::ContextMenu, "List these shortcuts"),
    b(C::ctrl(Key::Enter), A::ContextMenu, "List these shortcuts"),
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

/// Tick lists ("Choose what to show"), stacked above `Modal`. Space is bound as the KEY (the
/// `Modal` below would take it as Confirm); its `Text` twin arrives as `FilterInput(' ')`,
/// which tick lists ignore (their entries are one word).
pub static CHECKLIST: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Home), A::Home, "First / last"),
    b(C::key(Key::End), A::End, "First / last"),
    b(C::key(Key::Space), A::ToggleMark, "Tick / untick"),
    b(C::key(Key::Insert), A::ToggleMark, "Tick / untick"),
    b(C::ctrl(Key::A), A::MarkAll, "Tick all / none (of the rows shown)"),
    b(C::ctrl(Key::E), A::MarkDefaults, "Just my country + English"),
    b(C::key(Key::Enter), A::Activate, "Save"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
    b(C::key(Key::Escape), A::Back, "Clear the filter, then cancel"),
    // any other printable key filters (FilterInput)
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
    // ← always steps to the page list (spatial panes, DESIGN §15.1); values change with →
    // and Shift+←/→ (H/L) — on Interface almost every row has a value, so "← changes the
    // value where there is one" would almost never reach the page list
    b(C::key(Key::ArrowLeft), A::Left, "Back to the page list"),
    b(C::ch('h'), A::Left, "Back to the page list"),
    b(C::key(Key::ArrowRight), A::Right, "Increase / next option"),
    b(C::ch('l'), A::Right, "Increase / next option"),
    b(C::shift(Key::ArrowRight), A::Increase, "Increase / next option"),
    b(C::ch('L'), A::Increase, "Increase / next option"),
    b(C::shift(Key::ArrowLeft), A::Decrease, "Decrease / previous option"),
    b(C::ch('H'), A::Decrease, "Decrease / previous option"),
    b(C::alt(Key::ArrowUp), A::MoveUp, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::ch('K'), A::MoveUp, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::alt(Key::ArrowDown), A::MoveDown, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::ch('J'), A::MoveDown, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous section"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous section"),
    b(C::key(Key::Enter), A::Activate, "Toggle / open"),
    b(C::key(Key::Space), A::Activate, "Toggle / open"),
    b(C::ch('x'), A::ContextMenu, "Delete / more actions"),
    b(C::key(Key::Delete), A::ContextMenu, "Delete / more actions"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::ch('?'), A::ToggleHelp, "Keyboard help"),
];

pub static SETTINGS: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Move"),
    b(C::key(Key::ArrowDown), A::Down, "Move"),
    // ← always steps to the page list (spatial panes, DESIGN §15.1); values change with →
    // and Shift+←/→ — on Interface almost every row has a value, so "← changes the
    // value where there is one" would almost never reach the page list
    b(C::key(Key::ArrowLeft), A::Left, "Back to the page list"),
    b(C::key(Key::ArrowRight), A::Right, "Increase / next option"),
    b(C::shift(Key::ArrowRight), A::Increase, "Increase / next option"),
    b(C::shift(Key::ArrowLeft), A::Decrease, "Decrease / previous option"),
    b(C::alt(Key::ArrowUp), A::MoveUp, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::alt(Key::ArrowDown), A::MoveDown, "Move up / down (reorder: guide priority, playlists, lists, Following rules)"),
    b(C::key(Key::Tab), A::FocusNext, "Next / previous section"),
    b(C::shift(Key::Tab), A::FocusPrev, "Next / previous section"),
    b(C::key(Key::Enter), A::Activate, "Toggle / open"),
    b(C::ch(' '), A::FilterInput(' '), "Toggle / open (types a space while filtering)"),
    b(C::key(Key::Delete), A::ContextMenu, "Delete / more actions"),
    b(C::key(Key::Escape), A::Back, "Back"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete filter character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete filter word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
];

/// Watchlist play order and entry actions (DESIGN §42 §2.4).
pub static WATCHLISTS: &[Binding] = &[
    b(C::key(Key::ArrowRight), A::Right, "Open entries / move right"),
    b(C::ctrl(Key::I), A::ShowDetails, "Episodes… / film details"),
    b(C::ctrl(Key::W), A::MarkWatched, "Mark current episode / title watched; done entry unwatched"),
    b(C::ctrl_shift(Key::Enter), A::WatchlistOne, "Play just this one"),
    b(C::ch('s'), A::WatchlistSkip, "Skip in this list / unskip"),
    b(C::ch('o'), A::WatchlistOrder, "In order / Any order"),
    b(C::ch('+'), A::WatchlistNew, "New list…"),
];

/// The help overlay. Every printable key types into its filter (`?` too: it closes the overlay
/// only while the filter is empty), so no letter shortcut can fire while help is open.
pub static HELP: &[Binding] = &[
    b(C::key(Key::ArrowUp), A::Up, "Scroll"),
    b(C::key(Key::ArrowDown), A::Down, "Scroll"),
    b(C::key(Key::PageUp), A::PageUp, "Page up / down"),
    b(C::key(Key::PageDown), A::PageDown, "Page up / down"),
    b(C::key(Key::Home), A::Home, "Top / bottom"),
    b(C::key(Key::End), A::End, "Top / bottom"),
    b(C::key(Key::Backspace), A::FilterBackspace, "Delete character"),
    b(C::ctrl(Key::Backspace), A::FilterDeleteWord, "Delete word"),
    b(C::ctrl(Key::U), A::FilterClear, "Clear filter"),
    b(C::key(Key::Escape), A::Back, "Clear the filter, then close"),
    b(C::key(Key::F1), A::ToggleHelp, "Clear the filter, then close"),
    b(C::ch('?'), A::FilterInput('?'), "Close (types ? while filtering)"),
    b(C::key(Key::Enter), A::Activate, "Close"),
];

/// The bindings of one context, in priority order (context-specific first).
pub fn tables(ctx: KeyContext) -> Vec<&'static [Binding]> {
    match ctx {
        KeyContext::Global => vec![GLOBAL],
        KeyContext::Watchlists => vec![WATCHLISTS],
        KeyContext::List => vec![LIST, ITEM_ACTIONS],
        KeyContext::ListFiltering => vec![LIST_FILTERING, ITEM_ACTIONS],
        KeyContext::PosterGrid => vec![POSTER_GRID, ITEM_ACTIONS],
        KeyContext::PosterGridFiltering => vec![POSTER_GRID_FILTERING, ITEM_ACTIONS],
        KeyContext::Detail => vec![DETAIL, ITEM_ACTIONS],
        KeyContext::Guide => vec![GUIDE],
        KeyContext::PlayerLive => vec![PLAYER_LIVE, PLAYER_COMMON],
        KeyContext::PlayerVod => vec![PLAYER_VOD, PLAYER_COMMON],
        KeyContext::PlayerOsd => vec![PLAYER_OSD],
        KeyContext::PlayerSettings => vec![PLAYER_SETTINGS],
        KeyContext::PopOut => vec![POP_OUT],
        KeyContext::NumericEntry => vec![NUMERIC_ENTRY],
        KeyContext::EpgMapping => vec![EPG_MAPPING],
        KeyContext::Search => vec![SEARCH, SEARCH_ITEM_ACTIONS],
        KeyContext::Modal => vec![MODAL],
        KeyContext::TextInput => vec![TEXT_INPUT],
        KeyContext::Panel => vec![PANEL],
        KeyContext::Settings => vec![SETTINGS],
        KeyContext::Checklist => vec![CHECKLIST],
        KeyContext::Help => vec![HELP],
        KeyContext::MultiView => vec![MULTI_VIEW],
    }
}

/// Contexts whose unbound printable characters become `FilterInput`.
pub fn is_filtering(ctx: KeyContext) -> bool {
    matches!(
        ctx,
        KeyContext::Settings
            | KeyContext::List
            | KeyContext::ListFiltering
            | KeyContext::PosterGrid
            | KeyContext::PosterGridFiltering
            | KeyContext::Search
            | KeyContext::Checklist
            | KeyContext::Help
    )
}

fn find(tables: &[&'static [Binding]], ev: &egui::Event) -> Option<Action> {
    tables.iter().find_map(|t| t.iter().find(|bd| bd.chord.matches(ev)).map(|bd| bd.action))
}

/// F1…F35.
fn is_fkey(key: Key) -> bool {
    key.name().strip_prefix('F').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Ctrl/Alt chord or F-key: the only events a focused text field lets through (to `Global`).
fn is_chord_or_fkey(ev: &egui::Event) -> bool {
    match ev {
        egui::Event::Key { key, modifiers, .. } => modifiers.ctrl || modifiers.alt || modifiers.command || is_fkey(*key),
        _ => false,
    }
}

/// Resolve one input event against a context stack (innermost first; `Global` is appended
/// automatically). Returns `None` for unbound events (they flow to egui widgets).
pub fn resolve(stack: &[KeyContext], ev: &egui::Event) -> Option<Action> {
    let action = resolve_inner(stack, ev)?;
    // Auto-repeat never steps out of a pane/layer (holding Backspace to clear a filter; a
    // filtered list's ← has no movement to repeat) and never flips between the last two
    // channels (the ← that closed a player side panel, held on).
    let steps_out = matches!(action, Action::Parent | Action::Back | Action::LastChannel)
        || (action == Action::Left && stack.first() == Some(&KeyContext::ListFiltering));
    if steps_out && matches!(ev, egui::Event::Key { repeat: true, .. }) {
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
                        && !c.is_control()
                    {
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
/// Whether `a` belongs to a feature that is on: the batch features' keys stay bound (so the
/// no-duplicate tests cover them) but are left out of the help overlay and the hint line until
/// their feature's `ENABLED` flips (base-B; each track flips only its flag).
pub fn feature_on(a: Action) -> bool {
    match a {
        A::QueueAdd | A::QueuePlayNext | A::GoQueue | A::QueuePanel | A::QueueNext | A::QueuePrev => crate::queue::ENABLED,
        A::FollowProgramme | A::UndoFollow(_) | A::OpenFollowing => crate::follow::ENABLED,
        A::RevealSpoilers | A::SpoilerMenu => crate::spoiler::ENABLED,
        A::AcceptFix | A::ShiftLater | A::ShiftEarlier | A::UndoGuideFix(_) => crate::guide_fix::ENABLED,
        A::NextPicture
        | A::FocusPictureLeft
        | A::FocusPictureRight
        | A::FocusPictureUp
        | A::FocusPictureDown
        | A::SoundHere
        | A::SwapPictures
        | A::CycleLayout
        | A::ClosePicture
        | A::WatchAlongside => crate::ui::multiview::ENABLED,
        A::SleepTimer | A::EndNewsFlash | A::ReminderOptions => crate::reminders::ENABLED,
        A::CastMenu | A::StopCasting | A::PlayHere | A::CastItem | A::PlayOnMenu | A::CastUnlocked => crate::cast::ENABLED,
        _ => true,
    }
}

pub fn help_rows(ctx: KeyContext) -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for table in tables(ctx) {
        for bd in table.iter().filter(|bd| feature_on(bd.action)) {
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
    // also Home's rails (Enter plays a channel or opens a title; Home has no sort): only verbs
    // that hold for both
    (
        KeyContext::PosterGrid,
        &[(A::Activate, "open"), (A::ActivateAlt, "play"), (A::ToggleFavourite, "fav"), (A::ContextMenu, "more"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::PosterGridFiltering, &[(A::Activate, "open"), (A::FilterClear, "clear")]),
    (KeyContext::Detail, &[(A::Activate, "play"), (A::TabNext, "season"), (A::MarkWatched, "watched"), (A::Back, "back")]),
    (
        KeyContext::Guide,
        &[(A::Activate, "details"), (A::GuideWatch, "watch"), (A::GuideRemind, "remind"), (A::FollowProgramme, "follow"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::Panel, &[(A::Activate, "toggle"), (A::Right, "change"), (A::MoveUp, "reorder"), (A::ContextMenu, "more"), (A::ToggleHelp, "help")]),
    (
        KeyContext::Settings,
        &[(A::Activate, "toggle"), (A::Right, "change"), (A::MoveUp, "reorder"), (A::ContextMenu, "more"), (A::ToggleHelp, "help")],
    ),
    (KeyContext::EpgMapping, &[(A::Activate, "pick"), (A::ClearEpg, "reset")]),
    (KeyContext::NumericEntry, &[(A::Activate, "go"), (A::FilterBackspace, "delete"), (A::Back, "cancel")]),
    (KeyContext::Search, &[(A::Activate, "open"), (A::ActivateAlt, "details"), (A::FocusNext, "section"), (A::Back, "close")]),
    (KeyContext::TextInput, &[(A::Activate, "confirm"), (A::FocusNext, "next field"), (A::Back, "leave")]),
    (KeyContext::Modal, &[(A::Activate, "confirm"), (A::Back, "cancel")]),
    (KeyContext::Checklist, &[(A::ToggleMark, "tick"), (A::MarkAll, "all/none"), (A::MarkDefaults, "my country"), (A::Activate, "save")]),
    (KeyContext::Help, &[(A::Back, "clear / close"), (A::FilterClear, "clear filter")]),
    (
        KeyContext::MultiView,
        &[
            (A::NextPicture, "next picture"),
            (A::SoundHere, "sound here"),
            (A::SwapPictures, "swap"),
            (A::CycleLayout, "layout"),
            (A::ClosePicture, "close picture"),
        ],
    ),
];

/// Hints of a filtering list inside a dialog or menu (Esc clears the filter there).
const MODAL_FILTERING_HINTS: &[(Action, &str)] = &[(A::Activate, "confirm"), (A::FilterClear, "clear")];
const SETTINGS_FILTERING_HINTS: &[(Action, &str)] = &[(A::Activate, "show setting"), (A::FilterClear, "clear")];

/// The hints of a context stack: (action, key, verb) for the innermost context that has
/// hints. A list inside a dialog or menu (`[List, Modal]`) gets the dialog's verbs: the
/// screen-list verbs (play, fav…) don't apply there. Keys come from the stack's own tables.
pub fn hints(stack: &[KeyContext]) -> Vec<(Action, KeyChord, &'static str)> {
    let modal = stack.contains(&KeyContext::Modal);
    for ctx in stack {
        let items = match ctx {
            KeyContext::List if modal => continue,
            KeyContext::ListFiltering if modal => MODAL_FILTERING_HINTS,
            KeyContext::ListFiltering if stack.contains(&KeyContext::Settings) => SETTINGS_FILTERING_HINTS,
            _ => match HINTS.iter().find(|(c, _)| c == ctx) {
                Some((_, items)) => *items,
                None => continue,
            },
        };
        let t = tables(*ctx);
        let v: Vec<_> = items
            .iter()
            .filter(|(a, _)| feature_on(*a))
            .filter_map(|(a, verb)| {
                t.iter().chain(std::iter::once(&GLOBAL)).find_map(|tb| tb.iter().find(|bd| bd.action == *a)).map(|bd| (*a, bd.chord, *verb))
            })
            .collect();
        // a context whose hints are all of a feature that is off (multi-view) gives way
        if !v.is_empty() {
            return v;
        }
    }
    Vec::new()
}

/// Status-bar hint line for the innermost context that has hints, e.g.
/// "Enter play · Shift+Enter fullscreen · Ctrl+F fav · Shift+F10 more · ? help".
pub fn hint_line(stack: &[KeyContext]) -> String {
    hint_line_with(stack, &[])
}

/// [`hint_line`] with `extra` hints the screen adds (Movies with two playlists: "Alt+P
/// playlist"; a stack card: "Space copies · Ctrl+O play from"), before "help" — keys looked up
/// in the stack's own tables, then the global one. Not inside dialogs and text fields, whose
/// keys come first.
pub fn hint_line_with(stack: &[KeyContext], extra: &[(Action, &'static str)]) -> String {
    let mut v = hints(stack);
    if !stack.iter().any(|c| matches!(c, KeyContext::Modal | KeyContext::TextInput | KeyContext::Search | KeyContext::Help)) {
        let at = v.iter().position(|(a, ..)| *a == A::ToggleHelp).unwrap_or(v.len());
        let bound =
            |a: Action| stack.iter().flat_map(|c| tables(*c)).chain(std::iter::once(GLOBAL)).find_map(|tb| tb.iter().find(|bd| bd.action == a));
        let add: Vec<_> = extra.iter().filter_map(|(a, verb)| bound(*a).map(|bd| (*a, bd.chord, *verb))).collect();
        v.splice(at..at, add);
    }
    v.iter().map(|(_, chord, verb)| format!("{} {verb}", chord.label())).collect::<Vec<_>>().join(" · ")
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
    fn key(k: Key, ctrl: bool) -> egui::Event {
        key_ev(k, ctrl, false, false, false)
    }
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
        // a held ←: a filtered list steps out once, the player behind it never zaps on repeat;
        // held arrows still move in a grid and still change a Settings value
        let held = |k, shift| key_ev(k, false, false, shift, true);
        assert_eq!(resolve(&[K::ListFiltering], &held(Key::ArrowLeft, false)), None);
        assert_eq!(resolve(&[K::ListFiltering, K::PlayerLive], &held(Key::ArrowLeft, false)), None);
        assert_eq!(resolve(&[K::PlayerLive], &held(Key::ArrowLeft, false)), Some(Action::SeekBack), "held skips repeat");
        assert_eq!(resolve(&[K::PlayerLive], &key(Key::ArrowLeft, false)), Some(Action::SeekBack));
        assert_eq!(resolve(&[K::PosterGrid], &held(Key::ArrowLeft, false)), Some(Action::Left));
        assert_eq!(resolve(&[K::Panel], &held(Key::ArrowLeft, true)), Some(Action::Decrease));
    }

    #[test]
    fn grids_filter_and_detail_has_seasons() {
        assert_eq!(resolve(&[K::PosterGrid], &egui::Event::Text("a".into())), Some(Action::FilterInput('a')));
        assert_eq!(resolve(&[K::PosterGridFiltering], &key(Key::Backspace, false)), Some(Action::FilterBackspace));
        assert_eq!(resolve(&[K::Detail], &egui::Event::Text("]".into())), Some(Action::TabNext));
        assert_eq!(resolve(&[K::Detail], &key(Key::W, true)), Some(Action::MarkWatched));
        assert!(help_rows(K::PosterGrid).iter().any(|(_, h)| *h == "Type to filter"));
    }

    /// Spatial panes: ← steps left in Settings (values change with → and Shift+←/→) and
    /// in a filtered list (Backspace still edits the filter); a text field keeps its arrows.
    #[test]
    fn left_steps_to_the_pane_on_the_left() {
        let shift = |k| key_ev(k, false, false, true, false);
        assert_eq!(resolve(&[K::Settings], &key(Key::ArrowLeft, false)), Some(Action::Left));
        assert_eq!(resolve(&[K::Settings], &egui::Event::Text("h".into())), Some(Action::FilterInput('h')));
        assert_eq!(resolve(&[K::Settings], &key(Key::ArrowRight, false)), Some(Action::Right));
        assert_eq!(resolve(&[K::Settings], &shift(Key::ArrowLeft)), Some(Action::Decrease));
        assert_eq!(resolve(&[K::Settings], &egui::Event::Text("H".into())), Some(Action::FilterInput('H')));
        assert_eq!(resolve(&[K::Settings], &shift(Key::ArrowRight)), Some(Action::Increase));
        assert_eq!(resolve(&[K::Settings], &egui::Event::Text("L".into())), Some(Action::FilterInput('L')));
        // a text field owns its arrows, shifted (selection) too, and its letters
        assert_eq!(resolve(&[K::TextInput, K::Settings], &key(Key::ArrowLeft, false)), None);
        assert_eq!(resolve(&[K::TextInput, K::Settings], &shift(Key::ArrowLeft)), None);
        assert_eq!(resolve(&[K::TextInput, K::Settings], &egui::Event::Text("H".into())), None);
        // filtered lists: ← leaves the pane, Backspace edits the filter
        assert_eq!(resolve(&[K::ListFiltering], &key(Key::ArrowLeft, false)), Some(Action::Left));
        assert_eq!(resolve(&[K::ListFiltering, K::Modal], &key(Key::ArrowLeft, false)), Some(Action::Left));
        assert_eq!(resolve(&[K::ListFiltering, K::PlayerLive], &key(Key::ArrowLeft, false)), Some(Action::Left), "not the player's ←");
        assert_eq!(resolve(&[K::ListFiltering], &key(Key::Backspace, false)), Some(Action::FilterBackspace));
        assert_eq!(resolve(&[K::List], &key(Key::ArrowLeft, false)), Some(Action::Parent));
        // help: one row per meaning
        let rows = help_rows(K::Settings);
        assert!(rows.iter().any(|(k, h)| *h == "Back to the page list" && k == "←"), "{rows:?}");
        assert!(rows.iter().any(|(k, h)| *h == "Increase / next option" && k == "→ / Shift+→"), "{rows:?}");
        assert!(rows.iter().any(|(k, h)| *h == "Decrease / previous option" && k == "Shift+←"), "{rows:?}");
        assert!(hint_line(&[K::Settings]).contains("→ change") && !hint_line(&[K::Settings]).contains("Shift+"), "{}", hint_line(&[K::Settings]));
    }

    #[test]
    fn panel_reorders_and_mapping_clears() {
        assert_eq!(resolve(&[K::Panel], &key_ev(Key::ArrowUp, false, true, false, false)), Some(Action::MoveUp));
        assert_eq!(resolve(&[K::Settings], &egui::Event::Text("J".into())), Some(Action::FilterInput('J')));
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
    fn every_hint_verb_finds_its_key() {
        for (ctx, items) in HINTS {
            let line = hint_line(&[*ctx]);
            // a feature that is off leaves its verbs out (a context of only those gives way)
            let on = items.iter().filter(|(a, _)| feature_on(*a)).count();
            assert_eq!(if line.is_empty() { 0 } else { line.split(" · ").count() }, on, "{ctx:?}: {line}");
        }
        assert_eq!(hint_line(&[K::TextInput, K::Modal]), "Enter confirm · Tab next field · Esc leave");
        assert_eq!(hint_line(&[K::PlayerLive]), "", "no hint line in the player (no status bar)");
    }

    /// The event a chord produces (what `resolve` sees for it).
    fn event_of(c: KeyChord) -> egui::Event {
        match c.trigger {
            Trigger::Key(k) => key_ev(k, c.ctrl, c.alt, c.shift, false),
            Trigger::Char(ch) => egui::Event::Text(ch.to_string()),
        }
    }

    /// Every hint names a key that, pressed in that stack, resolves to the hinted action (not
    /// shadowed by an inner context, not a key of another table).
    #[test]
    fn hint_lines_only_name_keys_that_do_what_they_say() {
        let mut stacks: Vec<Vec<KeyContext>> = KeyContext::ALL.iter().map(|c| vec![*c]).collect();
        stacks.extend([
            vec![K::List, K::Modal],
            vec![K::ListFiltering, K::Modal],
            vec![K::TextInput, K::Modal],
            vec![K::TextInput, K::Panel],
            vec![K::EpgMapping, K::List],
            vec![K::NumericEntry, K::Guide],
            vec![K::NumericEntry, K::PlayerLive],
            vec![K::PlayerOsd, K::PlayerVod],
        ]);
        for stack in &stacks {
            for (action, chord, verb) in hints(stack) {
                assert_eq!(resolve(stack, &event_of(chord)), Some(action), "{stack:?}: “{} {verb}”", chord.label());
            }
        }
    }

    /// The playlist keys (DESIGN §29) work from every list, grid and the guide — Ctrl+PgDn is
    /// not PgDn — and "Alt+P playlist" joins a screen's hint line before "help", never a
    /// dialog's or a text field's.
    #[test]
    fn playlist_keys_and_their_hint() {
        for stack in [vec![K::List], vec![K::PosterGrid], vec![K::Guide], vec![K::TextInput, K::Panel]] {
            assert_eq!(resolve(&stack, &key_ev(Key::PageDown, true, false, false, false)), Some(Action::NextPlaylist), "{stack:?}");
            assert_eq!(resolve(&stack, &key_ev(Key::PageUp, true, false, false, false)), Some(Action::PrevPlaylist), "{stack:?}");
            assert_eq!(resolve(&stack, &key_ev(Key::P, false, true, false, false)), Some(Action::PlaylistPicker), "{stack:?}");
        }
        assert_eq!(resolve(&[K::List], &key(Key::PageDown, false)), Some(Action::PageDown));
        let extra = [(Action::PlaylistPicker, "playlist")];
        let line = hint_line_with(&[K::PosterGrid], &extra);
        assert!(line.ends_with("Alt+P playlist · F1 help") || line.ends_with("Alt+P playlist · ? help"), "{line}");
        assert_eq!(hint_line_with(&[K::List, K::Modal], &extra), hint_line(&[K::List, K::Modal]));
    }

    /// Help filters as you type: every printable key (`?`, `/`, j/k, digits) goes to the filter,
    /// so no letter shortcut of the screen below or of a list can fire while help is open.
    #[test]
    fn help_types_every_printable_key() {
        for c in ' '..='~' {
            assert_eq!(resolve(&[K::Help], &egui::Event::Text(c.to_string())), Some(Action::FilterInput(c)), "{c:?}");
        }
        assert_eq!(resolve(&[K::Help], &egui::Event::Text("é".into())), Some(Action::FilterInput('é')));
        assert_eq!(resolve(&[K::Help], &key(Key::Backspace, false)), Some(Action::FilterBackspace));
        assert_eq!(resolve(&[K::Help], &key(Key::Backspace, true)), Some(Action::FilterDeleteWord));
        assert_eq!(resolve(&[K::Help], &key(Key::U, true)), Some(Action::FilterClear));
        assert_eq!(resolve(&[K::Help], &key(Key::Escape, false)), Some(Action::Back));
        assert_eq!(resolve(&[K::Help], &key(Key::F1, false)), Some(Action::ToggleHelp));
        assert_eq!(resolve(&[K::Help], &key(Key::Q, true)), Some(Action::Quit), "globals keep working");
    }

    /// "Cast to…" on an item (DESIGN §40 §3.1): `C` where typing doesn't filter (the guide, a
    /// title's page; the player's `C` casts what plays), Alt+C everywhere an item is focused —
    /// a list or a grid filtering or not, search — because there a plain `C` types: it starts
    /// or extends the filter as before. Alt+T opens the Play on chooser from anywhere, a text
    /// field included (§3.9).
    #[test]
    fn cast_keys_never_take_a_typed_letter() {
        let c = egui::Event::Text("C".into());
        let alt = |k| key_ev(k, false, true, false, false);
        assert_eq!(resolve(&[K::Guide], &c), Some(Action::CastItem));
        assert_eq!(resolve(&[K::Detail], &c), Some(Action::CastItem));
        assert_eq!(resolve(&[K::PlayerLive], &c), Some(Action::CastMenu), "the player casts what plays");
        for stack in [vec![K::List], vec![K::ListFiltering], vec![K::PosterGrid], vec![K::PosterGridFiltering], vec![K::Search]] {
            assert_eq!(resolve(&stack, &alt(Key::C)), Some(Action::CastItem), "{stack:?}");
        }
        assert_eq!(resolve(&[K::Guide], &alt(Key::C)), Some(Action::CastItem));
        assert_eq!(resolve(&[K::List], &c), Some(Action::FilterInput('C')), "typing still filters");
        assert_eq!(resolve(&[K::PosterGrid], &c), Some(Action::FilterInput('C')));
        assert_eq!(resolve(&[K::Search], &c), Some(Action::FilterInput('C')));
        for stack in [vec![K::List], vec![K::Guide], vec![K::Panel], vec![K::TextInput, K::Panel], vec![K::PlayerVod]] {
            assert_eq!(resolve(&stack, &alt(Key::T)), Some(Action::PlayOnMenu), "{stack:?}");
        }
        assert!(help_rows(K::Guide).iter().any(|(k, h)| *h == "Cast to a TV…" && k == "C / Alt+C"), "{:?}", help_rows(K::Guide));
    }

    #[test]
    fn fkeys() {
        assert!(is_fkey(Key::F1) && is_fkey(Key::F11) && is_fkey(Key::F35));
        assert!(!is_fkey(Key::F) && !is_fkey(Key::Enter));
    }

    #[test]
    fn every_context_has_a_title_and_rows() {
        for c in KeyContext::ALL {
            assert!(!c.title().is_empty());
            // a context of a feature that is off (multi-view) lists nothing yet
            let on = tables(c).iter().any(|t| t.iter().any(|b| feature_on(b.action)));
            assert_eq!(help_rows(c).is_empty(), !on, "{c:?}");
        }
    }
}
