//! Up next's UI pieces (DESIGN §32). OWNER: Q-ui.
//!
//! * Add from anywhere: the item menus' "Add to Up next" / "Play next" ([`menu_items`]), the
//!   programme popover's "Up next" button ([`popover_button`]) and Ctrl+N / Ctrl+Shift+N
//!   ([`on_action`]) on a film, an episode, a series (the episode-count picker, [`SeriesPicker`]),
//!   a guide programme or a channel (its programme on now).
//! * [`card`]: the card between items (the player, the Up next screen's banner, the mini
//!   player's chip) computed from the engine's snapshot.
//! * [`list`]: the list the Up next screen and the player's `u` panel share (sections, marks,
//!   keyboard and drag reorder).
//! * [`mini_chip`]: "Next in 8 s ✕" over the mini player's video.
//!
//! The UI never decides what plays: every choice is a [`QueueCommand`] to the engine
//! (`AppShared::queue_handle`), and everything drawn comes from its snapshot
//! (`AppShared::queue`). No DB work per frame: an add does a few indexed reads once, on the key
//! press.

pub mod card;
pub mod list;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use eframe::egui::{self, Align2, FontId, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, ItemKind, ItemRef, QueueId, SeriesId, Timestamp, VodKind};
use crate::msg::{Toast, ToastKind};
use crate::queue::{AddTarget, AddedBy, Insert, QueueCommand, QueueEntry, QueueReply, QueueSnapshot, RowState, Slot};
use crate::shared::AppShared;
use crate::theme::{SpaceToken, TypeToken, with_alpha};
use crate::ui::item_menu::Target;
use crate::ui::overlays::dialogs::ContextMenuOverlay;
use crate::ui::overlays::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::ui::widgets::glyphs;
use crate::ui::{OverlayRequest, UiCtx};

/// `Action::Digit` codes of Up next's menus and PIN card (above typed digits and the player's
/// "Play from" copies, which start at 100).
pub const PIN_CODE: u8 = 250;

/// The latest snapshot, as the spoiler guard lets it show (B4, Normal: a guarded programme's
/// title, an episode's title, the next-episode card's title). Every Up next UI piece reads it
/// through here, which also starts (once per app) a watcher that repaints the window whenever
/// the engine publishes: a countdown that starts while nothing else moves must still be drawn.
pub fn snapshot(ctx: &UiCtx<'_>) -> Arc<QueueSnapshot> {
    remember(ctx);
    let raw = ctx.services.agent.queue();
    if raw.rows.is_empty() && raw.next_episode.is_none() && raw.watchlist.is_none() && raw.finished_watchlist.is_none() {
        return raw;
    }
    // once per published snapshot, minute (the guard's windows move with the clock) and guard
    // stamp (a new guard snapshot, a reveal or a hide)
    thread_local! {
        static GUARDED: std::cell::RefCell<Option<Guarded>> = const { std::cell::RefCell::new(None) };
    }
    let sp = crate::ui::widgets::spoiler::Spoilers::of(ctx);
    let visible = crate::ui::watchlists::snapshot(ctx);
    let hides = ctx.parental.hides_explicit(&ctx.config.parental);
    let key = (ctx.now / 60, sp.stamp(), ctx.lib.generation, hides, crate::ui::watchlists::revision());
    GUARDED.with_borrow_mut(|g| {
        if let Some((w, k, s)) = g.as_ref()
            && *k == key
            && std::ptr::eq(w.as_ptr(), Arc::as_ptr(&raw))
        {
            return s.clone();
        }
        let mut filtered = guard_snapshot(&sp, &raw);
        if hides {
            let places: std::collections::HashMap<_, _> = visible
                .lists
                .iter()
                .flat_map(|l| l.entries.iter().enumerate().map(move |(i, e)| (e.entry.id, (l.list.name.clone(), i + 1, l.total))))
                .collect();
            filtered.rows = filtered
                .rows
                .iter()
                .filter(|r| {
                    !ctx.lib.conceals(&r.entry.item)
                        && !ctx.lib.explicit.contains(&r.entry.item)
                        && r.entry.watchlist_item_id.is_none_or(|id| places.contains_key(&id))
                })
                .cloned()
                .collect();
            if filtered.watchlist.as_ref().is_some_and(|l| visible.list(l.id).is_none()) {
                filtered.watchlist = None;
            }
            if matches!(filtered.finished,Some(crate::queue::FinishedWhy::EndOfList(id)) if visible.list(id).is_none()) {
                filtered.finished = None;
                filtered.finished_watchlist = None;
            }
            filtered.watchlist_chips.retain(|id, chip| {
                if let Some((name, position, total)) = places.get(id) {
                    chip.name = name.clone();
                    chip.position = *position;
                    chip.total = *total;
                    true
                } else {
                    false
                }
            });
            if let Some(l) = &mut filtered.watchlist {
                l.then.retain(|e| places.contains_key(&e.id));
                if let Some(v) = visible.list(l.id) {
                    l.total = v.total;
                    let current = filtered.rows.iter().find(|r| Some(r.entry.id) == filtered.current).and_then(|r| r.entry.watchlist_item_id);
                    l.position = current.or(v.next).and_then(|id| v.place(id)).map_or(0, |p| p.0);
                }
            }
        }
        let s = Arc::new(filtered);
        *g = Some((Arc::downgrade(&raw), key, s.clone()));
        s
    })
}

