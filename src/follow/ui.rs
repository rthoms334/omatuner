//! Following's UI pieces (DESIGN §33, the news-flash preset of the big-features additions).
//! OWNER: F-C.
//!
//! * `R` on a guide programme, "Follow…" (or "News flash this daily…" on a news bulletin) in
//!   the programme popover, "Follow this programme…" in the guide's and search's item menus:
//!   all open the Follow sheet (`ui::overlays::follow_sheet`), which hosts the rule editor and
//!   its live preview (`follow::preview::PreviewService`, started by the sheet).
//! * The guide tooltip's "From Following: Arsenal" line ([`tooltip_line`]), read from the ledger
//!   in the background (a per-thread index refreshed at most once a minute, and at once after a
//!   rule is saved here).
//! * The in-app summary toast when no notification server answered ([`on_event`]).
//! * The words every Following surface uses for a rule ([`rule_summary`], [`mode_text`]…).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::follow::flash::{FlashLength, FlashVolume};
use crate::follow::plan::{MatchState, Upcoming};
use crate::follow::{Airings, ChannelScope, FollowEvent, FollowKind, FollowMode, FollowRule, Shape};
use crate::keymap::{Action, KeyContext};
use crate::model::{FollowRuleId, Timestamp};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;
use crate::ui::overlays::follow_sheet::FollowSheetSpec;
use crate::ui::widgets::glyphs;

/// Following is built and switched on (Settings ▸ Following ▸ "Check the guide for what I
/// follow").
pub fn on(ctx: &UiCtx<'_>) -> bool {
    crate::follow::ENABLED && ctx.config.follow.enabled
}

/// Its entries in an item menu (`ui::item_menu::extend`): "Follow this programme…" on a guide
/// programme; none while the feature is off.
pub fn menu_items(target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    remember(ctx);
    match target {
        Target::Programme { .. } if on(ctx) => {
            vec![crate::ui::item_menu::entry(glyphs::FOLLOW, "Follow this programme…", Action::FollowProgramme, keys)]
        }
        _ => Vec::new(),
    }
}

