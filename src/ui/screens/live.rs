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
//!
//! Also home of the playback helpers the other WP-11 views share (guide, player, programme
//! popover, edit information): the Enter rule, locked/DRM checks, zap lists, favourites and
//! reminder toggles.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{
    ChannelId, ChannelSummary, Collection, FavListId, Group, GroupId, GroupKey, ItemKind, ItemRef, MediaRef, PlayOrigin, Programme, ProgrammeSlim,
    Reminder, ReminderId, ReminderState, SortMode, SourceId, ZapList,
};
use crate::msg::{AppEvent, DataScope, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerEvent, PlayerSnapshot, PlayerState};
use crate::store::library::FavIndex;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::epg_grid::{hhmm, time_left, time_range};
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, ListPickerSpec, MenuItem, OverlayResult, RenameSpec, RenameTarget};
use crate::ui::video_host::Placement;
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::rows::{ChannelRowOpts, channel_row, group_row, menu_row, row_height};
use crate::ui::widgets::{RowState, badge, empty_state::empty_state, glyphs, logo, progress, section, text_field};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, SettingsPage, UiCtx};

// ====================================================================== shared helpers

/// The stable reference of a channel.
pub(crate) fn channel_ref(ch: &ChannelSummary) -> ItemRef { ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key.clone() } }

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
    let channels = ctx.lib.live.collection(collection, ctx.favs, &ctx.lib.recent_channels, ctx.config.interface.live_sort);
    (!channels.is_empty()).then(|| ZapList { collection: collection.clone(), channels: channels.into() })
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

/// Pending reminders (Settings ▸ Reminders query; a small table read on events, not per frame).
pub(crate) fn load_reminders(ctx: &UiCtx<'_>) -> Vec<Reminder> {
    crate::store::user::all_reminders(ctx.db).unwrap_or_else(|e| {
        tracing::debug!("reminders: {e}");
        Vec::new()
    })
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
pub(crate) fn choice_action(i: usize) -> Action { Action::FilterInput(char::from_u32(0xF0000 + i as u32).unwrap_or('\u{F0000}')) }

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
        p.rect_filled(Rect::from_min_size(egui::pos2(rect.left() + pad, y - 6.0), egui::vec2(w, 12.0)), theme.tokens.rounding, theme.tokens.fill_field);
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
    focus: Pane,
    /// Medium: the groups drawer is open; Compact: the groups list is shown.
    drawer: bool,
    /// Library snapshot + favourites signature the lists were built from.
    built: Option<(usize, u64, SortMode, bool)>,
    dirty: bool,
    /// Keyboard cursor rest (preview autoplay).
    rest: Option<(ChannelId, Instant)>,
    failed: HashSet<ItemRef>,
    details: ProgrammeDetails,
    menu: Option<Menu>,
    /// Delete-list confirmation token → list.
    confirm: Option<(u64, FavListId)>,
    layout: Option<LayoutClass>,
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
        locked: false,
        item_count: count as u32,
        provider_id: None,
    }
}

impl LiveScreen {
    fn preview_visible(layout: LayoutClass) -> bool { layout != LayoutClass::Compact }

    fn open_collection(&self) -> Option<Collection> { self.open.as_ref().and_then(Entry::collection) }

    fn open_index(&self) -> Option<usize> { self.open.as_ref().and_then(|e| self.groups.iter().position(|g| &g.entry == e)) }

    /// Rebuild when the library, the favourites, the sort or the hide-empty setting changed.
    fn refresh(&mut self, ctx: &mut UiCtx<'_>) {
        let sig = (Arc::as_ptr(ctx.lib) as usize, fav_signature(ctx.favs), ctx.config.interface.live_sort, ctx.config.interface.hide_empty_groups);
        if self.built == Some(sig) && !self.dirty {
            return;
        }
        let reload_side = self.dirty || self.built.is_none_or(|b| b.0 != sig.0);
        self.built = Some(sig);
        self.dirty = false;
        if reload_side {
            self.reminders = load_reminders(ctx);
            self.failed = crate::store::live::failures(ctx.db, ctx.now - 86_400)
                .map(|m| m.into_iter().filter(|(_, f)| f.count >= 2).map(|(k, _)| k).collect())
                .unwrap_or_default();
        }
        self.rebuild_groups(ctx);
        if self.open.as_ref().is_none_or(|e| !self.groups.iter().any(|g| &g.entry == e)) {
            self.open = self.default_entry(ctx);
        }
        self.rebuild_channels(ctx, false);
    }

