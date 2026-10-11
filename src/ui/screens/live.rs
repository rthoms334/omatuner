//! Live TV (DESIGN §13.2): groups pane | channel list (logo, number, name, now/next +
//! progress via `ctx.now_next`) | preview pane (video via VideoHost Preview + programme
//! details + upcoming). Group list has virtual groups on top: each favourites list, History,
//! Reminders, All channels.
//! * Enter rule: Enter on the channel that is already playing → fullscreen player; otherwise
//!   play in the preview (Shift+Enter always fullscreen). Returning from the player moves the
//!   cursor to the channel that was zapped to.
//! * Preview autoplay (`live_preview_autoplay`, OFF by default): only after the KEYBOARD cursor
//!   rested `preview_delay_ms` (1200), only if nothing plays or the current stream was itself
//!   started by autoplay (`snapshot.origin == LivePreview`); never from hover.
//! * Hover moves the cursor only after real pointer motion (Omarchy `pointerGate`).
//! * Layouts: Wide 3 panes; Medium = channels + preview on the right, groups in a drawer
//!   (← / Backspace or the breadcrumb); Compact = stack, Enter plays fullscreen.
//! * Key contexts: `[List|ListFiltering]` of the focused pane. Owner: WP-11.
//! * Several playlists with channels (DESIGN §29): the scope bar on top (tabs at Wide, the
//!   `● TV 2 ▾` button otherwise; Alt+P, Ctrl+PgDn/PgUp) narrows the groups, All channels,
//!   the favourites lists, History and Reminders — and so the zap lists — to one playlist;
//!   the guide follows. In All, group and channel rows name their playlist in its colour.
//!   "Show in All playlists" off keeps a playlist out of All channels and the groups, not out
//!   of the favourites lists, History or Reminders.
//!
//! Also home of the playback helpers the other WP-11 views share (guide, player, programme
//! popover, edit information): the Enter rule, locked/DRM checks, zap lists, favourites and
//! reminder toggles.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::egui::{self, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{
    ChannelId, ChannelSummary, Collection, FavListId, Group, GroupId, GroupKey, ItemKind, ItemRef, MediaRef, PlayOrigin, Programme, ProgrammeSlim,
    Reminder, ReminderId, ReminderState, SortMode, SourceId, Timestamp, ZapList,
};
use crate::msg::{AppEvent, DataScope, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerEvent, PlayerSnapshot, PlayerState};
use crate::store::library::FavIndex;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::epg_grid::{hhmm, time_left, time_range};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, ListPickerSpec, MenuItem, OverlayResult, RenameSpec, RenameTarget};
use crate::ui::scope::{self, Scope, ScopeFilter, ScopeKind};
use crate::ui::video_host::Placement;
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::rows::{ChannelRowOpts, GroupDecor, channel_row, group_row_decor, grouped, menu_row_source, row_height};
use crate::ui::widgets::{RowState, badge, empty_state::empty_state, glyphs, logo, progress, scope_bar, section, text_field};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, SettingsPage, UiCtx};

use super::group_marks::{self, GroupMarks};

// ====================================================================== shared helpers

/// The stable reference of a channel.
pub(crate) fn channel_ref(ch: &ChannelSummary) -> ItemRef {
    ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key.clone() }
}

/// The live channel the player is on (or zapping to), if any. A failed stream doesn't count
/// (Enter on it plays it again).
pub(crate) fn playing_channel(snap: &PlayerSnapshot) -> Option<ChannelId> {
    if matches!(snap.state, PlayerState::Idle | PlayerState::Error(_) | PlayerState::Ended) {
        return None;
    }
    match snap.playable.as_ref().map(|p| &p.media) {
        Some(MediaRef::Channel(id)) => Some(*id),
        _ => None,
    }
}

/// The zap list of a collection in the Live sort order (`None` when empty).
pub(crate) fn zap_list(ctx: &UiCtx<'_>, collection: &Collection) -> Option<ZapList> {
    zap_list_in(ctx, collection, &ScopeFilter::Everything)
}

/// [`zap_list`] of the channels a playlist scope shows (Live TV and the guide, DESIGN §29).
pub(crate) fn zap_list_in(ctx: &UiCtx<'_>, collection: &Collection, scope: &ScopeFilter) -> Option<ZapList> {
    let channels = scoped_collection(ctx, collection, ctx.config.interface.live_sort, scope);
    (!channels.is_empty()).then(|| ZapList { collection: collection.clone(), channels: channels.into() })
}

/// A collection's channels (`LiveIndex::collection`) from the playlists `scope` shows (the
/// user's own lists: [`ScopeFilter::for_user_list`]).
pub(crate) fn scoped_collection(ctx: &UiCtx<'_>, collection: &Collection, sort: SortMode, scope: &ScopeFilter) -> Vec<ChannelId> {
    let live = &ctx.lib.live;
    let mut ids = live.collection(collection, ctx.favs, &ctx.lib.recent_channels, sort);
    let scope = match collection {
        Collection::Favourites(_) | Collection::History => scope.for_user_list(),
        _ => scope.clone(),
    };
    if scope != ScopeFilter::Everything {
        ids.retain(|id| live.get(*id).is_some_and(|c| scope.admits(c.source_id)));
    }
    ids
}

/// Zap list of a channel's primary group (Search, Home, popovers — DESIGN §13.0).
pub(crate) fn primary_zap(ctx: &UiCtx<'_>, id: ChannelId) -> Option<ZapList> {
    let g = ctx.lib.live.get(id)?.group_ids.first().copied()?;
    zap_list(ctx, &Collection::Group(g))
}

/// Play a channel. Refuses DRM channels (toast), asks for the PIN for locked ones (the PIN
/// overlay hands `retry` back to the opener once unlocked). Returns true when a play command
/// was sent.
pub(crate) fn play_channel(ctx: &mut UiCtx<'_>, id: ChannelId, zap: Option<ZapList>, origin: PlayOrigin, retry: Action) -> bool {
    let Some(ch) = ctx.lib.live.get(id) else {
        ctx.toast(Toast::info("That channel is no longer in your playlists"));
        return false;
    };
    if ch.unsupported {
        ctx.toast(Toast::info("DRM-protected — can't be played"));
        return false;
    }
    if ch.locked && !ctx.parental.is_unlocked(&ctx.config.parental) {
        let reason = format!("“{}” is locked", ch.name);
        ctx.open(OverlayRequest::Pin { then: Box::new(retry), reason });
        return false;
    }
    ctx.play(PlayerCommand::Play(PlayRequest { media: MediaRef::Channel(id), zap, origin, resume: false, immediate: true }));
    true
}

/// The Enter rule (DESIGN §13.0): Enter on the playing channel opens the fullscreen player;
/// otherwise play — here when a preview pane is visible, else fullscreen.
pub(crate) fn activate_channel(ctx: &mut UiCtx<'_>, id: ChannelId, zap: Option<ZapList>, origin: PlayOrigin, preview_visible: bool, retry: Action) {
    if playing_channel(ctx.player) == Some(id) {
        ctx.push(Route::Player);
        return;
    }
    if play_channel(ctx, id, zap, origin, retry) && !preview_visible {
        ctx.push(Route::Player);
    }
}

/// Ctrl+F: toggle membership in the default favourites list (optimistic, DESIGN §5.4).
pub(crate) fn toggle_favourite(ctx: &mut UiCtx<'_>, item: ItemRef, name: &str) {
    let Some(list) = ctx.favs.default_list() else {
        ctx.toast(Toast::error("There is no favourites list"));
        return;
    };
    let on = !ctx.favs.is_fav(&item);
    let now = ctx.now;
    let it = item.clone();
    let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, list, &it, on, now));
    ctx.favs.apply_toggle(list, &item, on);
    ctx.toast(Toast::info(if on { format!("Added “{name}” to Favourites") } else { format!("Removed “{name}” from Favourites") }));
}

/// Set or cancel a reminder for a (future) programme as displayed on `ch`.
pub(crate) fn toggle_reminder(ctx: &mut UiCtx<'_>, ch: &ChannelSummary, p: &ProgrammeSlim, existing: Option<ReminderId>) {
    let writer = ctx.services.store.writer();
    match existing {
        Some(id) => {
            let _ = writer.submit_notify(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id));
            ctx.toast(Toast::info(format!("Reminder cancelled: {}", p.title)));
        }
        None => {
            if p.start <= ctx.now {
                ctx.toast(Toast::info("Reminders are for programmes that haven't started"));
                return;
            }
            let r = Reminder {
                id: ReminderId(0),
                channel: channel_ref(ch),
                channel_name: ch.name.to_string(),
                programme_title: p.title.to_string(),
                start: p.start,
                stop: p.stop,
                lead_s: i64::from(ctx.config.reminders.lead_min) * 60,
                state: ReminderState::Pending,
            };
            let _ = writer.submit_notify(DataScope::Reminders, move |c| crate::store::user::add_reminder(c, &r).map(|_| ()));
            let when = hhmm(p.start, ctx.config.interface.clock_24h);
            ctx.toast(Toast::info(format!("Reminder set: {when} {} on {}", p.title, ch.name)));
        }
    }
}

/// Pending reminders (Settings ▸ Reminders query; a small table read on events, not per
/// frame), inside the frame budget: `None` = deferred to the next frame.
pub(crate) fn load_reminders(ctx: &mut UiCtx<'_>) -> Option<Vec<Reminder>> {
    let db = ctx.db;
    let r = ctx.budget.run(|| crate::store::user::all_reminders(db))?;
    Some(r.unwrap_or_else(|e| {
        tracing::debug!("reminders: {e}");
        Vec::new()
    }))
}

/// Guide freshness (DESIGN §9.5, §13.12): one aggregate over the small `epg_source` table.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GuideInfo {
    /// Enabled EPG sources.
    pub sources: i64,
    /// End of the newest guide data (`None` = no guide imported yet).
    pub max_stop: Option<Timestamp>,
    pub last_refresh: Option<Timestamp>,
}

/// What the Live preview, the guide and the player OSD say about the guide as a whole: the
/// cached [`GuideInfo`] (re-read after `DataScope::Epg/Sources/All`), the progress of the
/// first guide import, and how many channels the guides cover.
#[derive(Default)]
pub(crate) struct GuideStatus {
    info: Option<GuideInfo>,
    /// (library snapshot, channels with a guide, all channels).
    coverage: Option<(u64, usize, usize)>,
}

/// Why a channel has no programmes (DESIGN §13.12), so the copy can say what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoGuide {
    /// The first guide import runs: "Loading guide… 45 %".
    Loading(String),
    /// No guide is set up (or every one is disabled).
    Missing,
    /// Guides are set up but none has data yet (never imported, or the import failed).
    NotLoaded,
    /// The guides are loaded but don't list this channel; they cover `covered` of `total`.
    NotCovered { covered: usize, total: usize },
}

impl NoGuide {
    /// One line for guide cells (row detail lines keep "No guide data" unless loading).
    pub(crate) fn short(&self) -> String {
        match self {
            NoGuide::Loading(l) => l.clone(),
            NoGuide::Missing => "No guide — Ctrl+E adds one".into(),
            NoGuide::NotLoaded => "Guide not loaded — Ctrl+E".into(),
            NoGuide::NotCovered { .. } => "Not in your guide — Ctrl+E maps it".into(),
        }
    }

    /// (title, detail) for the preview pane and the guide header.
    pub(crate) fn copy(&self) -> (String, Option<String>) {
        match self {
            NoGuide::Loading(l) => (l.clone(), None),
            NoGuide::Missing => ("No TV guide yet".into(), Some("Ctrl+E adds an XMLTV guide in Settings ▸ EPG".into())),
            NoGuide::NotLoaded => ("The guide hasn't loaded".into(), Some("Ctrl+E shows it in Settings ▸ EPG".into())),
            // a guide that lists few of the channels (iptv-org: 2 of 11 249): mapping won't
            // help much, a guide that lists them will
            NoGuide::NotCovered { covered, total } if self.poor() => (
                "Not in your guide".into(),
                Some(format!(
                    "Your guides cover {} of {} channels — add one that lists yours in Settings ▸ EPG · Ctrl+E maps this one",
                    grouped(*covered),
                    grouped(*total)
                )),
            ),
            NoGuide::NotCovered { covered, total } => (
                "Not in your guide".into(),
                Some(format!("Your guides cover {} of {} channels · Ctrl+E maps this one", grouped(*covered), grouped(*total))),
            ),
        }
    }

