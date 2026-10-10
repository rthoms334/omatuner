//! Store: EPG generations (import, flip, sweep, abort), FTS consistency, grid / now-next /
//! search reads and channel → guide mapping resolution. Owner: WP-02.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tuner::model::*;
use tuner::redact::Secret;
use tuner::sources::identity::{group_key, normalize_name};
use tuner::store::epg::{self, EpgImportSummary, ImportEpgChannel, ImportProgramme, ProgrammeScope};
use tuner::store::import::{self, ImportChannel, ImportGroup};
use tuner::store::{Store, library, sources};

/// On the hour (all slots below are aligned to it).
const T0: Timestamp = 1_791_576_000;
const H: i64 = 3600;

fn store(name: &str) -> Store { Store::open_temp(name, None).expect("temp store") }

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn count(c: &Connection, sql: &str) -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() }

/// Programme and FTS rows agree: every FTS row belongs to a programme and vice versa.
fn assert_fts_consistent(c: &Connection) {
    assert_eq!(count(c, "SELECT count(*) FROM programme_fts WHERE rowid NOT IN (SELECT id FROM programme)"), 0, "orphan FTS rows");
    assert_eq!(count(c, "SELECT count(*) FROM programme_fts"), count(c, "SELECT count(*) FROM programme"), "FTS row per programme");
}

fn url_guide(s: &Store, name: &str, offset_min: i32) -> EpgSourceId {
    let name = name.to_owned();
    w(s, move |c| sources::insert_epg_source(c, &name, &EpgSourceKind::Url { url: Secret::new(format!("http://e.test/{name}.xml")) }, offset_min))
}

fn echan(id: &str, names: &[&str]) -> ImportEpgChannel {
    ImportEpgChannel { xmltv_id: id.into(), display_names: names.iter().map(|n| (n.to_string(), normalize_name(n))).collect(), icon: None }
}

fn prog(xmltv_id: &str, start: Timestamp, stop: Timestamp, title: &str) -> ImportProgramme {
    ImportProgramme {
        xmltv_id: xmltv_id.into(),
        start,
        stop,
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

/// `n` one-hour programmes per channel from `from`, titled `"{prefix} {i}"`.
fn hourly(ids: &[&str], from: Timestamp, n: i64, prefix: &str) -> Vec<ImportProgramme> {
    ids.iter().flat_map(|id| (0..n).map(move |i| prog(id, from + i * H, from + (i + 1) * H, &format!("{prefix} {i}")))).collect()
}

/// A complete EPG import: channels, programmes in batches of 5 000, flip, sweep, FTS compaction
/// (as `jobs::import_epg` does; later FTS checks and searches run on the compacted index).
fn import_guide(s: &Store, src: EpgSourceId, chans: Vec<ImportEpgChannel>, progs: Vec<ImportProgramme>, now: Timestamp) -> EpgImportSummary {
    w(s, move |c| {
        let mut imp = epg::begin_epg_import(c, src)?;
        epg::write_epg_channels(c, &mut imp, &chans)?;
        for b in progs.chunks(5_000) {
            epg::write_programmes(c, &mut imp, b)?;
        }
        let sum = epg::finish_epg_import(c, &imp, now)?;
        while epg::sweep_old_generations(c, src, 20_000)? > 0 {}
        while epg::merge_fts_step(c)? {}
        Ok(sum)
    })
}

fn epg_id(c: &Connection, src: EpgSourceId, xmltv_id: &str) -> EpgChannelId {
    c.query_row("SELECT id FROM epg_channel WHERE epg_source_id = ?1 AND xmltv_id = ?2", rusqlite::params![src, xmltv_id], |r| r.get(0))
        .unwrap()
}

/// A playlist source with one channel per `(key, name, tvg_id, tvg_shift)`.
fn playlist(s: &Store, chans: &[(&str, &str, Option<&str>, i32)]) -> SourceId {
    let chans: Vec<ImportChannel> = chans
        .iter()
        .enumerate()
        .map(|(i, (key, name, tvg, shift))| ImportChannel {
            key: ItemKey((*key).into()),
            provider_id: None,
            name: (*name).into(),
            clean_name: (*name).into(),
            norm_name: normalize_name(name),
            number: Some(i as u32 + 1),
            logo: None,
            tvg_id: tvg.map(str::to_owned),
            tvg_name: None,
            tvg_shift_min: *shift,
            stream: StreamLocator::Url { url: format!("http://h.test/{key}.ts") },
            url_key: None,
            headers: None,
            catchup: None,
            radio: false,
            unsupported: None,
            provider_order: i as i32,
            added_at: None,
            groups: vec![group_key(ItemKind::Channel, "All")],
        })
        .collect();
    w(s, move |c| {
        let src = sources::insert_source(
            c,
            &SourceDraft {
                name: "P".into(),
                kind: SourceKind::M3uUrl,
                icon: "x".into(),
                color: SourceColor::Blue,
                url: Some("http://h.test/p.m3u".into()),
                path: None,
                xtream: None,
                settings: SourceSettings::default(),
            },
            T0,
        )?;
        let imp = import::begin_catalogue(c, src, T0)?;
        let g = ImportGroup { kind: ItemKind::Channel, key: group_key(ItemKind::Channel, "All"), name: "All".into(), provider_id: None, provider_order: 0 };
        import::upsert_groups(c, &imp, &[g])?;
        import::write_channels(c, &imp, &chans)?;
        import::finish_catalogue(c, &imp, &[ItemKind::Channel], T0)?;
        Ok(src)
    })
}

fn auto_map(s: &Store, src: SourceId, guide: EpgSourceId, maps: &[(&str, &str, EpgMatchMethod)]) {
    let maps: Vec<EpgMapping> = maps
        .iter()
        .map(|(key, xmltv, method)| EpgMapping {
            channel: ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey((*key).into()) },
            epg_source_id: guide,
            xmltv_id: (*xmltv).into(),
            method: *method,
            confidence: 1.0,
        })
        .collect();
    w(s, move |c| {
        epg::replace_auto_mappings(c, src, guide, &maps)?;
        epg::resolve_mappings(c)
    });
}

fn channel_id(c: &Connection, src: SourceId, key: &str) -> ChannelId {
    c.query_row("SELECT id FROM channel WHERE source_id = ?1 AND key = ?2", rusqlite::params![src, key], |r| r.get(0)).unwrap()
}

fn ch_ref(src: SourceId, key: &str) -> ItemRef { ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(key.into()) } }

