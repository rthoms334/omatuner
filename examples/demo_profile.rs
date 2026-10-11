//! A made-up demo profile for screenshots (README) and manual checks: three fictional playlists,
//! a day of guide, movies and series with generated posters, and some favourites, history,
//! progress and a reminder.
//!
//! `cargo run --example demo_profile -- <dir> [--media <video file>] [--silent]`, then
//! `tuner --profile-dir <dir>`. `--silent` gives mpv no audio output (screenshots).
//!
//! Everything is invented and generated here — names, programmes, logos (monograms), posters
//! (gradients + title) — so screenshots never show a real provider, channel or artwork. The
//! content is fixed (seeded); only the guide's times follow today's date. Live channels play
//! `--media` (looped; `scripts/screenshots.sh` passes a Creative Commons clip), movies and
//! episodes point at `*.tuner-demo.invalid` and never play.
//!
//! It goes through the app's own import path: M3U files are imported by the `RefreshSource`
//! job (smart names, quality badges, country tags, stacks), the XMLTV file by `RefreshEpg`;
//! details and user data are written with the `store` functions the app uses. Images are put
//! straight into the profile's image cache under their (never fetched) URLs.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::Receiver;
use rusqlite::{Connection, params};
use tuner::config::{Config, StartScreen};
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager, JobOutcome};
use tuner::model::{
    EpgSourceKind, HistoryEntry, ItemKey, ItemKind, ItemRef, MovieId, Reminder, ReminderId, ReminderState, SourceColor, SourceDraft, SourceId,
    SourceKind, SourceSettings, Timestamp, WatchProgress,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::sources::http::HttpClient;
use tuner::store::Store;
use tuner::store::import::MovieDetails;
use tuner::ui::widgets::glyphs;

const SEED: u64 = 0x7475_6e65_7264_656d; // "tunerdem"
const IMG: &str = "https://img.tuner-demo.invalid";

#[allow(dead_code)] // the test includes this file and uses `build` only
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut media = None;
    let mut silent = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--media" => media = Some(PathBuf::from(args.next().context("--media needs a file")?)),
            "--silent" => silent = true,
            "-h" | "--help" => {
                println!("usage: demo_profile <dir> [--media <video file>] [--silent]");
                return Ok(());
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            other => bail!("unexpected argument {other}"),
        }
    }
    let dir = dir.context("usage: demo_profile <dir> [--media <video file>] [--silent]")?;
    let summary = build(&dir, media.as_deref(), silent)?;
    println!("{summary}");
    println!("run: tuner --profile-dir {}", dir.display());
    Ok(())
}

// --------------------------------------------------------------------------------- content

#[derive(Clone, Copy, PartialEq, Eq)]
enum Genre {
    News,
    Sports,
    Kids,
    Docs,
    Cinema,
    Music,
    Food,
    Entertainment,
    Weather,
}

/// (name, genre, hue) of every channel brand; the slug is its tvg-id.
const BRANDS: &[(&str, Genre, u32)] = &[
    ("Meridian News", Genre::News, 210),
    ("Harbor 24", Genre::News, 195),
    ("Granite Business", Genre::News, 30),
    ("Atlas World", Genre::News, 170),
    ("Tidewater Weather", Genre::Weather, 200),
    ("Bluefield Sports", Genre::Sports, 220),
    ("Summit Sports", Genre::Sports, 0),
    ("Summit Sports 2", Genre::Sports, 10),
    ("Kestrel Motors", Genre::Sports, 25),
    ("Courtside", Genre::Sports, 280),
    ("Goalpost TV", Genre::Sports, 120),
    ("Pinecone Kids", Genre::Kids, 95),
    ("Little Comet", Genre::Kids, 45),
    ("Doodle Lab", Genre::Kids, 320),
    ("Lumen Docs", Genre::Docs, 50),
    ("Polar Nature", Genre::Docs, 185),
    ("Orbit Science", Genre::Docs, 260),
    ("Atlas Travel", Genre::Docs, 150),
    ("Northstar Cinema", Genre::Cinema, 240),
    ("Ciné Lumière", Genre::Cinema, 340),
    ("Velvet Classics", Genre::Cinema, 350),
    ("Kino Funke", Genre::Cinema, 15),
    ("Cobalt Music", Genre::Music, 230),
    ("Night Shift", Genre::Music, 270),
    ("Ember Food", Genre::Food, 20),
    ("Saffron Kitchen", Genre::Food, 40),
    ("Brightside", Genre::Entertainment, 55),
    ("Fernwood Home", Genre::Entertainment, 110),
    ("Lakeside One", Genre::Entertainment, 190),
    ("Paperlight Drama", Genre::Entertainment, 300),
];

/// One playlist's channels: (category, brand, name suffix).
type Lineup = &'static [(&'static str, &'static str, &'static str)];

const NORTHWIND_LIVE: Lineup = &[
    ("UK | News", "Meridian News", " HD"),
    ("UK | News", "Harbor 24", " FHD"),
    ("UK | News", "Granite Business", " HD"),
    ("UK | News", "Tidewater Weather", ""),
    ("UK | Entertainment", "Brightside", " HD"),
    ("UK | Entertainment", "Lakeside One", " FHD"),
    ("UK | Entertainment", "Paperlight Drama", " HD"),
    ("UK | Entertainment", "Fernwood Home", " SD"),
    ("UK | Kids", "Pinecone Kids", " HD"),
    ("UK | Kids", "Little Comet", ""),
    ("UK | Kids", "Doodle Lab", " SD"),
    ("US | Sports", "Bluefield Sports", " 4K"),
    ("US | Sports", "Summit Sports", " FHD"),
    ("US | Sports", "Summit Sports 2", " HD"),
    ("US | Sports", "Courtside", " HD"),
    ("US | Sports", "Kestrel Motors", " FHD"),
    ("US | Documentary", "Lumen Docs", " 4K"),
    ("US | Documentary", "Polar Nature", " HD"),
    ("US | Documentary", "Orbit Science", " HD"),
    ("FR | Cinéma", "Ciné Lumière", " HD"),
    ("FR | Cinéma", "Velvet Classics", " SD"),
    ("DE | Musik", "Cobalt Music", " HD"),
    ("DE | Musik", "Night Shift", ""),
    ("DE | Kino", "Kino Funke", " FHD"),
    ("UK | Lifestyle", "Ember Food", " HD"),
    ("UK | Lifestyle", "Saffron Kitchen", " HD"),
];

