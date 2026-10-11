//! How "All playlists" stacks a real catalogue (DESIGN §29.10–§29.12, P2): merge rates per
//! kind and rule, refusals, the largest stacks, and random stacks to check by eye — across
//! playlists AND inside one playlist (4K / brand-shelf copies).
//!
//! `cargo run --release --example dedup_report -- <tuner.db> [samples] [seed]`
//!
//! Reads the database READ-ONLY and only titles, years, TMDB ids and category names (never a
//! playlist's login, URLs or streams). Keys are computed from the provider names with the same
//! code the app stores them with (`sources::vod_match`), so any schema version works. Every
//! copy counts — the app stacks only copies that aren't hidden, so with a category filter its
//! stacks are smaller.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use tuner::model::{GroupId, ItemKey, ItemKind, MovieId, SourceId, VodKind, VodSummary};
use tuner::sources::vod_match::{self, ClusterItem, MatchInfo, PrefixCodes};
use tuner::store::stacks::{self, CopyRaw};

struct Row {
    src: i64,
    name: String,
    info: MatchInfo,
    tmdb: Option<u32>,
    cat: String,
}

/// (id, source, name, year, TMDB id).
type RawRow = (i64, i64, String, Option<u16>, Option<String>);

fn has_column(c: &Connection, table: &str, col: &str) -> bool {
    c.query_row("SELECT count(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2", [table, col], |r| r.get(0)).unwrap_or(false)
}

fn load(c: &Connection, kind: ItemKind, timing: &mut BTreeMap<String, f64>) -> Vec<Row> {
    let (table, link, col) = match kind {
        ItemKind::Movie => ("movie", "movie_group", "movie_id"),
        _ => ("series", "series_group", "series_id"),
    };
    let tmdb = if has_column(c, table, "tmdb_id") { "tmdb_id" } else { "NULL" };
    let gname = if has_column(c, "grp", "raw_name") { "COALESCE(g.raw_name, g.name)" } else { "g.name" };
    let t = Instant::now();
    let mut cats: HashMap<i64, String> = HashMap::new();
    let mut stmt = c.prepare(&format!("SELECT l.{col}, {gname} FROM {link} l JOIN grp g ON g.id = l.group_id")).expect("groups");
    for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).expect("groups") {
        let (id, name) = r.expect("group row");
        cats.entry(id).or_insert(name);
    }
    let mut stmt =
        c.prepare(&format!("SELECT id, source_id, name, year, {tmdb} FROM {table} ORDER BY source_id, provider_order, id")).expect("titles");
    let raw: Vec<RawRow> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<rusqlite::types::Value>>(4)?.map(text))))
        .expect("titles")
        .collect::<rusqlite::Result<_>>()
        .expect("title rows");
    timing.insert(format!("{table}: read rows"), t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    let mut codes: HashMap<i64, PrefixCodes> = HashMap::new();
    let mut per_src: HashMap<i64, Vec<&str>> = HashMap::new();
    for (_, src, name, _, _) in &raw {
        per_src.entry(*src).or_default().push(name);
    }
    for (src, names) in per_src {
        codes.insert(src, PrefixCodes::from_names(names));
    }
    let rows: Vec<Row> = raw
        .iter()
        .map(|(id, src, name, year, tmdb)| Row {
            src: *src,
            name: name.clone(),
            info: vod_match::parse(name, *year, &codes[src]),
            tmdb: tmdb.as_deref().and_then(vod_match::tmdb_id),
            cat: cats.get(id).cloned().unwrap_or_default(),
        })
        .collect();
    timing.insert(format!("{table}: parse {} names", rows.len()), t.elapsed().as_secs_f64() * 1e3);
    rows
}

fn text(v: rusqlite::types::Value) -> String {
    match v {
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Text(s) => s,
        _ => String::new(),
    }
}

/// Tiny deterministic RNG (xorshift) for the samples.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn sample<T: Clone>(&mut self, v: &[T], n: usize) -> Vec<T> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        for i in (1..idx.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            idx.swap(i, j);
        }
        idx.into_iter().take(n).map(|i| v[i].clone()).collect()
    }
}

fn show(r: &Row) -> String {
    let lang = r.info.lang_text().unwrap_or_else(|| "·".into());
    let year = r.info.year.map_or("—".to_owned(), |y| y.to_string());
    let tmdb = r.tmdb.map(|t| format!(" tmdb {t}")).unwrap_or_default();
    format!("`{}` (src {}, {lang}, {year}{tmdb}) <{}>", r.name.trim(), r.src, r.cat.chars().take(28).collect::<String>())
}

