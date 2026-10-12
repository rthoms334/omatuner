//! Settings ▸ Spoiler guard (DESIGN §34 §2.3). OWNER: S-ui. Listed once `spoiler::ENABLED`.
//!
//! ```text
//! SPOILER GUARD
//! Spoiler guard                                          [ On  ]
//! Series I'm part-way through …                          [ On  ]
//! Sport I follow (new follow rules guard spoilers)       [ On  ]
//! Hide scores in every sport title                       [ Off ]
//! Don't tune guarded channels when zapping               [ On  ]
//! Guard time after it airs                               ‹ 48 h ›
//! RULES                                         [+ Guard a topic or team…]
//! 󰈉 Arsenal      topic · Sport · until seen, at most 48 h · guarding now: Arsenal v Chelsea (Sat 17:30)
//! 󰈉 Severance    series · automatic · you're on S2E4
//! 󰈈 The Traitors series · not guarded (your choice)
//! SEEN RECENTLY
//! Arsenal v Chelsea   watched live · Sat 19:28 · Enter to undo
//! ```
//! Enter on a rule opens its menu (Edit…, Turn off, Delete); on a series, Guard / Don't guard;
//! on a seen event, Undo. The rules are the user's own words; the series and seen events come
//! from the guard, which never holds explicit content.

use super::*;
use crate::model::{SeenHow, SeenRow, SpoilerMode, SpoilerRule, SpoilerRuleKind};
use crate::spoiler::ui::MenuFor;

/// Settings rows' ids: rules by their own (positive) id, the guard's automatic series and the
/// seen events by position (negative).
const SEEN_BASE: i64 = -1_000_000;

/// `Act::Feature(Feature::Spoiler, n)`.
const ACT_ADD: u16 = 0;
const ACT_PAUSE: u16 = 1;

/// The page's state (loaded rows), kept in `SettingsScreen`.
#[derive(Default)]
pub(super) struct State {
    rules: Vec<SpoilerRule>,
    seen: Vec<SeenRow>,
    error: Option<String>,
}

static TOGGLES: [CfgToggle; 5] = [
    CfgToggle { label: "Spoiler guard", get: |c| c.spoiler.enabled, set: |c, v| c.spoiler.enabled = v },
    CfgToggle {
        label: "Series I'm part-way through (titles and plots after the next episode)",
        get: |c| c.spoiler.series_auto,
        set: |c, v| c.spoiler.series_auto = v,
    },
    CfgToggle {
        label: "Sport I follow (new follow rules guard spoilers)",
        get: |c| c.follow.spoiler_safe_default,
        set: |c, v| c.follow.spoiler_safe_default = v,
    },
    CfgToggle {
        label: "Hide scores in every sport title",
        get: |c| c.spoiler.hide_scores_everywhere,
        set: |c, v| c.spoiler.hide_scores_everywhere = v,
    },
    CfgToggle { label: "Don't tune guarded channels when zapping", get: |c| c.spoiler.shield_zap, set: |c, v| c.spoiler.shield_zap = v },
];

static WINDOW: CfgNumber = CfgNumber {
    label: "Guard time after it airs",
    unit: "h (0 = until watched)",
    min: 0.0,
    max: 168.0,
    step: 6.0,
    get: |c| f64::from(c.spoiler.default_window_h),
    set: |c, v| c.spoiler.default_window_h = v.round().max(0.0) as u32,
};

/// The series the guard covers by itself (part-way through), by name.
fn auto_series(ctx: &UiCtx<'_>) -> Vec<(String, crate::spoiler::snapshot::SeriesMark)> {
    let snap = ctx.services.agent.guard().snapshot();
    let mut v: Vec<(String, crate::spoiler::snapshot::SeriesMark)> =
        snap.series.iter().filter(|(_, m)| m.automatic && m.rule.is_none()).map(|(k, m)| (k.clone(), m.clone())).collect();
    // every alias of a series points at its mark: one row each
    v.sort_by(|a, b| a.1.label.cmp(&b.1.label).then_with(|| a.0.cmp(&b.0)));
    v.dedup_by(|a, b| a.1.label == b.1.label);
    v
}

fn se(next_up: (i32, i32)) -> String {
    format!("you're on S{}E{}", next_up.0, next_up.1)
}

