//! The Copies section of the movie and series pages (DESIGN §29.14): when the page's title has
//! several copies (a stack, `interface.same_title = combine`), a row of mini-cards
//! (`widgets::copy_card`), one per copy in copy order — its playlist, language and version,
//! progress and status ("plays first", "always plays first", "failed 2 h ago", locked).
//!
//! * The copies and their state come from `store::copies::plan` (`ui::copies::plan`; pin,
//!   progress, trouble, language, playlist order…), read again when the library, progress,
//!   the session's lock or the copy-order setting change — a few indexed rows per copy, in the
//!   frame budget.
//! * The copy Play uses: the one picked here (a click, Enter on a focused mini-card, "Play
//!   from…"), or the copy opened on purpose (a fanned-out copy, one playlist's list:
//!   `ui::stacks::open_copy`), else the title's first in play order — whichever card led to
//!   the page, so Play and Shift+Enter on the card agree. Picking a locked copy asks for the
//!   PIN first.
//! * A mini-card's menu (right-click, the menu key): play from it, "Always play from" it (one
//!   pin per stack), "Not the same title" (Undo through the app's toast), and for a series its
//!   own page.
//! * "Play from…" (Ctrl+O, the page's button): every copy, the one that plays first marked.
//! * A page whose copy left its stack because its details brought another TMDB id says so.

use eframe::egui;

use super::home::kit;
use crate::keymap::Action;
use crate::model::{VodKind, VodSummary};
use crate::store::copies::{CopyPlan, CopyStatus};
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

/// What the copies were listed for: (library generation, page, combine, session unlocked,
/// copy order).
type Built = (u64, VodKind, bool, bool, crate::config::CopyOrder);

#[derive(Default)]
pub(super) struct Copies {
    movies: bool,
    built: Option<Built>,
    /// The plan must be read again (progress or overrides changed).
    stale: bool,
    /// The page's copy (index into the kind's list).
    page: Option<u32>,
    stack: Option<StackId>,
    /// The copies in copy order; empty when the title has one.
    members: Vec<u32>,
    /// The copies' state and play order (`store::copies::plan`).
    plan: CopyPlan,
    /// The copy picked for Play.
    chosen: Option<u32>,
    /// The focused mini-card (keyboard), set by the page.
    pub focus: Option<usize>,
    /// The copy whose menu is open.
    menu_copy: Option<u32>,
    /// The copies "Play from…" listed.
    play_from: Vec<u32>,
    /// A locked copy waiting for the PIN.
    waiting: Option<(u32, Waiting)>,
    /// The page's copy left its stack of `n + 1` copies (its details named another title).
    separated: Option<usize>,
    /// The page copy's TMDB id when the copies were listed.
    tmdb: Option<u32>,
}

impl Copies {
    fn all<'l>(&self, ctx: &'l UiCtx<'_>) -> &'l [VodSummary] {
        st::all(ctx.lib, self.movies)
    }

    /// List the copies of the page `kind` (every frame; work only when the library, the page,
    /// a setting, the session's lock or the copies' data changed).
    pub(super) fn sync(&mut self, ctx: &mut UiCtx<'_>, kind: VodKind) {
        let key = (ctx.lib.generation, kind, st::combine(ctx.config), kit::unlocked(ctx), ctx.config.interface.copy_order);
        if self.built == Some(key) && !self.stale {
            return;
        }
        let same_page = self.built.is_some_and(|b| b.1 == kind);
        let before = if same_page { self.members.len() } else { 0 };
        self.movies = st::is_movie(kind);
        let page = st::index_of(ctx.lib, kind);
        let members = page.map(|i| st::copies_of(ctx.lib, ctx.config, self.movies, i)).unwrap_or_default();
        let plan = match page.filter(|_| members.len() > 1) {
            Some(p) => {
                let movies = self.movies;
                // a few indexed rows per copy, in the frame budget (else next frame)
                let Some(r) = ({
                    let c: &UiCtx<'_> = ctx;
                    let (db, lib, unlocked, order, now) = (c.db, c.lib.clone(), kit::unlocked(c), c.config.interface.copy_order, c.now);
                    ctx.budget.run(move || crate::store::copies::plan(db, &lib.vod, movies, p, unlocked, order, now))
                }) else {
                    return;
                };
                r.unwrap_or_else(|e| {
                    tracing::warn!("copies: {e}");
                    CopyPlan::default()
                })
            }
            None => CopyPlan::default(),
        };
        self.built = Some(key);
        self.stale = false;
        self.page = page;
        self.plan = plan;
        self.members = if members.len() > 1 { members } else { Vec::new() };
        let stacks = ctx.lib.vod.stacks(self.movies);
        self.stack = self.page.and_then(|p| stacks.stack_of(p)).filter(|_| !self.members.is_empty());
        let meta = self.page.map(|p| stacks.meta(p));
        let tmdb = meta.and_then(|m| m.tmdb);
        if !same_page {
            self.separated = None;
            self.focus = None;
            // the copy opened on purpose plays (a fanned-out copy, one playlist's list)
            self.chosen = (st::take_pick(kind) && !self.members.is_empty()).then_some(page).flatten();
        } else if !self.members.is_empty() {
            self.separated = None;
        } else if before > 1 && !meta.is_some_and(|m| m.split) && tmdb.is_some() && tmdb != self.tmdb {
            self.separated = Some(before - 1);
        }
        self.tmdb = tmdb;
        self.chosen = self.chosen.filter(|c| self.members.contains(c));
        self.focus = self.focus.filter(|f| *f < self.members.len());
    }