/// The guarded snapshot of (the raw one, the minute and guard stamp it was made for).
type Guarded = (Weak<QueueSnapshot>, (i64, (u64, u64), u64, bool, u64), Arc<QueueSnapshot>);

/// `raw` with its rows' and next episode's text through the guard.
fn guard_snapshot(sp: &crate::ui::widgets::spoiler::Spoilers, raw: &QueueSnapshot) -> QueueSnapshot {
    let mut s = raw.clone();
    s.rows = s.rows.iter().map(|r| crate::queue::QueueRow { entry: guarded(sp, &r.entry), ..r.clone() }).collect();
    s.next_episode = s.next_episode.map(|n| guarded_next(sp, n));
    if let Some(list) = &mut s.watchlist {
        for e in &mut list.then {
            if e.kind == ItemKind::Episode
                && let (Some(series), Some(season), Some(episode)) = (&e.series, e.season, e.episode)
            {
                let norm = crate::spoiler::series_key(series);
                let subject = crate::guard::Subject::Episode { series_norm: &norm, season: season as i32, episode: episode as i32 };
                e.title = sp
                    .text(&subject, crate::guard::Field::EpisodeTitle, &e.title)
                    .map_or_else(|| format!("{series} · S{season}E{episode}"), |t| t.into_owned());
            }
        }
    }
    s
}

/// The next-episode card's "S02E06 · Title": the title as the guard lets it show (the number
/// always shows).
fn guarded_next(sp: &crate::ui::widgets::spoiler::Spoilers, mut n: crate::queue::NextEpisode) -> crate::queue::NextEpisode {
    use crate::guard::{Field, Subject};
    let Some((num, title)) = n.label.split_once(" · ") else { return n };
    let norm = crate::spoiler::series_key(&n.series);
    let subj = Subject::Episode { series_norm: &norm, season: n.season as i32, episode: n.episode as i32 };
    let label = match sp.text(&subj, Field::EpisodeTitle, title) {
        Some(t) => format!("{num} · {t}"),
        None => num.to_owned(),
    };
    n.label = label;
    n
}

/// A row's title and subtitle through the guard: a programme slot's title, an episode's own
/// title ("S02E05 · …" keeps its number).
fn guarded(sp: &crate::ui::widgets::spoiler::Spoilers, e: &QueueEntry) -> QueueEntry {
    use crate::guard::{Field, Subject};
    let mut out = e.clone();
    if let Some(slot) = &e.programme {
        let subj = Subject::Programme { title: &e.title, sub_title: None, categories: &[], season_episode: None, start: slot.start, is_new: false };
        out.title = sp.text(&subj, Field::Title, &e.title).map_or_else(|| "Programme (spoiler guard)".to_owned(), |t| t.into_owned());
        return out;
    }
    let Some(sub) = e.subtitle.as_deref().filter(|_| e.item.kind == ItemKind::Episode) else { return out };
    let Some((num, title)) = sub.split_once(" · ") else { return out };
    let se = num.strip_prefix('S').and_then(|x| x.split_once('E')).and_then(|(s, e)| Some((s.parse::<i32>().ok()?, e.parse::<i32>().ok()?)));
    let Some((season, episode)) = se else { return out };
    let norm = crate::spoiler::series_key(&e.title);
    let subj = Subject::Episode { series_norm: &norm, season, episode };
    out.subtitle = Some(match sp.text(&subj, Field::EpisodeTitle, title) {
        Some(t) => format!("{num} · {t}"),
        None => num.to_owned(),
    });
    out
}

/// The apps whose shared state the UI has seen, the latest last (the mini chip sends through
/// it: it has no `UiCtx`).
static SEEN: Mutex<Vec<Weak<AppShared>>> = Mutex::new(Vec::new());

/// Remember this app's shared state and start its repaint watcher (once).
fn remember(ctx: &UiCtx<'_>) {
    let shared = &ctx.services.agent;
    let mut w = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    w.retain(|x| x.strong_count() > 0);
    let same = |x: &Weak<AppShared>| std::ptr::eq(x.as_ptr(), Arc::as_ptr(shared));
    if w.last().is_some_and(same) {
        return;
    }
    if let Some(i) = w.iter().position(same) {
        let x = w.remove(i);
        w.push(x);
        return;
    }
    w.push(Arc::downgrade(shared));
    let mut rx = shared.subscribe_queue();
    let bus = ctx.services.bus.clone();
    ctx.services.rt.spawn(async move {
        while rx.changed().await.is_ok() {
            bus.request_repaint();
        }
    });
}

/// The engine of the app the UI drew last.
fn remembered() -> Option<crate::queue::QueueHandle> {
    let w = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    w.iter().rev().find_map(Weak::upgrade).and_then(|s| s.queue_handle())
}

