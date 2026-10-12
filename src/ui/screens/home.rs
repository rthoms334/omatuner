//! Home (DESIGN §13.3): rails — Continue watching (posters w/ progress), Favourite channels
//! (live now/next tiles; one rail per non-empty favourites list), Recently watched channels,
//! Recently added movies / series. Key context `[PosterGrid]` (2-D: ↑/↓ between rails, ←/→
//! within a rail; letters do NOT filter on Home — the screen ignores `FilterInput`). Enter on a
//! channel plays FULLSCREEN (no preview pane here; DESIGN §13 "Enter rule"), zap collection =
//! the favourites list of the rail / the channel's primary group. Fewer than 2 rails → a
//! "Get started" rail (Browse Live TV · Open guide · Add favourites (Ctrl+F) · first group's
//! channels with now/next). Owner: WP-12.
//!
//! Item shortcuts on the focused tile (all listed in Help for `[PosterGrid]`): Ctrl+F /
//! Ctrl+Shift+F favourites, Delete removes from the favourites list of the rail / from
//! Continue watching, Alt+↑/↓ moves a tile within its favourites list, Ctrl+W watched,
//! Ctrl+L lock, Ctrl+H hide, F2 rename, Ctrl+I details / edit information, Ctrl+G guide,
//! Ctrl+E assign EPG, Shift+F10 the menu of all of them; the rest toast "Not available here".
//! Rails are rebuilt only when the library, the favourites or Continue watching change.
//!
//! Copies of one title (DESIGN §29.14) are one tile wherever Home lists titles — Recently
//! added, Recently updated series, the favourites rails and Continue watching (the most recent
//! copy) — while Settings ▸ Interface ▸ "Same title in several playlists" is Combine. A stack
//! tile shows its copy count and playlists; Enter opens the page of the tile's copy (Copies
//! section there), Shift+Enter plays the copy that plays first, Ctrl+F follows the stack rules
//! (the copy that would play is added; a favourite stack leaves Favourites with every copy,
//! Undo).
//!
//! Data: everything but "Continue watching" comes from the in-memory `Library` and the
//! app-owned favourites; Continue watching is read off the UI thread ([`kit::Loader`]) on enter
//! and whenever progress/history/the library changes. An empty library (no sources) shows the
//! "No playlists yet" state with an "Add playlist" action (→ onboarding).
//!
//! This file also hosts [`kit`]: helpers shared by the WP-12 screens and overlays (background
//! reads, fullscreen playback with the right zap list, favourites, parental checks).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use eframe::egui::{self, Align2, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, Collection, FavListId, ItemKind, ItemRef, MediaRef, NowNext, PlayOrigin, SortMode, Timestamp, VodKind, VodSummary};
use crate::msg::{AppEvent, DataScope};
use crate::store::library::FavIndex;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::stacks as st;
use crate::ui::widgets::badge::{InlineBadge, name_with_quality};
use crate::ui::widgets::poster::{self, StackDecor, TileDecor};
use crate::ui::widgets::{badge, empty_state, glyphs, section};
use crate::ui::{OnboardingStart, OverlayRequest, Route, Screen, ScreenId, UiCtx};

use kit::Loader;

/// Shared helpers of the WP-12 screens and overlays.
pub(crate) mod kit {
    use crate::error::Result;
    use crate::keymap::Action;
    use crate::model::{ChannelId, Collection, FavListId, Group, GroupId, GroupOverride, ItemRef, MediaRef, PlayOrigin, ZapList};
    use crate::msg::{DataScope, Toast};
    use crate::player::{PlayRequest, PlayerCommand};
    use crate::ui::overlays::{ListPickerSpec, RenameSpec, RenameTarget};
    use crate::ui::{OverlayRequest, Route, UiCtx};

    /// A read on a fresh reader connection on the runtime's blocking pool (never on the UI
    /// thread); the UI polls [`Loader::poll`] every frame. Dropping it discards the result.
    pub(crate) struct Loader<T> {
        rx: tokio::sync::oneshot::Receiver<Result<T>>,
    }

    impl<T: Send + 'static> Loader<T> {
        pub(crate) fn start(ctx: &UiCtx<'_>, f: impl FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static) -> Self {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let store = ctx.services.store.clone();
            let bus = ctx.services.bus.clone();
            ctx.services.rt.spawn_blocking(move || {
                let r = store.reader().and_then(|c| f(&c));
                let _ = tx.send(r);
                bus.request_repaint();
            });
            Self { rx }
        }

        /// `Some` exactly once, when the read finished.
        pub(crate) fn poll(&mut self) -> Option<Result<T>> {
            match self.rx.try_recv() {
                Ok(r) => Some(r),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => Some(Err(crate::error::Error::other("background read failed"))),
            }
        }
    }

    /// Poll an optional loader: `Some(result)` once (and the slot is cleared).
    pub(crate) fn poll<T: Send + 'static>(slot: &mut Option<Loader<T>>) -> Option<Result<T>> {
        let r = slot.as_mut()?.poll();
        if r.is_some() {
            *slot = None;
        }
        r
    }

    /// The parental session is open (no PIN, disabled, or unlocked and not expired).
    pub(crate) fn unlocked(ctx: &UiCtx<'_>) -> bool {
        ctx.parental.is_unlocked(&ctx.config.parental)
    }

    /// A channel needs the PIN right now (it or one of its groups is locked).
    pub(crate) fn channel_locked(ctx: &UiCtx<'_>, id: ChannelId) -> bool {
        let Some(c) = ctx.lib.live.get(id) else { return false };
        let group_locked = c.group_ids.iter().any(|g| ctx.lib.live.groups.iter().any(|x| x.id == *g && x.locked));
        (c.locked || group_locked) && !unlocked(ctx)
    }

    /// Ask for the PIN; `then` comes back as `OverlayResult::Unlocked(then)`.
    pub(crate) fn ask_pin(ctx: &mut UiCtx<'_>, then: Action, reason: &str) {
        ctx.open(OverlayRequest::Pin { then: Box::new(then), reason: reason.to_owned() });
    }

    /// The zap list of a collection in the Live sort order (`None` when empty).
    pub(crate) fn zap_list(ctx: &UiCtx<'_>, collection: Collection) -> Option<ZapList> {
        let channels = ctx.lib.live.collection(&collection, ctx.favs, &ctx.lib.recent_channels, ctx.config.interface.live_sort);
        (!channels.is_empty()).then(|| ZapList { collection, channels: channels.into() })
    }

    /// Play a channel FULLSCREEN (DESIGN §13.0): zap through `collection` (a favourites list)
    /// or else the channel's primary group. Returns false (and plays nothing) when the channel
    /// is locked — the caller asks for the PIN.
    pub(crate) fn play_channel(ctx: &mut UiCtx<'_>, id: ChannelId, collection: Option<Collection>, origin: PlayOrigin) -> bool {
        if channel_locked(ctx, id) {
            return false;
        }
        let collection = collection.or_else(|| ctx.lib.live.get(id).and_then(|c| c.group_ids.first().copied()).map(Collection::Group));
        let zap = collection.and_then(|c| zap_list(ctx, c)).filter(|z| z.channels.contains(&id));
        ctx.play(PlayerCommand::Play(PlayRequest { media: MediaRef::Channel(id), zap, origin, resume: false, immediate: true }));
        ctx.push(Route::Player);
        true
    }

    /// Play VOD / catch-up fullscreen (`resume` = apply the saved position).
    pub(crate) fn play_media(ctx: &mut UiCtx<'_>, media: MediaRef, resume: bool, origin: PlayOrigin) {
        ctx.play(PlayerCommand::Play(PlayRequest { media, zap: None, origin, resume, immediate: true }));
        ctx.push(Route::Player);
    }

    /// Toggle membership in the default favourites list (Ctrl+F): written through the store,
    /// patched optimistically into the app-owned index.
    pub(crate) fn toggle_favourite(ctx: &mut UiCtx<'_>, item: &ItemRef) {
        let Some(list) = ctx.favs.default_list() else {
            ctx.toast(Toast::error("The Favourites list is missing"));
            return;
        };
        let on = !ctx.favs.is_fav(item);
        let (now, write_item) = (ctx.now, item.clone());
        let _ = ctx
            .services
            .store
            .writer()
            .submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, list, &write_item, on, now));
        ctx.favs.apply_toggle(list, item, on);
        ctx.toast(Toast::info(if on { "Added to Favourites" } else { "Removed from Favourites" }));
    }

    /// Locks can change: a PIN exists and the session is open (else a toast / the PIN prompt,
    /// answered with `OverlayResult::Unlocked(ToggleLock)`).
    pub(crate) fn may_change_locks(ctx: &mut UiCtx<'_>) -> bool {
        if ctx.config.parental.pin_hash.is_none() || !ctx.config.parental.enabled {
            ctx.toast(Toast::info("Set a PIN in Settings ▸ Parental first"));
            return false;
        }
        if !unlocked(ctx) {
            ask_pin(ctx, Action::ToggleLock, "Enter the PIN to change locks");
            return false;
        }
        true
    }

    /// One of the groups `ids` (looked up in `groups`) is locked.
    pub(crate) fn any_group_locked(groups: &[Group], ids: &[GroupId]) -> bool {
        ids.iter().any(|id| groups.iter().any(|g| g.id == *id && g.locked))
    }

    /// Ctrl+L on an item: lock / unlock it (needs a PIN; unlocking needs the open session).
    /// `locked_now` is the effective lock; when it comes from a locked category
    /// (`group_locked`) the item's own lock changes nothing, so the user is told instead.
    pub(crate) fn toggle_lock(ctx: &mut UiCtx<'_>, item: &ItemRef, locked_now: bool, group_locked: bool) {
        if !may_change_locks(ctx) {
            return;
        }
        if group_locked {
            ctx.toast(Toast::info("Locked by its category — unlock the category with Ctrl+L"));
            return;
        }
        let write_item = item.clone();
        let on = !locked_now;
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let mut ov = crate::store::user::item_override(c, &write_item)?.unwrap_or_default();
            ov.locked = on;
            crate::store::user::set_item_override(c, &write_item, &ov)
        });
        ctx.toast(Toast::info(if on { "Locked" } else { "Unlocked" }));
    }

    /// Change a group's override (read in the write, so fields this screen doesn't know
    /// about — `pinned_home` — survive).
    fn edit_group_override(ctx: &UiCtx<'_>, g: &Group, f: impl FnOnce(&mut GroupOverride) + Send + 'static) {
        let (source, kind, key) = (g.source_id, g.kind, g.key.clone());
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let mut ov = crate::store::user::group_overrides(c)?
                .into_iter()
                .find(|r| r.source == source && r.kind == kind && r.key == key)
                .map(|r| r.ov)
                .unwrap_or_default();
            f(&mut ov);
            crate::store::user::set_group_override(c, source, kind, &key, &ov)
        });
    }

    /// Ctrl+L on a category.
    pub(crate) fn toggle_group_lock(ctx: &mut UiCtx<'_>, g: &Group) {
        if !may_change_locks(ctx) {
            return;
        }
        let on = !g.locked;
        edit_group_override(ctx, g, move |ov| ov.locked = on);
        ctx.toast(Toast::info(if on { format!("Locked {}", g.display_name()) } else { format!("Unlocked {}", g.display_name()) }));
    }

    /// Ctrl+H on an item (Settings ▸ Hidden items shows it again).
    pub(crate) fn hide_item(ctx: &mut UiCtx<'_>, item: &ItemRef, name: &str) {
        let write_item = item.clone();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let mut ov = crate::store::user::item_override(c, &write_item)?.unwrap_or_default();
            ov.hidden = true;
            crate::store::user::set_item_override(c, &write_item, &ov)
        });
        ctx.toast(Toast::info(format!("Hid {name} — Settings ▸ Hidden items shows it again")));
    }

    /// Alt+↑/↓ on a category: move it among the categories of its playlist (`visible` in display
    /// order) and store the order as `user_order`. False when it can't move that way.
    pub(crate) fn reorder_group(ctx: &UiCtx<'_>, visible: &[&Group], id: GroupId, delta: isize) -> bool {
        let Some(g) = visible.iter().find(|g| g.id == id) else { return false };
        let mut order: Vec<&Group> = visible.iter().copied().filter(|x| x.source_id == g.source_id && x.kind == g.kind).collect();
        let Some(i) = order.iter().position(|x| x.id == id) else { return false };
        let j = i as isize + delta;
        if j < 0 || j as usize >= order.len() {
            return false;
        }
        order.swap(i, j as usize);
        let (source, kind) = (g.source_id, g.kind);
        let keys: Vec<crate::model::GroupKey> = order.iter().map(|x| x.key.clone()).collect();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| {
            let existing = crate::store::user::group_overrides(c)?;
            for (pos, key) in keys.iter().enumerate() {
                let mut ov =
                    existing.iter().find(|r| r.source == source && r.kind == kind && r.key == *key).map(|r| r.ov.clone()).unwrap_or_default();
                ov.user_order = Some(pos as i32);
                crate::store::user::set_group_override(c, source, kind, key, &ov)?;
            }
            Ok(())
        });
        true
    }

    /// Add to / remove from one favourites list (store + optimistic index patch).
    pub(crate) fn set_in_list(ctx: &mut UiCtx<'_>, list: FavListId, item: &ItemRef, on: bool) {
        let (now, write_item) = (ctx.now, item.clone());
        let _ = ctx
            .services
            .store
            .writer()
            .submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, list, &write_item, on, now));
        ctx.favs.apply_toggle(list, item, on);
    }

    /// Ctrl+W (`on`) / "remove from Continue watching" (`!on`, which forgets the progress).
    pub(crate) fn mark_watched(ctx: &mut UiCtx<'_>, item: &ItemRef, title: &str, on: bool) {
        let (now, items) = (ctx.now, vec![(item.clone(), title.to_owned())]);
        let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| crate::store::user::mark_watched(c, &items, on, now));
        ctx.toast(Toast::info(if on { "Marked as watched" } else { "Removed from Continue watching" }));
    }

    /// F2 on an item (the overlay writes the rename).
    pub(crate) fn rename_item(ctx: &mut UiCtx<'_>, item: &ItemRef, name: &str) {
        ctx.open(OverlayRequest::Rename(RenameSpec {
            title: "Rename".into(),
            initial: name.to_owned(),
            target: RenameTarget::Item { item: item.clone() },
        }));
    }

    /// F2 on a category.
    pub(crate) fn rename_group(ctx: &mut UiCtx<'_>, g: &Group) {
        let target = RenameTarget::Group { source: g.source_id, kind: g.kind, key: g.key.clone() };
        ctx.open(OverlayRequest::Rename(RenameSpec { title: "Rename category".into(), initial: g.display_name().to_owned(), target }));
    }

    /// Ctrl+Shift+F: the favourites-list picker (the overlay writes the membership).
    pub(crate) fn list_picker(ctx: &mut UiCtx<'_>, item: &ItemRef, title: &str) {
        ctx.open(OverlayRequest::ListPicker(ListPickerSpec { item: item.clone(), title: title.to_owned() }));
    }

    /// A shortcut from Help that has no meaning for what is focused.
    pub(crate) fn not_here(ctx: &mut UiCtx<'_>) {
        ctx.toast(Toast::info("Not available here"));
    }

    /// A palette colour used as TEXT on the background (source badges, the onboarding
    /// preview): the colour itself when it reads (≥ 3:1), else the foreground (yellow on a
    /// light theme).
    pub(crate) fn legible(theme: &crate::theme::Theme, color: eframe::egui::Color32) -> eframe::egui::Color32 {
        crate::ui::widgets::badge::legible(theme, color)
    }

    /// Set a reminder for a future programme on `channel` (times as displayed, i.e. shifted).
    pub(crate) fn set_reminder(ctx: &mut UiCtx<'_>, channel: ChannelId, title: &str, start: crate::model::Timestamp, stop: crate::model::Timestamp) {
        let Some(c) = ctx.lib.live.get(channel) else { return };
        let r = crate::model::Reminder {
            id: crate::model::ReminderId(0),
            channel: ItemRef { source: c.source_id, kind: crate::model::ItemKind::Channel, key: c.key.clone() },
            channel_name: c.name.to_string(),
            programme_title: title.to_owned(),
            start,
            stop,
            lead_s: i64::from(ctx.config.reminders.lead_min) * 60,
            state: crate::model::ReminderState::Pending,
            mode: Default::default(),
            offset_s: 0,
            open_in: None,
            rule_id: None,
        };
        let _ = ctx.services.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::add_reminder(c, &r).map(|_| ()));
        ctx.toast(Toast::info(format!("Reminder set: {title} at {}", hhmm(start, ctx.config.interface.clock_24h))));
    }

    /// Open a file dialog (portal) for `purpose`; the answer arrives as
    /// `PlatformEvent::FilePicked`. Unit tests record the request instead of opening a
    /// dialog on the desktop.
    pub(crate) fn pick_file(ctx: &UiCtx<'_>, purpose: crate::platform::PickPurpose) {
        #[cfg(not(test))]
        crate::platform::filepicker::pick(ctx.services.rt.handle(), ctx.services.bus.clone(), purpose);
        #[cfg(test)]
        {
            let _ = ctx;
            PICKED.with_borrow_mut(|v| v.push(purpose));
        }
    }

    #[cfg(test)]
    thread_local! {
        /// File dialogs requested by this test thread.
        pub(crate) static PICKED: std::cell::RefCell<Vec<crate::platform::PickPurpose>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// "1:02:13" / "41:20".
    pub(crate) fn clock(seconds: f64) -> String {
        let s = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
        let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
        if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
    }

    /// "2 h 16" / "42 min".
    pub(crate) fn runtime(seconds: u32) -> String {
        let m = (seconds + 30) / 60;
        if m >= 60 { format!("{} h {:02}", m / 60, m % 60) } else { format!("{m} min") }
    }

    /// Local "21:00" / "9:00 PM" and "Thu 21:00" (Settings ▸ Interface ▸ Clock).
    pub(crate) use crate::ui::widgets::time::{day_time, hhmm, local};

    /// Local "YYYY-MM-DD".
    pub(crate) fn date(ts: crate::model::Timestamp) -> String {
        local(ts, "%Y-%m-%d", "?")
    }

    /// "2 h ago" / "just now" / "3 d ago".
    pub(crate) fn ago(now: crate::model::Timestamp, then: crate::model::Timestamp) -> String {
        let d = (now - then).max(0);
        match d {
            0..=59 => "just now".into(),
            60..=3599 => format!("{} min ago", d / 60),
            3600..=172_799 => format!("{} h ago", d / 3600),
            _ => format!("{} d ago", d / 86_400),
        }
    }

    /// What VOD plays now, as a poster-grid identity: the movie, or the series of the playing
    /// episode (resolved once per episode with one point read in the frame budget). Drives
    /// the now-playing marker on poster tiles.
    #[derive(Default)]
    pub(crate) struct PlayingVod {
        episode: Option<(crate::model::EpisodeId, Option<crate::model::SeriesId>)>,
    }

    impl PlayingVod {
        pub(crate) fn get(&mut self, ctx: &mut UiCtx<'_>) -> Option<crate::model::VodKind> {
            use crate::model::VodKind;
            match ctx.player.playable.as_ref().map(|p| p.media.clone()) {
                Some(MediaRef::Movie(id)) => Some(VodKind::Movie(id)),
                Some(MediaRef::Episode(e)) => {
                    if self.episode.map(|x| x.0) != Some(e) {
                        let db = ctx.db;
                        // budget spent: try again next frame
                        let r = ctx.budget.run(|| crate::store::vod::episode(db, e))?;
                        self.episode = Some((e, r.ok().map(|ep| ep.series_id)));
                    }
                    self.episode.and_then(|x| x.1).map(VodKind::Series)
                }
                _ => None,
            }
        }
    }

    /// Show a (redacted) error toast.
    pub(crate) fn error_toast(ctx: &mut UiCtx<'_>, what: &str, e: &crate::error::Error) {
        ctx.toast(Toast::error(format!("{what}: {}", crate::redact::redact(&e.to_string()))));
    }
}

