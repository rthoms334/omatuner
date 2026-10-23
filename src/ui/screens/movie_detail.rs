//! Movie detail (DESIGN §13.6): backdrop (dimmed), poster, title, year · duration · rating ·
//! genre, plot, cast/director, buttons Play/Resume · Start over · Favourite · Mark watched ·
//! Remove from Continue watching (while unfinished progress exists).
//! Loads details lazily (`JobKind::LoadMovieDetails`) with a skeleton state; a playlist (M3U)
//! movie has none to load and shows a sparse page instead (DESIGN §27.7). Key context
//! `[Detail]`. When a movie ends in the fullscreen player (marked watched by the player) the
//! app leaves the player for its page, wherever it was started. Owner: WP-12.
//!
//! Data: the list-level row (`Library`) shows at once; the full row and its progress are read
//! off the UI thread (`kit::Loader`) and re-read on `DataScope::{VodDetails, Progress,
//! Overrides}`. A detail-load failure keeps the list-level data and toasts. Locked movies ask
//! for the PIN before playing. Enter on the main button plays or resumes; Shift+Enter always
//! starts over.
//!
//! A title with several copies (DESIGN §29.14) gets a Copies section of mini-cards after the
//! buttons (`copies::Copies`; ←/→ walk from the buttons onto them, Enter picks the copy Play
//! uses, the menu key opens its menu) and a "Play from" button (Ctrl+O). The Play button names
//! the playlist of the copy it plays ("Resume 41:20 · TV").

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::images::ImageKind;
use crate::jobs::{JobEvent, JobKind};
use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, MediaRef, Movie, MovieId, PlayOrigin, SourceKind, VodKind, VodSummary, WatchProgress};
use crate::msg::{AppEvent, DataScope};
use crate::player::{PlayerCommand, PlayerEvent};
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult, ResumeSpec};
use crate::ui::video_host::Placement;
use crate::ui::widgets::button::{ButtonStyle, button, icon_button};
use crate::ui::widgets::poster::paint_poster_image;
use crate::ui::widgets::{badge, empty_state, glyphs, section};
use crate::ui::{NavRequest, OverlayRequest, Route, Screen, ScreenId, UiCtx};

/// Play a movie fullscreen from a grid/rail/search (Shift+Enter): the resume prompt when
/// there is progress past `resume_min_s`, else straight away.
pub(crate) fn play_movie(ctx: &mut UiCtx<'_>, id: MovieId) {
    match ctx.lib.vod.find(VodKind::Movie(id)) {
        Some(v) => {
            let item = super::home::vod_ref(v);
            let title = v.name.to_string();
            resume_or_play(ctx, MediaRef::Movie(id), &item, title, PlayOrigin::Fullscreen);
        }
        None => kit::play_media(ctx, MediaRef::Movie(id), true, PlayOrigin::Fullscreen),
    }
}

/// Resume prompt (overlay) when progress > `resume_min_s` (DESIGN §13.6), else play. The
/// progress is one indexed point read within the frame budget; when the budget is spent the
/// player applies the saved position itself.
pub(crate) fn resume_or_play(ctx: &mut UiCtx<'_>, media: MediaRef, item: &ItemRef, title: String, origin: PlayOrigin) {
    let db = ctx.db;
    let p = ctx.budget.run(|| crate::store::user::progress(db, item)).and_then(|r| r.ok()).flatten();
    match p {
        Some(p) if resumable(&p, ctx.config.player.resume_min_s) => {
            ctx.open(OverlayRequest::Resume(ResumeSpec {
                media,
                title,
                position_s: p.position_s,
                duration_s: p.duration_s,
                copy: None,
                queue: None,
                cast: None,
                watchlist: None,
            }));
        }
        _ => kit::play_media(ctx, media, true, origin),
    }
}

/// Unfinished, past the resume threshold.
pub(crate) fn resumable(p: &WatchProgress, min_s: u32) -> bool {
    !p.finished && p.position_s > f64::from(min_s)
}

/// Progress 0..1 of a row (unknown duration → none).
pub(crate) fn fraction(p: &WatchProgress) -> Option<f32> {
    p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d).clamp(0.0, 1.0) as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Download,
    Play,
    StartOver,
    /// "Play from ▾": the title's copies (DESIGN §29.14).
    PlayFrom,
    Favourite,
    Watched,
    RemoveContinue,
}

#[derive(Default)]
pub struct MovieDetailScreen {
    id: Option<MovieId>,
    watchlist: Option<crate::ui::watchlists::Origin>,
    movie: Option<Movie>,
    progress: Option<WatchProgress>,
    loader: Option<Loader<(Movie, Option<WatchProgress>)>>,
    /// The row is gone (vanished on refresh).
    missing: bool,
    /// `LoadMovieDetails` was submitted for this id.
    details_requested: Option<MovieId>,
    /// The focused button, then the Copies mini-cards (`buttons().len() + k`).
    focus: usize,
    scroll_to_cursor: bool,
    copies: super::copies::Copies,
}

