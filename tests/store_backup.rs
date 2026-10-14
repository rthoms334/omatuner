//! Store: backup export / file format / restore (DESIGN §19). Owner: WP-02.

mod support;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;

use rusqlite::Connection;
use tuner::model::*;
use tuner::redact::Secret;
use tuner::sources::identity::{group_key, normalize_name};
use tuner::store::backup::{self, Backup, BackupOptions, RestoreMode};
use tuner::store::import::{self, ImportChannel, ImportGroup, ImportMovie};
use tuner::store::{Store, epg, sources, user};

const NOW: Timestamp = 1_791_576_000;

fn store(name: &str) -> Store {
    Store::open_temp(name, None).expect("temp store")
}

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Credentials unique to one test (the redaction registry is process-global).
struct Creds {
    xt_pass: String,
    m3u_token: String,
    epg_key: String,
}

impl Creds {
    fn new(tag: &str) -> Self {
        Self { xt_pass: format!("xtpw-{tag}-71x"), m3u_token: format!("tok-{tag}-93q"), epg_key: format!("key-{tag}-55z") }
    }
}

struct Fixture {
    xt: SourceId,
    m3u: SourceId,
}

fn chan(key: &str, name: &str) -> ImportChannel {
    ImportChannel {
        adult: false,
        key: ItemKey(key.into()),
        provider_id: key.strip_prefix("xt:live:").map(str::to_owned),
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        number: None,
        logo: None,
        tvg_id: Some(format!("{}.tv", normalize_name(name))),
        tvg_name: None,
        tvg_shift_min: 0,
        stream: StreamLocator::Url { url: format!("http://h.test/{key}.ts") },
        url_key: None,
        headers: None,
        catchup: None,
        radio: false,
        unsupported: None,
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Channel, "News")],
        quality: None,
    }
}

fn movie(key: &str, name: &str) -> ImportMovie {
    ImportMovie {
        adult: false,
        key: ItemKey(key.into()),
        provider_id: None,
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        year: Some(1995),
        poster: None,
        rating: None,
        stream: StreamLocator::Url { url: format!("http://h.test/{key}.mkv") },
        url_key: None,
        provider_order: 0,
        added_at: None,
        groups: vec![group_key(ItemKind::Movie, "Films")],
        tmdb: None,
    }
}

fn ch(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(key.into()) }
}
fn mv(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey(key.into()) }
}