/// "topic · Sport · until seen, at most 48 h · guarding now: Arsenal v Chelsea (Sat 17:30)".
fn rule_detail(
    r: &SpoilerRule,
    status: Option<&crate::guard::TopicStatus>,
    series: Option<&crate::spoiler::snapshot::SeriesMark>,
    h24: bool,
) -> String {
    let when = |t| crate::ui::widgets::time::day_time(t, h24);
    let mut parts: Vec<String> = Vec::new();
    match r.kind {
        SpoilerRuleKind::Series if !r.enabled => return "series · not guarded (your choice)".into(),
        SpoilerRuleKind::Series => {
            parts.push("series · guarded (yours)".into());
            if let Some(m) = series {
                parts.push(se(m.next_up));
            }
            return parts.join(" · ");
        }
        SpoilerRuleKind::Blackout => {
            parts.push("I'll watch it later".into());
            if let Some(u) = r.until_at {
                parts.push(format!("until {}", when(u)));
            }
        }
        SpoilerRuleKind::Topic => parts.push("topic".into()),
    }
    parts.push(if r.categories.iter().any(|c| c.to_lowercase().starts_with("sport")) { "Sport".into() } else { "anything".into() });
    if r.kind == SpoilerRuleKind::Topic {
        parts.push(match (r.mode, r.window_h) {
            (SpoilerMode::Always, _) => "always".into(),
            (_, 0) => "until seen".into(),
            (_, h) => format!("until seen, at most {h} h"),
        });
    }
    if r.by_agent {
        parts.push(format!("added by {}", r.agent.name()));
    }
    if !r.enabled {
        parts.push("off".into());
    } else if let Some(s) = status {
        match (&s.event, s.active) {
            (Some((t, at)), true) => parts.push(format!("guarding now: {t} ({})", when(*at))),
            (Some((t, at)), false) => parts.push(format!("next: {t} ({})", when(*at))),
            (None, true) => parts.push("guarding now".into()),
            (None, false) => {}
        }
    }
    parts.join(" · ")
}

fn how(r: &SeenRow) -> String {
    match r.how {
        SeenHow::Live => "watched live".into(),
        SeenHow::Catchup => "watched on catch-up".into(),
        SeenHow::Vod => "watched".into(),
        SeenHow::Marked => "marked seen".into(),
        SeenHow::Agent => format!("marked seen by {}", r.agent.name()),
    }
}

