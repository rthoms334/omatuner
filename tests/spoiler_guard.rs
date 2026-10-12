//! The spoiler guard engine over a real store (DESIGN §34 §7): embargo windows from the guide,
//! series boundaries from merged progress, the zap shield, the MCP reply guard, notifications,
//! Following's spoiler-safe rules and the backup round trip. OWNER: S1.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{Connection, params};
use tuner::config::Config;
use tuner::guard::{Field, Guard, Guarded, Policy, ReasonKind, Subject};
use tuner::model::*;
use tuner::spoiler::snapshot::build;
use tuner::store::Store;
use tuner::store::library::{self, Library};
use tuner::store::spoiler as db;

/// Sat 17:30 (on the hour + 30 min).
const KICK_OFF: Timestamp = 1_791_576_000 + 1800;
const H: i64 = 3600;

fn store(name: &str) -> Store {
    Store::open_temp(name, None).expect("temp store")
}

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

/// The world: two playlists ("TV", "TV 2"), sport and news channels sharing one guide, an
/// explicit channel, a series in both playlists.
struct World {
    store: Store,
    /// name → (channel id, guide channel id)
    ch: HashMap<&'static str, (ChannelId, i64)>,
}

impl World {
    fn new(name: &str) -> Self {
        let store = store(name);
        let ch = w(&store, |c| {
            c.execute_batch(
                "INSERT INTO source(name, kind, url, created_at, generation) VALUES ('TV', 'm3u-url', 'http://127.0.0.1/a.m3u', 0, 1);
                 INSERT INTO source(name, kind, url, created_at, generation) VALUES ('TV 2', 'm3u-url', 'http://127.0.0.1/b.m3u', 0, 1);
                 INSERT INTO epg_source(name, kind, url, active_gen, n_channels) VALUES ('G', 'url', 'http://127.0.0.1/g.xml', 1, 4);",
            )?;
            let mut out = HashMap::new();
            // (name, source, guide xmltv id, explicit)
            for (i, (name, src, xmltv, explicit)) in [
                ("Sky Sports Main Event", 1, "sse", false),
                ("Sky Sports News", 1, "ssn", false),
                ("BBC Two", 1, "bbc2", false),
                ("Blue Night", 1, "blue", true),
                ("Sky Sports Main Event 2", 2, "sse", false),
            ]
            .into_iter()
            .enumerate()
            {
                c.execute(
                    "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, stream, provider_order, seen_gen, explicit)
                     VALUES (?1, ?2, ?3, ?3, ?4, ?5, '{\"t\":\"url\",\"url\":\"http://127.0.0.1/x.ts\"}', ?5, 1, ?6)",
                    params![src, format!("m3u:{i}"), name, tuner::sources::identity::normalize_name(name), i as i64 + 1, explicit],
                )?;
                let id = c.last_insert_rowid();
                let eid: i64 = match c.query_row("SELECT id FROM epg_channel WHERE xmltv_id = ?1", [xmltv], |r| r.get(0)) {
                    Ok(e) => e,
                    Err(_) => {
                        c.execute("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (1, ?1, 1)", [xmltv])?;
                        c.last_insert_rowid()
                    }
                };
                c.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)", params![id, eid])?;
                out.insert(name, (ChannelId(id), eid));
            }
            // Severance in both playlists, 2 seasons × 9 episodes
            for src in [1, 2] {
                c.execute(
                    "INSERT INTO series(source_id, key, name, clean_name, norm_name, provider_order, seen_gen) VALUES (?1, 'm3u:s:sev', 'Severance', 'Severance', 'severance', 0, 1)",
                    [src],
                )?;
                let sid = c.last_insert_rowid();
                for s in 1..=2 {
                    for e in 1..=9 {
                        c.execute(
                            "INSERT INTO episode(series_id, source_id, key, season, episode, title, stream) VALUES (?1, ?2, ?3, ?4, ?5, 'E', '{}')",
                            params![sid, src, format!("m3u:ep-{s}-{e}"), s, e],
                        )?;
                    }
                }
            }
            Ok(out)
        });
        Self { store, ch }
    }

    /// A programme on a guide channel (and its FTS row).
    #[allow(clippy::too_many_arguments)]
    fn prog(
        &self,
        chan: &'static str,
        start: Timestamp,
        stop: Timestamp,
        title: &str,
        sub: Option<&str>,
        descr: Option<&str>,
        cats: &str,
        se: Option<(i32, i32)>,
    ) {
        let eid = self.ch[chan].1;
        let (title, sub, descr, cats) = (title.to_owned(), sub.map(str::to_owned), descr.map(str::to_owned), cats.to_owned());
        w(&self.store, move |c| {
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, descr, categories, season, episode)
                 VALUES (1, ?1, 1, ?2, ?3, ?4, ?5, ?6, NULLIF(?7, ''), ?8, ?9)",
                params![eid, start, stop, title, sub, descr, cats, se.map(|s| s.0), se.map(|s| s.1)],
            )?;
            c.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, ?3)", params![c.last_insert_rowid(), title, sub])?;
            Ok(())
        });
    }

    fn lib(&self) -> Library {
        library::load(&self.store.reader().unwrap()).unwrap()
    }

    fn guard(&self, cfg: &Config, now: Timestamp) -> Guard {
        let snap = build(&self.store.reader().unwrap(), cfg, &self.lib(), now).unwrap();
        Guard::with_snapshot(snap)
    }

    fn topic(&self, label: &str, terms: &[&str], mode: SpoilerMode, cats: &[&str]) -> SpoilerRuleId {
        let r = SpoilerRule {
            id: SpoilerRuleId(0),
            kind: SpoilerRuleKind::Topic,
            label: label.into(),
            terms: terms.iter().map(|t| t.to_string()).collect(),
            series_norm: None,
            categories: cats.iter().map(|t| t.to_string()).collect(),
            mode,
            window_h: 48,
            until_at: None,
            sport_descr: true,
            shield: true,
            enabled: true,
            by_agent: false,
            agent: AgentName::default(),
            created_at: 0,
        };
        w(&self.store, move |c| db::add_rule(c, &r, 0))
    }
}

