//! Store: sources, catalogue imports (upsert / sweep / reattach), user data, live & VOD reads,
//! library load, source deletion. Owner: WP-02.

mod support;

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use support::seed;
use tuner::model::*;
use tuner::redact::Secret;
use tuner::sources::identity::{group_key, normalize_name, url_key};
use tuner::sources::smart_names::clean_channel_name;
use tuner::store::import::{self, ImportChannel, ImportGroup, ImportMovie, ImportSeries, ImportSummary, NameQuality};
use tuner::store::{Store, epg, library, live, sources, user, vod};

const NOW: Timestamp = 1_791_576_000;

fn store(name: &str) -> Store {
    Store::open_temp(name, None).expect("temp store")
}

/// Run `f` on the writer connection.
fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn m3u_draft(name: &str, url: &str) -> SourceDraft {
    SourceDraft {
        name: name.into(),
        kind: SourceKind::M3uUrl,
        icon: "x".into(),
        color: SourceColor::Blue,
        url: Some(url.into()),
        path: None,
        xtream: None,
        settings: SourceSettings::default(),
    }
}

fn chan(key: &str, name: &str, tvg: Option<&str>, grp: &str, url: &str, order: i32) -> ImportChannel {
    ImportChannel {
        adult: false,
        key: ItemKey(key.into()),
        provider_id: key.strip_prefix("xt:live:").map(str::to_owned),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        number: Some(order as u32 + 1),
        logo: None,
        tvg_id: tvg.map(str::to_owned),
        tvg_name: None,
        tvg_shift_min: 0,
        stream: StreamLocator::Url { url: url.into() },
        url_key: url_key(url),
        headers: None,
        catchup: None,
        radio: false,
        unsupported: None,
        provider_order: order,
        added_at: None,
        groups: vec![group_key(ItemKind::Channel, grp)],
        quality: None,
    }
}

fn movie(key: &str, name: &str, grp: &str, order: i32) -> ImportMovie {
    let url = format!("http://h.tv/movie/{key}.mp4");
    ImportMovie {
        adult: false,
        key: ItemKey(key.into()),
        provider_id: key.strip_prefix("xt:vod:").map(str::to_owned),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: Some(2001),
        poster: Some(format!("http://img/{key}.jpg")),
        rating: Some(7.5),
        url_key: url_key(&url),
        stream: StreamLocator::Url { url },
        provider_order: order,
        added_at: Some(1_700_000_000 + order as i64),
        groups: vec![group_key(ItemKind::Movie, grp)],
        tmdb: None,
    }
}

fn series(key: &str, name: &str, grp: &str, order: i32) -> ImportSeries {
    ImportSeries {
        adult: false,
        key: ItemKey(key.into()),
        provider_id: key.strip_prefix("xt:series:").map(str::to_owned),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: None,
        cover: None,
        backdrop: None,
        rating: None,
        plot: Some("plot".into()),
        genre: None,
        cast: None,
        director: None,
        release_date: None,
        last_modified: Some(100),
        provider_order: order,
        added_at: None,
        groups: vec![group_key(ItemKind::Series, grp)],
        tmdb: None,
    }
}

/// One complete import (groups derived from the items), batches of 2 000.
fn run_import(
    s: &Store,
    src: SourceId,
    channels: Vec<ImportChannel>,
    movies: Vec<ImportMovie>,
    series: Vec<ImportSeries>,
    completed: Vec<ItemKind>,
    now: Timestamp,
) -> ImportSummary {
    w(s, move |c| {
        let imp = import::begin_catalogue(c, src, now)?;
        let mut groups: Vec<ImportGroup> = Vec::new();
        let mut add = |kind, keys: &[GroupKey]| {
            for k in keys {
                if !groups.iter().any(|g| g.kind == kind && &g.key == k) {
                    let name = k.0.split_once(':').unwrap().1.to_owned();
                    groups.push(ImportGroup {
                        adult: false,
                        kind,
                        key: k.clone(),
                        name,
                        raw_name: None,
                        provider_id: None,
                        provider_order: groups.len() as i32,
                    });
                }
            }
        };
        channels.iter().for_each(|x| add(ItemKind::Channel, &x.groups));
        movies.iter().for_each(|x| add(ItemKind::Movie, &x.groups));
        series.iter().for_each(|x| add(ItemKind::Series, &x.groups));
        import::upsert_groups(c, &imp, &groups)?;
        for b in channels.chunks(2000) {
            import::write_channels(c, &imp, b)?;
        }
        for b in movies.chunks(2000) {
            import::write_movies(c, &imp, b)?;
        }
        for b in series.chunks(2000) {
            import::write_series(c, &imp, b)?;
        }
        import::finish_catalogue(c, &imp, &completed, now)
    })
}

const ALL: [ItemKind; 3] = [ItemKind::Channel, ItemKind::Movie, ItemKind::Series];

fn ch_ref(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(key.into()) }
}