/// Its primary button in the programme popover: "Follow…", or "News flash this daily…" on a
/// news bulletin (the sheet then offers the news flash first); none while the feature is off.
pub fn popover_button(target: &Target, ctx: &UiCtx<'_>) -> Option<MenuItem> {
    remember(ctx);
    let Target::Programme { programme, .. } = target else { return None };
    if !on(ctx) {
        return None;
    }
    let news = crate::store::epg::programme(ctx.db, *programme).is_ok_and(|p| {
        let s = crate::follow::suggest::suggest(&p.title, p.sub_title.as_deref(), &p.categories, p.episode.is_some(), 5);
        s.first().is_some_and(|s| s.kind == FollowKind::News)
    });
    let label = if news { "News flash this daily…" } else { "Follow…" };
    Some(crate::ui::item_menu::entry(glyphs::FOLLOW, label, Action::FollowProgramme, KeyContext::Guide))
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`): `R` / "Follow…" opens the
/// Follow sheet on a programme. False while the feature is off.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    remember(ctx);
    if action != Action::FollowProgramme || !crate::follow::ENABLED {
        return false;
    }
    if !ctx.config.follow.enabled {
        ctx.toast(Toast::info("Following is switched off — turn it on in Settings ▸ Following"));
        return true;
    }
    match target {
        Target::Programme { programme, channel } => {
            ctx.open(crate::ui::OverlayRequest::FollowSheet(FollowSheetSpec { programme: Some((*programme, *channel)), rule: None }));
            true
        }
        // a gap in the guide: nothing to follow there
        Target::Channel(_) => {
            ctx.toast(Toast::info("Move to a programme to follow it"));
            true
        }
        _ => false,
    }
}

/// What the app hears from Following (`AppEvent::Follow`): the summary of new matches as a toast
/// with Open (Settings ▸ Following) when no notification server answered ("Following: 3 new —
/// Arsenal · Sat 12:30 · Sky Sports Main Event; …"). Preview answers go to the sheet that
/// asked (its own bus); the app acts on `Open` itself.
pub fn on_event(ev: &FollowEvent) -> Option<Toast> {
    match ev {
        FollowEvent::Summary { title, lines } => {
            invalidate();
            let text = if lines.is_empty() { title.clone() } else { format!("{title} — {}", lines.join("; ")) };
            Some(Toast { kind: ToastKind::Info, text, action: Some(("Open".into(), Action::OpenFollowing)), ttl_ms: 8000 })
        }
        FollowEvent::PreviewReady { .. } | FollowEvent::Open => None,
    }
}

/// Undo "Following X": delete the rule and cancel the reminders it set (a toast's Undo; the
/// integrator routes the toast action here).
pub fn undo_follow(services: &crate::services::Services, id: FollowRuleId) {
    let jobs = services.jobs.clone();
    let _ = services.store.writer().submit_notify(DataScope::Follow, move |c| {
        crate::store::follow::delete_rule(c, id, false)?;
        jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Rules });
        Ok(())
    });
    invalidate();
}

/// Stop following: delete the rule; its pending reminders are cancelled, or kept as the user's
/// own (`keep`). Toasts "Stopped following Arsenal · 4 reminders cancelled" (the sheet's and
/// Settings' Delete…).
pub fn delete_rule(services: &crate::services::Services, id: FollowRuleId, name: String, keep: bool) {
    let bus = services.bus.clone();
    let _ = services.store.writer().submit_notify(DataScope::Follow, move |c| {
        let n = crate::store::follow::delete_rule(c, id, keep)?;
        bus.data_changed(DataScope::Reminders);
        let what = match n {
            0 => String::new(),
            1 => " · 1 reminder cancelled".into(),
            n => format!(" · {n} reminders cancelled"),
        };
        bus.toast(Toast::info(format!("Stopped following {name}{what}")));
        Ok(())
    });
    invalidate();
}

/// Pause (`on` false) or resume a rule, then check the guide (a paused rule's reminders go,
/// a resumed one's come back).
pub fn set_enabled(services: &crate::services::Services, id: FollowRuleId, on: bool, now: Timestamp) {
    let jobs = services.jobs.clone();
    let _ = services.store.writer().submit_notify(DataScope::Follow, move |c| {
        crate::store::follow::set_rule_enabled(c, id, on, now)?;
        jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Rules });
        Ok(())
    });
    invalidate();
}

/// The list order of the rules (first wins a clash), then check the guide.
pub fn reorder_rules(services: &crate::services::Services, order: Vec<FollowRuleId>, now: Timestamp) {
    let jobs = services.jobs.clone();
    let _ = services.store.writer().submit_notify(DataScope::Follow, move |c| {
        crate::store::follow::reorder_rules(c, &order, now)?;
        jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Rules });
        Ok(())
    });
}

/// "Test against the guide" (Settings ▸ Following's row menu): what a rule finds in the next
/// 14 days, without opening it — "Arsenal: 4 in the next 14 days · next Sat 12:30 on Sky Sports
/// Main Event", "Arsenal: nothing in the next 14 days · 2 left out (repeat)". The rule's name,
/// times and channels only (spoiler-safe whatever the rule says). Pure.
pub fn test_text(name: &str, p: &crate::follow::plan::Preview, h24: bool) -> String {
    let coming: Vec<&Upcoming> = p.matches.iter().filter(|u| !matches!(u.state, MatchState::Skipped | MatchState::OverCap)).collect();
    let mut text = match coming.iter().min_by_key(|u| u.start) {
        None => format!("{name}: nothing in the next 14 days"),
        Some(next) => {
            let on = if next.channel_name.is_empty() { String::new() } else { format!(" on {}", next.channel_name) };
            format!("{name}: {} in the next 14 days · next {}{on}", coming.len(), crate::ui::widgets::time::day_time(next.start, h24))
        }
    };
    let held = p.matches.len() - coming.len();
    if held > 0 {
        text.push_str(&format!(" · {held} skipped or over the weekly limit"));
    }
    if !p.left_out.is_empty() {
        let mut reasons: Vec<&str> = p.left_out.iter().filter_map(|u| u.note.as_deref()).filter(|n| !n.is_empty()).collect();
        reasons.sort_unstable();
        reasons.dedup();
        reasons.truncate(2);
        text.push_str(&format!(" · {} left out", p.left_out.len()));
        if !reasons.is_empty() {
            text.push_str(&format!(" ({})", reasons.join(", ")));
        }
    }
    if let Some(w) = p.warnings.first() {
        text.push_str(&format!(" · {w}"));
    }
    text
}

// ------------------------------------------------------------------ the words

/// "Team", "Show", "Event", "News flash", "Custom".
pub fn kind_text(kind: FollowKind) -> &'static str {
    match kind {
        FollowKind::Team => "Team",
        FollowKind::Show => "Show",
        FollowKind::Event => "Event",
        FollowKind::Custom => "Custom",
        FollowKind::News => "News flash",
    }
}

/// "live", "live or new", "new episodes", "every airing".
pub fn airings_text(a: Airings) -> &'static str {
    match a {
        Airings::LiveOnly => "live",
        Airings::LiveOrNew => "live or new",
        Airings::NewOnly => "new episodes",
        Airings::Any => "every airing",
    }
}

/// What a rule's matches do: "remind 2 min", "switch at start", "just list", "pop up · first 5
/// min".
pub fn mode_text(r: &FollowRule) -> String {
    let offset = if r.offset_s > 0 { format!(" +{} min", r.offset_s / 60) } else { String::new() };
    match r.mode {
        FollowMode::Notify if r.lead_s <= 0 => "remind at start".into(),
        FollowMode::Notify => format!("remind {} min", (r.lead_s + 59) / 60),
        FollowMode::Switch => format!("switch at start{offset}"),
        FollowMode::List => "just list".into(),
        FollowMode::Flash => match r.spec.flash.map(|f| f.length) {
            Some(FlashLength::Whole) => format!("pop up · whole programme{offset}"),
            Some(FlashLength::Headlines(m)) => format!("pop up · first {m} min{offset}"),
            None => format!("pop up{offset}"),
        },
    }
}

/// A rule's one-line description: "Team · live · remind 2 min · spoiler-safe · paused".
pub fn rule_summary(r: &FollowRule) -> String {
    let mut parts = vec![kind_text(r.kind).to_owned()];
    if r.kind != FollowKind::News {
        parts.push(airings_text(r.spec.airings).to_owned());
    }
    if r.spec.days == crate::follow::spec::WEEKDAYS {
        parts.push("weekdays".into());
    } else if r.spec.days == WEEKENDS {
        parts.push("weekends".into());
    }
    parts.push(mode_text(r));
    match &r.spec.channels {
        ChannelScope::Any => {}
        ChannelScope::Favourites => parts.push("favourites".into()),
        ChannelScope::List(_) | ChannelScope::Playlist(_) | ChannelScope::Channels(_) => parts.push("some channels".into()),
    }
    if r.spoiler_safe {
        parts.push("spoiler-safe".into());
    }
    if r.by_agent {
        parts.push(format!("by {}", r.agent.name()));
    }
    if !r.enabled {
        parts.push("paused".into());
    }
    parts.join(" · ")
}

/// Weekday mask of Saturday and Sunday (`FollowSpec::days`).
pub const WEEKENDS: u8 = 0b110_0000;

/// What an Upcoming row or a preview row says after its time and channel: "remind",
/// "switch", "pop up", "list", "! clash · remind", "over the weekly limit", "skipped".
pub fn state_text(u: &Upcoming) -> String {
    let what = match u.mode {
        FollowMode::Notify => "remind",
        FollowMode::Switch => "switch",
        FollowMode::List => "list",
        FollowMode::Flash => "pop up",
    };
    let note = u.note.as_deref().unwrap_or("");
    match u.state {
        MatchState::OverCap => "over the weekly limit".into(),
        MatchState::Skipped => "skipped".into(),
        MatchState::Adopted => "your own reminder".into(),
        MatchState::LeftOut => format!("left out: {note}"),
        _ if note.starts_with("clash:") => format!("! clash · {what}"),
        _ if note == "locked" => format!("needs your PIN · {what}"),
        _ => what.into(),
    }
}

/// The glyph of an Upcoming / preview row.
pub fn state_glyph(u: &Upcoming) -> &'static str {
    match (u.state, u.mode) {
        (MatchState::OverCap | MatchState::Skipped | MatchState::LeftOut, _) => glyphs::CLOSE,
        (_, FollowMode::Switch) => glyphs::LIVE_TV,
        (_, FollowMode::Flash) => glyphs::POP_OUT,
        (_, FollowMode::List) => glyphs::DOT,
        (_, FollowMode::Notify) => glyphs::REMINDER,
    }
}

// ------------------------------------------------------------------ the guide tooltip

/// How long the tooltip index is trusted before it is read again (an evaluation in the
/// background changes the ledger without telling the guide).
const TIPS_FRESH: Duration = Duration::from_secs(60);

/// The guide tooltip's index of rule matches: (displayed start, title) → "From Following:
/// Arsenal". Per thread (the UI thread; each test its own).
#[derive(Default)]
struct Tips {
    /// Where to read from: the store and the runtime of the last `UiCtx` seen.
    source: Option<(crate::store::Store, tokio::runtime::Handle, crate::msg::EventBus)>,
    map: HashMap<(Timestamp, String), String>,
    read_at: Option<Instant>,
    reading: Option<std::sync::mpsc::Receiver<HashMap<(Timestamp, String), String>>>,
}

thread_local! {
    static TIPS: RefCell<Tips> = RefCell::new(Tips::default());
}

/// Remember where the tooltip index is read from (any Following hook with a `UiCtx`).
fn remember(ctx: &UiCtx<'_>) {
    if !crate::follow::ENABLED {
        return;
    }
    TIPS.with(|t| {
        let mut t = t.borrow_mut();
        if t.source.is_none() {
            t.source = Some((ctx.services.store.clone(), ctx.services.rt.handle().clone(), ctx.services.bus.clone()));
        }
    });
}

/// Read the tooltip index again on its next use (a rule was saved, a summary arrived).
pub fn invalidate() {
    TIPS.with(|t| t.borrow_mut().read_at = None);
}

impl Tips {
    fn refresh(&mut self) {
        if let Some(rx) = &self.reading {
            match rx.try_recv() {
                Ok(map) => {
                    self.map = map;
                    self.reading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.reading = None,
            }
        }
        if self.read_at.is_some_and(|at| at.elapsed() < TIPS_FRESH) {
            return;
        }
        let Some((store, rt, bus)) = self.source.clone() else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        self.reading = Some(rx);
        self.read_at = Some(Instant::now());
        rt.spawn_blocking(move || {
            let map = store.reader().and_then(|c| tooltip_index(&c, crate::model::now())).unwrap_or_default();
            let _ = tx.send(map);
            bus.request_repaint();
        });
    }
}

/// The ledger's matches still to come, by (displayed start, title).
fn tooltip_index(conn: &rusqlite::Connection, now: Timestamp) -> crate::error::Result<HashMap<(Timestamp, String), String>> {
    let names: HashMap<FollowRuleId, String> = crate::store::follow::rules(conn)?.into_iter().map(|r| (r.id, r.name)).collect();
    let mut map = HashMap::new();
    for row in crate::store::follow::ledger(conn, &|_| false)? {
        if row.stop <= now {
            continue;
        }
        let Some(name) = names.get(&row.rule) else { continue };
        let line = match row.state {
            MatchState::Scheduled | MatchState::Adopted => format!("From Following: {name}"),
            MatchState::Listed => format!("Listed by Following: {name}"),
            MatchState::OverCap => format!("Following: {name} (over its weekly limit)"),
            MatchState::Skipped => format!("Following: {name} (skipped)"),
            MatchState::Gone | MatchState::Aired | MatchState::LeftOut => continue,
        };
        map.entry((row.start, row.title)).or_insert(line);
    }
    Ok(map)
}

/// The guide tooltip's line for a programme a rule matched ("From Following: Arsenal"); none
/// while Following is off.
pub fn tooltip_line(p: &crate::model::ProgrammeSlim) -> Option<String> {
    if !crate::follow::ENABLED {
        return None;
    }
    TIPS.with(|t| {
        let mut t = t.borrow_mut();
        t.refresh();
        t.map.get(&(p.start, p.title.to_string())).cloned()
    })
}

/// Save `rule` (a new one, or an edit), then check the guide for it (`EvaluateFollows`). The
/// ticket answers with the rule's id.
pub fn save_rule(ctx: &UiCtx<'_>, rule: FollowRule) -> crate::store::Ticket<FollowRuleId> {
    let (max, now, jobs) = (ctx.config.follow.max_rules, ctx.now, ctx.services.jobs.clone());
    invalidate();
    ctx.services.store.writer().submit_notify(DataScope::Follow, move |c| {
        let id = crate::store::follow::save_rule(c, &rule, max, now)?;
        jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Rules });
        Ok(id)
    })
}

/// Shape options of the editor, in order.
pub const SHAPES: [Shape; 4] = [Shape::Fixture, Shape::TitleIs { spin_offs: false }, Shape::TitleIs { spin_offs: true }, Shape::Anywhere];

/// "Fixtures only ("Arsenal v …")", "The title is the name"…
pub fn shape_text(s: &Shape, name: &str) -> String {
    match s {
        Shape::Fixture => format!("Fixtures only (\"{} v …\")", if name.is_empty() { "Team" } else { name }),
        Shape::TitleIs { spin_offs: false } => "The title is the name".into(),
        Shape::TitleIs { spin_offs: true } => "The title, spin-offs too".into(),
        Shape::Anywhere => "Anywhere it looks".into(),
    }
}

/// Flash volume words.
pub fn volume_text(v: FlashVolume) -> &'static str {
    match v {
        FlashVolume::Normal => "Normal",
        FlashVolume::Mute => "Silent (subtitles)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::plan::Preview;

    fn row(start: Timestamp, channel: &str, state: MatchState, note: Option<&str>) -> Upcoming {
        Upcoming {
            rule: FollowRuleId(1),
            event_key: format!("e{start}"),
            start,
            stop: start + 7200,
            title: "Premier League Football".into(),
            sub_title: Some("Arsenal v Chelsea".into()),
            channel: None,
            channel_name: channel.into(),
            alternates: Vec::new(),
            state,
            mode: FollowMode::Notify,
            note: note.map(str::to_owned),
        }
    }

    /// "Test against the guide" says how many it finds, the next one's time and channel (never
    /// the fixture: the rule's name only), what is held back and what it leaves out and why.
    #[test]
    fn the_test_toast_counts_without_spoiling() {
        let (a, b) = (1_800_000_000, 1_800_100_000);
        let p = Preview {
            matches: vec![
                row(b, "BBC One", MatchState::Scheduled, None),
                row(a, "Sky Sports", MatchState::Scheduled, None),
                row(b + 9000, "", MatchState::Skipped, None),
            ],
            left_out: vec![row(a, "", MatchState::LeftOut, Some("repeat")), row(b, "", MatchState::LeftOut, Some("repeat"))],
            warnings: vec!["'Arsenal' matches football and rugby".into()],
        };
        let text = test_text("Arsenal", &p, true);
        assert_eq!(
            text,
            format!(
                "Arsenal: 2 in the next 14 days · next {} on Sky Sports · 1 skipped or over the weekly limit · 2 left out (repeat) · 'Arsenal' matches football and rugby",
                crate::ui::widgets::time::day_time(a, true)
            )
        );
        assert!(!text.contains("Chelsea"));
        assert_eq!(test_text("Wimbledon", &Preview::default(), true), "Wimbledon: nothing in the next 14 days");
    }

    /// The summary's in-app toast has Open (Settings ▸ Following); `Open` itself is the app's.
    #[test]
    fn the_summary_toast_opens_following() {
        let t = on_event(&FollowEvent::Summary { title: "Following: 3 new".into(), lines: vec!["a".into(), "b".into()] }).expect("a toast");
        assert_eq!(t.text, "Following: 3 new — a; b");
        assert_eq!(t.action, Some(("Open".into(), Action::OpenFollowing)));
        assert!(on_event(&FollowEvent::Open).is_none());
    }
}
