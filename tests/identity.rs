//! Golden and property tests for the FROZEN `sources::identity` bodies (DESIGN §8.1). Owner:
//! WP-03. A failing golden test means stored keys in users' databases would be orphaned —
//! never "fix" a golden value; fix the code (or get architect sign-off and a migration plan).

use std::collections::HashSet;

use tuner::model::{GroupKey, ItemKind};
use tuner::sources::identity::{KeyAllocator, fingerprint, group_key, m3u_base_key, normalize_group, normalize_name, url_key, xtream_key};

// ---------------------------------------------------------------------------------------------
// golden values

#[test]
fn golden_normalize_name() {
    let table = [
        ("UK | BBC One HD", "bbcone"),
        ("UK | BBC One FHD", "bbcone"),
        ("uk|bbc one", "bbcone"),
        ("BBC One", "bbcone"),
        ("|EN| CNN HD (backup)", "cnn"),
        ("[VIP] US: ESPN 2 4K", "espn2"),
        ("DE - Das Erste HEVC", "daserste"),
        ("NL| NPO 1 1080p", "npo1"),
        ("Sky Cinema +1", "skycinema+1"),
        ("Sky Cinema +1 HD", "skycinema+1"),
        ("Sky Cinema +2", "skycinema+2"),
        ("ITV +1 (720p)", "itv+1"),
        ("Ça Tévé", "cateve"),
        ("Das Erste H.265", "daserste"),
        ("Channel 4 SD", "channel4"),
        ("HD", "hd"),
        ("4K", "4k"),
        ("  ", ""),
        ("", ""),
        ("[VIP] [UK] |EN| US: Fox", "fox"),
        ("E! Entertainment", "eentertainment"),
        ("Nick Jr.", "nickjr"),
        ("Москва 24", "moskva24"),
    ];
    for (raw, want) in table {
        assert_eq!(normalize_name(raw), want, "normalize_name({raw:?})");
    }
}

#[test]
fn golden_normalize_group_and_keys() {
    assert_eq!(normalize_group("UK | News"), "uknews");
    assert_eq!(normalize_group("US | News"), "usnews");
    assert_eq!(normalize_group("Ça Va"), "cava");
    assert_eq!(normalize_group(""), "uncategorized");
    assert_eq!(normalize_group(" | "), "uncategorized");
    assert_eq!(normalize_group("Sports +1"), "sports+1");
    assert_eq!(group_key(ItemKind::Channel, "UK | News"), GroupKey("channel:uknews".into()));
    assert_eq!(group_key(ItemKind::Movie, "  "), GroupKey("movie:uncategorized".into()));
    assert_eq!(group_key(ItemKind::Series, "Drama"), GroupKey("series:drama".into()));
    assert_eq!(xtream_key(ItemKind::Channel, 42).0, "xt:live:42");
    assert_eq!(xtream_key(ItemKind::Movie, 7).0, "xt:vod:7");
    assert_eq!(xtream_key(ItemKind::Series, 9).0, "xt:series:9");
    assert_eq!(xtream_key(ItemKind::Episode, 1001).0, "xt:ep:1001");
    assert_eq!(xtream_key(ItemKind::Channel, -1).0, "xt:live:-1");
}

/// These hex values are stored in users' databases. NEVER change them.
#[test]
fn golden_m3u_keys() {
    let table: [(Option<&str>, &str, Option<&str>, &str); 6] = [
        (None, "", None, GOLDEN_EMPTY),
        (None, "bbcone", None, GOLDEN_BBCONE),
        (Some("bbc1.uk"), "bbcone", Some("uknews"), GOLDEN_BBCONE_FULL),
        (Some("bbc1.uk"), "bbcone", None, GOLDEN_BBCONE_TVG),
        (None, "skycinema+1", Some("movies"), GOLDEN_SKY1),
        (Some("cnn.us@SD"), "cnninternational", Some("news"), GOLDEN_CNN),
    ];
    for (tvg, name, group, want) in table {
        assert_eq!(m3u_base_key(tvg, name, group), want, "m3u_base_key({tvg:?}, {name:?}, {group:?})");
    }
    // `None` and `Some("")` hash identically ("empty fields stay empty")
    assert_eq!(m3u_base_key(Some(""), "bbcone", Some("")), GOLDEN_BBCONE);
}