/// A continue-watching entry resolved off the UI thread.
#[derive(Debug, Clone, PartialEq)]
struct ContinueItem {
    item: ItemRef,
    media: MediaRef,
    /// Poster card stand-in (name = "Show — S02E05" for episodes).
    card: VodSummary,
    progress: f32,
    open: Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartAction {
    BrowseLive,
    OpenGuide,
    AddFavourites,
}

#[derive(Debug, Clone, PartialEq)]
enum Tile {
    /// A channel; `collection` = the favourites list it was shown in (else its primary group).
    Channel {
        id: ChannelId,
        collection: Option<Collection>,
    },
    Vod(VodKind),
    Continue(usize),
    Start(StartAction),
    /// Up next: carry on where it stopped, or start it (DESIGN §32).
    UpNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailKind {
    UpNext,
    Continue,
    Favourites,
    Recent,
    Movies,
    Series,
    GetStarted,
}

struct Rail {
    kind: RailKind,
    title: String,
    /// The favourites list of a favourites rail (Delete removes from it, Alt+←/→ reorders).
    list: Option<FavListId>,
    tiles: Vec<Tile>,
}

/// Items per rail.
const RAIL_MAX: usize = 30;
/// Channel tile width (pt × rem).
const CHANNEL_TILE_W: f32 = 230.0;
/// Poster width in rails (pt × rem).
const RAIL_POSTER_W: f32 = 120.0;

/// What the rails were built from: rebuilt only when one of these changes, never per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RailsKey {
    /// `Library::generation`: a new snapshot, including a zap's new recently watched list.
    generation: u64,
    favs: u64,
    continue_gen: u64,
    /// Copies of one title are one tile (`interface.same_title`).
    combine: bool,
    /// Up next's tile: (items to go, the row it resumes) while nothing plays.
    up_next: Option<(usize, Option<crate::model::QueueId>)>,
}

#[derive(Default)]
pub struct HomeScreen {
    /// (rail index, per-rail column).
    rail: usize,
    cols: Vec<(RailKind, String, usize)>,
    continue_items: Vec<ContinueItem>,
    /// Bumped whenever `continue_items` is replaced.
    continue_gen: u64,
    continue_loader: Option<Loader<Vec<ContinueItem>>>,
    /// Library build the "recently added" lists were taken from.
    recent_built_at: Timestamp,
    recent_movies: Vec<VodSummary>,
    recent_series: Vec<VodSummary>,
    /// (kind, source, key) → movie / series of the favourited titles (all lists), and the
    /// (VOD lists, favourites fingerprint) they were resolved for: a zap's library copy shares
    /// the lists and rescans nothing. Only favourites: a map of every title cloned every key
    /// (~40 MiB for 380k titles).
    vod_refs: HashMap<ItemRef, VodKind>,
    vod_refs_for: Option<(crate::store::library::VodIdentity, u64)>,
    /// The rails the user sees (actions resolve against them).
    rails: Vec<Rail>,
    rails_key: Option<RailsKey>,
    scroll_to_cursor: bool,
    playing: kit::PlayingVod,
}

/// A cheap fingerprint of the favourites lists (their order, names and items).
fn favs_signature(favs: &FavIndex) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    for l in &favs.lists {
        (l.id, &l.name, l.is_default).hash(&mut h);
        favs.items(l.id).hash(&mut h);
    }
    h.finish()
}

impl HomeScreen {
    fn reload_continue(&mut self, ctx: &UiCtx<'_>) {
        self.continue_loader = Some(Loader::start(ctx, load_continue));
    }

    /// Rebuild the rails when the library, the favourites or Continue watching changed.
    fn build_rails(&mut self, ctx: &mut UiCtx<'_>) {
        let combine = st::combine(ctx.config);
        let key = RailsKey {
            generation: ctx.lib.generation,
            favs: favs_signature(ctx.favs),
            continue_gen: self.continue_gen,
            combine,
            up_next: up_next_key(ctx),
        };
        if self.rails_key == Some(key) {
            return;
        }
        let old_key = self.rails_key.replace(key);
        if self.recent_built_at != ctx.lib.built_at || old_key.is_some_and(|k| k.combine != combine) {
            self.recent_built_at = ctx.lib.built_at;
            // one tile per stack, its top copy, where its newest copy comes (DESIGN §29.14)
            let take = |movies: bool, all: &[VodSummary]| -> Vec<VodSummary> {
                let stacks = ctx.lib.vod.stacks(movies);
                let mut seen = std::collections::HashSet::new();
                ctx.lib
                    .vod
                    .all_sorted(movies, SortMode::RecentlyAdded)
                    .into_iter()
                    .filter_map(|i| match stacks.stack_of(i).filter(|_| combine) {
                        Some(s) => seen.insert(s).then(|| stacks.top(s)),
                        None => Some(i),
                    })
                    .take(RAIL_MAX)
                    .filter_map(|i| all.get(i as usize).cloned())
                    .collect()
            };
            self.recent_movies = take(true, &ctx.lib.vod.movies);
            self.recent_series = take(false, &ctx.lib.vod.series);
        }
        let refs_for = (ctx.lib.vod_identity(), key.favs);
        if self.vod_refs_for.as_ref() != Some(&refs_for) {
            self.vod_refs_for = Some(refs_for);
            self.vod_refs = favourite_vod(ctx.lib, ctx.favs);
        }
        let mut rails = Vec::new();
        // Up next first while it has items to go and nothing plays (DESIGN §32)
        if key.up_next.is_some() {
            rails.push(Rail { kind: RailKind::UpNext, title: "Up next".into(), list: None, tiles: vec![Tile::UpNext] });
        }
        // a title's copies are one tile: the first (newest) of them in Continue watching, the
        // first in a favourites list
        let stack_key = |kind: VodKind| -> Option<(bool, crate::store::stacks::StackId)> {
            let movies = st::is_movie(kind);
            combine.then(|| st::index_of(ctx.lib, kind).and_then(|i| ctx.lib.vod.stacks(movies).stack_of(i)).map(|s| (movies, s))).flatten()
        };
        let mut seen = std::collections::HashSet::new();
        // read from the database, not the library: explicit titles parental control hides are
        // left out here (DESIGN §20.3)
        let continues: Vec<Tile> = (0..self.continue_items.len())
            .filter(|i| {
                let c = &self.continue_items[*i].card;
                let kind = if matches!(c.kind, VodKind::Movie(_)) { ItemKind::Movie } else { ItemKind::Series };
                !ctx.lib.conceals(&ItemRef { source: c.source_id, kind, key: c.key.clone() })
            })
            .filter(|i| stack_key(self.continue_items[*i].card.kind).is_none_or(|k| seen.insert(k)))
            .map(Tile::Continue)
            .collect();
        if !continues.is_empty() {
            rails.push(Rail { kind: RailKind::Continue, title: "Continue watching".into(), list: None, tiles: continues });
        }
        for list in ctx.favs.non_empty_lists() {
            let mut seen = std::collections::HashSet::new();
            let tiles: Vec<Tile> = ctx
                .favs
                .items(list.id)
                .iter()
                .filter_map(|r| match r.kind {
                    ItemKind::Channel => ctx
                        .lib
                        .live
                        .by_ref(r)
                        .filter(|id| !ctx.lib.live.hidden.contains(id))
                        .map(|id| Tile::Channel { id, collection: Some(Collection::Favourites(list.id)) }),
                    _ => self.vod_refs.get(r).copied().filter(|k| stack_key(*k).is_none_or(|s| seen.insert(s))).map(Tile::Vod),
                })
                .take(RAIL_MAX)
                .collect();
            if !tiles.is_empty() {
                // the default list holds channels and titles alike
                let title = if list.is_default { "Favourites".to_owned() } else { list.name.clone() };
                rails.push(Rail { kind: RailKind::Favourites, title, list: Some(list.id), tiles });
            }
        }
        let recent: Vec<Tile> = ctx
            .lib
            .recent_channels
            .iter()
            .filter(|id| ctx.lib.live.get(**id).is_some() && !ctx.lib.live.hidden.contains(id))
            .take(RAIL_MAX)
            .map(|id| Tile::Channel { id: *id, collection: None })
            .collect();
        if !recent.is_empty() {
            rails.push(Rail { kind: RailKind::Recent, title: "Recently watched".into(), list: None, tiles: recent });
        }
        if !self.recent_movies.is_empty() {
            rails.push(Rail {
                kind: RailKind::Movies,
                title: "Recently added movies".into(),
                list: None,
                tiles: self.recent_movies.iter().map(|v| Tile::Vod(v.kind)).collect(),
            });
        }
        if !self.recent_series.is_empty() {
            rails.push(Rail {
                kind: RailKind::Series,
                title: "Recently updated series".into(),
                list: None,
                tiles: self.recent_series.iter().map(|v| Tile::Vod(v.kind)).collect(),
            });
        }
        if rails.len() < 2 {
            let mut tiles = vec![Tile::Start(StartAction::BrowseLive), Tile::Start(StartAction::OpenGuide), Tile::Start(StartAction::AddFavourites)];
            if let Some(g) = ctx.lib.live.visible_groups(None, true).first() {
                tiles.extend(ctx.lib.live.group_members(g.id).iter().take(RAIL_MAX).map(|id| Tile::Channel { id: *id, collection: None }));
            }
            rails.push(Rail { kind: RailKind::GetStarted, title: "Get started".into(), list: None, tiles });
        }
        // keep the cursor on the same rail (by kind + title) across rebuilds
        let current = self.cols.get(self.rail).map(|(k, t, _)| (*k, t.clone()));
        let old = std::mem::take(&mut self.cols);
        self.cols = rails
            .iter()
            .map(|r| {
                let col = old.iter().find(|(k, t, _)| *k == r.kind && *t == r.title).map(|(_, _, c)| *c).unwrap_or(0);
                (r.kind, r.title.clone(), col.min(r.tiles.len().saturating_sub(1)))
            })
            .collect();
        if let Some((k, t)) = current
            && let Some(i) = self.cols.iter().position(|(k2, t2, _)| *k2 == k && *t2 == t)
        {
            self.rail = i;
        }
        self.rail = self.rail.min(rails.len().saturating_sub(1));
        self.rails = rails;
    }

