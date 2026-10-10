//! Movie detail (DESIGN §13.6): backdrop (dimmed), poster, title, year · duration · rating ·
//! genre, plot, cast/director, buttons Play/Resume · Start over · Favourite · Mark watched.
//! Loads details lazily (`JobKind::LoadMovieDetails`) with a skeleton state. Key context
//! `[Detail]`. When a movie ends it is marked watched and the player returns here. Owner: WP-12.
//!
//! Data: the list-level row (`Library`) shows at once; the full row and its progress are read
//! off the UI thread (`kit::Loader`) and re-read on `DataScope::{VodDetails, Progress,
//! Overrides}`. A detail-load failure keeps the list-level data and toasts. Locked movies ask
//! for the PIN before playing. Enter on the main button plays or resumes; Shift+Enter always
//! starts over.

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::jobs::{JobEvent, JobKind};
use crate::keymap::{Action, KeyContext};
use crate::model::{ItemKind, ItemRef, MediaRef, Movie, MovieId, PlayOrigin, SourceKind, VodKind, VodSummary, WatchProgress};
use crate::msg::{AppEvent, DataScope};
use crate::player::PlayerEvent;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::overlays::{OverlayResult, ResumeSpec};
use crate::ui::video_host::Placement;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::{empty_state, glyphs};
use crate::ui::widgets::poster::paint_poster_image;
use crate::images::ImageKind;
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
pub(crate) fn resumable(p: &WatchProgress, min_s: u32) -> bool { !p.finished && p.position_s > f64::from(min_s) }

/// Progress 0..1 of a row (unknown duration → none).
pub(crate) fn fraction(p: &WatchProgress) -> Option<f32> {
    p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d).clamp(0.0, 1.0) as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Btn {
    Play,
    StartOver,
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
    focus: usize,
    /// The movie this page started (end of playback returns here).
    started: Option<MovieId>,
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

    fn summary<'l>(&self, ctx: &'l UiCtx<'_>) -> Option<&'l VodSummary> { self.id.and_then(|id| ctx.lib.vod.find(VodKind::Movie(id))) }

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

    fn watched(&self) -> bool { self.progress.as_ref().is_some_and(|p| p.finished) }

    fn resume_at(&self, ctx: &UiCtx<'_>) -> Option<f64> {
        self.progress.as_ref().filter(|p| resumable(p, ctx.config.player.resume_min_s)).map(|p| p.position_s)
    }

    fn buttons(&self, ctx: &UiCtx<'_>) -> Vec<Btn> {
        let mut v = vec![Btn::Play];
        if self.resume_at(ctx).is_some() {
            v.push(Btn::StartOver);
        }
        v.extend([Btn::Favourite, Btn::Watched]);
        v
    }

    fn play(&mut self, ctx: &mut UiCtx<'_>, resume: bool) {
        let Some(id) = self.id else { return };
        if self.missing {
            return;
        }
        if self.locked(ctx) {
            kit::ask_pin(ctx, if resume { Action::Activate } else { Action::ActivateAlt }, "This title is locked");
            return;
        }
        self.started = Some(id);
        kit::play_media(ctx, MediaRef::Movie(id), resume, PlayOrigin::Fullscreen);
    }

    fn press(&mut self, ctx: &mut UiCtx<'_>, b: Btn) {
        match b {
            Btn::Play => self.play(ctx, true),
            Btn::StartOver => self.play(ctx, false),
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
        self.focus = self.focus.min(self.buttons(ctx).len() - 1);
    }
}

impl Screen for MovieDetailScreen {
    fn id(&self) -> ScreenId { ScreenId::MovieDetail }
    fn key_contexts(&self) -> Vec<KeyContext> { vec![KeyContext::Detail] }

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
        let Some(id) = self.id else { return };
        match ev {
            AppEvent::Data(DataScope::VodDetails | DataScope::Progress | DataScope::Overrides | DataScope::Vod | DataScope::All) => self.reload(ctx),
            AppEvent::Job(JobEvent::Finished { kind: JobKind::LoadMovieDetails(m), outcome: Err(msg), .. }) if *m == id => {
                ctx.toast(crate::msg::Toast::error(format!("Couldn't load the details: {}", crate::redact::redact(msg))));
            }
            AppEvent::Player(PlayerEvent::Ended { item: Some(item), .. }) if Some(item) == self.item(ctx).as_ref() => {
                self.reload(ctx);
                // DESIGN §13.6: a finished movie returns from the player to its page
                if self.started == Some(id) && ctx.video.placement() == Placement::Fullscreen {
                    self.started = None;
                    ctx.navigate(NavRequest::Back);
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let n = self.buttons(ctx).len();
        match action {
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
            Action::ToggleLock => {
                if let Some(item) = self.item(ctx) {
                    let locked = self.movie.as_ref().map(|m| m.locked).unwrap_or(false);
                    kit::toggle_lock(ctx, &item, locked);
                }
                true
            }
            Action::TabPrev | Action::TabNext => true,
            _ => false,
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Unlocked(Action::Activate) => self.play(ctx, true),
            OverlayResult::Unlocked(Action::ActivateAlt) => self.play(ctx, false),
            OverlayResult::Unlocked(Action::ToggleLock) => {
                if let Some(item) = self.item(ctx) {
                    let locked = self.movie.as_ref().map(|m| m.locked).unwrap_or(false);
                    kit::toggle_lock(ctx, &item, locked);
                }
            }
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
            empty_state::empty_state(ui, theme, glyphs::MOVIES, "This movie is no longer in your playlists", None, None);
            return;
        }
        let summary = self.summary(ctx).cloned();
        if self.movie.is_none() && summary.is_none() {
            skeleton(ui, theme);
            return;
        }
        let m = self.movie.clone();
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
        let loading = m.is_none() || self.loader.is_some() && m.as_ref().is_some_and(|m| m.details_loaded_at.is_none());
        let playing = ctx.player.playable.as_ref().is_some_and(|p| self.id.is_some_and(|id| p.media == MediaRef::Movie(id)));
        let buttons = self.buttons(ctx);
        let resume_at = self.resume_at(ctx);
        let fav = self.item(ctx).is_some_and(|i| ctx.favs.is_fav(&i));
        let watched = self.watched();
        let locked = m.as_ref().map(|m| m.locked).or(summary.as_ref().map(|s| s.locked)).unwrap_or(false);
        let mut pressed = None;
        egui::ScrollArea::vertical().id_salt("movie-detail").auto_shrink([false, false]).show(ui, |ui| {
            let header = DetailHeader { title: &title, meta: &meta.join(" · "), text: &text, poster: poster_url.as_deref(), backdrop: backdrop.as_deref(), playing, locked, loading };
            header.show(ui, ctx, |ui, ctx| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Md));
                    for (i, b) in buttons.iter().enumerate() {
                        let (icon, label, primary) = match b {
                            Btn::Play => (Some(glyphs::PLAY), resume_at.map(|s| format!("Resume {}", kit::clock(s))).unwrap_or_else(|| "Play".into()), true),
                            Btn::StartOver => (Some(glyphs::SKIP_BACK), "Start over".into(), false),
                            Btn::Favourite => (Some(if fav { glyphs::FAVOURITE } else { glyphs::FAVOURITE_OFF }), if fav { "Favourite".into() } else { "Add to Favourites".into() }, false),
                            Btn::Watched => (Some(glyphs::CHECK), if watched { "Watched".into() } else { "Mark watched".into() }, false),
                        };
                        let style = ButtonStyle { primary, focused: i == self.focus, selected: matches!(b, Btn::Favourite) && fav || matches!(b, Btn::Watched) && watched, bordered: true, ..Default::default() };
                        if button(ui, ctx.theme, icon, &label, style).clicked() {
                            pressed = Some((i, *b));
                        }
                    }
                });
            });
        });
        if let Some((i, b)) = pressed {
            self.focus = i;
            self.press(ctx, b);
        }
        self.focus = self.focus.min(self.buttons(ctx).len().saturating_sub(1));
    }
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
}

