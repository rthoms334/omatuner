//! Reminder engine, MCP pass 2 (remind track): the stored modes, edits and their rules, the
//! old switch marker's migration, clash checks, and what `tuner --reminder <id>` does while
//! Tuner is closed.

use rusqlite::Connection;
use tuner::config::Config;
use tuner::guard::Guard;
use tuner::model::*;
use tuner::reminders::closed::{self, ClosedOutcome};
use tuner::store::Store;
use tuner::store::user::{self, ReminderEdit};

fn store(name: &str) -> Store {
    let s = Store::open_temp(name, None).unwrap();
    w(&s, |c| {
        c.execute("INSERT INTO source(id, name, kind, url, created_at, generation) VALUES (1, 'S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1)", [])?;
        Ok(())
    });
    s
}

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).unwrap()
}

fn chan(key: &str) -> ItemRef {
    ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(key.into()) }
}

fn rem(key: &str, start: Timestamp, stop: Timestamp, lead_s: i64) -> Reminder {
    Reminder::notify(chan(key), "BBC One".into(), "The Final".into(), start, stop, lead_s)
}

fn add(s: &Store, r: Reminder) -> ReminderId {
    w(s, move |c| user::add_reminder(c, &r))
}

fn get(s: &Store, id: ReminderId) -> Reminder {
    user::reminder(&s.reader().unwrap(), id).unwrap().unwrap()
}

/// Migration 0008: rows made with the pass-1 switch marker become `mode = 'switch'` with no
/// lead; other rows are untouched; running it again changes nothing.
#[test]
fn old_switch_markers_become_switch_mode() {
    let mut c = Connection::open_in_memory().unwrap();
    tuner::store::migrations::migrate(&mut c).unwrap();
    c.execute("INSERT INTO source(id, name, kind, url, created_at, generation) VALUES (1, 'S', 'm3u-url', 'http://h/p.m3u', 0, 1)", []).unwrap();
    for (key, lead) in [("xt:live:1", -1), ("xt:live:2", 120)] {
        c.execute(
            "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state, fp) VALUES (1, ?1, 'C', 'T', 100, 200, ?2, 'pending', '{}')",
            rusqlite::params![key, lead],
        )
        .unwrap();
    }
    let rows = |c: &Connection| -> Vec<(String, String, i64)> {
        let mut st = c.prepare("SELECT channel_key, mode, lead_s FROM reminder ORDER BY channel_key").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
    };
    for _ in 0..2 {
        c.pragma_update(None, "user_version", 7).unwrap();
        tuner::store::migrations::migrate(&mut c).unwrap();
        assert_eq!(rows(&c), [("xt:live:1".into(), "switch".into(), -1), ("xt:live:2".into(), "notify".into(), 120)]);
    }
}