const GOLDEN_EMPTY: &str = "m3u:2182f490945c0799";
const GOLDEN_BBCONE: &str = "m3u:7224ddbbe7a6b713";
const GOLDEN_BBCONE_FULL: &str = "m3u:fb795ca44fb0bcf2";
const GOLDEN_BBCONE_TVG: &str = "m3u:fbd56eeaec82cf29";
const GOLDEN_SKY1: &str = "m3u:04fd77c452ae0276";
const GOLDEN_CNN: &str = "m3u:8412777eb58357ab";

#[test]
fn golden_url_keys() {
    let table = [
        ("http://User:Pw@Host:8080/live/u/p/123.ts?token=x", Some("http://host:8080/live/123.ts")),
        ("http://h/u/p/123.ts", Some("http://h/123.ts")),
        ("http://h/u/p/123", Some("http://h/123")),
        ("http://h/movie/user/pass/55.mkv", Some("http://h/movie/55.mkv")),
        ("http://h/series/user/pass/1001.mp4", Some("http://h/series/1001.mp4")),
        ("http://h/timeshift/u/p/60/2026-10-09:21-00/1234.ts", Some("http://h/timeshift/60/2026-10-09:21-00/1234.ts")),
        ("https://cdn.example/ch/index.m3u8?sig=1#frag", Some("https://cdn.example/ch/index.m3u8")),
        ("https://cdn.example//a//b/", Some("https://cdn.example/a/b")),
        ("http://h/a/b/name.ts", Some("http://h/a/b/name.ts")),
        ("  HTTP://H.EXAMPLE/x  ", Some("http://h.example/x")),
        ("rtmp://h/live/stream", Some("rtmp://h/live/stream")),
        ("http://h", Some("http://h/")),
        ("not a url", None),
        ("file:///home/u/a.ts", None),
        ("", None),
    ];
    for (url, want) in table {
        assert_eq!(url_key(url).as_deref(), want, "url_key({url:?})");
    }
}

#[test]
fn golden_key_allocator() {
    let mut a = KeyAllocator::default();
    let got: Vec<String> = ["m3u:a", "m3u:b", "m3u:a", "m3u:a", "m3u:b", "m3u:c"].iter().map(|k| a.alloc((*k).to_owned()).0).collect();
    assert_eq!(got, ["m3u:a", "m3u:b", "m3u:a#2", "m3u:a#3", "m3u:b#2", "m3u:c"]);
}

#[test]
fn golden_fingerprint() {
    let fp = fingerprint(Some("123"), Some("bbc1.uk"), "UK | BBC One HD", Some("UK | News"), Some("http://u:p@h/live/u/p/123.ts?x=1"));
    assert_eq!(fp.provider_id.as_deref(), Some("123"));
    assert_eq!(fp.tvg_id.as_deref(), Some("bbc1.uk"));
    assert_eq!(fp.norm_name, "bbcone");
    assert_eq!(fp.norm_group.as_deref(), Some("uknews"));
    assert_eq!(fp.url_key.as_deref(), Some("http://h/live/123.ts"));
    let fp = fingerprint(None, None, "X", None, Some("garbage"));
    assert_eq!((fp.provider_id, fp.tvg_id, fp.norm_name.as_str(), fp.norm_group, fp.url_key), (None, None, "x", None, None));
}

// ---------------------------------------------------------------------------------------------
// property tests (deterministic generator; no extra crates)

/// xorshift64* — deterministic so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