fn titles(v: &[ProgrammeSlim]) -> Vec<&str> { v.iter().map(|p| &*p.title).collect() }

// ------------------------------------------------------------------ generations

#[test]
fn epg_import_writes_flips_and_sweeps_generations() {
    let s = store("tuner-se-gen");
    let g = url_guide(&s, "G", 60);
    let mut news = prog("bbc1.uk", T0, T0 + 1800, "News");
    news.sub_title = Some("Late edition".into());
    news.description = Some("Headlines".into());
    news.categories = vec!["News".into(), "Current affairs".into()];
    news.episode = Some(EpisodeNum { season: Some(2), episode: Some(5) });
    news.catchup_id = Some("c1".into());
    news.is_new = true;
    news.previously_shown = true;
    let progs = vec![
        news,
        prog("bbc1.uk", T0 + 1800, T0 + H, "Weather"),
        prog("bbc1.uk", T0, T0 + 900, "Duplicate start"), // same (channel, start): first wins
        prog("itv.uk", T0, T0 + H, "Quiz"),
        prog("ghost.tv", T0, T0 + 2 * H, "Undeclared channel"), // channel created on the fly
    ];
    let sum = import_guide(&s, g, vec![echan("bbc1.uk", &["BBC One", "BBC 1"]), echan("itv.uk", &["ITV"])], progs, T0);
    assert_eq!(sum, EpgImportSummary { channels: 3, programmes: 4, max_stop: Some(T0 + 3 * H), mapped_channels: 0 });

    let r = s.reader().unwrap();
    let src = sources::get_epg_source(&r, g).unwrap();
    assert_eq!((src.active_generation, src.channel_count, src.programme_count, src.max_stop), (1, 3, 4, Some(T0 + 3 * H)));
    assert_eq!(src.last_refresh, Some(T0));
    let bbc = epg_id(&r, g, "bbc1.uk");
    let ch = epg::epg_channel(&r, bbc).unwrap();
    assert_eq!(ch.display_names.len(), 2);
    assert_eq!(epg::epg_channels(&r, g).unwrap().len(), 3);

    // the source offset (+60 min) is applied at write: reads are in EPG time
    let win = epg::window(&r, bbc, T0 + H, T0 + 2 * H).unwrap();
    assert_eq!(titles(&win), vec!["News", "Weather"]);
    assert_eq!(win[0].start, T0 + H);
    assert!(win[0].flags.is_new && win[0].flags.has_catchup_id && !win[0].flags.is_live);
    assert_eq!(&*win[0].sub_title.clone().unwrap(), "Late edition");
    let full = epg::programme(&r, win[0].id).unwrap();
    assert_eq!(full.categories, vec!["News".to_string(), "Current affairs".to_string()]);
    assert_eq!(full.episode, Some(EpisodeNum { season: Some(2), episode: Some(5) }));
    assert_eq!(full.description.as_deref(), Some("Headlines"));
    assert!(full.is_new && full.previously_shown && !full.is_live);
    assert_eq!(full.epg_channel_id, bbc);
    assert!(epg::window(&r, bbc, T0 - 2 * H, T0 + H).unwrap().is_empty());
    assert_fts_consistent(&r);

    // generation 2 is written but invisible until the flip
    let mut imp = w(&s, move |c| {
        let mut imp = epg::begin_epg_import(c, g)?;
        epg::write_epg_channels(c, &mut imp, &[echan("bbc1.uk", &["BBC One"])])?;
        epg::write_programmes(c, &mut imp, &hourly(&["bbc1.uk"], T0, 3, "Second"))?;
        Ok(imp)
    });
    assert_eq!(imp.generation, 2);
    assert_eq!(titles(&epg::window(&r, bbc, T0 + H, T0 + 2 * H).unwrap()), vec!["News", "Weather"]);
    imp.channel_ids.clear(); // the id map is a cache: finish/sweep don't depend on it
    let sum = w(&s, move |c| epg::finish_epg_import(c, &imp, T0 + 10));
    assert_eq!((sum.channels, sum.programmes, sum.max_stop), (1, 3, Some(T0 + 4 * H)));
    assert_eq!(titles(&epg::window(&r, bbc, T0 + H, T0 + 2 * H).unwrap()), vec!["Second 0"]);
    // the old generation is still on disk until swept, but never read
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 7);
    let mut swept = 0;
    loop {
        let n = w(&s, move |c| epg::sweep_old_generations(c, g, 2));
        if n == 0 {
            break;
        }
        assert!(n <= 2 || swept >= 4, "programmes are swept in chunks");
        swept += n;
    }
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 3);
    assert_eq!(epg_id(&r, g, "bbc1.uk"), bbc, "epg channel row ids are stable across imports");
    assert_eq!(count(&r, "SELECT count(*) FROM epg_channel"), 1, "channels unseen in the active generation are swept");
    assert_eq!(epg::epg_channel(&r, bbc).unwrap().display_names, vec!["BBC One".to_string()]);
    assert_fts_consistent(&r);
}

