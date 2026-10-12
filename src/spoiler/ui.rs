//! The spoiler guard's UI pieces (DESIGN §34 §2.5). OWNER: S-ui.
//!
//! * **Ctrl+B** (`RevealSpoilers`) on the focused item: its hidden text shows (the [`RevealSet`]
//!   of this UI thread, memory only) until focus leaves it — any movement key, or a key on
//!   another item — and hides again. On an item with nothing hidden: "Nothing hidden here".
//!   A click on a redaction bar or a "Description hidden" block reveals that item the same way
//!   (`ui::widgets::spoiler`).
//! * **Ctrl+W** (`MarkWatched`) on a guide programme the guard tracks: "Mark seen" (lifts its
//!   event or episode); on films and episodes Ctrl+W stays "Mark watched" (the screens').
//! * **Ctrl+Shift+B** (`SpoilerMenu`) on an item: the guard's menu about it (I'll watch it
//!   later, guard a topic from its title, stop guarding a series…); anywhere else the app opens
//!   the plain menu (pause for 15 min / 1 h / the session, guard a topic, settings).
//! * The status bar's chip while paused ("󰈈 Spoilers shown · 14 min ✕").
//!
//! Every hook is a no-op while `spoiler::ENABLED` is off. Explicit content never reaches any of
//! this: the guard runs after `Visibility` and its snapshot holds none; revealing never
//! bypasses a PIN (it only changes what text shows of items already on screen).

use std::cell::RefCell;

use crate::guard::{Guard, Reason, Subject};
use crate::keymap::{Action, KeyContext};
use crate::model::{Programme, SeenHow, SeenRow, Timestamp};
use crate::msg::{DataScope, Toast};
use crate::ui::UiCtx;
use crate::ui::item_menu::{Target, entry};
use crate::ui::overlays::MenuItem;
use crate::ui::widgets::glyphs;

/// "I'll watch it later" / an Undo of a seen event is offered in the menu this long.
const RECENT_SEEN_S: i64 = 30 * 60;

/// What a reveal covers: the subjects painters ask about (`Guard::text`), matched loosely
/// enough that the same item painted by another screen (the popover over the guide, the
/// details pane) is covered too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealKey {
    /// An airing (title, EPG start): airings of the same title within 6 h are covered too.
    Programme {
        title: String,
        start: Timestamp,
    },
    Episode {
        series_norm: String,
        season: i32,
        episode: i32,
    },
    /// Every episode of a series and its own page.
    Series {
        series_norm: String,
    },
    /// A film (or a series card) by its name.
    Vod {
        title: String,
    },
}

impl RevealKey {
    /// The key a click on one of `subject`'s fields reveals.
    pub fn of(subject: &Subject<'_>) -> Option<Self> {
        Some(match *subject {
            Subject::Programme { title, start, .. } => RevealKey::Programme { title: title.to_owned(), start },
            Subject::Episode { series_norm, season, episode } => RevealKey::Episode { series_norm: series_norm.to_owned(), season, episode },
            Subject::Vod { series_norm: Some(n), .. } => RevealKey::Series { series_norm: n.to_owned() },
            Subject::Vod { title, series_norm: None } => RevealKey::Vod { title: title.to_owned() },
            Subject::Notification { .. } => return None,
        })
    }

    /// Does this reveal cover `subject`?
    pub fn covers(&self, subject: &Subject<'_>) -> bool {
        match (self, subject) {
            (RevealKey::Programme { title, start }, Subject::Programme { title: t, start: s, .. }) => title == t && (start - s).abs() <= 6 * 3600,
            (RevealKey::Episode { series_norm, season, episode }, Subject::Episode { series_norm: n, season: s, episode: e }) => {
                series_norm == n && season == s && episode == e
            }
            (RevealKey::Series { series_norm }, Subject::Episode { series_norm: n, .. }) => series_norm == n,
            (RevealKey::Series { series_norm }, Subject::Vod { series_norm: Some(n), .. }) => series_norm == n,
            (RevealKey::Vod { title }, Subject::Vod { title: t, .. }) => title == t,
            _ => false,
        }
    }
}

/// The items shown on purpose right now (memory only; this UI thread's).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RevealSet {
    /// The item Ctrl+B was pressed on (`None`: a click on a bar).
    target: Option<Target>,
    keys: Vec<RevealKey>,
}

