//! Widget and generic-overlay kittests (DESIGN §13.1, §13.11). Owner: WP-10a.
//!
//! Headless `egui_kittest` harnesses (no GPU): widgets are found through their accesskit
//! labels; colours are checked on the painted shapes. Overlay tests use `TestUi` (real
//! services in a temp profile, mpv `vo=null`). Store writes behind the dialogs land with WP-02;
//! until then the writer reports an error, which the overlays must survive.

mod support;

use std::sync::Arc;

use eframe::egui::{self, Color32, Pos2, Rect, Shape};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use tuner::config::Density;
use tuner::keymap::{Action, KeyContext};
use tuner::model::{FavListId, FavouriteList, ItemKey, ItemKind, ItemRef, SourceId};
use tuner::msg::Toast;
use tuner::store::library::FavIndex;
use tuner::theme::Theme;
use tuner::ui::OverlayRequest;
use tuner::ui::overlays::{self, ConfirmSpec, ContextMenuSpec, ListPickerSpec, MenuItem, Overlay, OverlayOutcome, RenameSpec, RenameTarget};
use tuner::ui::testing::TestUi;
use tuner::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use tuner::ui::widgets::toast::Toasts;
use tuner::ui::widgets::{RowState, rows, slider, toggle};

// ------------------------------------------------------------------ helpers

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

/// Filled rects of `color` that overlap `area`.
fn fills_in(shapes: &[Shape], area: Rect, color: Color32) -> usize {
    let area = area.shrink(1.0);
    shapes.iter().filter(|s| matches!(s, Shape::Rect(r) if r.fill == color && r.rect.intersects(area))).count()
}

/// Colour of the painted text `text` (first section of its galley).
fn text_color(shapes: &[Shape], text: &str) -> Option<Color32> {
    shapes.iter().find_map(|s| match s {
        Shape::Text(t) if t.galley.job.text == text => t.galley.job.sections.first().map(|s| s.format.color),
        _ => None,
    })
}

fn outcome(o: &OverlayOutcome) -> String {
    match o {
        OverlayOutcome::Keep => "Keep".into(),
        OverlayOutcome::Ignored => "Ignored".into(),
        OverlayOutcome::Close => "Close".into(),
        OverlayOutcome::CloseWith(r) => format!("CloseWith({r:?})"),
    }
}

// ------------------------------------------------------------------ virtual list

struct ListApp {
    theme: Theme,
    names: Vec<String>,
    rows: Vec<u32>,
    list: ListState,
    outcomes: Vec<ListOutcome>,
}

impl ListApp {
    fn new(n: usize) -> Self {
        let mut list = ListState::default();
        list.set_len(n);
        Self {
            theme: Theme::fallback(),
            names: (0..n).map(|i| format!("Row {i}")).collect(),
            rows: (0..n as u32).collect(),
            list,
            outcomes: Vec::new(),
        }
    }

    /// What a screen does with a key action.
    fn act(&mut self, a: Action) -> ListOutcome {
        let o = self.list.handle(a);
        if o == ListOutcome::FilterChanged {
            self.rows = self.list.filter.apply(self.names.iter().map(String::as_str));
            self.list.set_len(self.rows.len());
        }
        o
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let h = rows::row_height(&self.theme, Density::Comfortable, false);
        let (theme, names, rows) = (&self.theme, &self.names, &self.rows);
        let o = virtual_list(ui, theme, "test-list", &mut self.list, h, |ui, rc| {
            rows::menu_row(ui, theme, None, &names[rows[rc.index] as usize], None, false, rc.state)
        });
        if o != ListOutcome::None {
            self.outcomes.push(o);
        }
    }
}

fn list_harness(n: usize) -> Harness<'static, ListApp> {
    let mut h =
        Harness::builder().with_size(egui::vec2(420.0, 600.0)).with_step_dt(0.02).build_ui_state(|ui, app: &mut ListApp| app.ui(ui), ListApp::new(n));
    h.run();
    h
}

#[test]
fn list_virtualizes_and_keeps_the_cursor_in_view() {
    let mut h = list_harness(10_000);
    assert!(h.query_by_label("Row 0").is_some());
    assert!(h.query_by_label("Row 9999").is_none(), "only visible rows are drawn");
    let page = h.state().list.page;
    assert!((8..=13).contains(&page), "rows per page from the viewport: {page}");

    assert_eq!(h.state_mut().act(Action::End), ListOutcome::Moved);
    h.run();
    assert!(h.query_by_label("Row 9999").is_some(), "End scrolls to the last row");
    assert!(h.query_by_label("Row 0").is_none());

    h.state_mut().act(Action::Home);
    h.state_mut().act(Action::PageDown);
    h.run();
    assert_eq!(h.state().list.cursor, page);
    assert!(h.query_by_label(&format!("Row {page}")).is_some());

    h.state_mut().list.select(5000);
    h.run();
    assert!(h.query_by_label("Row 5000").is_some(), "select scrolls to the row");
}

#[test]
fn list_type_to_filter() {
    let mut h = list_harness(200);
    for c in "19".chars() {
        assert_eq!(h.state_mut().act(Action::FilterInput(c)), ListOutcome::FilterChanged);
    }
    assert_eq!(h.state().list.key_context(), KeyContext::ListFiltering);
    h.run();
    let shown = h.state().rows.len();
    assert!(shown > 0 && shown < 200, "{shown}");
    assert!(h.state().rows.iter().all(|&i| h.state().names[i as usize].contains('1')));
    assert!(h.query_by_label("Row 0").is_none());
    assert_eq!(h.state().list.cursor, 0);
    assert_eq!(h.state_mut().act(Action::Parent), ListOutcome::None, "Parent only on an empty filter");
    h.state_mut().act(Action::FilterClear);
    h.run();
    assert_eq!(h.state().rows.len(), 200);
    assert_eq!(h.state().list.key_context(), KeyContext::List);
    assert!(h.query_by_label("Row 0").is_some());
}

