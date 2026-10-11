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
use tuner::agent::proto::{Call, ErrorCode, Reply, Request};
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
    let progs: [(i64, i64, i64, &str, &str); 8] = [
        (1, now - 3 * 3600, now - 2 * 3600, "Old News", "Yesterday's headlines at http://news.example/"),
        (1, now - 1800, now + 1800, "News at Six", "The day's news"),
        (2, now - 7200, now - 3600, "Touring Car Highlights", "Last weekend's races"),
        (2, now - 1800, now + 3600, "British Touring Car Championship", "Live from Brands Hatch"),
        (2, now + 7200, now + 9000, "Touring Cars Qualifying", "Qualifying"),
        (3, now - 600, now + 3000, "Locked Match", "A locked match"),
        (4, now - 600, now + 3000, "Touring Cars After Dark", "Explicit"),
        (2, now - 3 * 86400, now - 3 * 86400 + 1800, "Touring Car Classics", "From the archive"),
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
            AgentHandler::with_limits(s.agent.clone(), s.store.clone(), s.config.clone(), s.player.handle(), s.bus.clone(), s.jobs.clone(), limits);
        Self { ui, handler, now, next: 1 }
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
        let reply = self.ui.services().rt.block_on(self.handler.handle(Request::new(self.next, call)));
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
    let AgentCommand::Play(play) = &sent[0];
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
    use tuner::agent::shared::ParentalView;
    use tuner::model::{Playable, PlayableKind};
    use tuner::player::{PlayerSnapshot, PlayerState};
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
