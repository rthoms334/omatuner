//! Resolve: `MediaRef` → `Playable` against a real store (WP-02 writers + readers) and the
//! frozen seeders. Owner: WP-05b. DESIGN §10, §11, §20.

mod support;

use rusqlite::Connection;
use support::seed;
use tuner::config::Config;
use tuner::error::Error;
use tuner::model::*;
use tuner::redact::Secret;
use tuner::sources::catchup::{self, CatchupRequest};
use tuner::sources::identity::{group_key, normalize_name, url_key};
use tuner::sources::resolve::{catchup_available, resolve};
use tuner::store::import::{self, ImportChannel, ImportEpisode, ImportGroup, ImportMovie, ImportSeries, MovieDetails, SeriesDetails};
use tuner::store::{Store, live, sources, user, vod};

/// 2026-10-09 20:00:00 UTC (21:00 in London, BST).
const T20: Timestamp = 1_791_576_000;
const HOUR: i64 = 3600;

fn store(name: &str) -> Store { Store::open_temp(name, None).expect("temp store") }

/// Run `f` on the writer connection.
fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn reader(s: &Store) -> Connection { s.reader().expect("reader") }

fn xtream_source(s: &Store, server: &str, password: &str, settings: SourceSettings, account: Option<AccountInfo>) -> SourceId {
    let draft = SourceDraft {
        name: "Panel".into(),
        kind: SourceKind::Xtream,
        icon: "x".into(),
        color: SourceColor::Blue,
        url: None,
        path: None,
        xtream: Some(XtreamCredentials { server: server.into(), username: "user".into(), password: Secret::new(password.into()) }),
        settings,
    };
    w(s, move |c| {
        let id = sources::insert_source(c, &draft, T20)?;
        if let Some(a) = account {
            sources::set_account(c, id, &a)?;
        }
        Ok(id)
    })
}

fn m3u_source(s: &Store, settings: SourceSettings) -> SourceId {
    w(s, move |c| {
        let id = seed::seed_source(c, "Playlist")?;
        sources::set_source_settings(c, id, &settings)?;
        Ok(id)
    })
}

fn account(formats: &[&str], tz: Option<&str>) -> AccountInfo {
    AccountInfo {
        status: AccountStatus::Active,
        allowed_formats: formats.iter().map(|f| f.to_string()).collect(),
        server_tz: tz.map(str::to_owned),
        ..Default::default()
    }
}

fn chan(key: &str, name: &str, stream: StreamLocator) -> ImportChannel {
    let url = match &stream {
        StreamLocator::Url { url } => url_key(url),
        StreamLocator::Xtream { .. } => None,
    };
    ImportChannel {
        key: ItemKey(key.into()),
        provider_id: key.strip_prefix("xt:live:").map(str::to_owned),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        number: Some(7),
        logo: Some("http://img/logo.png".into()),
        tvg_id: None,
        tvg_name: None,
        tvg_shift_min: 0,
        stream,
        url_key: url,
        headers: None,
        catchup: None,
        radio: false,
        unsupported: None,
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Channel, "News")],
    }
}

fn xt_live(id: i64) -> StreamLocator { StreamLocator::Xtream { kind: XtreamStreamKind::Live, stream_id: id, ext: None } }

fn url_stream(url: &str) -> StreamLocator { StreamLocator::Url { url: url.into() } }

fn spec(kind: CatchupKind, template: Option<&str>, days: u32) -> CatchupSpec {
    CatchupSpec { kind, template: template.map(str::to_owned), days, correction_s: 0 }
}

/// Import `channels` (one catalogue run, channels completed); returns their ids in order.
fn import_channels(s: &Store, source: SourceId, channels: Vec<ImportChannel>) -> Vec<ChannelId> {
    let keys: Vec<ItemKey> = channels.iter().map(|c| c.key.clone()).collect();
    w(s, move |c| {
        let imp = import::begin_catalogue(c, source, T20)?;
        let g = ImportGroup { kind: ItemKind::Channel, key: group_key(ItemKind::Channel, "News"), name: "News".into(), provider_id: None, provider_order: 0 };
        import::upsert_groups(c, &imp, &[g])?;
        import::write_channels(c, &imp, &channels)?;
        import::finish_catalogue(c, &imp, &[ItemKind::Channel], T20)?;
        Ok(())
    });
    let r = reader(s);
    keys.into_iter()
        .map(|key| {
            let item = ItemRef { source, kind: ItemKind::Channel, key };
            live::channel_by_ref(&r, &item).expect("read").expect("imported channel").id
        })
        .collect()
}

fn movie(key: &str, name: &str, stream: StreamLocator) -> ImportMovie {
    ImportMovie {
        key: ItemKey(key.into()),
        provider_id: None,
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: Some(2019),
        poster: Some("http://img/poster.jpg".into()),
        rating: None,
        stream,
        url_key: None,
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Movie, "Films")],
    }
}