#[test]
fn aborted_import_keeps_the_old_guide_and_leaves_no_rows() {
    let s = store("tuner-se-abort");
    let g = url_guide(&s, "G", 0);
    import_guide(&s, g, vec![echan("a.tv", &["A"])], hourly(&["a.tv"], T0, 24, "Old"), T0);
    let r = s.reader().unwrap();
    let a = epg_id(&r, g, "a.tv");
    w(&s, move |c| {
        let mut imp = epg::begin_epg_import(c, g)?;
        epg::write_epg_channels(c, &mut imp, &[echan("a.tv", &["A"]), echan("new.tv", &["New"])])?;
        for b in hourly(&["a.tv", "new.tv"], T0, 24, "New").chunks(10) {
            epg::write_programmes(c, &mut imp, b)?;
        }
        epg::abort_epg_import(c, &imp)
    });
    assert_eq!(sources::get_epg_source(&r, g).unwrap().active_generation, 1);
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 24);
    assert_eq!(titles(&epg::window(&r, a, T0, T0 + H).unwrap()), vec!["Old 0"]);
    assert_eq!(count(&r, "SELECT count(*) FROM meta WHERE key LIKE 'epg-import:%'"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM epg_channel"), 1, "the channel the aborted run introduced is gone");
    assert_fts_consistent(&r);
    // the next import starts cleanly
    let sum = import_guide(&s, g, vec![echan("a.tv", &["A"])], hourly(&["a.tv"], T0, 2, "Next"), T0 + 1);
    assert_eq!((sum.channels, sum.programmes), (1, 2));
    assert_eq!(count(&r, "SELECT count(*) FROM epg_channel"), 1);
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 2);
    assert_fts_consistent(&r);
}