const AURORA_LIVE: Lineup = &[
    ("US | News", "Meridian News", " FHD"),
    ("US | News", "Atlas World", " HD"),
    ("US | News", "Harbor 24", " HD"),
    ("US | Sports", "Bluefield Sports", " FHD"),
    ("US | Sports", "Goalpost TV", " 4K"),
    ("US | Sports", "Summit Sports", " HD"),
    ("US | Sports", "Courtside", " SD"),
    ("UK | Documentary", "Lumen Docs", " FHD"),
    ("UK | Documentary", "Atlas Travel", " HD"),
    ("UK | Documentary", "Polar Nature", " 4K"),
    ("US | Movies", "Northstar Cinema", " 4K"),
    ("US | Movies", "Velvet Classics", " HD"),
    ("FR | Cinéma", "Ciné Lumière", " FHD"),
    ("ES | Deportes", "Goalpost TV", " HD"),
    ("US | Kids", "Pinecone Kids", " FHD"),
    ("US | Kids", "Little Comet", " HD"),
    ("US | Entertainment", "Brightside", " FHD"),
    ("US | Entertainment", "Paperlight Drama", " 4K"),
    ("US | Lifestyle", "Saffron Kitchen", " FHD"),
    ("US | Music", "Cobalt Music", " FHD"),
    ("US | Music", "Night Shift", " HD"),
    ("US | Documentary", "Orbit Science", " FHD"),
    ("US | Sports", "Kestrel Motors", " HD"),
    ("US | Lifestyle", "Ember Food", " HD"),
];

const HARBOR_LIVE: Lineup = &[
    ("Community", "Lakeside One", ""),
    ("Community", "Fernwood Home", " HD"),
    ("Community", "Atlas Travel", ""),
    ("Weather", "Tidewater Weather", " HD"),
    ("News", "Atlas World", ""),
    ("News", "Granite Business", ""),
    ("Music", "Night Shift", " HD"),
    ("Kids", "Doodle Lab", " HD"),
    ("Community", "Brightside", ""),
    ("Music", "Cobalt Music", ""),
];

/// (title, year, genre line, minutes): the movie catalogue.
const MOVIES: &[(&str, u16, &str, u32)] = &[
    ("The Lighthouse Keeper", 2019, "Drama, Mystery", 118),
    ("Midnight Orchard", 2021, "Thriller", 104),
    ("Paper Satellites", 2018, "Science Fiction", 126),
    ("Glass Harbour", 2022, "Crime, Drama", 111),
    ("The Last Cartographer", 2016, "Adventure", 132),
    ("Copper Sky", 2020, "Western", 97),
    ("Northbound", 2023, "Road Movie, Comedy", 101),
    ("Salt & Ember", 2017, "Romance", 109),
    ("Quiet Frequencies", 2024, "Science Fiction, Thriller", 115),
    ("The Orchid Protocol", 2015, "Action, Thriller", 122),
    ("Kites Over Lisbon", 2019, "Comedy, Romance", 94),
    ("Tin Moon", 2014, "Animation, Family", 88),
    ("Velvet Static", 2022, "Music, Drama", 107),
    ("Wolves of Wintermere", 2020, "Fantasy, Adventure", 141),
    ("Small Hours", 2018, "Drama", 99),
    ("A Map of Tides", 2021, "Documentary", 92),
    ("Hollow Crown Road", 2013, "Crime", 116),
    ("The Saffron Detective", 2023, "Mystery, Comedy", 103),
    ("Signal Lost", 2016, "Horror", 91),
    ("Echoes in Amber", 2025, "Drama, History", 128),
    ("The Clockmaker's Daughter", 2012, "Fantasy", 112),
    ("Blue Hour Express", 2024, "Action", 106),
];

/// Movies or series of one playlist: (index in MOVIES / SERIES, language prefix, 4K copy,
/// category). The same title in two playlists (or twice in one) is a stack in "All playlists".
type VodLineup = &'static [(usize, &'static str, bool, &'static str)];

const NORTHWIND_MOVIES: VodLineup = &[
    (0, "EN - ", false, "EN | Movies"),
    (0, "FR - ", false, "FR | Films"),
    (1, "EN - ", false, "EN | Movies"),
    (2, "EN - ", false, "EN | Movies"),
    (3, "EN - ", false, "EN | Movies"),
    (4, "EN - ", false, "EN | Movies"),
    (5, "EN - ", false, "EN | Movies"),
    (6, "EN - ", false, "EN | Movies"),
    (7, "FR - ", false, "FR | Films"),
    (8, "EN - ", true, "EN | 4K"),
    (9, "EN - ", false, "EN | Movies"),
    (10, "EN - ", false, "EN | Movies"),
    (11, "EN - ", false, "EN | Kids"),
    (13, "EN - ", true, "EN | 4K"),
    (15, "EN - ", false, "EN | Movies"),
    (17, "EN - ", false, "EN | Movies"),
    (19, "EN - ", false, "EN | Movies"),
    (20, "EN - ", false, "EN | Kids"),
];
const AURORA_MOVIES: VodLineup = &[
    (0, "EN - ", true, "EN | 4K Movies"),
    (1, "DE - ", false, "DE | Filme"),
    (2, "EN - ", false, "EN | New Releases"),
    (3, "EN - ", true, "EN | 4K Movies"),
    (8, "EN - ", false, "EN | New Releases"),
    (9, "DE - ", false, "DE | Filme"),
    (12, "EN - ", false, "EN | New Releases"),
    (13, "EN - ", false, "EN | New Releases"),
    (14, "EN - ", false, "EN | New Releases"),
    (16, "EN - ", false, "EN | New Releases"),
    (18, "EN - ", false, "EN | New Releases"),
    (19, "EN - ", true, "EN | 4K Movies"),
    (21, "EN - ", false, "EN | New Releases"),
];

