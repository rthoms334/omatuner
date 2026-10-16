//! Settings ▸ Following (DESIGN §33 §3.2, the news-flash preset). OWNER: F-C. Listed once
//! `follow::ENABLED`.
//!
//! * "+ Follow…" opens the Follow sheet on the templates (Team, Show, Event, News flash, Custom).
//! * Rules, in list order (higher wins a clash): "Arsenal — Team · live · remind 2 min ·
//!   spoiler-safe · 4 coming". Enter or a click opens the rule in the sheet's editor. Delete / right-click / the gear: the row's menu — Edit…, Pause / Resume, Test against the
//!   guide (a toast of what it finds now), Move up / down, Delete… (cancel or keep its
//!   reminders). Alt+↑/↓ or the drag grip reorder them.
//! * Upcoming (next 14 days, the ledger): "Arsenal — Sat 12:30 · Sky Sports Main Event · remind".
//!   Spoiler-safe rules show only the rule's name. Enter, a click or Delete opens the programme
//!   popover (Watch, Cancel reminder = skip this one, Follow…).
//! * The switches of `[follow]` and the news flash's quiet hours, "not over another app's
//!   fullscreen" and headlines length.

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;
use crate::follow::FollowRule;
use crate::follow::plan::{MatchState, Upcoming};
use crate::follow::ui as fui;
use crate::model::FollowRuleId;

/// "+ Follow…".
const ACT_NEW: u16 = 1;
/// Upcoming rows are listed at most this many.
const UPCOMING_MAX: usize = 300;
/// The rule menu's own codes (`Action::Digit`; Edit, Pause / Resume, Move and Delete… use the
/// page's shared `MENU_*`).
const MENU_TEST: u8 = 40;
const MENU_DELETE_CANCEL: u8 = 41;
const MENU_DELETE_KEEP: u8 = 42;

/// What the page shows, read in the background.
struct Data {
    rules: Vec<FollowRule>,
    upcoming: Vec<Upcoming>,
    /// When the guide was last checked (`meta follow:last_run`).
    checked: Option<crate::model::Timestamp>,
    /// Each rule's pending reminders (what Delete… cancels or keeps).
    owned: HashMap<FollowRuleId, usize>,
}

fn load(c: &rusqlite::Connection) -> crate::error::Result<Data> {
    let now = crate::model::now();
    let checked = crate::store::follow::meta_get(c, "follow:last_run")?.and_then(|v| v.parse().ok());
    let mut owned = HashMap::new();
    for r in crate::store::user::all_reminders(c)? {
        if let Some(rule) = r.rule_id.filter(|_| r.state == crate::model::ReminderState::Pending) {
            *owned.entry(rule).or_insert(0) += 1;
        }
    }
    Ok(Data { rules: crate::store::follow::rules(c)?, upcoming: crate::store::follow::upcoming(c, now, UPCOMING_MAX)?, checked, owned })
}

/// The page's state (loaded rows), kept in `SettingsScreen`. Interior mutability: the rows are
/// built from `&SettingsScreen`, and that is where the read is polled.
#[derive(Default)]
pub(super) struct State {
    loader: RefCell<Option<Loader<Data>>>,
    data: RefCell<Option<Data>>,
}

impl State {
    fn poll(&self) {
        let mut slot = self.loader.borrow_mut();
        match kit::poll(&mut slot) {
            Some(Ok(d)) => *self.data.borrow_mut() = Some(d),
            Some(Err(e)) => tracing::warn!(target: "tuner::follow", "reading Following: {e}"),
            None => {}
        }
    }
}

