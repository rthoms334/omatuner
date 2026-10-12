//! Double-click safety and the focusing click (DESIGN §15.1 "Double-click safety"). The app
//! owns one [`ClickGuard`] and feeds it raw input before egui sees it.
//!
//! One click does what Enter does, so a double-click is "one activation" in two layers:
//! * a widget whose content stays put acts on [`widgets::fresh_click`](super::widgets::fresh_click)
//!   and ignores the later clicks of a multi-click;
//! * when a click changed what is under the pointer — the route or overlay stack changed (the
//!   app notices), or a screen swapped a pane's content ([`mark_swapped`]) — the next primary
//!   press near that click within the double-click delay is dropped with its release, so the
//!   second click of a double-click can't play, open or flip what appeared there (nor toggle
//!   fullscreen on a player that just opened).
//!
//! The click into an unfocused window: egui doesn't say whether a click also focused the
//! window. On Hyprland's default (`follow_mouse = 1`) the pointer focuses the window before any
//! click, and the click lands on the item hover already lit — what the user meant. In a
//! click-to-focus setup that first click is ambiguous (the user may only want the window), so a
//! primary press that the window's focus comes with — focus arriving while the press is held
//! back, or within [`FOCUS_CLICK`] before it with no pointer motion in between (hover focus
//! always comes with motion) — is dropped with its release. A press into an unfocused window is
//! held back until its release: no focus by then (`follow_mouse = 3`, a window that never takes
//! the keyboard) and it is delivered as the click it was. A held press that turns into a drag
//! (the pointer moves past egui's click distance) is delivered at once, so scrollbars, sliders,
//! grips and window moves work there too; one held still past egui's click duration does
//! nothing, as it wouldn't be a click. Pointer motion always passes, so hover keeps pointing.

use eframe::egui;

const PRIMARY: egui::PointerButton = egui::PointerButton::Primary;

/// A primary press this soon (s) after the window gained focus is the click that focused it.
pub const FOCUS_CLICK: f64 = 0.1;

/// The double-click delay (s) both layers use, set on the context by [`install`]. egui's 0.3 s
/// default is shorter than desktops count (GTK: 0.4 s): a slower double-click would act twice.
pub const DOUBLE_CLICK_DELAY: f64 = 0.5;

/// Set egui's double-click delay to [`DOUBLE_CLICK_DELAY`] (`TunerApp::new`).
pub fn install(ctx: &egui::Context) {
    ctx.options_mut(|o| o.input_options.max_double_click_delay = DOUBLE_CLICK_DELAY);
}

/// Slack (s) on egui's double-click delay: the 2nd press arrives before its release.
const SLACK: f64 = 0.05;

fn swapped_id() -> egui::Id {
    egui::Id::new("click-guard-swapped")
}

/// This frame's click swapped what is under the pointer (see the module docs).
pub fn mark_swapped(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(swapped_id(), true));
}

#[derive(Debug, Clone, Copy)]
struct Armed {
    pos: egui::Pos2,
    /// egui time after which a press is a new click.
    until: f64,
}

#[derive(Debug)]
pub struct ClickGuard {
    /// The last primary click: egui time, position, pass number.
    click: Option<(f64, egui::Pos2, u64)>,
    /// Drop primary presses near here until then (the rest of a multi-click).
    armed: Option<Armed>,
    /// A primary press was dropped: drop its release too, keeping egui's view consistent.
    drop_release: bool,
    /// A primary press into the unfocused window, decided at its release: (event, time).
    held: Option<(egui::Event, f64)>,
    focused: bool,
    /// egui time the window last gained focus.
    focused_at: Option<f64>,
    /// The pointer moved in the window since it gained focus (hover focus, not a click).
    moved_since_focus: bool,
}

impl Default for ClickGuard {
    fn default() -> Self {
        Self { click: None, armed: None, drop_release: false, held: None, focused: true, focused_at: None, moved_since_focus: false }
    }
}