/// Modes, lead, offset and where it opens round-trip; the pass-1 marker is stored as the
/// switch mode; nonsense times are refused; an edit that moves a fired reminder's time ahead
/// makes it pending again.
#[test]
fn modes_offsets_and_edits() {
    let s = store("tuner-rem-edit");
    let now = now();
    let old_style = add(&s, rem("xt:live:1", now + 3600, now + 7200, Reminder::SWITCH_LEAD));
    let r = get(&s, old_style);
    assert_eq!((r.mode, r.lead_s), (ReminderMode::Switch, Reminder::SWITCH_LEAD));
    assert!(r.switches());

    let id = add(&s, Reminder { offset_s: 600, open_in: Some(OpenIn::FullWindow), ..rem("xt:live:2", now + 3600, now + 7200, 300) });
    let r = get(&s, id);
    assert_eq!((r.mode, r.lead_s, r.offset_s, r.open_in), (ReminderMode::Notify, 300, 600, Some(OpenIn::FullWindow)));
    let edited = w(&s, move |c| {
        user::edit_reminder(c, id, &ReminderEdit { lead_s: Some(60), mode: Some(ReminderMode::Switch), open_in: Some(None), ..Default::default() })
    });
    assert_eq!(
        (edited.mode, edited.lead_s, edited.offset_s, edited.open_in),
        (ReminderMode::Switch, Reminder::SWITCH_LEAD, 600, None),
        "a switch has no lead"
    );
    assert!(s.writer().blocking(move |c| user::edit_reminder(c, id, &ReminderEdit { lead_s: Some(2 * 86_400), ..Default::default() })).is_err());
    assert!(s.writer().blocking(move |c| user::edit_reminder(c, id, &ReminderEdit { offset_s: Some(-7200), ..Default::default() })).is_err());
    assert!(s.writer().blocking(move |c| user::add_reminder(c, &Reminder { offset_s: 13 * 3600, ..rem("xt:live:3", 1, 2, 0) })).is_err());
    // an offset past the programme's end would be dismissed unseen: refused
    assert!(s.writer().blocking(move |c| user::add_reminder(c, &Reminder { offset_s: 3600, ..rem("xt:live:3", now, now + 3600, 0) })).is_err());
    assert!(s.writer().blocking(move |c| user::edit_reminder(c, id, &ReminderEdit { offset_s: Some(3600), ..Default::default() })).is_err());

    w(&s, move |c| user::set_reminder_state(c, id, ReminderState::Fired));
    let back = w(&s, move |c| user::edit_reminder(c, id, &ReminderEdit { offset_s: Some(900), ..Default::default() }));
    assert_eq!(back.state, ReminderState::Pending, "its new time is ahead");

    // a notification turned into a switch (MCP schedule_switch on a reminded programme)
    let n = add(&s, rem("xt:live:4", now + 3600, now + 7200, 120));
    w(&s, move |c| user::set_reminder_switch(c, n));
    let pending: Vec<i64> = w(&s, |c| user::pending_switches(c)).into_iter().map(|r| r.id.0).collect();
    assert_eq!(pending, [old_style.0, id.0, n.0]);
    // and back: a note again, with the default lead (the switch marker never lingers)
    let note = w(&s, move |c| user::edit_reminder(c, n, &ReminderEdit { mode: Some(ReminderMode::Notify), ..Default::default() }));
    assert_eq!((note.mode, note.lead_s, note.switches()), (ReminderMode::Notify, 0, false));
}

/// A channel is locked by its own override or by a locked group (a Following switch to it
/// then needs the PIN).
#[test]
fn channels_lock_by_item_or_group() {
    let s = store("tuner-rem-lock");
    w(&s, |c| {
        for (id, key) in [(1, "m3u:a"), (2, "m3u:b"), (3, "m3u:c")] {
            c.execute(
                "INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                 VALUES (?1, 1, ?2, 'C', 'C', 'c', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/1.ts\"}', 0, 1)",
                rusqlite::params![id, key],
            )?;
        }
        c.execute("INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (1, 1, 'channel', 'channel:kids', 'Kids', 0, 1, 1)", [])?;
        c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (1, 2, 0, 1)", [])?;
        c.execute("INSERT INTO group_override(source_id, kind, group_key, locked) VALUES (1, 'channel', 'channel:kids', 1)", [])?;
        c.execute("INSERT INTO item_override(source_id, kind, item_key, locked, fp) VALUES (1, 'channel', 'm3u:a', 1, '{}')", [])?;
        Ok(())
    });
    let c = s.reader().unwrap();
    let locked: Vec<bool> = ["m3u:a", "m3u:b", "m3u:c"].iter().map(|k| user::channel_locked(&c, &chan(k)).unwrap()).collect();
    assert_eq!(locked, [true, true, false]);
}

/// Clash checks read the pending reminders around a programme and say which would clash
/// ("Both semis are at 8").
#[test]
fn clash_checks_against_the_store() {
    let s = store("tuner-rem-clash");
    let semi1 = add(&s, rem("xt:live:1", 72_000, 79_200, 120));
    add(&s, rem("xt:live:2", 90_000, 93_600, 120)); // later
    let fired = add(&s, rem("xt:live:3", 72_000, 79_200, 120));
    w(&s, move |c| user::set_reminder_state(c, fired, ReminderState::Fired));
    let semi2 = rem("xt:live:4", 72_000, 79_200, 120);
    let around = w(&s, |c| user::reminders_overlapping(c, 72_000, 79_200));
    assert_eq!(around.iter().map(|r| r.id).collect::<Vec<_>>(), [semi1]);
    let clashes = tuner::reminders::clashes_with(&semi2, &around);
    assert_eq!(clashes.iter().map(|(r, k)| (r.id, *k)).collect::<Vec<_>>(), [(semi1, tuner::reminders::ClashKind::Overlap)]);
}