/// (title, year, seasons, episodes per season, genre).
const SERIES: &[(&str, u16, u32, u32, &str)] = &[
    ("Harbour Lights", 2022, 2, 6, "Drama"),
    ("The Cartographers", 2021, 1, 8, "Adventure"),
    ("Static Bloom", 2023, 2, 5, "Science Fiction"),
    ("Fernwood Station", 2019, 3, 6, "Comedy"),
    ("Blue Hour", 2024, 1, 6, "Crime"),
    ("Ninth Floor", 2020, 2, 8, "Thriller"),
    ("Kitchen Confidential Club", 2018, 1, 10, "Reality"),
    ("Moss & Stone", 2025, 1, 6, "Fantasy"),
];
const NORTHWIND_SERIES: VodLineup = &[
    (0, "EN - ", false, "EN | Series"),
    (1, "EN - ", false, "EN | Series"),
    (2, "EN - ", false, "EN | Series"),
    (3, "EN - ", false, "EN | Comedy"),
    (6, "EN - ", false, "EN | Series"),
];
const AURORA_SERIES: VodLineup = &[
    (0, "EN - ", true, "EN | Series 4K"),
    (2, "DE - ", false, "DE | Serien"),
    (4, "EN - ", false, "EN | Series"),
    (5, "EN - ", false, "EN | Series"),
    (7, "EN - ", false, "EN | Series"),
];

const EPISODE_TITLES: &[&str] = &[
    "Pilot",
    "The Long Way Round",
    "Low Tide",
    "Lanterns",
    "A Quiet Arrangement",
    "North of Nowhere",
    "Old Maps",
    "Crosswinds",
    "The Visitor",
    "Paper Walls",
];

const FIRST_NAMES: &[&str] =
    &["Ada", "Marek", "Ines", "Tobias", "Noor", "Elliot", "Sana", "Ruben", "Mila", "Jonas", "Priya", "Callum", "Lea", "Oskar"];
const LAST_NAMES: &[&str] =
    &["Hollis", "Varga", "Okafor", "Lindqvist", "Moreau", "Castellan", "Brandt", "Achterberg", "Quill", "Sato", "Ferreira", "Dunmore"];

fn programmes(g: Genre) -> &'static [&'static str] {
    match g {
        Genre::News => &[
            "Morning Briefing",
            "World Desk",
            "Market Watch",
            "The Midday Report",
            "Newsroom Live",
            "The Evening Report",
            "Late Edition",
            "Week in Review",
        ],
        Genre::Weather => &["Weather Outlook", "Coast & Country Forecast", "Storm Watch", "Weather Outlook", "Travel Weather"],
        Genre::Sports => &[
            "Matchday Live",
            "Goal Rush",
            "Inside the Paddock",
            "Courtside Tonight",
            "Sports Desk",
            "Classic Finals",
            "Training Ground",
            "Extra Time",
        ],
        Genre::Kids => &["Paper Boats", "The Clockwork Fox", "Captain Pebble", "Doodle Lab", "Star Garden", "Mapmakers Club", "Bedtime Tales"],
        Genre::Docs => &[
            "Planet in Motion",
            "Deep Ocean Diaries",
            "The Silk Roads",
            "Engineering Giants",
            "Wild Frontiers",
            "Secrets of the Glacier",
            "Cities After Dark",
        ],
        Genre::Cinema => &[
            "The Lighthouse Keeper",
            "Copper Sky",
            "Small Hours",
            "Tin Moon",
            "Kites Over Lisbon",
            "Hollow Crown Road",
            "Signal Lost",
            "Midnight Orchard",
        ],
        Genre::Music => &["Night Shift Mix", "Top 20 Countdown", "Unplugged Sessions", "Vinyl Hour", "Festival Highlights", "Fresh Tracks"],
        Genre::Food => &["Fire & Salt", "The Weekend Baker", "Market to Table", "Street Food Atlas", "Kitchen Rescue"],
        Genre::Entertainment => {
            &["The Lakeside Show", "Quiz Night", "Home Rescue", "Late Talk", "Garden Stories", "Comedy Cellar", "The Long Weekend"]
        }
    }
}

fn blurb(g: Genre, title: &str) -> String {
    match g {
        Genre::News => format!("{title}: the latest headlines, analysis and weather."),
        Genre::Weather => "Forecasts for the coast, the hills and the commute.".into(),
        Genre::Sports => format!("{title} with highlights, interviews and analysis."),
        Genre::Kids => "Animated adventures for younger viewers.".into(),
        Genre::Docs => format!("{title}: a documentary series about our changing world."),
        Genre::Cinema => format!("Feature film: {title}."),
        Genre::Music => "Non-stop music, old favourites and new releases.".into(),
        Genre::Food => "Cooks, kitchens and markets from around the world.".into(),
        Genre::Entertainment => format!("{title}, an entertainment programme."),
    }
}

// --------------------------------------------------------------------------------- helpers

/// SplitMix64: small, fixed, and the same on every machine.
struct Rng(u64);