impl ClickGuard {
    /// Before egui sees the input (`raw_input_hook`): drop (or hold back) the presses described
    /// in the module docs. Input injected after this (developer screenshots) is never dropped.
    pub fn filter(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // the time egui will give this pass (`InputState::begin_pass`)
        let now = raw.time.unwrap_or_else(|| ctx.input(|i| i.time) + f64::from(raw.predicted_dt));
        let (delay, dist, longest) =
            ctx.options(|o| (o.input_options.max_double_click_delay, o.input_options.max_click_dist, o.input_options.max_click_duration));
        if !raw.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(_))) {
            self.focused = raw.focused;
        }
        let mut out = Vec::with_capacity(raw.events.len());
        for ev in std::mem::take(&mut raw.events) {
            match ev {
                egui::Event::WindowFocused(f) => {
                    self.focused = f;
                    if f {
                        self.focused_at = Some(now);
                        self.moved_since_focus = false;
                        // the held press focused the window: it and its release go
                        if self.held.take().is_some() {
                            self.drop_release = true;
                        }
                    }
                    out.push(ev);
                }
                egui::Event::PointerButton { button: PRIMARY, pressed: true, pos, .. } => {
                    let repeat = self.armed.is_some_and(|a| now <= a.until && a.pos.distance(pos) <= dist);
                    // a burst of clicks stays one: each dropped press extends the window
                    self.armed = repeat.then_some(Armed { pos, until: now + delay + SLACK });
                    let focusing = !self.moved_since_focus && self.focused_at.is_some_and(|t| now - t <= FOCUS_CLICK);
                    self.drop_release = repeat || focusing;
                    match (self.drop_release, self.focused) {
                        (true, _) => {}
                        (false, true) => out.push(ev),
                        (false, false) => self.held = Some((ev, now)),
                    }
                }
                egui::Event::PointerButton { button: PRIMARY, pressed: false, pos, .. } => {
                    if let Some((press, at)) = self.held.take() {
                        // no focus came with it: the click it was (not a drag or a long press)
                        let still = matches!(press, egui::Event::PointerButton { pos: p, .. } if p.distance(pos) <= dist);
                        if still && now - at <= longest {
                            out.push(press);
                            out.push(ev);
                        }
                    } else if !std::mem::take(&mut self.drop_release) {
                        out.push(ev);
                    }
                }
                egui::Event::PointerButton { pressed: true, .. } => {
                    self.armed = None;
                    out.push(ev);
                }
                egui::Event::PointerMoved(pos) => {
                    self.moved_since_focus = true;
                    // a held press that became a drag: deliver it now, the drag starts from it
                    let drag = matches!(self.held, Some((egui::Event::PointerButton { pos: p, .. }, _)) if p.distance(pos) > dist);
                    if drag && let Some((press, _)) = self.held.take() {
                        out.push(press);
                    }
                    out.push(ev);
                }
                _ => out.push(ev),
            }
        }
        raw.events = out;
    }

    /// At the end of the frame. `changed` = the route or overlay stack differs from the last
    /// frame's. A change in the frame of a primary click (or the next one, for anything applied
    /// a frame late) arms the drop of that multi-click's later presses.
    pub fn end_frame(&mut self, ctx: &egui::Context, changed: bool) {
        let swapped = ctx.data_mut(|d| d.remove_temp::<bool>(swapped_id())).unwrap_or(false);
        let pass = ctx.cumulative_pass_nr();
        let (now, clicked, pos) = ctx.input(|i| (i.time, i.pointer.button_clicked(PRIMARY), i.pointer.latest_pos()));
        if let (true, Some(pos)) = (clicked, pos) {
            self.click = Some((now, pos, pass));
        }
        let Some((at, pos, click_pass)) = self.click else { return };
        if pass > click_pass + 1 {
            self.click = None;
        } else if changed || swapped {
            let delay = ctx.options(|o| o.input_options.max_double_click_delay);
            self.armed = Some(Armed { pos, until: at + delay + SLACK });
            self.click = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: PRIMARY, pressed, modifiers: egui::Modifiers::NONE }
    }

    /// Run one frame through the guard (`changed` = the click changed the page); returns the
    /// events egui got.
    fn frame(g: &mut ClickGuard, ctx: &egui::Context, events: Vec<egui::Event>, dt: f32, changed: bool) -> Vec<egui::Event> {
        let mut raw = egui::RawInput { events, predicted_dt: dt, ..Default::default() };
        g.filter(ctx, &mut raw);
        let kept = raw.events.clone();
        run(ctx, raw, |_| {});
        g.end_frame(ctx, changed);
        kept
    }

    fn run(ctx: &egui::Context, raw: egui::RawInput, f: impl FnMut(&mut egui::Ui)) {
        ctx.run_ui(raw, f).textures_delta.clear(); // no renderer here
    }

    #[test]
    fn the_second_click_on_a_changed_page_is_dropped_with_its_release() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        frame(&mut g, &ctx, vec![egui::Event::PointerMoved(at)], 1.0, false);
        assert_eq!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.02, true).len(), 2);
        // the second and third clicks of the burst, a pixel off
        let near = at + egui::vec2(1.0, 1.0);
        assert!(frame(&mut g, &ctx, vec![press(near, true)], 0.1, false).is_empty());
        assert!(frame(&mut g, &ctx, vec![press(near, false)], 0.05, false).is_empty());
        assert!(frame(&mut g, &ctx, vec![press(near, true), press(near, false)], 0.2, false).is_empty());
        // later: a new click
        assert_eq!(frame(&mut g, &ctx, vec![press(near, true), press(near, false)], 0.5, false).len(), 2);
    }

    #[test]
    fn clicks_are_kept_when_nothing_changed_or_elsewhere() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        frame(&mut g, &ctx, vec![egui::Event::PointerMoved(at)], 1.0, false);
        frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.02, false);
        assert_eq!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.1, false).len(), 2, "same page: a widget's own business");
        frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.5, true);
        let far = egui::pos2(200.0, 50.0);
        assert_eq!(frame(&mut g, &ctx, vec![press(far, true), press(far, false)], 0.1, false).len(), 2, "another place");
    }

    #[test]
    fn a_screen_swap_arms_like_a_route_change() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        frame(&mut g, &ctx, vec![egui::Event::PointerMoved(at)], 1.0, false);
        let mut raw = egui::RawInput { events: vec![press(at, true), press(at, false)], predicted_dt: 0.02, ..Default::default() };
        g.filter(&ctx, &mut raw);
        run(&ctx, raw, |ui| mark_swapped(ui.ctx()));
        g.end_frame(&ctx, false);
        assert!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.1, false).is_empty());
    }

    #[test]
    fn the_click_that_focuses_the_window_is_dropped_but_motion_passes() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        let mut raw = egui::RawInput { focused: false, events: vec![egui::Event::WindowFocused(false)], ..Default::default() };
        g.filter(&ctx, &mut raw);
        run(&ctx, raw, |_| {});
        // click-to-focus: the press arrives before (or with) the focus
        let kept =
            frame(&mut g, &ctx, vec![egui::Event::PointerMoved(at), press(at, true), egui::Event::WindowFocused(true), press(at, false)], 1.0, false);
        assert!(matches!(kept[..], [egui::Event::PointerMoved(_), egui::Event::WindowFocused(true)]), "{kept:?}");
        // …or just after it
        assert_eq!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.05, false).len(), 0, "just after the focus");
        assert_eq!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.5, false).len(), 2, "focused: clicks pass");
    }

    /// A window that never takes the keyboard (`follow_mouse = 3`): its clicks still work, each
    /// delivered at its release.
    #[test]
    fn a_click_that_brings_no_focus_is_delivered_at_its_release() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        let unfocused = |events| egui::RawInput { focused: false, events, ..Default::default() };
        let mut raw = unfocused(vec![egui::Event::WindowFocused(false), press(at, true)]);
        g.filter(&ctx, &mut raw);
        assert!(raw.events.len() == 1, "held back: {:?}", raw.events);
        run(&ctx, raw, |_| {});
        let mut raw = unfocused(vec![press(at, false)]);
        g.filter(&ctx, &mut raw);
        assert!(matches!(raw.events[..], [egui::Event::PointerButton { pressed: true, .. }, egui::Event::PointerButton { pressed: false, .. }]));
        run(&ctx, raw, |_| {});
        // a drag is delivered as it starts (scrollbars, grips, sliders)…
        let far = egui::pos2(200.0, 50.0);
        let mut raw = unfocused(vec![press(at, true)]);
        g.filter(&ctx, &mut raw);
        assert!(raw.events.is_empty(), "{:?}", raw.events);
        run(&ctx, raw, |_| {});
        let mut raw = unfocused(vec![egui::Event::PointerMoved(at + egui::vec2(3.0, 0.0)), egui::Event::PointerMoved(far)]);
        g.filter(&ctx, &mut raw);
        assert!(
            matches!(raw.events[..], [egui::Event::PointerMoved(_), egui::Event::PointerButton { pressed: true, pos, .. }, egui::Event::PointerMoved(_)] if pos == at),
            "{:?}",
            raw.events
        );
        run(&ctx, raw, |_| {});
        let mut raw = unfocused(vec![press(far, false)]);
        g.filter(&ctx, &mut raw);
        assert_eq!(raw.events.len(), 1, "its release");
        run(&ctx, raw, |_| {});
        // …and a press held still too long isn't turned into a click
        let mut raw = unfocused(vec![press(far, true)]);
        g.filter(&ctx, &mut raw);
        run(&ctx, raw, |_| {});
        let mut raw = egui::RawInput { focused: false, events: vec![press(far, false)], predicted_dt: 1.0, ..Default::default() };
        g.filter(&ctx, &mut raw);
        assert!(raw.events.is_empty(), "{:?}", raw.events);
    }

    /// Hover focus (`follow_mouse = 1`) comes with motion: a quick click after it is meant.
    #[test]
    fn a_quick_click_after_hover_focus_passes() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        let at = egui::pos2(50.0, 50.0);
        let mut raw = egui::RawInput { focused: false, events: vec![egui::Event::WindowFocused(false)], ..Default::default() };
        g.filter(&ctx, &mut raw);
        run(&ctx, raw, |_| {});
        frame(&mut g, &ctx, vec![egui::Event::WindowFocused(true), egui::Event::PointerMoved(at)], 1.0, false);
        assert_eq!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.03, false).len(), 2);
    }

    /// Desktops count a double-click up to ~0.4–0.5 s (GTK: 0.4 s), longer than egui's 0.3 s.
    #[test]
    fn a_slow_double_click_on_a_changed_page_is_still_one_click() {
        let (ctx, mut g) = (egui::Context::default(), ClickGuard::default());
        install(&ctx);
        let at = egui::pos2(50.0, 50.0);
        frame(&mut g, &ctx, vec![egui::Event::PointerMoved(at)], 1.0, false);
        frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.02, true);
        assert!(frame(&mut g, &ctx, vec![press(at, true), press(at, false)], 0.45, false).is_empty());
    }
}