impl DetailHeader<'_> {
    pub(super) fn show(&self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, buttons: impl FnOnce(&mut egui::Ui, &mut UiCtx<'_>)) {
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
                if !self.meta.is_empty() {
                    ui.label(RichText::new(self.meta).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                }
                if self.loading && self.text.is_empty() {
                    for w in [0.9, 0.75, 0.5] {
                        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width() * w, theme.font(TypeToken::Body)), egui::Sense::hover());
                        ui.painter().rect_filled(r, t.rounding, t.fill_field);
                    }
                }
                for (p, secondary) in self.text {
                    let (size, color) = if *secondary { (theme.font(TypeToken::BodySmall), t.text_secondary) } else { (theme.font(TypeToken::Body), t.fg) };
                    ui.add(egui::Label::new(RichText::new(p).size(size).color(color)).wrap());
                }
                ui.add_space(theme.space(SpaceToken::Md));
                buttons(ui, ctx);
            });
        });
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
                let (r, _) = ui.allocate_exact_size(egui::vec2((ui.available_width() * w).max(40.0), theme.font(TypeToken::Title)), egui::Sense::hover());
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
        let video = &mut t.video;
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let r = ui.max_rect();
            video.paint(ui, r, Placement::Fullscreen, &theme, &snap);
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
            crate::store::user::save_progress(c, &WatchProgress { item, position_s: 600.0, duration_s: Some(1200.0), updated_at: 5, finished: false, title: "Movie 0".into(), fingerprint: Default::default() })?;
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
        assert!(h.state_mut().nav().contains(&crate::ui::NavRequest::Back), "returns to the detail page");
        // a vanished movie
        h.state_mut().enter(Route::MovieDetail(MovieId(999)));
        until(&mut h, "the missing state", |h| has_label(h, "This movie is no longer in your playlists"));
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