const WORDS: &[&str] = &[
    "bbc",
    "one",
    "news",
    "sky",
    "cinema",
    "sports",
    "arena",
    "kanal",
    "das",
    "erste",
    "rai",
    "uno",
    "télé",
    "música",
    "Ελλάδα",
    "日本",
    "Москва",
    "24",
    "7",
    "2",
    "max",
    "kids",
    "zdf",
    "info",
    "première",
    "e!",
    "nick",
    "jr.",
    "a&e",
    "mtv",
    "fox",
    "life",
];
const PREFIXES: &[&str] = &["UK | ", "uk|", "US: ", "|EN| ", "[VIP] ", "[UK] ", "DE - ", "FR:", "|AR| ", "[HD] "];
const SUFFIXES: &[&str] = &[" HD", " FHD", " UHD", " 4K", " SD", " 1080p", " 720p", " HEVC", " H.265", " (backup)", " h264"];

/// A plain channel name: words only (no prefixes, quality tags, dashes or colons).
fn plain_name(r: &mut Rng) -> String {
    let n = 1 + r.below(3);
    let mut words: Vec<&str> = (0..n).map(|_| r.pick(WORDS)).collect();
    // quality-tag-like words would legitimately be dropped; WORDS has none, but keep it explicit
    words.retain(|w| !["hd", "sd", "4k"].contains(&w.to_lowercase().as_str()));
    let mut s = words.join(" ");
    if r.chance(30) {
        s = s.to_uppercase();
    }
    s
}

fn decorated(r: &mut Rng, plain: &str) -> String {
    let mut s = String::new();
    for _ in 0..r.below(3) {
        s.push_str(r.pick(PREFIXES));
    }
    s.push_str(plain);
    for _ in 0..r.below(3) {
        s.push_str(r.pick(SUFFIXES));
    }
    if r.chance(20) {
        s = format!("  {s}  ");
    }
    s
}

const CASES: usize = 4000;

#[test]
fn prop_normalize_name_output_is_compact_and_idempotent() {
    let mut r = Rng(0x5EED_0001);
    for _ in 0..CASES {
        let plain = plain_name(&mut r);
        let raw = decorated(&mut r, &plain);
        let n = normalize_name(&raw);
        assert!(n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '+'), "{raw:?} → {n:?}");
        assert_eq!(normalize_name(&n), n, "idempotent for {raw:?}");
        let has_alnum = deunicode::deunicode(&raw).chars().any(|c| c.is_ascii_alphanumeric());
        assert_eq!(!n.is_empty(), has_alnum, "{raw:?}");
    }
}

#[test]
fn prop_prefixes_quality_case_and_spacing_do_not_change_the_name() {
    let mut r = Rng(0x5EED_0002);
    for _ in 0..CASES {
        let plain = plain_name(&mut r);
        let raw = decorated(&mut r, &plain);
        let base = normalize_name(&plain);
        assert_eq!(normalize_name(&raw), base, "{raw:?} vs {plain:?}");
        assert_eq!(normalize_name(&raw.to_lowercase()), base, "lower {raw:?}");
        assert_eq!(normalize_name(&plain.to_uppercase()), base, "upper {plain:?}");
        assert_eq!(normalize_name(&plain.replace(' ', "   ")), base, "spacing {plain:?}");
    }
}

#[test]
fn prop_timeshift_suffix_is_kept_and_distinct() {
    let mut r = Rng(0x5EED_0003);
    for _ in 0..CASES {
        let plain = plain_name(&mut r);
        let shift = ["+1", "+2", "+24"][r.below(3)];
        let shifted = decorated(&mut r, &format!("{plain} {shift}"));
        let n = normalize_name(&shifted);
        assert!(n.ends_with(shift), "{shifted:?} → {n:?}");
        assert_ne!(n, normalize_name(&plain), "{shifted:?} must differ from {plain:?}");
    }
}