fn ids(c: &Connection, table: &str) -> Vec<(String, i64)> {
    let mut st = c.prepare(&format!("SELECT key, id FROM {table} ORDER BY key")).unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

// ------------------------------------------------------------------ sources

#[test]
fn sources_crud_order_status_and_secrets() {
    let s = store("tuner-st-src");
    let secret = "pw-sources-crud-7731";
    let (a, b) = w(&s, move |c| {
        let a = sources::insert_source(c, &m3u_draft("A", &format!("http://h.tv/get.php?username=usr-crud-5512&password={secret}")), NOW)?;
        let mut d = m3u_draft("B", "http://x");
        d.kind = SourceKind::Xtream;
        d.url = None;
        d.xtream = Some(XtreamCredentials { server: "http://xt.tv".into(), username: "alice".into(), password: Secret::new("xt-pass-9931".into()) });
        let b = sources::insert_source(c, &d, NOW)?;
        Ok((a, b))
    });
    // secrets registered on insert
    assert!(!tuner::redact::redact(&format!("oops {secret}")).contains(secret));
    assert!(!tuner::redact::redact("xt-pass-9931").contains("xt-pass-9931"));

    let r = s.reader().unwrap();
    let list = sources::list_sources(&r).unwrap();
    assert_eq!(list.iter().map(|x| x.id).collect::<Vec<_>>(), vec![a, b]);
    let xb = sources::get_source(&r, b).unwrap();
    assert_eq!(xb.kind, SourceKind::Xtream);
    assert_eq!(xb.xtream.as_ref().unwrap().password.expose(), "xt-pass-9931");
    assert_eq!(xb.color, SourceColor::Blue);
    assert!(matches!(sources::get_source(&r, SourceId(999)), Err(tuner::error::Error::NotFound(_))));

    w(&s, move |c| {
        sources::reorder_sources(c, &[b, a])?;
        let mut src = sources::get_source(c, a)?;
        src.name = "A2".into();
        src.enabled = false;
        sources::update_source(c, &src)?;
        sources::set_source_settings(c, b, &SourceSettings { refresh_h: Some(2), ..Default::default() })?;
        sources::mark_attempt(c, b, NOW)?;
        sources::mark_failed(c, b, "boom xt-pass-9931", NOW)?;
        sources::set_account(c, b, &AccountInfo { max_connections: Some(2), ..Default::default() })?;
        Ok(())
    });
    let list = sources::list_sources(&r).unwrap();
    assert_eq!(list.iter().map(|x| x.id).collect::<Vec<_>>(), vec![b, a]);
    assert_eq!(list[1].name, "A2");
    assert!(!list[1].enabled);
    let xb = &list[0];
    assert_eq!(xb.settings.refresh_h, Some(2));
    assert!(!xb.status.last_error.as_deref().unwrap().contains("xt-pass-9931"), "stored errors are redacted");
    assert_eq!(sources::account_limits(&r).unwrap().into_iter().find(|x| x.0 == b).unwrap().1, Some(2));

    // staleness: B (refresh_h 2) never refreshed → stale once the back-off of its failure just
    // now is over; A disabled → never
    assert!(sources::stale_sources(&r, NOW, 24).unwrap().is_empty());
    assert_eq!(sources::stale_sources(&r, NOW + 600, 24).unwrap(), vec![b]);
    w(&s, move |c| {
        let st = SourceStatus { last_refresh: Some(NOW - 3600), counts: SourceCounts { channels: 3, ..Default::default() }, ..Default::default() };
        sources::mark_refreshed(c, b, &st)
    });
    assert!(sources::stale_sources(&r, NOW, 24).unwrap().is_empty());
    assert_eq!(sources::stale_sources(&r, NOW + 3601, 24).unwrap(), vec![b]);
    let xb = sources::get_source(&r, b).unwrap();
    assert_eq!(xb.status.last_error, None);
    assert_eq!(xb.status.counts.channels, 3);
    assert_eq!(xb.status.account.unwrap().max_connections, Some(2), "account survives mark_refreshed");
}

#[test]
fn epg_sources_provider_lifecycle_priority_and_staleness() {
    let s = store("tuner-st-epgsrc");
    let (src, user_id, f) = w(&s, |c| {
        let src = sources::insert_source(c, &m3u_draft("P", "http://h/p.m3u"), NOW)?;
        let user_id = sources::insert_epg_source(c, "User", &EpgSourceKind::Url { url: Secret::new("http://e/u.xml".into()) }, 30)?;
        let f = sources::insert_epg_source(c, "File", &EpgSourceKind::File { path: "/tmp/g.xml".into() }, 0)?;
        Ok((src, user_id, f))
    });
    let urls = vec!["http://h/a.xml".to_string(), "http://h/b.xml".to_string(), "http://h/a.xml".to_string()];
    let ids1 = w(&s, move |c| sources::ensure_provider_epg(c, src, &urls));
    assert_eq!(ids1.len(), 2, "deduplicated");
    let r = s.reader().unwrap();
    let list = sources::list_epg_sources(&r).unwrap();
    // provider guides first (priority 0), user guides after in insertion order
    assert_eq!(list.iter().map(|e| e.id).collect::<Vec<_>>(), vec![ids1[0], ids1[1], user_id, f]);
    assert!(matches!(&list[0].kind, EpgSourceKind::Provider { source_id, .. } if *source_id == src));
    assert_eq!(list[2].offset_min, 30);

    // re-advertising keeps ids; dropping a URL removes its source
    let urls = vec!["http://h/b.xml".to_string()];
    let ids2 = w(&s, move |c| sources::ensure_provider_epg(c, src, &urls));
    assert_eq!(ids2, vec![ids1[1]]);
    assert!(sources::get_epg_source(&r, ids1[0]).is_err());

    // reorder + update + failure
    let order = vec![f, user_id, ids2[0]];
    w(&s, move |c| {
        sources::reorder_epg_sources(c, &order)?;
        let mut e = sources::get_epg_source(c, user_id)?;
        e.enabled = false;
        e.refresh_h = Some(6);
        sources::update_epg_source(c, &e)?;
        sources::mark_epg_failed(c, f, "nope", NOW)
    });
    let list = sources::list_epg_sources(&r).unwrap();
    assert_eq!(list.iter().map(|e| e.id).collect::<Vec<_>>(), vec![f, user_id, ids2[0]]);
    assert!(!list[1].enabled && list[1].refresh_h == Some(6));
    assert_eq!(list[0].last_error.as_deref(), Some("nope"));

    // staleness: never refreshed → stale (F never imported and failed just now: retried after an
    // hour, not a whole interval); disabled never; max_stop rule
    assert_eq!(sources::stale_epg_sources(&r, NOW, 12).unwrap(), vec![ids2[0]]);
    assert_eq!(sources::stale_epg_sources(&r, NOW + 3600 - 1, 12).unwrap(), vec![ids2[0]]);
    assert_eq!(sources::stale_epg_sources(&r, NOW + 3600, 12).unwrap(), vec![f, ids2[0]]);
    let p = ids2[0];
    w(&s, move |c| {
        c.execute("UPDATE epg_source SET last_refresh = ?2, max_stop = ?3 WHERE id = ?1", rusqlite::params![p, NOW - 7200, NOW + 3 * 86400])?;
        let ok = "UPDATE epg_source SET last_refresh = ?2, max_stop = ?3, last_error = NULL WHERE id = ?1";
        c.execute(ok, rusqlite::params![f, NOW - 7200, NOW + 3600])?;
        Ok(())
    });
    assert_eq!(sources::stale_epg_sources(&r, NOW, 12).unwrap(), vec![f], "guide running out");
    assert_eq!(sources::stale_epg_sources(&r, NOW + 11 * 3600, 12).unwrap(), vec![f, p]);
    // a running-out guide that fails is not fetched again on every tick: it has been due for an
    // hour (since NOW - 1 h), so it waits an hour
    w(&s, move |c| sources::mark_epg_failed(c, f, "down", NOW));
    assert!(sources::stale_epg_sources(&r, NOW + 3540, 12).unwrap().is_empty());
    assert_eq!(sources::stale_epg_sources(&r, NOW + 3600, 12).unwrap(), vec![f]);
}

/// Audit fix: a provider that is down was retried (WARN + error toast) on every 15-minute
/// scheduler tick forever. Failed refreshes now back off: the wait doubles with how long the
/// source has been failing, from 10 min up to its interval; a success resets it.
#[test]
fn failing_refreshes_back_off_up_to_the_interval() {
    let s = store("tuner-st-backoff");
    let id = w(&s, |c| {
        let id = sources::insert_source(c, &m3u_draft("Down", "http://h/p.m3u"), NOW - 86_400)?;
        sources::set_source_settings(c, id, &SourceSettings { refresh_h: Some(4), ..Default::default() })?;
        let st = SourceStatus { last_refresh: Some(NOW - 4 * 3600), ..Default::default() };
        sources::mark_refreshed(c, id, &st)?;
        sources::mark_failed(c, id, "HTTP 503", NOW)?;
        Ok(id)
    });
    let r = s.reader().unwrap();
    let stale = |at: Timestamp| sources::stale_sources(&r, at, 24).unwrap() == vec![id];
    // due since NOW, failed at once: the shortest wait
    assert!(!stale(NOW + 599) && stale(NOW + 600));
    // failed again after an hour of failing: waits an hour
    w(&s, move |c| sources::mark_failed(c, id, "HTTP 503", NOW + 3600));
    assert!(!stale(NOW + 2 * 3600 - 1) && stale(NOW + 2 * 3600));
    // failing for 10 h: capped at the 4 h interval
    w(&s, move |c| sources::mark_failed(c, id, "HTTP 503", NOW + 10 * 3600));
    assert!(!stale(NOW + 14 * 3600 - 1) && stale(NOW + 14 * 3600));
    // a success resets the back-off: the next failure is retried soon again
    w(&s, move |c| {
        sources::mark_refreshed(c, id, &SourceStatus { last_refresh: Some(NOW + 14 * 3600), ..Default::default() })?;
        sources::mark_failed(c, id, "HTTP 503", NOW + 18 * 3600)
    });
    assert!(stale(NOW + 18 * 3600 + 600));
}

// ------------------------------------------------------------------ imports

fn sample(n: usize) -> (Vec<ImportChannel>, Vec<ImportMovie>, Vec<ImportSeries>) {
    let ch = (0..n)
        .map(|i| {
            chan(
                &format!("xt:live:{i}"),
                &format!("Chan {i}"),
                Some(&format!("c{i}.tv")),
                if i % 2 == 0 { "News" } else { "Sport" },
                &format!("http://h.tv/live/u/p/{i}.ts"),
                i as i32,
            )
        })
        .collect();
    let mv = (0..10).map(|i| movie(&format!("xt:vod:{i}"), &format!("Movie {i}"), "Films", i)).collect();
    let se = (0..5).map(|i| series(&format!("xt:series:{i}"), &format!("Show {i}"), "Shows", i)).collect();
    (ch, mv, se)
}

#[test]
fn reimport_of_identical_data_keeps_row_ids() {
    let s = store("tuner-st-reimport");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    let (ch, mv, se) = sample(4500);
    let first = run_import(&s, src, ch.clone(), mv.clone(), se.clone(), ALL.to_vec(), NOW);
    assert_eq!(first.channels, import::KindCounts { added: 4500, updated: 0, removed: 0, total: 4500 });
    assert_eq!(first.movies.added, 10);
    assert_eq!(first.groups.added, 4);
    let r = s.reader().unwrap();
    let before = (ids(&r, "channel"), ids(&r, "movie"), ids(&r, "series"), ids(&r, "grp"));
    let second = run_import(&s, src, ch, mv, se, ALL.to_vec(), NOW + 60);
    assert_eq!(second.channels, import::KindCounts { added: 0, updated: 4500, removed: 0, total: 4500 });
    assert_eq!(second.groups, import::KindCounts { added: 0, updated: 4, removed: 0, total: 4 });
    let after = (ids(&r, "channel"), ids(&r, "movie"), ids(&r, "series"), ids(&r, "grp"));
    assert_eq!(before, after);
    let g: i64 = r.query_row("SELECT generation FROM source WHERE id = ?1", [src], |x| x.get(0)).unwrap();
    assert_eq!(g, 2);
    assert_eq!(count(&r, "SELECT n_channels FROM source"), 4500);
    assert_eq!(count(&r, "SELECT item_count FROM grp WHERE key = 'channel:news'"), 2250);
    assert_eq!(count(&r, "SELECT count(*) FROM meta WHERE key LIKE 'import:%'"), 0, "counters cleaned up");
    // added_at is kept for M3U-style items without a provider date
    assert_eq!(count(&r, "SELECT count(*) FROM channel WHERE added_at = 1791576000"), 4500);
    let fk = count(&r, "SELECT count(*) FROM pragma_foreign_key_check");
    assert_eq!(fk, 0);
}

#[test]
fn vanished_items_are_swept_only_for_completed_kinds() {
    let s = store("tuner-st-sweep");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    let (ch, mv, se) = sample(20);
    run_import(&s, src, ch.clone(), mv.clone(), se.clone(), ALL.to_vec(), NOW);
    // live completed with 10 channels; movies failed half-way (5 written, not completed)
    let s2 = run_import(&s, src, ch[..10].to_vec(), mv[..5].to_vec(), se.clone(), vec![ItemKind::Channel, ItemKind::Series], NOW);
    assert_eq!(s2.channels.removed, 10);
    assert_eq!(s2.channels.total, 10);
    assert_eq!(s2.movies.removed, 0);
    assert_eq!(s2.movies.total, 10, "incomplete kind is never swept");
    let r = s.reader().unwrap();
    assert_eq!(count(&r, "SELECT count(*) FROM channel"), 10);
    assert_eq!(count(&r, "SELECT count(*) FROM movie"), 10);
    // a group whose channels all vanished disappears with its kind's sweep
    let only_news: Vec<_> = ch[..10].iter().filter(|c| c.groups[0].0 == "channel:news").cloned().collect();
    let s3 = run_import(&s, src, only_news, mv.clone(), se, ALL.to_vec(), NOW);
    assert_eq!(s3.groups.removed, 1);
    assert_eq!(count(&r, "SELECT count(*) FROM grp WHERE kind = 'channel'"), 1);
    assert_eq!(count(&r, "SELECT count(*) FROM channel_group"), 5);
}

#[test]
fn item_group_missing_from_upsert_groups_survives_the_sweep() {
    let s = store("tuner-st-stale-group");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    run_import(&s, src, vec![chan("m3u:a", "A", None, "News", "http://h.tv/a.ts", 0)], vec![], vec![], ALL.to_vec(), NOW);
    let r = s.reader().unwrap();
    let (gid, name): (i64, String) = r.query_row("SELECT id, name FROM grp WHERE kind = 'channel'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    // second import: the group list omits "News" but the channel still names it
    let a = chan("m3u:a", "A", None, "News", "http://h.tv/a.ts", 0);
    w(&s, move |c| {
        let imp = import::begin_catalogue(c, src, NOW)?;
        import::upsert_groups(c, &imp, &[])?;
        import::write_channels(c, &imp, &[a])?;
        import::finish_catalogue(c, &imp, &ALL, NOW)
    });
    assert_eq!(count(&r, "SELECT count(*) FROM channel_group"), 1, "membership kept");
    let (gid2, name2): (i64, String) = r.query_row("SELECT id, name FROM grp WHERE kind = 'channel'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((gid2, name2), (gid, name), "same group row, name kept");
}

#[test]
fn cancelled_import_neither_sweeps_nor_bumps_generation() {
    let s = store("tuner-st-cancel");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    let (ch, mv, se) = sample(10);
    run_import(&s, src, ch.clone(), mv, se, ALL.to_vec(), NOW);
    let part = ch[..3].to_vec();
    w(&s, move |c| {
        let imp = import::begin_catalogue(c, src, NOW)?;
        assert_eq!(imp.generation, 2);
        import::write_channels(c, &imp, &part)?;
        Ok(()) // cancelled: no finish
    });
    let r = s.reader().unwrap();
    assert_eq!(count(&r, "SELECT count(*) FROM channel"), 10);
    assert_eq!(count(&r, "SELECT generation FROM source"), 1);
    // the next run starts from the same generation and still sweeps correctly
    let sum = run_import(&s, src, ch[..4].to_vec(), vec![], vec![], vec![ItemKind::Channel], NOW);
    assert_eq!(sum.channels.removed, 6);
    assert_eq!(sum.channels.added, 0);
    assert_eq!(count(&r, "SELECT generation FROM source"), 2);
}

// ------------------------------------------------------------------ reattach (§8.3)

/// Attach every kind of user data to `item`.
fn attach_all(c: &mut Connection, item: &ItemRef, epg_src: EpgSourceId) -> tuner::error::Result<()> {
    let list = user::default_fav_list(c)?;
    user::set_favourite(c, list, item, true, NOW)?;
    user::set_item_override(c, item, &ItemOverride { rename: Some("Mine".into()), ..Default::default() })?;
    user::save_progress(
        c,
        &WatchProgress {
            item: item.clone(),
            position_s: 12.0,
            duration_s: None,
            updated_at: NOW,
            finished: false,
            title: "t".into(),
            fingerprint: Fingerprint::default(),
        },
    )?;
    if item.kind == ItemKind::Channel {
        user::add_reminder(
            c,
            &Reminder {
                id: ReminderId(0),
                channel: item.clone(),
                channel_name: "c".into(),
                programme_title: "p".into(),
                start: NOW + 600,
                stop: NOW + 1200,
                lead_s: 120,
                state: ReminderState::Pending,
            },
        )?;
        epg::set_manual_mapping(c, item, epg_src, "manual.tv")?;
    }
    Ok(())
}

/// Keys of `kind` holding user data in each table.
fn holders(c: &Connection, item: &ItemRef) -> Vec<&'static str> {
    let mut out = Vec::new();
    let k = item.key.as_str();
    let q = |sql: &str| -> i64 { c.query_row(sql, rusqlite::params![item.source, k], |r| r.get(0)).unwrap() };
    if q("SELECT count(*) FROM fav_item WHERE source_id = ?1 AND item_key = ?2 AND orphaned_since IS NULL") > 0 {
        out.push("favourite");
    }
    if q("SELECT count(*) FROM item_override WHERE source_id = ?1 AND item_key = ?2 AND orphaned_since IS NULL") > 0 {
        out.push("override");
    }
    if q("SELECT count(*) FROM watch_progress WHERE source_id = ?1 AND item_key = ?2 AND orphaned_since IS NULL") > 0 {
        out.push("progress");
    }
    if q("SELECT count(*) FROM reminder WHERE source_id = ?1 AND channel_key = ?2 AND orphaned_since IS NULL") > 0 {
        out.push("reminder");
    }
    if q("SELECT count(*) FROM channel_epg_map WHERE source_id = ?1 AND channel_key = ?2 AND method = 'manual' AND orphaned_since IS NULL") > 0 {
        out.push("epg");
    }
    out
}

fn reattach_setup(name: &str) -> (Store, SourceId, EpgSourceId) {
    let s = store(name);
    let (src, e) = w(&s, |c| {
        let src = sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW)?;
        let e = sources::insert_epg_source(c, "G", &EpgSourceKind::Url { url: Secret::new("http://e/g.xml".into()) }, 0)?;
        Ok((src, e))
    });
    (s, src, e)
}

#[test]
fn reattach_after_xtream_renumbering_moves_every_kind_of_user_data() {
    let (s, src, e) = reattach_setup("tuner-st-renumber");
    let v1 = vec![
        chan("xt:live:1", "BBC One HD", Some("bbc1.uk"), "UK", "http://h/live/u/p/1.ts", 0),
        chan("xt:live:2", "ITV", Some("itv.uk"), "UK", "http://h/live/u/p/2.ts", 1),
    ];
    run_import(&s, src, v1, vec![], vec![], vec![ItemKind::Channel], NOW);
    let old = ch_ref(src, "xt:live:1");
    let o2 = old.clone();
    w(&s, move |c| attach_all(c, &o2, e));
    // provider renumbered every stream and renamed the URLs
    let v2 = vec![
        chan("xt:live:901", "BBC One HD", Some("bbc1.uk"), "UK", "http://h/live/u/p/901.ts", 0),
        chan("xt:live:902", "ITV", Some("itv.uk"), "UK", "http://h/live/u/p/902.ts", 1),
    ];
    let sum = run_import(&s, src, v2, vec![], vec![], vec![ItemKind::Channel], NOW + 10);
    assert_eq!(sum.reattached, 1);
    assert_eq!(sum.orphaned, 0);
    let r = s.reader().unwrap();
    let new = ch_ref(src, "xt:live:901");
    assert_eq!(holders(&r, &new), vec!["favourite", "override", "progress", "reminder", "epg"]);
    assert!(holders(&r, &old).is_empty());
    // the fingerprint now describes the new item
    let fav = user::fav_items(&r, user::default_fav_list(&r).unwrap()).unwrap();
    assert_eq!(fav[0].fingerprint.provider_id.as_deref(), Some("901"));
    assert!(user::orphans(&r).unwrap().is_empty());
}

#[test]
fn reattach_steps_renamed_group_url_and_ambiguous_names() {
    let (s, src, e) = reattach_setup("tuner-st-steps");
    let v1 = vec![
        chan("m3u:a", "Discovery", None, "Docs", "http://cdn/disc/index.m3u8?token=1", 0),
        chan("m3u:b", "Fox", None, "Movies", "http://cdn/fox/index.m3u8?token=1", 1),
        chan("m3u:c", "Euro News", None, "News", "http://cdn/en/1.ts", 2),
    ];
    run_import(&s, src, v1, vec![], vec![], vec![ItemKind::Channel], NOW);
    for k in ["m3u:a", "m3u:b", "m3u:c"] {
        let item = ch_ref(src, k);
        w(&s, move |c| attach_all(c, &item, e));
    }
    let v2 = vec![
        // a: group renamed + new key; same URL path (token rotated) → url step
        chan("m3u:a2", "Discovery", None, "Documentaries", "http://cdn/disc/index.m3u8?token=2", 0),
        // b: now duplicated by name in two groups, both new URLs → ambiguous → orphan
        chan("m3u:b2", "Fox", None, "Films", "http://cdn2/fox1.ts", 1),
        chan("m3u:b3", "Fox", None, "Cinema", "http://cdn2/fox2.ts", 2),
        // c: same name, same group, new key + URL → name+group
        chan("m3u:c2", "Euro News", None, "News", "http://cdn/en/2.ts", 3),
    ];
    let sum = run_import(&s, src, v2, vec![], vec![], vec![ItemKind::Channel], NOW + 10);
    assert_eq!(sum.reattached, 2);
    assert_eq!(sum.orphaned, 1);
    let r = s.reader().unwrap();
    assert_eq!(holders(&r, &ch_ref(src, "m3u:a2")).len(), 5);
    assert_eq!(holders(&r, &ch_ref(src, "m3u:c2")).len(), 5);
    assert!(holders(&r, &ch_ref(src, "m3u:b2")).is_empty(), "never a wrong match");
    assert!(holders(&r, &ch_ref(src, "m3u:b3")).is_empty());
    let orphans = user::orphans(&r).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].item, ch_ref(src, "m3u:b"));
    assert_eq!(orphans[0].since, NOW + 10);
    assert_eq!(orphans[0].holders.len(), 5);
    assert_eq!(orphans[0].fingerprint.norm_name, "fox");
    // the orphan's FavIndex entry is skipped; its manual EPG mapping does not resolve
    let favs = library::load_favs(&r).unwrap();
    assert_eq!(favs.items(favs.default_list().unwrap()).len(), 2);

    // outcomes reported per vanished key
    let out = w(&s, move |c| import::reattach_user_data(c, src, NOW + 20));
    assert_eq!(out, vec![(ItemKey("m3u:b".into()), ReattachOutcome::Orphaned)]);
    // a second pass does not move the orphan date
    assert_eq!(user::orphans(&r).unwrap()[0].since, NOW + 10);

    // manual "Attach to…" moves everything and resolves the mapping
    let (from, to) = (ch_ref(src, "m3u:b"), ch_ref(src, "m3u:b3"));
    w(&s, move |c| user::reassign(c, &from, &to));
    assert_eq!(holders(&r, &ch_ref(src, "m3u:b3")).len(), 5);
    assert!(user::orphans(&r).unwrap().is_empty());
}

/// Quality badges (DESIGN §6): the name's quality word is stored at import; a measured size
/// beats it in the library, survives a refresh, follows the channel to a new key (reattach
/// steps) and is dropped with a channel that is gone — never listed as an orphan.
#[test]
fn measured_quality_beats_the_name_and_follows_the_channel() {
    let (s, src, _) = reattach_setup("tuner-st-quality");
    let named = |key: &str, name: &str, url: &str, order| ImportChannel {
        quality: NameQuality::of(&clean_channel_name(name)),
        ..chan(key, name, None, "UK", url, order)
    };
    let v1 = vec![
        named("m3u:a", "BBC One 4K", "http://cdn/bbc4k.ts", 0),
        named("m3u:b", "BBC One HD", "http://cdn/bbchd.ts", 1),
        named("m3u:c", "ITV", "http://cdn/itv.ts", 2),
    ];
    run_import(&s, src, v1.clone(), vec![], vec![], vec![ItemKind::Channel], NOW);
    let r = s.reader().unwrap();
    let quality = || -> HashMap<String, Option<Quality>> {
        library::load(&r).unwrap().live.channels.iter().map(|c| (c.key.0.clone(), c.quality.map(|b| b.class))).collect()
    };
    assert_eq!(quality(), HashMap::from([("m3u:a".into(), Some(Quality::Uhd)), ("m3u:b".into(), Some(Quality::Hd)), ("m3u:c".into(), None)]));

    // the "4K" one plays at 1080p, the untagged one turns out SD
    w(&s, move |c| {
        live::record_video(c, &ch_ref(src, "m3u:a"), 1920, 1080, NOW)?;
        live::record_video(c, &ch_ref(src, "m3u:c"), 720, 576, NOW)
    });
    let measured = HashMap::from([("m3u:a".into(), Some(Quality::Fhd)), ("m3u:b".into(), Some(Quality::Hd)), ("m3u:c".into(), Some(Quality::Sd))]);
    assert_eq!(quality(), measured);
    run_import(&s, src, v1, vec![], vec![], vec![ItemKind::Channel], NOW + 10);
    assert_eq!(quality(), measured, "a refresh keeps both");

    // re-keyed (same stream path, rotated token): the measurement follows; ITV is gone
    let v2 = vec![named("m3u:a2", "BBC One 4K", "http://cdn/bbc4k.ts?token=2", 0), named("m3u:b", "BBC One HD", "http://cdn/bbchd.ts", 1)];
    run_import(&s, src, v2, vec![], vec![], vec![ItemKind::Channel], NOW + 20);
    assert_eq!(quality(), HashMap::from([("m3u:a2".into(), Some(Quality::Fhd)), ("m3u:b".into(), Some(Quality::Hd))]));
    assert_eq!(count(&r, "SELECT count(*) FROM channel_video"), 1);
    assert!(user::orphans(&r).unwrap().is_empty());
}

/// Quality-label trust (DESIGN §13.0.1) through the store: the evidence is one row per channel
/// (a re-measure replaces it), `library::load` turns it into `4K?` on the unmeasured claims,
/// a re-measure clears the doubt, and a refresh drops the evidence of vanished names
/// (thresholds: `model::live::label_trust_thresholds`). Also: channels of one category that
/// clean to the same name and badge keep what tells them apart (`TV5` / `TV5 · RAW`).
#[test]
fn contradicted_4k_claims_make_the_unmeasured_ones_doubtful() {
    let (s, src, _) = reattach_setup("tuner-st-quality-trust");
    let named = |key: &str, name: &str, order| ImportChannel {
        clean_name: clean_channel_name(name).name,
        quality: NameQuality::of(&clean_channel_name(name)),
        ..chan(key, name, None, "UK", &format!("http://cdn/{key}.ts"), order)
    };
    let chans = vec![
        named("m3u:a", "UK ★ Alpha 4K ◉", 0),
        named("m3u:b", "UK ★ Bravo 4K", 1),
        named("m3u:c", "UK ★ Charlie 4K", 2),
        named("m3u:d", "UK ★ Delta", 3),
        named("m3u:f", "FI: TV5", 4),
        named("m3u:g", "FI: TV5 ᴿᴬᵂ", 5),
    ];
    run_import(&s, src, chans.clone(), vec![], vec![], vec![ItemKind::Channel], NOW);
    let r = s.reader().unwrap();
    let badges = || -> HashMap<String, String> {
        library::load(&r).unwrap().live.channels.iter().filter_map(|c| Some((c.key.0.clone(), c.quality?.label().to_owned()))).collect()
    };
    let measure = |key: &'static str, width: i64, height: i64| w(&s, move |c| live::record_video(c, &ch_ref(src, key), width, height, NOW));
    let names: Vec<String> = library::load(&r).unwrap().live.channels.iter().map(|c| c.name.to_string()).collect();
    assert_eq!(names[4..], ["TV5", "TV5 · RAW"]);

    measure("m3u:a", 1280, 720);
    measure("m3u:a", 1280, 720);
    measure("m3u:b", 1280, 720);
    measure("m3u:d", 720, 576);
    assert_eq!(badges()["m3u:c"], "4K?");
    assert_eq!(count(&r, "SELECT count(*) FROM quality_check"), 3, "one row per channel");
    measure("m3u:a", 3840, 2160);
    measure("m3u:b", 3840, 2160);
    assert_eq!(badges()["m3u:c"], "4K", "re-measured right: no doubt");

    // a refresh without Bravo drops its evidence
    run_import(&s, src, chans.into_iter().filter(|c| c.key.0 != "m3u:b").collect(), vec![], vec![], vec![ItemKind::Channel], NOW + 10);
    assert_eq!(count(&r, "SELECT count(*) FROM quality_check"), 2);
}

