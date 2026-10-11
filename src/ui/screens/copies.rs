//! The Copies section of the movie and series pages (DESIGN §29.14): when the page's title has
//! several copies (a stack, `interface.same_title = combine`), a row of mini-cards
//! (`widgets::copy_card`), one per copy in copy order — its playlist, language and version,
//! progress and status ("plays first", "always plays first", "failed 2 h ago", locked).
//!
//! * The copy Play uses: the one picked here (a click, or Enter on a focused mini-card), else
//!   — on the page of a stack's top card — the copy that plays first (`ui::stacks::play_order`),
//!   else the page's own copy. Picking a locked copy asks for the PIN first.
//! * A mini-card's menu (right-click, the menu key): play from it, "Always play from" it (one
//!   pin per stack), "Not the same title" (with Undo), and for a series its own page.
//! * "Play from…" (Ctrl+O, the page's button): every copy, the one that plays first marked.
//! * Per-copy progress, recency and container are read off the UI thread whenever the
//!   library or progress changes.
//! * A page whose copy left its stack because its details brought another TMDB id says so.

use std::collections::HashMap;

use eframe::egui;

use super::home::kit::{self, Loader};
use crate::keymap::Action;
use crate::model::{ItemKind, ItemRef, StreamLocator, Timestamp, VodKind, VodSummary, WatchProgress};
use crate::store::stacks::StackId;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::stacks as st;
use crate::ui::widgets::copy_card::{self, CopyCard, StatusTone};
use crate::ui::widgets::{glyphs, section};
use crate::ui::{OverlayRequest, UiCtx};

/// "Play from…": copy `k` is `PLAY_FROM + k`.
const PLAY_FROM: u8 = 100;
/// Copies listed in "Play from…".
const MAX_COPIES: usize = 150;
/// A mini-card's menu.
pub(super) const CARD_PLAY: u8 = 90;
pub(super) const CARD_PIN: u8 = 91;
pub(super) const CARD_SPLIT: u8 = 92;
pub(super) const CARD_PAGE: u8 = 93;

/// What the page reads per copy.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Facts {
    /// Watch progress 0..1 while partly watched.
    fraction: Option<f32>,
    /// When it was last played (any progress row; for a series, its latest episode's).
    played_at: Option<Timestamp>,
    /// Where a movie resumes (past the resume threshold, unfinished).
    resume_s: Option<f64>,
    /// mkv / mp4 (`Some(true)`), another container, unknown.
    container_ok: Option<bool>,
}

/// What the page does after a Copies action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyAct {
    /// Play this copy (index into the kind's list).
    Play(u32),
    /// Open this copy's page (a series copy's own episodes).
    Open(VodKind),
}

/// The answer to an overlay result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Not a Copies result: the page handles it.
    NotMine,
    Done,
    Act(CopyAct),
}

/// A click on a mini-card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Click {
    Pick(usize),
    Menu(usize),
}

/// One mini-card to draw: (copy, language and version, status, drawn disabled).
type CardRow = (u32, Option<String>, Option<(String, StatusTone)>, bool);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Pick,
    Play,
}

#[derive(Default)]
pub(super) struct Copies {
    movies: bool,
    /// (library generation, page, combine) the copies were listed for.
    built: Option<(u64, VodKind, bool)>,
    /// The page's copy (index into the kind's list).
    page: Option<u32>,
    stack: Option<StackId>,
    /// The copies in copy order; empty when the title has one.
    members: Vec<u32>,
    /// The copy picked for Play.
    chosen: Option<u32>,
    facts: HashMap<ItemRef, Facts>,
    loader: Option<(Vec<ItemRef>, Loader<Vec<Facts>>)>,
    /// The focused mini-card (keyboard), set by the page.
    pub focus: Option<usize>,
    /// The copy whose menu is open.
    menu_copy: Option<u32>,
    /// The copies "Play from…" listed.
    play_from: Vec<u32>,
    /// A locked copy waiting for the PIN.
    waiting: Option<(u32, Waiting)>,
    pub undo: Option<st::Undo>,
    /// The page's copy left its stack of `n + 1` copies (its details named another title).
    separated: Option<usize>,
    /// The page copy's TMDB id when the copies were listed.
    tmdb: Option<u32>,
}