fn prog<'a>(title: &'a str, sub: Option<&'a str>, cats: &'a [String]) -> Subject<'a> {
    Subject::Programme { title, sub_title: sub, categories: cats, season_episode: None, start: 0, is_new: false }
}

fn hidden(g: &Guarded<'_>) -> bool {
    matches!(g, Guarded::Hidden { .. })
}

/// The Arsenal match on Sky Sports Main Event (both playlists), a news show mentioning it, the
/// highlights on BBC Two and an "Arsenal" match on the explicit channel.
fn match_day(name: &str) -> World {
    let world = World::new(name);
    let sport = "Sport\u{1f}Football";
    world.prog("Sky Sports Main Event", KICK_OFF, KICK_OFF + 2 * H, "PL: Arsenal v Chelsea", None, Some("Live coverage"), sport, None);
    world.prog(
        "Sky Sports News",
        KICK_OFF + H,
        KICK_OFF + 3 * H,
        "Sky Sports News",
        Some("Arsenal v Chelsea reaction"),
        Some("Saka scored twice"),
        sport,
        None,
    );
    world.prog("BBC Two", KICK_OFF + 5 * H, KICK_OFF + 6 * H, "Match of the Day", None, Some("Arsenal beat Chelsea 2-1"), "Sport", None);
    world.prog("Blue Night", KICK_OFF + 3 * H, KICK_OFF + 4 * H, "Arsenal Nights", None, None, "", None);
    world.topic("Arsenal", &["Arsenal", "Gunners"], SpoilerMode::UntilSeen, &["Sport"]);
    world
}