/// The Spoiler guard page's rows.
pub(super) fn rows(s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    let st = &s.spoiler;
    let guard = ctx.services.agent.guard();
    let now = ctx.now;
    let h24 = ctx.config.interface.clock_24h;
    let item = |id: i64, icon: &str, label: String, detail: String| Row::Item {
        item: Item::Feature(Feature::Spoiler, id),
        icon: icon.to_owned(),
        color: None,
        label,
        detail: Some(detail),
        reorder: false,
    };
    v.push(Row::Section("Spoiler guard".into()));
    v.push(Row::Note(
        format!(
            "Hides what could spoil a match or an episode you haven't watched yet — in the guide, search, Home, notifications and your agent's answers. {} shows what's hidden on the item you're on; {} pauses it.",
            crate::spoiler::ui::reveal_key_label(),
            crate::keymap::GLOBAL.iter().find(|b| b.action == crate::keymap::Action::SpoilerMenu).map(|b| b.chord.label()).unwrap_or_default()
        ),
        false,
    ));
    v.extend(TOGGLES.iter().map(Row::Toggle));
    v.push(Row::Number(&WINDOW));
    let paused = guard.paused_until(now);
    let pause_label = match paused {
        Some(crate::model::Timestamp::MAX) => {
            v.push(Row::Note("Spoilers are shown everywhere until Tuner restarts.".into(), true));
            "Guard again now"
        }
        Some(t) => {
            v.push(Row::Note(format!("Spoilers are shown everywhere until {}.", crate::ui::widgets::time::hhmm(t, h24)), true));
            "Guard again now"
        }
        None => "Show everything for 15 minutes",
    };
    v.push(Row::Act {
        act: Act::Feature(Feature::Spoiler, ACT_PAUSE),
        icon: crate::ui::widgets::spoiler::SHOWN,
        label: pause_label.into(),
        danger: false,
    });

    v.push(Row::Section("Rules".into()));
    v.push(Row::Act { act: Act::Feature(Feature::Spoiler, ACT_ADD), icon: glyphs::ADD, label: "Guard a topic or team…".into(), danger: false });
    if let Some(e) = &st.error {
        v.push(Row::Note(e.clone(), true));
    }
    let topics = guard.topics(now);
    let snap = guard.snapshot();
    for r in &st.rules {
        let status = topics.iter().find(|t| t.rule == Some(r.id));
        let mark = r.series_norm.as_deref().and_then(|n| snap.series.get(n));
        let icon = if r.enabled { crate::ui::widgets::spoiler::HIDDEN } else { crate::ui::widgets::spoiler::SHOWN };
        v.push(item(r.id.0, icon, r.label.clone(), rule_detail(r, status, mark, h24)));
    }
    // Following's spoiler-safe rules guard their teams too (edited in Following)
    for t in topics.iter().filter(|t| t.rule.is_none()) {
        let now_text = match (&t.event, t.active) {
            (Some((e, at)), true) => format!(" · guarding now: {e} ({})", crate::ui::widgets::time::day_time(*at, h24)),
            _ => String::new(),
        };
        v.push(Row::Note(format!("★ {} — from Following (spoiler-safe){now_text}", t.label), false));
    }
    for (i, (_, m)) in auto_series(ctx).iter().enumerate() {
        v.push(item(-(i as i64) - 1, crate::ui::widgets::spoiler::HIDDEN, m.label.clone(), format!("series · automatic · {}", se(m.next_up))));
    }
    if st.rules.is_empty() && topics.is_empty() && snap.series.is_empty() {
        v.push(Row::Note(
            "Nothing guarded yet. Series you're part-way through are guarded by themselves; Ctrl+Shift+B on a match adds it.".into(),
            false,
        ));
    }
    if !st.seen.is_empty() {
        v.push(Row::Section("Seen recently".into()));
        for (i, r) in st.seen.iter().enumerate() {
            let title = guard.outside_title(&r.title, now).into_owned();
            let detail = format!("{} · {} · Enter to undo", how(r), crate::ui::widgets::time::day_time(r.seen_at, h24));
            v.push(item(SEEN_BASE - i as i64, glyphs::CHECK, title, detail));
        }
    }
}

/// Its `Act::Feature(Feature::Spoiler, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    match n {
        ACT_ADD => ctx.open(OverlayRequest::SpoilerRule(crate::ui::overlays::spoiler_rule::SpoilerRuleSpec {
            rule: None,
            initial: String::new(),
            sub_title: None,
            later: None,
        })),
        ACT_PAUSE => {
            let guard = ctx.services.agent.guard();
            if guard.paused_until(ctx.now).is_some() {
                guard.resume();
                ctx.toast(Toast::info("Spoiler guard back on"));
            } else {
                guard.pause(Some(ctx.now + 15 * 60));
                ctx.toast(Toast::info("Spoilers shown for 15 minutes"));
            }
        }
        _ => {}
    }
}

/// Enter on its `Item::Feature(Feature::Spoiler, id)` rows: the row's menu.
pub(super) fn item(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) {
    let for_ = if id > 0 {
        s.spoiler.rules.iter().find(|r| r.id.0 == id).cloned().map(MenuFor::Rule)
    } else if id <= SEEN_BASE {
        s.spoiler.seen.get((SEEN_BASE - id) as usize).cloned().map(MenuFor::Seen)
    } else {
        auto_series(ctx).into_iter().nth((-id - 1) as usize).map(|(norm, m)| MenuFor::Series { series_norm: norm, label: m.label, guarded: true })
    };
    if let Some(f) = for_ {
        crate::spoiler::ui::open_menu(ctx, f);
    }
}