#[test]
fn hover_moves_the_cursor_only_on_real_pointer_motion() {
    let mut h = list_harness(300);
    let r3 = h.get_by_label("Row 3").rect();
    // the pointer's first appearance has no motion yet; the second event moves it
    h.hover_at(r3.center() - egui::vec2(10.0, 0.0));
    h.hover_at(r3.center());
    h.run();
    assert_eq!(h.state().list.cursor, 3, "pointer motion over a row moves the cursor");
    assert_eq!(h.state().outcomes.last(), Some(&ListOutcome::Moved));

    // keyboard moves while the pointer rests: the still pointer must not pull the cursor back
    h.state_mut().act(Action::Down);
    h.run();
    h.run_steps(3);
    assert_eq!(h.state().list.cursor, 4);

    // rows scrolling under a still pointer don't move the cursor either
    for _ in 0..5 {
        h.state_mut().act(Action::PageDown);
    }
    h.run();
    h.run_steps(3);
    let cursor = h.state().list.cursor;
    assert!(cursor > 40, "{cursor}");
    assert!(h.query_by_label("Row 3").is_none(), "the list scrolled");
    h.run_steps(3);
    assert_eq!(h.state().list.cursor, cursor);

    // moving the pointer again picks the row under it
    let under = Pos2::new(r3.center().x, r3.center().y + 1.0);
    h.hover_at(under);
    h.run();
    assert_ne!(h.state().list.cursor, cursor);
}