    /// The guides cover under half the channels.
    pub(crate) fn poor(&self) -> bool {
        matches!(self, NoGuide::NotCovered { covered, total } if covered * 2 < *total)
    }
}

impl GuideStatus {
    /// Forget the cached info when the guide sources or their data changed.
    pub(crate) fn on_event(&mut self, ev: &AppEvent) {
        if matches!(ev, AppEvent::Data(DataScope::Epg | DataScope::Sources | DataScope::All)) {
            self.info = None;
        }
    }

    /// (channels with a guide, all channels) of the library (cached per snapshot).
    pub(crate) fn coverage(&mut self, ctx: &UiCtx<'_>) -> (usize, usize) {
        let key = ctx.lib.generation;
        match self.coverage {
            Some((k, covered, total)) if k == key => (covered, total),
            _ => {
                let all = &ctx.lib.live.channels;
                let covered = all.iter().filter(|c| c.epg_channel_id.is_some()).count();
                self.coverage = Some((key, covered, all.len()));
                (covered, all.len())
            }
        }
    }

    /// Why a channel without a guide mapping has none; `None` while the read waits for budget.
    pub(crate) fn no_guide(&mut self, ctx: &mut UiCtx<'_>) -> Option<NoGuide> {
        if let Some(l) = self.loading_label(ctx) {
            return Some(NoGuide::Loading(l));
        }
        let info = self.info(ctx)?;
        Some(if info.sources == 0 {
            NoGuide::Missing
        } else if info.max_stop.is_none() {
            NoGuide::NotLoaded
        } else {
            let (covered, total) = self.coverage(ctx);
            NoGuide::NotCovered { covered, total }
        })
    }

    /// The guide info; `None` while the read waits for frame budget.
    pub(crate) fn info(&mut self, ctx: &mut UiCtx<'_>) -> Option<GuideInfo> {
        if self.info.is_none() {
            let db = ctx.db;
            self.info = ctx.budget.run(|| {
                db.query_row("SELECT count(*), max(max_stop), max(last_refresh) FROM epg_source WHERE enabled = 1", [], |r| {
                    Ok(GuideInfo { sources: r.get(0)?, max_stop: r.get(1)?, last_refresh: r.get(2)? })
                })
                .unwrap_or_else(|e| {
                    tracing::debug!("guide info: {e}");
                    GuideInfo::default()
                })
            });
        }
        self.info
    }

    /// "Loading guide… 45 %" while the first guide import runs (no guide data at all yet):
    /// rows, grid and preview say that instead of "No guide data", which would send new users
    /// off to map EPG channels by hand (DESIGN §13.12).
    pub(crate) fn loading_label(&mut self, ctx: &mut UiCtx<'_>) -> Option<String> {
        if self.info(ctx)?.max_stop.is_some() {
            return None;
        }
        let job = ctx.services.jobs.active().into_iter().find(|j| matches!(j.kind, crate::jobs::JobKind::RefreshEpg { .. }))?;
        let pct = job.fraction.filter(|f| f.is_finite()).map(|f| format!(" {:.0} %", f.clamp(0.0, 1.0) * 100.0)).unwrap_or_default();
        Some(format!("Loading guide…{pct}"))
    }
}

/// Channels the resolver's zap skips for failures (`store::live::should_skip`: ≥ 2 failures
/// in the last 24 h), read inside the frame budget (`None` when it is spent).
pub(crate) fn failing_set(ctx: &mut UiCtx<'_>) -> Option<HashSet<ItemRef>> {
    let (db, since) = (ctx.db, ctx.now - 86_400);
    let r = ctx.budget.run(|| crate::store::live::failures(db, since))?;
    Some(match r {
        Ok(m) => m.into_iter().filter(|(_, f)| f.count >= 2).map(|(k, _)| k).collect(),
        Err(e) => {
            tracing::debug!("zap failures: {e}");
            HashSet::new()
        }
    })
}

/// Ctrl+E on a channel: the EPG mapping screen for it — or, while no guide is loaded (there is
/// nothing to map to), Settings ▸ EPG to add one.
pub(crate) fn assign_epg(ctx: &mut UiCtx<'_>, guide: &mut GuideStatus, channel: ChannelId) {
    match guide.no_guide(ctx) {
        Some(NoGuide::Missing | NoGuide::NotLoaded) => ctx.push(Route::Settings(SettingsPage::Epg)),
        _ => ctx.push(Route::EpgMapping { channel: Some(channel) }),
    }
}

/// "3 h ago" / "2 d ago" (the status bar's freshness wording).
pub(crate) fn age_label(now: Timestamp, at: Timestamp) -> String {
    let age = (now - at).max(0);
    match age {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} min ago", age / 60),
        3_600..=172_799 => format!("{} h ago", age / 3_600),
        _ => format!("{} d ago", age / 86_400),
    }
}

/// Play a past programme from the archive, fullscreen.
pub(crate) fn play_catchup(ctx: &mut UiCtx<'_>, channel: ChannelId, p: &ProgrammeSlim, retry: Action) {
    let Some(ch) = ctx.lib.live.get(channel) else { return };
    if ch.locked && !ctx.parental.is_unlocked(&ctx.config.parental) {
        let reason = format!("“{}” is locked", ch.name);
        ctx.open(OverlayRequest::Pin { then: Box::new(retry), reason });
        return;
    }
    // the provider's catch-up id (Kodi `catchup-id`) is only on the full programme row
    let catchup_id = if p.flags.has_catchup_id {
        let db = ctx.db;
        ctx.budget.run(|| crate::store::epg::programme(db, p.id)).and_then(Result::ok).and_then(|full| full.catchup_id)
    } else {
        None
    };
    let media = MediaRef::Catchup { channel, start: p.start, stop: p.stop, catchup_id };
    ctx.play(PlayerCommand::Play(PlayRequest { media, zap: None, origin: PlayOrigin::Guide, resume: false, immediate: true }));
    ctx.push(Route::Player);
}

/// Encode a menu choice index as an action (menus report an `Action`; see `ContextMenuSpec`).
/// Uses Supplementary Private Use Area-A characters, which no keyboard produces.
pub(crate) fn choice_action(i: usize) -> Action {
    Action::FilterInput(char::from_u32(0xF0000 + i as u32).unwrap_or('\u{F0000}'))
}

/// Decode [`choice_action`].
pub(crate) fn choice_index(a: Action) -> Option<usize> {
    match a {
        Action::FilterInput(c) if (0xF0000..0xFFFFE).contains(&(c as u32)) => Some(c as usize - 0xF0000),
        _ => None,
    }
}

/// Shortcut label of an action in a key context (menu hints come from the keymap tables).
pub(crate) fn shortcut(ctx: KeyContext, action: Action) -> Option<String> {
    crate::keymap::tables(ctx).iter().find_map(|t| t.iter().find(|b| b.action == action)).map(|b| b.chord.label())
}

/// One context-menu row.
pub(crate) fn menu_item(icon: &'static str, label: impl Into<String>, action: Action, enabled: bool, key_ctx: KeyContext) -> MenuItem {
    MenuItem { icon, label: label.into(), action, enabled, shortcut: shortcut(key_ctx, action) }
}

/// Programme details for the preview pane / OSD: description + upcoming, loaded within the
/// frame budget and kept until the current programme ends.
#[derive(Default)]
pub(crate) struct ProgrammeDetails {
    key: Option<(ChannelId, Option<crate::model::ProgrammeId>)>,
    pub description: Option<String>,
    pub upcoming: Option<Vec<ProgrammeSlim>>,
    loaded_desc: bool,
}

impl ProgrammeDetails {
    /// Make sure the details of `ch` (current programme `now_prog`) are loaded; DB reads go
    /// through the frame budget (deferred reads retry next frame).
    pub(crate) fn ensure(&mut self, ctx: &mut UiCtx<'_>, ch: &ChannelSummary, now_prog: Option<&ProgrammeSlim>) {
        let key = (ch.id, now_prog.map(|p| p.id));
        if self.key != Some(key) {
            *self = Self { key: Some(key), ..Default::default() };
        }
        let db = ctx.db;
        if !self.loaded_desc {
            match now_prog {
                Some(p) => {
                    if let Some(r) = ctx.budget.run(|| crate::store::epg::programme(db, p.id)) {
                        self.description = r.ok().and_then(|full: Programme| full.description);
                        self.loaded_desc = true;
                    }
                }
                None => self.loaded_desc = true,
            }
        }
        if self.upcoming.is_none() {
            match ch.epg_channel_id {
                Some(epg) => {
                    let shift = i64::from(ch.tvg_shift_min) * 60;
                    let from = now_prog.map_or(ctx.now, |p| p.stop) - shift;
                    if let Some(r) = ctx.budget.run(|| crate::store::epg::upcoming(db, epg, from, 4)) {
                        self.upcoming = Some(r.map(|v| v.iter().map(|p| p.shifted(ch.tvg_shift_min)).collect()).unwrap_or_default());
                    }
                }
                None => self.upcoming = Some(Vec::new()),
            }
        }
    }
}

/// Now/next of a channel through the shared cache (`None` = not loaded yet / no guide).
pub(crate) fn now_next_of(ctx: &mut UiCtx<'_>, ch: &ChannelSummary) -> Option<crate::model::NowNext> {
    let epg = ch.epg_channel_id?;
    let mut reader = ctx.epg_reader();
    ctx.now_next.get((epg, ch.tvg_shift_min), ctx.now, &mut reader, ctx.budget).cloned()
}

/// A neutral skeleton bar list (library loading, DESIGN §13.12).
pub(crate) fn skeleton_rows(ui: &mut egui::Ui, theme: &Theme, row_h: f32) {
    let rect = ui.available_rect_before_wrap();
    let p = ui.painter_at(rect);
    let pad = theme.space(SpaceToken::Lg);
    let n = ((rect.height() / row_h).ceil() as usize).min(40);
    for i in 0..n {
        let y = rect.top() + i as f32 * row_h + row_h / 2.0;
        let w = rect.width() * (0.45 + 0.25 * ((i * 7 % 5) as f32 / 5.0));
        let h = (12.0 * theme.rem()).round();
        p.rect_filled(
            Rect::from_min_size(egui::pos2(rect.left() + pad, y - h / 2.0), egui::vec2(w, h)),
            theme.tokens.rounding,
            theme.tokens.fill_field,
        );
    }
}

// ====================================================================== Live screen

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Entry {
    FavList(FavListId),
    History,
    Reminders,
    All,
    Group(GroupId),
}

impl Entry {
    fn collection(&self) -> Option<Collection> {
        match self {
            Entry::FavList(l) => Some(Collection::Favourites(*l)),
            Entry::History => Some(Collection::History),
            Entry::All => Some(Collection::AllChannels),
            Entry::Group(g) => Some(Collection::Group(*g)),
            Entry::Reminders => None,
        }
    }
    fn of(c: &Collection) -> Option<Entry> {
        match c {
            Collection::AllChannels => Some(Entry::All),
            Collection::Group(g) => Some(Entry::Group(*g)),
            Collection::Favourites(l) => Some(Entry::FavList(*l)),
            Collection::History => Some(Entry::History),
            Collection::Adhoc(_) => None,
        }
    }
}