    fn current_tile(&self) -> Option<(usize, &Tile)> {
        let rail = self.rails.get(self.rail)?;
        let col = self.cols.get(self.rail)?.2;
        rail.tiles.get(col).map(|t| (col, t))
    }

    fn move_col(&mut self, delta: isize) -> bool {
        let Some(rail) = self.rails.get(self.rail) else { return false };
        let Some(entry) = self.cols.get_mut(self.rail) else { return false };
        let n = rail.tiles.len() as isize;
        let next = (entry.2 as isize + delta).clamp(0, (n - 1).max(0)) as usize;
        entry.2 = next;
        self.scroll_to_cursor = true;
        true
    }

    fn activate(&mut self, ctx: &mut UiCtx<'_>, alt: bool) {
        let Some((_, tile)) = self.current_tile() else { return };
        match tile.clone() {
            Tile::Channel { id, collection } => {
                if !kit::play_channel(ctx, id, collection, PlayOrigin::Home) {
                    kit::ask_pin(ctx, if alt { Action::ActivateAlt } else { Action::Activate }, "This channel is locked");
                }
            }
            // a stack tile (DESIGN §29.12): Shift+Enter plays the copy that plays first, with
            // its fallback; Enter opens the tile copy's page (its PIN when that copy is locked)
            Tile::Vod(_) if alt && self.tile_stack(ctx, tile).is_some() => {
                let Some((movies, i)) = self.tile_stack(ctx, tile) else { return };
                if !crate::ui::copies::play_title(ctx, movies, i, None, true) {
                    kit::ask_pin(ctx, Action::ActivateAlt, "Every copy of this title is locked");
                }
            }
            Tile::Vod(kind) => {
                let locked = ctx.lib.vod.find(kind).is_some_and(|v| v.locked) && !kit::unlocked(ctx);
                if locked {
                    kit::ask_pin(ctx, if alt { Action::ActivateAlt } else { Action::Activate }, "This title is locked");
                    return;
                }
                match (kind, alt) {
                    (VodKind::Movie(id), true) => super::movie_detail::play_movie(ctx, id),
                    (VodKind::Movie(id), false) => ctx.push(Route::MovieDetail(id)),
                    (VodKind::Series(id), _) => ctx.push(Route::SeriesDetail(id)),
                }
            }
            Tile::Continue(i) => {
                let Some(c) = self.continue_items.get(i).cloned() else { return };
                // `card.locked` is the effective lock (the title or one of its categories)
                if c.card.locked && !kit::unlocked(ctx) {
                    kit::ask_pin(ctx, if alt { Action::ActivateAlt } else { Action::Activate }, "This title is locked");
                    return;
                }
                if alt {
                    // Shift+Enter resumes directly — a title's copies with their fallback
                    match self.tile_stack(ctx, tile) {
                        Some((movies, i)) => {
                            crate::ui::copies::play_title(ctx, movies, i, None, false);
                        }
                        None => kit::play_media(ctx, c.media, true, PlayOrigin::Home),
                    }
                } else {
                    ctx.push(c.open);
                }
            }
            Tile::Start(StartAction::BrowseLive) => ctx.navigate(crate::ui::NavRequest::Switch(Route::Live { group: None, channel: None })),
            Tile::Start(StartAction::OpenGuide) => ctx.navigate(crate::ui::NavRequest::Switch(Route::Guide { collection: None, focus: None })),
            Tile::Start(StartAction::AddFavourites) => {
                ctx.toast(crate::msg::Toast::info("Press Ctrl+F on any channel to add it to your favourites"));
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Live { group: None, channel: None }));
            }
            Tile::UpNext => resume_up_next(ctx),
        }
    }

    /// The title of a movie / series tile while its copies combine: (movies, index).
    fn tile_stack(&self, ctx: &UiCtx<'_>, tile: &Tile) -> Option<(bool, u32)> {
        let kind = match tile {
            Tile::Vod(kind) => *kind,
            Tile::Continue(i) => self.continue_items.get(*i)?.card.kind,
            _ => return None,
        };
        st::stack_of_kind(ctx.lib, ctx.config, kind)?;
        Some((st::is_movie(kind), st::index_of(ctx.lib, kind)?))
    }

    fn tile_item(&self, ctx: &UiCtx<'_>, tile: &Tile) -> Option<ItemRef> {
        match tile {
            Tile::Channel { id, .. } => ctx.lib.live.get(*id).map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }),
            Tile::Vod(kind) => ctx.lib.vod.find(*kind).map(vod_ref),
            Tile::Continue(i) => self.continue_items.get(*i).map(|c| c.item.clone()),
            Tile::Start(_) | Tile::UpNext => None,
        }
    }

    /// What the batch features act on for a tile.
    fn tile_target(&self, tile: &Tile) -> Option<crate::ui::item_menu::Target> {
        use crate::ui::item_menu::Target;
        Some(match tile {
            Tile::Channel { id, .. } => Target::Channel(*id),
            Tile::Vod(kind) => Target::vod(*kind),
            Tile::Continue(i) => Target::vod(self.continue_items.get(*i)?.card.kind),
            Tile::Start(_) | Tile::UpNext => return None,
        })
    }

    /// Display name of a tile (rename / hide / menu titles).
    fn tile_name(&self, ctx: &UiCtx<'_>, tile: &Tile) -> String {
        match tile {
            Tile::Channel { id, .. } => ctx.lib.live.get(*id).map(|c| c.name.to_string()),
            Tile::Vod(kind) => ctx.lib.vod.find(*kind).map(|v| v.name.to_string()),
            Tile::Continue(i) => self.continue_items.get(*i).map(|c| c.card.name.to_string()),
            Tile::UpNext => Some("Up next".to_owned()),
            Tile::Start(_) => None,
        }
        .unwrap_or_default()
    }

    /// (effective lock, locked through a category) of a tile.
    fn tile_lock(&self, ctx: &UiCtx<'_>, tile: &Tile) -> (bool, bool) {
        match tile {
            Tile::Channel { id, .. } => {
                ctx.lib.live.get(*id).map_or((false, false), |c| (c.locked, kit::any_group_locked(&ctx.lib.live.groups, &c.group_ids)))
            }
            Tile::Vod(kind) => ctx.lib.vod.find(*kind).map_or((false, false), |v| (v.locked, vod_group_locked(ctx, v))),
            Tile::Continue(i) => self.continue_items.get(*i).map_or((false, false), |c| (c.card.locked, vod_group_locked(ctx, &c.card))),
            Tile::Start(_) | Tile::UpNext => (false, false),
        }
    }

    /// Item shortcuts (Help lists them for `[PosterGrid]`) and the context menu.
    fn item_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) {
        // a feature's choice from the menu
        if let Some(t) = self.current_tile().and_then(|(_, t)| self.tile_target(t))
            && crate::ui::item_menu::dispatch(ctx, action, &t)
        {
            return;
        }
        if action == Action::SortMenu {
            ctx.toast(crate::msg::Toast::info("Home follows your lists — sort in Live TV, Movies or Series"));
            return;
        }
        let Some(tile) = self.current_tile().map(|(_, t)| t.clone()) else { return kit::not_here(ctx) };
        let rail = self.rails.get(self.rail).map(|r| (r.kind, r.list));
        let Some(item) = self.tile_item(ctx, &tile) else { return kit::not_here(ctx) };
        let name = self.tile_name(ctx, &tile);
        // a stack tile (DESIGN §29.12, §29.15): ★, ✓, lock, hide, rename and forgetting
        // progress cover the title's copies
        let combined = matches!(action, Action::ToggleFavourite | Action::ToggleHidden | Action::ToggleLock | Action::Rename | Action::MarkWatched)
            || (action == Action::Remove && matches!(rail, Some((RailKind::Continue, _))));
        if let Some((movies, i)) = self.tile_stack(ctx, &tile).filter(|_| combined) {
            let plan = crate::ui::copies::plan(ctx, movies, i).unwrap_or_default();
            match action {
                Action::ToggleFavourite => crate::ui::copies::toggle_favourite(ctx, &plan),
                Action::ToggleHidden => crate::ui::copies::hide(ctx, &plan, &name),
                Action::ToggleLock => crate::ui::copies::toggle_lock(ctx, &plan, movies),
                Action::Rename => crate::ui::copies::rename_dialog(ctx, &plan, &name),
                Action::MarkWatched if movies => crate::ui::copies::mark_watched(ctx, &plan, &name, true),
                Action::MarkWatched => ctx.toast(crate::msg::Toast::info("Open the series to mark its episodes watched")),
                _ if movies => crate::ui::copies::mark_watched(ctx, &plan, &name, false),
                _ => crate::ui::copies::forget_series(ctx, &plan),
            }
            return;
        }
        match (action, &tile) {
            (Action::ToggleFavourite, _) => kit::toggle_favourite(ctx, &item),
            (Action::AddToList, _) => kit::list_picker(ctx, &item, &name),
            (Action::ToggleHidden, _) => kit::hide_item(ctx, &item, &name),
            (Action::Rename, _) => kit::rename_item(ctx, &item, &name),
            (Action::ToggleLock, _) => {
                let (locked, by_group) = self.tile_lock(ctx, &tile);
                kit::toggle_lock(ctx, &item, locked, by_group);
            }
            (Action::ShowDetails, Tile::Channel { .. }) => ctx.open(OverlayRequest::EditItem(item)),
            (Action::ShowDetails, Tile::Vod(VodKind::Movie(id))) => ctx.push(Route::MovieDetail(*id)),
            (Action::ShowDetails, Tile::Vod(VodKind::Series(id))) => ctx.push(Route::SeriesDetail(*id)),
            (Action::ShowDetails, Tile::Continue(i)) => {
                if let Some(c) = self.continue_items.get(*i) {
                    ctx.push(c.open.clone());
                }
            }
            (Action::OpenInGuide, Tile::Channel { id, .. }) => ctx.push(Route::Guide { collection: None, focus: Some(*id) }),
            (Action::AssignEpg, Tile::Channel { id, .. }) => ctx.push(Route::EpgMapping { channel: Some(*id) }),
            (Action::MarkWatched, Tile::Vod(VodKind::Movie(_)) | Tile::Continue(_)) => kit::mark_watched(ctx, &item, &name, true),
            (Action::MarkWatched, Tile::Vod(VodKind::Series(_))) => {
                ctx.toast(crate::msg::Toast::info("Open the series to mark its episodes watched"))
            }
            (Action::Remove, _) => match rail {
                Some((RailKind::Favourites, Some(list))) => {
                    kit::set_in_list(ctx, list, &item, false);
                    let list_name = ctx.favs.lists.iter().find(|l| l.id == list).map(|l| l.name.clone()).unwrap_or_default();
                    ctx.toast(crate::msg::Toast::info(format!("Removed from {list_name}")));
                }
                Some((RailKind::Continue, _)) => kit::mark_watched(ctx, &item, &name, false),
                Some((RailKind::Recent, _)) => {
                    // History rebuilds the library (`recent_channels`), so the rail follows
                    let write_item = item.clone();
                    let _ = ctx
                        .services
                        .store
                        .writer()
                        .submit_notify(DataScope::History, move |c| crate::store::user::remove_history(c, &write_item).map(|_| ()));
                    ctx.toast(crate::msg::Toast::info(format!("Removed “{name}” from Recently watched")));
                }
                _ => kit::not_here(ctx),
            },
            (Action::MoveUp | Action::MoveDown, _) => match rail {
                Some((RailKind::Favourites, Some(list))) => self.move_in_list(ctx, list, &item, if action == Action::MoveUp { -1 } else { 1 }),
                _ => kit::not_here(ctx),
            },
            (Action::ContextMenu, _) => self.menu(ctx, &tile, rail, &name),
            _ => kit::not_here(ctx),
        }
    }

    /// Alt+↑/↓ on a favourites rail: move the tile within its list (the cursor follows).
    fn move_in_list(&mut self, ctx: &mut UiCtx<'_>, list: FavListId, item: &ItemRef, delta: isize) {
        let items = ctx.favs.items(list);
        let Some(i) = items.iter().position(|r| r == item) else { return };
        let j = i as isize + delta;
        if j < 0 || j as usize >= items.len() {
            return;
        }
        let write_item = item.clone();
        let _ = ctx
            .services
            .store
            .writer()
            .submit_notify(DataScope::Favourites, move |c| crate::store::user::move_favourite(c, list, &write_item, j as usize));
        self.move_col(delta);
    }

    /// Shift+F10 / right-click: the tile's actions (each echoed back as `Chosen(action)`).
    fn menu(&self, ctx: &mut UiCtx<'_>, tile: &Tile, rail: Option<(RailKind, Option<FavListId>)>, name: &str) {
        let mi = |icon: &'static str, label: &str, action: Action, shortcut: &str| MenuItem {
            icon,
            label: label.into(),
            action,
            enabled: true,
            shortcut: Some(shortcut.into()),
        };
        let mut items = Vec::new();
        match tile {
            Tile::Channel { .. } => {
                items.push(mi(glyphs::PLAY, "Play", Action::Activate, "Enter"));
                items.push(mi(glyphs::GUIDE, "Show in TV guide", Action::OpenInGuide, "Ctrl+G"));
                items.push(mi(glyphs::INFO, "Edit information…", Action::ShowDetails, "Ctrl+I"));
                items.push(mi(glyphs::GUIDE, "Assign guide channel…", Action::AssignEpg, "Ctrl+E"));
            }
            Tile::Vod(_) | Tile::Continue(_) => {
                items.push(mi(glyphs::INFO, "Details", Action::Activate, "Enter"));
                items.push(mi(glyphs::PLAY, "Play / resume", Action::ActivateAlt, "Shift+Enter"));
                if !matches!(tile, Tile::Vod(VodKind::Series(_))) {
                    items.push(mi(glyphs::CHECK, "Mark watched", Action::MarkWatched, "Ctrl+W"));
                }
            }
            Tile::UpNext => {
                let items = vec![
                    mi(glyphs::PLAY, "Carry on with Up next", Action::Activate, "Enter"),
                    mi(glyphs::UP_NEXT, "Open Up next", Action::GoQueue, "Alt+6"),
                ];
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: name.to_owned(), items }));
                return;
            }
            Tile::Start(_) => return,
        }
        let fav = self.tile_item(ctx, tile).is_some_and(|i| ctx.favs.is_fav(&i));
        items.push(mi(glyphs::FAVOURITE, if fav { "Remove from Favourites" } else { "Add to Favourites" }, Action::ToggleFavourite, "Ctrl+F"));
        items.push(mi(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, "Ctrl+Shift+F"));
        match rail {
            Some((RailKind::Favourites, Some(_))) => {
                items.push(mi(glyphs::CLOSE, "Remove from this list", Action::Remove, "Delete"));
                items.push(mi(glyphs::REORDER, "Move earlier", Action::MoveUp, "Alt+↑"));
                items.push(mi(glyphs::REORDER, "Move later", Action::MoveDown, "Alt+↓"));
            }
            Some((RailKind::Continue, _)) => items.push(mi(glyphs::CLOSE, "Remove from Continue watching", Action::Remove, "Delete")),
            Some((RailKind::Recent, _)) => items.push(mi(glyphs::CLOSE, "Remove from Recently watched", Action::Remove, "Delete")),
            _ => {}
        }
        items.push(mi(glyphs::TAG, "Rename…", Action::Rename, "F2"));
        items.push(mi(glyphs::LOCK, "Lock / unlock", Action::ToggleLock, "Ctrl+L"));
        items.push(mi(glyphs::HIDDEN, "Hide", Action::ToggleHidden, "Ctrl+H"));
        if let Some(t) = self.tile_target(tile) {
            crate::ui::item_menu::extend(&mut items, &t, ctx, KeyContext::List);
        }
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: name.to_owned(), items }));
    }
}