#[test]
fn prop_m3u_keys_are_stable_well_formed_and_distinct() {
    let mut r = Rng(0x5EED_0004);
    let mut seen: HashSet<(Option<String>, String, Option<String>)> = HashSet::new();
    let mut keys: HashSet<String> = HashSet::new();
    for i in 0..CASES {
        let tvg = r.chance(60).then(|| format!("{}.{}", r.pick(WORDS), i % 97));
        let plain = plain_name(&mut r);
        let name = normalize_name(&decorated(&mut r, &plain));
        let group = r.chance(70).then(|| normalize_group(r.pick(WORDS)));
        let k = m3u_base_key(tvg.as_deref(), &name, group.as_deref());
        assert_eq!(k, m3u_base_key(tvg.as_deref(), &name, group.as_deref()), "deterministic");
        assert_eq!(k.len(), 20);
        assert!(k.starts_with("m3u:") && k[4..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{k}");
        if seen.insert((tvg, name, group)) {
            assert!(keys.insert(k.clone()), "collision on {k}");
        }
    }
}

#[test]
fn prop_key_allocator_is_unique_and_order_deterministic() {
    let mut r = Rng(0x5EED_0005);
    let bases: Vec<String> = (0..CASES).map(|_| format!("m3u:{}", r.below(50))).collect();
    let run = || {
        let mut a = KeyAllocator::default();
        bases.iter().map(|b| a.alloc(b.clone()).0).collect::<Vec<_>>()
    };
    let first = run();
    assert_eq!(first, run(), "same file order → same keys");
    assert_eq!(first.iter().collect::<HashSet<_>>().len(), first.len(), "unique within an import");
    for (i, k) in first.iter().enumerate() {
        let earlier = bases[..i].iter().filter(|b| **b == bases[i]).count();
        let want = if earlier == 0 { bases[i].clone() } else { format!("{}#{}", bases[i], earlier + 1) };
        assert_eq!(*k, want);
    }
}

#[test]
fn prop_url_key_ignores_credentials_query_and_host_case() {
    let mut r = Rng(0x5EED_0006);
    let hosts = ["panel.example", "Panel.Example:8080", "10.0.0.1:25461", "cdn.example"];
    let users = ["u", "user.name", "a1b2c3", "x%40y"];
    for _ in 0..CASES {
        let host = r.pick(&hosts);
        let id = r.below(100_000);
        let ext = r.pick(&[".ts", ".m3u8", "", ".mkv"]);
        let kind = r.pick(&["live", "movie", "series"]);
        let canonical = url_key(&format!("http://{host}/{kind}/u/p/{id}{ext}")).expect("parsable");
        let (u, p) = (r.pick(&users), r.pick(&users));
        let query = if r.chance(50) { format!("?token={}", r.next()) } else { String::new() };
        let userinfo = if r.chance(30) { format!("{u}:{p}@") } else { String::new() };
        let variant = format!("http://{userinfo}{}/{kind}/{u}/{p}/{id}{ext}{query}", host.to_uppercase());
        assert_eq!(url_key(&variant).as_deref(), Some(canonical.as_str()), "{variant}");
        assert!(!canonical.contains('?') && !canonical.contains('@'), "{canonical}");
        // short form `/u/p/<id>` too
        let short = format!("http://{host}/{u}/{p}/{id}{ext}{query}");
        assert_eq!(url_key(&short), url_key(&format!("http://{host}/x/y/{id}{ext}")), "{short}");
    }
}

#[test]
fn prop_fingerprint_matches_the_key_functions() {
    let mut r = Rng(0x5EED_0007);
    for _ in 0..CASES {
        let plain = plain_name(&mut r);
        let name = decorated(&mut r, &plain);
        let word = r.pick(WORDS);
        let group = decorated(&mut r, word);
        let fp = fingerprint(None, None, &name, Some(&group), None);
        assert_eq!(fp.norm_name, normalize_name(&name));
        let gk = group_key(ItemKind::Channel, &group);
        assert_eq!(gk.0, format!("channel:{}", fp.norm_group.as_deref().unwrap()), "norm_group = group key without the kind prefix");
    }
}