/// One click does what Enter does; a double-click activates once (the list ignores a
/// multi-click's later clicks, also when a busy frame batches both); right-click = the menu.
#[test]
fn list_clicks_activate_once_and_open_the_menu() {
    let mut h = list_harness(50);
    h.get_by_label("Row 2").click();
    h.run();
    assert_eq!(h.state().list.cursor, 2);
    assert_eq!(h.state().outcomes.iter().filter(|o| matches!(o, ListOutcome::Activated(_))).collect::<Vec<_>>(), [&ListOutcome::Activated(2)]);
    // let the first click age out of egui's double/triple-click window
    h.run_steps(40);
    let r = h.get_by_label("Row 4").rect();
    h.hover_at(r.center());
    h.run_steps(1);
    h.state_mut().outcomes.clear();
    let press =
        |pressed| egui::Event::PointerButton { pos: r.center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
    for _ in 0..2 {
        h.event(press(true));
        h.event(press(false));
        h.run_steps(2);
    }
    assert_eq!(h.state().outcomes, [ListOutcome::Activated(4)], "a double-click: once");
    h.run_steps(40);
    h.state_mut().outcomes.clear();
    for pressed in [true, false, true, false] {
        h.event(press(pressed));
    }
    h.run();
    assert_eq!(h.state().outcomes, [ListOutcome::Activated(4)], "both clicks in one frame: once");
    h.get_by_label("Row 6").click_secondary();
    h.run();
    assert_eq!(h.state().outcomes.last(), Some(&ListOutcome::ContextMenu(6)));
    assert_eq!(h.state().list.cursor, 6);
}

// ------------------------------------------------------------------ rows

#[test]
fn row_states_paint_the_omarchy_fills_and_accent_text() {
    let theme = Theme::fallback();
    let t = theme.tokens.clone();
    let mut h = Harness::builder().with_size(egui::vec2(420.0, 400.0)).build_ui(|ui| {
        rows::menu_row(ui, &theme, Some("󰋜"), "Idle row", None, false, RowState::Idle);
        rows::menu_row(ui, &theme, None, "Cursor row", Some("detail"), true, RowState::Cursor);
        rows::menu_row(ui, &theme, None, "Selected row", None, false, RowState::Selected);
        rows::menu_row(ui, &theme, None, "Both row", None, false, RowState::CursorSelected);
    });
    h.run();
    let s = shapes(&h);
    let rect = |l: &str| h.get_by_label(l).rect();
    assert_eq!(fills_in(&s, rect("Idle row"), t.list_cursor_bg) + fills_in(&s, rect("Idle row"), t.fill_selected), 0);
    assert_eq!(fills_in(&s, rect("Cursor row"), t.list_cursor_bg), 1);
    assert_eq!(fills_in(&s, rect("Selected row"), t.fill_selected), 1);
    assert_eq!(fills_in(&s, rect("Both row"), t.fill_selected), 1);
    assert_eq!(text_color(&s, "Idle row"), Some(t.fg));
    assert_eq!(text_color(&s, "Cursor row"), Some(t.list_cursor_text));
    assert_eq!(text_color(&s, "Selected row"), Some(t.fg));
    assert_eq!(text_color(&s, "Both row"), Some(t.list_cursor_text));
    assert_eq!(text_color(&s, "detail"), Some(t.text_secondary));
    // comfortable heights: 50, 58 with a detail line
    assert_eq!(rect("Idle row").height().round(), 50.0);
    assert_eq!(rect("Cursor row").height().round(), 58.0);
}

#[test]
fn rows_fit_compact_list_slots() {
    let theme = Theme::fallback();
    let mut list = ListState::default();
    list.set_len(20);
    let h_compact = rows::row_height(&theme, Density::Compact, false);
    let mut h = Harness::builder().with_size(egui::vec2(420.0, 300.0)).build_ui(|ui| {
        virtual_list(ui, &theme, "compact", &mut list, h_compact, |ui, rc| {
            rows::menu_row(ui, &theme, None, &format!("Item {}", rc.index), None, false, rc.state)
        });
    });
    h.run();
    assert_eq!(h.get_by_label("Item 1").rect().height().round(), 36.0);
    let (a, b) = (h.get_by_label("Item 1").rect(), h.get_by_label("Item 2").rect());
    assert!((b.top() - a.bottom()).abs() < 0.5, "rows are contiguous: {a:?} {b:?}");
}

// ------------------------------------------------------------------ toggle, segmented, slider, seek bar

#[test]
fn toggle_and_segmented_change_values_on_click() {
    struct S {
        theme: Theme,
        on: bool,
        mode: u8,
        changes: usize,
    }
    let mut h = Harness::builder().with_size(egui::vec2(420.0, 200.0)).build_ui_state(
        |ui, s: &mut S| {
            if toggle::toggle(ui, &s.theme, &mut s.on, "Autoplay previews", false).changed() {
                s.changes += 1;
            }
            if toggle::segmented(ui, &s.theme, &mut s.mode, &[(0, "Auto"), (1, "Expanded"), (2, "Rail")], true).changed() {
                s.changes += 1;
            }
        },
        S { theme: Theme::fallback(), on: false, mode: 0, changes: 0 },
    );
    h.run();
    h.get_by_label("Autoplay previews").click();
    h.run();
    assert!(h.state().on);
    h.get_by_label("Autoplay previews").click();
    h.run();
    assert!(!h.state().on);
    h.get_by_label("Rail").click();
    h.run();
    assert_eq!(h.state().mode, 2);
    h.get_by_label("Rail").click();
    h.run();
    assert_eq!(h.state().changes, 3, "re-selecting the current segment is not a change");
    // checked track = selected fill + accent knob
    h.get_by_label("Autoplay previews").click();
    h.run();
    let s = shapes(&h);
    let t = &h.state().theme.tokens;
    assert!(fills_in(&s, h.get_by_label("Autoplay previews").rect(), t.accent) >= 1);
}

#[test]
fn slider_snaps_to_steps() {
    struct S {
        theme: Theme,
        v: f32,
    }
    let mut h = Harness::builder().with_size(egui::vec2(300.0, 100.0)).build_ui_state(
        |ui, s: &mut S| {
            slider::slider(ui, &s.theme, &mut s.v, 0.0..=100.0, 5.0, false);
        },
        S { theme: Theme::fallback(), v: 50.0 },
    );
    h.run();
    let r = h.get_by_role(egui::accesskit::Role::Slider).rect();
    let knob = 14.0;
    let at = |f: f32| Pos2::new(r.left() + knob / 2.0 + f * (r.width() - knob), r.center().y);
    h.hover_at(at(0.74));
    h.drag_at(at(0.74));
    h.drop_at(at(0.74));
    h.run();
    assert_eq!(h.state().v, 75.0);
    h.hover_at(at(0.0));
    h.drag_at(at(0.0));
    h.drop_at(at(0.0));
    h.run();
    assert_eq!(h.state().v, 0.0);
}

#[test]
fn seek_bar_requests_positions_only_for_seekable_media() {
    struct S {
        theme: Theme,
        duration: f64,
        requests: Vec<f64>,
    }
    let mut h = Harness::builder().with_size(egui::vec2(414.0, 100.0)).build_ui_state(
        |ui, s: &mut S| {
            if let Some(p) = slider::seek_bar(ui, &s.theme, 100.0, s.duration, Some(160.0), true) {
                s.requests.push(p);
            }
        },
        S { theme: Theme::fallback(), duration: 1000.0, requests: Vec::new() },
    );
    h.run();
    let r = h.get_by_label("Seek").rect();
    let knob = 14.0;
    let x = r.left() + knob / 2.0 + 0.25 * (r.width() - knob);
    h.get_by_label("Seek").hover();
    h.hover_at(Pos2::new(x, r.center().y));
    h.drag_at(Pos2::new(x, r.center().y));
    h.drop_at(Pos2::new(x, r.center().y));
    h.run();
    let last = *h.state().requests.last().expect("a seek request");
    assert!((last - 250.0).abs() < 3.0, "{last}");
    // played part uses the OSD accent (not the theme fg)
    let s = shapes(&h);
    assert!(fills_in(&s, r, h.state().theme.tokens.osd_accent) >= 1);

    // a drag seeks as the knob moves, not on every frame it is held still
    let x50 = r.left() + knob / 2.0 + 0.5 * (r.width() - knob);
    h.state_mut().requests.clear();
    h.hover_at(Pos2::new(x, r.center().y));
    h.drag_at(Pos2::new(x, r.center().y));
    h.run();
    h.hover_at(Pos2::new(x50, r.center().y));
    h.run();
    let moved = h.state().requests.len();
    let last = *h.state().requests.last().expect("a seek while dragging");
    assert!((last - 500.0).abs() < 3.0, "{last}");
    h.run_steps(5);
    assert_eq!(h.state().requests.len(), moved, "knob held still: no new seeks");
    h.drop_at(Pos2::new(x50, r.center().y));
    h.run();
    assert_eq!(h.state().requests.len(), moved + 1, "the release seeks once more");

    // live: duration unknown → never seeks
    h.state_mut().duration = 0.0;
    h.state_mut().requests.clear();
    h.run();
    h.get_by_label("Seek").click();
    h.run();
    assert!(h.state().requests.is_empty());
}

// ------------------------------------------------------------------ sidebar

#[test]
fn sidebar_rail_and_compact_glyph_strip() {
    use tuner::ui::ScreenId;
    use tuner::ui::widgets::sidebar::sidebar;
    struct S {
        theme: Theme,
        strip: bool,
        clicked: Option<ScreenId>,
        search: bool,
        play_on: bool,
    }
    // below 480·rem the sidebar panel is gone; the app draws the strip in a horizontal layout
    assert_eq!(tuner::ui::layout::sidebar_width(400.0, 1.0, tuner::config::SidebarMode::Auto), 0.0);
    let mut h = Harness::builder().with_size(egui::vec2(400.0, 500.0)).build_ui_state(
        |ui, s: &mut S| {
            let (mut search, mut play_on_clicked) = (false, false);
            let play_on = Some(tuner::ui::widgets::sidebar::PlayOn { tv: None, key: "Alt+T" });
            let clicked = if s.strip {
                ui.horizontal(|ui| sidebar(ui, &s.theme, ScreenId::MovieDetail, false, true, None, play_on, &mut search, &mut play_on_clicked)).inner
            } else {
                sidebar(ui, &s.theme, ScreenId::Live, false, false, None, None, &mut search, &mut play_on_clicked)
            };
            s.clicked = s.clicked.or(clicked);
            s.search |= search;
            s.play_on |= play_on_clicked;
        },
        S { theme: Theme::fallback(), strip: true, clicked: None, search: false, play_on: false },
    );
    h.run();
    let labels = ["Play on: This computer (Alt+T)", "Search", "Home", "Live TV", "Guide", "Movies", "Series", "Settings"];
    let rects: Vec<Rect> = labels.iter().map(|l| h.get_by_label(l).rect()).collect();
    for w in rects.windows(2) {
        assert_eq!(w[0].top(), w[1].top(), "one row");
        assert!(w[1].left() >= w[0].right(), "left to right in order");
    }
    // a detail page highlights its grid entry
    let s = shapes(&h);
    let t = h.state().theme.tokens.clone();
    assert_eq!(fills_in(&s, h.get_by_label("Movies").rect(), t.fill_selected), 1);
    h.get_by_label("Guide").click();
    h.run();
    assert_eq!(h.state().clicked, Some(ScreenId::Guide));
    h.get_by_label("Search").click();
    h.run();
    assert!(h.state().search);
    // Play on is a control: it reports its own click and is never a screen
    h.get_by_label("Play on: This computer (Alt+T)").click();
    h.run();
    assert!(h.state().play_on && h.state().clicked == Some(ScreenId::Guide));

    // vertical rail without VOD: Movies/Series hidden, entries stacked, Search on top
    h.state_mut().strip = false;
    h.run();
    assert!(h.query_by_label("Movies").is_none() && h.query_by_label("Series").is_none());
    let (search, a, b) = (h.get_by_label("Search").rect(), h.get_by_label("Home").rect(), h.get_by_label("Live TV").rect());
    assert!(a.top() >= search.bottom() && b.top() >= a.bottom() && a.left() == b.left());
    assert!(h.query_by_label("Play on: This computer (Alt+T)").is_none(), "no Play on without a TV");
}

// ------------------------------------------------------------------ toasts

struct ToastApp {
    theme: Theme,
    toasts: Toasts,
    avoid: Option<Rect>,
    actions: Vec<Action>,
}

const STATUS_TOP: f32 = 974.0;

fn toast_harness(avoid: Option<Rect>) -> Harness<'static, ToastApp> {
    Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(
        |ui, s: &mut ToastApp| {
            if let Some(a) = s.toasts.show(ui.ctx(), &s.theme, s.avoid, STATUS_TOP) {
                s.actions.push(a);
            }
        },
        ToastApp { theme: Theme::fallback(), toasts: Toasts::default(), avoid, actions: Vec::new() },
    )
}