/// Send a command to the engine; false (with a toast) when it isn't running.
pub fn send(ctx: &mut UiCtx<'_>, cmd: QueueCommand) -> bool {
    remember(ctx);
    match ctx.services.agent.queue_handle() {
        Some(h) => {
            h.send(cmd);
            true
        }
        None => {
            ctx.toast(Toast::info("Up next isn't running"));
            false
        }
    }
}

/// What Enter on a row did ([`play_row`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowPlay {
    /// Sent to the engine: it plays from here.
    Sent,
    /// The Resume prompt is open; its answer goes to the engine.
    Prompt,
    /// Up next isn't running (a toast says so).
    NotRunning,
}

/// Enter (a click, "Play now") on a row of the Up next screen or the player's panel
/// (up-next-design §3 "Resume": "Enter on a row uses the normal Resume prompt"): the Resume
/// prompt when the row's film or episode has progress past `resume_min_s` and the row doesn't
/// ask for the start; else play from here as before. Locked, concealed and gone rows go
/// straight to the engine, which says why. One read on the key press; when the frame's budget
/// is spent, it plays (and resumes silently, as advancing does).
pub fn play_row(ctx: &mut UiCtx<'_>, s: &QueueSnapshot, id: QueueId, fullscreen: bool) -> RowPlay {
    if let Some(row) = s.row(id).filter(|r| !r.entry.from_start && r.entry.programme.is_none()) {
        let cfg = ctx.config;
        let view = crate::shared::ParentalView {
            unlocked: ctx.parental.is_unlocked(&cfg.parental),
            hides_explicit: ctx.parental.hides_explicit(&cfg.parental),
            pin_active: cfg.parental.pin_active(),
        };
        let (db, lib, now) = (ctx.db, ctx.lib.clone(), ctx.now);
        let at = ctx
            .budget
            .run(|| crate::queue::service::resume_point(db, &lib, view, cfg, &row.entry, now))
            .and_then(|r| r.map_err(|e| tracing::debug!("queue: resume point of #{id}: {e}")).ok().flatten());
        if let Some(at) = at.filter(|a| a.position_s > f64::from(cfg.player.resume_min_s)) {
            if ctx.services.agent.queue_handle().is_none() {
                ctx.toast(Toast::info("Up next isn't running"));
                return RowPlay::NotRunning;
            }
            // the prompt says what the row says: through the guard
            let e = &guarded(&crate::ui::widgets::spoiler::Spoilers::of(ctx), &row.entry);
            let title = match &e.subtitle {
                Some(sub) => format!("{} — {sub}", e.title),
                None => e.title.clone(),
            };
            ctx.open(crate::ui::OverlayRequest::Resume(crate::ui::overlays::ResumeSpec {
                media: at.media,
                title,
                position_s: at.position_s,
                duration_s: at.duration_s,
                copy: None,
                queue: Some(crate::ui::overlays::QueueResume { id, fullscreen }),
                cast: None,
                watchlist: None,
            }));
            return RowPlay::Prompt;
        }
    }
    if send(ctx, QueueCommand::PlayAt(id)) { RowPlay::Sent } else { RowPlay::NotRunning }
}

// ------------------------------------------------------------------ text

/// "2 h 10" / "7 min" / "?" (unknown).
pub fn duration(secs: Option<f64>) -> String {
    match secs {
        Some(s) if s.is_finite() && s > 0.0 => crate::ui::screens::home::kit::runtime(s as u32),
        _ => "?".into(),
    }
}

/// "~00:45".
pub fn eta(ts: Timestamp, h24: bool) -> String {
    format!("~{}", crate::ui::widgets::time::hhmm(ts, h24))
}

/// The header's summary: "5 to go · 9 h 41 · ends ~03:12" ("ends ≥ 03:12" with an unknown
/// duration in it); `None` with nothing to go.
pub fn summary(s: &QueueSnapshot, now: Timestamp, h24: bool) -> Option<String> {
    let left = s.pending();
    if left == 0 {
        return None;
    }
    let mut parts = vec![format!("{left} to go")];
    if let Some((end, exact)) = s.eta_end {
        if end > now {
            parts.push(crate::ui::screens::home::kit::runtime((end - now) as u32));
        }
        let at = crate::ui::widgets::time::hhmm(end, h24);
        parts.push(if exact { format!("ends ~{at}") } else { format!("ends ≥ {at}") });
    }
    Some(parts.join(" · "))
}

/// A row's display title ("Thunderball (1965)", "Slow Horses · S02E03 · …", "Bake Off · BBC
/// One · Sat 20:00").
pub fn row_title(e: &QueueEntry) -> String {
    match &e.subtitle {
        Some(sub) => format!("{} · {sub}", e.title),
        None => e.title.clone(),
    }
}

// ------------------------------------------------------------------ adding

