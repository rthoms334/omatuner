//! "Delete playlist…" (Settings ▸ Playlists; DESIGN §29.16, P3). Deleting a playlist removes
//! its catalogue and, with it, its favourites, progress and watched marks. When another
//! playlist has the same titles the dialog **offers** — unticked by default — to move them
//! first to those copies (`store::copies::move_plan` / `move_titles`):
//!
//! * only copies matched by TMDB id, by title and the same known year, or joined by the user
//!   ("Same title as…") — never one either side split off; the dialog lists what moves where;
//! * an episode's progress moves to the same episode of the series' other copy, once that
//!   copy's episodes are loaded: ticking the box loads them (`ui::copies::EpisodeFetch`, with
//!   progress) and lists the episodes again; Delete waits until they settled;
//! * what can't move is listed (no copy elsewhere, channels — never matched across playlists,
//!   episodes whose other copy couldn't load), and the toast after the delete says how much
//!   moved and how much couldn't.
//!
//! The overlay writes the move and the delete itself (one store-writer job: move, then
//! delete) and closes with `Confirmed(token)` so Settings tidies its own state. There is no
//! Undo: the playlist and its catalogue are gone (the moved rows are ordinary favourites and
//! progress of the other copies, which the user can change as usual).

use eframe::egui::{self, RichText};

use super::dialogs::{buttons, card_area, scrim, title};
use super::{Overlay, OverlayOutcome, OverlayResult};
use crate::keymap::{Action, KeyContext};
use crate::model::SourceId;
use crate::msg::{DataScope, Toast, ToastKind};
use crate::store::copies::{MoveItem, MovePlan};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::UiCtx;
use crate::ui::screens::home::kit::{self, Loader};
use crate::ui::widgets::button::ButtonStyle;
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::toggle::toggle;

/// Rows of each list shown before "and n more".
const SHOWN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Move,
    Cancel,
    Delete,
}

pub struct DeletePlaylistOverlay {
    source: SourceId,
    token: u64,
    loader: Option<Loader<MovePlan>>,
    plan: Option<MovePlan>,
    /// "Move favourites and progress…" (off until the user ticks it).
    pub(crate) tick: bool,
    fetch: crate::ui::copies::EpisodeFetch,
    /// Playlists whose series copies couldn't load their episodes.
    failed: Vec<String>,
    focus: Focus,
    started: bool,
}

impl DeletePlaylistOverlay {
    pub fn new(source: SourceId, token: u64) -> Self {
        Self {
            source,
            token,
            loader: None,
            plan: None,
            tick: false,
            fetch: Default::default(),
            failed: Vec::new(),
            focus: Focus::Cancel,
            started: false,
        }
    }

    fn name(&self, ctx: &UiCtx<'_>) -> String {
        ctx.lib.source(self.source).map_or_else(|| "this playlist".to_owned(), |s| s.name.clone())
    }

    fn reload(&mut self, ctx: &UiCtx<'_>) {
        let source = self.source;
        self.loader = Some(Loader::start(ctx, move |c| crate::store::copies::move_plan(c, source)));
    }

