//! A real-size, two-playlist catalogue for the library-load benches (perf track). Owner: perf.
//!
//! Shaped like the field catalogue whose load regressed after the shared category rule and
//! "Also hide titles in other languages" (DESIGN §28.3, §28.12): ≈ 80k channels, 300k movies
//! and 40k series over two Xtream-like playlists, ≈ 2.5k categories (country / language
//! tagged with every separator style, untagged Latin names, and untagged names written in
//! Arabic, Greek, Hebrew, Hangul, Thai, Cyrillic and Han script), titles carrying their own
//! market prefix (`EN - …`, `DE ★ …`, `AR-SUBS - …`, `MULTI - …`) or none, the same titles on
//! both playlists (so they stack), a few hundred favourites and titles with progress, and an
//! English-only shared rule (which hides other-language titles automatically).
//!
//! Everything goes through the importer (`store::import`, 2 000-row batches, title match data
//! by `finish_catalogue`), so `grp.raw_name`, the keys and the `match_*` columns are what a
//! refresh writes. Deterministic: no randomness, the same database on every run.

use std::path::Path;
use std::time::Duration;

use tuner::model::{CategoryFilter, GroupKey, ItemKey, ItemKind, SourceId, StreamLocator, Timestamp};
use tuner::sources::identity::{group_key, normalize_name, url_key};
use tuner::sources::smart_names::{clean_channel_name, clean_vod_name, group_display};
use tuner::store::Store;
use tuner::store::import::{self, ImportChannel, ImportEpisode, ImportGroup, ImportMovie, ImportSeries, ImportSeriesEpisodes, NameQuality};

const T0: Timestamp = 1_791_576_000;
const BATCH: usize = 2_000;
/// Episodes of every series (all listed, as an M3U playlist imports them).
const EPISODES: usize = 20;

/// One playlist's size: (channels, movies, series) and its categories per kind.
pub struct Playlist {
    pub name: &'static str,
    pub channels: usize,
    pub movies: usize,
    pub series: usize,
    pub channel_groups: usize,
    pub movie_groups: usize,
    pub series_groups: usize,
}

pub const PLAYLISTS: [Playlist; 2] = [
    Playlist { name: "Alpha", channels: 50_000, movies: 200_000, series: 25_000, channel_groups: 450, movie_groups: 800, series_groups: 300 },
    Playlist { name: "Beta", channels: 30_000, movies: 100_000, series: 15_000, channel_groups: 250, movie_groups: 500, series_groups: 200 },
];

/// Totals of [`PLAYLISTS`] (what a full load must hold before any rule).
pub fn totals() -> (usize, usize, usize, usize) {
    PLAYLISTS
        .iter()
        .fold((0, 0, 0, 0), |(c, m, s, g), p| (c + p.channels, m + p.movies, s + p.series, g + p.channel_groups + p.movie_groups + p.series_groups))
}

/// Tag prefixes as panels write them; the title market each one's titles mostly carry.
const PREFIXES: &[(&str, &str)] = &[
    ("EN", "EN"),
    ("UK", "EN"),
    ("US", "EN"),
    ("CA", "EN"),
    ("AU", "EN"),
    ("DE", "DE"),
    ("FR", "FR"),
    ("IT", "IT"),
    ("ES", "ES"),
    ("NL", "NL"),
    ("PL", "PL"),
    ("TR", "TR"),
    ("AR", "AR"),
    ("GR", "GR"),
    ("PT", "PT"),
    ("SE", "SW"),
    ("NO", "NO"),
    ("DK", "DK"),
    ("FI", "FI"),
    ("IN", "IN"),
    ("PK", "PK"),
    ("AL", "ALB"),
    ("EX-YU", "EX"),
    ("RO", "RO"),
    ("HU", "HU"),
    ("KU", "KU"),
    ("IR", "IR"),
    ("SO", "SO"),
];
const SEPARATORS: &[&str] = &[" | ", ": ", " ★ ", " ✪ ", " - ", " ❖ "];
const GENRES: &[&str] = &[
    "Action",
    "Comedy",
    "Drama",
    "Kids",
    "Documentary",
    "Horror",
    "Sport",
    "News",
    "4K",
    "Classics",
    "2024",
    "2025",
    "Netflix",
    "Disney+",
    "Romance",
    "Thriller",
    "Animation",
    "Crime",
    "SCI-FI",
    "Family",
];
/// Untagged names in a script one language uses (read as that language), and in scripts that
/// say too little (no tag).
const SCRIPT_NAMES: &[&str] = &[
    "مسلسلات شاهد الأصلية",
    "أفلام أجنبية قديمه",
    "الجزيرة 360",
    "VIP ✪ كأس العالم",
    "⁴ᴷ ³⁸⁴⁰ᴾ ᴰᴼᴸᴮʸ ᴬᵁᴰᴵᴼ افلام عربية",
    "Netflix مسلسلات أجنبية",
    "ταινίες με υπότιτλους",
    "σειρές",
    "סרטים",
    "한국 영화",
    "ภาพยนตร์",
    "Фильмы",
    "電影",
];
const LATIN_NAMES: &[&str] = &["4K | Sports", "24/7 Movies", "Netflix", "Kids", "Top 250", "Oscar Winners", "New Releases", "VIP Cinema"];