#[test]
fn an_embargo_opens_at_kick_off_and_lifts_when_seen_or_after_its_window() {
    let world = match_day("spoiler-embargo");
    // the repeat the next day on another channel is the same match
    let repeat = KICK_OFF + 24 * H;
    world.prog("BBC Two", repeat, repeat + 2 * H, "Arsenal v Chelsea (R)", None, None, "Sport", None);
    let cfg = Config::default();
    let g = world.guard(&cfg, KICK_OFF - 6 * H);
    let sport: Vec<String> = vec!["Sport".into()];
    let news = prog("Sky Sports News", Some("Arsenal v Chelsea reaction"), &sport);

    // before kick-off nothing is guarded: fixtures stay plannable
    assert!(!g.judge(&news, KICK_OFF - 60).guarded());
    assert_eq!(g.outside_title("Arsenal 2-1 Chelsea highlights", KICK_OFF - 60), "Arsenal 2-1 Chelsea highlights");

    // half-time: the news sub-title and every sport description are hidden, scores rewritten
    let t = KICK_OFF + H / 2;
    assert!(hidden(&g.text(&news, Field::SubTitle, "Arsenal v Chelsea reaction", Policy::Normal, t)));
    let golf = prog("Golf Highlights", Some("Round three"), &sport);
    assert!(hidden(&g.text(&golf, Field::Description, "McIlroy leads", Policy::Normal, t)), "sport descriptions while embargoed");
    let docs: Vec<String> = vec!["Documentary".into()];
    let doc = prog("Arsenal of Democracy", None, &docs);
    assert!(!g.judge(&doc, t).guarded(), "out of the Sport scope");
    assert_eq!(g.outside_title("Arsenal 2-1 Chelsea highlights", t), "Arsenal v Chelsea highlights");
    assert_eq!(g.outside_title("Gunners beat Chelsea", t), "Reminder", "a result verb with the topic hides the title");
    let v = g.judge(&news, t);
    let r = v.reason.expect("a reason");
    assert_eq!((r.rule.as_str(), r.kind, r.until), ("Arsenal", ReasonKind::Embargo, Some(KICK_OFF + 2 * H + 48 * H)));
    // Settings / MCP status name the event (never the explicit channel's)
    let topics = g.topics(t);
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].event, Some(("PL: Arsenal v Chelsea".to_owned(), KICK_OFF)));
    assert!(topics[0].active);

    // lifted by itself 48 h after the final whistle, with the same snapshot
    assert!(!g.judge(&news, KICK_OFF + 2 * H + 48 * H).guarded());

    // one "seen" (the repeat on TV 2's copy has the same event) lifts it everywhere
    let seen = |title: &str, start: Timestamp| {
        let key = tuner::spoiler::event_key_of(title, None, None, None, start);
        let row = SeenRow {
            event_key: key.clone(),
            first_start: start,
            title: title.into(),
            series_norm: None,
            season: None,
            episode: None,
            seen_at: t,
            how: SeenHow::Marked,
            agent: AgentName::default(),
        };
        assert!(w(&world.store, move |c| db::mark_seen(c, &row)));
        key
    };
    let key = seen("PL: Arsenal v Chelsea", KICK_OFF);
    let g = world.guard(&cfg, t);
    assert!(!g.judge(&news, t).guarded(), "seen: lifted");
    assert!(!g.topics(t)[0].active);
    let at_repeat = repeat + H;
    assert!(!world.guard(&cfg, at_repeat).judge(&news, at_repeat).guarded(), "the next day's repeat opens no new window");
    // Undo guards again
    assert!(w(&world.store, move |c| db::unmark_seen(c, &key, KICK_OFF)));
    assert!(world.guard(&cfg, t).judge(&news, t).guarded());
    // watching only the repeat lifts the match's window too
    let key = seen("Arsenal v Chelsea (R)", repeat);
    let after = repeat + 3 * H;
    assert!(!world.guard(&cfg, after).judge(&news, after).guarded(), "seen on the repeat");
    assert!(w(&world.store, move |c| db::unmark_seen(c, &key, repeat)));

    // the guard off: nothing
    let off = Config { spoiler: tuner::config::SpoilerConfig { enabled: false, ..Default::default() }, ..Default::default() };
    assert!(!world.guard(&off, t).judge(&news, t).guarded());
}