fn import_movies(s: &Store, source: SourceId, movies: Vec<ImportMovie>) -> Vec<MovieId> {
    let keys: Vec<ItemKey> = movies.iter().map(|m| m.key.clone()).collect();
    w(s, move |c| {
        let imp = import::begin_catalogue(c, source, T20)?;
        let g = ImportGroup { kind: ItemKind::Movie, key: group_key(ItemKind::Movie, "Films"), name: "Films".into(), provider_id: None, provider_order: 0 };
        import::upsert_groups(c, &imp, &[g])?;
        import::write_movies(c, &imp, &movies)?;
        import::finish_catalogue(c, &imp, &[ItemKind::Movie], T20)?;
        Ok(())
    });
    let r = reader(s);
    keys.into_iter()
        .map(|key| vod::resolve_movie(&r, &ItemRef { source, kind: ItemKind::Movie, key }).expect("read").expect("imported movie"))
        .collect()
}

/// One Xtream series with `episodes` (S1) stored through the lazy details path.
fn import_series(s: &Store, source: SourceId, name: &str, episodes: Vec<ImportEpisode>) -> (SeriesId, Vec<EpisodeId>) {
    let series = ImportSeries {
        key: ItemKey("xt:series:77".into()),
        provider_id: Some("77".into()),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: None,
        cover: Some("http://img/cover.jpg".into()),
        backdrop: None,
        rating: None,
        plot: None,
        genre: None,
        cast: None,
        director: None,
        release_date: None,
        last_modified: None,
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Series, "Shows")],
    };
    let id = w(s, move |c| {
        let imp = import::begin_catalogue(c, source, T20)?;
        let g = ImportGroup { kind: ItemKind::Series, key: group_key(ItemKind::Series, "Shows"), name: "Shows".into(), provider_id: None, provider_order: 0 };
        import::upsert_groups(c, &imp, &[g])?;
        import::write_series(c, &imp, &[series])?;
        import::finish_catalogue(c, &imp, &[ItemKind::Series], T20)?;
        let id: SeriesId = c.query_row("SELECT id FROM series WHERE source_id = ?1 AND key = 'xt:series:77'", [source], |r| r.get(0))?;
        import::store_series_details(c, id, &SeriesDetails { episodes, ..Default::default() }, T20)?;
        Ok(id)
    });
    let eps = vod::episodes(&reader(s), id, None).expect("episodes").into_iter().map(|e| e.id).collect();
    (id, eps)
}

fn episode(id: i64, n: u32, title: &str, ext: Option<&str>) -> ImportEpisode {
    ImportEpisode {
        key: ItemKey(format!("xt:ep:{id}")),
        provider_id: Some(id.to_string()),
        season: 1,
        episode: n,
        title: title.into(),
        plot: None,
        duration_s: Some(2700),
        still: None,
        air_date: None,
        rating: None,
        stream: StreamLocator::Xtream { kind: XtreamStreamKind::Series, stream_id: id, ext: ext.map(str::to_owned) },
    }
}

fn save_progress(s: &Store, item: ItemRef, position_s: f64, duration_s: Option<f64>, finished: bool) {
    w(s, move |c| {
        user::save_progress(
            c,
            &WatchProgress { item, position_s, duration_s, updated_at: T20, finished, title: "t".into(), fingerprint: Fingerprint::default() },
        )
    });
}

fn set_override(s: &Store, item: ItemRef, ov: ItemOverride) { w(s, move |c| user::set_item_override(c, &item, &ov)); }

fn catchup(channel: ChannelId, start: Timestamp, stop: Timestamp) -> MediaRef { MediaRef::Catchup { channel, start, stop, catchup_id: None } }

fn url_of(p: &Playable) -> &str { p.url.expose() }