/// A row of the groups pane (virtual groups are drawn as groups with a glyph prefix).
struct GroupItem {
    entry: Entry,
    /// Group look-alike for `rows::group_row` (virtual groups get a synthetic one).
    group: Group,
    /// Filter haystack.
    name: String,
    /// Short code of the group's country / language tag, when its playlist has several and
    /// the name doesn't show it already.
    tag: Option<String>,
    /// What "Hide these N groups" matches words against: the name and the tag's code
    /// (provider groups only).
    words: String,
    /// The playlist of a provider group (its name shows in All with several playlists).
    source: Option<SourceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    Groups,
    #[default]
    Channels,
}

/// Which menu is open (results come back as `OverlayResult::Chosen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    Context,
    Sort,
}

/// The groups menu's "Countries & languages…" (echoed back as `Action::Digit`; channel numbers
/// never come from a menu).
const MENU_COUNTRIES: u8 = 200;

/// Detail line of the "No playlists yet" empty state (Live and the guide).
pub(crate) const EMPTY_LIBRARY_DETAIL: &str = "Add an M3U playlist or an Xtream login to start watching — press Enter.";

/// Sort choices of the Ctrl+S menu (label, mode).
const SORTS: &[(&str, SortMode)] = &[
    ("Custom order", SortMode::Provider),
    ("Channel number", SortMode::Number),
    ("Name A–Z", SortMode::NameAsc),
    ("Name Z–A", SortMode::NameDesc),
    ("Recently added", SortMode::RecentlyAdded),
];

#[derive(Default)]
pub struct LiveScreen {
    groups: Vec<GroupItem>,
    group_rows: Vec<u32>,
    group_list: ListState,
    /// Index into `groups` of the group whose channels are listed.
    open: Option<Entry>,
    channels: Vec<ChannelId>,
    haystacks: Vec<String>,
    channel_rows: Vec<u32>,
    chan_list: ListState,
    reminders: Vec<Reminder>,
    /// The Reminders group lists one row per reminder: `reminders` index of each `channels`
    /// entry while it is open.
    reminder_rows: Vec<usize>,
    focus: Pane,
    /// Medium: the groups drawer is open; Compact: the groups list is shown.
    drawer: bool,
    /// Library snapshot + favourites signature the lists were built from.
    built: Option<(u64, u64, SortMode, bool)>,
    /// The playlist scope the lists were built for (DESIGN §29).
    built_scope: Option<ScopeFilter>,
    /// The sort a reorder switched to and the config sort it replaced, until the config edit
    /// lands at the end of the frame.
    sort_pending: Option<(SortMode, SortMode)>,
    /// Rebuild groups and channels on the next frame.
    dirty: bool,
    /// Re-read the failure cache (a zap succeeded or failed) — nothing else.
    failed_dirty: bool,
    /// Re-read the reminders (a reminder changed).
    reminders_dirty: bool,
    guide: GuideStatus,
    /// Keyboard cursor rest (preview autoplay).
    rest: Option<(ChannelId, Instant)>,
    failed: HashSet<ItemRef>,
    details: ProgrammeDetails,
    menu: Option<Menu>,
    /// Delete-list confirmation token → list.
    confirm: Option<(u64, FavListId)>,
    layout: Option<LayoutClass>,
    /// Groups marked for a bulk hide (DESIGN §28.7).
    marks: GroupMarks,
    /// Indices into `groups` of the provider groups the filter shows whose names have the
    /// words typed: what "Hide these N groups" hides.
    matching: Vec<u32>,
}

/// Channel management shared by Live and the Guide: favourite, lists, hide, rename, lock (the
/// PIN first; its `Unlocked(ToggleLock)` comes back to the screen) and edit information.
pub(crate) fn channel_item_action(ctx: &mut UiCtx<'_>, ch: &ChannelSummary, action: Action) {
    match action {
        Action::ToggleFavourite => toggle_favourite(ctx, channel_ref(ch), &ch.name),
        Action::AddToList => {
            ctx.open(OverlayRequest::ListPicker(ListPickerSpec { item: channel_ref(ch), title: format!("Lists for “{}”", ch.name) }));
        }
        Action::ToggleHidden => {
            let item = channel_ref(ch);
            let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
                let mut ov = crate::store::user::item_override(c, &item)?.unwrap_or_default();
                ov.hidden = !ov.hidden;
                crate::store::user::set_item_override(c, &item, &ov)
            });
            ctx.toast(Toast::info(format!("Hid “{}” — Settings ▸ Hidden items brings it back", ch.name)));
        }
        Action::Rename => ctx.open(OverlayRequest::Rename(RenameSpec {
            title: "Rename channel".into(),
            initial: ch.name.to_string(),
            target: RenameTarget::Item { item: channel_ref(ch) },
        })),
        Action::ToggleLock if !ctx.parental.is_unlocked(&ctx.config.parental) => {
            ctx.open(OverlayRequest::Pin { then: Box::new(Action::ToggleLock), reason: "Enter your PIN to change locks".into() });
        }
        Action::ToggleLock => {
            let item = channel_ref(ch);
            let lock = !ch.locked;
            let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
                let mut ov = crate::store::user::item_override(c, &item)?.unwrap_or_default();
                ov.locked = lock;
                crate::store::user::set_item_override(c, &item, &ov)
            });
            let hint = pin_hint(ctx);
            ctx.toast(Toast::info(format!("{} “{}”{hint}", if lock { "Locked" } else { "Unlocked" }, ch.name)));
        }
        Action::ShowDetails => ctx.open(OverlayRequest::EditItem(channel_ref(ch))),
        _ => {}
    }
}

/// Appended to lock toasts while no PIN is set (locks do nothing without one).
pub(crate) fn pin_hint(ctx: &UiCtx<'_>) -> &'static str {
    if ctx.config.parental.enabled && ctx.config.parental.pin_hash.is_some() { "" } else { " — set a PIN in Settings ▸ Parental to enforce it" }
}

/// Cheap signature of the favourites (lists and their sizes).
fn fav_signature(f: &FavIndex) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    for l in &f.lists {
        (l.id, &l.name, f.items(l.id).len(), f.items(l.id).first(), f.items(l.id).last()).hash(&mut h);
    }
    h.finish()
}

fn synthetic_group(name: String, count: usize) -> Group {
    Group {
        id: GroupId(-1),
        source_id: SourceId(0),
        kind: ItemKind::Channel,
        key: GroupKey(String::new()),
        name,
        rename: None,
        provider_order: 0,
        user_order: None,
        hidden: false,
        user_hidden: false,
        locked: false,
        item_count: count as u32,
        provider_id: None,
        tag: None,
        filter: Default::default(),
    }
}

impl LiveScreen {
    fn preview_visible(layout: LayoutClass) -> bool {
        layout != LayoutClass::Compact
    }

    fn open_collection(&self) -> Option<Collection> {
        self.open.as_ref().and_then(Entry::collection)
    }

    fn open_index(&self) -> Option<usize> {
        self.open.as_ref().and_then(|e| self.groups.iter().position(|g| &g.entry == e))
    }