static FOLLOW_T: [CfgToggle; 4] = [
    CfgToggle { label: "Check the guide for what I follow", get: |c| c.follow.enabled, set: |c, v| c.follow.enabled = v },
    CfgToggle { label: "Summary of new matches (at most twice a day)", get: |c| c.follow.summary, set: |c, v| c.follow.summary = v },
    CfgToggle {
        label: "New team and event rules are spoiler-safe",
        get: |c| c.follow.spoiler_safe_default,
        set: |c, v| c.follow.spoiler_safe_default = v,
    },
    CfgToggle { label: "A rule's switch may interrupt a film or episode", get: |c| c.follow.interrupt_vod, set: |c, v| c.follow.interrupt_vod = v },
];
static FLASH_T: CfgToggle = CfgToggle {
    label: "Not while another app is fullscreen",
    get: |c| c.follow.news_flash.skip_when_fullscreen,
    set: |c, v| c.follow.news_flash.skip_when_fullscreen = v,
};
static FLASH_N: CfgNumber = CfgNumber {
    label: "Headlines only: the first",
    unit: "min",
    min: 1.0,
    max: 30.0,
    step: 1.0,
    get: |c| f64::from(c.follow.news_flash.headlines_min),
    set: |c, v| c.follow.news_flash.headlines_min = v.round().clamp(1.0, 30.0) as u16,
};
static FLASH_QUIET: CfgText = CfgText {
    label: "Quiet hours (no news flash)",
    placeholder: "22:00–07:00 (empty = none)",
    get: |c| quiet_text(c.follow.news_flash.quiet_from_min, c.follow.news_flash.quiet_to_min),
    set: |c, v| {
        if let Some((from, to)) = parse_quiet(&v) {
            c.follow.news_flash.quiet_from_min = from;
            c.follow.news_flash.quiet_to_min = to;
        }
    },
};

/// "22:00–07:00", or "" for none.
fn quiet_text(from: Option<u16>, to: Option<u16>) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!("{:02}:{:02}–{:02}:{:02}", f / 60, f % 60, t / 60, t % 60),
        _ => String::new(),
    }
}