/// No spoiler guard.
fn ident(_: &Connection) -> Guard {
    Guard::default()
}

fn cfg(f: impl FnOnce(&mut Config)) -> Config {
    let mut c = Config::default();
    f(&mut c);
    c
}

fn state(s: &Store, id: ReminderId) -> ReminderState {
    get(s, id).state
}

/// `tuner --reminder <id>` with Tuner closed: a due note goes out from that process (no
/// window), naming nothing but what the reminder says, and Watch plays its channel; a switch
/// starts Tuner (in the pop-out unless it says otherwise); early, missed, already fired and
/// hidden ones do nothing visible.
#[test]
fn closed_tuner_fires_by_mode() {
    let s = store("tuner-rem-closed");
    let now = now();
    let on = Config::default();

    let due = add(&s, rem("xt:live:1", now + 100, now + 1800, 120));
    let out = closed::fire(&s, &on, ident, due.0, now).unwrap();
    let ClosedOutcome::Notify(note) = out else { panic!("{out:?}") };
    assert_eq!((note.summary.as_str(), note.body.as_str(), note.play.as_str()), ("The Final", "Starts in 2 min on BBC One", "1:xt:live:1"));
    assert_eq!(state(&s, due), ReminderState::Fired);
    assert_eq!(closed::fire(&s, &on, ident, due.0, now).unwrap(), ClosedOutcome::Exit, "fires once");

    let early = add(&s, rem("xt:live:2", now + 3600, now + 7200, 120));
    assert_eq!(closed::fire(&s, &on, ident, early.0, now).unwrap(), ClosedOutcome::Exit);
    assert_eq!(state(&s, early), ReminderState::Pending, "the next start re-arms it");
    assert_eq!(closed::fire(&s, &cfg(|c| c.reminders.enabled = false), ident, early.0, now + 3500).unwrap(), ClosedOutcome::Exit);

    let ended = add(&s, rem("xt:live:3", now - 7200, now - 3600, 120));
    assert_eq!(closed::fire(&s, &on, ident, ended.0, now).unwrap(), ClosedOutcome::Exit);
    assert_eq!(state(&s, ended), ReminderState::Dismissed);

    let sw = add(&s, Reminder { mode: ReminderMode::Switch, ..rem("xt:live:4", now + 30, now + 1800, 0) });
    assert_eq!(closed::fire(&s, &on, ident, sw.0, now).unwrap(), ClosedOutcome::StartApp { popout: true });
    assert_eq!(state(&s, sw), ReminderState::Pending, "the started Tuner switches");
    let full = add(&s, Reminder { mode: ReminderMode::Switch, open_in: Some(OpenIn::FullWindow), ..rem("xt:live:5", now + 30, now + 1800, 0) });
    assert_eq!(closed::fire(&s, &on, ident, full.0, now).unwrap(), ClosedOutcome::StartApp { popout: false });
    let gone = add(&s, Reminder { mode: ReminderMode::Switch, ..rem("xt:live:6", now - 1000, now + 1800, 0) });
    assert_eq!(closed::fire(&s, &on, ident, gone.0, now).unwrap(), ClosedOutcome::Exit);
    assert_eq!(state(&s, gone), ReminderState::Dismissed, "more than 15 min late");
    assert_eq!(closed::fire(&s, &on, ident, 9999, now).unwrap(), ClosedOutcome::Exit, "no such reminder");

    // the note's title passes the spoiler guard built from the store (a blackout on the topic)
    let guarded = cfg(|c| c.spoiler.enabled = true);
    w(&s, move |c| {
        let rule = SpoilerRule {
            id: SpoilerRuleId(0),
            kind: SpoilerRuleKind::Blackout,
            label: "Arsenal".into(),
            terms: vec!["Arsenal".into()],
            series_norm: None,
            categories: Vec::new(),
            mode: SpoilerMode::Always,
            window_h: 48,
            until_at: Some(now + 86_400),
            sport_descr: true,
            shield: true,
            enabled: true,
            by_agent: false,
            created_at: 0,
        };
        tuner::store::spoiler::add_rule(c, &rule, now).map(drop)
    });
    let result = add(&s, Reminder { programme_title: "Arsenal 2-1 Chelsea".into(), ..rem("xt:live:7", now + 100, now + 1800, 120) });
    let store_guard = |c: &Connection| closed::store_guard(c, &guarded, now);
    let out = closed::fire(&s, &guarded, store_guard, result.0, now).unwrap();
    assert!(matches!(&out, ClosedOutcome::Notify(n) if n.summary == "Arsenal v Chelsea"), "{out:?}");

    // a news flash in quiet hours does nothing (and is done); outside them it starts Tuner
    w(&s, |c| {
        c.execute("INSERT INTO follow_rule(id, name, kind, spec, mode, priority, created_at, updated_at) VALUES (6, 'News at One', 'news', '{}', 'flash', 0, 0, 0)", [])?;
        Ok(())
    });
    let flash = |key: &str| {
        let id = add(&s, Reminder { mode: ReminderMode::Switch, open_in: Some(OpenIn::PopOut), ..rem(key, now + 30, now + 1800, 0) });
        w(&s, move |c| {
            c.execute("UPDATE reminder SET rule_id = 6 WHERE id = ?1", [id.0])?;
            Ok(())
        });
        id
    };
    let quiet = cfg(|c| {
        c.follow.news_flash.quiet_from_min = Some(0);
        c.follow.news_flash.quiet_to_min = Some(24 * 60);
        c.follow.news_flash.skip_when_fullscreen = false;
    });
    let hushed = flash("xt:live:8");
    assert_eq!(closed::fire(&s, &quiet, ident, hushed.0, now).unwrap(), ClosedOutcome::Exit);
    assert_eq!(state(&s, hushed), ReminderState::Fired);
    let loud = flash("xt:live:9");
    let awake = cfg(|c| c.follow.news_flash.skip_when_fullscreen = false);
    assert_eq!(closed::fire(&s, &awake, ident, loud.0, now).unwrap(), ClosedOutcome::StartApp { popout: true });
}