/// The quality copies of an M3U channel share a base key and are numbered by playlist position
/// (`KeyAllocator`), so a reordered or grown playlist moves keys between them: a measurement
/// follows its copy (provider name), not the slot; a renamed copy drops it. Copies the identity
/// key tells apart ("2160p") are still one family for best-first ordering.
#[test]
fn measured_quality_follows_the_copy_not_its_slot() {
    use tuner::jobs::import_source::m3u_entry_to_channel;
    use tuner::sources::identity::KeyAllocator;
    use tuner::sources::m3u::M3uEntry;
    let (s, src, _) = reattach_setup("tuner-st-quality-slots");
    let playlist = |entries: &[(&str, &str)]| {
        let mut keys = KeyAllocator::default();
        let entries = entries.iter().enumerate().map(|(i, (name, url))| {
            let e = M3uEntry {
                title: (*name).into(),
                tvg_id: Some("bbc1.uk".into()),
                groups: vec!["UK".into()],
                url: (*url).into(),
                ..Default::default()
            };
            m3u_entry_to_channel(&e, i as i32, false, &mut keys, &mut Vec::new())
        });
        entries.collect::<Vec<_>>()
    };
    let r = s.reader().unwrap();
    // raw name → quality, and the names in the default (best-first) order
    let state = || {
        let lib = library::load(&r).unwrap();
        let by_name: HashMap<String, Option<Quality>> = lib.live.channels.iter().map(|c| (c.name.to_string(), c.quality.map(|b| b.class))).collect();
        let order = lib.live.collection(&Collection::AllChannels, &lib.favs, &[], SortMode::Provider);
        let order: Vec<String> = order.iter().map(|id| lib.live.get(*id).unwrap().name.to_string()).collect();
        (by_name, order)
    };

    let v1 = playlist(&[("BBC One HD", "http://cdn/hd.ts"), ("BBC One 4K", "http://cdn/4k.ts")]);
    assert_eq!(v1[1].key.0, format!("{}#2", v1[0].key.0), "the copies share a base key");
    let (hd, uhd) = (ch_ref(src, &v1[0].key.0), ch_ref(src, &v1[1].key.0));
    run_import(&s, src, v1, vec![], vec![], vec![ItemKind::Channel], NOW);
    // both deliver less than their names say
    w(&s, move |c| {
        live::record_video(c, &hd, 720, 576, NOW)?;
        live::record_video(c, &uhd, 1920, 1080, NOW)
    });

    // the provider puts a new FHD copy first: every key moves to another copy
    let v2 = playlist(&[("BBC One FHD", "http://cdn/fhd.ts"), ("BBC One 4K", "http://cdn/4k.ts"), ("BBC One HD", "http://cdn/hd.ts")]);
    run_import(&s, src, v2, vec![], vec![], vec![ItemKind::Channel], NOW + 10);
    let st = state();
    assert_eq!(st.0["BBC One FHD"], Some(Quality::Fhd), "its name: not the HD copy's measurement");
    assert_eq!(st.0["BBC One 4K"], Some(Quality::Fhd), "measured");
    assert_eq!(st.0["BBC One HD"], Some(Quality::Sd), "measured");
    assert_eq!(st.1, ["BBC One FHD", "BBC One 4K", "BBC One HD"], "best first, equal ones keep their order");

    // the HD copy is renamed: its old measurement goes; a 2160p copy is one of the family
    let v3 = playlist(&[
        ("BBC One FHD", "http://cdn/fhd.ts"),
        ("BBC One 4K", "http://cdn/4k.ts"),
        ("BBC One 1080p", "http://cdn/hd.ts"),
        ("BBC One 2160p", "http://cdn/uhd.ts"),
    ]);
    assert_ne!(v3[3].norm_name, v3[0].norm_name, "identity keeps 2160p in the name");
    run_import(&s, src, v3, vec![], vec![], vec![ItemKind::Channel], NOW + 20);
    let st = state();
    assert_eq!(st.0["BBC One 1080p"], Some(Quality::Fhd), "the new name's word, not the old measurement");
    assert_eq!(st.0["BBC One 4K"], Some(Quality::Fhd), "still measured");
    assert_eq!(st.1[0], "BBC One 2160p");
    assert_eq!(count(&r, "SELECT count(*) FROM channel_video"), 1);
    assert!(user::orphans(&r).unwrap().is_empty());
}

#[test]
fn removed_then_readded_item_is_unorphaned_and_forget_deletes() {
    let (s, src, e) = reattach_setup("tuner-st-readd");
    let v1 = vec![chan("m3u:x", "Alpha", Some("a.tv"), "G", "http://h/a.ts", 0), chan("m3u:y", "Beta", None, "G", "http://h/b.ts", 1)];
    run_import(&s, src, v1.clone(), vec![], vec![], vec![ItemKind::Channel], NOW);
    let (x, y) = (ch_ref(src, "m3u:x"), ch_ref(src, "m3u:y"));
    let (x2, y2) = (x.clone(), y.clone());
    w(&s, move |c| {
        attach_all(c, &x2, e)?;
        attach_all(c, &y2, e)
    });
    let sum = run_import(&s, src, vec![], vec![], vec![], vec![ItemKind::Channel], NOW + 1);
    assert_eq!(sum.orphaned, 2);
    let r = s.reader().unwrap();
    assert_eq!(user::orphans(&r).unwrap().len(), 2);
    // x comes back with its key
    let sum = run_import(&s, src, v1[..1].to_vec(), vec![], vec![], vec![ItemKind::Channel], NOW + 2);
    assert_eq!(sum.orphaned, 0, "y was already an orphan");
    assert_eq!(holders(&r, &x).len(), 5);
    let orphans = user::orphans(&r).unwrap();
    assert_eq!(orphans.iter().map(|o| &o.item).collect::<Vec<_>>(), vec![&y]);
    let y3 = y.clone();
    w(&s, move |c| user::forget_orphan(c, &y3));
    assert!(user::orphans(&r).unwrap().is_empty());
    for t in ["fav_item", "item_override", "watch_progress"] {
        assert_eq!(count(&r, &format!("SELECT count(*) FROM {t} WHERE item_key = 'm3u:y'")), 0, "{t}");
    }
    // the re-added channel's manual EPG mapping is live again
    assert!(live::channel_by_ref(&r, &x).unwrap().is_some());
    let has = count(&r, "SELECT count(*) FROM channel_epg_map WHERE channel_key = 'm3u:x' AND method = 'manual' AND orphaned_since IS NULL");
    assert_eq!(has, 1);
}

#[test]
fn movie_progress_reattaches_by_name_and_group() {
    let (s, src, e) = reattach_setup("tuner-st-movie");
    run_import(&s, src, vec![], vec![movie("m3u:m1", "Heat", "Crime", 0), movie("m3u:m2", "Alien", "SciFi", 1)], vec![], vec![ItemKind::Movie], NOW);
    let item = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("m3u:m1".into()) };
    let i2 = item.clone();
    w(&s, move |c| attach_all(c, &i2, e));
    let mut m = movie("m3u:m9", "Heat", "Crime", 0);
    m.url_key = None;
    m.stream = StreamLocator::Url { url: "http://other/heat.mkv".into() };
    let sum = run_import(&s, src, vec![], vec![m, movie("m3u:m2", "Alien", "SciFi", 1)], vec![], vec![ItemKind::Movie], NOW + 5);
    assert_eq!(sum.reattached, 1);
    let r = s.reader().unwrap();
    let p = user::progress(&r, &ItemRef { key: ItemKey("m3u:m9".into()), ..item.clone() }).unwrap().unwrap();
    assert_eq!(p.position_s, 12.0);
    assert_eq!(p.fingerprint.norm_group.as_deref(), Some("crime"));
    let cw = user::continue_watching(&r, 10).unwrap();
    assert_eq!(cw.len(), 1);
    assert!(vod::resolve_movie(&r, &cw[0].item).unwrap().is_some());
}

// ------------------------------------------------------------------ user data

#[test]
fn fingerprints_are_computed_inside_writes() {
    let s = store("tuner-st-fp");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    run_import(
        &s,
        src,
        vec![chan("xt:live:5", "UK: BBC Two HD", Some("bbc2.uk"), "UK | News", "http://h/live/u/p/5.ts", 0)],
        vec![],
        vec![],
        vec![ItemKind::Channel],
        NOW,
    );
    let item = ch_ref(src, "xt:live:5");
    let i2 = item.clone();
    w(&s, move |c| user::set_favourite(c, user::default_fav_list(c)?, &i2, true, NOW));
    let r = s.reader().unwrap();
    let fp = user::fav_items(&r, user::default_fav_list(&r).unwrap()).unwrap()[0].fingerprint.clone();
    assert_eq!(
        fp,
        Fingerprint {
            provider_id: Some("5".into()),
            tvg_id: Some("bbc2.uk".into()),
            norm_name: "bbctwo".into(),
            norm_group: Some(tuner::sources::identity::normalize_group("UK | News")),
            url_key: Some("http://h/live/5.ts".into()),
        }
    );
    assert_eq!(user::fingerprint_of(&r, &item).unwrap(), fp);
    assert_eq!(user::fingerprint_of(&r, &ch_ref(src, "nope")).unwrap(), Fingerprint::default());
    // a write for a vanished item keeps the stored fingerprint
    w(&s, move |c| {
        c.execute("DELETE FROM channel", [])?;
        user::set_item_override(c, &item, &ItemOverride { hidden: true, ..Default::default() })?;
        user::set_item_override(c, &item, &ItemOverride { hidden: true, locked: true, ..Default::default() })
    });
    let ov = user::item_overrides(&r, Some(src)).unwrap();
    assert_eq!(ov[0].fingerprint, Fingerprint::default(), "no facts at write time");
}

#[test]
fn favourite_lists_and_entries() {
    let s = store("tuner-st-fav");
    let src = w(&s, |c| {
        let src = seed::seed_source(c, "S")?;
        seed::seed_channels(c, src, 5)?;
        Ok(src)
    });
    let r = s.reader().unwrap();
    let def = user::default_fav_list(&r).unwrap();
    assert!(w(&s, move |c| Ok(user::delete_fav_list(c, def).is_err())), "default list is permanent");
    let k = |i: usize| ch_ref(src, &format!("m3u:seed{i:011}"));
    let (a, b, c0) = (k(0), k(1), k(2));
    let (a2, b2, c2) = (a.clone(), b.clone(), c0.clone());
    let sports = w(&s, move |c| {
        let sports = user::create_fav_list(c, "Sports", "S")?;
        for it in [&a2, &b2, &c2] {
            user::set_favourite(c, def, it, true, NOW)?;
        }
        user::set_favourite(c, def, &a2, true, NOW)?; // idempotent
        user::set_favourite(c, sports, &b2, true, NOW)?;
        user::move_favourite(c, def, &c2, 0)?;
        user::rename_fav_list(c, sports, "Sport", "T")?;
        user::reorder_fav_lists(c, &[sports, def])?;
        Ok(sports)
    });
    let items: Vec<ItemRef> = user::fav_items(&r, def).unwrap().into_iter().map(|e| e.item).collect();
    assert_eq!(items, vec![c0.clone(), a.clone(), b.clone()]);
    assert_eq!(user::lists_containing(&r, &b).unwrap(), vec![sports, def]);
    let lists = user::fav_lists(&r).unwrap();
    assert_eq!((lists[0].id, lists[0].name.as_str(), lists[0].icon.as_str()), (sports, "Sport", "T"));
    let idx = library::load_favs(&r).unwrap();
    assert!(idx.is_fav(&a));
    assert_eq!(idx.items(def), &[c0, a.clone(), b.clone()]);
    let a3 = a.clone();
    w(&s, move |c| {
        user::set_favourite(c, def, &a3, false, NOW)?;
        user::delete_fav_list(c, sports)
    });
    assert!(!library::load_favs(&r).unwrap().is_fav(&a));
    assert_eq!(user::fav_lists(&r).unwrap().len(), 1);
}

#[test]
fn overrides_progress_history_and_reminders() {
    let s = store("tuner-st-user");
    let (src, movies) = w(&s, |c| {
        let src = seed::seed_source(c, "S")?;
        seed::seed_channels(c, src, 3)?;
        let mv = seed::seed_movies(c, src, 3)?;
        Ok((src, movies_keys(mv.len())))
    });
    let r = s.reader().unwrap();
    let ch0 = ch_ref(src, "m3u:seed00000000000");
    let mref = |i: usize| ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey(movies[i].clone()) };

    // item overrides: upsert, default deletes, reorder
    let (c1, c2) = (ch0.clone(), ch0.clone());
    w(&s, move |c| {
        user::set_item_override(c, &c1, &ItemOverride { rename: Some("Zero".into()), locked: true, ..Default::default() })?;
        user::reorder_items(c, src, ItemKind::Channel, &[ItemKey("m3u:seed00000000002".into()), ItemKey("m3u:seed00000000000".into())])
    });
    let ov = user::item_override(&r, &ch0).unwrap().unwrap();
    assert_eq!((ov.rename.as_deref(), ov.locked, ov.user_order), (Some("Zero"), true, Some(1)));
    w(&s, move |c| user::set_item_override(c, &c2, &ItemOverride::default()));
    assert_eq!(user::item_override(&r, &ch0).unwrap(), None);
    assert_eq!(user::item_overrides(&r, None).unwrap().len(), 1);

    // group overrides
    w(&s, move |c| {
        user::set_group_override(c, src, ItemKind::Channel, &GroupKey("channel:news".into()), &GroupOverride { hidden: true, ..Default::default() })?;
        user::set_group_override(
            c,
            src,
            ItemKind::Channel,
            &GroupKey("channel:all".into()),
            &GroupOverride { pinned_home: true, ..Default::default() },
        )?;
        user::set_group_override(c, src, ItemKind::Channel, &GroupKey("channel:all".into()), &GroupOverride::default())
    });
    let go = user::group_overrides(&r).unwrap();
    assert_eq!(go.len(), 1);
    assert!(go[0].ov.hidden);

    // progress + continue watching + mark watched
    let (m0, m1, m2) = (mref(0), mref(1), mref(2));
    let wp = |item: &ItemRef, pos: f64, at: Timestamp| WatchProgress {
        item: item.clone(),
        position_s: pos,
        duration_s: Some(100.0),
        updated_at: at,
        finished: false,
        title: "x".into(),
        fingerprint: Fingerprint::default(),
    };
    let (p0, p1, p2) = (wp(&m0, 10.0, 1), wp(&m1, 20.0, 3), wp(&m2, 30.0, 2));
    w(&s, move |c| {
        user::save_progress(c, &p0)?;
        user::save_progress(c, &p1)?;
        user::save_progress(c, &p2)
    });
    let cw: Vec<ItemRef> = user::continue_watching(&r, 10).unwrap().into_iter().map(|p| p.item).collect();
    assert_eq!(cw, vec![m1.clone(), m2.clone(), m0.clone()]);
    assert_eq!(user::progress(&r, &m0).unwrap().unwrap().fingerprint.norm_name, "movie0");
    let (a, b) = (m1.clone(), m0.clone());
    w(&s, move |c| {
        user::mark_watched(c, &[(a, "A".into())], true, 50)?;
        user::mark_watched(c, &[(b, "B".into())], false, 50)
    });
    let cw: Vec<ItemRef> = user::continue_watching(&r, 10).unwrap().into_iter().map(|p| p.item).collect();
    assert_eq!(cw, vec![m2.clone()]);
    assert!(user::progress(&r, &m1).unwrap().unwrap().finished);
    assert_eq!(user::progress(&r, &m0).unwrap(), None);

    // history: consecutive duplicates collapse; distinct recent channels; 500 cap
    let h = |key: &str, at: Timestamp| HistoryEntry { item: ch_ref(src, key), at, title: key.into() };
    let entries = vec![h("m3u:seed00000000000", 1), h("m3u:seed00000000000", 2), h("m3u:seed00000000001", 3), h("m3u:seed00000000000", 4)];
    w(&s, move |c| {
        for e in &entries {
            user::push_history(c, e)?;
        }
        Ok(())
    });
    assert_eq!(count(&r, "SELECT count(*) FROM history"), 3);
    let rc: Vec<String> = user::recent_channels(&r, 10).unwrap().into_iter().map(|e| e.item.key.0).collect();
    assert_eq!(rc, vec!["m3u:seed00000000000", "m3u:seed00000000001"]);
    w(&s, move |c| {
        for i in 0..600 {
            user::push_history(c, &HistoryEntry { item: ch_ref(src, &format!("k{}", i % 2)), at: 100 + i, title: "t".into() })?;
        }
        Ok(())
    });
    assert_eq!(count(&r, "SELECT count(*) FROM history"), 500);

    // reminders
    let rem = |start: Timestamp, lead: i64| Reminder {
        id: ReminderId(0),
        channel: ch_ref(src, "m3u:seed00000000001"),
        channel_name: "C1".into(),
        programme_title: "P".into(),
        start,
        stop: start + 60,
        lead_s: lead,
        state: ReminderState::Pending,
    };
    let (r1, r2, r3) = (rem(1000, 100), rem(950, 0), rem(1000, 5));
    let (id1, id2, id3) = w(&s, move |c| Ok((user::add_reminder(c, &r1)?, user::add_reminder(c, &r2)?, user::add_reminder(c, &r3)?)));
    assert_eq!(id1, id3, "unique per channel + start");
    let pending: Vec<ReminderId> = user::pending_reminders(&r, 0).unwrap().into_iter().map(|x| x.id).collect();
    assert_eq!(pending, vec![id1, id2], "fire order: 900 then 950");
    assert_eq!(user::pending_reminders(&r, 901).unwrap().len(), 1);
    w(&s, move |c| user::set_reminder_state(c, id2, ReminderState::Fired));
    assert_eq!(user::all_reminders(&r).unwrap().len(), 1);
    assert_eq!(user::reminders_for_channel(&r, &ch_ref(src, "m3u:seed00000000001")).unwrap().len(), 2);
    w(&s, move |c| user::remove_reminder(c, id1));
    assert!(user::all_reminders(&r).unwrap().is_empty());
    let bad = Reminder { channel: ItemRef { kind: ItemKind::Movie, ..ch0.clone() }, ..rem(1, 1) };
    assert!(s.writer().blocking(move |c| user::add_reminder(c, &bad)).is_err());
}