/// The `i`-th category of a kind: (raw name, title market of its titles or `None` for mixed).
fn category(kind: ItemKind, i: usize) -> (String, Option<&'static str>) {
    let k = match kind {
        ItemKind::Channel => 0,
        ItemKind::Movie => 1,
        _ => 2,
    };
    match i % 10 {
        0..=6 => {
            let (p, market) = PREFIXES[(i * 7 + k) % PREFIXES.len()];
            let sep = SEPARATORS[(i / 3) % SEPARATORS.len()];
            let genre = GENRES[(i / 2) % GENRES.len()];
            (format!("{p}{sep}{genre} {i}"), Some(market))
        }
        7 | 8 => {
            let n = SCRIPT_NAMES[(i / 10 + k) % SCRIPT_NAMES.len()];
            let market = match n.chars().find(|c| c.is_alphabetic() && !c.is_ascii() && (*c as u32) > 0x0370).map(|c| c as u32) {
                Some(0x0600..=0x06FF) => Some("AR"),
                Some(0x0370..=0x03FF) => Some("GR"),
                _ => None,
            };
            (format!("{n} {i}"), market)
        }
        _ => (format!("{} {i}", LATIN_NAMES[(i / 10) % LATIN_NAMES.len()]), None),
    }
}

/// A title name with its own market prefix (or none), in the styles panels use.
fn title_name(market: Option<&str>, title: usize, j: usize, series: bool) -> String {
    let year = 1960 + title % 65;
    let what = if series { "Show" } else { "Title" };
    // the category's market mostly; sometimes another, multi-audio, subtitled or untagged
    let tag = match (j % 20, market) {
        (0, _) => Some("MULTI".to_owned()),
        (1 | 2, _) => None,
        (3, Some("AR")) => Some("AR-SUBS".to_owned()),
        (4, _) => Some(PREFIXES[j % PREFIXES.len()].1.to_owned()),
        (5, Some(m)) => Some(format!("{m}-SUB")),
        (_, Some(m)) => Some(m.to_owned()),
        (_, None) => j.is_multiple_of(3).then(|| PREFIXES[(j / 3) % PREFIXES.len()].1.to_owned()),
    };
    match (tag, j % 3) {
        (Some(t), 0) => format!("{t} - {what} {title} ({year})"),
        (Some(t), 1) => format!("{t} ★ {what} {title} - {year}"),
        (Some(t), _) => format!("{t}| {what} {title} [{year}]"),
        (None, _) => format!("{what} {title} ({year})"),
    }
}

fn groups(kind: ItemKind, n: usize, offset: usize) -> (Vec<ImportGroup>, Vec<Option<&'static str>>) {
    (0..n)
        .map(|i| {
            let (raw, market) = category(kind, i + offset);
            let g = ImportGroup {
                kind,
                key: group_key(kind, &raw),
                name: group_display(&raw, true),
                raw_name: Some(raw),
                provider_id: Some(i.to_string()),
                provider_order: i as i32,
                adult: false,
            };
            (g, market)
        })
        .unzip()
}

/// Category of the `j`-th of `n` items over `ng` categories: contiguous blocks, like a panel's
/// listing.
fn block(j: usize, n: usize, ng: usize) -> usize {
    (j * ng / n).min(ng - 1)
}

