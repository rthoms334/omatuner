//! Home (DESIGN §13.3): rails — Continue watching (posters w/ progress), Favourite channels
//! (live now/next tiles; one rail per non-empty favourites list), Recently watched channels,
//! Recently added movies / series. Key context `[PosterGrid]` (2-D: ↑/↓ between rails, ←/→
//! within a rail; letters do NOT filter on Home — the screen ignores `FilterInput`). Enter on a
//! channel plays FULLSCREEN (no preview pane here; DESIGN §13 "Enter rule"), zap collection =
//! the favourites list of the rail / the channel's primary group. Fewer than 2 rails → a
//! "Get started" rail (Browse Live TV · Open guide · Add favourites (Ctrl+F) · first group's
//! channels with now/next). Owner: WP-12.
//!
//! Data: everything but "Continue watching" comes from the in-memory `Library` and the
//! app-owned favourites; Continue watching is read off the UI thread ([`kit::Loader`]) on enter
//! and whenever progress/history/the library changes. An empty library (no sources) shows the
//! "No playlists yet" state with an "Add playlist" action (→ onboarding).
//!
//! This file also hosts [`kit`]: helpers shared by the WP-12 screens and overlays (background
//! reads, fullscreen playback with the right zap list, favourites, parental checks).

use std::sync::Arc;

use eframe::egui::{self, Align2, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, Collection, ItemKind, ItemRef, MediaRef, NowNext, PlayOrigin, SortMode, Timestamp, VodKind, VodSummary};
use crate::msg::{AppEvent, DataScope};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::widgets::poster::{self, TileDecor};
use crate::ui::widgets::{empty_state, glyphs, section};
use crate::ui::{OnboardingStart, OverlayRequest, Route, Screen, ScreenId, UiCtx};

use kit::Loader;

/// Shared helpers of the WP-12 screens and overlays.
pub(crate) mod kit {
    use crate::error::Result;
    use crate::keymap::Action;
    use crate::model::{ChannelId, Collection, ItemRef, MediaRef, PlayOrigin, ZapList};
    use crate::msg::{DataScope, Toast};
    use crate::player::{PlayRequest, PlayerCommand};
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
    pub(crate) fn unlocked(ctx: &UiCtx<'_>) -> bool { ctx.parental.is_unlocked(&ctx.config.parental) }

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
        let _ = ctx.services.store.writer().submit_notify(DataScope::Favourites, move |c| crate::store::user::set_favourite(c, list, &write_item, on, now));
        ctx.favs.apply_toggle(list, item, on);
        ctx.toast(Toast::info(if on { "Added to Favourites" } else { "Removed from Favourites" }));
    }