/// The menu entries of an item ("Add to Up next", "Play next"); none while Up next is off.
pub fn menu_items(target: &Target, _ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    if !crate::queue::ENABLED {
        return Vec::new();
    }
    let add = if matches!(target, Target::Series(_)) { "Add to Up next…" } else { "Add to Up next" };
    vec![
        crate::ui::item_menu::entry(glyphs::UP_NEXT, add, Action::QueueAdd, keys),
        crate::ui::item_menu::entry(glyphs::NEXT, "Play next", Action::QueuePlayNext, keys),
    ]
}

/// The programme popover's "Up next" button; none while Up next is off.
pub fn popover_button(target: &Target, _ctx: &UiCtx<'_>) -> Option<MenuItem> {
    if !crate::queue::ENABLED || !matches!(target, Target::Programme { .. }) {
        return None;
    }
    Some(MenuItem { icon: glyphs::UP_NEXT, label: "Up next".into(), action: Action::QueueAdd, enabled: true, shortcut: Some("Ctrl+N".into()) })
}

/// Ctrl+N / Ctrl+Shift+N (and the menus' choices) on an item; false for every other action.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    if !crate::queue::ENABLED || !matches!(action, Action::QueueAdd | Action::QueuePlayNext) {
        return false;
    }
    let at = if action == Action::QueuePlayNext { Insert::Next } else { Insert::End };
    if let Target::Series(series) = target {
        ctx.open(OverlayRequest::QueueSeries { series: *series, play_next: at == Insert::Next });
        return true;
    }
    match add_target(ctx, target) {
        Ok(t) => {
            send(ctx, QueueCommand::Add { targets: vec![t], at, by: AddedBy::App });
        }
        Err(why) => ctx.toast(Toast::info(why)),
    }
    true
}

/// What to add for an item (a few indexed reads, once per key press).
fn add_target(ctx: &UiCtx<'_>, target: &Target) -> Result<AddTarget, &'static str> {
    const GONE: &str = "That is no longer in your playlists";
    let plain = |item: ItemRef| AddTarget { item, programme: None, from_start: false, any_copy: true };
    match *target {
        Target::Movie(id) => ctx.lib.vod.find(VodKind::Movie(id)).map(|v| plain(crate::store::copies::vod_ref(v))).ok_or(GONE),
        // the engine adds a series' next episode (the picker sends episodes)
        Target::Series(id) => ctx.lib.vod.find(VodKind::Series(id)).map(|v| plain(crate::store::copies::vod_ref(v))).ok_or(GONE),
        Target::Episode { episode, .. } => crate::store::vod::episode(ctx.db, episode)
            .map(|e| plain(ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }))
            .map_err(|_| GONE),
        Target::Programme { programme, channel } => {
            let ch = ctx.lib.live.get(channel).ok_or(GONE)?;
            let p = crate::store::epg::programme(ctx.db, programme).map_err(|_| "That programme is no longer in the guide")?;
            let shift = i64::from(ch.tvg_shift_min) * 60;
            Ok(slot_target(ch, p.start + shift, p.stop + shift))
        }
        Target::Channel(channel) => {
            let ch = ctx.lib.live.get(channel).ok_or(GONE)?;
            on_now(ctx, channel).map(|(start, stop)| slot_target(ch, start, stop)).ok_or(NO_GUIDE)
        }
    }
}

const NO_GUIDE: &str = "Live channels join Up next by programme; this one has no guide";

/// A channel's programme slot (displayed times; the engine finds the programme again).
fn slot_target(ch: &crate::model::ChannelSummary, start: Timestamp, stop: Timestamp) -> AddTarget {
    AddTarget {
        item: crate::ui::screens::live::channel_ref(ch),
        programme: Some(Slot { start, stop, catchup_id: None }),
        from_start: false,
        any_copy: false,
    }
}

/// The programme on now on a channel (displayed times), one indexed read.
fn on_now(ctx: &UiCtx<'_>, channel: ChannelId) -> Option<(Timestamp, Timestamp)> {
    let ch = ctx.lib.live.get(channel)?;
    let epg = ch.epg_channel_id?;
    let at = ctx.now - i64::from(ch.tvg_shift_min) * 60;
    let p = crate::store::epg::window(ctx.db, epg, at, at + 1).ok()?.into_iter().find(|p| p.contains(at))?.shifted(ch.tvg_shift_min);
    Some((p.start, p.stop))
}

// ------------------------------------------------------------------ reorder and remove

/// Commands that put `ids` (in their list order) right before the visible row `before`, or at
/// the end. The store's `Insert::Index` counts every row, the concealed ones included, which
/// the snapshot never shows; so this moves the rows to the end, then everything that must
/// follow them: the visible order is exact whatever is concealed.
pub fn move_before(s: &QueueSnapshot, ids: &[QueueId], before: Option<QueueId>) -> Vec<QueueCommand> {
    let mut out = vec![QueueCommand::Move { ids: ids.to_vec(), to: Insert::End }];
    let Some(b) = before.filter(|b| !ids.contains(b)) else { return out };
    let Some(at) = s.rows.iter().position(|r| r.entry.id == b) else { return out };
    let tail: Vec<QueueId> = s.rows[at..].iter().map(|r| r.entry.id).filter(|id| !ids.contains(id)).collect();
    out.push(QueueCommand::Move { ids: tail, to: Insert::End });
    out
}