fn import_playlist(store: &Store, src: SourceId, p: &Playlist, pi: usize) {
    let w = store.writer();
    let (cg, _) = groups(ItemKind::Channel, p.channel_groups, pi * 1_000);
    let (mg, m_market) = groups(ItemKind::Movie, p.movie_groups, pi * 1_000);
    let (sg, s_market) = groups(ItemKind::Series, p.series_groups, pi * 1_000);
    let all: Vec<ImportGroup> = cg.iter().chain(&mg).chain(&sg).cloned().collect();
    let imp = w
        .blocking(move |c| {
            let imp = import::begin_catalogue(c, src, T0)?;
            import::upsert_groups(c, &imp, &all)?;
            Ok(imp)
        })
        .expect("begin");

    let channels: Vec<ImportChannel> = (0..p.channels)
        .map(|j| {
            let g = &cg[block(j, p.channels, cg.len())];
            let (prefix, _) = PREFIXES[j % PREFIXES.len()];
            // quality duplicates of one channel (`… HD` / `… FHD` / `… 4K`)
            let quality = ["", " HD", " FHD", " 4K", " SD"][j % 5];
            let name = format!("{prefix}: Channel {}{quality}", j / 3);
            let url = format!("http://panel{pi}.test:8080/live/user/pass/{j}.ts");
            let cleaned = clean_channel_name(&name);
            ImportChannel {
                key: ItemKey(format!("xt:live:{j}")),
                provider_id: Some(j.to_string()),
                clean_name: cleaned.name.clone(),
                quality: NameQuality::of(&cleaned),
                norm_name: normalize_name(&name),
                name,
                number: Some(j as u32 + 1),
                logo: Some(format!("http://img.test/logo/{j}.png")),
                tvg_id: Some(format!("channel{}.{}", j / 3, prefix.to_lowercase())),
                tvg_name: None,
                tvg_shift_min: 0,
                url_key: url_key(&url),
                stream: StreamLocator::Url { url },
                headers: None,
                catchup: None,
                radio: false,
                unsupported: None,
                provider_order: j as i32,
                added_at: Some(T0 - j as i64),
                groups: vec![g.key.clone()],
                adult: false,
            }
        })
        .collect();
    for batch in channels.chunks(BATCH) {
        let (imp, batch) = (imp.clone(), batch.to_vec());
        w.blocking(move |c| import::write_channels(c, &imp, &batch)).expect("channels");
    }
    drop(channels);

    let movie = |j: usize| -> ImportMovie {
        let gi = block(j, p.movies, mg.len());
        // the same titles on both playlists (copies stack), about two copies per title each
        let title = j / 2 + (j % 7) * 11;
        let name = title_name(m_market[gi], title, j, false);
        let url = format!("http://panel{pi}.test:8080/movie/user/pass/{j}.mkv");
        let (clean_name, year) = clean_vod_name(&name);
        let mut groups: Vec<GroupKey> = vec![mg[gi].key.clone()];
        // one in 25 also in a second category
        if j.is_multiple_of(25) {
            groups.push(mg[(gi + 37) % mg.len()].key.clone());
        }
        ImportMovie {
            key: ItemKey(format!("xt:vod:{j}")),
            provider_id: Some(j.to_string()),
            norm_name: normalize_name(&name),
            clean_name,
            name,
            year,
            poster: Some(format!("http://img.test/poster/{j}.jpg")),
            rating: Some((j % 100) as f32 / 10.0),
            url_key: url_key(&url),
            stream: StreamLocator::Url { url },
            provider_order: j as i32,
            added_at: Some(T0 - j as i64),
            groups,
            tmdb: j.is_multiple_of(50).then_some(title as u32 + 1),
            adult: false,
        }
    };
    for start in (0..p.movies).step_by(BATCH) {
        let batch: Vec<ImportMovie> = (start..(start + BATCH).min(p.movies)).map(movie).collect();
        let imp = imp.clone();
        w.blocking(move |c| import::write_movies(c, &imp, &batch)).expect("movies");
    }

    let series = |j: usize| -> ImportSeries {
        let gi = block(j, p.series, sg.len());
        let title = j / 2 + (j % 5) * 7;
        let name = title_name(s_market[gi], title, j, true);
        let (clean_name, year) = clean_vod_name(&name);
        ImportSeries {
            key: ItemKey(format!("xt:series:{j}")),
            provider_id: Some(j.to_string()),
            norm_name: normalize_name(&name),
            clean_name,
            name,
            year,
            cover: Some(format!("http://img.test/cover/{j}.jpg")),
            backdrop: None,
            rating: Some((j % 100) as f32 / 10.0),
            plot: None,
            genre: Some("Drama".to_owned()),
            cast: None,
            director: None,
            release_date: None,
            last_modified: Some(T0),
            provider_order: j as i32,
            added_at: Some(T0 - j as i64),
            groups: vec![sg[gi].key.clone()],
            tmdb: None,
            adult: false,
        }
    };
    for start in (0..p.series).step_by(BATCH) {
        // every episode listed, like an M3U playlist imports them
        let batch: Vec<ImportSeriesEpisodes> = (start..(start + BATCH).min(p.series))
            .map(|j| ImportSeriesEpisodes {
                series: series(j),
                episodes: (0..EPISODES)
                    .map(|e| ImportEpisode {
                        key: ItemKey(format!("xt:episode:{j}:{e}")),
                        provider_id: Some(format!("{j}{e:03}")),
                        season: 1 + (e / 10) as u32,
                        episode: 1 + (e % 10) as u32,
                        title: format!("Episode {}", e + 1),
                        plot: None,
                        duration_s: Some(2_700),
                        still: None,
                        air_date: None,
                        rating: None,
                        stream: StreamLocator::Url { url: format!("http://panel{pi}.test:8080/series/user/pass/{j}{e:03}.mkv") },
                    })
                    .collect(),
            })
            .collect();
        let imp = imp.clone();
        w.blocking(move |c| import::write_series_episodes(c, &imp, &batch)).expect("series");
    }
    w.blocking(move |c| import::finish_catalogue(c, &imp, &[ItemKind::Channel, ItemKind::Movie, ItemKind::Series], T0)).expect("finish");
}