/// Two sources with a small catalogue and every kind of user data.
fn populate(s: &Store, creds: &Creds) -> Fixture {
    let (xt_pass, token, key) = (creds.xt_pass.clone(), creds.m3u_token.clone(), creds.epg_key.clone());
    w(s, move |c| {
        let xt = sources::insert_source(
            c,
            &SourceDraft {
                name: "Panel".into(),
                kind: SourceKind::Xtream,
                icon: "a".into(),
                color: SourceColor::Magenta,
                url: None,
                path: None,
                xtream: Some(XtreamCredentials {
                    server: "http://panel.test:8080".into(),
                    username: "viewer".into(),
                    password: Secret::new(xt_pass),
                }),
                settings: SourceSettings {
                    refresh_h: Some(6),
                    category_filter: Some(CategoryFilter {
                        keep: [CategoryTag::country("GB"), CategoryTag::language("en")].into(),
                        keep_untagged: false,
                        known: [CategoryTag::country("GB"), CategoryTag::country("FR")].into(),
                        ..Default::default()
                    }),
                    // its own rule (DESIGN §28.3); the other playlist follows the shared one
                    own_category_filter: Some(true),
                    ..Default::default()
                },
            },
            NOW,
        )?;
        let m3u = sources::insert_source(
            c,
            &SourceDraft {
                name: "Playlist".into(),
                kind: SourceKind::M3uUrl,
                icon: "b".into(),
                color: SourceColor::Green,
                url: Some(format!("http://cdn.test/list.m3u?token={token}")),
                path: None,
                xtream: None,
                settings: SourceSettings::default(),
            },
            NOW,
        )?;
        sources::set_shared_category_filter(
            c,
            Some(&CategoryFilter { keep: [CategoryTag::country("US")].into(), hide_other_languages: Some(true), ..Default::default() }),
        )?;
        let guide =
            sources::insert_epg_source(c, "My guide", &EpgSourceKind::Url { url: Secret::new(format!("http://epg.test/g.xml?apikey={key}")) }, -30)?;
        sources::insert_epg_source(c, "Local file", &EpgSourceKind::File { path: "/tmp/guide.xml".into() }, 0)?;

        for (src, chans, movies) in [
            (xt, vec![chan("xt:live:1", "BBC One"), chan("xt:live:2", "ITV")], vec![movie("xt:vod:7", "Heat")]),
            (m3u, vec![chan("m3u:aa", "Arte")], vec![movie("m3u:mv1", "Heat (playlist)")]),
        ] {
            let imp = import::begin_catalogue(c, src, NOW)?;
            let groups = [
                ImportGroup {
                    adult: false,
                    kind: ItemKind::Channel,
                    key: group_key(ItemKind::Channel, "News"),
                    name: "News".into(),
                    raw_name: None,
                    provider_id: None,
                    provider_order: 0,
                },
                ImportGroup {
                    adult: false,
                    kind: ItemKind::Movie,
                    key: group_key(ItemKind::Movie, "Films"),
                    name: "Films".into(),
                    raw_name: None,
                    provider_id: None,
                    provider_order: 0,
                },
            ];
            import::upsert_groups(c, &imp, &groups)?;
            import::write_channels(c, &imp, &chans)?;
            import::write_movies(c, &imp, &movies)?;
            import::finish_catalogue(c, &imp, &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series], NOW)?;
        }

        let fav = user::default_fav_list(c)?;
        user::rename_fav_list(c, fav, "Mine", "*")?;
        let kids = tuner::store::watchlist::create(c, "Kids", None, tuner::watchlist::PlayOrder::Any, "app", NOW)?;
        user::set_favourite(c, fav, &ch(xt, "xt:live:2"), true, NOW)?;
        user::set_favourite(c, fav, &ch(xt, "xt:live:1"), true, NOW + 1)?;
        user::set_favourite(c, fav, &ch(m3u, "m3u:aa"), true, NOW + 2)?;
        tuner::store::watchlist::add(
            c,
            &tuner::store::library::load(c)?,
            kids,
            &[tuner::store::watchlist::new_entry(c, &mv(xt, "xt:vod:7"))?],
            tuner::watchlist::Insert::End,
            NOW + 3,
        )?;
        user::set_item_override(
            c,
            &ch(xt, "xt:live:1"),
            &ItemOverride { rename: Some("BBC 1".into()), hidden: false, locked: true, user_order: Some(3), catchup_offset_s: Some(-60) },
        )?;
        user::set_item_override(c, &mv(xt, "xt:vod:7"), &ItemOverride { hidden: true, ..Default::default() })?;
        user::set_group_override(
            c,
            m3u,
            ItemKind::Channel,
            &group_key(ItemKind::Channel, "News"),
            &GroupOverride {
                rename: Some("Nachrichten".into()),
                hidden: false,
                locked: false,
                user_order: Some(1),
                pinned_home: true,
                shown: true,
                explicit: Some(false),
            },
        )?;
        user::save_progress(
            c,
            &WatchProgress {
                item: mv(xt, "xt:vod:7"),
                position_s: 1234.5,
                duration_s: Some(6000.0),
                updated_at: NOW + 50,
                finished: false,
                title: "Heat".into(),
                fingerprint: Fingerprint::default(),
            },
        )?;
        for (i, item) in [ch(xt, "xt:live:1"), ch(m3u, "m3u:aa"), ch(xt, "xt:live:2")].into_iter().enumerate() {
            user::push_history(c, &HistoryEntry { item, at: NOW + 100 + i as i64, title: format!("h{i}") })?;
        }
        user::add_reminder(
            c,
            &Reminder {
                id: ReminderId(0),
                channel: ch(xt, "xt:live:2"),
                channel_name: "ITV".into(),
                programme_title: "Quiz".into(),
                start: NOW + 3600,
                stop: NOW + 7200,
                lead_s: 300,
                state: ReminderState::Pending,
                mode: Default::default(),
                offset_s: 0,
                open_in: None,
                rule_id: None,
            },
        )?;
        // Guide fixes: a mapping an agent picked (with the guide channel's name), your time shift
        // and an "It's right"
        epg::set_manual_mapping_as(c, &ch(m3u, "m3u:aa"), guide, "arte.de", Some("Arte HD"), FixActor::Agent)?;
        let fixes = Proposal {
            changes: vec![
                FixChange { channel: Some(ch(xt, "xt:live:1")), shift_min: Some(Some(60)), ..Default::default() },
                FixChange { channel: Some(ch(xt, "xt:live:2")), dismiss: Some(IssueKind::LooksWrong), ..Default::default() },
            ],
            ..Default::default()
        };
        tuner::store::guide_fix::apply(c, FixActor::You, &Default::default(), "t", &fixes, &Default::default(), NOW)?;
        // "Not the same title" (DESIGN §29.11): the split survives a restore
        tuner::store::titles::set_split(c, &mv(xt, "xt:vod:7"), true, NOW + 4)?;
        // "Same title as…" (DESIGN §29.16): the join (both copies, one group) survives too
        tuner::store::titles::join(c, &mv(xt, "xt:vod:7"), &mv(m3u, "m3u:mv1"), NOW + 5)?;
        Ok(Fixture { xt, m3u })
    })
}