/// Removed rows kept for the toast's Undo: (number, rows, the row they stood before).
type Removed = (u16, Vec<QueueEntry>, Option<QueueId>);

/// Up next's Undo numbers (`Action::Undo(n)`, far above the copies' own numbers).
const UNDO_BASE: u16 = 0xF000;
static UNDO: Mutex<VecDeque<Removed>> = Mutex::new(VecDeque::new());
/// Counts Undo numbers: a number comes back only after 4096 more removals, never while a toast
/// that carries it may still show.
static UNDO_NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// Whether `n` is one of Up next's Undo numbers.
pub fn is_undo(n: u16) -> bool {
    n >= UNDO_BASE
}

/// Remove `ids` (Delete, the row's ✕, the menu); the toast offers Undo for pending rows.
pub fn remove(ctx: &mut UiCtx<'_>, s: &QueueSnapshot, ids: Vec<QueueId>) {
    if ids.is_empty() || !send(ctx, QueueCommand::Remove(ids.clone())) {
        return;
    }
    let gone: Vec<QueueEntry> = s.rows.iter().filter(|r| ids.contains(&r.entry.id)).map(|r| r.entry.clone()).collect();
    let first = s.rows.iter().position(|r| ids.contains(&r.entry.id));
    let before = first.and_then(|i| s.rows[i..].iter().find(|r| !ids.contains(&r.entry.id))).map(|r| r.entry.id);
    let text = match gone.len() {
        1 => "Removed from Up next".to_owned(),
        n => format!("Removed {n} from Up next"),
    };
    let undoable: Vec<QueueEntry> = gone.into_iter().filter(|e| !e.state.is_done()).collect();
    if undoable.is_empty() {
        ctx.toast(Toast::info(text));
        return;
    }
    let mut q = UNDO.lock().unwrap_or_else(|e| e.into_inner());
    let n = UNDO_BASE + UNDO_NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % (u16::MAX - UNDO_BASE + 1);
    q.push_back((n, undoable, before));
    while q.len() > 8 {
        q.pop_front();
    }
    ctx.toast(Toast { kind: ToastKind::Info, text, action: Some(("Undo".into(), Action::Undo(n))), also: None, ttl_ms: 6000 });
}

/// The toast's Undo: put the rows back where they were (once). True when `n` was Up next's.
pub fn undo(ctx: &mut UiCtx<'_>, n: u16) -> bool {
    if !is_undo(n) {
        return false;
    }
    if let Err(why) = undo_in(ctx.services, n) {
        ctx.toast(Toast::info(why));
    }
    true
}

/// [`undo`] without a `UiCtx` (the app's global Undo, whatever screen shows): `Err` with what
/// to say when it can't.
pub fn undo_in(services: &crate::services::Services, n: u16) -> Result<(), &'static str> {
    let taken = {
        let mut q = UNDO.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().position(|(k, ..)| *k == n).and_then(|i| q.remove(i))
    };
    let Some((_, rows, before)) = taken else { return Err("That can no longer be undone") };
    let Some(h) = services.agent.queue_handle() else { return Err("Up next isn't running") };
    let targets = rows
        .iter()
        .map(|e| AddTarget { item: e.item.clone(), programme: e.programme.clone(), from_start: e.from_start, any_copy: e.any_copy })
        .collect();
    services.rt.spawn(async move {
        if let QueueReply::Added { added, .. } = h.request(QueueCommand::Add { targets, at: Insert::End, by: AddedBy::App }).await
            && !added.is_empty()
        {
            let ids: Vec<QueueId> = added.iter().map(|(id, _)| *id).collect();
            let s = h.snapshot();
            for c in move_before(&s, &ids, before).into_iter().skip(1) {
                h.send(c);
            }
        }
    });
    Ok(())
}

// ------------------------------------------------------------------ copies

