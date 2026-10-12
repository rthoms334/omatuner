//! Following's evaluation end to end on a real database (DESIGN §33 §9.5): rules in, the guide
//! scanned across two generations, reminders made with `rule_id`, a moved kick-off moving its
//! reminder, a removed reminder staying removed, explicit channels never chosen, the early
//! exit. Owner: F-AB (fcore).

mod support;

use rusqlite::{Connection, params};
use tuner::follow::plan::MatchState;
use tuner::follow::{FollowKind, FollowMode, FollowReason, FollowRule};
use tuner::jobs::follow::{Opts, commit, prepare};
use tuner::model::*;
use tuner::store::{Store, follow as store_follow, library};

const H: i64 = 3600;
/// "Now": Monday 2026-09-21 12:00 UTC.
const NOW: Timestamp = 1_789_992_000;
/// Saturday 15:00 UTC.
const SAT: Timestamp = NOW + 5 * 24 * H + 3 * H;

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

/// Three channels on one guide; channel 2 is explicit and shows the same match.
fn seed(s: &Store) -> Vec<ChannelId> {
    w(s, |c| {
        let src = support::seed::seed_source(c, "TV")?;
        let ids = support::seed::seed_channels(c, src, 3)?;
        c.execute("UPDATE channel SET explicit = 1 WHERE id = ?1", [ids[2].0])?;
        c.execute(
            "INSERT INTO epg_source(name, kind, url, active_gen, max_stop) VALUES ('Guide', 'url', 'http://127.0.0.1/g.xml', 1, ?1)",
            [SAT + 14 * 24 * H],
        )?;
        let epg = c.last_insert_rowid();
        for (i, ch) in ids.iter().enumerate() {
            c.execute("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, 1)", params![epg, format!("ch{i}.tv")])?;
            c.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)", params![ch.0, c.last_insert_rowid()])?;
        }
        Ok(ids)
    })
}

/// A guide generation: the match on channel 0 (and the explicit channel 2) at `kick_off`, a
/// women's match, a repeat; the previous generation goes.
fn guide(s: &Store, gen_: i64, kick_off: Timestamp) {
    w(s, move |c| {
        let epg: i64 = c.query_row("SELECT id FROM epg_source", [], |r| r.get(0))?;
        let ch =
            |i: i64| -> rusqlite::Result<i64> { c.query_row("SELECT id FROM epg_channel WHERE xmltv_id = ?1", [format!("ch{i}.tv")], |r| r.get(0)) };
        let rows = [
            (ch(0)?, kick_off, "Premier League Football", Some("Arsenal v Chelsea"), 2),
            (ch(2)?, SAT - 6 * H, "Arsenal v Spurs", None, 2),
            (ch(1)?, SAT + 24 * H, "Arsenal Women v Chelsea Women", None, 2),
            (ch(1)?, SAT + 2 * 24 * H, "Chelsea v Arsenal (R)", None, 0),
        ];
        for (epg_ch, start, title, sub, flags) in rows {
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![epg, epg_ch, gen_, start, start + 2 * H, title, sub, flags],
            )?;
        }
        c.execute("UPDATE epg_source SET active_gen = ?1", [gen_])?;
        c.execute("DELETE FROM programme WHERE gen < ?1", [gen_])?;
        Ok(())
    })
}

fn evaluate(s: &Store, reason: FollowReason, now: Timestamp) -> Option<tuner::jobs::follow::Committed> {
    let opts = Opts { now, reason, cfg: Default::default(), pin_active: false, tz: jiff::tz::TimeZone::UTC };
    let c = s.reader().unwrap();
    // an unlocked session's snapshot: explicit channels must stay out all the same
    let lib = library::load_visible(&c, library::Visibility::ALL).unwrap();
    let ev = prepare(&c, &lib, &opts, &|| false).unwrap()?;
    Some(w(s, move |c| commit(c, &ev, &opts)))
}

fn reminders(s: &Store) -> Vec<Reminder> {
    tuner::store::user::all_reminders(&s.reader().unwrap()).unwrap()
}