fn not_found(r: tuner::error::Result<Playable>) -> String {
    match r {
        Err(Error::NotFound(m)) => m,
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// live

#[test]
fn seeded_m3u_channel_plays_its_url_with_programme_and_default_agent() {
    let s = store("resolve-seed-live");
    let ids = w(&s, |c| {
        let src = seed::seed_source(c, "Seed")?;
        let ids = seed::seed_channels(c, src, 3)?;
        seed::seed_epg(c, &ids, T20 - 2 * HOUR, 6)?;
        Ok(ids)
    });
    let cfg = Config::default();
    let p = resolve(&reader(&s), &cfg, &MediaRef::Channel(ids[1]), true, T20 + 600).expect("resolve");
    assert_eq!(url_of(&p), "http://127.0.0.1/live/1.ts");
    assert_eq!(p.kind, PlayableKind::Live);
    assert_eq!(p.media, MediaRef::Channel(ids[1]));
    assert_eq!(p.title, "Channel 1");
    assert_eq!(p.number, Some(2));
    assert_eq!(p.item.as_ref().map(|i| (i.kind, i.key.as_str())), Some((ItemKind::Channel, "m3u:seed00000000001")));
    assert!(p.source_id.is_some());
    // 20:10 is in the 5th 30-minute slot after 18:00 → "Show 4 on 1"
    assert_eq!(p.subtitle.as_deref(), Some("Show 4 on 1"), "current programme for the OSD/MPRIS");
    assert_eq!(p.headers.user_agent.as_deref(), Some(cfg.player.user_agent.as_str()));
    assert_eq!((p.start_at, p.duration_hint), (None, None));
    assert!(!p.kind.is_seekable());
}

#[test]
fn xtream_live_url_and_format_pick() {
    let s = store("resolve-xt-live");
    let src = xtream_source(&s, "http://panel.example:8080", "pw-live-91c2", SourceSettings::default(), Some(account(&["ts", "rtmp"], None)));
    let ch = import_channels(&s, src, vec![chan("xt:live:101", "BBC One", xt_live(101))])[0];
    let mut cfg = Config::default();
    let play = |cfg: &Config| resolve(&reader(&s), cfg, &MediaRef::Channel(ch), false, T20).expect("resolve");

    assert_eq!(url_of(&play(&cfg)), "http://panel.example:8080/live/user/pw-live-91c2/101.ts", "Auto + ts-only panel");

    w(&s, move |c| sources::set_account(c, src, &account(&["m3u8", "ts"], None)));
    assert_eq!(url_of(&play(&cfg)), "http://panel.example:8080/live/user/pw-live-91c2/101.m3u8", "Auto prefers m3u8 when allowed");

    cfg.general.live_format = LiveFormat::Ts;
    assert!(url_of(&play(&cfg)).ends_with("/101.ts"), "Auto source defers to the config default");

    w(&s, move |c| sources::set_source_settings(c, src, &SourceSettings { live_format: LiveFormat::M3u8, ..Default::default() }));
    assert!(url_of(&play(&cfg)).ends_with("/101.m3u8"), "the source's own choice wins");

    let p = play(&cfg);
    assert_eq!(p.title, "BBC One");
    assert_eq!(p.logo.as_deref(), Some("http://img/logo.png"));
    assert_eq!(p.item, Some(ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey("xt:live:101".into()) }));
}

#[test]
fn title_comes_from_the_catalogue_and_never_the_url() {
    let s = store("resolve-title");
    let pw = "pw-title-55e1";
    let src = xtream_source(&s, "http://panel.example", pw, SourceSettings::default(), None);
    let ch = import_channels(&s, src, vec![chan("xt:live:5", "Raw Name", xt_live(5))])[0];
    set_override(&s, ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey("xt:live:5".into()) }, ItemOverride {
        rename: Some("My Channel".into()),
        ..Default::default()
    });
    let p = resolve(&reader(&s), &Config::default(), &MediaRef::Channel(ch), false, T20).expect("resolve");
    assert_eq!(p.title, "My Channel", "user rename");
    let dbg = format!("{p:?}");
    assert!(!dbg.contains(pw), "Playable Debug must not leak the password: {dbg}");
    assert!(!p.title.contains(pw) && p.subtitle.as_deref().is_none_or(|t| !t.contains(pw)));
}

#[test]
fn m3u_headers_merge_entry_then_source_then_config() {
    let s = store("resolve-headers");
    let src = m3u_source(&s, SourceSettings { user_agent: Some("SourceUA/2".into()), ..Default::default() });
    let mut with_headers = chan("m3u:a", "A", url_stream("http://cdn.example/a.m3u8"));
    with_headers.headers = Some(HttpHeaders {
        user_agent: Some("EntryUA/1".into()),
        referrer: Some("http://ref.example/".into()),
        origin: Some("http://origin.example".into()),
        extra: vec![("Cookie".into(), "k=v, w".into())],
    });
    let mut referrer_only = chan("m3u:b", "B", url_stream("http://cdn.example/b.m3u8"));
    referrer_only.headers = Some(HttpHeaders { referrer: Some("http://ref.example/b".into()), ..Default::default() });
    let ids = import_channels(&s, src, vec![with_headers, referrer_only]);
    let mut cfg = Config::default();
    cfg.player.user_agent = "ConfigUA/3".into();
    let r = reader(&s);

    let a = resolve(&r, &cfg, &MediaRef::Channel(ids[0]), false, T20).expect("a");
    assert_eq!(url_of(&a), "http://cdn.example/a.m3u8");
    assert_eq!(a.headers.user_agent.as_deref(), Some("EntryUA/1"), "entry wins");
    assert_eq!(a.headers.referrer.as_deref(), Some("http://ref.example/"));
    assert_eq!(a.headers.origin.as_deref(), Some("http://origin.example"));
    assert_eq!(a.headers.extra, vec![("Cookie".to_owned(), "k=v, w".to_owned())]);

    let b = resolve(&r, &cfg, &MediaRef::Channel(ids[1]), false, T20).expect("b");
    assert_eq!(b.headers.user_agent.as_deref(), Some("SourceUA/2"), "source UA fills the gap");
    assert_eq!(b.headers.referrer.as_deref(), Some("http://ref.example/b"), "other entry headers kept");

    w(&s, move |c| sources::set_source_settings(c, src, &SourceSettings::default()));
    let b = resolve(&reader(&s), &cfg, &MediaRef::Channel(ids[1]), false, T20).expect("b");
    assert_eq!(b.headers.user_agent.as_deref(), Some("ConfigUA/3"), "config default last");
}