    /// Ctrl+L on an item: lock / unlock it (needs a PIN; unlocking needs the open session).
    pub(crate) fn toggle_lock(ctx: &mut UiCtx<'_>, item: &ItemRef, locked_now: bool) {
        if ctx.config.parental.pin_hash.is_none() || !ctx.config.parental.enabled {
            ctx.toast(Toast::info("Set a PIN in Settings ▸ Parental first"));
            return;
        }
        if !unlocked(ctx) {
            ask_pin(ctx, Action::ToggleLock, "Enter the PIN to change locks");
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
        };
        let _ = ctx.services.store.writer().submit_notify(DataScope::Reminders, move |c| crate::store::user::add_reminder(c, &r).map(|_| ()));
        ctx.toast(Toast::info(format!("Reminder set: {title} at {}", hhmm(start))));
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

    /// Local "HH:MM".
    pub(crate) fn hhmm(ts: crate::model::Timestamp) -> String {
        match jiff::Timestamp::from_second(ts) {
            Ok(t) => t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string(),
            Err(_) => "--:--".into(),
        }
    }

    /// Local "Thu 21:00".
    pub(crate) fn day_time(ts: crate::model::Timestamp) -> String {
        match jiff::Timestamp::from_second(ts) {
            Ok(t) => t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %H:%M").to_string(),
            Err(_) => "--".into(),
        }
    }

    /// Local "YYYY-MM-DD".
    pub(crate) fn date(ts: crate::model::Timestamp) -> String {
        match jiff::Timestamp::from_second(ts) {
            Ok(t) => t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d").to_string(),
            Err(_) => "?".into(),
        }
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
    Channel { id: ChannelId, collection: Option<Collection> },
    Vod(VodKind),
    Continue(usize),
    Start(StartAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailKind {
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
    tiles: Vec<Tile>,
}

/// Items per rail.
const RAIL_MAX: usize = 30;
/// Channel tile width (pt × rem).
const CHANNEL_TILE_W: f32 = 230.0;
/// Poster width in rails (pt × rem).
const RAIL_POSTER_W: f32 = 120.0;

#[derive(Default)]
pub struct HomeScreen {
    /// (rail index, per-rail column).
    rail: usize,
    cols: Vec<(RailKind, String, usize)>,
    continue_items: Vec<ContinueItem>,
    continue_loader: Option<Loader<Vec<ContinueItem>>>,
    /// Library build the "recently added" lists were taken from.
    recent_built_at: Timestamp,
    recent_movies: Vec<VodSummary>,
    recent_series: Vec<VodSummary>,
    /// Rails of the last frame (actions resolve against what the user saw).
    rails: Vec<Rail>,
    scroll_to_cursor: bool,
    playing: kit::PlayingVod,
}

impl HomeScreen {
    fn reload_continue(&mut self, ctx: &UiCtx<'_>) {
        self.continue_loader = Some(Loader::start(ctx, load_continue));
    }

    /// Rebuild the rails from the library, favourites and the cached reads.
    fn build_rails(&mut self, ctx: &mut UiCtx<'_>) {
        if self.recent_built_at != ctx.lib.built_at {
            self.recent_built_at = ctx.lib.built_at;
            let take = |movies: bool, all: &[VodSummary]| -> Vec<VodSummary> {
                ctx.lib.vod.all_sorted(movies, SortMode::RecentlyAdded).into_iter().take(RAIL_MAX).filter_map(|i| all.get(i as usize).cloned()).collect()
            };
            self.recent_movies = take(true, &ctx.lib.vod.movies);
            self.recent_series = take(false, &ctx.lib.vod.series);
        }
        let mut rails = Vec::new();
        if !self.continue_items.is_empty() {
            rails.push(Rail { kind: RailKind::Continue, title: "Continue watching".into(), tiles: (0..self.continue_items.len()).map(Tile::Continue).collect() });
        }
        for list in ctx.favs.non_empty_lists() {
            let tiles: Vec<Tile> = ctx
                .favs
                .items(list.id)
                .iter()
                .filter_map(|r| match r.kind {
                    ItemKind::Channel => ctx.lib.live.by_ref(r).filter(|id| !ctx.lib.live.hidden.contains(id)).map(|id| Tile::Channel { id, collection: Some(Collection::Favourites(list.id)) }),
                    _ => vod_by_ref(ctx, r).map(Tile::Vod),
                })
                .take(RAIL_MAX)
                .collect();
            if !tiles.is_empty() {
                let title = if list.is_default { "Favourite channels".to_owned() } else { list.name.clone() };
                rails.push(Rail { kind: RailKind::Favourites, title, tiles });
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
            rails.push(Rail { kind: RailKind::Recent, title: "Recently watched".into(), tiles: recent });
        }
        if !self.recent_movies.is_empty() {
            rails.push(Rail { kind: RailKind::Movies, title: "Recently added movies".into(), tiles: self.recent_movies.iter().map(|v| Tile::Vod(v.kind)).collect() });
        }
        if !self.recent_series.is_empty() {
            rails.push(Rail { kind: RailKind::Series, title: "Recently added series".into(), tiles: self.recent_series.iter().map(|v| Tile::Vod(v.kind)).collect() });
        }
        if rails.len() < 2 {
            let mut tiles = vec![Tile::Start(StartAction::BrowseLive), Tile::Start(StartAction::OpenGuide), Tile::Start(StartAction::AddFavourites)];
            if let Some(g) = ctx.lib.live.visible_groups(None, true).first() {
                tiles.extend(ctx.lib.live.group_members(g.id).iter().take(RAIL_MAX).map(|id| Tile::Channel { id: *id, collection: None }));
            }
            rails.push(Rail { kind: RailKind::GetStarted, title: "Get started".into(), tiles });
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
                if alt {
                    // Shift+Enter resumes directly
                    kit::play_media(ctx, c.media, true, PlayOrigin::Home);
                } else {
                    ctx.push(c.open);
                }
            }
            Tile::Start(StartAction::BrowseLive) => ctx.navigate(crate::ui::NavRequest::Switch(Route::Live { group: None, channel: None })),
            Tile::Start(StartAction::OpenGuide) => ctx.navigate(crate::ui::NavRequest::Switch(Route::Guide { collection: None, focus: None })),
            Tile::Start(StartAction::AddFavourites) => {
                ctx.toast(crate::msg::Toast::info("Press Ctrl+F on any channel to add it here"));
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Live { group: None, channel: None }));
            }
        }
    }

    fn tile_item(&self, ctx: &UiCtx<'_>, tile: &Tile) -> Option<ItemRef> {
        match tile {
            Tile::Channel { id, .. } => ctx.lib.live.get(*id).map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }),
            Tile::Vod(kind) => ctx.lib.vod.find(*kind).map(vod_ref),
            Tile::Continue(i) => self.continue_items.get(*i).map(|c| c.item.clone()),
            Tile::Start(_) => None,
        }
    }
}