/// A reader polling the guide while a new generation is imported in many small transactions
/// sees the complete old guide until the flip and the complete new guide after it — never an
/// empty, partial or mixed result (temp WAL DB, real concurrent reader connection).
#[test]
fn generation_flip_is_atomic_for_a_concurrent_reader() {
    const CHANNELS: usize = 20;
    const SLOTS: i64 = 48;
    let s = store("tuner-se-flip");
    let g = url_guide(&s, "G", 0);
    let ids: Vec<String> = (0..CHANNELS).map(|i| format!("c{i}.tv")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let chans: Vec<ImportEpgChannel> = ids.iter().map(|i| echan(i, &[i.as_str()])).collect();
    import_guide(&s, g, chans.clone(), hourly(&id_refs, T0, SLOTS, "A"), T0);
    let r = s.reader().unwrap();
    let epg_ids: Vec<EpgChannelId> = ids.iter().map(|i| epg_id(&r, g, i)).collect();

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader = {
        let (s, stop, reads, epg_ids) = (s.clone(), stop.clone(), reads.clone(), epg_ids.clone());
        std::thread::spawn(move || {
            let conn = s.reader().unwrap();
            let mut seen_new = false;
            let mut observations = Vec::new();
            while !stop.load(Ordering::Acquire) {
                for ch in &epg_ids {
                    let win = epg::window(&conn, *ch, T0, T0 + SLOTS * H).unwrap();
                    assert_eq!(win.len(), SLOTS as usize, "a complete guide on every read");
                    let gen_b = win[0].title.starts_with('B');
                    assert!(win.iter().all(|p| p.title.starts_with('B') == gen_b), "never a mix of generations");
                    assert!(!(seen_new && !gen_b), "never back to the old generation after the flip");
                    seen_new |= gen_b;
                    let nn = epg::now_next(&conn, &[(*ch, 0)], T0 + 90).unwrap();
                    assert!(nn[&(*ch, 0)].now.is_some() && nn[&(*ch, 0)].next.is_some(), "now/next never empty");
                    observations.push(gen_b);
                }
                reads.fetch_add(1, Ordering::Release);
            }
            observations
        })
    };
    let wait_for_reader = |reads: &AtomicUsize| {
        let before = reads.load(Ordering::Acquire);
        let deadline = Instant::now() + Duration::from_secs(5);
        while reads.load(Ordering::Acquire) == before && Instant::now() < deadline {
            std::thread::yield_now();
        }
    };

    let mut imp = w(&s, move |c| {
        let mut imp = epg::begin_epg_import(c, g)?;
        epg::write_epg_channels(c, &mut imp, &chans)?;
        Ok(imp)
    });
    for batch in hourly(&id_refs, T0, SLOTS, "B").chunks(97) {
        let batch = batch.to_vec();
        imp = w(&s, move |c| {
            epg::write_programmes(c, &mut imp, &batch)?;
            Ok(imp)
        });
        wait_for_reader(&reads);
    }
    w(&s, move |c| epg::finish_epg_import(c, &imp, T0 + 1).map(|_| ()));
    wait_for_reader(&reads);
    while w(&s, move |c| epg::sweep_old_generations(c, g, 100)) > 0 {
        wait_for_reader(&reads);
    }
    wait_for_reader(&reads);
    stop.store(true, Ordering::Release);
    let obs = reader.join().expect("reader assertions hold");
    assert!(obs.iter().any(|b| !b), "the reader ran during the import");
    assert_eq!(obs.last(), Some(&true), "the reader ends on the new generation");
    assert_fts_consistent(&r);
}

// ------------------------------------------------------------------ FTS consistency

#[test]
fn fts_has_no_orphans_after_reimports_prune_and_source_deletion() {
    let s = store("tuner-se-fts");
    let p = playlist(&s, &[("m3u:a", "Alpha", Some("a.tv"), 0), ("m3u:b", "Beta", Some("b.tv"), 0)]);
    let g = url_guide(&s, "G", 0);
    let r = s.reader().unwrap();
    for (round, title) in ["Morning Show", "Evening Show", "Night Show"].into_iter().enumerate() {
        let shift = round as i64 * 6 * H; // overlapping windows: some programmes repeat, some are new
        let chans = vec![echan("a.tv", &["Alpha"]), echan("b.tv", &["Beta"])];
        import_guide(&s, g, chans, hourly(&["a.tv", "b.tv"], T0 + shift, 24, title), T0 + shift);
        auto_map(&s, p, g, &[("m3u:a", "a.tv", EpgMatchMethod::TvgIdExact), ("m3u:b", "b.tv", EpgMatchMethod::TvgIdExact)]);
        assert_eq!(count(&r, "SELECT count(*) FROM programme"), 48);
        assert_fts_consistent(&r);
        assert_eq!(w(&s, |c| epg::fts_cleanup(c)), 0);
        // only the active generation is searchable
        let hits = epg::search(&r, "show", T0, 500).unwrap();
        assert_eq!(hits.len(), 48);
        assert!(hits.iter().all(|h| h.programme.title.starts_with(title.split(' ').next().unwrap())));
        assert_eq!(epg::search(&r, "morning", T0, 10).unwrap().is_empty(), round > 0);
    }
    // prune everything that ended by the last import's midpoint (chunked, FTS first)
    let before = T0 + 24 * H + 1;
    let mut pruned = 0;
    loop {
        let n = w(&s, move |c| epg::prune(c, before, 7));
        assert!(n <= 7);
        if n == 0 {
            break;
        }
        pruned += n;
    }
    assert_eq!(pruned, 24);
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 24);
    assert_fts_consistent(&r);

    // explicit channel scope (used before deleting single epg channels)
    let b = epg_id(&r, g, "b.tv");
    while w(&s, move |c| epg::delete_programmes_of(c, &ProgrammeScope::EpgChannels(vec![b]), 5)) > 0 {}
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 12);
    assert_fts_consistent(&r);

    // a provider guide removed because the playlist stopped advertising it
    let provider = w(&s, move |c| sources::ensure_provider_epg(c, p, &["http://p.test/xmltv.php?username=u&password=pw".into()]))[0];
    import_guide(&s, provider, vec![echan("a.tv", &["Alpha"])], hourly(&["a.tv"], T0, 10, "Provider Show"), T0);
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 22);
    assert!(w(&s, move |c| sources::ensure_provider_epg(c, p, &[])).is_empty());
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 12);
    assert_fts_consistent(&r);

    // deleting the user guide removes its programmes and FTS rows first
    w(&s, move |c| sources::delete_epg_source(c, g));
    assert_eq!(count(&r, "SELECT count(*) FROM programme"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM programme_fts"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM epg_channel"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg"), 0);
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg_map"), 0);
    assert!(epg::search(&r, "show", T0, 10).unwrap().is_empty());
    assert_eq!(w(&s, |c| epg::fts_cleanup(c)), 0);
}

// ------------------------------------------------------------------ reads

#[test]
fn now_next_applies_tvg_shift_once() {
    let s = store("tuner-se-shift");
    let g = url_guide(&s, "G", 0);
    let mut progs = hourly(&["a.tv"], T0, 4, "P");
    progs.push(prog("a.tv", T0 + 5 * H, T0 + 6 * H, "After gap"));
    import_guide(&s, g, vec![echan("a.tv", &["A"]), echan("empty.tv", &["E"])], progs, T0);
    let r = s.reader().unwrap();
    let a = epg_id(&r, g, "a.tv");
    let empty = epg_id(&r, g, "empty.tv");

    let now = T0 + H + 600; // inside "P 1" in EPG time
    let nn = epg::now_next(&r, &[(a, 0), (a, 60), (a, -60), (a, 60), (empty, 0)], now).unwrap();
    assert_eq!(nn.len(), 4, "one entry per (channel, shift)");
    let pair = |k: (EpgChannelId, i32)| {
        let n = &nn[&k];
        (n.now.as_ref().map(|p| (p.title.to_string(), p.start)), n.next.as_ref().map(|p| (p.title.to_string(), p.start)))
    };
    assert_eq!(pair((a, 0)), (Some(("P 1".into(), T0 + H)), Some(("P 2".into(), T0 + 2 * H))));
    // "+1" channel: shows what aired an hour earlier, at displayed (shifted) times
    assert_eq!(pair((a, 60)), (Some(("P 0".into(), T0 + H)), Some(("P 1".into(), T0 + 2 * H))));
    assert_eq!(pair((a, -60)), (Some(("P 2".into(), T0 + H)), Some(("P 3".into(), T0 + 2 * H))));
    assert_eq!(pair((empty, 0)), (None, None));
    // in a gap: nothing now, the next programme upcoming
    let gap = epg::now_next(&r, &[(a, 0)], T0 + 4 * H + 60).unwrap();
    assert_eq!(gap[&(a, 0)].now, None);
    assert_eq!(gap[&(a, 0)].next.as_ref().map(|p| &*p.title), Some("After gap"));
    // window/upcoming return EPG time (callers shift once)
    let up = epg::upcoming(&r, a, T0 + H, 2).unwrap();
    assert_eq!(titles(&up), vec!["P 1", "P 2"]);
    assert_eq!(up[0].shifted(60).start, T0 + 2 * H);
    // a programme longer than the window that started before it is still returned
    assert_eq!(titles(&epg::window(&r, a, T0 + H + 1200, T0 + H + 1800).unwrap()), vec!["P 1"]);
}

#[test]
fn search_orders_airing_upcoming_past_and_requires_a_mapped_channel() {
    let s = store("tuner-se-search");
    let p = playlist(&s, &[("m3u:a", "Alpha", Some("a.tv"), 0), ("m3u:a1", "Alpha +1", Some("a.tv"), 60)]);
    let g = url_guide(&s, "G", 0);
    let now = T0;
    let progs = vec![
        prog("a.tv", now - 3 * H, now - 2 * H, "Tom and Jerry: past"),
        prog("a.tv", now - 600, now + 1200, "Tom and Jerry: now"),
        prog("a.tv", now + 2 * H, now + 3 * H, "Tom and Jerry: later"),
        prog("a.tv", now + H, now + 2 * H, "Tom and Jerry: soon"),
        prog("a.tv", now + 4 * H, now + 5 * H, "Café Society"),
        prog("unmapped.tv", now, now + H, "Tom and Jerry: unmapped"),
    ];
    import_guide(&s, g, vec![echan("a.tv", &["Alpha"]), echan("unmapped.tv", &["U"])], progs, T0);
    auto_map(&s, p, g, &[("m3u:a", "a.tv", EpgMatchMethod::TvgIdExact), ("m3u:a1", "a.tv", EpgMatchMethod::TvgIdExact)]);
    let r = s.reader().unwrap();
    let hits = epg::search(&r, "tom jer", now, 10).unwrap();
    let got: Vec<&str> = hits.iter().map(|h| &*h.programme.title).collect();
    assert_eq!(got, vec!["Tom and Jerry: now", "Tom and Jerry: soon", "Tom and Jerry: later", "Tom and Jerry: past"]);
    let (a, a1) = (channel_id(&r, p, "m3u:a"), channel_id(&r, p, "m3u:a1"));
    assert_eq!(hits[0].channel_ids, vec![a, a1], "both playlist channels sharing the guide channel");
    assert_eq!(hits[0].epg_channel_id, epg_id(&r, g, "a.tv"));
    // diacritics folded, prefix on the last token only, hostile input is just text
    assert_eq!(epg::search(&r, "cafe soc", now, 10).unwrap().len(), 1);
    assert_eq!(epg::search(&r, "soc cafe", now, 10).unwrap().len(), 0, "\"soc\" is a whole token, only the last is a prefix");
    assert!(epg::search(&r, "\" OR * NEAR(", now, 10).unwrap().is_empty());
    assert!(epg::search(&r, "   ", now, 10).unwrap().is_empty());
    assert_eq!(epg::search(&r, "tom", now, 2).unwrap().len(), 2, "limit");
    // a disabled guide is not searched
    let mut src = sources::get_epg_source(&r, g).unwrap();
    src.enabled = false;
    w(&s, move |c| sources::update_epg_source(c, &src));
    assert!(epg::search(&r, "tom", now, 10).unwrap().is_empty());
}

// ------------------------------------------------------------------ mappings

#[test]
fn mapping_resolution_manual_priority_method_and_fallbacks() {
    let s = store("tuner-se-map");
    let p = playlist(&s, &[("m3u:a", "Alpha", Some("a.tv"), 0), ("m3u:b", "Beta", None, 0)]);
    let g1 = url_guide(&s, "G1", 0);
    let g2 = url_guide(&s, "G2", 0);
    for g in [g1, g2] {
        import_guide(&s, g, vec![echan("a.tv", &["Alpha"]), echan("b.tv", &["Beta"])], hourly(&["a.tv", "b.tv"], T0, 2, "X"), T0);
    }
    auto_map(&s, p, g1, &[("m3u:a", "a.tv", EpgMatchMethod::TvgIdExact)]);
    auto_map(&s, p, g2, &[("m3u:a", "a.tv", EpgMatchMethod::TvgIdExact), ("m3u:b", "b.tv", EpgMatchMethod::NormalizedName)]);
    let r = s.reader().unwrap();
    let (a, b) = (channel_id(&r, p, "m3u:a"), channel_id(&r, p, "m3u:b"));
    let resolved = |ch| epg::epg_channel_for(&r, ch).unwrap();
    assert_eq!(resolved(a), Some(epg_id(&r, g1, "a.tv")), "lowest priority number wins");
    assert_eq!(resolved(b), Some(epg_id(&r, g2, "b.tv")), "only guide with data");

    w(&s, move |c| sources::reorder_epg_sources(c, &[g2, g1]));
    assert_eq!(resolved(a), Some(epg_id(&r, g2, "a.tv")), "reordering re-resolves");

    // manual wins over priority and survives automatic re-matching
    let ra = ch_ref(p, "m3u:a");
    let ra2 = ra.clone();
    w(&s, move |c| epg::set_manual_mapping(c, &ra2, g1, "b.tv"));
    assert_eq!(resolved(a), Some(epg_id(&r, g1, "b.tv")));
    auto_map(&s, p, g1, &[]);
    assert_eq!(resolved(a), Some(epg_id(&r, g1, "b.tv")));
    let maps = epg::mappings_for_channel(&r, &ra).unwrap();
    assert_eq!(maps[0].method, EpgMatchMethod::Manual);
    assert_eq!((maps[0].epg_source_id, maps[0].xmltv_id.as_str()), (g1, "b.tv"));
    assert_eq!(maps.len(), 2);
    let fp: String = r
        .query_row("SELECT fp FROM channel_epg_map WHERE method = 'manual'", [], |row| row.get(0))
        .unwrap();
    assert!(fp.contains("\"tvg_id\":\"a.tv\""), "manual rows carry the channel fingerprint: {fp}");
    // re-assigning to another guide keeps one manual row per channel
    let ra3 = ra.clone();
    w(&s, move |c| epg::set_manual_mapping(c, &ra3, g2, "a.tv"));
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg_map WHERE method = 'manual'"), 1);
    assert_eq!(resolved(a), Some(epg_id(&r, g2, "a.tv")));

    // a disabled guide is skipped, even for a manual mapping
    let mut src2 = sources::get_epg_source(&r, g2).unwrap();
    src2.enabled = false;
    w(&s, move |c| {
        sources::update_epg_source(c, &src2)?;
        epg::resolve_mappings(c)
    });
    assert_eq!(resolved(a), None, "g1 lost its automatic row above");
    assert_eq!(resolved(b), None);
    auto_map(&s, p, g1, &[("m3u:a", "a.tv", EpgMatchMethod::TvgIdCaseless)]);
    assert_eq!(resolved(a), Some(epg_id(&r, g1, "a.tv")));

    // clearing the manual mapping lets automatic rows apply; deleting a guide falls back
    let ra4 = ra.clone();
    w(&s, move |c| epg::clear_manual_mapping(c, &ra4));
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg_map WHERE method = 'manual'"), 0);
    w(&s, move |c| sources::delete_epg_source(c, g1));
    assert_eq!(resolved(a), None);
    // the library snapshot carries the resolved guide channel
    let lib = library::load(&r).unwrap();
    assert!(lib.live.channels.iter().all(|c| c.epg_channel_id.is_none()));
}

#[test]
fn matching_inputs_and_automatic_mapping_replacement() {
    let s = store("tuner-se-match");
    let p = playlist(&s, &[("m3u:a", "UK: Alpha HD", Some("Alpha.uk"), 0), ("m3u:b", "Beta", Some(""), 0)]);
    let g = url_guide(&s, "G", 0);
    let chans = vec![echan("alpha.uk", &["Alpha"]), echan("Alpha.uk@SD", &["Alpha SD"]), echan("beta.uk", &["Beta"])];
    import_guide(&s, g, chans, hourly(&["alpha.uk"], T0, 1, "X"), T0);
    let r = s.reader().unwrap();
    let rows = epg::channels_for_matching(&r, p).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].channel, ch_ref(p, "m3u:a"));
    assert_eq!(rows[0].tvg_id.as_deref(), Some("Alpha.uk"));
    assert_eq!(rows[0].name, "UK: Alpha HD");
    assert_eq!(rows[0].norm_name, normalize_name("UK: Alpha HD"));
    assert_eq!(rows[1].tvg_id, None, "an empty tvg-id is no tvg-id");
    let mut names: Vec<Vec<String>> = epg::epg_channels(&r, g).unwrap().into_iter().map(|c| c.display_names).collect();
    names.sort();
    assert_eq!(names, vec![vec!["Alpha".to_string()], vec!["Alpha SD".to_string()], vec!["Beta".to_string()]]);

    // one row per (channel, guide): the matcher's first candidate wins; rows with method
    // Manual or for another source are not automatic and are ignored
    auto_map(
        &s,
        p,
        g,
        &[
            ("m3u:a", "Alpha.uk@SD", EpgMatchMethod::FeedStripped),
            ("m3u:a", "alpha.uk", EpgMatchMethod::TvgIdCaseless),
            ("m3u:b", "beta.uk", EpgMatchMethod::Manual),
        ],
    );
    let other = SourceId(p.0 + 1000);
    w(&s, move |c| {
        let mut maps = epg::mappings_for_channel(c, &ch_ref(p, "m3u:a"))?;
        maps.push(EpgMapping {
            channel: ch_ref(other, "m3u:b"),
            epg_source_id: g,
            xmltv_id: "beta.uk".into(),
            method: EpgMatchMethod::TvgIdExact,
            confidence: 1.0,
        });
        epg::replace_auto_mappings(c, p, g, &maps)?;
        epg::resolve_mappings(c)
    });
    let (a, b) = (channel_id(&r, p, "m3u:a"), channel_id(&r, p, "m3u:b"));
    assert_eq!(epg::epg_channel_for(&r, a).unwrap(), Some(epg_id(&r, g, "Alpha.uk@SD")));
    assert_eq!(epg::epg_channel_for(&r, b).unwrap(), None);
    assert_eq!(count(&r, "SELECT count(*) FROM channel_epg_map"), 1);
    // replacing with nothing clears the automatic rows
    auto_map(&s, p, g, &[]);
    assert_eq!(epg::epg_channel_for(&r, a).unwrap(), None);
}