/// A title is locked through one of its categories.
fn vod_group_locked(ctx: &UiCtx<'_>, v: &VodSummary) -> bool {
    let groups = match v.kind {
        VodKind::Movie(_) => &ctx.lib.vod.movie_groups,
        VodKind::Series(_) => &ctx.lib.vod.series_groups,
    };
    kit::any_group_locked(groups, &v.group_ids)
}

/// The favourited movies and series (any list) of the library, by their refs (one pass over
/// the titles; only the favourites' keys are cloned).
fn favourite_vod(lib: &crate::store::library::Library, favs: &FavIndex) -> HashMap<ItemRef, VodKind> {
    let wanted: std::collections::HashSet<(crate::model::SourceId, ItemKind, &str)> = favs
        .lists
        .iter()
        .flat_map(|l| favs.items(l.id))
        .filter(|r| matches!(r.kind, ItemKind::Movie | ItemKind::Series))
        .map(|r| (r.source, r.kind, r.key.0.as_str()))
        .collect();
    if wanted.is_empty() {
        return HashMap::new();
    }
    let kind_of = |v: &VodSummary| if matches!(v.kind, VodKind::Movie(_)) { ItemKind::Movie } else { ItemKind::Series };
    lib.vod
        .movies
        .iter()
        .chain(&lib.vod.series)
        .filter(|v| wanted.contains(&(v.source_id, kind_of(v), v.key.0.as_str())))
        .map(|v| (vod_ref(v), v.kind))
        .collect()
}

/// The durable reference of a poster-grid row.
pub(crate) fn vod_ref(v: &VodSummary) -> ItemRef {
    let kind = match v.kind {
        VodKind::Movie(_) => ItemKind::Movie,
        VodKind::Series(_) => ItemKind::Series,
    };
    ItemRef { source: v.source_id, kind, key: v.key.clone() }
}

/// Continue watching: unfinished movies and episodes with their posters (blocking pool).
fn load_continue(c: &rusqlite::Connection) -> crate::error::Result<Vec<ContinueItem>> {
    use crate::store::{user, vod};
    let mut out = Vec::new();
    for p in user::continue_watching(c, RAIL_MAX)? {
        let progress = p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d) as f32).unwrap_or(0.0).clamp(0.0, 1.0);
        match p.item.kind {
            ItemKind::Movie => {
                let Some(id) = vod::resolve_movie(c, &p.item)? else { continue };
                let m = vod::movie(c, id)?;
                let card = VodSummary {
                    kind: VodKind::Movie(id),
                    source_id: m.source_id,
                    key: m.key.clone(),
                    name: Arc::from(m.display_name()),
                    year: m.year,
                    poster: m.poster.as_deref().map(Arc::from),
                    rating: m.rating,
                    group_ids: m.group_ids.clone(),
                    added_at: m.added_at,
                    provider_order: m.provider_order,
                    locked: m.locked,
                };
                out.push(ContinueItem { item: p.item, media: MediaRef::Movie(id), card, progress, open: Route::MovieDetail(id) });
            }
            ItemKind::Episode => {
                let Some(id) = vod::resolve_episode(c, &p.item)? else { continue };
                let e = vod::episode(c, id)?;
                let s = vod::series(c, e.series_id)?;
                let card = VodSummary {
                    kind: VodKind::Series(s.id),
                    source_id: s.source_id,
                    key: s.key.clone(),
                    name: Arc::from(format!("{} — S{:02}E{:02}", s.display_name(), e.season, e.episode)),
                    year: s.year,
                    poster: s.cover.as_deref().map(Arc::from),
                    rating: None,
                    group_ids: s.group_ids.clone(),
                    added_at: s.added_at,
                    provider_order: s.provider_order,
                    locked: s.locked,
                };
                out.push(ContinueItem { item: p.item, media: MediaRef::Episode(id), card, progress, open: Route::SeriesDetail(s.id) });
            }
            _ => {}
        }
    }
    Ok(out)
}

impl Screen for HomeScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Home
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::PosterGrid]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, _route: &Route) {
        self.reload_continue(ctx);
        self.scroll_to_cursor = true;
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(DataScope::Progress | DataScope::History | DataScope::All | DataScope::Sources | DataScope::Vod) => {
                self.reload_continue(ctx)
            }
            AppEvent::Player(crate::player::PlayerEvent::Ended { .. }) => self.reload_continue(ctx),
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if ctx.lib.built_at != 0 && ctx.lib.sources.is_empty() {
            // "No playlists yet": Enter is the "Add playlist" button
            if action == Action::Activate {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(OnboardingStart::Welcome)));
                return true;
            }
            return matches!(action, Action::Up | Action::Down | Action::Left | Action::Right | Action::FilterInput(_));
        }
        // the batch features' keys on the tile (Ctrl+N, Ctrl+M…)
        if let Some(t) = self.current_tile().and_then(|(_, t)| self.tile_target(t))
            && crate::ui::item_menu::dispatch(ctx, action, &t)
        {
            return true;
        }
        match action {
            Action::Up | Action::Down => {
                let n = self.rails.len();
                if n == 0 {
                    return true;
                }
                self.rail = if action == Action::Up { self.rail.saturating_sub(1) } else { (self.rail + 1).min(n - 1) };
                self.scroll_to_cursor = true;
                true
            }
            Action::PageUp => {
                self.rail = 0;
                self.scroll_to_cursor = true;
                true
            }
            Action::PageDown => {
                self.rail = self.rails.len().saturating_sub(1);
                self.scroll_to_cursor = true;
                true
            }
            Action::Left => self.move_col(-1),
            Action::Right => self.move_col(1),
            Action::Home => self.move_col(-(RAIL_MAX as isize) * 4),
            Action::End => self.move_col(RAIL_MAX as isize * 4),
            Action::Activate => {
                self.activate(ctx, false);
                true
            }
            Action::ActivateAlt => {
                self.activate(ctx, true);
                true
            }
            Action::ToggleFavourite
            | Action::AddToList
            | Action::ToggleHidden
            | Action::Rename
            | Action::ToggleLock
            | Action::MoveUp
            | Action::MoveDown
            | Action::SortMenu
            | Action::OpenInGuide
            | Action::ShowDetails
            | Action::AssignEpg
            | Action::MarkWatched
            | Action::Remove
            | Action::ContextMenu => {
                self.item_action(ctx, action);
                true
            }
            // letters never filter on Home (DESIGN §13.3)
            Action::FilterInput(_) | Action::Parent => true,
            _ => false,
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Unlocked(a @ (Action::Activate | Action::ActivateAlt))
            | OverlayResult::Chosen(a @ (Action::Activate | Action::ActivateAlt)) => self.activate(ctx, a == Action::ActivateAlt),
            OverlayResult::Unlocked(Action::ToggleLock) => self.item_action(ctx, Action::ToggleLock),
            // Up next's tile menu
            OverlayResult::Chosen(Action::GoQueue) => ctx.navigate(crate::ui::NavRequest::Switch(Route::Queue)),
            // the context menu (its entries are the shortcuts themselves)
            OverlayResult::Chosen(a) if a != Action::ContextMenu => self.item_action(ctx, a),
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.continue_loader) {
            Some(Ok(items)) => {
                if items != self.continue_items {
                    self.continue_items = items;
                    self.continue_gen += 1;
                }
            }
            Some(Err(e)) => tracing::warn!("continue watching: {e}"),
            None => {}
        }
        let theme = ctx.theme;
        if ctx.lib.built_at == 0 {
            loading_rails(ui, theme);
            return;
        }
        if ctx.lib.sources.is_empty() {
            // nothing to act on: no stale tile of a deleted playlist stays reachable
            self.rails.clear();
            self.cols.clear();
            self.rails_key = None;
            let r = empty_state::empty_state(
                ui,
                theme,
                glyphs::PLAYLIST,
                "No playlists yet",
                Some("Add an M3U playlist or an Xtream login to start watching."),
                Some("Add playlist"),
            );
            if r.is_some_and(|r| r.clicked()) {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(OnboardingStart::Welcome)));
            }
            return;
        }
        self.build_rails(ctx);
        let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let mut clicked: Option<(usize, usize, bool)> = None;
        let mut menu: Option<(usize, usize)> = None;
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        let rails = std::mem::take(&mut self.rails);
        let playing_vod = self.playing.get(ctx);
        egui::ScrollArea::vertical().id_salt("home-rails").auto_shrink([false, false]).show(ui, |ui| {
            for (ri, rail) in rails.iter().enumerate() {
                section::section_header(ui, theme, &rail.title);
                ui.add_space(theme.space(SpaceToken::Md));
                let cursor_col = (ri == self.rail).then(|| self.cols.get(ri).map(|c| c.2)).flatten();
                let rail_rect = egui::ScrollArea::horizontal()
                    .id_salt(("home-rail", rail.kind as u8, &rail.title))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xxl);
                            for (ci, tile) in rail.tiles.iter().enumerate() {
                                let is_cursor = cursor_col == Some(ci);
                                let resp = draw_tile(ui, ctx, &self.continue_items, tile, is_cursor, playing_vod);
                                if is_cursor && scroll {
                                    ui.scroll_to_rect(resp.rect, None);
                                }
                                if resp.double_clicked() {
                                    clicked = Some((ri, ci, true));
                                } else if resp.secondary_clicked() {
                                    menu = Some((ri, ci));
                                } else if resp.clicked() || (pointer_moved && resp.hovered() && !is_cursor) {
                                    clicked = Some((ri, ci, false));
                                }
                            }
                        });
                    })
                    .inner_rect;
                if ri == self.rail && scroll {
                    ui.scroll_to_rect(rail_rect, None);
                }
                ui.add_space(theme.space(SpaceToken::Xxl));
            }
        });
        self.rails = rails;
        if let Some((ri, ci, activate)) = clicked {
            self.rail = ri;
            if let Some(c) = self.cols.get_mut(ri) {
                c.2 = ci;
            }
            if activate {
                self.activate(ctx, false);
            }
        }
        // right-click: the tile's menu, as the menu key opens it
        if let Some((ri, ci)) = menu {
            self.rail = ri;
            if let Some(c) = self.cols.get_mut(ri) {
                c.2 = ci;
            }
            self.on_action(ctx, Action::ContextMenu);
        }
        if ctx.now_next.pending() || ctx.budget.deferred() {
            ui.ctx().request_repaint();
        }
    }
}

/// Skeleton rails while the first library loads (no spinner, DESIGN §13.12).
fn loading_rails(ui: &mut egui::Ui, theme: &Theme) {
    let t = &theme.tokens;
    for _ in 0..3 {
        let (r, _) = ui.allocate_exact_size(egui::vec2(160.0 * theme.rem(), theme.font(TypeToken::Caption)), egui::Sense::hover());
        ui.painter().rect_filled(r, t.rounding, t.fill_field);
        ui.add_space(theme.space(SpaceToken::Md));
        ui.horizontal(|ui| {
            for _ in 0..6 {
                let w = RAIL_POSTER_W * theme.rem();
                let (r, _) = ui.allocate_exact_size(egui::vec2(w, w * 1.5), egui::Sense::hover());
                ui.painter().rect_filled(r, t.rounding, t.fill_field);
            }
        });
        ui.add_space(theme.space(SpaceToken::Xxl));
    }
}

