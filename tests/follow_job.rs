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

    // editing the rule's mode and lead remakes its reminder with them
    let women_id = r[0].rule_id.unwrap();
    let mut edited = store_follow::rule(&s.reader().unwrap(), women_id).unwrap().unwrap();
    (edited.mode, edited.lead_s) = (FollowMode::Notify, 600);
    w(&s, move |c| store_follow::save_rule(c, &edited, 100, NOW + 360));
    evaluate(&s, FollowReason::Rules, NOW + 360).expect("evaluated");
    let r = reminders(&s);
    assert_eq!(r.iter().map(|r| (r.start, r.switches(), r.lead_s)).collect::<Vec<_>>(), [(SAT + 24 * H, false, 600)]);

    // a second rule on the same match adopts that reminder; pausing the first hands it over
    let mut gunners = FollowRule::draft("Gunners", FollowKind::Team, vec!["Arsenal Women".into()], &Default::default(), NOW);
    gunners.spec.without.retain(|w| w != "Women");
    let gunners_id = w(&s, move |c| store_follow::save_rule(c, &gunners, 100, NOW + 420));
    evaluate(&s, FollowReason::Rules, NOW + 420).expect("evaluated");
    assert_eq!(reminders(&s).len(), 1, "adopted, not doubled");
    w(&s, move |c| store_follow::set_rule_enabled(c, women_id, false, NOW + 480));
    evaluate(&s, FollowReason::Rules, NOW + 480).expect("evaluated");
    let r = reminders(&s);
    assert_eq!(r.iter().map(|r| (r.start, r.rule_id)).collect::<Vec<_>>(), [(SAT + 24 * H, Some(gunners_id))]);

    // deleting a rule cancels the reminders it made
    assert_eq!(w(&s, move |c| store_follow::delete_rule(c, gunners_id, false)), 1);
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

    // at fire time: it pops up, but never in quiet hours or over a film (a notification)
    use tuner::follow::flash::{FlashCtx, FlashSkip};
    let mut cfg = tuner::config::Config::default();
    let minute = tuner::reminders::local_minute_of_day(r[0].start);
    let at = |cfg: &tuner::config::Config, main_kind| {
        tuner::reminders::flash_skip(&flash, &cfg.follow.news_flash, &FlashCtx { minute_of_day: minute, main_kind, ..Default::default() }, &r[0])
    };
    assert_eq!(at(&cfg, Some(tuner::playback::Kind::Live)), None);
    assert_eq!(at(&cfg, Some(tuner::playback::Kind::Vod)), Some(FlashSkip::WouldInterrupt), "never over a film");
    (cfg.follow.news_flash.quiet_from_min, cfg.follow.news_flash.quiet_to_min) = (Some(0), Some(24 * 60));
    assert_eq!(at(&cfg, None), Some(FlashSkip::QuietHours));
}

/// DESIGN §43 §8.7: the same evaluator/ledger produces recording jobs, never reminders.
#[test]
fn recording_rule_schedules_new_matches_once_and_cancels_with_rule() {
    let s = Store::open_temp("follow-recording", None).unwrap();
    seed(&s);
    let mut rule = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &Default::default(), NOW);
    rule.mode = FollowMode::Record;
    let id = w(&s, move |c| store_follow::save_rule(c, &rule, 100, NOW));
    guide(&s, 1, SAT);
    evaluate(&s, FollowReason::Rules, NOW).unwrap();
    let schedule = |c: &mut Connection| {
        let lib = library::load(c)?;
        tuner::downloads::maintenance::following(c, &lib, &Default::default(), NOW)
    };
    w(&s, schedule);
    w(&s, schedule);
    let rows = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap();
    assert_eq!(rows.len(), 1, "one new event; explicit channel and repeat excluded");
    assert_eq!(rows[0].state, tuner::downloads::State::Scheduled);
    assert_eq!(rows[0].prog_start, Some(SAT));
    assert!(reminders(&s).is_empty(), "record is a Following action, not a reminder");
    guide(&s, 2, SAT + 600);
    evaluate(&s, FollowReason::Guide, NOW + 120).unwrap();
    w(&s, schedule);
    let rows = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].prog_start, Some(SAT + 600));
    w(&s, move |c| {
        c.execute("DELETE FROM follow_rule WHERE id=?1", [id])?;
        Ok(())
    });
    w(&s, schedule);
    assert_eq!(tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap()[0].state, tuner::downloads::State::Cancelled);
}