/// Whether a movie's container is mkv / mp4 (DESIGN §29.12, rule 6).
fn container_ok(stream: &StreamLocator) -> Option<bool> {
    let ext = match stream {
        StreamLocator::Xtream { ext, .. } => ext.clone()?,
        StreamLocator::Url { url } => {
            let path = url.split(['?', '#']).next().unwrap_or("");
            let last = path.rsplit('/').next().unwrap_or("");
            let (_, ext) = last.rsplit_once('.')?;
            ext.to_owned()
        }
    };
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "mkv" | "mp4" => Some(true),
        e if !e.is_empty() && e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()) => Some(false),
        _ => None,
    }
}

fn load_facts(c: &rusqlite::Connection, copies: Vec<(VodKind, ItemRef)>, resume_min: u32) -> crate::error::Result<Vec<Facts>> {
    use crate::store::{user, vod};
    let mut out = Vec::with_capacity(copies.len());
    for (kind, r) in copies {
        let f = match kind {
            VodKind::Movie(id) => {
                let container = match vod::movie(c, id) {
                    Ok(m) => container_ok(&m.stream),
                    Err(crate::error::Error::NotFound(_)) => None,
                    Err(e) => return Err(e),
                };
                let p = user::progress(c, &r)?;
                Facts {
                    fraction: p.as_ref().filter(|p| !p.finished).and_then(super::movie_detail::fraction),
                    played_at: p.as_ref().map(|p| p.updated_at),
                    resume_s: p.as_ref().filter(|p| super::movie_detail::resumable(p, resume_min)).map(|p| p.position_s),
                    container_ok: container,
                }
            }
            VodKind::Series(id) => {
                let mut last: Option<WatchProgress> = None;
                for e in vod::episodes(c, id, None)? {
                    let er = ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key };
                    if let Some(p) = user::progress(c, &er)?
                        && last.as_ref().is_none_or(|l| p.updated_at > l.updated_at)
                    {
                        last = Some(p);
                    }
                }
                Facts {
                    fraction: last.as_ref().filter(|p| !p.finished).and_then(super::movie_detail::fraction),
                    played_at: last.as_ref().map(|p| p.updated_at),
                    resume_s: None,
                    container_ok: None,
                }
            }
        };
        out.push(f);
    }
    Ok(out)
}

impl Copies {
    fn all<'l>(&self, ctx: &'l UiCtx<'_>) -> &'l [VodSummary] {
        st::all(ctx.lib, self.movies)
    }

    /// List the copies of the page `kind` (every frame; work only when the library, the page
    /// or the setting changed).
    pub(super) fn sync(&mut self, ctx: &UiCtx<'_>, kind: VodKind) {
        let key = (ctx.lib.generation, kind, st::combine(ctx.config));
        if self.built == Some(key) {
            return;
        }
        let same_page = self.built.is_some_and(|b| b.1 == kind);
        let before = if same_page { self.members.len() } else { 0 };
        self.built = Some(key);
        self.movies = st::is_movie(kind);
        self.page = st::index_of(ctx.lib, kind);
        let stacks = ctx.lib.vod.stacks(self.movies);
        let members = self.page.map(|i| st::copies_of(ctx.lib, ctx.config, self.movies, i)).unwrap_or_default();
        self.members = if members.len() > 1 { members } else { Vec::new() };
        self.stack = self.page.and_then(|p| stacks.stack_of(p)).filter(|_| !self.members.is_empty());
        let meta = self.page.map(|p| stacks.meta(p));
        let tmdb = meta.and_then(|m| m.tmdb);
        if !same_page {
            self.separated = None;
            self.chosen = None;
            self.focus = None;
        } else if !self.members.is_empty() {
            self.separated = None;
        } else if before > 1 && !meta.is_some_and(|m| m.split) && tmdb.is_some() && tmdb != self.tmdb {
            self.separated = Some(before - 1);
        }
        self.tmdb = tmdb;
        self.chosen = self.chosen.filter(|c| self.members.contains(c));
        self.focus = self.focus.filter(|f| *f < self.members.len());
        self.reload(ctx);
    }

    /// Re-read the copies' progress (the page's `DataScope::Progress`).
    pub(super) fn reload(&mut self, ctx: &UiCtx<'_>) {
        if self.members.is_empty() {
            self.loader = None;
            return;
        }
        let all = self.all(ctx);
        let copies: Vec<(VodKind, ItemRef)> = self.members.iter().filter_map(|c| all.get(*c as usize)).map(|v| (v.kind, st::item_ref(v))).collect();
        let refs = copies.iter().map(|c| c.1.clone()).collect();
        let resume_min = ctx.config.player.resume_min_s;
        self.loader = Some((refs, Loader::start(ctx, move |c| load_facts(c, copies, resume_min))));
    }