/// An aborted run's new guide channels are removed and mappings that resolved to them fall
/// back; channels the active guide still has programmes for stay current.
#[test]
fn aborted_import_removes_its_new_channels_and_re_resolves() {
    let s = store("tuner-se-abort-resolve");
    let p = playlist(&s, &[("m3u:n", "N", None, 0)]);
    let g1 = url_guide(&s, "G1", 0);
    let g2 = url_guide(&s, "G2", 0);
    import_guide(&s, g1, vec![echan("a.tv", &["A"])], hourly(&["a.tv"], T0, 2, "A"), T0);
    import_guide(&s, g2, vec![echan("n.tv", &["N"])], hourly(&["n.tv"], T0, 2, "N"), T0);
    auto_map(&s, p, g1, &[("m3u:n", "n.tv", EpgMatchMethod::TvgIdExact)]);
    auto_map(&s, p, g2, &[("m3u:n", "n.tv", EpgMatchMethod::TvgIdExact)]);
    let r = s.reader().unwrap();
    let n = channel_id(&r, p, "m3u:n");
    assert_eq!(epg::epg_channel_for(&r, n).unwrap(), Some(epg_id(&r, g2, "n.tv")), "G1 has no n.tv yet");
    let imp = w(&s, move |c| {
        let mut imp = epg::begin_epg_import(c, g1)?;
        epg::write_epg_channels(c, &mut imp, &[echan("a.tv", &["A"]), echan("n.tv", &["N"])])?;
        epg::write_programmes(c, &mut imp, &hourly(&["a.tv", "n.tv"], T0, 2, "New"))?;
        // e.g. another guide finished meanwhile: the in-progress channel is picked up
        epg::resolve_mappings(c)?;
        Ok(imp)
    });
    assert_eq!(epg::epg_channel_for(&r, n).unwrap(), Some(epg_id(&r, g1, "n.tv")));
    w(&s, move |c| epg::abort_epg_import(c, &imp));
    assert_eq!(epg::epg_channel_for(&r, n).unwrap(), Some(epg_id(&r, g2, "n.tv")), "falls back after the abort");
    let current: Vec<String> = epg::epg_channels(&r, g1).unwrap().into_iter().map(|c| c.xmltv_id).collect();
    assert_eq!(current, vec!["a.tv".to_string()]);
    assert_eq!(count(&r, &format!("SELECT count(*) FROM epg_channel WHERE epg_source_id = {}", g1.0)), 1);
    assert_fts_consistent(&r);
}