/// Backup with install-specific ids and timestamps replaced by stable positions, so two
/// exports of equivalent data from different databases compare equal.
fn normalized(mut b: Backup) -> Backup {
    let sources: HashMap<i64, i64> = b.sources.iter().enumerate().map(|(i, s)| (s.id, i as i64 + 1)).collect();
    let lists: HashMap<FavListId, FavListId> = b.fav_lists.iter().enumerate().map(|(i, l)| (l.id, FavListId(i as i64 + 1))).collect();
    let src = |id: SourceId| SourceId(sources[&id.0]);
    b.created_at = 0;
    b.sources.iter_mut().for_each(|s| s.id = sources[&s.id]);
    b.fav_lists.iter_mut().for_each(|l| l.id = lists[&l.id]);
    b.fav_items.iter_mut().for_each(|e| {
        e.list = lists[&e.list];
        e.item.source = src(e.item.source);
    });
    b.item_overrides.iter_mut().for_each(|o| o.item.source = src(o.item.source));
    b.group_overrides.iter_mut().for_each(|g| g.source = src(g.source));
    b.progress.iter_mut().for_each(|p| p.item.source = src(p.item.source));
    b.history.iter_mut().for_each(|h| h.item.source = src(h.item.source));
    b.reminders.iter_mut().for_each(|r| {
        r.id = ReminderId(0);
        r.channel.source = src(r.channel.source);
    });
    b.manual_mappings.iter_mut().for_each(|m| m.source_id = sources[&m.source_id]);
    b.title_links.iter_mut().for_each(|t| t.item.source = src(t.item.source));
    for c in b.features.guide_fix.iter_mut().flat_map(|v| v["channels"].as_array_mut().unwrap()) {
        c["item"]["source"] = sources[&c["item"]["source"].as_i64().unwrap()].into();
    }
    if let Some(w) = b.features.watchlists.as_mut() {
        let ids: HashMap<i64, i64> =
            w["lists"].as_array().unwrap().iter().enumerate().map(|(i, l)| (l["id"].as_i64().unwrap(), i as i64 + 1)).collect();
        for l in w["lists"].as_array_mut().unwrap() {
            l["id"] = ids[&l["id"].as_i64().unwrap()].into();
        }
        for (i, e) in w["entries"].as_array_mut().unwrap().iter_mut().enumerate() {
            e["id"] = (i as i64 + 1).into();
            e["list_id"] = ids[&e["list_id"].as_i64().unwrap()].into();
            if let Some(source) = e["item"]["source"].as_i64() {
                e["item"]["source"] = sources[&source].into();
            }
        }
    }
    b
}

const FULL: BackupOptions = BackupOptions { include_credentials: true, include_history: true };

