//! Live TV, TV guide and player UI tests (DESIGN §13.2, §13.4, §13.8; WP-11 acceptance).
//! Owner: WP-11.
//!
//! * Grid / focus-model tests use a fake `EpgReader` (no database).
//! * Screen and overlay tests use `TestUi` (real services in a temp profile, mpv `vo=null`)
//!   with seeded data (`tests/support/seed.rs`). Player commands go to the real controller,
//!   so their effect is observed on the player's snapshot; the snapshot the SCREEN sees
//!   (`TestUi::snapshot`) can be replaced to simulate playback states.
//! * Layout tests render screens in `egui_kittest` harnesses and query accesskit labels.
//!
//! One `TestUi` (mpv + runtime + store) at a time keeps the test binary's memory flat.

mod support;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Rect, Shape};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use support::seed;
use tuner::keymap::{self, Action, KeyContext};
use tuner::model::{
    ChannelId, ChannelSummary, Collection, EpgChannelId, GroupId, ItemKey, ItemKind, ItemRef, MediaRef, NowNext, PlayOrigin, Playable, PlayableKind, ProgrammeFlags, ProgrammeId,
    ProgrammeSlim, SourceId, Timestamp, ZapList,
};
use tuner::player::{PlayerSnapshot, PlayerState};
use tuner::store::library::LiveIndex;
use tuner::theme::Theme;
use tuner::ui::data::{EpgReader, FrameBudget};
use tuner::ui::epg_grid::{self, EpgGridState, GridMetrics, GridOutcome, GuideRow, ProgrammeCache};
use tuner::ui::overlays::{self, Overlay, OverlayOutcome};
use tuner::ui::screens::{guide::GuideScreen, live::LiveScreen, player::PlayerScreen};
use tuner::ui::testing::TestUi;
use tuner::ui::video_host::VideoHost;
use tuner::ui::{LayoutClass, NavRequest, OverlayRequest, Route, Screen};

// ====================================================================== helpers

static ONE_UI: Mutex<()> = Mutex::new(());

fn one() -> MutexGuard<'static, ()> { ONE_UI.lock().unwrap_or_else(|e| e.into_inner()) }

/// A multiple of 30 minutes near "now" so programmes line up with the ruler.
fn slot(now: Timestamp) -> Timestamp { now - now.rem_euclid(1800) }

/// `TestUi` with one source, `n` channels ("Channel i", number i+1) and a guide from 3 h ago
/// for 8 h for the first `epg` channels. Returns the channel ids.
fn seeded(n: usize, epg: usize, cfg: Option<tuner::config::Config>) -> (TestUi, Vec<ChannelId>) {
    let mut t = match cfg {
        Some(c) => TestUi::with_config(c),
        None => TestUi::new(),
    }
    .expect("TestUi");
    let start = slot(t.now) - 3 * 3600;
    let ids = t
        .services()
        .store
        .writer()
        .blocking(move |c| {
            let src = seed::seed_source(c, "Seed")?;
            let ids = seed::seed_channels(c, src, n)?;
            if epg > 0 {
                seed::seed_epg(c, &ids[..epg], start, 8)?;
            }
            Ok(ids)
        })
        .expect("seed");
    reload(&mut t);
    (t, ids)
}

/// Load the library from the profile like the RebuildLibrary job.
fn reload(t: &mut TestUi) {
    let lib = tuner::store::library::load(&t.db).expect("library");
    t.set_library(lib);
}

/// Run one frame of a screen headlessly.
fn draw(t: &mut TestUi, screen: &mut dyn Screen) {
    let egui = t.egui.clone();
    let mut out = egui.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0))), ..Default::default() }, |ui| {
        t.with_ctx(|ctx| screen.ui(ui, ctx))
    });
    out.textures_delta.clear();
}

fn act(t: &mut TestUi, screen: &mut dyn Screen, a: Action) -> bool { t.with_ctx(|ctx| screen.on_action(ctx, a)) }

