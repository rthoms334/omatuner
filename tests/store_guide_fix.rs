//! Guide fixes engine (DESIGN §31): the per-playlist rules ("+1/+2 follow their base",
//! "Copies share a guide") as re-derived after a rematch, the effective time shift, logged
//! batches with Undo, persistence through a renumbered refresh, the issue list and the
//! before/after preview. Owner: G-core.

use rusqlite::Connection;
use tuner::guide_fix::catalog::Catalog;
use tuner::guide_fix::issues::{ClassifyOpts, IssueReport, classify};
use tuner::guide_fix::preview::preview;
use tuner::model::*;
use tuner::redact::Secret;
use tuner::sources::epg_match;
use tuner::sources::identity::{group_key, normalize_name};
use tuner::store::epg::{self, ImportEpgChannel, ImportProgramme};
use tuner::store::guide_fix::{self as gf, GuideOpts};
use tuner::store::import::{self, ImportChannel, ImportGroup};
use tuner::store::library::{self, Library, Visibility};
use tuner::store::{Store, sources, user};

/// On the hour.
const T0: Timestamp = 1_791_576_000;
const H: i64 = 3600;
/// "Now" in these tests: half past.
const NOW: Timestamp = T0 + H / 2;

fn store(name: &str) -> Store {
    Store::open_temp(name, None).expect("temp store")
}

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).expect("writer job")
}

fn opts() -> GuideOpts {
    GuideOpts { rules_default: true, fuzzy_threshold: 0.92 }
}

fn guide(s: &Store, name: &str) -> EpgSourceId {
    let name = name.to_owned();
    w(s, move |c| sources::insert_epg_source(c, &name, &EpgSourceKind::Url { url: Secret::new(format!("http://e.test/{name}.xml")) }, 0))
}

