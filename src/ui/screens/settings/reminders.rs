//! Settings ▸ Reminders (B6). OWNER: rui. After the pending list (built in `settings.rs`):
//! * the list's rows say what each reminder does — a switch's start offset and where it opens,
//!   "from Following · ‹rule›" — and a warning under each one that clashes with another;
//! * Scheduled playback: "Open scheduled playback in" (pop-out / full window / where I am) and
//!   "Fire while Tuner is closed" (transient systemd user timers);
//! * Sleep timer: what it does now, "Sleep timer…", "Turn off", and its warning.

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;
use crate::model::{FollowRuleId, OpenIn};

/// "Sleep timer…" (opens the menu).
const ACT_SLEEP: u16 = 1;
/// "Turn off the sleep timer".
const ACT_SLEEP_OFF: u16 = 2;

static OPEN_IN: CfgChoice = CfgChoice {
    label: "Open scheduled playback in",
    options: &["Pop-out", "Full window", "Where I am"],
    get: |c| pos(&crate::reminders::ui::OPEN_IN, &c.reminders.open_in),
    set: |c, i| c.reminders.open_in = crate::reminders::ui::OPEN_IN[i],
};
// (the `toggle_row!` / `number_row!` macros are defined after this module's `mod` line)
static WHEN_CLOSED: CfgToggle =
    CfgToggle { label: "Fire while Tuner is closed", get: |c| c.reminders.when_closed, set: |c, v| c.reminders.when_closed = v };
static SLEEP_WARN: CfgNumber = CfgNumber {
    label: "Warn before the sleep timer stops",
    unit: "s",
    min: 0.0,
    max: 600.0,
    step: 30.0,
    get: |c| f64::from(c.reminders.sleep_warn_s),
    set: |c, v| c.reminders.sleep_warn_s = v as u32,
};

thread_local! {
    /// Following rules' names by id, read once per set of ids shown (the page's rows are built
    /// every frame; a rename shows on the next reload of the list).
    static RULE_NAMES: RefCell<(Vec<FollowRuleId>, HashMap<FollowRuleId, String>)> = RefCell::default();
}

fn rule_names(db: &rusqlite::Connection, ids: Vec<FollowRuleId>) -> HashMap<FollowRuleId, String> {
    RULE_NAMES.with_borrow_mut(|(key, names)| {
        if *key != ids {
            *names = ids.iter().filter_map(|id| crate::store::follow::rule(db, *id).ok().flatten().map(|r| (*id, r.name))).collect();
            *key = ids;
        }
        names.clone()
    })
}

/// What a reminder does, after the list's "channel · day time · date": " · 10 min in · opens
/// in the pop-out · from Following · Arsenal".
fn extra(r: &Reminder, default: OpenIn, rules: &HashMap<FollowRuleId, String>) -> String {
    let mut s = String::new();
    if r.offset_s != 0 {
        s.push_str(" · ");
        s.push_str(&crate::reminders::ui::offset_label(r.offset_s / 60).to_lowercase());
    }
    if r.switches() {
        let place = match crate::reminders::open_in(r, default) {
            OpenIn::PopOut => "opens in the pop-out",
            OpenIn::FullWindow => "opens in the full window",
            OpenIn::WhereIAm => "plays where you are",
        };
        s.push_str(" · ");
        s.push_str(place);
    }
    if let Some(rule) = r.rule_id {
        s.push_str(" · from Following");
        if let Some(name) = rules.get(&rule) {
            s.push_str(" · ");
            s.push_str(name);
        }
    }
    s
}