impl RevealSet {
    pub fn covers(&self, subject: &Subject<'_>) -> bool {
        self.keys.iter().any(|k| k.covers(subject))
    }
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Where the menu opens (Ctrl+Shift+B on an item, Enter on a Settings row).
#[derive(Debug, Clone, PartialEq)]
pub enum MenuFor {
    /// The plain menu, about the focused item when there is one.
    Item(Option<Box<Focus>>),
    /// A rule of Settings ▸ Spoiler guard.
    Rule(crate::model::SpoilerRule),
    /// A series the guard covers by itself (part-way through) or by the user's row.
    Series { series_norm: String, label: String, guarded: bool },
    /// A seen event of Settings ▸ "Seen recently".
    Seen(SeenRow),
}

/// What the guard knows about the focused item (built on a key press: one or two indexed
/// reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Focus {
    pub target: Target,
    pub keys: Vec<RevealKey>,
    /// Something of it is hidden or rewritten now.
    pub hidden: bool,
    pub reason: Option<Reason>,
    /// The guide programme (a programme, or the channel's current one).
    pub programme: Option<Programme>,
    /// The series (key, name) of an episode, a series or a guide airing with S/E.
    pub series: Option<(String, String)>,
}

#[derive(Default)]
struct UiState {
    reveal: RevealSet,
    /// Changes with every reveal or hide (painters' caches compare it).
    reveal_gen: u64,
    menu: Option<MenuFor>,
    /// Seen events of this session (the tracker's and Ctrl+W's), newest last, for Undo.
    seen: Vec<SeenRow>,
    /// The hint line's last answer: (focused item, guard generation, minute) → something of
    /// it is hidden.
    hint: Option<((Target, u64, Timestamp), bool)>,
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::default();
}

fn with<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// What is revealed now (a frame's painters read it once, `ui::widgets::spoiler::Spoilers`).
pub fn reveals() -> RevealSet {
    with(|s| s.reveal.clone())
}

/// Reveal `key` (a click on a bar or a "Description hidden" block), replacing other reveals:
/// one item at a time.
pub fn reveal_key(key: RevealKey) {
    set_reveal(RevealSet { target: None, keys: vec![key] });
}

fn set_reveal(r: RevealSet) {
    with(|s| {
        if s.reveal != r {
            s.reveal = r;
            s.reveal_gen += 1;
        }
    });
}

/// Changes whenever what is revealed changes: a painter that caches guarded text (Live's
/// details pane, the guide's tooltip) reloads when this or `Guard::generation()` moves.
pub fn reveal_generation() -> u64 {
    with(|s| s.reveal_gen)
}

/// Reveal the focused item (the menu's "Show spoilers here").
pub fn reveal_focus(f: &Focus) {
    set_reveal(RevealSet { target: Some(f.target), keys: f.keys.clone() });
}

/// Hide everything revealed (a click on 󰈈, focus left the item).
pub fn hide_all() {
    set_reveal(RevealSet::default());
}

/// Open the menu for `for_` (the overlay takes it when it's built).
pub fn open_menu(ctx: &mut UiCtx<'_>, for_: MenuFor) {
    with(|s| s.menu = Some(for_));
    ctx.open(crate::ui::OverlayRequest::SpoilerMenu);
}

/// The menu's subject (the overlay, once).
pub(crate) fn take_menu() -> Option<MenuFor> {
    with(|s| s.menu.take())
}

/// The next menu's subject (tests building the menu directly).
#[cfg(test)]
pub(crate) fn with_menu_for_test(for_: MenuFor) {
    with(|s| s.menu = Some(for_));
}

/// Seen events of the last half hour, newest first, after taking in the tracker's.
pub(crate) fn recent_seen(guard: &Guard, now: Timestamp) -> Vec<SeenRow> {
    absorb_seen(guard, now);
    with(|s| s.seen.iter().rev().cloned().collect())
}

/// Forget a seen event that was undone.
pub(crate) fn forget_seen(row: &SeenRow) {
    with(|s| s.seen.retain(|r| !(r.event_key == row.event_key && r.first_start == row.first_start)));
}

fn absorb_seen(guard: &Guard, now: Timestamp) {
    let fresh = guard.take_seen();
    with(|s| {
        s.seen.extend(fresh);
        s.seen.retain(|r| now - r.seen_at < RECENT_SEEN_S);
        let n = s.seen.len();
        if n > 8 {
            s.seen.drain(..n - 8);
        }
    });
}

