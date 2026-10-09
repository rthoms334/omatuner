//! EPG channel matching: steps 1–5 of DESIGN §9.4 incl. the ambiguity rule. Owner: WP-05a.

use tuner::model::{EpgChannel, EpgChannelId, EpgMatchMethod, EpgSourceId, ItemKey, ItemKind, ItemRef, SourceId};
use tuner::sources::epg_match::{EpgCandidateIndex, build_index, match_all, match_channel, strip_feed, suggest};
use tuner::sources::identity::normalize_name;
use tuner::store::epg::ChannelMatchRow;

const EPG: EpgSourceId = EpgSourceId(3);

fn epg_channels() -> Vec<EpgChannel> {
    let c = |id: i64, xmltv: &str, names: &[&str]| EpgChannel {
        id: EpgChannelId(id),
        epg_source_id: EPG,
        xmltv_id: xmltv.into(),
        display_names: names.iter().map(|s| s.to_string()).collect(),
        icon: None,
    };
    vec![
        c(1, "bbc1.uk", &["BBC One", "BBC 1 HD"]),
        c(2, "BBC2.uk", &["BBC Two"]),
        c(3, "CNN.us@SD", &["CNN International"]),
        c(4, "ITV1.uk", &["ITV 1"]),
        // Two EPG channels share the norm "skynews" → never auto-matched by name.
        c(5, "sky.news.uk", &["Sky News"]),
        c(6, "skynews.ie", &["Sky News HD"]),
        c(7, "Discovery.us", &["Discovery Channel", "Discovery Channel HD", "Discovery"]),
        c(8, "sky.cinema.plus1", &["Sky Cinema +1"]),
        c(9, "sky.cinema", &["Sky Cinema"]),
        c(10, "", &["No id (ignored)"]),
    ]
}

fn index() -> EpgCandidateIndex { build_index(EPG, &epg_channels()) }

fn row(key: &str, tvg: Option<&str>, name: &str) -> ChannelMatchRow {
    ChannelMatchRow {
        channel: ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(key.into()) },
        tvg_id: tvg.map(str::to_owned),
        name: name.into(),
        norm_name: normalize_name(name),
    }
}

fn hit(idx: &EpgCandidateIndex, r: &ChannelMatchRow, provider_guide: bool, name_match: bool) -> Option<(String, EpgMatchMethod)> {
    match_channel(idx, r, provider_guide, name_match).map(|m| {
        assert_eq!(m.channel, r.channel);
        assert_eq!(m.epg_source_id, EPG);
        assert_eq!(m.confidence, 1.0);
        (m.xmltv_id, m.method)
    })
}

fn h(id: &str, m: EpgMatchMethod) -> Option<(String, EpgMatchMethod)> { Some((id.into(), m)) }

#[test]
fn step1_tvg_id_exact_then_caseless() {
    let idx = index();
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc1.uk"), "x"), false, true), h("bbc1.uk", EpgMatchMethod::TvgIdExact));
    assert_eq!(hit(&idx, &row("m3u:a", Some(" bbc1.uk "), "x"), false, true), h("bbc1.uk", EpgMatchMethod::TvgIdExact));
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc2.UK"), "x"), false, true), h("BBC2.uk", EpgMatchMethod::TvgIdCaseless));
    // Exact id wins over a name that matches another channel.
    assert_eq!(hit(&idx, &row("m3u:a", Some("ITV1.uk"), "BBC One"), false, true), h("ITV1.uk", EpgMatchMethod::TvgIdExact));
}