fn prog(xmltv_id: &str, start: Timestamp, title: String) -> ImportProgramme {
    ImportProgramme {
        xmltv_id: xmltv_id.into(),
        start,
        stop: start + H,
        title,
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

/// Import guide `g` with `(xmltv id, display name)` channels, each with hourly programmes from
/// `T0 − 6 h` titled "`{name}` `{hour}`" (hour 6 airs at T0), then match every playlist against
/// it and re-derive the rules — the B7 pipeline of a guide import.
fn import_guide(s: &Store, g: EpgSourceId, chans: &[(&str, &str)]) {
    let ec: Vec<ImportEpgChannel> = chans
        .iter()
        .map(|(id, name)| ImportEpgChannel { xmltv_id: (*id).into(), display_names: vec![((*name).into(), normalize_name(name))], icon: None })
        .collect();
    let progs: Vec<ImportProgramme> =
        chans.iter().flat_map(|(id, name)| (0..12).map(move |i| prog(id, T0 - 6 * H + i * H, format!("{name} {i}")))).collect();
    w(s, move |c| {
        let mut imp = epg::begin_epg_import(c, g)?;
        epg::write_epg_channels(c, &mut imp, &ec)?;
        epg::write_programmes(c, &mut imp, &progs)?;
        let idx = epg_match::build_index(g, &epg::epg_channels(c, g)?);
        for src in sources::list_sources(c)? {
            let rows = epg::channels_for_matching(c, src.id)?;
            epg::replace_auto_mappings(c, src.id, g, &epg_match::match_all(&idx, &rows, false, false))?;
        }
        epg::finish_epg_import(c, &imp, NOW)?;
        while epg::sweep_old_generations(c, g, 20_000)? > 0 {}
        Ok(())
    });
    derive(s);
}

fn derive(s: &Store) -> gf::DeriveReport {
    w(s, |c| gf::derive_rule_rows(c, &opts()))
}

/// A channel; one named "XXX …" is flagged adult by the provider (explicit).
fn chan(i: usize, key: &str, name: &str, tvg: Option<&str>, shift: i32) -> ImportChannel {
    ImportChannel {
        adult: name.starts_with("XXX "),
        key: ItemKey(key.into()),
        provider_id: None,
        name: name.into(),
        clean_name: name.into(),
        norm_name: normalize_name(name),
        number: Some(i as u32 + 1),
        logo: None,
        tvg_id: tvg.map(str::to_owned),
        tvg_name: None,
        tvg_shift_min: shift,
        stream: StreamLocator::Url { url: format!("http://h.test/{key}.ts") },
        url_key: None,
        headers: None,
        catchup: None,
        radio: false,
        unsupported: None,
        provider_order: i as i32,
        added_at: None,
        groups: vec![group_key(ItemKind::Channel, "All")],
        quality: None,
    }
}

type Row<'a> = (&'a str, &'a str, Option<&'a str>, i32);

/// A playlist named `name` with one channel per `(key, name, tvg id, tvg-shift)`.
fn playlist(s: &Store, name: &str, chans: &[Row<'_>]) -> SourceId {
    let name = name.to_owned();
    let src = w(s, move |c| {
        sources::insert_source(
            c,
            &SourceDraft {
                name: name.clone(),
                kind: SourceKind::M3uUrl,
                icon: "x".into(),
                color: SourceColor::Blue,
                url: Some(format!("http://h.test/{name}.m3u")),
                path: None,
                xtream: None,
                settings: SourceSettings::default(),
            },
            T0,
        )
    });
    refresh(s, src, chans);
    src
}

/// A playlist refresh with these channels (gone keys reattach their user data, §8.3).
fn refresh(s: &Store, src: SourceId, chans: &[Row<'_>]) {
    let chans: Vec<ImportChannel> = chans.iter().enumerate().map(|(i, (k, n, t, sh))| chan(i, k, n, *t, *sh)).collect();
    w(s, move |c| {
        let imp = import::begin_catalogue(c, src, T0)?;
        let g = ImportGroup {
            adult: false,
            kind: ItemKind::Channel,
            key: group_key(ItemKind::Channel, "All"),
            name: "All".into(),
            raw_name: None,
            provider_id: None,
            provider_order: 0,
        };
        import::upsert_groups(c, &imp, &[g])?;
        import::write_channels(c, &imp, &chans)?;
        import::finish_catalogue(c, &imp, &[ItemKind::Channel], T0)?;
        Ok(())
    });
}

fn r(src: SourceId, key: &str) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(key.into()) }
}

fn lib(s: &Store) -> Library {
    library::load(&s.reader().unwrap()).unwrap()
}

/// `(guide channel xmltv id, effective shift)` of a channel as the library shows it.
fn shown(s: &Store, l: &Library, ch: &ItemRef) -> (Option<String>, i32) {
    let c = l.live.get(l.live.by_ref(ch).expect("channel")).unwrap();
    let id = c.epg_channel_id.map(|e| epg::epg_channel(&s.reader().unwrap(), e).unwrap().xmltv_id);
    (id, c.tvg_shift_min)
}

fn report(s: &Store, l: &Library, catalog: bool) -> IssueReport {
    let conn = s.reader().unwrap();
    let facts = gf::facts(&conn, &opts(), NOW).unwrap();
    let cat = catalog.then(|| Catalog::load(&conn).unwrap());
    classify(l, &facts, cat.as_ref(), ClassifyOpts::default())
}

fn issue<'a>(rep: &'a IssueReport, l: &Library, ch: &ItemRef) -> Option<&'a tuner::guide_fix::issues::IssueRow> {
    let id = l.live.by_ref(ch).unwrap();
    rep.rows.iter().find(|r| r.channel == id)
}

fn reminder(s: &Store, ch: &ItemRef, start: Timestamp) -> ReminderId {
    let rem = Reminder::notify(ch.clone(), "c".into(), "p".into(), start, start + H, 120);
    w(s, move |c| user::add_reminder(c, &rem))
}

fn reminder_start(s: &Store, id: ReminderId) -> Timestamp {
    user::reminder(&s.reader().unwrap(), id).unwrap().unwrap().start
}

/// An agent's batches are Codex's here.
fn apply(s: &Store, actor: FixActor, p: Proposal) -> tuner::error::Result<gf::ApplyReport> {
    s.writer().blocking(move |c| gf::apply(c, actor, &AgentName::named("Codex"), "test", &p, &opts(), NOW))
}

fn change(ch: &ItemRef) -> FixChange {
    FixChange { channel: Some(ch.clone()), ..Default::default() }
}