/// The pending list's rows annotated (what each does, clashes), then the page's own rows.
pub(super) fn rows(s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    if !crate::reminders::ENABLED {
        return;
    }
    let (h24, now, default) = (ctx.config.interface.clock_24h, ctx.now, ctx.config.reminders.open_in);
    let shown: Vec<&Reminder> = s.reminders.iter().filter(|r| !ctx.lib.conceals(&r.channel)).collect();
    let mut ids: Vec<FollowRuleId> = shown.iter().filter_map(|r| r.rule_id).collect();
    ids.sort_unstable_by_key(|id| id.0);
    ids.dedup();
    let rules = if ids.is_empty() { HashMap::new() } else { rule_names(ctx.db, ids) };
    // clashes among the shown ones (a hidden explicit channel's reminder is never named)
    let pending: Vec<Reminder> = shown.iter().map(|r| (*r).clone()).collect();
    let clashes = crate::reminders::clashes(&pending);
    let guard = ctx.services.agent.guard();
    let mut out = Vec::with_capacity(v.len() + clashes.len());
    for row in v.drain(..) {
        let id = match &row {
            Row::Item { item: Item::Reminder(id), .. } => Some(*id),
            _ => None,
        };
        let Some(id) = id else {
            out.push(row);
            continue;
        };
        let Some(r) = pending.iter().find(|r| r.id == id) else {
            out.push(row);
            continue;
        };
        let row = match row {
            Row::Item { item, icon, color, label, detail, reorder } => {
                Row::Item { item, icon, color, label, detail: Some(format!("{}{}", detail.unwrap_or_default(), extra(r, default, &rules))), reorder }
            }
            other => other,
        };
        out.push(row);
        for c in clashes.iter().filter(|c| c.a == id || c.b == id) {
            let other = if c.a == id { c.b } else { c.a };
            let Some(o) = pending.iter().find(|r| r.id == other) else { continue };
            let title =
                if o.programme_title.trim().is_empty() { o.channel_name.clone() } else { guard.outside_title(&o.programme_title, now).into_owned() };
            out.push(Row::Note(format!("{} {}", glyphs::WARNING, crate::reminders::ui::clash_text(o, c.kind, &title, h24)), true));
        }
    }
    *v = out;

    v.push(Row::Section("Scheduled playback".into()));
    v.push(Row::Choice(&OPEN_IN));
    v.push(Row::Note(
        "Where automatic switches, Following's switches and news flashes play. The pop-out stays on every workspace and never takes the keyboard from what you're doing; if Tuner is in front and playing, it switches in place.".into(),
        false,
    ));
    v.push(Row::Toggle(&WHEN_CLOSED));
    v.push(Row::Note(
        "Reminders up to 48 hours ahead fire through systemd user timers, named only \"Tuner reminder\": a note shows, or Tuner starts for a switch."
            .into(),
        false,
    ));

    v.push(Row::Section("Sleep timer".into()));
    let status = ctx.services.agent.sleep().and_then(|h| h.status());
    v.push(Row::Note(
        match &status {
            Some(st) => crate::reminders::ui::sleep_status_text(st, now, h24),
            None => "Off. Ctrl+Shift+T sets it while something plays.".into(),
        },
        false,
    ));
    v.push(Row::Act { act: Act::Feature(Feature::Reminders, ACT_SLEEP), icon: glyphs::SLEEP, label: "Sleep timer…".into(), danger: false });
    if status.is_some() {
        v.push(Row::Act {
            act: Act::Feature(Feature::Reminders, ACT_SLEEP_OFF),
            icon: glyphs::CLOSE,
            label: "Turn off the sleep timer".into(),
            danger: true,
        });
    }
    v.push(Row::Number(&SLEEP_WARN));
}

/// Its `Act::Feature(Feature::Reminders, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    match n {
        ACT_SLEEP => ctx.open(OverlayRequest::SleepTimer),
        ACT_SLEEP_OFF if ctx.services.agent.sleep().is_some_and(|h| h.cancel()) => ctx.toast(Toast::info("Sleep timer off")),
        _ => {}
    }
}

/// Enter on its `Item::Feature(Feature::Reminders, id)` rows (none: reminder rows are the
/// list's own `Item::Reminder`).
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, ReminderMode};
    use crate::ui::screens::home::testkit::{self, harness, has_label_containing, rig, seed, until};

    /// The list says what each reminder does — a switch's offset and where it opens, the
    /// Following rule that made it — and warns under the ones that clash; the page offers
    /// "Open scheduled playback in".
    #[test]
    fn the_list_says_what_each_reminder_does_and_what_clashes() {
        let (mut t, one) = rig();
        seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            testkit::channels(c, s, 2)?;
            c.execute(
                "INSERT INTO follow_rule(id, name, kind, spec, mode, priority, created_at, updated_at) VALUES (7, 'Arsenal', 'team', '{}', 'switch', 0, 0, 0)",
                [],
            )?;
            let r = |i: usize| ItemRef { source: s, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
            let mut game = Reminder::notify(r(0), "Channel 0".into(), "The Big Match".into(), 4_000_000_000, 4_000_007_200, 120);
            (game.mode, game.lead_s, game.offset_s, game.rule_id) = (ReminderMode::Switch, Reminder::SWITCH_LEAD, 600, Some(FollowRuleId(7)));
            crate::store::user::add_reminder(c, &game)?;
            let film = Reminder::notify(r(1), "Channel 1".into(), "Late Film".into(), 4_000_003_600, 4_000_010_800, 120);
            crate::store::user::add_reminder(c, &film)?;
            Ok(())
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1400.0, 1000.0));
        h.state_mut().enter(crate::ui::Route::Settings(crate::ui::SettingsPage::Reminders));
        until(&mut h, "the reminders", |h| has_label_containing(h, "The Big Match"));
        for text in [
            "10 min in · opens in the pop-out · from Following · Arsenal",
            "Clashes with Late Film on Channel 1 at ",
            "Clashes with The Big Match on Channel 0 at ",
            "Open scheduled playback in",
            "Fire while Tuner is closed",
        ] {
            assert!(has_label_containing(&h, text), "{text}");
        }
    }
}