    fn poll(&mut self) {
        let Some((_, l)) = &mut self.loader else { return };
        match l.poll() {
            Some(Ok(v)) => {
                if let Some((refs, _)) = self.loader.take() {
                    self.facts = refs.into_iter().zip(v).collect();
                }
            }
            Some(Err(e)) => {
                tracing::warn!("copies: {e}");
                self.loader = None;
            }
            None => {}
        }
    }

    fn facts(&self, ctx: &UiCtx<'_>, c: u32) -> Facts {
        self.all(ctx).get(c as usize).and_then(|v| self.facts.get(&st::item_ref(v))).copied().unwrap_or_default()
    }

    /// The title has several copies (the section shows).
    pub(super) fn is_stacked(&self) -> bool {
        !self.members.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.members.len()
    }

    /// The copies in the order they are tried.
    pub(super) fn order(&self, ctx: &UiCtx<'_>) -> Vec<u32> {
        st::play_order(
            ctx.lib,
            ctx.config,
            self.movies,
            &self.members,
            kit::unlocked(ctx),
            |c| self.facts(ctx, c).played_at,
            |c| self.facts(ctx, c).container_ok,
        )
    }

    /// The copy Play uses (`None`: the title has one copy — the page's).
    pub(super) fn plays(&self, ctx: &UiCtx<'_>) -> Option<u32> {
        if self.members.is_empty() {
            return None;
        }
        if self.chosen.is_some() {
            return self.chosen;
        }
        let top = self.stack.map(|s| ctx.lib.vod.stacks(self.movies).top(s));
        if top.is_some() && top == self.page { self.order(ctx).first().copied().or(self.page) } else { self.page }
    }

    /// Where a copy resumes (movies).
    pub(super) fn resume_at(&self, ctx: &UiCtx<'_>, c: u32) -> Option<f64> {
        self.facts(ctx, c).resume_s
    }

    /// The page's copy.
    pub(super) fn page(&self) -> Option<u32> {
        self.page
    }

    /// The playlist of a copy ("TV 2").
    pub(super) fn playlist<'l>(&self, ctx: &'l UiCtx<'_>, c: u32) -> Option<&'l str> {
        self.all(ctx).get(c as usize).and_then(|v| ctx.lib.source(v.source_id)).map(|s| s.name.as_str())
    }

    fn locked(&self, ctx: &UiCtx<'_>, c: u32) -> bool {
        self.all(ctx).get(c as usize).is_some_and(|v| v.locked) && !kit::unlocked(ctx)
    }