fn target(g: EpgSourceId, id: &str, name: &str) -> GuideTarget {
    GuideTarget { epg_source: g, xmltv_id: id.into(), name: name.into() }
}

// ------------------------------------------------------------------ rules

/// "+1/+2 follow their base": a "+1" with no guide channel of its own gets its base's with
/// +1 h; one reusing the base's tvg-id keeps that row and gains the shift; one the guide lists
/// itself and one with two disagreeing bases are left alone. The shown times and pending
/// reminders move with the rule, a re-import doesn't move them twice, a refresh that adds a
/// "+1" is covered, and turning the rule off (and Undo of that) moves everything back.
#[test]
fn plus_one_channels_follow_their_base() {
    let s = store("tuner-gf-plus1");
    let tv = playlist(
        &s,
        "TV",
        &[
            ("m3u:e4", "E4", Some("e4.uk"), 0),
            ("m3u:e4p1", "E4 +1", None, 0),
            ("m3u:f4", "Film4", Some("film4.uk"), 0),
            ("m3u:f4p1", "Film4 +1", Some("film4.uk"), 0),
            ("m3u:c4", "Channel 4", Some("c4.uk"), 0),
            ("m3u:c4p1", "Channel 4 +1", Some("c4p1.uk"), 0),
            ("m3u:q", "Quest Red", Some("questred.uk"), 0),
            ("m3u:qhd", "Quest Red HD", Some("questredhd.uk"), 0),
            ("m3u:qp1", "Quest Red +1", None, 0),
            ("m3u:prov", "Prov", Some("prov.uk"), 120),
        ],
    );
    let (e4p1, f4p1, c4p1, qp1, prov) = (r(tv, "m3u:e4p1"), r(tv, "m3u:f4p1"), r(tv, "m3u:c4p1"), r(tv, "m3u:qp1"), r(tv, "m3u:prov"));
    let rem = reminder(&s, &e4p1, T0 + 2 * H);
    let rem_f4 = reminder(&s, &f4p1, T0 + 2 * H);
    let g = guide(&s, "G");
    let gchans = [
        ("e4.uk", "E4"),
        ("film4.uk", "Film4"),
        ("c4.uk", "Channel 4"),
        ("c4p1.uk", "Channel 4 +1"),
        ("questred.uk", "Quest Red"),
        ("questredhd.uk", "Quest Red Extra"),
        ("prov.uk", "Prov"),
        ("dave.uk", "Dave"),
    ];
    import_guide(&s, g, &gchans);

    let l = lib(&s);
    assert_eq!(shown(&s, &l, &e4p1), (Some("e4.uk".into()), 60), "no guide channel of its own: the base's, +1 h");
    assert_eq!(shown(&s, &l, &f4p1), (Some("film4.uk".into()), 60), "the base's tvg-id: same row, +1 h");
    assert_eq!(shown(&s, &l, &c4p1), (Some("c4p1.uk".into()), 0), "the guide lists the +1 itself");
    assert_eq!(shown(&s, &l, &qp1), (None, 0), "two bases on different guide channels: no rule row");
    assert_eq!(shown(&s, &l, &prov), (Some("prov.uk".into()), 120), "the provider's tvg-shift when no rule applies");
    assert_eq!(reminder_start(&s, rem), T0 + 3 * H, "the reminder moved with the shown time");

    // the shown programme is the base's from an hour earlier
    let e4 = l.live.get(l.live.by_ref(&e4p1).unwrap()).unwrap().epg_channel_id.unwrap();
    let nn = epg::now_next(&s.reader().unwrap(), &[(e4, 60)], NOW).unwrap().remove(&(e4, 60)).unwrap();
    let now = nn.now.unwrap();
    assert_eq!((&*now.title, now.start), ("E4 5", T0));

    let rep = report(&s, &l, false);
    let q = issue(&rep, &l, &qp1).expect("Quest Red +1 is listed");
    assert_eq!((q.kind, q.proposal.is_none(), q.why.as_str()), (IssueKind::PlusNoShift, true, "2 bases, pick one"));
    assert!(issue(&rep, &l, &e4p1).is_none() && issue(&rep, &l, &f4p1).is_none(), "fixed by the rule");

    // a refresh adds "Dave" and "Dave +1"; the next guide import covers the new +1 without
    // moving what the rule had already moved
    let mut rows: Vec<Row<'_>> = vec![
        ("m3u:e4", "E4", Some("e4.uk"), 0),
        ("m3u:e4p1", "E4 +1", None, 0),
        ("m3u:f4", "Film4", Some("film4.uk"), 0),
        ("m3u:f4p1", "Film4 +1", Some("film4.uk"), 0),
    ];
    rows.extend([("m3u:dave", "Dave", Some("dave.uk"), 0), ("m3u:davep1", "Dave +1", None, 0)]);
    refresh(&s, tv, &rows);
    import_guide(&s, g, &gchans);
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &r(tv, "m3u:davep1")), (Some("dave.uk".into()), 60));
    assert_eq!(shown(&s, &l, &e4p1), (Some("e4.uk".into()), 60));
    assert_eq!(reminder_start(&s, rem), T0 + 3 * H, "re-deriving the same rows moves nothing");
    assert_eq!(reminder_start(&s, rem_f4), T0 + 3 * H, "a rematch keeps the rule's shift on an unchanged row");

    // the rule off for TV: rows gone, shifts back, and the issue list proposes the fixes
    let off = GuideRules { timeshift_siblings: Some(false), ..Default::default() };
    let rep_off = apply(&s, FixActor::You, Proposal { rules: vec![(tv, off)], ..Default::default() }).unwrap();
    assert_eq!(rep_off.reminders_moved, 2);
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &e4p1), (None, 0));
    assert_eq!(shown(&s, &l, &f4p1), (Some("film4.uk".into()), 0));
    assert_eq!(reminder_start(&s, rem), T0 + 2 * H);
    let rep = report(&s, &l, false);
    let f = issue(&rep, &l, &f4p1).unwrap();
    assert_eq!((f.kind, f.why.as_str(), f.one_key()), (IssueKind::PlusNoShift, "+1 of Film4", true));
    assert_eq!(f.proposal.as_ref().unwrap().shift_min, Some(Some(60)));
    assert_eq!(f.proposal.as_ref().unwrap().map, None, "already on the base's guide channel: only the shift");
    let e = issue(&rep, &l, &e4p1).unwrap().proposal.clone().unwrap();
    assert_eq!((e.map.unwrap().unwrap().xmltv_id.as_str(), e.shift_min), ("e4.uk", Some(Some(60))));

    // Undo turns it back on
    let undone = s.writer().blocking(move |c| gf::undo(c, rep_off.log.unwrap(), &opts(), NOW)).unwrap();
    assert_eq!((undone.reverted, undone.reminders_moved), (1, 2));
    assert_eq!(shown(&s, &lib(&s), &e4p1), (Some("e4.uk".into()), 60));
    assert_eq!(reminder_start(&s, rem), T0 + 3 * H);
}