/// Deleting a playlist source drops its provider guide; other sources' channels mapped to it
/// fall back to the next guide at once (same when a refresh stops advertising the guide).
#[test]
fn delete_source_re_resolves_channels_mapped_to_its_provider_guide() {
    let s = store("tuner-se-delsrc-resolve");
    let other = playlist(&s, &[("m3u:x", "X", None, 0)]);
    let owner = playlist(&s, &[("m3u:y", "Y", None, 0)]);
    let pe = w(&s, move |c| Ok(sources::ensure_provider_epg(c, owner, &["http://h.test/xmltv.php".to_string()])?[0]));
    let g2 = url_guide(&s, "G2", 0);
    import_guide(&s, pe, vec![echan("x.tv", &["X"])], hourly(&["x.tv"], T0, 2, "p"), T0);
    import_guide(&s, g2, vec![echan("x.tv", &["X"])], hourly(&["x.tv"], T0, 2, "u"), T0);
    auto_map(&s, other, pe, &[("m3u:x", "x.tv", EpgMatchMethod::TvgIdExact)]);
    auto_map(&s, other, g2, &[("m3u:x", "x.tv", EpgMatchMethod::TvgIdExact)]);
    let r = s.reader().unwrap();
    let x = channel_id(&r, other, "m3u:x");
    assert_eq!(epg::epg_channel_for(&r, x).unwrap(), Some(epg_id(&r, pe, "x.tv")), "provider guide first");
    w(&s, move |c| sources::delete_source(c, owner));
    assert_eq!(epg::epg_channel_for(&r, x).unwrap(), Some(epg_id(&r, g2, "x.tv")));

    let owner2 = playlist(&s, &[("m3u:z", "Z", None, 0)]);
    let pe2 = w(&s, move |c| Ok(sources::ensure_provider_epg(c, owner2, &["http://h.test/xmltv2.php".to_string()])?[0]));
    import_guide(&s, pe2, vec![echan("x.tv", &["X"])], hourly(&["x.tv"], T0, 2, "p2"), T0);
    auto_map(&s, other, pe2, &[("m3u:x", "x.tv", EpgMatchMethod::TvgIdExact)]);
    assert_eq!(epg::epg_channel_for(&r, x).unwrap(), Some(epg_id(&r, pe2, "x.tv")));
    w(&s, move |c| sources::ensure_provider_epg(c, owner2, &[]));
    assert_eq!(epg::epg_channel_for(&r, x).unwrap(), Some(epg_id(&r, g2, "x.tv")));
}