    fn default_entry(&self, ctx: &UiCtx<'_>) -> Option<Entry> {
        ctx.favs.non_empty_lists().next().map(|l| Entry::FavList(l.id)).or_else(|| self.groups.iter().find(|g| g.entry == Entry::All).map(|g| g.entry.clone()))
    }

    fn rebuild_groups(&mut self, ctx: &UiCtx<'_>) {
        let live = &ctx.lib.live;
        let mut v = Vec::new();
        if !ctx.lib.is_empty() || !live.channels.is_empty() {
            for l in &ctx.favs.lists {
                let name = format!("{} {}", glyphs::FAVOURITE, l.name);
                v.push(GroupItem { entry: Entry::FavList(l.id), group: synthetic_group(name.clone(), ctx.favs.items(l.id).len()), name });
            }
            let name = format!("{} History", glyphs::HISTORY);
            v.push(GroupItem { entry: Entry::History, group: synthetic_group(name.clone(), ctx.lib.recent_channels.len()), name });
            if !self.reminders.is_empty() {
                let name = format!("{} Reminders", glyphs::REMINDER);
                v.push(GroupItem { entry: Entry::Reminders, group: synthetic_group(name.clone(), self.reminders.len()), name });
            }
            let visible = live.channels.len().saturating_sub(live.hidden.len());
            v.push(GroupItem { entry: Entry::All, group: synthetic_group("All channels".into(), visible), name: "All channels".into() });
            let multi = ctx.lib.sources.len() > 1;
            for g in live.visible_groups(None, ctx.config.interface.hide_empty_groups) {
                let mut group = g.clone();
                group.item_count = live.group_members(g.id).len() as u32;
                let mut name = g.display_name().to_owned();
                if multi && let Some(s) = ctx.lib.source(g.source_id) {
                    name = format!("{name} · {}", s.name);
                    group.rename = Some(name.clone());
                }
                v.push(GroupItem { entry: Entry::Group(g.id), group, name });
            }
        }
        self.groups = v;
        self.refilter_groups();
    }

    fn refilter_groups(&mut self) {
        self.group_rows = self.group_list.filter.apply(self.groups.iter().map(|g| g.name.as_str()));
        self.group_list.set_len(self.group_rows.len());
    }

    /// Channels of the open group (sorted like the zap order) + filter haystacks.
    fn rebuild_channels(&mut self, ctx: &UiCtx<'_>, reset_cursor: bool) {
        let keep = self.cursor_channel();
        self.channels = match &self.open {
            Some(Entry::Reminders) => {
                let mut seen = HashSet::new();
                self.reminders.iter().filter_map(|r| ctx.lib.live.by_ref(&r.channel)).filter(|c| seen.insert(*c)).collect()
            }
            Some(e) => e.collection().map(|c| ctx.lib.live.collection(&c, ctx.favs, &ctx.lib.recent_channels, ctx.config.interface.live_sort)).unwrap_or_default(),
            None => Vec::new(),
        };
        let live = &ctx.lib.live;
        self.haystacks = self
            .channels
            .iter()
            .map(|id| match live.get(*id) {
                Some(c) => match c.number {
                    Some(n) => format!("{n} {}", c.name),
                    None => c.name.to_string(),
                },
                None => String::new(),
            })
            .collect();
        self.refilter_channels();
        match keep.and_then(|k| self.row_of(k)).filter(|_| !reset_cursor) {
            Some(i) => self.chan_list.select(i),
            None if reset_cursor => self.chan_list.select(0),
            None => {}
        }
    }

    fn refilter_channels(&mut self) {
        self.channel_rows = self.chan_list.filter.apply(self.haystacks.iter().map(String::as_str));
        self.chan_list.set_len(self.channel_rows.len());
    }

    fn cursor_channel(&self) -> Option<ChannelId> {
        self.channel_rows.get(self.chan_list.cursor).and_then(|&i| self.channels.get(i as usize)).copied()
    }

    fn row_of(&self, id: ChannelId) -> Option<usize> { self.channel_rows.iter().position(|&i| self.channels.get(i as usize) == Some(&id)) }