#[test]
fn always_topics_never_lift_and_blackouts_end_on_time() {
    let world = World::new("spoiler-always");
    world.topic("Strictly", &["Strictly"], SpoilerMode::Always, &[]);
    let until = KICK_OFF + 3 * H;
    let blackout = SpoilerRule {
        id: SpoilerRuleId(0),
        kind: SpoilerRuleKind::Blackout,
        label: "F1".into(),
        terms: vec!["Grand Prix".into()],
        series_norm: None,
        categories: vec![],
        mode: SpoilerMode::UntilSeen,
        window_h: 0,
        until_at: Some(until),
        sport_descr: false,
        shield: true,
        enabled: true,
        by_agent: false,
        agent: AgentName::default(),
        created_at: 0,
    };
    w(&world.store, move |c| db::add_rule(c, &blackout, 0));
    let g = world.guard(&Config::default(), KICK_OFF);
    let ent: Vec<String> = vec!["Entertainment".into()];
    let strictly = prog("Strictly Come Dancing", Some("The final: who lifts the glitterball?"), &ent);
    for t in [KICK_OFF - 100 * H, KICK_OFF + 1000 * H] {
        assert!(hidden(&g.text(&strictly, Field::SubTitle, "x", Policy::Normal, t)), "always");
    }
    let gp = prog("Brazilian Grand Prix", Some("Verstappen wins"), &[]);
    assert_eq!(g.judge(&gp, until - 1).reason.map(|r| (r.kind, r.until)), Some((ReasonKind::Blackout, Some(until))));
    assert!(!g.judge(&gp, until).guarded(), "the blackout ended");
}

#[test]
fn series_hide_episodes_after_next_up_across_playlists() {
    let world = World::new("spoiler-series");
    // S2E3 finished on TV 2's copy, S1 watched on TV a while ago
    w(&world.store, |c| {
        for (src, s, e, fin) in [(1, 1, 9, true), (2, 2, 3, true)] {
            c.execute(
                "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
                 VALUES (?1, 'episode', ?2, 10, 3000, ?3, ?4, 'Severance', json_object('norm_name', ?5))",
                params![src, format!("m3u:ep-{s}-{e}"), fin, KICK_OFF - 86_400, format!("severance:s{s}e{e}")],
            )?;
        }
        Ok(())
    });
    let cfg = Config::default();
    let g = world.guard(&cfg, KICK_OFF);
    let ep = |s, e| Subject::Episode { series_norm: "severance", season: s, episode: e };
    assert!(!g.judge(&ep(2, 4), KICK_OFF).guarded(), "the next-up episode shows");
    assert!(!g.judge(&ep(1, 2), KICK_OFF).guarded());
    let v = g.judge(&ep(2, 5), KICK_OFF);
    assert!(v.hide_sub && v.hide_descr && v.obscure_image);
    assert_eq!(v.reason.map(|r| (r.rule, r.kind)), Some(("Severance".into(), ReasonKind::Series)));
    // guide airings: S/E after next-up hidden, without S/E the sub-title and description go
    let drama: Vec<String> = vec!["Drama".into()];
    let airing = |se| Subject::Programme {
        title: "Severance",
        sub_title: Some("Cold Harbor"),
        categories: &drama,
        season_episode: se,
        start: 0,
        is_new: false,
    };
    assert!(g.judge(&airing(Some((2, 9))), KICK_OFF).hide_sub);
    assert!(!g.judge(&airing(Some((2, 4))), KICK_OFF).guarded());
    assert!(g.judge(&airing(None), KICK_OFF).hide_descr);
    assert_eq!(g.text(&airing(Some((2, 9))), Field::Title, "Severance", Policy::Normal, KICK_OFF).text(), Some("Severance"));

    // watching S2E5 on TV (a seen airing) moves the boundary on
    let row = SeenRow {
        event_key: "severance#s02e05".into(),
        first_start: KICK_OFF - H,
        title: "Severance".into(),
        series_norm: Some("severance".into()),
        season: Some(2),
        episode: Some(5),
        seen_at: KICK_OFF,
        how: SeenHow::Live,
        agent: AgentName::default(),
    };
    w(&world.store, move |c| db::mark_seen(c, &row));
    let g = world.guard(&cfg, KICK_OFF);
    assert!(!g.judge(&ep(2, 6), KICK_OFF).guarded() && g.judge(&ep(2, 7), KICK_OFF).guarded());

    // "don't guard this series" wins over the automatic rule
    w(&world.store, |c| db::set_series_guard(c, "severance", "Severance", false, None, KICK_OFF));
    assert!(!world.guard(&cfg, KICK_OFF).judge(&ep(2, 9), KICK_OFF).guarded());
    // and the automatic rule is a setting
    w(&world.store, |c| db::set_series_guard(c, "severance", "Severance", true, None, KICK_OFF).map(|_| ()));
    assert!(world.guard(&cfg, KICK_OFF).judge(&ep(2, 9), KICK_OFF).guarded(), "the user's own row");
    let manual_off = Config { spoiler: tuner::config::SpoilerConfig { series_auto: false, ..Default::default() }, ..Default::default() };
    assert!(world.guard(&manual_off, KICK_OFF).judge(&ep(2, 9), KICK_OFF).guarded(), "explicit rows guard with the automatic rule off");
}