impl Rng {
    fn new(salt: &str) -> Self {
        let mut h = SEED;
        for b in salt.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        Self(h)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

fn slug(s: &str) -> String {
    let folded = deunicode::deunicode(s).to_ascii_lowercase();
    let mut out = String::new();
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn hsl(h: u32, s: f32, l: f32) -> String {
    let h = (h % 360) as f32 / 60.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let v = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", v(r), v(g), v(b))
}

// --------------------------------------------------------------------------------- images

struct Painter {
    opts: resvg::usvg::Options<'static>,
}

impl Painter {
    /// Fonts: egui's bundled ones (the same everywhere), never the system's.
    fn new() -> Result<Self> {
        let defs = eframe::egui::FontDefinitions::default();
        let mut db = resvg::usvg::fontdb::Database::new();
        for name in ["Ubuntu-Light", "Hack"] {
            let data = defs.font_data.get(name).ok_or_else(|| anyhow!("egui has no {name} font"))?;
            db.load_font_data(data.font.to_vec());
        }
        let family =
            |mono: bool| db.faces().find(|f| f.monospaced == mono).and_then(|f| f.families.first().map(|(n, _)| n.clone())).unwrap_or_default();
        let (sans, mono) = (family(false), family(true));
        db.set_sans_serif_family(sans);
        db.set_monospace_family(mono);
        let opts = resvg::usvg::Options { fontdb: Arc::new(db), ..Default::default() };
        Ok(Self { opts })
    }

    fn png(&self, svg: &str) -> Result<Vec<u8>> {
        let tree = resvg::usvg::Tree::from_str(svg, &self.opts).context("svg")?;
        let size = tree.size().to_int_size();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height()).context("pixmap")?;
        resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
        // opaque art: premultiplied = straight
        let img = image::RgbaImage::from_raw(size.width(), size.height(), pixmap.take()).context("image")?;
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)?;
        Ok(out.into_inner())
    }
}

/// A channel logo: the brand's initials on a two-tone tile.
fn logo_svg(name: &str, hue: u32) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.chars().next().is_some_and(char::is_alphanumeric)).collect();
    let initials: String = match words.as_slice() {
        [one] => one.chars().take(2).collect(),
        [a, b, ..] if b.chars().all(|c| c.is_ascii_digit()) => format!("{}{}", a.chars().next().unwrap_or('?'), b),
        [a, b, ..] => format!("{}{}", a.chars().next().unwrap_or('?'), b.chars().next().unwrap_or('?')),
        [] => "?".into(),
    };
    let (c1, c2) = (hsl(hue, 0.62, 0.46), hsl(hue + 35, 0.70, 0.30));
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{c1}"/><stop offset="1" stop-color="{c2}"/></linearGradient></defs>
<rect width="256" height="256" rx="44" fill="url(#g)"/>
<circle cx="214" cy="42" r="70" fill="#ffffff" fill-opacity="0.10"/>
<text x="128" y="160" text-anchor="middle" font-family="sans-serif" font-size="{size}" fill="#ffffff" stroke="#ffffff" stroke-width="5" letter-spacing="2">{text}</text>
</svg>"##,
        size = if initials.chars().count() > 2 { 84 } else { 104 },
        text = xml(&initials.to_uppercase()),
    )
}

/// Title split into lines of at most `width` characters.
fn wrap(title: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for w in title.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + w.chars().count() <= width => {
                l.push(' ');
                l.push_str(w);
            }
            _ => lines.push(w.to_owned()),
        }
    }
    lines
}