/// "Copies share a guide": the same channel (B5 family) in another playlist lends its guide
/// channel, never across a tvg-id veto; with the rule off the issue list proposes it instead.
#[test]
fn copies_share_a_guide_across_playlists() {
    let s = store("tuner-gf-copies");
    let tv = playlist(&s, "TV", &[("m3u:bbc1", "BBC One", Some("bbc1.uk"), 0), ("m3u:dave", "Dave", Some("dave.uk"), 0)]);
    let tv2 = playlist(&s, "TV 2", &[("m3u:bbc1hd", "BBC One HD", None, 0), ("m3u:dave", "Dave", Some("other.uk"), 0)]);
    let g = guide(&s, "G");
    import_guide(&s, g, &[("bbc1.uk", "BBC 1 London"), ("dave.uk", "Dave")]);
    let l = lib(&s);
    let (copy, vetoed) = (r(tv2, "m3u:bbc1hd"), r(tv2, "m3u:dave"));
    assert_eq!(shown(&s, &l, &copy), (Some("bbc1.uk".into()), 0));
    assert_eq!(shown(&s, &l, &vetoed), (None, 0), "another tvg-id is another channel");
    assert_eq!(shown(&s, &l, &r(tv, "m3u:bbc1")).0.as_deref(), Some("bbc1.uk"));

    w(&s, move |c| sources::set_guide_rules(c, tv2, &GuideRules { share_copies: Some(false), ..Default::default() }));
    let rep = derive(&s);
    assert_eq!(rep.copies, 0);
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &copy), (None, 0));
    let rep = report(&s, &l, false);
    let row = issue(&rep, &l, &copy).unwrap();
    assert_eq!((row.kind, row.why.as_str()), (IssueKind::CopyHasGuide, "same channel in TV"));
    assert_eq!(row.proposal.as_ref().unwrap().map.clone().unwrap().unwrap().xmltv_id, "bbc1.uk");
    assert_eq!(issue(&rep, &l, &vetoed).unwrap().kind, IssueKind::NoGuide);
}