/// Wait for the REAL player (commands sent by the UI) to satisfy `f`.
fn player_until(t: &TestUi, what: &str, f: impl Fn(&PlayerSnapshot) -> bool) -> Arc<PlayerSnapshot> {
    let h = t.services().player.handle();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = h.snapshot();
        if f(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what} (last: {:?} {:?})", s.state, s.playable.as_ref().map(|p| &p.media));
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The real player received nothing for `ms`.
fn player_stays_idle(t: &TestUi, ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
    let s = t.services().player.handle().snapshot();
    assert!(s.playable.is_none(), "nothing may play: {:?}", s.playable.as_ref().map(|p| &p.media));
}

fn plays(media: MediaRef) -> impl Fn(&PlayerSnapshot) -> bool { move |s| s.playable.as_ref().is_some_and(|p| p.media == media) }

/// A snapshot as the screens would see it while `media` plays.
fn fake_playing(media: MediaRef, kind: PlayableKind, zap: Option<ZapList>, origin: PlayOrigin) -> Arc<PlayerSnapshot> {
    Arc::new(PlayerSnapshot {
        state: PlayerState::Playing,
        playable: Some(Arc::new(Playable {
            media,
            kind,
            item: None,
            source_id: Some(SourceId(1)),
            title: "Channel 0".into(),
            subtitle: None,
            logo: None,
            number: Some(1),
            url: tuner::redact::Secret::new("http://127.0.0.1/x".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: Some(3600.0),
        })),
        zap,
        origin: Some(origin),
        position: Some(10.0),
        duration: Some(3600.0),
        video_size: Some((1920, 1080)),
        ..Default::default()
    })
}

/// Some accesskit node has this label (several may: a row and the preview's logo).
fn has<S>(h: &Harness<'_, S>, label: &str) -> bool { h.query_all_by_label(label).next().is_some() }

fn pushed(t: &TestUi, r: &Route) -> bool { t.nav.iter().any(|n| matches!(n, NavRequest::Push(x) if x == r)) }

fn opened(t: &TestUi) -> Vec<OverlayRequest> {
    t.nav
        .iter()
        .filter_map(|n| match n {
            NavRequest::Open(o) => Some(o.clone()),
            _ => None,
        })
        .collect()
}

/// Every shape of the last frame, `Shape::Vec`s flattened.
fn shapes<S>(h: &Harness<'_, S>) -> Vec<Shape> {
    fn flat(s: &Shape, out: &mut Vec<Shape>) {
        match s {
            Shape::Vec(v) => v.iter().for_each(|s| flat(s, out)),
            s => out.push(s.clone()),
        }
    }
    let mut out = Vec::new();
    for c in &h.output().shapes {
        flat(&c.shape, &mut out);
    }
    out
}

fn texts(shapes: &[Shape]) -> Vec<(String, Color32, Rect)> {
    shapes
        .iter()
        .filter_map(|s| match s {
            Shape::Text(t) => Some((t.galley.job.text.clone(), t.galley.job.sections.first().map_or(t.fallback_color, |s| s.format.color), t.visual_bounding_rect())),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- fake EPG

const T0: Timestamp = 1_791_576_000; // a multiple of 30 minutes

/// 30-minute programmes everywhere (ids encode channel and slot); counts window calls.
#[derive(Default)]
struct FakeEpg {
    calls: u32,
}

impl EpgReader for FakeEpg {
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> tuner::error::Result<Vec<ProgrammeSlim>> {
        self.calls += 1;
        let mut out = Vec::new();
        let mut s = from - from.rem_euclid(1800);
        while s < to {
            let slot = (s - T0).div_euclid(1800);
            out.push(ProgrammeSlim { id: ProgrammeId(ch.0 * 100_000 + slot + 50_000), start: s, stop: s + 1800, title: format!("Show {slot}").into(), sub_title: None, flags: ProgrammeFlags::default() });
            s += 1800;
        }
        Ok(out)
    }
    fn now_next(&mut self, _: &[(EpgChannelId, i32)], _: Timestamp) -> tuner::error::Result<HashMap<(EpgChannelId, i32), NowNext>> { Ok(HashMap::new()) }
}

fn summary(i: i64) -> ChannelSummary {
    ChannelSummary {
        id: ChannelId(i),
        source_id: SourceId(1),
        key: ItemKey(format!("k{i}")),
        name: format!("Channel {i}").into(),
        number: Some(i as u32),
        logo: None,
        group_ids: vec![GroupId(1)],
        epg_channel_id: Some(EpgChannelId(i)),
        tvg_shift_min: 0,
        has_catchup: true,
        catchup_days: 7,
        radio: false,
        locked: false,
        unsupported: false,
        provider_order: i as i32,
        user_order: None,
        added_at: None,
    }
}

fn live_index(n: i64) -> LiveIndex {
    let chans: Vec<ChannelSummary> = (1..=n).map(summary).collect();
    let members = HashMap::from([(GroupId(1), chans.iter().map(|c| c.id).collect())]);
    LiveIndex::from_parts(chans, vec![], members, HashSet::new())
}

fn guide_rows(n: i64) -> Arc<[GuideRow]> { (1..=n).map(|i| GuideRow { channel: ChannelId(i), epg: Some(EpgChannelId(i)), tvg_shift_min: 0, catchup_days: 7 }).collect() }

fn big_budget() -> FrameBudget { FrameBudget::new(Duration::from_secs(10)) }

// ====================================================================== EPG grid

#[test]
fn guide_focus_model_keeps_focus_time_across_rows() {
    let now = T0 + 600;
    let mut g = EpgGridState::new(guide_rows(20), now);
    let mut r = FakeEpg::default();
    let mut b = big_budget();
    g.handle(Action::GuideNow, &mut r, &mut b, now);
    assert_eq!(g.focused().unwrap().1.unwrap().start, T0, "now: the airing programme");
    // → → : T = start of the focused programme
    g.handle(Action::Right, &mut r, &mut b, now);
    g.handle(Action::Right, &mut r, &mut b, now);
    assert_eq!(g.focus_time, T0 + 3600);
    // ↓ keeps T on the next channel
    assert_eq!(g.handle(Action::Down, &mut r, &mut b, now), GridOutcome::FocusChanged);
    let (ch, p) = g.focused().unwrap();
    assert_eq!((ch, p.unwrap().start), (ChannelId(2), T0 + 3600));
    // ← back; T follows the programme start
    g.handle(Action::Left, &mut r, &mut b, now);
    assert_eq!(g.focus_time, T0 + 1800);
    // [ ] ± 24 h, Shift+←/→ ± 3 h, n = now
    let v = g.view_start;
    g.handle(Action::GuideNextDay, &mut r, &mut b, now);
    assert_eq!(g.view_start, v + 86_400);
    g.handle(Action::GuideBackHours, &mut r, &mut b, now);
    assert_eq!(g.view_start, v + 86_400 - 3 * 3600);
    g.handle(Action::GuideNow, &mut r, &mut b, now);
    assert_eq!(g.focus_time, now);
    // g / G = first / last channel
    g.handle(Action::End, &mut r, &mut b, now);
    assert_eq!(g.focused().unwrap().0, ChannelId(20));
    g.handle(Action::Home, &mut r, &mut b, now);
    assert_eq!(g.focused().unwrap().0, ChannelId(1));
    // PgDn moves by the visible rows of the last frame (default 10 before a paint)
    g.handle(Action::PageDown, &mut r, &mut b, now);
    assert_eq!(g.focused().unwrap().0, ChannelId(11));
}

#[test]
fn guide_actions_open_watch_catch_up_and_remind() {
    let now = T0 + 600;
    let mut g = EpgGridState::new(guide_rows(3), now);
    let mut r = FakeEpg::default();
    let mut b = big_budget();
    g.handle(Action::GuideNow, &mut r, &mut b, now);
    assert!(matches!(g.handle(Action::Activate, &mut r, &mut b, now), GridOutcome::Open { channel: ChannelId(1), .. }));
    assert_eq!(g.handle(Action::GuideWatch, &mut r, &mut b, now), GridOutcome::Watch(ChannelId(1)));
    assert_eq!(g.handle(Action::GuideCatchup, &mut r, &mut b, now), GridOutcome::None, "airing: no catch-up");
    g.handle(Action::Left, &mut r, &mut b, now);
    assert!(matches!(g.handle(Action::GuideCatchup, &mut r, &mut b, now), GridOutcome::Catchup { channel: ChannelId(1), .. }));
    assert_eq!(g.handle(Action::GuideRemind, &mut r, &mut b, now), GridOutcome::None, "past: no reminder");
    g.handle(Action::Right, &mut r, &mut b, now);
    g.handle(Action::Right, &mut r, &mut b, now);
    assert!(matches!(g.handle(Action::GuideRemind, &mut r, &mut b, now), GridOutcome::Remind { channel: ChannelId(1), .. }));
}

#[test]
fn programme_cache_budgets_bucket_loads_per_frame() {
    let mut c = ProgrammeCache::default();
    let mut r = FakeEpg::default();
    let mut b = big_budget();
    let mut missing = 0;
    for ch in 1..=40 {
        if c.window(&mut r, &mut b, EpgChannelId(ch), 0, T0, T0 + 3600).is_none() {
            missing += 1;
        }
    }
    assert_eq!(r.calls, ProgrammeCache::LOADS_PER_FRAME);
    assert_eq!(missing, 40 - ProgrammeCache::LOADS_PER_FRAME);
    c.begin_frame();
    for ch in 1..=40 {
        assert!(c.window(&mut r, &mut b, EpgChannelId(ch), 0, T0, T0 + 3600).is_some());
    }
    // tvg-shift applies after the cache: +60 shows the 20:00 programme at 21:00
    let shifted = c.window(&mut r, &mut b, EpgChannelId(1), 60, T0 + 3600, T0 + 5400).unwrap();
    assert_eq!(shifted[0].start, T0 + 3600);
    assert_eq!(shifted[0].title.as_ref(), "Show 0");
    // a spent 2 ms frame budget defers the rest to the next frame
    let mut none = FrameBudget::new(Duration::ZERO);
    c.begin_frame();
    assert!(c.window(&mut r, &mut none, EpgChannelId(99), 0, T0, T0 + 3600).is_none());
    assert!(none.deferred(), "the app requests a repaint");
}

struct GridApp {
    theme: Theme,
    live: LiveIndex,
    state: EpgGridState,
    reader: FakeEpg,
    images: tuner::images::ImageCache,
    now: Timestamp,
    outcomes: Vec<GridOutcome>,
    /// Window reads per grid paint.
    loads: Vec<u32>,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

fn grid_harness(n: i64, now: Timestamp, budget: Duration) -> Harness<'static, GridApp> {
    let dir = tempfile::tempdir().unwrap();
    let paths = tuner::paths::AppPaths::rooted(dir.path());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let http = tuner::sources::http::HttpClient::new(&Default::default()).unwrap();
    let ctx = egui::Context::default();
    let images = tuner::images::ImageCache::new(&paths, http, rt.handle().clone(), ctx, 1 << 20);
    let app = GridApp { theme: Theme::fallback(), live: live_index(n), state: EpgGridState::new(guide_rows(n), now), reader: FakeEpg::default(), images, now, outcomes: Vec::new(), loads: Vec::new(), _rt: rt, _dir: dir };
    Harness::builder().with_size(egui::vec2(1200.0, 700.0)).build_ui_state(
        move |ui, a: &mut GridApp| {
            let mut b = FrameBudget::new(budget);
            let m = GridMetrics::for_layout(LayoutClass::Wide, false, &a.theme);
            let before = a.reader.calls;
            let out = epg_grid::epg_grid(ui, &a.theme, &mut a.images, &mut a.reader, &mut b, &a.live, &mut a.state, m, a.now);
            a.loads.push(a.reader.calls - before);
            if out != GridOutcome::None {
                a.outcomes.push(out);
            }
        },
        app,
    )
}

#[test]
fn grid_paints_the_now_line_and_virtualizes_rows() {
    let now = T0 + 900;
    let mut h = grid_harness(5000, now, Duration::from_secs(10));
    h.step();
    h.step();
    let theme = Theme::fallback();
    let s = shapes(&h);
    let now_line = s.iter().any(|s| matches!(s, Shape::LineSegment { stroke, .. } if stroke.width == 2.0 && stroke.color == theme.tokens.accent));
    assert!(now_line, "2 px accent now-line");
    // only the visible rows (± 2 prefetched) are queried: 5 000 channels cost ~20 rows
    let calls = h.state().reader.calls;
    assert!(calls > 0 && calls < 200, "virtualized: {calls} window reads");
    let t = texts(&s);
    assert!(t.iter().any(|(x, _, _)| x == "Show 0"), "the airing programme is painted");
    assert!(!t.iter().any(|(x, _, _)| x == "Channel 400"), "far rows aren't painted");
    // the focused cell uses the list-cursor colours
    let focus = t.iter().find(|(x, _, r)| x == "Show 0" && r.top() < 120.0).expect("focused title on the first row");
    assert_eq!(focus.1, theme.tokens.list_cursor_text);
}

#[test]
fn grid_defers_loads_over_the_budget_and_fills_in_later() {
    let now = T0 + 900;
    // zero budget: everything deferred → skeleton cells, no programme text
    let mut h = grid_harness(30, now, Duration::ZERO);
    h.step();
    assert_eq!(h.state().reader.calls, 0);
    assert!(!texts(&shapes(&h)).iter().any(|(x, _, _)| x.starts_with("Show")));
    // with budget, the next frames load (≤ 24 bucket loads per grid paint)
    let mut h = grid_harness(30, now, Duration::from_secs(10));
    for _ in 0..5 {
        h.step();
    }
    let loads = &h.state().loads;
    assert!(loads.iter().all(|n| *n <= ProgrammeCache::LOADS_PER_FRAME), "per-paint cap: {loads:?}");
    assert!(loads.iter().filter(|n| **n > 0).count() >= 2, "the rest loads over the next frames: {loads:?}");
    assert!(texts(&shapes(&h)).iter().any(|(x, _, _)| x == "Show 0"));
}

#[test]
fn grid_mouse_click_focuses_and_header_click_watches() {
    let now = T0 + 900;
    let mut h = grid_harness(10, now, Duration::from_secs(10));
    h.step();
    h.step();
    let theme = Theme::fallback();
    let m = GridMetrics::for_layout(LayoutClass::Wide, false, &theme);
    // a cell on the third row, an hour after the view start
    let view_start = h.state().state.view_start;
    let x = m.header_width + ((T0 + 3600 + 600 - view_start) as f32 / 60.0) * m.px_per_min;
    let y = m.ruler_height + 2.5 * m.row_height;
    h.input_mut().events.push(egui::Event::PointerMoved(egui::pos2(x, y)));
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton { pos: egui::pos2(x, y), button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton { pos: egui::pos2(x, y), button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.step();
    let (ch, p) = h.state().state.focused().map(|(c, p)| (c, p.cloned())).unwrap();
    assert_eq!(ch, ChannelId(3));
    assert_eq!(p.unwrap().start, T0 + 3600);
    // click on the channel column = watch
    let hy = m.ruler_height + 1.5 * m.row_height;
    h.input_mut().events.push(egui::Event::PointerMoved(egui::pos2(20.0, hy)));
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton { pos: egui::pos2(20.0, hy), button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.input_mut().events.push(egui::Event::PointerButton { pos: egui::pos2(20.0, hy), button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.step();
    assert!(h.state().outcomes.contains(&GridOutcome::Watch(ChannelId(2))), "{:?}", h.state().outcomes);
    // …and focuses that row first: a locked channel's PIN retry (`GuideWatch`) watches the focus
    assert_eq!(h.state().state.focused().map(|f| f.0), Some(ChannelId(2)));
}

// ====================================================================== guide screen

#[test]
fn guide_digits_jump_to_a_channel_number_and_watch_plays_it_fullscreen() {
    let _one = one();
    let (mut t, ids) = seeded(30, 30, None);
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    draw(&mut t, &mut g);
    assert_eq!(g.key_contexts(), [KeyContext::Guide]);
    assert!(act(&mut t, &mut g, Action::Digit(1)));
    assert_eq!(g.key_contexts(), [KeyContext::NumericEntry, KeyContext::Guide], "digits open numeric entry");
    act(&mut t, &mut g, Action::Digit(2));
    act(&mut t, &mut g, Action::Activate);
    assert_eq!(g.key_contexts(), [KeyContext::Guide]);
    draw(&mut t, &mut g);
    // the focus is on channel 12 (number 12 = ids[11]): Watch plays it fullscreen
    assert!(act(&mut t, &mut g, Action::GuideWatch));
    assert!(pushed(&t, &Route::Player), "the guide's Watch is fullscreen");
    let s = player_until(&t, "channel 12 plays", plays(MediaRef::Channel(ids[11])));
    assert_eq!(s.origin, Some(PlayOrigin::Guide));
    assert_eq!(s.zap.as_ref().map(|z| z.collection.clone()), Some(Collection::AllChannels), "zaps through the guide's collection");
    // an unknown number toasts
    t.toasts.clear();
    act(&mut t, &mut g, Action::Digit(9));
    act(&mut t, &mut g, Action::Digit(9));
    act(&mut t, &mut g, Action::Activate);
    assert!(t.toasts.iter().any(|x| x.text.contains("No channel 99")));
}

#[test]
fn guide_defaults_to_the_first_non_empty_favourites_list() {
    let _one = one();
    let (mut t, ids) = seeded(10, 10, None);
    let fav = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 6)) };
    t.services()
        .store
        .writer()
        .blocking(move |c| {
            let list = tuner::store::user::default_fav_list(c)?;
            tuner::store::user::set_favourite(c, list, &fav, true, 0)
        })
        .unwrap();
    reload(&mut t);
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    draw(&mut t, &mut g);
    // the only row is the favourite: Watch plays it
    act(&mut t, &mut g, Action::GuideWatch);
    let s = player_until(&t, "the favourite plays", plays(MediaRef::Channel(ids[6])));
    assert!(matches!(s.zap.as_ref().map(|z| &z.collection), Some(Collection::Favourites(_))));
    // `f` opens the collection picker (a launcher menu)
    act(&mut t, &mut g, Action::GuideCategory);
    let menus = opened(&t);
    let Some(OverlayRequest::ContextMenu(spec)) = menus.last() else { panic!("picker: {menus:?}") };
    let labels: Vec<&str> = spec.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"Favourites") && labels.contains(&"All channels") && labels.contains(&"News"), "{labels:?}");
}

/// `snap` after a newer load (the player bumps the generation on every load, zaps included).
fn reloaded(snap: Arc<PlayerSnapshot>, generation: u64) -> Arc<PlayerSnapshot> { Arc::new(PlayerSnapshot { load_generation: generation, ..(*snap).clone() }) }

/// The collection the picker marks as current.
fn picked_collection(t: &mut TestUi, g: &mut GuideScreen) -> String {
    t.nav.clear();
    act(t, g, Action::GuideCategory);
    let menus = opened(t);
    let Some(OverlayRequest::ContextMenu(spec)) = menus.last() else { panic!("picker: {menus:?}") };
    spec.items.iter().find(|i| i.icon == tuner::ui::widgets::glyphs::CHECK).map(|i| i.label.clone()).unwrap_or_default()
}

#[test]
fn guide_returns_focus_to_the_channel_just_watched() {
    let _one = one();
    let (mut t, ids) = seeded(20, 20, None);
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    draw(&mut t, &mut g);
    act(&mut t, &mut g, Action::GuideWatch);
    // the user zapped to channel 15 in the player, then came back
    t.snapshot = reloaded(fake_playing(MediaRef::Channel(ids[14]), PlayableKind::Live, None, PlayOrigin::Guide), 3);
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    t.nav.clear();
    // the focus is the playing channel: Watch (w) on it opens the player without a new play
    act(&mut t, &mut g, Action::GuideWatch);
    assert!(pushed(&t, &Route::Player), "the focused channel is the playing one");
}

/// Ctrl+G from Live enters with a focus; after Watch and zaps, coming back follows the zap
/// (not the route's focus) and keeps the collection picked with `f` (DESIGN §13.0, §13.4).
#[test]
fn guide_return_beats_the_route_focus_and_keeps_the_picked_collection() {
    let _one = one();
    let (mut t, ids) = seeded(20, 20, None);
    let news = t.lib.live.groups.iter().find(|g| g.name == "News").map(|g| g.id).expect("News");
    let route = Route::Guide { collection: Some(Collection::Group(news)), focus: Some(ids[10]) };
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &route));
    draw(&mut t, &mut g);
    assert_eq!(picked_collection(&mut t, &mut g), "News");
    // `f` → All channels
    let all = {
        let Some(OverlayRequest::ContextMenu(spec)) = opened(&t).pop() else { panic!() };
        spec.items.iter().find(|i| i.label == "All channels").map(|i| i.action).unwrap()
    };
    t.with_ctx(|ctx| g.on_overlay_result(ctx, tuner::ui::overlays::OverlayResult::Chosen(all)));
    draw(&mut t, &mut g);
    act(&mut t, &mut g, Action::GuideWatch);
    // zapped to channel 5 in the player, then Esc back to the same route
    t.snapshot = reloaded(fake_playing(MediaRef::Channel(ids[4]), PlayableKind::Live, None, PlayOrigin::Guide), 5);
    t.with_ctx(|ctx| g.on_enter(ctx, &route));
    draw(&mut t, &mut g);
    t.nav.clear();
    act(&mut t, &mut g, Action::GuideWatch);
    assert!(pushed(&t, &Route::Player), "the focus followed the zap, not the route's focus");
    assert_eq!(picked_collection(&mut t, &mut g), "All channels", "the picked collection stays");

    // the popover's Watch (the usual Enter path) counts as a guide launch too
    t.snapshot = Arc::new(PlayerSnapshot::default());
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: Some(ids[2]) }));
    draw(&mut t, &mut g);
    t.nav.clear();
    act(&mut t, &mut g, Action::Activate);
    assert!(matches!(opened(&t).last(), Some(OverlayRequest::Programme { channel, .. }) if *channel == ids[2]));
    t.snapshot = reloaded(fake_playing(MediaRef::Channel(ids[7]), PlayableKind::Live, None, PlayOrigin::Guide), 9);
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: Some(ids[2]) }));
    t.nav.clear();
    act(&mut t, &mut g, Action::GuideWatch);
    assert!(pushed(&t, &Route::Player), "popover Watch → zap → back follows the zap");

    // a fresh Ctrl+G (another route) focuses what it asks for, even after a newer load
    t.snapshot = reloaded(t.snapshot.clone(), 11);
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: Some(ids[3]) }));
    act(&mut t, &mut g, Action::GuideWatch);
    player_until(&t, "channel 4 (the route's focus) plays", plays(MediaRef::Channel(ids[3])));
}