    /// The playlist scope (DESIGN §29; the guide follows it).
    fn scope<'l>(ctx: &UiCtx<'l>) -> Scope<'l> {
        Scope::current(ctx, ScopeKind::Live)
    }

    /// A collection's channels in this screen's scope.
    fn collection(&self, ctx: &UiCtx<'_>, coll: &Collection, sort: SortMode) -> Vec<ChannelId> {
        scoped_collection(ctx, coll, sort, &Self::scope(ctx).filter())
    }

    fn signature(ctx: &UiCtx<'_>, sort: SortMode) -> (u64, u64, SortMode, bool) {
        (ctx.lib.generation, fav_signature(ctx.favs), sort, ctx.config.interface.hide_empty_groups)
    }

    /// The channel sort in effect: the config's, or the one a reorder just switched to.
    fn sort(&self, ctx: &UiCtx<'_>) -> SortMode {
        self.sort_pending.map_or(ctx.config.interface.live_sort, |(s, _)| s)
    }

    /// Rebuild when the library, the favourites, the sort or the hide-empty setting changed.
    /// Side tables (failures, reminders) are re-read only when their own events say so, inside
    /// the frame budget — a zap never rebuilds the channel list.
    fn refresh(&mut self, ctx: &mut UiCtx<'_>) {
        if self.built.is_none() {
            self.failed_dirty = true;
            self.reminders_dirty = true;
        }
        if self.failed_dirty
            && let Some(f) = failing_set(ctx)
        {
            self.failed = f;
            self.failed_dirty = false;
        }
        if self.reminders_dirty
            && let Some(r) = load_reminders(ctx)
        {
            self.reminders = r;
            self.reminders_dirty = false;
            // the Reminders group comes and goes with them (rare: a full rebuild is fine)
            self.dirty = true;
        }
        // the config changed since (the edit landed, or something else set the sort)
        if self.sort_pending.is_some_and(|(_, was)| was != ctx.config.interface.live_sort) {
            self.sort_pending = None;
        }
        let sig = Self::signature(ctx, self.sort(ctx));
        let scope = Self::scope(ctx).filter();
        let rescoped = self.built_scope.as_ref() != Some(&scope);
        if self.built == Some(sig) && !self.dirty && !rescoped {
            return;
        }
        if rescoped && self.built_scope.is_some() {
            // another playlist: from the top of its lists
            self.group_list.filter.clear();
            self.chan_list.filter.clear();
            self.chan_list.select(0);
        }
        self.built = Some(sig);
        self.built_scope = Some(scope);
        self.dirty = false;
        self.rebuild_groups(ctx);
        if self.open.as_ref().is_none_or(|e| !self.groups.iter().any(|g| &g.entry == e)) {
            self.open = self.default_entry(ctx);
        }
        self.rebuild_channels(ctx, false);
    }

    fn default_entry(&self, ctx: &UiCtx<'_>) -> Option<Entry> {
        ctx.favs
            .non_empty_lists()
            .next()
            .map(|l| Entry::FavList(l.id))
            .or_else(|| self.groups.iter().find(|g| g.entry == Entry::All).map(|g| g.entry.clone()))
    }

    fn rebuild_groups(&mut self, ctx: &UiCtx<'_>) {
        let keep = self.cursor_group().map(|g| g.entry.clone());
        let live = &ctx.lib.live;
        let scope = Self::scope(ctx);
        let filter = scope.filter();
        // the user's own lists: every playlist in All, even one switched out of it
        let mine = filter.for_user_list();
        let marks = scope.marks(ctx);
        // channel references in scope (favourites lists may hold movies and other playlists')
        let in_scope = |r: &ItemRef| r.kind == ItemKind::Channel && mine.admits(r.source);
        let mut v = Vec::new();
        if !ctx.lib.is_empty() || !live.channels.is_empty() {
            for l in &ctx.favs.lists {
                let name = format!("{} {}", glyphs::FAVOURITE, l.name);
                let n = ctx.favs.items(l.id).iter().filter(|r| in_scope(r)).count();
                v.push(GroupItem {
                    entry: Entry::FavList(l.id),
                    group: synthetic_group(name.clone(), n),
                    name,
                    tag: None,
                    words: String::new(),
                    source: None,
                });
            }
            let name = format!("{} History", glyphs::HISTORY);
            let n = ctx.lib.recent_channels.iter().filter(|id| live.get(**id).is_some_and(|c| mine.admits(c.source_id))).count();
            v.push(GroupItem { entry: Entry::History, group: synthetic_group(name.clone(), n), name, tag: None, words: String::new(), source: None });
            if !self.reminders.is_empty() {
                let name = format!("{} Reminders", glyphs::REMINDER);
                let n = self.reminders.iter().filter(|r| mine.admits(r.channel.source)).count();
                v.push(GroupItem {
                    entry: Entry::Reminders,
                    group: synthetic_group(name.clone(), n),
                    name,
                    tag: None,
                    words: String::new(),
                    source: None,
                });
            }
            let visible = if filter == ScopeFilter::Everything {
                live.channels.len().saturating_sub(live.hidden.len())
            } else {
                scope.count(ctx.lib, scope.selected.map(|s| s.id))
            };
            v.push(GroupItem {
                entry: Entry::All,
                group: synthetic_group("All channels".into(), visible),
                name: "All channels".into(),
                tag: None,
                words: String::new(),
                source: None,
            });
            let tagged = crate::ui::overlays::categories::multi_tag_sources(ctx.lib);
            for g in live.visible_groups(None, ctx.config.interface.hide_empty_groups).into_iter().filter(|g| filter.admits(g.source_id)) {
                let mut group = g.clone();
                group.item_count = live.group_members(g.id).len() as u32;
                let mut name = g.display_name().to_owned();
                // in All, the playlist's name filters too ("tv 2" finds TV 2's groups)
                if let Some(s) = marks.of(g.source_id) {
                    name = format!("{name} {}", s.name);
                }
                let code = g.tag.as_ref().filter(|_| tagged.contains(&g.source_id)).map(crate::sources::category_tags::tag_short);
                // the code filters too ("uk" finds the UK groups smart names renamed to "Sports")
                let words = match &code {
                    Some(code) => {
                        name = format!("{name} {code}");
                        format!("{} {code}", g.display_name())
                    }
                    None => g.display_name().to_owned(),
                };
                let tag = code.filter(|c| !group_marks::has_words(g.display_name(), c));
                v.push(GroupItem { entry: Entry::Group(g.id), group, name, tag, words, source: Some(g.source_id) });
            }
        }
        self.groups = v;
        let listed: HashSet<GroupId> = self.groups.iter().filter_map(|g| if let Entry::Group(id) = g.entry { Some(id) } else { None }).collect();
        self.marks.retain(|id| listed.contains(&id));
        self.refilter_groups();
        // the cursor stays on its group (a reorder moves it)
        if let Some(i) = keep.and_then(|e| self.group_rows.iter().position(|&r| self.groups.get(r as usize).is_some_and(|g| g.entry == e))) {
            self.group_list.select(i);
        }
    }

    fn refilter_groups(&mut self) {
        self.group_rows = self.group_list.filter.apply(self.groups.iter().map(|g| g.name.as_str()));
        self.group_list.set_len(self.group_rows.len());
        let q = self.group_list.filter.query();
        self.matching = if q.is_empty() {
            Vec::new()
        } else {
            let groups = &self.groups;
            self.group_rows
                .iter()
                .copied()
                .filter(|&r| groups.get(r as usize).is_some_and(|g| matches!(g.entry, Entry::Group(_)) && group_marks::word_match(q, &g.words)))
                .collect()
        };
    }

    /// Channels of the open group (sorted like the zap order) + filter haystacks.
    fn rebuild_channels(&mut self, ctx: &UiCtx<'_>, reset_cursor: bool) {
        let keep = self.cursor_channel();
        // Reminders: keep the reminder (a channel can have several)
        let keep_reminder = self.cursor_reminder().map(|r| r.id);
        self.reminder_rows.clear();
        self.channels = match &self.open {
            Some(Entry::Reminders) => {
                // one row per reminder (several on one channel are separate programmes)
                let mut v = Vec::new();
                let scope = Self::scope(ctx).filter().for_user_list();
                for (i, r) in self.reminders.iter().enumerate().filter(|(_, r)| scope.admits(r.channel.source)) {
                    if let Some(c) = ctx.lib.live.by_ref(&r.channel) {
                        v.push(c);
                        self.reminder_rows.push(i);
                    }
                }
                v
            }
            Some(e) => e.collection().map(|c| self.collection(ctx, &c, self.sort(ctx))).unwrap_or_default(),
            None => Vec::new(),
        };
        self.rebuild_haystacks(ctx);
        let reminder_row = keep_reminder.and_then(|id| {
            self.channel_rows
                .iter()
                .position(|&i| self.reminder_rows.get(i as usize).and_then(|r| self.reminders.get(*r)).is_some_and(|r| r.id == id))
        });
        match reminder_row.or_else(|| keep.and_then(|k| self.row_of(k))).filter(|_| !reset_cursor) {
            Some(i) => self.chan_list.select(i),
            None if reset_cursor => self.chan_list.select(0),
            None => {}
        }
    }

    /// Filter haystacks (`"<number> <name>"`, reminders `"<programme> <channel>"`) of
    /// `self.channels` + refilter.
    fn rebuild_haystacks(&mut self, ctx: &UiCtx<'_>) {
        let live = &ctx.lib.live;
        self.haystacks = self
            .channels
            .iter()
            .enumerate()
            .map(|(i, id)| match (live.get(*id), self.reminder_rows.get(i).and_then(|r| self.reminders.get(*r))) {
                (Some(c), Some(r)) => format!("{} {}", r.programme_title, c.name),
                (Some(c), None) => match c.number {
                    Some(n) => format!("{n} {}", c.name),
                    None => c.name.to_string(),
                },
                (None, _) => String::new(),
            })
            .collect();
        self.refilter_channels();
    }

    /// The reminder of the cursor row (Reminders group).
    fn cursor_reminder(&self) -> Option<&Reminder> {
        let row = *self.channel_rows.get(self.chan_list.cursor)? as usize;
        self.reminder_rows.get(row).and_then(|i| self.reminders.get(*i))
    }

    fn refilter_channels(&mut self) {
        self.channel_rows = self.chan_list.filter.apply(self.haystacks.iter().map(String::as_str));
        self.chan_list.set_len(self.channel_rows.len());
    }

    fn cursor_channel(&self) -> Option<ChannelId> {
        self.channel_rows.get(self.chan_list.cursor).and_then(|&i| self.channels.get(i as usize)).copied()
    }

    fn row_of(&self, id: ChannelId) -> Option<usize> {
        self.channel_rows.iter().position(|&i| self.channels.get(i as usize) == Some(&id))
    }

    fn cursor_group(&self) -> Option<&GroupItem> {
        self.group_rows.get(self.group_list.cursor).and_then(|&i| self.groups.get(i as usize))
    }

    /// The provider group under the groups cursor (as the library has it).
    fn cursor_provider_group(&self, ctx: &UiCtx<'_>) -> Option<Group> {
        let Some(Entry::Group(id)) = self.cursor_group().map(|g| &g.entry) else { return None };
        ctx.lib.live.groups.iter().find(|x| x.id == *id).cloned()
    }

    /// Open a group: list its channels, cursor on the first.
    fn open_entry(&mut self, ctx: &mut UiCtx<'_>, entry: Entry) {
        if self.open.as_ref() != Some(&entry) {
            self.open = Some(entry);
            self.chan_list.filter.clear();
            self.rebuild_channels(ctx, true);
        }
        if let Some(i) = self.open_index().and_then(|gi| self.group_rows.iter().position(|&r| r as usize == gi)) {
            self.group_list.select(i);
        }
    }

    /// Put the cursor on `id`, opening a group that contains it if the open one doesn't.
    fn select_channel(&mut self, ctx: &mut UiCtx<'_>, id: ChannelId, prefer: Option<Entry>) {
        if self.row_of(id).is_none() {
            if self.chan_list.filter.is_active() {
                self.chan_list.filter.clear();
                self.refilter_channels();
            }
            if self.row_of(id).is_none() {
                // only a group this scope lists (another playlist's group would show nothing)
                let listed = |e: &Entry| self.groups.iter().any(|g| &g.entry == e);
                let entry =
                    prefer.filter(listed).or_else(|| ctx.lib.live.get(id).and_then(|c| c.group_ids.first()).map(|g| Entry::Group(*g)).filter(listed));
                if let Some(e) = entry {
                    self.open_entry(ctx, e);
                }
            }
        }
        if let Some(i) = self.row_of(id) {
            self.chan_list.select(i);
        }
    }

    fn zap_for_open(&self, ctx: &UiCtx<'_>, id: ChannelId) -> Option<ZapList> {
        match self.open_collection() {
            Some(c) => zap_list_in(ctx, &c, &Self::scope(ctx).filter()),
            None => primary_zap(ctx, id),
        }
    }

    fn activate(&mut self, ctx: &mut UiCtx<'_>, fullscreen: bool) {
        let Some(id) = self.cursor_channel() else { return };
        self.rest = None;
        let zap = self.zap_for_open(ctx, id);
        let preview = Self::preview_visible(ctx.layout) && !fullscreen;
        let retry = if fullscreen { Action::ActivateAlt } else { Action::Activate };
        if fullscreen && playing_channel(ctx.player) == Some(id) {
            ctx.push(Route::Player);
            return;
        }
        // explicit plays are never "preview autoplay" (autoplay may only replace its own)
        activate_channel(ctx, id, zap, PlayOrigin::Fullscreen, preview, retry);
    }

    fn focus_groups(&mut self, layout: LayoutClass) {
        self.focus = Pane::Groups;
        self.drawer = layout != LayoutClass::Wide;
        if let Some(i) = self.open_index().and_then(|gi| self.group_rows.iter().position(|&r| r as usize == gi)) {
            self.group_list.select(i);
        }
    }

    fn close_drawer(&mut self) {
        self.drawer = false;
        self.focus = Pane::Channels;
    }

    // ------------------------------------------------------------------ actions: groups

    fn groups_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        match self.group_list.handle(action) {
            ListOutcome::Moved => true,
            ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                let Some(g) = self.cursor_group() else { return true };
                if g.group.locked && !ctx.parental.is_unlocked(&ctx.config.parental) {
                    let reason = format!("“{}” is locked", g.group.display_name());
                    ctx.open(OverlayRequest::Pin { then: Box::new(Action::Activate), reason });
                    return true;
                }
                let e = g.entry.clone();
                self.open_entry(ctx, e);
                self.close_drawer();
                true
            }
            ListOutcome::Parent => true,
            ListOutcome::FilterChanged => {
                self.refilter_groups();
                true
            }
            ListOutcome::ContextMenu(_) => {
                self.group_menu(ctx);
                true
            }
            ListOutcome::None => match action {
                Action::FocusNext | Action::FocusPrev => {
                    self.close_drawer();
                    true
                }
                Action::Back if self.drawer => {
                    self.close_drawer();
                    true
                }
                Action::Rename => {
                    self.rename_group(ctx);
                    true
                }
                Action::ToggleHidden => {
                    // the marked groups, else the focused one
                    if !self.marks.hide_marked(ctx)
                        && let Some(g) = self.cursor_provider_group(ctx)
                    {
                        group_marks::hide(ctx, group_marks::Noun::Group, vec![crate::model::GroupRef::from(&g)], Some(g.display_name()));
                    }
                    true
                }
                Action::ToggleMark => {
                    match self.cursor_provider_group(ctx) {
                        Some(g) => {
                            self.marks.toggle(&g);
                            self.group_list.handle(Action::Down);
                        }
                        None if self.cursor_group().is_some() => ctx.toast(Toast::info("Only playlist groups can be marked")),
                        None => {}
                    }
                    true
                }
                Action::HideShown => {
                    // the row's group carries the source, kind and key the hide needs
                    let shown: Vec<&Group> = self.matching.iter().filter_map(|&r| self.groups.get(r as usize)).map(|g| &g.group).collect();
                    self.marks.hide_shown(ctx, self.group_list.filter.query(), &shown);
                    true
                }
                Action::ToggleLock => {
                    match self.cursor_group().map(|g| (matches!(g.entry, Entry::Group(_)), g.group.locked)) {
                        Some((true, locked)) => {
                            if ctx.parental.is_unlocked(&ctx.config.parental) {
                                if let Some(name) = self.group_override(ctx, move |o| o.locked = !locked) {
                                    let hint = pin_hint(ctx);
                                    ctx.toast(Toast::info(format!("{} “{name}”{hint}", if locked { "Unlocked" } else { "Locked" })));
                                }
                            } else {
                                ctx.open(OverlayRequest::Pin { then: Box::new(Action::ToggleLock), reason: "Enter your PIN to change locks".into() });
                            }
                        }
                        Some((false, _)) => ctx.toast(Toast::info("Only playlist groups can be locked")),
                        None => {}
                    }
                    true
                }
                Action::MoveUp | Action::MoveDown => {
                    match self.cursor_group().map(|g| g.entry.clone()) {
                        Some(Entry::FavList(_)) => self.move_list(ctx, action == Action::MoveUp),
                        Some(Entry::Group(id)) => self.move_group(ctx, id, action == Action::MoveUp),
                        Some(_) => ctx.toast(Toast::info("Built-in groups stay on top")),
                        None => {}
                    }
                    true
                }
                Action::Remove => {
                    self.delete_list(ctx);
                    true
                }
                Action::Up
                | Action::Down
                | Action::PageUp
                | Action::PageDown
                | Action::Home
                | Action::End
                | Action::Activate
                | Action::ActivateAlt => true,
                Action::FilterInput(_) | Action::FilterBackspace | Action::FilterClear | Action::FilterDeleteWord => true,
                _ => false,
            },
        }
    }

    fn group_menu(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(g) = self.cursor_group() else { return };
        let k = KeyContext::List;
        let items = match &g.entry {
            Entry::FavList(l) => {
                let deletable = ctx.favs.lists.iter().any(|x| x.id == *l && !x.is_default);
                vec![
                    menu_item(glyphs::TAG, "Rename list", Action::Rename, true, k),
                    menu_item(glyphs::CLOSE, "Delete list", Action::Remove, deletable, k),
                    menu_item(glyphs::REORDER, "Move up", Action::MoveUp, true, k),
                    menu_item(glyphs::REORDER, "Move down", Action::MoveDown, true, k),
                ]
            }
            Entry::Group(id) => {
                let mut v = vec![
                    menu_item(glyphs::TAG, "Rename group", Action::Rename, true, k),
                    if self.marks.is_empty() {
                        menu_item(glyphs::HIDDEN, "Hide group", Action::ToggleHidden, true, k)
                    } else {
                        menu_item(glyphs::HIDDEN, "Hide the marked groups", Action::ToggleHidden, true, k)
                    },
                    menu_item(glyphs::CHECK, if self.marks.is_marked(*id) { "Unmark" } else { "Mark for hiding" }, Action::ToggleMark, true, k),
                ];
                if self.group_list.filter.is_active() {
                    let n = self.matching.len();
                    v.push(menu_item(glyphs::HIDDEN, format!("Hide these {n} groups"), Action::HideShown, n > 0, KeyContext::ListFiltering));
                }
                // the category filter of the group's playlist (DESIGN §28.6)
                if crate::ui::overlays::categories::multi_tag_sources(ctx.lib).contains(&g.group.source_id) {
                    v.push(MenuItem {
                        icon: glyphs::EARTH,
                        label: "Countries & languages…".into(),
                        action: Action::Digit(MENU_COUNTRIES),
                        enabled: true,
                        shortcut: None,
                    });
                }
                v.extend([
                    menu_item(glyphs::LOCK, if g.group.locked { "Unlock group" } else { "Lock group" }, Action::ToggleLock, true, k),
                    menu_item(glyphs::REORDER, "Move up", Action::MoveUp, true, k),
                    menu_item(glyphs::REORDER, "Move down", Action::MoveDown, true, k),
                ]);
                v
            }
            _ => return,
        };
        let title = g.group.display_name().to_owned();
        self.menu = Some(Menu::Context);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    fn rename_group(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(g) = self.cursor_group() else { return };
        let spec = match &g.entry {
            Entry::FavList(l) => {
                let name = ctx.favs.lists.iter().find(|x| x.id == *l).map(|x| x.name.clone()).unwrap_or_default();
                RenameSpec { title: "Rename list".into(), initial: name, target: RenameTarget::FavList(*l) }
            }
            Entry::Group(id) => match ctx.lib.live.groups.iter().find(|x| x.id == *id) {
                Some(gr) => RenameSpec {
                    title: "Rename group".into(),
                    initial: gr.display_name().to_owned(),
                    target: RenameTarget::Group { source: gr.source_id, kind: gr.kind, key: gr.key.clone() },
                },
                None => return,
            },
            _ => return,
        };
        ctx.open(OverlayRequest::Rename(spec));
    }

    /// Change the cursor group's override; returns its display name (for the toast).
    fn group_override(&mut self, ctx: &mut UiCtx<'_>, f: impl FnOnce(&mut crate::model::GroupOverride) + Send + 'static) -> Option<String> {
        let Some(Entry::Group(id)) = self.cursor_group().map(|g| g.entry.clone()) else { return None };
        let gr = ctx.lib.live.groups.iter().find(|x| x.id == id).cloned()?;
        let (source, kind, key) = (gr.source_id, gr.kind, gr.key.clone());
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let mut ov = crate::store::user::group_overrides(c)?
                .into_iter()
                .find(|r| r.source == source && r.kind == kind && r.key == key)
                .map(|r| r.ov)
                .unwrap_or_default();
            f(&mut ov);
            crate::store::user::set_group_override(c, source, kind, &key, &ov)
        });
        Some(gr.display_name().to_owned())
    }

    fn move_list(&mut self, ctx: &mut UiCtx<'_>, up: bool) {
        let Some(Entry::FavList(id)) = self.cursor_group().map(|g| g.entry.clone()) else { return };
        let mut order: Vec<FavListId> = ctx.favs.lists.iter().map(|l| l.id).collect();
        let Some(i) = order.iter().position(|l| *l == id) else { return };
        let j = if up { i.checked_sub(1) } else { (i + 1 < order.len()).then_some(i + 1) };
        let Some(j) = j else { return };
        order.swap(i, j);
        let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::reorder_fav_lists(c, &order));
    }

    /// Alt+↑/↓ on a provider group: move it among its playlist's groups
    /// (`group_override.user_order`); the cursor follows it once the library is rebuilt.
    fn move_group(&mut self, ctx: &mut UiCtx<'_>, id: GroupId, up: bool) {
        if self.group_list.filter.is_active() {
            ctx.toast(Toast::info("Clear the filter to reorder"));
            return;
        }
        let visible = ctx.lib.live.visible_groups(None, ctx.config.interface.hide_empty_groups);
        super::home::kit::reorder_group(&*ctx, &visible, id, if up { -1 } else { 1 });
    }

    fn delete_list(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(Entry::FavList(id)) = self.cursor_group().map(|g| g.entry.clone()) else { return };
        let Some(list) = ctx.favs.lists.iter().find(|l| l.id == id) else { return };
        if list.is_default {
            ctx.toast(Toast::info("The default Favourites list can't be deleted"));
            return;
        }
        let token = 0x11_0000 + id.0 as u64;
        self.confirm = Some((token, id));
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: format!("Delete “{}”?", list.name),
            body: "The channels stay in your playlists; only this list goes.".into(),
            confirm_label: "Delete".into(),
            destructive: true,
            token,
        }));
    }

    // ------------------------------------------------------------------ actions: channels

    fn channels_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        // the empty state's button, from the keyboard
        if self.channel_rows.is_empty() && action == Action::Activate && !self.chan_list.filter.is_active() && self.all_hidden(ctx) {
            ctx.push(Route::Settings(SettingsPage::Hidden));
            return true;
        }
        match self.chan_list.handle(action) {
            ListOutcome::Moved => {
                // keyboard rest starts the (opt-in) preview autoplay timer; hover never does
                self.rest = self.cursor_channel().map(|c| (c, Instant::now()));
                true
            }
            ListOutcome::Activated(_) => {
                self.activate(ctx, !Self::preview_visible(ctx.layout));
                true
            }
            ListOutcome::ActivatedAlt(_) => {
                self.activate(ctx, true);
                true
            }
            ListOutcome::Parent => {
                self.focus_groups(ctx.layout);
                true
            }
            ListOutcome::FilterChanged => {
                self.refilter_channels();
                true
            }
            ListOutcome::ContextMenu(_) => {
                self.channel_menu(ctx);
                true
            }
            ListOutcome::None => self.channel_command(ctx, action),
        }
    }

    /// The open provider group has channels, all of them hidden ("Everything here is hidden").
    fn all_hidden(&self, ctx: &UiCtx<'_>) -> bool {
        match &self.open {
            Some(Entry::Group(g)) => self.channels.is_empty() && ctx.lib.live.groups.iter().find(|x| x.id == *g).is_some_and(|x| x.item_count > 0),
            _ => false,
        }
    }

    fn channel_command(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let cur = self.cursor_channel().and_then(|id| ctx.lib.live.get(id).cloned());
        match action {
            Action::FocusNext | Action::FocusPrev => {
                self.focus_groups(ctx.layout);
                true
            }
            Action::Back if self.drawer => {
                self.close_drawer();
                true
            }
            Action::ToggleFavourite | Action::AddToList | Action::ToggleHidden | Action::Rename | Action::ToggleLock | Action::ShowDetails => {
                if let Some(ch) = cur {
                    channel_item_action(ctx, &ch, action);
                }
                true
            }
            Action::MoveUp | Action::MoveDown => {
                self.reorder(ctx, action == Action::MoveUp);
                true
            }
            Action::SortMenu => {
                let current = ctx.config.interface.live_sort;
                let items = SORTS
                    .iter()
                    .enumerate()
                    .map(|(i, (label, mode))| MenuItem {
                        icon: if *mode == current { glyphs::CHECK } else { "" },
                        label: (*label).into(),
                        action: choice_action(i),
                        enabled: true,
                        shortcut: None,
                    })
                    .collect();
                self.menu = Some(Menu::Sort);
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Sort channels".into(), items }));
                true
            }
            Action::OpenInGuide => {
                if let Some(ch) = cur {
                    ctx.push(Route::Guide { collection: self.open_collection(), focus: Some(ch.id) });
                }
                true
            }
            Action::AssignEpg => {
                if let Some(ch) = cur {
                    assign_epg(ctx, &mut self.guide, ch.id);
                }
                true
            }
            Action::Remove => {
                if let Some(ch) = cur {
                    self.remove(ctx, &ch);
                }
                true
            }
            Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::Activate | Action::ActivateAlt => {
                true
            }
            Action::FilterInput(_) | Action::FilterBackspace | Action::FilterClear | Action::FilterDeleteWord => true,
            _ => false,
        }
    }

    fn channel_menu(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(ch) = self.cursor_channel().and_then(|id| ctx.lib.live.get(id).cloned()) else { return };
        let k = KeyContext::List;
        let fav = ctx.favs.is_fav(&channel_ref(&ch));
        let mut items = vec![
            menu_item(glyphs::PLAY, "Play fullscreen", Action::ActivateAlt, !ch.unsupported, k),
            menu_item(
                if fav { glyphs::FAVOURITE_OFF } else { glyphs::FAVOURITE },
                if fav { "Remove from Favourites" } else { "Add to Favourites" },
                Action::ToggleFavourite,
                true,
                k,
            ),
            menu_item(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, true, k),
            menu_item(glyphs::GUIDE, "Show in TV guide", Action::OpenInGuide, true, k),
            menu_item(glyphs::INFO, "Edit information…", Action::ShowDetails, true, k),
            menu_item(glyphs::TAG, "Rename", Action::Rename, true, k),
            menu_item(glyphs::GUIDE, "Assign EPG channel…", Action::AssignEpg, true, k),
            menu_item(glyphs::HIDDEN, "Hide", Action::ToggleHidden, true, k),
            menu_item(glyphs::LOCK, if ch.locked { "Unlock" } else { "Lock with PIN" }, Action::ToggleLock, true, k),
            menu_item(glyphs::SORT, "Sort & view…", Action::SortMenu, true, k),
        ];
        if matches!(self.open, Some(Entry::FavList(_))) {
            items.push(menu_item(glyphs::CLOSE, "Remove from this list", Action::Remove, true, k));
        }
        if matches!(self.open, Some(Entry::Reminders)) {
            items.push(menu_item(glyphs::CLOSE, "Cancel reminder", Action::Remove, true, k));
        }
        self.menu = Some(Menu::Context);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: ch.name.to_string(), items }));
    }

    /// Alt+↑/↓: reorder inside a provider group (`item_override.user_order`) or a favourites
    /// list. A non-custom sort switches to custom order first (DESIGN §13.0) and the move then
    /// happens in that order: the cursor channel moves one place in the list the user is
    /// about to see.
    fn reorder(&mut self, ctx: &mut UiCtx<'_>, up: bool) {
        if self.chan_list.filter.is_active() {
            ctx.toast(Toast::info("Clear the filter to reorder"));
            return;
        }
        let Some(cur) = self.cursor_channel() else { return };
        let coll = match &self.open {
            Some(Entry::Group(g)) => Collection::Group(*g),
            Some(Entry::FavList(l)) => Collection::Favourites(*l),
            _ => {
                ctx.toast(Toast::info("This list can't be reordered"));
                return;
            }
        };
        if self.sort(ctx) != SortMode::Provider {
            ctx.edit_config(|c| c.interface.live_sort = SortMode::Provider);
            ctx.toast(Toast::info("Switched to custom order"));
            // the list below already is in custom order: neither this frame (old config) nor
            // the sort change may rebuild it from a library that doesn't carry the move yet —
            // the next move would save that order over this one
            self.sort_pending = Some((SortMode::Provider, ctx.config.interface.live_sort));
            self.built = Some(Self::signature(ctx, SortMode::Provider));
            self.channels = self.collection(ctx, &coll, SortMode::Provider);
            self.rebuild_haystacks(ctx);
        }
        // unfiltered and in custom order: the rows are `channels`
        if let Some(i) = self.channels.iter().position(|c| *c == cur) {
            let j = if up { i.checked_sub(1) } else { (i + 1 < self.channels.len()).then_some(i + 1) };
            if let Some(j) = j {
                self.write_order(ctx, &coll, i, j);
                self.channels.swap(i, j);
                self.haystacks.swap(i, j);
                self.refilter_channels();
            }
        }
        if let Some(r) = self.row_of(cur) {
            self.chan_list.select(r);
        }
    }

    /// Persist moving `channels[i]` to its neighbour `channels[j]` (called before the swap).
    fn write_order(&self, ctx: &mut UiCtx<'_>, coll: &Collection, i: usize, j: usize) {
        let live = &ctx.lib.live;
        match coll {
            Collection::Group(_) => {
                let Some(src) = live.get(self.channels[i]).map(|c| c.source_id) else { return };
                let mut order = self.channels.clone();
                order.swap(i, j);
                let keys: Vec<crate::model::ItemKey> =
                    order.iter().filter_map(|c| live.get(*c)).filter(|c| c.source_id == src).map(|c| c.key.clone()).collect();
                let _ = ctx
                    .services
                    .store
                    .writer()
                    .submit_notify(DataScope::Overrides, move |c| crate::store::user::reorder_items(c, src, ItemKind::Channel, &keys));
            }
            Collection::Favourites(l) => {
                let (Some(a), Some(b)) = (live.get(self.channels[i]), live.get(self.channels[j])) else { return };
                let (item, other) = (channel_ref(a), channel_ref(b));
                // positions in the list itself (it may hold hidden or vanished items)
                let items = ctx.favs.items(*l);
                let Some(target) = items.iter().position(|x| *x == other) else { return };
                if !items.contains(&item) {
                    return;
                }
                let l = *l;
                let _ = ctx
                    .services
                    .store
                    .writer()
                    .submit_notify(DataScope::Favourites, move |c| crate::store::user::move_favourite(c, l, &item, target));
            }
            _ => {}
        }
    }

    fn remove(&mut self, ctx: &mut UiCtx<'_>, ch: &ChannelSummary) {
        match self.open.clone() {
            Some(Entry::FavList(l)) => {
                let item = channel_ref(ch);
                let now = ctx.now;
                let it = item.clone();
                let _ = ctx
                    .services
                    .store
                    .writer()
                    .submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, l, &it, false, now));
                ctx.favs.apply_toggle(l, &item, false);
                let list = ctx.favs.lists.iter().find(|x| x.id == l).map(|x| x.name.clone()).unwrap_or_default();
                ctx.toast(Toast::info(format!("Removed “{}” from {list}", ch.name)));
            }
            Some(Entry::Reminders) => {
                if let Some(r) = self.cursor_reminder().filter(|r| r.channel == channel_ref(ch)) {
                    let id = r.id;
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id));
                    ctx.toast(Toast::info(format!("Reminder cancelled: {}", r.programme_title)));
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ drawing

    fn draw_groups(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, focused: bool) {
        let theme = ctx.theme;
        text_field::search_header(ui, theme, self.group_list.filter.query(), "Groups", Some(glyphs::SEARCH));
        ui.add_space(theme.space(SpaceToken::Sm));
        if self.group_rows.is_empty() {
            let q = self.group_list.filter.query();
            if !q.is_empty() {
                empty_state(ui, theme, glyphs::SEARCH, &format!("No matches for “{q}”"), None, None);
            }
            return;
        }
        let row_h = row_height(theme, ctx.config.interface.density, false);
        let open = self.open_index();
        let virtual_count = self.groups.iter().take_while(|g| !matches!(g.entry, Entry::Group(_))).count();
        let footer = self.marks.footer(self.group_list.filter.query(), self.matching.len());
        let sources = Self::scope(ctx).marks(ctx);
        let (groups, rows, marks) = (&self.groups, &self.group_rows, &self.marks);
        let mut clicked = None;
        let list = &mut self.group_list;
        let out = group_marks::with_footer(ui, theme, footer.as_deref(), |ui| {
            virtual_list(ui, theme, "live-groups", list, row_h, |ui, rc| {
                let gi = rows[rc.index] as usize;
                let g = &groups[gi];
                let state = RowState::from_flags(focused && rc.state.is_cursor(), open == Some(gi));
                let marked = matches!(g.entry, Entry::Group(id) if marks.is_marked(id));
                let source = g.source.and_then(|s| sources.of(s));
                let resp = group_row_decor(ui, theme, &g.group, state, GroupDecor { marked, tag: g.tag.as_deref(), source });
                if gi + 1 == virtual_count {
                    let r = resp.rect;
                    ui.painter().hline(r.x_range(), r.bottom() - 0.5, egui::Stroke::new(1.0, theme.tokens.group_divider));
                }
                // the list reports `Moved` only when the cursor wasn't there yet, and a hover
                // with real pointer motion has already put it there before the click
                if resp.clicked() {
                    clicked = Some(rc.index);
                }
                resp
            })
        });
        if let Some(i) = clicked.filter(|_| !matches!(out, ListOutcome::ContextMenu(_))) {
            // a click (not a hover) focuses the pane and opens the group
            self.group_list.select(i);
            self.focus = Pane::Groups;
            self.groups_action(ctx, Action::Activate);
            return;
        }
        match out {
            ListOutcome::Activated(_) => {
                self.focus = Pane::Groups;
                self.groups_action(ctx, Action::Activate);
            }
            ListOutcome::ContextMenu(_) => {
                self.focus = Pane::Groups;
                self.group_menu(ctx);
            }
            _ => {}
        }
    }

    fn draw_channels(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, focused: bool) {
        let theme = ctx.theme;
        let open = self.open_index().map(|i| &self.groups[i]);
        let mut group_name = open.map(|g| g.group.display_name().to_owned()).unwrap_or_else(|| "Channels".into());
        let scope = Self::scope(ctx);
        if scope.multi() {
            // the breadcrumb: which playlist, then the group (DESIGN §29); in All a provider
            // group is one playlist's, so it names that one too ("All playlists › Sports · TV 2")
            if let Some(src) = open.and_then(|g| g.source).and_then(|s| scope.marks(ctx).of(s)) {
                group_name = format!("{group_name} · {}", src.name);
            }
            group_name = format!("{} › {group_name}", scope.label());
        }
        let breadcrumb = ctx.layout != LayoutClass::Wide;
        ui.horizontal(|ui| {
            if breadcrumb {
                let b = crate::ui::widgets::button::button(ui, theme, None, &format!("{group_name} ▾"), Default::default());
                if b.clicked() {
                    self.focus_groups(ctx.layout);
                }
            }
            let placeholder = if breadcrumb { "Filter…".to_owned() } else { format!("{group_name}…") };
            text_field::search_header(ui, theme, self.chan_list.filter.query(), &placeholder, None);
        });
        ui.add_space(theme.space(SpaceToken::Sm));
        let density = ctx.config.interface.density;
        let row_h = row_height(theme, density, density == crate::config::Density::Comfortable);

        if ctx.lib.built_at == 0 && ctx.lib.live.channels.is_empty() {
            skeleton_rows(ui, theme, row_h);
            return;
        }
        if self.channel_rows.is_empty() {
            self.draw_empty_channels(ui, ctx);
            return;
        }
        if self.open == Some(Entry::Reminders) {
            self.draw_reminders(ui, ctx, focused, row_h);
            return;
        }
        let playing = playing_channel(ctx.player);
        // the first guide import says so instead of "No guide data" (DESIGN §13.12)
        let loading = self.guide.loading_label(ctx);
        let opts = ChannelRowOpts {
            density,
            show_number: ctx.config.interface.show_channel_numbers,
            show_logo: ctx.config.interface.show_logos,
            twelve_hour: !ctx.config.interface.clock_24h,
            no_guide_label: loading.as_deref(),
            ..Default::default()
        };
        let sources = scope.marks(ctx);
        let (channels, rows, failed) = (&self.channels, &self.channel_rows, &self.failed);
        let lib = ctx.lib.clone();
        let now = ctx.now;
        let mut clicked = false;
        let out = virtual_list(ui, theme, ("live-channels", self.open.clone()), &mut self.chan_list, row_h, |ui, rc| {
            let id = channels[rows[rc.index] as usize];
            let Some(ch) = lib.live.get(id) else { return ui.allocate_response(egui::vec2(ui.available_width(), row_h), egui::Sense::hover()) };
            let item = channel_ref(ch);
            let nn = ch.epg_channel_id.and_then(|epg| {
                let mut reader = crate::ui::data::DbEpg(ctx.db);
                ctx.now_next.get((epg, ch.tvg_shift_min), now, &mut reader, ctx.budget).cloned()
            });
            let o = ChannelRowOpts {
                source: sources.of(ch.source_id),
                favourite: ctx.favs.is_fav(&item),
                failed: failed.contains(&item),
                playing: playing == Some(id),
                quality: ctx.quality(ch),
                ..opts
            };
            let state = if focused {
                rc.state
            } else if rc.state.is_cursor() {
                RowState::Selected
            } else {
                RowState::Idle
            };
            let resp = channel_row(ui, theme, ctx.images, ch, nn.as_ref(), now, o, state);
            clicked |= resp.clicked();
            resp
        });
        if clicked {
            // a click (also on the row a hover already moved the cursor to) takes the focus
            self.focus = Pane::Channels;
        }
        match out {
            ListOutcome::Moved => {
                // pointer (hover/click): never autoplay
                self.rest = None;
            }
            ListOutcome::Activated(_) => {
                self.focus = Pane::Channels;
                self.activate(ctx, !Self::preview_visible(ctx.layout));
            }
            ListOutcome::ContextMenu(_) => {
                self.focus = Pane::Channels;
                self.channel_menu(ctx);
            }
            _ => {}
        }
    }

    fn draw_reminders(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, focused: bool, row_h: f32) {
        let theme = ctx.theme;
        let h24 = ctx.config.interface.clock_24h;
        let (channels, rows, reminders, reminder_rows) = (&self.channels, &self.channel_rows, &self.reminders, &self.reminder_rows);
        let lib = ctx.lib.clone();
        let sources = Self::scope(ctx).marks(ctx);
        virtual_list(ui, theme, "live-reminders", &mut self.chan_list, row_h, |ui, rc| {
            let row = rows[rc.index] as usize;
            let ch = lib.live.get(channels[row]);
            let r = reminder_rows.get(row).and_then(|i| reminders.get(*i));
            let label = match (ch, r) {
                (Some(c), Some(r)) => format!("{} · {}", r.programme_title, c.name),
                (Some(c), None) => c.name.to_string(),
                _ => String::new(),
            };
            let detail = r.map(|r| format!("{} {}", crate::ui::epg_grid::day_label(r.start), hhmm(r.start, h24)));
            let state = if focused {
                rc.state
            } else if rc.state.is_cursor() {
                RowState::Selected
            } else {
                RowState::Idle
            };
            let source = ch.and_then(|c| sources.of(c.source_id));
            menu_row_source(ui, theme, Some(glyphs::REMINDER), &label, detail.as_deref(), source, state)
        });
    }

    fn draw_empty_channels(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let q = self.chan_list.filter.query().to_owned();
        if !q.is_empty() {
            empty_state(ui, theme, glyphs::SEARCH, &format!("No matches for “{q}”"), None, None);
            return;
        }
        let (glyph, title, action): (&str, String, Option<&str>) = match &self.open {
            Some(Entry::FavList(_)) => (glyphs::FAVOURITE, "Press Ctrl+F on any channel to add it here".into(), None),
            Some(Entry::History) => (glyphs::HISTORY, "Nothing watched yet".into(), None),
            Some(Entry::Reminders) => (glyphs::REMINDER, "No reminders — press r on a programme in the TV guide".into(), None),
            Some(Entry::Group(_)) if self.all_hidden(ctx) => (glyphs::HIDDEN, "Everything here is hidden".into(), Some("Show hidden")),
            Some(Entry::Group(_)) => (glyphs::LIVE_TV, "This group is empty".into(), None),
            _ => (glyphs::LIVE_TV, "No channels".into(), None),
        };
        let detail = action.map(|_| "Enter opens Settings ▸ Hidden items");
        if empty_state(ui, theme, glyph, &title, detail, action).is_some_and(|r| r.clicked()) {
            ctx.push(Route::Settings(SettingsPage::Hidden));
        }
    }

    /// The preview pane: video + details of the cursor channel.
    fn draw_preview(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let rem = theme.rem();
        let width = ui.available_width();
        let video_rect = Rect::from_min_size(ui.cursor().min, egui::vec2(width, (width * 9.0 / 16.0).round()));
        let snap = ctx.player.clone();
        let cursor = self.cursor_channel();
        if snap.state != PlayerState::Idle {
            let resp = ctx.video.paint(ui, video_rect, Placement::Preview, theme, ctx.images, &snap);
            if resp.on_hover_text("Open the player").clicked() {
                ctx.push(Route::Player);
            }
            // the text below describes the cursor channel: say which one the video is
            if let Some(pc) = playing_channel(&snap).filter(|p| Some(*p) != cursor).and_then(|p| ctx.lib.live.get(p)) {
                let next = cursor.and_then(|c| ctx.lib.live.get(c)).map(|c| format!(" · Enter on {} switches", c.name)).unwrap_or_default();
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{} {}{next}", glyphs::PLAY, pc.name))
                            .size(theme.font(TypeToken::BodySmall))
                            .color(t.text_secondary),
                    )
                    .truncate(),
                );
            }
        } else {
            let r = ui.allocate_rect(video_rect, egui::Sense::hover()).rect;
            ui.painter().rect_filled(r, t.rounding, t.fill_field);
            let hint = if cursor.is_some() { "Enter plays here · Shift+Enter fullscreen" } else { "" };
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                hint,
                egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                t.text_secondary,
            );
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let Some(ch) = cursor.and_then(|id| ctx.lib.live.get(id).cloned()) else { return };
        let h24 = ctx.config.interface.clock_24h;
        // the preview always says it (stream info, like the player's OSD), whatever the list setting
        let quality = crate::ui::data::QualityBadges(Some(ctx.measured)).of(&ch);
        // name · number + badges
        ui.horizontal_wrapped(|ui| {
            let size = (theme.font(TypeToken::Heading) * 1.6).round();
            logo::logo(ui, theme, ctx.images, ch.logo.as_deref(), &ch.name, size);
            let mut title = ch.name.to_string();
            if let Some(n) = ch.number {
                title.push_str(&format!(" · {n}"));
            }
            ui.label(egui::RichText::new(title).size(theme.font(TypeToken::Heading)).color(t.fg));
            if let Some(q) = quality {
                badge::quality_badge(ui, theme, q, t.fg);
            }
            if ch.has_catchup && ch.catchup_days > 0 {
                badge::badge(ui, theme, &format!("{} {} d", glyphs::CATCHUP, ch.catchup_days), t.fg);
            }
            if ch.radio {
                badge::badge(ui, theme, "Radio", t.fg);
            }
            // the playlist, whenever two or more have channels (like a detail page)
            if let Some(src) = ctx.source_of(ItemKind::Channel, ch.source_id) {
                badge::source_badge(ui, theme, src, false);
            }
        });
        ui.add_space(theme.space(SpaceToken::Md));
        if ch.unsupported {
            ui.label(egui::RichText::new("DRM-protected — can't be played").size(theme.font(TypeToken::Body)).color(t.urgent));
            return;
        }
        if ch.epg_channel_id.is_none() {
            // say why (loading / no guide / not in the guide) and what to do (DESIGN §13.12)
            let (title, detail) = match self.guide.no_guide(ctx) {
                Some(reason) => reason.copy(),
                None => ("No guide data".to_owned(), Some("Ctrl+E assigns an EPG channel".to_owned())),
            };
            let loading = detail.is_none();
            ui.label(egui::RichText::new(title).size(theme.font(TypeToken::Body)).color(if loading { t.text_secondary } else { t.text_disabled }));
            if let Some(d) = detail {
                ui.add(egui::Label::new(egui::RichText::new(d).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).wrap());
            }
            return;
        }
        let nn = now_next_of(ctx, &ch);
        let now_prog = nn.as_ref().and_then(|n| n.now.clone());
        self.details.ensure(ctx, &ch, now_prog.as_ref());
        let line_h = ui.spacing().interact_size.y.max((theme.font(TypeToken::Title) * 1.4).round());
        match (&nn, &now_prog) {
            (None, _) => {
                // still loading: a skeleton bar (DESIGN §13.12)
                let (r, _) = ui.allocate_exact_size(egui::vec2((width * 0.6).round(), theme.font(TypeToken::Title).round()), egui::Sense::hover());
                ui.painter().rect_filled(r, t.rounding, t.fill_field);
            }
            (Some(_), Some(p)) => {
                // time on the right first, the title truncated into what is left
                ui.allocate_ui_with_layout(egui::vec2(width, line_h), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(time_range(p.start, p.stop, h24)).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Title)).color(t.fg)).truncate());
                    });
                });
                ui.horizontal(|ui| {
                    let w = (ui.available_width() * 0.6).max(60.0);
                    ui.allocate_ui(egui::vec2(w, (8.0 * rem).round()), |ui| {
                        progress::progress_bar(ui, theme, p.progress(ctx.now), (3.0 * rem).round())
                    });
                    ui.label(egui::RichText::new(time_left(p.stop - ctx.now)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                });
                if let Some(d) = self.details.description.as_deref().filter(|d| !d.is_empty()) {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    ui.add(egui::Label::new(egui::RichText::new(d).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
                }
            }
            (Some(_), None) => {
                ui.label(egui::RichText::new("No information for now").size(theme.font(TypeToken::Body)).color(t.text_disabled));
            }
        }
        if let Some(up) = self.details.upcoming.clone().filter(|v| !v.is_empty()) {
            ui.add_space(theme.space(SpaceToken::Lg));
            section::section_header(ui, theme, "Upcoming");
            let item = channel_ref(&ch);
            // a fixed time column ("20:00" / "10:00 pm"), titles truncated to the pane
            let time_w = (theme.font(TypeToken::Body) * 5.0).round();
            for p in up {
                let remind = self.reminders.iter().any(|r| r.channel == item && r.start == p.start);
                ui.allocate_ui_with_layout(egui::vec2(width, line_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.allocate_ui_with_layout(egui::vec2(time_w, line_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(time_w);
                        ui.label(egui::RichText::new(hhmm(p.start, h24)).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if remind {
                            ui.label(egui::RichText::new(glyphs::REMINDER).size(theme.font(TypeToken::Body)).color(t.accent));
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Body)).color(t.fg)).truncate());
                        });
                    });
                });
            }
        }
    }

    /// Opt-in preview autoplay after the keyboard cursor rested.
    fn autoplay(&mut self, ctx: &mut UiCtx<'_>, ui: &egui::Ui) {
        let Some((ch, at)) = self.rest else { return };
        let cfg = &ctx.config.interface;
        if !cfg.live_preview_autoplay || !Self::preview_visible(ctx.layout) || self.cursor_channel() != Some(ch) {
            self.rest = None;
            return;
        }
        let delay = Duration::from_millis(u64::from(cfg.preview_delay_ms));
        let elapsed = at.elapsed();
        if elapsed < delay {
            ui.ctx().request_repaint_after(delay - elapsed);
            return;
        }
        self.rest = None;
        let snap = ctx.player;
        // never replaces something the user started (only idle or its own autoplay)
        let allowed = snap.state == PlayerState::Idle || snap.origin == Some(PlayOrigin::LivePreview);
        let Some(c) = ctx.lib.live.get(ch) else { return };
        if !allowed || playing_channel(snap) == Some(ch) || c.unsupported || (c.locked && !ctx.parental.is_unlocked(&ctx.config.parental)) {
            return;
        }
        let zap = self.zap_for_open(ctx, ch);
        ctx.play(PlayerCommand::Play(PlayRequest {
            media: MediaRef::Channel(ch),
            zap,
            origin: PlayOrigin::LivePreview,
            resume: false,
            immediate: true,
        }));
    }
}