#[test]
fn series_recording_skips_a_downloaded_episode_and_repeated_airings() {
    let s = Store::open_temp("follow-downloaded", None).unwrap();
    seed(&s);
    w(&s, |c| {
        let (epg, ch): (i64, i64) =
            c.query_row("SELECT epg_source_id,id FROM epg_channel WHERE xmltv_id='ch0.tv'", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        for (offset, episode) in [(0, 4), (3600, 5), (86400, 5)] {
            c.execute("INSERT INTO programme(epg_source_id,epg_channel_id,gen,start,stop,title,season,episode,flags) VALUES (?1,?2,1,?3,?4,'Taskmaster',20,?5,2)",params![epg,ch,SAT+offset,SAT+offset+1800,episode])?;
        }
        c.execute(
            "INSERT INTO download(position,kind,title,state,added_at,fp) VALUES (0,'episode','Taskmaster','done',?1,?2)",
            params![NOW - 3600, r#"{"download_season":20,"download_episode":4}"#],
        )?;
        let mut rule = FollowRule::draft("Taskmaster", FollowKind::Show, vec!["Taskmaster".into()], &Default::default(), NOW);
        rule.mode = FollowMode::Record;
        store_follow::save_rule(c, &rule, 100, NOW)?;
        Ok(())
    });
    evaluate(&s, FollowReason::Rules, NOW).unwrap();
    w(&s, |c| {
        let lib = library::load(c)?;
        tuner::downloads::maintenance::following(c, &lib, &Default::default(), NOW)?;
        tuner::downloads::maintenance::following(c, &lib, &Default::default(), NOW + 60)?;
        Ok(())
    });
    let rows = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap();
    assert_eq!(rows.len(), 2, "one downloaded episode plus one new recording; no repeat");
    assert_eq!(rows[1].fp["download_event"], "taskmaster#s20e05");
}

#[test]
fn recording_identity_keeps_fixture_dates_and_failed_matches_can_be_scheduled_again() {
    let s = Store::open_temp("follow-dates", None).unwrap();
    seed(&s);
    let mut rule = FollowRule::draft("Arsenal", FollowKind::Team, vec!["Arsenal".into()], &Default::default(), NOW);
    rule.mode = FollowMode::Record;
    w(&s, move |c| store_follow::save_rule(c, &rule, 100, NOW));
    guide(&s, 1, SAT);
    evaluate(&s, FollowReason::Rules, NOW).unwrap();
    let schedule = |c: &mut Connection| {
        let lib = library::load(c)?;
        tuner::downloads::maintenance::following(c, &lib, &Default::default(), NOW)
    };
    w(&s, schedule);
    let first = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap()[0].clone();
    assert!(first.fp["download_event"].as_str().unwrap().contains('@'));
    let first_id = first.id;
    w(&s, move |c| {
        c.execute("UPDATE download SET state='done',fp=json_set(fp,'$.download_follow_rule',999) WHERE id=?1", [first_id])?;
        Ok(())
    });
    guide(&s, 2, SAT + 3600);
    evaluate(&s, FollowReason::Guide, NOW + 60).unwrap();
    w(&s, schedule);
    assert_eq!(
        tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap().len(),
        2,
        "another rule's completed recording does not consume this rule's identity"
    );
    guide(&s, 3, SAT + 86400);
    evaluate(&s, FollowReason::Guide, NOW + 120).unwrap();
    w(&s, schedule);
    let rows = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap();
    assert_eq!(rows.len(), 3, "the next day's fixture is new");
    assert_ne!(rows[0].fp["download_event"], rows[2].fp["download_event"]);
    for state in [tuner::downloads::State::Missed, tuner::downloads::State::Failed, tuner::downloads::State::Cancelled] {
        let rows = tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap();
        let id = rows.last().unwrap().id;
        w(&s, move |c| tuner::store::downloads::set_state(c, id, state, None));
        w(&s, schedule);
        assert_eq!(tuner::store::downloads::jobs(&s.reader().unwrap()).unwrap().len(), rows.len() + 1);
    }
}