/// Ctrl+O on a row: the title's copies to pin ("Use another copy…"), as (menu items, copies);
/// `None` with a reason when the row has no other copy. A few indexed reads, once per press.
pub fn copy_choices(ctx: &UiCtx<'_>, e: &QueueEntry, first_code: u8) -> Result<(Vec<MenuItem>, Vec<ItemRef>), &'static str> {
    const ONE: &str = "This title has no other copy";
    let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
    let (db, lib) = (ctx.db, ctx.lib);
    let copies: Vec<ItemRef> = match e.item.kind {
        ItemKind::Channel => return Err("A programme plays from its channel"),
        ItemKind::Movie => {
            let idx = crate::store::vod::resolve_movie(db, &e.item).ok().flatten().and_then(|m| lib.vod.index_of(VodKind::Movie(m))).ok_or(ONE)?;
            let plan = crate::ui::copies::plan(ctx, true, idx).map_err(|_| ONE)?;
            plan.copies.iter().filter(|c| unlocked || !c.locked).map(|c| c.item_ref.clone()).collect()
        }
        ItemKind::Episode | ItemKind::Series => {
            let ep =
                crate::store::vod::resolve_episode(db, &e.item).ok().flatten().and_then(|id| crate::store::vod::episode(db, id).ok()).ok_or(ONE)?;
            let idx = lib.vod.index_of(VodKind::Series(ep.series_id)).ok_or(ONE)?;
            let plan = crate::ui::copies::plan(ctx, false, idx).map_err(|_| ONE)?;
            plan.copies
                .iter()
                .filter(|c| unlocked || !c.locked)
                .filter_map(|c| match lib.vod.series.get(c.item as usize)?.kind {
                    VodKind::Series(s) => crate::store::copies::same_episode(db, s, ep.season, ep.episode).ok().flatten(),
                    VodKind::Movie(_) => None,
                })
                .filter_map(|id| crate::store::vod::episode(db, id).ok())
                .map(|x| ItemRef { source: x.source_id, kind: ItemKind::Episode, key: x.key })
                .collect()
        }
    };
    if copies.len() < 2 {
        return Err(ONE);
    }
    let items = copies
        .iter()
        .take(usize::from(u8::MAX - first_code))
        .enumerate()
        .map(|(k, r)| {
            let name = lib.sources.iter().find(|s| s.id == r.source).map_or_else(|| "Another playlist".to_owned(), |s| s.name.clone());
            let this = !e.any_copy && *r == e.item;
            MenuItem {
                icon: if this { glyphs::PIN } else { glyphs::PLAY },
                label: if this { format!("Play from {name} (chosen)") } else { format!("Play from {name}") },
                action: Action::Digit(first_code + k as u8),
                enabled: true,
                shortcut: None,
            }
        })
        .collect();
    Ok((items, copies))
}

// ------------------------------------------------------------------ the series picker

/// The series picker (`OverlayRequest::QueueSeries`): how many episodes to add — next
/// episode · next 3 · the rest of the season · next 50 (rows are concrete episodes).
pub struct SeriesPicker {
    series: SeriesId,
    play_next: bool,
    menu: Option<ContextMenuOverlay>,
    /// Episodes from the next one on, in play order.
    episodes: Vec<crate::model::Episode>,
    /// How many of them each choice adds.
    counts: Vec<usize>,
}

impl SeriesPicker {
    pub fn new(series: SeriesId, play_next: bool) -> Self {
        Self { series, play_next, menu: None, episodes: Vec::new(), counts: Vec::new() }
    }

    /// Read the episodes and build the menu (once, on the first frame or key).
    fn ready(&mut self, ctx: &mut UiCtx<'_>) -> bool {
        if self.menu.is_some() {
            return true;
        }
        let lib = ctx.lib;
        let Some(idx) = lib.vod.index_of(VodKind::Series(self.series)) else {
            ctx.toast(Toast::info("That is no longer in your playlists"));
            return false;
        };
        // the title's copies: its next episode wherever it was watched last
        let ids: Vec<SeriesId> = lib
            .vod
            .stacks(false)
            .copies(idx)
            .iter()
            .filter_map(|c| match lib.vod.series.get(*c as usize)?.kind {
                VodKind::Series(id) => Some(id),
                VodKind::Movie(_) => None,
            })
            .chain(std::iter::once(self.series))
            .collect();
        let next = crate::store::copies::next_episode(ctx.db, &ids).ok().flatten();
        let (sid, first) = match next {
            Some((s, e)) => (s, Some(e.id)),
            None => (self.series, None),
        };
        let all = crate::store::vod::episodes(ctx.db, sid, None).unwrap_or_default();
        let from = first.and_then(|id| all.iter().position(|e| e.id == id)).unwrap_or(0);
        self.episodes = all.into_iter().skip(from).collect();
        let Some(e0) = self.episodes.first() else {
            ctx.toast(Toast::info("Open the series once to load its episodes"));
            return false;
        };
        let season = e0.season;
        let rest = self.episodes.iter().take_while(|e| e.season == season).count();
        let label0 = format!("Next episode · S{:02}E{:02}", e0.season, e0.episode);
        let mut choices: Vec<(String, usize)> = vec![(label0, 1)];
        if self.episodes.len() > 1 {
            choices.push((format!("Next {}", self.episodes.len().min(3)), self.episodes.len().min(3)));
        }
        if rest > 3 || (rest > 1 && self.episodes.len() > 3) {
            choices.push((format!("Rest of season {season} ({rest})"), rest));
        }
        if self.episodes.len() > 3 && self.episodes.len().min(crate::store::queue::MAX_ADD) > rest {
            let n = self.episodes.len().min(crate::store::queue::MAX_ADD);
            choices.push((format!("Next {n}"), n));
        }
        let items = choices
            .iter()
            .enumerate()
            .map(|(k, (label, _))| MenuItem {
                icon: glyphs::UP_NEXT,
                label: label.clone(),
                action: Action::Digit(k as u8),
                enabled: true,
                shortcut: None,
            })
            .collect();
        self.counts = choices.into_iter().map(|(_, n)| n).collect();
        let name = lib.vod.series.get(idx as usize).map(|v| v.name.to_string()).unwrap_or_default();
        let title = if self.play_next { format!("Play next · {name}") } else { format!("Add to Up next · {name}") };
        self.menu = Some(ContextMenuOverlay::new(ContextMenuSpec { title, items }));
        true
    }

    /// The menu's outcome: a choice adds its episodes.
    fn outcome(&mut self, ctx: &mut UiCtx<'_>, o: OverlayOutcome) -> OverlayOutcome {
        match o {
            OverlayOutcome::CloseWith(OverlayResult::Chosen(Action::Digit(k))) => {
                if let Some(n) = self.counts.get(usize::from(k)) {
                    let targets = self.episodes[..*n]
                        .iter()
                        .map(|e| AddTarget {
                            item: ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() },
                            programme: None,
                            from_start: false,
                            any_copy: true,
                        })
                        .collect();
                    let at = if self.play_next { Insert::Next } else { Insert::End };
                    send(ctx, QueueCommand::Add { targets, at, by: AddedBy::App });
                }
                OverlayOutcome::Close
            }
            OverlayOutcome::CloseWith(_) => OverlayOutcome::Close,
            o => o,
        }
    }
}