fn movies_keys(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("m3u:movie{i:010}")).collect()
}

// ------------------------------------------------------------------ live & VOD reads

#[test]
fn live_reads_groups_and_failure_cache() {
    let s = store("tuner-st-live");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    let mut drm = chan("m3u:drm", "Locked TV", None, "Premium", "http://h/drm.mpd", 2);
    drm.unsupported = Some(Unsupported::Drm);
    let mut multi = chan("m3u:a", "Alpha", Some("a.tv"), "News", "http://h/a.ts", 0);
    multi.groups.push(group_key(ItemKind::Channel, "Premium"));
    multi.headers = Some(HttpHeaders { user_agent: Some("UA".into()), ..Default::default() });
    multi.catchup = Some(CatchupSpec { kind: CatchupKind::Shift, template: None, days: 3, correction_s: 0 });
    run_import(&s, src, vec![multi, chan("m3u:b", "Beta", None, "News", "http://h/b.ts", 1), drm], vec![], vec![], vec![ItemKind::Channel], NOW);
    let a = ch_ref(src, "m3u:a");
    let a2 = a.clone();
    w(&s, move |c| {
        user::set_item_override(c, &a2, &ItemOverride { rename: Some("A!".into()), catchup_offset_s: Some(30), ..Default::default() })?;
        user::set_group_override(
            c,
            src,
            ItemKind::Channel,
            &group_key(ItemKind::Channel, "Premium"),
            &GroupOverride { locked: true, user_order: Some(0), ..Default::default() },
        )
    });
    let r = s.reader().unwrap();
    let ch = live::channel_by_ref(&r, &a).unwrap().unwrap();
    assert_eq!(ch.display_name(), "A!");
    assert_eq!(ch.catchup_offset_s, 30);
    assert_eq!(ch.headers.user_agent.as_deref(), Some("UA"));
    assert_eq!(ch.catchup.as_ref().unwrap().days, 3);
    assert!(ch.locked, "locked through its Premium group");
    assert_eq!(ch.group_ids.len(), 2);
    let news = live::group(&r, ch.group_ids[0]).unwrap();
    assert_eq!(news.name, "news", "primary first");
    assert_eq!(live::channel(&r, ch.id).unwrap(), ch);
    assert!(live::channel_by_ref(&r, &ch_ref(src, "nope")).unwrap().is_none());
    let groups = live::groups(&r, ItemKind::Channel, Some(src)).unwrap();
    assert_eq!(groups.iter().map(|g| g.key.0.as_str()).collect::<Vec<_>>(), vec!["channel:premium", "channel:news"], "user order first");
    assert_eq!(groups[1].item_count, 2);
    assert!(live::groups(&r, ItemKind::Movie, None).unwrap().is_empty());

    // failure cache
    let drm_id = live::channel_by_ref(&r, &ch_ref(src, "m3u:drm")).unwrap().unwrap().id;
    assert!(live::should_skip(&r, drm_id, 0).unwrap(), "DRM is always skipped");
    assert!(!live::should_skip(&r, ch.id, 0).unwrap());
    let (f1, f2) = (a.clone(), a.clone());
    w(&s, move |c| {
        live::record_failure(c, &f1, "Http403", NOW)?;
        live::record_failure(c, &f1, "Timeout", NOW + 5)
    });
    assert!(live::should_skip(&r, ch.id, NOW).unwrap());
    assert!(!live::should_skip(&r, ch.id, NOW + 10).unwrap(), "older than `since`");
    let f = live::failures(&r, NOW).unwrap();
    assert_eq!(f[&a], live::StreamFailure { reason: "Timeout".into(), at: NOW + 5, count: 2 });
    w(&s, move |c| live::clear_failure(c, &f2));
    assert!(live::failures(&r, 0).unwrap().is_empty());
    assert!(!live::should_skip(&r, ChannelId(424242), 0).unwrap());
}

#[test]
fn vod_reads_details_and_next_episode() {
    use tuner::store::import::{ImportEpisode, ImportSeason, MovieDetails, SeriesDetails};
    let s = store("tuner-st-vod");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    let mut m = movie("xt:vod:7", "Heat", "Crime", 0);
    m.stream = StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 7, ext: Some("mp4".into()) };
    run_import(
        &s,
        src,
        vec![],
        vec![m, movie("xt:vod:8", "Alien", "SciFi", 1)],
        vec![series("xt:series:1", "Lost", "Drama", 0)],
        vec![ItemKind::Movie, ItemKind::Series],
        NOW,
    );
    let r = s.reader().unwrap();
    let mid = vod::resolve_movie(&r, &ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("xt:vod:7".into()) }).unwrap().unwrap();
    w(&s, move |c| {
        import::store_movie_details(
            c,
            mid,
            &MovieDetails { plot: Some("Cops".into()), duration_s: Some(9000), container_ext: Some("mkv".into()), ..Default::default() },
            NOW,
        )
    });
    let mv = vod::movie(&r, mid).unwrap();
    assert_eq!((mv.plot.as_deref(), mv.duration_s, mv.details_loaded_at), (Some("Cops"), Some(9000), Some(NOW)));
    assert_eq!(mv.stream, StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: 7, ext: Some("mkv".into()) });
    assert_eq!(mv.rating, Some(7.5));
    assert_eq!(mv.group_ids.len(), 1);

    let sid: SeriesId = r.query_row("SELECT id FROM series", [], |x| x.get(0)).unwrap();
    let ep = |season: u32, e: u32| ImportEpisode {
        key: ItemKey(format!("xt:ep:{season}{e:02}")),
        provider_id: Some(format!("{season}{e:02}")),
        season,
        episode: e,
        title: format!("E{e}"),
        plot: None,
        duration_s: Some(2400),
        still: None,
        air_date: None,
        rating: None,
        stream: StreamLocator::Xtream { kind: XtreamStreamKind::Series, stream_id: (season * 100 + e) as i64, ext: Some("mp4".into()) },
    };
    let d =
        SeriesDetails { seasons: vec![], episodes: vec![ep(1, 1), ep(1, 2), ep(2, 1)], plot: Some("Island".into()), backdrop: None, tmdb_id: None };
    w(&s, move |c| import::store_series_details(c, sid, &d, NOW));
    let seasons = vod::seasons(&r, sid).unwrap();
    assert_eq!(seasons.iter().map(|x| (x.number, x.episode_count)).collect::<Vec<_>>(), vec![(1, 2), (2, 1)]);
    assert_eq!(vod::episodes(&r, sid, Some(1)).unwrap().len(), 2);
    let all = vod::episodes(&r, sid, None).unwrap();
    assert_eq!(vod::episode(&r, all[2].id).unwrap().title, "E1");
    assert_eq!(vod::series(&r, sid).unwrap().plot.as_deref(), Some("Island"));
    assert_eq!(vod::next_episode(&r, sid).unwrap().unwrap().key, all[0].key, "S1E1 when nothing watched");

    let eref = |e: &Episode| ItemRef { source: src, kind: ItemKind::Episode, key: e.key.clone() };
    let (e0, e1) = (eref(&all[0]), eref(&all[1]));
    w(&s, move |c| user::mark_watched(c, &[(e0, "E1".into())], true, NOW));
    assert_eq!(vod::next_episode(&r, sid).unwrap().unwrap().key, all[1].key, "after the last finished");
    let p = WatchProgress {
        item: e1.clone(),
        position_s: 60.0,
        duration_s: None,
        updated_at: NOW + 1,
        finished: false,
        title: "E2".into(),
        fingerprint: Fingerprint::default(),
    };
    w(&s, move |c| user::save_progress(c, &p));
    assert_eq!(vod::next_episode(&r, sid).unwrap().unwrap().key, all[1].key, "in-progress episode");
    assert_eq!(vod::resolve_episode(&r, &e1).unwrap(), Some(all[1].id));
    assert_eq!(user::progress(&r, &e1).unwrap().unwrap().fingerprint.norm_name, "lost:s1e2");

    // provider renumbers the episodes: progress follows (series-scoped reattach)
    let mut renum = vec![ep(1, 1), ep(1, 2), ep(2, 1)];
    for e in &mut renum {
        e.key = ItemKey(format!("{}-new", e.key.0));
        e.provider_id = None;
    }
    let d2 = SeriesDetails {
        seasons: vec![ImportSeason { number: 1, name: Some("One".into()), cover: None, air_date: None, overview: None, episode_count: 0 }],
        episodes: renum,
        plot: None,
        backdrop: None,
        tmdb_id: None,
    };
    w(&s, move |c| import::store_series_details(c, sid, &d2, NOW + 2));
    let p = user::progress(&r, &ItemRef { key: ItemKey("xt:ep:102-new".into()), ..e1 }).unwrap().unwrap();
    assert_eq!(p.position_s, 60.0);
    let seasons = vod::seasons(&r, sid).unwrap();
    assert_eq!(
        seasons.iter().map(|x| (x.number, x.name.clone(), x.episode_count)).collect::<Vec<_>>(),
        vec![(1, Some("One".into()), 2), (2, None, 1)]
    );
    assert_eq!(count(&r, "SELECT count(*) FROM episode"), 3, "old episodes deleted");

    // recently added: newest first, hidden left out
    let hid = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("xt:vod:8".into()) };
    assert_eq!(vod::recently_added(&r, true, 10).unwrap().len(), 2);
    w(&s, move |c| user::set_item_override(c, &hid, &ItemOverride { hidden: true, ..Default::default() }));
    let ra = vod::recently_added(&r, true, 10).unwrap();
    assert_eq!(ra.iter().map(|v| v.key.0.as_str()).collect::<Vec<_>>(), vec!["xt:vod:7"]);
    assert_eq!(vod::recently_added(&r, false, 10).unwrap().len(), 1);
}

#[test]
fn series_details_reset_when_provider_last_modified_changes() {
    let s = store("tuner-st-lastmod");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("S", "http://h/s.m3u"), NOW));
    run_import(&s, src, vec![], vec![], vec![series("xt:series:1", "Lost", "Drama", 0)], vec![ItemKind::Series], NOW);
    w(&s, |c| Ok(c.execute("UPDATE series SET details_at = 5", [])?));
    run_import(&s, src, vec![], vec![], vec![series("xt:series:1", "Lost", "Drama", 0)], vec![ItemKind::Series], NOW);
    let r = s.reader().unwrap();
    assert_eq!(count(&r, "SELECT details_at FROM series"), 5, "unchanged → details kept");
    let mut changed = series("xt:series:1", "Lost", "Drama", 0);
    changed.last_modified = Some(200);
    run_import(&s, src, vec![], vec![], vec![changed], vec![ItemKind::Series], NOW);
    assert_eq!(count(&r, "SELECT count(*) FROM series WHERE details_at IS NULL"), 1);
}

// ------------------------------------------------------------------ library

#[test]
fn library_load_merges_overrides_and_skips_disabled_sources() {
    let s = store("tuner-st-lib");
    let (src, chans, off) = w(&s, |c| {
        let src = seed::seed_source(c, "S")?;
        let chans = seed::seed_channels(c, src, 50)?;
        seed::seed_movies(c, src, 20)?;
        seed::seed_series_with_episodes(c, src, 4, 1, 2)?;
        let off = seed::seed_source(c, "Off")?;
        seed::seed_channels(c, off, 5)?;
        c.execute("UPDATE source SET enabled = 0 WHERE id = ?1", [off])?;
        Ok((src, chans, off))
    });
    let k = |i: usize| ch_ref(src, &format!("m3u:seed{i:011}"));
    let (k1, k2, k3, k4) = (k(1), k(2), k(3), k(4));
    w(&s, move |c| {
        user::set_item_override(c, &k1, &ItemOverride { hidden: true, ..Default::default() })?;
        user::set_item_override(c, &k2, &ItemOverride { rename: Some("Two!".into()), locked: true, ..Default::default() })?;
        user::reorder_items(c, src, ItemKind::Channel, &[k3.key.clone(), k4.key.clone()])?;
        user::set_favourite(c, user::default_fav_list(c)?, &k4, true, NOW)?;
        user::push_history(c, &HistoryEntry { item: k3.clone(), at: 5, title: "x".into() })?;
        user::set_group_override(c, src, ItemKind::Movie, &GroupKey("movie:movies".into()), &GroupOverride { locked: true, ..Default::default() })
    });
    let r = s.reader().unwrap();
    let lib = library::load(&r).unwrap();
    assert_eq!(lib.sources.len(), 2);
    assert!(lib.source(src).unwrap().has_vod && lib.source(src).unwrap().has_series);
    assert!(!lib.source(off).unwrap().enabled);
    assert_eq!(lib.live.channels.len(), 50, "disabled source excluded");
    assert!(lib.live.hidden.contains(&chans[1]));
    // display order: user-ordered first (3, 4), then provider order
    let order = lib.live.collection(&Collection::AllChannels, &lib.favs, &[], SortMode::Provider);
    assert_eq!(&order[..3], &[chans[3], chans[4], chans[0]]);
    assert!(!order.contains(&chans[1]));
    let two = lib.live.get(chans[2]).unwrap();
    assert_eq!((&*two.name, two.locked), ("Two!", true));
    assert_eq!(lib.live.get(chans[0]).unwrap().epg_channel_id, None);
    let news = lib.live.groups.iter().find(|g| g.key.0 == "channel:news").unwrap();
    assert_eq!(lib.live.group_members(news.id).len(), 5);
    assert_eq!(lib.live.get(chans[0]).unwrap().group_ids.len(), 2);
    assert_eq!(lib.recent_channels, vec![chans[3]]);
    assert!(lib.favs.is_fav(&k(4)));
    assert_eq!(lib.vod.movies.len(), 20);
    assert!(lib.vod.movies.iter().all(|m| m.locked), "movie group locked");
    assert_eq!(lib.vod.series.len(), 4);
    assert_eq!(lib.vod.group_items(lib.vod.movie_groups[0].id, SortMode::Provider).len(), 20);

    // hiding every group of an item hides the item
    w(&s, move |c| {
        user::set_group_override(c, src, ItemKind::Channel, &GroupKey("channel:all".into()), &GroupOverride { hidden: true, ..Default::default() })?;
        user::set_group_override(c, src, ItemKind::Movie, &GroupKey("movie:movies".into()), &GroupOverride { hidden: true, ..Default::default() })
    });
    let lib = library::load(&r).unwrap();
    assert!(lib.live.hidden.contains(&chans[1]) && lib.live.hidden.contains(&chans[5]));
    assert!(!lib.live.hidden.contains(&chans[0]), "still visible through News");
    assert!(lib.vod.movies.is_empty());
}

// ------------------------------------------------------------------ stacks (DESIGN §29.10)

/// A movie as an Xtream list sends it: the year only in the name, maybe a TMDB id.
fn listed(key: &str, name: &str, grp: &str, order: i32, tmdb: Option<u32>) -> ImportMovie {
    ImportMovie { year: None, tmdb, ..movie(key, name, grp, order) }
}

