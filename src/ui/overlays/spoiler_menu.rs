//! The spoiler guard menu (Ctrl+Shift+B; DESIGN §34 §2.3). OWNER: S-ui.
//!
//! A launcher card (the context menu's look, type-to-filter, Enter chooses, a right-click opens
//! it at the pointer). Rows, by what it was opened on (`spoiler::ui::MenuFor`):
//! * anywhere: Show everything for 15 minutes / 1 hour / until Tuner restarts (paused: "Guard
//!   again now" first), Undo of an event marked seen in the last half hour, Guard a topic or
//!   team…, Spoiler guard settings…;
//! * on an item: Show spoilers here (Ctrl+B), Mark seen (Ctrl+W), I'll watch "…" later…, Guard
//!   spoilers about…, Stop guarding ‹topic›, Guard / Don't guard ‹series›;
//! * on a Settings rule, series or seen row: Edit…, Turn off / on, Delete; Guard again / Don't
//!   guard; Undo.

use eframe::egui;

use super::dialogs::ContextMenuOverlay;
use super::spoiler_rule::SpoilerRuleSpec;
use super::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::keymap::{Action, KeyContext};
use crate::model::{SeenRow, SpoilerRuleId, Timestamp};
use crate::msg::{DataScope, Toast};
use crate::spoiler::ui::{Focus, MenuFor};
use crate::ui::widgets::glyphs;
use crate::ui::widgets::spoiler::SHOWN;
use crate::ui::{OverlayRequest, Route, SettingsPage, UiCtx};

/// nf-md-undo / nf-md-pencil.
const UNDO: &str = "\u{f054c}";
const EDIT: &str = "\u{f03eb}";

