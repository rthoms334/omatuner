//! Series detail (DESIGN §13.6): header (backdrop, title, plot) + season tabs (`[` / `]`,
//! Tab moves between buttons / seasons / episodes) + episode list opened at
//! `store::vod::next_episode`; Enter plays, Shift+Enter from the start, Ctrl+W marks watched,
//! bulk "mark season watched" in the context menu. Key context `[Detail]`. End of an episode:
//! the page follows along (cursor on the next episode). Owner: WP-12.
//!
//! Data is read off the UI thread (series, seasons, episodes, per-episode progress and the
//! episode to open at) and re-read on `DataScope::{VodDetails, Progress, Overrides}`; Xtream
//! series without loaded details submit `JobKind::LoadSeriesDetails` and show a skeleton;
//! playlist (M3U) series are complete at import and get the sparse header (DESIGN §27.7).
//! The "Next episode in 10 s" card itself is the fullscreen player's (WP-11, drawn over the
//! video in OSD colours, for an episode started anywhere); this page only moves its cursor.
//!
//! A series with several copies (DESIGN §29.14) gets a Copies section of mini-cards between
//! the buttons and the seasons (`copies::Copies`; ↑/↓ and Tab reach it, ←/→ walk it, Enter
//! picks the copy Play uses — Play then opens at that copy's next episode — the menu key opens
//! its menu, "Open its page" included) and a "Play from" button (Ctrl+O).

use std::collections::HashMap;

use eframe::egui;

use super::home::kit::{self, Loader};
use super::movie_detail::{DetailHeader, NO_DESCRIPTION, detail_bar, fraction, resumable, resume_or_play, sparse_meta};
use crate::jobs::{JobEvent, JobKind};
use crate::keymap::{Action, KeyContext};
use crate::model::{Episode, EpisodeId, ItemKey, ItemKind, ItemRef, MediaRef, PlayOrigin, Series, SeriesId, SourceKind, VodKind, WatchProgress};
use crate::msg::{AppEvent, DataScope};
use crate::player::PlayerEvent;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, section};
use crate::ui::{OverlayRequest, Route, Screen, ScreenId, UiCtx};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    Buttons,
    /// The Copies mini-cards (only while the series has several copies).
    Copies,
    Seasons,
    #[default]
    Episodes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    Cursor,
    SelectedEpisode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Play,
    /// "Play from ▾": the series' copies (DESIGN §29.14).
    PlayFrom,
    Favourite,
    SeasonWatched,
    RemoveContinue,
}

/// Context-menu choices (echoed back as `Action::Digit`).
const MENU_SEASON_WATCHED: u8 = 1;
const MENU_SEASON_UNWATCHED: u8 = 2;

#[derive(Debug, Clone)]
struct Data {
    series: Series,
    /// Season numbers in order (from the season rows and the episodes).
    seasons: Vec<u32>,
    episodes: Vec<Episode>,
    /// This copy's own progress, by episode.
    progress: HashMap<ItemKey, WatchProgress>,
    next: Option<EpisodeId>,
    /// The other copies of the series (a stack while copies combine, DESIGN §29.14).
    others: Vec<SeriesId>,
    /// Of those, the ones whose episodes aren't loaded yet (Xtream loads them lazily).
    unloaded: Vec<SeriesId>,
    /// Progress matched across the copies by season and episode (P3): a copy's watched mark
    /// wins, else the latest progress — what a combined page shows.
    across: HashMap<ItemKey, WatchProgress>,
    /// The next episode from the copy with the newest progress, as this copy's episode.
    next_across: Option<EpisodeId>,
    /// An unfinished episode in another copy, including episodes absent from this copy.
    continuing_others: bool,
}

impl Data {
    fn season_episodes(&self, season: u32) -> Vec<&Episode> {
        self.episodes.iter().filter(|e| e.season == season).collect()
    }
}

fn load(c: &rusqlite::Connection, id: SeriesId, others: Vec<SeriesId>) -> crate::error::Result<Data> {
    use crate::store::{user, vod};
    let series = vod::series(c, id)?;
    let mut seasons: Vec<u32> = vod::seasons(c, id)?.into_iter().map(|s| s.number).collect();
    let episodes = vod::episodes(c, id, None)?;
    for e in &episodes {
        if !seasons.contains(&e.season) {
            seasons.push(e.season);
        }
    }
    seasons.sort_unstable();
    seasons.dedup();
    let mut progress = HashMap::new();
    for e in &episodes {
        if let Some(p) = user::progress(c, &episode_ref(e))? {
            progress.insert(e.key.clone(), p);
        }
    }
    let next = vod::next_episode(c, id)?.map(|e| e.id);
    let continuing_others = crate::store::copies::series_in_continue_watching(c, &others)?;
    let mut across = progress.clone();
    let (mut next_across, mut unloaded) = (next, Vec::new());
    if !others.is_empty() {
        use crate::store::copies;
        unloaded = copies::unloaded_series(c, &others)?;
        let theirs = copies::episode_progress(c, &others)?;
        for e in &episodes {
            let Some(p) = theirs.get(&(e.season, e.episode)) else { continue };
            match across.get(&e.key) {
                Some(own) if (own.finished, own.updated_at) >= (p.finished, p.updated_at) => {}
                _ => {
                    across.insert(e.key.clone(), p.clone());
                }
            }
        }
        // the copy with the newest progress decides; its episode as this copy's
        let all: Vec<SeriesId> = std::iter::once(id).chain(others.iter().copied()).collect();
        if let Some((s, ep)) = copies::next_episode(c, &all)? {
            next_across =
                if s == id { Some(ep.id) } else { episodes.iter().find(|x| (x.season, x.episode) == (ep.season, ep.episode)).map(|x| x.id).or(next) };
        }
    }
    Ok(Data { series, seasons, episodes, progress, next, others, unloaded, across, next_across, continuing_others })
}

fn episode_ref(e: &Episode) -> ItemRef {
    ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() }
}

/// "S01E03 · Pilot" ("S01E03 · Episode 3" for an untitled playlist episode).
fn episode_label(e: &Episode) -> String {
    format!("S{:02}E{:02} · {}", e.season, e.episode, rows::episode_title(e))
}

#[derive(Default)]
pub struct SeriesDetailScreen {
    id: Option<SeriesId>,
    data: Option<Data>,
    loader: Option<Loader<Data>>,
    missing: bool,
    details_requested: Option<SeriesId>,
    section: Section,
    btn: usize,
    season: usize,
    /// Cursor inside the current season's episodes.
    ep: usize,
    /// Place the cursor on the next episode when the data arrives (first load of a series).
    position_pending: bool,
    episode_pending: Option<(u32, u32)>,
    scroll_target: Option<ScrollTarget>,
    copies: super::copies::Copies,
    /// Episodes of other copies being loaded before a combined ✓ (DESIGN §29.15, P3).
    fetch: crate::ui::copies::EpisodeFetch,
    /// The ✓ marks waiting for them, each with the copies it still has to write (so leaving
    /// for another series never redirects them).
    pending_marks: Vec<PendingMark>,
}

/// A combined ✓ whose copies `copies` (episodes not loaded when it was made) are written once
/// the [`EpisodeFetch`](crate::ui::copies::EpisodeFetch) settles; the page's own copy and the
/// loaded ones were written at once.
#[derive(Debug, Clone)]
struct PendingMark {
    copies: Vec<SeriesId>,
    eps: Vec<(u32, u32)>,
    on: bool,
}

