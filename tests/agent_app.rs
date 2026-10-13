//! The running Tuner's agent handler (DESIGN §30.4–§30.6) against real services in a temp
//! profile and a seeded store: every tool, and the guardrails — explicit content absent from
//! every tool and count while the session is locked (also after the relock timer with no UI
//! frame), locked items flagged and `play` asking for the PIN on screen, no address in any
//! reply, limits. The app side (`AppEvent::Agent`, switches firing) is tested with the app rig
//! in `src/app.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tuner::agent::guard::Limits;
use tuner::agent::proto::{Call, ClientInfo, ErrorCode, Reply, Request};
use tuner::agent::server::{AgentHandler, AgentService};
use tuner::agent::{AgentCommand, PlayTarget};
use tuner::config::Config;
use tuner::model::{ItemKey, ItemKind, ItemRef, MediaRef, SourceId, Timestamp};
use tuner::msg::AppEvent;
use tuner::store::library::{LoadOptions, Visibility};
use tuner::ui::testing::TestUi;

const PIN: &str = "1234";

fn pin_config() -> Config {
    let mut cfg = Config::default();
    let salt = "00".repeat(16);
    cfg.parental.enabled = true;
    cfg.parental.pin_hash = Some(tuner::ui::parental::hash_pin(PIN, &salt));
    cfg.parental.pin_salt = Some(salt);
    cfg.parental.relock_min = 1;
    cfg
}

/// A catalogue that exercises every visibility rule. Every row that can carry an address does
/// (streams, logos, posters, the playlist URL).
///
/// * "TV One" (category filter keeps GB + untagged): News (BBC One HD), Sports (ITV4 with 7 days
///   of catch-up, ITV4 +1 shifted 60 min, Locked Sports — locked), Hidden Group (Secret Channel,
///   group hidden by the user), XXX (Late Night, explicit), "FR: Sport" (Canal Sport, hidden by
///   the category filter). Movies: Comedy (Airplane! 88 min, Long Comedy 150 min, Mystery Comedy
///   without a runtime, Watched Comedy with progress), Drama (Dune), Adult Movies (Adult Film,
///   explicit), Locked Films (Locked Film, group locked). Series: Shows (The Show, 1 × 3, the
///   2nd episode half watched).
/// * "TV Two": Films (Dune — the same title: one stack with TV One's).
/// * "Extra" (not in All playlists): Extra News.
/// * Guide: ITV4 has "Touring Car Highlights" (ended 1 h ago), "British Touring Car
///   Championship" (live) and "Touring Cars Qualifying" (in 2 h); Late Night has "Touring Cars
///   After Dark" (live, explicit); BBC One has "Old News" (ended), "News at Six" (live).
/// * Favourites: ITV4, Airplane!, Late Night. Progress: Watched Comedy, The Show 1×2, Adult Film.
#[allow(clippy::type_complexity)] // the seed tables read best as tuples
fn seed(c: &mut Connection, now: Timestamp) -> tuner::error::Result<()> {
    let tx = c.transaction()?;
    let filter = r#"{"category_filter":{"keep":["GB"],"keep_untagged":true,"known":["GB","FR"]}}"#;
    for (id, name, settings) in [(1, "TV One", filter), (2, "TV Two", "{}"), (3, "Extra", r#"{"in_all":false}"#)] {
        tx.execute(
            "INSERT INTO source(id, name, kind, url, settings, sort_index, created_at, generation) VALUES (?1, ?2, 'm3u-url', ?3, ?4, ?1, 0, 1)",
            params![id, name, format!("http://127.0.0.1/{id}/playlist.m3u?username=u&password=p"), settings],
        )?;
    }
    let group = |tx: &rusqlite::Transaction, src: i64, kind: &str, name: &str, explicit: i64| -> rusqlite::Result<i64> {
        let key = format!("{kind}:{}", name.to_lowercase());
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, raw_name, provider_order, item_count, seen_gen, explicit) VALUES (?1, ?2, ?3, ?4, ?4, 0, 1, 1, ?5)",
            params![src, kind, key, name, explicit],
        )?;
        Ok(tx.last_insert_rowid())
    };
    let news = group(&tx, 1, "channel", "News", 0)?;
    let sports = group(&tx, 1, "channel", "Sports", 0)?;
    let hidden = group(&tx, 1, "channel", "Hidden Group", 0)?;
    let xxx = group(&tx, 1, "channel", "XXX", 1)?;
    let fr = group(&tx, 1, "channel", "FR: Sport", 0)?;
    let extra = group(&tx, 3, "channel", "Extra", 0)?;
    tx.execute("INSERT INTO group_override(source_id, kind, group_key, hidden) VALUES (1, 'channel', 'channel:hidden group', 1)", [])?;
    let catchup = r#"{"kind":"default","template":null,"days":7,"correction_s":0}"#;
    // (source, key, name, number, group, shift, catch-up, epg channel)
    let channels: [(i64, &str, &str, i64, i64, i64, Option<&str>, i64); 8] = [
        (1, "m3u:bbc1", "BBC One HD", 1, news, 0, None, 1),
        (1, "m3u:itv4", "ITV4", 4, sports, 0, Some(catchup), 2),
        (1, "m3u:itv4p1", "ITV4 +1", 5, sports, 60, None, 2),
        (1, "m3u:locked", "Locked Sports", 6, sports, 0, None, 3),
        (1, "m3u:secret", "Secret Channel", 7, hidden, 0, None, 2),
        (1, "m3u:late", "Late Night", 99, xxx, 0, None, 4),
        (1, "m3u:canal", "Canal Sport", 8, fr, 0, None, 2),
        (3, "m3u:extra", "Extra News", 1, extra, 0, None, 1),
    ];
    tx.execute(
        "INSERT INTO epg_source(id, name, kind, url, active_gen, n_channels) VALUES (1, 'Guide', 'url', 'http://127.0.0.1/epg.xml', 1, 4)",
        [],
    )?;
    for ec in 1..=4 {
        tx.execute(
            "INSERT INTO epg_channel(id, epg_source_id, xmltv_id, icon, seen_gen) VALUES (?1, 1, ?2, 'http://127.0.0.1/icon.png', 1)",
            params![ec, format!("ec{ec}")],
        )?;
    }
    for (i, (src, key, name, number, grp, shift, cu, ec)) in channels.iter().enumerate() {
        let url = format!("http://127.0.0.1/live/{key}.ts");
        tx.execute(
            "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, logo, tvg_shift_min, stream, url_key, catchup, provider_order, added_at, seen_gen, quality)
             VALUES (?1, ?2, ?3, ?3, lower(?3), ?4, 'http://127.0.0.1/logo.png', ?5, ?6, ?7, ?8, ?9, ?9, 1, 2)",
            params![src, key, name, number, shift, format!(r#"{{"t":"url","url":"{url}"}}"#), url, cu, i as i64],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, 0, 1)", params![grp, id])?;
        tx.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)", params![id, ec])?;
    }
    tx.execute("INSERT INTO item_override(source_id, kind, item_key, locked, fp) VALUES (1, 'channel', 'm3u:locked', 1, '{}')", [])?;
    let progs: [(i64, i64, i64, &str, &str); 9] = [
        (1, now - 3 * 3600, now - 2 * 3600, "Old News", "Yesterday's headlines at http://news.example/"),
        (1, now - 1800, now + 1800, "News at Six", "The day's news"),
        (2, now - 7200, now - 3600, "Touring Car Highlights", "Last weekend's races"),
        (2, now - 1800, now + 3600, "British Touring Car Championship", "Live from Brands Hatch"),
        (2, now + 7200, now + 9000, "Touring Cars Qualifying", "Qualifying"),
        (3, now - 600, now + 3000, "Locked Match", "A locked match"),
        (4, now - 600, now + 3000, "Touring Cars After Dark", "Explicit"),
        (2, now - 3 * 86400, now - 3 * 86400 + 1800, "Touring Car Classics", "From the archive"),
        (3, now + 7200, now + 9000, "Locked Final", "A locked final"),
    ];
    for (ec, start, stop, title, descr) in progs {
        tx.execute(
            "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, descr, categories, icon) VALUES (1, ?1, 1, ?2, ?3, ?4, ?5, 'Sport\x1fMotoring', 'http://127.0.0.1/p.jpg')",
            params![ec, start, stop, title, descr],
        )?;
        tx.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, NULL)", params![tx.last_insert_rowid(), title])?;
    }
    // movies
    let comedy = group(&tx, 1, "movie", "Comedy", 0)?;
    let drama = group(&tx, 1, "movie", "Drama", 0)?;
    let adult = group(&tx, 1, "movie", "Adult Movies", 1)?;
    let locked_films = group(&tx, 1, "movie", "Locked Films", 0)?;
    let films = group(&tx, 2, "movie", "Films", 0)?;
    tx.execute("INSERT INTO group_override(source_id, kind, group_key, locked) VALUES (1, 'movie', 'movie:locked films', 1)", [])?;
    // (source, key, name, group, runtime s, year, rating, match hash, plot)
    let movies: [(i64, &str, &str, i64, Option<i64>, i64, f64, Option<i64>, &str); 9] = [
        (1, "m3u:dune1", "Dune", drama, Some(9360), 2021, 8.0, Some(111), "Spice."),
        (2, "m3u:dune2", "Dune", films, Some(9360), 2021, 8.0, Some(111), "Spice."),
        (1, "m3u:airplane", "Airplane!", comedy, Some(5280), 1980, 7.7, None, "Surely you can't be serious."),
        (1, "m3u:long", "Long Comedy", comedy, Some(9000), 2001, 6.0, None, "Long."),
        (1, "m3u:mystery", "Mystery Comedy", comedy, None, 1999, 5.0, None, "Who knows."),
        (1, "m3u:watched", "Watched Comedy", comedy, Some(5400), 2010, 6.5, None, "Seen it."),
        (1, "m3u:adult", "Adult Film", adult, Some(5400), 2010, 5.0, None, "Explicit."),
        (1, "m3u:lockedfilm", "Locked Film", locked_films, Some(5400), 2015, 7.0, None, "A secret plot."),
        (3, "m3u:extramovie", "Extra Movie", 0, Some(5400), 2015, 7.0, None, "Out of All."),
    ];
    for (i, (src, key, name, grp, runtime, year, rating, hash, plot)) in movies.iter().enumerate() {
        tx.execute(
            "INSERT INTO movie(source_id, key, name, clean_name, norm_name, year, rating, poster, plot, duration_s, stream, provider_order, added_at, seen_gen, match_hash)
             VALUES (?1, ?2, ?3, ?3, lower(?3), ?4, ?5, 'http://127.0.0.1/poster.jpg', ?6, ?7, ?8, ?9, ?10, 1, ?11)",
            params![
                src,
                key,
                name,
                year,
                rating,
                plot,
                runtime,
                format!(r#"{{"t":"url","url":"http://127.0.0.1/movie/{key}.mp4"}}"#),
                i as i64,
                1_700_000_000 + i as i64,
                hash
            ],
        )?;
        let id = tx.last_insert_rowid();
        if *grp != 0 {
            tx.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, 0)", params![grp, id])?;
        }
    }
    let extra_movies = group(&tx, 3, "movie", "Extra Films", 0)?;
    tx.execute("INSERT INTO movie_group(group_id, movie_id, position) SELECT ?1, id, 0 FROM movie WHERE key = 'm3u:extramovie'", [extra_movies])?;
    // a series
    let shows = group(&tx, 1, "series", "Shows", 0)?;
    tx.execute(
        "INSERT INTO series(source_id, key, name, clean_name, norm_name, cover, plot, provider_order, added_at, details_at, seen_gen)
         VALUES (1, 'm3u:show', 'The Show', 'The Show', 'the show', 'http://127.0.0.1/cover.jpg', 'A show.', 0, 1, 1, 1)",
        [],
    )?;
    let show = tx.last_insert_rowid();
    tx.execute("INSERT INTO series_group(group_id, series_id, position) VALUES (?1, ?2, 0)", params![shows, show])?;
    tx.execute("INSERT INTO season(series_id, number, episode_count) VALUES (?1, 1, 3)", [show])?;
    for e in 1..=3 {
        tx.execute(
            "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, 1, ?2, 1, ?3, ?4, 2700, ?5)",
            params![show, format!("m3u:show-1-{e}"), e, format!("Episode {e}"), format!(r#"{{"t":"url","url":"http://127.0.0.1/series/{e}.mp4"}}"#)],
        )?;
    }
    for (kind, key, pos, title) in [
        ("movie", "m3u:watched", 1200.0, "Watched Comedy"),
        ("episode", "m3u:show-1-2", 900.0, "The Show"),
        ("movie", "m3u:adult", 600.0, "Adult Film"),
    ] {
        tx.execute(
            "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, finished, updated_at, title) VALUES (1, ?1, ?2, ?3, 5400, 0, ?4, ?5)",
            params![kind, key, pos, now - 60, title],
        )?;
    }
    tx.commit()?;
    // the match keys `match_titles` compares (an import computes them)
    for src in 1..=3 {
        tuner::store::titles::refresh_source(c, SourceId(src))?;
    }
    // watched on this machine: a channel, a movie, an episode and an explicit film
    for (i, (kind, key, title)) in [
        (ItemKind::Channel, "m3u:itv4", "ITV4"),
        (ItemKind::Movie, "m3u:airplane", "Airplane!"),
        (ItemKind::Episode, "m3u:show-1-2", "Episode 2"),
        (ItemKind::Movie, "m3u:adult", "Adult Film"),
    ]
    .into_iter()
    .enumerate()
    {
        let item = ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) };
        tuner::store::user::push_history(c, &tuner::model::HistoryEntry { item, at: now - 3600 + i as i64, title: title.into() })?;
    }
    let list = tuner::store::user::default_fav_list(c)?;
    for (kind, key) in [(ItemKind::Channel, "m3u:itv4"), (ItemKind::Movie, "m3u:airplane"), (ItemKind::Channel, "m3u:late")] {
        tuner::store::user::set_favourite(c, list, &ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) }, true, now)?;
    }
    Ok(())
}