/// The page became current (or its data changed): read the rules and the last seen events
/// (two small tables, one indexed read each).
pub(super) fn enter(s: &mut SettingsScreen, ctx: &UiCtx<'_>) {
    let st = &mut s.spoiler;
    st.error = None;
    match crate::store::spoiler::list_rules(ctx.db) {
        Ok(mut rules) => {
            // topics and "later"s first, then series rows, each by name
            rules.sort_by(|a, b| (a.kind == SpoilerRuleKind::Series).cmp(&(b.kind == SpoilerRuleKind::Series)).then_with(|| a.label.cmp(&b.label)));
            st.rules = rules;
        }
        Err(e) => st.error = Some(format!("Couldn't read the rules: {}", crate::redact::redact(&e.to_string()))),
    }
    st.seen = crate::store::spoiler::seen_recent(ctx.db, 5).unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SpoilerRuleId;
    use crate::ui::screens::home::testkit::{harness, has_label, rig, seed, until};

    fn arsenal() -> SpoilerRule {
        SpoilerRule {
            id: SpoilerRuleId(0),
            kind: SpoilerRuleKind::Topic,
            label: "Arsenal".into(),
            terms: vec!["Arsenal".into()],
            series_norm: None,
            categories: vec!["Sport".into()],
            mode: SpoilerMode::UntilSeen,
            window_h: 48,
            until_at: None,
            sport_descr: true,
            shield: true,
            enabled: true,
            by_agent: false,
            agent: Default::default(),
            created_at: 0,
        }
    }

    /// The page lists the rules with what they guard and the events seen lately; Enter on a
    /// rule opens its menu, whose Delete removes it.
    #[test]
    fn rules_and_seen_events_are_listed_and_a_rule_menu_deletes() {
        let (mut t, one) = rig();
        let id = seed(&mut t, |c| {
            let id = crate::store::spoiler::add_rule(c, &arsenal(), 0)?;
            let seen = SeenRow {
                event_key: "arsenal v chelsea@2026-10-10".into(),
                first_start: 1_791_576_000,
                title: "Arsenal v Chelsea".into(),
                series_norm: None,
                season: None,
                episode: None,
                seen_at: 1_791_583_000,
                how: SeenHow::Live,
                agent: Default::default(),
            };
            crate::store::spoiler::mark_seen(c, &seen)?;
            Ok(id)
        });
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1600.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::SpoilerGuard));
        until(&mut h, "the rules", |h| has_label(h, "Arsenal"));
        assert!(has_label(&h, "Arsenal v Chelsea"), "seen recently");
        // the page fits at 790 and 1600 pt with 12 and 16 px text
        for (w, scale) in [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)] {
            use egui_kittest::kittest::Queryable;
            let base = crate::theme::Theme::fallback();
            let theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.state_mut().t.layout = crate::ui::LayoutClass::from_width(w, theme.rem());
            h.state_mut().t.theme = theme;
            h.set_size(egui::vec2(w, 1000.0));
            h.run_steps(3);
            for node in h.query_all_by_label_contains("") {
                let r = node.rect();
                assert!(r.left() >= -1.0 && r.right() <= w + 1.0, "at {w} pt × {scale}: {node:?}");
            }
        }
        h.state_mut().t.layout = crate::ui::LayoutClass::Wide;
        h.set_size(egui::vec2(1600.0, 1000.0));
        h.run_steps(2);
        // Enter on the rule: its menu
        let app = h.state_mut();
        let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
        app.s.row = rows.iter().position(|r| matches!(r, Row::Item { label, .. } if label == "Arsenal")).unwrap();
        app.s.pane = Pane::Content;
        app.act(crate::keymap::Action::Activate);
        h.run_steps(1); // Enter acts in the next frame
        let rule = match crate::spoiler::ui::take_menu() {
            Some(MenuFor::Rule(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rule.id, id);
        crate::spoiler::ui::with_menu_for_test(MenuFor::Rule(rule));
        let mut menu = crate::ui::overlays::spoiler_menu::SpoilerMenu::default();
        let app = h.state_mut();
        // Edit…, Turn off, Delete
        for a in [crate::keymap::Action::Down, crate::keymap::Action::Down, crate::keymap::Action::Activate] {
            app.t.with_ctx(|ctx| crate::ui::overlays::Overlay::on_action(&mut menu, ctx, a));
        }
        let rules = app.t.services().store.writer().blocking(|c| crate::store::spoiler::list_rules(c)).unwrap();
        assert!(rules.is_empty(), "{rules:?}");
    }
}
