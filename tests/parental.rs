//! Parental PIN UI (DESIGN §20.3, WP-12): Settings ▸ Parental sets / changes / removes the
//! PIN, locked categories and titles ask for it, search hides locked items until the session
//! is unlocked, and the PIN prompt unlocks the session. Real screens in `egui_kittest`
//! harnesses over `TestUi`. Owner: WP-12.

mod support;

use std::sync::{Arc, Mutex, MutexGuard};

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use support::seed;
use tuner::config::ParentalConfig;
use tuner::keymap::{Action, KeyContext};
use tuner::model::{GroupOverride, ItemKind, ItemOverride, ItemRef};
use tuner::ui::overlays::{self, Overlay, OverlayOutcome, OverlayResult};
use tuner::ui::parental::hash_pin;
use tuner::ui::screens::movies::MoviesScreen;
use tuner::ui::screens::settings::SettingsScreen;
use tuner::ui::testing::TestUi;
use tuner::ui::{NavRequest, OverlayRequest, Route, Screen, SettingsPage};

static ONE: Mutex<()> = Mutex::new(());

struct App<S> {
    t: TestUi,
    s: S,
    _one: MutexGuard<'static, ()>,
}

impl<S: Screen> App<S> {
    fn act(&mut self, a: Action) -> bool {
        let s = &mut self.s;
        self.t.with_ctx(|ctx| s.on_action(ctx, a))
    }
    fn result(&mut self, r: OverlayResult) {
        let s = &mut self.s;
        self.t.with_ctx(|ctx| s.on_overlay_result(ctx, r));
    }
    /// Apply queued config edits like the app does.
    fn apply_config(&mut self) {
        let mut c = (*self.t.config).clone();
        for e in self.t.config_edits.drain(..) {
            (e.apply)(&mut c);
        }
        self.t.config = Arc::new(c);
    }
    fn nav(&mut self) -> Vec<NavRequest> {
        std::mem::take(&mut self.t.nav)
    }
}

fn harness<S: Screen + 'static>(t: TestUi, s: S, route: Route) -> Harness<'static, App<S>> {
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 900.0)).with_step_dt(0.02).build_ui_state(
        |ui, app: &mut App<S>| {
            let s = &mut app.s;
            app.t.with_ctx(|ctx| s.ui(ui, ctx));
        },
        App { t, s, _one: one },
    );
    let app = h.state_mut();
    let s = &mut app.s;
    app.t.with_ctx(|ctx| s.on_enter(ctx, &route));
    h.run_steps(2);
    h
}

fn has<T>(h: &Harness<'static, T>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn press<S: Screen>(h: &mut Harness<'static, App<S>>, a: Action) {
    h.state_mut().act(a);
    h.run_steps(2);
}

fn type_text<T>(h: &mut Harness<'static, T>, text: &str) {
    h.run_steps(2);
    h.event(egui::Event::Text(text.into()));
    h.run_steps(1);
}

fn with_pin(t: &mut TestUi, pin: &str) {
    let mut c = (*t.config).clone();
    c.parental =
        ParentalConfig { show_explicit: false, enabled: true, pin_hash: Some(hash_pin(pin, "salt")), pin_salt: Some("salt".into()), relock_min: 0 };
    t.config = Arc::new(c);
}

fn load_library(t: &mut TestUi) {
    let r = t.services().store.reader().unwrap();
    t.set_library(tuner::store::library::load(&r).unwrap());
    t.favs = tuner::store::library::load_favs(&r).unwrap();
}

#[test]
fn settings_sets_a_pin_then_lock_now_and_change_need_it() {
    let t = TestUi::new().unwrap();
    let mut h = harness(t, SettingsScreen::default(), Route::Settings(SettingsPage::Parental));
    assert!(h.query_all_by_label_contains("speed bump for children").next().is_some(), "the honest note");
    assert!(has(&h, "SET A PIN (4–8 DIGITS)"));
    // the first field: New PIN (Enter edits, Tab to the repeat field)
    press(&mut h, Action::Activate);
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Settings]);
    type_text(&mut h, "12a34");
    press(&mut h, Action::FocusNext);
    type_text(&mut h, "4321");
    press(&mut h, Action::Activate); // Enter in the repeat field sets the PIN
    assert!(has(&h, "The PINs don't match"));
    assert!(h.state().t.config_edits.is_empty());
    // the repeat field was cleared: type it again
    press(&mut h, Action::Activate);
    type_text(&mut h, "1234");
    press(&mut h, Action::Activate);
    h.state_mut().apply_config();
    let cfg = h.state().t.config.parental.clone();
    assert!(cfg.enabled, "setting a PIN turns the lock on");
    assert_eq!(cfg.pin_hash.as_deref(), Some(hash_pin("1234", cfg.pin_salt.as_deref().unwrap()).as_str()), "letters were dropped: 12a34 → 1234");
    assert!(h.state().t.parental.is_unlocked(&cfg), "the session that set the PIN stays open");
    h.run_steps(2);
    assert!(has(&h, "Lock now") && has(&h, "Remove PIN…") && has(&h, "Change PIN"));
    // Lock now
    h.state_mut().t.parental.lock();
    assert!(!h.state().t.parental.is_unlocked(&h.state().t.config.parental));
    // with the session locked, changing a parental setting asks for the PIN first
    let app = h.state_mut();
    app.act(Action::Up); // leave any field
    for _ in 0..10 {
        app.act(Action::Up);
    }
    app.act(Action::Activate); // the "Parental lock" toggle
    h.run_steps(2);
    let nav = h.state_mut().nav();
    assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]), "{nav:?}");
    assert!(h.state().t.config_edits.is_empty(), "nothing changed without the PIN");
}