fn report(kind: ItemKind, rows: &[Row], samples: usize, rng: &mut Rng, timing: &mut BTreeMap<String, f64>) {
    let label = if kind == ItemKind::Movie { "Movies" } else { "Series" };
    let items: Vec<ClusterItem> = rows.iter().map(|r| ClusterItem { hash: r.info.stack_hash(kind), tmdb: r.tmdb, origin: r.info.origin }).collect();
    let mut best = f64::MAX;
    let mut stacks = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        stacks = vod_match::cluster(&items);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    timing.insert(format!("{label}: cluster {} copies (best of 5)", rows.len()), best);
    // the library build's whole stacking step (languages, categories, clustering, copy order)
    let mut texts: HashMap<String, Arc<str>> = HashMap::new();
    let mut cats: HashMap<&str, i64> = HashMap::new();
    let summaries: Vec<VodSummary> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let n = cats.len() as i64;
            VodSummary {
                kind: VodKind::Movie(MovieId(i as i64)),
                source_id: SourceId(r.src),
                key: ItemKey(String::new()),
                name: Arc::from(""),
                year: r.info.year,
                poster: None,
                rating: None,
                group_ids: vec![GroupId(*cats.entry(r.cat.as_str()).or_insert(n))],
                added_at: None,
                provider_order: 0,
                locked: false,
            }
        })
        .collect();
    let raws: Vec<CopyRaw> = rows
        .iter()
        .map(|r| CopyRaw {
            hash: r.info.stack_hash(kind),
            lang: r.info.lang_text().map(|t| texts.entry(t.clone()).or_insert_with(|| Arc::from(t.as_str())).clone()),
            tags: r.info.tags,
            origin: r.info.origin,
            tmdb: r.tmdb,
            ..Default::default()
        })
        .collect();
    let prefs = vec!["en".to_owned()];
    let mut best = f64::MAX;
    let mut built = None;
    for _ in 0..5 {
        let raw = raws.clone();
        let t = Instant::now();
        let s = stacks::build(&summaries, raw, |_| true, &prefs);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        built = Some(s);
    }
    timing.insert(format!("{label}: stacks::build {} copies (best of 5)", rows.len()), best);
    let built = built.expect("built");
    let mut top_langs: HashMap<String, usize> = HashMap::new();
    for s in built.ids() {
        *top_langs.entry(built.lang_label(built.top(s)).unwrap_or_else(|| "(none)".into())).or_default() += 1;
    }
    let mut top_langs: Vec<_> = top_langs.into_iter().collect();
    top_langs.sort_by_key(|a| std::cmp::Reverse(a.1));
    let in_stacks: usize = stacks.iter().map(Vec::len).sum();
    let cards = rows.len() - in_stacks + stacks.len();
    let cross: Vec<&Vec<u32>> = stacks.iter().filter(|s| s.iter().any(|i| rows[*i as usize].src != rows[s[0] as usize].src)).collect();
    let same: Vec<&Vec<u32>> = stacks.iter().filter(|s| s.iter().all(|i| rows[*i as usize].src == rows[s[0] as usize].src)).collect();
    let by_tmdb = stacks
        .iter()
        .filter(|s| s.iter().map(|i| rows[*i as usize].info.stack_hash(kind)).collect::<std::collections::HashSet<_>>().len() > 1)
        .count();
    let multi_lang =
        stacks.iter().filter(|s| s.iter().map(|i| rows[*i as usize].info.lang.clone()).collect::<std::collections::HashSet<_>>().len() > 1).count();
    println!("## {label}\n");
    println!(
        "| | copies | All cards | saved | stacks | cross-playlist stacks | same-playlist-only stacks | stacks joined by TMDB id | stacks with 2+ languages |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    println!(
        "| {label} | {} | {cards} | {} ({:.1} %) | {} | {} | {} | {by_tmdb} | {multi_lang} |\n",
        rows.len(),
        rows.len() - cards,
        100.0 * (rows.len() - cards) as f64 / rows.len().max(1) as f64,
        stacks.len(),
        cross.len(),
        same.len()
    );
    let mut sizes: BTreeMap<usize, usize> = BTreeMap::new();
    for s in &stacks {
        *sizes.entry(s.len().min(10)).or_default() += 1;
    }
    println!("Stack sizes: {}\n", sizes.iter().map(|(k, v)| format!("{}{k}: {v}", if *k == 10 { "≥" } else { "" })).collect::<Vec<_>>().join(" · "));
    let no_year = rows.iter().filter(|r| r.info.refusal() == Some(vod_match::Refusal::NoYear)).count();
    let empty = rows.iter().filter(|r| r.info.refusal() == Some(vod_match::Refusal::Empty)).count();
    let generic_stacked = stacks.iter().filter(|s| rows[s[0] as usize].info.generic).count();
    let tmdb = rows.iter().filter(|r| r.tmdb.is_some()).count();
    let origin_split = {
        let mut by: HashMap<i64, Vec<&Row>> = HashMap::new();
        for r in rows {
            if let Some(h) = r.info.stack_hash(kind) {
                by.entry(h).or_default().push(r);
            }
        }
        by.values().filter(|v| v.iter().filter_map(|r| r.info.origin).collect::<std::collections::HashSet<_>>().len() > 1).count()
    };
    println!(
        "Refused by key: no year {no_year}, nothing left of the title {empty}. Copies with a TMDB id: {tmdb}. Short / all-digit / stop-list titles stacked with their year: {generic_stacked} stacks. Same key and year kept apart by two different `(XX)` origins: {origin_split}.\n"
    );
    let mut langs: HashMap<String, usize> = HashMap::new();
    for r in rows {
        *langs.entry(r.info.lang_text().unwrap_or_else(|| "(none)".into())).or_default() += 1;
    }
    let mut langs: Vec<_> = langs.into_iter().collect();
    langs.sort_by_key(|a| std::cmp::Reverse(a.1));
    println!("Language / market tags (top 30): {}\n", langs.iter().take(30).map(|(l, n)| format!("{l} {n}")).collect::<Vec<_>>().join(", "));
    println!(
        "Top card language for an English user (`?` = taken from the category): {}\n",
        top_langs.iter().take(15).map(|(l, n)| format!("{l} {n}")).collect::<Vec<_>>().join(", ")
    );

    let mut largest: Vec<&Vec<u32>> = stacks.iter().collect();
    largest.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    println!("### Largest {label} stacks\n");
    for (n, s) in largest.iter().take(15).enumerate() {
        let r0 = &rows[s[0] as usize];
        println!("{}. **{}** {} — {} copies", n + 1, r0.info.key, r0.info.year.map_or("—".into(), |y| y.to_string()), s.len());
        for i in s.iter().take(8) {
            println!("   - {}", show(&rows[*i as usize]));
        }
        if s.len() > 8 {
            println!("   - … {} more", s.len() - 8);
        }
    }
    for (title, pool) in [("across playlists", &cross), ("inside one playlist", &same)] {
        println!("\n### {samples} random {label} stacks {title}\n");
        for (n, s) in rng.sample(pool, samples).iter().enumerate() {
            let shown: Vec<String> = s.iter().take(5).map(|i| show(&rows[*i as usize])).collect();
            println!("{}. {}{}", n + 1, shown.join(" ⇄ "), if s.len() > 5 { format!(" (+{} more)", s.len() - 5) } else { String::new() });
        }
    }
    println!();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dedup_report <tuner.db> [samples] [seed]");
        std::process::exit(2);
    };
    let samples: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(25);
    let seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0x5eed);
    let c = Connection::open_with_flags(format!("file:{path}?mode=ro"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .expect("open read-only");
    let mut timing = BTreeMap::new();
    let mut rng = Rng(seed | 1);
    println!("# Stacks in \"All playlists\" — `{}`\n", std::path::Path::new(&path).file_name().and_then(|f| f.to_str()).unwrap_or(""));
    println!(
        "Rules v{} (`sources::vod_match`): same key + same known year, across languages; TMDB ids decide when present; two different ids or `(XX)` origins never stack.\n",
        vod_match::RULES_VERSION
    );
    for kind in [ItemKind::Movie, ItemKind::Series] {
        let rows = load(&c, kind, &mut timing);
        report(kind, &rows, samples, &mut rng, &mut timing);
    }
    println!("## Timing (ms)\n");
    for (k, v) in &timing {
        println!("- {k}: {v:.1}");
    }
}