impl MovieDetailScreen {
    fn reload(&mut self, ctx: &UiCtx<'_>) {
        let Some(id) = self.id else { return };
        self.loader = Some(Loader::start(ctx, move |c| {
            let m = crate::store::vod::movie(c, id)?;
            let item = ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key.clone() };
            let p = crate::store::user::progress(c, &item)?;
            Ok((m, p))
        }));
    }

    fn summary<'l>(&self, ctx: &'l UiCtx<'_>) -> Option<&'l VodSummary> {
        self.id.and_then(|id| ctx.lib.vod.find(VodKind::Movie(id)))
    }

    fn item(&self, ctx: &UiCtx<'_>) -> Option<ItemRef> {
        match (&self.movie, self.summary(ctx)) {
            (Some(m), _) => Some(ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key.clone() }),
            (None, Some(v)) => Some(super::home::vod_ref(v)),
            _ => None,
        }
    }

    fn title(&self, ctx: &UiCtx<'_>) -> String {
        match (&self.movie, self.summary(ctx)) {
            (Some(m), _) => m.display_name().to_owned(),
            (None, Some(v)) => v.name.to_string(),
            _ => String::new(),
        }
    }

    fn locked(&self, ctx: &UiCtx<'_>) -> bool {
        let locked = self.movie.as_ref().map(|m| m.locked).or(self.summary(ctx).map(|v| v.locked)).unwrap_or(false);
        locked && !kit::unlocked(ctx)
    }

    /// ✓: the page's copy is watched — for a combined title, any copy (DESIGN §29.15).
    fn watched(&self) -> bool {
        if self.copies.combined() {
            return self.copies.any_watched();
        }
        self.progress.as_ref().is_some_and(|p| p.finished)
    }

    /// ★: the page's copy is a favourite — for a combined title, any copy.
    fn favourite(&self, ctx: &UiCtx<'_>) -> bool {
        if self.copies.combined() {
            return self.copies.any_favourite(ctx);
        }
        self.item(ctx).is_some_and(|i| ctx.favs.is_fav(&i))
    }

    /// Where Play resumes: the latest progress of any of the title's copies (whichever copy
    /// plays starts there, DESIGN §29.15), else the page's own.
    fn resume_at(&self, ctx: &UiCtx<'_>) -> Option<f64> {
        if self.copies.is_stacked() {
            return self.copies.resume_at(ctx);
        }
        self.progress.as_ref().filter(|p| resumable(p, ctx.config.player.resume_min_s)).map(|p| p.position_s)
    }

    fn in_continue_watching(&self) -> bool {
        if self.copies.combined() {
            return self.copies.plan().copies.iter().any(|c| c.progress.as_ref().is_some_and(|p| !p.finished && p.position_s > 0.0));
        }
        self.progress.as_ref().is_some_and(|p| !p.finished && p.position_s > 0.0)
    }

    fn buttons(&self, ctx: &UiCtx<'_>) -> Vec<Btn> {
        let mut v = vec![Btn::Play];
        if self.resume_at(ctx).is_some() {
            v.push(Btn::StartOver);
        }
        if self.copies.is_stacked() {
            v.push(Btn::PlayFrom);
        }
        v.extend([Btn::Download, Btn::Favourite, Btn::Watched]);
        if self.in_continue_watching() {
            v.push(Btn::RemoveContinue);
        }
        v
    }

    fn play(&mut self, ctx: &mut UiCtx<'_>, resume: bool) {
        let Some(id) = self.id else { return };
        if self.missing {
            return;
        }
        if let Some(origin) = self.watchlist {
            // The summary enables Play before the async detail read finishes. Use the same
            // fallback here so an early Enter or click still plays through the list.
            if let Some(item) = self.item(ctx) {
                crate::ui::watchlists::play_from_page(ctx, origin, item, MediaRef::Movie(id), self.title(ctx), None, resume, false);
            }
            return;
        }
        if self.copies.is_stacked() {
            // the copy picked on the page, else the title's first; the next one as fallback
            let pick = self.copies.chosen();
            self.play_copies(ctx, pick, resume);
            return;
        }
        if self.locked(ctx) {
            kit::ask_pin(ctx, if resume { Action::Activate } else { Action::ActivateAlt }, "This title is locked");
            return;
        }
        kit::play_media(ctx, MediaRef::Movie(id), resume, PlayOrigin::Fullscreen);
    }

    /// Play the title's copies (`pick`: this copy first; the caller checked its PIN), with the
    /// fallback once before the first frame (DESIGN §29.15).
    fn play_copies(&mut self, ctx: &mut UiCtx<'_>, pick: Option<u32>, resume: bool) {
        if self.watchlist.is_some() {
            self.play(ctx, resume);
            return;
        }
        match crate::ui::copies::movie_command(&ctx.lib.vod, self.copies.plan(), pick, resume, PlayOrigin::Fullscreen) {
            Some(PlayerCommand::PlayCopy(copy)) => crate::ui::copies::start(ctx, copy),
            Some(cmd) => {
                ctx.play(cmd);
                ctx.push(Route::Player);
            }
            None => kit::ask_pin(ctx, if resume { Action::Activate } else { Action::ActivateAlt }, "Every copy of this title is locked"),
        }
    }

    /// Number of buttons; the Copies mini-cards follow them in the focus order.
    fn focus_len(&self, ctx: &UiCtx<'_>) -> usize {
        self.buttons(ctx).len() + self.copies.len()
    }

    /// Keep the Copies section's focus in step with the page's.
    fn sync_focus(&mut self, ctx: &UiCtx<'_>) {
        let n = self.buttons(ctx).len();
        self.copies.focus = self.focus.checked_sub(n);
    }

    /// (effective lock, locked through a category) of the movie.
    fn lock_state(&self, ctx: &UiCtx<'_>) -> (bool, bool) {
        let groups = &ctx.lib.vod.movie_groups;
        match (&self.movie, self.summary(ctx)) {
            (Some(m), _) => (m.locked, kit::any_group_locked(groups, &m.group_ids)),
            (None, Some(v)) => (v.locked, kit::any_group_locked(groups, &v.group_ids)),
            _ => (false, false),
        }
    }

    /// Item shortcuts of the page (and its context menu).
    fn item_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) {
        // a feature's choice from the menu
        if let Some(id) = self.id
            && crate::ui::item_menu::dispatch(ctx, action, &crate::ui::item_menu::Target::Movie(id))
        {
            return;
        }
        let Some(item) = self.item(ctx) else { return };
        let title = self.title(ctx);
        // a combined title (DESIGN §29.15, P3): lock, hide and rename cover every copy, with Undo
        if self.copies.combined() && matches!(action, Action::Rename | Action::ToggleHidden | Action::ToggleLock) {
            let plan = self.copies.plan().clone();
            match action {
                Action::Rename => crate::ui::copies::rename_dialog(ctx, &plan, &title),
                Action::ToggleHidden => crate::ui::copies::hide(ctx, &plan, &title),
                _ => crate::ui::copies::toggle_lock(ctx, &plan, true),
            }
            return;
        }
        match action {
            Action::AddToList => kit::list_picker(ctx, &item, &title),
            Action::Rename => kit::rename_item(ctx, &item, &title),
            Action::ToggleHidden => kit::hide_item(ctx, &item, &title),
            Action::ToggleLock => {
                let (locked, by_group) = self.lock_state(ctx);
                kit::toggle_lock(ctx, &item, locked, by_group);
            }
            Action::ContextMenu => {
                let fav = self.favourite(ctx);
                let every = self.copies.combined();
                let mi = |icon: &'static str, label: &str, action: Action, shortcut: &str| MenuItem {
                    icon,
                    label: label.into(),
                    action,
                    enabled: true,
                    shortcut: Some(shortcut.into()),
                };
                let mut items = vec![
                    mi(glyphs::PLAY, if self.resume_at(ctx).is_some() { "Resume" } else { "Play" }, Action::Activate, "Enter"),
                    mi(glyphs::SKIP_BACK, "Play from the start", Action::ActivateAlt, "Shift+Enter"),
                ];
                // several copies (DESIGN §29.14): pick one; take this one apart
                if self.copies.is_stacked() {
                    items.push(mi(glyphs::PLAY, "Play from…", Action::PlayFrom, "Ctrl+O"));
                    items.push(MenuItem { shortcut: None, ..mi(glyphs::CLOSE, "Not the same title", Action::Digit(super::copies::CARD_SPLIT), "") });
                    self.copies.aim_at_page();
                }
                // combine it with a title the matcher kept apart (DESIGN §29.16; two movie
                // playlists or more)
                if crate::ui::stacks::combine(ctx.config) && ctx.lib.eligible(ItemKind::Movie).len() > 1 {
                    items.push(MenuItem { shortcut: None, ..mi(glyphs::PLAYLIST, "Same title as…", Action::Digit(super::copies::SAME_TITLE), "") });
                }
                items.extend([
                    mi(glyphs::FAVOURITE, if fav { "Remove from My list" } else { "Save to My list" }, Action::ToggleFavourite, "Ctrl+F"),
                    mi(glyphs::FAV_LIST, "Watchlists…", Action::AddToList, "Ctrl+Shift+F"),
                    mi(glyphs::CHECK, if self.watched() { "Mark unwatched" } else { "Mark watched" }, Action::MarkWatched, "Ctrl+W"),
                ]);
                if self.in_continue_watching() {
                    items.push(mi(glyphs::CLOSE, "Remove from Continue watching", Action::Remove, "Delete"));
                }
                items.extend([
                    mi(glyphs::TAG, if every { "Rename every copy…" } else { "Rename…" }, Action::Rename, "F2"),
                    mi(glyphs::LOCK, if every { "Lock / unlock every copy" } else { "Lock / unlock" }, Action::ToggleLock, "Ctrl+L"),
                    mi(glyphs::HIDDEN, if every { "Hide every copy" } else { "Hide" }, Action::ToggleHidden, "Ctrl+H"),
                ]);
                if let Some(id) = self.id {
                    crate::ui::item_menu::extend(&mut items, &crate::ui::item_menu::Target::Movie(id), ctx, KeyContext::Detail);
                }
                ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
            }
            _ => kit::not_here(ctx),
        }
    }
}

/// End of a movie in the fullscreen player: leave the player for the movie's page (no-op push
/// when the page is already under the player).
fn return_to_page(ctx: &mut UiCtx<'_>, item: &ItemRef) {
    // one pass over the in-memory list, once per finished movie
    let Some(id) = ctx.lib.vod.movies.iter().find(|v| v.source_id == item.source && v.key == item.key).and_then(|v| match v.kind {
        VodKind::Movie(id) => Some(id),
        VodKind::Series(_) => None,
    }) else {
        return;
    };
    ctx.navigate(NavRequest::Back);
    ctx.push(Route::MovieDetail(id));
}

impl MovieDetailScreen {
    fn press(&mut self, ctx: &mut UiCtx<'_>, b: Btn) {
        match b {
            Btn::Download => {
                if let Some(id) = self.id {
                    ctx.open(OverlayRequest::Download(crate::ui::item_menu::Target::Movie(id)));
                }
            }
            Btn::Play => self.play(ctx, true),
            Btn::StartOver => self.play(ctx, false),
            Btn::PlayFrom => self.copies.play_from_menu(ctx),
            Btn::Favourite if self.copies.combined() => {
                // add: the copy Play would use; remove: every copy, with Undo
                let plan = self.copies.plan().clone();
                crate::ui::copies::toggle_favourite_of(ctx, &plan, self.copies.plays());
            }
            Btn::Favourite => {
                if let Some(item) = self.item(ctx) {
                    kit::toggle_favourite(ctx, &item);
                }
            }
            Btn::Watched => self.toggle_watched(ctx),
            Btn::RemoveContinue => self.remove_continue(ctx),
        }
    }