/// "Attach to…" onto a channel that already has an automatic row for the same guide: the
/// manual mapping is never overwritten. If the target has a manual mapping of its own, it wins.
#[test]
fn reassign_keeps_manual_mapping_over_target_automatic_row() {
    let s = store("tuner-se-reassign-manual");
    let p = playlist(&s, &[("m3u:old", "Old", None, 0), ("m3u:new", "New", None, 0), ("m3u:other", "Other", None, 0)]);
    let g = url_guide(&s, "G", 0);
    let ids = ["a.tv", "b.tv", "c.tv"];
    import_guide(&s, g, ids.iter().map(|i| echan(i, &[i])).collect(), hourly(&ids, T0, 2, "x"), T0);
    auto_map(&s, p, g, &[("m3u:new", "b.tv", EpgMatchMethod::TvgIdExact)]);
    let (old, new) = (ch_ref(p, "m3u:old"), ch_ref(p, "m3u:new"));
    let (o, n) = (old.clone(), new.clone());
    w(&s, move |c| {
        epg::set_manual_mapping(c, &o, g, "a.tv")?;
        tuner::store::user::reassign(c, &o, &n)
    });
    let r = s.reader().unwrap();
    let maps = epg::mappings_for_channel(&r, &new).unwrap();
    assert_eq!(maps.len(), 1, "{maps:?}");
    assert_eq!((maps[0].method, maps[0].xmltv_id.as_str()), (EpgMatchMethod::Manual, "a.tv"));
    assert!(epg::mappings_for_channel(&r, &old).unwrap().is_empty());
    assert_eq!(epg::epg_channel_for(&r, channel_id(&r, p, "m3u:new")).unwrap(), Some(epg_id(&r, g, "a.tv")));

    // the target's own manual mapping wins; the moved one is dropped (one manual row each)
    let other = ch_ref(p, "m3u:other");
    let (ot, n) = (other.clone(), new.clone());
    w(&s, move |c| {
        epg::set_manual_mapping(c, &ot, g, "c.tv")?;
        tuner::store::user::reassign(c, &ot, &n)
    });
    let maps = epg::mappings_for_channel(&r, &new).unwrap();
    assert_eq!(maps.len(), 1, "{maps:?}");
    assert_eq!((maps[0].method, maps[0].xmltv_id.as_str()), (EpgMatchMethod::Manual, "a.tv"));
    assert!(epg::mappings_for_channel(&r, &other).unwrap().is_empty());
}