fn mref(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey(key.into()) }
}

/// The copies of one title stack in All across playlists and languages, from match keys the
/// import stores: hidden copies (category hidden) and playlists out of All don't join, a
/// locked copy does (and isn't the top card), the English copy is the top, different TMDB ids
/// never stack, hidden explicit content is left out of everything (at build and on relock). The
/// user's split survives a refresh and its Undo restores the stack; a renamed copy is keyed
/// again.
#[test]
fn copies_stack_across_playlists_and_follow_the_users_corrections() {
    let s = store("tuner-st-stacks");
    let (a, b, c3) = w(&s, |c| Ok((seed::seed_source(c, "TV")?, seed::seed_source(c, "TV 2")?, seed::seed_source(c, "Free")?)));
    let tv = vec![
        listed("a1", "EN ★ Dune - 2021", "EN Movies", 0, None),
        listed("a2", "FR ★ Dune - 2021", "FR Films", 1, None),
        listed("a3", "EN ★ Heat - 1995", "EN Movies", 2, None),
        listed("a4", "EN ★ Leo - 2023", "EN Movies", 3, Some(1_075_794)),
    ];
    let tv2 = vec![
        listed("b1", "EN - Dune (2021)", "Hidden", 0, None),
        listed("b2", "AR-SUBS - Dune (2021)", "Locked", 1, None),
        listed("b3", "EN - Heat (1995)", "Films", 2, None),
        listed("b4", "IN-TL - Leo (2023)", "Films", 3, Some(1_000_837)),
    ];
    run_import(&s, a, vec![], tv.clone(), vec![], vec![ItemKind::Movie], NOW);
    run_import(&s, b, vec![], tv2, vec![], vec![ItemKind::Movie], NOW);
    run_import(&s, c3, vec![], vec![listed("c1", "EN - Dune (2021)", "Films", 0, None)], vec![], vec![ItemKind::Movie], NOW);
    w(&s, move |c| {
        user::set_group_override(c, b, ItemKind::Movie, &GroupKey("movie:hidden".into()), &GroupOverride { hidden: true, ..Default::default() })?;
        user::set_group_override(c, b, ItemKind::Movie, &GroupKey("movie:locked".into()), &GroupOverride { locked: true, ..Default::default() })?;
        sources::set_source_settings(c, c3, &SourceSettings { in_all: Some(false), ..Default::default() })
    });
    let r = s.reader().unwrap();
    assert_eq!(count(&r, "SELECT count(*) FROM movie WHERE match_hash IS NULL OR match_tags IS NULL"), 0, "keyed at import");
    assert_eq!(r.query_row("SELECT tmdb_id FROM movie WHERE key = 'a4'", [], |x| x.get::<_, String>(0)).unwrap(), "1075794");

    let lib = library::load(&r).unwrap();
    let at = |key: &str| lib.vod.movies.iter().position(|m| m.key.0 == key).map(|i| i as u32);
    let st = lib.vod.stacks(true);
    let names = |v: &[u32]| v.iter().map(|i| lib.vod.movies[*i as usize].key.0.clone()).collect::<Vec<_>>();
    let dune = st.stack_of(at("a1").unwrap()).unwrap();
    assert_eq!(names(st.members(dune)), ["a1", "b2", "a2"], "English, then subtitled, then French; hidden b1 and out-of-All c1 left out");
    assert_eq!(at("b1"), None, "hidden");
    assert_eq!(st.stack_of(at("c1").unwrap()), None, "Free is out of All");
    assert!(lib.vod.movies[at("b2").unwrap() as usize].locked, "the locked copy stays in the stack");
    assert_eq!(names(&[st.top(dune)]), ["a1"]);
    assert_eq!(st.stack_of(at("a4").unwrap()), None, "two TMDB ids: two titles");
    assert_eq!(st.len(), 2, "Dune and Heat");
    assert_eq!(lib.vod.movies.len() - st.saved(), 5, "All: a1 (Dune), a3 (Heat), a4, b4, c1");

    // explicit content hidden by parental control (DESIGN §20.3): an explicit copy is nowhere,
    // so it never stacks — at build time, and when a snapshot re-filters on relock
    w(&s, |c| Ok(c.execute("UPDATE movie SET explicit = 1 WHERE key = 'b3'", []).map(|_| ())?));
    let hidden = library::load_visible(&r, library::Visibility::NO_EXPLICIT).unwrap();
    assert!(hidden.vod.movies.iter().all(|m| m.key.0 != "b3"));
    assert_eq!(hidden.vod.stacks(true).len(), 1, "Heat has one copy left");
    let shown = library::load(&r).unwrap();
    assert_eq!(shown.vod.stacks(true).len(), 2, "an unlocked session stacks it");
    let relocked = shown.hiding_explicit();
    let (vod, st) = (&relocked.vod, relocked.vod.stacks(true));
    assert_eq!((st.len(), vod.movies.len() - st.saved()), (1, 5), "Heat is a lone card, Dune one card for three copies");
    let at = |key: &str| vod.movies.iter().position(|m| m.key.0 == key).map(|i| i as u32);
    assert_eq!(at("b3"), None);
    assert_eq!(st.stack_of(at("a3").unwrap()), None);
    let dune = st.stack_of(at("a1").unwrap()).unwrap();
    let keys: Vec<&str> = st.members(dune).iter().map(|i| vod.movies[*i as usize].key.0.as_str()).collect();
    assert_eq!((keys, vod.movies[st.top(dune) as usize].key.0.as_str()), (vec!["a1", "b2", "a2"], "a1"), "re-pointed at the shorter vector");
    w(&s, |c| Ok(c.execute("UPDATE movie SET explicit = 0 WHERE key = 'b3'", []).map(|_| ())?));

    // "Not the same title": the copy leaves its stack, a refresh keeps it out, Undo restores
    w(&s, move |c| tuner::store::titles::set_split(c, &mref(a, "a2"), true, NOW));
    run_import(&s, a, vec![], tv.clone(), vec![], vec![ItemKind::Movie], NOW + 10);
    let lib = library::load(&r).unwrap();
    let a2 = lib.vod.movies.iter().position(|m| m.key.0 == "a2").unwrap() as u32;
    assert_eq!(lib.vod.stacks(true).stack_of(a2), None);
    assert_eq!(tuner::store::titles::links(&r).unwrap()[0].name.as_deref(), Some("FR ★ Dune - 2021"));
    w(&s, move |c| tuner::store::titles::set_split(c, &mref(a, "a2"), false, NOW));
    let lib = library::load(&r).unwrap();
    let a2 = lib.vod.movies.iter().position(|m| m.key.0 == "a2").unwrap() as u32;
    assert!(lib.vod.stacks(true).stack_of(a2).is_some());

    // a pin is one per stack
    w(&s, move |c| {
        tuner::store::titles::set_pin(c, &mref(b, "b2"), &[], true, NOW)?;
        tuner::store::titles::set_pin(c, &mref(a, "a1"), &[mref(b, "b2"), mref(a, "a2")], true, NOW)
    });
    let lib = library::load(&r).unwrap();
    let pinned: Vec<&str> =
        lib.vod.movies.iter().enumerate().filter(|(i, _)| lib.vod.stacks(true).meta(*i as u32).pinned).map(|(_, m)| &*m.key.0).collect();
    assert_eq!(pinned, ["a1"]);

    // the provider renames a copy: its key follows (Heat 1996 is another film)
    let mut renamed = tv;
    renamed[2] = listed("a3", "EN ★ Heat - 1996", "EN Movies", 2, None);
    run_import(&s, a, vec![], renamed, vec![], vec![ItemKind::Movie], NOW + 20);
    let lib = library::load(&r).unwrap();
    assert_eq!(lib.vod.stacks(true).len(), 1, "only Dune");
}

// ------------------------------------------------------------------ copies (DESIGN §29.14)

/// A listed movie whose stream is an Xtream file with container `ext`.
fn listed_ext(key: &str, name: &str, grp: &str, order: i32, ext: &str) -> ImportMovie {
    ImportMovie {
        stream: StreamLocator::Xtream { kind: XtreamStreamKind::Movie, stream_id: order as i64 + 1, ext: Some(ext.into()) },
        ..listed(key, name, grp, order, None)
    }
}

/// Two playlists with copies of Dune: TV has an English avi, an English 4K mkv and a French
/// copy (and Heat); TV 2 an English mkv and a locked Arabic-subtitled copy.
fn dune_copies(name: &str) -> (Store, SourceId, SourceId) {
    let s = store(name);
    let (a, b) = w(&s, |c| Ok((seed::seed_source(c, "TV")?, seed::seed_source(c, "TV 2")?)));
    let tv = vec![
        listed_ext("a1", "EN ★ Dune - 2021", "EN Movies", 0, "avi"),
        listed_ext("a2", "FR ★ Dune - 2021", "FR Films", 1, "mkv"),
        listed_ext("a3", "EN ★ Heat - 1995", "EN Movies", 2, "mkv"),
        listed_ext("a5", "EN ★ Dune - 2021 [4K]", "EN Movies", 3, "mkv"),
    ];
    let tv2 = vec![listed_ext("b1", "EN - Dune (2021)", "Films", 0, "mkv"), listed_ext("b2", "AR-SUBS - Dune (2021)", "Locked", 1, "mp4")];
    run_import(&s, a, vec![], tv, vec![], vec![ItemKind::Movie], NOW);
    run_import(&s, b, vec![], tv2, vec![], vec![ItemKind::Movie], NOW);
    w(&s, move |c| {
        user::set_group_override(c, b, ItemKind::Movie, &GroupKey("movie:locked".into()), &GroupOverride { locked: true, ..Default::default() })
    });
    (s, a, b)
}

/// Which copy of a title plays (DESIGN §29.14): with nothing known, the user's language, the
/// playlist order, then mkv / mp4 before other containers; a locked copy only once the session
/// is unlocked; a copy that failed lately, whose playlist refused a stream in the last minutes
/// (connection limit) or whose account expired goes last; the copy with the latest progress
/// beats all that, and the user's pin beats the progress. The second copy is the fallback.
#[test]
fn copy_choice_follows_pin_progress_trouble_language_playlist_and_container() {
    use tuner::config::CopyOrder;
    use tuner::store::copies::{self, CopyStatus, Trouble};
    let (s, a, b) = dune_copies("tuner-st-copy-choice");
    let r = s.reader().unwrap();
    let plan = |unlocked: bool| {
        let lib = library::load(&r).unwrap();
        let at = |key: &str| lib.vod.movies.iter().position(|m| m.key.0 == key).unwrap() as u32;
        let p = copies::plan(&r, &lib.vod, true, at("a1"), unlocked, CopyOrder::PlaylistOrder, NOW).unwrap();
        let keys = |v: &[u32]| v.iter().map(|i| lib.vod.movies[*i as usize].key.0.clone()).collect::<Vec<_>>();
        let status: HashMap<String, CopyStatus> = p.copies.iter().map(|c| (c.item_ref.key.0.clone(), p.status(c.item))).collect();
        (keys(&p.order), keys(&p.fallback().into_iter().collect::<Vec<_>>()), status, p)
    };

    let (order, fallback, status, p) = plan(false);
    assert_eq!(order, ["a5", "a1", "b1", "a2"], "English, TV before TV 2, mkv before avi; the locked copy left out");
    assert_eq!(fallback, ["a1"]);
    assert_eq!((status["a5"], status["b2"], status["a1"]), (CopyStatus::PlaysFirst, CopyStatus::Locked, CopyStatus::Plain));
    assert_eq!(p.copies.len(), 5, "the locked copy is still one of the copies");
    assert_eq!(p.get(p.first().unwrap()).unwrap().container.as_deref(), Some("mkv"));
    assert_eq!(plan(true).0, ["a5", "a1", "b1", "b2", "a2"], "an unlocked session plays it too");

    // a copy that failed 1 h ago goes last
    w(&s, move |c| live::record_failure(c, &mref(a, "a5"), "http-not-found", NOW - 3600));
    let (order, _, status, _) = plan(false);
    assert_eq!(order, ["a1", "b1", "a2", "a5"]);
    assert_eq!(status["a5"], CopyStatus::Failed { at: NOW - 3600 });
    // ... but not one that failed yesterday
    w(&s, move |c| live::record_failure(c, &mref(a, "a5"), "http-not-found", NOW - 2 * 86_400));
    assert_eq!(plan(false).0[0], "a5");

    // TV's account expired: TV 2 plays first
    let expired = AccountInfo { status: AccountStatus::Active, exp_date: Some(NOW - 60), ..Default::default() };
    w(&s, move |c| sources::set_account(c, a, &expired));
    let (order, _, status, p) = plan(false);
    assert_eq!(order, ["b1", "a5", "a1", "a2"]);
    assert_eq!(status["a1"], CopyStatus::Expired);
    assert_eq!(p.get(p.order[1]).unwrap().trouble, Some(Trouble::Expired));
    let active = AccountInfo { status: AccountStatus::Active, exp_date: Some(NOW + 86_400), ..Default::default() };
    w(&s, move |c| sources::set_account(c, a, &active));

    // TV refused another stream 5 min ago: at its connection limit; 20 min ago no longer
    w(&s, move |c| live::record_failure(c, &mref(a, "a3"), "http-forbidden", NOW - 300));
    let (order, _, status, _) = plan(false);
    assert_eq!(order[0], "b1");
    assert_eq!(status["a1"], CopyStatus::AtLimit);
    w(&s, move |c| live::record_failure(c, &mref(a, "a3"), "http-forbidden", NOW - 1200));
    assert_eq!(plan(false).0[0], "a5");

    // the copy you were watching plays, failed or not; the latest progress wins
    let watch = |item: ItemRef, at: Timestamp| WatchProgress {
        item,
        position_s: 2480.0,
        duration_s: Some(9000.0),
        updated_at: at,
        finished: false,
        title: "Dune".into(),
        fingerprint: Default::default(),
    };
    let (p1, p2) = (watch(mref(b, "b1"), NOW - 600), watch(mref(a, "a2"), NOW - 60));
    w(&s, move |c| user::save_progress(c, &p1));
    assert_eq!(plan(false).0[0], "b1");
    w(&s, move |c| live::record_failure(c, &mref(b, "b1"), "network", NOW - 30));
    assert_eq!(plan(false).0[0], "b1", "progress beats a recent failure");
    w(&s, move |c| user::save_progress(c, &p2));
    let (order, _, _, p) = plan(false);
    assert_eq!(order[0], "a2", "the French copy was watched last");
    assert_eq!(p.resume().map(|c| c.item_ref.key.0.as_str()), Some("a2"));
    assert_eq!(p.resume().and_then(|c| c.progress.as_ref()).map(|x| x.position_s), Some(2480.0));

    // "Always play from" beats the progress (stored per title, read by the library build)
    w(&s, move |c| tuner::store::titles::set_pin(c, &mref(a, "a1"), &[mref(a, "a2"), mref(b, "b1")], true, NOW));
    let (order, fallback, _, p) = plan(false);
    assert_eq!((order[0].as_str(), fallback[0].as_str()), ("a1", "a2"));
    assert!(p.get(p.first().unwrap()).unwrap().pinned);
    assert_eq!(p.instead_of(p.first().unwrap()), p.fallback(), "the error card offers the next copy");

    // a title with one copy: itself, nothing to fall back on
    let lib = library::load(&r).unwrap();
    let heat = lib.vod.movies.iter().position(|m| m.key.0 == "a3").unwrap() as u32;
    let p = copies::plan(&r, &lib.vod, true, heat, false, CopyOrder::PlaylistOrder, NOW).unwrap();
    assert_eq!((p.order.clone(), p.fallback()), (vec![heat], None));
}