#[test]
fn drm_channels_are_refused_as_bad_payload_drm() {
    let s = store("resolve-drm");
    let src = m3u_source(&s, SourceSettings::default());
    let mut c = chan("m3u:drm", "Locked Sports", url_stream("http://cdn.example/drm.mpd"));
    c.unsupported = Some(Unsupported::Drm);
    c.catchup = Some(spec(CatchupKind::Shift, None, 3));
    let ch = import_channels(&s, src, vec![c])[0];
    let r = reader(&s);
    for media in [MediaRef::Channel(ch), catchup(ch, T20, T20 + HOUR)] {
        match resolve(&r, &Config::default(), &media, false, T20 + 2 * HOUR) {
            Err(Error::BadPayload(m)) => assert_eq!(m, "drm"),
            other => panic!("expected BadPayload(drm) for {media:?}, got {other:?}"),
        }
    }
}

#[test]
fn missing_rows_and_disabled_sources_are_not_found() {
    let s = store("resolve-missing");
    let r = reader(&s);
    let cfg = Config::default();
    for media in [MediaRef::Channel(ChannelId(999)), catchup(ChannelId(999), T20, T20 + HOUR), MediaRef::Movie(MovieId(999)), MediaRef::Episode(EpisodeId(999))] {
        not_found(resolve(&r, &cfg, &media, true, T20));
    }

    let src = m3u_source(&s, SourceSettings::default());
    let ch = import_channels(&s, src, vec![chan("m3u:x", "X", url_stream("http://cdn.example/x.ts"))])[0];
    assert!(resolve(&reader(&s), &cfg, &MediaRef::Channel(ch), false, T20).is_ok());
    w(&s, move |c| {
        let mut source = sources::get_source(c, src)?;
        source.enabled = false;
        sources::update_source(c, &source)
    });
    assert_eq!(not_found(resolve(&reader(&s), &cfg, &MediaRef::Channel(ch), false, T20)), "source is disabled");
}

#[test]
fn xtream_stream_in_a_source_without_credentials_is_a_config_error() {
    let s = store("resolve-nocreds");
    let src = m3u_source(&s, SourceSettings::default());
    let ch = import_channels(&s, src, vec![chan("xt:live:3", "Odd", xt_live(3))])[0];
    assert!(matches!(resolve(&reader(&s), &Config::default(), &MediaRef::Channel(ch), false, T20), Err(Error::Config(_))));
}

// ---------------------------------------------------------------------------------------------
// VOD

#[test]
fn xtream_movie_url_extension_and_metadata() {
    let s = store("resolve-movie");
    let src = xtream_source(&s, "http://panel.example:8080/", "pw-movie-0b7d", SourceSettings::default(), None);
    let ids = import_movies(&s, src, vec![
        movie("xt:vod:55", "Inception", StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 55, ext: Some("mkv".into()) }),
        movie("xt:vod:56", "No Ext", StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 56, ext: None }),
    ]);
    let cfg = Config::default();
    let p = resolve(&reader(&s), &cfg, &MediaRef::Movie(ids[0]), true, T20).expect("movie");
    assert_eq!(url_of(&p), "http://panel.example:8080/movie/user/pw-movie-0b7d/55.mkv");
    assert_eq!(p.kind, PlayableKind::Movie);
    assert!(p.kind.is_seekable());
    assert_eq!(p.title, "Inception");
    assert_eq!(p.subtitle.as_deref(), Some("2019"));
    assert_eq!(p.logo.as_deref(), Some("http://img/poster.jpg"));
    assert_eq!(p.item, Some(ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("xt:vod:55".into()) }));
    assert_eq!(p.headers.user_agent.as_deref(), Some(cfg.player.user_agent.as_str()));
    assert_eq!((p.start_at, p.duration_hint), (None, None), "no progress, no duration known yet");

    assert!(url_of(&resolve(&reader(&s), &cfg, &MediaRef::Movie(ids[1]), true, T20).expect("no ext")).ends_with("/56.mp4"));

    // lazy details: container extension and duration
    let movie_id = ids[0];
    w(&s, move |c| {
        import::store_movie_details(c, movie_id, &MovieDetails { container_ext: Some("mp4".into()), duration_s: Some(8880), ..Default::default() }, T20)
    });
    let p = resolve(&reader(&s), &cfg, &MediaRef::Movie(ids[0]), true, T20).expect("movie");
    assert!(url_of(&p).ends_with("/55.mp4"));
    assert_eq!(p.duration_hint, Some(8880.0));
}

#[test]
fn seeded_movie_and_episode_urls_play_as_is() {
    let s = store("resolve-seed-vod");
    let (movies, series) = w(&s, |c| {
        let src = seed::seed_source(c, "Seed")?;
        Ok((seed::seed_movies(c, src, 2)?, seed::seed_series_with_episodes(c, src, 1, 2, 3)?))
    });
    let r = reader(&s);
    let cfg = Config::default();
    let m = resolve(&r, &cfg, &MediaRef::Movie(movies[1]), true, T20).expect("movie");
    assert_eq!((url_of(&m), m.title.as_str()), ("http://127.0.0.1/movie/1.mp4", "Movie 1"));

    let eps = vod::episodes(&r, series[0], Some(2)).expect("episodes");
    let e = resolve(&r, &cfg, &MediaRef::Episode(eps[2].id), true, T20).expect("episode");
    assert_eq!(url_of(&e), "http://127.0.0.1/series/0/2/3.mp4");
    assert_eq!(e.kind, PlayableKind::Episode { season: 2, episode: 3 });
    assert_eq!(e.title, "Series 0");
    assert_eq!(e.subtitle.as_deref(), Some("S02E03 · Episode 3"));
    assert_eq!(e.duration_hint, Some(2700.0));
}