/// A row: icon, label, shortcut hint, meaning.
type Entry = (&'static str, String, Option<String>, Choice);

/// One row's meaning (rows carry `Action::Digit(i)` into this list).
#[derive(Debug, Clone, PartialEq)]
enum Choice {
    Pause(Option<i64>),
    Resume,
    Undo(SeenRow),
    Reveal,
    MarkSeen,
    /// "I'll watch it later…": the editor as a blackout from the programme.
    Later,
    /// "Guard spoilers about…" (prefilled from the focused title, or empty).
    GuardTopic(String),
    StopTopic(SpoilerRuleId, String),
    Series {
        norm: String,
        label: String,
        guard: bool,
    },
    Settings,
    Edit(SpoilerRuleId),
    Enable(SpoilerRuleId, bool),
    Delete(SpoilerRuleId, String),
}

/// The menu (its subject is taken from `spoiler::ui::open_menu`; none: the plain menu).
pub struct SpoilerMenu {
    for_: MenuFor,
    built: Option<(ContextMenuOverlay, Vec<Choice>)>,
    at: Option<egui::Pos2>,
}

impl Default for SpoilerMenu {
    fn default() -> Self {
        Self { for_: crate::spoiler::ui::take_menu().unwrap_or(MenuFor::Item(None)), built: None, at: None }
    }
}

fn row(icon: &'static str, label: impl Into<String>, shortcut: Option<String>, i: usize) -> MenuItem {
    MenuItem { icon, label: label.into(), action: Action::Digit(i as u8), enabled: true, shortcut }
}

fn key(a: Action) -> Option<String> {
    crate::keymap::GLOBAL.iter().chain(crate::keymap::GUIDE.iter()).find(|b| b.action == a).map(|b| b.chord.label())
}

/// "Arsenal v Chelsea" shortened for a menu row.
fn short(s: &str) -> String {
    crate::ui::widgets::button::short_label(s, 40).into_owned()
}

impl SpoilerMenu {
    /// The rows and their meanings.
    fn choices(&self, ctx: &UiCtx<'_>) -> (String, Vec<Entry>) {
        let guard = ctx.services.agent.guard();
        let now = ctx.now;
        let mut v: Vec<Entry> = Vec::new();
        match &self.for_ {
            MenuFor::Item(focus) => {
                if guard.paused_until(now).is_some() {
                    v.push((glyphs::SPOILER, "Guard again now".into(), None, Choice::Resume));
                } else {
                    v.push((SHOWN, "Show everything for 15 minutes".into(), None, Choice::Pause(Some(15 * 60))));
                    v.push((SHOWN, "Show everything for 1 hour".into(), None, Choice::Pause(Some(3600))));
                    v.push((SHOWN, "Show everything until Tuner restarts".into(), None, Choice::Pause(None)));
                }
                for r in crate::spoiler::ui::recent_seen(&guard, now).into_iter().take(2) {
                    let t = guard.outside_title(&r.title, now).into_owned();
                    v.push((UNDO, format!("Undo: “{}” marked seen", short(&t)), None, Choice::Undo(r)));
                }
                if let Some(f) = focus {
                    self.item_rows(ctx, f, &mut v);
                } else {
                    v.push((glyphs::ADD, "Guard a topic or team…".into(), None, Choice::GuardTopic(String::new())));
                }
                v.push((glyphs::SETTINGS, "Spoiler guard settings…".into(), None, Choice::Settings));
                ("Spoiler guard".into(), v)
            }
            MenuFor::Rule(r) => {
                if r.kind != crate::model::SpoilerRuleKind::Series {
                    v.push((EDIT, "Edit…".into(), None, Choice::Edit(r.id)));
                }
                let (icon, label) = if r.enabled { (SHOWN, "Turn off") } else { (glyphs::SPOILER, "Turn on") };
                v.push((icon, label.into(), None, Choice::Enable(r.id, !r.enabled)));
                v.push((glyphs::CLOSE, "Delete".into(), None, Choice::Delete(r.id, r.label.clone())));
                (r.label.clone(), v)
            }
            MenuFor::Series { series_norm, label, guarded } => {
                let (icon, text) = if *guarded { (SHOWN, "Don't guard this series") } else { (glyphs::SPOILER, "Guard this series") };
                v.push((icon, text.into(), None, Choice::Series { norm: series_norm.clone(), label: label.clone(), guard: !guarded }));
                (label.clone(), v)
            }
            MenuFor::Seen(r) => {
                v.push((UNDO, "Undo — not seen yet (guard it again)".into(), None, Choice::Undo(r.clone())));
                (guard.outside_title(&r.title, now).into_owned(), v)
            }
        }
    }

    /// The rows about the focused item.
    fn item_rows(&self, ctx: &UiCtx<'_>, f: &Focus, v: &mut Vec<Entry>) {
        let guard = ctx.services.agent.guard();
        let now = ctx.now;
        if f.hidden {
            v.push((SHOWN, "Show spoilers here".into(), key(Action::RevealSpoilers), Choice::Reveal));
        }
        if let Some(p) = &f.programme {
            let subj = crate::ui::widgets::spoiler::programme_subject(p);
            if matches!(f.target, crate::ui::item_menu::Target::Programme { .. }) && crate::spoiler::verdict::tracks(&guard.snapshot(), &subj, now) {
                v.push((glyphs::CHECK, "Mark seen".into(), key(Action::MarkWatched), Choice::MarkSeen));
            }
            let title = guard.outside_title(&p.title, now).into_owned();
            if p.stop > now {
                v.push((glyphs::REMINDER, format!("I'll watch “{}” later…", short(&title)), None, Choice::Later));
            }
            v.push((glyphs::ADD, "Guard spoilers about…".into(), None, Choice::GuardTopic(p.title.clone())));
        } else {
            v.push((glyphs::ADD, "Guard a topic or team…".into(), None, Choice::GuardTopic(String::new())));
        }
        // the topic hiding this item: stop guarding it (a Following rule's lives in Following)
        if let Some(r) = &f.reason
            && r.kind != crate::guard::ReasonKind::Series
            && let Some(id) = guard.topics(now).into_iter().find(|t| t.label == r.rule).and_then(|t| t.rule)
        {
            v.push((SHOWN, format!("Stop guarding {}", short(&r.rule)), None, Choice::StopTopic(id, r.rule.clone())));
        }
        if let Some((norm, label)) = &f.series {
            let guarded = guard.snapshot().series.contains_key(norm);
            let text = if guarded { format!("Don't guard {}", short(label)) } else { format!("Guard {} after the episode you're on", short(label)) };
            v.push((
                if guarded { SHOWN } else { glyphs::SPOILER },
                text,
                None,
                Choice::Series { norm: norm.clone(), label: label.clone(), guard: !guarded },
            ));
        }
    }

    fn build(&mut self, ctx: &UiCtx<'_>) -> &mut (ContextMenuOverlay, Vec<Choice>) {
        if self.built.is_none() {
            let (title, rows) = self.choices(ctx);
            let items = rows.iter().enumerate().map(|(i, (icon, label, k, _))| row(icon, label.clone(), k.clone(), i)).collect();
            let mut menu = ContextMenuOverlay::new(ContextMenuSpec { title, items });
            if let Some(at) = self.at {
                menu.anchor_at(at);
            }
            self.built = Some((menu, rows.into_iter().map(|r| r.3).collect()));
        }
        self.built.as_mut().expect("built")
    }

    /// The inner menu's outcome: a choice is acted on here, never handed to the screen.
    fn settle(&mut self, ctx: &mut UiCtx<'_>, outcome: OverlayOutcome) -> OverlayOutcome {
        match outcome {
            OverlayOutcome::CloseWith(OverlayResult::Chosen(Action::Digit(i))) => {
                let choice = self.built.as_ref().and_then(|(_, c)| c.get(i as usize).cloned());
                if let Some(c) = choice {
                    self.run(ctx, c);
                }
                OverlayOutcome::Close
            }
            OverlayOutcome::CloseWith(_) => OverlayOutcome::Close,
            o => o,
        }
    }

    fn run(&self, ctx: &mut UiCtx<'_>, c: Choice) {
        let guard = ctx.services.agent.guard();
        let now = ctx.now;
        let focus = match &self.for_ {
            MenuFor::Item(f) => f.as_deref(),
            _ => None,
        };
        let writer = ctx.services.store.writer();
        match c {
            Choice::Pause(secs) => {
                guard.pause(secs.map(|s| now + s));
                crate::spoiler::ui::hide_all();
                let how = match secs {
                    Some(s) if s < 3600 => format!("for {} minutes", s / 60),
                    Some(_) => "for an hour".into(),
                    None => "until Tuner restarts".into(),
                };
                ctx.toast(Toast::info(format!("Spoilers shown {how} — {} to guard again", key(Action::SpoilerMenu).unwrap_or_default())));
            }
            Choice::Resume => {
                guard.resume();
                ctx.toast(Toast::info("Spoiler guard back on"));
            }
            Choice::Undo(r) => crate::spoiler::ui::unmark_seen(ctx, &r),
            Choice::Reveal => {
                if let Some(f) = focus {
                    crate::spoiler::ui::reveal_focus(f);
                }
            }
            Choice::MarkSeen => {
                if let Some(p) = focus.and_then(|f| f.programme.as_ref()) {
                    crate::spoiler::ui::mark_seen(ctx, &guard, p);
                }
            }
            Choice::Later => {
                if let Some(p) = focus.and_then(|f| f.programme.as_ref()) {
                    ctx.open(OverlayRequest::SpoilerRule(SpoilerRuleSpec {
                        rule: None,
                        initial: p.title.clone(),
                        sub_title: p.sub_title.clone(),
                        later: Some(p.stop),
                    }));
                }
            }
            Choice::GuardTopic(initial) => {
                let sub_title = focus.and_then(|f| f.programme.as_ref()).and_then(|p| p.sub_title.clone());
                ctx.open(OverlayRequest::SpoilerRule(SpoilerRuleSpec { rule: None, initial, sub_title, later: None }))
            }
            Choice::StopTopic(id, label) => {
                let _ = writer.submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::set_enabled(c, id, false));
                ctx.toast(Toast::info(format!("Stopped guarding {label} (Settings ▸ Spoiler guard turns it back on)")));
            }
            Choice::Series { norm, label, guard: on } => {
                let l = label.clone();
                let _ = writer
                    .submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::set_series_guard(c, &norm, &l, on, false, now).map(|_| ()));
                ctx.toast(Toast::info(if on {
                    format!("Guarding {label} after the episode you're on")
                } else {
                    format!("{label} isn't guarded any more")
                }));
            }
            Choice::Settings => ctx.navigate(crate::ui::NavRequest::Switch(Route::Settings(SettingsPage::SpoilerGuard))),
            Choice::Edit(id) => {
                ctx.open(OverlayRequest::SpoilerRule(SpoilerRuleSpec { rule: Some(id), initial: String::new(), sub_title: None, later: None }))
            }
            Choice::Enable(id, on) => {
                let _ = writer.submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::set_enabled(c, id, on));
            }
            Choice::Delete(id, label) => {
                let _ = writer.submit_notify(DataScope::Spoiler, move |c| crate::store::spoiler::remove_rule(c, id).map(|_| ()));
                ctx.toast(Toast::info(format!("Deleted “{label}”")));
            }
        }
    }
}