#[test]
fn step2_xtream_epg_channel_id_on_provider_guide() {
    let idx = index();
    let xt = row("xt:live:42", Some("bbc1.uk"), "x");
    assert_eq!(hit(&idx, &xt, true, true), h("bbc1.uk", EpgMatchMethod::XtreamEpgId));
    assert_eq!(hit(&idx, &row("xt:live:42", Some("BBC1.UK"), "x"), true, true), h("bbc1.uk", EpgMatchMethod::XtreamEpgId));
    // Not the provider guide → plain tvg-id methods.
    assert_eq!(hit(&idx, &xt, false, true), h("bbc1.uk", EpgMatchMethod::TvgIdExact));
    // M3U channels against their url-tvg guide are tvg-id matches, not Xtream ones.
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc1.uk"), "x"), true, true), h("bbc1.uk", EpgMatchMethod::TvgIdExact));
}

#[test]
fn step3_feed_suffix_stripped_on_either_side() {
    let idx = index();
    assert_eq!(strip_feed("CNN.us@SD"), "CNN.us");
    assert_eq!(strip_feed("CNN.us"), "CNN.us");
    assert_eq!(hit(&idx, &row("m3u:a", Some("CNN.us"), "x"), false, true), h("CNN.us@SD", EpgMatchMethod::FeedStripped));
    assert_eq!(hit(&idx, &row("m3u:a", Some("CNN.us@HD"), "x"), false, true), h("CNN.us@SD", EpgMatchMethod::FeedStripped));
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc1.uk@London"), "x"), false, true), h("bbc1.uk", EpgMatchMethod::FeedStripped));
    assert_eq!(hit(&idx, &row("xt:live:1", Some("cnn.US@SD"), "x"), true, true), h("CNN.us@SD", EpgMatchMethod::XtreamEpgId));
}

#[test]
fn step4_normalized_name_only_when_unique() {
    let idx = index();
    assert_eq!(hit(&idx, &row("m3u:a", None, "UK | ITV 1 FHD"), false, true), h("ITV1.uk", EpgMatchMethod::NormalizedName));
    assert_eq!(hit(&idx, &row("m3u:a", None, "BBC 1"), false, true), h("bbc1.uk", EpgMatchMethod::NormalizedName));
    // An unknown tvg-id falls through to the name step.
    assert_eq!(hit(&idx, &row("m3u:a", Some("nope.xx"), "BBC One HD"), false, true), h("bbc1.uk", EpgMatchMethod::NormalizedName));
    // Two EPG channels normalize to "skynews": ambiguous → no match.
    assert_eq!(hit(&idx, &row("m3u:a", None, "Sky News"), false, true), None);
    // A channel with two display names that share a norm is not ambiguous with itself.
    assert_eq!(hit(&idx, &row("m3u:a", None, "Discovery Channel"), false, true), h("Discovery.us", EpgMatchMethod::NormalizedName));
    // +1 channels stay distinct.
    assert_eq!(hit(&idx, &row("m3u:a", None, "Sky Cinema +1 HD"), false, true), h("sky.cinema.plus1", EpgMatchMethod::NormalizedName));
    assert_eq!(hit(&idx, &row("m3u:a", None, "Sky Cinema"), false, true), h("sky.cinema", EpgMatchMethod::NormalizedName));
    // Disabled by config.
    assert_eq!(hit(&idx, &row("m3u:a", None, "UK | ITV 1 FHD"), false, false), None);
    // norm_name missing in the row → computed from the name.
    let mut r = row("m3u:a", None, "ITV 1");
    r.norm_name.clear();
    assert_eq!(hit(&idx, &r, false, true), h("ITV1.uk", EpgMatchMethod::NormalizedName));
    // No fuzzy auto-match.
    assert_eq!(hit(&idx, &row("m3u:a", None, "Discovery Chanel"), false, true), None);
}