/// Guide airings of a guarded series without S/E in the guide (DESIGN §34.5): the slim rows
/// the grid paints carry the guide's S/E; a sub-title naming a catalogue episode or carrying an
/// "S02E04" marker places the airing; a seen repeat shows; the catalogue's air dates and the
/// guide's "new" flag place what's left; an airing nothing places is guarded (also of a series
/// the user guarded before starting it).
#[test]
fn series_airings_without_s_e_are_placed_before_they_are_guarded() {
    let world = World::new("spoiler-airings");
    let bbc2 = world.ch["BBC Two"].1;
    w(&world.store, |c| {
        c.execute(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
             VALUES (1, 'episode', 'm3u:ep-2-3', 10, 3000, 1, ?1, 'Severance', json_object('norm_name', 'severance:s2e3'))",
            [KICK_OFF - 86_400],
        )?;
        c.execute("UPDATE episode SET title = 'Severance - S02E02 - Goodbye, Mrs. Selvig' WHERE season = 2 AND episode = 2", [])?;
        c.execute("UPDATE episode SET title = 'Cold Harbor' WHERE season = 2 AND episode = 9", [])?;
        Ok(())
    });
    // (sub-title, guide S/E, new) on BBC Two, an hour apart from kick-off
    let airings = [
        ("Goodbye, Mrs. Selvig", None, false),
        ("Cold Harbor", None, false),
        ("S02E04", None, false),
        ("Woe's Hollow", None, false),
        ("Attila", Some((2, 4)), false),
        ("Sweet Vitriol", None, true),
    ];
    for (i, (sub, se, new)) in airings.into_iter().enumerate() {
        w(&world.store, move |c| {
            let start = KICK_OFF + i as i64 * H;
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title, season, episode, flags)
                 VALUES (1, ?1, 1, ?2, ?3, 'Severance', ?4, ?5, ?6, ?7)",
                params![bbc2, start, start + H, sub, se.map(|s| s.0), se.map(|s| s.1), if new { 1 } else { 0 }],
            )?;
            Ok(())
        });
    }
    let cfg = Config::default();
    // what the grid paints: slim rows, judged as the painters judge them
    let slim = |world: &World| tuner::store::epg::window(&world.store.reader().unwrap(), EpgChannelId(bbc2), KICK_OFF, KICK_OFF + 12 * H).unwrap();
    let hidden_subs = |world: &World, g: &Guard| -> Vec<String> {
        slim(world)
            .iter()
            .filter(|p| g.judge(&tuner::ui::widgets::spoiler::slim_subject(p), KICK_OFF).hide_sub)
            .filter_map(|p| p.sub_title.as_deref().map(str::to_owned))
            .collect()
    };
    assert_eq!(slim(&world)[4].episode, Some(EpisodeNum { season: Some(2), episode: Some(4) }), "slim rows carry the guide's S/E");
    let g = world.guard(&cfg, KICK_OFF);
    assert_eq!(
        hidden_subs(&world, &g),
        vec!["Cold Harbor", "Woe's Hollow", "Sweet Vitriol"],
        "S2E2 by its catalogue title, S2E4 by its marker or the guide's S/E show; S2E9, an unplaced repeat of a part-way series and a first showing don't"
    );

    // Woe's Hollow watched (Ctrl+W): a repeat of it shows; Cold Harbor would record S2E9
    let full = |sub: &str| {
        let r = world.store.reader().unwrap();
        let id = slim(&world).into_iter().find(|p| p.sub_title.as_deref() == Some(sub)).unwrap().id;
        tuner::store::epg::programme(&r, id).unwrap()
    };
    let placed = tuner::spoiler::placed_seen_row(&g.snapshot(), &full("Cold Harbor"), SeenHow::Marked, KICK_OFF);
    assert_eq!((placed.series_norm.as_deref(), placed.season, placed.episode), (Some("severance"), Some(2), Some(9)));
    let row = tuner::spoiler::placed_seen_row(&g.snapshot(), &full("Woe's Hollow"), SeenHow::Marked, KICK_OFF);
    assert_eq!((row.series_norm.as_deref(), row.episode), (Some("severance"), None), "kept as the series' seen state");
    w(&world.store, move |c| db::mark_seen(c, &row));
    let g = world.guard(&cfg, KICK_OFF);
    assert_eq!(hidden_subs(&world, &g), vec!["Cold Harbor", "Sweet Vitriol"]);
    let prune_later = KICK_OFF + 400 * 86_400;
    w(&world.store, move |c| db::prune(c, prune_later));
    assert_eq!(hidden_subs(&world, &world.guard(&cfg, KICK_OFF)), vec!["Cold Harbor", "Sweet Vitriol"], "pruning keeps it");

    // air dates: S2E4 aired in November, S2E5 on in December — tonight's airings can't be S2E5+
    w(&world.store, |c| {
        c.execute("UPDATE episode SET air_date = '2026-11-20' WHERE season = 2 AND episode = 4", [])?;
        c.execute("UPDATE episode SET air_date = '2026-12-01' WHERE season = 2 AND episode > 4", [])?;
        Ok(())
    });
    let g = world.guard(&cfg, KICK_OFF);
    assert_eq!(hidden_subs(&world, &g), vec!["Cold Harbor"], "the catalogue title still places S2E9");

    // a series guarded before it's started: the user's own guard holds — an airing nothing
    // places loses its text, a first showing too
    w(&world.store, |c| db::set_series_guard(c, "taskmaster", "Taskmaster", true, None, KICK_OFF).map(|_| ()));
    let g = world.guard(&cfg, KICK_OFF);
    let tm = |sub, is_new| Subject::Programme {
        title: "Taskmaster",
        sub_title: Some(sub),
        categories: &[],
        season_episode: None,
        start: KICK_OFF,
        is_new,
    };
    let v = g.judge(&tm("Hot Dogs", false), KICK_OFF);
    assert!(v.hide_sub && v.hide_descr, "{v:?}");
    assert!(g.judge(&tm("Hot Dogs", true), KICK_OFF).hide_sub);
}