/// A landscape of layered hills under a big sun, in two hues: `w`×`h`.
fn scene(rng: &mut Rng, w: u32, h: u32, hue: u32) -> String {
    let mut s = String::new();
    let (sky1, sky2) = (hsl(hue, 0.55, 0.22), hsl(hue + 40, 0.65, 0.55));
    let _ = write!(
        s,
        r##"<defs><linearGradient id="sky" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{sky1}"/><stop offset="1" stop-color="{sky2}"/></linearGradient></defs>
<rect width="{w}" height="{h}" fill="url(#sky)"/>"##
    );
    let (sx, sy, r) = (w as u64 * (25 + rng.below(50)) / 100, h as u64 * (20 + rng.below(20)) / 100, (w.min(h) as u64) * (12 + rng.below(10)) / 100);
    let _ = write!(s, r##"<circle cx="{sx}" cy="{sy}" r="{r}" fill="{}" fill-opacity="0.85"/>"##, hsl(hue + 60, 0.9, 0.78));
    for layer in 0..4u32 {
        let base = h as f32 * (0.50 + 0.12 * layer as f32);
        let mut d = format!("M0 {h} L0 {base:.0}");
        let steps = 6;
        for i in 1..=steps {
            let x = w as f32 * i as f32 / steps as f32;
            let y = base - (rng.below(90) as f32 / 100.0) * h as f32 * 0.12;
            let _ = write!(d, " L{x:.0} {y:.0}");
        }
        let _ = write!(d, " L{w} {h} Z");
        let _ = write!(s, r##"<path d="{d}" fill="{}"/>"##, hsl(hue + 200 + layer * 8, 0.35, 0.30 - 0.06 * layer as f32));
    }
    s
}

fn poster_svg(title: &str, year: u16, hue: u32, rng: &mut Rng) -> String {
    let (w, h) = (400u32, 600u32);
    let mut s = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"##);
    s.push_str(&scene(rng, w, h, hue));
    let _ = write!(s, r##"<rect y="{}" width="{w}" height="{}" fill="#000000" fill-opacity="0.35"/>"##, h - 240, 240);
    let lines = wrap(&title.to_uppercase(), 12);
    let size = if lines.len() > 2 { 34 } else { 40 };
    // the last line ends above the corner where cards put their badges
    let top = h as usize - 100 - lines.len() * (size + 6);
    for (i, l) in lines.iter().enumerate() {
        let _ = write!(
            s,
            r##"<text x="{}" y="{}" text-anchor="middle" font-family="sans-serif" font-size="{size}" fill="#ffffff" stroke="#ffffff" stroke-width="1.6" letter-spacing="3">{}</text>"##,
            w / 2,
            top + (i + 1) * (size + 6),
            xml(l)
        );
    }
    let _ = write!(
        s,
        r##"<text x="{}" y="{}" text-anchor="middle" font-family="monospace" font-size="18" fill="#ffffff" fill-opacity="0.8" letter-spacing="6">{year}</text></svg>"##,
        w / 2,
        h - 26
    );
    s
}

fn backdrop_svg(hue: u32, rng: &mut Rng) -> String {
    let (w, h) = (1280u32, 720u32);
    format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">{}</svg>"##, scene(rng, w, h, hue))
}

fn title_hue(title: &str) -> u32 {
    (Rng::new(title).below(360)) as u32
}

// --------------------------------------------------------------------------------- playlists

struct Playlist {
    name: &'static str,
    file: &'static str,
    color: SourceColor,
    icon: &'static str,
    host: &'static str,
    live: Lineup,
    movies: VodLineup,
    series: VodLineup,
}

fn playlists() -> [Playlist; 3] {
    [
        Playlist {
            name: "Northwind",
            file: "northwind.m3u",
            color: SourceColor::Blue,
            icon: glyphs::SOURCE_XTREAM,
            host: "northwind.tuner-demo.invalid",
            live: NORTHWIND_LIVE,
            movies: NORTHWIND_MOVIES,
            series: NORTHWIND_SERIES,
        },
        Playlist {
            name: "Aurora",
            file: "aurora.m3u",
            color: SourceColor::Magenta,
            icon: glyphs::LIVE_TV,
            host: "aurora.tuner-demo.invalid",
            live: AURORA_LIVE,
            movies: AURORA_MOVIES,
            series: AURORA_SERIES,
        },
        Playlist {
            name: "Harbor Free",
            file: "harbor-free.m3u",
            color: SourceColor::Green,
            icon: glyphs::SOURCE_URL,
            host: "harbor.tuner-demo.invalid",
            live: HARBOR_LIVE,
            movies: &[],
            series: &[],
        },
    ]
}

fn m3u(p: &Playlist, media: &str) -> String {
    let mut s = String::from("#EXTM3U\n");
    for (i, (group, name, suffix)) in p.live.iter().enumerate() {
        let id = slug(name);
        let _ = writeln!(
            s,
            r#"#EXTINF:-1 tvg-id="{id}.demo" tvg-chno="{}" tvg-logo="{IMG}/logo/{id}.png" group-title="{group}",{name}{suffix}"#,
            101 + i
        );
        s.push_str(media);
        s.push('\n');
    }
    let uhd = |on: bool| if on { " [4K]" } else { "" };
    for (n, (i, prefix, four_k, group)) in p.movies.iter().enumerate() {
        let (title, year, ..) = MOVIES[*i];
        let q = uhd(*four_k);
        let _ = writeln!(s, r#"#EXTINF:-1 tvg-logo="{IMG}/poster/{}.png" group-title="{group}",{prefix}{title} ({year}){q}"#, slug(title));
        let _ = writeln!(s, "http://{}/movie/viewer/k7q2/{}.mkv", p.host, 5000 + n);
    }
    let mut ep_id = 9000;
    for (i, prefix, four_k, group) in p.series {
        let (title, year, seasons, eps, _) = SERIES[*i];
        let q = uhd(*four_k);
        for season in 1..=seasons {
            for e in 1..=eps {
                let ep_title = EPISODE_TITLES[((e - 1 + season * 3) as usize) % EPISODE_TITLES.len()];
                let _ = writeln!(
                    s,
                    r#"#EXTINF:-1 tvg-logo="{IMG}/poster/{}.png" group-title="{group}",{prefix}{title} ({year}){q} S{season:02} E{e:02} {ep_title}"#,
                    slug(title)
                );
                let _ = writeln!(s, "http://{}/series/viewer/k7q2/{ep_id}.mkv", p.host);
                ep_id += 1;
            }
        }
    }
    s
}

/// One programme of the demo guide.
struct Prog {
    channel: String,
    start: Timestamp,
    stop: Timestamp,
    title: String,
}

/// A guide from 06:00 yesterday to 06:00 the day after tomorrow (local), for every brand.
fn guide(day_start: Timestamp) -> (String, Vec<Prog>) {
    let mut progs = Vec::new();
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"tuner demo_profile\">\n");
    for (name, ..) in BRANDS {
        let _ = writeln!(s, "  <channel id=\"{}.demo\"><display-name>{}</display-name></channel>", slug(name), xml(name));
    }
    let fmt = |t: Timestamp| jiff::Timestamp::from_second(t).map(|t| t.strftime("%Y%m%d%H%M%S +0000").to_string()).unwrap_or_default();
    for (name, genre, _) in BRANDS {
        let mut rng = Rng::new(name);
        let titles = programmes(*genre);
        let mut t = day_start - 18 * 3600;
        let end = day_start + 2 * 86_400 + 6 * 3600;
        while t < end {
            let len = match genre {
                Genre::Cinema => 90 + 15 * rng.below(3) as i64,
                Genre::News | Genre::Weather => 30 * (1 + rng.below(2) as i64),
                _ => 30 * (1 + rng.below(3) as i64),
            } * 60;
            let title = rng.pick(titles).to_string();
            let _ = writeln!(
                s,
                "  <programme start=\"{}\" stop=\"{}\" channel=\"{}.demo\"><title>{}</title><desc>{}</desc></programme>",
                fmt(t),
                fmt(t + len),
                slug(name),
                xml(&title),
                xml(&blurb(*genre, &title))
            );
            progs.push(Prog { channel: slug(name), start: t, stop: t + len, title });
            t += len;
        }
    }
    s.push_str("</tv>\n");
    (s, progs)
}

// --------------------------------------------------------------------------------- build

/// What `build` made (printed by the example, checked by the test).
#[derive(Default)]
pub struct Summary {
    pub sources: Vec<SourceId>,
    /// The library the app builds from the profile.
    pub library: Arc<tuner::store::library::Library>,
    pub channels: usize,
    pub movies: usize,
    pub series: usize,
    pub programmes: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "demo profile: {} playlists, {} channels, {} movies, {} series, {} programmes",
            self.sources.len(),
            self.channels,
            self.movies,
            self.series,
            self.programmes
        )
    }
}

/// Build the demo profile in `dir` (must be empty or missing). Live channels play `media`;
/// `silent`: mpv gets no audio output. Also writes `demo/refs.env` (shell variables naming
/// demo items for `scripts/screenshots.sh`).
pub fn build(dir: &Path, media: Option<&Path>, silent: bool) -> Result<Summary> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} is not empty", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let dir = std::fs::canonicalize(dir)?;
    let paths = AppPaths::rooted(&dir);
    paths.ensure()?;
    let fixtures = dir.join("demo");
    std::fs::create_dir_all(&fixtures)?;
    let media = match media {
        Some(m) => std::fs::canonicalize(m).unwrap_or_else(|_| m.to_path_buf()),
        None => fixtures.join("missing-demo-video.mp4"),
    };
    let media_url = url::Url::from_file_path(&media).map_err(|()| anyhow!("media path must be absolute"))?.to_string();

    // config: Home first, live previews loop the clip, nothing refreshes on its own
    let mut cfg = Config::default();
    cfg.general.start_screen = StartScreen::Home;
    cfg.general.epg_keep_past_days = 1;
    cfg.player.mpv_options.insert("loop-file".into(), "inf".into());
    if silent {
        cfg.player.mpv_options.insert("ao".into(), "null".into());
    }
    cfg.integration.mpris = false;
    cfg.save(&paths.config_file())?;

    // images
    let painter = Painter::new()?;
    let cache = paths.image_cache();
    let put = |url: &str, png: Vec<u8>| -> Result<()> {
        let p = tuner::images::cache_path(&cache, url);
        std::fs::create_dir_all(p.parent().context("cache dir")?)?;
        std::fs::write(&p, png)?;
        Ok(())
    };
    for (name, _, hue) in BRANDS {
        put(&format!("{IMG}/logo/{}.png", slug(name)), painter.png(&logo_svg(name, *hue))?)?;
    }
    for (title, year, ..) in MOVIES.iter().map(|m| (m.0, m.1)).chain(SERIES.iter().map(|s| (s.0, s.1))) {
        let mut rng = Rng::new(title);
        let hue = title_hue(title);
        put(&format!("{IMG}/poster/{}.png", slug(title)), painter.png(&poster_svg(title, year, hue, &mut rng))?)?;
        put(&format!("{IMG}/backdrop/{}.png", slug(title)), painter.png(&backdrop_svg(hue, &mut rng))?)?;
    }

    // playlists + guide files
    let lists = playlists();
    for p in &lists {
        std::fs::write(fixtures.join(p.file), m3u(p, &media_url))?;
    }
    let day_start = jiff::Zoned::now().start_of_day().map(|z| z.timestamp().as_second()).unwrap_or_else(|_| tuner::model::now() / 86_400 * 86_400);
    let (xmltv, progs) = guide(day_start);
    let guide_path = fixtures.join("guide.xml");
    std::fs::write(&guide_path, xmltv)?;

    // import through the app's jobs
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (bus, events) = EventBus::new(None);
    let store = Store::open(&paths.db_file(), Some(bus.clone()))?;
    let http = HttpClient::new(&tuner::services::http_options(&cfg))?;
    let shared = Arc::new(RwLock::new(Arc::new(cfg.clone())));
    let jobs = JobManager::new(JobContext::new(store.clone(), http, bus, paths.clone(), shared), rt.handle().clone());
    let mut summary = Summary::default();
    for p in &lists {
        let draft = SourceDraft {
            name: p.name.into(),
            kind: SourceKind::M3uFile,
            icon: p.icon.into(),
            color: p.color,
            url: None,
            path: Some(fixtures.join(p.file)),
            xtream: None,
            settings: SourceSettings { keep_m3u: true, use_provider_epg: Some(false), ..Default::default() },
        };
        let id = store.writer().blocking(move |c| tuner::store::sources::insert_source(c, &draft, tuner::model::now()))?;
        run(&jobs, &events, JobKind::RefreshSource { source: id, force: true })?;
        summary.sources.push(id);
    }
    let gp = guide_path.clone();
    let epg = store.writer().blocking(move |c| tuner::store::sources::insert_epg_source(c, "Demo Guide", &EpgSourceKind::File { path: gp }, 0))?;
    run(&jobs, &events, JobKind::RefreshEpg { epg_source: epg, force: true })?;
    settle(&jobs, &events)?;

    // details the M3U playlists can't carry, then the user's own data
    let now = tuner::model::now();
    let progs = Arc::new(progs);
    let p2 = progs.clone();
    store.writer().blocking(move |c| {
        details(c, now)?;
        user_data(c, now, &p2)?;
        Ok(())
    })?;
    if let JobOutcome::Library(lib) = run(&jobs, &events, JobKind::RebuildLibrary)? {
        summary.library = lib;
    }

    let c = store.reader()?;
    std::fs::write(fixtures.join("refs.env"), refs(&c)?)?;
    let count = |t: &str| -> Result<usize> { Ok(c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0))? as usize) };
    summary.channels = count("channel")?;
    summary.movies = count("movie")?;
    summary.series = count("series")?;
    summary.programmes = progs.len();
    drop(c);
    jobs.cancel_all();
    rt.shutdown_timeout(Duration::from_secs(2));
    if !store.close(Duration::from_secs(5)) {
        bail!("the database writer did not finish");
    }
    Ok(summary)
}