/// While explicit content is hidden, explicit channels are absent from the issue list and the
/// coverage; shown, they count like any other (DESIGN §31, parental).
#[test]
fn explicit_channels_count_only_while_shown() {
    let s = store("tuner-gf-explicit");
    playlist(&s, "P", &[("m3u:news", "News 24", Some("news24.uk"), 0), ("m3u:hot", "XXX Hot", Some("hot.uk"), 0), ("m3u:late", "XXX Late", None, 0)]);
    let g = guide(&s, "G");
    import_guide(&s, g, &[("news24.uk", "News 24"), ("hot.uk", "Hot")]);
    let c = s.reader().unwrap();
    let (hidden, shown) = (library::load_visible(&c, Visibility::NO_EXPLICIT).unwrap(), library::load_visible(&c, Visibility::ALL).unwrap());
    let seen = |l: &Library| {
        let rep = report(&s, l, true);
        let rows: Vec<String> = rep.rows.iter().map(|r| l.live.get(r.channel).unwrap().name.to_string()).collect();
        let cov = rep.coverage.iter().map(|c| (c.shown, c.with_guide, c.hidden_with_guide)).collect::<Vec<_>>();
        (rows, cov)
    };
    assert_eq!(seen(&hidden), (vec![], vec![(1, 1, 0)]));
    assert_eq!(seen(&shown), (vec!["XXX Late".to_owned()], vec![(3, 2, 0)]));
}

// ------------------------------------------------------------------ batches