impl Overlay for SpoilerMenu {
    fn key_contexts(&self) -> Vec<KeyContext> {
        match &self.built {
            Some((m, _)) => m.key_contexts(),
            None => vec![KeyContext::List, KeyContext::Modal],
        }
    }
    fn anchor_at(&mut self, pos: egui::Pos2) {
        self.at = Some(pos);
        if let Some((m, _)) = &mut self.built {
            m.anchor_at(pos);
        }
    }
    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        // Ctrl+Shift+B again closes it
        if action == Action::SpoilerMenu {
            return OverlayOutcome::Close;
        }
        let o = self.build(ctx).0.on_action(ctx, action);
        self.settle(ctx, o)
    }
    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let o = self.build(ctx).0.ui(egui_ctx, ctx);
        self.settle(ctx, o)
    }
}

/// When an "I'll watch it later" from a programme ending at `stop` lifts by default.
pub fn later_until(stop: Timestamp, now: Timestamp) -> Timestamp {
    stop.max(now) + 24 * 3600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::Guard;
    use crate::ui::testing::TestUi;

    /// Ctrl+Shift+B, Enter: everything shows for 15 minutes; the menu then offers to guard
    /// again first.
    #[test]
    fn the_first_row_pauses_for_15_minutes_then_guards_again() {
        let mut t = TestUi::new().unwrap();
        let guard = Guard::live();
        t.services().agent.set_guard(guard.clone());
        let now = t.now;
        let mut m = SpoilerMenu::default();
        assert!(matches!(t.with_ctx(|ctx| m.on_action(ctx, Action::Activate)), OverlayOutcome::Close));
        assert_eq!(guard.paused_until(now), Some(now + 15 * 60));
        let mut m = SpoilerMenu::default();
        t.with_ctx(|ctx| m.on_action(ctx, Action::Activate));
        assert!(guard.paused_until(now).is_none());
    }
}