impl SeriesDetailScreen {
    fn reload(&mut self, ctx: &UiCtx<'_>) {
        let Some(id) = self.id else { return };
        // the other copies of the series while copies combine (their episodes match by
        // season and number)
        let others: Vec<SeriesId> = crate::ui::stacks::index_of(ctx.lib, VodKind::Series(id))
            .map(|i| crate::ui::stacks::copies_of(ctx.lib, ctx.config, false, i))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| match ctx.lib.vod.series.get(c as usize)?.kind {
                VodKind::Series(s) if s != id => Some(s),
                _ => None,
            })
            .collect();
        self.loader = Some(Loader::start(ctx, move |c| load(c, id, others)));
    }

    /// The progress the page shows by episode: matched across the copies on a combined page.
    fn shown_progress(&self) -> Option<&HashMap<ItemKey, WatchProgress>> {
        let d = self.data.as_ref()?;
        Some(if self.copies.combined() { &d.across } else { &d.progress })
    }

    fn finished(&self, e: &Episode) -> bool {
        self.shown_progress().and_then(|p| p.get(&e.key)).is_some_and(|p| p.finished)
    }

    fn season_number(&self) -> Option<u32> {
        self.data.as_ref().and_then(|d| d.seasons.get(self.season).copied())
    }

    fn current_episodes(&self) -> Vec<&Episode> {
        match (&self.data, self.season_number()) {
            (Some(d), Some(s)) => d.season_episodes(s),
            _ => Vec::new(),
        }
    }

    /// What the batch features act on: the focused episode, else the series.
    fn feature_target(&self) -> Option<crate::ui::item_menu::Target> {
        use crate::ui::item_menu::Target;
        let series = self.id?;
        Some(match self.focused_episode() {
            Some(e) => Target::Episode { series, episode: e.id },
            None => Target::Series(series),
        })
    }

    fn focused_episode(&self) -> Option<Episode> {
        self.current_episodes().get(self.ep).map(|e| (*e).clone())
    }

    /// The next episode to watch (button "Play S01E03"): the stored one, else the first.
    fn next_episode(&self) -> Option<Episode> {
        let d = self.data.as_ref()?;
        let next = if self.copies.combined() { d.next_across } else { d.next };
        next.and_then(|id| d.episodes.iter().find(|e| e.id == id)).or(d.episodes.first()).cloned()
    }

    fn series_ref(&self) -> Option<ItemRef> {
        self.data.as_ref().map(|d| ItemRef { source: d.series.source_id, kind: ItemKind::Series, key: d.series.key.clone() })
    }

    fn locked(&self, ctx: &UiCtx<'_>) -> bool {
        let locked = self
            .data
            .as_ref()
            .map(|d| d.series.locked)
            .or_else(|| self.id.and_then(|id| ctx.lib.vod.find(VodKind::Series(id))).map(|v| v.locked))
            .unwrap_or(false);
        locked && !kit::unlocked(ctx)
    }

    /// Put the cursor on `id` (switching season).
    fn select_episode(&mut self, id: EpisodeId) {
        let Some(d) = &self.data else { return };
        let Some(e) = d.episodes.iter().find(|e| e.id == id) else { return };
        if let Some(si) = d.seasons.iter().position(|s| *s == e.season) {
            self.season = si;
            self.ep = d.season_episodes(e.season).iter().position(|x| x.id == id).unwrap_or(0);
            self.scroll_target = Some(ScrollTarget::SelectedEpisode);
        }
    }

    fn set_season(&mut self, idx: usize) {
        let n = self.data.as_ref().map_or(0, |d| d.seasons.len());
        if n == 0 {
            return;
        }
        let idx = idx.min(n - 1);
        if idx != self.season {
            self.season = idx;
            self.ep = 0;
            self.scroll_target = Some(ScrollTarget::SelectedEpisode);
        }
    }

    fn play(&mut self, ctx: &mut UiCtx<'_>, e: &Episode, from_start: bool, alt: bool) {
        if self.locked(ctx) {
            kit::ask_pin(ctx, if alt { Action::ActivateAlt } else { Action::Activate }, "This series is locked");
            return;
        }
        // several copies: the same episode of the next copy is the fallback (DESIGN §29.15)
        if let Some(id) = self.id.filter(|_| self.copies.is_stacked()) {
            let plan = self.copies.plan().clone();
            crate::ui::copies::play_episode(ctx, &plan, id, e, from_start, true);
            return;
        }
        if from_start {
            kit::play_media(ctx, MediaRef::Episode(e.id), false, PlayOrigin::Fullscreen);
        } else {
            resume_or_play(ctx, MediaRef::Episode(e.id), &episode_ref(e), episode_label(e), PlayOrigin::Fullscreen);
        }
    }

    fn buttons(&self) -> Vec<Btn> {
        let mut buttons = if self.copies.is_stacked() {
            vec![Btn::Play, Btn::PlayFrom, Btn::Favourite, Btn::SeasonWatched]
        } else {
            vec![Btn::Play, Btn::Favourite, Btn::SeasonWatched]
        };
        if self.in_continue_watching() {
            buttons.push(Btn::RemoveContinue);
        }
        buttons
    }

    fn in_continue_watching(&self) -> bool {
        self.data
            .as_ref()
            .is_some_and(|d| d.progress.values().any(|p| !p.finished && p.position_s > 0.0) || self.copies.combined() && d.continuing_others)
    }

    fn remove_continue(&mut self, ctx: &mut UiCtx<'_>) {
        if !self.in_continue_watching() {
            return;
        }
        if self.copies.combined() {
            crate::ui::copies::forget_series(ctx, self.copies.plan());
        } else if let Some(id) = self.id {
            crate::ui::copies::write_with_undo(ctx, DataScope::Progress, "Removed from Continue watching".into(), move |c| {
                crate::store::copies::forget_series(c, &[id])
            });
        }
        self.btn = 0;
    }

    /// The copy Play uses when it isn't this page's (DESIGN §29.14).
    fn other_copy(&self) -> Option<u32> {
        self.copies.plays().filter(|c| Some(*c) != self.copies.page())
    }

    /// Play another copy: the episode its page would open at.
    fn play_copy(&mut self, ctx: &mut UiCtx<'_>, c: u32) {
        let Some(v) = ctx.lib.vod.series.get(c as usize).cloned() else { return };
        let VodKind::Series(id) = v.kind else { return };
        // a locked copy: the PIN first (its answer comes back through the Copies section)
        if v.locked && !kit::unlocked(ctx) {
            self.copies.request_play(ctx, c);
            return;
        }
        // its own next episode, with the next copy's as the fallback
        if !crate::ui::copies::play_title(ctx, false, c, Some(c), true) {
            super::movies::play_series(ctx, id, &v.name);
        }
    }

    /// The sections ↑/↓ and Tab walk, in order.
    fn sections(&self) -> Vec<Section> {
        if self.copies.is_stacked() {
            vec![Section::Buttons, Section::Copies, Section::Seasons, Section::Episodes]
        } else {
            vec![Section::Buttons, Section::Seasons, Section::Episodes]
        }
    }

    fn season_watched(&self) -> bool {
        let eps = self.current_episodes();
        !eps.is_empty() && eps.iter().all(|e| self.finished(e))
    }

    fn mark(&mut self, ctx: &mut UiCtx<'_>, eps: Vec<Episode>, on: bool) {
        if eps.is_empty() {
            return;
        }
        let now = ctx.now;
        // optimistic; the reload on DataScope::Progress confirms it
        if let Some(d) = &mut self.data {
            for e in &eps {
                for map in [&mut d.progress, &mut d.across] {
                    if on {
                        let p = map.entry(e.key.clone()).or_insert_with(|| WatchProgress {
                            item: episode_ref(e),
                            position_s: 0.0,
                            duration_s: None,
                            updated_at: now,
                            finished: true,
                            title: String::new(),
                            fingerprint: Default::default(),
                        });
                        p.finished = true;
                    } else {
                        map.remove(&e.key);
                    }
                }
            }
        }
        // a combined series (P3): the same episodes of every copy, matched by season and
        // number — after the copies whose episodes aren't loaded yet have loaded them
        if let (true, Some(d)) = (self.copies.combined(), self.data.as_ref())
            && !d.others.is_empty()
        {
            let keys: Vec<(u32, u32)> = eps.iter().map(|e| (e.season, e.episode)).collect();
            // this copy and the loaded ones now; the unloaded ones once their episodes are in
            let (later, now): (Vec<SeriesId>, Vec<SeriesId>) =
                std::iter::once(d.series.id).chain(d.others.iter().copied()).partition(|s| d.unloaded.contains(s));
            crate::ui::copies::mark_episodes(ctx, now, keys.clone(), on);
            if !later.is_empty() {
                self.fetch.start(ctx, &later);
                self.pending_marks.push(PendingMark { copies: later, eps: keys, on });
            }
            return;
        }
        let items: Vec<(ItemRef, String)> = eps.iter().map(|e| (episode_ref(e), episode_label(e))).collect();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| crate::store::user::mark_watched(c, &items, on, now));
    }

    /// The other copies' episodes settled: write the ✓ marks that waited for them, to the
    /// copies each one named (whatever page shows now); a copy that couldn't load is named.
    fn finish_pending_marks(&mut self, ctx: &mut UiCtx<'_>, failed: Vec<SeriesId>) {
        let mut missing: Vec<SeriesId> = Vec::new();
        for m in std::mem::take(&mut self.pending_marks) {
            let (lost, ok): (Vec<SeriesId>, Vec<SeriesId>) = m.copies.into_iter().partition(|s| failed.contains(s));
            crate::ui::copies::mark_episodes(ctx, ok, m.eps, m.on);
            for s in lost {
                if !missing.contains(&s) {
                    missing.push(s);
                }
            }
        }
        if !missing.is_empty() {
            let names = crate::ui::copies::series_playlists(ctx.lib, &missing);
            let text = format!("{names} couldn't load its episodes — marked in the other copies only");
            ctx.toast(crate::msg::Toast { kind: crate::msg::ToastKind::Warning, text, action: None, also: None, ttl_ms: 6000 });
        }
    }

    fn press(&mut self, ctx: &mut UiCtx<'_>, b: Btn) {
        match b {
            Btn::Play => {
                if let Some(c) = self.other_copy() {
                    self.play_copy(ctx, c);
                } else if let Some(e) = self.next_episode() {
                    self.play(ctx, &e, false, false);
                }
            }
            Btn::PlayFrom => self.copies.play_from_menu(ctx),
            Btn::Favourite if self.copies.combined() => {
                // add: the copy Play would use; remove: every copy, with Undo (DESIGN §29.15)
                let plan = self.copies.plan().clone();
                crate::ui::copies::toggle_favourite_of(ctx, &plan, self.copies.plays());
            }
            Btn::Favourite => {
                if let Some(item) = self.series_ref() {
                    kit::toggle_favourite(ctx, &item);
                }
            }
            Btn::SeasonWatched => {
                let on = !self.season_watched();
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, on);
            }
            Btn::RemoveContinue => self.remove_continue(ctx),
        }
    }

    fn move_vertical(&mut self, down: bool) {
        let n_eps = self.current_episodes().len();
        let sections = self.sections();
        let at = sections.iter().position(|s| *s == self.section).unwrap_or(0);
        match (self.section, down) {
            (Section::Episodes, false) if self.ep > 0 => self.ep -= 1,
            (Section::Episodes, true) => self.ep = (self.ep + 1).min(n_eps.saturating_sub(1)),
            (_, true) => self.section = sections[(at + 1).min(sections.len() - 1)],
            (_, false) => self.section = sections[at.saturating_sub(1)],
        }
        if self.section == Section::Copies && self.copies.focus.is_none() {
            self.copies.focus = Some(0);
        }
        self.scroll_target = Some(ScrollTarget::Cursor);
    }

    /// After an episode ended: the episode after `key` (next in season order), if any.
    fn after(&self, key: &ItemKey) -> Option<Episode> {
        let d = self.data.as_ref()?;
        let i = d.episodes.iter().position(|e| &e.key == key)?;
        d.episodes.get(i + 1).cloned()
    }
}