/// Actions on a combined title write per copy (the recommendation's §3 table) and Undo puts
/// back exactly the rows that were there: ★ adds the copy that would play (nothing when a copy
/// already is one) and removes every copy; ✓ marks the copy that would play and forgets every
/// copy's progress; lock and hide take every copy; a rename skips copies with their own rename
/// unless told to overwrite.
#[test]
fn combined_actions_write_per_copy_and_undo_restores_the_rows() {
    use tuner::store::copies;
    let (s, a, b) = dune_copies("tuner-st-copy-actions");
    let r = s.reader().unwrap();
    let (a1, b1) = (mref(a, "a1"), mref(b, "b1"));
    let both = vec![a1.clone(), b1.clone()];
    let list = user::default_fav_list(&r).unwrap();
    let favs = || user::fav_items(&r, list).unwrap();
    let undo = |u: copies::Undo| w(&s, move |c| copies::undo(c, &u));

    // ★
    let (all, plays) = (both.clone(), a1.clone());
    w(&s, move |c| copies::favourite(c, list, &all, &plays, true, NOW));
    assert_eq!(favs().iter().map(|f| &f.item).collect::<Vec<_>>(), [&a1]);
    let (all, plays) = (both.clone(), b1.clone());
    w(&s, move |c| copies::favourite(c, list, &all, &plays, true, NOW + 1));
    assert_eq!(favs().len(), 1, "a copy already is a favourite");
    let other = b1.clone();
    w(&s, move |c| user::set_favourite(c, list, &other, true, NOW + 2));
    let before = favs();
    let all = both.clone();
    let u = w(&s, move |c| copies::favourite(c, list, &all, &all[0], false, NOW + 3));
    assert!(favs().is_empty(), "removed from every copy");
    assert_eq!(u.scope(), tuner::msg::DataScope::Favourites);
    undo(u);
    assert_eq!(favs(), before, "positions, times and fingerprints back");

    // ✓
    let p = WatchProgress {
        item: b1.clone(),
        position_s: 600.0,
        duration_s: Some(9000.0),
        updated_at: NOW - 100,
        finished: false,
        title: "Dune".into(),
        fingerprint: Default::default(),
    };
    w(&s, move |c| user::save_progress(c, &p));
    let progress = |i: &ItemRef| user::progress(&r, i).unwrap();
    let before = (progress(&a1), progress(&b1));
    let (all, plays) = (both.clone(), a1.clone());
    let u = w(&s, move |c| copies::watched(c, &all, (&plays, "Dune"), true, NOW));
    assert!(progress(&a1).is_some_and(|x| x.finished), "the copy that would play");
    assert!(progress(&b1).is_some_and(|x| !x.finished), "the other copy keeps its progress");
    undo(u);
    assert_eq!((progress(&a1), progress(&b1)), before);
    let all = both.clone();
    let u = w(&s, move |c| copies::watched(c, &all, (&all[0], "Dune"), false, NOW));
    assert_eq!((progress(&a1), progress(&b1)), (None, None), "forgotten on every copy");
    undo(u);
    assert_eq!((progress(&a1), progress(&b1)), before, "Undo restores the rows");

    // lock / hide every copy
    let own = a1.clone();
    w(&s, move |c| user::set_item_override(c, &own, &ItemOverride { rename: Some("Dune (mine)".into()), ..Default::default() }));
    let locked = |keys: &[&str]| {
        let lib = library::load(&r).unwrap();
        keys.iter().map(|k| lib.vod.movies.iter().find(|m| m.key.0 == *k).map(|m| m.locked)).collect::<Vec<_>>()
    };
    let all = both.clone();
    let u = w(&s, move |c| copies::lock(c, &all, true));
    assert_eq!(locked(&["a1", "b1"]), [Some(true), Some(true)]);
    assert_eq!(user::item_override(&r, &a1).unwrap().unwrap().rename.as_deref(), Some("Dune (mine)"), "other fields survive");
    undo(u);
    assert_eq!(locked(&["a1", "b1"]), [Some(false), Some(false)]);
    assert_eq!(user::item_override(&r, &b1).unwrap(), None, "no row was there");
    let all = both.clone();
    let u = w(&s, move |c| copies::hide(c, &all));
    assert_eq!(locked(&["a1", "b1"]), [None, None], "hidden");
    undo(u);
    assert_eq!(locked(&["a1", "b1"]), [Some(false), Some(false)]);

    // rename: copies with their own rename are asked about first
    let conflicts = copies::rename_conflicts(&r, &both, "Dune: Part One").unwrap();
    assert_eq!(conflicts, [(a1.clone(), "Dune (mine)".to_owned())]);
    let rename = |i: &ItemRef| user::item_override(&r, i).unwrap().and_then(|o| o.rename);
    let all = both.clone();
    w(&s, move |c| copies::rename(c, &all, "Dune: Part One", false));
    assert_eq!((rename(&a1).as_deref(), rename(&b1).as_deref()), (Some("Dune (mine)"), Some("Dune: Part One")));
    let all = both.clone();
    let u = w(&s, move |c| copies::rename(c, &all, "Dune: Part One", true));
    assert_eq!(rename(&a1).as_deref(), Some("Dune: Part One"));
    undo(u);
    assert_eq!((rename(&a1).as_deref(), rename(&b1).as_deref()), (Some("Dune (mine)"), Some("Dune: Part One")));

    // "Not the same title" and "Always play from": persisted, and Undo puts the links back
    let stacks = || {
        let lib = library::load(&r).unwrap();
        let st = lib.vod.stacks(true);
        let of = |k: &str| lib.vod.movies.iter().position(|m| m.key.0 == k).map(|i| i as u32).unwrap();
        let pinned: Vec<String> = ["a1", "b1"].iter().filter(|k| st.meta(of(k)).pinned).map(|k| k.to_string()).collect();
        (st.stack_of(of("b1")).is_some(), pinned)
    };
    let one = b1.clone();
    let u = w(&s, move |c| copies::split(c, &one, true, NOW));
    assert_eq!(stacks(), (false, vec![]), "split off");
    undo(u);
    assert_eq!(stacks(), (true, vec![]));
    let (all, one) = (both.clone(), a1.clone());
    w(&s, move |c| copies::pin(c, &one, &all, true, NOW));
    let (all, one) = (both.clone(), b1.clone());
    let u = w(&s, move |c| copies::pin(c, &one, &all, true, NOW + 1));
    assert_eq!(stacks().1, ["b1"], "one pin per title");
    undo(u);
    assert_eq!(stacks().1, ["a1"], "the earlier pin is back");
}

/// A series in two playlists continues from the copy with the newest episode progress (that
/// copy also plays first); the same episode is found in the other copy (fallback, "Play from");
/// forgetting the combined series forgets both copies' episodes, and Undo brings them back.
#[test]
fn series_copies_continue_from_the_newest_progress() {
    use tuner::config::CopyOrder;
    use tuner::store::copies;
    use tuner::store::import::{ImportEpisode, SeriesDetails};
    let s = store("tuner-st-copy-series");
    let (a, b) = w(&s, |c| Ok((seed::seed_source(c, "TV")?, seed::seed_source(c, "TV 2")?)));
    run_import(&s, a, vec![], vec![], vec![series("sa", "EN ★ Lost - 2004", "EN Series", 0)], vec![ItemKind::Series], NOW);
    run_import(&s, b, vec![], vec![], vec![series("sb", "EN - Lost (2004)", "Series", 0)], vec![ItemKind::Series], NOW);
    let r = s.reader().unwrap();
    let sid = |key: &str| r.query_row("SELECT id FROM series WHERE key = ?1", [key], |x| x.get::<_, SeriesId>(0)).unwrap();
    let (sa, sb) = (sid("sa"), sid("sb"));
    for (series, p) in [(sa, "a"), (sb, "b")] {
        let ep = |season: u32, e: u32| ImportEpisode {
            key: ItemKey(format!("ep:{p}{season}{e:02}")),
            provider_id: None,
            season,
            episode: e,
            title: format!("E{e}"),
            plot: None,
            duration_s: Some(2400),
            still: None,
            air_date: None,
            rating: None,
            stream: StreamLocator::Xtream { kind: XtreamStreamKind::Series, stream_id: (season * 100 + e) as i64, ext: Some("mkv".into()) },
        };
        let d = SeriesDetails { seasons: vec![], episodes: vec![ep(1, 1), ep(1, 2), ep(2, 1)], plot: None, backdrop: None, tmdb_id: None };
        w(&s, move |c| import::store_series_details(c, series, &d, NOW));
    }
    let lib = library::load(&r).unwrap();
    let at = |id: SeriesId| lib.vod.index_of(VodKind::Series(id)).unwrap();
    assert_eq!(lib.vod.stacks(false).stack_of(at(sa)), lib.vod.stacks(false).stack_of(at(sb)), "one stack");
    let first = |r: &Connection| copies::plan(r, &lib.vod, false, at(sa), false, CopyOrder::PlaylistOrder, NOW).unwrap().first();
    assert_eq!(first(&r), Some(at(sa)), "nothing watched: playlist order");
    let (e, ep) = copies::next_episode(&r, &[sa, sb]).unwrap().unwrap();
    assert_eq!((e, ep.season, ep.episode), (sa, 1, 1));

    let epref = |src: SourceId, key: &str| ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey(key.into()) };
    let watch = |item: ItemRef, at: Timestamp, finished: bool| WatchProgress {
        item,
        position_s: 1200.0,
        duration_s: Some(2400.0),
        updated_at: at,
        finished,
        title: "Lost".into(),
        fingerprint: Default::default(),
    };
    let (old, new) = (watch(epref(a, "ep:a101"), NOW - 1000, true), watch(epref(b, "ep:b102"), NOW - 10, false));
    w(&s, move |c| {
        user::save_progress(c, &old)?;
        user::save_progress(c, &new)
    });
    let (e, ep) = copies::next_episode(&r, &[sa, sb]).unwrap().unwrap();
    assert_eq!((e, ep.key.0.as_str()), (sb, "ep:b102"), "TV 2 was watched last");
    assert_eq!(first(&r), Some(at(sb)));
    let same = copies::same_episode(&r, sa, 1, 2).unwrap().unwrap();
    assert_eq!(vod::episode(&r, same).unwrap().key.0, "ep:a102");
    assert_eq!(copies::same_episode(&r, sa, 3, 1).unwrap(), None);
    // playing TV 2's episode falls back on TV's same episode
    let p = copies::plan(&r, &lib.vod, false, at(sb), false, CopyOrder::PlaylistOrder, NOW).unwrap();
    match tuner::ui::copies::episode_command(&r, &lib.vod, &p, sb, &ep, true, PlayOrigin::Fullscreen) {
        tuner::player::PlayerCommand::PlayCopy(c) => {
            assert_eq!((c.req.media, c.fallback), (MediaRef::Episode(ep.id), Some(MediaRef::Episode(same))));
        }
        other => panic!("{other:?}"),
    }

    let rows = || r.query_row("SELECT count(*) FROM watch_progress WHERE kind = 'episode'", [], |x| x.get::<_, i64>(0)).unwrap();
    let u = w(&s, move |c| copies::forget_series(c, &[sa, sb]));
    assert_eq!(rows(), 0, "every copy's episodes forgotten");
    w(&s, move |c| copies::undo(c, &u));
    assert_eq!(rows(), 2);
    assert_eq!(copies::next_episode(&r, &[sa, sb]).unwrap().unwrap().1.key.0, "ep:b102");
}