struct World {
    ui: TestUi,
    handler: AgentHandler,
    now: Timestamp,
    next: u64,
    /// The MCP client the requests come from (`None`: a bridge that doesn't say).
    agent: Option<ClientInfo>,
}

impl World {
    fn new(cfg: Config) -> Self {
        Self::with_limits(cfg, Limits::new((1000.0, 1000.0), (1000.0, 1000.0)))
    }

    fn with_limits(cfg: Config, limits: Limits) -> Self {
        let ui = TestUi::with_config(cfg).unwrap();
        let now = tuner::model::now();
        ui.services().store.writer().blocking(move |c| seed(c, now)).unwrap();
        let s = ui.services();
        let handler =
            AgentHandler::with_limits(s.agent.clone(), s.store.clone(), s.config.clone(), s.tiles.focused(), s.bus.clone(), s.jobs.clone(), limits);
        Self { ui, handler, now, next: 1, agent: None }
    }

    /// Publish a snapshot built with `visibility`, like the app does on every frame.
    fn publish(&self, visibility: Visibility) {
        let s = self.ui.services();
        let lib = tuner::store::library::load_with(&s.store.reader().unwrap(), &LoadOptions { visibility, ..Default::default() }).unwrap();
        let _ = s.agent.publish_library(&Arc::new(lib));
    }

    fn unlock(&self, at: Option<Instant>) {
        self.ui.services().agent.set_unlocked_at(at);
    }

    fn reply(&mut self, method: &str, params: Value) -> Reply {
        let call = Call::from_tool(method, params).expect("valid arguments");
        self.next += 1;
        let reply = self.ui.services().rt.block_on(self.handler.handle(Request::new(self.next, call).with_agent(self.agent.clone())));
        let wire = serde_json::to_string(&reply).unwrap();
        assert!(!wire.contains("://"), "{method}: an address on the wire: {wire}");
        assert!(!wire.contains("http-header") && !wire.contains("password"), "{method}: {wire}");
        reply
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.reply(method, params.clone());
        assert!(r.ok, "{method} {params}: {:?}", r.error);
        r.result.unwrap()
    }

    fn err(&mut self, method: &str, params: Value) -> ErrorCode {
        let r = self.reply(method, params.clone());
        assert!(!r.ok, "{method} {params} should fail: {:?}", r.result);
        r.error.unwrap().code
    }

    /// Agent commands the handler sent to the app since the last call.
    fn commands(&self) -> Vec<AgentCommand> {
        self.ui
            .events
            .try_iter()
            .filter_map(|e| match e {
                AppEvent::Agent(c) => Some(c),
                _ => None,
            })
            .collect()
    }
}

fn names(v: &Value, list: &str, field: &str) -> Vec<String> {
    v[list].as_array().unwrap().iter().map(|x| x[field].as_str().unwrap_or_default().to_owned()).collect()
}

/// Every text of a reply, for "is X mentioned anywhere" checks.
fn mentions(v: &Value, what: &str) -> bool {
    v.to_string().to_lowercase().contains(&what.to_lowercase())
}

#[test]
fn before_the_first_snapshot_queries_say_loading_and_status_works() {
    let mut w = World::new(pin_config());
    assert_eq!(w.err("search", json!({"query": "itv"})), ErrorCode::Loading);
    let st = w.ok("status", json!({}));
    assert_eq!((st["state"].as_str(), st["parental_locked"].as_bool()), (Some("idle"), Some(true)));
}

/// "Touring cars is on": the live race first, on the visible channel with its catch-up; its
/// copies on a hidden channel, a category-filtered one and an explicit one never show (not even
/// in `also_on`); the shifted channel's airing is its own; past and upcoming follow.
#[test]
fn guide_search_and_whats_on() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok("guide_search", json!({"query": "touring car"}));
    let progs = r["programmes"].as_array().unwrap();
    let first = &progs[0];
    assert_eq!(first["title"], "British Touring Car Championship");
    assert_eq!((first["airing"].as_str(), first["catchup"].as_bool()), (Some("live"), Some(true)));
    assert_eq!(first["channel"]["name"], "ITV4");
    assert_eq!(first["channel"]["catchup_days"], 7);
    assert_eq!(first["channel"]["quality"], "HD");
    assert!(first["channel"]["favourite"].as_bool().unwrap());
    assert!(first.get("also_on").is_none(), "hidden, filtered and explicit channels are no alternative: {first}");
    let airing: Vec<(&str, &str, &str)> =
        progs.iter().map(|p| (p["title"].as_str().unwrap(), p["channel"]["name"].as_str().unwrap(), p["airing"].as_str().unwrap())).collect();
    assert!(airing.contains(&("British Touring Car Championship", "ITV4 +1", "upcoming")), "{airing:?}");
    assert!(airing.contains(&("Touring Cars Qualifying", "ITV4", "upcoming")));
    assert!(airing.contains(&("Touring Car Highlights", "ITV4", "past")));
    let order: Vec<&str> = airing.iter().map(|a| a.2).collect();
    let mut sorted = order.clone();
    sorted.sort_by_key(|a| ["live", "upcoming", "past"].iter().position(|x| x == a));
    assert_eq!(order, sorted, "live, upcoming, past");
    for hidden in ["After Dark", "Secret Channel", "Canal Sport", "Late Night"] {
        assert!(!mentions(&r, hidden), "{hidden} in {r}");
    }
    assert!(!r["truncated"].as_bool().unwrap());
    // time window and past
    let r = w.ok("guide_search", json!({"query": "touring car", "include_past": false, "limit": 1}));
    assert_eq!(names(&r, "programmes", "title"), ["British Touring Car Championship"]);
    assert!(r["truncated"].as_bool().unwrap());
    let r = w.ok("guide_search", json!({"query": "touring", "from": (w.now + 3600).to_string()}));
    assert!(names(&r, "programmes", "title").iter().all(|t| t != "Touring Car Highlights"));
    assert_eq!(w.err("guide_search", json!({"query": "x", "from": "tonight"})), ErrorCode::BadRequest);

    // what's on ITV4 now / later; a hidden channel isn't a channel
    let r = w.ok("whats_on", json!({"channel": "itv4"}));
    assert_eq!(names(&r, "programmes", "title")[0], "British Touring Car Championship");
    let later = w.ok("whats_on", json!({"channel": "4", "at": (w.now + 7300).to_string()}));
    assert_eq!(names(&later, "programmes", "title"), ["Touring Cars Qualifying"]);
    assert_eq!(w.err("whats_on", json!({"channel": "Secret"})), ErrorCode::NotFound);
    assert_eq!(w.err("whats_on", json!({"channel": "Late Night"})), ErrorCode::NotFound);
    let all = w.ok("whats_on", json!({}));
    let chans = all["programmes"].as_array().unwrap().iter().map(|p| p["channel"]["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(chans[0], "ITV4", "favourites first: {chans:?}");
    assert!(!chans.iter().any(|c| c == "Late Night" || c == "Secret Channel" || c == "Canal Sport"), "{chans:?}");
    assert!(chans.contains(&"Locked Sports".to_owned()), "locked channels are listed (flagged)");
    let locked = all["programmes"].as_array().unwrap().iter().find(|p| p["channel"]["name"] == "Locked Sports").unwrap();
    assert_eq!(locked["channel"]["locked"], true);
}

/// Search, categories, browse (mood picks), continue watching, favourites, details.
#[test]
fn catalogue_tools() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    // one row per title: the two Dune copies are one result with one other copy
    let r = w.ok("search", json!({"query": "dune"}));
    let dunes: Vec<&Value> = r["results"].as_array().unwrap().iter().filter(|x| x["name"] == "Dune").collect();
    assert_eq!(dunes.len(), 1, "{r}");
    assert_eq!((dunes[0]["kind"].as_str(), dunes[0]["other_copies"].as_u64()), (Some("movie"), Some(1)));
    let r = w.ok("search", json!({"query": "itv", "kinds": ["channel"]}));
    assert_eq!(r["results"][0]["kind"], "channel");
    assert_eq!(r["results"][0]["now"], "British Touring Car Championship");
    for q in ["secret channel", "canal sport", "late night", "adult film"] {
        let r = w.ok("search", json!({"query": q}));
        assert!(!mentions(&r, q), "{q}: {r}");
    }

    // categories: visible, non-empty, All playlists (Extra is out of All), counts after filtering
    let r = w.ok("categories", json!({"kind": "channel"}));
    let cats = names(&r, "categories", "name");
    assert!(
        cats.contains(&"Sports".to_owned()) && !cats.iter().any(|c| c == "XXX" || c == "Hidden Group" || c == "FR: Sport" || c == "Extra"),
        "{cats:?}"
    );
    let sports = r["categories"].as_array().unwrap().iter().find(|c| c["name"] == "Sports").unwrap();
    assert_eq!(sports["count"], 3);
    let r = w.ok("categories", json!({"kind": "movie"}));
    let cats = names(&r, "categories", "name");
    assert!(!cats.iter().any(|c| c == "Adult Movies"), "{cats:?}");
    let locked = r["categories"].as_array().unwrap().iter().find(|c| c["name"] == "Locked Films").unwrap();
    assert_eq!(locked["locked"], true);
    let r = w.ok("categories", json!({"kind": "movie", "playlist": "extra"}));
    assert_eq!(names(&r, "categories", "name"), ["Extra Films"]);
    assert_eq!(w.err("categories", json!({"kind": "movie", "playlist": "nope"})), ErrorCode::NotFound);

    // "something light under 2 hours": comedies ≤ 120 min, unknown runtimes after known ones
    let r = w.ok("browse", json!({"kind": "movie", "group": "comedy", "max_minutes": 120, "sort": "name"}));
    assert_eq!(names(&r, "items", "name"), ["Airplane!", "Watched Comedy", "Mystery Comedy"]);
    assert_eq!(r["total"], 3);
    assert_eq!(r["items"][0]["runtime_min"], 88);
    let r = w.ok("browse", json!({"kind": "movie", "group": "comedy", "max_minutes": 120, "unwatched": true, "sort": "random"}));
    let mut got = names(&r, "items", "name");
    got.sort();
    assert_eq!(got, ["Airplane!", "Mystery Comedy"]);
    let r = w.ok("browse", json!({"kind": "movie", "min_rating": 7.5, "sort": "rating"}));
    assert_eq!(names(&r, "items", "name"), ["Dune", "Airplane!"], "one Dune, no Extra Movie (out of All), no explicit title");
    let r = w.ok("browse", json!({"kind": "movie", "limit": 2, "offset": 1, "sort": "name"}));
    assert_eq!(r["total"], 6, "Airplane!, Dune (one stack), Locked Film, Long Comedy, Mystery Comedy, Watched Comedy: {r}");
    assert_eq!(names(&r, "items", "name"), ["Dune", "Locked Film"]);
    assert_eq!(w.err("browse", json!({"kind": "movie", "group": "adult"})), ErrorCode::NotFound);
    let r = w.ok("browse", json!({"kind": "channel", "group": "sports", "sort": "name"}));
    assert_eq!(names(&r, "items", "name"), ["ITV4", "ITV4 +1", "Locked Sports"]);

    // continue watching: the movie, the series with its next episode; never the explicit one
    let r = w.ok("continue_watching", json!({}));
    let items = names(&r, "items", "name");
    assert!(items.contains(&"Watched Comedy".to_owned()) && items.contains(&"The Show".to_owned()), "{items:?}");
    assert!(!mentions(&r, "adult"));
    let show = r["items"].as_array().unwrap().iter().find(|x| x["name"] == "The Show").unwrap();
    assert_eq!(show["next_episode"]["season"], 1);
    assert_eq!(show["progress"]["position_min"], 15);

    // favourites: ITV4 and Airplane!, never Late Night
    let r = w.ok("favourites", json!({}));
    let list = &r["lists"][0];
    assert_eq!(list["name"], "Favourites");
    assert_eq!(names(list, "items", "name"), ["ITV4", "Airplane!"]);
    let r = w.ok("favourites", json!({"kind": "movie"}));
    assert_eq!(names(&r["lists"][0], "items", "name"), ["Airplane!"]);

    // details
    let airplane = w.ok("search", json!({"query": "airplane", "kinds": ["movie"]}))["results"][0]["ref"].as_str().unwrap().to_owned();
    let d = w.ok("details", json!({"ref": airplane}));
    assert_eq!((d["plot"].as_str(), d["runtime_min"].as_u64(), d["year"].as_u64()), (Some("Surely you can't be serious."), Some(88), Some(1980)));
    assert!(d.get("details_pending").is_none());
    let show = w.ok("search", json!({"query": "the show", "kinds": ["series"]}))["results"][0]["ref"].as_str().unwrap().to_owned();
    let d = w.ok("details", json!({"ref": show}));
    assert_eq!(d["seasons"], json!([{"number": 1, "episodes": 3}]));
    assert!(d["next_episode"]["ref"].as_str().unwrap().starts_with("episode:1:"));
    let ep = d["next_episode"]["ref"].as_str().unwrap().to_owned();
    let d = w.ok("details", json!({"ref": ep}));
    assert_eq!(d["kind"], "episode");
    let live = w.ok("guide_search", json!({"query": "british touring"}))["programmes"][0]["ref"].as_str().unwrap().to_owned();
    let d = w.ok("details", json!({"ref": live}));
    assert_eq!((d["kind"].as_str(), d["description"].as_str()), (Some("programme"), Some("Live from Brands Hatch")));
    assert_eq!(d["categories"], json!(["Sport", "Motoring"]));
    let d = w.ok("details", json!({"ref": "channel:1:m3u:itv4"}));
    assert_eq!(d["schedule"][0]["title"], "British Touring Car Championship");
    assert_eq!(d["schedule"][1]["title"], "Touring Cars Qualifying");
    // a web address in a description is dropped, not passed on
    let old = w.ok("guide_search", json!({"query": "old news"}))["programmes"][0]["ref"].as_str().unwrap().to_owned();
    assert_eq!(w.ok("details", json!({"ref": old}))["description"], "Yesterday's headlines at [link]");
    assert_eq!(w.err("details", json!({"ref": "movie:1:m3u:nothing"})), ErrorCode::NotFound);
    assert_eq!(w.err("details", json!({"ref": "not a ref"})), ErrorCode::BadRequest);
}