    fn remove_continue(&mut self, ctx: &mut UiCtx<'_>) {
        if !self.in_continue_watching() {
            return;
        }
        let title = self.title(ctx);
        if self.copies.combined() {
            crate::ui::copies::mark_watched(ctx, self.copies.plan(), &title, false);
        } else if let Some(item) = self.item(ctx) {
            crate::ui::downloads::offer_cleanup(ctx, std::slice::from_ref(&item));
            let now = ctx.now;
            crate::ui::copies::write_with_undo(ctx, DataScope::Progress, "Removed from Continue watching".into(), move |c| {
                crate::store::copies::watched_items(c, &[(item, title)], false, now)
            });
        }
        self.focus = 0;
        self.sync_focus(ctx);
    }

    fn toggle_watched(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(item) = self.item(ctx) else { return };
        let on = !self.watched();
        let title = self.title(ctx);
        if self.copies.combined() {
            // mark: the copy Play would use; unmark: every copy's progress, with Undo
            let plan = self.copies.plan().clone();
            crate::ui::copies::mark_watched_of(ctx, &plan, self.copies.plays(), &title, on);
            self.focus = self.focus.min(self.focus_len(ctx) - 1);
            return;
        }
        let (now, write_item) = (ctx.now, item.clone());
        let _ = ctx
            .services
            .store
            .writer()
            .submit_notify(DataScope::Progress, move |c| crate::store::user::mark_watched(c, &[(write_item, title)], on, now));
        // optimistic: the reload on DataScope::Progress confirms it
        self.progress = on.then(|| WatchProgress {
            item,
            position_s: self.progress.as_ref().map_or(0.0, |p| p.position_s),
            duration_s: self.progress.as_ref().and_then(|p| p.duration_s),
            updated_at: now,
            finished: true,
            title: String::new(),
            fingerprint: Default::default(),
        });
        self.focus = self.focus.min(self.focus_len(ctx) - 1);
    }
}