#[test]
fn guide_remind_toggles_a_reminder() {
    let _one = one();
    let (mut t, _ids) = seeded(5, 5, None);
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    draw(&mut t, &mut g);
    act(&mut t, &mut g, Action::Right);
    act(&mut t, &mut g, Action::Right);
    act(&mut t, &mut g, Action::GuideRemind);
    let count = |t: &TestUi| t.db.query_row("SELECT count(*) FROM reminder", [], |r| r.get::<_, i64>(0)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&t) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(count(&t), 1);
    assert!(t.toasts.iter().any(|x| x.text.starts_with("Reminder set")));
    // the Reminders scope reloads the set; r again cancels
    t.with_ctx(|ctx| g.on_event(ctx, &tuner::msg::AppEvent::Data(tuner::msg::DataScope::Reminders)));
    draw(&mut t, &mut g);
    act(&mut t, &mut g, Action::GuideRemind);
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&t) == 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(count(&t), 0);
}

#[test]
fn guide_enter_opens_the_programme_popover() {
    let _one = one();
    let (mut t, ids) = seeded(5, 5, None);
    let mut g = GuideScreen::default();
    t.with_ctx(|ctx| g.on_enter(ctx, &Route::Guide { collection: None, focus: None }));
    draw(&mut t, &mut g);
    act(&mut t, &mut g, Action::Activate);
    let o = opened(&t);
    assert!(matches!(o.last(), Some(OverlayRequest::Programme { channel, .. }) if *channel == ids[0]), "{o:?}");
}