#[test]
fn a_rule_keeps_its_reminders_in_step_with_the_guide() {
    let s = Store::open_temp("follow-job", None).unwrap();
    let ids = seed(&s);
    let rule = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &Default::default(), NOW);
    let rule_id = w(&s, move |c| store_follow::save_rule(c, &rule, 100, NOW));
    guide(&s, 1, SAT);

    // one reminder: the live match on the clean channel — not the explicit channel's game, the
    // women's match or the repeat
    evaluate(&s, FollowReason::Rules, NOW).expect("evaluated");
    let r = reminders(&s);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].start, r[0].rule_id, r[0].mode), (SAT, Some(rule_id), ReminderMode::Notify));
    assert_eq!(r[0].programme_title, "Arsenal", "Team rules are spoiler-safe by default: the rule name only");
    let lib = library::load(&s.reader().unwrap()).unwrap();
    assert_eq!(lib.live.by_ref(&r[0].channel), Some(ids[0]));

    // the same guide again a minute later: nothing to do
    assert!(evaluate(&s, FollowReason::Guide, NOW + 60).is_none(), "early exit");

    // the next import moves the kick-off to 17:30: the same match, its reminder moved
    guide(&s, 2, SAT + 9000);
    let done = evaluate(&s, FollowReason::Guide, NOW + 120).expect("a new guide is evaluated");
    assert_eq!(done.moves, ["Arsenal moved to Sat 17:30"]);
    let r = reminders(&s);
    assert_eq!(r.iter().map(|r| r.start).collect::<Vec<_>>(), [SAT + 9000], "moved, not duplicated");
    let ledger = store_follow::ledger(&s.reader().unwrap(), &|_| true).unwrap();
    assert_eq!(ledger.len(), 1);

    // you remove the reminder: it stays removed through later evaluations
    let id = r[0].id;
    w(&s, move |c| tuner::store::user::remove_reminder(c, id, RemoveWhy::User));
    for t in [NOW + 180, NOW + 240] {
        evaluate(&s, FollowReason::Rules, t).expect("evaluated");
        assert!(reminders(&s).is_empty());
    }
    let upcoming = store_follow::upcoming(&s.reader().unwrap(), NOW, 10).unwrap();
    assert_eq!(upcoming.iter().map(|u| (u.state, u.title.as_str())).collect::<Vec<_>>(), [(MatchState::Skipped, "Arsenal")]);

    // switch mode: a new rule for the women's side switches by itself
    let mut women = FollowRule::draft("Arsenal Women", FollowKind::Team, vec!["Arsenal Women".into()], &Default::default(), NOW);
    women.spec.without.retain(|w| w != "Women");
    women.mode = FollowMode::Switch;
    w(&s, move |c| store_follow::save_rule(c, &women, 100, NOW + 300));
    evaluate(&s, FollowReason::Rules, NOW + 300).expect("evaluated");
    let r = reminders(&s);
    assert_eq!(r.iter().map(|r| (r.start, r.switches())).collect::<Vec<_>>(), [(SAT + 24 * H, true)]);

    // deleting a rule cancels the reminders it made
    let women_id = r[0].rule_id.unwrap();
    assert_eq!(w(&s, move |c| store_follow::delete_rule(c, women_id, false)), 1);
    assert!(reminders(&s).is_empty());
}

/// News flash: the bulletin matched each weekday from the guide (not a clock time), each one a
/// pop-out switch the scheduler recognises as a flash, headlines only.
#[test]
fn a_news_flash_follows_the_bulletin_on_weekdays() {
    let s = Store::open_temp("follow-news", None).unwrap();
    seed(&s);
    w(&s, |c| {
        let (epg, ch): (i64, i64) =
            c.query_row("SELECT epg_source_id, id FROM epg_channel WHERE xmltv_id = 'ch1.tv'", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        for day in 0..7 {
            // Monday–Sunday at 13:00; Wednesday's moved to 13:05
            let start = NOW + H + day * 24 * H + if day == 2 { 300 } else { 0 };
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title) VALUES (?1, ?2, 1, ?3, ?4, 'BBC News at One')",
                params![epg, ch, start, start + 1800],
            )?;
        }
        Ok(())
    });
    let mut news = FollowRule::draft("News at One", FollowKind::News, vec!["BBC News at One".into()], &Default::default(), NOW);
    news.spec.days = tuner::follow::spec::WEEKDAYS;
    w(&s, move |c| store_follow::save_rule(c, &news, 100, NOW));
    evaluate(&s, FollowReason::Rules, NOW).expect("evaluated");
    let r = reminders(&s);
    let starts: Vec<Timestamp> = r.iter().map(|r| r.start - NOW - H).collect();
    assert_eq!(starts, [0, 24 * H, 2 * 24 * H + 300, 3 * 24 * H, 4 * 24 * H], "Monday to Friday, at the guide's time");
    assert!(r.iter().all(|r| r.switches() && r.open_in == Some(OpenIn::PopOut)));
    let flash = store_follow::flash_of(&s.reader().unwrap(), r[0].id).unwrap().expect("a flash");
    assert_eq!(flash.spec.length, tuner::follow::flash::FlashLength::Headlines(5));
}