/// DESIGN §30.6: explicit content is absent from every tool and every count while the session
/// is locked; present once unlocked; absent again after the relock timer — with the snapshot
/// built while unlocked still published (no UI frame ran).
#[test]
fn explicit_content_is_absent_while_locked_and_after_the_relock_without_a_frame() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let probe = |w: &mut World| -> (Value, Value, Value, Value, Value, Value, Value) {
        (
            w.ok("guide_search", json!({"query": "touring cars"})),
            w.ok("search", json!({"query": "late night"})),
            w.ok("categories", json!({"kind": "channel"})),
            w.ok("browse", json!({"kind": "movie"})),
            w.ok("continue_watching", json!({})),
            w.ok("favourites", json!({})),
            w.ok("whats_on", json!({})),
        )
    };
    let locked = probe(&mut w);
    let gone = |p: &(Value, Value, Value, Value, Value, Value, Value)| {
        for v in [&p.0, &p.1, &p.2, &p.3, &p.4, &p.5, &p.6] {
            for x in ["After Dark", "Late Night", "Adult Film", "Adult Movies", "XXX"] {
                assert!(!mentions(v, x), "{x} in {v}");
            }
        }
    };
    gone(&locked);
    assert_eq!(w.err("play", json!({"ref": "channel:1:m3u:late"})), ErrorCode::NotFound);
    assert_eq!(w.err("details", json!({"ref": "movie:1:m3u:adult"})), ErrorCode::NotFound);
    assert_eq!(w.err("schedule_switch", json!({"ref": "channel:1:m3u:late", "at": (w.now + 600).to_string()})), ErrorCode::NotFound);

    // unlocked (the PIN typed in Tuner), the app rebuilt with explicit content
    w.unlock(Some(Instant::now()));
    w.publish(Visibility::ALL);
    let open = probe(&mut w);
    assert!(mentions(&open.0, "After Dark") && mentions(&open.1, "Late Night") && mentions(&open.3, "Adult Film"));
    assert!(mentions(&open.5, "Late Night"));
    assert!(open.3["total"].as_u64() > locked.3["total"].as_u64());
    assert!(w.ok("details", json!({"ref": "movie:1:m3u:adult"}))["plot"].is_string());

    // two minutes later with a one-minute relock: locked again, the UI never ran
    w.unlock(Instant::now().checked_sub(Duration::from_secs(120)));
    let relocked = probe(&mut w);
    gone(&relocked);
    assert_eq!(relocked.2, locked.2, "category counts as before");
    assert_eq!(relocked.3["total"], locked.3["total"], "browse total as before");
    assert_eq!(w.err("details", json!({"ref": "movie:1:m3u:adult"})), ErrorCode::NotFound);
    assert_eq!(w.err("play", json!({"ref": "channel:1:m3u:late"})), ErrorCode::NotFound);
    assert!(w.commands().is_empty());
    let st = w.ok("status", json!({}));
    assert_eq!(st["parental_locked"], true);
}

/// Locked ≠ hidden: flagged, details withheld while locked, and `play` sends the request (the
/// app asks for the PIN on its screen) with `pin_required`. No tool takes a PIN.
#[test]
fn locked_items_are_flagged_and_play_asks_for_the_pin_on_screen() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let d = w.ok("details", json!({"ref": "channel:1:m3u:locked"}));
    assert_eq!(d, json!({"ref": "channel:1:m3u:locked", "kind": "channel", "name": "Locked Sports", "locked": true}));
    let film = w.ok("search", json!({"query": "locked film"}))["results"][0].clone();
    assert_eq!(film["locked"], true);
    let d = w.ok("details", json!({"ref": film["ref"]}));
    assert!(d["locked"].as_bool().unwrap() && d.get("plot").is_none(), "{d}");
    let p = w.ok("play", json!({"ref": "channel:1:m3u:locked"}));
    assert_eq!(p["outcome"], "pin_required");
    assert!(p["message"].as_str().unwrap().contains("PIN"));
    let sent = w.commands();
    assert!(matches!(&sent[..], [AgentCommand::Play(tuner::agent::AgentPlay { target: PlayTarget::Media(MediaRef::Channel(_)), .. })]), "{sent:?}");
    let p = w.ok("play", json!({"ref": film["ref"]}));
    assert_eq!(p["outcome"], "pin_required");
    assert!(matches!(&w.commands()[..], [AgentCommand::Play(tuner::agent::AgentPlay { target: PlayTarget::Media(MediaRef::Movie(_)), .. })]));
    // unlocked: plain play, and the details show
    w.unlock(Some(Instant::now()));
    assert_eq!(w.ok("play", json!({"ref": "channel:1:m3u:locked"}))["outcome"], "playing");
    assert!(w.ok("details", json!({"ref": film["ref"]}))["plot"].is_string());
    // the PIN is never a parameter
    assert!(Call::from_tool("play", json!({"ref": "channel:1:m3u:locked", "pin": PIN})).is_err());
}

/// `play` of programmes: live → the channel (or the archive from the start); past → the
/// archive when the channel keeps it, else `unavailable`; upcoming → `not_started`.
#[test]
fn play_resolves_programmes_movies_and_series() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let find = |w: &mut World, q: &str, airing: &str, channel: &str| -> String {
        let r = w.ok("guide_search", json!({"query": q, "limit": 50}));
        r["programmes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["airing"] == airing && p["channel"]["name"] == channel)
            .unwrap_or_else(|| panic!("{q} {airing} on {channel}: {r}"))["ref"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let live = find(&mut w, "british touring", "live", "ITV4");
    let p = w.ok("play", json!({"ref": live}));
    assert_eq!((p["outcome"].as_str(), p["ref"].as_str()), (Some("playing"), Some("channel:1:m3u:itv4")));
    assert!(matches!(&w.commands()[..], [AgentCommand::Play(tuner::agent::AgentPlay { target: PlayTarget::Media(MediaRef::Channel(_)), .. })]));
    let p = w.ok("play", json!({"ref": live, "from_start": true}));
    assert_eq!(p["outcome"], "catch_up");
    let sent = w.commands();
    let AgentCommand::Play(play) = &sent[0] else { panic!("{sent:?}") };
    let PlayTarget::Media(MediaRef::Catchup { start, stop, .. }) = &play.target else { panic!("{sent:?}") };
    assert_eq!((*start, *stop), (w.now - 1800, w.now + 3600));
    let past = find(&mut w, "highlights", "past", "ITV4");
    assert_eq!(w.ok("play", json!({"ref": past}))["outcome"], "catch_up");
    let old = find(&mut w, "classics", "past", "ITV4");
    assert_eq!(w.ok("play", json!({"ref": old}))["outcome"], "catch_up", "3 days ago, inside 7 days");
    let no_archive = find(&mut w, "highlights", "past", "ITV4 +1");
    assert_eq!(w.err("play", json!({"ref": no_archive})), ErrorCode::Unavailable);
    let soon = find(&mut w, "qualifying", "upcoming", "ITV4");
    assert_eq!(w.err("play", json!({"ref": soon})), ErrorCode::NotStarted);
    let shifted = find(&mut w, "british touring", "upcoming", "ITV4 +1");
    assert_eq!(w.err("play", json!({"ref": shifted})), ErrorCode::NotStarted, "on +1 it starts in 30 min");
    w.commands();

    // a movie resumes, from_start doesn't; a series plays its next episode
    let watched = w.ok("search", json!({"query": "watched comedy"}))["results"][0]["ref"].as_str().unwrap().to_owned();
    let p = w.ok("play", json!({"ref": watched}));
    assert_eq!((p["outcome"].as_str(), p["resume_at_min"].as_u64()), (Some("playing"), Some(20)));
    let p = w.ok("play", json!({"ref": watched, "from_start": true}));
    assert!(p.get("resume_at_min").is_none());
    let sent = w.commands();
    assert!(matches!(&sent[..], [AgentCommand::Play(a), AgentCommand::Play(b)] if !a.from_start && b.from_start), "{sent:?}");
    let p = w.ok("play", json!({"ref": "series:1:m3u:show"}));
    assert!(p["ref"].as_str().unwrap().starts_with("episode:1:m3u:show-1-"), "{p}");
    assert!(p["title"].as_str().unwrap().starts_with("The Show — S01E"));
    assert!(matches!(&w.commands()[..], [AgentCommand::Play(tuner::agent::AgentPlay { target: PlayTarget::Series(_), .. })]));
    assert_eq!(w.err("play", json!({"ref": "channel:1:m3u:secret"})), ErrorCode::NotFound, "a hidden channel");
    assert_eq!(w.err("play", json!({"ref": "channel:1:m3u:canal"})), ErrorCode::NotFound, "a category-filtered channel");
    assert_eq!(w.ok("play", json!({"ref": "1:m3u:itv4"}))["outcome"], "playing", "the --play form is a channel");
}

/// `schedule_switch` writes a switch reminder (`lead_s` −1), `status` lists it, `cancel_switch`
/// removes it; an existing notification reminder of the programme becomes the switch; started
/// programmes and other refs are refused.
#[test]
fn switches_are_scheduled_listed_and_cancelled() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok("guide_search", json!({"query": "qualifying"}));
    let soon = r["programmes"].as_array().unwrap().iter().find(|p| p["channel"]["name"] == "ITV4").unwrap()["ref"].as_str().unwrap().to_owned();
    // a notification reminder already set in the guide
    let start = w.now + 7200;
    w.ui.services()
        .store
        .writer()
        .blocking(move |c| {
            c.execute(
                "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s, state) VALUES (1, 'm3u:itv4', 'ITV4', 'Touring Cars Qualifying', ?1, ?2, 120, 'pending')",
                params![start, start + 1800],
            )?;
            Ok(())
        })
        .unwrap();
    let s = w.ok("schedule_switch", json!({"ref": soon}));
    assert_eq!((s["channel"]["name"].as_str(), s["title"].as_str()), (Some("ITV4"), Some("Touring Cars Qualifying")));
    assert_eq!(s["start"].as_str().unwrap(), tuner::agent::proto::fmt_time(start));
    assert_eq!(s["warn_at"].as_str().unwrap(), tuner::agent::proto::fmt_time(start - 10));
    let id = s["id"].as_i64().unwrap();
    let rows = |w: &World| -> Vec<(i64, i64, String)> {
        let conn = w.ui.services().store.reader().unwrap();
        let mut st = conn.prepare("SELECT id, lead_s, state FROM reminder ORDER BY id").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(rows(&w), [(id, -1, "pending".to_owned())], "the notification became the switch");
    assert_eq!(w.ok("schedule_switch", json!({"ref": soon}))["id"].as_i64(), Some(id), "idempotent");
    // a channel at a time
    let at = w.now + 3 * 3600;
    let s2 = w.ok("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": at.to_string()}));
    assert!(s2.get("title").is_none());
    let st = w.ok("status", json!({}));
    let ids: Vec<i64> = st["scheduled"].as_array().unwrap().iter().map(|x| x["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [id, s2["id"].as_i64().unwrap()]);
    assert_eq!(w.ok("cancel_switch", json!({"id": id})), json!({"cancelled": true}));
    assert_eq!(rows(&w).len(), 1);
    assert_eq!(w.err("cancel_switch", json!({"id": id})), ErrorCode::NotFound);
    // refused
    let live = w.ok("guide_search", json!({"query": "british touring"}))["programmes"][0]["ref"].as_str().unwrap().to_owned();
    assert_eq!(w.err("schedule_switch", json!({"ref": live})), ErrorCode::Unavailable, "already on: play it");
    assert_eq!(w.err("schedule_switch", json!({"ref": "channel:1:m3u:bbc1"})), ErrorCode::BadRequest, "a channel needs at");
    assert_eq!(w.err("schedule_switch", json!({"ref": "movie:1:m3u:airplane"})), ErrorCode::BadRequest);
    assert_eq!(w.err("schedule_switch", json!({"ref": soon, "at": "21:00"})), ErrorCode::BadRequest);
    // a plain reminder isn't the agent's to cancel
    let plain = w
        .ui
        .services()
        .store
        .writer()
        .blocking(move |c| {
            c.execute(
                "INSERT INTO reminder(source_id, channel_key, channel_name, title, start, stop, lead_s) VALUES (1, 'm3u:bbc1', 'BBC One HD', 'Weather', ?1, ?2, 120)",
                params![at + 7200, at + 9000],
            )?;
            Ok(c.last_insert_rowid())
        })
        .unwrap();
    assert_eq!(w.err("cancel_switch", json!({"id": plain})), ErrorCode::NotFound);
}

#[test]
fn control_reaches_the_player_and_refuses_nonsense() {
    let mut w = World::new(Config::default());
    w.publish(Visibility::NO_EXPLICIT);
    assert_eq!(w.err("control", json!({"action": "pause"})), ErrorCode::Unavailable, "nothing plays");
    assert_eq!(w.err("control", json!({"action": "volume", "value": 500})), ErrorCode::BadRequest);
    assert_eq!(w.err("control", json!({"action": "volume"})), ErrorCode::BadRequest);
    assert_eq!(w.err("control", json!({"action": "stop", "value": 3})), ErrorCode::BadRequest);
    let st = w.ok("control", json!({"action": "volume", "value": 40}));
    assert_eq!(st["state"], "idle");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let st = w.ok("status", json!({}));
        if st["volume"].as_f64() == Some(40.0) {
            break;
        }
        assert!(Instant::now() < deadline, "volume never reached the player: {st}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = w.ok("control", json!({"action": "mute"}));
    let _ = st;
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.ok("status", json!({}))["muted"] != true {
        assert!(Instant::now() < deadline, "never muted");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(w.ok("status", json!({}))["parental_locked"], false, "no PIN set");
}

#[test]
fn requests_are_rate_limited() {
    let mut w = World::with_limits(pin_config(), Limits::new((1.0, 3.0), (0.1, 1.0)));
    w.publish(Visibility::NO_EXPLICIT);
    for _ in 0..3 {
        w.ok("categories", json!({"kind": "movie"}));
    }
    assert_eq!(w.err("categories", json!({"kind": "movie"})), ErrorCode::RateLimited);
    assert_eq!(w.ok("play", json!({"ref": "channel:1:m3u:itv4"}))["outcome"], "playing");
    assert_eq!(w.err("play", json!({"ref": "channel:1:m3u:itv4"})), ErrorCode::RateLimited, "actions have a tighter budget");
}

/// `status` while explicit content is hidden names nothing explicit that still plays — an
/// explicit channel, movie or an episode of an explicit series (the snapshot's explicit index
/// has no episodes) — also when the snapshot was built unlocked and the session relocked by
/// its timer with no frame since; a normal episode is named.
#[test]
fn status_names_nothing_explicit_that_still_plays() {
    use tuner::agent::query::Q;
    use tuner::model::{Playable, PlayableKind};
    use tuner::player::{PlayerSnapshot, PlayerState};
    use tuner::shared::ParentalView;
    let w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(|c| {
            c.execute("INSERT INTO grp(source_id, kind, key, name, raw_name, provider_order, item_count, seen_gen, explicit) VALUES (1, 'series', 'series:adult shows', 'Adult Shows', 'Adult Shows', 0, 1, 1, 1)", [])?;
            let g = c.last_insert_rowid();
            c.execute(
                "INSERT INTO series(source_id, key, name, clean_name, norm_name, provider_order, added_at, details_at, seen_gen) VALUES (1, 'm3u:adultshow', 'Adult Show', 'Adult Show', 'adult show', 1, 1, 1, 1)",
                [],
            )?;
            let s = c.last_insert_rowid();
            c.execute("INSERT INTO series_group(group_id, series_id, position) VALUES (?1, ?2, 0)", params![g, s])?;
            c.execute(
                "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, 1, 'm3u:adultshow-1-1', 1, 1, 'Night One', 2700, '{\"t\":\"url\",\"url\":\"http://127.0.0.1/a.mp4\"}')",
                [s],
            )?;
            Ok(())
        })
        .unwrap();
    let conn = w.ui.services().store.reader().unwrap();
    let unlocked_build = tuner::store::library::load_with(&conn, &LoadOptions { visibility: Visibility::ALL, ..Default::default() }).unwrap();
    assert!(unlocked_build.shows_explicit());
    let cfg = pin_config();
    let item = |kind, key: &str| ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) };
    let playing = |kind: PlayableKind, item: ItemRef, title: &str, subtitle: Option<&str>| PlayerSnapshot {
        state: PlayerState::Playing,
        playable: Some(Arc::new(Playable {
            media: MediaRef::Channel(tuner::model::ChannelId(1)),
            kind,
            item: Some(item),
            source_id: Some(SourceId(1)),
            title: title.into(),
            subtitle: subtitle.map(str::to_owned),
            logo: None,
            number: None,
            url: tuner::redact::Secret::new("http://127.0.0.1/x".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        })),
        ..Default::default()
    };
    let episode = || PlayableKind::Episode { season: 1, episode: 1 };
    let cases = [
        (playing(PlayableKind::Live, item(ItemKind::Channel, "m3u:late"), "Late Night", None), "Late Night"),
        (playing(PlayableKind::Movie, item(ItemKind::Movie, "m3u:adult"), "Adult Film", None), "Adult Film"),
        (playing(episode(), item(ItemKind::Episode, "m3u:adultshow-1-1"), "Adult Show", Some("S01E01 · Night One")), "Night One"),
    ];
    // relocked (hiding) with the unlocked build still published, and a hiding build
    let hiding = ParentalView { unlocked: false, hides_explicit: true, pin_active: true };
    let hidden_build = unlocked_build.hiding_explicit();
    for lib in [&unlocked_build, &hidden_build] {
        for (snap, name) in &cases {
            let q = Q::new(lib, &conn, hiding, w.now, &cfg);
            let st = serde_json::to_value(tuner::agent::act::status(&q, snap).unwrap()).unwrap();
            assert!(st["now_playing"].is_null(), "{name}: {st}");
            assert!(!mentions(&st, name) && !mentions(&st, "Adult"), "{st}");
        }
    }
    // unlocked: named; a normal episode is named while locked too
    let open = ParentalView { unlocked: true, hides_explicit: false, pin_active: true };
    let q = Q::new(&unlocked_build, &conn, open, w.now, &cfg);
    let st = serde_json::to_value(tuner::agent::act::status(&q, &cases[2].0).unwrap()).unwrap();
    assert_eq!(st["now_playing"]["title"], "Adult Show");
    let q = Q::new(&hidden_build, &conn, hiding, w.now, &cfg);
    let show = playing(episode(), item(ItemKind::Episode, "m3u:show-1-2"), "The Show", Some("S01E02 · Episode 2"));
    let st = serde_json::to_value(tuner::agent::act::status(&q, &show).unwrap()).unwrap();
    assert_eq!(st["now_playing"]["title"], "The Show");
}

/// A switch to an explicit channel scheduled while unlocked vanishes from `status` after the
/// relock (no frame needed), can't be cancelled by ref-guessing, and the switch limit's
/// message names no count; `at` too far ahead is refused, a passed `HH:MM` means tomorrow.
#[test]
fn explicit_switches_vanish_on_relock_and_at_is_bounded() {
    let mut w = World::new(pin_config());
    w.unlock(Some(Instant::now()));
    w.publish(Visibility::ALL);
    let s = w.ok("schedule_switch", json!({"ref": "channel:1:m3u:late", "at": (w.now + 600).to_string()}));
    let id = s["id"].as_i64().unwrap();
    assert_eq!(w.ok("status", json!({}))["scheduled"].as_array().unwrap().len(), 1);
    w.unlock(Instant::now().checked_sub(Duration::from_secs(120)));
    let st = w.ok("status", json!({}));
    assert!(st["scheduled"].as_array().is_none_or(Vec::is_empty) && !mentions(&st, "Late Night"), "{st}");
    assert_eq!(w.err("cancel_switch", json!({"id": id})), ErrorCode::NotFound);
    // fill up to the limit with visible switches: the refusal names no number
    for i in 1..20 {
        w.ok("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": (w.now + 3600 + i * 60).to_string()}));
    }
    let r = w.reply("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": (w.now + 9000).to_string()}));
    let e = r.error.unwrap_or_else(|| panic!("over the limit: {:?}", r.result));
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(!e.message.chars().any(|c| c.is_ascii_digit()), "{}", e.message);
    // bounds of at (a fresh world: below the limit)
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    assert_eq!(w.err("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": (w.now + 15 * 86_400).to_string()})), ErrorCode::BadRequest);
    assert_eq!(w.err("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": i64::MAX.to_string()})), ErrorCode::BadRequest);
    // a clock time that has passed today: tomorrow
    let past = jiff::Timestamp::from_second(w.now - 120).unwrap().to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string();
    let s = w.ok("schedule_switch", json!({"ref": "channel:1:m3u:bbc1", "at": past}));
    let start = s["start"].as_str().unwrap();
    assert!(start > tuner::agent::proto::fmt_time(w.now + 82_000).as_str(), "{start}");
}