// ====================================================================== programme popover

fn programme_at(t: &TestUi, ch: ChannelId, at: Timestamp) -> tuner::model::ProgrammeId {
    t.db.query_row(
        "SELECT p.id FROM programme p JOIN channel_epg ce ON ce.epg_channel_id = p.epg_channel_id WHERE ce.channel_id = ?1 AND p.start <= ?2 AND p.stop > ?2",
        rusqlite::params![ch.0, at],
        |r| r.get(0),
    )
    .unwrap()
}

fn popover(t: &mut TestUi, programme: ProgrammeId, channel: ChannelId) -> Box<dyn Overlay> {
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::Programme { programme, channel }, ctx));
    let egui = t.egui.clone();
    let mut out = egui.run_ui(Default::default(), |_| {
        t.with_ctx(|ctx| {
            let _ = o.ui(&egui, ctx);
        })
    });
    out.textures_delta.clear();
    o
}

#[test]
fn programme_popover_watch_catch_up_and_remind() {
    let _one = one();
    let (mut t, ids) = seeded(3, 3, None);
    // channel 0 has a 7-day archive
    t.services()
        .store
        .writer()
        .blocking(|c| Ok(c.execute("UPDATE channel SET catchup = '{\"kind\":\"shift\",\"days\":7,\"correction_s\":0}' WHERE number = 1", [])?))
        .unwrap();
    reload(&mut t);
    let now = t.now;
    // airing → Watch (first action) plays fullscreen
    let p = programme_at(&t, ids[0], now);
    let mut o = popover(&mut t, p, ids[0]);
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Activate)), OverlayOutcome::Close));
    assert!(pushed(&t, &Route::Player));
    player_until(&t, "watch", plays(MediaRef::Channel(ids[0])));
    // past → Catch up
    t.nav.clear();
    let past = programme_at(&t, ids[0], now - 3600);
    let mut o = popover(&mut t, past, ids[0]);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate));
    player_until(&t, "catch-up", |s| matches!(s.playable.as_ref().map(|p| &p.media), Some(MediaRef::Catchup { channel, .. }) if *channel == ids[0]));
    assert!(pushed(&t, &Route::Player));
    // future → Remind me, then Cancel reminder
    let future = programme_at(&t, ids[1], now + 2 * 3600);
    let mut o = popover(&mut t, future, ids[1]);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate));
    let count = |t: &TestUi| t.db.query_row("SELECT count(*) FROM reminder", [], |r| r.get::<_, i64>(0)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&t) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(count(&t), 1, "reminder set");
    let mut o = popover(&mut t, future, ids[1]);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate));
    let deadline = Instant::now() + Duration::from_secs(5);
    while count(&t) == 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(count(&t), 0, "reminder cancelled");
    // a vanished programme still offers Watch + Assign EPG and never panics
    let mut o = popover(&mut t, ProgrammeId(987_654), ids[2]);
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Down)), OverlayOutcome::Keep));
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Back)), OverlayOutcome::Close));
}

// ====================================================================== Live TV

struct ScreenApp<S: Screen> {
    t: TestUi,
    s: S,
}

fn screen_harness<S: Screen + 'static>(t: TestUi, s: S, w: f32) -> Harness<'static, ScreenApp<S>> {
    Harness::builder().with_size(egui::vec2(w, 800.0)).build_ui_state(
        |ui, a: &mut ScreenApp<S>| {
            let s = &mut a.s;
            a.t.with_ctx(|ctx| s.ui(ui, ctx));
        },
        ScreenApp { t, s },
    )
}

#[test]
fn live_layouts_three_panes_two_panes_and_a_stack() {
    let _one = one();
    for (layout, w) in [(LayoutClass::Wide, 1600.0), (LayoutClass::Medium, 790.0), (LayoutClass::Compact, 500.0)] {
        let (mut t, _ids) = seeded(12, 12, None);
        t.layout = layout;
        let mut s = LiveScreen::default();
        t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
        let mut h = screen_harness(t, s, w);
        for _ in 0..3 {
            h.step();
        }
        let groups = has(&h, "All channels");
        let channels = has(&h, "Channel 0");
        let preview = has(&h, "Channel 0 · 1");
        match layout {
            LayoutClass::Wide => assert!(groups && channels && preview, "Wide: groups + channels + preview"),
            LayoutClass::Medium => assert!(!groups && channels && preview, "Medium: channels + preview, groups in a drawer"),
            LayoutClass::Compact => assert!(!groups && channels && !preview, "Compact: one column"),
        }
        // ← on the channel list opens the groups (drawer / stack)
        {
            let ScreenApp { t, s } = h.state_mut();
            t.with_ctx(|ctx| s.on_action(ctx, Action::Parent));
        }
        for _ in 0..2 {
            h.step();
        }
        assert!(has(&h, "All channels"), "{layout:?}: groups visible after ←");
        assert_eq!(h.state().s.key_contexts(), [KeyContext::List]);
    }
}

#[test]
fn live_enter_rule_plays_here_then_opens_the_player() {
    let _one = one();
    let (mut t, ids) = seeded(5, 5, None);
    t.layout = LayoutClass::Wide;
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    draw(&mut t, &mut s);
    // Enter plays in the preview (no navigation), as a user-started stream
    assert!(act(&mut t, &mut s, Action::Activate));
    assert!(!pushed(&t, &Route::Player));
    let snap = player_until(&t, "plays here", plays(MediaRef::Channel(ids[0])));
    assert_eq!(snap.origin, Some(PlayOrigin::Fullscreen), "explicit plays are never 'preview autoplay'");
    // Enter on the playing channel opens the fullscreen player
    t.snapshot = fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, None, PlayOrigin::Fullscreen);
    act(&mut t, &mut s, Action::Activate);
    assert!(pushed(&t, &Route::Player));
    // Shift+Enter is always fullscreen
    t.nav.clear();
    act(&mut t, &mut s, Action::Down);
    act(&mut t, &mut s, Action::ActivateAlt);
    assert!(pushed(&t, &Route::Player));
    player_until(&t, "shift+enter", plays(MediaRef::Channel(ids[1])));
    // Compact (no preview): Enter is fullscreen
    t.nav.clear();
    t.layout = LayoutClass::Compact;
    act(&mut t, &mut s, Action::Down);
    act(&mut t, &mut s, Action::Activate);
    assert!(pushed(&t, &Route::Player));
}

