//! Following benchmark (DESIGN §33 §5.5). Owner: F-AB. Gate: a full evaluation of 100 rules ×
//! 10 terms over 760k programmes < 1.5 s and a single-rule preview < 1 s (release, M2); cancel
//! within 50 ms. Run: `cargo bench --bench follow` (harness = false).
//!
//! The dataset is the user's scale, generated in-process into a temp database: 9 300 channels
//! each on its own guide channel, 760 466 programmes (2 h slots from 66 h ago, so about 455k
//! are still to come), 2 % of them football fixtures, descriptions of ~200 bytes. Team rules
//! look in descriptions too (the costliest fold). Each measurement is the best of three.

#[path = "../tests/support/seed.rs"]
#[allow(dead_code)]
mod seed;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::params;
use tuner::follow::preview::{PreviewOpts, preview};
use tuner::follow::{FollowKind, FollowReason, FollowRule};
use tuner::jobs::follow::{Opts, prepare};
use tuner::model::{FollowRuleId, Timestamp};
use tuner::store::{Store, follow as store_follow, library};

const CHANNELS: usize = 9_300;
const PROGRAMMES: usize = 760_466;
const NOW: Timestamp = 1_790_000_000;
const SLOT: i64 = 2 * 3600;
const CLUBS: usize = 200;

fn club(i: usize) -> String {
    const PARTS: [&str; 10] = ["Athletic", "Rovers", "United", "City", "Wanderers", "Albion", "Town", "County", "Rangers", "Villa"];
    format!("{} {}", ["North", "South", "East", "West", "Port", "Old", "New", "Upper", "Lower", "Great"][i % 10], PARTS[(i / 10) % 10])
        + &format!(" {}", i / 100)
}