/// `NAME=value` lines for the screenshot script: playlist ids and `--play` references.
fn refs(c: &Connection) -> Result<String> {
    let mut out = String::new();
    for (var, name) in [("NORTHWIND", "Northwind"), ("AURORA", "Aurora"), ("HARBOR", "Harbor Free")] {
        let id: i64 = c.query_row("SELECT id FROM source WHERE name = ?1", [name], |r| r.get(0))?;
        let _ = writeln!(out, "{var}_ID={id}");
    }
    for (var, source, name) in
        [("PLAY_DOCS", "Northwind", "Lumen Docs"), ("PLAY_SPORTS", "Aurora", "Goalpost TV"), ("PLAY_NEWS", "Northwind", "Meridian News")]
    {
        let (id, key): (i64, String) = c.query_row(
            "SELECT c.source_id, c.key FROM channel c JOIN source s ON s.id = c.source_id WHERE s.name = ?1 AND c.name LIKE ?2 || '%' ORDER BY c.id",
            params![source, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let _ = writeln!(out, "{var}='{id}:{key}'");
    }
    Ok(out)
}

/// Submit `kind` and wait for it (and its follow-ups) to finish.
fn run(jobs: &JobManager, events: &Receiver<AppEvent>, kind: JobKind) -> Result<JobOutcome> {
    let what = format!("{kind:?}");
    let id = jobs.submit(kind);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow!("{what}: timed out"))?;
        match events.recv_timeout(left) {
            Ok(AppEvent::Job(JobEvent::Finished { id: done, outcome, .. })) if done == id => {
                let outcome = outcome.map_err(|e| anyhow!("{what}: {e}"))?;
                settle(jobs, events)?;
                return Ok(outcome);
            }
            Ok(_) => {}
            Err(e) => bail!("{what}: {e}"),
        }
    }
}