/// One rail tile.
fn draw_tile(
    ui: &mut egui::Ui,
    ctx: &mut UiCtx<'_>,
    continues: &[ContinueItem],
    tile: &Tile,
    cursor: bool,
    playing_vod: Option<VodKind>,
) -> egui::Response {
    let theme = ctx.theme;
    let playing = ctx.player.playable.as_ref().map(|p| p.media.clone());
    let poster_w = (RAIL_POSTER_W * theme.rem()).round();
    match tile {
        Tile::Vod(kind) => match ctx.lib.vod.find(*kind) {
            Some(v) => {
                let item_kind = crate::ui::scope::vod_item_kind(v.kind);
                let mut decor =
                    TileDecor { playing: playing_vod == Some(*kind), source: ctx.source_of(item_kind, v.source_id), ..Default::default() };
                // a stack tile: its copy count and playlists (no fan-out in a rail)
                if let Some(c) = st::index_of(ctx.lib, *kind).filter(|_| st::combine(ctx.config)).and_then(|i| {
                    let movies = st::is_movie(*kind);
                    st::card_stack(ctx.lib.vod.stacks(movies), i, st::all(ctx.lib, movies))
                }) {
                    let others: Vec<_> = c.playlists.iter().filter(|p| **p != v.source_id).collect();
                    decor.stack = Some(StackDecor {
                        count: c.count,
                        open: false,
                        fan: false,
                        next: [0, 1].map(|k| others.get(k).and_then(|p| ctx.source_of(item_kind, **p))),
                        others: others.len(),
                    });
                    decor.dimmed = c.all_locked && !kit::unlocked(ctx);
                }
                poster::poster_tile(ui, theme, ctx.images, v, poster_w, cursor, decor)
            }
            None => ui.allocate_exact_size(egui::vec2(poster_w, poster_w), egui::Sense::hover()).1,
        },
        Tile::Continue(i) => match continues.get(*i) {
            Some(c) => {
                let decor = TileDecor {
                    progress: Some(c.progress),
                    playing: playing.as_ref() == Some(&c.media),
                    source: ctx.source_of(crate::ui::scope::vod_item_kind(c.card.kind), c.card.source_id),
                    ..Default::default()
                };
                poster::poster_tile(ui, theme, ctx.images, &c.card, poster_w, cursor, decor)
            }
            None => ui.allocate_exact_size(egui::vec2(poster_w, poster_w), egui::Sense::hover()).1,
        },
        Tile::Channel { id, .. } => {
            let is_playing = playing == Some(MediaRef::Channel(*id));
            channel_tile(ui, ctx, *id, cursor, is_playing)
        }
        Tile::UpNext => up_next_tile(ui, ctx, cursor),
        Tile::Start(a) => {
            let (glyph, label) = match a {
                StartAction::BrowseLive => (glyphs::LIVE_TV, "Browse Live TV"),
                StartAction::OpenGuide => (glyphs::GUIDE, "Open guide"),
                StartAction::AddFavourites => (glyphs::FAVOURITE, "Add favourites (Ctrl+F)"),
            };
            action_tile(ui, theme, glyph, label, cursor)
        }
    }
}

/// Tile frame (fill on cursor/hover, 2 px accent outline on the cursor) → content rect.
fn tile_frame(ui: &mut egui::Ui, theme: &Theme, size: egui::Vec2, label: &str, cursor: bool) -> (Rect, egui::Response) {
    let t = &theme.tokens;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(rect, t.rounding, if cursor || resp.hovered() { t.list_cursor_bg } else { t.fill_field });
        if cursor {
            p.rect_stroke(rect, t.rounding, egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
        }
    }
    (rect, resp)
}

/// `[logo] BBC One / News at Eight ▕██░▏` (now/next through the shared cache).
fn channel_tile(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, id: ChannelId, cursor: bool, playing: bool) -> egui::Response {
    let theme = ctx.theme;
    let t = &theme.tokens;
    let rem = theme.rem();
    let size = egui::vec2((CHANNEL_TILE_W * rem).round(), (64.0 * rem).round());
    let Some(ch) = ctx.lib.live.get(id).cloned() else { return ui.allocate_exact_size(size, egui::Sense::hover()).1 };
    let source = ctx.source_of(ItemKind::Channel, ch.source_id);
    let (rect, resp) = tile_frame(ui, theme, size, &badge::label_with_source(&ch.name, source), cursor);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let pad = theme.space(SpaceToken::Lg);
    // Settings ▸ Interface ▸ Channel logos
    let text_left = if ctx.config.interface.show_logos {
        let logo_size = rect.height() - 2.0 * pad;
        let logo_rect = Rect::from_min_size(rect.min + egui::vec2(pad, pad), egui::vec2(logo_size, logo_size));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(logo_rect));
        crate::ui::widgets::logo::logo(&mut child, theme, ctx.images, ch.logo.as_deref(), &ch.name, logo_size);
        logo_rect.right() + pad
    } else {
        rect.left() + pad
    };
    let mut left = text_left;
    let right = rect.right() - pad;
    let p = ui.painter();
    let top_y = rect.top() + rect.height() * 0.32;
    // top line, from the right edge: lock, then the playlist's code chip (DESIGN §13.0)
    let mut name_right = right;
    if ch.locked {
        name_right -= theme.font(TypeToken::Icon) + theme.space(SpaceToken::Md);
    }
    if let Some(src) = source {
        let r = badge::paint_code_chip(p, theme, egui::pos2(name_right, top_y), Align2::RIGHT_CENTER, src, t.bg, t.fg);
        name_right = r.left() - theme.space(SpaceToken::Md);
        ui.interact(r, resp.id.with("source"), egui::Sense::hover())
            .on_hover_text(egui::RichText::new(&src.name).size(theme.font(TypeToken::BodySmall)));
    }
    let name_color = if cursor { t.list_cursor_text } else { t.fg };
    if playing {
        let r = p.text(egui::pos2(left, top_y), Align2::LEFT_CENTER, glyphs::PLAY, egui::FontId::proportional(theme.font(TypeToken::Icon)), t.accent);
        left = r.right() + theme.space(SpaceToken::Sm);
    }
    if ch.locked {
        p.text(
            egui::pos2(right, top_y),
            Align2::RIGHT_CENTER,
            glyphs::LOCK,
            egui::FontId::proportional(theme.font(TypeToken::Icon)),
            t.text_secondary,
        );
    }
    let badge = ctx.quality(&ch).map(|q| InlineBadge::quality(p, theme, q, t.text_secondary, 1.0));
    let font = egui::FontId::proportional(theme.font(TypeToken::Body));
    let name_w = if name_right < right { name_right - left } else { right - left - theme.font(TypeToken::Icon) };
    name_with_quality(p, theme, egui::pos2(left, top_y), &ch.name, font, name_color, badge, name_w);
    let detail_y = rect.top() + rect.height() * 0.64;
    let left = text_left;
    let nn: Option<NowNext> = ch.epg_channel_id.and_then(|e| {
        let mut reader = ctx.epg_reader();
        ctx.now_next.get((e, ch.tvg_shift_min), ctx.now, &mut reader, ctx.budget).cloned()
    });
    match (ch.epg_channel_id, nn.as_ref().and_then(|n| n.now.as_ref())) {
        (Some(_), Some(now)) => {
            let bar_w = (40.0 * rem).round();
            elided(
                p,
                egui::pos2(left, detail_y),
                &now.title,
                egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                t.text_secondary,
                right - left - bar_w - pad,
            );
            let bar = Rect::from_center_size(egui::pos2(right - bar_w / 2.0, detail_y), egui::vec2(bar_w, (3.0 * rem).round().max(2.0)));
            crate::ui::widgets::progress::paint_progress(p, bar, theme, now.progress(ctx.now), false);
        }
        (Some(_), None) if nn.is_none() => {}
        _ => {
            elided(
                p,
                egui::pos2(left, detail_y),
                "No guide data",
                egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                t.text_disabled,
                right - left,
            );
        }
    }
    resp
}

/// Up next's tile key: (items to go, the row it resumes) while it has some and nothing plays.
fn up_next_key(ctx: &UiCtx<'_>) -> Option<(usize, Option<crate::model::QueueId>)> {
    if !crate::queue::ENABLED || ctx.player.state != crate::player::PlayerState::Idle {
        return None;
    }
    let s = crate::queue::ui::snapshot(ctx);
    let n = s.pending();
    (n > 0).then(|| (n, s.current.or(s.next)))
}

/// Enter on Up next's tile: carry on where it stopped (else start it), in the player.
fn resume_up_next(ctx: &mut UiCtx<'_>) {
    use crate::queue::QueueCommand;
    let s = crate::queue::ui::snapshot(ctx);
    let cmd = match (s.current, s.next) {
        (Some(_), _) => QueueCommand::Resume,
        (None, Some(n)) => QueueCommand::PlayAt(n),
        (None, None) => return,
    };
    if crate::queue::ui::send(ctx, cmd) {
        ctx.push(Route::Player);
    }
}

/// "Resume Up next / Goldfinger · 5 more" (the row it resumes, what follows).
fn up_next_tile(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, cursor: bool) -> egui::Response {
    let theme = ctx.theme;
    let t = &theme.tokens;
    let rem = theme.rem();
    let s = crate::queue::ui::snapshot(ctx);
    let resumes = s.current.or(s.next).and_then(|id| s.row(id));
    let head = if s.current.is_some() { "Resume Up next" } else { "Start Up next" };
    let more = s.pending().saturating_sub(1);
    let detail = match (resumes, more) {
        (Some(r), 0) => crate::queue::ui::row_title(&r.entry),
        (Some(r), n) => format!("{} · {n} more", crate::queue::ui::row_title(&r.entry)),
        (None, _) => String::new(),
    };
    let size = egui::vec2((CHANNEL_TILE_W * 1.5 * rem).round(), (64.0 * rem).round());
    let (rect, resp) = tile_frame(ui, theme, size, &format!("{head} · {detail}"), cursor);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let pad = theme.space(SpaceToken::Lg);
        let g = p.text(
            egui::pos2(rect.left() + pad, rect.center().y),
            Align2::LEFT_CENTER,
            glyphs::UP_NEXT,
            egui::FontId::proportional(theme.font(TypeToken::IconLarge)),
            t.accent,
        );
        let x = g.right() + pad;
        let w = rect.right() - x - pad;
        let color = if cursor { t.list_cursor_text } else { t.fg };
        elided(p, egui::pos2(x, rect.top() + rect.height() * 0.32), head, egui::FontId::proportional(theme.font(TypeToken::Body)), color, w);
        elided(
            p,
            egui::pos2(x, rect.top() + rect.height() * 0.66),
            &detail,
            egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
            t.text_secondary,
            w,
        );
    }
    resp
}

/// Get-started action tile (glyph + label).
fn action_tile(ui: &mut egui::Ui, theme: &Theme, glyph: &str, label: &str, cursor: bool) -> egui::Response {
    let t = &theme.tokens;
    let rem = theme.rem();
    let size = egui::vec2((CHANNEL_TILE_W * rem).round(), (64.0 * rem).round());
    let (rect, resp) = tile_frame(ui, theme, size, label, cursor);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let pad = theme.space(SpaceToken::Lg);
        let color = if cursor { t.list_cursor_text } else { t.fg };
        let r = p.text(
            egui::pos2(rect.left() + pad, rect.center().y),
            Align2::LEFT_CENTER,
            glyph,
            egui::FontId::proportional(theme.font(TypeToken::IconLarge)),
            t.accent,
        );
        elided(
            p,
            egui::pos2(r.right() + pad, rect.center().y),
            label,
            egui::FontId::proportional(theme.font(TypeToken::Body)),
            color,
            rect.right() - r.right() - 2.0 * pad,
        );
    }
    resp
}

/// One elided line, vertically centred on `pos.y`.
fn elided(p: &egui::Painter, pos: egui::Pos2, text: &str, font: egui::FontId, color: egui::Color32, max_w: f32) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(0.0));
    let g = p.layout_job(job);
    p.galley(egui::pos2(pos.x, pos.y - g.size().y / 2.0), g, color);
}

/// Test rig shared by the WP-12 screen/overlay unit tests: a `TestUi` (real services in a
/// temp profile, headless mpv) + the screen in an `egui_kittest` harness, raw-SQL seeders and
/// helpers that deliver events, library swaps and config edits like the app does.
#[cfg(test)]
pub(crate) mod testkit {
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use eframe::egui;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use rusqlite::{Connection, params};

    use crate::error::Result;
    use crate::jobs::{JobEvent, JobOutcome};
    use crate::keymap::Action;
    use crate::model::{ChannelId, MovieId, SeriesId, SourceId, Timestamp};
    use crate::msg::AppEvent;
    use crate::ui::testing::TestUi;
    use crate::ui::{NavRequest, Route, Screen};

    /// One rig (mpv + runtime + store) at a time keeps the test binary's memory flat.
    static ONE: Mutex<()> = Mutex::new(());

    pub(crate) struct App<S> {
        pub t: TestUi,
        pub s: S,
        _one: MutexGuard<'static, ()>,
    }

    impl<S: Screen> App<S> {
        pub fn act(&mut self, a: Action) -> bool {
            // a toast's Undo is the app's, whatever screen shows (`ui::copies::undo`)
            if let Action::Undo(n) = a {
                return crate::ui::copies::undo(self.t.services(), n);
            }
            let s = &mut self.s;
            self.t.with_ctx(|ctx| s.on_action(ctx, a))
        }
        /// The newest toast with an Undo button, once the store write that offers it is done.
        pub fn undo_toast(&mut self) -> Option<crate::msg::Toast> {
            let _ = self.t.services().store.writer().blocking(|_| Ok(()));
            self.pump();
            self.t.toasts.iter().rev().find(|t| matches!(t.action, Some((_, Action::Undo(_))))).cloned()
        }
        pub fn enter(&mut self, r: Route) {
            let s = &mut self.s;
            self.t.with_ctx(|ctx| s.on_enter(ctx, &r));
        }
        pub fn result(&mut self, r: crate::ui::overlays::OverlayResult) {
            let s = &mut self.s;
            self.t.with_ctx(|ctx| s.on_overlay_result(ctx, r));
        }
        /// Deliver bus events like the app (a library swap first, then the screen).
        pub fn pump(&mut self) {
            while let Ok(ev) = self.t.events.try_recv() {
                if let AppEvent::Job(JobEvent::Finished { outcome: Ok(JobOutcome::Library(lib)), .. }) = &ev {
                    self.t.lib = lib.clone();
                    self.t.favs = lib.favs.clone();
                    self.t.search_index = None;
                }
                if let AppEvent::Toast(t) = &ev {
                    self.t.toasts.push(t.clone());
                }
                let s = &mut self.s;
                self.t.with_ctx(|ctx| s.on_event(ctx, &ev));
            }
        }
        /// Apply queued config edits like the app (without saving the file).
        pub fn apply_config(&mut self) {
            let mut c = (*self.t.config).clone();
            for e in self.t.config_edits.drain(..) {
                e(&mut c);
            }
            self.t.config = Arc::new(c);
        }
        pub fn nav(&mut self) -> Vec<NavRequest> {
            std::mem::take(&mut self.t.nav)
        }
    }

    pub(crate) fn rig() -> (TestUi, MutexGuard<'static, ()>) {
        let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        (TestUi::new().expect("TestUi"), one)
    }