/// "22:00-07:00", "22–7", "2200 0700" → local minutes; "" → none; anything else → `None`
/// (the field keeps what was stored).
fn parse_quiet(s: &str) -> Option<(Option<u16>, Option<u16>)> {
    let s = s.trim();
    if s.is_empty() {
        return Some((None, None));
    }
    let parts: Vec<&str> = s.split(['–', '-', ' ', '—']).filter(|p| !p.trim().is_empty()).collect();
    let [a, b] = parts.as_slice() else { return None };
    let minute = |p: &str| -> Option<u16> {
        let p = p.trim();
        // the four-digit form slices bytes
        if !p.is_ascii() {
            return None;
        }
        let (h, m) = match p.split_once([':', '.']) {
            Some((h, m)) => (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?),
            None if p.len() > 2 => (p[..p.len() - 2].parse().ok()?, p[p.len() - 2..].parse().ok()?),
            None => (p.parse().ok()?, 0),
        };
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let (from, to) = (minute(a)?, minute(b)?);
    (from != to).then_some((Some(from), Some(to)))
}

/// The Following page's rows.
pub(super) fn rows(s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    s.following.poll();
    let data = s.following.data.borrow();
    let h24 = ctx.config.interface.clock_24h;
    v.push(Row::Section("Following".into()));
    let mut intro = "Tuner reads the guide after every refresh and sets these reminders for you.".to_owned();
    if let Some(d) = data.as_ref() {
        let fresh = d.upcoming.iter().filter(|u| counts(u)).count();
        intro.push_str(&format!(" {fresh} coming"));
        if let Some(at) = d.checked {
            intro.push_str(&format!(" · checked {}", crate::ui::widgets::time::day_time(at, h24)));
        }
        intro.push('.');
    }
    if !ctx.config.follow.enabled {
        intro.push_str(" Switched off: nothing is checked until it's on again.");
    }
    v.push(Row::Note(intro, false));
    v.push(Row::Act { act: Act::Feature(Feature::Following, ACT_NEW), icon: glyphs::ADD, label: "Follow…".into(), danger: false });
    let Some(d) = data.as_ref() else {
        v.push(Row::Note("Loading…".into(), false));
        return;
    };
    v.push(Row::Section("Rules · higher wins a clash".into()));
    if d.rules.is_empty() {
        v.push(Row::Note(
            "Nothing yet. Press R on a programme in the guide, or Follow… above. A news flash pops up a bulletin every day.".into(),
            false,
        ));
    }
    for r in &d.rules {
        let mut detail = fui::rule_summary(r);
        // a "just list" rule sets no reminders: count what it lists instead
        let listed = r.mode == crate::follow::FollowMode::List;
        let coming = d.upcoming.iter().filter(|u| u.rule == r.id && if listed { u.state == MatchState::Listed } else { counts(u) }).count();
        detail.push_str(&match (coming, listed) {
            (0, _) => " · nothing coming".to_owned(),
            (n, true) => format!(" · {n} listed"),
            (n, false) => format!(" · {n} coming"),
        });
        let icon = if !r.enabled {
            glyphs::PAUSE
        } else if r.kind == crate::follow::FollowKind::News {
            glyphs::POP_OUT
        } else {
            glyphs::FOLLOW
        };
        v.push(Row::Item {
            item: Item::Feature(Feature::Following, r.id.0),
            icon: icon.into(),
            color: None,
            label: r.name.clone(),
            detail: Some(detail),
            // one rule needs no grip
            reorder: d.rules.len() > 1,
        });
    }
    v.push(Row::Section(format!("Upcoming · next {} days", 14)));
    let horizon = ctx.now + 14 * 86_400;
    let mut any = false;
    let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
    // a channel that became explicit (or hidden by parental control) since the evaluation
    let shown = |u: &Upcoming| {
        u.channel
            .as_ref()
            .map_or(!s.search_all || kit::unlocked(ctx), |c| !ctx.lib.conceals(c) && (!s.search_all || super::search::searchable_item(ctx, c)))
    };
    for (i, u) in d.upcoming.iter().enumerate().filter(|(_, u)| u.start < horizon && shown(u)) {
        any = true;
        let mut detail = crate::ui::widgets::time::local(u.start, if h24 { "%a %-d %b %H:%M" } else { "%a %-d %b %-I:%M %p" }, "");
        if !u.channel_name.is_empty() {
            detail.push_str(" · ");
            detail.push_str(&u.channel_name);
        }
        detail.push_str(" · ");
        detail.push_str(&fui::state_text(u));
        // spoiler-safe rules' rows carry the rule name only; the others go through the guard
        // (a show you're part-way through, a followed sport's score)
        let subject = crate::guard::Subject::Programme {
            title: &u.title,
            sub_title: u.sub_title.as_deref(),
            categories: &[],
            season_episode: None,
            start: u.start,
            is_new: false,
        };
        let guarded = |field, s: &str| spoilers.text(&subject, field, s).map(std::borrow::Cow::into_owned).unwrap_or_default();
        let title = guarded(crate::guard::Field::Title, &u.title);
        let title = match u.sub_title.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{title} · {}", guarded(crate::guard::Field::SubTitle, s)),
            None => title,
        };
        v.push(Row::Item {
            item: Item::Feature(Feature::Following, upcoming_id(i)),
            icon: fui::state_glyph(u).into(),
            color: None,
            label: title,
            detail: Some(detail),
            reorder: false,
        });
    }
    if !any {
        v.push(Row::Note("Nothing coming up in the guide yet.".into(), false));
    }
    v.push(Row::Section("Settings".into()));
    for t in &FOLLOW_T {
        v.push(Row::Toggle(t));
    }
    v.push(Row::Section("News flash".into()));
    v.push(Row::Note(
        "A news flash pops a bulletin up in the small player at its time, then closes it and puts back what you were watching. It never interrupts a film or episode: you get a notification instead.".into(),
        false,
    ));
    v.push(Row::Text(&FLASH_QUIET));
    v.push(Row::Toggle(&FLASH_T));
    v.push(Row::Number(&FLASH_N));
}

/// Counts as coming (not over the limit, skipped or listed only).
fn counts(u: &Upcoming) -> bool {
    matches!(u.state, MatchState::Scheduled | MatchState::Adopted)
}

/// Upcoming rows are `Item::Feature` with negative ids (rules have positive ones).
fn upcoming_id(i: usize) -> i64 {
    -(i as i64) - 1
}

/// Its `Act::Feature(Feature::Following, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    if n == ACT_NEW {
        ctx.open(OverlayRequest::FollowSheet(crate::ui::overlays::follow_sheet::FollowSheetSpec { record: false, programme: None, rule: None }));
    }
}

/// Enter on its `Item::Feature(Feature::Following, id)` rows: a rule opens in the editor, an
/// Upcoming row opens its programme's popover.
pub(super) fn item(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) {
    if id > 0 {
        let rule = Some(FollowRuleId(id));
        ctx.open(OverlayRequest::FollowSheet(crate::ui::overlays::follow_sheet::FollowSheetSpec { record: false, programme: None, rule }));
        return;
    }
    let i = (-id - 1) as usize;
    let Some(u) = s.following.data.borrow().as_ref().and_then(|d| d.upcoming.get(i).cloned()) else { return };
    match programme_of(ctx, &u) {
        Some((programme, channel)) => ctx.open(OverlayRequest::Programme { programme, channel, zap: None }),
        None => ctx.toast(Toast::info("That airing is no longer in the guide")),
    }
}