#[test]
fn the_zap_shield_stops_on_mentions_but_not_on_the_match_itself() {
    let world = match_day("spoiler-shield");
    let cfg = Config::default();
    let g = world.guard(&cfg, KICK_OFF);
    let t = KICK_OFF + 90 * 60;
    let ssn = world.ch["Sky Sports News"].0;
    assert_eq!(g.shield(ssn, t).map(|r| r.rule), Some("Arsenal".into()), "news about the match");
    assert!(g.shield(world.ch["Sky Sports Main Event"].0, t).is_none(), "the match itself");
    assert!(g.shield(world.ch["Sky Sports Main Event 2"].0, t).is_none());
    assert!(g.shield(ssn, KICK_OFF - 1).is_none(), "nothing before kick-off");
    assert!(g.shield(world.ch["Blue Night"].0, KICK_OFF + 3 * H + 60).is_none(), "explicit channels are never looked at");
    g.pause(Some(t + 900));
    assert!(g.shield(ssn, t).is_none(), "paused");
    let no_shield = Config { spoiler: tuner::config::SpoilerConfig { shield_zap: false, ..Default::default() }, ..Default::default() };
    let g = world.guard(&no_shield, KICK_OFF);
    assert!(g.shield(ssn, t).is_none());
    // without the shield, events come from titles alone: the news sub-title isn't an event
    assert_eq!(g.topics(t)[0].event, Some(("PL: Arsenal v Chelsea".to_owned(), KICK_OFF)));
}