#[test]
fn match_all_returns_only_matched_rows_in_order() {
    let idx = index();
    let rows = vec![
        row("m3u:1", Some("bbc1.uk"), "BBC One"),
        row("m3u:2", None, "Unknown Channel"),
        row("m3u:3", Some("CNN.us"), "CNN"),
        row("m3u:4", None, "Sky News"),
        row("m3u:5", None, "ITV 1"),
    ];
    let maps = match_all(&idx, &rows, false, true);
    let keys: Vec<(&str, &str, EpgMatchMethod)> = maps.iter().map(|m| (m.channel.key.as_str(), m.xmltv_id.as_str(), m.method)).collect();
    assert_eq!(
        keys,
        [
            ("m3u:1", "bbc1.uk", EpgMatchMethod::TvgIdExact),
            ("m3u:3", "CNN.us@SD", EpgMatchMethod::FeedStripped),
            ("m3u:5", "ITV1.uk", EpgMatchMethod::NormalizedName),
        ]
    );
    assert!(match_all(&idx, &[], false, true).is_empty());
}

#[test]
fn empty_index_matches_nothing() {
    let idx = EpgCandidateIndex::default();
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc1.uk"), "BBC One"), false, true), None);
    assert!(suggest(&idx, "BBC One", 5, 0.0).is_empty());
    let idx = build_index(EPG, &[]);
    assert_eq!(hit(&idx, &row("m3u:a", Some("bbc1.uk"), "BBC One"), false, true), None);
}

#[test]
fn step5_fuzzy_suggestions() {
    let idx = index();
    let s = suggest(&idx, "Discovery Chanel", 5, 0.92);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!((s[0].xmltv_id.as_str(), s[0].display_name.as_str(), s[0].epg_channel_id), ("Discovery.us", "Discovery Channel", EpgChannelId(7)));
    assert_eq!(s[0].epg_source_id, EPG);
    assert!(s[0].score > 0.95 && s[0].score < 1.0);

    // The ambiguous name case gets both candidates (exact norms score 1.0), sorted by name.
    let s = suggest(&idx, "UK: Sky News", 5, 0.92);
    let names: Vec<&str> = s.iter().map(|x| x.display_name.as_str()).collect();
    assert_eq!(names, ["Sky News", "Sky News HD"]);
    assert!(s.iter().all(|x| x.score == 1.0));
    assert_eq!(suggest(&idx, "Sky News", 1, 0.92).len(), 1, "limit respected");

    // Best first, threshold respected, one entry per EPG channel.
    let s = suggest(&idx, "Sky Cinema 1", 10, 0.80);
    assert!(s.windows(2).all(|w| w[0].score >= w[1].score), "{s:?}");
    assert!(s.iter().all(|x| x.score >= 0.80));
    let mut ids: Vec<EpgChannelId> = s.iter().map(|x| x.epg_channel_id).collect();
    ids.dedup();
    assert_eq!(ids.len(), s.len());
    assert!(suggest(&idx, "Totally Unrelated", 5, 0.92).is_empty());
    assert!(suggest(&idx, "", 5, 0.0).is_empty());
    assert!(suggest(&idx, "BBC One", 0, 0.0).is_empty());
}

#[test]
fn suggestions_scale_with_a_prefilter() {
    // 50k EPG channels: a suggestion lookup must not compare against all of them.
    let chans: Vec<EpgChannel> = (0..50_000)
        .map(|i| EpgChannel {
            id: EpgChannelId(i),
            epg_source_id: EPG,
            xmltv_id: format!("ch{i}.tv"),
            display_names: vec![format!("Channel {i}"), format!("Kanal {i}")],
            icon: None,
        })
        .chain(std::iter::once(EpgChannel {
            id: EpgChannelId(99_999),
            epg_source_id: EPG,
            xmltv_id: "nat.geo".into(),
            display_names: vec!["National Geographic".into()],
            icon: None,
        }))
        .collect();
    let idx = build_index(EPG, &chans);
    let t = std::time::Instant::now();
    for _ in 0..20 {
        let s = suggest(&idx, "Natonal Geographic HD", 5, 0.92);
        assert_eq!(s.first().map(|x| x.xmltv_id.as_str()), Some("nat.geo"));
    }
    // Generous bound for debug builds on a loaded machine; a full JW scan of 100k norms × 20
    // would take far longer.
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
}
