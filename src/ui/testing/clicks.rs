//! Pointer regressions use painted text bounds, rather than a widget's (often empty) centre.

use eframe::egui;
use egui_kittest::Harness;

use crate::theme::Theme;

pub(crate) fn harness<S>(state: S, draw: impl FnMut(&mut egui::Ui, &mut S) + 'static) -> Harness<'static, S> {
    let mut h = Harness::builder().with_size(egui::vec2(800.0, 600.0)).with_step_dt(0.02).build_ui_state(draw, state);
    crate::fonts::LoadedFonts::default().install(&h.ctx);
    crate::theme::egui_style::apply(&h.ctx, &Theme::fallback());
    h.run_steps(2);
    h
}

pub(crate) fn text_rect<S>(h: &Harness<'_, S>, text: &str) -> egui::Rect {
    fn find(shape: &egui::Shape, text: &str) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Text(t) if t.galley.job.text == text => Some(t.galley.rect.translate(t.pos.to_vec2())),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, text)),
            _ => None,
        }
    }
    h.output().shapes.iter().find_map(|s| find(&s.shape, text)).unwrap_or_else(|| panic!("no painted text {text:?}"))
}

pub(crate) fn click_at<S>(h: &mut Harness<'_, S>, pos: egui::Pos2) {
    h.hover_at(pos);
    h.run_steps(1);
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
        h.run_steps(1);
    }
}

pub(crate) fn click_text<S>(h: &mut Harness<'_, S>, text: &str) {
    click_at(h, text_rect(h, text).center());
}

pub(crate) fn copied_text<'a, S>(h: &'a Harness<'_, S>) -> Option<&'a str> {
    h.output().platform_output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::CopyText(text) => Some(text.as_str()),
        _ => None,
    })
}

#[test]
fn button_text_icon_and_padding_click() {
    use crate::ui::widgets::{button::button, glyphs};
    let mut h = harness((0, egui::Rect::NOTHING), |ui, (count, rect)| {
        let r = button(ui, &Theme::fallback(), Some(glyphs::ADD), "Add playlist", Default::default());
        *rect = r.rect;
        *count += usize::from(r.clicked());
    });
    click_text(&mut h, "Add playlist");
    click_text(&mut h, glyphs::ADD);
    let padding = h.state().1.left_top() + egui::vec2(2.0, 2.0);
    click_at(&mut h, padding);
    assert_eq!(h.state().0, 3);
}

#[test]
fn painted_row_text_detail_and_icon_click() {
    use crate::ui::widgets::{RowState, glyphs, rows};
    let mut h = harness(0, |ui, count| {
        let r = rows::menu_row(ui, &Theme::fallback(), Some(glyphs::PLAY), "Channel", Some("Now playing"), true, RowState::Idle);
        *count += usize::from(r.clicked());
    });
    for text in ["Channel", "Now playing", glyphs::PLAY] {
        click_text(&mut h, text);
    }
    assert_eq!(*h.state(), 3);
}

#[test]
fn toggle_label_and_switch_click_once_on_double_click() {
    use crate::ui::widgets::toggle::toggle;
    let mut h = harness((false, 0, egui::Rect::NOTHING), |ui, (on, count, rect)| {
        let r = toggle(ui, &Theme::fallback(), on, "Enable subtitles", false);
        *rect = r.rect;
        *count += usize::from(r.changed());
    });
    click_text(&mut h, "Enable subtitles");
    click_text(&mut h, "Enable subtitles");
    assert!(h.state().0);
    assert_eq!(h.state().1, 1, "a double-click changes the toggle once");
    h.run_steps(30);
    let switch = h.state().2.right_center() - egui::vec2(20.0, 0.0);
    click_at(&mut h, switch);
    assert!(!h.state().0);
    assert_eq!(h.state().1, 2);
}

#[test]
fn copy_card_facts_and_status_click() {
    use crate::ui::widgets::copy_card::{CopyCard, StatusTone, copy_card};
    let mut h = harness(0, |ui, count| {
        let card = CopyCard { facts: Some("EN · 4K HDR"), status: Some(("plays first", StatusTone::Accent)), ..Default::default() };
        *count += usize::from(copy_card(ui, &Theme::fallback(), 350.0, card).clicked());
    });
    click_text(&mut h, "EN · 4K HDR");
    click_text(&mut h, "plays first");
    assert_eq!(*h.state(), 2);
}

#[test]
fn framed_spoiler_block_text_clicks() {
    use crate::guard::Field;
    use crate::ui::widgets::spoiler::hidden_block;
    let mut h = harness(0, |ui, count| {
        *count += usize::from(hidden_block(ui, &Theme::fallback(), Field::Description, None, 0, true).clicked());
    });
    use egui_kittest::kittest::Queryable;
    // The only label is the block's child, not its enclosing frame.
    let label = h.query_all_by_role(egui::accesskit::Role::Label).next().expect("hidden description label").rect();
    click_at(&mut h, label.center());
    assert_eq!(*h.state(), 1);
}

#[test]
fn scope_tab_and_source_chip_text_click() {
    use crate::ui::scope::{Scope, ScopeKind};
    use crate::ui::screens::home::testkit;
    use crate::ui::widgets::{badge, scope_bar};
    let (mut t, _one) = testkit::rig();
    let source = testkit::seed(&mut t, |c| {
        let s = testkit::source(c, "TV")?;
        testkit::channels(c, s, 1)?;
        let other = testkit::source(c, "Radio")?;
        testkit::channels(c, other, 1)?;
        Ok(s)
    });
    let mut h = harness((t, scope_bar::BarClick::None, 0), |ui, (t, picked, count)| {
        let scope = Scope::of(&t.lib, &t.config.interface.playlist_scope, ScopeKind::Live);
        let r = scope_bar::scope_bar(ui, &t.theme, &scope, &[2, 1, 1], true, "");
        if r != scope_bar::BarClick::None {
            *picked = r;
        }
        let src = scope.eligible[0];
        *count += usize::from(badge::source_chip_button(ui, &t.theme, src, "TV (HD)", false, false, "Play from TV").clicked());
    });
    click_text(&mut h, "TV");
    assert_eq!(h.state().1, scope_bar::BarClick::Choose(Some(source)));
    let text = format!("{} TV (HD)", badge::source_glyph(&h.state().0.lib.sources[0]));
    click_text(&mut h, &text);
    assert_eq!(h.state().2, 1);
}

#[test]
fn poster_title_and_stack_badge_click_separately() {
    use crate::ui::screens::home::testkit;
    use crate::ui::widgets::poster::{StackDecor, TileDecor, poster_tile_ex};
    let (mut t, _one) = testkit::rig();
    testkit::seed(&mut t, |c| {
        let s = testkit::source(c, "TV")?;
        testkit::movies(c, s, 1)
    });
    let mut h = harness((t, 0, 0), |ui, (t, cards, badges)| {
        let decor = TileDecor { stack: Some(StackDecor { count: 2, fan: true, ..Default::default() }), ..Default::default() };
        let (r, badge) = poster_tile_ex(ui, &t.theme, &mut t.images, &t.lib.vod.movies[0], 180.0, false, decor);
        *cards += usize::from(r.clicked());
        *badges += usize::from(badge);
    });
    click_text(&mut h, "Movie 0");
    assert_eq!((h.state().1, h.state().2), (1, 0));
    click_text(&mut h, "2");
    assert_eq!((h.state().1, h.state().2), (1, 1), "the badge fans out without opening the title");
}