#[test]
fn toasts_stack_above_the_mini_player_and_the_status_bar() {
    let mini = Rect::from_min_max(Pos2::new(1260.0, 760.0), Pos2::new(1590.0, 960.0));
    let mut h = toast_harness(Some(mini));
    for i in 1..=3 {
        h.state_mut().toasts.push(Toast::info(format!("Toast {i}")));
    }
    h.run();
    for i in 1..=3 {
        let r = h.get_by_label(&format!("Toast {i}")).rect();
        assert!(r.bottom() <= mini.top(), "toast {i} {r:?} overlaps the mini player");
        assert!(r.right() <= 1600.0);
    }
    let (t2, t3) = (h.get_by_label("Toast 2").rect(), h.get_by_label("Toast 3").rect());
    assert!(t3.top() > t2.top(), "newest toast sits at the bottom of the stack");

    // without a mini player the stack sits right above the status bar
    let mut h = toast_harness(None);
    h.state_mut().toasts.push(Toast::error("Couldn't refresh"));
    h.run();
    let r = h.get_by_label("Couldn't refresh").rect();
    assert!(r.bottom() <= STATUS_TOP && r.bottom() > mini.top(), "{r:?}");
}

#[test]
fn toast_stack_is_capped_and_actions_fire_once() {
    let mut h = toast_harness(None);
    for i in 1..=6 {
        h.state_mut().toasts.push(Toast::info(format!("Note {i}")));
    }
    h.run();
    assert!(h.query_by_label("Note 1").is_none() && h.query_by_label("Note 2").is_none(), "older toasts wait");
    assert!(h.query_by_label("Note 3").is_some() && h.query_by_label("Note 6").is_some());

    // queued toasts don't expire while waiting: they show once the newer ones are gone
    let mut h = toast_harness(None);
    for i in 1..=2 {
        h.state_mut().toasts.push(Toast { ttl_ms: 300, ..Toast::error(format!("Failure {i}")) });
    }
    for i in 3..=6 {
        h.state_mut().toasts.push(Toast { ttl_ms: 600, ..Toast::info(format!("Note {i}")) });
    }
    h.run();
    assert!(h.query_by_label("Failure 1").is_none() && h.query_by_label("Note 3").is_some());
    std::thread::sleep(std::time::Duration::from_millis(650));
    h.run();
    assert!(h.query_by_label("Note 3").is_none() && h.query_by_label("Note 6").is_none(), "newer toasts expired");
    assert!(h.query_by_label("Failure 1").is_some() && h.query_by_label("Failure 2").is_some(), "queued toasts shown in turn");

    let mut h = toast_harness(None);
    h.state_mut().toasts.push(Toast { action: Some(("Undo".into(), Action::ToggleHidden)), ..Toast::info("Channel hidden") });
    h.run();
    h.get_by_label("Undo").click();
    h.run();
    assert_eq!(h.state().actions, vec![Action::ToggleHidden]);
    assert!(h.query_by_label("Channel hidden").is_none(), "pressing the action dismisses the toast");
    assert!(h.state().toasts.is_empty());
}

// ------------------------------------------------------------------ overlays (TestUi)

struct OvApp {
    t: TestUi,
    ov: Box<dyn Overlay>,
    /// Last outcome of `ui` other than `Keep`.
    last: String,
}

impl OvApp {
    fn new(mut t: TestUi, req: OverlayRequest) -> Self {
        let ov = t.with_ctx(|ctx| overlays::build(req, ctx));
        Self { t, ov, last: String::new() }
    }
    fn act(&mut self, a: Action) -> String {
        let ov = &mut self.ov;
        outcome(&self.t.with_ctx(|ctx| ov.on_action(ctx, a)))
    }
}

fn overlay_harness(app: OvApp, size: egui::Vec2) -> Harness<'static, OvApp> {
    let mut h = Harness::builder().with_size(size).with_step_dt(0.02).build_ui_state(
        |ui, app: &mut OvApp| {
            let egui_ctx = ui.ctx().clone();
            let ov = &mut app.ov;
            let o = outcome(&app.t.with_ctx(|ctx| ov.ui(&egui_ctx, ctx)));
            // the frame that produced a result is followed by more frames; keep the result
            if o != "Keep" {
                app.last = o;
            }
        },
        app,
    );
    h.run();
    h
}