#[test]
fn export_wipe_restore_round_trip_is_lossless() {
    let creds = Creds::new("rt");
    let s = store("tuner-sb-roundtrip");
    populate(&s, &creds);
    let b1 = w(&s, |c| backup::export(c, FULL, Some("[general]\nui_scale = 1.25\n".into()), NOW));
    assert_eq!((b1.format.as_str(), b1.version), (backup::FORMAT, backup::VERSION));
    assert_eq!(b1.sources.len(), 2);
    assert_eq!(b1.sources[0].xt_password.as_deref(), Some(creds.xt_pass.as_str()));
    assert_eq!(b1.epg_sources.len(), 2);
    assert_eq!((b1.fav_lists.len(), b1.fav_items.len()), (1, 3));
    assert_eq!((b1.item_overrides.len(), b1.group_overrides.len(), b1.progress.len()), (3, 1, 1));
    assert_eq!((b1.history.len(), b1.reminders.len(), b1.manual_mappings.len()), (3, 1, 1));
    let m = &b1.manual_mappings[0];
    assert_eq!((m.epg_source_name.as_str(), m.epg_name.as_deref(), m.actor.as_deref()), ("My guide", Some("Arte HD"), Some("claude")));
    let fixes = b1.features.guide_fix.as_ref().expect("the time shift and the dismissal");
    assert_eq!(fixes["channels"].as_array().map(Vec::len), Some(2));
    let actions: Vec<(&str, Option<i64>)> = b1.title_links.iter().map(|t| (t.action.as_str(), t.group)).collect();
    assert_eq!(actions, [("join", Some(1)), ("join", Some(1)), ("split", None)]);
    assert!(b1.fav_items.iter().all(|e| !e.fingerprint.norm_name.is_empty()), "fingerprints travel with the data");

    // file round trip, written 0600
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tuner-backup.json");
    backup::write_file(&path, &b1).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!dir.path().join("tuner-backup.json.part").exists());
    let read = backup::read_file(&path).unwrap();
    assert_eq!(read, b1);

    // wipe: Replace into the same database, and restore into a brand-new profile
    let fresh = store("tuner-sb-roundtrip-fresh");
    for target in [&s, &fresh] {
        let b = read.clone();
        let report = w(target, move |c| backup::restore(c, &b, RestoreMode::Replace));
        assert_eq!((report.sources_added, report.sources_matched), (2, 0));
        assert!(report.needs_credentials.is_empty());
        assert!(report.user_rows >= 14, "{report:?}");
        let b2 = w(target, |c| backup::export(c, FULL, Some("[general]\nui_scale = 1.25\n".into()), NOW + 99));
        assert_eq!(normalized(b2), normalized(b1.clone()));
        let r = target.reader().unwrap();
        // catalogue rows are not part of a backup: they come back with the next refresh
        assert_eq!(count(&r, "SELECT count(*) FROM channel"), 0);
        assert_eq!(count(&r, "SELECT count(*) FROM fav_list"), 1);
        assert_eq!(count(&r, "SELECT count(*) FROM epg_source WHERE kind = 'provider'"), 0);
        assert_eq!(count(&r, "SELECT count(*) FROM pragma_foreign_key_check"), 0);
        assert_eq!(count(&r, "SELECT count(*) FROM source"), 2, "Replace does not keep the old sources next to the restored ones");
        assert_eq!(count(&r, "SELECT count(*) FROM fav_item"), 3);
    }
}