    /// The section: "Copies · N", the mini-cards, how they were matched. Returns a click.
    pub(super) fn show(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<Click> {
        self.poll();
        let theme = ctx.theme;
        let t = &theme.tokens;
        if let Some(n) = self.separated.filter(|_| self.members.is_empty()) {
            ui.add_space(theme.space(SpaceToken::Lg));
            let others = if n == 1 { "its other copy".to_owned() } else { format!("its {n} other copies") };
            let text = format!("Shown on its own now: its details name another title than {others}");
            ui.label(egui::RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            return None;
        }
        if self.members.is_empty() {
            return None;
        }
        let stacks = ctx.lib.vod.stacks(self.movies);
        let all = self.all(ctx);
        let plays = self.plays(ctx);
        let rows: Vec<CardRow> = self
            .members
            .iter()
            .map(|&c| {
                let v = all.get(c as usize);
                let dimmed = self.locked(ctx, c);
                let pinned = stacks.meta(c).pinned;
                let status = if dimmed {
                    Some(("locked".to_owned(), StatusTone::Muted))
                } else if Some(c) == plays {
                    Some((if pinned { "always plays first" } else { "plays first" }.to_owned(), StatusTone::Accent))
                } else if let Some(tr) = v.and_then(|v| st::trouble(ctx.lib, v)) {
                    let what = match tr.kind {
                        st::TroubleKind::Failed => format!("failed {}", kit::ago(ctx.now, tr.at)),
                        st::TroubleKind::AtLimit => "account at its limit".to_owned(),
                        st::TroubleKind::Expired => "account expired".to_owned(),
                    };
                    Some((what, StatusTone::Warning))
                } else {
                    None
                };
                (c, st::copy_facts_label(stacks, c), status, dimmed)
            })
            .collect();
        ui.add_space(theme.space(SpaceToken::Xxl));
        section::section_header(ui, theme, &format!("Copies · {}", self.members.len()));
        ui.add_space(theme.space(SpaceToken::Md));
        let width = (copy_card::CARD_W * theme.rem()).round().min(ui.available_width());
        let mut click = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Lg), theme.space(SpaceToken::Lg));
            for (k, (c, facts, status, dimmed)) in rows.iter().enumerate() {
                let card = CopyCard {
                    source: all.get(*c as usize).and_then(|v| ctx.lib.source(v.source_id)),
                    facts: facts.as_deref(),
                    status: status.as_ref().map(|(s, tone)| (s.as_str(), *tone)),
                    progress: self.facts(ctx, *c).fraction,
                    dimmed: *dimmed,
                    chosen: Some(*c) == plays,
                    focused: self.focus == Some(k),
                };
                let r = copy_card::copy_card(ui, theme, width, card);
                if r.secondary_clicked() {
                    click = Some(Click::Menu(k));
                } else if r.clicked() {
                    click = Some(Click::Pick(k));
                }
            }
        });
        // how the copies were matched; how to take one apart
        let how = match self.page.and_then(|p| stacks.meta(p).tmdb) {
            Some(_) => "Matched by TMDB id".to_owned(),
            None => match self.page.and_then(|p| all.get(p as usize)).and_then(|v| v.year) {
                Some(y) => format!("Matched by title and year ({y})"),
                None => "Matched by title".to_owned(),
            },
        };
        let menu_key = super::live::shortcut(crate::keymap::KeyContext::Detail, Action::ContextMenu).unwrap_or_default();
        ui.add_space(theme.space(SpaceToken::Sm));
        ui.label(
            egui::RichText::new(format!("{how} · Not the same title? {menu_key} on a copy"))
                .size(theme.font(TypeToken::BodySmall))
                .color(t.text_secondary),
        );
        click
    }

    /// A mini-card was clicked (or Enter / the menu key on the focused one).
    pub(super) fn click(&mut self, ctx: &mut UiCtx<'_>, click: Click) {
        let (Click::Pick(k) | Click::Menu(k)) = click;
        let Some(&c) = self.members.get(k) else { return };
        self.focus = Some(k);
        match click {
            Click::Pick(_) if self.locked(ctx, c) => {
                self.waiting = Some((c, Waiting::Pick));
                kit::ask_pin(ctx, Action::PlayFrom, "This copy is locked");
            }
            Click::Pick(_) => self.chosen = Some(c),
            Click::Menu(_) => self.card_menu(ctx, c),
        }
    }

    /// A copy's menu (`c` may be the page's own copy: the page menu's "Not the same title").
    pub(super) fn card_menu(&mut self, ctx: &mut UiCtx<'_>, c: u32) {
        let stacks = ctx.lib.vod.stacks(self.movies);
        let mi = |icon: &'static str, label: String, code: u8| MenuItem { icon, label, action: Action::Digit(code), enabled: true, shortcut: None };
        let playlist = self.playlist(ctx, c).unwrap_or("this playlist").to_owned();
        let mut items = vec![mi(glyphs::PLAY, format!("Play from {playlist}"), CARD_PLAY)];
        items.push(if stacks.meta(c).pinned {
            mi(glyphs::CLOSE, "Don't always play from it".to_owned(), CARD_PIN)
        } else {
            mi(glyphs::FAVOURITE, format!("Always play from {playlist}"), CARD_PIN)
        });
        items.push(mi(glyphs::CLOSE, "Not the same title".to_owned(), CARD_SPLIT));
        if !self.movies && Some(c) != self.page {
            items.push(mi(glyphs::SERIES, "Open its page".to_owned(), CARD_PAGE));
        }
        self.menu_copy = Some(c);
        let title = st::copy_label(ctx.lib, self.movies, c);
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    /// The page's menu offers "Not the same title" for the page's own copy.
    pub(super) fn aim_at_page(&mut self) {
        self.menu_copy = self.page;
    }

    /// Ctrl+O / the "Play from" button.
    pub(super) fn play_from_menu(&mut self, ctx: &mut UiCtx<'_>) {
        if self.members.is_empty() {
            ctx.toast(crate::msg::Toast::info("This title has one copy"));
            return;
        }
        let plays = self.plays(ctx);
        let items = self
            .members
            .iter()
            .take(MAX_COPIES)
            .enumerate()
            .map(|(k, &c)| MenuItem {
                icon: if self.locked(ctx, c) { glyphs::LOCK } else { glyphs::PLAY },
                label: st::copy_label(ctx.lib, self.movies, c),
                action: Action::Digit(PLAY_FROM + k as u8),
                enabled: true,
                shortcut: (Some(c) == plays).then(|| "plays first".to_owned()),
            })
            .collect();
        self.play_from = self.members.clone();
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Play from".into(), items }));
    }

    /// Play `c` (the PIN first when it is locked).
    pub(super) fn request_play(&mut self, ctx: &mut UiCtx<'_>, c: u32) -> Outcome {
        if self.locked(ctx, c) {
            self.waiting = Some((c, Waiting::Play));
            kit::ask_pin(ctx, Action::PlayFrom, "This copy is locked");
            return Outcome::Done;
        }
        Outcome::Act(CopyAct::Play(c))
    }

    /// A toast's Undo.
    pub(super) fn undo(&mut self, ctx: &mut UiCtx<'_>) {
        if let Some(u) = self.undo.take() {
            st::undo(ctx, u);
        }
    }

    /// Menu choices and PIN answers of the section.
    pub(super) fn on_result(&mut self, ctx: &mut UiCtx<'_>, r: &OverlayResult) -> Outcome {
        let all = self.all(ctx);
        match *r {
            OverlayResult::Chosen(Action::Digit(d)) if d >= PLAY_FROM => {
                match self.play_from.get(usize::from(d - PLAY_FROM)).copied().filter(|c| self.members.contains(c)) {
                    Some(c) => self.request_play(ctx, c),
                    None => Outcome::Done,
                }
            }
            OverlayResult::Chosen(Action::Digit(code @ (CARD_PLAY | CARD_PIN | CARD_SPLIT | CARD_PAGE))) => {
                let Some(c) = self.menu_copy.filter(|c| self.members.contains(c)) else { return Outcome::Done };
                let Some(v) = all.get(c as usize).cloned() else { return Outcome::Done };
                match code {
                    CARD_PLAY => self.request_play(ctx, c),
                    CARD_PIN => {
                        let on = !ctx.lib.vod.stacks(self.movies).meta(c).pinned;
                        let others = self.members.iter().filter(|m| **m != c).filter_map(|m| all.get(*m as usize)).map(st::item_ref).collect();
                        let playlist = self.playlist(ctx, c).unwrap_or_default().to_owned();
                        st::set_pin(ctx, &st::item_ref(&v), others, on, &playlist);
                        Outcome::Done
                    }
                    CARD_SPLIT => {
                        self.undo = Some(st::split(ctx, &st::item_ref(&v), &v.name));
                        if self.chosen == Some(c) {
                            self.chosen = None;
                        }
                        Outcome::Done
                    }
                    _ => Outcome::Act(CopyAct::Open(v.kind)),
                }
            }
            OverlayResult::Unlocked(Action::PlayFrom) => match self.waiting.take() {
                Some((c, Waiting::Pick)) => {
                    self.chosen = Some(c);
                    Outcome::Done
                }
                Some((c, Waiting::Play)) => Outcome::Act(CopyAct::Play(c)),
                None => Outcome::Done,
            },
            _ => Outcome::NotMine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 6 of the copy order needs the container: an Xtream extension or the URL's.
    #[test]
    fn containers() {
        let x = |e: Option<&str>| StreamLocator::Xtream { kind: crate::model::XtreamStreamKind::Movie, stream_id: 1, ext: e.map(str::to_owned) };
        let u = |s: &str| StreamLocator::Url { url: s.to_owned() };
        assert_eq!(container_ok(&x(Some("MKV"))), Some(true));
        assert_eq!(container_ok(&x(Some("avi"))), Some(false));
        assert_eq!(container_ok(&x(None)), None);
        assert_eq!(container_ok(&u("http://h/movie/u/p/12.mp4?token=1")), Some(true));
        assert_eq!(container_ok(&u("http://h/movie/12.ts")), Some(false));
        assert_eq!(container_ok(&u("http://h/play/12")), None);
    }
}