#[test]
fn xtream_episode_url_and_labels() {
    let s = store("resolve-episode");
    let src = xtream_source(&s, "https://panel.example", "pw-ep-44a0", SourceSettings { user_agent: Some("PanelUA".into()), ..Default::default() }, None);
    let (_, eps) = import_series(&s, src, "The Show", vec![episode(9001, 1, "Pilot", Some("mkv")), episode(9002, 2, "", None)]);
    let r = reader(&s);
    let cfg = Config::default();
    let e = resolve(&r, &cfg, &MediaRef::Episode(eps[0]), false, T20).expect("episode");
    assert_eq!(url_of(&e), "https://panel.example/series/user/pw-ep-44a0/9001.mkv");
    assert_eq!(e.kind, PlayableKind::Episode { season: 1, episode: 1 });
    assert_eq!(e.title, "The Show");
    assert_eq!(e.subtitle.as_deref(), Some("S01E01 · Pilot"));
    assert_eq!(e.logo.as_deref(), Some("http://img/cover.jpg"), "series cover when the episode has no still");
    assert_eq!(e.item, Some(ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey("xt:ep:9001".into()) }));
    assert_eq!(e.headers.user_agent.as_deref(), Some("PanelUA"));

    let e2 = resolve(&r, &cfg, &MediaRef::Episode(eps[1]), false, T20).expect("episode 2");
    assert!(url_of(&e2).ends_with("/9002.mp4"));
    assert_eq!(e2.subtitle.as_deref(), Some("S01E02"));
}

#[test]
fn resume_threshold_and_flags() {
    let s = store("resolve-resume");
    let src = xtream_source(&s, "http://panel.example", "pw-resume-1d2c", SourceSettings::default(), None);
    let ids = import_movies(&s, src, vec![movie("xt:vod:1", "M", StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 1, ext: None })]);
    let (_, eps) = import_series(&s, src, "S", vec![episode(500, 1, "E", None)]);
    let movie_ref = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("xt:vod:1".into()) };
    let ep_ref = ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey("xt:ep:500".into()) };
    let mut cfg = Config::default();
    cfg.player.resume_min_s = 60;
    let at = |media: &MediaRef, resume: bool, cfg: &Config| resolve(&reader(&s), cfg, media, resume, T20).expect("resolve").start_at;
    let m = MediaRef::Movie(ids[0]);

    save_progress(&s, movie_ref.clone(), 2480.0, Some(5400.0), false);
    assert_eq!(at(&m, true, &cfg), Some(2480.0));
    assert_eq!(at(&m, false, &cfg), None, "Start over drops the resume point");
    assert_eq!(resolve(&reader(&s), &cfg, &m, true, T20).expect("m").duration_hint, Some(5400.0), "duration from progress");

    save_progress(&s, movie_ref.clone(), 45.0, Some(5400.0), false);
    assert_eq!(at(&m, true, &cfg), None, "below resume_min_s");
    cfg.player.resume_min_s = 30;
    assert_eq!(at(&m, true, &cfg), Some(45.0), "threshold is live config");

    save_progress(&s, movie_ref.clone(), 2480.0, Some(5400.0), true);
    assert_eq!(at(&m, true, &cfg), None, "finished items start from the beginning");

    save_progress(&s, movie_ref, 5300.0, Some(5400.0), false);
    assert_eq!(at(&m, true, &cfg), None, "past watched_pct");

    let e = MediaRef::Episode(eps[0]);
    assert_eq!(at(&e, true, &cfg), None);
    save_progress(&s, ep_ref, 600.0, None, false);
    assert_eq!(at(&e, true, &cfg), Some(600.0));
}

// ---------------------------------------------------------------------------------------------
// catch-up

fn xtream_catchup_channel(s: &Store, server: &str, settings: SourceSettings, acc: Option<AccountInfo>, shift_min: i32) -> (SourceId, ChannelId) {
    let src = xtream_source(s, server, "pw-cu-6f1e", settings, acc);
    let mut c = chan("xt:live:1234", "Archive One", xt_live(1234));
    c.catchup = Some(spec(CatchupKind::Xc, None, 3));
    c.tvg_shift_min = shift_min;
    let ch = import_channels(s, src, vec![c])[0];
    (src, ch)
}

fn london() -> SourceSettings { SourceSettings { server_tz: Some("Europe/London".into()), ..Default::default() } }