    pub(crate) fn harness<S: Screen + 'static>(t: TestUi, one: MutexGuard<'static, ()>, s: S, size: egui::Vec2) -> Harness<'static, App<S>> {
        let mut h = Harness::builder().with_size(size).with_step_dt(0.02).build_ui_state(
            |ui, app: &mut App<S>| {
                app.pump();
                let s = &mut app.s;
                app.t.with_ctx(|ctx| s.ui(ui, ctx));
            },
            App { t, s, _one: one },
        );
        h.run_steps(2);
        h
    }

    /// An overlay under test and what it answered last ("Keep" frames don't overwrite it).
    pub(crate) struct OvApp<O> {
        pub t: TestUi,
        pub o: O,
        pub last: String,
        _one: MutexGuard<'static, ()>,
    }

    pub(crate) fn outcome(o: &crate::ui::overlays::OverlayOutcome) -> String {
        use crate::ui::overlays::OverlayOutcome as O;
        match o {
            O::Keep => "Keep".into(),
            O::Ignored => "Ignored".into(),
            O::Close => "Close".into(),
            O::CloseWith(r) => format!("CloseWith({r:?})"),
        }
    }

    impl<O: crate::ui::overlays::Overlay> OvApp<O> {
        pub fn act(&mut self, a: Action) -> String {
            let o = &mut self.o;
            outcome(&self.t.with_ctx(|ctx| o.on_action(ctx, a)))
        }
        pub fn nav(&mut self) -> Vec<NavRequest> {
            std::mem::take(&mut self.t.nav)
        }
    }

    pub(crate) fn overlay_harness<O: crate::ui::overlays::Overlay + 'static>(
        t: TestUi,
        one: MutexGuard<'static, ()>,
        o: O,
        size: egui::Vec2,
    ) -> Harness<'static, OvApp<O>> {
        let mut h = Harness::builder().with_size(size).with_step_dt(0.02).build_ui_state(
            |ui, app: &mut OvApp<O>| {
                let egui_ctx = ui.ctx().clone();
                let o = &mut app.o;
                let r = outcome(&app.t.with_ctx(|ctx| o.ui(&egui_ctx, ctx)));
                if r != "Keep" {
                    app.last = r;
                }
            },
            OvApp { t, o, last: String::new(), _one: one },
        );
        h.run_steps(2);
        h
    }

    /// Step overlay frames until `done` (10 s limit).
    pub(crate) fn until_ov<O>(h: &mut Harness<'static, OvApp<O>>, what: &str, mut done: impl FnMut(&mut Harness<'static, OvApp<O>>) -> bool) {
        let start = Instant::now();
        while !done(h) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(15));
            h.step();
        }
    }

    /// Step frames until `done` (10 s limit).
    pub(crate) fn until<S: Screen>(h: &mut Harness<'static, App<S>>, what: &str, mut done: impl FnMut(&mut Harness<'static, App<S>>) -> bool) {
        let start = Instant::now();
        while !done(h) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(15));
            h.step();
        }
    }

    pub(crate) fn has_label<T>(h: &Harness<'static, T>, text: &str) -> bool {
        h.query_all_by_label(text).next().is_some()
    }

    pub(crate) fn has_label_containing<T>(h: &Harness<'static, T>, text: &str) -> bool {
        h.query_all_by_label_contains(text).next().is_some()
    }

    /// Every text painted in the last frame (painter text isn't in the accessibility tree).
    pub(crate) fn painted<T>(h: &Harness<'static, T>) -> Vec<String> {
        fn flat(s: &egui::Shape, out: &mut Vec<String>) {
            match s {
                egui::Shape::Vec(v) => v.iter().for_each(|s| flat(s, out)),
                egui::Shape::Text(t) => out.push(t.galley.job.text.clone()),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for c in &h.output().shapes {
            flat(&c.shape, &mut out);
        }
        out
    }

    /// Seed with `f` on the writer, then load the library into the rig.
    pub(crate) fn seed<T: Send + 'static>(t: &mut TestUi, f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static) -> T {
        let r = t.services().store.writer().blocking(f).expect("seed");
        reload_library(t);
        r
    }

    pub(crate) fn reload_library(t: &mut TestUi) {
        let reader = t.services().store.reader().expect("reader");
        t.set_library(crate::store::library::load(&reader).expect("library"));
        t.favs = crate::store::library::load_favs(&reader).expect("favourites");
        t.search_index = None;
    }

    pub(crate) fn source(c: &mut Connection, name: &str) -> Result<SourceId> {
        c.execute("INSERT INTO source(name, kind, url, created_at, generation, last_refresh) VALUES (?1, 'm3u-url', 'http://127.0.0.1:9/playlist.m3u', 0, 1, 1)", [name])?;
        Ok(SourceId(c.last_insert_rowid()))
    }

    /// `n` channels "Channel {i}" (number i+1) in group "All" (+ every 10th in "News").
    pub(crate) fn channels(c: &mut Connection, src: SourceId, n: usize) -> Result<Vec<ChannelId>> {
        let tx = c.transaction()?;
        let mut ids = Vec::new();
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:all','All',0,?2,1)",
            params![src, n as i64],
        )?;
        let all = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:news','News',1,?2,1)",
            params![src, (n / 10) as i64],
        )?;
        let news = tx.last_insert_rowid();
        for i in 0..n {
            let url = format!("http://127.0.0.1:9/live/{i}.ts");
            tx.execute(
                "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, tvg_id, stream, url_key, provider_order, added_at, seen_gen)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
                params![src, format!("m3u:seed{i:011}"), format!("Channel {i}"), format!("channel{i}"), (i + 1) as i64, format!("ch{i}.tv"), format!(r#"{{"t":"url","url":"{url}"}}"#), url, i as i64, 1_700_000_000 + i as i64],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, 1)", params![all, id, i as i64])?;
            if i % 10 == 0 {
                tx.execute(
                    "INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, 0)",
                    params![news, id, (i / 10) as i64],
                )?;
            }
            ids.push(ChannelId(id));
        }
        tx.commit()?;
        Ok(ids)
    }

    /// One URL guide with an epg channel per playlist channel (mapped when `map`) and
    /// 30-minute programmes "Show {slot % 17} on {i}" from `start` for `hours`.
    pub(crate) fn epg(c: &mut Connection, chans: &[ChannelId], start: Timestamp, hours: u32, map: bool) -> Result<()> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO epg_source(name, kind, url, active_gen, n_channels) VALUES ('Seed EPG','url','http://127.0.0.1:9/epg.xml',1,?1)",
            [chans.len() as i64],
        )?;
        let src = tx.last_insert_rowid();
        for (i, ch) in chans.iter().enumerate() {
            tx.execute("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, 1)", params![src, format!("ch{i}.tv")])?;
            let eid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO epg_display_name(epg_channel_id, name, norm) VALUES (?1, ?2, ?3)",
                params![eid, format!("Channel {i}"), format!("channel{i}")],
            )?;
            if map {
                tx.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)", params![ch.0, eid])?;
            }
            for slot in 0..(hours * 2) as i64 {
                let s = start + slot * 1800;
                let title = format!("Show {} on {i}", slot % 17);
                tx.execute(
                    "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title) VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL)",
                    params![src, eid, s, s + 1800, title],
                )?;
                tx.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, NULL)", params![tx.last_insert_rowid(), title])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `n` movies "Movie {i}" (details loaded) in group "Movies".
    pub(crate) fn movies(c: &mut Connection, src: SourceId, n: usize) -> Result<Vec<MovieId>> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'movie','movie:movies','Movies',0,?2,1)",
            params![src, n as i64],
        )?;
        let g = tx.last_insert_rowid();
        let mut ids = Vec::new();
        for i in 0..n {
            tx.execute(
                "INSERT INTO movie(source_id, key, name, clean_name, norm_name, year, rating, stream, provider_order, added_at, seen_gen, plot, details_at)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, 1)",
                params![
                    src,
                    format!("m3u:movie{i:010}"),
                    format!("Movie {i}"),
                    format!("movie{i}"),
                    1980 + (i % 45) as i64,
                    (i % 10) as f64,
                    format!(r#"{{"t":"url","url":"http://127.0.0.1:9/movie/{i}.mp4"}}"#),
                    i as i64,
                    1_700_000_000 + i as i64,
                    format!("Plot of movie {i}.")
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, ?3)", params![g, id, i as i64])?;
            ids.push(MovieId(id));
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Movies of one playlist named like a provider names them (`(provider name, display
    /// name)`), in a category "Films", newest last, with their match keys computed as an
    /// import computes them (so copies stack, DESIGN §29.11). Keys `m3u:named{i}`.
    pub(crate) fn named_movies(c: &mut Connection, src: SourceId, names: &[(&str, &str)]) -> Result<Vec<MovieId>> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'movie','movie:films','Films',0,?2,1)",
            params![src, names.len() as i64],
        )?;
        let g = tx.last_insert_rowid();
        let mut ids = Vec::new();
        for (i, (raw, clean)) in names.iter().enumerate() {
            tx.execute(
                "INSERT INTO movie(source_id, key, name, clean_name, norm_name, stream, provider_order, added_at, seen_gen, plot, details_at, year)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 'A plot.', 1, ?9)",
                params![
                    src,
                    format!("m3u:named{i:04}"),
                    raw,
                    clean,
                    clean.to_lowercase(),
                    format!(r#"{{"t":"url","url":"http://127.0.0.1:9/movie/{}/{i}.mkv"}}"#, src.0),
                    i as i64,
                    1_700_000_000 + i as i64,
                    crate::sources::vod_match::parse(raw, None, &Default::default()).year,
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, ?3)", params![g, id, i as i64])?;
            ids.push(MovieId(id));
        }
        crate::store::titles::refresh_source(&tx, src)?;
        tx.commit()?;
        Ok(ids)
    }

    /// Series of one playlist named like a provider names them, with one season of
    /// `episodes` episodes each, match keys computed (see [`named_movies`]).
    pub(crate) fn named_series(c: &mut Connection, src: SourceId, names: &[(&str, &str)], episodes: u32) -> Result<Vec<SeriesId>> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'series','series:shows','Shows',0,?2,1)",
            params![src, names.len() as i64],
        )?;
        let g = tx.last_insert_rowid();
        let mut ids = Vec::new();
        for (i, (raw, clean)) in names.iter().enumerate() {
            tx.execute(
                "INSERT INTO series(source_id, key, name, clean_name, norm_name, provider_order, added_at, details_at, seen_gen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 1)",
                params![src, format!("m3u:named{i:04}"), raw, clean, clean.to_lowercase(), i as i64, 1_700_000_000 + i as i64],
            )?;
            let sid = tx.last_insert_rowid();
            tx.execute("INSERT INTO series_group(group_id, series_id, position) VALUES (?1, ?2, ?3)", params![g, sid, i as i64])?;
            tx.execute("INSERT INTO season(series_id, number, episode_count) VALUES (?1, 1, ?2)", params![sid, episodes])?;
            for e in 1..=episodes {
                tx.execute(
                    "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, 1, ?4, ?5, 2700, ?6)",
                    params![sid, src, format!("m3u:named-ep{i}-{e}"), e, format!("Episode {e}"), format!(r#"{{"t":"url","url":"http://127.0.0.1:9/series/{}/{i}/{e}.mp4"}}"#, src.0)],
                )?;
            }
            ids.push(SeriesId(sid));
        }
        crate::store::titles::refresh_source(&tx, src)?;
        tx.commit()?;
        Ok(ids)
    }

    /// `n` series "Series {i}" with `seasons` × `episodes` episodes (details loaded).
    pub(crate) fn series(c: &mut Connection, src: SourceId, n: usize, seasons: u32, episodes: u32) -> Result<Vec<SeriesId>> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'series','series:shows','Shows',0,?2,1)",
            params![src, n as i64],
        )?;
        let g = tx.last_insert_rowid();
        let mut ids = Vec::new();
        for i in 0..n {
            tx.execute(
                "INSERT INTO series(source_id, key, name, clean_name, norm_name, provider_order, added_at, details_at, seen_gen) VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?6, 1)",
                params![src, format!("m3u:series{i:09}"), format!("Series {i}"), format!("series{i}"), i as i64, 1_700_000_000 + i as i64],
            )?;
            let sid = tx.last_insert_rowid();
            tx.execute("INSERT INTO series_group(group_id, series_id, position) VALUES (?1, ?2, ?3)", params![g, sid, i as i64])?;
            for sn in 1..=seasons {
                tx.execute("INSERT INTO season(series_id, number, episode_count) VALUES (?1, ?2, ?3)", params![sid, sn, episodes])?;
                for e in 1..=episodes {
                    tx.execute(
                        "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2700, ?7)",
                        params![sid, src, format!("m3u:ep{i}-{sn}-{e}"), sn, e, format!("Episode {e}"), format!(r#"{{"t":"url","url":"http://127.0.0.1:9/series/{i}/{sn}/{e}.mp4"}}"#)],
                    )?;
                }
            }
            ids.push(SeriesId(sid));
        }
        tx.commit()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;

    use super::testkit::{self, harness, has_label, has_label_containing, rig, seed, until};
    use super::*;
    use crate::config::PlaylistOnCards;
    use crate::model::{ItemKey, SourceId, WatchProgress};
    use crate::ui::NavRequest;

    fn channel_ref(s: SourceId, i: usize) -> ItemRef {
        ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) }
    }

    /// Delete on a "Recently watched" tile forgets that channel's history (it used to say
    /// "Not available here").
    #[test]
    fn delete_removes_a_recently_watched_channel() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 4)?;
            testkit::movies(c, s, 3)?;
            for (i, at) in [(1usize, 100), (2, 200)] {
                crate::store::user::push_history(c, &crate::model::HistoryEntry { item: channel_ref(s, i), title: format!("Channel {i}"), at })?;
            }
            Ok(())
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(3);
        assert!(has_label(&h, "RECENTLY WATCHED"));
        // the first rail is Recently watched; its first tile the latest channel (2)
        assert!(h.state_mut().act(Action::Remove));
        assert!(h.state().t.toasts.iter().all(|t| t.text != "Not available here"), "{:?}", h.state().t.toasts);
        let history = |h: &egui_kittest::Harness<'_, testkit::App<HomeScreen>>| {
            crate::store::user::recent_channels(&h.state().t.services().store.reader().unwrap(), 10)
                .unwrap()
                .into_iter()
                .map(|e| e.item.key.0)
                .collect::<Vec<_>>()
        };
        until(&mut h, "the history row is gone", |h| history(h) == vec![format!("m3u:seed{:011}", 1)]);
    }

    /// The status-bar hint line on Home names only keys Home acts on (it used to offer
    /// "Enter details · Ctrl+S sort"; Home plays channels and has no sort).
    #[test]
    fn hint_line_names_only_what_home_does() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 4)?;
            testkit::movies(c, s, 3)?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(s, 1), true, 1)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(3);
        let stack = h.state().s.key_contexts();
        let line = crate::keymap::hint_line(&stack);
        assert!(!line.contains("sort") && !line.contains("details"), "{line}");
        for (action, chord, verb) in crate::keymap::hints(&stack) {
            if action == Action::ToggleHelp {
                continue; // the app's global
            }
            assert!(h.state_mut().act(action), "Home handles “{} {verb}”", chord.label());
            let toasts = std::mem::take(&mut h.state_mut().t.toasts);
            assert!(!toasts.iter().any(|t| t.text == "Not available here"), "“{} {verb}”: {toasts:?}", chord.label());
            h.state_mut().nav();
        }
    }

    #[test]
    fn rails_navigate_in_two_dimensions_and_channels_play_fullscreen_with_the_list_zap() {
        let (mut t, one) = rig();
        let (chans, movies) = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ch = testkit::channels(c, s, 20)?;
            let mv = testkit::movies(c, s, 5)?;
            let list = crate::store::user::default_fav_list(c)?;
            for i in [3usize, 5] {
                crate::store::user::set_favourite(c, list, &channel_ref(s, i), true, 1)?;
            }
            Ok((ch, mv))
        });
        let list = t.favs.default_list().unwrap();
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(3);
        assert!(has_label(&h, "FAVOURITES"), "favourites rail");
        assert!(has_label(&h, "RECENTLY ADDED MOVIES"));
        assert!(!has_label(&h, "GET STARTED"), "two rails: no get-started rail");
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PosterGrid]);
        // letters never filter on Home
        assert!(h.state_mut().act(Action::FilterInput('x')));
        assert!(h.state_mut().act(Action::Right));
        assert!(h.state_mut().act(Action::Activate));
        let nav = h.state_mut().nav();
        assert_eq!(nav, vec![NavRequest::Push(Route::Player)], "Enter plays FULLSCREEN");
        until(&mut h, "the zap list of the favourites rail", |h| {
            let app = h.state_mut();
            app.t.refresh_snapshot();
            app.t.snapshot.zap.as_ref().is_some_and(|z| z.collection == Collection::Favourites(list))
        });
        let snap = h.state().t.snapshot.clone();
        assert_eq!(snap.zap.as_ref().unwrap().channels.as_ref(), &[chans[3], chans[5]]);
        // ↓ to the movies rail: Enter opens the newest movie's page
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(movies[4]))]);
        h.state_mut().act(Action::End);
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(movies[0]))]);
    }

    /// While Up next has items to go and nothing plays, "Resume Up next" leads Home (after a
    /// restart nothing plays by itself: Enter carries on, in the player); once something plays
    /// the tile goes.
    #[test]
    fn up_next_leads_home_while_nothing_plays() {
        use crate::queue::{QueueRow, QueueSnapshot, RowState};
        let (mut t, one) = rig();
        testkit::seed(&mut t, |c| {
            let s = testkit::source(c, "TV")?;
            testkit::channels(c, s, 3).map(|_| ())
        });
        crate::ui::screens::queue::tests::start_engine(&t);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let rows: Vec<QueueRow> = [(1, RowState::Current), (2, RowState::Pending)]
            .map(|(id, s)| QueueRow { entry: crate::queue::ui::tests::entry(id, s), locked: false, eta: None, note: None })
            .to_vec();
        let snap = QueueSnapshot { rows: rows.into(), current: Some(crate::model::QueueId(1)), interrupted: true, ..Default::default() };
        t.services().agent.publish_queue(Arc::new(snap));
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1000.0, 800.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(2);
        assert!(has_label(&h, "Resume Up next · Movie 1 · 1 more"));
        assert_eq!(h.state().s.rails.first().map(|r| r.kind), Some(RailKind::UpNext), "first");
        h.state_mut().nav();
        assert!(h.state_mut().act(Action::Activate));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        let mut playing = (*h.state().t.snapshot).clone();
        playing.state = crate::player::PlayerState::Playing;
        h.state_mut().t.snapshot = Arc::new(playing);
        h.run_steps(2);
        assert!(h.state().s.rails.iter().all(|r| r.kind != RailKind::UpNext), "gone while something plays");
    }

    #[test]
    fn get_started_rail_continue_watching_and_the_empty_library() {
        let (mut t, one) = rig();
        let movie = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)?;
            let m = testkit::movies(c, s, 1)?[0];
            let item = ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000000".into()) };
            crate::store::user::save_progress(
                c,
                &WatchProgress {
                    item,
                    position_s: 600.0,
                    duration_s: Some(1200.0),
                    updated_at: 5,
                    finished: false,
                    title: "Movie 0".into(),
                    fingerprint: Default::default(),
                },
            )?;
            Ok(m)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1000.0, 800.0));
        h.state_mut().enter(Route::Home);
        until(&mut h, "continue watching", |h| has_label(h, "CONTINUE WATCHING"));
        // continue + movies = 2 rails: no get-started rail. The cursor stayed on the movies
        // rail it was on: Shift+Enter there asks Resume / Start over (progress > 60 s)
        assert!(!has_label(&h, "GET STARTED"));
        h.state_mut().act(Action::ActivateAlt);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Resume(r))] if r.position_s == 600.0), "{nav:?}");
        // on the continue-watching rail Shift+Enter resumes directly
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::ActivateAlt);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        until(&mut h, "the movie playing", |h| {
            let app = h.state_mut();
            app.t.refresh_snapshot();
            app.t.snapshot.playable.as_ref().is_some_and(|p| p.media == MediaRef::Movie(movie))
        });
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::MovieDetail(movie))], "Enter opens the details");

        // a library without favourites, history or VOD gets the get-started rail
        let app = h.state_mut();
        app.t.services().tiles.focused().send(crate::player::PlayerCommand::Stop);
        std::thread::sleep(std::time::Duration::from_millis(300));
        app.t.services().store.writer().blocking(|c| Ok(c.execute("DELETE FROM watch_progress", [])?)).unwrap();
        let mut lib = crate::store::library::load(&app.t.services().store.reader().unwrap()).unwrap();
        lib.vod = Default::default();
        app.t.set_library(lib);
        app.s = HomeScreen::default();
        h.run_steps(2);
        assert!(has_label(&h, "GET STARTED"));
        assert!(has_label(&h, "Browse Live TV") && has_label(&h, "Add favourites (Ctrl+F)"));
        assert!(has_label(&h, "Channel 0"), "the first group's channels with now/next");
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Switch(Route::Live { group: None, channel: None })]);

        // no playlists at all: the empty state routes to onboarding
        let app = h.state_mut();
        app.t.set_library(crate::store::library::Library { built_at: 1, ..Default::default() });
        h.run_steps(2);
        assert!(has_label(&h, "No playlists yet"));
        h.get_by_label("Add playlist").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Switch(Route::Onboarding(OnboardingStart::Welcome))]);
        // …and by keyboard: Enter is "Add playlist"; no tile of the deleted playlist is left
        assert!(h.state().s.rails.is_empty(), "stale rails dropped");
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Switch(Route::Onboarding(OnboardingStart::Welcome))]);
    }

    /// A zap swaps in a copy of the same build (same `built_at`) with a new recently watched
    /// list: the rail follows without a library rebuild.
    #[test]
    fn recently_watched_follows_a_zap_without_a_rebuild() {
        let (mut t, one) = rig();
        let chans = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1000.0, 800.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(2);
        assert!(!has_label(&h, "RECENTLY WATCHED"));
        let app = h.state_mut();
        let zapped = app.t.lib.with_recent_channels(vec![chans[1]]);
        assert_eq!(zapped.built_at, app.t.lib.built_at);
        app.t.set_library(zapped);
        h.run_steps(2);
        assert!(has_label(&h, "RECENTLY WATCHED"));
    }

    #[test]
    fn locked_channels_ask_for_the_pin_before_playing() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(s, 1), true, 1)?;
            crate::store::user::set_item_override(c, &channel_ref(s, 1), &crate::model::ItemOverride { locked: true, ..Default::default() })
        });
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
        };
        t.config = Arc::new(cfg);
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1000.0, 800.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(2);
        h.state_mut().act(Action::Activate);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]), "{nav:?}");
        // the PIN overlay answers Unlocked(Activate): the channel plays now
        let app = h.state_mut();
        let cfg = app.t.config.clone();
        assert!(app.t.parental.unlock("1234", &cfg.parental));
        app.result(crate::ui::overlays::OverlayResult::Unlocked(Action::Activate));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
    }

    /// Review fixes: a locked title on Continue watching asks for the PIN before Shift+Enter
    /// resumes it; favourited titles get a rail; Delete removes a tile from its favourites
    /// list; Ctrl+H hides; the item shortcuts and the context menu work on tiles.
    /// DESIGN §20.3: Continue watching is read from the database, so it leaves out what the
    /// library conceals itself — an explicit movie is in no rail (nor its title on screen)
    /// while parental control hides it, and back once it shows.
    #[test]
    fn rails_leave_out_explicit_titles_while_hidden() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 3)?;
            c.execute("UPDATE movie SET explicit = 1 WHERE key = 'm3u:movie0000000000'", [])?;
            let list = crate::store::user::default_fav_list(c)?;
            for i in 0..2 {
                let item = ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) };
                let p = WatchProgress {
                    item: item.clone(),
                    position_s: 600.0,
                    duration_s: Some(1200.0),
                    updated_at: 5 + i as i64,
                    finished: false,
                    title: format!("Movie {i}"),
                    fingerprint: Default::default(),
                };
                crate::store::user::save_progress(c, &p)?;
                crate::store::user::set_favourite(c, list, &item, true, 1)?;
            }
            Ok(())
        });
        let shown = crate::store::library::load(&t.services().store.reader().unwrap()).unwrap();
        t.set_library(shown.hiding_explicit());
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        until(&mut h, "continue watching", |h| has_label(h, "CONTINUE WATCHING"));
        let tiles = |h: &egui_kittest::Harness<'static, testkit::App<HomeScreen>>, kind: RailKind| {
            h.state().s.rails.iter().find(|r| r.kind == kind).map_or(0, |r| r.tiles.len())
        };
        assert_eq!((tiles(&h, RailKind::Continue), tiles(&h, RailKind::Favourites), tiles(&h, RailKind::Movies)), (1, 1, 2));
        assert!(!testkit::painted(&h).iter().any(|p| p.contains("Movie 0")), "the title isn't on screen");
        h.state_mut().t.set_library(shown);
        h.run_steps(2);
        assert_eq!((tiles(&h, RailKind::Continue), tiles(&h, RailKind::Favourites), tiles(&h, RailKind::Movies)), (2, 2, 3));
    }

    #[test]
    fn continue_watching_respects_locks_and_tiles_take_the_item_shortcuts() {
        let (mut t, one) = rig();
        let src = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 5)?;
            testkit::movies(c, s, 3)?;
            let movie = |i: usize| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) };
            crate::store::user::save_progress(
                c,
                &WatchProgress {
                    item: movie(0),
                    position_s: 600.0,
                    duration_s: Some(1200.0),
                    updated_at: 5,
                    finished: false,
                    title: "Movie 0".into(),
                    fingerprint: Default::default(),
                },
            )?;
            crate::store::user::set_item_override(c, &movie(0), &crate::model::ItemOverride { locked: true, ..Default::default() })?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &movie(1), true, 1)?;
            crate::store::user::set_favourite(c, list, &channel_ref(s, 2), true, 2)?;
            Ok(s)
        });
        let mut cfg = (*t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
        };
        t.config = Arc::new(cfg);
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        until(&mut h, "continue watching", |h| has_label(h, "CONTINUE WATCHING"));
        assert_eq!(h.state().s.rails[0].kind, RailKind::Continue);
        h.state_mut().s.rail = 0;
        // the locked movie: Shift+Enter asks for the PIN instead of playing
        h.state_mut().act(Action::ActivateAlt);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Pin { then, .. })] if **then == Action::ActivateAlt), "{nav:?}");
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        app.result(OverlayResult::Unlocked(Action::ActivateAlt));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)], "resumed once unlocked");

        // the favourites rail holds the movie and the channel (in list order)
        assert!(has_label(&h, "FAVOURITES"));
        h.state_mut().act(Action::Down);
        let rail = &h.state().s.rails[h.state().s.rail];
        assert_eq!(rail.kind, RailKind::Favourites);
        assert!(matches!(rail.tiles.as_slice(), [Tile::Vod(VodKind::Movie(_)), Tile::Channel { .. }]), "{:?}", rail.tiles);
        // the context menu lists the tile's actions; Delete removes the movie from the list
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.action == Action::Remove)),
            "{nav:?}"
        );
        // a right-click on the tile opens that same menu
        h.state_mut().s.rail = 0;
        h.run_steps(1);
        // (the movie is in later rails too; the favourites rail is the first that has it)
        let tile = h.query_all_by_label("Movie 1").min_by(|a, b| a.rect().top().total_cmp(&b.rect().top())).expect("the tile");
        tile.click_secondary();
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), nav, "right-click = the menu key");
        assert_eq!(h.state().s.rails[h.state().s.rail].kind, RailKind::Favourites, "on the clicked tile");
        h.state_mut().act(Action::Remove);
        let movie1 = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000001".into()) };
        until(&mut h, "removed from the list", |h| {
            crate::store::user::lists_containing(&h.state().t.services().store.reader().unwrap(), &movie1).unwrap().is_empty()
        });
        h.run_steps(1);
        let rail = &h.state().s.rails[h.state().s.rail];
        assert!(matches!(rail.tiles.as_slice(), [Tile::Channel { .. }]), "the rail follows at once: {:?}", rail.tiles);
        // Ctrl+H hides the channel; Ctrl+G shows it in the guide; Ctrl+S explains itself
        h.state_mut().act(Action::ToggleHidden);
        until(&mut h, "hidden", |h| {
            crate::store::user::item_override(&h.state().t.services().store.reader().unwrap(), &channel_ref(src, 2))
                .unwrap()
                .is_some_and(|o| o.hidden)
        });
        h.state_mut().act(Action::OpenInGuide);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Push(Route::Guide { focus: Some(_), .. })]));
        let toasts = h.state().t.toasts.len();
        assert!(h.state_mut().act(Action::SortMenu));
        assert_eq!(h.state().t.toasts.len(), toasts + 1, "Ctrl+S on Home says why nothing sorts");
        assert!(h.state_mut().nav().is_empty());
    }

    /// The rails are rebuilt only when their inputs change (not per frame).
    #[test]
    fn rails_are_cached_until_the_library_or_favourites_change() {
        let (mut t, one) = rig();
        let src = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)?;
            testkit::movies(c, s, 2)?;
            Ok(s)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(2);
        let key = h.state().s.rails_key;
        assert!(key.is_some());
        h.run_steps(3);
        assert_eq!(h.state().s.rails_key, key, "same inputs, same rails");
        let app = h.state_mut();
        let list = app.t.favs.default_list().unwrap();
        app.t.favs.apply_toggle(list, &channel_ref(src, 1), true);
        h.run_steps(1);
        assert_ne!(h.state().s.rails_key, key, "a favourite rebuilds them");
        assert!(has_label(&h, "FAVOURITES"));
    }

    /// Favourited movies are resolved by key for their rail, and only they (a map of every
    /// title cloned every key: ~40 MiB for 380k titles); one favourited later joins the rail.
    #[test]
    fn only_favourite_titles_are_resolved_for_the_rails() {
        let (mut t, one) = rig();
        let src = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::movies(c, s, 4)?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &movie_ref(s, 1), true, 1)?;
            Ok(s)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(2);
        assert_eq!(h.state().s.vod_refs.keys().collect::<Vec<_>>(), [&movie_ref(src, 1)]);
        assert!(has_label(&h, "FAVOURITES"));
        let app = h.state_mut();
        let list = app.t.favs.default_list().unwrap();
        app.t.favs.apply_toggle(list, &movie_ref(src, 3), true);
        h.run_steps(1);
        assert_eq!(h.state().s.vod_refs.len(), 2);
        let favs = h.state().s.rails.iter().find(|r| r.kind == RailKind::Favourites).map(|r| r.tiles.len());
        assert_eq!(favs, Some(2), "both favourites have a tile");
        // a zap (the library copied with another recent list) shares the VOD lists: the rails
        // are rebuilt for the recent list, the titles are not resolved again
        let resolved = h.state().s.vod_refs_for.clone();
        let app = h.state_mut();
        app.t.lib = Arc::new(app.t.lib.with_recent_channels(Vec::new()));
        h.run_steps(2);
        assert!(resolved.is_some() && h.state().s.vod_refs_for == resolved, "no rescan for a zap");
        // a rebuild has new lists (and the favourites as stored: the toggle above was in memory)
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(2);
        assert!(h.state().s.vod_refs_for != resolved);
        assert_eq!(h.state().s.vod_refs.keys().collect::<Vec<_>>(), [&movie_ref(src, 1)]);
    }

    fn movie_ref(s: SourceId, i: usize) -> ItemRef {
        ItemRef { source: s, kind: ItemKind::Movie, key: crate::model::ItemKey(format!("m3u:movie{i:010}")) }
    }

    /// With two playlists every card and tile says which one its item comes from — its own,
    /// by name in the accessible label and by code in the corner chip, the meta line keeping
    /// year and rating (DESIGN §13.0 "Playlist codes") — and Settings ▸ Interface ▸
    /// Playlist on cards applies at once; Auto counts eligible playlists only: enabled, and
    /// with that kind of item (DESIGN §13.0, §29).
    #[test]
    fn cards_name_the_playlist_of_their_own_item() {
        let (mut t, one) = rig();
        let bravo = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            testkit::movies(c, a, 2)?;
            testkit::movies(c, b, 2)?;
            testkit::channels(c, b, 3)?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(b, 1), true, 1)?;
            Ok(b)
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        h.run_steps(3);
        assert!(has_label(&h, "Movie 0 · Alpha") && has_label(&h, "Movie 0 · Bravo"), "same title, each card its own playlist");
        // only Bravo has channels: one playlist of channels, so channel tiles carry no mark
        assert!(has_label(&h, "Channel 1") && !has_label_containing(&h, "Channel 1 ·"), "per kind: {:?}", testkit::painted(&h));
        let painted = testkit::painted(&h);
        assert!(painted.iter().any(|s| s == "BR"), "the code chip shows it without hovering: {painted:?}");
        assert!(!painted.iter().any(|s| s.contains("Bravo")), "no names on the cards: {painted:?}");
        let set = |h: &mut egui_kittest::Harness<'static, testkit::App<HomeScreen>>, mode: PlaylistOnCards| {
            let app = h.state_mut();
            let mut c = (*app.t.config).clone();
            c.interface.playlist_on_cards = mode;
            app.t.config = std::sync::Arc::new(c);
            h.run_steps(2);
        };
        set(&mut h, PlaylistOnCards::Never);
        assert!(has_label(&h, "Movie 0") && !has_label_containing(&h, "· Alpha"), "Never: no playlist anywhere");
        assert!(!testkit::painted(&h).iter().any(|s| s == "BR"));
        // Auto with one of the two playlists disabled: a single playlist is noise
        set(&mut h, PlaylistOnCards::Auto);
        testkit::seed(&mut h.state_mut().t, move |c| Ok(c.execute("UPDATE source SET enabled = 0 WHERE id = ?1", [bravo])?));
        h.run_steps(3);
        assert!(has_label(&h, "Movie 0") && !has_label_containing(&h, "· Alpha"), "Auto: one enabled playlist");
        set(&mut h, PlaylistOnCards::Always);
        assert!(has_label(&h, "Movie 0 · Alpha"), "Always: even a single playlist");
    }

    /// Copies of one title are one Home tile (DESIGN §29.14): Recently added shows the stack's
    /// top copy once with its count and playlists, Continue watching the copy played last, a
    /// favourites list the first favourite copy; Ctrl+F on a stack tile without favourites adds
    /// the copy that plays first. "Show each copy" brings every copy back. The series rail is
    /// "Recently updated series".
    #[test]
    fn titles_in_several_playlists_are_one_tile() {
        let (mut t, one) = rig();
        let (a, b) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("FR ★ Dune - 2021", "Dune (FR)"), ("EN ★ Heat - 1995", "Heat")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune"), ("EN - Solo (2018)", "Solo")])?;
            testkit::series(c, a, 1, 1, 1)?;
            for (i, (src, key)) in [(a, "m3u:named0001"), (b, "m3u:named0000")].into_iter().enumerate() {
                let item = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey(key.into()) };
                let p = WatchProgress {
                    item,
                    position_s: 600.0,
                    duration_s: Some(6000.0),
                    updated_at: 10 + i as i64,
                    finished: false,
                    title: String::new(),
                    fingerprint: Default::default(),
                };
                crate::store::user::save_progress(c, &p)?;
            }
            Ok((a, b))
        });
        let mut h = harness(t, one, HomeScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::Home);
        until(&mut h, "Continue watching", |h| h.state().s.rails.iter().any(|r| r.kind == RailKind::Continue));
        let rail = |h: &egui_kittest::Harness<'static, testkit::App<HomeScreen>>, k: RailKind| {
            h.state().s.rails.iter().find(|r| r.kind == k).map(|r| r.tiles.len()).unwrap_or(0)
        };
        assert_eq!(rail(&h, RailKind::Movies), 3, "Heat, Dune, Solo");
        assert_eq!(rail(&h, RailKind::Continue), 1, "two copies in progress, one tile");
        assert!(has_label(&h, "Dune · Alpha + Bravo · 3 copies"));
        assert!(h.state().s.rails.iter().any(|r| r.kind == RailKind::Series && r.title == "Recently updated series"));
        // Ctrl+F on the stack tile: the copy that plays first (Bravo's, the one watched last)
        let rail_at = h.state().s.rails.iter().position(|r| r.kind == RailKind::Movies).unwrap();
        h.state_mut().s.rail = rail_at;
        let col = h.state().s.rails[rail_at]
            .tiles
            .iter()
            .position(|t| matches!(t, Tile::Vod(k) if h.state().t.lib.vod.find(*k).is_some_and(|v| &*v.name == "Dune")))
            .unwrap();
        h.state_mut().s.cols[rail_at].2 = col;
        h.state_mut().act(Action::ToggleFavourite);
        let r = |s: SourceId, i: usize| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) };
        assert!(h.state().t.favs.is_fav(&r(b, 0)) && !h.state().t.favs.is_fav(&r(a, 0)));
        h.run_steps(2);
        let favs = h.state().s.rails.iter().find(|x| x.kind == RailKind::Favourites).map(|x| x.tiles.len());
        assert_eq!(favs, Some(1));
        // show each copy
        let app = h.state_mut();
        let mut c = (*app.t.config).clone();
        c.interface.same_title = crate::config::SameTitle::ShowEach;
        app.t.config = std::sync::Arc::new(c);
        h.run_steps(2);
        assert_eq!(rail(&h, RailKind::Movies), 5);
        assert_eq!(rail(&h, RailKind::Continue), 2);
    }

    /// A boxed screen as a `Screen` (one harness type for every screen).
    struct Dyn(Box<dyn Screen>);

    impl Screen for Dyn {
        fn id(&self) -> ScreenId {
            self.0.id()
        }
        fn key_contexts(&self) -> Vec<KeyContext> {
            self.0.key_contexts()
        }
        fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
            self.0.on_enter(ctx, route)
        }
        fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
            self.0.on_event(ctx, ev)
        }
        fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
            self.0.on_action(ctx, action)
        }
        fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
            self.0.ui(ui, ctx)
        }
    }

    /// A screen factory and the route it is entered with.
    type ScreenCase = (fn() -> Box<dyn Screen>, Route);

    /// The id of the playlist named `name` in the rig's library.
    fn h_source(t: &crate::ui::testing::TestUi, name: &str) -> SourceId {
        t.lib.sources.iter().find(|s| s.name == name).map(|s| s.id).expect("source")
    }

    /// DESIGN §12.1 / WP-12: every WP-12 screen (and Live TV and the guide, with their playlist
    /// scope, §29) at a tiled half (790 pt) and full screen
    /// (1600 pt), Omarchy text size 12 and 16 (rem 1 and 4/3): it draws without panicking and
    /// nothing it lays out leaves the window horizontally.
    #[test]
    fn every_screen_fits_790_and_1600_pt_at_text_size_12_and_16() {
        let (mut t, one) = rig();
        let (movie, series) = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV with a rather long playlist name")?;
            // a second playlist of every kind: every card, rail and detail page shows its source
            // (§13.0), and Movies, Series, Live TV and the guide get the scope bar (§29)
            let s2 = testkit::source(c, "Another provider with an even longer playlist name")?;
            testkit::movies(c, s2, 3)?;
            testkit::series(c, s2, 1, 1, 1)?;
            testkit::channels(c, s2, 3)?;
            let ch = testkit::channels(c, s, 40)?;
            testkit::epg(c, &ch[..10], 0, 2, true)?;
            let m = testkit::movies(c, s, 30)?[0];
            let sr = testkit::series(c, s, 3, 2, 4)?[0];
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(s, 1), true, 1)?;
            // Live TV (and so the guide) shows one playlist: its button, stripe and breadcrumb
            c.execute("UPDATE source SET color = 'red' WHERE id = ?1", [s2])?;
            Ok((m, sr))
        });
        let s2 = h_source(&t, "Another provider with an even longer playlist name");
        let mut c = (*t.config).clone();
        c.interface.playlist_scope.live = Some(s2);
        t.config = std::sync::Arc::new(c);
        let mut h = harness(t, one, Dyn(Box::new(HomeScreen::default())), egui::vec2(1600.0, 900.0));
        let screens: Vec<ScreenCase> = vec![
            (|| Box::new(HomeScreen::default()), Route::Home),
            (|| Box::new(super::super::movies::MoviesScreen::default()), Route::Movies { group: None }),
            (|| Box::new(super::super::live::LiveScreen::default()), Route::Live { group: None, channel: None }),
            (|| Box::new(super::super::guide::GuideScreen::default()), Route::Guide { collection: None, focus: None }),
            (|| Box::new(super::super::series::SeriesScreen::default()), Route::Series { group: None }),
            (|| Box::new(super::super::movie_detail::MovieDetailScreen::default()), Route::MovieDetail(movie)),
            (|| Box::new(super::super::series_detail::SeriesDetailScreen::default()), Route::SeriesDetail(series)),
            (|| Box::new(super::super::settings::SettingsScreen::default()), Route::Settings(crate::ui::SettingsPage::Playlists)),
            (|| Box::new(super::super::settings::SettingsScreen::default()), Route::Settings(crate::ui::SettingsPage::Interface)),
            (|| Box::new(super::super::onboarding::OnboardingScreen::default()), Route::Onboarding(OnboardingStart::AddSource)),
            (|| Box::new(super::super::epg_mapping::EpgMappingScreen::default()), Route::EpgMapping { channel: None }),
            (|| Box::new(super::super::logs::LogsScreen::default()), Route::Logs),
        ];
        for (w, scale) in [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)] {
            let base = crate::theme::Theme::fallback();
            let theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.set_size(egui::vec2(w, 900.0));
            for (make, route) in &screens {
                let app = h.state_mut();
                app.t.theme = theme.clone();
                app.t.layout = crate::ui::LayoutClass::from_width(w, theme.rem());
                app.s = Dyn(make());
                app.enter(route.clone());
                h.run_steps(4);
                std::thread::sleep(std::time::Duration::from_millis(30));
                h.run_steps(2);
                let id = h.state().s.id();
                for node in h.query_all_by_label_contains("") {
                    // Home's rails scroll horizontally: their tiles may continue past the edge
                    let rail_tile = id == ScreenId::Home && format!("{node:?}").contains("role: Button");
                    let r = node.rect();
                    assert!(rail_tile || r.right() <= w + 1.0, "{id:?} at {w} pt × {scale}: {node:?} ends at {}", r.right());
                    assert!(r.left() >= -1.0, "{id:?} at {w} pt × {scale}: {node:?} starts at {}", r.left());
                }
            }
        }
    }

    #[test]
    fn time_labels() {
        assert_eq!(kit::clock(2480.0), "41:20");
        assert_eq!(kit::clock(3733.0), "1:02:13");
        assert_eq!(kit::runtime(8160), "2 h 16");
        assert_eq!(kit::runtime(2520), "42 min");
        assert_eq!(kit::ago(1000, 990), "just now");
        assert_eq!(kit::ago(10_000, 10_000 - 7200), "2 h ago");
        assert_eq!(kit::ago(1_000_000, 1_000_000 - 3 * 86_400), "3 d ago");
        assert_eq!(kit::date(1_791_576_000).len(), 10);
    }
}