    /// The move section shows: the playlist has favourites or progress to talk about.
    fn offers(&self) -> bool {
        self.plan.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Delete waits for the plan (when moving) and for episodes being loaded.
    fn can_delete(&self) -> bool {
        !self.tick || (self.plan.is_some() && self.loader.is_none() && !self.fetch.busy())
    }

    fn set_tick(&mut self, ctx: &UiCtx<'_>, on: bool) {
        self.tick = on;
        // the other copies' episodes load once the user asks to move
        if let Some(p) = self.plan.as_ref().filter(|_| on)
            && !p.waiting.is_empty()
        {
            let waiting = p.waiting.clone();
            self.fetch.start(ctx, &waiting);
        }
    }

    /// Move (when ticked), then delete; the toast reports what moved.
    fn delete(&self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if !self.can_delete() {
            return OverlayOutcome::Keep;
        }
        let id = self.source;
        let name = self.name(ctx);
        let (items, left): (Vec<MoveItem>, usize) = match (&self.plan, self.tick) {
            (Some(p), true) => (p.items.clone(), p.cannot.len() + p.pending.len()),
            _ => (Vec::new(), 0),
        };
        let moving = self.tick;
        let bus = ctx.services.bus.clone();
        let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
            let moved = if moving { crate::store::copies::move_titles(c, &items)? } else { Default::default() };
            crate::store::sources::delete_source(c, id)?;
            let mut text = format!("Deleted {name}");
            if moving {
                text.push_str(&format!(" — moved {} to the other playlists", moved_text(moved)));
                if left > 0 {
                    text.push_str(&format!("; {left} couldn't move"));
                }
            }
            bus.toast(Toast { kind: ToastKind::Info, text, action: None, ttl_ms: 6000 });
            Ok(())
        });
        OverlayOutcome::CloseWith(OverlayResult::Confirmed(self.token))
    }

    fn focus_order(&self) -> Vec<Focus> {
        if self.offers() { vec![Focus::Move, Focus::Cancel, Focus::Delete] } else { vec![Focus::Cancel, Focus::Delete] }
    }

    fn step(&mut self, by: isize) {
        let order = self.focus_order();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = order[(at + by).rem_euclid(order.len() as isize) as usize];
    }
}

/// "3 favourites and 2 progress marks".
fn moved_text(m: crate::store::copies::Moved) -> String {
    let n = |k: usize, one: &str, many: &str| if k == 1 { format!("1 {one}") } else { format!("{k} {many}") };
    format!("{} and {}", n(m.favourites, "favourite", "favourites"), n(m.progress, "progress mark", "progress marks"))
}

/// "Dune (2021) → TV 2 · ★ · 41:20".
fn item_line(ctx: &UiCtx<'_>, m: &MoveItem) -> String {
    let to = ctx.lib.source(m.to.source).map_or("another playlist", |s| s.name.as_str());
    let mut parts = vec![format!("{} → {to}", m.title)];
    if !m.lists.is_empty() {
        parts.push("★".to_owned());
    }
    match &m.progress {
        Some(p) if p.finished => parts.push("✓ watched".to_owned()),
        Some(p) if p.position_s > 0.0 => parts.push(kit::clock(p.position_s)),
        _ => {}
    }
    parts.join(" · ")
}