#[test]
fn help_and_jobs_overlays() {
    let t = TestUi::new().expect("TestUi");
    // a job failure as the job system logs it (already redacted)
    t.services().logs.push(tuner::logging::LogLine {
        at: tuner::model::now(),
        level: tracing::Level::WARN,
        target: "tuner::jobs::import_source".into(),
        message: "Couldn't refresh My IPTV: HTTP 404".into(),
    });

    // help: every context, generated from the keymap (tall window: nothing scrolled away)
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::Help), egui::vec2(1600.0, 5000.0));
    // (a context of a feature that is off, multi-view, has no keys to list yet)
    for ctx in KeyContext::ALL.into_iter().filter(|c| !tuner::keymap::help_rows(*c).is_empty()) {
        assert!(h.query_by_label(&ctx.title().to_uppercase()).is_some(), "help misses {ctx:?}");
    }
    for (_, help) in tuner::keymap::help_rows(KeyContext::PlayerLive) {
        assert!(!h.query_all_by_label(help).collect::<Vec<_>>().is_empty(), "help misses '{help}'");
    }
    assert_eq!(h.state_mut().act(Action::Down), "Keep");
    assert_eq!(h.state_mut().act(Action::Quit), "Ignored", "globals keep working");
    assert_eq!(h.state_mut().act(Action::ToggleHelp), "Close");
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::Help]);

    // jobs: nothing running, the logged failure listed, Enter expands it, Ctrl+J closes
    let t = h.into_state().t;
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::Jobs), egui::vec2(1200.0, 900.0));
    assert!(h.query_by_label("Nothing is running").is_some());
    assert!(h.query_by_label("Couldn't refresh My IPTV: HTTP 404").is_some());
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::List, KeyContext::Modal]);
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert_eq!(h.state_mut().act(Action::Remove), "Keep", "nothing to cancel on a problem row");
    assert_eq!(h.state_mut().act(Action::FilterInput('x')), "Keep");
    assert_eq!(h.state_mut().act(Action::GoLive), "Ignored");
    h.run();
    // a newer failure is listed above it: the cursor stays on the row it was on
    let cursor_bg = h.state().t.theme.tokens.list_cursor_bg;
    let (old, new) = ("Couldn't refresh My IPTV: HTTP 404", "Couldn't refresh Backup: timed out");
    assert_eq!(fills_in(&shapes(&h), h.get_by_label(old).rect(), cursor_bg), 1);
    h.state().t.services().logs.push(tuner::logging::LogLine {
        at: tuner::model::now(),
        level: tracing::Level::ERROR,
        target: "tuner::jobs::import_source".into(),
        message: new.into(),
    });
    std::thread::sleep(std::time::Duration::from_millis(550));
    h.run();
    let s = shapes(&h);
    let (old_r, new_r) = (h.get_by_label(old).rect(), h.get_by_label(new).rect());
    assert!(new_r.bottom() <= old_r.top(), "newest problem first");
    assert_eq!(fills_in(&s, old_r, cursor_bg), 1, "the cursor stayed on its row");
    assert_eq!(fills_in(&s, new_r, cursor_bg), 0);
    assert_eq!(h.state_mut().act(Action::ShowJobs), "Close");
}

#[test]
fn confirm_context_menu_and_rename_dialogs() {
    let t = TestUi::new().expect("TestUi");
    let size = egui::vec2(1200.0, 900.0);

    // destructive confirm: Cancel focused first
    let spec = ConfirmSpec {
        title: "Delete playlist?".into(),
        body: "This removes 9 812 channels.".into(),
        confirm_label: "Delete".into(),
        destructive: true,
        token: 7,
    };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::Confirm(spec.clone())), size);
    assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Cancelled(7))");
    assert_eq!(h.state_mut().act(Action::Right), "Keep");
    assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Confirmed(7))");
    assert_eq!(h.state_mut().act(Action::Back), "CloseWith(Cancelled(7))");
    assert_eq!(h.state_mut().act(Action::Quit), "Ignored");
    h.get_by_label("Delete").click();
    h.run();
    assert_eq!(h.state().last, "CloseWith(Confirmed(7))");
    // the destructive button paints in `urgent`
    let s = shapes(&h);
    assert_eq!(text_color(&s, "Delete"), Some(h.state().t.theme.tokens.urgent));

    // non-destructive confirm: Enter confirms
    let t = h.into_state().t;
    let mut app = OvApp::new(t, OverlayRequest::Confirm(ConfirmSpec { destructive: false, token: 8, ..spec }));
    assert_eq!(app.act(Action::Activate), "CloseWith(Confirmed(8))");

    // context menu: first enabled item preselected, type-to-filter, disabled rows inert
    let item = |icon: &'static str, label: &str, action, enabled| MenuItem { icon, label: label.into(), action, enabled, shortcut: None };
    let spec = ContextMenuSpec {
        title: "BBC One".into(),
        items: vec![
            item("", "Locked thing", Action::ToggleLock, false),
            MenuItem { shortcut: Some("Ctrl+F".into()), ..item("󰓎", "Add to Favourites", Action::ToggleFavourite, true) },
            item("", "Hide", Action::ToggleHidden, false),
            item("", "Rename", Action::Rename, true),
        ],
    };
    let mut h = overlay_harness(OvApp::new(app.t, OverlayRequest::ContextMenu(spec)), size);
    assert!(h.query_by_label("Ctrl+F").is_none(), "shortcuts are painted, the row carries the label");
    assert!(h.query_by_label("Add to Favourites").is_some());
    assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Chosen(ToggleFavourite))");
    assert_eq!(h.state_mut().act(Action::Down), "Keep");
    assert_eq!(h.state_mut().act(Action::Activate), "Keep", "disabled item");
    for c in "ren".chars() {
        assert_eq!(h.state_mut().act(Action::FilterInput(c)), "Keep");
    }
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::ListFiltering, KeyContext::Modal]);
    h.run();
    assert!(h.query_by_label("Hide").is_none());
    assert_eq!(h.state_mut().act(Action::Activate), "CloseWith(Chosen(Rename))");
    for c in "zzz".chars() {
        h.state_mut().act(Action::FilterInput(c));
    }
    h.run();
    assert!(h.query_by_label_contains("No matches").is_some());
    assert_eq!(h.state_mut().act(Action::FilterClear), "Keep");
    assert_eq!(h.state_mut().act(Action::Back), "Close");
    h.run();
    h.get_by_label("Rename").click();
    h.run();
    assert_eq!(h.state().last, "CloseWith(Chosen(Rename))");

    // rename: unchanged → close; an empty source name is refused with a message
    let t = h.into_state().t;
    let spec = RenameSpec { title: "Rename playlist".into(), initial: "My IPTV".into(), target: RenameTarget::Source(SourceId(1)) };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::Rename(spec)), size);
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
    // Enter is acted on in `ui`, after the field took that frame's typing
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    h.run();
    assert_eq!(h.state().last, "Close");
    h.run_steps(2);
    h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::CTRL, egui::Key::A);
    h.key_press(egui::Key::Backspace);
    h.run();
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    h.run();
    assert!(h.query_by_label("The name can't be empty").is_some());
    assert_eq!(h.state_mut().act(Action::Back), "Close");
}