/// The durable reference of a poster-grid row.
pub(crate) fn vod_ref(v: &VodSummary) -> ItemRef {
    let kind = match v.kind {
        VodKind::Movie(_) => ItemKind::Movie,
        VodKind::Series(_) => ItemKind::Series,
    };
    ItemRef { source: v.source_id, kind, key: v.key.clone() }
}

/// A favourited movie/series in the library (linear scan of one kind; favourites lists are short).
fn vod_by_ref(ctx: &UiCtx<'_>, r: &ItemRef) -> Option<VodKind> {
    let all = match r.kind {
        ItemKind::Movie => &ctx.lib.vod.movies,
        ItemKind::Series => &ctx.lib.vod.series,
        _ => return None,
    };
    all.iter().find(|v| v.source_id == r.source && v.key == r.key).map(|v| v.kind)
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
    fn id(&self) -> ScreenId { ScreenId::Home }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::PosterGrid] }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, _route: &Route) {
        self.reload_continue(ctx);
        self.scroll_to_cursor = true;
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(DataScope::Progress | DataScope::History | DataScope::All | DataScope::Sources | DataScope::Vod) => self.reload_continue(ctx),
            AppEvent::Player(crate::player::PlayerEvent::Ended { .. }) => self.reload_continue(ctx),
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
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
            Action::ToggleFavourite => {
                if let Some(item) = self.current_tile().and_then(|(_, t)| self.tile_item(ctx, t)) {
                    kit::toggle_favourite(ctx, &item);
                }
                true
            }
            // letters never filter on Home (DESIGN §13.3)
            Action::FilterInput(_) | Action::Parent => true,
            _ => false,
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: crate::ui::overlays::OverlayResult) {
        if let crate::ui::overlays::OverlayResult::Unlocked(a @ (Action::Activate | Action::ActivateAlt)) = result {
            self.activate(ctx, a == Action::ActivateAlt);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.continue_loader) {
            Some(Ok(items)) => self.continue_items = items,
            Some(Err(e)) => tracing::warn!("continue watching: {e}"),
            None => {}
        }
        let theme = ctx.theme;
        if ctx.lib.built_at == 0 {
            loading_rails(ui, theme);
            return;
        }
        if ctx.lib.sources.is_empty() {
            let r = empty_state::empty_state(ui, theme, glyphs::PLAYLIST, "No playlists yet", Some("Add an M3U playlist or an Xtream login to start watching."), Some("Add playlist"));
            if r.is_some_and(|r| r.clicked()) {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(OnboardingStart::Welcome)));
            }
            return;
        }
        self.build_rails(ctx);
        let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let mut clicked: Option<(usize, usize, bool)> = None;
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
fn draw_tile(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, continues: &[ContinueItem], tile: &Tile, cursor: bool, playing_vod: Option<VodKind>) -> egui::Response {
    let theme = ctx.theme;
    let playing = ctx.player.playable.as_ref().map(|p| p.media.clone());
    let poster_w = (RAIL_POSTER_W * theme.rem()).round();
    match tile {
        Tile::Vod(kind) => match ctx.lib.vod.find(*kind) {
            Some(v) => {
                let decor = TileDecor { playing: playing_vod == Some(*kind), ..Default::default() };
                poster::poster_tile(ui, theme, ctx.images, v, poster_w, cursor, decor)
            }
            None => ui.allocate_exact_size(egui::vec2(poster_w, poster_w), egui::Sense::hover()).1,
        },
        Tile::Continue(i) => match continues.get(*i) {
            Some(c) => {
                let decor = TileDecor { progress: Some(c.progress), playing: playing.as_ref() == Some(&c.media), watched: false };
                poster::poster_tile(ui, theme, ctx.images, &c.card, poster_w, cursor, decor)
            }
            None => ui.allocate_exact_size(egui::vec2(poster_w, poster_w), egui::Sense::hover()).1,
        },
        Tile::Channel { id, .. } => {
            let is_playing = playing == Some(MediaRef::Channel(*id));
            channel_tile(ui, ctx, *id, cursor, is_playing)
        }
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
    let (rect, resp) = tile_frame(ui, theme, size, &ch.name, cursor);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let pad = theme.space(SpaceToken::Lg);
    let logo_size = rect.height() - 2.0 * pad;
    let logo_rect = Rect::from_min_size(rect.min + egui::vec2(pad, pad), egui::vec2(logo_size, logo_size));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(logo_rect));
    crate::ui::widgets::logo::logo(&mut child, theme, ctx.images, ch.logo.as_deref(), &ch.name, logo_size);
    let mut left = logo_rect.right() + pad;
    let right = rect.right() - pad;
    let p = ui.painter();
    let top_y = rect.top() + rect.height() * 0.32;
    let name_color = if cursor { t.list_cursor_text } else { t.fg };
    if playing {
        let r = p.text(egui::pos2(left, top_y), Align2::LEFT_CENTER, glyphs::PLAY, egui::FontId::proportional(theme.font(TypeToken::Icon)), t.accent);
        left = r.right() + theme.space(SpaceToken::Sm);
    }
    if ch.locked {
        p.text(egui::pos2(right, top_y), Align2::RIGHT_CENTER, glyphs::LOCK, egui::FontId::proportional(theme.font(TypeToken::Icon)), t.text_secondary);
    }
    elided(p, egui::pos2(left, top_y), &ch.name, egui::FontId::proportional(theme.font(TypeToken::Body)), name_color, right - left - theme.font(TypeToken::Icon));
    let detail_y = rect.top() + rect.height() * 0.64;
    let left = logo_rect.right() + pad;
    let nn: Option<NowNext> = ch.epg_channel_id.and_then(|e| {
        let mut reader = ctx.epg_reader();
        ctx.now_next.get((e, ch.tvg_shift_min), ctx.now, &mut reader, ctx.budget).cloned()
    });
    match (ch.epg_channel_id, nn.as_ref().and_then(|n| n.now.as_ref())) {
        (Some(_), Some(now)) => {
            let bar_w = (40.0 * rem).round();
            elided(p, egui::pos2(left, detail_y), &now.title, egui::FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_secondary, right - left - bar_w - pad);
            let bar = Rect::from_center_size(egui::pos2(right - bar_w / 2.0, detail_y), egui::vec2(bar_w, (3.0 * rem).round().max(2.0)));
            crate::ui::widgets::progress::paint_progress(p, bar, theme, now.progress(ctx.now), false);
        }
        (Some(_), None) if nn.is_none() => {}
        _ => {
            elided(p, egui::pos2(left, detail_y), "No guide data", egui::FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_disabled, right - left);
        }
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
        let r = p.text(egui::pos2(rect.left() + pad, rect.center().y), Align2::LEFT_CENTER, glyph, egui::FontId::proportional(theme.font(TypeToken::IconLarge)), t.accent);
        elided(p, egui::pos2(r.right() + pad, rect.center().y), label, egui::FontId::proportional(theme.font(TypeToken::Body)), color, rect.right() - r.right() - 2.0 * pad);
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
            let s = &mut self.s;
            self.t.with_ctx(|ctx| s.on_action(ctx, a))
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
        pub fn nav(&mut self) -> Vec<NavRequest> { std::mem::take(&mut self.t.nav) }
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
        pub fn nav(&mut self) -> Vec<NavRequest> { std::mem::take(&mut self.t.nav) }
    }

    pub(crate) fn overlay_harness<O: crate::ui::overlays::Overlay + 'static>(t: TestUi, one: MutexGuard<'static, ()>, o: O, size: egui::Vec2) -> Harness<'static, OvApp<O>> {
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

    pub(crate) fn has_label<T>(h: &Harness<'static, T>, text: &str) -> bool { h.query_all_by_label(text).next().is_some() }

    pub(crate) fn has_label_containing<T>(h: &Harness<'static, T>, text: &str) -> bool { h.query_all_by_label_contains(text).next().is_some() }

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
        tx.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:all','All',0,?2,1)", params![src, n as i64])?;
        let all = tx.last_insert_rowid();
        tx.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:news','News',1,?2,1)", params![src, (n / 10) as i64])?;
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
                tx.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, 0)", params![news, id, (i / 10) as i64])?;
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
        tx.execute("INSERT INTO epg_source(name, kind, url, active_gen, n_channels) VALUES ('Seed EPG','url','http://127.0.0.1:9/epg.xml',1,?1)", [chans.len() as i64])?;
        let src = tx.last_insert_rowid();
        for (i, ch) in chans.iter().enumerate() {
            tx.execute("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, 1)", params![src, format!("ch{i}.tv")])?;
            let eid = tx.last_insert_rowid();
            tx.execute("INSERT INTO epg_display_name(epg_channel_id, name, norm) VALUES (?1, ?2, ?3)", params![eid, format!("Channel {i}"), format!("channel{i}")])?;
            if map {
                tx.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)", params![ch.0, eid])?;
            }
            for slot in 0..(hours * 2) as i64 {
                let s = start + slot * 1800;
                let title = format!("Show {} on {i}", slot % 17);
                tx.execute("INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title) VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL)", params![src, eid, s, s + 1800, title])?;
                tx.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, NULL)", params![tx.last_insert_rowid(), title])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `n` movies "Movie {i}" (details loaded) in group "Movies".
    pub(crate) fn movies(c: &mut Connection, src: SourceId, n: usize) -> Result<Vec<MovieId>> {
        let tx = c.transaction()?;
        tx.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'movie','movie:movies','Movies',0,?2,1)", params![src, n as i64])?;
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

    /// `n` series "Series {i}" with `seasons` × `episodes` episodes (details loaded).
    pub(crate) fn series(c: &mut Connection, src: SourceId, n: usize, seasons: u32, episodes: u32) -> Result<Vec<SeriesId>> {
        let tx = c.transaction()?;
        tx.execute("INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'series','series:shows','Shows',0,?2,1)", params![src, n as i64])?;
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

    use super::testkit::{self, harness, has_label, rig, seed, until};
    use super::*;
    use crate::model::{ItemKey, SourceId, WatchProgress};
    use crate::ui::NavRequest;

    fn channel_ref(s: SourceId, i: usize) -> ItemRef { ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) } }

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
        assert!(has_label(&h, "FAVOURITE CHANNELS"), "favourites rail");
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

    #[test]
    fn get_started_rail_continue_watching_and_the_empty_library() {
        let (mut t, one) = rig();
        let movie = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 3)?;
            let m = testkit::movies(c, s, 1)?[0];
            let item = ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000000".into()) };
            crate::store::user::save_progress(c, &WatchProgress { item, position_s: 600.0, duration_s: Some(1200.0), updated_at: 5, finished: false, title: "Movie 0".into(), fingerprint: Default::default() })?;
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
        app.t.services().player.handle().send(crate::player::PlayerCommand::Stop);
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
        cfg.parental = crate::config::ParentalConfig { enabled: true, pin_salt: Some("s".into()), pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")), relock_min: 0 };
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

    /// A boxed screen as a `Screen` (one harness type for every screen).
    struct Dyn(Box<dyn Screen>);

    impl Screen for Dyn {
        fn id(&self) -> ScreenId { self.0.id() }
        fn key_contexts(&self) -> Vec<KeyContext> { self.0.key_contexts() }
        fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) { self.0.on_enter(ctx, route) }
        fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) { self.0.on_event(ctx, ev) }
        fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool { self.0.on_action(ctx, action) }
        fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) { self.0.ui(ui, ctx) }
    }

    /// A screen factory and the route it is entered with.
    type ScreenCase = (fn() -> Box<dyn Screen>, Route);

    /// DESIGN §12.1 / WP-12: every WP-12 screen at a tiled half (790 pt) and full screen
    /// (1600 pt), Omarchy text size 12 and 16 (rem 1 and 4/3): it draws without panicking and
    /// nothing it lays out leaves the window horizontally.
    #[test]
    fn every_screen_fits_790_and_1600_pt_at_text_size_12_and_16() {
        let (mut t, one) = rig();
        let (movie, series) = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV with a rather long playlist name")?;
            let ch = testkit::channels(c, s, 40)?;
            testkit::epg(c, &ch[..10], 0, 2, true)?;
            let m = testkit::movies(c, s, 30)?[0];
            let sr = testkit::series(c, s, 3, 2, 4)?[0];
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &channel_ref(s, 1), true, 1)?;
            Ok((m, sr))
        });
        let mut h = harness(t, one, Dyn(Box::new(HomeScreen::default())), egui::vec2(1600.0, 900.0));
        let screens: Vec<ScreenCase> = vec![
            (|| Box::new(HomeScreen::default()), Route::Home),
            (|| Box::new(super::super::movies::MoviesScreen::default()), Route::Movies { group: None }),
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
        assert_eq!(kit::hhmm(1_791_576_000).len(), 5);
        assert_eq!(kit::date(1_791_576_000).len(), 10);
    }
}