/// A provider name holding a web address shows `[link]`; it never turns the answer into
/// `internal`.
#[test]
fn names_with_links_are_cleaned_not_fatal() {
    let mut w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(|c| {
            let news = c.query_row("SELECT id FROM grp WHERE key = 'channel:news'", [], |r| r.get::<_, i64>(0))?;
            c.execute(
                "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, stream, url_key, provider_order, seen_gen)
                 VALUES (1, 'm3u:promo', 'Promo https://x.y/z', 'Promo https://x.y/z', 'promo', 50, '{\"t\":\"url\",\"url\":\"http://127.0.0.1/p.ts\"}', 'http://127.0.0.1/p.ts', 50, 1)",
                [],
            )?;
            c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, 9, 1)", params![news, c.last_insert_rowid()])?;
            c.execute(
                "INSERT INTO movie(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen) VALUES (1, 'm3u:linkfilm', 'See http://a.b/c now', 'See http://a.b/c now', 'see now', '{\"t\":\"url\",\"url\":\"http://127.0.0.1/f.mp4\"}', 99, 1)",
                [],
            )?;
            let comedy = c.query_row("SELECT id FROM grp WHERE key = 'movie:comedy'", [], |r| r.get::<_, i64>(0))?;
            c.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, 9)", params![comedy, c.last_insert_rowid()])?;
            Ok(())
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok("search", json!({"query": "promo"}));
    assert_eq!(r["results"][0]["name"], "Promo [link]", "{r}");
    let r = w.ok("browse", json!({"kind": "movie", "group": "comedy", "limit": 50}));
    assert!(names(&r, "items", "name").contains(&"See [link] now".to_owned()), "{r}");
    w.ok("browse", json!({"kind": "channel", "group": "news"}));
    w.ok("whats_on", json!({}));
    assert_eq!(w.ok("play", json!({"ref": "channel:1:m3u:promo"}))["title"], "Promo [link]");
}