    fn cursor_group(&self) -> Option<&GroupItem> { self.group_rows.get(self.group_list.cursor).and_then(|&i| self.groups.get(i as usize)) }

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
                let entry = prefer.filter(|e| self.groups.iter().any(|g| &g.entry == e)).or_else(|| ctx.lib.live.get(id).and_then(|c| c.group_ids.first()).map(|g| Entry::Group(*g)));
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
            Some(c) => zap_list(ctx, &c),
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
                    self.group_override(ctx, |o| o.hidden = !o.hidden, "Hidden");
                    true
                }
                Action::ToggleLock => {
                    if self.cursor_group().is_some_and(|g| matches!(g.entry, Entry::Group(_))) {
                        if ctx.parental.is_unlocked(&ctx.config.parental) {
                            self.group_override(ctx, |o| o.locked = !o.locked, "Lock changed");
                        } else {
                            ctx.open(OverlayRequest::Pin { then: Box::new(Action::ToggleLock), reason: "Enter your PIN to change locks".into() });
                        }
                    }
                    true
                }
                Action::MoveUp | Action::MoveDown => {
                    self.move_list(ctx, action == Action::MoveUp);
                    true
                }
                Action::Remove => {
                    self.delete_list(ctx);
                    true
                }
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::Activate | Action::ActivateAlt => true,
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
            Entry::Group(_) => vec![
                menu_item(glyphs::TAG, "Rename group", Action::Rename, true, k),
                menu_item(glyphs::HIDDEN, "Hide group", Action::ToggleHidden, true, k),
                menu_item(glyphs::LOCK, if g.group.locked { "Unlock group" } else { "Lock group" }, Action::ToggleLock, true, k),
            ],
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

    fn group_override(&mut self, ctx: &mut UiCtx<'_>, f: impl FnOnce(&mut crate::model::GroupOverride) + Send + 'static, what: &str) {
        let Some(Entry::Group(id)) = self.cursor_group().map(|g| g.entry.clone()) else { return };
        let Some(gr) = ctx.lib.live.groups.iter().find(|x| x.id == id).cloned() else { return };
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
        ctx.toast(Toast::info(format!("{what}: {}", gr.display_name())));
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
            Action::ToggleFavourite => {
                if let Some(ch) = cur {
                    toggle_favourite(ctx, channel_ref(&ch), &ch.name);
                }
                true
            }
            Action::AddToList => {
                if let Some(ch) = cur {
                    ctx.open(OverlayRequest::ListPicker(ListPickerSpec { item: channel_ref(&ch), title: format!("Lists for “{}”", ch.name) }));
                }
                true
            }
            Action::ToggleHidden => {
                if let Some(ch) = cur {
                    let item = channel_ref(&ch);
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
                        let mut ov = crate::store::user::item_override(c, &item)?.unwrap_or_default();
                        ov.hidden = !ov.hidden;
                        crate::store::user::set_item_override(c, &item, &ov)
                    });
                    ctx.toast(Toast::info(format!("Hid “{}” — Settings ▸ Hidden items brings it back", ch.name)));
                }
                true
            }
            Action::Rename => {
                if let Some(ch) = cur {
                    ctx.open(OverlayRequest::Rename(RenameSpec { title: "Rename channel".into(), initial: ch.name.to_string(), target: RenameTarget::Item { item: channel_ref(&ch) } }));
                }
                true
            }
            Action::ToggleLock => {
                if let Some(ch) = cur {
                    if !ctx.parental.is_unlocked(&ctx.config.parental) {
                        ctx.open(OverlayRequest::Pin { then: Box::new(Action::ToggleLock), reason: "Enter your PIN to change locks".into() });
                    } else {
                        let item = channel_ref(&ch);
                        let lock = !ch.locked;
                        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
                            let mut ov = crate::store::user::item_override(c, &item)?.unwrap_or_default();
                            ov.locked = lock;
                            crate::store::user::set_item_override(c, &item, &ov)
                        });
                        let hint = if ctx.config.parental.enabled && ctx.config.parental.pin_hash.is_some() { "" } else { " — set a PIN in Settings ▸ Parental to enforce it" };
                        ctx.toast(Toast::info(format!("{} “{}”{hint}", if lock { "Locked" } else { "Unlocked" }, ch.name)));
                    }
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
                    .map(|(i, (label, mode))| MenuItem { icon: if *mode == current { glyphs::CHECK } else { "" }, label: (*label).into(), action: choice_action(i), enabled: true, shortcut: None })
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
            Action::ShowDetails => {
                if let Some(ch) = cur {
                    ctx.open(OverlayRequest::EditItem(channel_ref(&ch)));
                }
                true
            }
            Action::AssignEpg => {
                if let Some(ch) = cur {
                    ctx.push(Route::EpgMapping { channel: Some(ch.id) });
                }
                true
            }
            Action::Remove => {
                if let Some(ch) = cur {
                    self.remove(ctx, &ch);
                }
                true
            }
            Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::Activate | Action::ActivateAlt => true,
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
            menu_item(if fav { glyphs::FAVOURITE_OFF } else { glyphs::FAVOURITE }, if fav { "Remove from Favourites" } else { "Add to Favourites" }, Action::ToggleFavourite, true, k),
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
    /// list; a non-custom sort switches to custom order first (DESIGN §13.0).
    fn reorder(&mut self, ctx: &mut UiCtx<'_>, up: bool) {
        if self.chan_list.filter.is_active() {
            ctx.toast(Toast::info("Clear the filter to reorder"));
            return;
        }
        // no filter: displayed rows are the channels in order
        let i = self.chan_list.cursor;
        if i >= self.channels.len() {
            return;
        }
        let j = if up { i.checked_sub(1) } else { (i + 1 < self.channels.len()).then_some(i + 1) };
        let Some(j) = j else { return };
        let switch = ctx.config.interface.live_sort != SortMode::Provider;
        match self.open.clone() {
            Some(Entry::Group(g)) => {
                if switch {
                    ctx.edit_config(|c| c.interface.live_sort = SortMode::Provider);
                    ctx.toast(Toast::info("Switched to custom order"));
                    // reorder from the provider order the list is about to show
                    self.channels = ctx.lib.live.group_members(g).to_vec();
                }
                let Some(src) = self.channels.first().and_then(|c| ctx.lib.live.get(*c)).map(|c| c.source_id) else { return };
                self.channels.swap(i, j);
                let keys: Vec<crate::model::ItemKey> = self.channels.iter().filter_map(|c| ctx.lib.live.get(*c)).filter(|c| c.source_id == src).map(|c| c.key.clone()).collect();
                let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::reorder_items(c, src, ItemKind::Channel, &keys));
            }
            Some(Entry::FavList(l)) => {
                let Some(ch) = ctx.lib.live.get(self.channels[i]).cloned() else { return };
                let item = channel_ref(&ch);
                // position in the list's own order (favourites keep list order)
                let Some(pos) = ctx.favs.items(l).iter().position(|x| *x == item) else { return };
                let target = if up { pos.saturating_sub(1) } else { pos + 1 };
                self.channels.swap(i, j);
                let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::move_favourite(c, l, &item, target));
            }
            _ => {
                ctx.toast(Toast::info("This list can't be reordered"));
                return;
            }
        }
        self.haystacks.swap(i, j);
        self.refilter_channels();
        self.chan_list.select(j);
    }

    fn remove(&mut self, ctx: &mut UiCtx<'_>, ch: &ChannelSummary) {
        match self.open.clone() {
            Some(Entry::FavList(l)) => {
                let item = channel_ref(ch);
                let now = ctx.now;
                let it = item.clone();
                let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, l, &it, false, now));
                ctx.favs.apply_toggle(l, &item, false);
                let list = ctx.favs.lists.iter().find(|x| x.id == l).map(|x| x.name.clone()).unwrap_or_default();
                ctx.toast(Toast::info(format!("Removed “{}” from {list}", ch.name)));
            }
            Some(Entry::Reminders) => {
                let item = channel_ref(ch);
                if let Some(r) = self.reminders.iter().find(|r| r.channel == item) {
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
        let (groups, rows) = (&self.groups, &self.group_rows);
        let clicked = ui.input(|i| i.pointer.any_click());
        let out = virtual_list(ui, theme, "live-groups", &mut self.group_list, row_h, |ui, rc| {
            let gi = rows[rc.index] as usize;
            let g = &groups[gi];
            let state = RowState::from_flags(focused && rc.state.is_cursor(), open == Some(gi));
            let resp = group_row(ui, theme, &g.group, state);
            if gi + 1 == virtual_count {
                let r = resp.rect;
                ui.painter().hline(r.x_range(), r.bottom() - 0.5, egui::Stroke::new(1.0, theme.tokens.group_divider));
            }
            resp
        });
        match out {
            // a click (not a hover) focuses the pane and opens the group
            ListOutcome::Moved if clicked => {
                self.focus = Pane::Groups;
                self.groups_action(ctx, Action::Activate);
            }
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
        let group_name = self.open_index().map(|i| self.groups[i].group.display_name().to_owned()).unwrap_or_else(|| "Channels".into());
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
        let opts = ChannelRowOpts {
            density,
            show_number: ctx.config.interface.show_channel_numbers,
            show_logo: ctx.config.interface.show_logos,
            ..Default::default()
        };
        let multi = ctx.lib.sources.len() > 1;
        let (channels, rows, failed) = (&self.channels, &self.channel_rows, &self.failed);
        let lib = ctx.lib.clone();
        let now = ctx.now;
        let clicked = ui.input(|i| i.pointer.any_click());
        let out = virtual_list(ui, theme, ("live-channels", self.open.clone()), &mut self.chan_list, row_h, |ui, rc| {
            let id = channels[rows[rc.index] as usize];
            let Some(ch) = lib.live.get(id) else { return ui.allocate_response(egui::vec2(ui.available_width(), row_h), egui::Sense::hover()) };
            let item = channel_ref(ch);
            let nn = ch.epg_channel_id.and_then(|epg| {
                let mut reader = crate::ui::data::DbEpg(ctx.db);
                ctx.now_next.get((epg, ch.tvg_shift_min), now, &mut reader, ctx.budget).cloned()
            });
            let o = ChannelRowOpts {
                source: if multi { lib.source(ch.source_id) } else { None },
                favourite: ctx.favs.is_fav(&item),
                failed: failed.contains(&item),
                playing: playing == Some(id),
                ..opts
            };
            let state = if focused { rc.state } else if rc.state.is_cursor() { RowState::Selected } else { RowState::Idle };
            channel_row(ui, theme, ctx.images, ch, nn.as_ref(), now, o, state)
        });
        match out {
            ListOutcome::Moved => {
                // pointer (hover/click): never autoplay; only a click takes the focus
                self.rest = None;
                if clicked {
                    self.focus = Pane::Channels;
                }
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
        let (channels, rows, reminders) = (&self.channels, &self.channel_rows, &self.reminders);
        let lib = ctx.lib.clone();
        virtual_list(ui, theme, "live-reminders", &mut self.chan_list, row_h, |ui, rc| {
            let id = channels[rows[rc.index] as usize];
            let ch = lib.live.get(id);
            let r = ch.and_then(|c| reminders.iter().find(|r| r.channel == channel_ref(c)));
            let label = match (ch, r) {
                (Some(c), Some(r)) => format!("{} · {}", r.programme_title, c.name),
                (Some(c), None) => c.name.to_string(),
                _ => String::new(),
            };
            let detail = r.map(|r| format!("{} {}", crate::ui::epg_grid::day_label(r.start), hhmm(r.start, h24)));
            let state = if focused { rc.state } else { RowState::Idle };
            menu_row(ui, theme, Some(glyphs::REMINDER), &label, detail.as_deref(), false, state)
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
            Some(Entry::Group(g)) => {
                let total = ctx.lib.live.groups.iter().find(|x| x.id == *g).map_or(0, |x| x.item_count);
                if total > 0 { (glyphs::HIDDEN, "Everything here is hidden".into(), Some("Show hidden")) } else { (glyphs::LIVE_TV, "This group is empty".into(), None) }
            }
            _ => (glyphs::LIVE_TV, "No channels".into(), None),
        };
        if empty_state(ui, theme, glyph, &title, None, action).is_some_and(|r| r.clicked()) {
            ctx.push(Route::Settings(SettingsPage::Hidden));
        }
    }

    /// The preview pane: video + details of the cursor channel.
    fn draw_preview(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let width = ui.available_width();
        let video_rect = Rect::from_min_size(ui.cursor().min, egui::vec2(width, (width * 9.0 / 16.0).round()));
        let snap = ctx.player.clone();
        if snap.state != PlayerState::Idle {
            let resp = ctx.video.paint(ui, video_rect, Placement::Preview, theme, &snap);
            if resp.on_hover_text("Open the player").clicked() {
                ctx.push(Route::Player);
            }
        } else {
            let r = ui.allocate_rect(video_rect, egui::Sense::hover()).rect;
            ui.painter().rect_filled(r, t.rounding, t.fill_field);
            let hint = if self.cursor_channel().is_some() { "Enter plays here · Shift+Enter fullscreen" } else { "" };
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, hint, egui::FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_secondary);
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let Some(ch) = self.cursor_channel().and_then(|id| ctx.lib.live.get(id).cloned()) else { return };
        let h24 = ctx.config.interface.clock_24h;
        // name · number + badges
        ui.horizontal_wrapped(|ui| {
            let size = (theme.font(TypeToken::Heading) * 1.6).round();
            logo::logo(ui, theme, ctx.images, ch.logo.as_deref(), &ch.name, size);
            let mut title = ch.name.to_string();
            if let Some(n) = ch.number {
                title.push_str(&format!(" · {n}"));
            }
            ui.label(egui::RichText::new(title).size(theme.font(TypeToken::Heading)).color(t.fg));
            if ch.has_catchup && ch.catchup_days > 0 {
                badge::badge(ui, theme, &format!("{} {} d", glyphs::CATCHUP, ch.catchup_days), t.fg);
            }
            if ch.radio {
                badge::badge(ui, theme, "Radio", t.fg);
            }
            if ctx.lib.sources.len() > 1
                && let Some(src) = ctx.lib.source(ch.source_id)
            {
                badge::source_badge(ui, theme, src, true);
            }
        });
        ui.add_space(theme.space(SpaceToken::Md));
        if ch.unsupported {
            ui.label(egui::RichText::new("DRM-protected — can't be played").size(theme.font(TypeToken::Body)).color(t.urgent));
            return;
        }
        if ch.epg_channel_id.is_none() {
            ui.label(egui::RichText::new("No guide data").size(theme.font(TypeToken::Body)).color(t.text_disabled));
            ui.label(egui::RichText::new("Ctrl+E assigns an EPG channel").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            return;
        }
        let nn = now_next_of(ctx, &ch);
        let now_prog = nn.as_ref().and_then(|n| n.now.clone());
        self.details.ensure(ctx, &ch, now_prog.as_ref());
        match (&nn, &now_prog) {
            (None, _) => {
                ui.label(egui::RichText::new("…").color(t.text_secondary));
            }
            (Some(_), Some(p)) => {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Title)).color(t.fg));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(time_range(p.start, p.stop, h24)).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    });
                });
                ui.horizontal(|ui| {
                    let w = (ui.available_width() * 0.6).max(60.0);
                    ui.allocate_ui(egui::vec2(w, 8.0), |ui| progress::progress_bar(ui, theme, p.progress(ctx.now), (3.0 * theme.rem()).round()));
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
            for p in up {
                let remind = self.reminders.iter().any(|r| r.channel == item && r.start == p.start);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(hhmm(p.start, h24)).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    ui.label(egui::RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Body)).color(t.fg));
                    if remind {
                        ui.label(egui::RichText::new(glyphs::REMINDER).size(theme.font(TypeToken::Body)).color(t.accent));
                    }
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
        ctx.play(PlayerCommand::Play(PlayRequest { media: MediaRef::Channel(ch), zap, origin: PlayOrigin::LivePreview, resume: false, immediate: true }));
    }
}

impl Screen for LiveScreen {
    fn id(&self) -> ScreenId { ScreenId::Live }

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
        match ev {
            AppEvent::Data(DataScope::Favourites | DataScope::Reminders | DataScope::History | DataScope::Epg | DataScope::Overrides) => self.dirty = true,
            AppEvent::Data(DataScope::Live | DataScope::Sources | DataScope::All) => self.dirty = true,
            AppEvent::Player(PlayerEvent::Failed(_) | PlayerEvent::NowPlaying(_)) => self.dirty = true,
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.refresh(ctx);
        if self.layout == Some(LayoutClass::Wide) && self.drawer {
            self.drawer = false;
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
            if empty_state(ui, theme, glyphs::PLAYLIST, "No playlists yet", Some("Add an M3U playlist or an Xtream login to start watching."), Some("Add playlist")).is_some_and(|r| r.clicked()) {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(crate::ui::OnboardingStart::AddSource)));
            }
            return;
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
                _ => {
                    self.on_action(ctx, action);
                }
            },
            OverlayResult::Confirmed(token) => {
                if let Some((_, list)) = self.confirm.take().filter(|(t, _)| *t == token) {
                    let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::delete_fav_list(c, list));
                    if self.open == Some(Entry::FavList(list)) {
                        self.open = None;
                    }
                }
            }
            OverlayResult::Cancelled(_) => self.confirm = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        s.playable = Some(Arc::new(crate::model::Playable {
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