/// A batch writes manual rows (with the guide channel's name and the actor), overrides and its
/// log; a shift moves pending reminders. Undo restores what the batch changed and skips (and
/// names) a field changed since and a channel gone from its playlist. An agent's batches are
/// capped; a proposal from before a guide import is stale.
#[test]
fn batches_apply_log_and_undo() {
    let s = store("tuner-gf-batch");
    let p = playlist(&s, "P", &[("m3u:bbc", "BBC One", Some("bbc1.uk"), 0), ("m3u:sky", "Sky Mix", None, 0)]);
    let g = guide(&s, "G");
    let gchans = [("bbc1.uk", "BBC One"), ("skymixhd.uk", "Sky Mix HD")];
    import_guide(&s, g, &gchans);
    let (bbc, sky) = (r(p, "m3u:bbc"), r(p, "m3u:sky"));
    let rem = reminder(&s, &bbc, T0 + 2 * H);
    let conn = s.reader().unwrap();
    let stamp = gf::stamp(&conn, &opts()).unwrap();

    let batch = Proposal {
        changes: vec![
            FixChange { map: Some(Some(target(g, "skymixhd.uk", "Sky Mix HD"))), catchup_offset_s: Some(Some(120)), ..change(&sky) },
            FixChange { shift_min: Some(Some(60)), dismiss: Some(IssueKind::LooksWrong), ..change(&bbc) },
        ],
        rules: vec![],
        stamp,
    };
    // the preview shows the same programmes now_next gives for the new pairs
    let pv = preview(&conn, &batch.changes, NOW).unwrap();
    assert_eq!((pv[0].before.now.is_none(), pv[0].after.now.as_ref().map(|p| p.title.to_string())), (true, Some("Sky Mix HD 6".into())));
    assert_eq!(pv[1].before.now.as_ref().map(|p| &*p.title), Some("BBC One 6"));
    assert_eq!(pv[1].after.now.as_ref().map(|p| (&*p.title, p.start)), Some(("BBC One 5", T0)), "+1 h: the programme of an hour earlier");

    let rep = apply(&s, FixActor::Agent, batch.clone()).unwrap();
    assert_eq!((rep.applied, rep.reminders_moved, rep.mappings_changed), (2, 1, true));
    let log = rep.log.unwrap();
    let entry = &gf::recent(&conn, 10).unwrap()[0];
    assert_eq!((entry.id, entry.actor, entry.agent.name(), entry.n, entry.undone_at), (log, FixActor::Agent, "Codex", 2, None));
    let (name, actor): (Option<String>, Option<String>) = conn
        .query_row("SELECT epg_name, actor FROM channel_epg_map WHERE channel_key = 'm3u:sky' AND method = 'manual'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // an agent's key stays 'claude' (0009's CHECK, older backups); the name is the log's
    assert_eq!((name.as_deref(), actor.as_deref()), (Some("Sky Mix HD"), Some("claude")));
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &sky), (Some("skymixhd.uk".into()), 0));
    assert_eq!(shown(&s, &l, &bbc), (Some("bbc1.uk".into()), 60));
    assert_eq!(reminder_start(&s, rem), T0 + 3 * H);
    let facts = gf::facts(&conn, &opts(), NOW).unwrap();
    let bbc_id = l.live.by_ref(&bbc).unwrap();
    assert_eq!(facts.extras[&bbc_id].guide_ok, IssueKind::LooksWrong.bit());

    // the same proposal again changes nothing and logs nothing; the stamp still matches
    let again = apply(&s, FixActor::Agent, batch).unwrap();
    assert_eq!((again.applied, again.log), (0, None));

    // you shift BBC One again afterwards: Undo of the agent's batch leaves that alone
    apply(&s, FixActor::You, Proposal { changes: vec![FixChange { shift_min: Some(Some(30)), ..change(&bbc) }], ..Default::default() }).unwrap();
    // the user's own batch is never shown under an agent's name
    assert_eq!(gf::recent(&conn, 1).unwrap()[0].agent.stored(), None);
    let un = s.writer().blocking(move |c| gf::undo(c, log, &opts(), NOW)).unwrap();
    assert_eq!(un.reverted, 2, "Sky Mix fully, BBC One's dismissal");
    assert_eq!(un.skipped, vec![("m3u:bbc".to_owned(), "changed since".to_owned())]);
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &sky), (None, 0), "no manual row, no automatic one");
    assert_eq!(shown(&s, &l, &bbc), (Some("bbc1.uk".into()), 30), "the automatic row is intact, your later shift kept");
    assert_eq!(gf::facts(&conn, &opts(), NOW).unwrap().extras[&bbc_id].guide_ok, 0);
    assert!(s.writer().blocking(move |c| gf::undo(c, log, &opts(), NOW)).is_err(), "undone once");

    // a batch on a channel that is gone by the time of Undo
    let shift_sky =
        apply(&s, FixActor::You, Proposal { changes: vec![FixChange { shift_min: Some(Some(15)), ..change(&sky) }], ..Default::default() }).unwrap();
    refresh(&s, p, &[("m3u:bbc", "BBC One", Some("bbc1.uk"), 0)]);
    let un = s.writer().blocking(move |c| gf::undo(c, shift_sky.log.unwrap(), &opts(), NOW)).unwrap();
    assert_eq!((un.reverted, un.skipped), (0, vec![("m3u:sky".to_owned(), "no longer in the playlist".to_owned())]));

    // stale after a guide import; an agent's batches are capped
    import_guide(&s, g, &gchans);
    let stale = Proposal { changes: vec![FixChange { shift_min: Some(None), ..change(&bbc) }], rules: vec![], stamp };
    assert!(apply(&s, FixActor::Agent, stale).unwrap_err().to_string().contains(gf::STALE));
    let big = Proposal { changes: vec![change(&bbc); gf::MAX_BATCH + 1], ..Default::default() };
    assert!(apply(&s, FixActor::Agent, big).is_err());

    // Undo of an agent's remap gives your mapping back as yours
    let map = |actor, id: &str, name: &str| {
        let p = Proposal { changes: vec![FixChange { map: Some(Some(target(g, id, name))), ..change(&bbc) }], ..Default::default() };
        apply(&s, actor, p).unwrap().log.unwrap()
    };
    map(FixActor::You, "bbc1.uk", "BBC One");
    let theirs = map(FixActor::Agent, "skymixhd.uk", "Sky Mix HD");
    s.writer().blocking(move |c| gf::undo(c, theirs, &opts(), NOW)).unwrap();
    let row: (String, Option<String>) = conn
        .query_row("SELECT xmltv_id, actor FROM channel_epg_map WHERE channel_key = 'm3u:bbc' AND method = 'manual'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(row, ("bbc1.uk".to_owned(), Some("you".to_owned())));
}