impl Screen for LiveScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Live
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        match self.focus {
            Pane::Groups => vec![self.group_list.key_context()],
            Pane::Channels => vec![self.chan_list.key_context()],
        }
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        self.refresh(ctx);
        let Route::Live { group, channel } = route else { return };
        if let Some(g) = group {
            self.open_entry(ctx, Entry::Group(*g));
            self.focus = Pane::Channels;
        }
        if let Some(c) = channel {
            self.select_channel(ctx, *c, None);
            self.focus = Pane::Channels;
        } else if let Some(id) = playing_channel(ctx.player) {
            // back from the player: follow the channel that was zapped to
            let prefer = ctx.player.zap.as_ref().and_then(|z| Entry::of(&z.collection));
            self.select_channel(ctx, id, prefer);
        }
    }

    fn on_event(&mut self, _ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        self.guide.on_event(ev);
        match ev {
            AppEvent::Data(DataScope::Reminders) => self.reminders_dirty = true,
            // a zap only changes the failure marks (its History write, like Overrides / Epg
            // changes, rebuilds the library, and a new library rebuilds the lists anyway)
            AppEvent::Player(PlayerEvent::Failed(_) | PlayerEvent::NowPlaying(_)) => self.failed_dirty = true,
            // a favourites move keeps the cheap signature: rebuild explicitly
            AppEvent::Data(DataScope::Favourites | DataScope::Live | DataScope::Sources | DataScope::All) => self.dirty = true,
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.refresh(ctx);
        if ctx.lib.built_at != 0 && ctx.lib.is_empty() {
            // "No playlists yet": Enter is the "Add playlist" button
            if action == Action::Activate {
                ctx.push(Route::Onboarding(crate::ui::OnboardingStart::AddSource));
                return true;
            }
            return false;
        }
        if self.layout == Some(LayoutClass::Wide) && self.drawer {
            self.drawer = false;
        }
        if scope::on_action(ctx, ScopeKind::Live, action) {
            return true;
        }
        // Esc ladder: the marks go first
        if action == Action::Back && self.marks.clear() {
            return true;
        }
        match self.focus {
            Pane::Groups => self.groups_action(ctx, action),
            Pane::Channels => self.channels_action(ctx, action),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.refresh(ctx);
        let theme = ctx.theme;
        let layout = ctx.layout;
        if self.layout != Some(layout) {
            // a layout change closes the drawer and keeps keyboard focus on a visible pane
            self.layout = Some(layout);
            if layout == LayoutClass::Wide {
                self.drawer = false;
            } else if !self.drawer {
                self.focus = Pane::Channels;
            }
        }
        if ctx.lib.built_at != 0 && ctx.lib.is_empty() {
            if empty_state(ui, theme, glyphs::PLAYLIST, "No playlists yet", Some(EMPTY_LIBRARY_DETAIL), Some("Add playlist"))
                .is_some_and(|r| r.clicked())
            {
                ctx.push(Route::Onboarding(crate::ui::OnboardingStart::AddSource));
            }
            return;
        }
        let scope = Self::scope(ctx);
        if scope.multi() {
            let counts: Vec<usize> = scope.choices().iter().map(|c| scope.count(ctx.lib, c.map(|s| s.id))).collect();
            let hint = shortcut(KeyContext::Global, Action::PlaylistPicker).map(|k| format!("{k} playlist")).unwrap_or_default();
            let click = scope_bar::scope_bar(ui, theme, &scope, &counts, layout == LayoutClass::Wide, &hint);
            scope_bar::paint_stripe(ui.painter(), theme, ui.max_rect().x_range(), ui.cursor().top(), &scope);
            ui.add_space(theme.space(SpaceToken::Md));
            match click {
                scope_bar::BarClick::Choose(c) => scope::choose(ctx, ScopeKind::Live, c, false),
                scope_bar::BarClick::Menu(id) => ctx.open(OverlayRequest::PlaylistMenu(id, ScopeKind::Live)),
                scope_bar::BarClick::Picker => ctx.open(OverlayRequest::PlaylistPicker(ScopeKind::Live)),
                scope_bar::BarClick::None => {}
            }
        }
        let full = ui.available_rect_before_wrap();
        let rem = theme.rem();
        let gap = theme.space(SpaceToken::PanelGap);
        let focus = self.focus;
        match layout {
            LayoutClass::Wide => {
                let groups_w = (240.0 * rem).round();
                let min_channels = 380.0 * rem;
                let preview_w = (full.width() * 0.38).max(420.0 * rem).min(full.width() - groups_w - min_channels - 2.0 * gap).max(0.0);
                let g = Rect::from_min_size(full.min, egui::vec2(groups_w, full.height()));
                let p = Rect::from_min_max(egui::pos2(full.right() - preview_w, full.top()), full.max);
                let c = Rect::from_min_max(egui::pos2(g.right() + gap, full.top()), egui::pos2(p.left() - gap, full.bottom()));
                ui.scope_builder(egui::UiBuilder::new().max_rect(g), |ui| self.draw_groups(ui, ctx, focus == Pane::Groups));
                section::pane_divider(ui.painter(), g.right() + gap / 2.0, full.y_range(), theme);
                ui.scope_builder(egui::UiBuilder::new().max_rect(c), |ui| self.draw_channels(ui, ctx, focus == Pane::Channels));
                if preview_w > 0.0 {
                    section::pane_divider(ui.painter(), p.left() - gap / 2.0, full.y_range(), theme);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(p), |ui| self.draw_preview(ui, ctx));
                }
            }
            LayoutClass::Medium => {
                let preview_w = (full.width() * 0.42).clamp(260.0 * rem, 520.0 * rem).min(full.width() * 0.5);
                let p = Rect::from_min_max(egui::pos2(full.right() - preview_w, full.top()), full.max);
                let c = Rect::from_min_max(full.min, egui::pos2(p.left() - gap, full.bottom()));
                ui.scope_builder(egui::UiBuilder::new().max_rect(c), |ui| self.draw_channels(ui, ctx, focus == Pane::Channels && !self.drawer));
                section::pane_divider(ui.painter(), p.left() - gap / 2.0, full.y_range(), theme);
                ui.scope_builder(egui::UiBuilder::new().max_rect(p), |ui| self.draw_preview(ui, ctx));
                if self.drawer {
                    // the groups drawer slides over the channel list
                    let d = Rect::from_min_size(full.min, egui::vec2((280.0 * rem).min(c.width()), full.height()));
                    ui.painter().rect_filled(d.expand(theme.space(SpaceToken::Sm)), 0.0, theme.tokens.bg);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(d), |ui| self.draw_groups(ui, ctx, true));
                    section::pane_divider(ui.painter(), d.right() + theme.space(SpaceToken::Sm), full.y_range(), theme);
                }
            }
            LayoutClass::Compact => {
                if self.drawer {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| self.draw_groups(ui, ctx, true));
                } else {
                    ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| self.draw_channels(ui, ctx, true));
                }
            }
        }
        self.autoplay(ctx, ui);
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Unlocked(action) => {
                self.on_action(ctx, action);
            }
            OverlayResult::Chosen(action) => match self.menu.take() {
                Some(Menu::Sort) => {
                    if let Some((_, mode)) = choice_index(action).and_then(|i| SORTS.get(i)) {
                        let mode = *mode;
                        ctx.edit_config(move |c| c.interface.live_sort = mode);
                    }
                }
                Some(Menu::Context) if action == Action::Digit(MENU_COUNTRIES) => {
                    if let Some(g) = self.cursor_provider_group(ctx) {
                        ctx.open(OverlayRequest::Categories(g.source_id));
                    }
                }
                _ => {
                    self.on_action(ctx, action);
                }
            },
            OverlayResult::Confirmed(token) if self.marks.confirmed(ctx, token) => {
                self.group_list.filter.clear();
                self.refilter_groups();
            }
            OverlayResult::Confirmed(token) => {
                if let Some((_, list)) = self.confirm.take().filter(|(t, _)| *t == token) {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::delete_fav_list(c, list));
                    if self.open == Some(Entry::FavList(list)) {
                        self.open = None;
                    }
                }
            }
            OverlayResult::Cancelled(token) => {
                self.marks.cancelled(token);
                self.confirm = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::home::testkit::{self, harness, has_label, has_label_containing, rig, seed};

    /// Several playlists with channels (DESIGN §29): in All, group and channel rows name their
    /// playlist (two "All" groups told apart), the breadcrumb of a provider group names its
    /// playlist, and a playlist whose "Show in All playlists" is off stays out of All channels
    /// and its groups but not out of your favourites; picking one playlist narrows the groups,
    /// All channels and the zap list to it (marks off inside it); another playlist's channel
    /// never opens a group this scope doesn't list; a playlist out of All still shows when
    /// picked.
    #[test]
    fn the_playlist_scope_narrows_groups_lists_and_zap_lists() {
        let (mut t, one) = rig();
        let (a_ch, b, free) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let free = testkit::source(c, "Free")?;
            let a_ch = testkit::channels(c, a, 3)?;
            testkit::channels(c, b, 2)?;
            testkit::channels(c, free, 4)?;
            c.execute(r#"UPDATE source SET settings = '{"in_all":false}' WHERE id = ?1"#, [free])?;
            let list = crate::store::user::default_fav_list(c)?;
            let fav = ItemRef { source: free, kind: ItemKind::Channel, key: crate::model::ItemKey("m3u:seed00000000000".into()) };
            crate::store::user::set_favourite(c, list, &fav, true, 1)?;
            Ok((a_ch[0], b, free))
        });
        let mut h = harness(t, one, LiveScreen::default(), egui::vec2(1600.0, 900.0));
        h.state_mut().enter(Route::Live { group: None, channel: None });
        h.run_steps(2);
        let zap_len = |h: &mut egui_kittest::Harness<'static, testkit::App<LiveScreen>>| {
            let app = h.state_mut();
            let (s, id) = (&app.s, app.s.channels[0]);
            app.t.with_ctx(|ctx| s.zap_for_open(ctx, id)).map_or(0, |z| z.channels.len())
        };
        // a favourites list opens first: Free's favourite is still yours in All
        assert!(matches!(h.state().s.open, Some(Entry::FavList(_))));
        assert_eq!(h.state().s.channels.len(), 1, "Free is out of All, not out of your favourites");
        assert_eq!(h.state().s.groups[0].group.item_count, 1);
        let open = |h: &mut egui_kittest::Harness<'static, testkit::App<LiveScreen>>, e: Entry| {
            let app = h.state_mut();
            let s = &mut app.s;
            app.t.with_ctx(|ctx| s.open_entry(ctx, e));
            h.run_steps(2);
        };
        open(&mut h, Entry::All);
        assert_eq!(h.state().s.channels.len(), 5, "Free is out of All");
        assert_eq!(zap_len(&mut h), 5, "the zap list too");
        assert!(has_label(&h, "All · Alpha") && has_label(&h, "All · Bravo"), "groups name their playlist");
        assert!(!has_label(&h, "All · Free"), "Free's groups are out of All");
        // channel rows paint the name, not just a glyph: Bravo's tab, its 2 groups, its 2 channels
        let painted = testkit::painted(&h);
        assert!(painted.iter().filter(|p| *p == "Bravo").count() >= 5, "{painted:?}");
        let news = h.state().s.groups.iter().rev().find(|g| g.group.display_name() == "News").map(|g| g.entry.clone()).unwrap();
        open(&mut h, news);
        assert!(has_label_containing(&h, "All playlists › News · Bravo"), "the breadcrumb names the group's playlist");
        open(&mut h, Entry::All);

        let pick = |h: &mut egui_kittest::Harness<'static, testkit::App<LiveScreen>>, id| {
            h.state_mut().t.config_edits.push(Box::new(move |c| c.interface.playlist_scope.live = Some(id)));
            h.state_mut().apply_config();
            h.run_steps(2);
        };
        pick(&mut h, b);
        assert_eq!(h.state().s.channels.len(), 2, "Bravo's channels");
        assert_eq!(zap_len(&mut h), 2);
        let provider_groups = |h: &egui_kittest::Harness<'static, testkit::App<LiveScreen>>| {
            h.state().s.groups.iter().filter(|g| matches!(g.entry, Entry::Group(_))).count()
        };
        assert_eq!(provider_groups(&h), 2, "Bravo's All and News");
        assert!(has_label(&h, "All") && !has_label(&h, "All · Bravo"), "no marks inside one playlist");
        // Alpha's channel (from Search, or what plays) never opens Alpha's group under "Bravo ›"
        h.state_mut().enter(Route::Live { group: None, channel: Some(a_ch) });
        h.run_steps(1);
        let s = &h.state().s;
        assert!(!s.channels.is_empty() && s.open_index().is_some(), "{:?}", s.open);
        pick(&mut h, free);
        assert_eq!(h.state().s.channels.len(), 4, "out of All, but its own tab shows it");
    }

    #[test]
    fn menu_choices_round_trip() {
        for i in [0usize, 1, 7, 4999] {
            assert_eq!(choice_index(choice_action(i)), Some(i));
        }
        assert_eq!(choice_index(Action::FilterInput('a')), None);
        assert_eq!(choice_index(Action::Up), None);
    }

    #[test]
    fn shortcuts_come_from_the_keymap() {
        assert_eq!(shortcut(KeyContext::List, Action::ToggleFavourite).as_deref(), Some("Ctrl+F"));
        assert_eq!(shortcut(KeyContext::Guide, Action::GuideRemind).as_deref(), Some("r"));
    }

    #[test]
    fn playing_channel_ignores_failed_and_idle() {
        let mut s = PlayerSnapshot::default();
        assert_eq!(playing_channel(&s), None);
        s.state = PlayerState::Playing;
        s.playable = Some(std::sync::Arc::new(crate::model::Playable {
            media: MediaRef::Channel(ChannelId(4)),
            kind: crate::model::PlayableKind::Live,
            item: None,
            source_id: None,
            title: "x".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: crate::redact::Secret::new(String::new()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        }));
        assert_eq!(playing_channel(&s), Some(ChannelId(4)));
        s.state = PlayerState::Error(crate::player::PlayerError { kind: crate::player::PlayerErrorKind::Network, message: String::new() });
        assert_eq!(playing_channel(&s), None);
    }
}