/// Wait until no job runs (imports submit follow-ups: matching, library builds).
fn settle(jobs: &JobManager, events: &Receiver<AppEvent>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut quiet = None;
    while Instant::now() < deadline {
        while let Ok(ev) = events.try_recv() {
            if let AppEvent::Job(JobEvent::Finished { kind, outcome: Err(e), .. }) = ev {
                bail!("{kind:?}: {e}");
            }
        }
        if jobs.active().is_empty() {
            let since = *quiet.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_millis(300) {
                return Ok(());
            }
        } else {
            quiet = None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("jobs never settled: {:?}", jobs.active())
}

/// Plots, cast and backdrops for every movie; plots for series.
fn details(c: &mut Connection, now: Timestamp) -> tuner::error::Result<()> {
    let movies: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT id, name FROM movie ORDER BY id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (id, name) in movies {
        let Some(&(title, year, genre, minutes)) = MOVIES.iter().find(|m| name.contains(m.0)) else { continue };
        let mut rng = Rng::new(title);
        let person = |rng: &mut Rng| format!("{} {}", rng.pick(FIRST_NAMES), rng.pick(LAST_NAMES));
        let cast = (0..4).map(|_| person(&mut rng)).collect::<Vec<_>>().join(", ");
        let d = MovieDetails {
            plot: Some(movie_plot(title, genre)),
            genre: Some(genre.into()),
            cast: Some(cast),
            director: Some(person(&mut rng)),
            duration_s: Some(minutes * 60),
            release_date: Some(format!("{year}-{:02}-{:02}", 1 + rng.below(12), 1 + rng.below(28))),
            rating: Some(5.8 + rng.below(32) as f32 / 10.0),
            backdrop: Some(format!("{IMG}/backdrop/{}.png", slug(title))),
            ..Default::default()
        };
        tuner::store::import::store_movie_details(c, MovieId(id), &d, now)?;
    }
    for (title, _, _, _, genre) in SERIES {
        c.execute(
            "UPDATE series SET plot = ?1, genre = ?2, backdrop = ?3 WHERE name LIKE '%' || ?4 || '%'",
            params![series_plot(title), genre, format!("{IMG}/backdrop/{}.png", slug(title)), title],
        )?;
    }
    Ok(())
}

fn movie_plot(title: &str, genre: &str) -> String {
    let lead = match genre.split(',').next().unwrap_or("") {
        "Drama" | "Music" => "A family secret surfaces one winter and changes three lives for good.",
        "Thriller" | "Action" => "With forty-eight hours on the clock, an unlikely team chases a signal across the city.",
        "Science Fiction" => "A small research crew picks up a message that should not exist.",
        "Crime" => "A retired inspector takes one last case that leads back to her own past.",
        "Adventure" | "Fantasy" => "An apprentice sets out across the mountains with an unfinished map.",
        "Comedy" | "Road Movie" => "Two estranged siblings share one car, one week and far too many detours.",
        "Romance" => "A chef and a ferry captain keep missing each other by one crossing.",
        "Animation" => "A tin robot and a moth set out to fix the moon.",
        "Documentary" => "Five coastal towns, one year of tides, and the people who read them.",
        "Horror" => "The night shift at a remote relay station hears a voice on a dead channel.",
        "Mystery" => "A small-town detective follows a trail of saffron and forged postcards.",
        "Western" => "A drifter and a telegraph operator stand between a town and a copper baron.",
        _ => "An unforgettable story, told with warmth and wit.",
    };
    format!("{lead} ({title} is invented demo content.)")
}

fn series_plot(title: &str) -> String {
    format!("Season after season, the people of {title} keep each other's secrets — until they don't. (Invented demo content.)")
}

/// Favourites, recently watched channels, Continue Watching and one reminder.
fn user_data(c: &mut Connection, now: Timestamp, progs: &[Prog]) -> tuner::error::Result<()> {
    use tuner::store::user;
    let one = |c: &Connection, sql: &str, source: i64, like: &str| -> tuner::error::Result<Option<ItemRef>> {
        let mut st = c.prepare(sql)?;
        let mut rows = st.query(params![source, format!("%{like}%")])?;
        Ok(match rows.next()? {
            Some(r) => Some(ItemRef { source: SourceId(r.get(0)?), kind: ItemKind::Channel, key: ItemKey(r.get(1)?) }),
            None => None,
        })
    };
    let source_of =
        |c: &Connection, name: &str| -> tuner::error::Result<i64> { Ok(c.query_row("SELECT id FROM source WHERE name = ?1", [name], |r| r.get(0))?) };
    let (north, aurora) = (source_of(c, "Northwind")?, source_of(c, "Aurora")?);
    let channel = |c: &Connection, src: i64, name: &str| {
        one(c, "SELECT source_id, key FROM channel WHERE source_id = ?1 AND name LIKE ?2 ORDER BY id", src, name)
    };
    let movie = |c: &Connection, src: i64, name: &str| {
        one(c, "SELECT source_id, key FROM movie WHERE source_id = ?1 AND name LIKE ?2 ORDER BY id", src, name)
            .map(|r| r.map(|i| ItemRef { kind: ItemKind::Movie, ..i }))
    };
    let series = |c: &Connection, src: i64, name: &str| {
        one(c, "SELECT source_id, key FROM series WHERE source_id = ?1 AND name LIKE ?2 ORDER BY id", src, name)
            .map(|r| r.map(|i| ItemRef { kind: ItemKind::Series, ..i }))
    };
    let need = |r: Option<ItemRef>, what: &str| r.ok_or_else(|| tuner::error::Error::Other(format!("demo item {what} missing")));

    // favourites
    let list = user::default_fav_list(c)?;
    let favs = [
        need(channel(c, north, "Meridian News")?, "Meridian News")?,
        need(channel(c, north, "Bluefield Sports")?, "Bluefield Sports")?,
        need(channel(c, north, "Lumen Docs")?, "Lumen Docs")?,
        need(channel(c, aurora, "Northstar Cinema")?, "Northstar Cinema")?,
        need(channel(c, north, "Pinecone Kids")?, "Pinecone Kids")?,
    ];
    for (i, f) in favs.iter().enumerate() {
        user::set_favourite(c, list, f, true, now - 86_400 + i as i64 * 60)?;
    }
    // titles go in a list of their own: a rail mixing channel tiles and posters leaves a gap
    // under the tiles on Home
    let night = user::create_fav_list(c, "Movie night", tuner::ui::widgets::glyphs::FAV_LIST)?;
    let titles = [
        need(movie(c, north, "Lighthouse Keeper")?, "The Lighthouse Keeper")?,
        need(movie(c, aurora, "Quiet Frequencies")?, "Quiet Frequencies")?,
        need(series(c, north, "Harbour Lights")?, "Harbour Lights")?,
        need(movie(c, aurora, "Blue Hour Express")?, "Blue Hour Express")?,
        need(movie(c, north, "Copper Sky")?, "Copper Sky")?,
    ];
    for (i, f) in titles.iter().enumerate() {
        user::set_favourite(c, night, f, true, now - 80_000 + i as i64 * 60)?;
    }

    // recently watched channels (oldest first)
    for (i, (src, name)) in [(north, "Courtside"), (aurora, "Atlas World"), (north, "Polar Nature"), (north, "Meridian News")].iter().enumerate() {
        let item = need(channel(c, *src, name)?, name)?;
        let title: String =
            c.query_row("SELECT name FROM channel WHERE source_id = ?1 AND key = ?2", params![item.source, item.key.0], |r| r.get(0))?;
        user::push_history(c, &HistoryEntry { item, at: now - 7200 + i as i64 * 900, title })?;
    }

    // Continue Watching: two movies and an episode
    for (src, name, pos, dur, ago) in [(north, "Paper Satellites", 2830.0, 7560.0, 3600), (aurora, "Small Hours", 4100.0, 5940.0, 26 * 3600)] {
        let item = need(movie(c, src, name)?, name)?;
        user::save_progress(
            c,
            &WatchProgress {
                item,
                position_s: pos,
                duration_s: Some(dur),
                updated_at: now - ago,
                finished: false,
                title: name.into(),
                fingerprint: Default::default(),
            },
        )?;
    }
    let ep: Option<(i64, String, String)> = c
        .query_row(
            "SELECT e.source_id, e.key, s.name FROM episode e JOIN series s ON s.id = e.series_id
             WHERE s.source_id = ?1 AND s.name LIKE '%Fernwood Station%' AND e.season = 2 AND e.episode = 3",
            [north],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((src, key, show)) = ep {
        let item = ItemRef { source: SourceId(src), kind: ItemKind::Episode, key: ItemKey(key) };
        let title = format!("{show} · S02E03");
        user::save_progress(
            c,
            &WatchProgress {
                item,
                position_s: 812.0,
                duration_s: Some(1560.0),
                updated_at: now - 5400,
                finished: false,
                title,
                fingerprint: Default::default(),
            },
        )?;
    }

    // a reminder for the next sports programme starting in an hour or more
    let ch = need(channel(c, north, "Bluefield Sports")?, "Bluefield Sports")?;
    if let Some(p) = progs.iter().filter(|p| p.channel == "bluefield-sports" && p.start >= now + 3600).min_by_key(|p| p.start) {
        let channel_name: String =
            c.query_row("SELECT name FROM channel WHERE source_id = ?1 AND key = ?2", params![ch.source, ch.key.0], |r| r.get(0))?;
        user::add_reminder(
            c,
            &Reminder {
                id: ReminderId(0),
                channel: ch,
                channel_name,
                programme_title: p.title.clone(),
                start: p.start,
                stop: p.stop,
                lead_s: 300,
                state: ReminderState::Pending,
            },
        )?;
    }
    Ok(())
}