/// What the UI sends to play a combined movie: the first copy with the next as its fallback
/// (or the copy the user picked, falling back to the best other one); after a failure mid-film
/// the error card offers the next copy from where playback stopped — never a locked one while
/// the session is locked — and the fallback toast names both playlists.
#[test]
fn play_commands_and_the_error_cards_other_copy() {
    use std::sync::Arc;
    use tuner::config::CopyOrder;
    use tuner::player::{CopyPlay, PlayerCommand, PlayerSnapshot, PlayerState};
    use tuner::store::copies;
    use tuner::ui::copies as ui;
    let (s, _, _) = dune_copies("tuner-st-copy-commands");
    let r = s.reader().unwrap();
    let lib = library::load(&r).unwrap();
    let at = |key: &str| lib.vod.movies.iter().position(|m| m.key.0 == key).unwrap() as u32;
    let movie = |key: &str| {
        MediaRef::Movie(match lib.vod.movies[at(key) as usize].kind {
            VodKind::Movie(id) => id,
            VodKind::Series(_) => unreachable!(),
        })
    };
    let plan = |unlocked: bool| copies::plan(&r, &lib.vod, true, at("a1"), unlocked, CopyOrder::PlaylistOrder, NOW).unwrap();
    let copy = |cmd: Option<PlayerCommand>| match cmd {
        Some(PlayerCommand::PlayCopy(CopyPlay { req, fallback, start_at, .. })) => (req.media, fallback, start_at, req.resume),
        other => panic!("not a copy play: {other:?}"),
    };
    let p = plan(false);
    assert_eq!(copy(ui::movie_command(&lib.vod, &p, None, true, PlayOrigin::Fullscreen)), (movie("a5"), Some(movie("a1")), None, true));
    // the next copy is in the same playlist: a refusing playlist (403) falls back to TV 2's
    let Some(PlayerCommand::PlayCopy(c)) = ui::movie_command(&lib.vod, &p, None, true, PlayOrigin::Fullscreen) else { unreachable!() };
    assert_eq!(c.elsewhere, Some(movie("b1")));
    assert_eq!(CopyPlay::fallback_for(c.fallback.clone(), c.elsewhere.clone(), tuner::player::PlayerErrorKind::HttpForbidden), Some(movie("b1")));
    assert_eq!(CopyPlay::fallback_for(c.fallback, c.elsewhere, tuner::player::PlayerErrorKind::Timeout), Some(movie("a1")), "a dead copy: the next");
    let picked = ui::movie_command(&lib.vod, &p, Some(at("b1")), false, PlayOrigin::Fullscreen);
    assert_eq!(copy(picked), (movie("b1"), Some(movie("a5")), None, false), "a picked copy falls back on the best other one");
    let none = copies::CopyPlan { order: vec![], ..p.clone() };
    assert_eq!(ui::movie_command(&lib.vod, &none, None, true, PlayOrigin::Fullscreen), None, "all locked: the PIN first");

    // the 4K copy failed at 41:20: TV's avi copy is offered, starting there
    let failed = |key: &str| PlayerSnapshot {
        state: PlayerState::Error(tuner::player::PlayerError { kind: tuner::player::PlayerErrorKind::Network, message: "x".into() }),
        playable: Some(Arc::new(Playable {
            media: movie(key),
            kind: PlayableKind::Movie,
            item: None,
            source_id: None,
            title: "Dune".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: Secret::new(String::new()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        })),
        last_position: Some(2480.0),
        ..Default::default()
    };
    let offer = |key: &str, unlocked: bool| {
        ui::other_copy(&r, &lib, |m, i| copies::plan(&r, &lib.vod, m, i, unlocked, CopyOrder::PlaylistOrder, NOW).ok(), &failed(key))
    };
    let o = offer("a5", false).unwrap();
    assert_eq!(o.label, "Play from TV · EN at 41:20", "another copy of the same playlist says which");
    assert_eq!((o.play.req.media.clone(), o.play.start_at, o.play.fallback.clone()), (movie("a1"), Some(2480.0), None));
    assert_eq!(offer("a1", false).unwrap().play.req.media, movie("a5"));
    assert_eq!(offer("b1", false).unwrap().label, "Play from TV at 41:20", "TV 2's copy failed: TV's best");

    // the toast of a fallback
    let (tv, tv2) = (lib.vod.movies[at("a1") as usize].source_id, lib.vod.movies[at("b1") as usize].source_id);
    assert_eq!(ui::fell_back_toast(&lib, "Dune", Some(tv), Some(tv2)).text, "TV couldn't play Dune — playing from TV 2");
    assert_eq!(ui::fell_back_toast(&lib, "Dune", None, Some(tv2)).text, "Dune couldn't start — playing from TV 2");

    // "Resume 41:20" continues the latest progress, also on a copy the user picked
    let watched = WatchProgress {
        item: mref(lib.vod.movies[at("b1") as usize].source_id, "b1"),
        position_s: 2480.0,
        duration_s: Some(9000.0),
        updated_at: NOW - 60,
        finished: false,
        title: "Dune".into(),
        fingerprint: Default::default(),
    };
    w(&s, move |c| user::save_progress(c, &watched));
    let p = plan(false);
    assert_eq!(copy(ui::movie_command(&lib.vod, &p, None, true, PlayOrigin::Fullscreen)), (movie("b1"), Some(movie("a5")), None, true));
    let picked = ui::movie_command(&lib.vod, &p, Some(at("a5")), true, PlayOrigin::Fullscreen);
    assert_eq!(copy(picked).2, Some(2480.0));
    assert_eq!(copy(ui::movie_command(&lib.vod, &p, Some(at("a5")), false, PlayOrigin::Fullscreen)).2, None, "Start over");

    // with TV's copies hidden, only the locked Arabic copy is left: offered to an open session only
    w(&s, move |c| {
        for k in ["a1", "a2", "a5"] {
            user::set_item_override(c, &mref(tv, k), &ItemOverride { hidden: true, ..Default::default() })?;
        }
        Ok(())
    });
    let lib = library::load(&r).unwrap();
    let VodKind::Movie(b1) = lib.vod.movies.iter().find(|m| m.key.0 == "b1").unwrap().kind else { unreachable!() };
    let mut snap = failed("b1");
    snap.playable = snap.playable.map(|p| Arc::new(Playable { media: MediaRef::Movie(b1), ..(*p).clone() }));
    let offer =
        |unlocked: bool| ui::other_copy(&r, &lib, |m, i| copies::plan(&r, &lib.vod, m, i, unlocked, CopyOrder::PlaylistOrder, NOW).ok(), &snap);
    assert_eq!(offer(false), None, "locked while the session is");
    assert_eq!(offer(true).map(|o| o.label), Some("Play from TV 2 · AR-SUBS at 41:20".to_owned()), "its playlist's other copy");
}

/// A split is user data like a favourite (DESIGN §29.11): when the provider re-keys the copy,
/// the refresh re-attaches the split to the new key — the wrong merge doesn't come back.
#[test]
fn a_split_follows_its_copy_to_a_new_key() {
    let s = store("tuner-st-split-rekey");
    let (a, b) = w(&s, |c| Ok((seed::seed_source(c, "TV")?, seed::seed_source(c, "TV 2")?)));
    run_import(&s, a, vec![], vec![listed("a1", "EN ★ Leo - 2023", "EN", 0, None)], vec![], vec![ItemKind::Movie], NOW);
    let tamil = listed("b1", "IN-TL - Leo (2023)", "Films", 0, None);
    run_import(&s, b, vec![], vec![tamil.clone()], vec![], vec![ItemKind::Movie], NOW);
    w(&s, move |c| tuner::store::titles::set_split(c, &mref(b, "b1"), true, NOW));
    // the provider lists the same stream under another key
    let moved = ImportMovie { key: ItemKey("b1-new".into()), ..tamil };
    run_import(&s, b, vec![], vec![moved], vec![], vec![ItemKind::Movie], NOW + 10);
    let r = s.reader().unwrap();
    let links = tuner::store::titles::links(&r).unwrap();
    assert_eq!(links.iter().map(|l| (l.item.key.0.as_str(), l.action)).collect::<Vec<_>>(), [("b1-new", tuner::store::titles::LinkAction::Split)]);
    let lib = library::load(&r).unwrap();
    assert_eq!(lib.vod.stacks(true).len(), 0, "still not the same title");
}

/// A movie's failure orders its copies ("failed 2 h ago", DESIGN §29.15): a refresh that also
/// imports channels keeps it; a movie that is gone takes its failure along.
#[test]
fn movie_failures_survive_a_refresh_with_channels() {
    let s = store("tuner-st-vod-failures");
    let a = w(&s, |c| seed::seed_source(c, "TV"));
    let channels = || vec![chan("c1", "News", None, "News", "http://h.tv/c1", 0)];
    let movies = vec![movie("m1", "Dune", "Films", 0), movie("m2", "Heat", "Films", 1)];
    run_import(&s, a, channels(), movies, vec![], vec![ItemKind::Channel, ItemKind::Movie], NOW);
    w(&s, move |c| {
        live::record_failure(c, &mref(a, "m1"), "timeout", NOW)?;
        live::record_failure(c, &mref(a, "m2"), "timeout", NOW)
    });
    run_import(&s, a, channels(), vec![movie("m1", "Dune", "Films", 0)], vec![], vec![ItemKind::Channel, ItemKind::Movie], NOW + 10);
    let r = s.reader().unwrap();
    let keys: Vec<String> = r
        .prepare("SELECT item_key FROM stream_failure ORDER BY item_key")
        .unwrap()
        .query_map([], |x| x.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(keys, ["m1"]);
}

// ------------------------------------------------------------------ category filter

/// One import of channels (and movies) whose groups carry provider raw names (`"UK | News"`),
/// displayed smart-cleaned like the importer does.
fn import_tagged(s: &Store, src: SourceId, channels: Vec<(&str, &str, Vec<&str>)>, movies: Vec<(&str, &str)>, now: Timestamp) {
    let grp = |kind: ItemKind, raw: &str, order: usize| ImportGroup {
        adult: false,
        kind,
        key: group_key(kind, raw),
        name: raw.rsplit(['|', ':']).next().unwrap().trim().to_owned(),
        raw_name: Some(raw.to_owned()),
        provider_id: None,
        provider_order: order as i32,
    };
    let mut groups: Vec<ImportGroup> = Vec::new();
    for raw in channels.iter().flat_map(|c| c.2.iter()) {
        if !groups.iter().any(|g| g.key == group_key(ItemKind::Channel, raw)) {
            groups.push(grp(ItemKind::Channel, raw, groups.len()));
        }
    }
    groups.extend(movies.iter().enumerate().map(|(i, m)| grp(ItemKind::Movie, m.1, i)));
    let chans: Vec<ImportChannel> = channels
        .iter()
        .enumerate()
        .map(|(i, (key, name, raws))| ImportChannel {
            groups: raws.iter().map(|r| group_key(ItemKind::Channel, r)).collect(),
            ..chan(key, name, None, raws[0], &format!("http://h.tv/{key}"), i as i32)
        })
        .collect();
    let films: Vec<ImportMovie> = movies.iter().enumerate().map(|(i, (key, raw))| movie(key, &format!("Film {key}"), raw, i as i32)).collect();
    w(s, move |c| {
        let imp = import::begin_catalogue(c, src, now)?;
        import::upsert_groups(c, &imp, &groups)?;
        import::write_channels(c, &imp, &chans)?;
        import::write_movies(c, &imp, &films)?;
        import::finish_catalogue(c, &imp, &ALL, now)
    });
}

#[test]
fn category_filter_hides_tagged_groups_everywhere_and_follows_refreshes() {
    let s = store("tuner-st-catfilter");
    let src = w(&s, |c| sources::insert_source(c, &m3u_draft("Tagged", "http://h.tv/list.m3u"), NOW));
    let channels = || {
        vec![
            ("m3u:uk1", "BBC News", vec!["UK | News"]),
            ("m3u:ar1", "Arabiya News", vec!["AR | News"]),
            ("m3u:both", "Shared News", vec!["AR | News", "UK | News"]),
            ("m3u:sp1", "Sky Sports News", vec!["Sports"]),
        ]
    };
    import_tagged(&s, src, channels(), vec![("m3u:film1", "|AR| Films")], NOW);
    // a source editor opened before the picker saves
    let stale = w(&s, move |c| sources::get_source(c, src));

    let r = s.reader().unwrap();
    let summary = library::category_summary(&r, src, false).unwrap();
    let tags: Vec<Option<&str>> = summary.tags.iter().map(|t| t.tag.as_ref().map(|t| t.0.as_str())).collect();
    assert_eq!(tags, [Some("lang:ar"), Some("GB"), None], "AR has 2 channels + 1 movie, GB 2 channels, untagged last");
    assert_eq!((summary.tags[0].live_groups, summary.tags[0].movie_groups, summary.needs_refresh), (1, 1, false));

    let rule = CategoryFilter { keep: [CategoryTag::country("GB")].into(), keep_untagged: true, known: summary.present(), ..Default::default() };
    let saved = rule.clone();
    w(&s, move |c| sources::set_category_filter(c, src, Some(&saved)));

    let id = |lib: &library::Library, key: &str| lib.live.by_ref(&ch_ref(src, key)).unwrap();
    let group = |lib: &library::Library, raw: &str| lib.live.groups.iter().find(|g| g.key == group_key(ItemKind::Channel, raw)).unwrap().clone();
    let visible = |lib: &library::Library| -> HashSet<ChannelId> {
        lib.live.collection(&Collection::AllChannels, &lib.favs, &[], SortMode::Provider).into_iter().collect()
    };

    let lib = library::load(&r).unwrap();
    assert_eq!(lib.source(src).unwrap().category_filter.as_ref(), Some(&rule));
    let (ar, uk, sports) = (group(&lib, "AR | News"), group(&lib, "UK | News"), group(&lib, "Sports"));
    assert_eq!((ar.tag.clone(), ar.filter, ar.hidden), (Some(CategoryTag::language("ar")), GroupFilter::Hidden, true));
    assert_eq!((uk.tag.clone(), uk.filter, uk.hidden), (Some(CategoryTag::country("GB")), GroupFilter::Kept, false));
    assert_eq!((sports.tag.clone(), sports.filter, sports.hidden), (None, GroupFilter::Kept, false), "untagged kept");
    let shown = visible(&lib);
    assert_eq!(shown, [id(&lib, "m3u:uk1"), id(&lib, "m3u:both"), id(&lib, "m3u:sp1")].into(), "the AR-only channel is filtered");
    assert!(lib.live.hidden.contains(&id(&lib, "m3u:ar1")));
    assert!(lib.vod.movies.is_empty(), "the |AR| movie category is filtered too");

    // search never indexes filtered items
    let mut idx = tuner::search::SearchIndex::new(std::sync::Arc::new(|| {}));
    idx.rebuild(&lib, false);
    idx.set_query("news");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let hits = loop {
        idx.tick(10);
        let hits = idx.results(tuner::search::SearchSection::All, 50);
        if hits.len() >= 3 || std::time::Instant::now() > deadline {
            break hits;
        }
    };
    let found: HashSet<ChannelId> = hits
        .iter()
        .filter_map(|h| match h.item.target {
            tuner::search::SearchTarget::Channel(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(found, shown);

    // a refresh adds a category for a tag the rule does not keep: hidden, the rule untouched
    let mut more = channels();
    more.push(("m3u:fr1", "France Info", vec!["|FR| Info"]));
    import_tagged(&s, src, more, vec![("m3u:film1", "|AR| Films")], NOW + 10);
    let lib = library::load(&r).unwrap();
    let fr = group(&lib, "|FR| Info");
    assert_eq!((fr.tag.clone(), fr.filter, fr.hidden), (Some(CategoryTag::country("FR")), GroupFilter::Hidden, true));
    assert!(!visible(&lib).contains(&id(&lib, "m3u:fr1")));
    assert_eq!(lib.source(src).unwrap().category_filter.as_ref(), Some(&rule));

    // the user shows one filtered group: an exemption from the rule
    let ar_ref = GroupRef::from(&ar);
    w(&s, move |c| user::show_group(c, &ar_ref, true));
    let lib = library::load(&r).unwrap();
    let ar = group(&lib, "AR | News");
    assert_eq!((ar.filter, ar.hidden), (GroupFilter::Exempt, false));
    assert!(visible(&lib).contains(&id(&lib, "m3u:ar1")));

    // hiding beats everything and clears the exemption; an item hides once all its groups do
    let refs = vec![GroupRef::from(&ar), GroupRef::from(&uk)];
    w(&s, move |c| user::hide_groups(c, &refs));
    let lib = library::load(&r).unwrap();
    let (ar, uk) = (group(&lib, "AR | News"), group(&lib, "UK | News"));
    assert_eq!((ar.filter, ar.hidden, uk.filter, uk.hidden), (GroupFilter::Hidden, true, GroupFilter::Kept, true));
    assert!(ar.user_hidden && uk.user_hidden, "the user's hide stays visible under the rule's");
    let ov = w(&s, |c| user::group_overrides(c));
    assert!(ov.iter().all(|o| o.ov.hidden && !o.ov.shown), "{ov:?}");
    assert_eq!(visible(&lib), [id(&lib, "m3u:sp1")].into());

    // stale editor copies (update_source / set_source_settings) keep the rule
    w(&s, move |c| {
        sources::update_source(c, &Source { name: "Renamed".into(), ..stale.clone() })?;
        sources::set_source_settings(c, src, &SourceSettings { refresh_h: Some(3), ..stale.settings })
    });
    let src_now = w(&s, move |c| sources::get_source(c, src));
    assert_eq!((src_now.name.as_str(), src_now.settings.refresh_h), ("Renamed", Some(3)));
    assert_eq!(src_now.settings.category_filter, Some(rule));
    let lib = library::load(&r).unwrap();
    assert!(group(&lib, "|FR| Info").hidden, "still filtered after the editor saves");

    // a group imported before raw names were stored: "needs refresh", and detection reads the
    // (smart-cleaned, untagged) display name, so the rule keeps it as untagged
    w(&s, move |c| Ok(c.execute("UPDATE grp SET raw_name = NULL WHERE source_id = ?1 AND name = 'Info'", [src])?));
    assert!(library::category_summary(&r, src, false).unwrap().needs_refresh);
    let fr = group(&library::load(&r).unwrap(), "|FR| Info");
    assert_eq!((fr.tag, fr.filter, fr.hidden), (None, GroupFilter::Kept, false));
}

// ------------------------------------------------------------------ deletion

#[test]
fn delete_source_leaves_no_orphans_in_any_table() {
    let s = store("tuner-st-delete");
    let (src, other) = w(&s, |c| {
        let src = sources::insert_source(c, &m3u_draft("Del", "http://h/d.m3u"), NOW)?;
        let other = sources::insert_source(c, &m3u_draft("Keep", "http://h/k.m3u"), NOW)?;
        Ok((src, other))
    });
    let (ch, mv, se) = sample(3000);
    run_import(&s, src, ch.clone(), mv.clone(), se.clone(), ALL.to_vec(), NOW);
    run_import(&s, other, ch[..10].to_vec(), vec![], vec![], ALL.to_vec(), NOW);
    // provider guide with programmes + FTS, mapped to both playlists' channels
    let urls = vec!["http://h/xmltv.php?username=u&password=p".to_string()];
    let pe = w(&s, move |c| Ok(sources::ensure_provider_epg(c, src, &urls)?[0]));
    w(&s, move |c| {
        let mut imp = epg::begin_epg_import(c, pe)?;
        let chans: Vec<epg::ImportEpgChannel> = (0..50)
            .map(|i| epg::ImportEpgChannel {
                xmltv_id: format!("c{i}.tv"),
                display_names: vec![(format!("Chan {i}"), format!("chan{i}"))],
                icon: None,
            })
            .collect();
        epg::write_epg_channels(c, &mut imp, &chans)?;
        let progs: Vec<epg::ImportProgramme> =
            (0..50 * 20).map(|i| prog(&format!("c{}.tv", i % 50), NOW + (i / 50) as i64 * 1800, "News at ten")).collect();
        epg::write_programmes(c, &mut imp, &progs)?;
        epg::finish_epg_import(c, &imp, NOW)?;
        let maps: Vec<EpgMapping> = (0..10)
            .map(|i| EpgMapping {
                channel: ch_ref(other, &format!("xt:live:{i}")),
                epg_source_id: pe,
                xmltv_id: format!("c{i}.tv"),
                method: EpgMatchMethod::TvgIdExact,
                confidence: 1.0,
            })
            .collect();
        epg::replace_auto_mappings(c, other, pe, &maps)?;
        epg::resolve_mappings(c)
    });
    let item = ch_ref(src, "xt:live:3");
    let mref = ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey("xt:vod:1".into()) };
    w(&s, move |c| {
        attach_all(c, &item, pe)?;
        attach_all(c, &mref, pe)?;
        user::set_group_override(
            c,
            src,
            ItemKind::Channel,
            &group_key(ItemKind::Channel, "News"),
            &GroupOverride { hidden: true, ..Default::default() },
        )?;
        user::push_history(c, &HistoryEntry { item: item.clone(), at: 1, title: "x".into() })?;
        live::record_failure(c, &item, "x", 1)
    });
    let r = s.reader().unwrap();
    assert!(count(&r, "SELECT count(*) FROM channel_epg") > 0);

    w(&s, move |c| sources::delete_source(c, src));

    for t in [
        "grp",
        "channel",
        "movie",
        "series",
        "episode",
        "fav_item",
        "item_override",
        "group_override",
        "watch_progress",
        "history",
        "reminder",
        "stream_failure",
        "channel_epg_map",
    ] {
        assert_eq!(count(&r, &format!("SELECT count(*) FROM {t} WHERE source_id = {}", src.0)), 0, "{t}");
    }
    assert_eq!(count(&r, "SELECT count(*) FROM epg_source WHERE kind = 'provider'"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM epg_channel"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM programme_fts"), 0, "FTS rows deleted with their programmes");
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg"), 0, "other source's links to the deleted guide cascade");
    assert_eq!(count(&r, "SELECT count(*) FROM channel_group cg WHERE NOT EXISTS (SELECT 1 FROM channel c WHERE c.id = cg.channel_id)"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM season"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM pragma_foreign_key_check"), 0);
    assert_eq!(count(&r, &format!("SELECT count(*) FROM channel WHERE source_id = {}", other.0)), 10, "other source untouched");
    let keep: HashSet<SourceId> = sources::list_sources(&r).unwrap().into_iter().map(|x| x.id).collect();
    assert_eq!(keep, HashSet::from([other]));
}

fn prog(xmltv_id: &str, start: Timestamp, title: &str) -> epg::ImportProgramme {
    epg::ImportProgramme {
        xmltv_id: xmltv_id.into(),
        start,
        stop: start + 1800,
        title: title.into(),
        sub_title: None,
        description: None,
        categories: vec![],
        icon: None,
        episode: None,
        rating: None,
        star_rating: None,
        catchup_id: None,
        is_new: false,
        is_live: false,
        previously_shown: false,
    }
}

// ------------------------------------------------------------------ write rules

/// DESIGN §6.3: `INSERT OR REPLACE` is forbidden on FK-referenced tables; the store uses none.
#[test]
fn store_sources_never_use_insert_or_replace() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store");
    let mut files = vec![];
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs" || x == "sql") {
                files.push(p);
            }
        }
    }
    assert!(files.len() >= 10);
    let re = regex::Regex::new(r"(?i)\b(insert\s+or\s+replace|replace\s+into)\b").unwrap();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with("--") {
                continue;
            }
            assert!(!re.is_match(code), "{}:{}: {line}", f.display(), n + 1);
        }
    }
}

