//! Movie detail (DESIGN §13.6): backdrop (dimmed), poster, title, year · duration · rating ·
//! genre, plot, cast/director, buttons Play/Resume · Start over · Favourite · Mark watched.
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
use crate::player::PlayerEvent;
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
            ctx.open(OverlayRequest::Resume(ResumeSpec { media, title, position_s: p.position_s, duration_s: p.duration_s }));
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
    Play,
    StartOver,
    /// "Play from ▾": the title's copies (DESIGN §29.14).
    PlayFrom,
    Favourite,
    Watched,
}

#[derive(Default)]
pub struct MovieDetailScreen {
    id: Option<MovieId>,
    movie: Option<Movie>,
    progress: Option<WatchProgress>,
    loader: Option<Loader<(Movie, Option<WatchProgress>)>>,
    /// The row is gone (vanished on refresh).
    missing: bool,
    /// `LoadMovieDetails` was submitted for this id.
    details_requested: Option<MovieId>,
    /// The focused button, then the Copies mini-cards (`buttons().len() + k`).
    focus: usize,
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

    fn watched(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.finished)
    }

    /// Where Play resumes: the page's progress, or that of the other copy that plays.
    fn resume_at(&self, ctx: &UiCtx<'_>) -> Option<f64> {
        match self.other_copy(ctx) {
            Some(c) => self.copies.resume_at(ctx, c),
            None => self.progress.as_ref().filter(|p| resumable(p, ctx.config.player.resume_min_s)).map(|p| p.position_s),
        }
    }

    /// The copy Play uses when it isn't the page's own (DESIGN §29.14).
    fn other_copy(&self, ctx: &UiCtx<'_>) -> Option<u32> {
        self.copies.plays(ctx).filter(|c| Some(*c) != self.copies.page())
    }

    fn buttons(&self, ctx: &UiCtx<'_>) -> Vec<Btn> {
        let mut v = vec![Btn::Play];
        if self.resume_at(ctx).is_some() {
            v.push(Btn::StartOver);
        }
        if self.copies.is_stacked() {
            v.push(Btn::PlayFrom);
        }
        v.extend([Btn::Favourite, Btn::Watched]);
        v
    }

    fn play(&mut self, ctx: &mut UiCtx<'_>, resume: bool) {
        let Some(id) = self.id else { return };
        if self.missing {
            return;
        }
        if let Some(c) = self.other_copy(ctx) {
            self.play_copy(ctx, c, resume);
            return;
        }
        if self.locked(ctx) {
            kit::ask_pin(ctx, if resume { Action::Activate } else { Action::ActivateAlt }, "This title is locked");
            return;
        }
        kit::play_media(ctx, MediaRef::Movie(id), resume, PlayOrigin::Fullscreen);
    }

    /// Play another copy of the title (`resume`: at its saved position).
    fn play_copy(&mut self, ctx: &mut UiCtx<'_>, c: u32, resume: bool) {
        let Some(v) = ctx.lib.vod.movies.get(c as usize) else { return };
        let VodKind::Movie(id) = v.kind else { return };
        if v.locked && !kit::unlocked(ctx) {
            kit::ask_pin(ctx, if resume { Action::Activate } else { Action::ActivateAlt }, "This copy is locked");
            return;
        }
        kit::play_media(ctx, MediaRef::Movie(id), resume, PlayOrigin::Fullscreen);
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
        let Some(item) = self.item(ctx) else { return };
        let title = self.title(ctx);
        match action {
            Action::AddToList => kit::list_picker(ctx, &item, &title),
            Action::Rename => kit::rename_item(ctx, &item, &title),
            Action::ToggleHidden => kit::hide_item(ctx, &item, &title),
            Action::ToggleLock => {
                let (locked, by_group) = self.lock_state(ctx);
                kit::toggle_lock(ctx, &item, locked, by_group);
            }
            Action::ContextMenu => {
                let fav = ctx.favs.is_fav(&item);
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
                items.extend([
                    mi(glyphs::FAVOURITE, if fav { "Remove from Favourites" } else { "Add to Favourites" }, Action::ToggleFavourite, "Ctrl+F"),
                    mi(glyphs::FAV_LIST, "Favourites lists…", Action::AddToList, "Ctrl+Shift+F"),
                    mi(glyphs::CHECK, if self.watched() { "Mark unwatched" } else { "Mark watched" }, Action::MarkWatched, "Ctrl+W"),
                    mi(glyphs::TAG, "Rename…", Action::Rename, "F2"),
                    mi(glyphs::LOCK, "Lock / unlock", Action::ToggleLock, "Ctrl+L"),
                    mi(glyphs::HIDDEN, "Hide", Action::ToggleHidden, "Ctrl+H"),
                ]);
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
            Btn::Play => self.play(ctx, true),
            Btn::StartOver => self.play(ctx, false),
            Btn::PlayFrom => self.copies.play_from_menu(ctx),
            Btn::Favourite => {
                if let Some(item) = self.item(ctx) {
                    kit::toggle_favourite(ctx, &item);
                }
            }
            Btn::Watched => self.toggle_watched(ctx),
        }
    }

    fn toggle_watched(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(item) = self.item(ctx) else { return };
        let on = !self.watched();
        let title = self.title(ctx);
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
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Detail]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let Route::MovieDetail(id) = route else { return };
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
                // wherever it was started (grid, Home, search, this page)
                if *finished && ctx.video.placement() == Placement::Fullscreen {
                    return_to_page(ctx, item);
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let n = self.focus_len(ctx);
        let buttons = self.buttons(ctx).len();
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
            Action::Undo => {
                self.copies.undo(ctx);
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
            super::copies::Outcome::Act(super::copies::CopyAct::Play(c)) => return self.play_copy(ctx, c, true),
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Movie(id))) => return ctx.push(Route::MovieDetail(id)),
            super::copies::Outcome::Act(super::copies::CopyAct::Open(VodKind::Series(id))) => return ctx.push(Route::SeriesDetail(id)),
        }
        match result {
            OverlayResult::Chosen(Action::PlayFrom) => self.copies.play_from_menu(ctx),
            OverlayResult::Unlocked(Action::Activate) | OverlayResult::Chosen(Action::Activate) => self.play(ctx, true),
            OverlayResult::Unlocked(Action::ActivateAlt) | OverlayResult::Chosen(Action::ActivateAlt) => self.play(ctx, false),
            OverlayResult::Unlocked(Action::ToggleLock) => self.item_action(ctx, Action::ToggleLock),
            OverlayResult::Chosen(Action::ToggleFavourite) => self.press(ctx, Btn::Favourite),
            OverlayResult::Chosen(Action::MarkWatched) => self.toggle_watched(ctx),
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
        if let Some(m) = &m {
            if let Some(p) = m.plot.as_deref().filter(|p| !p.trim().is_empty()) {
                text.push((p.to_owned(), false));
            }
            if let Some(c) = m.cast.as_deref().filter(|c| !c.trim().is_empty()) {
                text.push((format!("Cast: {c}"), true));
            }
            if let Some(d) = m.director.as_deref().filter(|d| !d.trim().is_empty()) {
                text.push((format!("Director: {d}"), true));
            }
        }
        if sparse && text.is_empty() {
            text.push((NO_DESCRIPTION.to_owned(), true));
        }
        let loading = !sparse && (m.is_none() || self.loader.is_some() && m.as_ref().is_some_and(|m| m.details_loaded_at.is_none()));
        let playing = ctx.player.playable.as_ref().is_some_and(|p| self.id.is_some_and(|id| p.media == MediaRef::Movie(id)));
        let buttons = self.buttons(ctx);
        let resume_at = self.resume_at(ctx);
        let fav = self.item(ctx).is_some_and(|i| ctx.favs.is_fav(&i));
        let watched = self.watched();
        let locked = m.as_ref().map(|m| m.locked).or(summary.as_ref().map(|s| s.locked)).unwrap_or(false);
        // several copies: the Play button names the playlist it plays from
        let plays_from = self.copies.plays(ctx).and_then(|c| self.copies.playlist(ctx, c)).map(str::to_owned);
        let mut pressed = None;
        let mut copy_click = None;
        egui::ScrollArea::vertical().id_salt("movie-detail").auto_shrink([false, false]).show(ui, |ui| {
            let header = DetailHeader {
                title: &title,
                meta: &meta.join(" · "),
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
                            Btn::PlayFrom => (Some(glyphs::PLAYLIST), "Play from ▾".into(), false),
                            Btn::Favourite => (
                                Some(if fav { glyphs::FAVOURITE } else { glyphs::FAVOURITE_OFF }),
                                if fav { "Favourite".into() } else { "Add to Favourites".into() },
                                false,
                            ),
                            Btn::Watched => (Some(glyphs::CHECK), if watched { "Watched".into() } else { "Mark watched".into() }, false),
                        };
                        let style = ButtonStyle {
                            primary,
                            focused: i == self.focus,
                            selected: matches!(b, Btn::Favourite) && fav || matches!(b, Btn::Watched) && watched,
                            bordered: true,
                            ..Default::default()
                        };
                        if button(ui, ctx.theme, icon, &label, style).clicked() {
                            pressed = Some((i, *b));
                        }
                    }
                });
            });
            copy_click = self.copies.show(ui, ctx);
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
    /// (text, secondary) paragraphs.
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
                if self.loading && self.text.is_empty() {
                    for w in [0.9, 0.75, 0.5] {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width() * w, theme.font(TypeToken::Body)), egui::Sense::hover());
                        ui.painter().rect_filled(r, t.rounding, t.fill_field);
                    }
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
        assert!(has_label(&h, "Resume 10:00") && has_label(&h, "Start over") && has_label(&h, "Add to Favourites") && has_label(&h, "Mark watched"));
        // → to "Mark watched", Enter: stored as finished
        for _ in 0..3 {
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
        assert!(has_label(&h, "Favourite"));
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
        h.state().t.services().bus.send(AppEvent::Player(PlayerEvent::Ended { item: Some(other), finished: false }));
        h.run_steps(1);
        assert!(h.state_mut().nav().is_empty());
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
        let (a, b, ids) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let ids = testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("FR ★ Dune - 2021", "Dune (FR)")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?;
            Ok((a, b, ids))
        });
        let named = |s: SourceId, i: usize| ItemRef { source: s, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) };
        let mut h = harness(t, one, MovieDetailScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::MovieDetail(ids[0]));
        until(&mut h, "the copies", |h| has_label(h, "COPIES · 3"));
        assert!(has_label(&h, "Alpha · EN · plays first") && has_label(&h, "Bravo · EN") && has_label(&h, "Alpha · FR"));
        assert!(has_label(&h, "Play · Alpha") && has_label(&h, "Play from ▾"));
        // → past the four buttons onto the mini-cards; Enter on Bravo's: Play uses it
        for _ in 0..5 {
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
        let toast = h.state().t.toasts.last().cloned().unwrap();
        assert_eq!(toast.action.map(|a| a.1), Some(Action::Undo), "{}", toast.text);
        until(&mut h, "the split", |h| stored(h).iter().any(|l| l.item == named(b, 0) && l.action == LinkAction::Split));
        testkit::reload_library(&mut h.state_mut().t);
        h.run_steps(1);
        assert!(has_label(&h, "COPIES · 2") && has_label(&h, "Alpha · FR · always plays first"), "the pinned copy plays first now");
        assert!(has_label(&h, "Play · Alpha"));
        h.state_mut().act(Action::Undo);
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
        };
        h.state_mut().t.config = std::sync::Arc::new(cfg);
        h.run_steps(1);
        assert!(has_label(&h, "Bravo · EN · locked"));
        // (four buttons, then Bravo's copy second)
        h.state_mut().s.focus = 5;
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]));
        let app = h.state_mut();
        let p = app.t.config.parental.clone();
        assert!(app.t.parental.unlock("1234", &p));
        app.result(OverlayResult::Unlocked(Action::PlayFrom));
        h.run_steps(1);
        assert!(has_label(&h, "Play · Bravo"), "picked after the PIN");
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