/// Delete / right-click / the gear on its rows: a rule's menu (title, items); an Upcoming
/// row opens its programme's popover instead (`None`).
pub(super) fn menu(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) -> Option<(String, Vec<MenuItem>)> {
    if id < 0 {
        item(s, ctx, id);
        return None;
    }
    let data = s.following.data.borrow();
    let d = data.as_ref()?;
    let r = d.rules.iter().find(|r| r.id.0 == id)?;
    let mi = |icon: &'static str, label: &str, code: u8| MenuItem {
        icon,
        label: label.into(),
        action: Action::Digit(code),
        enabled: true,
        shortcut: None,
    };
    let at = d.rules.iter().position(|x| x.id == r.id).unwrap_or(0);
    let mut up = mi(glyphs::REORDER, "Move up (wins more clashes)", MENU_UP);
    up.enabled = at > 0;
    let mut down = mi(glyphs::REORDER, "Move down", MENU_DOWN);
    down.enabled = at + 1 < d.rules.len();
    let items = vec![
        mi(glyphs::SETTINGS, "Edit…", MENU_EDIT),
        if r.enabled { mi(glyphs::PAUSE, "Pause", MENU_TOGGLE) } else { mi(glyphs::PLAY, "Resume", MENU_TOGGLE) },
        mi(glyphs::SEARCH, "Test against the guide", MENU_TEST),
        up,
        down,
        mi(glyphs::CLOSE, "Delete…", MENU_DELETE),
    ];
    Some((r.name.clone(), items))
}

/// A choice in a rule's menu (Move up / down go through the page's `reorder`).
pub(super) fn menu_choice(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64, code: u8) {
    let rule = s.following.data.borrow().as_ref().and_then(|d| d.rules.iter().find(|r| r.id.0 == id).cloned());
    let Some(r) = rule else { return };
    let owned = s.following.data.borrow().as_ref().and_then(|d| d.owned.get(&r.id).copied()).unwrap_or(0);
    match code {
        MENU_EDIT => item(s, ctx, id),
        MENU_TOGGLE => {
            let on = !r.enabled;
            fui::set_enabled(ctx.services, r.id, on, ctx.now);
            if let Some(x) = s.following.data.borrow_mut().as_mut().and_then(|d| d.rules.iter_mut().find(|x| x.id == r.id)) {
                x.enabled = on;
            }
            ctx.toast(Toast::info(if on {
                format!("Resumed {} — Tuner checks the guide for it again", r.name)
            } else if owned == 1 {
                format!("Paused {} — its coming reminder is cancelled", r.name)
            } else if owned > 1 {
                format!("Paused {} — its {owned} coming reminders are cancelled", r.name)
            } else {
                format!("Paused {}", r.name)
            }));
        }
        MENU_TEST => test(ctx, r),
        MENU_DELETE => {
            let mi = |label: String, code| MenuItem { icon: glyphs::CLOSE, label, action: Action::Digit(code), enabled: true, shortcut: None };
            let mut items = vec![mi(
                match owned {
                    0 => "Delete the rule".into(),
                    1 => "Delete it and cancel its reminder".into(),
                    n => format!("Delete it and cancel its {n} reminders"),
                },
                MENU_DELETE_CANCEL,
            )];
            if owned > 0 {
                items.push(mi(format!("Delete it, keep the reminder{} as my own", if owned == 1 { "" } else { "s" }), MENU_DELETE_KEEP));
            }
            s.pending = Some(Pending::Menu(Item::Feature(Feature::Following, id)));
            ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: format!("Stop following {}?", r.name), items }));
        }
        MENU_DELETE_CANCEL | MENU_DELETE_KEEP => {
            fui::delete_rule(ctx.services, r.id, r.name.clone(), code == MENU_DELETE_KEEP);
            if let Some(d) = s.following.data.borrow_mut().as_mut() {
                d.rules.retain(|x| x.id != r.id);
                d.upcoming.retain(|u| u.rule != r.id);
            }
        }
        _ => {}
    }
}