#[test]
fn xtream_catchup_uses_server_local_time() {
    let s = store("resolve-xt-cu");
    let (src, ch) = xtream_catchup_channel(&s, "http://panel.example:8080", london(), Some(account(&["ts"], Some("Europe/Berlin"))), 0);
    let cfg = Config::default();
    let now = T20 + 2 * HOUR;
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20, T20 + HOUR), false, now).expect("catch-up");
    assert_eq!(url_of(&p), "http://panel.example:8080/timeshift/user/pw-cu-6f1e/60/2026-10-09:21-00/1234.ts", "source tz override wins");
    assert_eq!(p.kind, PlayableKind::Catchup { start: T20, stop: T20 + HOUR });
    assert!(p.kind.is_seekable());
    assert_eq!(p.title, "Archive One");
    assert_eq!(p.duration_hint, Some(3600.0));
    assert_eq!(p.item.as_ref().map(|i| i.kind), Some(ItemKind::Channel), "catch-up is keyed by its channel");
    assert_eq!(p.start_at, None);

    // a programme still running: the archive (and the hint) stop at now
    let running = resolve(&reader(&s), &cfg, &catchup(ch, T20, T20 + HOUR), false, T20 + 1800).expect("running");
    assert!(url_of(&running).ends_with("/30/2026-10-09:21-00/1234.ts"), "{}", running.url.redacted());
    assert_eq!(running.duration_hint, Some(1800.0));

    // same URL as the Xc flavour of sources::catchup builds from the live URL
    let live = resolve(&reader(&s), &cfg, &MediaRef::Channel(ch), false, now).expect("live");
    let xc = spec(CatchupKind::Xc, None, 3);
    let req = CatchupRequest {
        live_url: live.url.expose(),
        spec: &xc,
        start: T20,
        stop: T20 + HOUR,
        now,
        tvg_shift_min: 0,
        user_offset_s: 0,
        catchup_id: None,
        tz: jiff::tz::TimeZone::get("Europe/London").expect("tz"),
    };
    assert_eq!(catchup::build_url(&req).expect("xc"), url_of(&p));

    // no override → AccountInfo.server_tz (Berlin, CEST)
    w(&s, move |c| sources::set_source_settings(c, src, &SourceSettings::default()));
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20, T20 + HOUR), false, now).expect("catch-up");
    assert!(url_of(&p).ends_with("/60/2026-10-09:22-00/1234.ts"), "{}", p.url.redacted());

    // m3u8 allowed → m3u8 archive
    w(&s, move |c| sources::set_account(c, src, &account(&["m3u8"], Some("UTC"))));
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20, T20 + HOUR), false, now).expect("catch-up");
    assert!(url_of(&p).ends_with("/60/2026-10-09:20-00/1234.m3u8"), "{}", p.url.redacted());
}

#[test]
fn xtream_catchup_applies_shift_offset_and_caps_running_programmes() {
    let s = store("resolve-xt-cu-shift");
    // "+1" channel: the guide shows the 20:00 UTC programme at 21:00 UTC
    let (src, ch) = xtream_catchup_channel(&s, "http://panel.example", london(), None, 60);
    let cfg = Config::default();
    let now = T20 + 3 * HOUR;
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20 + HOUR, T20 + 2 * HOUR), false, now).expect("shifted");
    assert!(url_of(&p).ends_with("/60/2026-10-09:21-00/1234.m3u8"), "tvg-shift converts back to EPG time: {}", p.url.redacted());

    // per-channel user offset (Ctrl+I) +2 min
    set_override(&s, ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey("xt:live:1234".into()) }, ItemOverride {
        catchup_offset_s: Some(120),
        ..Default::default()
    });
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20 + HOUR, T20 + 2 * HOUR), false, now).expect("offset");
    assert!(url_of(&p).ends_with("/60/2026-10-09:21-02/1234.m3u8"), "{}", p.url.redacted());

    assert_eq!(p.duration_hint, Some(3600.0));

    // on air on the "+1" channel right now, but complete in the original channel's archive
    let now = T20 + HOUR + 30 * 60;
    let p = resolve(&reader(&s), &cfg, &catchup(ch, T20 + HOUR, T20 + 2 * HOUR), false, now).expect("+1 running");
    assert!(url_of(&p).contains("/60/2026-10-09:21-02/"), "{}", p.url.redacted());
    assert_eq!(p.duration_hint, Some(3600.0));
}

#[test]
fn xtream_catchup_with_a_server_path_prefix() {
    let s = store("resolve-xt-cu-path");
    let (_, ch) = xtream_catchup_channel(&s, "http://panel.example/iptv", london(), Some(account(&["ts"], None)), 0);
    let p = resolve(&reader(&s), &Config::default(), &catchup(ch, T20, T20 + HOUR), false, T20 + 2 * HOUR).expect("catch-up");
    assert_eq!(url_of(&p), "http://panel.example/iptv/timeshift/user/pw-cu-6f1e/60/2026-10-09:21-00/1234.ts");
}

#[test]
fn catchup_window_and_missing_archive() {
    let s = store("resolve-cu-window");
    let src = xtream_source(&s, "http://panel.example", "pw-cu-window", london(), None);
    let mut archived = chan("xt:live:1234", "Archive One", xt_live(1234));
    archived.catchup = Some(spec(CatchupKind::Xc, None, 3));
    let ids = import_channels(&s, src, vec![archived, chan("xt:live:1", "No Archive", xt_live(1))]);
    let (ch, plain) = (ids[0], ids[1]);
    let r = reader(&s);
    let cfg = Config::default();
    let now = T20 + 2 * HOUR;
    assert_eq!(not_found(resolve(&r, &cfg, &catchup(ch, now - 4 * 86_400, now - 4 * 86_400 + HOUR), false, now)), "outside catch-up window");
    assert_eq!(not_found(resolve(&r, &cfg, &catchup(ch, now + HOUR, now + 2 * HOUR), false, now)), "outside catch-up window");
    assert!(resolve(&r, &cfg, &catchup(ch, now - 3 * 86_400 + 60, now - 3 * 86_400 + HOUR), false, now).is_ok(), "inside the 3-day window");
    assert_eq!(not_found(resolve(&r, &cfg, &catchup(plain, T20, T20 + HOUR), false, now)), "no catch-up for this channel");

    assert!(catchup_available(true, 3, T20, now));
    assert!(!catchup_available(true, 3, now - 4 * 86_400, now));
    assert!(!catchup_available(false, 3, T20, now));
    assert!(!catchup_available(true, 3, now, now));
}

