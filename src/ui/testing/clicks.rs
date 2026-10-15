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