/// The live guard when the feature is on (`None`: nothing of this applies).
fn guard_of(ctx: &UiCtx<'_>) -> Option<Guard> {
    crate::spoiler::ENABLED.then(|| ctx.services.agent.guard()).filter(Guard::is_live)
}

/// The Ctrl+B key's label ("Ctrl+B").
pub fn reveal_key_label() -> String {
    key_label(Action::RevealSpoilers)
}

fn key_label(a: Action) -> String {
    crate::keymap::GLOBAL.iter().find(|b| b.action == a).map(|b| b.chord.label()).unwrap_or_default()
}

/// What the guard knows about `target` now. `None` when the item is gone.
pub fn focus(ctx: &UiCtx<'_>, guard: &Guard, target: &Target) -> Option<Focus> {
    let now = ctx.now;
    let db = ctx.db;
    let mut f = Focus { target: *target, keys: Vec::new(), hidden: false, reason: None, programme: None, series: None };
    let judge = |subject: &Subject<'_>, f: &mut Focus| {
        let v = guard.judge(subject, now);
        let rewrites = v.neutralise_title
            && match subject {
                Subject::Programme { title, .. } | Subject::Vod { title, .. } => {
                    matches!(crate::textmatch::scores::neutralise_scores(title), std::borrow::Cow::Owned(_))
                }
                _ => false,
            };
        f.hidden |= v.guarded() || rewrites;
        if f.reason.is_none() {
            f.reason = v.reason.clone();
        }
        if let Some(k) = RevealKey::of(subject) {
            f.keys.push(k);
        }
    };
    match *target {
        Target::Programme { programme, .. } => {
            let p = crate::store::epg::programme(db, programme).ok()?;
            judge(&crate::ui::widgets::spoiler::programme_subject(&p), &mut f);
            f.series = crate::spoiler::programme_se(&p).map(|_| (crate::spoiler::series_key(&p.title), p.title.clone()));
            f.programme = Some(p);
        }
        Target::Channel(ch) => {
            let c = ctx.lib.live.get(ch)?;
            if let Some(epg) = c.epg_channel_id {
                let key = (epg, c.tvg_shift_min);
                let nn = crate::store::epg::now_next(db, &[key], now).ok()?;
                let nn = nn.get(&key);
                if let Some(p) = nn.and_then(|n| n.now.as_ref()).and_then(|s| crate::store::epg::programme(db, s.id).ok()) {
                    judge(&crate::ui::widgets::spoiler::programme_subject(&p), &mut f);
                    f.series = crate::spoiler::programme_se(&p).map(|_| (crate::spoiler::series_key(&p.title), p.title.clone()));
                    f.programme = Some(p);
                }
                // a channel row (and the player's banner) shows what's next too
                if let Some(next) = nn.and_then(|n| n.next.as_ref()) {
                    judge(&crate::ui::widgets::spoiler::slim_subject(next), &mut f);
                }
            }
        }
        Target::Episode { series, episode } => {
            let s = crate::store::vod::series(db, series).ok()?;
            let e = crate::store::vod::episode(db, episode).ok()?;
            let norm = crate::spoiler::series_key(&s.name);
            judge(&Subject::Episode { series_norm: &norm, season: e.season as i32, episode: e.episode as i32 }, &mut f);
            f.series = Some((norm, s.display_name().to_owned()));
        }
        Target::Series(series) => {
            let s = crate::store::vod::series(db, series).ok()?;
            let norm = crate::spoiler::series_key(&s.name);
            judge(&Subject::Vod { title: s.display_name(), series_norm: Some(&norm) }, &mut f);
            // the page's episodes: hidden after next-up when the series is guarded
            if let Some(m) = guard.snapshot().series.get(&norm) {
                f.hidden = true;
                f.reason.get_or_insert(Reason { rule: m.label.clone(), kind: crate::guard::ReasonKind::Series, until: None });
            }
            f.series = Some((norm, s.display_name().to_owned()));
        }
        Target::Movie(id) => {
            let m = crate::store::vod::movie(db, id).ok()?;
            judge(&Subject::Vod { title: &m.name, series_norm: None }, &mut f);
        }
    }
    Some(f)
}