/// "Test against the guide": preview the rule (paused or not) over the next 14 days in the
/// background and toast what it finds ([`fui::test_text`]).
fn test(ctx: &mut UiCtx<'_>, rule: FollowRule) {
    if ctx.lib.built_at == 0 {
        ctx.toast(Toast::info("Your playlists are still loading — try again in a moment"));
        return;
    }
    ctx.toast(Toast::info(format!("Checking the guide for {}…", rule.name)));
    let (store, lib, bus) = (ctx.services.store.clone(), ctx.lib.clone(), ctx.services.bus.clone());
    let opts = crate::follow::preview::PreviewOpts {
        now: ctx.now,
        days: 14,
        limit: 50,
        pin_active: ctx.config.parental.pin_active(),
        max_pending: ctx.config.follow.max_pending as usize,
    };
    let h24 = ctx.config.interface.clock_24h;
    ctx.services.rt.spawn_blocking(move || {
        let tz = jiff::tz::TimeZone::system();
        let toast = match store.reader().and_then(|c| crate::follow::preview::preview(&c, &lib, &rule, &opts, &tz, &|| false)) {
            Ok(p) => Toast { ttl_ms: 8000, ..Toast::info(fui::test_text(&rule.name, &p, h24)) },
            Err(e) => {
                tracing::debug!(target: "tuner::follow", "test failed: {e}");
                Toast::error(format!("Couldn't check the guide for {}", rule.name))
            }
        };
        bus.toast(toast);
    });
}

/// Alt+↑/↓, the drag grip or the menu's Move: move a rule within the list now (higher
/// wins a clash), then check the guide.
pub(super) fn reorder(s: &SettingsScreen, ctx: &mut UiCtx<'_>, id: i64, delta: isize) {
    let mut data = s.following.data.borrow_mut();
    let Some(d) = data.as_mut() else { return };
    let mut order: Vec<FollowRuleId> = d.rules.iter().map(|r| r.id).collect();
    let Some(i) = order.iter().position(|x| x.0 == id) else { return };
    let j = (i as isize + delta).clamp(0, order.len() as isize - 1) as usize;
    if i == j {
        return;
    }
    let v = order.remove(i);
    order.insert(j, v);
    d.rules.sort_by_key(|r| order.iter().position(|x| *x == r.id));
    fui::reorder_rules(ctx.services, order, ctx.now);
}

/// The guide programme of an Upcoming row: its channel's programme starting at the row's
/// (displayed) start.
fn programme_of(ctx: &UiCtx<'_>, u: &Upcoming) -> Option<(crate::model::ProgrammeId, ChannelId)> {
    let id = ctx.lib.live.by_ref(u.channel.as_ref()?)?;
    let ch = ctx.lib.live.get(id)?;
    let epg = ch.epg_channel_id?;
    let start = u.start - i64::from(ch.tvg_shift_min) * 60;
    let p = crate::store::epg::window(ctx.db, epg, start, start + 1).ok()?.into_iter().find(|p| p.start == start)?;
    Some((p.id, id))
}

/// The page became current (or its data changed): start loading.
pub(super) fn enter(s: &mut SettingsScreen, ctx: &UiCtx<'_>) {
    *s.following.loader.borrow_mut() = Some(Loader::start(ctx, load));
}