#[test]
fn the_pin_prompt_unlocks_the_session() {
    let mut t = TestUi::new().unwrap();
    with_pin(&mut t, "2468");
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let o = t.with_ctx(|ctx| overlays::build(OverlayRequest::Pin { then: Box::new(Action::Activate), reason: "Locked".into() }, ctx));
    struct Ov {
        t: TestUi,
        o: Box<dyn overlays::Overlay>,
        last: Option<OverlayResult>,
        _one: MutexGuard<'static, ()>,
    }
    let mut h = Harness::builder().with_size(egui::vec2(800.0, 600.0)).build_ui_state(
        |ui, app: &mut Ov| {
            let egui_ctx = ui.ctx().clone();
            let o = &mut app.o;
            if let OverlayOutcome::CloseWith(r) = app.t.with_ctx(|ctx| o.ui(&egui_ctx, ctx)) {
                app.last = Some(r);
            }
        },
        Ov { t, o, last: None, _one: one },
    );
    assert!(!h.state().t.parental.is_unlocked(&h.state().t.config.parental));
    type_text(&mut h, "2468");
    let app = h.state_mut();
    let ov = &mut app.o;
    let _ = app.t.with_ctx(|ctx| ov.on_action(ctx, Action::Activate));
    h.run_steps(2);
    assert_eq!(h.state().last, Some(OverlayResult::Unlocked(Action::Activate)));
    assert!(h.state().t.parental.is_unlocked(&h.state().t.config.parental));
}

#[test]
fn locked_categories_and_titles_need_the_pin() {
    let mut t = TestUi::new().unwrap();
    let group = t
        .services()
        .store
        .writer()
        .blocking(|c| {
            let s = seed::seed_source(c, "My IPTV")?;
            seed::seed_movies(c, s, 4)?;
            let key = tuner::model::GroupKey("movie:movies".into());
            tuner::store::user::set_group_override(c, s, ItemKind::Movie, &key, &GroupOverride { locked: true, ..Default::default() })?;
            Ok(c.query_row("SELECT id FROM grp WHERE key = 'movie:movies'", [], |r| r.get::<_, i64>(0))?)
        })
        .unwrap();
    load_library(&mut t);
    with_pin(&mut t, "1357");
    let mut h = harness(t, MoviesScreen::default(), Route::Movies { group: Some(tuner::model::GroupId(group)) });
    assert!(has(&h, "This category is locked"), "the locked category doesn't show its posters");
    h.query_all_by_label("Unlock").next().expect("Unlock button").click();
    h.run_steps(2);
    let nav = h.state_mut().nav();
    assert!(matches!(nav.as_slice(), [NavRequest::Open(OverlayRequest::Pin { .. })]), "{nav:?}");
    // the PIN prompt answered: the category opens
    let app = h.state_mut();
    let cfg = app.t.config.clone();
    assert!(app.t.parental.unlock("1357", &cfg.parental));
    app.result(OverlayResult::Unlocked(Action::Activate));
    h.run_steps(2);
    assert!(has(&h, "Movie 0") && !has(&h, "This category is locked"));
}

#[test]
fn search_hides_locked_channels_until_unlocked() {
    let mut t = TestUi::new().unwrap();
    t.services()
        .store
        .writer()
        .blocking(|c| {
            let s = seed::seed_source(c, "My IPTV")?;
            seed::seed_channels(c, s, 5)?;
            let item = ItemRef { source: s, kind: ItemKind::Channel, key: tuner::model::ItemKey(format!("m3u:seed{:011}", 3)) };
            tuner::store::user::set_item_override(c, &item, &ItemOverride { locked: true, ..Default::default() })
        })
        .unwrap();
    load_library(&mut t);
    with_pin(&mut t, "1111");
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    struct Ov {
        t: TestUi,
        o: overlays::search::SearchOverlay,
        _one: MutexGuard<'static, ()>,
    }
    let mut h = Harness::builder().with_size(egui::vec2(1000.0, 800.0)).with_step_dt(0.02).build_ui_state(
        |ui, app: &mut Ov| {
            let egui_ctx = ui.ctx().clone();
            let o = &mut app.o;
            let _ = app.t.with_ctx(|ctx| o.ui(&egui_ctx, ctx));
        },
        Ov { t, o: overlays::search::SearchOverlay::new("Channel".into()), _one: one },
    );
    let start = std::time::Instant::now();
    while !has(&h, "Channel 1") {
        assert!(start.elapsed().as_secs() < 10, "results");
        h.step();
    }
    assert!(!has(&h, "Channel 3"), "locked: excluded while the session is locked");
    let app = h.state_mut();
    let cfg = app.t.config.clone();
    assert!(app.t.parental.unlock("1111", &cfg.parental));
    let start = std::time::Instant::now();
    while !has(&h, "Channel 3") {
        assert!(start.elapsed().as_secs() < 10, "the unlocked channel");
        h.step();
    }
}