/// The English-only shared rule (the defaults of an English-speaking user): it hides other
/// countries' categories and, automatically, titles in other languages.
pub fn english_rule() -> CategoryFilter {
    CategoryFilter { keep: tuner::sources::category_tags::default_keep(None), ..Default::default() }
}

/// Seed the catalogue into a new profile database at `db`.
pub fn seed(db: &Path) {
    let store = Store::open(db, None).expect("store");
    for (pi, p) in PLAYLISTS.iter().enumerate() {
        let name = p.name;
        let src = store.writer().blocking(move |c| super::seed::seed_source(c, name)).expect("source");
        import_playlist(&store, src, p, pi);
    }
    store
        .writer()
        .blocking(|c| {
            let rule = english_rule();
            tuner::store::sources::set_shared_category_filter(c, Some(&rule))?;
            // favourites and progress (never hidden by the language rule)
            let list = tuner::store::user::default_fav_list(c)?;
            for j in (0..300).map(|j| j * 613) {
                let item = tuner::model::ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey(format!("xt:vod:{j}")) };
                tuner::store::user::set_favourite(c, list, &item, true, T0)?;
            }
            c.execute(
                "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, updated_at, title)
                 SELECT source_id, 'movie', key, 600, 5400, ?1, name FROM movie WHERE id % 997 = 0",
                [T0],
            )?;
            // episodes watched (series in progress: a few thousand rows)
            c.execute(
                "INSERT INTO watch_progress(source_id, kind, item_key, position_s, duration_s, updated_at, title)
                 SELECT source_id, 'episode', key, 600, 2700, ?1, title FROM episode WHERE id % 211 = 0",
                [T0],
            )?;
            c.execute_batch("ANALYZE")?;
            Ok(())
        })
        .expect("rule");
    assert!(store.close(Duration::from_secs(60)), "seed writer did not finish");
}

/// Replace the shared rule of a seeded database (`None`: no rule; the variants measured).
pub fn set_rule(store: &Store, rule: Option<CategoryFilter>) {
    store.writer().blocking(move |c| tuner::store::sources::set_shared_category_filter(c, rule.as_ref())).expect("rule");
}