fn build() -> Store {
    let store = Store::open_temp("follow-bench", None).expect("temp store");
    store
        .writer()
        .blocking(|c| {
            let src = seed::seed_source(c, "TV")?;
            let ids = seed::seed_channels(c, src, CHANNELS)?;
            let tx = c.transaction()?;
            tx.execute("INSERT INTO epg_source(name, kind, url, active_gen, max_stop) VALUES ('Guide', 'url', 'http://127.0.0.1/g.xml', 1, ?1)", [NOW + 30 * 86_400])?;
            let epg = tx.last_insert_rowid();
            let mut epg_ids = Vec::with_capacity(CHANNELS);
            {
                let mut ec = tx.prepare("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, 1)")?;
                let mut map = tx.prepare("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)")?;
                for (i, ch) in ids.iter().enumerate() {
                    ec.execute(params![epg, format!("ch{i}.tv")])?;
                    let e = tx.last_insert_rowid();
                    map.execute(params![ch.0, e])?;
                    epg_ids.push(e);
                }
                let mut pr = tx.prepare(
                    "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, descr, categories, season, episode, flags)
                     VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )?;
                let per = PROGRAMMES.div_ceil(CHANNELS);
                let mut n = 0;
                'all: for (ci, e) in epg_ids.iter().enumerate() {
                    for slot in 0..per {
                        if n == PROGRAMMES {
                            break 'all;
                        }
                        let start = NOW - 33 * SLOT + slot as i64 * SLOT;
                        let k = ci * 31 + slot * 7;
                        if k % 50 == 0 {
                            let (a, b) = (club(k % CLUBS), club((k / 3 + 1) % CLUBS));
                            let descr = format!("{a} take on {b} in this league fixture, with all the build-up, analysis and reaction from the ground.");
                            pr.execute(params![epg, e, start, start + SLOT, "Live Football League", format!("{a} v {b}"), descr, "Sport\x1fFootball", None::<i64>, None::<i64>, 2])?;
                        } else {
                            let title = format!("Programme {}", k % 2_000);
                            let descr = format!(
                                "Episode {} of the series in which the presenters visit another part of the country, meet the people who live there and find out what makes it special."
                            , k % 40);
                            pr.execute(params![epg, e, start, start + SLOT, title, format!("Part {}", k % 12), descr, "Entertainment", (k % 9) as i64, (k % 40) as i64, 0])?;
                        }
                        n += 1;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .expect("seed");
    store
}

fn rules() -> Vec<FollowRule> {
    let cfg = Default::default();
    (0..100)
        .map(|i| {
            let (kind, terms): (FollowKind, Vec<String>) = match i % 10 {
                0..=4 => (FollowKind::Team, (0..10).map(|j| if j == 0 { club(i * 2) } else { format!("{} alias {j}", club(i * 2)) }).collect()),
                5..=7 => (FollowKind::Show, (0..10).map(|j| format!("Programme {}", i * 10 + j)).collect()),
                _ => (FollowKind::Event, (0..10).map(|j| format!("Cup {i} round {j}")).collect()),
            };
            let mut r = FollowRule::draft(format!("rule {i}"), kind, terms, &cfg, NOW);
            r.id = FollowRuleId(i as i64 + 1);
            r.priority = i as i64;
            r
        })
        .collect()
}

fn best_of<R>(n: usize, mut f: impl FnMut() -> R) -> (Duration, R) {
    let mut best = Duration::MAX;
    let mut last = None;
    for _ in 0..n {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed());
        last = Some(r);
    }
    (best, last.expect("ran"))
}

fn main() {
    let t = Instant::now();
    let store = build();
    eprintln!("follow bench: dataset of {PROGRAMMES} programmes on {CHANNELS} channels built in {:.1} s", t.elapsed().as_secs_f64());
    let rules = rules();
    store
        .writer()
        .blocking({
            let rules = rules.clone();
            move |c| {
                for r in &rules {
                    let mut r = r.clone();
                    r.id = FollowRuleId(0);
                    store_follow::save_rule(c, &r, 1000, NOW)?;
                }
                Ok(())
            }
        })
        .expect("rules");
    let conn = store.reader().expect("reader");
    let lib = library::load(&conn).expect("library");
    let opts = Opts { now: NOW, reason: FollowReason::Rules, cfg: Default::default(), pin_active: false, tz: jiff::tz::TimeZone::UTC };

    let (full, ev) = best_of(3, || prepare(&conn, &lib, &opts, &|| false).expect("evaluate").expect("not skipped"));
    eprintln!(
        "evaluation, 100 rules × 10 terms: {:.0} ms ({} rows, {} hits, {} steps)",
        full.as_secs_f64() * 1e3,
        ev.stats.rows,
        ev.stats.hits,
        ev.plan.steps.len()
    );

    let popts = PreviewOpts { now: NOW, days: 14, limit: 50, pin_active: false, max_pending: 300 };
    let (one, p) = best_of(3, || preview(&conn, &lib, &rules[0], &popts, &jiff::tz::TimeZone::UTC, &|| false).expect("preview"));
    eprintln!("single-rule preview: {:.0} ms ({} matches, {} left out)", one.as_secs_f64() * 1e3, p.matches.len(), p.left_out.len());

    // cancel: the flag goes up 100 ms into a full scan; the scan notices within 8 192 rows
    let flag = AtomicBool::new(false);
    let raised = std::sync::OnceLock::new();
    let latency = std::thread::scope(|s| {
        let started = Instant::now();
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            let _ = raised.set(Instant::now());
            flag.store(true, Ordering::Release);
        });
        let r = prepare(&conn, &lib, &opts, &|| flag.load(Ordering::Acquire));
        let ended = Instant::now();
        assert!(matches!(r, Err(tuner::error::Error::Cancelled)) || started.elapsed() < Duration::from_millis(100), "cancelled");
        raised.get().map_or(Duration::ZERO, |r| ended.saturating_duration_since(*r))
    });
    eprintln!("cancel latency: {:.1} ms", latency.as_secs_f64() * 1e3);

    let gate = [
        ("evaluation < 1.5 s", full < Duration::from_millis(1500)),
        ("preview < 1 s", one < Duration::from_secs(1)),
        ("cancel < 50 ms", latency < Duration::from_millis(50)),
    ];
    for (what, ok) in gate {
        eprintln!("  {} {what}", if ok { "PASS" } else { "FAIL" });
    }
}