impl Overlay for SeriesPicker {
    fn key_contexts(&self) -> Vec<KeyContext> {
        self.menu.as_ref().map_or_else(|| vec![KeyContext::Modal], |m| m.key_contexts())
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        if !self.ready(ctx) {
            return OverlayOutcome::Close;
        }
        let o = self.menu.as_mut().map_or(OverlayOutcome::Close, |m| m.on_action(ctx, action));
        self.outcome(ctx, o)
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if !self.ready(ctx) {
            return OverlayOutcome::Close;
        }
        let o = self.menu.as_mut().map_or(OverlayOutcome::Close, |m| m.ui(egui_ctx, ctx));
        self.outcome(ctx, o)
    }
    fn anchor_at(&mut self, pos: egui::Pos2) {
        if let Some(m) = &mut self.menu {
            m.anchor_at(pos);
        }
    }
}

// ------------------------------------------------------------------ mini player chip

/// The Up next chip over the mini player's video ("Next in 8 s ✕"; `VideoHost::mini_player`
/// calls it); its ✕ cancels.
pub fn mini_chip(ui: &egui::Ui, theme: &crate::theme::Theme, queue: &QueueSnapshot, video: Rect) {
    if !crate::queue::ENABLED {
        return;
    }
    let now = crate::model::now();
    let Some(c) = card::Card::of(queue, now) else { return };
    let Some(text) = c.short(now) else { return };
    let t = &theme.tokens;
    let font = FontId::proportional(theme.font(TypeToken::BodySmall));
    let pad = theme.space(SpaceToken::Md);
    let p = ui.painter();
    let g = p.layout_no_wrap(text.clone(), font.clone(), t.osd_text);
    let h = (font.size * 2.0).round();
    let w = (g.size().x + 2.0 * pad + h).min(video.width() - 2.0 * pad);
    let r = Rect::from_min_size(egui::pos2(video.right() - pad - w, video.bottom() - pad - h), egui::vec2(w, h));
    let close = Rect::from_min_size(egui::pos2(r.right() - h, r.top()), egui::vec2(h, h));
    p.rect_filled(r, t.rounding, t.osd_scrim);
    crate::ui::video_host::osd_text(
        p,
        egui::pos2(r.left() + pad, r.center().y),
        Align2::LEFT_CENTER,
        &text,
        font.clone(),
        t.osd_text,
        close.left() - r.left() - pad,
    );
    let x = ui.interact(close, ui.id().with("queue-chip-cancel"), egui::Sense::click()).on_hover_text("Cancel (Esc in the player)");
    x.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Cancel Up next"));
    p.text(close.center(), Align2::CENTER_CENTER, glyphs::CLOSE, font, if x.hovered() { t.osd_accent } else { with_alpha(t.osd_text, 0.8) });
    if x.clicked()
        && let Some(h) = remembered()
    {
        h.send(QueueCommand::Cancel);
    }
    if c.counts_down() {
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

/// The sleep timer as the list marks it, when one is set.
pub fn sleep_mark(ctx: &UiCtx<'_>) -> Option<list::SleepMark> {
    use crate::reminders::sleep::SleepWhen;
    let st = ctx.services.agent.sleep()?.status()?;
    match st.when {
        SleepWhen::At(t) => Some(list::SleepMark::At(t)),
        // a film's end (when known) or a live programme's: the rows starting after it are late
        SleepWhen::EndOfProgramme => st.fires_at.map(list::SleepMark::At),
        SleepWhen::AfterItems(n) => Some(list::SleepMark::Items(st.items_left.unwrap_or(n))),
        SleepWhen::EndOfQueue => Some(list::SleepMark::End),
    }
}

/// Rows that are still to play (current first), for the panel's footer and Home's tile.
pub fn to_go(s: &QueueSnapshot) -> impl Iterator<Item = &crate::queue::QueueRow> {
    s.rows.iter().filter(|r| matches!(r.entry.state, RowState::Current | RowState::Pending))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{ItemKey, SourceId};
    use crate::queue::QueueRow;

    pub(crate) fn entry(id: i64, state: RowState) -> QueueEntry {
        QueueEntry {
            id: QueueId(id),
            item: ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey(format!("m{id}")) },
            any_copy: true,
            programme: None,
            title: format!("Movie {id}"),
            subtitle: None,
            duration_s: Some(3600.0),
            from_start: false,
            state,
            by_agent: false,
            orphaned: false,
            watchlist_item_id: None,
            list_context: false,
        }
    }

    /// The store's move (`store::queue::move_rows`) over the whole list, concealed rows included.
    fn store_move(all: &mut Vec<i64>, ids: &[QueueId], to: Insert) {
        let moving: Vec<i64> = all.iter().copied().filter(|id| ids.contains(&QueueId(*id))).collect();
        all.retain(|id| !ids.contains(&QueueId(*id)));
        let at = match to {
            Insert::End => all.len(),
            Insert::Index(i) => i.min(all.len()),
            Insert::Next => unreachable!(),
        };
        all.splice(at..at, moving);
    }

    /// Alt+↑/↓ and a drop land exactly where they were aimed among the visible rows, even with
    /// rows parental control conceals in between (the store's index counts those too).
    #[test]
    fn a_move_lands_before_its_visible_target_whatever_is_concealed() {
        // 1 and 2 are concealed (explicit while hidden): the snapshot has 3, 4, 5
        let full = vec![1, 2, 3, 4, 5];
        let visible = |all: &[i64]| all.iter().copied().filter(|id| *id >= 3).collect::<Vec<_>>();
        let snap = QueueSnapshot {
            rows: [3, 4, 5].map(|id| QueueRow { entry: entry(id, RowState::Pending), locked: false, eta: None, note: None }).to_vec().into(),
            ..Default::default()
        };
        for (moving, before, want) in
            [(5, Some(4), vec![3, 5, 4]), (3, Some(5), vec![4, 3, 5]), (4, None, vec![3, 5, 4]), (5, Some(3), vec![5, 3, 4])]
        {
            let mut all = full.clone();
            for c in move_before(&snap, &[QueueId(moving)], before.map(QueueId)) {
                let QueueCommand::Move { ids, to } = c else { panic!("{c:?}") };
                store_move(&mut all, &ids, to);
            }
            assert_eq!(visible(&all), want, "move {moving} before {before:?}");
            assert_eq!(all.len(), 5, "nothing lost");
        }
    }

    /// The next-episode card of a series watched part-way: a later episode's title is guarded
    /// (the number shows), the next-up one shows as is.
    #[test]
    fn the_next_episode_card_is_spoiler_guarded() {
        use crate::spoiler::snapshot::{GuardSnapshot, SeriesMark, Settings};
        let mut series = std::collections::HashMap::new();
        series.insert(
            crate::spoiler::series_key("Queue Guard Show"),
            SeriesMark { label: "Queue Guard Show".into(), next_up: (2, 4), automatic: true, ..Default::default() },
        );
        let guard = crate::guard::Guard::with_snapshot(GuardSnapshot::from_parts(Settings::default(), Vec::new(), series, Default::default()));
        let sp = crate::ui::widgets::spoiler::Spoilers::with_guard(guard, 0, true);
        let next = |episode: u32, title: &str| crate::queue::NextEpisode {
            id: crate::model::EpisodeId(i64::from(episode)),
            label: format!("S02E{episode:02} · {title}"),
            series: "Queue Guard Show".into(),
            season: 2,
            episode,
        };
        let raw = QueueSnapshot { next_episode: Some(next(6, "Who Dies")), ..Default::default() };
        let label = guard_snapshot(&sp, &raw).next_episode.unwrap().label;
        assert!(label.starts_with("S02E06 · ") && !label.contains("Who Dies"), "{label}");
        let raw = QueueSnapshot { next_episode: Some(next(4, "Fine")), ..Default::default() };
        assert_eq!(guard_snapshot(&sp, &raw).next_episode.unwrap().label, "S02E04 · Fine");
    }

    #[test]
    fn the_summary_says_how_many_are_left_and_when_they_end() {
        let rows: Vec<QueueRow> = [(1, RowState::Played), (2, RowState::Current), (3, RowState::Pending)]
            .map(|(id, s)| QueueRow { entry: entry(id, s), locked: false, eta: None, note: None })
            .to_vec();
        let mut s = QueueSnapshot { rows: rows.into(), eta_end: Some((7200, true)), ..Default::default() };
        // local time formatting depends on the zone: compare the shape only
        let text = summary(&s, 0, true).unwrap();
        assert!(text.starts_with("2 to go · 2 h 00 · ends ~"), "{text}");
        s.eta_end = Some((7200, false));
        assert!(summary(&s, 0, true).unwrap().contains("ends ≥ "));
        s.rows = Arc::from(Vec::new());
        assert_eq!(summary(&s, 0, true), None);
    }
}