/// The same programme on a better channel of a playlist kept out of All: the All playlist's
/// channel is the one picked (and played), the other is only in `also_on`; `search` lists the
/// All playlist's channel first too.
#[test]
fn guide_search_and_search_prefer_playlists_in_all() {
    let mut w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(|c| {
            c.execute("UPDATE channel SET quality = 3 WHERE key = 'm3u:extra'", [])?;
            c.execute(
                "UPDATE channel SET quality = 0, name = 'Extra News', clean_name = 'Extra News', norm_name = 'extra news' WHERE key = 'm3u:bbc1'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok("guide_search", json!({"query": "news at six"}));
    let first = &r["programmes"][0];
    assert_eq!(first["channel"]["ref"], "channel:1:m3u:bbc1", "{first}");
    assert!(first["also_on"].as_array().unwrap().iter().any(|c| c["ref"] == "channel:3:m3u:extra"), "{first}");
    let r = w.ok("search", json!({"query": "extra news", "kinds": ["channel"]}));
    let refs: Vec<&str> = r["results"].as_array().unwrap().iter().map(|x| x["ref"].as_str().unwrap()).collect();
    assert_eq!(refs[..2], ["channel:1:m3u:bbc1", "channel:3:m3u:extra"], "{r}");
}

/// The same title from the same start on two channels, ended on one and still on on the other,
/// is two programmes: the live one's ref is the channel it is live on (so `play` plays it), the
/// ended one's the channel it ended on.
#[test]
fn guide_search_keeps_a_title_apart_when_it_ends_at_different_times() {
    let mut w = World::new(pin_config());
    let now = w.now;
    w.ui.services()
        .store
        .writer()
        .blocking(move |c| {
            for (ec, stop) in [(1, now - 600), (2, now + 1200)] {
                c.execute(
                    "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, categories) VALUES (1, ?1, 1, ?2, ?3, 'Goal Rush', 'Sport')",
                    params![ec, now - 2400, stop],
                )?;
                c.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, 'Goal Rush', NULL)", [c.last_insert_rowid()])?;
            }
            Ok(())
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok("guide_search", json!({"query": "goal rush"}));
    let rows: Vec<(&str, &str)> =
        r["programmes"].as_array().unwrap().iter().map(|p| (p["airing"].as_str().unwrap(), p["channel"]["name"].as_str().unwrap())).collect();
    assert!(rows.contains(&("live", "ITV4")), "{r}");
    assert!(rows.contains(&("past", "BBC One HD")), "{r}");
    let live = r["programmes"].as_array().unwrap().iter().find(|p| p["airing"] == "live").unwrap();
    assert_eq!(w.ok("play", json!({"ref": live["ref"]}))["outcome"], "playing");
}

/// DESIGN §28.3 / §28.12: the agent sees what the screens show. The shared category rule (TV
/// Two has no rule of its own) hides its German sports category and its French films category
/// from every tool and count — their channel is no `also_on` alternative either — and "Also
/// hide titles in other languages" (on by itself for an English-only rule) hides a Dutch title
/// in its kept, untagged category. Without the shared rule they all return.
#[test]
fn the_shared_category_rule_and_title_languages_apply_to_every_tool() {
    use tuner::model::{CategoryFilter, CategoryTag};
    let mut w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(|c| {
            let group = |name: &str, kind: &str| -> rusqlite::Result<i64> {
                c.execute(
                    "INSERT INTO grp(source_id, kind, key, name, raw_name, provider_order, item_count, seen_gen) VALUES (2, ?1, ?2, ?3, ?3, 1, 1, 1)",
                    params![kind, format!("{kind}:{}", name.to_lowercase()), name],
                )?;
                Ok(c.last_insert_rowid())
            };
            let de = group("DE: Sport", "channel")?;
            c.execute(
                "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, tvg_shift_min, stream, url_key, provider_order, added_at, seen_gen, quality)
                 VALUES (2, 'm3u:skyde', 'Sky Sport DE', 'Sky Sport DE', 'sky sport de', 20, 0, '{\"t\":\"url\",\"url\":\"http://127.0.0.1/live/skyde.ts\"}', 'http://127.0.0.1/live/skyde.ts', 0, 0, 1, 2)",
                [],
            )?;
            let sky = c.last_insert_rowid();
            c.execute("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, 0, 1)", params![de, sky])?;
            c.execute("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, 2)", [sky])?;
            let fr = group("FR ✪ CINEMA", "movie")?;
            let films: i64 = c.query_row("SELECT id FROM grp WHERE source_id = 2 AND key = 'movie:films'", [], |r| r.get(0))?;
            for (key, name, lang, grp) in [("m3u:lefilm", "Le Film", None, fr), ("m3u:nl", "NL ✪ Zwartboek", Some("NL"), films), ("m3u:heat", "Heat", None, films)] {
                c.execute(
                    "INSERT INTO movie(source_id, key, name, clean_name, norm_name, year, stream, provider_order, added_at, seen_gen, match_lang)
                     VALUES (2, ?1, ?2, ?2, lower(?2), 2006, ?3, 9, 1700000100, 1, ?4)",
                    params![key, name, format!(r#"{{"t":"url","url":"http://127.0.0.1/movie/{key}.mp4"}}"#), lang],
                )?;
                c.execute("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, 0)", params![grp, c.last_insert_rowid()])?;
            }
            let english = CategoryFilter {
                keep: ["AU", "CA", "GB", "IE", "NZ", "US"].into_iter().map(CategoryTag::country).chain([CategoryTag::language("en")]).collect(),
                keep_untagged: true,
                ..Default::default()
            };
            tuner::store::sources::set_shared_category_filter(c, Some(&english))
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    for q in ["zwartboek", "le film", "sky sport"] {
        let r = w.ok("search", json!({"query": q}));
        assert!(!mentions(&r, q), "{q}: {r}");
    }
    let r = w.ok("categories", json!({"kind": "movie", "playlist": "tv two"}));
    assert_eq!(names(&r, "categories", "name"), ["Films"], "{r}");
    assert_eq!(r["categories"][0]["count"], 2, "Dune and Heat, not the Dutch title: {r}");
    let r = w.ok("categories", json!({"kind": "channel"}));
    assert!(!names(&r, "categories", "name").iter().any(|c| c == "DE: Sport"), "{r}");
    let r = w.ok("browse", json!({"kind": "movie", "playlist": "tv two", "sort": "name"}));
    assert_eq!((names(&r, "items", "name"), r["total"].as_u64()), (vec!["Dune".to_owned(), "Heat".to_owned()], Some(2)));
    assert_eq!(w.err("browse", json!({"kind": "movie", "group": "cinema"})), ErrorCode::NotFound);
    let r = w.ok("guide_search", json!({"query": "touring car"}));
    assert!(!mentions(&r, "sky sport"), "{r}");

    // no shared rule: everything of TV Two shows
    w.ui.services().store.writer().blocking(|c| tuner::store::sources::set_shared_category_filter(c, None)).unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    for q in ["zwartboek", "le film", "sky sport"] {
        let r = w.ok("search", json!({"query": q}));
        assert!(mentions(&r, q), "{q}: {r}");
    }
    let r = w.ok("categories", json!({"kind": "movie", "playlist": "tv two"}));
    assert_eq!(names(&r, "categories", "name").len(), 2, "{r}");
    let r = w.ok("browse", json!({"kind": "movie", "playlist": "tv two"}));
    assert_eq!(r["total"], 4, "{r}");
    let r = w.ok("guide_search", json!({"query": "british touring"}));
    assert!(mentions(&r, "sky sport"), "{r}");
}

// ------------------------------------------------------------------ MCP pass 2 (the skill plan)

/// A ref from a tool answer (`list[i].ref`).
fn first_ref(v: &Value, list: &str) -> String {
    v[list][0]["ref"].as_str().unwrap_or_else(|| panic!("no {list}[0].ref in {v}")).to_owned()
}

/// The ref of a programme found by title words.
fn programme(w: &mut World, q: &str) -> String {
    first_ref(&w.ok("guide_search", json!({ "query": q })), "programmes")
}

/// "What football's on this weekend": the guide by category over a window (no title words),
/// with its categories on every row; explicit and hidden channels never show; a window is
/// required and bounded. Details name a channel's copies; spoiler_safe strips descriptions
/// (the guard's choke point), and reveal_spoilers gives them back for that ref only.
#[test]
fn the_guide_by_category_and_spoiler_safe_answers() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let win = json!({ "categories": ["motoring"], "from": (w.now - 3600).to_string(), "to": (w.now + 3 * 3600).to_string() });
    let r = w.ok("guide_search", win.clone());
    let titles = names(&r, "programmes", "title");
    assert!(titles.contains(&"British Touring Car Championship".to_owned()) && titles.contains(&"Touring Cars Qualifying".to_owned()), "{titles:?}");
    assert!(titles.contains(&"Locked Final".to_owned()), "locked channels are listed, flagged: {titles:?}");
    for hidden in ["After Dark", "Secret Channel", "Canal Sport", "Late Night"] {
        assert!(!mentions(&r, hidden), "{hidden} in {r}");
    }
    assert_eq!(r["programmes"][0]["categories"], json!(["Sport", "Motoring"]));
    assert!(r["programmes"][0]["remaining_min"].as_u64().is_some(), "live rows say how long is left");
    let none = w.ok("guide_search", json!({ "categories": ["cookery"], "from": "now", "to": (w.now + 3600).to_string() }));
    assert!(names(&none, "programmes", "title").is_empty());
    let r = w.ok("guide_search", json!({ "categories": ["motoring"], "channel": "itv4 +1", "from": "now", "to": (w.now + 6 * 3600).to_string() }));
    assert!(r["programmes"].as_array().unwrap().iter().all(|p| p["channel"]["name"] == "ITV4 +1"), "{r}");
    assert_eq!(w.err("guide_search", json!({ "categories": ["motoring"] })), ErrorCode::BadRequest, "a window is needed");
    assert_eq!(
        w.err("guide_search", json!({ "categories": ["motoring"], "from": "now", "to": (w.now + 4 * 86_400).to_string() })),
        ErrorCode::BadRequest,
        "at most 3 days"
    );
    let r = w.ok("guide_search", json!({ "query": "touring", "catchup_only": true }));
    assert!(r["programmes"].as_array().unwrap().iter().all(|p| p["catchup"] == true && p["catchup_until"].is_string()), "{r}");

    // details: the channel and its copies; spoiler-safe answers
    let d = w.ok("details", json!({ "ref": "channel:1:m3u:itv4" }));
    assert_eq!(d["copies"][0]["ref"], "channel:1:m3u:itv4");
    assert_eq!((d["copies"][0]["has_guide"].as_bool(), d["copies"][0]["catchup_days"].as_u64()), (Some(true), Some(7)));
    let live = programme(&mut w, "british touring");
    let safe = w.ok("details", json!({ "ref": live, "spoiler_safe": true }));
    assert!(safe.get("description").is_none(), "{safe}");
    assert_eq!(safe["spoiler"]["reason"], "spoiler_safe");
    let shown = w.ok("details", json!({ "ref": live, "spoiler_safe": true, "reveal_spoilers": [live] }));
    assert_eq!(shown["description"], "Live from Brands Hatch");
}

/// The library tools keep parental control's rules: an explicit title is "missing" to
/// match_titles and absent from history; a locked title is never added to a list; a lock can
/// only be added (episodes aren't lockable), and "lock the session now" locks it.
#[test]
fn library_tools_keep_the_parental_rules() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let r = w.ok(
        "match_titles",
        json!({ "titles": [{ "title": "Dune", "year": 2021 }, { "title": "Adult Film" }, { "title": "Not In The Library" }, { "title": "AIRPLANE!" }] }),
    );
    let res = r["results"].as_array().unwrap();
    assert_eq!(res[0]["matched"].as_array().unwrap().len(), 1, "one Dune (its two copies are one title): {r}");
    assert_eq!(res[0]["matched"][0]["other_copies"], 1);
    assert_eq!((res[1]["missing"].as_bool(), res[2]["missing"].as_bool()), (Some(true), Some(true)), "explicit = missing: {r}");
    assert_eq!(res[3]["matched"][0]["name"], "Airplane!");
    assert!(!mentions(&r, "explicit."));

    let h = w.ok("history", json!({}));
    let kinds: Vec<(String, String)> =
        h["items"].as_array().unwrap().iter().map(|i| (i["kind"].as_str().unwrap().to_owned(), i["title"].as_str().unwrap().to_owned())).collect();
    assert_eq!(kinds.len(), 3, "{h}");
    assert!(!mentions(&h, "adult"), "{h}");
    let ep = h["items"].as_array().unwrap().iter().find(|i| i["kind"] == "episode").unwrap();
    assert_eq!((ep["series"].as_str(), ep["season"].as_u64()), (Some("The Show"), Some(1)));
    assert_eq!(names(&w.ok("history", json!({ "kind": "movie" })), "items", "title"), ["Airplane!"]);

    let show = first_ref(&w.ok("search", json!({ "query": "the show", "kinds": ["series"] })), "results");
    let eps = w.ok("episodes", json!({ "series_ref": show }));
    assert_eq!(eps["episodes"].as_array().unwrap().len(), 3);
    assert_eq!(eps["episodes"][1]["progress"]["position_min"], 15, "progress of the copy in progress: {eps}");
    assert_eq!(eps["next_episode"], eps["episodes"][1]["ref"]);
    assert_eq!(names(&w.ok("episodes", json!({ "series_ref": show, "query": "episode 3" })), "episodes", "title"), ["Episode 3"]);

    // favourites_edit: a new list; the locked film and the explicit one are refused
    let dune = first_ref(&w.ok("search", json!({ "query": "dune" })), "results");
    let locked = first_ref(&w.ok("search", json!({ "query": "locked film" })), "results");
    let out = w.ok("favourites_edit", json!({ "create": "Trending", "add": [dune, locked, "movie:1:m3u:adult"] }));
    assert_eq!((out["created"].as_bool(), out["added"].as_u64()), (Some(true), Some(1)), "{out}");
    let errors: Vec<&str> = out["refused"].as_array().unwrap().iter().map(|r| r["error"].as_str().unwrap()).collect();
    assert_eq!(errors, ["locked: needs your PIN in Tuner", "not_found"]);
    let favs = w.ok("favourites", json!({ "list": "trending" }));
    assert_eq!(names(&favs["lists"][0], "items", "name"), ["Dune"]);
    let out = w.ok("favourites_edit", json!({ "list": "trending", "remove": [dune] }));
    assert_eq!(out["removed"], 1);

    // lock: adds only; an episode isn't lockable
    let airplane = first_ref(&w.ok("search", json!({ "query": "airplane", "kinds": ["movie"] })), "results");
    let ep_ref = eps["episodes"][0]["ref"].as_str().unwrap().to_owned();
    let out = w.ok("lock", json!({ "refs": [airplane, ep_ref, locked] }));
    assert_eq!((out["locked"].as_u64(), out["already_locked"].as_u64()), (Some(1), Some(1)), "{out}");
    assert!(out["refused"][0]["error"].as_str().unwrap().starts_with("not_lockable"));
    w.publish(Visibility::NO_EXPLICIT);
    assert_eq!(w.ok("play", json!({ "ref": airplane }))["outcome"], "pin_required", "locked by the agent: the PIN on screen");
    w.commands();

    // the session: unlocked by the user, locked by the agent on request
    w.unlock(Some(Instant::now()));
    let st = w.ok("parental_status", json!({}));
    assert_eq!((st["pin_set"].as_bool(), st["session_unlocked"].as_bool(), st["relock_min"].as_u64()), (Some(true), Some(true), Some(1)));
    let out = w.ok("lock", json!({ "session_now": true }));
    assert_eq!(out["session_locked"], true);
    assert_eq!(w.commands(), [AgentCommand::LockSession]);
    let st = w.ok("parental_status", json!({}));
    assert_eq!((st["session_unlocked"].as_bool(), st["explicit_shown"].as_bool()), (Some(false), Some(false)));
}

/// "Why can't she find it?": search names the user's own settings that hide a title (a hidden
/// category, the category filter, a playlist out of All) — never parental control.
#[test]
fn search_explains_the_users_own_filters_only() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let why = |w: &mut World, q: &str| -> Vec<(String, Option<String>)> {
        let r = w.ok("search", json!({ "query": q, "explain_filtered": true }));
        r["filtered"]
            .as_array()
            .map(|a| a.iter().map(|f| (f["why"].as_str().unwrap().to_owned(), f["category"].as_str().map(str::to_owned))).collect())
            .unwrap_or_default()
    };
    assert_eq!(why(&mut w, "secret channel"), [("hidden_category".to_owned(), Some("Hidden Group".to_owned()))]);
    assert_eq!(why(&mut w, "canal sport"), [("category_filter".to_owned(), Some("FR: Sport".to_owned()))]);
    assert_eq!(why(&mut w, "extra movie"), [("not_in_all".to_owned(), None)]);
    for q in ["late night", "adult film"] {
        assert!(why(&mut w, q).is_empty(), "explicit content is never explained: {q}");
    }
    w.unlock(Some(Instant::now()));
    w.publish(Visibility::default());
    assert!(why(&mut w, "adult film").is_empty(), "not even while unlocked");
}

/// diagnose reads the failures and the pictures; sources the playlists and guides — with no
/// address, login or provider message in either (`World::reply` checks every answer).
#[test]
fn diagnose_and_sources() {
    let mut w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(move |c| {
            let itv = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("m3u:itv4".into()) };
            for _ in 0..2 {
                tuner::store::live::record_failure(c, &itv, "http-forbidden", tuner::model::now())?;
            }
            let late = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("m3u:late".into()) };
            tuner::store::live::record_failure(c, &late, "stalled", tuner::model::now())
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    let d = w.ok("diagnose", json!({ "ref": "channel:1:m3u:itv4" }));
    assert_eq!((d["item"]["failures"].as_u64(), d["item"]["last_failure"]["reason"].as_str()), (Some(2), Some("http-forbidden")));
    assert_eq!(d["likely"], json!(["dead_channel"]));
    let tv_one = d["playlists"].as_array().unwrap().iter().find(|p| p["playlist"] == "TV One").unwrap();
    assert_eq!(tv_one["failed_channels"], 1, "the explicit channel's failure isn't counted while locked: {d}");
    assert_eq!(w.err("diagnose", json!({ "ref": "channel:1:m3u:late" })), ErrorCode::NotFound);
    let s = w.ok("sources", json!({}));
    let one = s["playlists"].as_array().unwrap().iter().find(|p| p["playlist"] == "TV One").unwrap();
    assert_eq!((one["kind"].as_str(), one["failures_24h"].as_u64()), (Some("m3u"), Some(2)));
    assert_eq!(one["channels"], 4, "what Tuner shows, the explicit channel not counted while locked: {one}");
    assert!(one["guide_coverage"]["with_guide"].as_u64().unwrap() >= 1);
    assert_eq!(s["guides"][0]["guide"], "Guide");
}

/// reminders: notify, then the same programme as a switch (one row), listed, cancelled; a
/// started programme and a locked channel are refused. The sleep timer is Tuner's own.
#[test]
fn reminders_and_the_sleep_timer() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let qual = programme(&mut w, "touring cars qualifying");
    let add = w.ok("reminders", json!({ "action": "add", "programme": qual, "lead_min": 5 }));
    let r = &add["reminder"];
    assert_eq!((r["mode"].as_str(), r["lead_min"].as_u64(), add["existed"].as_bool()), (Some("notify"), Some(5), Some(false)));
    let id = r["id"].as_i64().unwrap();
    let again = w.ok("reminders", json!({ "action": "add", "programme": qual, "mode": "switch", "open_in": "popout" }));
    assert_eq!((again["reminder"]["id"].as_i64(), again["existed"].as_bool()), (Some(id), Some(true)));
    assert_eq!((again["reminder"]["mode"].as_str(), again["reminder"]["open_in"].as_str()), (Some("switch"), Some("popout")));
    let list = w.ok("reminders", json!({ "action": "list" }));
    assert_eq!(names(&list, "reminders", "title"), ["Touring Cars Qualifying"]);
    let live = programme(&mut w, "british touring");
    assert_eq!(w.err("reminders", json!({ "action": "add", "programme": live })), ErrorCode::Unavailable);
    let locked = programme(&mut w, "locked final");
    let e = w.reply("reminders", json!({ "action": "add", "programme": locked }));
    assert!(e.error.unwrap().message.starts_with("locked: needs your PIN"), "locked channels can't be scheduled");
    assert_eq!(w.err("reminders", json!({ "action": "add", "programme": qual, "mode": "switch", "lead_min": 3 })), ErrorCode::BadRequest);
    assert_eq!(w.ok("reminders", json!({ "action": "cancel", "id": id }))["cancelled"], true);
    assert!(names(&w.ok("reminders", json!({ "action": "list" })), "reminders", "title").is_empty());
    assert_eq!(w.err("reminders", json!({ "action": "cancel", "id": id })), ErrorCode::NotFound);
    // "switch me over when it starts" after its notification went out: the fired row switches
    let note = w.ok("reminders", json!({ "action": "add", "programme": qual }));
    let id = tuner::model::ReminderId(note["reminder"]["id"].as_i64().unwrap());
    w.ui.services().store.writer().blocking(move |c| tuner::store::user::set_reminder_state(c, id, tuner::model::ReminderState::Fired)).unwrap();
    let switch = w.ok("reminders", json!({ "action": "add", "programme": qual, "mode": "switch" }));
    assert_eq!((switch["reminder"]["id"].as_i64(), switch["existed"].as_bool()), (Some(id.0), Some(true)));
    assert_eq!(switch["reminder"]["mode"], "switch");
    let list = w.ok("reminders", json!({ "action": "list" }));
    assert_eq!(list["reminders"][0]["mode"], "switch", "pending again: {list}");
    w.ok("reminders", json!({ "action": "cancel", "id": id.0 }));

    // the sleep timer runs in Tuner
    assert_eq!(w.err("sleep_timer", json!({ "action": "status" })), ErrorCode::Unavailable, "not running yet");
    let s = w.ui.services();
    let player = s.tiles.focused();
    let timer = tuner::reminders::sleep::SleepService::start(
        s.rt.handle(),
        player.subscribe(),
        s.agent.subscribe_queue(),
        tuner::reminders::sleep::player_actuator(vec![player.clone()]),
        s.bus.clone(),
    );
    s.agent.set_sleep(Some(timer));
    let set = w.ok("sleep_timer", json!({ "action": "set", "after_min": 30, "then": "pause" }));
    assert_eq!((set["set"]["when"].as_str(), set["set"]["then"].as_str()), (Some("at"), Some("pause")));
    assert_eq!(w.ok("status", json!({}))["sleep_timer"]["then"], "pause");
    assert_eq!(w.err("sleep_timer", json!({ "action": "set", "after_min": 30, "end_of_queue": true })), ErrorCode::BadRequest);
    assert_eq!(w.err("sleep_timer", json!({ "action": "set", "at": "2001-01-01T23:00" })), ErrorCode::BadRequest, "in the past");
    assert_eq!(w.err("sleep_timer", json!({ "action": "set", "end_of_programme": true })), ErrorCode::Unavailable, "nothing plays");
    assert_eq!(w.ok("sleep_timer", json!({ "action": "cancel" }))["cancelled"], true);
    assert!(w.ok("sleep_timer", json!({ "action": "status" }))["set"].is_null());
}

/// queue: an agent's adds go through the Up next engine, which refuses a locked title ("locked:
/// needs your PIN in Tuner") and an explicit one (not_found); a bare channel has no slot.
#[test]
fn the_queue_tool_drives_up_next() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    assert_eq!(w.err("queue", json!({ "action": "list" })), ErrorCode::Unavailable, "no engine yet");
    let s = w.ui.services();
    let engine = tuner::queue::service::spawn(
        s.rt.handle(),
        s.agent.clone(),
        tuner::queue::service::EngineDeps {
            store: s.store.clone(),
            config: s.config.clone(),
            player: Arc::new(s.tiles.focused()),
            bus: s.bus.clone(),
        },
    );
    s.agent.set_queue_handle(Some(engine));
    let airplane = first_ref(&w.ok("search", json!({ "query": "airplane", "kinds": ["movie"] })), "results");
    let locked = first_ref(&w.ok("search", json!({ "query": "locked film" })), "results");
    let qual = programme(&mut w, "touring cars qualifying");
    let out = w.ok("queue", json!({ "action": "add", "refs": [airplane, locked, qual, "channel:1:m3u:itv4", "movie:1:m3u:adult"] }));
    let added: Vec<&str> = out["added"].as_array().unwrap().iter().map(|a| a["ref"].as_str().unwrap()).collect();
    assert_eq!(added, [airplane.as_str(), qual.as_str()], "{out}");
    let refused: Vec<(&str, &str)> =
        out["refused"].as_array().unwrap().iter().map(|r| (r["ref"].as_str().unwrap(), r["error"].as_str().unwrap())).collect();
    assert!(refused.contains(&(locked.as_str(), "locked: needs your PIN in Tuner")), "{refused:?}");
    assert!(refused.contains(&("channel:1:m3u:itv4", "no_guide")), "{refused:?}");
    assert!(refused.contains(&("movie:1:m3u:adult", "not_found")), "{refused:?}");
    let list = w.ok("queue", json!({ "action": "list" }));
    assert_eq!(names(&list, "items", "title"), ["Airplane! (1980)", "Touring Cars Qualifying"]);
    assert_eq!(list["items"][1]["kind"], "programme");
    let second = list["items"][1]["id"].as_i64().unwrap();
    w.ok("queue", json!({ "action": "move", "ids": [second], "to": "top" }));
    assert_eq!(names(&w.ok("queue", json!({ "action": "list" })), "items", "title"), ["Touring Cars Qualifying", "Airplane! (1980)"]);
    assert_eq!(w.ok("queue", json!({ "action": "remove", "ids": [second] }))["removed"], 1);
    assert_eq!(w.ok("status", json!({}))["queue"]["left"], 1);
    assert_eq!(w.ok("queue", json!({ "action": "clear", "played_only": false }))["removed"], 1);
    assert!(w.ok("status", json!({})).get("queue").is_none());
}

/// DESIGN §30.13: an agent's play opens in Tuner's full window when the window is on screen —
/// whatever it shows, unraised (the agent's terminal keeps the keyboard) — and in the pop-out
/// when it's out of sight or asked for, and says where (`opened_in`). Off the desktop Hyprland
/// can't tell whether the window is seen: it counts as on screen (nothing moves uninvited);
/// without Hyprland (no window published) it opens in the window, raised, as before.
/// `queue play` shows what Up next started the same way (the app never heard of it before:
/// the corner mini player); the next items follow it into a pop-out. A failed play puts an
/// empty pop-out back in.
#[test]
fn agent_plays_open_in_the_window_on_screen_and_say_where() {
    use tuner::agent::act::AgentShow;
    use tuner::reminders::ui::WindowNow;
    let mut w = World::new(Config::default());
    w.publish(Visibility::NO_EXPLICIT);
    let shown = |w: &World| match &w.commands()[..] {
        [AgentCommand::Play(p)] => p.show,
        [AgentCommand::Show(s)] => *s,
        other => panic!("{other:?}"),
    };
    let full = AgentShow { fullscreen: true, ..AgentShow::window(true) };
    let bbc = json!({ "ref": "channel:1:m3u:bbc1" });
    let out = w.ok("play", bbc.clone());
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("window"), AgentShow::window(true)), "no Hyprland: {out}");

    let shared = w.ui.services().agent.clone();
    let away = WindowNow { can_pop: true, ..Default::default() };
    shared.set_window(away);
    let out = w.ok("play", bbc.clone());
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("window"), AgentShow::window(false)), "browsing, seen: {out}");
    assert!(out["message"].as_str().unwrap().ends_with("It's in Tuner's window."), "{out}");
    let out = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1", "open_in": "popout" }));
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("popout"), AgentShow::POP_OUT), "{out}");
    assert!(out["message"].as_str().unwrap().ends_with("It's in the pop-out."), "{out}");
    shared.set_window(WindowNow { fullscreen: true, watching: true, ..away });
    assert_eq!((w.ok("play", bbc.clone())["opened_in"].as_str(), shown(&w)), (Some("window"), AgentShow::window(false)), "fullscreen stays");
    shared.set_window(away);
    let tv = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1", "open_in": "full_window" }));
    assert_eq!((tv["opened_in"].as_str(), shown(&w)), (Some("window"), full));
    assert_eq!(w.err("play", json!({ "ref": "channel:1:m3u:bbc1", "open_in": "tv" })), ErrorCode::BadRequest);
    assert!(w.commands().is_empty());
    // a play that fails while the pop-out shows nothing (Tuner started for it): back in
    shared.set_window(WindowNow { popped_out: true, ..away });
    assert_eq!(w.err("play", json!({ "ref": "channel:1:m3u:gone" })), ErrorCode::NotFound);
    assert_eq!(w.commands(), [AgentCommand::PopIn]);
    shared.set_window(away);

    let s = w.ui.services();
    let engine = tuner::queue::service::spawn(
        s.rt.handle(),
        s.agent.clone(),
        tuner::queue::service::EngineDeps {
            store: s.store.clone(),
            config: s.config.clone(),
            player: Arc::new(s.tiles.focused()),
            bus: s.bus.clone(),
        },
    );
    shared.set_queue_handle(Some(engine));
    let films: Vec<String> = ["airplane", "long comedy", "dune"]
        .iter()
        .map(|q| first_ref(&w.ok("search", json!({ "query": q, "kinds": ["movie"] })), "results"))
        .collect();
    assert_eq!(w.ok("queue", json!({ "action": "add", "refs": films }))["added"].as_array().unwrap().len(), 3);
    assert_eq!(w.err("queue", json!({ "action": "list", "open_in": "popout" })), ErrorCode::BadRequest);
    let out = w.ok("queue", json!({ "action": "play" }));
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("window"), AgentShow::window(false)), "{out}");
    shared.set_window(WindowNow { popped_out: true, ..away });
    // a stop ends Up next's drive (it keeps its place): the skip places it again. (Not "the
    // test player never plays": under a loaded run Up next can drive its failing loads for long.)
    w.ok("control", json!({ "action": "stop" }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while shared.queue_handle().unwrap().snapshot().driving {
        assert!(Instant::now() < deadline, "a stop never ended Up next's drive");
        std::thread::sleep(Duration::from_millis(20));
    }
    w.commands();
    let out = w.ok("queue", json!({ "action": "skip" }));
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("popout"), AgentShow::POP_OUT), "the pop-out stays: {out}");
    let out = w.ok("queue", json!({ "action": "skip", "open_in": "full_window" }));
    assert_eq!((out["opened_in"].as_str(), shown(&w)), (Some("window"), full), "{out}");
}