#[test]
fn m3u_catchup_flavours_and_headers() {
    let s = store("resolve-m3u-cu");
    let src = m3u_source(&s, london());
    let mut shift = chan("m3u:shift", "Shift", url_stream("http://h/ch/index.m3u8"));
    shift.catchup = Some(spec(CatchupKind::Shift, None, 2));
    shift.headers = Some(HttpHeaders { referrer: Some("http://ref/".into()), ..Default::default() });
    let mut fluss = chan("m3u:fs", "Fluss", url_stream("http://h/ch/index.m3u8"));
    fluss.catchup = Some(spec(CatchupKind::Flussonic, None, 2));
    let mut vodcu = chan("m3u:vod", "Vod", url_stream("http://h/live/9"));
    vodcu.catchup = Some(spec(CatchupKind::Default, Some("http://arch/{catchup-id}?s={utc}&d={duration}"), 2));
    let mut xc = chan("m3u:xc", "Xc", url_stream("http://h:80/live/u/p/42.ts"));
    xc.catchup = Some(spec(CatchupKind::Xc, None, 2));
    let ids = import_channels(&s, src, vec![shift, fluss, vodcu, xc]);
    let r = reader(&s);
    let cfg = Config::default();
    let now = T20 + 2 * HOUR;

    let p = resolve(&r, &cfg, &catchup(ids[0], T20, T20 + HOUR), false, now).expect("shift");
    assert_eq!(url_of(&p), "http://h/ch/index.m3u8?utc=1791576000&lutc=1791583200");
    assert_eq!(p.headers.referrer.as_deref(), Some("http://ref/"), "entry headers apply to the archive too");
    assert_eq!(p.headers.user_agent.as_deref(), Some(cfg.player.user_agent.as_str()));

    let p = resolve(&r, &cfg, &catchup(ids[1], T20, T20 + HOUR), false, now).expect("flussonic");
    assert_eq!(url_of(&p), "http://h/ch/timeshift_rel-7200.m3u8");

    let media = MediaRef::Catchup { channel: ids[2], start: T20, stop: T20 + HOUR, catchup_id: Some("abc".into()) };
    let p = resolve(&r, &cfg, &media, false, now).expect("default template");
    assert_eq!(url_of(&p), "http://arch/abc?s=1791576000&d=3600");
    assert_eq!(p.media, media);

    let p = resolve(&r, &cfg, &catchup(ids[3], T20, T20 + HOUR), false, now).expect("xc");
    assert_eq!(url_of(&p), "http://h:80/timeshift/u/p/60/2026-10-09:21-00/42.ts", "M3U catch-up honours the source tz override");
}

#[test]
fn catchup_subtitle_is_the_archived_programme() {
    let s = store("resolve-cu-epg");
    let (_, ch) = xtream_catchup_channel(&s, "http://panel.example", london(), None, 0);
    w(&s, move |c| seed::seed_epg(c, &[ch], T20 - 2 * HOUR, 6).map(|_| ()));
    let p = resolve(&reader(&s), &Config::default(), &catchup(ch, T20, T20 + 1800), false, T20 + 3 * HOUR).expect("catch-up");
    // 20:00 is the 5th 30-minute slot after 18:00 → "Show 4 on 0"
    assert_eq!(p.subtitle.as_deref(), Some("Show 4 on 0"));
    let live = resolve(&reader(&s), &Config::default(), &MediaRef::Channel(ch), false, T20 + 3 * HOUR + 60).expect("live");
    assert_eq!(live.subtitle.as_deref(), Some("Show 10 on 0"));
}

/// WP-05a gap: the Xc flavour cannot parse an M3U live URL with a path before `live/`
/// (`catchup::XC_RE`). Enable once `sources::catchup` accepts a path prefix.
#[test]
#[ignore = "needs WP-05a: Xc catch-up with a path before live/"]
fn m3u_xc_catchup_with_a_path_prefix() {
    let s = store("resolve-m3u-xc-path");
    let src = m3u_source(&s, london());
    let mut xc = chan("m3u:xcp", "XcPath", url_stream("http://h/panel/live/u/p/42.ts"));
    xc.catchup = Some(spec(CatchupKind::Xc, None, 2));
    let ch = import_channels(&s, src, vec![xc])[0];
    let p = resolve(&reader(&s), &Config::default(), &catchup(ch, T20, T20 + HOUR), false, T20 + 2 * HOUR).expect("xc with path");
    assert_eq!(url_of(&p), "http://h/panel/timeshift/u/p/60/2026-10-09:21-00/42.ts");
}

// ---------------------------------------------------------------------------------------------
// ad-hoc URLs and files