/// Movies of one playlist with their provider category names as imported (`raw_name`, what tag
/// detection reads): `(key, name, category)`.
fn import_movies_raw(s: &Store, src: SourceId, movies: &[(&str, &str, &str)], now: Timestamp) {
    import_raw(s, src, movies, &[], now);
}

/// [`import_movies_raw`] with series too: `(key, name, raw category name)` each.
fn import_raw(s: &Store, src: SourceId, movies: &[(&str, &str, &str)], shows: &[(&str, &str, &str)], now: Timestamp) {
    let mut groups: Vec<ImportGroup> = Vec::new();
    for (kind, raw) in movies.iter().map(|m| (ItemKind::Movie, m.2)).chain(shows.iter().map(|x| (ItemKind::Series, x.2))) {
        if !groups.iter().any(|g| g.kind == kind && g.key == group_key(kind, raw)) {
            groups.push(ImportGroup {
                adult: false,
                kind,
                key: group_key(kind, raw),
                name: raw.to_owned(),
                raw_name: Some(raw.to_owned()),
                provider_id: None,
                provider_order: groups.len() as i32,
            });
        }
    }
    let films: Vec<ImportMovie> = movies.iter().enumerate().map(|(i, (key, name, raw))| listed(key, name, raw, i as i32, None)).collect();
    let shows: Vec<ImportSeries> = shows.iter().enumerate().map(|(i, (key, name, raw))| series(key, name, raw, i as i32)).collect();
    let kinds: Vec<ItemKind> = if shows.is_empty() { vec![ItemKind::Movie] } else { vec![ItemKind::Movie, ItemKind::Series] };
    w(s, move |c| {
        let imp = import::begin_catalogue(c, src, now)?;
        import::upsert_groups(c, &imp, &groups)?;
        import::write_movies(c, &imp, &films)?;
        import::write_series(c, &imp, &shows)?;
        import::finish_catalogue(c, &imp, &kinds, now)
    });
}

/// The English set v0.1.0's picker saved ("show my country (English)", field report).
fn english_rule() -> CategoryFilter {
    CategoryFilter {
        keep: ["AU", "CA", "GB", "IE", "NZ", "US"].into_iter().map(CategoryTag::country).chain([CategoryTag::language("en")]).collect(),
        keep_untagged: true,
        ..Default::default()
    }
}

/// The movies a snapshot shows, by key.
fn shown_movies(lib: &library::Library) -> HashSet<String> {
    lib.vod.movies.iter().map(|m| m.key.0.clone()).collect()
}

/// Field report: "When I view Movies I see stacked cards from other languages when my settings
/// clearly have show my country (English)". v0.1.0 kept the rule per playlist and only "TV" had
/// one; "TV 2" showed every country, and its foreign copies stacked with TV's English ones. The
/// rule is now shared (DESIGN §28.3): the first start adopts TV's rule for every playlist, a
/// playlist added later follows it, one playlist can still have its own, and Settings ▸ Hidden's
/// "show this country" changes the rule that hid it. Hidden copies never join a stack.
#[test]
fn one_category_rule_covers_every_playlist_and_a_new_one() {
    let dir = std::env::temp_dir().join(format!("tuner-st-shared-rule-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tuner.db");
    let (tv, tv2) = {
        let s = Store::open(&path, None).unwrap();
        let (tv, tv2) = w(&s, |c| Ok((seed::seed_source(c, "TV")?, seed::seed_source(c, "TV 2")?)));
        import_movies_raw(
            &s,
            tv,
            &[
                ("a1", "EN ✪ Dracula - 2025", "EN ✪ DRAMA"),
                ("a2", "DE ✪ Dracula - 2025", "DE ✪ FILME"),
                ("a3", "NL ✪ Heat - 1995", "NL ✪ FILMS [SUB]"),
            ],
            NOW,
        );
        import_movies_raw(
            &s,
            tv2,
            &[
                ("b1", "EN - Dracula (2025)", "EN | Movies"),
                ("b2", "DE - Dracula (2025)", "DE | Filme"),
                ("b3", "IT - Dracula (2025)", "IT ✪ FILM"),
                ("b4", "GR - Dracula (2025)", "GR ✪ ταινίες με υπότιτλους"),
                ("b5", "EN - Heat (1995)", "EN | Movies"),
            ],
            NOW,
        );
        // what v0.1.0 stored: the English rule on TV only (no "own" flag yet), nothing on TV 2
        let legacy = serde_json::json!({ "category_filter": english_rule() }).to_string();
        w(&s, move |c| Ok(c.execute("UPDATE source SET settings = ?2 WHERE id = ?1", rusqlite::params![tv, legacy])?));
        let before = library::load(&s.reader().unwrap()).unwrap();
        assert!(shown_movies(&before).contains("b2"), "v0.1.0's shape, read as it was: TV 2 unfiltered");
        assert!(before.source(tv).unwrap().own_category_filter && !before.source(tv2).unwrap().own_category_filter);
        assert!(s.close(std::time::Duration::from_secs(5)));
        (tv, tv2)
    };
    // the first start of this build adopts TV's rule as the shared one
    let s = Store::open(&path, None).unwrap();
    let r = s.reader().unwrap();
    let shared = sources::shared_category_filter(&r).unwrap().expect("TV's rule is shared");
    assert!(shared.same_choice(&english_rule()));
    let st = |id| sources::get_source(&r, id).unwrap().settings;
    assert_eq!((st(tv).own_category_filter, st(tv2).own_category_filter), (Some(false), None));
    assert!(!w(&s, |c| sources::adopt_legacy_category_filters(c)), "done once");
    // TV 2's tags are known (not "new" in the picker), and the first window says what changed, once
    assert!([CategoryTag::country("IT"), CategoryTag::country("GR")].iter().all(|t| shared.known.contains(t)), "{:?}", shared.known);
    let said = w(&s, |c| sources::take_adopted_category_rule(c)).expect("a notice");
    assert!(said.contains("from TV") && said.contains("all your playlists") && said.contains("other languages hidden"), "{said}");
    assert_eq!(w(&s, |c| sources::take_adopted_category_rule(c)), None);

    let lib = library::load(&r).unwrap();
    assert_eq!(shown_movies(&lib), ["a1", "b1", "b5"].map(String::from).into(), "TV 2's German, Italian and Greek categories hide too");
    for src in [tv, tv2] {
        assert!(lib.source(src).unwrap().category_filter.as_ref().is_some_and(|f| f.same_choice(&shared)));
    }
    let de2 = lib.vod.movie_groups.iter().find(|g| g.source_id == tv2 && g.tag == Some(CategoryTag::country("DE"))).unwrap();
    assert_eq!((de2.filter, de2.hidden), (GroupFilter::Hidden, true));
    // Dracula stacks TV's and TV 2's English copies only: no hidden copy is in any stack
    let stacks = lib.vod.stacks(true);
    let dracula = stacks.ids().find(|id| stacks.members(*id).iter().any(|i| lib.vod.movies[*i as usize].key.0 == "a1")).unwrap();
    let members: Vec<&str> = stacks.members(dracula).iter().map(|i| lib.vod.movies[*i as usize].key.0.as_str()).collect();
    assert_eq!(members, ["a1", "b1"]);
    for id in stacks.ids() {
        for i in stacks.members(id) {
            assert_eq!(stacks.lang(*i), Some("EN"), "every stacked copy is an English one");
        }
    }

    // a playlist added later follows the shared rule without being asked
    let tv3 = w(&s, |c| sources::insert_source(c, &m3u_draft("TV 3", "http://h.tv/3.m3u"), NOW));
    import_movies_raw(&s, tv3, &[("c1", "FR - Dracula (2025)", "FR | Films"), ("c2", "UK - Heat (1995)", "UK | Movies")], NOW);
    let lib = library::load(&r).unwrap();
    assert_eq!(shown_movies(&lib), ["a1", "b1", "b5", "c2"].map(String::from).into());

    // "Use a different rule for this playlist": TV 3 shows everything, the others keep the rule
    w(&s, move |c| sources::set_category_rule(c, tv3, &sources::CategoryRule::Own(None)));
    let lib = library::load(&r).unwrap();
    assert!(lib.source(tv3).unwrap().own_category_filter && lib.source(tv3).unwrap().category_filter.is_none());
    assert_eq!(shown_movies(&lib), ["a1", "b1", "b5", "c1", "c2"].map(String::from).into());
    // ... and back to the shared one
    w(&s, move |c| sources::set_category_rule(c, tv3, &sources::CategoryRule::Shared));
    assert!(!shown_movies(&library::load(&r).unwrap()).contains("c1"));

    // Settings ▸ Hidden ▸ "show Germany" on TV 2: the shared rule keeps DE, for TV too
    assert!(w(&s, move |c| sources::keep_tag(c, tv2, Some(CategoryTag::country("DE")))), "the shared rule changed");
    let lib = library::load(&r).unwrap();
    assert!(shown_movies(&lib).is_superset(&["a2", "b2"].map(String::from).into()));
    // a stale source editor keeps the rule choice
    let stale = w(&s, move |c| sources::get_source(c, tv3));
    w(&s, move |c| sources::set_category_rule(c, tv3, &sources::CategoryRule::Own(None)));
    w(&s, move |c| sources::update_source(c, &Source { name: "TV three".into(), ..stale.clone() }));
    assert_eq!(st(tv3).own_category_filter, Some(true));
    drop(r);
    assert!(s.close(std::time::Duration::from_secs(5)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Several playlists with different rules before the rule was shared: the first becomes the
/// shared rule, the others keep theirs as their own; playlists without a rule follow the shared
/// one (DESIGN §28.3).
#[test]
fn differing_old_rules_stay_per_playlist() {
    let s = store("tuner-st-adopt-differ");
    let (a, b, c3) = w(&s, |c| Ok((seed::seed_source(c, "A")?, seed::seed_source(c, "B")?, seed::seed_source(c, "C")?)));
    let fr = CategoryFilter { keep: [CategoryTag::country("FR")].into(), ..Default::default() };
    let (en, fr2) = (english_rule(), fr.clone());
    w(&s, move |c| {
        for (id, rule) in [(a, &en), (b, &fr2)] {
            let legacy = serde_json::json!({ "category_filter": rule }).to_string();
            c.execute("UPDATE source SET settings = ?2 WHERE id = ?1", rusqlite::params![id, legacy])?;
        }
        Ok(())
    });
    assert!(w(&s, |c| sources::adopt_legacy_category_filters(c)));
    let r = s.reader().unwrap();
    assert!(sources::shared_category_filter(&r).unwrap().unwrap().same_choice(&english_rule()), "seeded from the first playlist");
    let lib = library::load(&r).unwrap();
    let rule = |id| lib.source(id).unwrap();
    assert!(!rule(a).own_category_filter && rule(b).own_category_filter && !rule(c3).own_category_filter);
    assert_eq!(rule(b).category_filter.as_ref(), Some(&fr));
    assert!(rule(c3).category_filter.as_ref().unwrap().same_choice(&english_rule()));
}

/// "Also hide titles in other languages" (DESIGN §28.12), on by itself for an English-only rule:
/// titles whose own name marks another market are hidden also in kept and untagged categories —
/// a Dutch title among English ones, Québec films in "CA ✪", Arabic-subtitled films in an
/// untagged category (and its untagged and bare "SUBS" titles, by the category's market) — while
/// untagged and multi-audio titles stay, and so do a favourite and a title the user started.
/// Arabic-named categories ("VIP ✪ كأس العالم 2026", Shahid series) are Arabic and hide as
/// categories. The user's audio languages count as kept. Off, the titles all return.
#[test]
fn titles_in_other_languages_hide_but_never_favourites_or_started_ones() {
    let s = store("tuner-st-title-langs");
    let tv = w(&s, |c| seed::seed_source(c, "TV"));
    let mut movies: Vec<(String, String, String)> = vec![
        ("en1".into(), "EN ✪ Dracula - 2025".into(), "EN ✪ MULTI-SUB [2025/2026]".into()),
        ("nl1".into(), "NL ✪ Zwartboek - 2006".into(), "EN ✪ MULTI-SUB [2025/2026]".into()),
        ("qc1".into(), "QC ✪ Starbuck - 2011".into(), "CA ✪ QUEBEC FILMS".into()),
        // Arabic-named categories are Arabic (field report: about 580 series and films)
        ("vip1".into(), "AR ✪ Final - 2026".into(), "VIP ✪ كأس العالم 2026".into()),
        ("doc".into(), "Planet - 2020".into(), "الجزيرة الوثائقية".into()),
        ("multi".into(), "Babel [Multi-Audio] - 2006".into(), "OLD MOVIES".into()),
        ("plain".into(), "Heat - 1995".into(), "EN ✪ DRAMA".into()),
        ("fav".into(), "DE ✪ Lola rennt - 1998".into(), "EN ✪ DRAMA".into()),
        ("seen".into(), "FR ✪ Amélie - 2001".into(), "EN ✪ DRAMA".into()),
        ("old".into(), "Casablanca - 1942".into(), "OLD MOVIES".into()),
        // a bare subtitle mark is for its category's language
        ("subs".into(), "SUBS - Four Brothers - 2005".into(), "OLD MOVIES".into()),
        ("sv1".into(), "SE ✪ Jägarna - 1996".into(), "SVENSKA 2020 & GAMMAL".into()),
    ];
    movies.extend((0..12).map(|i| (format!("sub{i}"), format!("AR-SUBS - Film {i} ({})", 1990 + i), "OLD MOVIES".to_owned())));
    let rows: Vec<(&str, &str, &str)> = movies.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())).collect();
    let shows = [
        ("wire", "The Wire", "EN ✪ SERIES"),
        ("sh1", "4K-SH - طالع نازل", "مسلسلات شاهد الأصلية"),
        ("jinn", "Jinn - جن", "مسلسلات شاهد الأصلية"),
        ("kids", "Lamput", "مسلسلات شاهد كيدز"),
    ];
    import_raw(&s, tv, &rows, &shows, NOW);
    let r = s.reader().unwrap();
    let lang = |key: &str| r.query_row("SELECT match_lang FROM movie WHERE key = ?1", [key], |x| x.get::<_, Option<String>>(0)).unwrap();
    assert_eq!(
        (lang("nl1").as_deref(), lang("qc1").as_deref(), lang("sub0").as_deref(), lang("subs").as_deref()),
        (Some("NL"), Some("QC"), Some("AR-SUBS"), Some("SUBS"))
    );
    w(&s, move |c| {
        let list = user::default_fav_list(c)?;
        user::set_favourite(c, list, &mref(tv, "fav"), true, NOW)?;
        user::save_progress(
            c,
            &WatchProgress {
                item: mref(tv, "seen"),
                position_s: 600.0,
                duration_s: Some(5400.0),
                updated_at: NOW,
                finished: false,
                title: "Amélie".into(),
                fingerprint: Default::default(),
            },
        )?;
        sources::set_shared_category_filter(c, Some(&english_rule()))
    });
    let lib = library::load(&r).unwrap();
    let kept: HashSet<String> = ["en1", "multi", "plain", "fav", "seen"].map(String::from).into();
    assert_eq!(shown_movies(&lib), kept, "SVENSKA is Sweden, Arabic-named categories Arabic; the rest hide by title");
    assert_eq!(lib.source(tv).unwrap().language_hidden, 16, "nl1 qc1 old subs + 12 AR-SUBS");
    for name in ["VIP ✪ كأس العالم 2026", "الجزيرة الوثائقية"] {
        let g = lib.vod.movie_groups.iter().find(|g| g.name == name).unwrap();
        assert!(g.hidden && g.tag == Some(CategoryTag::language("ar")), "{name}");
    }
    let old = lib.vod.movie_groups.iter().find(|g| g.name == "OLD MOVIES").unwrap();
    assert_eq!((old.hidden, old.item_count), (false, 1), "the untagged category stays, without the titles it lost");
    // series: Arabic-named categories hide, the English one stays
    let shown_series: HashSet<String> = lib.vod.series.iter().map(|x| x.key.0.clone()).collect();
    assert_eq!(shown_series, HashSet::from(["wire".to_owned()]));
    assert!(lib.vod.series_groups.iter().filter(|g| g.name.starts_with("مسلسلات")).all(|g| g.hidden));
    // Dutch is one of the user's audio languages: Dutch titles stay
    let dutch = library::load_with(&r, &library::LoadOptions { audio_langs: vec!["nld".into()], ..Default::default() }).unwrap();
    assert!(shown_movies(&dutch).contains("nl1") && !shown_movies(&dutch).contains("qc1"));
    // turned off: only the categories filter
    let off = CategoryFilter { hide_other_languages: Some(false), ..english_rule() };
    w(&s, move |c| sources::set_shared_category_filter(c, Some(&off)));
    let lib = library::load(&r).unwrap();
    assert_eq!(lib.source(tv).unwrap().language_hidden, 0);
    let all: HashSet<String> = movies.iter().map(|m| m.0.clone()).filter(|k| !["sv1", "vip1", "doc"].contains(&k.as_str())).collect();
    assert_eq!(shown_movies(&lib), all);
}