/// follow: a preview never looks at explicit channels; a spoiler-safe rule shows only its
/// name; rules an agent adds are marked with its name and capped at 10 an hour.
#[test]
fn follow_rules_preview_add_and_cap() {
    let mut w = World::new(pin_config());
    w.agent = Some(ClientInfo { name: "codex-mcp-client".into(), title: Some("Codex".into()) });
    w.publish(Visibility::NO_EXPLICIT);
    let rule = json!({ "name": "Touring", "kind": "custom", "terms": ["touring"] });
    let p = w.ok("follow", json!({ "action": "preview", "rule": rule }));
    let titles = names(&p, "matches", "title");
    assert!(titles.contains(&"Touring Cars Qualifying".to_owned()), "{p}");
    assert!(!mentions(&p, "after dark") && !mentions(&p, "late night"), "{p}");
    let safe = w.ok(
        "follow",
        json!({ "action": "preview", "rule": { "name": "Cars", "kind": "event", "terms": ["touring"], "airings": "any" }, "spoiler_safe": true }),
    );
    assert!(safe["matches"].as_array().unwrap().iter().all(|m| m["title"] == "Cars" && m.get("sub_title").is_none()), "{safe}");
    assert_eq!(w.err("follow", json!({ "action": "add", "rule": { "name": "x", "kind": "derby" } })), ErrorCode::BadRequest);
    let news = w.ok(
        "follow",
        json!({ "action": "preview", "rule": { "name": "News at Six", "kind": "news_flash", "days": ["mon", "fri"], "headlines_min": 3 } }),
    );
    assert_eq!(news["rule"]["mode"], "flash");

    let added = w.ok("follow", json!({ "action": "add", "rule": rule }));
    let id = added["rule"]["id"].as_i64().unwrap();
    assert_eq!(added["rule"]["by_agent"], true);
    let toasts: Vec<String> = w.ui.events.try_iter().filter_map(|e| if let AppEvent::Toast(t) = e { Some(t.text) } else { None }).collect();
    assert!(toasts.iter().any(|t| t.starts_with("Codex added Touring · ")), "{toasts:?}");
    let stored = tuner::store::follow::rule(&w.ui.services().store.reader().unwrap(), tuner::model::FollowRuleId(id)).unwrap().unwrap();
    assert!(tuner::follow::ui::rule_summary(&stored).ends_with("by Codex"), "Settings ▸ Following names it");
    let list = w.ok("follow", json!({ "action": "list" }));
    assert_eq!(names(&list, "rules", "name"), ["Touring"]);
    for i in 1..10 {
        w.ok("follow", json!({ "action": "add", "rule": { "name": format!("Rule {i}"), "kind": "custom", "terms": [format!("word{i}")] } }));
    }
    let e = w.reply("follow", json!({ "action": "add", "rule": { "name": "One too many", "kind": "custom" } }));
    assert_eq!(e.error.as_ref().unwrap().code, ErrorCode::Unavailable, "10 an hour: {e:?}");
    let out = w.ok("follow", json!({ "action": "remove", "id": id }));
    assert_eq!(out["removed"], true);
    assert_eq!(w.err("follow", json!({ "action": "remove", "id": id })), ErrorCode::NotFound);
}