#[test]
fn pin_prompt_and_list_picker() {
    let salt = tuner::ui::parental::new_salt();
    let parental = tuner::config::ParentalConfig {
        show_explicit: false,
        enabled: true,
        pin_hash: Some(tuner::ui::parental::hash_pin("1234", &salt)),
        pin_salt: Some(salt),
        relock_min: 0,
    };
    let cfg = tuner::config::Config { parental, ..Default::default() };
    let t = TestUi::with_config(cfg).expect("TestUi");
    let size = egui::vec2(1200.0, 900.0);
    let req = || OverlayRequest::Pin { then: Box::new(Action::ToggleFavourite), reason: "“Adults” is locked".into() };

    let mut h = overlay_harness(OvApp::new(t, req()), size);
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
    h.run_steps(2);
    h.event(egui::Event::Text("12a".into()));
    h.run();
    assert_eq!(h.state_mut().act(Action::Activate), "Keep", "too short");
    h.run();
    assert!(h.query_by_label("Enter 4–8 digits").is_some());
    // the last digits and Enter in the same frame: Enter is acted on after the field took them
    h.event(egui::Event::Text("34".into()));
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    h.run();
    assert_eq!(h.state().last, "CloseWith(Unlocked(ToggleFavourite))", "letters are dropped: 12a34 → 1234");
    assert!(h.state().t.parental.is_unlocked(&h.state().t.config.parental));

    // three wrong PINs → cooldown: no text field, even the right PIN is refused
    let mut t = h.into_state().t;
    t.parental.lock();
    let mut h = overlay_harness(OvApp::new(t, req()), size);
    h.run_steps(2);
    for attempt in 1..=3 {
        h.event(egui::Event::Text("0000".into()));
        assert_eq!(h.state_mut().act(Action::Activate), "Keep", "attempt {attempt}");
        h.run();
        assert!(h.state().last.is_empty(), "attempt {attempt}: {}", h.state().last);
    }
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::Modal]);
    assert!(h.query_by_label_contains("try again in").is_some());
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    h.run();
    assert!(h.state().last.is_empty());
    assert!(!h.state().t.parental.is_unlocked(&h.state().t.config.parental));
    assert_eq!(h.state_mut().act(Action::Back), "Close");

    // favourites-list picker: optimistic toggles in the app-owned FavIndex, undone (with a
    // message) when the write fails — the writer is stopped here, so every write fails
    let mut t = h.into_state().t;
    assert!(t.services().store.writer().shutdown(std::time::Duration::from_secs(5)));
    let list = |id, name: &str, default| FavouriteList {
        id: FavListId(id),
        name: name.into(),
        icon: String::new(),
        sort_index: id as i32,
        is_default: default,
    };
    t.favs = FavIndex::from_parts(vec![list(1, "Favourites", true), list(2, "Sport", false)], Vec::new());
    let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("bbc-one".into()) };
    let spec = ListPickerSpec { item: item.clone(), title: "Add “BBC One” to…".into() };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::ListPicker(spec)), size);
    assert!(h.query_by_label("Sport").is_some() && h.query_by_label("New list…").is_some());
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert_eq!(h.state().t.favs.lists_of(&item), &[FavListId(1)], "optimistic");
    h.state_mut().act(Action::Down);
    h.state_mut().act(Action::Activate);
    assert_eq!(h.state().t.favs.lists_of(&item), &[FavListId(1), FavListId(2)]);
    h.run();
    assert!(h.state().t.favs.lists_of(&item).is_empty(), "failed writes are undone");
    assert!(h.query_by_label_contains("Couldn't update “").is_some());
    h.get_by_label("Favourites").click();
    h.run();
    assert!(h.state().t.favs.lists_of(&item).is_empty());
    assert!(h.query_by_label_contains("Couldn't update “Favourites”").is_some());
    // "New list…" (below the lists; the click put the cursor on row 0) opens an inline field
    // (TextInput); Esc returns to the checklist
    h.state_mut().act(Action::Down);
    h.state_mut().act(Action::Down);
    assert_eq!(h.state_mut().act(Action::Down), "Keep", "the cursor stops at the last row");
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
    assert_eq!(h.state_mut().act(Action::Activate), "Keep", "empty name");
    h.run();
    assert!(h.query_by_label("The name can't be empty").is_some());
    // a name typed in the same frame as Enter is used (the create then fails: no writer)
    h.event(egui::Event::Text("Sport 2".into()));
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    h.run();
    assert!(h.query_by_label_contains("Couldn't create the list").is_some());
    assert_eq!(h.state_mut().act(Action::Back), "Keep");
    assert_eq!(h.state().ov.key_contexts(), vec![KeyContext::Modal]);
    assert_eq!(h.state_mut().act(Action::Back), "Close");
}