    /// Read the copies' state again (the page's `DataScope::Progress` / `Overrides`).
    pub(super) fn reload(&mut self, _ctx: &UiCtx<'_>) {
        self.stale = true;
    }

    /// The copies' state and play order.
    pub(super) fn plan(&self) -> &CopyPlan {
        &self.plan
    }

    /// The copy picked on the page (or opened on purpose), if any.
    pub(super) fn chosen(&self) -> Option<u32> {
        self.chosen
    }

    /// Watch progress 0..1 of a copy while partly watched.
    fn fraction(&self, c: u32) -> Option<f32> {
        let p = self.plan.get(c)?.progress.as_ref().filter(|p| !p.finished)?;
        p.duration_s.filter(|d| *d > 0.0).map(|d| (p.position_s / d).clamp(0.0, 1.0) as f32)
    }

    /// The title has several copies (the section shows).
    pub(super) fn is_stacked(&self) -> bool {
        !self.members.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.members.len()
    }

    /// The copy Play uses (`None`: the title has one copy — the page's —, or every copy is
    /// locked while the session is).
    pub(super) fn plays(&self) -> Option<u32> {
        if self.members.is_empty() {
            return None;
        }
        self.chosen.or(self.plan.first())
    }

    /// Where Play resumes a combined movie: the latest progress of any copy (whichever copy
    /// plays, `ui::copies::movie_command` starts it there).
    pub(super) fn resume_at(&self, ctx: &UiCtx<'_>) -> Option<f64> {
        let p = self.plan.resume()?.progress.as_ref()?;
        (!p.finished && p.position_s > f64::from(ctx.config.player.resume_min_s)).then_some(p.position_s)
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
        let plays = self.plays();
        let rows: Vec<CardRow> = self
            .members
            .iter()
            .map(|&c| {
                let dimmed = self.locked(ctx, c);
                let pinned = stacks.meta(c).pinned;
                let status = if dimmed {
                    Some(("locked".to_owned(), StatusTone::Muted))
                } else if Some(c) == plays {
                    Some((if pinned { "always plays first" } else { "plays first" }.to_owned(), StatusTone::Accent))
                } else {
                    match self.plan.status(c) {
                        CopyStatus::Failed { at } => Some((format!("failed {}", kit::ago(ctx.now, at)), StatusTone::Warning)),
                        CopyStatus::AtLimit => Some(("playlist at its connection limit".to_owned(), StatusTone::Warning)),
                        CopyStatus::Expired => Some(("account expired".to_owned(), StatusTone::Warning)),
                        CopyStatus::Locked => Some(("locked".to_owned(), StatusTone::Muted)),
                        CopyStatus::PlaysFirst | CopyStatus::Plain => None,
                    }
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
                    progress: self.fraction(*c),
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
            mi(glyphs::PIN, format!("Always play from {playlist}"), CARD_PIN)
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
        let plays = self.plays();
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
                        let plan = self.plan.clone();
                        crate::ui::copies::pin(ctx, &plan, &st::item_ref(&v), on);
                        Outcome::Done
                    }
                    CARD_SPLIT => {
                        crate::ui::copies::split(ctx, &st::item_ref(&v), &v.name);
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