/// spoiler_guard: an agent adds a topic and marks a programme seen (once), removes its rule; a
/// topic blackout needs an end.
#[test]
fn the_spoiler_guard_tool() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let id =
        w.ok("spoiler_guard", json!({ "action": "add_topic", "label": "Touring cars", "terms": ["touring car", "btcc"] }))["id"].as_i64().unwrap();
    let list = w.ok("spoiler_guard", json!({ "action": "list" }));
    assert_eq!((list["rules"][0]["kind"].as_str(), list["rules"][0]["by_agent"].as_bool()), (Some("topic"), Some(true)));
    let live = programme(&mut w, "british touring");
    assert_eq!(w.ok("spoiler_guard", json!({ "action": "mark_seen", "ref": live }))["marked"], true);
    assert_eq!(w.ok("spoiler_guard", json!({ "action": "mark_seen", "ref": live }))["marked"], false, "seen once");
    let show = first_ref(&w.ok("search", json!({ "query": "the show", "kinds": ["series"] })), "results");
    assert!(w.ok("spoiler_guard", json!({ "action": "add_series", "series_ref": show }))["id"].as_i64().is_some());
    assert_eq!(w.err("spoiler_guard", json!({ "action": "blackout", "topic": "Final" })), ErrorCode::BadRequest);
    let qual = programme(&mut w, "touring cars qualifying");
    let b = w.ok("spoiler_guard", json!({ "action": "blackout", "ref": qual }));
    assert!(b["id"].as_i64().is_some());
    // a fixture's blackout guards its two teams (news sub-titles name teams, not the fixture)
    let now = w.now;
    w.ui.services()
        .store
        .writer()
        .blocking(move |c| {
            c.execute(
                "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, categories) VALUES (1, 2, 1, ?1, ?2, 'PL: Arsenal v Chelsea', 'Sport')",
                params![now + 10_000, now + 17_000],
            )?;
            c.execute("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, 'PL: Arsenal v Chelsea', NULL)", [c.last_insert_rowid()])?;
            Ok(())
        })
        .unwrap();
    let fixture = programme(&mut w, "arsenal chelsea");
    let b = w.ok("spoiler_guard", json!({ "action": "blackout", "ref": fixture }))["id"].as_i64().unwrap();
    let list = w.ok("spoiler_guard", json!({ "action": "list" }));
    let rule = list["rules"].as_array().unwrap().iter().find(|r| r["id"] == b).unwrap();
    assert_eq!((rule["label"].as_str(), rule["terms"].clone()), (Some("Arsenal v Chelsea"), json!(["arsenal", "chelsea"])));
    assert_eq!(w.ok("spoiler_guard", json!({ "action": "remove", "id": id }))["removed"], true);
    // the user's own rule stays theirs: an agent never loosens the guard
    use tuner::model::{SpoilerMode, SpoilerRule, SpoilerRuleId, SpoilerRuleKind};
    let mine =
        w.ui.services()
            .store
            .writer()
            .blocking(move |c| {
                let r = SpoilerRule {
                    id: SpoilerRuleId(0),
                    kind: SpoilerRuleKind::Topic,
                    label: "Strictly".into(),
                    terms: vec!["strictly".into()],
                    series_norm: None,
                    categories: vec![],
                    mode: SpoilerMode::Always,
                    window_h: 0,
                    until_at: None,
                    sport_descr: false,
                    shield: false,
                    enabled: true,
                    by_agent: false,
                    agent: Default::default(),
                    created_at: 0,
                };
                tuner::store::spoiler::add_rule(c, &r, now)
            })
            .unwrap();
    assert_eq!(w.err("spoiler_guard", json!({ "action": "remove", "id": mine.0 })), ErrorCode::BadRequest);
    assert!(w.ok("spoiler_guard", json!({ "action": "status" }))["topics"].is_array());
}

/// guide_fix: propose → apply (only with confirm) → undo; a locked channel is refused while
/// the session is locked, an explicit one doesn't exist; an agent undoes only agents' batches.
#[test]
fn guide_fixes_round_trip() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    let issues = w.ok("guide_fix", json!({ "action": "issues" }));
    assert!(issues["coverage"].as_array().unwrap().iter().any(|c| c["playlist"] == "TV One"), "{issues}");
    assert!(!mentions(&issues, "late night") && !mentions(&issues, "secret"), "{issues}");
    let p = w.ok(
        "guide_fix",
        json!({ "action": "propose", "changes": [
            { "ref": "channel:1:m3u:bbc1", "shift_min": 5 },
            { "ref": "channel:1:m3u:locked", "shift_min": 5 },
            { "ref": "channel:1:m3u:late", "shift_min": 5 },
        ] }),
    );
    assert_eq!(p["summary"]["channels"], 1, "{p}");
    let rejected: Vec<&str> = p["rejected"].as_array().unwrap().iter().map(|r| r["reason"].as_str().unwrap()).collect();
    assert_eq!(rejected, ["locked: needs your PIN in Tuner", "not_found"]);
    assert_eq!(p["changes"][0]["before"]["now"]["title"], "News at Six");
    let id = p["proposal_id"].as_str().unwrap().to_owned();
    assert_eq!(w.err("guide_fix", json!({ "action": "apply", "proposal_id": id })), ErrorCode::BadRequest, "confirm=true only after a yes");
    let done = w.ok("guide_fix", json!({ "action": "apply", "proposal_id": id, "confirm": true }));
    assert_eq!(done["applied"], 1);
    assert_eq!(w.err("guide_fix", json!({ "action": "apply", "proposal_id": id, "confirm": true })), ErrorCode::NotFound, "applied once");
    let undo = done["undo_id"].as_i64().unwrap();
    assert_eq!(w.ok("guide_fix", json!({ "action": "undo", "undo_id": undo }))["reverted"], 1);
    assert_eq!(w.err("guide_fix", json!({ "action": "undo", "undo_id": undo })), ErrorCode::Unavailable, "undone already");
    // a playlist rule re-maps its locked channels too
    let rule = json!({ "action": "propose", "rules": [{ "playlist": "TV One", "timeshift_siblings": true }] });
    assert_eq!(w.err("guide_fix", rule), ErrorCode::Unavailable, "TV One has a locked channel and the session is locked");
}