#[test]
fn live_preview_autoplay_is_off_by_default_and_keyboard_rest_only() {
    let _one = one();
    // default config: resting on a channel never plays
    let (mut t, _ids) = seeded(5, 0, None);
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    act(&mut t, &mut s, Action::Down);
    std::thread::sleep(Duration::from_millis(50));
    draw(&mut t, &mut s);
    player_stays_idle(&t, 300);
    drop(t);

    // opted in (short delay): the keyboard rest plays as LivePreview
    let mut cfg = tuner::config::Config::default();
    cfg.interface.live_preview_autoplay = true;
    cfg.interface.preview_delay_ms = 5;
    let (mut t, ids) = seeded(5, 0, Some(cfg.clone()));
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    draw(&mut t, &mut s);
    act(&mut t, &mut s, Action::Down);
    std::thread::sleep(Duration::from_millis(30));
    draw(&mut t, &mut s);
    let snap = player_until(&t, "autoplay", plays(MediaRef::Channel(ids[1])));
    assert_eq!(snap.origin, Some(PlayOrigin::LivePreview));
    drop(t);

    // never replaces a stream the user started
    let (mut t, ids) = seeded(5, 0, Some(cfg));
    t.snapshot = fake_playing(MediaRef::Channel(ids[3]), PlayableKind::Live, None, PlayOrigin::Fullscreen);
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    draw(&mut t, &mut s);
    act(&mut t, &mut s, Action::Down);
    std::thread::sleep(Duration::from_millis(30));
    draw(&mut t, &mut s);
    player_stays_idle(&t, 300);
}

#[test]
fn live_marks_the_playing_channel_and_follows_zaps_on_return() {
    let _one = one();
    let (mut t, ids) = seeded(8, 0, None);
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    t.snapshot = fake_playing(MediaRef::Channel(ids[5]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen);
    let mut s = LiveScreen::default();
    // returning from the player: the cursor is on the channel that was zapped to
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    t.snapshot = Arc::new(PlayerSnapshot::default());
    act(&mut t, &mut s, Action::ActivateAlt);
    player_until(&t, "cursor channel", plays(MediaRef::Channel(ids[5])));
    // the accent ▶ glyph marks the playing channel's row
    t.snapshot = fake_playing(MediaRef::Channel(ids[5]), PlayableKind::Live, None, PlayOrigin::Fullscreen);
    let theme = t.theme.clone();
    let mut h = screen_harness(t, s, 1600.0);
    for _ in 0..3 {
        h.step();
    }
    let glyph = texts(&shapes(&h)).into_iter().find(|(x, _, _)| x == tuner::ui::widgets::glyphs::PLAY).expect("now-playing glyph");
    assert_eq!(glyph.1, theme.tokens.accent);
}

#[test]
fn live_empty_states() {
    let _one = one();
    let (mut t, _ids) = seeded(3, 0, None);
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    // filter without matches
    for c in "zzz".chars() {
        act(&mut t, &mut s, Action::FilterInput(c));
    }
    assert_eq!(s.key_contexts(), [KeyContext::ListFiltering]);
    let mut h = screen_harness(t, s, 1600.0);
    h.step();
    h.step();
    assert!(has(&h, "No matches for “zzz”"));
    // an empty favourites list
    {
        let ScreenApp { t, s } = h.state_mut();
        t.with_ctx(|ctx| {
            s.on_action(ctx, Action::FilterClear);
            s.on_action(ctx, Action::Parent);
            s.on_action(ctx, Action::Home);
            s.on_action(ctx, Action::Activate);
        });
    }
    h.step();
    h.step();
    assert!(has(&h, "Press Ctrl+F on any channel to add it here"));
    drop(h);

    // everything in a group hidden: Enter is the "Show hidden" button (News = Channel 0 here)
    let (mut t, _ids) = seeded(10, 0, None);
    t.services()
        .store
        .writer()
        .blocking(|c| Ok(c.execute("INSERT INTO item_override(source_id, kind, item_key, hidden, fp) VALUES (1, 'channel', ?1, 1, '')", [format!("m3u:seed{:011}", 0)])?))
        .unwrap();
    reload(&mut t);
    let news = t.lib.live.groups.iter().find(|g| g.name == "News").map(|g| g.id).expect("News");
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: Some(news), channel: None }));
    assert!(act(&mut t, &mut s, Action::Activate));
    assert!(pushed(&t, &Route::Settings(tuner::ui::SettingsPage::Hidden)), "{:?}", t.nav);
    let mut h = screen_harness(t, s, 1600.0);
    h.step();
    h.step();
    assert!(has(&h, "Everything here is hidden") && has(&h, "Show hidden"));
    drop(h);

    // empty library → onboarding, by click or Enter
    let mut t = TestUi::new().unwrap();
    t.set_library(tuner::store::library::Library { built_at: 1, ..Default::default() });
    let mut s = LiveScreen::default();
    assert!(act(&mut t, &mut s, Action::Activate));
    assert!(t.nav.iter().any(|n| matches!(n, NavRequest::Switch(Route::Onboarding(_)))), "{:?}", t.nav);
    let mut g = GuideScreen::default();
    t.nav.clear();
    assert!(act(&mut t, &mut g, Action::Activate));
    assert!(t.nav.iter().any(|n| matches!(n, NavRequest::Switch(Route::Onboarding(_)))), "guide: {:?}", t.nav);
    let mut h = screen_harness(t, LiveScreen::default(), 1600.0);
    h.step();
    assert!(has(&h, "No playlists yet"));
}

/// Alt+↓ under a non-custom sort switches to custom order and moves the CURSOR channel one
/// place down in that order (provider groups and favourites lists alike).
#[test]
fn live_reorder_under_a_sort_moves_the_cursor_channel_in_custom_order() {
    let _one = one();
    let mut cfg = tuner::config::Config::default();
    cfg.interface.live_sort = tuner::model::SortMode::NameAsc;
    let (mut t, ids) = seeded(12, 0, Some(cfg.clone()));
    let key = |i: usize| format!("m3u:seed{i:011}");
    let all = t.lib.live.groups.iter().find(|g| g.name == "All").map(|g| g.id).expect("All");
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: Some(all), channel: None }));
    // Name A–Z: Channel 0, Channel 1, Channel 10, …
    act(&mut t, &mut s, Action::Down);
    act(&mut t, &mut s, Action::Down);
    act(&mut t, &mut s, Action::MoveDown);
    assert!(t.toasts.iter().any(|x| x.text == "Switched to custom order"));
    assert_eq!(t.config_edits.len(), 1, "the sort switch is queued");
    let order = |t: &TestUi, k: String| t.db.query_row("SELECT user_order FROM item_override WHERE item_key = ?1", [k], |r| r.get::<_, Option<i64>>(0)).ok().flatten();
    let deadline = Instant::now() + Duration::from_secs(5);
    while order(&t, key(10)).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!((order(&t, key(10)), order(&t, key(11))), (Some(11), Some(10)), "Channel 10 moved below Channel 11");
    assert_eq!(order(&t, key(2)), Some(2), "Channel 2 stays");
    // the cursor stays on the moved channel: Shift+Enter plays it
    act(&mut t, &mut s, Action::ActivateAlt);
    player_until(&t, "the moved channel", plays(MediaRef::Channel(ids[10])));

    // a favourites list: list order ch2, ch0, ch1 — shown A–Z as ch0, ch1, ch2
    t.services()
        .store
        .writer()
        .blocking(move |c| {
            let list = tuner::store::user::default_fav_list(c)?;
            for i in [2usize, 0, 1] {
                let it = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
                tuner::store::user::set_favourite(c, list, &it, true, 0)?;
            }
            Ok(())
        })
        .unwrap();
    reload(&mut t);
    t.config_edits.clear();
    t.toasts.clear();
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    // cursor on ch0 (first A–Z); Alt+↓ moves it below ch1 in the list's own order
    act(&mut t, &mut s, Action::MoveDown);
    assert!(t.toasts.iter().any(|x| x.text == "Switched to custom order"), "favourites switch too");
    let list_order = |t: &TestUi| {
        let mut st = t.db.prepare("SELECT item_key FROM fav_item ORDER BY position").unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    let want = vec![key(2), key(1), key(0)];
    let deadline = Instant::now() + Duration::from_secs(5);
    while list_order(&t) != want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(list_order(&t), want);
}