#[test]
fn mcp_replies_lose_guarded_text_unless_revealed() {
    let world = match_day("spoiler-mcp");
    let g = world.guard(&Config::default(), KICK_OFF);
    let t = KICK_OFF + 3 * H;
    let reply = serde_json::json!({
        "programmes": [
            {"ref": "programme:1:ssn:1", "title": "Arsenal 2-1 Chelsea: Reaction", "subtitle": "Saka double", "start": "x",
             "channel": {"ref": "channel:1:ssn", "name": "Sky Sports News"}},
            {"ref": "programme:1:bbc2:2", "title": "Taskmaster", "subtitle": "Series 20, episode 4"}
        ]
    });
    let mut normal = reply.clone();
    tuner::guard::reply::guard_json(&mut normal, &g, Policy::Normal, &|_| false, t);
    let text = serde_json::to_string(&normal).unwrap();
    assert!(!text.contains("2-1") && !text.contains("Saka"), "{text}");
    assert_eq!(normal["programmes"][0]["title"], "Arsenal v Chelsea: Reaction");
    assert_eq!(normal["programmes"][0]["spoiler"]["rule"], "Arsenal");
    assert_eq!(normal["programmes"][0]["spoiler"]["hidden"], serde_json::json!(["subtitle"]));
    assert_eq!(normal["programmes"][1], reply["programmes"][1], "unguarded items untouched");
    // reveal_spoilers for that ref only
    let mut revealed = reply.clone();
    tuner::guard::reply::guard_json(&mut revealed, &g, Policy::Normal, &|r| r == "programme:1:ssn:1", t);
    assert_eq!(revealed, reply);
    // spoiler_safe strips every sub-title, guard on or off
    let mut safe = reply.clone();
    tuner::guard::reply::guard_json(&mut safe, &Guard::default(), Policy::SpoilerSafe, &|_| false, t);
    assert!(safe["programmes"][1].get("subtitle").is_none());
    assert_eq!(safe["programmes"][1]["spoiler"]["reason"], "spoiler_safe");
    // an episode list under a series: titles after next-up go
    w(&world.store, |c| {
        c.execute(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title, fp)
             VALUES (1, 'episode', 'm3u:ep-1-2', 3000, 3000, 1, ?1, 'Severance', json_object('norm_name', 'severance:s1e2'))",
            [KICK_OFF],
        )?;
        Ok(())
    });
    let g = world.guard(&Config::default(), KICK_OFF);
    let mut details = serde_json::json!({"ref": "series:1:sev", "kind": "series", "name": "Severance", "plot": "Office workers",
        "next_episode": {"ref": "episode:1:3", "season": 1, "episode": 3, "title": "In Perpetuity"},
        "episodes": [{"ref": "episode:1:4", "season": 1, "episode": 4, "title": "The You You Are"}]});
    tuner::guard::reply::guard_json(&mut details, &g, Policy::Normal, &|_| false, t);
    assert_eq!(details["next_episode"]["title"], "In Perpetuity");
    assert_eq!(details["plot"], "Office workers");
    assert!(details["episodes"][0].get("title").is_none());
    assert_eq!(details["episodes"][0]["spoiler"]["reason"], "series");
}