impl Screen for MovieDetailScreen {
    fn id(&self) -> ScreenId {
        ScreenId::MovieDetail
    }
    fn reveal_target(&self, _ctx: &UiCtx<'_>) -> Option<crate::ui::item_menu::Target> {
        self.id.map(crate::ui::item_menu::Target::Movie)
    }
    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        if self.copies.is_stacked() { vec![(Action::PlayFrom, "play from")] } else { Vec::new() }
    }
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Detail]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let (id, watchlist) = match route {
            Route::MovieDetail(id) => (id, None),
            Route::MovieDetailFromList { movie, origin } => (movie, Some(*origin)),
            _ => return,
        };
        self.watchlist = watchlist;
        if self.id != Some(*id) {
            self.movie = None;
            self.progress = None;
            self.focus = 0;
        }
        self.id = Some(*id);
        self.missing = false;
        self.reload(ctx);
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Data(DataScope::VodDetails | DataScope::Progress | DataScope::Overrides | DataScope::Vod | DataScope::All) => {
                self.reload(ctx);
                self.copies.reload(ctx);
            }
            AppEvent::Job(JobEvent::Finished { kind: JobKind::LoadMovieDetails(m), outcome: Err(msg), .. }) if Some(*m) == self.id => {
                ctx.toast(crate::msg::Toast::error(format!("Couldn't load the details: {}", crate::redact::redact(msg))));
            }
            AppEvent::Player(PlayerEvent::Ended { item: Some(item), finished }) if item.kind == ItemKind::Movie => {
                if Some(item) == self.item(ctx).as_ref() {
                    self.reload(ctx);
                }
                // DESIGN §13.6: a finished movie returns from the fullscreen player to its page,
                // wherever it was started (grid, Home, search, this page) — but not one Up next
                // plays: its card and the next film follow in the player
                if *finished && ctx.video.placement() == Placement::Fullscreen && ctx.player.origin != Some(PlayOrigin::Queue) {
                    return_to_page(ctx, item);
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        // the batch features' keys on the movie (Ctrl+N, Ctrl+M…)
        if let Some(id) = self.id
            && crate::ui::item_menu::dispatch(ctx, action, &crate::ui::item_menu::Target::Movie(id))
        {
            return true;
        }
        let n = self.focus_len(ctx);
        let buttons = self.buttons(ctx).len();
        if matches!(
            action,
            Action::Left
                | Action::Right
                | Action::Up
                | Action::Down
                | Action::FocusPrev
                | Action::FocusNext
                | Action::Home
                | Action::End
                | Action::PageUp
                | Action::PageDown
        ) {
            self.scroll_to_cursor = true;
        }
        let handled = match action {
            Action::Left | Action::Up | Action::FocusPrev => {
                self.focus = self.focus.saturating_sub(1);
                true
            }
            Action::Right | Action::Down | Action::FocusNext => {
                self.focus = (self.focus + 1).min(n.saturating_sub(1));
                true
            }
            Action::Home | Action::PageUp => {
                self.focus = 0;
                true
            }
            Action::End | Action::PageDown => {
                self.focus = n.saturating_sub(1);
                true
            }
            // a mini-card: Enter picks the copy Play uses, the menu key opens its menu
            Action::Activate | Action::ContextMenu if self.focus >= buttons => {
                let k = self.focus - buttons;
                let click = if action == Action::Activate { super::copies::Click::Pick(k) } else { super::copies::Click::Menu(k) };
                self.copies.click(ctx, click);
                true
            }
            Action::PlayFrom => {
                self.copies.play_from_menu(ctx);
                true
            }
            Action::Activate => {
                if let Some(b) = self.buttons(ctx).get(self.focus).copied() {
                    self.press(ctx, b);
                }
                true
            }
            Action::ActivateAlt => {
                self.play(ctx, false);
                true
            }
            Action::MarkWatched => {
                self.toggle_watched(ctx);
                true
            }
            Action::Remove if self.focus < buttons => {
                self.remove_continue(ctx);
                true
            }
            Action::ToggleFavourite => {
                self.press(ctx, Btn::Favourite);
                true
            }
            Action::AddToList | Action::Rename | Action::ToggleHidden | Action::ToggleLock | Action::ContextMenu => {
                self.item_action(ctx, action);
                true
            }
            // this is the details page already
            Action::ShowDetails | Action::TabPrev | Action::TabNext => true,
            Action::MoveUp | Action::MoveDown | Action::SortMenu | Action::OpenInGuide | Action::AssignEpg | Action::Remove => {
                kit::not_here(ctx);
                true
            }
            _ => false,
        };
        self.sync_focus(ctx);
        handled
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        // the Copies section's menus and PIN answers first (DESIGN §29.14)
        match self.copies.on_result(ctx, &result) {
            super::copies::Outcome::NotMine => {}
            super::copies::Outcome::Done => return,
            super::copies::Outcome::Act(super::copies::CopyAct::Play(c)) => return self.play_copies(ctx, Some(c), true),
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Movie(id))) => {
                let route = self.watchlist.map_or(Route::MovieDetail(id), |origin| Route::MovieDetailFromList { movie: id, origin });
                return ctx.push(route);
            }
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Series(id))) => return ctx.push(Route::SeriesDetail(id)),
        }
        match result {
            OverlayResult::Chosen(Action::PlayFrom) => self.copies.play_from_menu(ctx),
            OverlayResult::Chosen(Action::Digit(super::copies::SAME_TITLE)) => {
                if let Some(item) = self.item(ctx) {
                    let name = self.title(ctx);
                    crate::ui::copies::same_title_picker(ctx, item, &name);
                }
            }
            OverlayResult::Unlocked(Action::Activate) | OverlayResult::Chosen(Action::Activate) => self.play(ctx, true),
            OverlayResult::Unlocked(Action::ActivateAlt) | OverlayResult::Chosen(Action::ActivateAlt) => self.play(ctx, false),
            OverlayResult::Unlocked(Action::ToggleLock) => self.item_action(ctx, Action::ToggleLock),
            OverlayResult::Chosen(Action::ToggleFavourite) => self.press(ctx, Btn::Favourite),
            OverlayResult::Chosen(Action::MarkWatched) => self.toggle_watched(ctx),
            OverlayResult::Chosen(Action::Remove) => self.remove_continue(ctx),
            OverlayResult::Chosen(a) if a != Action::ContextMenu => self.item_action(ctx, a),
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.loader) {
            Some(Ok((m, p))) => {
                let lazy = m.details_loaded_at.is_none() && ctx.lib.source(m.source_id).is_some_and(|s| s.kind == SourceKind::Xtream);
                if lazy && self.details_requested != Some(m.id) {
                    self.details_requested = Some(m.id);
                    ctx.services.jobs.submit(JobKind::LoadMovieDetails(m.id));
                }
                self.movie = Some(m);
                self.progress = p;
                self.missing = false;
            }
            Some(Err(crate::error::Error::NotFound(_))) => self.missing = true,
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read this movie", &e),
            None => {}
        }
        let theme = ctx.theme;
        if self.missing || (self.id.is_none()) {
            detail_bar(ui, ctx, false);
            empty_state::empty_state(ui, theme, glyphs::MOVIES, "This movie is no longer in your playlists", None, None);
            return;
        }
        if let Some(id) = self.id {
            self.copies.sync(ctx, VodKind::Movie(id));
        }
        self.focus = self.focus.min(self.focus_len(ctx).saturating_sub(1));
        self.sync_focus(ctx);
        let summary = self.summary(ctx).cloned();
        if self.movie.is_none() && summary.is_none() {
            detail_bar(ui, ctx, false);
            skeleton(ui, theme);
            return;
        }
        let mut menu = detail_bar(ui, ctx, true);
        let m = self.movie.clone();
        // DESIGN §27.7: only Xtream movies have details to load; a playlist's movie has a title,
        // a poster and a category, and its page says so instead of waiting for more
        let source = m.as_ref().map(|m| m.source_id).or(summary.as_ref().map(|s| s.source_id));
        let sparse = !source.and_then(|s| ctx.lib.source(s)).is_some_and(|s| s.kind == SourceKind::Xtream);
        let backdrop = m.as_ref().and_then(|m| m.backdrop.clone());
        let poster_url = m.as_ref().and_then(|m| m.poster.clone()).or(summary.as_ref().and_then(|s| s.poster.as_deref().map(str::to_owned)));
        let title = self.title(ctx);
        let mut meta = Vec::new();
        if let Some(y) = m.as_ref().and_then(|m| m.year).or(summary.as_ref().and_then(|s| s.year)) {
            meta.push(y.to_string());
        }
        if let Some(d) = m.as_ref().and_then(|m| m.duration_s).filter(|d| *d > 0) {
            meta.push(kit::runtime(d));
        }
        if let Some(r) = m.as_ref().and_then(|m| m.rating).or(summary.as_ref().and_then(|s| s.rating)).filter(|r| *r > 0.0) {
            meta.push(format!("★ {r:.1}"));
        }
        if let Some(g) = m.as_ref().and_then(|m| m.genre.clone()).filter(|g| !g.is_empty()) {
            meta.push(g);
        }
        if sparse {
            let groups = match (&m, &summary) {
                (Some(m), _) => m.group_ids.as_slice(),
                (None, Some(s)) => s.group_ids.as_slice(),
                _ => &[],
            };
            meta.extend(sparse_meta(ctx, groups));
        }
        let mut text = Vec::new();
        let plot = m.as_ref().and_then(|m| {
            let p = m.plot.as_deref().filter(|p| !p.trim().is_empty())?;
            Some((crate::guard::Subject::Vod { title: &m.name, series_norm: None }, p))
        });
        if let Some(m) = &m {
            if let Some(c) = m.cast.as_deref().filter(|c| !c.trim().is_empty()) {
                text.push((format!("Cast: {c}"), true));
            }
            if let Some(d) = m.director.as_deref().filter(|d| !d.trim().is_empty()) {
                text.push((format!("Director: {d}"), true));
            }
        }
        if sparse && text.is_empty() && plot.is_none() {
            text.push((NO_DESCRIPTION.to_owned(), true));
        }
        let loading = !sparse && (m.is_none() || self.loader.is_some() && m.as_ref().is_some_and(|m| m.details_loaded_at.is_none()));
        let playing = ctx.player.playable.as_ref().is_some_and(|p| self.id.is_some_and(|id| p.media == MediaRef::Movie(id)));
        let buttons = self.buttons(ctx);
        let resume_at = self.resume_at(ctx);
        let fav = self.favourite(ctx);
        let watched = self.watched();
        let locked = m.as_ref().map(|m| m.locked).or(summary.as_ref().map(|s| s.locked)).unwrap_or(false);
        // several copies: the Play button names the playlist it plays from
        let plays_from = self.copies.plays().and_then(|c| self.copies.playlist(ctx, c)).map(str::to_owned);
        let mut pressed = None;
        let mut copy_click = None;
        let scroll = std::mem::take(&mut self.scroll_to_cursor);
        egui::ScrollArea::vertical().id_salt("movie-detail").auto_shrink([false, false]).show(ui, |ui| {
            let top = ui.cursor().min;
            let mut button_rect = None;
            let header = DetailHeader {
                title: &title,
                meta: &meta.join(" · "),
                plot,
                text: &text,
                poster: poster_url.as_deref(),
                backdrop: backdrop.as_deref(),
                playing,
                locked,
                loading,
                source,
                kind: ItemKind::Movie,
            };
            menu |= header.show(ui, ctx, |ui, ctx| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Md));
                    for (i, b) in buttons.iter().enumerate() {
                        let (icon, label, primary) = match b {
                            Btn::Play => {
                                let label = resume_at.map(|s| format!("Resume {}", kit::clock(s))).unwrap_or_else(|| "Play".into());
                                let label = match &plays_from {
                                    Some(p) => format!("{label} · {p}"),
                                    None => label,
                                };
                                (Some(glyphs::PLAY), label, true)
                            }
                            Btn::StartOver => (Some(glyphs::SKIP_BACK), "Start over".into(), false),
                            Btn::Download => (Some("⤓"), "Download ▾".into(), false),
                            Btn::PlayFrom => (Some(glyphs::PLAYLIST), "Play from ▾".into(), false),
                            Btn::Favourite => (
                                Some(if fav { glyphs::FAVOURITE } else { glyphs::FAVOURITE_OFF }),
                                if fav { "My list".into() } else { "Save to My list".into() },
                                false,
                            ),
                            Btn::Watched => (Some(glyphs::CHECK), if watched { "Watched".into() } else { "Mark watched".into() }, false),
                            Btn::RemoveContinue => (Some(glyphs::CLOSE), "Remove from Continue watching".into(), false),
                        };
                        let style = ButtonStyle {
                            primary,
                            focused: i == self.focus,
                            selected: matches!(b, Btn::Favourite) && fav || matches!(b, Btn::Watched) && watched,
                            bordered: true,
                            ..Default::default()
                        };
                        let resp = button(ui, ctx.theme, icon, &label, style);
                        if i == self.focus {
                            button_rect = Some(resp.rect);
                        }
                        if resp.clicked() {
                            pressed = Some((i, *b));
                        }
                    }
                });
                if let Some(item) = self.item(ctx) {
                    if button(ui, ctx.theme, Some(glyphs::ADD), "Watchlist ▾", ButtonStyle { bordered: true, ..Default::default() }).clicked() {
                        kit::list_picker(ctx, &item, &title);
                    }
                    if let Some(line) = crate::ui::watchlists::membership_line(ctx, &item) {
                        ui.label(line);
                    }
                }
            });
            if scroll && let Some(rect) = button_rect {
                let header = egui::Rect::from_min_max(top, ui.cursor().min);
                ui.scroll_to_rect(if header.height() <= ui.clip_rect().height() { rect.union(header) } else { rect }, None);
            }
            copy_click = self.copies.show(ui, ctx, scroll);
        });
        if let Some((i, b)) = pressed {
            self.focus = i;
            self.press(ctx, b);
        }
        if let Some(click) = copy_click {
            let (super::copies::Click::Pick(k) | super::copies::Click::Menu(k)) = click;
            self.focus = buttons.len() + k;
            self.copies.click(ctx, click);
            menu = false;
        }
        if menu {
            self.item_action(ctx, Action::ContextMenu);
        }
        self.focus = self.focus.min(self.focus_len(ctx).saturating_sub(1));
        self.sync_focus(ctx);
    }
}