/// Long programme titles in the preview pane are truncated inside the pane; the time range
/// stays visible (Wide and Medium).
#[test]
fn live_preview_text_stays_inside_the_pane() {
    let _one = one();
    for (layout, w) in [(LayoutClass::Wide, 1600.0), (LayoutClass::Medium, 790.0)] {
        let (mut t, _ids) = seeded(3, 3, None);
        let long = "An extraordinarily long programme title that keeps going well past the edge of any preview pane";
        t.services().store.writer().blocking(move |c| Ok(c.execute("UPDATE programme SET title = ?1", [long])?)).unwrap();
        t.layout = layout;
        let mut s = LiveScreen::default();
        t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
        let mut h = screen_harness(t, s, w);
        for _ in 0..4 {
            h.step();
        }
        let tx = texts(&shapes(&h));
        let titles: Vec<Rect> = tx.iter().filter(|(x, _, _)| x.starts_with("An extraordinarily")).map(|(_, _, r)| *r).collect();
        assert!(titles.len() >= 2, "{layout:?}: the now and upcoming titles are painted");
        for r in &titles {
            assert!(r.right() <= w + 0.5, "{layout:?}: a title ends at {} in a {w} pt window", r.right());
        }
        // the now programme's time range sits beside its (truncated) title, clear of it
        let range = tx.iter().find(|(x, _, r)| x.contains(" – ") && titles.iter().any(|t| (t.center().y - r.center().y).abs() < t.height())).expect("time range beside the title");
        assert!(range.2.right() <= w + 0.5 && titles.iter().all(|t| !t.intersects(range.2)), "{layout:?}: the time range {:?} overlaps a title", range.2);
    }
}

/// A click on a group row opens it even when a hover already put the cursor there.
#[test]
fn live_click_opens_a_hovered_group() {
    let _one = one();
    let (mut t, _ids) = seeded(12, 0, None);
    t.layout = LayoutClass::Wide;
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    let mut h = screen_harness(t, s, 1600.0);
    for _ in 0..3 {
        h.step();
    }
    assert!(has(&h, "Channel 1"), "All channels is open");
    h.get_by_label("All channels").hover();
    h.step();
    h.get_by_label("News").hover();
    h.step();
    h.get_by_label("News").click();
    for _ in 0..3 {
        h.step();
    }
    assert!(!has(&h, "Channel 1") && has(&h, "Channel 10"), "News is open");
    assert_eq!(h.state().s.key_contexts(), [KeyContext::List]);
}

/// Opted-in preview autoplay never starts from hovering (the pointer moves the cursor too).
#[test]
fn live_hover_never_autoplays() {
    let _one = one();
    let mut cfg = tuner::config::Config::default();
    cfg.interface.live_preview_autoplay = true;
    cfg.interface.preview_delay_ms = 5;
    let (mut t, _ids) = seeded(6, 0, Some(cfg));
    t.layout = LayoutClass::Wide;
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    let mut h = screen_harness(t, s, 1600.0);
    for _ in 0..3 {
        h.step();
    }
    // the row (its logo carries the same label)
    h.query_all_by_label("Channel 3").next().expect("row").hover();
    h.step();
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
    player_stays_idle(&h.state().t, 300);
}

#[test]
fn live_favourite_toggle_and_item_menus() {
    let _one = one();
    let (mut t, _ids) = seeded(3, 0, None);
    let mut s = LiveScreen::default();
    t.with_ctx(|ctx| s.on_enter(ctx, &Route::Live { group: None, channel: None }));
    act(&mut t, &mut s, Action::ToggleFavourite);
    let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 0)) };
    assert!(t.favs.is_fav(&item), "optimistic");
    act(&mut t, &mut s, Action::ShowDetails);
    act(&mut t, &mut s, Action::ContextMenu);
    act(&mut t, &mut s, Action::SortMenu);
    let o = opened(&t);
    assert!(matches!(o[0], OverlayRequest::EditItem(ref i) if *i == item));
    assert!(matches!(&o[1], OverlayRequest::ContextMenu(m) if m.items.iter().any(|i| i.label == "Show in TV guide" && i.shortcut.as_deref() == Some("Ctrl+G"))));
    assert!(matches!(&o[2], OverlayRequest::ContextMenu(m) if m.items.iter().any(|i| i.label == "Custom order")));
    act(&mut t, &mut s, Action::OpenInGuide);
    assert!(t.nav.iter().any(|n| matches!(n, NavRequest::Push(Route::Guide { focus: Some(_), .. }))));
}

// ====================================================================== player

fn player_with(t: &mut TestUi, snap: Arc<PlayerSnapshot>) -> PlayerScreen {
    t.snapshot = snap;
    let mut p = PlayerScreen::default();
    t.with_ctx(|ctx| p.on_enter(ctx, &Route::Player));
    p
}

#[test]
fn info_banner_never_changes_the_context_and_controls_need_focus() {
    let _one = one();
    let (mut t, ids) = seeded(4, 0, None);
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    let mut p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen));
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    // any key shows the banner — the context stays, so ↓ keeps zapping
    act(&mut t, &mut p, Action::Mute);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    let down = egui::Event::Key { key: egui::Key::ArrowDown, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let mut stack = p.key_contexts();
    stack.push(KeyContext::Global);
    assert_eq!(keymap::resolve(&stack, &down), Some(Action::ZapNext));
    // Enter / i / Tab enter controls focus; Esc leaves it
    act(&mut t, &mut p, Action::FocusOsdControls);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerOsd, KeyContext::PlayerLive]);
    assert_eq!(keymap::resolve(&p.key_contexts(), &down), Some(Action::Down));
    act(&mut t, &mut p, Action::LeaveOsdControls);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    // Esc ladder: hide the banner first, then let the app leave
    assert!(act(&mut t, &mut p, Action::Back), "Esc hides the banner");
    assert!(!act(&mut t, &mut p, Action::Back), "then the app handles Esc");
    // window keys never show the banner
    act(&mut t, &mut p, Action::ToggleWindowFullscreen);
    assert!(!act(&mut t, &mut p, Action::Back));

    // seekable content uses the VOD context
    let p2 = player_with(&mut t, fake_playing(MediaRef::Movie(tuner::model::MovieId(1)), PlayableKind::Movie, None, PlayOrigin::Fullscreen));
    assert_eq!(p2.key_contexts(), [KeyContext::PlayerVod]);
}

/// A pause keeps the banner up after its timeout; Esc still hides it before leaving.
#[test]
fn esc_hides_the_banner_a_pause_keeps_up() {
    let _one = one();
    let mut cfg = tuner::config::Config::default();
    cfg.interface.osd_timeout_ms = 1;
    let (mut t, ids) = seeded(2, 0, Some(cfg));
    let mut paused = (*fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, None, PlayOrigin::Fullscreen)).clone();
    paused.paused = true;
    let mut p = player_with(&mut t, Arc::new(paused));
    std::thread::sleep(Duration::from_millis(20));
    assert!(act(&mut t, &mut p, Action::Back), "Esc hides the paused banner");
    assert!(!act(&mut t, &mut p, Action::Back), "then the app handles Esc");
}

#[test]
fn zapping_before_the_zap_list_exists_is_a_no_op() {
    let _one = one();
    let (mut t, ids) = seeded(3, 0, None);
    // a startup `--play`: playing, but no zap list yet
    let mut p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, None, PlayOrigin::Cli));
    assert!(act(&mut t, &mut p, Action::ZapNext));
    assert!(act(&mut t, &mut p, Action::ZapPrev));
    assert!(act(&mut t, &mut p, Action::LastChannel));
    draw(&mut t, &mut p);
    player_stays_idle(&t, 200);
}

#[test]
fn zapping_uses_the_zap_list_and_skips_locked_channels_while_locked() {
    let _one = one();
    let mut cfg = tuner::config::Config::default();
    let salt = "00".repeat(16);
    cfg.parental.enabled = true;
    cfg.parental.pin_hash = Some(tuner::ui::parental::hash_pin("1234", &salt));
    cfg.parental.pin_salt = Some(salt);
    let (mut t, ids) = seeded(4, 0, Some(cfg));
    t.services()
        .store
        .writer()
        .blocking(|c| Ok(c.execute("INSERT INTO item_override(source_id, kind, item_key, locked, fp) VALUES (1, 'channel', ?1, 1, '')", [format!("m3u:seed{:011}", 1)])?))
        .unwrap();
    reload(&mut t);
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    let mut p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen));
    act(&mut t, &mut p, Action::ZapNext);
    player_until(&t, "skips the locked channel 1", plays(MediaRef::Channel(ids[2])));
}