/// A manual mapping, a time shift and a dismissal follow the channel through a refresh that
/// re-keys it (reattach), and keep working there.
#[test]
fn fixes_follow_a_rekeyed_channel() {
    let s = store("tuner-gf-reattach");
    let p = playlist(&s, "P", &[("xt:live:1", "Sky Mix", None, 0), ("xt:live:2", "ITV", Some("itv.uk"), 0)]);
    let g = guide(&s, "G");
    import_guide(&s, g, &[("skymix.uk", "Sky Mix HD"), ("itv.uk", "ITV")]);
    let old = r(p, "xt:live:1");
    let fix = FixChange {
        map: Some(Some(target(g, "skymix.uk", "Sky Mix HD"))),
        shift_min: Some(Some(60)),
        dismiss: Some(IssueKind::GuideEnded),
        ..change(&old)
    };
    apply(&s, FixActor::You, Proposal { changes: vec![fix], ..Default::default() }).unwrap();
    refresh(&s, p, &[("xt:live:901", "Sky Mix", None, 0), ("xt:live:902", "ITV", Some("itv.uk"), 0)]);
    import_guide(&s, g, &[("skymix.uk", "Sky Mix HD"), ("itv.uk", "ITV")]);
    let new = r(p, "xt:live:901");
    let l = lib(&s);
    assert_eq!(shown(&s, &l, &new), (Some("skymix.uk".into()), 60));
    let facts = gf::facts(&s.reader().unwrap(), &opts(), NOW).unwrap();
    let e = &facts.extras[&l.live.by_ref(&new).unwrap()];
    assert_eq!((e.shift_min, e.guide_ok), (Some(60), IssueKind::GuideEnded.bit()));
    assert!(e.manual.as_ref().is_some_and(|m| m.live && m.epg_name.as_deref() == Some("Sky Mix HD")));

    // saving Edit (Ctrl+I) with nothing of its own set keeps the shift and the dismissal
    let item = new.clone();
    s.writer().blocking(move |c| tuner::store::user::set_item_override(c, &item, &tuner::model::ItemOverride::default())).unwrap();
    let facts = gf::facts(&s.reader().unwrap(), &opts(), NOW).unwrap();
    let e = &facts.extras[&l.live.by_ref(&new).unwrap()];
    assert_eq!((e.shift_min, e.guide_ok), (Some(60), IssueKind::GuideEnded.bit()));
}

/// A guide that renames the id of a manually mapped channel: "Gone from guide", found again
/// by the id ignoring case, else by the guide channel's stored name.
#[test]
fn gone_from_guide_is_found_again() {
    let s = store("tuner-gf-gone");
    let p = playlist(&s, "P", &[("m3u:b4", "BBC Four", None, 0)]);
    let g = guide(&s, "G");
    import_guide(&s, g, &[("bbcfour.uk", "BBC Four")]);
    let b4 = r(p, "m3u:b4");
    w(&s, {
        let b4 = b4.clone();
        move |c| epg::set_manual_mapping(c, &b4, g, "bbcfour.uk")
    });
    // renamed id (case), then a new id with the same name
    for (id, why) in [("BBCFour.uk", "renamed in the guide"), ("bbc4.tv", "found by its name in the guide")] {
        import_guide(&s, g, &[(id, "BBC Four")]);
        let l = lib(&s);
        let rep = report(&s, &l, true);
        let row = issue(&rep, &l, &b4).expect("listed");
        assert_eq!((row.kind, row.why.as_str(), row.one_key()), (IssueKind::GoneFromGuide, why, true), "{id}");
        assert_eq!(row.proposal.as_ref().unwrap().map.clone().unwrap().unwrap().xmltv_id, id);
        import_guide(&s, g, &[("bbcfour.uk", "BBC Four")]);
    }
}