/// Parental control while closed: the session counts as locked. A Following switch to a locked
/// channel becomes a "needs your PIN" note; explicit channels' reminders wait untouched.
#[test]
fn closed_tuner_respects_parental_control() {
    let s = store("tuner-rem-closed-pin");
    let now = now();
    w(&s, |c| {
        c.execute("INSERT INTO item_override(source_id, kind, item_key, locked, fp) VALUES (1, 'channel', 'xt:live:1', 1, '{}')", [])?;
        c.execute(
            "INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
             VALUES (2, 1, 'm3u:late', 'C', 'C', 'c', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/1.ts\"}', 0, 1)",
            [],
        )?;
        c.execute("INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen, explicit) VALUES (2, 1, 'channel', 'channel:xxx', 'X', 0, 1, 1, 1)", [])?;
        c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (2, 2, 0, 1)", [])?;
        c.execute(
            "INSERT INTO follow_rule(id, name, kind, spec, priority, created_at, updated_at) VALUES (4, 'Arsenal', 'team', '{}', 0, 0, 0)",
            [],
        )?;
        Ok(())
    });
    let pin = cfg(|c| {
        c.parental.enabled = true;
        c.parental.pin_hash = Some("x".into());
    });
    let rule = add(&s, Reminder { mode: ReminderMode::Switch, ..rem("xt:live:1", now + 30, now + 1800, 0) });
    w(&s, move |c| {
        c.execute("UPDATE reminder SET rule_id = 4 WHERE id = ?1", [rule.0])?;
        Ok(())
    });
    let out = closed::fire(&s, &pin, ident, rule.0, now).unwrap();
    assert!(matches!(&out, ClosedOutcome::Notify(n) if n.body.ends_with("needs your PIN")), "{out:?}");
    // the user's own switch to the same channel starts Tuner, which asks for the PIN
    let own = add(&s, Reminder { mode: ReminderMode::Switch, ..rem("xt:live:1", now + 40, now + 1800, 0) });
    assert_eq!(closed::fire(&s, &pin, ident, own.0, now).unwrap(), ClosedOutcome::StartApp { popout: true });
    let explicit = add(&s, rem("m3u:late", now + 100, now + 1800, 120));
    assert_eq!(closed::fire(&s, &pin, ident, explicit.0, now).unwrap(), ClosedOutcome::Exit);
    assert_eq!(state(&s, explicit), ReminderState::Pending, "announced once it may be");
}