/// The detail pages' top bar: `‹ Movies` (the app's Back, when it leads somewhere) and, with
/// `menu`, a ⋯ button for the page's menu at the right. True when ⋯ was clicked.
pub(super) fn detail_bar(ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, menu: bool) -> bool {
    if ctx.back.is_none() && !menu {
        return false;
    }
    let theme = ctx.theme;
    let mut clicked = false;
    section::page_header(ui, ctx, "", |ui| {
        if menu {
            clicked = icon_button(ui, theme, glyphs::MORE, "More (Shift+F10)", ButtonStyle::default()).clicked();
        }
    });
    clicked
}

/// Where a playlist movie or series has no plot (DESIGN §27.7).
pub(super) const NO_DESCRIPTION: &str = "No description in this playlist";

/// The meta of a page without provider details: its category (first group). (Which playlist
/// it comes from is the header's source badge.)
pub(super) fn sparse_meta(ctx: &UiCtx<'_>, group_ids: &[crate::model::GroupId]) -> Vec<String> {
    let vod = &ctx.lib.vod;
    group_ids
        .first()
        .and_then(|g| vod.movie_groups.iter().chain(&vod.series_groups).find(|x| x.id == *g))
        .map(|g| g.display_name().to_owned())
        .into_iter()
        .collect()
}

/// The shared header of the detail pages: dimmed backdrop, poster, title, meta, text blocks
/// and a caller-provided button row.
pub(super) struct DetailHeader<'a> {
    pub title: &'a str,
    pub meta: &'a str,
    /// The plot and whose it is, painted through the spoiler guard
    /// (`ui::widgets::spoiler::field`): a hidden plot is a block that a click (or Ctrl+B)
    /// shows, and the shown one carries 󰈈 to hide it again.
    pub plot: Option<(crate::guard::Subject<'a>, &'a str)>,
    /// (text, secondary) paragraphs after the plot.
    pub text: &'a [(String, bool)],
    pub poster: Option<&'a str>,
    pub backdrop: Option<&'a str>,
    pub playing: bool,
    pub locked: bool,
    /// Details still loading: a skeleton line under the meta.
    pub loading: bool,
    /// The playlist of the item: a source badge (glyph + name) leads the meta line while
    /// Settings ▸ Interface ▸ Playlist on cards shows it — on Auto, whenever two or more
    /// playlists have this `kind` of item, Xtream pages included (DESIGN §13.0, §29).
    pub source: Option<crate::model::SourceId>,
    pub kind: ItemKind,
}

impl DetailHeader<'_> {
    /// Returns whether the header was right-clicked (the page's menu).
    pub(super) fn show(&self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, buttons: impl FnOnce(&mut egui::Ui, &mut UiCtx<'_>)) -> bool {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let rem = theme.rem();
        let width = ui.available_width();
        let compact = ctx.layout == crate::ui::LayoutClass::Compact;
        let top = ui.cursor().min;
        // backdrop band behind the header, dimmed to bg @ 0.85 so text stays readable
        let band_h = (width * 0.36).clamp(160.0 * rem, 360.0 * rem);
        let band = egui::Rect::from_min_size(top, egui::vec2(width, band_h));
        if let Some(url) = self.backdrop.filter(|u| !u.trim().is_empty()) {
            paint_poster_image(ui, theme, ctx.images, Some(url), "", band, ImageKind::Backdrop);
            ui.painter().rect_filled(band, 0.0, with_alpha(t.bg, 0.85));
        }
        ui.add_space(theme.space(SpaceToken::Xxl));
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Huge) + theme.space(SpaceToken::Lg);
            if !compact {
                let pw = (if ctx.layout == crate::ui::LayoutClass::Wide { 200.0 } else { 150.0 } * rem).round();
                let (r, _) = ui.allocate_exact_size(egui::vec2(pw, pw * 1.5), egui::Sense::hover());
                paint_poster_image(ui, theme, ctx.images, self.poster, self.title, r, ImageKind::Poster);
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = theme.space(SpaceToken::Md);
                ui.horizontal_wrapped(|ui| {
                    if self.playing {
                        ui.label(RichText::new(glyphs::PLAY).size(theme.font(TypeToken::Display)).color(t.accent));
                    }
                    ui.add(egui::Label::new(RichText::new(self.title).size(theme.font(TypeToken::Display)).color(t.fg)).wrap());
                    if self.locked {
                        ui.label(RichText::new(glyphs::LOCK).size(theme.font(TypeToken::Title)).color(t.text_secondary));
                    }
                });
                match self.source.and_then(|s| ctx.source_of(self.kind, s)) {
                    Some(src) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                            badge::source_badge(ui, theme, src, false);
                            if !self.meta.is_empty() {
                                ui.label(RichText::new(self.meta).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                            }
                        });
                    }
                    None if !self.meta.is_empty() => {
                        ui.label(RichText::new(self.meta).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    }
                    None => {}
                }
                if self.loading && self.text.is_empty() && self.plot.is_none() {
                    for w in [0.9, 0.75, 0.5] {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width() * w, theme.font(TypeToken::Body)), egui::Sense::hover());
                        ui.painter().rect_filled(r, t.rounding, t.fill_field);
                    }
                }
                if let Some((subject, plot)) = &self.plot {
                    let sp = crate::ui::widgets::spoiler::Spoilers::of(ctx);
                    crate::ui::widgets::spoiler::field(ui, theme, &sp, subject, crate::guard::Field::Plot, plot, theme.font(TypeToken::Body), t.fg);
                }
                for (p, secondary) in self.text {
                    let (size, color) =
                        if *secondary { (theme.font(TypeToken::BodySmall), t.text_secondary) } else { (theme.font(TypeToken::Body), t.fg) };
                    ui.add(egui::Label::new(RichText::new(p).size(size).color(color)).wrap());
                }
                ui.add_space(theme.space(SpaceToken::Md));
                buttons(ui, ctx);
            });
        });
        // a right-click anywhere on the header (poster, title, text; the buttons take only the
        // primary button) opens the page's menu, like the menu key
        let header = egui::Rect::from_min_max(top, egui::pos2(top.x + width, ui.cursor().min.y));
        ui.input(|i| i.pointer.secondary_clicked()) && ui.rect_contains_pointer(header)
    }
}