#[test]
fn side_panels_move_without_zapping_and_enter_tunes() {
    let _one = one();
    let (mut t, ids) = seeded(6, 6, None);
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    let mut p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen));
    act(&mut t, &mut p, Action::ChannelList);
    assert_eq!(p.key_contexts(), [KeyContext::List, KeyContext::PlayerLive]);
    draw(&mut t, &mut p);
    act(&mut t, &mut p, Action::Down);
    act(&mut t, &mut p, Action::Down);
    player_stays_idle(&t, 150);
    act(&mut t, &mut p, Action::Activate);
    player_until(&t, "tuned from the panel", plays(MediaRef::Channel(ids[2])));
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive], "tuning closes the panel");
    // mini guide and history open the same way; ← / Esc close
    act(&mut t, &mut p, Action::MiniGuide);
    draw(&mut t, &mut p);
    assert_eq!(p.key_contexts(), [KeyContext::List, KeyContext::PlayerLive]);
    act(&mut t, &mut p, Action::Parent);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    act(&mut t, &mut p, Action::ChannelHistory);
    act(&mut t, &mut p, Action::Back);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    // letters filter the panel
    act(&mut t, &mut p, Action::ChannelList);
    act(&mut t, &mut p, Action::FilterInput('4'));
    assert_eq!(p.key_contexts(), [KeyContext::ListFiltering, KeyContext::PlayerLive]);
}

#[test]
fn numeric_entry_tunes_by_channel_number() {
    let _one = one();
    let (mut t, ids) = seeded(15, 0, None);
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    let mut p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen));
    act(&mut t, &mut p, Action::Digit(1));
    assert_eq!(p.key_contexts(), [KeyContext::NumericEntry, KeyContext::PlayerLive]);
    act(&mut t, &mut p, Action::Digit(3));
    act(&mut t, &mut p, Action::Digit(9));
    act(&mut t, &mut p, Action::FilterBackspace);
    draw(&mut t, &mut p);
    act(&mut t, &mut p, Action::Activate);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    player_until(&t, "channel 13", plays(MediaRef::Channel(ids[12])));
    // Esc cancels
    act(&mut t, &mut p, Action::Digit(2));
    act(&mut t, &mut p, Action::Back);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    // another key commits the number (like the guide), then acts
    act(&mut t, &mut p, Action::Digit(5));
    act(&mut t, &mut p, Action::Mute);
    assert_eq!(p.key_contexts(), [KeyContext::PlayerLive]);
    player_until(&t, "channel 5", plays(MediaRef::Channel(ids[4])));
}

/// At 790 pt (a tiled half) every control is drawn — transport controls as glyphs — and
/// controls focus mode only walks drawn controls.
#[test]
fn player_controls_fit_a_tiled_half() {
    let _one = one();
    let (mut t, ids) = seeded(3, 0, None);
    t.layout = LayoutClass::Medium;
    let mut snap = (*fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, None, PlayOrigin::Fullscreen)).clone();
    snap.tracks.audio = vec![tuner::player::Track {
        id: 1,
        kind: tuner::player::TrackKind::Audio,
        title: Some("English (AC-3 5.1) Dolby Digital commentary".into()),
        lang: Some("eng".into()),
        codec: Some("ac3".into()),
        default: true,
        forced: false,
        selected: true,
        external: false,
        channels: Some(6),
    }];
    let p = player_with(&mut t, Arc::new(snap));
    let mut h = screen_harness(t, p, 790.0);
    {
        let ScreenApp { t, s } = h.state_mut();
        t.with_ctx(|ctx| s.on_action(ctx, Action::FocusOsdControls));
    }
    for _ in 0..3 {
        h.step();
    }
    for label in ["Previous channel", "Play / pause", "Next channel", "Audio", "Subtitles", "Guide", "History", "Info", "Fullscreen"] {
        assert!(has(&h, label), "{label} is drawn at 790 pt");
    }
    // the transport controls are glyphs (no "Previous channel" text painted)
    assert!(!texts(&shapes(&h)).iter().any(|(x, _, _)| x.contains("Previous channel")));
}

#[test]
fn episode_end_offers_the_next_episode() {
    let _one = one();
    let mut t = TestUi::new().unwrap();
    let eps: Vec<tuner::model::EpisodeId> = t
        .services()
        .store
        .writer()
        .blocking(|c| {
            let src = seed::seed_source(c, "S")?;
            seed::seed_series_with_episodes(c, src, 1, 1, 3)?;
            let mut st = c.prepare("SELECT id FROM episode ORDER BY season, episode")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })
        .unwrap();
    reload(&mut t);
    let mut p = player_with(&mut t, fake_playing(MediaRef::Episode(eps[0]), PlayableKind::Episode { season: 1, episode: 1 }, None, PlayOrigin::Fullscreen));
    draw(&mut t, &mut p);
    t.video.begin_frame(); // the player placed the video last frame
    t.with_ctx(|ctx| p.on_event(ctx, &tuner::msg::AppEvent::Player(tuner::player::PlayerEvent::Ended { item: None, finished: true })));
    // Enter = play now
    act(&mut t, &mut p, Action::FocusOsdControls);
    player_until(&t, "next episode", plays(MediaRef::Episode(eps[1])));
}

#[test]
fn osd_is_legible_in_a_light_theme() {
    let _one = one();
    let colors = std::fs::read_to_string(support::fixture("theme/white/colors.toml")).unwrap();
    let pal = tuner::theme::Palette::parse(&colors, false).unwrap();
    let white = Theme::from_parts("white".into(), pal, tuner::theme::ShellConfig::default(), 0.0, 1.0);
    let tk = &white.tokens;
    assert!(white.is_light());
    assert_ne!(tk.osd_accent, tk.accent, "the white theme's grey accent is too dark for the scrim");
    let bg = tuner::theme::tokens::OSD_EFFECTIVE_BG;
    assert!(tuner::theme::tokens::contrast_ratio(tk.osd_text, bg) >= 4.5);
    assert!(tuner::theme::tokens::contrast_ratio(tk.osd_accent, bg) >= 3.0);

    let (mut t, ids) = seeded(3, 3, None);
    t.theme = white.clone();
    let zap = ZapList { collection: Collection::AllChannels, channels: ids.clone().into() };
    let p = player_with(&mut t, fake_playing(MediaRef::Channel(ids[0]), PlayableKind::Live, Some(zap), PlayOrigin::Fullscreen));
    let mut h = screen_harness(t, p, 1600.0);
    {
        let ScreenApp { t, s } = h.state_mut();
        t.with_ctx(|ctx| s.on_action(ctx, Action::FocusOsdControls));
    }
    for _ in 0..3 {
        h.step();
    }
    let s = shapes(&h);
    let tx = texts(&s);
    assert!(!tx.is_empty());
    // every text over the video is OSD text / accent (or dimmed OSD text), never theme fg
    for (text, color, _) in &tx {
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        // same RGB (alpha-dimmed OSD text unpremultiplies with ±1 rounding)
        let osd_rgb = |c: Color32| {
            let [cr, cg, cb, _] = c.to_srgba_unmultiplied();
            [(cr, r), (cg, g), (cb, b)].iter().all(|(x, y)| x.abs_diff(*y) <= 2)
        };
        assert!(osd_rgb(tk.osd_text) || osd_rgb(tk.osd_accent) || *color == Color32::WHITE, "{text:?} painted in {color:?}");
        assert_ne!(*color, tk.fg, "{text:?} uses the theme foreground");
    }
    // the focused control uses the OSD accent
    assert!(tx.iter().any(|(_, c, _)| *c == tk.osd_accent), "focus in osd_accent");
    // the OSD scrim backs the bars
    assert!(s.iter().any(|s| matches!(s, Shape::Rect(r) if r.fill == tk.osd_scrim)));
}

// ====================================================================== mini player, overlays