pub(super) fn search_ready(s: &SettingsScreen) -> bool {
    s.following.poll();
    s.following.loader.borrow().is_none()
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::*;
    use crate::follow::{FollowKind, FollowReason};
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, has_label_containing, rig, seed, until};

    /// After an evaluation, Settings ▸ Following lists the rule ("1 coming") and its match
    /// under Upcoming by the rule's name only (spoiler-safe); Enter on the rule opens it in the
    /// sheet's editor, Enter on the match opens that programme's popover; and the guide's
    /// tooltip says which rule set it.
    #[test]
    fn the_page_lists_rules_and_matches_and_opens_them() {
        let (mut t, one) = rig();
        let (pid, ch, start) = crate::ui::overlays::follow_sheet::tests::fixture(&mut t);
        let id = seed(&mut t, |c| {
            let r = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &crate::config::FollowConfig::default(), 0);
            crate::store::follow::save_rule(c, &r, 100, 0)
        });
        let opts = crate::jobs::follow::Opts {
            now: crate::model::now(),
            reason: FollowReason::Rules,
            cfg: crate::config::FollowConfig::default(),
            pin_active: false,
            tz: jiff::tz::TimeZone::system(),
        };
        let reader = t.services().store.reader().unwrap();
        let ev = crate::jobs::follow::prepare(&reader, &t.lib, &opts, &|| false).unwrap().expect("evaluated");
        t.services().store.writer().blocking(move |c| crate::jobs::follow::commit(c, &ev, &opts)).unwrap();

        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1600.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Following));
        until(&mut h, "the rule", |h| has_label_containing(h, "1 coming"));
        assert!(has_label(&h, "Team · live · remind 2 min · spoiler-safe · 1 coming"));
        assert!(!has_label_containing(&h, "Chelsea"), "spoiler-safe: the rule's name, not the fixture");
        let open = |h: &mut egui_kittest::Harness<'static, testkit::App<SettingsScreen>>, item: Item| {
            let app = h.state_mut();
            let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
            app.s.pane = Pane::Content;
            app.s.row = rows.iter().position(|r| matches!(r, Row::Item { item: x, .. } if *x == item)).expect("row");
            app.act(Action::Activate);
            h.run_steps(2);
            h.state_mut().nav()
        };
        let sheet = crate::ui::overlays::follow_sheet::FollowSheetSpec { record: false, programme: None, rule: Some(id) };
        assert!(open(&mut h, Item::Feature(Feature::Following, id.0)).contains(&NavRequest::Open(OverlayRequest::FollowSheet(sheet))));
        let popover = OverlayRequest::Programme { programme: pid, channel: ch, zap: None };
        assert!(open(&mut h, Item::Feature(Feature::Following, upcoming_id(0))).contains(&NavRequest::Open(popover)));

        // the guide's tooltip (read in the background once a Following hook saw the store)
        let app = h.state_mut();
        let target = crate::ui::item_menu::Target::Programme { programme: pid, channel: ch };
        app.t.with_ctx(|ctx| crate::follow::ui::menu_items(&target, ctx, crate::keymap::KeyContext::Guide));
        let epg = app.t.lib.live.get(ch).and_then(|c| c.epg_channel_id).unwrap();
        let slim = crate::store::epg::window(&app.t.db, epg, start, start + 1).unwrap().into_iter().find(|p| p.id == pid).unwrap();
        let begun = std::time::Instant::now();
        while crate::follow::ui::tooltip_line(&slim).is_none() {
            assert!(begun.elapsed() < std::time::Duration::from_secs(10), "no tooltip line");
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert_eq!(crate::follow::ui::tooltip_line(&slim).as_deref(), Some("From Following: Arsenal"));
    }

    /// A rule row's menu and reorder: Alt+↑ and the drag grip change the order (higher wins a
    /// clash), the menu tests the rule against the guide, pauses it (its reminder goes at the
    /// evaluation that follows) and resumes it, and Delete… asks whether to cancel or keep its
    /// reminders, then stops following. Delete on an Upcoming row opens the programme's popover.
    #[test]
    fn rule_rows_have_a_menu_and_reorder() {
        use egui_kittest::kittest::Queryable;
        type H = egui_kittest::Harness<'static, testkit::App<SettingsScreen>>;

        let (mut t, one) = rig();
        let (pid, ch, _start) = crate::ui::overlays::follow_sheet::tests::fixture(&mut t);
        let cfg = crate::config::FollowConfig::default();
        let (arsenal, wimbledon) = seed(&mut t, move |c| {
            let a = crate::store::follow::save_rule(c, &FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &cfg, 0), 100, 0)?;
            let w =
                crate::store::follow::save_rule(c, &FollowRule::draft("Wimbledon", FollowKind::Event, vec!["Wimbledon".into()], &cfg, 0), 100, 0)?;
            Ok((a, w))
        });
        let opts = crate::jobs::follow::Opts {
            now: crate::model::now(),
            reason: FollowReason::Rules,
            cfg: crate::config::FollowConfig::default(),
            pin_active: false,
            tz: jiff::tz::TimeZone::system(),
        };
        let reader = t.services().store.reader().unwrap();
        let ev = crate::jobs::follow::prepare(&reader, &t.lib, &opts, &|| false).unwrap().expect("evaluated");
        t.services().store.writer().blocking(move |c| crate::jobs::follow::commit(c, &ev, &opts)).unwrap();

        let stored = |h: &H| crate::store::follow::rules(&h.state().t.services().store.reader().unwrap()).unwrap();
        let order = |h: &H| stored(h).into_iter().map(|r| r.name).collect::<Vec<_>>();
        let pending = |h: &H| {
            crate::store::user::all_reminders(&h.state().t.services().store.reader().unwrap())
                .unwrap()
                .into_iter()
                .filter(|r| r.rule_id == Some(arsenal) && r.state == crate::model::ReminderState::Pending)
                .count()
        };
        let focus = |h: &mut H, item: Item| {
            let app = h.state_mut();
            let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
            app.s.pane = Pane::Content;
            app.s.row = rows.iter().position(|r| matches!(r, Row::Item { item: x, .. } if *x == item)).expect("row");
        };
        let menu = |h: &mut H| match h.state_mut().nav().as_slice() {
            [NavRequest::Open(OverlayRequest::ContextMenu(m))] => m.clone(),
            other => panic!("{other:?}"),
        };
        let toasted = |h: &mut H, text: &str| {
            h.state_mut().pump();
            h.state().t.toasts.iter().any(|t| t.text.contains(text))
        };

        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1600.0, 1000.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Following));
        until(&mut h, "the rules", |h| has_label_containing(h, "1 coming") && has_label(h, "Wimbledon"));
        assert_eq!(order(&h), ["Arsenal", "Wimbledon"]);
        assert_eq!(pending(&h), 1);

        // x on an Upcoming row: its programme's popover (Watch, Cancel = skip this one)
        focus(&mut h, Item::Feature(Feature::Following, upcoming_id(0)));
        h.state_mut().act(Action::ContextMenu);
        assert_eq!(h.state_mut().nav(), [NavRequest::Open(OverlayRequest::Programme { programme: pid, channel: ch, zap: None })]);

        // Alt+↑ on Wimbledon: first now
        focus(&mut h, Item::Feature(Feature::Following, wimbledon.0));
        h.state_mut().act(Action::MoveUp);
        until(&mut h, "keyboard reorder", |h| order(h) == ["Wimbledon", "Arsenal"]);

        // the drag grip: Arsenal back above Wimbledon
        h.state_mut().enter(Route::Settings(SettingsPage::Following));
        until(&mut h, "the reloaded order", |h| {
            h.state().s.following.data.borrow().as_ref().is_some_and(|d| d.rules.first().is_some_and(|r| r.name == "Wimbledon"))
        });
        h.run_steps(2);
        let grip = |h: &H, name: &str| {
            let row = h.query_all_by_label(name).next().expect("row").rect();
            let grips: Vec<egui::Rect> = h.query_all_by_label(glyphs::REORDER).map(|n| n.rect()).collect();
            grips.into_iter().find(|g| g.center().y >= row.top() - 12.0 && g.center().y <= row.bottom() + 24.0).expect("grip of the row")
        };
        let from = grip(&h, "Arsenal").center();
        let to = h.query_all_by_label("Wimbledon").next().expect("the first rule").rect().center();
        h.drag_at(from);
        h.run_steps(1);
        h.hover_at(egui::pos2(from.x, (from.y + to.y) / 2.0));
        h.run_steps(1);
        h.hover_at(egui::pos2(from.x, to.y));
        h.run_steps(1);
        h.drop_at(egui::pos2(from.x, to.y));
        h.run_steps(2);
        until(&mut h, "drag reorder", |h| order(h) == ["Arsenal", "Wimbledon"]);

        // x: the menu (first: no Move up)
        h.state_mut().enter(Route::Settings(SettingsPage::Following));
        until(&mut h, "the reloaded order", |h| {
            h.state().s.following.data.borrow().as_ref().is_some_and(|d| d.rules.first().is_some_and(|r| r.name == "Arsenal"))
        });
        focus(&mut h, Item::Feature(Feature::Following, arsenal.0));
        h.state_mut().act(Action::ContextMenu);
        let m = menu(&mut h);
        assert_eq!(m.title, "Arsenal");
        let labels: Vec<(&str, bool)> = m.items.iter().map(|i| (i.label.as_str(), i.enabled)).collect();
        assert_eq!(
            labels,
            [
                ("Edit…", true),
                ("Pause", true),
                ("Test against the guide", true),
                ("Move up (wins more clashes)", false),
                ("Move down", true),
                ("Delete…", true)
            ]
        );
        // Edit… opens the rule in the sheet
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_EDIT)));
        let sheet = crate::ui::overlays::follow_sheet::FollowSheetSpec { record: false, programme: None, rule: Some(arsenal) };
        assert_eq!(h.state_mut().nav(), [NavRequest::Open(OverlayRequest::FollowSheet(sheet))]);

        // Test against the guide: a toast of what it finds, by the rule's name only
        h.state_mut().act(Action::ContextMenu);
        let _ = menu(&mut h);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_TEST)));
        until(&mut h, "the test toast", |h| toasted(h, "Arsenal: 1 in the next 14 days · next "));
        assert!(!h.state().t.toasts.iter().any(|t| t.text.contains("Chelsea")), "spoiler-safe");

        // Pause: stored, shown, and its reminder goes at the evaluation that follows
        h.state_mut().act(Action::ContextMenu);
        let _ = menu(&mut h);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_TOGGLE)));
        until(&mut h, "paused", |h| stored(h).iter().any(|r| r.id == arsenal && !r.enabled));
        assert!(toasted(&mut h, "Paused Arsenal — its coming reminder is cancelled"));
        until(&mut h, "its reminder cancelled", |h| pending(h) == 0);
        until(&mut h, "the row says paused", |h| has_label_containing(h, "paused"));
        h.state_mut().act(Action::ContextMenu);
        let m = menu(&mut h);
        assert_eq!(m.items[1].label, "Resume");
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_TOGGLE)));
        until(&mut h, "resumed", |h| stored(h).iter().any(|r| r.id == arsenal && r.enabled));
        until(&mut h, "its reminder back", |h| pending(h) == 1);

        // Delete…: cancel or keep its reminder; keep → the reminder is the user's own
        h.state_mut().enter(Route::Settings(SettingsPage::Following));
        until(&mut h, "the owned count", |h| h.state().s.following.data.borrow().as_ref().is_some_and(|d| d.owned.get(&arsenal) == Some(&1)));
        focus(&mut h, Item::Feature(Feature::Following, arsenal.0));
        h.state_mut().act(Action::ContextMenu);
        let _ = menu(&mut h);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_DELETE)));
        let m = menu(&mut h);
        assert_eq!(m.title, "Stop following Arsenal?");
        let labels: Vec<&str> = m.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Delete it and cancel its reminder", "Delete it, keep the reminder as my own"]);
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(MENU_DELETE_KEEP)));
        until(&mut h, "deleted", |h| order(h) == ["Wimbledon"]);
        until(&mut h, "the toast", |h| toasted(h, "Stopped following Arsenal"));
        let mine = crate::store::user::all_reminders(&h.state().t.services().store.reader().unwrap())
            .unwrap()
            .into_iter()
            .filter(|r| r.state == crate::model::ReminderState::Pending && r.rule_id.is_none())
            .count();
        assert_eq!(mine, 1, "kept as the user's own");
        // the page reloads once the store says the rule went (a frame or more later)
        until(&mut h, "the row gone", |h| !has_label(h, "Arsenal"));
    }

    /// Quiet hours as typed: both spellings of the dash, hours alone, four digits; empty
    /// clears them, nonsense keeps what was stored.
    #[test]
    fn quiet_hours_as_typed() {
        assert_eq!(parse_quiet("22:00–07:00"), Some((Some(22 * 60), Some(7 * 60))));
        assert_eq!(parse_quiet("22-7"), Some((Some(22 * 60), Some(7 * 60))));
        assert_eq!(parse_quiet("2230 0615"), Some((Some(22 * 60 + 30), Some(6 * 60 + 15))));
        assert_eq!(parse_quiet(""), Some((None, None)));
        assert_eq!(parse_quiet("late"), None);
        assert_eq!(parse_quiet("25:00-07:00"), None);
        assert_eq!(parse_quiet("２２００ ０７００"), None, "full-width digits: no panic");
        assert_eq!(parse_quiet("07ä0 2200"), None);
        assert_eq!(quiet_text(Some(22 * 60), Some(7 * 60)), "22:00–07:00");
    }
}
