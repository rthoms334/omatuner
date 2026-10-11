//! "Same title as…" (DESIGN §29.16, P3): the user combines a movie or series with another
//! title the matcher kept apart (another name, a missing year, different TMDB ids). Opened
//! from a card's menu or a detail page's menu; type to find the other title (one row per
//! title: a stack's top copy, or the title's only copy — never the title's own copies), ↑/↓
//! and Enter or a click pick it. The overlay writes the join itself (`ui::copies::join`,
//! with an Undo toast) and closes; the library rebuilds on `DataScope::Overrides` and the two
//! titles show as one stack in "All playlists" from then on. Settings ▸ Hidden ▸ "Titles you
//! separated, combined or pinned" lists the join.

use eframe::egui::{self, Align2, RichText};

use super::dialogs::{card_area, scrim, with_close};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::ItemRef;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::UiCtx;
use crate::ui::stacks as st;
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state, virtual_list};
use crate::ui::widgets::text_field::search_header;

/// Matches listed at most.
const MAX_ROWS: usize = 200;

/// What the picker combines: a copy of a movie or series and its display name.
#[derive(Debug, Clone, PartialEq)]
pub struct SameTitleSpec {
    pub item: ItemRef,
    pub name: String,
}

pub struct SameTitleOverlay {
    spec: SameTitleSpec,
    list: ListState,
    /// The titles to choose from (indices into the kind's vector), for library `built`.
    cards: Vec<u32>,
    built: Option<u64>,
    /// Shown rows: indices into `cards`, best match first.
    rows: Vec<u32>,
}

impl SameTitleOverlay {
    pub fn new(spec: SameTitleSpec) -> Self {
        Self { spec, list: ListState::default(), cards: Vec::new(), built: None, rows: Vec::new() }
    }

    fn movies(&self) -> bool {
        self.spec.item.kind == crate::model::ItemKind::Movie
    }

    /// The choices of this frame's library: one card per title, the title's own copies left
    /// out (again after a library swap; indices belong to a snapshot).
    fn start(&mut self, ctx: &UiCtx<'_>) {
        if self.built == Some(ctx.lib.generation) {
            return;
        }
        self.built = Some(ctx.lib.generation);
        let movies = self.movies();
        let all = st::all(ctx.lib, movies);
        let stacks = ctx.lib.vod.stacks(movies);
        let combine = st::combine(ctx.config);
        let own_idx = all.iter().position(|v| v.source_id == self.spec.item.source && v.key == self.spec.item.key).map(|i| i as u32);
        let own = own_idx.map(|i| st::copies_of(ctx.lib, ctx.config, movies, i)).unwrap_or_default();
        self.cards = (0..all.len() as u32)
            .filter(|i| !own.contains(i))
            .filter(|i| !combine || stacks.stack_of(*i).is_none_or(|s| stacks.top(s) == *i))
            .collect();
        self.refilter(ctx);
    }

    fn refilter(&mut self, ctx: &UiCtx<'_>) {
        if !self.list.filter.is_active() {
            self.rows.clear();
        } else {
            let all = st::all(ctx.lib, self.movies());
            let names = self.cards.iter().map(|&i| all.get(i as usize).map_or("", |v| &*v.name));
            self.rows = self.list.filter.apply(names).into_iter().take(MAX_ROWS).collect();
        }
        self.list.set_len(self.rows.len());
    }

    /// "Dune · 2021 · TV + TV 2": a row's detail.
    fn detail(&self, ctx: &UiCtx<'_>, i: u32) -> String {
        let movies = self.movies();
        let all = st::all(ctx.lib, movies);
        let Some(v) = all.get(i as usize) else { return String::new() };
        let copies = st::copies_of(ctx.lib, ctx.config, movies, i);
        let names: Vec<String> = st::playlists_of(ctx.lib.vod.stacks(movies), &copies, all)
            .into_iter()
            .filter_map(|s| ctx.lib.source(s).map(|m| m.name.clone()))
            .collect();
        let mut parts = Vec::new();
        if let Some(y) = v.year {
            parts.push(y.to_string());
        }
        if !names.is_empty() {
            parts.push(names.join(" + "));
        }
        if copies.len() > 1 {
            parts.push(format!("{} copies", copies.len()));
        }
        parts.join(" · ")
    }