/// Failed favourites-list writes settle to what the store holds: two failed toggles of the
/// same list end unticked (each used to undo "its" toggle, leaving the second one's tick), and
/// a failure answered after the picker closed still makes the app re-read the memberships.
#[test]
fn list_picker_failures_settle_to_the_stored_state() {
    let mut t = TestUi::new().expect("TestUi");
    // list 99 isn't in the database: every write to it fails (foreign key)
    t.favs = FavIndex::from_parts(
        vec![FavouriteList { id: FavListId(99), name: "Ghost".into(), icon: String::new(), sort_index: 0, is_default: false }],
        Vec::new(),
    );
    let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("bbc-one".into()) };
    let spec = ListPickerSpec { item: item.clone(), title: "Add “BBC One” to…".into() };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::ListPicker(spec)), egui::vec2(900.0, 700.0));
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert!(h.state().t.favs.lists_of(&item).is_empty(), "on, then off (optimistic)");
    for _ in 0..500 {
        h.run();
        if h.query_by_label_contains("Couldn't update “Ghost”").is_some() && h.state().t.favs.lists_of(&item).is_empty() {
            // let any answer still in flight arrive
            std::thread::sleep(std::time::Duration::from_millis(50));
            h.run();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(h.query_by_label_contains("Couldn't update “Ghost”").is_some());
    assert!(h.state().t.favs.lists_of(&item).is_empty(), "both writes failed: still not in the list");

    // a toggle, then the picker closes before the writer answers
    while h.state().t.events.try_recv().is_ok() {}
    assert_eq!(h.state_mut().act(Action::Activate), "Keep");
    assert_eq!(h.state_mut().act(Action::Back), "Close");
    let t = h.into_state().t;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut reread = false;
    while let Ok(ev) = t.events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
        if matches!(ev, tuner::msg::AppEvent::Data(tuner::msg::DataScope::Favourites)) {
            reread = true;
            break;
        }
    }
    assert!(reread, "the app is told to re-read the favourites after a failed write");
}

#[test]
fn data_rows_render_with_real_services() {
    use tuner::model::{
        ChannelId, ChannelSummary, EpgChannelId, Episode, EpisodeId, Group, GroupId, GroupKey, NowNext, ProgrammeId, ProgrammeSlim, SeriesId,
        StreamLocator,
    };
    let mut t = TestUi::new().expect("TestUi");
    let now = 1_791_576_000;
    let ch = ChannelSummary {
        id: ChannelId(1),
        source_id: SourceId(1),
        key: ItemKey("bbc".into()),
        name: Arc::from("BBC One HD"),
        provider_name: None,
        number: Some(101),
        logo: Some(Arc::from("http://127.0.0.1:9/logo.png")),
        group_ids: vec![GroupId(1)],
        epg_channel_id: Some(EpgChannelId(1)),
        tvg_shift_min: 0,
        has_catchup: true,
        catchup_days: 7,
        radio: false,
        locked: true,
        unsupported: false,
        provider_order: 0,
        user_order: None,
        added_at: None,
        quality: None,
        family: 0,
    };
    let prog = |id, title: &str, start, stop| ProgrammeSlim {
        id: ProgrammeId(id),
        start,
        stop,
        title: Arc::from(title),
        sub_title: None,
        flags: Default::default(),
        episode: None,
    };
    let nn = NowNext { now: Some(prog(1, "News at Eight", now - 600, now + 3000)), next: Some(prog(2, "Panorama", now + 3000, now + 6600)) };
    let group = Group {
        id: GroupId(1),
        source_id: SourceId(1),
        kind: ItemKind::Channel,
        key: GroupKey("channel:uk".into()),
        name: "UK News".into(),
        rename: None,
        provider_order: 0,
        user_order: None,
        hidden: false,
        user_hidden: false,
        locked: true,
        item_count: 9812,
        provider_id: None,
        tag: None,
        filter: Default::default(),
        explicit: Default::default(),
    };
    let ep = Episode {
        id: EpisodeId(1),
        series_id: SeriesId(1),
        source_id: SourceId(1),
        key: ItemKey("e5".into()),
        provider_id: None,
        season: 2,
        episode: 5,
        title: "The Return".into(),
        plot: None,
        duration_s: Some(2520),
        still: None,
        air_date: None,
        rating: None,
        stream: StreamLocator::Url { url: "http://x/e5.mkv".into() },
    };
    let theme = Theme::fallback();
    let mut h = Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_ui(|ui| {
        t.images.pump();
        let opts = rows::ChannelRowOpts {
            density: Density::Comfortable,
            show_number: true,
            show_logo: true,
            favourite: true,
            playing: true,
            ..Default::default()
        };
        rows::channel_row(ui, &theme, &mut t.images, &ch, Some(&nn), now, opts, RowState::Cursor);
        rows::channel_row(
            ui,
            &theme,
            &mut t.images,
            &ch,
            None,
            now,
            rows::ChannelRowOpts { density: Density::Compact, playing: false, ..opts },
            RowState::Idle,
        );
        rows::group_row(ui, &theme, &group, RowState::Selected);
        rows::programme_row(ui, &theme, &nn.now.clone().unwrap(), now, true, true, RowState::Idle);
        rows::episode_row(ui, &theme, &ep, Some(0.4), false, false, RowState::Idle);
        tuner::ui::widgets::badge::live_badge(ui, &theme);
        tuner::ui::widgets::progress::busy_bar(ui, &theme, 3.0);
        tuner::ui::widgets::text_field::search_header(ui, &theme, "", "Search channels", Some("󰍉"));
    });
    h.run_steps(2);
    // The logo URL is unreachable: the real image cache fails it asynchronously.
    let monogram = |h: &Harness<'_>| shapes(h).iter().any(|s| matches!(s, Shape::Text(t) if t.galley.job.text == "BO"));
    for _ in 0..400 {
        if monogram(&h) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        h.step();
    }
    let role = egui::accesskit::Role::Button;
    let channel_rows: Vec<_> = h.query_all_by_role_and_label(role, "BBC One HD").map(|n| n.rect().height().round()).collect();
    assert_eq!(channel_rows, vec![58.0, 36.0], "comfortable then compact row");
    assert_eq!(h.query_all_by_label("BBC One HD").count(), 4, "two rows + their logos");
    assert!(h.query_by_label("UK News").is_some());
    assert!(h.query_by_label("News at Eight").is_some());
    assert!(h.query_by_label("The Return").is_some());
    assert!(h.query_by_label("LIVE").is_some());
    assert!(h.query_by_label("Search channels…").is_some());
    let s = shapes(&h);
    assert!(
        s.iter().any(
            |s| matches!(s, Shape::Text(t) if t.galley.job.text.starts_with("News at Eight · ") && t.galley.job.text.ends_with("next: Panorama"))
        )
    );
    assert!(monogram(&h), "monogram for a failed logo");
    let play = s.iter().filter(|s| matches!(s, Shape::Text(t) if t.galley.job.text == tuner::ui::widgets::glyphs::PLAY)).count();
    assert_eq!(play, 1, "now-playing marker only on the playing row");
}