#[test]
fn mini_player_sits_bottom_right_and_toasts_avoid_it() {
    let theme = Theme::fallback();
    let snap = fake_playing(MediaRef::Channel(ChannelId(1)), PlayableKind::Live, None, PlayOrigin::Fullscreen);
    struct App {
        host: VideoHost,
        theme: Theme,
        snap: Arc<PlayerSnapshot>,
        toasts: tuner::ui::widgets::toast::Toasts,
        result: Vec<Option<bool>>,
        /// The app draws the mini-player (nothing else placed the video).
        mini: bool,
    }
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 900.0)).build_ui_state(
        |ui, a: &mut App| {
            a.host.begin_frame();
            if a.mini {
                let r = a.host.mini_player(ui.ctx(), &a.theme, &a.snap);
                a.result.push(r);
            }
            let bottom = 900.0 - 26.0;
            a.toasts.show(ui.ctx(), &a.theme, a.host.mini_rect(), bottom);
        },
        App { host: VideoHost::new(None), theme: theme.clone(), snap, toasts: Default::default(), result: Vec::new(), mini: true },
    );
    h.state_mut().toasts.push(tuner::msg::Toast::info("Reminder set"));
    for _ in 0..3 {
        h.step();
    }
    let mini = h.state().host.mini_rect().expect("mini rect");
    assert_eq!(mini.width(), 320.0, "Wide: 320 pt");
    assert!(mini.right() < 1600.0 && mini.bottom() <= 900.0 - 26.0);
    assert_eq!(h.state().host.placement(), tuner::ui::video_host::Placement::None, "the mini player isn't a placement (it would blink)");
    let toast = h.get_by_label("Reminder set").rect();
    assert!(toast.bottom() <= mini.top(), "toast {toast:?} above the mini player {mini:?}");
    // ✕ stops, a click on the video opens the player
    h.get_by_label("Stop playback").click();
    h.step();
    h.step();
    assert!(h.state().result.contains(&Some(false)));
    let video = egui::pos2(mini.center().x, mini.top() + mini.height() * 0.3);
    h.input_mut().events.push(egui::Event::PointerMoved(video));
    h.step();
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton { pos: video, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
        h.step();
    }
    assert!(h.state().result.contains(&Some(true)), "a click on the video opens the player");
    // a frame without the mini-player (a screen placed the video, Compact…): its rect goes
    h.state_mut().mini = false;
    h.step();
    h.step();
    assert!(h.state().host.mini_rect().is_none(), "no rect is avoided once the mini-player is gone");
    // nothing plays → no mini player
    h.state_mut().mini = true;
    h.state_mut().snap = Arc::new(PlayerSnapshot::default());
    h.step();
    assert!(h.state().host.mini_rect().is_none());
}

#[test]
fn tracks_overlay_selects_tracks_and_adjusts_subtitles() {
    let _one = one();
    let mut t = TestUi::new().unwrap();
    let mut snap = (*fake_playing(MediaRef::Channel(ChannelId(1)), PlayableKind::Live, None, PlayOrigin::Fullscreen)).clone();
    let track = |id: i64, kind, lang: &str, selected| tuner::player::Track { id, kind, title: None, lang: Some(lang.into()), codec: Some("aac".into()), default: false, forced: false, selected, external: false, channels: Some(2) };
    snap.tracks.audio = vec![track(1, tuner::player::TrackKind::Audio, "eng", true), track(2, tuner::player::TrackKind::Audio, "deu", false)];
    snap.tracks.subs = vec![track(3, tuner::player::TrackKind::Sub, "eng", false)];
    t.snapshot = Arc::new(snap);
    // audio: cursor starts on the selected track; Enter picks and closes
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::Tracks { audio: true }, ctx));
    assert_eq!(o.key_contexts(), [KeyContext::Modal]);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Activate)), OverlayOutcome::Close));
    // subtitles: Off, eng, delay, size — ←/→ on delay adjust and keep the menu open
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::Tracks { audio: false }, ctx));
    for _ in 0..2 {
        t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    }
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Right)), OverlayOutcome::Keep));
    player_until(&t, "sub delay +0.1", |s| (s.sub_delay - 0.1).abs() < 1e-6);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Right)), OverlayOutcome::Keep));
    player_until(&t, "sub scale 1.1", |s| (s.sub_scale - 1.1).abs() < 1e-6);
    // drawn over the video in OSD colours; Esc closes
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 720.0)).build_ui(|ui| {
        let egui_ctx = ui.ctx().clone();
        let _ = t.with_ctx(|ctx| o.ui(&egui_ctx, ctx));
    });
    h.step();
    assert!(has(&h, "Off") && has(&h, "Delay  ‹ +0.0 s ›"));
    drop(h);
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::Tracks { audio: false }, ctx));
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Back)), OverlayOutcome::Close));
}

#[test]
fn edit_information_saves_overrides() {
    let _one = one();
    let (mut t, _ids) = seeded(2, 0, None);
    t.services()
        .store
        .writer()
        .blocking(|c| Ok(c.execute("UPDATE channel SET catchup = '{\"kind\":\"shift\",\"days\":3,\"correction_s\":0}' WHERE number = 1", [])?))
        .unwrap();
    reload(&mut t);
    let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{:011}", 0)) };
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::EditItem(item.clone()), ctx));
    let egui = t.egui.clone();
    let frame = |t: &mut TestUi, o: &mut Box<dyn Overlay>| {
        let mut out = egui.run_ui(Default::default(), |_| {
            t.with_ctx(|ctx| {
                let _ = o.ui(&egui, ctx);
            })
        });
        out.textures_delta.clear();
    };
    frame(&mut t, &mut o);
    assert_eq!(o.key_contexts(), [KeyContext::Modal]);
    // Name → editing pushes TextInput; Esc leaves the field
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate));
    assert_eq!(o.key_contexts(), [KeyContext::TextInput, KeyContext::Modal]);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Back));
    assert_eq!(o.key_contexts(), [KeyContext::Modal]);
    // rows: Name, Offset, EPG, Locked, Hidden, Save, Cancel
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    for _ in 0..5 {
        t.with_ctx(|ctx| o.on_action(ctx, Action::Right));
    }
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate)); // Hidden on
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    t.with_ctx(|ctx| o.on_action(ctx, Action::Activate)); // Save
    frame(&mut t, &mut o);
    let read = |t: &TestUi| tuner::store::user::item_override(&t.db, &item).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while read(&t).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let ov = read(&t).expect("override written");
    assert!(ov.hidden);
    assert_eq!(ov.catchup_offset_s, Some(300));
    assert_eq!(ov.rename, None, "unchanged name = no rename");
    // the EPG row goes to the mapping screen for this channel
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::EditItem(item.clone()), ctx));
    frame(&mut t, &mut o);
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    t.with_ctx(|ctx| o.on_action(ctx, Action::Down));
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Activate)), OverlayOutcome::Close));
    assert!(t.nav.iter().any(|n| matches!(n, NavRequest::Push(Route::EpgMapping { channel: Some(_) }))));
    // a vanished item shows an error instead of panicking
    let gone = ItemRef { key: ItemKey("m3u:gone".into()), ..item };
    let mut o = t.with_ctx(|ctx| overlays::build(OverlayRequest::EditItem(gone), ctx));
    frame(&mut t, &mut o);
    assert!(matches!(t.with_ctx(|ctx| o.on_action(ctx, Action::Back)), OverlayOutcome::Close));
}

// ====================================================================== keymap / help

/// Every key the WP-11 views react to is in a keymap table, so the help overlay and the hint
/// line list it (DESIGN §13.11 "no screen reads raw keys").
#[test]
fn every_wp11_key_is_in_help() {
    let help = |c: KeyContext| keymap::help_rows(c).into_iter().map(|(_, h)| h).collect::<Vec<_>>();
    let guide = help(KeyContext::Guide);
    for h in ["Previous / next channel", "Previous / next programme", "Jump to channel number", "Remind me / cancel reminder", "Catch up (past programme)", "Watch channel", "Collection: favourites lists, groups, all", "Previous / next day", "Jump to now", "Scroll time −/+ 3 h"] {
        assert!(guide.contains(&h), "guide help lacks {h}");
    }
    let live = help(KeyContext::PlayerLive);
    for h in ["Previous / next channel (in this list)", "Last channel", "Channel history", "Channel list", "Mini guide (now / next)", "Type a channel number", "On-screen controls", "Stream info (codec, resolution, bitrate)", "Subtitle delay −/+ 100 ms", "Audio tracks…", "Subtitles…"] {
        assert!(live.contains(&h), "player help lacks {h}");
    }
    assert!(help(KeyContext::PlayerVod).contains(&"Jump to live (from catch-up)"));
    assert!(help(KeyContext::PlayerOsd).contains(&"Leave the controls"));
    assert!(help(KeyContext::NumericEntry).contains(&"Go now (otherwise after 1.5 s)"));
    let list = help(KeyContext::List);
    for h in ["Play here / open (playing → fullscreen)", "Play fullscreen", "Show in TV guide", "Details / edit information", "Assign EPG channel", "Sort & view options…"] {
        assert!(list.contains(&h), "list help lacks {h}");
    }
}