    /// Row `r` picked: write the join (Undo toast) and close.
    fn choose(&self, ctx: &mut UiCtx<'_>, r: usize) -> OverlayOutcome {
        let movies = self.movies();
        let Some(&card) = self.rows.get(r).and_then(|k| self.cards.get(*k as usize)) else { return OverlayOutcome::Keep };
        let Some(other) = st::all(ctx.lib, movies).get(card as usize).cloned() else { return OverlayOutcome::Keep };
        let text = format!("{} and {} are one title now", self.spec.name, other.name);
        crate::ui::copies::join(ctx, &self.spec.item, &st::item_ref(&other), text);
        OverlayOutcome::Close
    }
}

impl Overlay for SameTitleOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![self.list.key_context(), KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.start(ctx);
        match self.list.handle(action) {
            ListOutcome::Moved | ListOutcome::ContextMenu(_) => OverlayOutcome::Keep,
            ListOutcome::FilterChanged => {
                self.refilter(ctx);
                OverlayOutcome::Keep
            }
            ListOutcome::Activated(r) | ListOutcome::ActivatedAlt(r) => self.choose(ctx, r),
            ListOutcome::Parent => OverlayOutcome::Close,
            ListOutcome::None => match action {
                Action::Back => OverlayOutcome::Close,
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) => {
                    OverlayOutcome::Keep
                }
                Action::Activate | Action::ActivateAlt | Action::Left | Action::Right | Action::ToggleMark => OverlayOutcome::Keep,
                _ => OverlayOutcome::Ignored,
            },
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.start(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "same-title") {
            return OverlayOutcome::Close;
        }
        let rem = theme.rem();
        let row_h = (theme.font(TypeToken::Title) * 2.6).round();
        let movies = self.movies();
        let mut chosen = None;
        let mut close = false;
        let what = if movies { "movie" } else { "series" };
        card_area(egui_ctx, theme, "same-title", CardKind::Menu, 560.0 * rem, None, |ui| {
            close = with_close(ui, theme, |ui| {
                search_header(
                    ui,
                    theme,
                    self.list.filter.query(),
                    &format!("{} is the same title as… — type the other {what}", self.spec.name),
                    None,
                );
            });
            ui.add_space(theme.space(SpaceToken::Md));
            if self.rows.is_empty() {
                let text = if self.list.filter.is_active() {
                    format!("No {what} matches “{}”", self.list.filter.query())
                } else {
                    format!("Combine {} with another {what} that is the same title: both show as one card in All playlists.", self.spec.name)
                };
                ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
                return;
            }
            let height = (row_h * self.rows.len() as f32).min(420.0 * rem);
            let rows: Vec<(String, String)> = self
                .rows
                .iter()
                .map(|&k| {
                    let i = self.cards.get(k as usize).copied().unwrap_or(u32::MAX);
                    let name = st::all(ctx.lib, movies).get(i as usize).map(|v| v.name.to_string()).unwrap_or_default();
                    (name, self.detail(ctx, i))
                })
                .collect();
            let out = ui
                .allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
                    virtual_list(ui, theme, "same-title-rows", &mut self.list, row_h, |ui, rc| {
                        let rect = ui.max_rect();
                        let resp = ui.allocate_rect(rect, egui::Sense::click());
                        let (name, detail) = rows.get(rc.index).cloned().unwrap_or_default();
                        let label = if detail.is_empty() { name.clone() } else { format!("{name} · {detail}") };
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                        let p = ui.painter();
                        paint_row_state(p, rect, theme, rc.state);
                        let inset = theme.space(SpaceToken::Lg);
                        let text = if rc.state == crate::ui::widgets::RowState::Cursor { t.list_cursor_text } else { t.fg };
                        let right = p
                            .text(
                                egui::pos2(rect.right() - inset, rect.center().y),
                                Align2::RIGHT_CENTER,
                                detail,
                                egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                                t.text_secondary,
                            )
                            .left()
                            - theme.space(SpaceToken::Lg);
                        let left = rect.left() + inset;
                        let mut job = egui::text::LayoutJob::simple_singleline(name, egui::FontId::proportional(theme.font(TypeToken::Title)), text);
                        job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(0.0));
                        let g = p.layout_job(job);
                        p.galley(egui::pos2(left, rect.center().y - g.size().y / 2.0), g, text);
                        resp
                    })
                })
                .inner;
            if let ListOutcome::Activated(r) | ListOutcome::ActivatedAlt(r) = out {
                chosen = Some(r);
            }
        });
        if close {
            return OverlayOutcome::Close;
        }
        chosen.map_or(OverlayOutcome::Keep, |r| self.choose(ctx, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, VodKind};
    use crate::ui::screens::home::testkit::{self, has_label, overlay_harness, rig, seed, until_ov};

    /// Typing finds the other title — one row per title, never the title's own copies — and
    /// Enter combines them with an Undo; the library then stacks them (store side:
    /// `store_catalogue`).
    #[test]
    fn finds_the_other_title_and_combines_with_undo() {
        let (mut t, one) = rig();
        let (a, ids) = seed(&mut t, |c| {
            let a = testkit::source(c, "Alpha")?;
            let b = testkit::source(c, "Bravo")?;
            let ids = testkit::named_movies(c, a, &[("EN ★ Incendies - 2010", "Incendies"), ("EN ★ Heat - 1995", "Heat")])?;
            let other =
                testkit::named_movies(c, b, &[("DE - Die Frau die singt (2010)", "Die Frau die singt"), ("EN - Incendies (2010)", "Incendies")])?;
            Ok((a, [ids[0], other[0]]))
        });
        let item = ItemRef { source: a, kind: ItemKind::Movie, key: crate::model::ItemKey("m3u:named0000".into()) };
        let spec = SameTitleSpec { item: item.clone(), name: "Incendies".into() };
        let mut h = overlay_harness(t, one, SameTitleOverlay::new(spec), egui::vec2(1200.0, 800.0));
        for c in "incendies".chars() {
            h.state_mut().act(Action::FilterInput(c));
        }
        h.run_steps(2);
        assert!(h.state().o.rows.is_empty(), "the title's own copies are never offered");
        for _ in 0..9 {
            h.state_mut().act(Action::FilterBackspace);
        }
        for c in "frau".chars() {
            h.state_mut().act(Action::FilterInput(c));
        }
        h.run_steps(2);
        assert!(has_label(&h, "Die Frau die singt · 2010 · Bravo"), "one row with its year and playlist");
        assert_eq!(h.state_mut().act(Action::Activate), "Close");
        let toast = h.state_mut().t.services().store.writer().blocking(|_| Ok(())).map(|_| ());
        assert!(toast.is_ok());
        // the join is stored for both copies, in one group
        let groups = |h: &egui_kittest::Harness<'static, testkit::OvApp<SameTitleOverlay>>| {
            let r = h.state().t.services().store.reader().unwrap();
            crate::store::titles::links(&r).unwrap().into_iter().filter_map(|l| l.group).collect::<Vec<_>>()
        };
        until_ov(&mut h, "the join", |h| groups(h).len() == 2);
        let g = groups(&h);
        assert_eq!(g[0], g[1]);
        // the library stacks them now
        testkit::reload_library(&mut h.state_mut().t);
        let lib = h.state().t.lib.clone();
        let i = lib.vod.index_of(VodKind::Movie(ids[0])).unwrap();
        let j = lib.vod.index_of(VodKind::Movie(ids[1])).unwrap();
        assert_eq!(lib.vod.stacks(true).stack_of(i), lib.vod.stacks(true).stack_of(j));
        assert!(lib.vod.stacks(true).stack_of(i).is_some());
        // the Undo toast puts it back
        let mut undo = None;
        while let Ok(ev) = h.state().t.events.try_recv() {
            if let crate::msg::AppEvent::Toast(t) = ev
                && let Some((_, Action::Undo(n))) = t.action
            {
                assert_eq!(t.text, "Incendies and Die Frau die singt are one title now");
                undo = Some(n);
            }
        }
        assert!(crate::ui::copies::undo(h.state().t.services(), undo.expect("an Undo toast")));
        until_ov(&mut h, "undone", |h| groups(h).is_empty());
    }
}