/// Step `h` until `done` holds (the store writer answers asynchronously).
fn step_until(h: &mut Harness<'static, OvApp>, done: impl Fn(&OvApp) -> bool) {
    for _ in 0..400 {
        h.step();
        if done(h.state()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Rename and favourites-list writes go through the store and are visible afterwards.
#[test]
fn dialogs_write_through_the_store() {
    let t = TestUi::new().expect("TestUi");
    let (source, channel) = t
        .services()
        .store
        .writer()
        .blocking(|c| {
            let s = support::seed::seed_source(c, "Seed")?;
            let ch = support::seed::seed_channels(c, s, 1)?;
            Ok((s, ch[0]))
        })
        .expect("seed");
    let size = egui::vec2(1200.0, 900.0);
    let spec = RenameSpec { title: "Rename playlist".into(), initial: "Seed".into(), target: RenameTarget::Source(source) };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::Rename(spec)), size);
    h.run_steps(2);
    h.event(egui::Event::Text(" 2".into()));
    h.run();
    assert_eq!(h.state_mut().act(Action::Activate), "Keep", "saving");
    step_until(&mut h, |a| a.last == "Close");
    assert_eq!(h.state().last, "Close");
    let name: String = h.state().t.db.query_row("SELECT name FROM source WHERE id = ?1", [source.0], |r| r.get(0)).unwrap();
    assert_eq!(name, "Seed 2");

    // a new favourites list, created with the channel in it
    let mut t = h.into_state().t;
    t.favs = tuner::store::library::load_favs(&t.db).expect("favs");
    let key: String = t.db.query_row("SELECT key FROM channel WHERE id = ?1", [channel.0], |r| r.get(0)).unwrap();
    let item = ItemRef { source, kind: ItemKind::Channel, key: ItemKey(key) };
    let spec = ListPickerSpec { item: item.clone(), title: "Add to…".into() };
    let mut h = overlay_harness(OvApp::new(t, OverlayRequest::ListPicker(spec)), size);
    let lists = h.state().t.favs.lists.len();
    for _ in 0..lists {
        h.state_mut().act(Action::Down);
    }
    h.state_mut().act(Action::Activate);
    h.run_steps(2);
    h.event(egui::Event::Text("Sport".into()));
    h.state_mut().act(Action::Activate);
    h.step(); // the create is submitted at the end of this frame, after the field took the text
    // Esc while the create is pending: the new list still gets the typed name
    assert_eq!(h.state_mut().act(Action::Back), "Keep");
    step_until(&mut h, |a| a.t.favs.lists.len() > lists);
    let new = h.state().t.favs.lists.iter().find(|l| l.name == "Sport").expect("new list shown").id;
    let stored = tuner::store::user::lists_containing(&h.state().t.db, &item).expect("lists");
    assert!(stored.contains(&new));
}

/// Locked channels and groups stay listed while the parental session is locked — dimmed, with
/// the lock glyph (DESIGN §20.3) — and paint normally once the PIN opens the session.
#[test]
fn locked_rows_dim_while_the_session_is_locked() {
    use tuner::model::{ChannelId, ChannelSummary, Group, GroupId, GroupKey};
    let mut t = TestUi::new().expect("TestUi");
    let theme = Theme::fallback();
    let tok = theme.tokens.clone();
    let ch = |id: i64, name: &str, locked: bool| ChannelSummary {
        id: ChannelId(id),
        source_id: SourceId(1),
        key: ItemKey(format!("c{id}")),
        name: Arc::from(name),
        provider_name: None,
        number: None,
        logo: None,
        group_ids: vec![GroupId(1)],
        epg_channel_id: None,
        tvg_shift_min: 0,
        has_catchup: false,
        catchup_days: 0,
        radio: false,
        locked,
        unsupported: false,
        provider_order: id as i32,
        user_order: None,
        added_at: None,
        quality: None,
        family: 0,
    };
    let grp = |id: i64, name: &str, locked: bool| Group {
        id: GroupId(id),
        source_id: SourceId(1),
        kind: ItemKind::Channel,
        key: GroupKey(format!("channel:{id}")),
        name: name.into(),
        rename: None,
        provider_order: id as i32,
        user_order: None,
        hidden: false,
        user_hidden: false,
        locked,
        item_count: 3,
        provider_id: None,
        tag: None,
        filter: Default::default(),
        explicit: Default::default(),
    };
    let (locked_ch, open_ch) = (ch(1, "Locked Channel", true), ch(2, "Open Channel", false));
    let (locked_g, open_g) = (grp(1, "Locked Group", true), grp(2, "Open Group", false));
    let session_locked = std::cell::Cell::new(true);
    let mut h = Harness::builder().with_size(egui::vec2(700.0, 400.0)).build_ui(|ui| {
        tuner::ui::widgets::set_session_locked(ui.ctx(), session_locked.get());
        let opts = rows::ChannelRowOpts { density: Density::Compact, ..Default::default() };
        for c in [&locked_ch, &open_ch] {
            rows::channel_row(ui, &theme, &mut t.images, c, None, 0, opts, RowState::Idle);
        }
        for g in [&locked_g, &open_g] {
            rows::group_row(ui, &theme, g, RowState::Idle);
        }
    });
    h.run();
    let s = shapes(&h);
    assert_eq!(text_color(&s, "Locked Channel"), Some(tok.text_disabled), "locked channel dimmed while locked");
    assert_eq!(text_color(&s, "Locked Group"), Some(tok.text_disabled), "locked group dimmed while locked");
    assert_eq!(text_color(&s, "Open Channel"), Some(tok.fg));
    assert_eq!(text_color(&s, "Open Group"), Some(tok.fg));
    let locks = s.iter().filter(|s| matches!(s, Shape::Text(t) if t.galley.job.text == tuner::ui::widgets::glyphs::LOCK)).count();
    assert_eq!(locks, 2, "the lock glyph stays on both locked rows");

    // the PIN opens the session: the same rows paint normally
    session_locked.set(false);
    h.run();
    let s = shapes(&h);
    assert_eq!(text_color(&s, "Locked Channel"), Some(tok.fg));
    assert_eq!(text_color(&s, "Locked Group"), Some(tok.fg));
}