/// Its entries in an item menu (`ui::item_menu::extend`); none while the feature is off.
pub fn menu_items(target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    let Some(guard) = guard_of(ctx) else { return Vec::new() };
    if !guard.on() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let f = focus(ctx, &guard, target);
    if let Some(f) = &f
        && f.hidden
    {
        let shown = with(|s| s.reveal.target == Some(*target) && !s.reveal.is_empty());
        let label = if shown { "Hide spoilers" } else { "Show spoilers" };
        items.push(entry(crate::ui::widgets::spoiler::SHOWN, label, Action::RevealSpoilers, keys));
    }
    if let Some(p) = f.as_ref().and_then(|f| f.programme.as_ref())
        && matches!(target, Target::Programme { .. })
        && crate::spoiler::verdict::tracks(&guard.snapshot(), &crate::ui::widgets::spoiler::programme_subject(p), ctx.now)
    {
        items.push(entry(glyphs::CHECK, "Mark seen (lifts the spoiler guard)", Action::MarkWatched, keys));
    }
    items.push(entry(glyphs::SPOILER, "Spoiler guard…", Action::SpoilerMenu, keys));
    items
}

/// Movement keys: focus leaves the revealed item.
fn moves(a: Action) -> bool {
    matches!(
        a,
        Action::Up
            | Action::Down
            | Action::Left
            | Action::Right
            | Action::PageUp
            | Action::PageDown
            | Action::Home
            | Action::End
            | Action::TabNext
            | Action::TabPrev
            | Action::FocusNext
            | Action::FocusPrev
            | Action::Parent
            | Action::Back
    )
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`, which screens call with
/// every item key before their own handling); false while off.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    if !crate::spoiler::ENABLED {
        return false;
    }
    // focus left the revealed item: hide it again (the key itself goes on to the screen)
    if with(|s| !s.reveal.is_empty() && (moves(action) || s.reveal.target.is_some_and(|t| t != *target))) {
        hide_all();
    }
    let Some(guard) = guard_of(ctx) else { return false };
    match action {
        Action::RevealSpoilers => {
            toggle_reveal(ctx, &guard, target);
            true
        }
        Action::MarkWatched if matches!(target, Target::Programme { .. }) => {
            match focus(ctx, &guard, target).and_then(|f| f.programme) {
                Some(p) => mark_seen(ctx, &guard, &p),
                None => ctx.toast(Toast::info("This programme is no longer in the guide")),
            }
            true
        }
        Action::SpoilerMenu => {
            let f = focus(ctx, &guard, target).map(Box::new);
            open_menu(ctx, MenuFor::Item(f));
            true
        }
        _ => false,
    }
}

/// Ctrl+B on an item: show its hidden text, or hide it again.
fn toggle_reveal(ctx: &mut UiCtx<'_>, guard: &Guard, target: &Target) {
    if with(|s| s.reveal.target == Some(*target) && !s.reveal.is_empty()) {
        hide_all();
        return;
    }
    if !guard.on() {
        ctx.toast(Toast::info("The spoiler guard is off"));
        return;
    }
    if guard.paused_until(ctx.now).is_some() {
        ctx.toast(Toast::info("Spoilers are shown everywhere for now"));
        return;
    }
    match focus(ctx, guard, target) {
        Some(f) if f.hidden && !f.keys.is_empty() => set_reveal(RevealSet { target: Some(*target), keys: f.keys }),
        _ => ctx.toast(Toast::info("Nothing hidden here")),
    }
}

/// Ctrl+B on something that isn't a list item (the player's banner about `target`): show
/// `keys` — what of it the guard hides now — or hide them again.
pub fn toggle_reveal_keys(ctx: &mut UiCtx<'_>, target: Target, keys: Vec<RevealKey>) {
    if with(|s| s.reveal.target == Some(target) && !s.reveal.is_empty()) {
        hide_all();
        return;
    }
    let Some(guard) = guard_of(ctx) else { return };
    if !guard.on() {
        ctx.toast(Toast::info("The spoiler guard is off"));
    } else if guard.paused_until(ctx.now).is_some() {
        ctx.toast(Toast::info("Spoilers are shown everywhere for now"));
    } else if keys.is_empty() {
        ctx.toast(Toast::info("Nothing hidden here"));
    } else {
        set_reveal(RevealSet { target: Some(target), keys });
    }
}

/// Focus left `target` (the player's picture changed): its reveal goes, as when focus moves on
/// in a list.
pub fn hide_reveal_of(target: Target) {
    if with(|s| s.reveal.target == Some(target)) {
        hide_all();
    }
}

/// The hint line's spoiler entry for the focused item: "show spoilers" when the guard hides
/// something of it, "hide spoilers" while that is shown (DESIGN §34.5); `None` otherwise. The
/// answer is kept per item, guard generation and minute (one or two indexed reads each).
pub fn hint(ctx: &UiCtx<'_>, target: Option<Target>) -> Option<(Action, &'static str)> {
    let target = target?;
    let guard = guard_of(ctx)?;
    if !guard.on() || guard.paused_until(ctx.now).is_some() {
        return None;
    }
    if with(|s| s.reveal.target == Some(target) && !s.reveal.is_empty()) {
        return Some((Action::RevealSpoilers, "hide spoilers"));
    }
    let key = (target, guard.generation(), ctx.now.div_euclid(60));
    let hidden = match with(|s| s.hint.filter(|(k, _)| *k == key)) {
        Some((_, hidden)) => hidden,
        None => {
            let hidden = focus(ctx, &guard, &target).is_some_and(|f| f.hidden && !f.keys.is_empty());
            with(|s| s.hint = Some((key, hidden)));
            hidden
        }
    };
    hidden.then_some((Action::RevealSpoilers, "show spoilers"))
}

/// Ctrl+W on a guide programme: record it seen when the guard tracks it (only guarded subjects
/// are ever recorded).
pub fn mark_seen(ctx: &mut UiCtx<'_>, guard: &Guard, p: &Programme) {
    let now = ctx.now;
    if !crate::spoiler::verdict::tracks(&guard.snapshot(), &crate::ui::widgets::spoiler::programme_subject(p), now) {
        ctx.toast(Toast::info("The spoiler guard isn't hiding anything about this"));
        return;
    }
    let row = crate::spoiler::placed_seen_row(&guard.snapshot(), p, SeenHow::Marked, now);
    let shown = guard.outside_title(&row.title, now).into_owned();
    let write = row.clone();
    let _ = ctx.services.store.writer().submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::mark_seen(c, &write).map(|_| ()));
    with(|s| s.seen.push(row));
    hide_all();
    ctx.toast(Toast {
        action: Some(("Spoiler guard…".into(), Action::SpoilerMenu)),
        ttl_ms: 6000,
        ..Toast::info(format!("“{shown}” marked seen — spoiler guard lifted"))
    });
}

/// Undo a seen event (the menu, Settings ▸ Seen recently).
pub fn unmark_seen(ctx: &mut UiCtx<'_>, row: &SeenRow) {
    let (key, start) = (row.event_key.clone(), row.first_start);
    let _ = ctx.services.store.writer().submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::unmark_seen(c, &key, start).map(|_| ()));
    forget_seen(row);
    ctx.toast(Toast::info("Guarded again"));
}

/// The status bar's chip while the guard is paused ("󰈈 Spoilers shown · 14 min ✕"): a click
/// opens the menu, ✕ guards again. Also takes in the tracker's seen events for the menu's Undo.
pub fn status_chip(shared: &crate::shared::AppShared, now: Timestamp) -> Option<crate::ui::widgets::status_bar::StatusChip> {
    if !crate::spoiler::ENABLED {
        return None;
    }
    let guard = shared.guard();
    if !guard.is_live() {
        return None;
    }
    absorb_seen(&guard, now);
    let until = guard.paused_until(now)?;
    let left = if until == Timestamp::MAX { "until restart".to_owned() } else { format!("{} min", ((until - now) + 59) / 60) };
    let menu = key_label(Action::SpoilerMenu);
    Some(crate::ui::widgets::status_bar::StatusChip {
        text: format!("{} Spoilers shown · {left}", crate::ui::widgets::spoiler::SHOWN),
        label: format!("Spoilers shown {left} — the spoiler guard menu ({menu})"),
        action: Action::SpoilerMenu,
        warning: false,
        close: Some(crate::ui::widgets::status_bar::ChipClose::new("Guard spoilers again", move || guard.resume())),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::guard::Field;
    use crate::spoiler::snapshot::{GuardSnapshot, SeriesMark, Settings};
    use crate::ui::screens::home::testkit::{named_series, rig, source};
    use crate::ui::widgets::spoiler::Spoilers;

    /// Ctrl+B on a guarded episode shows it; a movement key on it hides it again; an episode
    /// with nothing hidden says so.
    #[test]
    fn ctrl_b_shows_the_focused_item_until_focus_moves_on() {
        hide_all();
        let (mut t, _one) = rig();
        let series = t
            .services()
            .store
            .writer()
            .blocking(|c| {
                let src = source(c, "TV")?;
                named_series(c, src, &[("Severance", "Severance")], 6)
            })
            .unwrap()[0];
        let norm = crate::spoiler::series_key("Severance");
        let mut marks = HashMap::new();
        marks.insert(norm.clone(), SeriesMark { label: "Severance".into(), next_up: (1, 4), automatic: true, part_way: true, ..Default::default() });
        t.services().agent.set_guard(Guard::with_snapshot(GuardSnapshot::from_parts(Settings::default(), Vec::new(), marks, HashMap::new())));
        let eps = crate::store::vod::episodes(&t.db, series, Some(1)).unwrap();
        let target = |n: usize| Target::Episode { series, episode: eps[n - 1].id };
        let e5 = Subject::Episode { series_norm: &norm, season: 1, episode: 5 };
        let shown = |t: &mut crate::ui::testing::TestUi| {
            t.with_ctx(|ctx| Spoilers::of(ctx).text(&e5, Field::EpisodeTitle, "Episode 5").map(|c| c.into_owned()))
        };
        assert_ne!(shown(&mut t).as_deref(), Some("Episode 5"));
        assert!(t.with_ctx(|ctx| on_action(ctx, Action::RevealSpoilers, &target(5))));
        assert_eq!(shown(&mut t).as_deref(), Some("Episode 5"));
        // the menu offers to hide it again
        let items = t.with_ctx(|ctx| menu_items(&target(5), ctx, KeyContext::Detail));
        assert!(items.iter().any(|i| i.label == "Hide spoilers"));
        // focus moves on (the key still goes to the screen)
        assert!(!t.with_ctx(|ctx| on_action(ctx, Action::Down, &target(5))));
        assert_ne!(shown(&mut t).as_deref(), Some("Episode 5"));
        // the next-up episode hides nothing
        assert!(t.with_ctx(|ctx| on_action(ctx, Action::RevealSpoilers, &target(4))));
        assert_eq!(t.toasts.last().map(|t| t.text.as_str()), Some("Nothing hidden here"));
        assert!(reveals().is_empty());
    }

    /// Paused: the chip counts down; its ✕ guards again at once.
    #[test]
    fn the_pause_chip_guards_again_from_its_close_button() {
        let shared = crate::shared::AppShared::default();
        let guard = Guard::live();
        shared.set_guard(guard.clone());
        assert!(status_chip(&shared, 1000).is_none());
        guard.pause(Some(1000 + 14 * 60));
        let chip = status_chip(&shared, 1000).unwrap();
        assert!(chip.text.ends_with("Spoilers shown · 14 min"), "{}", chip.text);
        chip.close.unwrap().run();
        assert!(guard.paused_until(1000).is_none() && status_chip(&shared, 1000).is_none());
    }

    #[test]
    fn reveal_keys_cover_the_same_item_wherever_it_is_painted() {
        let cats = vec!["Sport".to_owned()];
        let airing =
            Subject::Programme { title: "Arsenal v Chelsea", sub_title: None, categories: &cats, season_episode: None, start: 1000, is_new: false };
        let key = RevealKey::of(&airing).unwrap();
        // the slim grid row of the same airing (no categories), and a +1 channel's repeat
        assert!(key.covers(&Subject::Programme {
            title: "Arsenal v Chelsea",
            sub_title: Some("x"),
            categories: &[],
            season_episode: None,
            start: 1000,
            is_new: false,
        }));
        assert!(key.covers(&Subject::Programme {
            title: "Arsenal v Chelsea",
            sub_title: None,
            categories: &[],
            season_episode: None,
            start: 4600,
            is_new: false
        }));
        assert!(!key.covers(&Subject::Programme {
            title: "Spurs v Chelsea",
            sub_title: None,
            categories: &[],
            season_episode: None,
            start: 1000,
            is_new: false
        }));
        // a series page reveal covers its episodes, an episode's only itself
        let series = RevealKey::of(&Subject::Vod { title: "Severance", series_norm: Some("severance") }).unwrap();
        assert!(series.covers(&Subject::Episode { series_norm: "severance", season: 2, episode: 7 }));
        let ep = RevealKey::of(&Subject::Episode { series_norm: "severance", season: 2, episode: 5 }).unwrap();
        assert!(ep.covers(&Subject::Episode { series_norm: "severance", season: 2, episode: 5 }));
        assert!(!ep.covers(&Subject::Episode { series_norm: "severance", season: 2, episode: 6 }));
        assert!(RevealKey::of(&Subject::Notification { title: "x" }).is_none(), "text leaving the window is never revealed");
    }
}