/// Skeleton page while nothing is known yet.
fn skeleton(ui: &mut egui::Ui, theme: &Theme) {
    let t = &theme.tokens;
    let rem = theme.rem();
    ui.add_space(theme.space(SpaceToken::Xxl));
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(egui::vec2(150.0 * rem, 225.0 * rem), egui::Sense::hover());
        ui.painter().rect_filled(r, t.rounding, t.fill_field);
        ui.vertical(|ui| {
            for w in [0.5, 0.3, 0.9, 0.8] {
                let (r, _) =
                    ui.allocate_exact_size(egui::vec2((ui.available_width() * w).max(40.0), theme.font(TypeToken::Title)), egui::Sense::hover());
                ui.painter().rect_filled(r, t.rounding, t.fill_field);
                ui.add_space(theme.space(SpaceToken::Md));
            }
        });
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn wp(pos: f64, dur: Option<f64>, finished: bool) -> WatchProgress {
        WatchProgress {
            item: ItemRef { source: crate::model::SourceId(1), kind: ItemKind::Movie, key: crate::model::ItemKey("k".into()) },
            position_s: pos,
            duration_s: dur,
            updated_at: 0,
            finished,
            title: String::new(),
            fingerprint: Default::default(),
        }
    }

    use crate::model::{ItemKey, SourceId};
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed, until};

    #[test]
    fn watchlist_film_plays_from_summary_while_details_are_pending() {
        use crate::ui::screens::queue::tests::{seed_watchlist, start_engine};
        use egui_kittest::kittest::Queryable;

        let (mut t, one) = rig();
        let (list, entries) = seed_watchlist(&mut t, "Films", crate::watchlist::PlayOrder::Ordered, 3);
        let player = start_engine(&t);
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        for (n, action) in [Some(Action::Activate), None, Some(Action::ActivateAlt)].into_iter().enumerate() {
            let item = ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{n:010}")) };
            let movie = crate::store::vod::resolve_movie(&h.state().t.db, &item).unwrap().unwrap();
            let origin = crate::ui::watchlists::Origin { list, entry: entries[n] };
            h.state_mut().enter(Route::MovieDetailFromList { movie, origin });
            // Hold the detail read until after Play. The summary already renders an enabled
            // button, so neither keyboard activation nor a click may depend on this worker.
            // Dropping the sender also releases the worker if an assertion panics.
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let app = h.state_mut();
            app.s.loader = Some(app.t.with_ctx(|ctx| {
                Loader::start(ctx, move |db| {
                    let _ = wait.recv();
                    Ok((crate::store::vod::movie(db, movie)?, None))
                })
            }));
            h.run_steps(2);
            assert!(has_label(&h, "Play"));
            assert!(h.state().s.movie.is_none());
            if let Some(action) = action {
                assert!(h.state_mut().act(action));
            } else {
                h.get_by_label("Play").click();
                h.run_steps(2);
            }
            let resume = action != Some(Action::ActivateAlt);
            until(&mut h, "summary's film played through list", |_| {
                player.0.lock().unwrap().iter().any(|c| {
                    matches!(c, PlayerCommand::PlayCopy(p) if p.req.media == MediaRef::Movie(movie) && p.req.origin == PlayOrigin::Queue && p.req.resume == resume)
                })
            });
            until(&mut h, "film list context", |h| h.state().t.services().agent.queue().context == Some(list));
            let rows = crate::store::queue::load(&h.state().t.db).unwrap();
            let row = rows.iter().find(|row| row.item == item).expect("selected film stored");
            assert_eq!(row.watchlist_item_id, Some(origin.entry));
            assert!(row.list_context);
            assert!(h.state().s.movie.is_none(), "playback must precede the detail read");
            assert!(h.state_mut().nav().is_empty(), "no Resume prompt or ordinary fullscreen playback");
            drop(release);
        }
    }

    #[test]
    fn keyboard_scroll_reaches_wrapped_copies_and_returns_to_page_header() {
        use egui_kittest::kittest::Queryable;
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let mut ids = Vec::new();
            for i in 0..10 {
                let s = testkit::source(c, &format!("Playlist {i}"))?;
                ids.push(testkit::named_movies(c, s, &[("EN ★ Dune - 2021", "Dune")])?[0]);
            }
            Ok(ids[0])
        });
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(800.0, 500.0));
        h.state_mut().enter(Route::MovieDetail(id));
        until(&mut h, "copies", |h| has_label(h, "COPIES · 10"));
        h.run_steps(30);
        let top = h.get_by_label("Dune").rect().top();
        h.state_mut().act(Action::End);
        h.run_steps(30);
        let last = h.get_by_label("Playlist 9 · EN").rect();
        assert!(last.top() >= 8.0 && last.bottom() <= 492.0, "last copy: {last:?}");
        h.event(egui::Event::PointerMoved(egui::pos2(400.0, 250.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -10000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(30);
        assert!((h.get_by_label("Playlist 9 · EN").rect().bottom() - last.bottom()).abs() < 1.0, "keyboard reaches touchpad bottom");
        h.event(egui::Event::PointerGone);
        h.state_mut().act(Action::Home);
        h.run_steps(30);
        assert!((h.get_by_label("Dune").rect().top() - top).abs() < 1.0, "Home returns to the top");
        let play = h.get_by_label("Play · Playlist 0").rect();
        assert!(play.top() >= 8.0 && play.bottom() <= 492.0, "Play is visible: {play:?}");
    }

    /// Pretend the fullscreen player drew the video last frame (end-of-playback returns).
    pub(crate) fn fullscreen_placement(t: &mut crate::ui::testing::TestUi) {
        let theme = t.theme.clone();
        let snap = t.snapshot.clone();
        let (video, images) = (&mut t.video, &mut t.images);
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let r = ui.max_rect();
            video.paint(ui, r, Placement::Fullscreen, &theme, images, &snap);
        });
        out.textures_delta.clear();
        t.video.begin_frame();
    }

    fn progress_row(t: &crate::ui::testing::TestUi, key: &str) -> Option<WatchProgress> {
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey(key.into()) };
        crate::store::user::progress(&t.services().store.reader().unwrap(), &item).unwrap()
    }

    #[test]
    fn remove_from_continue_watching_button_menu_delete_and_undo() {
        use egui_kittest::kittest::Queryable;
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::movies(c, s, 5)?;
            for (i, pos, finished) in [(0, 600.0, false), (2, 0.0, false), (3, 600.0, true), (4, 1.0, false)] {
                crate::store::user::save_progress(
                    c,
                    &WatchProgress {
                        item: ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) },
                        ..wp(pos, Some(1200.0), finished)
                    },
                )?;
            }
            Ok(ids)
        });
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        let label = "Remove from Continue watching";
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        until(&mut h, "Resume and Remove", |h| has_label(h, "Resume 10:00") && has_label(h, label));
        h.get_by_label(label).click();
        h.run_steps(2);
        let toast = h.state_mut().undo_toast().expect("removal offers Undo");
        assert_eq!(toast.text, "Removed from Continue watching");
        let (_, undo) = toast.action.unwrap();
        assert!(progress_row(&h.state().t, "m3u:movie0000000000").is_none(), "forgotten, not marked watched");
        assert!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().iter().all(|p| p.item.key.as_str() != "m3u:movie0000000000"));
        until(&mut h, "Play without Remove", |h| has_label(h, "Play") && !has_label(h, label) && !has_label(h, "Start over"));
        h.state_mut().act(undo);
        until(&mut h, "progress restored", |h| has_label(h, "Resume 10:00") && has_label(h, label));
        assert!(
            crate::store::user::continue_watching(&h.state().t.db, 10)
                .unwrap()
                .iter()
                .any(|p| p.item.key.as_str() == "m3u:movie0000000000" && p.position_s == 600.0)
        );

        // Delete resolves through the page's real key context, with the buttons focused.
        let delete = crate::keymap::resolve(
            &h.state().s.key_contexts(),
            &egui::Event::Key { key: egui::Key::Delete, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE },
        )
        .unwrap();
        assert_eq!(delete, Action::Remove);
        h.state_mut().act(delete);
        let (_, undo) = h.state_mut().undo_toast().unwrap().action.unwrap();
        until(&mut h, "Delete removed progress", |h| !has_label(h, label));
        h.state_mut().act(undo);
        until(&mut h, "Undo restored Remove", |h| has_label(h, label));
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::ContextMenu(menu))] = nav.as_slice() else { panic!("{nav:?}") };
        assert!(menu.items.iter().any(|i| i.label == label && i.action == Action::Remove && i.shortcut.as_deref() == Some("Delete")));
        h.state_mut().result(OverlayResult::Chosen(Action::Remove));
        until(&mut h, "menu removed progress", |h| !has_label(h, label) && progress_row(&h.state().t, "m3u:movie0000000000").is_none());

        // No progress, zero progress and finished progress offer no removal action.
        for id in &ids[1..4] {
            h.state_mut().enter(Route::MovieDetail(*id));
            until(&mut h, "the next film", |h| h.state().s.movie.as_ref().is_some_and(|m| m.id == *id));
            assert!(!has_label(&h, label));
            h.state_mut().act(Action::ContextMenu);
            let nav = h.state_mut().nav();
            let [NavRequest::Open(OverlayRequest::ContextMenu(menu))] = nav.as_slice() else { panic!("{nav:?}") };
            assert!(menu.items.iter().all(|i| i.action != Action::Remove));
        }
        // Continue watching starts before the configurable Resume threshold.
        h.state_mut().enter(Route::MovieDetail(ids[4]));
        until(&mut h, "short progress can be removed", |h| has_label(h, "Play") && has_label(h, label));
    }

    #[test]
    fn remove_from_continue_watching_forgets_every_movie_copy() {
        let (mut t, one) = rig();
        let (id, refs) = seed(&mut t, |c| {
            let mut ids = Vec::new();
            let mut refs = Vec::new();
            for (name, finished) in [("Alpha", true), ("Bravo", false)] {
                let source = testkit::source(c, name)?;
                ids.push(testkit::named_movies(c, source, &[("EN ★ Dune - 2021", "Dune")])?[0]);
                let item = ItemRef { source, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) };
                crate::store::user::save_progress(c, &WatchProgress { item: item.clone(), ..wp(600.0, Some(1200.0), finished) })?;
                refs.push(item);
            }
            Ok((ids[0], refs))
        });
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::MovieDetail(id));
        until(&mut h, "combined film removal", |h| has_label(h, "COPIES · 2") && has_label(h, "Remove from Continue watching"));
        // The focus walk includes Download and reaches Remove after Watched.
        for _ in 0..6 {
            h.state_mut().act(Action::FocusNext);
        }
        h.state_mut().act(Action::Activate);
        let toast = h.state_mut().undo_toast().expect("removal offers Undo");
        assert_eq!(toast.text, "Removed from Continue watching");
        assert!(refs.iter().all(|r| crate::store::user::progress(&h.state().t.db, r).unwrap().is_none()));
        assert!(crate::store::user::continue_watching(&h.state().t.db, 10).unwrap().is_empty());
        until(&mut h, "Play without Remove", |h| !has_label(h, "Remove from Continue watching") && testkit::has_label_containing(h, "Play ·"));
        h.state_mut().act(toast.action.unwrap().1);
        until(&mut h, "both copies restored", |h| {
            has_label(h, "Remove from Continue watching") && refs.iter().all(|r| crate::store::user::progress(&h.state().t.db, r).unwrap().is_some())
        });
        assert!(crate::store::user::progress(&h.state().t.db, &refs[0]).unwrap().unwrap().finished);
        assert!(!crate::store::user::progress(&h.state().t.db, &refs[1]).unwrap().unwrap().finished);
    }

    #[test]
    fn page_buttons_watched_toggle_and_return_after_the_movie_ends() {
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ids = testkit::movies(c, s, 2)?;
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
            // a playlist movie as the M3U import writes it: no plot, no details
            c.execute("UPDATE movie SET plot = NULL, details_at = NULL WHERE id = ?1", [ids[1].0])?;
            Ok(ids)
        });
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        until(&mut h, "the details", |h| has_label(h, "Plot of movie 0."));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Detail]);
        assert!(has_label(&h, "Resume 10:00") && has_label(&h, "Start over") && has_label(&h, "Save to My list") && has_label(&h, "Mark watched"));
        // → past Start over, Download and My list to Mark watched.
        for _ in 0..4 {
            h.state_mut().act(Action::Right);
        }
        h.state_mut().act(Action::Activate);
        until(&mut h, "the watched mark", |h| progress_row(&h.state().t, "m3u:movie0000000000").is_some_and(|p| p.finished));
        until(&mut h, "the button follows", |h| has_label(h, "Watched"));
        // Ctrl+W again: unwatched (the row is forgotten)
        h.state_mut().act(Action::MarkWatched);
        until(&mut h, "unwatched", |h| progress_row(&h.state().t, "m3u:movie0000000000").is_none());
        // Ctrl+F: favourite
        h.state_mut().act(Action::ToggleFavourite);
        h.run_steps(1);
        assert!(has_label(&h, "My list"));
        // Enter on the first button plays; Shift+Enter starts over
        h.state_mut().act(Action::Home);
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state_mut().nav(), vec![crate::ui::NavRequest::Push(Route::Player)]);
        // the movie ends while the fullscreen player shows it: back to this page
        fullscreen_placement(&mut h.state_mut().t);
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000000".into()) };
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(item), finished: true }));
        h.run_steps(1);
        // (leave the player; the page is already under it, so the push is a no-op for the app)
        assert_eq!(
            h.state_mut().nav(),
            vec![crate::ui::NavRequest::Back, crate::ui::NavRequest::Push(Route::MovieDetail(ids[0]))],
            "returns to the detail page"
        );
        // a movie started anywhere else (grid, Home, search) also ends on its page
        let other = ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey("m3u:movie0000000001".into()) };
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(other.clone()), finished: true }));
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![crate::ui::NavRequest::Back, crate::ui::NavRequest::Push(Route::MovieDetail(ids[1]))]);
        // stopped before the end, or in the mini player: nothing moves
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(other.clone()), finished: false }));
        h.run_steps(1);
        assert!(h.state_mut().nav().is_empty());
        // a film Up next plays: the player stays for the next one
        std::sync::Arc::make_mut(&mut h.state_mut().t.snapshot).origin = Some(PlayOrigin::Queue);
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(other), finished: true }));
        h.run_steps(1);
        assert!(h.state_mut().nav().is_empty(), "Up next keeps the player");
        // DESIGN §27.7: the playlist movie's page has nothing to load: year · category, and a
        // muted "no description" where the plot (or a loading skeleton) would be
        h.state_mut().enter(Route::MovieDetail(ids[1]));
        until(&mut h, "the sparse page", |h| has_label(h, NO_DESCRIPTION));
        assert!(has_label(&h, "1981 · ★ 1.0 · Movies"), "year · rating · category");
        // a vanished movie
        h.state_mut().enter(Route::MovieDetail(MovieId(999)));
        until(&mut h, "the missing state", |h| has_label(h, "This movie is no longer in your playlists"));
    }

    /// A title in several copies (DESIGN §29.14): the page lists them as mini-cards with the
    /// one that plays first marked and Play names its playlist; Enter on a mini-card picks the
    /// copy Play uses (a locked one after the PIN); Ctrl+O lists them all; a mini-card's menu
    /// pins a copy ("Always play from") and takes one apart ("Not the same title", stored, with
    /// an Undo that puts it back).
    #[test]
    fn copies_section_picks_pins_and_splits_with_undo() {
        use crate::store::titles::{LinkAction, links};
        let (mut t, one) = rig();
        let (a, b, ids, bravo) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let ids = testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("FR ★ Dune - 2021", "Dune (FR)")])?;
            let bravo = testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?;
            Ok((a, b, ids, bravo[0]))
        });
        let named = |s: SourceId, i: usize| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) };
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        until(&mut h, "the copies", |h| has_label(h, "COPIES · 3"));
        assert!(has_label(&h, "Alpha · EN · plays first") && has_label(&h, "Bravo · EN") && has_label(&h, "Alpha · FR"));
        assert!(has_label(&h, "Play · Alpha") && has_label(&h, "Play from ▾"));
        // another copy's page (Search, a category, Favourites) plays the title's first copy too;
        // a copy opened on purpose (fanned out, one playlist's list) plays itself
        h.state_mut().enter(Route::MovieDetail(bravo));
        h.run_steps(1);
        assert!(has_label(&h, "COPIES · 3") && has_label(&h, "Play · Alpha"));
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        h.run_steps(1);
        h.state_mut().t.with_ctx(|ctx| crate::ui::stacks::open_copy(ctx, VodKind::Movie(bravo)));
        h.state_mut().nav();
        h.state_mut().enter(Route::MovieDetail(bravo));
        h.run_steps(1);
        assert!(has_label(&h, "Play · Bravo"));
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        h.run_steps(1);
        assert!(has_label(&h, "Play · Alpha"));
        // → past the five buttons onto the mini-cards; Enter on Bravo's: Play uses it
        for _ in 0..6 {
            h.state_mut().act(Action::Right);
        }
        h.state_mut().act(Action::Activate);
        h.run_steps(1);
        assert!(has_label(&h, "Play · Bravo") && has_label(&h, "Bravo · EN · plays first"));
        // Ctrl+O: every copy, the picked one marked
        h.state_mut().nav();
        h.state_mut().act(Action::PlayFrom);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.title == "Play from" && m.items.len() == 3 && m.items[1].shortcut.as_deref() == Some("plays first")),
            "{nav:?}"
        );
        // the French copy's menu: always play from it
        h.state_mut().act(Action::Right);
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        let [NavRequest::Open(OverlayRequest::ContextMenu(m))] = nav.as_slice() else { panic!("{nav:?}") };
        let items: Vec<&str> = m.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(items, ["Play from Alpha", "Always play from Alpha", "Not the same title"]);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(super::super::copies::CARD_PIN)));
        let stored =
            |h: &egui_kittest::Harness<'static, testkit::App<MovieDetailScreen>>| links(&h.state().t.services().store.reader().unwrap()).unwrap();
        until(&mut h, "the pin", |h| stored(h).iter().any(|l| l.item == named(a, 1) && l.action == LinkAction::Pin));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(1);
        h.state_mut().act(Action::ContextMenu);
        let nav = h.state_mut().nav();
        assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.items[1].label == "Don't always play from it"));
        // Bravo's copy is not the same title: it leaves the stack, with an Undo
        h.state_mut().act(Action::Left);
        h.state_mut().act(Action::ContextMenu);
        h.state_mut().nav();
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(super::super::copies::CARD_SPLIT)));
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Dune is shown on its own now");
        let Some((_, undo)) = toast.action else { unreachable!() };
        until(&mut h, "the split", |h| stored(h).iter().any(|l| l.item == named(b, 0) && l.action == LinkAction::Split));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(1);
        assert!(has_label(&h, "COPIES · 2") && has_label(&h, "Alpha · FR · always plays first"), "the pinned copy plays first now");
        assert!(has_label(&h, "Play · Alpha"));
        h.state_mut().act(undo);
        until(&mut h, "the split undone", |h| !stored(h).iter().any(|l| l.action == LinkAction::Split));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(1);
        assert!(has_label(&h, "COPIES · 3"));
        // a locked copy: picking it asks for the PIN, then Play uses it
        let item = named(b, 0);
        seed(&mut h.state_mut().t, move |c| {
            crate::store::user::set_item_override(c, &item, &crate::model::ItemOverride { locked: true, ..Default::default() })
        });
        let mut cfg = (*h.state().t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
            ..Default::default()
        };
        h.state_mut().t.config = std::sync::Arc::new(cfg);
        h.run_steps(1);
        assert!(has_label(&h, "Bravo · EN · locked"));
        // Five buttons, then Bravo's copy second.
        h.state_mut().s.focus = 6;
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]));
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        app.result(OverlayResult::Unlocked(Action::PlayFrom));
        h.run_steps(1);
        assert!(has_label(&h, "Play · Bravo"), "picked after the PIN");
    }

    /// P3 (DESIGN §29.15): on the page of a combined title ★ / ✓ / hide / lock / rename act
    /// like on its stack card — ★ shows for a favourite copy and removing it takes every copy
    /// out, with an Undo that puts them back; ✓ marks the copy Play uses and unmarking forgets
    /// every copy's progress (Undo restores the rows); hide and lock cover every copy; F2
    /// renames every copy. A page opened for one copy on purpose keeps acting on that copy.
    #[test]
    fn a_combined_pages_actions_cover_every_copy_with_undo() {
        let (mut t, one) = rig();
        let (a, b, alpha, bravo) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let alpha = testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune")])?[0];
            let bravo = testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?[0];
            let list = crate::store::user::default_fav_list(c)?;
            let fav = ItemRef { source: b, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) };
            crate::ui::screens::home::testkit::save_favourite(c, list, &fav, true, 1)?;
            Ok((a, b, alpha, bravo))
        });
        let copy = |s: SourceId| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) };
        type H = egui_kittest::Harness<'static, testkit::App<MovieDetailScreen>>;
        let reader = |h: &H| h.state().t.services().store.reader().unwrap();
        let fav_rows = |h: &H| {
            let r = reader(h);
            let n = |s: SourceId| -> i64 { r.query_row("SELECT count(*) FROM watchlist_item WHERE source_id = ?1", [s.0], |r| r.get(0)).unwrap() };
            [a, b].into_iter().filter(|s| n(*s) > 0).collect::<Vec<_>>()
        };
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::MovieDetail(alpha));
        until(&mut h, "the copies", |h| has_label(h, "COPIES · 2"));
        assert!(has_label(&h, "My list"), "Bravo's copy is a favourite: the title is");
        // ★ off: every copy leaves Favourites; Undo puts Bravo's back
        h.state_mut().act(Action::ToggleFavourite);
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Removed from My list");
        assert!(fav_rows(&h).is_empty());
        let Some((_, undo)) = toast.action else { unreachable!() };
        h.state_mut().act(undo);
        until(&mut h, "Bravo's favourite back", |h| fav_rows(h) == vec![b]);
        // ✓: the copy Play uses (Alpha's) is marked; the button follows
        h.state_mut().t.toasts.clear();
        h.state_mut().act(Action::MarkWatched);
        until(&mut h, "Alpha's copy watched", |h| progress_row(&h.state().t, "m3u:named0000").is_some_and(|p| p.finished));
        until(&mut h, "the button follows", |h| has_label(h, "Watched"));
        let bravo_progress = || {
            let p = WatchProgress {
                item: copy(b),
                position_s: 300.0,
                duration_s: Some(6000.0),
                updated_at: 2,
                finished: false,
                title: "Dune".into(),
                fingerprint: Default::default(),
            };
            move |c: &mut rusqlite::Connection| crate::store::user::save_progress(c, &p)
        };
        seed(&mut h.state_mut().t, bravo_progress());
        // ✓ off: every copy's progress is forgotten, Undo restores both rows
        h.state_mut().act(Action::MarkWatched);
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Marked unwatched in Alpha and Bravo");
        let progress_of = |h: &H, s: SourceId| crate::store::user::progress(&reader(h), &copy(s)).unwrap();
        assert!(progress_of(&h, a).is_none() && progress_of(&h, b).is_none());
        let Some((_, undo)) = toast.action else { unreachable!() };
        h.state_mut().act(undo);
        until(&mut h, "both rows back", |h| {
            progress_of(h, a).is_some_and(|p| p.finished) && progress_of(h, b).is_some_and(|p| p.position_s == 300.0)
        });
        // F2: rename every copy
        h.state_mut().nav();
        h.state_mut().act(Action::Rename);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Rename(r))]
                if matches!(&r.target, crate::ui::overlays::RenameTarget::Copies { items } if items.len() == 2)),
            "{nav:?}"
        );
        // Ctrl+L (PIN set, session open): every copy locked
        let mut cfg = (*h.state().t.config).clone();
        cfg.parental = crate::config::ParentalConfig {
            enabled: true,
            pin_salt: Some("s".into()),
            pin_hash: Some(crate::ui::parental::hash_pin("1234", "s")),
            relock_min: 0,
            ..Default::default()
        };
        h.state_mut().t.config = std::sync::Arc::new(cfg);
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        h.state_mut().t.toasts.clear();
        h.state_mut().act(Action::ToggleLock);
        let ov = |h: &H, s: SourceId| crate::store::user::item_override(&reader(h), &copy(s)).unwrap().unwrap_or_default();
        until(&mut h, "both copies locked", |h| ov(h, a).locked && ov(h, b).locked);
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert_eq!(toast.text, "Locked Dune in Alpha and Bravo");
        // Ctrl+H: every copy hidden, the toast names both playlists
        h.state_mut().t.toasts.clear();
        h.state_mut().act(Action::ToggleHidden);
        let toast = h.state_mut().undo_toast().expect("an Undo toast");
        assert!(toast.text.starts_with("Hid Dune in Alpha and Bravo"), "{}", toast.text);
        until(&mut h, "both copies hidden", |h| ov(h, a).hidden && ov(h, b).hidden);
        let Some((_, undo)) = toast.action else { unreachable!() };
        h.state_mut().act(undo);
        until(&mut h, "unhidden", |h| !ov(h, a).hidden && !ov(h, b).hidden);
        // Bravo's copy opened on purpose (fanned out, its playlist's list): F2 renames it alone
        h.state_mut().t.with_ctx(|ctx| crate::ui::stacks::open_copy(ctx, VodKind::Movie(bravo)));
        h.state_mut().nav();
        h.state_mut().enter(Route::MovieDetail(bravo));
        h.run_steps(2);
        h.state_mut().act(Action::Rename);
        let nav = h.state_mut().nav();
        assert!(
            matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Rename(r))]
                if r.target == crate::ui::overlays::RenameTarget::Item { item: copy(b) }),
            "{nav:?}"
        );
    }

    #[test]
    fn resume_threshold_and_fraction() {
        assert!(resumable(&wp(61.0, Some(100.0), false), 60));
        assert!(!resumable(&wp(60.0, Some(100.0), false), 60));
        assert!(!resumable(&wp(500.0, Some(1000.0), true), 60), "finished items start over");
        assert_eq!(fraction(&wp(25.0, Some(100.0), false)), Some(0.25));
        assert_eq!(fraction(&wp(25.0, None, false)), None);
        assert_eq!(fraction(&wp(250.0, Some(100.0), false)), Some(1.0));
    }
}