/// guide_fix: a "yes" to more than 50 channels applies nothing from chat; the whole proposal
/// goes to Tuner (`AgentCommand::GuideFixReview`, whose confirm card the user confirms with
/// Enter, tested with the app rig in `src/app.rs`) and is Tuner's from then on. A batch of 50
/// applies from chat and toasts with its Undo.
#[test]
fn guide_fix_batches_over_50_go_to_tuners_confirm_card() {
    let mut w = World::new(pin_config());
    w.ui.services()
        .store
        .writer()
        .blocking(|c| {
            let news: i64 = c.query_row("SELECT id FROM grp WHERE source_id = 1 AND key = 'channel:news'", [], |r| r.get(0))?;
            for i in 0..55 {
                let url = format!("http://127.0.0.1/live/more{i}.ts");
                c.execute(
                    "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, stream, url_key, provider_order, added_at, seen_gen)
                     VALUES (1, ?1, ?2, ?2, lower(?2), ?3, ?4, ?5, ?3, ?3, 1)",
                    params![format!("m3u:more{i}"), format!("More News {i}"), 100 + i, format!(r#"{{"t":"url","url":"{url}"}}"#), url],
                )?;
                c.execute(
                    "INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, 1)",
                    params![news, c.last_insert_rowid(), 10 + i],
                )?;
            }
            Ok(())
        })
        .unwrap();
    w.publish(Visibility::NO_EXPLICIT);
    let propose = |w: &mut World, n: usize| {
        let changes: Vec<Value> = (0..n).map(|i| json!({ "ref": format!("channel:1:m3u:more{i}"), "shift_min": 15 })).collect();
        let p = w.ok("guide_fix", json!({ "action": "propose", "changes": changes }));
        assert_eq!(p["summary"]["channels"].as_u64(), Some(n as u64), "{p}");
        assert_eq!(p["applies_in_chat"].as_bool(), Some(n <= 50));
        p["proposal_id"].as_str().unwrap().to_owned()
    };
    let drain = |w: &World| w.ui.events.try_iter().collect::<Vec<AppEvent>>();
    drain(&w);

    let id = propose(&mut w, 55);
    let out = w.ok("guide_fix", json!({ "action": "apply", "proposal_id": id, "confirm": true }));
    assert_eq!((out["pending_review"].as_bool(), out["channels"].as_u64()), (Some(true), Some(55)), "{out}");
    assert!(out["message"].as_str().unwrap().contains("Enter"), "{out}");
    let events = drain(&w);
    let reviews: Vec<&tuner::agent::GuideFixReview> = events
        .iter()
        .filter_map(|e| match e {
            AppEvent::Agent(AgentCommand::GuideFixReview(r)) => Some(r),
            _ => None,
        })
        .collect();
    let [review] = reviews.as_slice() else { panic!("one review: {events:?}") };
    assert_eq!(review.proposal.changes.len(), 55);
    assert!(review.proposal.changes.iter().all(|c| c.shift_min == Some(Some(15))));
    assert_ne!(review.proposal.stamp, [0; 16], "still bound to the guide check it was made from");
    assert!(review.expires > w.now);
    assert!(!events.iter().any(|e| matches!(e, AppEvent::Toast(_))), "nothing applied, nothing toasted");
    let written = |w: &World| tuner::store::guide_fix::recent(&w.ui.services().store.reader().unwrap(), 10).unwrap().len();
    assert_eq!(written(&w), 0);
    assert_eq!(w.err("guide_fix", json!({ "action": "apply", "proposal_id": id, "confirm": true })), ErrorCode::NotFound, "Tuner's now");

    let id = propose(&mut w, 50);
    let done = w.ok("guide_fix", json!({ "action": "apply", "proposal_id": id, "confirm": true }));
    assert_eq!(done["applied"], 50);
    assert_eq!(written(&w), 1);
    let undo = tuner::model::GuideFixLogId(done["undo_id"].as_i64().unwrap());
    let toasts: Vec<tuner::msg::Toast> = drain(&w)
        .into_iter()
        .filter_map(|e| match e {
            AppEvent::Toast(t) => Some(t),
            _ => None,
        })
        .collect();
    assert!(
        toasts.iter().any(|t| t.text == "Your agent fixed the guide for 50 channels"
            && t.action == Some(("Undo".into(), tuner::keymap::Action::UndoGuideFix(Some(undo))))),
        "{toasts:?}"
    );
}

/// multiview: two live pictures are sent to the app; movies stay single view, a future
/// programme hasn't started, and swap / close need multi-view on.
#[test]
fn the_multiview_tool() {
    let mut w = World::new(pin_config());
    w.publish(Visibility::NO_EXPLICIT);
    w.commands();
    let out = w.ok("multiview", json!({ "action": "open", "refs": ["channel:1:m3u:itv4", "channel:1:m3u:bbc1"], "sound": 1 }));
    assert_eq!(out["outcome"], "opening");
    match w.commands().as_slice() {
        [AgentCommand::MultiView(tuner::agent::AgentMultiView::Open { targets, sound: Some(1), rearrange: false, .. })] => {
            assert_eq!(targets.len(), 2)
        }
        other => panic!("{other:?}"),
    }
    let airplane = first_ref(&w.ok("search", json!({ "query": "airplane", "kinds": ["movie"] })), "results");
    assert_eq!(w.err("multiview", json!({ "action": "open", "refs": ["channel:1:m3u:itv4", airplane] })), ErrorCode::Unsupported);
    let later = programme(&mut w, "touring cars qualifying");
    assert_eq!(w.err("multiview", json!({ "action": "open", "refs": ["channel:1:m3u:itv4", later] })), ErrorCode::NotStarted);
    assert_eq!(w.err("multiview", json!({ "action": "open", "refs": ["channel:1:m3u:late", "channel:1:m3u:itv4"] })), ErrorCode::NotFound);
    assert_eq!(w.err("multiview", json!({ "action": "swap", "what": "sound" })), ErrorCode::Unavailable);
    assert_eq!(w.err("multiview", json!({ "action": "close" })), ErrorCode::Unavailable);
}

// ------------------------------------------------------------------ casting (DESIGN §40 §12, §14.8)

mod casting {
    use std::net::Ipv4Addr;

    use tuner::agent::act::AgentShow;
    use tuner::cast::firewall::{Detection, FirewallState, LanNet};
    use tuner::cast::testing::FakeCast;
    use tuner::cast::{CastDiagnose, CastHandle, Device, PairVariant, Pairing, Protocol, Usability};
    use tuner::model::{CastErrorKind, DeviceId, ReceiverKind};
    use tuner::playback::TilesState;

    use super::*;

    /// "Living Room" (an Apple TV, paired), "Bedroom" (a third-party AirPlay TV, not paired) and
    /// "Study" (a Mac). Each has an address the agent must never see.
    struct Tvs {
        fake: Arc<FakeCast>,
        living: Device,
        bedroom: Device,
        study: Device,
    }

    fn device(name: &str, kind: ReceiverKind, pairing: Pairing, host: u8) -> Device {
        let addr = std::net::SocketAddr::from((Ipv4Addr::new(192, 168, 1, host), 7000));
        let mut d = Device::new(DeviceId::derive("airplay", name), name, kind, Protocol::AirPlay).at(addr);
        d.pairing = pairing;
        d
    }

    fn cast_world(cfg: Config) -> (World, Tvs) {
        let w = World::new(cfg);
        let tvs = Tvs {
            fake: Arc::new(FakeCast::default()),
            living: device("Living Room", ReceiverKind::AppleTv, Pairing::Paired(PairVariant::Hkp3), 50),
            bedroom: device("Bedroom", ReceiverKind::AirPlayTv, Pairing::Required, 51),
            study: {
                let mut mac = device("Study", ReceiverKind::AppleTv, Pairing::Paired(PairVariant::Hkp3), 52);
                mac.usability = Usability::Mac;
                mac
            },
        };
        let all = vec![tvs.living.clone(), tvs.bedroom.clone(), tvs.study.clone()];
        tvs.fake.script(|s| s.devices = all);
        w.ui.services().agent.set_cast(Some(CastHandle::new(tvs.fake.clone())));
        (w, tvs)
    }

    fn device_ref(d: &Device) -> String {
        format!("device:{}", d.id)
    }

    /// The reply of a failing call: its code and message.
    fn refused(w: &mut World, method: &str, params: Value) -> (ErrorCode, String) {
        let e = w.reply(method, params.clone()).error.unwrap_or_else(|| panic!("{method} {params} should fail"));
        (e.code, e.message)
    }

    fn assert_no_address(v: &Value) {
        assert!(!v.to_string().contains("192.168"), "an address in {v}");
    }

    /// `cast_devices` reads the registry (it never starts a scan): refs, names, kinds and flags,
    /// no address; a device's own text is sanitised first.
    #[test]
    fn cast_devices_lists_the_registry_without_addresses() {
        let (mut w, tvs) = cast_world(Config::default());
        tvs.fake.script(|s| s.devices.push(device("http://evil.example/x", ReceiverKind::AppleTv, Pairing::Required, 53)));
        let out = w.ok("cast_devices", json!({}));
        assert_no_address(&out);
        assert_eq!(out["devices"].as_array().unwrap().len(), 4);
        assert_eq!(
            out["devices"][0],
            json!({ "ref": device_ref(&tvs.living), "name": "Living Room", "kind": "apple_tv", "paired": true, "available": true, "casting": false })
        );
        let bedroom = &out["devices"][1];
        assert_eq!((bedroom["kind"].as_str(), bedroom["paired"].as_bool()), (Some("airplay_tv"), Some(false)));
        assert_eq!(out["devices"][3]["name"], "TV", "a name with an address in it is replaced");
        assert!(tvs.fake.take_calls().is_empty(), "reading the list scans nothing");
        // casting is off (no service): an empty list, not an error
        assert_eq!(World::new(Config::default()).ok("cast_devices", json!({})), json!({ "devices": [] }));
    }

    /// `play {device}`: an unpaired TV is `pairing_required` and sends nothing; a paired one is
    /// `casting_started` with `opened_in: device`, in place whatever `open_in` says, by ref or
    /// by exact name; a locked item asks for the PIN in the window first and sends the cast
    /// with it.
    #[test]
    fn play_with_a_device_casts_in_place_and_asks_the_pin_on_screen() {
        let (mut w, tvs) = cast_world(pin_config());
        w.publish(Visibility::NO_EXPLICIT);
        let bbc = || json!({ "ref": "channel:1:m3u:bbc1", "device": device_ref(&tvs.living) });
        let sent = |w: &World| match &w.commands()[..] {
            [AgentCommand::Play(p)] => p.clone(),
            other => panic!("{other:?}"),
        };

        let out = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1", "device": "Bedroom" }));
        assert_eq!((out["outcome"].as_str(), out["device"].as_str()), (Some("pairing_required"), Some("Bedroom")), "{out}");
        assert!(out.get("opened_in").is_none() && out["message"].as_str().unwrap().contains("Settings ▸ Cast"), "{out}");
        assert!(w.commands().is_empty(), "nothing is cast to a TV that isn't paired");

        let out = w.ok("play", bbc());
        assert_eq!(
            (out["outcome"].as_str(), out["opened_in"].as_str(), out["device"].as_str()),
            (Some("casting_started"), Some("device"), Some("Living Room")),
            "{out}"
        );
        assert_eq!(out["message"], "Casting BBC One HD to Living Room.");
        let p = sent(&w);
        assert_eq!((p.device, p.show), (Some(tvs.living.id), AgentShow::IN_PLACE), "the window is not placed or raised");
        assert!(matches!(p.target, PlayTarget::Media(MediaRef::Channel(_))), "the channel plays on the TV");

        // by exact name, asked for the pop-out or the TV screen: still in place
        for open_in in ["popout", "full_window"] {
            let out = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1", "device": "living room", "open_in": open_in }));
            assert_eq!(out["opened_in"], "device", "{open_in}: {out}");
            let p = sent(&w);
            assert_eq!((p.device, p.show), (Some(tvs.living.id), AgentShow::IN_PLACE), "{open_in}");
        }
        // a window that is popped out is the app's to put back (unraised); the handler places nothing
        w.ui.services().agent.set_window(tuner::reminders::ui::WindowNow { can_pop: true, popped_out: true, ..Default::default() });
        w.ok("play", bbc());
        assert_eq!(sent(&w).show, AgentShow::IN_PLACE);

        // locked: the PIN is typed in Tuner's window, which comes forward; nothing is cast before
        let out = w.ok("play", json!({ "ref": "channel:1:m3u:locked", "device": device_ref(&tvs.living) }));
        assert_eq!(
            (out["outcome"].as_str(), out["opened_in"].as_str(), out["device"].as_str()),
            (Some("pin_required"), Some("window"), Some("Living Room")),
            "{out}"
        );
        assert!(out["message"].as_str().unwrap().ends_with("It will play on Living Room."), "{out}");
        let p = sent(&w);
        assert_eq!((p.device, p.show), (Some(tvs.living.id), AgentShow::window(true)));

        // a play without a device is as before
        let out = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1" }));
        assert!(out.get("device").is_none() && out["outcome"] == "playing", "{out}");
        assert_eq!(sent(&w).device, None);
        assert_no_address(&out);
    }

    /// While picture A casts to Living Room, a `play` without `device` lands on that TV
    /// (RULES.md): the reply says so (`opened_in: device`, the TV's name), and the window is
    /// neither popped out nor raised (DESIGN §40 §3.8), whatever "Agents play in" says.
    #[test]
    fn a_play_without_a_device_while_casting_stays_on_the_tv() {
        let (mut w, tvs) = cast_world(Config::default());
        w.publish(Visibility::NO_EXPLICIT);
        // the session's far ends live as long as the test
        let ends: Arc<std::sync::Mutex<Vec<Box<dyn std::any::Any + Send>>>> = Arc::default();
        let keep = ends.clone();
        tvs.fake.script(move |s| {
            s.open = Some(Box::new(move |device| {
                let (cmd, cmds) = tokio::sync::mpsc::unbounded_channel::<tuner::player::remote::RemoteCmd>();
                let (events_tx, events) = crossbeam_channel::unbounded::<tuner::player::remote::RemoteEvent>();
                keep.lock().unwrap().push(Box::new((cmds, events_tx)));
                let view = tuner::model::CastView {
                    device,
                    name: "Living Room".into(),
                    kind: ReceiverKind::AppleTv,
                    phase: tuner::model::CastPhase::Connecting,
                    level: None,
                    behind_live_s: None,
                    can_seek: true,
                    volume: false,
                };
                Ok(tuner::player::remote::RemoteOutput { cmd, events, view })
            }));
        });
        let id: i64 = w.ui.services().store.reader().unwrap().query_row("SELECT id FROM channel WHERE key = 'm3u:bbc1'", [], |r| r.get(0)).unwrap();
        let a = w.ui.services().tiles.handle(tuner::model::TileId::A);
        let media = MediaRef::Channel(tuner::model::ChannelId(id));
        let play = tuner::player::PlayRequest { media, zap: None, origin: tuner::model::PlayOrigin::Cli, resume: false, immediate: true };
        a.send(tuner::player::PlayerCommand::CastTo { device: tvs.living.id, play: Some(play) });
        let deadline = Instant::now() + Duration::from_secs(10);
        while a.snapshot().cast.is_none() {
            assert!(Instant::now() < deadline, "picture A never started casting");
            std::thread::sleep(Duration::from_millis(20));
        }

        let out = w.ok("play", json!({ "ref": "channel:1:m3u:bbc1" }));
        assert_eq!(
            (out["outcome"].as_str(), out["opened_in"].as_str(), out["device"].as_str()),
            (Some("playing"), Some("device"), Some("Living Room")),
            "{out}"
        );
        assert!(out["message"].as_str().unwrap().ends_with("It's on Living Room.") && !mentions(&out, "pop-out"), "{out}");
        match &w.commands()[..] {
            [AgentCommand::Play(p)] => assert_eq!((p.device, p.show), (None, AgentShow::IN_PLACE), "the window is not placed or raised"),
            other => panic!("{other:?}"),
        }
        drop(ends);
    }

    /// What stops a cast is typed, never device text: no such TV, a Mac, two pictures on, the
    /// firewall known to block the port. An unknown firewall, or one not looked at toward a
    /// network, blocks nothing here.
    #[test]
    fn play_with_a_device_refuses_with_a_typed_reason() {
        let (mut w, tvs) = cast_world(Config::default());
        w.publish(Visibility::NO_EXPLICIT);
        let ask = |d: &str| json!({ "ref": "channel:1:m3u:bbc1", "device": d });
        let (code, message) = refused(&mut w, "play", ask("device:00112233445566778899aabbccddeeff"));
        assert_eq!(code, ErrorCode::NotFound, "{message}");
        assert_eq!(refused(&mut w, "play", ask("Kitchen")).0, ErrorCode::NotFound);
        assert_eq!(refused(&mut w, "play", ask("device:xyz")).0, ErrorCode::BadRequest);
        let (code, message) = refused(&mut w, "play", ask("Study"));
        assert_eq!(code, ErrorCode::Unavailable);
        assert!(message.starts_with("cast-unsupported: "), "{message}");
        // the item is still checked
        assert_eq!(refused(&mut w, "play", json!({ "ref": "channel:1:m3u:gone", "device": "Living Room" })).0, ErrorCode::NotFound);

        let blocked = |net: Option<LanNet>| Detection {
            state: FirewallState::Blocked { shadowed: false },
            ufw: true,
            pkexec: true,
            net,
            tagged: Vec::new(),
            port: 47800,
        };
        let lan = LanNet { iface: "wlan0".into(), addr: Ipv4Addr::new(192, 168, 1, 20), prefix: 24 };
        tvs.fake.script(|s| s.firewall = Some(blocked(Some(lan))));
        let (code, message) = refused(&mut w, "play", ask("Living Room"));
        assert_eq!(code, ErrorCode::Unavailable);
        assert!(message.starts_with("cast-firewall: ") && message.contains("Allow on firewall"), "{message}");
        assert_no_address(&json!(message));
        tvs.fake.script(|s| s.firewall = Some(blocked(None)));
        assert_eq!(w.ok("play", ask("Living Room"))["outcome"], "casting_started", "no network was looked at: the session's own check decides");
        tvs.fake.script(|s| s.firewall = None);

        w.ui.services().agent.publish_tiles(TilesState { on: true, ..Default::default() });
        let (code, message) = refused(&mut w, "play", ask("Living Room"));
        assert_eq!(code, ErrorCode::Unavailable);
        assert!(message.starts_with("multiview-on: "), "{message}");
        assert_eq!(w.commands().len(), 1, "only the one that went through reached the app");
    }

    /// `status.casting` after a cast ended without the user asking: the TV and the typed reason,
    /// so an agent can say what happened while Tuner is out of sight; nothing is claimed when no
    /// cast failed. `control stop_casting` needs a cast; `play_here` after the TV stopped it doesn't.
    #[test]
    fn status_and_control_speak_of_a_cast_that_ended() {
        let (mut w, tvs) = cast_world(Config::default());
        w.publish(Visibility::NO_EXPLICIT);
        assert!(w.ok("status", json!({})).get("casting").is_none());
        tvs.fake.script(|s| {
            s.last_error = Some(CastErrorKind::StoppedOnTv);
            s.diagnose = Some(CastDiagnose { device: Some(tvs.living.id), timing: "ptp-declared", ..CastDiagnose::default() });
        });
        let st = w.ok("status", json!({}));
        assert_no_address(&st);
        assert_eq!(
            st["casting"],
            json!({ "ref": device_ref(&tvs.living), "device": "Living Room", "kind": "apple_tv", "phase": "lost", "error": "cast-stopped-on-tv" })
        );
        assert_eq!(st["state"], "idle");

        let (code, message) = refused(&mut w, "control", json!({ "action": "stop_casting" }));
        assert_eq!(code, ErrorCode::Unavailable, "{message}");
        // the alert's Play here (the fix skill offers it): the last item here, nothing to replay
        // in this world
        assert_eq!(w.ok("control", json!({ "action": "play_here" }))["state"], "idle");
        assert_eq!(w.err("control", json!({ "action": "stop_casting", "value": 1 })), ErrorCode::BadRequest);
    }

    /// `diagnose` has the cast section once casting has something to say, with the receiver, the
    /// pairing variant, the timing, the firewall and the last error as words, and no address.
    #[test]
    fn diagnose_has_a_cast_section_and_no_address() {
        let (mut w, tvs) = cast_world(Config::default());
        w.publish(Visibility::NO_EXPLICIT);
        tvs.fake.script(|s| {
            s.diagnose = Some(CastDiagnose {
                device: Some(tvs.living.id),
                model: Some("AppleTV14,1".into()),
                os_version: Some("26.6 (23L773)".into()),
                source_version: Some("940.38.1".into()),
                pairing: Some(PairVariant::Hkp3),
                timing: "ptp-declared",
                firewall: Some(FirewallState::Blocked { shadowed: false }),
                last_error: Some(CastErrorKind::Firewall),
            });
        });
        let out = w.ok("diagnose", json!({}));
        assert_no_address(&out);
        assert_eq!(
            out["cast"],
            json!({
                "model": "AppleTV14,1",
                "os_version": "26.6 (23L773)",
                "source_version": "940.38.1",
                "pairing": "hkp3",
                "timing": "ptp-declared",
                "firewall": "blocked",
                "last_error": "cast-firewall",
            })
        );
        // casting is off, or nothing is known: no section
        let mut off = World::new(Config::default());
        off.publish(Visibility::NO_EXPLICIT);
        assert!(off.ok("diagnose", json!({})).get("cast").is_none());
    }
}