impl Overlay for DeletePlaylistOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        match action {
            Action::Up | Action::Left | Action::FocusPrev => {
                self.step(-1);
                OverlayOutcome::Keep
            }
            Action::Down | Action::Right | Action::FocusNext => {
                self.step(1);
                OverlayOutcome::Keep
            }
            Action::Activate | Action::ToggleMark => match self.focus {
                Focus::Move => {
                    let on = !self.tick;
                    self.set_tick(ctx, on);
                    OverlayOutcome::Keep
                }
                Focus::Cancel => OverlayOutcome::CloseWith(OverlayResult::Cancelled(self.token)),
                Focus::Delete => self.delete(ctx),
            },
            Action::Back => OverlayOutcome::CloseWith(OverlayResult::Cancelled(self.token)),
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if !self.started {
            self.started = true;
            self.reload(ctx);
        }
        match kit::poll(&mut self.loader) {
            Some(Ok(p)) => {
                // ticked while it was being read: load the waiting copies now
                let fetch = self.tick && !p.waiting.is_empty() && !self.fetch.busy() && self.failed.is_empty();
                self.plan = Some(p);
                if fetch {
                    self.set_tick(ctx, true);
                }
            }
            Some(Err(e)) => {
                tracing::warn!("delete playlist: {e}");
                self.plan = Some(MovePlan::default());
            }
            None => {}
        }
        if let Some(failed) = self.fetch.poll(ctx, egui_ctx) {
            // read the plan again: loaded copies' episodes move now, the others are listed
            self.failed = failed
                .iter()
                .filter_map(|s| ctx.lib.vod.find(crate::model::VodKind::Series(*s)).and_then(|v| ctx.lib.source(v.source_id)).map(|m| m.name.clone()))
                .collect();
            self.failed.sort();
            self.failed.dedup();
            self.reload(ctx);
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "delete-playlist") {
            return OverlayOutcome::CloseWith(OverlayResult::Cancelled(self.token));
        }
        let name = self.name(ctx);
        let offers = self.offers();
        let (can_delete, fetching) = (self.can_delete(), self.fetch.busy().then(|| self.fetch.progress()));
        let mut tick = self.tick;
        let mut tick_clicked = false;
        let style = ButtonStyle { danger: true, bordered: true, ..Default::default() };
        let (cancel, ok) = card_area(egui_ctx, theme, "delete-playlist", CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            title(
                ui,
                theme,
                &format!("Delete {name}?"),
                Some("Its channels, movies, series and guide are removed — and with them its favourites, progress and watched marks."),
            );
            if let Some(plan) = self.plan.as_ref().filter(|_| offers) {
                ui.add_space(theme.space(SpaceToken::Lg));
                if !plan.items.is_empty() || !plan.pending.is_empty() {
                    let label = "Move favourites and progress to the same titles in other playlists";
                    tick_clicked = toggle(ui, theme, &mut tick, label, self.focus == Focus::Move).clicked();
                }
                let small = |ui: &mut egui::Ui, text: String, color| {
                    ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(color)).wrap());
                };
                if !plan.items.is_empty() {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    let n = plan.items.len();
                    small(
                        ui,
                        format!(
                            "{} matched by TMDB id, title and year, or by you:",
                            if n == 1 { "1 title".to_owned() } else { format!("{n} titles") }
                        ),
                        t.text_secondary,
                    );
                    for m in plan.items.iter().take(SHOWN) {
                        small(ui, format!("  {}", item_line(ctx, m)), if self.tick { t.fg } else { t.text_secondary });
                    }
                    if n > SHOWN {
                        small(ui, format!("  and {} more", n - SHOWN), t.text_secondary);
                    }
                }
                if let Some((done, n)) = fetching {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(theme.font(TypeToken::Body)));
                        small(ui, format!("Loading the other copies' episodes ({done} of {n} series)…"), t.text_secondary);
                    });
                } else if !plan.pending.is_empty() {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    let n = plan.pending.len();
                    let why = if self.failed.is_empty() {
                        "move once the other copy's episodes are loaded (tick the box to load them)".to_owned()
                    } else {
                        format!("can't move: {} couldn't load the episodes", self.failed.join(", "))
                    };
                    small(ui, format!("{} {why}", if n == 1 { "1 episode".to_owned() } else { format!("{n} episodes") }), t.text_secondary);
                }
                if !plan.cannot.is_empty() {
                    ui.add_space(theme.space(SpaceToken::Sm));
                    small(ui, "Can't move:".to_owned(), t.text_secondary);
                    for (what, why) in plan.cannot.iter().take(SHOWN) {
                        small(ui, format!("  {what} — {why}"), t.text_disabled);
                    }
                    if plan.cannot.len() > SHOWN {
                        small(ui, format!("  and {} more", plan.cannot.len() - SHOWN), t.text_disabled);
                    }
                }
            } else if self.plan.is_none() {
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.label(
                    RichText::new("Looking for the same titles in your other playlists…")
                        .size(theme.font(TypeToken::BodySmall))
                        .color(t.text_secondary),
                );
            }
            let focus = match self.focus {
                Focus::Delete => Some(true),
                Focus::Cancel => Some(false),
                Focus::Move => None,
            };
            buttons(ui, theme, if can_delete { "Delete" } else { "Delete (loading…)" }, style, focus)
        })
        .inner;
        if tick_clicked {
            self.focus = Focus::Move;
            self.set_tick(ctx, tick);
        }
        if ok {
            self.delete(ctx)
        } else if cancel {
            OverlayOutcome::CloseWith(OverlayResult::Cancelled(self.token))
        } else {
            OverlayOutcome::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, WatchProgress};
    use crate::ui::screens::home::testkit::{self, has_label, has_label_containing, overlay_harness, rig, seed, until_ov};

    /// The dialog offers (unticked) to move what has a copy elsewhere and lists what can't;
    /// unticked, Delete moves nothing; ticked, Delete moves TV's favourite and progress to
    /// TV 2's copy and deletes TV.
    #[test]
    fn offers_the_move_unticked_lists_it_and_moves_when_ticked() {
        let (mut t, one) = rig();
        let (a, b, list) = seed(&mut t, |c| {
            let a = testkit::source(c, "TV")?;
            let b = testkit::source(c, "TV 2")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune"), ("EN ★ Gold - 2022", "Gold")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?;
            let list = crate::store::user::default_fav_list(c)?;
            let r = |i: usize| ItemRef { source: a, kind: ItemKind::Movie, key: ItemKey(format!("m3u:named{i:04}")) };
            crate::store::user::set_favourite(c, list, &r(0), true, 1)?;
            crate::store::user::set_favourite(c, list, &r(1), true, 1)?;
            let p = WatchProgress {
                item: r(0),
                position_s: 2480.0,
                duration_s: Some(9000.0),
                updated_at: 5,
                finished: false,
                title: "Dune".into(),
                fingerprint: Default::default(),
            };
            crate::store::user::save_progress(c, &p)?;
            Ok((a, b, list))
        });
        let mut h = overlay_harness(t, one, DeletePlaylistOverlay::new(a, 7), egui::vec2(1200.0, 900.0));
        until_ov(&mut h, "the plan", |h| h.state().o.plan.is_some());
        h.run_steps(1);
        assert!(has_label(&h, "Move favourites and progress to the same titles in other playlists"));
        assert!(!h.state().o.tick, "unticked by default");
        assert!(has_label(&h, "  Dune (2021) → TV 2 · ★ · 41:20"));
        assert!(has_label_containing(&h, "Gold (2022) — no copy in another playlist"));
        // tick it (keyboard: ↑ from Cancel to the box, Enter), then Delete
        assert_eq!(h.state_mut().act(Action::Up), "Keep");
        assert_eq!(h.state_mut().act(Action::Activate), "Keep");
        assert!(h.state().o.tick);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Confirmed(7))");
        let r = h.state().t.services().store.reader().unwrap();
        let _ = h.state().t.services().store.writer().blocking(|_| Ok(()));
        let favs: Vec<(crate::model::SourceId, String)> =
            crate::store::user::fav_items(&r, list).unwrap().into_iter().map(|e| (e.item.source, e.item.key.0)).collect();
        assert_eq!(favs, [(b, "m3u:named0000".to_owned())], "Dune's favourite on TV 2's copy; TV is gone with Gold's");
        let moved = ItemRef { source: b, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) };
        assert_eq!(crate::store::user::progress(&r, &moved).unwrap().map(|p| p.position_s), Some(2480.0));
        let left: i64 = r.query_row("SELECT count(*) FROM source WHERE id = ?1", [a.0], |x| x.get(0)).unwrap();
        assert_eq!(left, 0, "deleted");
    }

    /// An episode's progress waits for the other copy's episodes: ticking the box loads them
    /// (Delete waits meanwhile), then the episode is listed and moves.
    #[test]
    fn ticking_loads_the_other_copys_episodes_before_moving_an_episode() {
        let (mut t, one) = rig();
        let (a, b, bravo) = seed(&mut t, |c| {
            let a = testkit::source(c, "TV")?;
            let b = testkit::source(c, "TV 2")?;
            testkit::named_series(c, a, &[("EN ★ Severance - 2022", "Severance")], 2)?;
            let bravo = testkit::named_series(c, b, &[("EN - Severance (2022)", "Severance")], 0)?[0];
            c.execute("UPDATE source SET kind = 'xtream' WHERE id = ?1", [b])?;
            c.execute("UPDATE series SET details_at = NULL WHERE id = ?1", [bravo])?;
            let p = WatchProgress {
                item: ItemRef { source: a, kind: ItemKind::Episode, key: ItemKey("m3u:named-ep0-2".into()) },
                position_s: 600.0,
                duration_s: Some(2700.0),
                updated_at: 5,
                finished: false,
                title: "S01E02".into(),
                fingerprint: Default::default(),
            };
            crate::store::user::save_progress(c, &p)?;
            Ok((a, b, bravo))
        });
        let mut h = overlay_harness(t, one, DeletePlaylistOverlay::new(a, 9), egui::vec2(1200.0, 900.0));
        until_ov(&mut h, "the plan", |h| h.state().o.plan.is_some());
        h.run_steps(1);
        assert!(has_label_containing(&h, "1 episode move once the other copy's episodes are loaded"));
        h.state_mut().act(Action::Up);
        h.state_mut().act(Action::Activate);
        assert!(h.state().o.fetch.busy() && !h.state().o.can_delete(), "Delete waits for the episodes");
        // the details job stores TV 2's episodes
        seed(&mut h.state_mut().t, move |c| {
            for e in 1..=2 {
                c.execute(
                    "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, 1, ?4, 'E', 2700, '{\"t\":\"url\",\"url\":\"http://127.0.0.1:9/x.mp4\"}')",
                    rusqlite::params![bravo, b, format!("b-ep{e}"), e],
                )?;
            }
            c.execute("UPDATE series SET details_at = 1 WHERE id = ?1", [bravo])?;
            Ok(())
        });
        until_ov(&mut h, "the episode listed", |h| h.state().o.can_delete() && h.state().o.plan.as_ref().is_some_and(|p| p.items.len() == 1));
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::Down);
        assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Confirmed(9))");
        let _ = h.state().t.services().store.writer().blocking(|_| Ok(()));
        let r = h.state().t.services().store.reader().unwrap();
        let moved = ItemRef { source: b, kind: ItemKind::Episode, key: ItemKey("b-ep2".into()) };
        assert_eq!(crate::store::user::progress(&r, &moved).unwrap().map(|p| p.position_s), Some(600.0));
    }

    /// Unticked (the default), Delete only deletes.
    #[test]
    fn unticked_delete_moves_nothing() {
        let (mut t, one) = rig();
        let (a, b) = seed(&mut t, |c| {
            let a = testkit::source(c, "TV")?;
            let b = testkit::source(c, "TV 2")?;
            testkit::named_movies(c, a, &[("EN ★ Dune - 2021", "Dune")])?;
            testkit::named_movies(c, b, &[("EN - Dune (2021)", "Dune")])?;
            let list = crate::store::user::default_fav_list(c)?;
            crate::store::user::set_favourite(c, list, &ItemRef { source: a, kind: ItemKind::Movie, key: ItemKey("m3u:named0000".into()) }, true, 1)?;
            Ok((a, b))
        });
        let mut h = overlay_harness(t, one, DeletePlaylistOverlay::new(a, 3), egui::vec2(1200.0, 900.0));
        until_ov(&mut h, "the plan", |h| h.state().o.plan.is_some());
        h.state_mut().act(Action::Right);
        assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Confirmed(3))");
        let _ = h.state().t.services().store.writer().blocking(|_| Ok(()));
        let r = h.state().t.services().store.reader().unwrap();
        let favs: i64 = r.query_row("SELECT count(*) FROM fav_item WHERE source_id = ?1", [b.0], |x| x.get(0)).unwrap();
        assert_eq!(favs, 0, "nothing moved");
    }
}