#[test]
fn export_without_credentials_strips_secrets_and_restores_disabled_sources() {
    let creds = Creds::new("nocred");
    let s = store("tuner-sb-nocred");
    populate(&s, &creds);
    // a provider guide (credentials in its URL) is not exported at all
    let xt = w(&s, |c| Ok(sources::list_sources(c)?[0].id));
    let pass = creds.xt_pass.clone();
    w(&s, move |c| sources::ensure_provider_epg(c, xt, &[format!("http://panel.test:8080/xmltv.php?username=viewer&password={pass}")]));
    let b = w(&s, |c| backup::export(c, BackupOptions::default(), None, NOW));
    let json = serde_json::to_string(&b).unwrap();
    for secret in [&creds.xt_pass, &creds.m3u_token, &creds.epg_key] {
        assert!(!json.contains(secret.as_str()), "{secret} leaked into the backup");
    }
    assert_eq!(b.sources[0].xt_password, None);
    assert_eq!(b.sources[0].xt_username.as_deref(), Some("viewer"));
    assert_eq!(b.sources[1].url.as_deref(), Some("http://cdn.test/list.m3u?token=***"));
    assert!(b.history.is_empty(), "history is opt-in");
    assert_eq!(b.config, None);
    assert!(b.epg_sources.iter().all(|e| e.kind != "provider"));

    let fresh = store("tuner-sb-nocred-fresh");
    let report = w(&fresh, move |c| backup::restore(c, &b, RestoreMode::Merge));
    assert_eq!(report.sources_added, 2);
    assert_eq!(report.needs_credentials, vec!["Panel".to_string(), "Playlist".to_string()]);
    let r = fresh.reader().unwrap();
    // never refreshed with placeholder credentials: restored disabled
    assert!(sources::list_sources(&r).unwrap().iter().all(|s| !s.enabled));
    let guides = sources::list_epg_sources(&r).unwrap();
    let mine = guides.iter().find(|g| g.name == "My guide").unwrap();
    assert!(!mine.enabled, "a guide URL with a masked key is restored disabled");
    assert!(guides.iter().find(|g| g.name == "Local file").unwrap().enabled);
    // user data is there, keyed by the new source ids
    assert_eq!(count(&r, "SELECT count(*) FROM fav_item"), 3);
    assert_eq!(count(&r, "SELECT count(*) FROM watch_progress"), 1);
    assert_eq!(count(&r, "SELECT count(*) FROM history"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg_map WHERE method = 'manual'"), 1);
}

/// Stripping masks credentials only: harmless query values survive even though every source
/// URL's query values are registered with the (log) redaction registry.
#[test]
fn export_without_credentials_keeps_non_secret_url_parts() {
    let s = store("tuner-sb-keepq");
    let urls = [
        ("Public", "https://pub.example/list?country=zz-keepq"),
        ("Mixed", "https://pub.example/list?region=yy-keepq&token=tk-keepq-81w"),
        ("Userinfo", "https://bob:pw-keepq-19r@pub.example/list.m3u?lang=xx-keepq"),
        ("Path", "http://pub.example:8080/live/bob/pw-keepq-27t/101.ts?quality=hd"),
    ];
    w(&s, move |c| {
        for (name, url) in urls {
            let draft = SourceDraft {
                name: name.into(),
                kind: SourceKind::M3uUrl,
                icon: "x".into(),
                color: SourceColor::Blue,
                url: Some(url.into()),
                path: None,
                xtream: None,
                settings: SourceSettings::default(),
            };
            sources::insert_source(c, &draft, NOW)?;
        }
        Ok(())
    });
    let b = w(&s, |c| backup::export(c, BackupOptions::default(), None, NOW));
    let got: Vec<_> = b.sources.iter().map(|s| s.url.as_deref().unwrap()).collect();
    assert_eq!(
        got,
        vec![
            "https://pub.example/list?country=zz-keepq",
            "https://pub.example/list?region=yy-keepq&token=***",
            "https://***@pub.example/list.m3u?lang=xx-keepq",
            "http://pub.example:8080/live/***/***/101.ts?quality=hd",
        ]
    );
    let fresh = store("tuner-sb-keepq-fresh");
    let report = w(&fresh, move |c| backup::restore(c, &b, RestoreMode::Merge));
    assert_eq!(report.needs_credentials, vec!["Mixed".to_string(), "Userinfo".to_string(), "Path".to_string()]);
    let r = fresh.reader().unwrap();
    let public = sources::list_sources(&r).unwrap().into_iter().find(|s| s.name == "Public").unwrap();
    assert!(public.enabled, "nothing was masked: restored enabled");
    assert_eq!(public.url.unwrap().expose(), "https://pub.example/list?country=zz-keepq");
}

#[test]
fn merge_restore_matches_sources_and_keeps_newer_local_data() {
    let creds = Creds::new("merge");
    let s = store("tuner-sb-merge");
    let fx = populate(&s, &creds);
    let b = w(&s, |c| backup::export(c, FULL, None, NOW));
    let r = s.reader().unwrap();
    let before: Vec<i64> = [
        "watchlist",
        "watchlist_item",
        "fav_item",
        "fav_list",
        "item_override",
        "group_override",
        "reminder",
        "channel_epg_map",
        "source",
        "epg_source",
    ]
    .iter()
    .map(|t| count(&r, &format!("SELECT count(*) FROM {t}")))
    .collect();

    // local progress moved on since the backup; another item has older local progress
    let (xt, m3u) = (fx.xt, fx.m3u);
    w(&s, move |c| {
        let base = WatchProgress {
            item: mv(xt, "xt:vod:7"),
            position_s: 4000.0,
            duration_s: Some(6000.0),
            updated_at: NOW + 500,
            finished: false,
            title: "Heat".into(),
            fingerprint: Fingerprint::default(),
        };
        user::save_progress(c, &base)?;
        user::set_favourite(c, user::default_fav_list(c)?, &ch(m3u, "m3u:aa"), false, NOW)
    });
    let mut b2 = b.clone();
    b2.progress.push(WatchProgress {
        item: ch(xt, "xt:live:1"),
        position_s: 1.0,
        duration_s: None,
        updated_at: NOW,
        finished: true,
        title: "x".into(),
        fingerprint: Fingerprint::default(),
    });
    let report = w(&s, move |c| backup::restore(c, &b2, RestoreMode::Merge));
    assert_eq!((report.sources_added, report.sources_matched), (0, 2));
    let after: Vec<i64> = [
        "watchlist",
        "watchlist_item",
        "fav_item",
        "fav_list",
        "item_override",
        "group_override",
        "reminder",
        "channel_epg_map",
        "source",
        "epg_source",
    ]
    .iter()
    .map(|t| count(&r, &format!("SELECT count(*) FROM {t}")))
    .collect();
    assert_eq!(after, before, "nothing duplicated; the removed favourite came back");
    let p = user::progress(&r, &mv(xt, "xt:vod:7")).unwrap().unwrap();
    assert_eq!(p.position_s, 4000.0, "newer local progress wins");
    assert!(user::progress(&r, &ch(xt, "xt:live:1")).unwrap().is_some());
    // a matched source keeps its id and credentials
    assert_eq!(sources::get_source(&r, xt).unwrap().xtream.unwrap().password.expose(), &creds.xt_pass);
    assert_eq!(count(&r, "SELECT count(*) FROM history"), 3, "known history entries are not duplicated");
}

#[test]
fn read_file_validates_format_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        p
    };
    let s = store("tuner-sb-read");
    let mut b = w(&s, |c| backup::export(c, BackupOptions::default(), None, NOW));
    let ok = dir.path().join("ok.json");
    backup::write_file(&ok, &b).unwrap();
    assert!(backup::read_file(&ok).is_ok());

    for (name, text) in [
        ("garbage.json", "this is not json"),
        ("other.json", r#"{"format":"something-else","version":1}"#),
        ("newer.json", r#"{"format":"tuner-backup","version":2}"#),
        ("zero.json", r#"{"format":"tuner-backup","version":0}"#),
        ("partial.json", r#"{"format":"tuner-backup","version":1,"sources":[]}"#),
    ] {
        assert!(backup::read_file(&write(name, text)).is_err(), "{name}");
    }
    assert!(backup::read_file(&dir.path().join("missing.json")).is_err());
    // an existing, more permissive file is replaced by a 0600 one
    let wide = write("wide.json", "{}");
    std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o644)).unwrap();
    backup::write_file(&wide, &b).unwrap();
    assert_eq!(std::fs::metadata(&wide).unwrap().permissions().mode() & 0o777, 0o600);
    // restore refuses documents that did not come through read_file's checks
    b.version = 99;
    assert!(s.writer().blocking(move |c| backup::restore(c, &b, RestoreMode::Merge)).is_err());
}

/// Phase 5 (§27.10): a playlist upgraded to its Xtream login keeps its playlist URL / file and
/// `keep_m3u` through a backup, and a Merge restore pairs it with the local source even when
/// one side has switched kinds since (upgraded after the backup, or switched back).
#[test]
fn upgraded_sources_survive_a_backup_and_merge_across_the_switch() {
    let pass = "xtpw-upgraded-62r";
    let s = store("tuner-sb-upgraded");
    let draft = |name: &str, kind, url: Option<String>, path: Option<&str>, keep_m3u| SourceDraft {
        name: name.into(),
        kind,
        icon: "a".into(),
        color: SourceColor::Blue,
        url,
        path: path.map(Into::into),
        xtream: (kind == SourceKind::Xtream).then(|| XtreamCredentials {
            server: "http://panel.test/sub".into(),
            username: "viewer".into(),
            password: Secret::new(pass.into()),
        }),
        settings: SourceSettings { keep_m3u, ..Default::default() },
    };
    let get_php = format!("http://panel.test/sub/get.php?username=viewer&password={pass}&type=m3u_plus");
    let (upgraded, kept) = (
        draft("Provider", SourceKind::Xtream, Some(get_php.clone()), None, false),
        draft("Provider file", SourceKind::M3uFile, None, Some("/media/lists/provider.m3u"), true),
    );
    w(&s, move |c| {
        sources::insert_source(c, &upgraded, NOW)?;
        sources::insert_source(c, &kept, NOW)
    });
    let full = w(&s, |c| backup::export(c, FULL, None, NOW));
    assert_eq!(full.sources[0].url.as_deref(), Some(get_php.as_str()), "the playlist URL of an upgraded source is exported");
    assert!(full.sources[1].settings.keep_m3u);
    let stripped = w(&s, |c| backup::export(c, BackupOptions::default(), None, NOW));

    // restored elsewhere: same kinds, playlist URL / file and keep_m3u
    let fresh = store("tuner-sb-upgraded-fresh");
    let b = full.clone();
    w(&fresh, move |c| backup::restore(c, &b, RestoreMode::Replace));
    let restored = sources::list_sources(&fresh.reader().unwrap()).unwrap();
    assert_eq!(restored[0].kind, SourceKind::Xtream);
    assert_eq!(restored[0].m3u_origin(), Some(SourceKind::M3uUrl));
    assert_eq!(restored[0].url.as_ref().unwrap().expose(), &get_php);
    assert_eq!(restored[1].path.as_deref(), Some(std::path::Path::new("/media/lists/provider.m3u")));
    assert!(restored[1].settings.keep_m3u);

    // merged into the original profile: both pair up, with or without the passwords
    for b in [full.clone(), stripped] {
        let report = w(&s, move |c| backup::restore(c, &b, RestoreMode::Merge));
        assert_eq!((report.sources_added, report.sources_matched), (0, 2));
    }
    // the first switched back to its playlist and the file got upgraded since the backup
    w(&s, |c| {
        c.execute_batch(
            "UPDATE source SET kind = 'm3u-url', xt_server = NULL, xt_username = NULL, xt_password = NULL WHERE name = 'Provider';
             UPDATE source SET kind = 'xtream', xt_server = 'http://panel.test', xt_username = 'viewer', xt_password = 'p'
             WHERE name = 'Provider file';",
        )?;
        Ok(())
    });
    let report = w(&s, move |c| backup::restore(c, &full, RestoreMode::Merge));
    assert_eq!((report.sources_added, report.sources_matched), (0, 2), "no duplicate sources");
}

/// DESIGN §42 §8.11: a pre-watchlists backup restored on a newer build follows the same
/// channel/VOD split as the migration, preserving orphan fingerprints and list order.
#[test]
fn restoring_legacy_favourites_moves_films_to_watchlists() {
    let original = store("watchlists-legacy-backup");
    let fx = populate(&original, &Creds::new("legacy-watchlists"));
    let mut old = w(&original, |c| backup::export(c, FULL, None, NOW));
    old.features.watchlists = None;
    old.fav_items.push(FavouriteEntry {
        list: old.fav_lists[0].id,
        item: mv(fx.xt, "gone"),
        position: 3,
        added_at: NOW,
        fingerprint: Fingerprint { norm_name: "missingfilm".into(), ..Default::default() },
        orphaned_since: Some(NOW - 100),
    });
    let restored = store("watchlists-legacy-restored");
    w(&restored, move |c| backup::restore(c, &old, RestoreMode::Replace));
    let c = restored.reader().unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM fav_item WHERE kind<>'channel'"), 0);
    let mine = tuner::store::watchlist::default_list(&c).unwrap();
    let rows = tuner::store::watchlist::entries(&c, mine).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].position, 0);
    assert_eq!(rows[0].fingerprint.norm_name, "missingfilm");
    assert_eq!(rows[0].orphaned_since, Some(NOW - 100));
    assert_eq!(tuner::store::watchlist::moved_card(&c).unwrap(), ["My list"]);
    assert!(
        std::fs::read_dir(restored.path().parent().unwrap()).unwrap().any(|p| p.unwrap().file_name().to_string_lossy().contains("before-watchlists")),
        "a backup precedes the one-way restore move"
    );
}