impl Screen for SeriesDetailScreen {
    fn id(&self) -> ScreenId {
        ScreenId::SeriesDetail
    }
    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        if self.copies.is_stacked() { vec![(Action::PlayFrom, "play from")] } else { Vec::new() }
    }
    fn reveal_target(&self, _ctx: &UiCtx<'_>) -> Option<crate::ui::item_menu::Target> {
        self.feature_target()
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Detail]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let (id, episode) = match route {
            Route::SeriesDetail(id) => (id, None),
            Route::SeriesDetailAt { series, season, episode } => (series, Some((*season, *episode))),
            _ => return,
        };
        if self.id != Some(*id) {
            self.data = None;
            self.section = Section::Episodes;
            self.season = 0;
            self.ep = 0;
            self.btn = 0;
            self.position_pending = true;
            self.episode_pending = None;
        }
        self.id = Some(*id);
        if let Some((season, episode)) = episode {
            self.episode_pending = Some((season, episode));
            self.section = Section::Episodes;
            self.position_pending = true;
            // Reopening a cached page must focus the requested row before another key
            // can act. Once applied, a refresh must keep any later user selection.
            if let Some(id) = self.data.as_ref().and_then(|d| d.episodes.iter().find(|e| (e.season, e.episode) == (season, episode))).map(|e| e.id) {
                self.select_episode(id);
                self.position_pending = false;
                self.episode_pending = None;
            }
        }
        self.missing = false;
        self.reload(ctx);
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(DataScope::VodDetails | DataScope::Progress | DataScope::Overrides | DataScope::Vod | DataScope::All) => {
                self.reload(ctx);
                self.copies.reload(ctx);
            }
            AppEvent::Job(JobEvent::Finished { kind: JobKind::LoadSeriesDetails(s), outcome: Err(msg), .. }) if Some(*s) == self.id => {
                ctx.toast(crate::msg::Toast::error(format!("Couldn't load the episodes: {}", crate::redact::redact(msg))));
            }
            AppEvent::Player(PlayerEvent::Ended { item: Some(item), .. }) if item.kind == ItemKind::Episode => {
                // this page shows the series: follow along
                let shown = self.data.as_ref().and_then(|d| d.episodes.iter().find(|e| e.key == item.key && e.source_id == item.source)).cloned();
                // (the "Next episode in 10 s" card is the fullscreen player's, DESIGN §13.6)
                if let Some(ended) = &shown {
                    self.reload(ctx);
                    if let Some(n) = self.after(&ended.key) {
                        self.select_episode(n.id);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        // the batch features' keys on the focused episode, else the series (Ctrl+N…)
        if let Some(t) = self.feature_target()
            && crate::ui::item_menu::dispatch(ctx, action, &t)
        {
            return true;
        }
        match action {
            Action::Up => self.move_vertical(false),
            Action::Down => self.move_vertical(true),
            Action::Left | Action::Right => {
                self.scroll_target = Some(ScrollTarget::Cursor);
                let d: isize = if action == Action::Left { -1 } else { 1 };
                match self.section {
                    Section::Buttons => self.btn = (self.btn as isize + d).clamp(0, self.buttons().len() as isize - 1) as usize,
                    Section::Copies => {
                        let k = self.copies.focus.unwrap_or(0) as isize + d;
                        self.copies.focus = Some(k.clamp(0, self.copies.len().saturating_sub(1) as isize) as usize);
                    }
                    Section::Seasons => self.set_season((self.season as isize + d).max(0) as usize),
                    Section::Episodes => {}
                }
            }
            Action::TabPrev => self.set_season(self.season.saturating_sub(1)),
            Action::TabNext => self.set_season(self.season + 1),
            Action::FocusNext | Action::FocusPrev => {
                self.scroll_target = Some(ScrollTarget::Cursor);
                let order = self.sections();
                let n = order.len();
                let i = order.iter().position(|s| *s == self.section).unwrap_or(0);
                let next = if action == Action::FocusNext { (i + 1) % n } else { (i + n - 1) % n };
                self.section = order[next];
                if self.section == Section::Copies && self.copies.focus.is_none() {
                    self.copies.focus = Some(0);
                }
            }
            // a mini-card: Enter picks the copy Play uses, the menu key opens its menu
            Action::Activate | Action::ContextMenu if self.section == Section::Copies => {
                let k = self.copies.focus.unwrap_or(0);
                let click = if action == Action::Activate { super::copies::Click::Pick(k) } else { super::copies::Click::Menu(k) };
                self.copies.click(ctx, click);
            }
            Action::PlayFrom => self.copies.play_from_menu(ctx),
            Action::PageUp | Action::Home if self.section == Section::Episodes => {
                self.ep = 0;
                self.scroll_target = Some(ScrollTarget::Cursor);
            }
            Action::PageDown | Action::End if self.section == Section::Episodes => {
                self.ep = self.current_episodes().len().saturating_sub(1);
                self.scroll_target = Some(ScrollTarget::Cursor);
            }
            Action::Activate => match self.section {
                Section::Buttons => {
                    if let Some(b) = self.buttons().get(self.btn).copied() {
                        self.press(ctx, b);
                    }
                }
                Section::Copies | Section::Seasons => self.section = Section::Episodes,
                Section::Episodes => {
                    if let Some(e) = self.focused_episode() {
                        self.play(ctx, &e, false, false);
                    }
                }
            },
            Action::ActivateAlt => {
                let e = if self.section == Section::Episodes { self.focused_episode() } else { self.next_episode() };
                if let Some(e) = e {
                    self.play(ctx, &e, true, true);
                }
            }
            Action::MarkWatched => {
                if let Some(e) = self.focused_episode() {
                    let on = !self.finished(&e);
                    self.mark(ctx, vec![e], on);
                }
            }
            Action::Remove if self.section == Section::Buttons => self.remove_continue(ctx),
            Action::ToggleFavourite if self.section == Section::Episodes => {
                if let Some(e) = self.focused_episode() {
                    kit::toggle_favourite(ctx, &ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key });
                }
            }
            Action::AddToList if self.section == Section::Episodes => {
                if let Some(e) = self.focused_episode() {
                    let title = format!("S{}E{}", e.season, e.episode);
                    kit::list_picker(ctx, &ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }, &title);
                }
            }
            Action::ToggleFavourite => self.press(ctx, Btn::Favourite),
            // a combined series: lock, hide and rename cover every copy, with Undo
            Action::ToggleLock | Action::Rename | Action::ToggleHidden if self.copies.combined() => {
                let plan = self.copies.plan().clone();
                let name = self.data.as_ref().map(|d| d.series.display_name().to_owned()).unwrap_or_default();
                match action {
                    Action::ToggleLock => crate::ui::copies::toggle_lock(ctx, &plan, false),
                    Action::Rename => crate::ui::copies::rename_dialog(ctx, &plan, &name),
                    _ => crate::ui::copies::hide(ctx, &plan, &name),
                }
            }
            Action::ToggleLock => {
                if let Some((item, d)) = self.series_ref().zip(self.data.as_ref()) {
                    let by_group = kit::any_group_locked(&ctx.lib.vod.series_groups, &d.series.group_ids);
                    kit::toggle_lock(ctx, &item, d.series.locked, by_group);
                }
            }
            Action::AddToList | Action::Rename | Action::ToggleHidden => {
                if let Some((item, d)) = self.series_ref().zip(self.data.as_ref()) {
                    let name = d.series.display_name().to_owned();
                    match action {
                        Action::AddToList => kit::list_picker(ctx, &item, &name),
                        Action::Rename => kit::rename_item(ctx, &item, &name),
                        _ => kit::hide_item(ctx, &item, &name),
                    }
                }
            }
            Action::ContextMenu => {
                let mi = |icon: &'static str, label: &str, action: Action, shortcut: Option<&str>| MenuItem {
                    icon,
                    label: label.into(),
                    action,
                    enabled: true,
                    shortcut: shortcut.map(str::to_owned),
                };
                let every = self.copies.combined();
                let mut episode = mi(glyphs::CHECK, "Mark episode watched / unwatched", Action::MarkWatched, Some("Ctrl+W"));
                episode.enabled = self.focused_episode().is_some();
                let mut items = Vec::new();
                // several copies (DESIGN §29.14): pick one; take this one apart
                if self.copies.is_stacked() {
                    items.push(mi(glyphs::PLAY, "Play from…", Action::PlayFrom, Some("Ctrl+O")));
                    items.push(mi(glyphs::CLOSE, "Not the same title", Action::Digit(super::copies::CARD_SPLIT), None));
                    self.copies.aim_at_page();
                }
                items.extend([
                    mi(glyphs::CHECK, "Mark season watched", Action::Digit(MENU_SEASON_WATCHED), None),
                    mi(glyphs::CLOSE, "Mark season unwatched", Action::Digit(MENU_SEASON_UNWATCHED), None),
                    episode,
                ]);
                if self.in_continue_watching() {
                    items.push(mi(glyphs::CLOSE, "Remove from Continue watching", Action::Remove, Some("Delete")));
                }
                items.extend([
                    mi(glyphs::FAVOURITE, "Save to / remove from My list", Action::ToggleFavourite, Some("Ctrl+F")),
                    mi(glyphs::FAV_LIST, "Watchlists…", Action::AddToList, Some("Ctrl+Shift+F")),
                    mi(glyphs::TAG, if every { "Rename every copy…" } else { "Rename…" }, Action::Rename, Some("F2")),
                    mi(glyphs::LOCK, if every { "Lock / unlock every copy" } else { "Lock / unlock" }, Action::ToggleLock, Some("Ctrl+L")),
                    mi(glyphs::HIDDEN, if every { "Hide every copy" } else { "Hide" }, Action::ToggleHidden, Some("Ctrl+H")),
                ]);
                // combine it with a title the matcher kept apart (DESIGN §29.16; two series
                // playlists or more)
                if crate::ui::stacks::combine(ctx.config) && ctx.lib.eligible(ItemKind::Series).len() > 1 {
                    items.push(mi(glyphs::PLAYLIST, "Same title as…", Action::Digit(super::copies::SAME_TITLE), None));
                }
                if let Some(t) = self.feature_target() {
                    crate::ui::item_menu::extend(&mut items, &t, ctx, KeyContext::Detail);
                }
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Series".into(), items }));
            }
            // this is the details page already
            Action::ShowDetails => {}
            Action::MoveUp | Action::MoveDown | Action::SortMenu | Action::OpenInGuide | Action::AssignEpg | Action::Remove => kit::not_here(ctx),
            _ => return false,
        }
        true
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        // the Copies section's menus and PIN answers first (DESIGN §29.14)
        match self.copies.on_result(ctx, &result) {
            super::copies::Outcome::NotMine => {}
            super::copies::Outcome::Done => return,
            super::copies::Outcome::Act(super::copies::CopyAct::Play(c)) => return self.play_copy(ctx, c),
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Series(id))) => return ctx.push(Route::SeriesDetail(id)),
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Movie(id))) => return ctx.push(Route::MovieDetail(id)),
        }
        match result {
            OverlayResult::Chosen(Action::PlayFrom) => self.copies.play_from_menu(ctx),
            OverlayResult::Chosen(Action::Remove) => self.remove_continue(ctx),
            OverlayResult::Chosen(Action::Digit(super::copies::SAME_TITLE)) => {
                if let Some((item, d)) = self.series_ref().zip(self.data.as_ref()) {
                    let name = d.series.display_name().to_owned();
                    crate::ui::copies::same_title_picker(ctx, item, &name);
                }
            }
            OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)) => {
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, true);
            }
            OverlayResult::Chosen(Action::Digit(MENU_SEASON_UNWATCHED)) => {
                let eps = self.current_episodes().into_iter().cloned().collect();
                self.mark(ctx, eps, false);
            }
            OverlayResult::Chosen(
                a @ (Action::MarkWatched | Action::ToggleFavourite | Action::AddToList | Action::Rename | Action::ToggleLock | Action::ToggleHidden),
            ) => {
                self.on_action(ctx, a);
            }
            OverlayResult::Unlocked(a @ (Action::Activate | Action::ActivateAlt | Action::ToggleLock)) => {
                self.on_action(ctx, a);
            }
            // a feature's choice from the menu
            OverlayResult::Chosen(a) => {
                if let Some(t) = self.feature_target() {
                    crate::ui::item_menu::dispatch(ctx, a, &t);
                }
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        if let Some(id) = self.id.filter(|_| !self.missing) {
            self.copies.sync(ctx, VodKind::Series(id));
        }
        if let Some(failed) = self.fetch.poll(ctx, ui.ctx()) {
            self.finish_pending_marks(ctx, failed);
        }
        match kit::poll(&mut self.loader) {
            Some(Ok(d)) => {
                let lazy = d.series.details_loaded_at.is_none() && ctx.lib.source(d.series.source_id).is_some_and(|s| s.kind == SourceKind::Xtream);
                if lazy && self.details_requested != Some(d.series.id) {
                    self.details_requested = Some(d.series.id);
                    ctx.services.jobs.submit(JobKind::LoadSeriesDetails(d.series.id));
                }
                let target = self.episode_pending.and_then(|(s, n)| d.episodes.iter().find(|e| (e.season, e.episode) == (s, n)).map(|e| e.id));
                let next = target.or(if self.copies.combined() { d.next_across } else { d.next });
                let had_episodes = !d.episodes.is_empty();
                self.data = Some(d);
                if self.position_pending && had_episodes && (self.episode_pending.is_none() || target.is_some() || !lazy) {
                    self.position_pending = false;
                    self.episode_pending = None;
                    if let Some(n) = next {
                        self.select_episode(n);
                    }
                }
                let n = self.data.as_ref().map_or(0, |d| d.seasons.len());
                self.season = self.season.min(n.saturating_sub(1));
                self.ep = self.ep.min(self.current_episodes().len().saturating_sub(1));
                self.missing = false;
            }
            Some(Err(crate::error::Error::NotFound(_))) => self.missing = true,
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read this series", &e),
            None => {}
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        if self.missing || self.id.is_none() {
            detail_bar(ui, ctx, false);
            empty_state::empty_state(ui, theme, glyphs::SERIES, "This series is no longer in your playlists", None, None);
            return;
        }
        if self.section == Section::Copies && !self.copies.is_stacked() {
            self.section = Section::Buttons;
        }
        self.btn = self.btn.min(self.buttons().len() - 1);
        let summary = self.id.and_then(|id| ctx.lib.vod.find(VodKind::Series(id))).cloned();
        // DESIGN §27.7: a playlist series is complete at import (no details to wait for)
        let source = self.data.as_ref().map(|d| d.series.source_id).or(summary.as_ref().map(|s| s.source_id));
        let sparse = !source.and_then(|s| ctx.lib.source(s)).is_some_and(|s| s.kind == SourceKind::Xtream);
        let Some(data) = self.data.clone() else {
            // list-level data while the page loads
            let title = summary.as_ref().map(|s| s.name.to_string()).unwrap_or_default();
            detail_bar(ui, ctx, false);
            let header = DetailHeader {
                title: &title,
                meta: "",
                text: &[],
                poster: summary.as_ref().and_then(|s| s.poster.as_deref()),
                backdrop: None,
                playing: false,
                locked: false,
                loading: !sparse,
                source,
                kind: ItemKind::Series,
            };
            header.show(ui, ctx, |_, _| {});
            return;
        };
        let s = &data.series;
        let mut meta = Vec::new();
        if let Some(y) = s.year {
            meta.push(y.to_string());
        }
        if let Some(g) = s.genre.clone().filter(|g| !g.is_empty()) {
            meta.push(g);
        }
        if let Some(r) = s.rating.filter(|r| *r > 0.0) {
            meta.push(format!("★ {r:.1}"));
        }
        let mut text = Vec::new();
        let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
        let series_norm = crate::spoiler::series_key(&s.name);
        let subj = crate::guard::Subject::Vod { title: s.display_name(), series_norm: Some(&series_norm) };
        if let Some(p) = s.plot.as_deref().filter(|p| !p.trim().is_empty()).and_then(|p| spoilers.text(&subj, crate::guard::Field::Plot, p)) {
            text.push((p.into_owned(), false));
        }
        if let Some(c) = s.cast.as_deref().filter(|c| !c.trim().is_empty()) {
            text.push((format!("Cast: {c}"), true));
        }
        if sparse {
            meta.extend(sparse_meta(ctx, &s.group_ids));
            if text.is_empty() {
                text.push((NO_DESCRIPTION.to_owned(), true));
            }
        }
        let playing_ep = ctx.player.playable.as_ref().and_then(|p| match p.media {
            MediaRef::Episode(id) => Some(id),
            _ => None,
        });
        let playing = playing_ep.is_some_and(|id| data.episodes.iter().any(|e| e.id == id));
        let fav = if self.copies.combined() { self.copies.any_favourite(ctx) } else { self.series_ref().is_some_and(|i| ctx.favs.is_fav(&i)) };
        let next = self.next_episode();
        let shown = self.shown_progress().cloned().unwrap_or_default();
        let fetching = self.fetch.busy().then(|| self.fetch.progress());
        // a combined page resumes from whichever copy is furthest along (`shown`)
        let resume_at = next.as_ref().and_then(|e| shown.get(&e.key)).filter(|p| resumable(p, ctx.config.player.resume_min_s)).map(|p| p.position_s);
        let season_watched = self.season_watched();
        let loading = data.episodes.is_empty() && (self.loader.is_some() || !sparse && s.details_loaded_at.is_none());
        let (section, btn_focus, season_idx, ep_idx) = (self.section, self.btn, self.season, self.ep);
        let target = self.scroll_target.take();
        let scroll = target == Some(ScrollTarget::Cursor);
        let buttons = self.buttons();
        // several copies: Play names the playlist it plays from; another copy opens at its own
        // next episode
        let other = self.other_copy();
        let plays_from = self.copies.plays().and_then(|c| self.copies.playlist(ctx, c)).map(str::to_owned);
        let mut copy_click = None;
        let mut clicked_btn = None;
        let mut clicked_season = None;
        let mut clicked_ep: Option<(usize, bool)> = None;
        let mut menu = detail_bar(ui, ctx, true);
        let mut ep_menu = None;
        egui::ScrollArea::vertical().id_salt("series-detail").auto_shrink([false, false]).show(ui, |ui| {
            let top = ui.cursor().min;
            let mut button_rect = None;
            let header = DetailHeader {
                title: s.display_name(),
                meta: &meta.join(" · "),
                text: &text,
                poster: s.cover.as_deref(),
                backdrop: s.backdrop.as_deref(),
                playing,
                locked: s.locked,
                loading: false,
                source: Some(s.source_id),
                kind: ItemKind::Series,
            };
            menu |= header.show(ui, ctx, |ui, ctx| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Md));
                    for (i, b) in buttons.iter().enumerate() {
                        let (icon, label, primary, selected) = match b {
                            Btn::Play => {
                                let label = match (&next, resume_at) {
                                    _ if other.is_some() => "Play".into(),
                                    (Some(e), Some(at)) => format!("Resume S{:02}E{:02} {}", e.season, e.episode, kit::clock(at)),
                                    (Some(e), None) => format!("Play S{:02}E{:02}", e.season, e.episode),
                                    (None, _) => "Play".into(),
                                };
                                let label = match &plays_from {
                                    Some(p) => format!("{label} · {p}"),
                                    None => label,
                                };
                                (glyphs::PLAY, label, true, false)
                            }
                            Btn::PlayFrom => (glyphs::PLAYLIST, "Play from ▾".into(), false, false),
                            Btn::Favourite => (
                                if fav { glyphs::FAVOURITE } else { glyphs::FAVOURITE_OFF },
                                if fav { "My list".into() } else { "Save to My list".into() },
                                false,
                                fav,
                            ),
                            Btn::SeasonWatched => (
                                glyphs::CHECK,
                                if season_watched { "Season watched".into() } else { "Mark season watched".into() },
                                false,
                                season_watched,
                            ),
                            Btn::RemoveContinue => (glyphs::CLOSE, "Remove from Continue watching".into(), false, false),
                        };
                        let style = ButtonStyle {
                            primary,
                            selected,
                            bordered: true,
                            focused: section == Section::Buttons && i == btn_focus,
                            ..Default::default()
                        };
                        let enabled = !matches!(b, Btn::Play) || next.is_some() || other.is_some();
                        let resp = ui.add_enabled_ui(enabled, |ui| button(ui, ctx.theme, Some(icon), &label, style)).inner;
                        if section == Section::Buttons && i == btn_focus {
                            button_rect = Some(resp.rect);
                        }
                        if resp.clicked() {
                            clicked_btn = Some(i);
                        }
                    }
                });
                if let Some(item) = self.series_ref() {
                    if button(ui, ctx.theme, Some(glyphs::ADD), "Watchlist ▾", ButtonStyle { bordered: true, ..Default::default() }).clicked() {
                        kit::list_picker(ctx, &item, s.display_name());
                    }
                    if let Some(line) = crate::ui::watchlists::membership_line(ctx, &item) {
                        ui.label(line);
                    }
                }
            });
            if scroll && let Some(rect) = button_rect {
                let header = egui::Rect::from_min_max(top, ui.cursor().min);
                // Show the title as well when it fits; a long synopsis must not keep the
                // focused control below the viewport.
                ui.scroll_to_rect(if header.height() <= ui.clip_rect().height() { rect.union(header) } else { rect }, None);
            }
            if let Some((done, n)) = fetching {
                // a combined ✓ waits for the other copies' episodes (P3)
                ui.add_space(theme.space(SpaceToken::Md));
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(theme.font(TypeToken::Body)));
                    let what =
                        if n == 1 { "the other copy's episodes".to_owned() } else { format!("the episodes of {n} other copies ({done} done)") };
                    ui.label(
                        egui::RichText::new(format!("Loading {what} to mark them too…"))
                            .size(theme.font(TypeToken::BodySmall))
                            .color(t.text_secondary),
                    );
                });
            }
            if section != Section::Copies {
                self.copies.focus = None;
            }
            copy_click = self.copies.show(ui, ctx, scroll && section == Section::Copies);
            ui.add_space(theme.space(SpaceToken::Xxl));
            if data.seasons.is_empty() {
                if loading {
                    section::section_header(ui, theme, "Episodes");
                    for _ in 0..4 {
                        let (r, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), rows::row_height(theme, ctx.config.interface.density, false) - 4.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(r, t.rounding, t.fill_field);
                        ui.add_space(4.0);
                    }
                } else {
                    empty_state::empty_state(
                        ui,
                        theme,
                        glyphs::SERIES,
                        "No episodes yet",
                        Some("The provider lists no episodes for this series."),
                        None,
                    );
                }
                return;
            }
            if self.section == Section::Episodes
                && let Some(e) = self.focused_episode()
            {
                let item = episode_ref(&e);
                let label = format!("S{}E{}", e.season, e.episode);
                ui.horizontal_wrapped(|ui| {
                    ui.label(&label);
                    if button(ui, ctx.theme, Some(glyphs::ADD), "Watchlist ▾", ButtonStyle::default()).clicked() {
                        kit::list_picker(ctx, &item, &label);
                    }
                    if let Some(line) = crate::ui::watchlists::membership_line(ctx, &item) {
                        ui.label(line);
                    }
                });
            }
            // season tabs
            let seasons_rect = ui
                .horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Xs);
                    for (i, n) in data.seasons.iter().enumerate() {
                        let style =
                            ButtonStyle { selected: i == season_idx, focused: section == Section::Seasons && i == season_idx, ..Default::default() };
                        let label = if *n == 0 { "Specials".to_owned() } else { format!("Season {n}") };
                        if button(ui, theme, None, &label, style).clicked() {
                            clicked_season = Some(i);
                        }
                    }
                    ui.label(egui::RichText::new("[ ]").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                })
                .response
                .rect;
            if scroll && section == Section::Seasons {
                ui.scroll_to_rect(seasons_rect, None);
            }
            ui.add_space(theme.space(SpaceToken::Md));
            let season = data.seasons.get(season_idx).copied().unwrap_or(0);
            for (i, e) in data.season_episodes(season).into_iter().enumerate() {
                // the spoiler guard's view of the episode (title and plot after next-up)
                let e = &*spoilers.episode(&series_norm, e);
                let p = shown.get(&e.key);
                let watched = p.is_some_and(|p| p.finished);
                let cursor = section == Section::Episodes && i == ep_idx;
                let selected = section != Section::Episodes && i == ep_idx;
                let resp =
                    rows::episode_row(ui, theme, e, p.and_then(fraction), watched, playing_ep == Some(e.id), RowState::from_flags(cursor, selected));
                // Selection changes (playback / season) reveal the episode even when
                // focus stays on buttons or copies; cursor moves reveal only the cursor.
                if cursor && scroll || i == ep_idx && target == Some(ScrollTarget::SelectedEpisode) {
                    let rect = if i == 0 { resp.rect.union(seasons_rect) } else { resp.rect };
                    ui.scroll_to_rect(rect, None);
                }
                // a click plays it, as Enter does (a double-click plays once)
                if crate::ui::widgets::fresh_click(&resp) {
                    clicked_ep = Some((i, true));
                } else if resp.secondary_clicked() {
                    ep_menu = Some(i);
                }
            }
        });
        if let Some(i) = clicked_btn {
            self.section = Section::Buttons;
            self.btn = i;
            let b = self.buttons()[i];
            self.press(ctx, b);
        }
        if let Some(click) = copy_click {
            self.section = Section::Copies;
            self.copies.click(ctx, click);
            menu = false;
        }
        if let Some(i) = clicked_season {
            self.section = Section::Seasons;
            self.set_season(i);
        }
        if let Some((i, play)) = clicked_ep {
            self.section = Section::Episodes;
            self.ep = i;
            if play && let Some(e) = self.focused_episode() {
                self.play(ctx, &e, false, false);
            }
        }
        // right-click on an episode: the menu key on it (its watched item applies to it)
        if let Some(i) = ep_menu {
            self.section = Section::Episodes;
            self.ep = i;
            menu = true;
        }
        if menu {
            self.on_action(ctx, Action::ContextMenu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceId, StreamLocator};

    fn ep(id: i64, season: u32, n: u32) -> Episode {
        Episode {
            id: EpisodeId(id),
            series_id: SeriesId(1),
            source_id: SourceId(1),
            key: ItemKey(format!("e{id}")),
            provider_id: None,
            season,
            episode: n,
            title: format!("Episode {n}"),
            plot: None,
            duration_s: Some(2700),
            still: None,
            air_date: None,
            rating: None,
            stream: StreamLocator::Url { url: "http://x".into() },
        }
    }

    fn screen() -> SeriesDetailScreen {
        let series = Series {
            id: SeriesId(1),
            source_id: SourceId(1),
            key: ItemKey("s".into()),
            provider_id: None,
            name: "Show".into(),
            clean_name: "Show".into(),
            rename: None,
            year: None,
            group_ids: vec![],
            cover: None,
            backdrop: None,
            rating: None,
            plot: None,
            genre: None,
            cast: None,
            director: None,
            release_date: None,
            last_modified: None,
            added_at: None,
            provider_order: 0,
            hidden: false,
            locked: false,
            details_loaded_at: Some(1),
        };
        let episodes = vec![ep(1, 1, 1), ep(2, 1, 2), ep(3, 2, 1), ep(4, 2, 2), ep(5, 2, 3)];
        SeriesDetailScreen {
            id: Some(SeriesId(1)),
            data: Some(Data {
                series,
                seasons: vec![1, 2],
                episodes,
                progress: HashMap::new(),
                next: Some(EpisodeId(4)),
                others: Vec::new(),
                unloaded: Vec::new(),
                across: HashMap::new(),
                next_across: Some(EpisodeId(4)),
                continuing_others: false,
            }),
            ..Default::default()
        }
    }

    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed, until};

    fn ep_progress(t: &crate::ui::testing::TestUi, key: &str) -> Option<WatchProgress> {
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Episode, key: ItemKey(key.into()) };
        crate::store::user::progress(&t.services().store.reader().unwrap(), &item).unwrap()
    }

    #[test]
    fn returning_during_a_load_keeps_pending_and_cached_episode_selections() {
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            Ok(testkit::series(c, src, 1, 2, 3)?[0])
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::SeriesDetailAt { series: id, season: 2, episode: 3 });
        // The app consumes the route target immediately, while the page is still loading.
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "requested episode", |h| h.state().s.focused_episode().is_some_and(|e| (e.season, e.episode) == (2, 3)));
        // A cached page applies the target immediately. Moving before its refresh arrives
        // must survive that refresh as well as re-entering the consumed route.
        h.state_mut().enter(Route::SeriesDetailAt { series: id, season: 2, episode: 1 });
        h.state_mut().act(Action::Down);
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "refreshed page", |h| h.state().s.loader.is_none());
        assert_eq!(h.state().s.focused_episode().map(|e| (e.season, e.episode)), Some((2, 2)));
    }

    #[test]
    fn keyboard_scroll_returns_from_last_episode_to_seasons_and_page_header() {
        use egui_kittest::kittest::Queryable;
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            Ok(testkit::series(c, s, 1, 1, 20)?[0])
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(800.0, 500.0));
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "episodes", |h| has_label(h, "Episode 20"));
        h.run_steps(30);
        h.state_mut().act(Action::End);
        h.run_steps(30);
        let last = h.get_by_label("Episode 20").rect();
        assert!(last.top() >= 8.0 && last.bottom() <= 492.0, "last episode: {last:?}");
        h.event(egui::Event::PointerMoved(egui::pos2(400.0, 250.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -10000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(30);
        assert!((h.get_by_label("Episode 20").rect().bottom() - last.bottom()).abs() < 1.0, "keyboard reaches touchpad bottom");
        h.event(egui::Event::PointerGone);
        h.state_mut().act(Action::Home);
        h.run_steps(30);
        h.state_mut().act(Action::Up);
        h.run_steps(30);
        let seasons = h.get_by_label("Season 1").rect();
        assert!(seasons.top() >= 8.0 && seasons.bottom() <= 492.0, "season tabs: {seasons:?}");
        h.state_mut().act(Action::Up);
        h.run_steps(30);
        let title = h.get_by_label("Series 0").rect();
        let play = h.get_by_label("Play S01E01").rect();
        assert!(title.top() >= 8.0 && play.bottom() <= 492.0, "header and controls: {title:?} {play:?}");
        h.event(egui::Event::PointerMoved(egui::pos2(400.0, 250.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 10000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(30);
        assert!((h.get_by_label("Series 0").rect().top() - title.top()).abs() < 1.0, "keyboard reaches touchpad top");
    }

    #[test]
    fn ended_episode_and_season_changes_reveal_selection_while_buttons_have_focus() {
        use egui_kittest::kittest::Queryable;
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            Ok(testkit::series(c, s, 1, 2, 20)?[0])
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(800.0, 500.0));
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "episodes", |h| has_label(h, "Episode 20"));
        h.run_steps(30);
        h.state_mut().act(Action::FocusNext);
        h.run_steps(30);
        assert_eq!(h.state().s.section, Section::Buttons);
        assert!(h.get_by_label("Episode 20").rect().top() > 492.0, "next episode starts below the viewport");
        let ended = h.state().s.current_episodes()[18].clone();
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(episode_ref(&ended)), finished: true }));
        h.run_steps(30);
        assert_eq!(h.state().s.section, Section::Buttons, "playback does not move focus");
        assert_eq!(h.state().s.ep, 19);
        let next = h.get_by_label("Episode 20").rect();
        assert!(next.top() >= 8.0 && next.bottom() <= 492.0, "next episode revealed: {next:?}");

        h.state_mut().act(Action::TabNext);
        h.run_steps(30);
        assert_eq!(h.state().s.section, Section::Buttons);
        assert_eq!((h.state().s.season, h.state().s.ep), (1, 0));
        let first = h.get_by_label("Episode 1").rect();
        assert!(first.top() >= 8.0 && first.bottom() <= 492.0, "new season's selected episode revealed: {first:?}");
        // A subsequent cursor move still reveals the controls, not the selected episode.
        h.state_mut().act(Action::Right);
        h.run_steps(30);
        assert!(h.get_by_label("Series 0").rect().top() >= 8.0);
    }

    #[test]
    fn page_opens_at_the_next_episode_seasons_marks_and_follows_an_ended_episode() {
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::series(c, s, 1, 2, 3)?;
            let p = |key: &str, pos: f64, finished: bool, at: i64| WatchProgress {
                item: ItemRef { source: s, kind: ItemKind::Episode, key: ItemKey(key.into()) },
                position_s: pos,
                duration_s: Some(2700.0),
                updated_at: at,
                finished,
                title: String::new(),
                fingerprint: Default::default(),
            };
            crate::store::user::save_progress(c, &p("m3u:ep0-1-1", 2600.0, true, 5))?;
            crate::store::user::save_progress(c, &p("m3u:ep0-1-2", 600.0, false, 6))?;
            Ok(ids)
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(ids[0]));
        until(&mut h, "the episodes", |h| has_label(h, "Resume S01E02 10:00"));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Detail]);
        assert!(has_label(&h, "Season 1") && has_label(&h, "Season 2"));
        assert_eq!((h.state().s.season, h.state().s.ep), (0, 1), "opens at the episode in progress");
        // Enter on it: the resume prompt
        h.state_mut().act(Action::Activate);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Resume(r))] if r.position_s == 600.0), "{nav:?}");
        // ] next season, [ back; Ctrl+W marks the focused episode
        h.state_mut().act(Action::TabNext);
        assert_eq!((h.state().s.season, h.state().s.ep), (1, 0));
        h.state_mut().act(Action::MarkWatched);
        until(&mut h, "S2E1 watched", |h| ep_progress(&h.state().t, "m3u:ep0-2-1").is_some_and(|p| p.finished));
        // the context menu marks the whole season
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items[0].label == "Mark season watched"), "{nav:?}");
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)));
        until(&mut h, "season 2 watched", |h| (1..=3).all(|e| ep_progress(&h.state().t, &format!("m3u:ep0-2-{e}")).is_some_and(|p| p.finished)));
        h.state_mut().act(Action::TabPrev);
        assert_eq!(h.state().s.season, 0);
        // Shift+Enter on S1E3 plays from the start, and the page remembers it started it
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ActivateAlt);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)]);
        let s13 = h.state().s.focused_episode().unwrap();
        assert_eq!((s13.season, s13.episode), (1, 3));
        // it ends (watched) in the fullscreen player: the page follows along to S2E1 and opens
        // nothing itself (the "Next episode in 10 s" card is the player's, tests/guide.rs)
        crate::ui::screens::movie_detail::tests::fullscreen_placement(&mut h.state_mut().t);
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(episode_ref(&s13)), finished: true }));
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![], "no second up-next card over the player's");
        let next = h.state().s.data.as_ref().unwrap().episodes.iter().find(|e| e.season == 2 && e.episode == 1).unwrap().id;
        assert_eq!(h.state().s.focused_episode().map(|e| e.id), Some(next));
    }

    #[test]
    fn opens_at_the_next_episode_and_walks_sections() {
        let mut s = screen();
        s.select_episode(EpisodeId(4));
        assert_eq!((s.season, s.ep), (1, 1));
        assert_eq!(s.next_episode().map(|e| e.id), Some(EpisodeId(4)));
        s.move_vertical(true);
        s.move_vertical(true);
        assert_eq!(s.ep, 2);
        s.move_vertical(true);
        assert_eq!(s.ep, 2, "clamped at the last episode");
        s.ep = 0;
        s.move_vertical(false);
        assert_eq!(s.section, Section::Seasons, "↑ from the first episode reaches the season tabs");
        s.move_vertical(false);
        assert_eq!(s.section, Section::Buttons);
        s.set_season(0);
        assert_eq!((s.season, s.ep), (0, 0));
        s.set_season(9);
        assert_eq!(s.season, 1, "clamped to the last season");
        assert_eq!(s.after(&ItemKey("e2".into())).map(|e| e.id), Some(EpisodeId(3)), "the next episode crosses seasons");
        assert!(s.after(&ItemKey("e5".into())).is_none());
    }

    /// A series in two playlists (DESIGN §29.14): its page has the Copies section between the
    /// buttons and the seasons (↓ reaches it); picking the other playlist's copy makes Play name
    /// it and play that copy's next episode; its menu opens that copy's own page.
    #[test]
    fn copies_of_a_series_pick_the_copy_play_uses() {
        let (mut t, one) = rig();
        let (a_ids, b_ids) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let a_ids = testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance")], 3)?;
            let b_ids = testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 2)?;
            Ok((a_ids, b_ids))
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(a_ids[0]));
        until(&mut h, "the page", |h| has_label(h, "COPIES · 2") && has_label(h, "Season 1"));
        assert!(has_label(&h, "Alpha · EN · plays first") && has_label(&h, "Play S01E01 · Alpha"));
        // ↑ to the buttons, ↓ to the copies, → to Bravo's, Enter picks it
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::Up);
        assert_eq!(h.state().s.section, Section::Copies);
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert!(has_label(&h, "Play · Bravo") && has_label(&h, "Bravo · EN · plays first"));
        h.state_mut().act(Action::Up);
        h.state_mut().s.btn = 0;
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::Player)], "Bravo's next episode plays");
        // Bravo's copy has its own page (its own episodes)
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items.iter().any(|i| i.label == "Open its page")),
            "{nav:?}"
        );
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(super::super::copies::CARD_PAGE)));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Push(Route::SeriesDetail(b_ids[0]))]);
    }

    type H = egui_kittest::Harness<'static, testkit::App<SeriesDetailScreen>>;

    /// The progress row of episode `n` (season 1) of `named_series` copy `src`.
    fn named_ep(h: &H, src: SourceId, n: u32) -> Option<WatchProgress> {
        let item = ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey(format!("m3u:named-ep0-{n}")) };
        crate::store::user::progress(&h.state().t.services().store.reader().unwrap(), &item).unwrap()
    }

    fn named_progress(src: SourceId, n: u32, pos: f64, finished: bool, at: i64) -> WatchProgress {
        WatchProgress {
            item: ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey(format!("m3u:named-ep0-{n}")) },
            position_s: pos,
            duration_s: Some(2700.0),
            updated_at: at,
            finished,
            title: String::new(),
            fingerprint: Default::default(),
        }
    }

    #[test]
    fn remove_from_continue_watching_series_button_delete_and_undo() {
        let (mut t, one) = rig();
        let (id, source) = seed(&mut t, |c| {
            let source = testkit::source(c, "Alpha")?;
            let id = testkit::named_series(c, source, &[("EN ★ Severance - 2022", "Severance")], 3)?[0];
            crate::store::user::save_progress(c, &named_progress(source, 1, 1.0, false, 10))?;
            crate::store::user::save_progress(c, &named_progress(source, 2, 2700.0, true, 20))?;
            Ok((id, source))
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1400.0, 900.0));
        let label = "Remove from Continue watching";
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "series removal", |h| has_label(h, label));
        // Delete in the episode list leaves the series alone.
        assert_eq!(h.state().s.section, Section::Episodes);
        h.state_mut().act(Action::Remove);
        h.state().t.services().store.writer().blocking(|_| Ok(())).unwrap();
        assert!(named_ep(&h, source, 1).is_some());
        // Tab to buttons, then right past Play, My list and Mark season watched.
        h.state_mut().act(Action::FocusNext);
        for _ in 0..3 {
            h.state_mut().act(Action::Right);
        }
        h.state_mut().act(Action::Activate);
        let toast = h.state_mut().undo_toast().expect("series removal offers Undo");
        assert_eq!(toast.text, "Removed from Continue watching");
        assert!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().is_empty());
        assert!(named_ep(&h, source, 1).is_none() && named_ep(&h, source, 2).is_none());
        until(&mut h, "series Remove disappears", |h| !has_label(h, label));
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "series Undo", |h| has_label(h, label) && named_ep(h, source, 1).is_some() && named_ep(h, source, 2).is_some());
        assert_eq!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().len(), 1);
        h.state_mut().act(Action::Remove);
        until(&mut h, "Delete with buttons focused", |h| {
            !has_label(h, label) && crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().is_empty()
        });
    }

    #[test]
    fn remove_from_continue_watching_series_menu_forgets_every_copy() {
        let (mut t, one) = rig();
        let (id, a, b) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let id = testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance")], 2)?[0];
            testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 3)?;
            crate::store::user::save_progress(c, &named_progress(a, 1, 2700.0, true, 30))?;
            crate::store::user::save_progress(c, &named_progress(b, 1, 600.0, false, 10))?;
            crate::store::user::save_progress(c, &named_progress(b, 2, 2700.0, true, 40))?;
            // This episode is absent from Alpha; Bravo's latest progress is finished.
            crate::store::user::save_progress(c, &named_progress(b, 3, 300.0, false, 20))?;
            Ok((id, a, b))
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1400.0, 900.0));
        let label = "Remove from Continue watching";
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "combined series removal", |h| has_label(h, "COPIES · 2") && has_label(h, label));
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::ContextMenu(menu))] = nav.as_slice() else { panic!("{nav:?}") };
        assert!(menu.items.iter().any(|i| i.label == label && i.action == Action::Remove && i.shortcut.as_deref() == Some("Delete")));
        // Menu removal works even with the episode list focused.
        h.state_mut().result(OverlayResult::Chosen(Action::Remove));
        let toast = h.state_mut().undo_toast().expect("combined series removal offers Undo");
        assert_eq!(toast.text, "Removed from Continue watching");
        assert!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().is_empty());
        assert!([a, b].iter().all(|&s| (1..=3).all(|n| named_ep(&h, s, n).is_none())));
        until(&mut h, "combined series Remove disappears", |h| !has_label(h, label));
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "combined series Undo", |h| has_label(h, label) && named_ep(h, b, 3).is_some());
        assert_eq!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().len(), 2);
        assert!(named_ep(&h, a, 1).unwrap().finished && named_ep(&h, b, 2).unwrap().finished);
    }

    /// P3 (DESIGN §29.15): a combined series' page matches episodes across its copies by
    /// season and number — the other copy's watched mark shows here, the page opens at the
    /// next episode of the copy with the newest progress, and "Mark season watched" marks the
    /// season in every copy with an Undo that puts every row back.
    #[test]
    fn watched_marks_match_episodes_across_copies() {
        let (mut t, one) = rig();
        let (a, b, alpha) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let alpha = testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance")], 3)?[0];
            testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 2)?;
            // watched S1E1 and stopped in S1E2 on Bravo's copy
            crate::store::user::save_progress(c, &named_progress(b, 1, 2690.0, true, 10))?;
            crate::store::user::save_progress(c, &named_progress(b, 2, 600.0, false, 20))?;
            Ok((a, b, alpha))
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(alpha));
        until(&mut h, "the page", |h| has_label(h, "COPIES · 2") && has_label(h, "Season 1"));
        until(&mut h, "Bravo's watched mark shows on Alpha's S1E1", |h| {
            h.state().s.data.as_ref().and_then(|d| d.episodes.first().cloned()).is_some_and(|e| h.state().s.finished(&e))
        });
        assert_eq!(h.state().s.next_episode().map(|e| (e.season, e.episode)), Some((1, 2)), "Bravo's copy is at S1E2");
        assert_eq!((h.state().s.season, h.state().s.ep), (0, 1), "the page opens there");
        // mark season 1 watched: every copy's episodes, one Undo
        h.state_mut().t.toasts.clear();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)));
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Marked S01 watched in Alpha and Bravo");
        assert!((1..=3).all(|n| named_ep(&h, a, n).is_some_and(|p| p.finished)), "Alpha's three episodes");
        assert!((1..=2).all(|n| named_ep(&h, b, n).is_some_and(|p| p.finished)), "Bravo's two episodes");
        let Some((_, undo)) = toast.action else { unreachable!() };
        h.state_mut().act(undo);
        until(&mut h, "the rows as they were", |h| {
            named_ep(h, a, 1).is_none()
                && named_ep(h, b, 2).is_some_and(|p| !p.finished && p.position_s == 600.0)
                && named_ep(h, b, 1).is_some_and(|p| p.finished)
        });
    }

    /// P3: a combined ✓ marks this copy at once and waits for a copy whose episodes aren't
    /// loaded yet (an Xtream series loads them lazily): the page says it is loading them, then
    /// marks that copy's episodes too — every mark made meanwhile, in order, to the copies it
    /// was made for even after the page moved on to another series; when they can't be loaded
    /// a toast says which playlist was left out.
    #[test]
    fn a_combined_mark_loads_the_other_copys_episodes_first() {
        let (mut t, one) = rig();
        let (a, b, alpha, heat, bravo) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let ids = testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance"), ("EN ★ Heat Wave - 1995", "Heat Wave")], 2)?;
            // an Xtream series whose details (and episodes) haven't been loaded
            let bravo = testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 0)?[0];
            c.execute("UPDATE source SET kind = 'xtream' WHERE id = ?1", [b])?;
            c.execute("UPDATE series SET details_at = NULL WHERE id = ?1", [bravo])?;
            Ok((a, b, ids[0], ids[1], bravo))
        });
        let bravo_episodes = move |c: &mut rusqlite::Connection| {
            for e in 1..=2 {
                c.execute(
                    "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, 1, ?4, 'E', 2700, '{\"t\":\"url\",\"url\":\"http://127.0.0.1:9/x.mp4\"}')",
                    rusqlite::params![bravo, b, format!("m3u:named-ep0-{e}"), e],
                )?;
            }
            c.execute("UPDATE series SET details_at = 1 WHERE id = ?1", [bravo])?;
            Ok(())
        };
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(alpha));
        until(&mut h, "the page", |h| has_label(h, "COPIES · 2") && has_label(h, "Season 1"));
        // nothing loads Bravo's episodes: Alpha's are marked at once, a toast names Bravo
        h.state_mut().t.toasts.clear();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)));
        assert!(h.state().s.fetch.busy());
        h.run_steps(1);
        assert!(has_label(&h, "Loading the other copy's episodes to mark them too…"));
        until(&mut h, "Alpha's episodes, before Bravo's settle", |h| (1..=2).all(|n| named_ep(h, a, n).is_some_and(|p| p.finished)));
        assert_eq!(h.state_mut().undo_toast().map(|t| t.text), Some("Marked S01 watched in Alpha".to_owned()));
        until(&mut h, "the warning", |h| {
            h.state().t.toasts.iter().any(|t| t.text == "Bravo couldn't load its episodes — marked in the other copies only")
        });
        // two marks while Bravo's episodes load (season unwatched, then E02 watched), then the
        // episodes arrive: both reach Bravo's copy, in order
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_UNWATCHED)));
        h.state_mut().s.ep = 1;
        h.state_mut().act(Action::MarkWatched);
        assert!(h.state().s.fetch.busy());
        seed(&mut h.state_mut().t, bravo_episodes);
        until(&mut h, "both marks in both copies", |h| {
            !h.state().s.fetch.busy()
                && named_ep(h, a, 1).is_none()
                && named_ep(h, a, 2).is_some_and(|p| p.finished)
                && named_ep(h, b, 1).is_none()
                && named_ep(h, b, 2).is_some_and(|p| p.finished)
        });
        // Bravo's episodes gone again; mark the season and open another series before they
        // arrive: the mark lands on Severance's copies, never on the series now shown
        seed(&mut h.state_mut().t, move |c| {
            c.execute("DELETE FROM episode WHERE series_id = ?1", [bravo])?;
            c.execute("UPDATE series SET details_at = NULL WHERE id = ?1", [bravo])?;
            Ok(())
        });
        // what a refresh announces (the seed itself sends nothing: without this the page only
        // reloaded on a late Progress event from the marks above)
        h.state().t.services().bus.send(AppEvent::Data(DataScope::Vod));
        until(&mut h, "Bravo unloaded", |h| h.state().s.data.as_ref().is_some_and(|d| !d.unloaded.is_empty()));
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_SEASON_WATCHED)));
        assert!(h.state().s.fetch.busy());
        h.state_mut().enter(Route::SeriesDetail(heat));
        seed(&mut h.state_mut().t, bravo_episodes);
        until(&mut h, "Bravo's season marked", |h| !h.state().s.fetch.busy() && (1..=2).all(|n| named_ep(h, b, n).is_some_and(|p| p.finished)));
        let heat_rows: i64 = h
            .state()
            .t
            .services()
            .store
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM watch_progress WHERE item_key LIKE 'm3u:named-ep1-%'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(heat_rows, 0, "the series shown now is never marked");
        assert_eq!(h.state().s.id, Some(heat));
    }

    /// P3: on a combined page the other copy's position is where Play resumes: the button says
    /// "Resume" and the resume prompt plays this copy's episode from there.
    #[test]
    fn a_combined_page_resumes_from_the_other_copys_progress() {
        let (mut t, one) = rig();
        let alpha = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let alpha = testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance")], 3)?[0];
            testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 3)?;
            // Bravo's copy stopped at 12:00 in S1E2; Alpha's copy is the one that plays
            crate::store::user::save_progress(c, &named_progress(b, 2, 720.0, false, 20))?;
            let r = |s| ItemRef { source: s, kind: ItemKind::Series, key: ItemKey("m3u:named0000".into()) };
            crate::store::titles::set_pin(c, &r(a), &[r(b)], true, 1)?;
            Ok(alpha)
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 900.0));
        h.state_mut().enter(Route::SeriesDetail(alpha));
        until(&mut h, "the page", |h| has_label(h, "COPIES · 2") && has_label(h, "Season 1"));
        until(&mut h, "Resume from Bravo's position", |h| testkit::has_label_containing(h, "Resume S01E02 12:00"));
        // Enter on S1E2 in this page's (Alpha's) list: the prompt resumes at 12:00 and Alpha's
        // copy starts there
        let e = h.state().s.next_episode().expect("the next episode");
        assert_eq!((h.state().s.section, h.state().s.focused_episode().map(|x| x.id)), (Section::Episodes, Some(e.id)));
        h.state_mut().nav();
        h.state_mut().act(Action::Activate);
        let nav = h.state_mut().nav();
        let Some(NavRequest::Open(OverlayRequest::Resume(spec))) = nav.last() else { panic!("{nav:?}") };
        assert_eq!(spec.position_s, 720.0);
        let copy = spec.copy.as_ref().expect("a copy");
        assert_eq!((copy.req.media.clone(), copy.start_at), (MediaRef::Episode(e.id), Some(720.0)), "this copy, from Bravo's position");
    }

    #[test]
    fn season_watched_follows_progress() {
        let mut s = screen();
        s.set_season(0);
        assert!(!s.season_watched());
        let d = s.data.as_mut().unwrap();
        for e in d.episodes.clone().iter().filter(|e| e.season == 1) {
            d.progress.insert(
                e.key.clone(),
                WatchProgress {
                    item: episode_ref(e),
                    position_s: 0.0,
                    duration_s: None,
                    updated_at: 0,
                    finished: true,
                    title: String::new(),
                    fingerprint: Default::default(),
                },
            );
        }
        assert!(s.season_watched());
        assert_eq!(episode_label(&ep(7, 1, 3)), "S01E03 · Episode 3");
        // a playlist episode without a title (DESIGN §27.7)
        assert_eq!(episode_label(&Episode { title: String::new(), ..ep(8, 2, 4) }), "S02E04 · Episode 4");
    }
    #[test]
    fn ctrl_shift_f_on_series_opens_the_season_range_picker() {
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let src = testkit::source(c, "TV")?;
            Ok(testkit::series(c, src, 1, 3, 2)?[0])
        });
        let mut h = harness(t, one, SeriesDetailScreen::default(), egui::vec2(1200.0, 800.0));
        h.state_mut().enter(Route::SeriesDetail(id));
        until(&mut h, "series page", |h| h.state().s.data.is_some());
        h.state_mut().act(Action::FocusNext);
        assert_eq!(h.state().s.section, Section::Buttons);
        h.state_mut().act(Action::AddToList);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.last(),Some(NavRequest::Open(OverlayRequest::WatchlistPicker(spec))) if spec.items.len()==1 && spec.items[0].kind==ItemKind::Series && spec.edit.is_none())
        );
    }
}