#[test]
fn following_spoiler_safe_rules_are_guard_topics() {
    let world = World::new("spoiler-follow");
    world.prog("Sky Sports Main Event", KICK_OFF, KICK_OFF + 2 * H, "Spurs v Arsenal", None, None, "Sport", None);
    // not the followed team's match: the women's side (`without`), a fan channel (no fixture)
    world.prog("BBC Two", KICK_OFF - 6 * H, KICK_OFF - 4 * H, "Spurs Women v Arsenal Women", None, None, "Sport", None);
    world.prog("Sky Sports News", KICK_OFF - 4 * H, KICK_OFF - 3 * H, "Spurs Fan TV", None, None, "Sport", None);
    w(&world.store, |c| {
        c.execute_batch(
            "INSERT INTO follow_rule(name, kind, spec, spoiler_safe, priority, created_at, updated_at)
               VALUES ('Spurs', 'team', '{\"v\":1,\"terms\":[\"Spurs\",\"Tottenham\"],\"without\":[\"Women\",\"Fan TV\"]}', 1, 0, 0, 0);
             INSERT INTO follow_rule(name, kind, spec, spoiler_safe, priority, created_at, updated_at)
               VALUES ('Taskmaster', 'show', '{\"v\":1,\"terms\":[\"Taskmaster\"]}', 0, 1, 0, 0);
             INSERT INTO follow_rule(name, kind, spec, spoiler_safe, priority, created_at, updated_at)
               VALUES ('Broken', 'team', 'not json', 1, 2, 0, 0);",
        )?;
        Ok(())
    });
    let g = world.guard(&Config::default(), KICK_OFF);
    let topics = g.topics(KICK_OFF + H);
    assert_eq!(topics.iter().map(|t| (t.label.as_str(), t.rule, t.active)).collect::<Vec<_>>(), [("Spurs", None, true)]);
    assert_eq!(g.outside_title("Tottenham 3-0 Arsenal", KICK_OFF + H), "Tottenham v Arsenal");
    // narrow: only the fixture opened a window, and other sport keeps its text meanwhile
    assert!(!g.topics(KICK_OFF - H)[0].active, "the women's match and the fan channel are not the event");
    let sport: Vec<String> = vec!["Sport".into()];
    let golf = prog("Golf", Some("Round three"), &sport);
    assert!(!g.judge(&golf, KICK_OFF + H).guarded(), "unrelated sport stays readable");
}

#[test]
fn rules_and_seen_rows_survive_a_backup_round_trip() {
    let world = match_day("spoiler-backup");
    let row = SeenRow {
        event_key: "k".into(),
        first_start: 5,
        title: "T".into(),
        series_norm: None,
        season: None,
        episode: None,
        seen_at: 6,
        how: SeenHow::Agent,
        agent: AgentName::named("Codex"),
    };
    w(&world.store, move |c| db::mark_seen(c, &row));
    let v = db::export(&world.store.reader().unwrap()).unwrap().expect("something to back up");
    let other = World::new("spoiler-backup-2");
    let v2 = v.clone();
    assert_eq!(w(&other.store, move |c| db::import(c, &v2, &|r| Some(r.clone()))), 2);
    let c = other.store.reader().unwrap();
    let rules = db::list_rules(&c).unwrap();
    assert_eq!((rules[0].label.as_str(), rules[0].terms.len(), rules[0].categories.as_slice()), ("Arsenal", 2, &["Sport".to_string()][..]));
    let seen = &db::seen_recent(&c, 10).unwrap()[0];
    assert_eq!((seen.how, seen.agent.name()), (SeenHow::Agent, "Codex"), "marked seen by Codex");
    drop(c);
    // a backup from before agent names (no "agent" keys): its agent rows are Claude's
    let mut old = v.clone();
    for rows in ["seen", "rules"] {
        old[rows].as_array_mut().unwrap().iter_mut().for_each(|r| _ = r.as_object_mut().unwrap().remove("agent"));
    }
    let third = World::new("spoiler-backup-3");
    w(&third.store, move |c| db::import(c, &old, &|r| Some(r.clone())));
    let c = third.store.reader().unwrap();
    assert_eq!(db::seen_recent(&c, 10).unwrap()[0].agent.name(), "Claude");
    assert_eq!(db::list_rules(&c).unwrap()[0].agent.stored(), None, "the user's own rule stays unnamed");
    drop(c);
    assert_eq!(w(&other.store, move |c| db::import(c, &v, &|r| Some(r.clone()))), 0, "a second restore adds nothing");
    // pruning keeps recent rows
    assert_eq!(w(&other.store, |c| db::prune(c, 10)), 0);
    assert_eq!(w(&other.store, |c| db::prune(c, 400 * 86_400)), 1);
    let _ = Arc::new(());
}