// ------------------------------------------------------------------ the guide check job

/// The guide check caches suggestions for unmapped shown channels (the issue list proposes
/// the top one), waits while a picture is Loading or Zapping, scores each channel once per
/// stamp (one without any candidate too) and rebuilds when its stamp changes; cancelled, it
/// leaves the cache as it was.
#[test]
fn guide_check_caches_suggestions_and_waits_for_playback() {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, RwLock};

    use tokio_util::sync::CancellationToken;
    use tuner::jobs::guide_check::check_now;
    use tuner::jobs::{JobContext, Progress};

    let s = store("tuner-gf-check");
    let p = playlist(&s, "P", &[("m3u:sky", "Sky Mix", None, 0), ("m3u:bbc", "BBC One", Some("bbc1.uk"), 0), ("m3u:none", "Qzx Vw Telly", None, 0)]);
    let g = guide(&s, "G");
    import_guide(&s, g, &[("skymixhd.uk", "Sky Mix HD"), ("bbc1.uk", "BBC One")]);
    let dir = tempfile::tempdir().unwrap();
    let (bus, _rx) = tuner::msg::EventBus::new(None);
    let config: tuner::config::SharedConfig = Arc::new(RwLock::new(Arc::new(tuner::config::Config::default())));
    let http = tuner::sources::http::HttpClient::new(&Default::default()).unwrap();
    let ctx = JobContext::new(s.clone(), http, bus, tuner::paths::AppPaths::rooted(dir.path()), config.clone());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let rows = |s: &Store| -> i64 { s.reader().unwrap().query_row("SELECT count(*) FROM epg_suggestion", [], |r| r.get(0)).unwrap() };
    let run = |ctx: &JobContext, cancel: CancellationToken| {
        let (c2, l2) = (ctx.clone(), Arc::new(lib(&s)));
        rt.block_on(async move { check_now(&c2, l2, &Progress::detached(), &cancel).await })
    };

    // a picture is loading: the check waits before scoring
    ctx.player_busy.store(true, Ordering::Relaxed);
    let (c2, l2) = (ctx.clone(), Arc::new(lib(&s)));
    let task = rt.spawn(async move { check_now(&c2, l2, &Progress::detached(), &CancellationToken::new()).await });
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(!task.is_finished());
    assert_eq!(rows(&s), 0);
    ctx.player_busy.store(false, Ordering::Relaxed);
    let first = rt.block_on(task).unwrap().unwrap();
    assert_eq!((first.rebuilt, first.scored), (true, 2), "only the unmapped channels");
    let l = lib(&s);
    let rep = report(&s, &l, false);
    let row = issue(&rep, &l, &r(p, "m3u:sky")).unwrap();
    assert_eq!((row.kind, row.why.as_str()), (IssueKind::Suggested, "similar name in the guide"));
    assert_eq!(row.proposal.as_ref().unwrap().map.clone().unwrap().unwrap().xmltv_id, "skymixhd.uk");

    let again = run(&ctx, CancellationToken::new()).unwrap();
    assert_eq!((again.rebuilt, again.scored), (false, 0), "cached, the channel without a candidate too");
    let none = issue(&report(&s, &l, false), &l, &r(p, "m3u:none")).unwrap().kind;
    assert_eq!(none, IssueKind::NoGuide);

    // a new threshold is a new stamp: rebuilt
    let mut cfg = tuner::config::Config::default();
    cfg.epg.fuzzy_threshold = 0.9;
    *config.write().unwrap() = Arc::new(cfg);
    let rebuilt = run(&ctx, CancellationToken::new()).unwrap();
    assert_eq!((rebuilt.rebuilt, rebuilt.scored), (true, 2));

    // cancelled while waiting for playback: nothing half-written
    ctx.player_busy.store(true, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(run(&ctx, cancel), Err(tuner::error::Error::Cancelled)));
    assert_eq!(rows(&s), 2, "a suggestion and the no-candidate marker");
}