#[test]
fn ad_hoc_urls_with_pipe_headers() {
    let s = store("resolve-url");
    let r = reader(&s);
    let mut cfg = Config::default();
    cfg.player.user_agent = "ConfigUA".into();
    let media = MediaRef::Url("http://cdn.example/path/My%20Stream.m3u8?token=t0k|User-Agent=Kodi%2F21&Referer=http://r/".into());
    let p = resolve(&r, &cfg, &media, true, T20).expect("url");
    assert_eq!(url_of(&p), "http://cdn.example/path/My%20Stream.m3u8?token=t0k");
    assert_eq!(p.headers.user_agent.as_deref(), Some("Kodi/21"));
    assert_eq!(p.headers.referrer.as_deref(), Some("http://r/"));
    assert_eq!(p.title, "My Stream.m3u8");
    assert_eq!(p.kind, PlayableKind::Url);
    assert!(p.kind.is_seekable());
    assert_eq!((p.item.as_ref(), p.source_id, p.start_at), (None, None, None));
    assert_eq!(p.media, media);

    let p = resolve(&r, &cfg, &MediaRef::Url(" https://u:secretpw@live.example/ ".into()), true, T20).expect("bare host");
    assert_eq!(p.title, "live.example", "never a title from userinfo");
    assert_eq!(p.headers.user_agent.as_deref(), Some("ConfigUA"));

    assert_eq!(not_found(resolve(&r, &cfg, &MediaRef::Url("   ".into()), true, T20)), "empty stream address");
}

#[test]
fn local_files_play_by_path_or_file_url() {
    let s = store("resolve-file");
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("clip one.mp4");
    std::fs::write(&file, b"not really a video").expect("write");
    let piped = dir.path().join("odd|name.mkv");
    std::fs::write(&piped, b"x").expect("write");
    let r = reader(&s);
    let cfg = Config::default();

    let p = resolve(&r, &cfg, &MediaRef::Url(file.to_string_lossy().into_owned()), true, T20).expect("path");
    assert_eq!(url_of(&p), file.to_string_lossy());
    assert_eq!(p.title, "clip one.mp4");
    assert_eq!(p.kind, PlayableKind::Url);
    assert!(p.headers.is_empty(), "no HTTP headers for local files");

    let file_url = url::Url::from_file_path(&file).expect("file url").to_string();
    let p = resolve(&r, &cfg, &MediaRef::Url(file_url.clone()), true, T20).expect("file url");
    assert_eq!((url_of(&p), p.title.as_str()), (file_url.as_str(), "clip one.mp4"));

    let p = resolve(&r, &cfg, &MediaRef::Url(piped.to_string_lossy().into_owned()), true, T20).expect("pipe in name");
    assert_eq!(p.title, "odd|name.mkv");

    let missing = dir.path().join("missing.mp4");
    assert_eq!(not_found(resolve(&r, &cfg, &MediaRef::Url(missing.to_string_lossy().into_owned()), true, T20)), "file not found");
}

// ---------------------------------------------------------------------------------------------
// secrets and cost

#[test]
fn errors_and_debug_never_carry_credentials() {
    let s = store("resolve-secrets");
    let pw = "pw-secret-3e9b77";
    let (_, ch) = {
        let src = xtream_source(&s, "http://panel.example", pw, SourceSettings::default(), None);
        let mut c = chan("xt:live:8", "Eight", xt_live(8));
        c.catchup = Some(spec(CatchupKind::Xc, None, 1));
        (src, import_channels(&s, src, vec![c])[0])
    };
    let r = reader(&s);
    let cfg = Config::default();
    let ok = resolve(&r, &cfg, &MediaRef::Channel(ch), false, T20).expect("live");
    assert!(ok.url.expose().contains(pw), "the URL itself carries the credentials");
    assert!(!format!("{ok:?}").contains(pw));
    assert!(!ok.url.redacted().contains(pw));
    for media in [catchup(ch, T20 - 5 * 86_400, T20 - 5 * 86_400 + HOUR), catchup(ch, T20 + HOUR, T20 + 2 * HOUR)] {
        let e = resolve(&r, &cfg, &media, false, T20).expect_err("outside window");
        assert!(!e.to_string().contains(pw) && !format!("{e:?}").contains(pw));
    }
}

#[test]
fn resolve_is_a_handful_of_point_queries() {
    let s = store("resolve-cost");
    let ids = w(&s, |c| {
        let src = seed::seed_source(c, "Big")?;
        let ids = seed::seed_channels(c, src, 5_000)?;
        seed::seed_epg(c, &ids[..500], T20 - 6 * HOUR, 12)?;
        Ok(ids)
    });
    let r = reader(&s);
    let cfg = Config::default();
    let n = 400;
    let started = std::time::Instant::now();
    for i in 0..n {
        let ch = ids[(i * 37) % 500];
        resolve(&r, &cfg, &MediaRef::Channel(ch), false, T20).expect("resolve");
    }
    let per = started.elapsed() / n as u32;
    // Target < 2 ms (DESIGN §5.3); generous bound so a loaded CI box does not flake.
    assert!(per < std::time::Duration::from_millis(20), "resolve took {per:?} per call");
}
