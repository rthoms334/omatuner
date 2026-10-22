//! The demo profile for the README's demo video and manual checks: three fictional playlists,
//! a few days of guide, public-domain films and series, and some favourites, history, progress,
//! reminders, Up next, watchlists, downloads, Following rules and channels for Guide fixes.
//!
//! `cargo run --example demo_profile -- <dir> [--media <video file>] [--media2 <video file>]
//! [--art <dir>] [--silent]`, then `tuner --profile-dir <dir>`. `--silent` gives mpv no audio
//! output (the video).
//!
//! The playlists, channels, programmes and logos (monograms) are invented here, so nothing shows
//! a real provider or channel. The films and series are real US public-domain titles (`MOVIES`,
//! `SERIES`); `--art` is a directory of their posters and backdrops (`poster/<slug>.jpg`,
//! `backdrop/<slug>.jpg`: public-domain files from Wikimedia Commons, listed in
//! `docs/demo-media.tsv`, which `scripts/demo-video.sh` fetches and cuts). Without it they get
//! generated posters (gradients + title). The content is fixed (seeded); only the guide's times
//! follow today's date. Live channels play `--media` (looped; the script passes public-domain
//! NASA footage), the channels in `SECOND_CLIP` play `--media2` instead when given (so
//! multi-view's two pictures differ), movies and episodes point at `*.tuner-demo.invalid` and
//! never play (downloaded ones play `--media`).
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
use tuner::config::{Config, LiveOpensOn, StartScreen};
use tuner::follow::{ChannelScope, FollowKind, FollowMode, FollowReason, FollowRule};
use tuner::jobs::{JobContext, JobEvent, JobKind, JobManager, JobOutcome};
use tuner::model::{
    AccountInfo, AccountStatus, EpgSourceKind, HistoryEntry, ItemKey, ItemKind, ItemRef, MovieId, OpenIn, Reminder, ReminderId, ReminderMode,
    ReminderState, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings, Timestamp, WatchProgress,
};
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::queue::model::{AddedBy, Insert, RowState};
use tuner::sources::http::HttpClient;
use tuner::store::Store;
use tuner::store::import::MovieDetails;
use tuner::store::queue::NewRow;
use tuner::ui::widgets::glyphs;

const SEED: u64 = 0x7475_6e65_7264_656d; // "tunerdem"
const IMG: &str = "https://img.tuner-demo.invalid";

const USAGE: &str = "usage: demo_profile <dir> [--media <video file>] [--media2 <video file>] [--art <dir>] [--silent]";

#[allow(dead_code)] // the test includes this file and uses `build` only
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut media = None;
    let mut media2 = None;
    let mut art = None;
    let mut silent = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--media" => media = Some(PathBuf::from(args.next().context("--media needs a file")?)),
            "--media2" => media2 = Some(PathBuf::from(args.next().context("--media2 needs a file")?)),
            "--art" => art = Some(PathBuf::from(args.next().context("--art needs a directory")?)),
            "--silent" => silent = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            other => bail!("unexpected argument {other}"),
        }
    }
    let dir = dir.context(USAGE)?;
    let summary = build_with_art(&dir, media.as_deref(), media2.as_deref(), art.as_deref(), silent)?;
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

/// The brands whose channels play the second clip (`--media2`): multi-view pairs Polar Nature
/// (first clip) with Lumen Docs, and the pop-out's Summit Sports differs from the player's shot.
const SECOND_CLIP: &[&str] = &["Lumen Docs", "Orbit Science", "Summit Sports"];

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
    // "+1 without shift" in Guide fixes: the base's guide, an hour early
    ("UK | Entertainment", "Brightside", " +1"),
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
    // no guide lists these (Guide fixes: "No guide")
    ("Community", "Quarry Lane TV", ""),
    ("Community", "Seabright Local", " HD"),
];

/// (name, hue) of the channels no guide lists: a logo, no programmes.
const UNLISTED: &[(&str, u32)] = &[("Quarry Lane TV", 75), ("Seabright Local", 165)];

/// One film of the catalogue: real public-domain titles (US), with their year, length, genre,
/// director and leads; the plots are written for the demo. Their posters and backdrops are
/// public-domain files from Wikimedia Commons (`docs/demo-media.tsv`), fetched by
/// `scripts/demo-video.sh` and passed in with `--art`; without them each gets a generated poster.
struct Film {
    title: &'static str,
    year: u16,
    genre: &'static str,
    minutes: u32,
    director: &'static str,
    cast: &'static [&'static str],
    plot: &'static str,
}

const MOVIES: &[Film] = &[
    // 0
    Film {
        title: "Metropolis",
        year: 1927,
        genre: "Science Fiction, Drama",
        minutes: 153,
        director: "Fritz Lang",
        cast: &["Brigitte Helm", "Alfred Abel", "Gustav Fröhlich"],
        plot: "In a towering future city, the son of its ruler joins the oppressed underground workers, while an inventor builds a robot in a woman's likeness.",
    },
    // 1
    Film {
        title: "Night of the Living Dead",
        year: 1968,
        genre: "Horror",
        minutes: 96,
        director: "George A. Romero",
        cast: &["Duane Jones", "Judith O'Dea", "Karl Hardman"],
        plot: "Strangers barricade themselves inside a rural Pennsylvania farmhouse as the recently dead rise and close in, and tensions among the survivors boil over.",
    },
    // 2
    Film {
        title: "His Girl Friday",
        year: 1940,
        genre: "Comedy, Romance",
        minutes: 92,
        director: "Howard Hawks",
        cast: &["Cary Grant", "Rosalind Russell", "Ralph Bellamy"],
        plot: "A fast-talking newspaper editor schemes to stop his star reporter and ex-wife from remarrying by luring her onto one last big story.",
    },
    // 3
    Film {
        title: "The General",
        year: 1926,
        genre: "Comedy, Adventure",
        minutes: 79,
        director: "Buster Keaton, Clyde Bruckman",
        cast: &["Buster Keaton", "Marion Mack", "Glen Cavender"],
        plot: "A Confederate train engineer, turned away by the army, chases Union spies across enemy lines after they steal his locomotive with his sweetheart aboard.",
    },
    // 4
    Film {
        title: "D.O.A.",
        year: 1949,
        genre: "Film Noir, Thriller",
        minutes: 84,
        director: "Rudolph Maté",
        cast: &["Edmond O'Brien", "Pamela Britton", "Luther Adler"],
        plot: "Told he has swallowed a slow-acting poison, an accountant spends his remaining hours racing around San Francisco and Los Angeles to identify his own killer.",
    },
    // 5
    Film {
        title: "The Little Shop of Horrors",
        year: 1960,
        genre: "Comedy, Horror",
        minutes: 72,
        director: "Roger Corman",
        cast: &["Jonathan Haze", "Jackie Joseph", "Mel Welles"],
        plot: "A clumsy florist's clerk wins fame by breeding a strange new plant, then learns it craves human flesh and will keep demanding more.",
    },
    // 6
    Film {
        title: "The Cabinet of Dr. Caligari",
        year: 1920,
        genre: "Horror",
        minutes: 74,
        director: "Robert Wiene",
        cast: &["Werner Krauss", "Conrad Veidt", "Lil Dagover"],
        plot: "A young man recounts how a sinister showman and his sleepwalking servant arrived at a town fair, soon followed by a string of murders.",
    },
    // 7
    Film {
        title: "The Phantom of the Opera",
        year: 1925,
        genre: "Horror",
        minutes: 107,
        director: "Rupert Julian",
        cast: &["Lon Chaney", "Mary Philbin", "Norman Kerry"],
        plot: "A disfigured genius haunting the cellars beneath the Paris Opera schemes to make a young understudy its leading star, whatever the cost.",
    },
    // 8
    Film {
        title: "Sherlock Jr.",
        year: 1924,
        genre: "Comedy",
        minutes: 45,
        director: "Buster Keaton",
        cast: &["Buster Keaton", "Kathryn McGuire", "Joe Keaton"],
        plot: "A cinema projectionist who dreams of being a detective is falsely accused of theft, then falls asleep and walks into the film he is screening.",
    },
    // 9
    Film {
        title: "Steamboat Bill, Jr.",
        year: 1928,
        genre: "Comedy",
        minutes: 70,
        director: "Charles Reisner",
        cast: &["Buster Keaton", "Ernest Torrence", "Marion Byron"],
        plot: "A dapper college boy joins his gruff riverboat-captain father on the Mississippi, falls for a rival's daughter, and faces a devastating cyclone.",
    },
    // 10
    Film {
        title: "The Kid",
        year: 1921,
        genre: "Comedy, Drama",
        minutes: 68,
        director: "Charlie Chaplin",
        cast: &["Charlie Chaplin", "Jackie Coogan", "Edna Purviance"],
        plot: "Five years after finding a baby left in an alley, the Tramp and the streetwise boy he raised fight to stay together when officials intervene.",
    },
    // 11
    Film {
        title: "The Gold Rush",
        year: 1925,
        genre: "Comedy, Adventure",
        minutes: 95,
        director: "Charlie Chaplin",
        cast: &["Charlie Chaplin", "Georgia Hale", "Mack Swain"],
        plot: "A lone prospector braves the Klondike winter, shares a storm-battered cabin with a hulking miner, and falls for a dance-hall girl in a boomtown.",
    },
    // 12
    Film {
        title: "Safety Last!",
        year: 1923,
        genre: "Comedy",
        minutes: 73,
        director: "Fred C. Newmeyer, Sam Taylor",
        cast: &["Harold Lloyd", "Mildred Davis", "Bill Strother"],
        plot: "A small-town clerk desperate to impress his sweetheart in the city ends up climbing the outside of a twelve-storey department store.",
    },
    // 13
    Film {
        title: "The Freshman",
        year: 1925,
        genre: "Comedy",
        minutes: 76,
        director: "Fred C. Newmeyer, Sam Taylor",
        cast: &["Harold Lloyd", "Jobyna Ralston", "Brooks Benedict"],
        plot: "A college freshman desperate to be popular becomes a campus joke without realising it, then gets one chance to prove himself on the football field.",
    },
    // 14
    Film {
        title: "The Stranger",
        year: 1946,
        genre: "Film Noir, Thriller",
        minutes: 95,
        director: "Orson Welles",
        cast: &["Edward G. Robinson", "Loretta Young", "Orson Welles"],
        plot: "A war-crimes investigator tracks a fugitive Nazi official to a quiet Connecticut college town, where the man now teaches and has just married.",
    },
    // 15
    Film {
        title: "The Lodger",
        year: 1927,
        genre: "Thriller",
        minutes: 90,
        director: "Alfred Hitchcock",
        cast: &["Ivor Novello", "June Tripp", "Malcolm Keen"],
        plot: "While a serial killer stalks fair-haired women in fog-bound London, a landlady begins to suspect the secretive young man renting her upstairs room.",
    },
    // 16
    Film {
        title: "Sunrise",
        year: 1927,
        genre: "Drama, Romance",
        minutes: 95,
        director: "F. W. Murnau",
        cast: &["George O'Brien", "Janet Gaynor", "Margaret Livingston"],
        plot: "Urged by a city seductress to drown his wife, a farmer rows her across the lake, and their unexpected day in the city changes everything.",
    },
    // 17
    Film {
        title: "Wings",
        year: 1927,
        genre: "War, Drama",
        minutes: 144,
        director: "William A. Wellman",
        cast: &["Clara Bow", "Charles \"Buddy\" Rogers", "Richard Arlen"],
        plot: "Two small-town rivals for the same girl train as fighter pilots in the First World War and forge a deep friendship in combat.",
    },
    // 18
    Film {
        title: "The Passion of Joan of Arc",
        year: 1928,
        genre: "Drama, History",
        minutes: 82,
        director: "Carl Theodor Dreyer",
        cast: &["Renée Jeanne Falconetti", "Eugène Silvain", "Antonin Artaud"],
        plot: "Captured and put on trial by English-aligned clergy, Joan of Arc endures relentless interrogation and pressure to recant in the final hours before her execution.",
    },
    // 19
    Film {
        title: "The Hunchback of Notre Dame",
        year: 1923,
        genre: "Drama, Romance",
        minutes: 102,
        director: "Wallace Worsley",
        cast: &["Lon Chaney", "Patsy Ruth Miller", "Norman Kerry"],
        plot: "In medieval Paris, the deformed bell-ringer of Notre Dame is drawn into a web of intrigue around a kind-hearted street dancer pursued by several men.",
    },
    // 20
    Film {
        title: "The Thief of Bagdad",
        year: 1924,
        genre: "Adventure, Fantasy",
        minutes: 140,
        director: "Raoul Walsh",
        cast: &["Douglas Fairbanks", "Julanne Johnston", "Anna May Wong"],
        plot: "A thief who sneaks into the palace falls for the Caliph's daughter and, posing as a prince, sets off on a quest for magical treasure.",
    },
    // 21
    Film {
        title: "Battleship Potemkin",
        year: 1925,
        genre: "Drama, History",
        minutes: 74,
        director: "Sergei Eisenstein",
        cast: &["Aleksandr Antonov", "Vladimir Barsky", "Grigori Aleksandrov"],
        plot: "Sailors on a Russian battleship rebel against their officers over rotten rations, and the people of Odessa rally to their cause before troops intervene.",
    },
    // 22
    Film {
        title: "Nosferatu",
        year: 1922,
        genre: "Horror",
        minutes: 94,
        director: "F. W. Murnau",
        cast: &["Max Schreck", "Gustav von Wangenheim", "Greta Schröder"],
        plot: "An estate agent travels to a remote castle to sell a house to a reclusive count, who follows him home and brings a plague with him.",
    },
];

/// Buster Keaton's three, in release order (Up next).
pub const UP_NEXT: &[&str] = &["Sherlock Jr.", "The General", "Steamboat Bill, Jr."];

/// Movies or series of one playlist: (index in MOVIES / SERIES, language prefix, 4K copy,
/// category). The same title in two playlists (or twice in one) is a stack in "All playlists".
type VodLineup = &'static [(usize, &'static str, bool, &'static str)];

// Silent films come with English intertitles, and the German, French and Russian ones also
// with their own (a second language copy to stack).
const NORTHWIND_MOVIES: VodLineup = &[
    (0, "EN - ", false, "EN | Movies"),
    (0, "DE - ", false, "DE | Filme"),
    (1, "EN - ", false, "EN | Movies"),
    (2, "EN - ", false, "EN | Movies"),
    (3, "EN - ", false, "EN | Movies"),
    (4, "EN - ", false, "EN | Movies"),
    (5, "EN - ", false, "EN | Movies"),
    (6, "EN - ", false, "EN | Movies"),
    (7, "EN - ", false, "EN | Movies"),
    (8, "EN - ", false, "EN | Movies"),
    (9, "EN - ", false, "EN | Movies"),
    (10, "EN - ", false, "EN | Kids"),
    (11, "EN - ", false, "EN | Movies"),
    (12, "EN - ", false, "EN | Movies"),
    (14, "EN - ", false, "EN | Movies"),
    (16, "EN - ", true, "EN | 4K"),
    (17, "EN - ", false, "EN | Movies"),
    (18, "FR - ", false, "FR | Films"),
    (21, "EN - ", false, "EN | Movies"),
    (22, "EN - ", false, "EN | Movies"),
    (22, "DE - ", false, "DE | Filme"),
];
const AURORA_MOVIES: VodLineup = &[
    (0, "EN - ", true, "EN | 4K Movies"),
    (1, "EN - ", false, "EN | Classics"),
    (2, "EN - ", true, "EN | 4K Movies"),
    (3, "EN - ", true, "EN | 4K Movies"),
    (6, "DE - ", false, "DE | Filme"),
    (7, "EN - ", false, "EN | Classics"),
    (11, "EN - ", true, "EN | 4K Movies"),
    (13, "EN - ", false, "EN | Classics"),
    (14, "EN - ", false, "EN | Classics"),
    (15, "EN - ", false, "EN | Classics"),
    (16, "EN - ", false, "EN | Classics"),
    (18, "EN - ", false, "EN | Classics"),
    (19, "EN - ", false, "EN | Classics"),
    (20, "EN - ", true, "EN | 4K Movies"),
    (22, "EN - ", true, "EN | 4K Movies"),
];

/// One series: real public-domain episodes (US; copyright not renewed), with their own titles
/// and numbers. The premise is written for the demo.
struct Show {
    title: &'static str,
    year: u16,
    genre: &'static str,
    minutes: u32,
    plot: &'static str,
    /// (season, episode, title)
    episodes: &'static [(u32, u32, &'static str)],
}

const SERIES: &[Show] = &[
    Show {
        title: "The Beverly Hillbillies",
        year: 1962,
        genre: "Comedy",
        minutes: 25,
        plot: "A widowed hill-country man's oil windfall moves his family to a Beverly Hills mansion, where their rustic habits puzzle a banker and his snobbish wife.",
        episodes: &[
            (1, 1, "The Clampetts Strike Oil"),
            (1, 2, "Getting Settled"),
            (1, 4, "The Clampetts Meet Mrs. Drysdale"),
            (1, 6, "Trick or Treat"),
            (1, 13, "Home for Christmas"),
            (1, 26, "Jed Cuts the Family Tree"),
            (2, 1, "Jed Gets the Misery"),
            (2, 3, "Granny's Garden"),
            (2, 5, "The Clampett Look"),
            (2, 10, "Turkey Day"),
            (2, 14, "Christmas at the Clampetts"),
        ],
    },
    Show {
        title: "One Step Beyond",
        year: 1959,
        genre: "Drama, Mystery",
        minutes: 25,
        plot: "Host John Newland introduces dramatised accounts of premonitions, ghosts and other unexplained events, each presented as drawn from real reported cases.",
        episodes: &[
            (1, 1, "The Bride Possessed"),
            (1, 2, "Night of April 14th"),
            (1, 9, "The Dead Part of the House"),
            (1, 17, "The Haunted U-Boat"),
            (1, 19, "The Captain's Guests"),
            (2, 2, "Ordeal on Locust Street"),
            (2, 6, "The Inheritance"),
            (2, 8, "Message From Clara"),
            (2, 14, "Make Me Not a Witch"),
            (2, 26, "The Explorer"),
        ],
    },
    Show {
        title: "Petticoat Junction",
        year: 1963,
        genre: "Comedy",
        minutes: 25,
        plot: "A widow runs a hotel by a branch railway with her three daughters and lazy uncle, while a railroad boss schemes to close the line.",
        episodes: &[
            (1, 1, "Spur Line to Shady Rest"),
            (1, 2, "Quick, Hide the Railroad"),
            (1, 3, "The President Who Came to Dinner"),
            (1, 5, "The Courtship of Floyd Smoot"),
            (1, 9, "The Little Train Robbery"),
            (1, 11, "Uncle Joe's Replacement"),
            (1, 13, "A Night at the Hooterville Hilton"),
            (1, 14, "Cannonball Christmas"),
        ],
    },
];
const NORTHWIND_SERIES: VodLineup = &[(0, "EN - ", false, "EN | Series"), (1, "EN - ", false, "EN | Series"), (2, "EN - ", false, "EN | Comedy")];
const AURORA_SERIES: VodLineup = &[(0, "EN - ", false, "EN | Classic TV"), (1, "EN - ", false, "EN | Classic TV")];

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
        Genre::Cinema => &["His Girl Friday", "The Gold Rush", "Sunrise", "The Kid", "Wings", "D.O.A.", "The Stranger", "Safety Last!"],
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

/// `media`: (the first clip's URL, the second's) for live channels.
fn m3u(p: &Playlist, media: (&str, &str)) -> String {
    let mut s = String::from("#EXTM3U\n");
    for (i, (group, name, suffix)) in p.live.iter().enumerate() {
        let id = slug(name);
        let _ = writeln!(
            s,
            r#"#EXTINF:-1 tvg-id="{id}.demo" tvg-chno="{}" tvg-logo="{IMG}/logo/{id}.png" group-title="{group}",{name}{suffix}"#,
            101 + i
        );
        s.push_str(if SECOND_CLIP.contains(name) { media.1 } else { media.0 });
        s.push('\n');
    }
    let uhd = |on: bool| if on { " [4K]" } else { "" };
    for (n, (i, prefix, four_k, group)) in p.movies.iter().enumerate() {
        let Film { title, year, .. } = MOVIES[*i];
        let q = uhd(*four_k);
        let _ = writeln!(s, r#"#EXTINF:-1 tvg-logo="{IMG}/poster/{}.png" group-title="{group}",{prefix}{title} ({year}){q}"#, slug(title));
        let _ = writeln!(s, "http://{}/movie/viewer/k7q2/{}.mkv", p.host, 5000 + n);
    }
    let mut ep_id = 9000;
    for (i, prefix, four_k, group) in p.series {
        let Show { title, year, episodes, .. } = &SERIES[*i];
        let q = uhd(*four_k);
        for &(season, e, ep_title) in *episodes {
            {
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

/// Fixtures at fixed local times, for Following's team rule: (channel brand, days after today,
/// local start in minutes, title, minutes, live). The rest of the guide is generated around them.
const FIXTURES: &[(&str, i64, i64, &str, i64, bool)] = &[
    ("Bluefield Sports", 1, 15 * 60, "Northport Albion v Granite Rovers", 120, true),
    ("Goalpost TV", 1, 20 * 60, "Kestrel United v Harbour City", 120, true),
    ("Summit Sports 2", 1, 23 * 60, "Northport Albion v Granite Rovers", 120, false),
    ("Summit Sports", 2, 19 * 60 + 45, "Harbour City v Northport Albion", 120, true),
];

/// The match that is on whenever the profile is built (the demo video's agent asks for it):
/// (channel brand, title, minutes). It started on the last half hour but one; no Following rule
/// names its teams.
pub const LIVE_MATCH: (&str, &str, i64) = ("Summit Sports", "Kestrel United v Granite Rovers", 150);

/// A fixture at an absolute time: (channel brand, start, title, seconds, live).
type Fixture = (&'static str, Timestamp, &'static str, i64, bool);

/// [`FIXTURES`] for the day starting at `day_start`, and [`LIVE_MATCH`] around `now`.
fn fixtures_at(day_start: Timestamp, now: Timestamp) -> Vec<Fixture> {
    let mut out: Vec<Fixture> = FIXTURES
        .iter()
        .map(|&(brand, day, minute, title, minutes, live)| (brand, day_start + day * 86_400 + minute * 60, title, minutes * 60, live))
        .collect();
    let (brand, title, minutes) = LIVE_MATCH;
    out.push((brand, now.div_euclid(1800) * 1800 - 1800, title, minutes * 60, true));
    out
}

/// One programme of the demo guide.
struct Prog {
    channel: String,
    start: Timestamp,
    stop: Timestamp,
    title: String,
}

/// A guide from 06:00 yesterday to 06:00 three days ahead (local), for every brand, around
/// `fixtures`.
fn guide(day_start: Timestamp, fixtures: &[Fixture]) -> (String, Vec<Prog>) {
    let mut progs = Vec::new();
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv generator-info-name=\"tuner demo_profile\">\n");
    for (name, ..) in BRANDS {
        let _ = writeln!(s, "  <channel id=\"{}.demo\"><display-name>{}</display-name></channel>", slug(name), xml(name));
    }
    let fmt = |t: Timestamp| jiff::Timestamp::from_second(t).map(|t| t.strftime("%Y%m%d%H%M%S +0000").to_string()).unwrap_or_default();
    for (name, genre, _) in BRANDS {
        let mut rng = Rng::new(name);
        let titles = programmes(*genre);
        let fixtures: Vec<(Timestamp, &str, i64, bool)> =
            fixtures.iter().filter(|f| f.0 == *name).map(|&(_, start, title, len, live)| (start, title, len, live)).collect();
        let mut prev = String::new();
        let mut t = day_start - 18 * 3600;
        let end = day_start + 3 * 86_400 + 6 * 3600;
        while t < end {
            let (title, len, live) = match fixtures.iter().find(|f| f.0 == t) {
                Some(&(_, title, len, live)) => (title.to_owned(), len, live),
                None => {
                    let mut len = match genre {
                        Genre::Cinema => 90 + 15 * rng.below(3) as i64,
                        Genre::News | Genre::Weather => 30 * (1 + rng.below(2) as i64),
                        _ => 30 * (1 + rng.below(3) as i64),
                    } * 60;
                    // end where the next fixture starts
                    if let Some(f) = fixtures.iter().find(|f| f.0 > t && f.0 < t + len) {
                        len = f.0 - t;
                    }
                    (programme_title(&mut rng, *genre, titles, (t - day_start).rem_euclid(86_400) / 3600, &prev), len, false)
                }
            };
            prev.clone_from(&title);
            let _ = writeln!(
                s,
                "  <programme start=\"{}\" stop=\"{}\" channel=\"{}.demo\"><title>{}</title><desc>{}</desc>{}</programme>",
                fmt(t),
                fmt(t + len),
                slug(name),
                xml(&title),
                xml(&blurb(*genre, &title)),
                if live { "<live />" } else { "" }
            );
            progs.push(Prog { channel: slug(name), start: t, stop: t + len, title });
            t += len;
        }
    }
    s.push_str("</tv>\n");
    (s, progs)
}

/// A title for a programme starting at `hour` (local): news follows the clock, and no title
/// runs twice in a row.
fn programme_title(rng: &mut Rng, genre: Genre, titles: &[&str], hour: i64, prev: &str) -> String {
    let slot: &[&str] = match (genre, hour) {
        (Genre::News, 5..=9) => &["Morning Briefing"],
        (Genre::News, 12..=13) => &["The Midday Report"],
        (Genre::News, 18..=20) => &["The Evening Report"],
        (Genre::News, 22..=23 | 0..=4) => &["Late Edition", "Newsroom Live"],
        (Genre::News, 21) => &["Week in Review", "World Desk"],
        (Genre::News, _) => &["World Desk", "Market Watch", "Newsroom Live"],
        _ => titles,
    };
    let fresh: Vec<&str> = slot.iter().copied().filter(|t| *t != prev).collect();
    let fallback: Vec<&str> = ["World Desk", "Newsroom Live"].into_iter().filter(|t| *t != prev).collect();
    let pool = if !fresh.is_empty() {
        fresh
    } else if genre == Genre::News {
        fallback
    } else {
        slot.to_vec()
    };
    rng.pick(&pool).to_string()
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

/// Build the demo profile in `dir` (must be empty or missing). Live channels play `media`, those
/// in `SECOND_CLIP` play `media2` (`media` without it); `silent`: mpv gets no audio output. Also
/// writes `demo/refs.env` (shell variables naming demo items for `scripts/demo-video.sh`).
pub fn build(dir: &Path, media: Option<&Path>, media2: Option<&Path>, silent: bool) -> Result<Summary> {
    build_with_art(dir, media, media2, None, silent)
}

/// [`build`] with the films' and series' artwork from `art` (see the module docs).
pub fn build_with_art(dir: &Path, media: Option<&Path>, media2: Option<&Path>, art: Option<&Path>, silent: bool) -> Result<Summary> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} is not empty", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let dir = std::fs::canonicalize(dir)?;
    let paths = AppPaths::rooted(&dir);
    paths.ensure()?;
    let fixtures = dir.join("demo");
    std::fs::create_dir_all(&fixtures)?;
    let file_url = |m: Option<&Path>| -> Result<String> {
        let m = match m {
            Some(m) => std::fs::canonicalize(m).unwrap_or_else(|_| m.to_path_buf()),
            None => fixtures.join("missing-demo-video.mp4"),
        };
        Ok(url::Url::from_file_path(&m).map_err(|()| anyhow!("media path must be absolute"))?.to_string())
    };
    let media_url = file_url(media)?;
    let media2_url = if media2.is_some() { file_url(media2)? } else { media_url.clone() };

    // config: Home first, live previews loop the clip, nothing refreshes on its own
    let mut cfg = Config::default();
    cfg.general.start_screen = StartScreen::Home;
    // the Live TV shots show the curated favourites list, as before the All channels default
    cfg.interface.live_opens_on = LiveOpensOn::Favourites;
    cfg.general.epg_keep_past_days = 1;
    cfg.player.mpv_options.insert("loop-file".into(), "inf".into());
    if silent {
        cfg.player.mpv_options.insert("ao".into(), "null".into());
    }
    cfg.integration.mpris = false;
    // nothing looks at (or updates) the AI agents installed on this computer
    cfg.agents.offer_connect = false;
    cfg.agents.keep_in_sync = false;
    cfg.agents.checked_version = Some(env!("CARGO_PKG_VERSION").into());
    cfg.follow.news_flash.quiet_from_min = Some(22 * 60);
    cfg.follow.news_flash.quiet_to_min = Some(7 * 60);
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
    for (name, hue) in BRANDS.iter().map(|b| (b.0, b.2)).chain(UNLISTED.iter().copied()) {
        put(&format!("{IMG}/logo/{}.png", slug(name)), painter.png(&logo_svg(name, hue))?)?;
    }
    // the real artwork when `art` has it (`<art>/poster/<slug>.jpg`, `<art>/backdrop/<slug>.jpg`)
    let artwork = |kind: &str, title: &str| art.and_then(|a| std::fs::read(a.join(kind).join(format!("{}.jpg", slug(title)))).ok());
    for (title, year) in MOVIES.iter().map(|m| (m.title, m.year)).chain(SERIES.iter().map(|s| (s.title, s.year))) {
        let mut rng = Rng::new(title);
        let hue = title_hue(title);
        let poster = match artwork("poster", title) {
            Some(jpg) => jpg,
            None => painter.png(&poster_svg(title, year, hue, &mut rng))?,
        };
        put(&format!("{IMG}/poster/{}.png", slug(title)), poster)?;
        let backdrop = match artwork("backdrop", title) {
            Some(jpg) => jpg,
            None => painter.png(&backdrop_svg(hue, &mut rng))?,
        };
        put(&format!("{IMG}/backdrop/{}.png", slug(title)), backdrop)?;
    }

    // playlists + guide files
    let lists = playlists();
    for p in &lists {
        std::fs::write(fixtures.join(p.file), m3u(p, (&media_url, &media2_url)))?;
    }
    let day_start = jiff::Zoned::now().start_of_day().map(|z| z.timestamp().as_second()).unwrap_or_else(|_| tuner::model::now() / 86_400 * 86_400);
    let (xmltv, progs) = guide(day_start, &fixtures_at(day_start, tuner::model::now()));
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
    let follow_cfg = cfg.follow.clone();
    let dl_cfg = cfg.clone();
    let clip = media.map(|m| std::fs::canonicalize(m).unwrap_or_else(|_| m.to_path_buf()));
    store.writer().blocking(move |c| {
        details(c, now)?;
        user_data(c, now, &p2)?;
        follow_rules(c, now, &follow_cfg)?;
        downloads(c, now, &p2, &dl_cfg, clip.as_deref())?;
        Ok(())
    })?;
    run(&jobs, &events, JobKind::EvaluateFollows { reason: FollowReason::Rules })?;
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

/// `NAME=value` lines for the video script: playlist ids and `--play` references.
fn refs(c: &Connection) -> Result<String> {
    let mut out = String::new();
    for (var, name) in [("NORTHWIND", "Northwind"), ("AURORA", "Aurora"), ("HARBOR", "Harbor Free")] {
        let id: i64 = c.query_row("SELECT id FROM source WHERE name = ?1", [name], |r| r.get(0))?;
        let _ = writeln!(out, "{var}_ID={id}");
    }
    for (var, source, name) in
        [("PLAY_DOCS", "Northwind", "Polar Nature"), ("PLAY_SPORTS", "Aurora", "Summit Sports"), ("PLAY_NEWS", "Northwind", "Meridian News")]
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
        // the longest title that matches ("The Kid" is also in no other title, but be sure)
        let Some(film) = MOVIES.iter().filter(|m| name.contains(m.title)).max_by_key(|m| m.title.len()) else { continue };
        let d = MovieDetails {
            plot: Some(film.plot.into()),
            genre: Some(film.genre.into()),
            cast: Some(film.cast.join(", ")),
            director: Some(film.director.into()),
            duration_s: Some(film.minutes * 60),
            backdrop: Some(format!("{IMG}/backdrop/{}.png", slug(film.title))),
            ..Default::default()
        };
        tuner::store::import::store_movie_details(c, MovieId(id), &d, now)?;
    }
    // M3U series have no details path: `store_series_details` is the Xtream one, which
    // replaces the episode list and carries no genre, so plots and backdrops go in directly.
    for show in SERIES {
        c.execute(
            "UPDATE series SET plot = ?1, genre = ?2, backdrop = ?3 WHERE name LIKE '%' || ?4 || '%'",
            params![show.plot, show.genre, format!("{IMG}/backdrop/{}.png", slug(show.title)), show.title],
        )?;
    }
    Ok(())
}

/// Following: a team with fixtures in the guide, a daily news flash, a show that is only listed.
/// Nothing they set fires within the next few hours (the fixtures are tomorrow and the day
/// after; the news flash takes a bulletin that isn't due soon).
fn follow_rules(c: &mut Connection, now: Timestamp, cfg: &tuner::config::FollowConfig) -> tuner::error::Result<()> {
    use tuner::store::follow::save_rule;
    let team = FollowRule::draft("Northport Albion", FollowKind::Team, vec!["Northport Albion".into()], cfg, now);
    save_rule(c, &team, cfg.max_rules, now)?;

    let (key, source): (String, i64) = c.query_row(
        "SELECT c.key, c.source_id FROM channel c JOIN source s ON s.id = c.source_id WHERE s.name = 'Northwind' AND c.name LIKE 'Meridian News%'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let meridian = ItemRef { source: SourceId(source), kind: ItemKind::Channel, key: ItemKey(key) };
    let hour = jiff::Zoned::now().hour();
    let (title, between) = if (15..21).contains(&hour) { ("Morning Briefing", (7 * 60, 8 * 60)) } else { ("The Evening Report", (18 * 60, 19 * 60)) };
    let mut news = FollowRule::draft(title, FollowKind::News, vec![title.into()], cfg, now);
    news.spec.between = Some(between);
    news.spec.channels = ChannelScope::Channels(vec![meridian]);
    save_rule(c, &news, cfg.max_rules, now)?;

    let mut show = FollowRule::draft("Wild Frontiers", FollowKind::Show, vec!["Wild Frontiers".into()], cfg, now);
    show.mode = FollowMode::List;
    save_rule(c, &show, cfg.max_rules, now)?;
    Ok(())
}

/// Favourites, watchlists, recently watched channels, Continue Watching, reminders, Up next and the
/// playlists' connection limits.
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
    // Saved titles live in watchlists; favourites hold channels (DESIGN §42 §4.1).
    let night = tuner::store::watchlist::create(c, "Films & series", None, tuner::watchlist::PlayOrder::Any, "app", now - 80_000)?;
    let library = tuner::store::library::load(c)?;
    let titles = [
        need(movie(c, north, "Night of the Living Dead")?, "Night of the Living Dead")?,
        need(movie(c, aurora, "His Girl Friday")?, "His Girl Friday")?,
        need(series(c, north, "Beverly Hillbillies")?, "The Beverly Hillbillies")?,
        need(movie(c, aurora, "Nosferatu")?, "Nosferatu")?,
        need(movie(c, north, "The Gold Rush")?, "The Gold Rush")?,
    ];
    for (i, f) in titles.iter().enumerate() {
        let entry = tuner::store::watchlist::new_entry(c, f)?;
        tuner::store::watchlist::add(c, &library, night, &[entry], tuner::watchlist::Insert::End, now - 80_000 + i as i64 * 60)?;
    }

    // recently watched channels (oldest first)
    for (i, (src, name)) in [(north, "Courtside"), (aurora, "Atlas World"), (north, "Polar Nature"), (north, "Meridian News")].iter().enumerate() {
        let item = need(channel(c, *src, name)?, name)?;
        let title: String =
            c.query_row("SELECT name FROM channel WHERE source_id = ?1 AND key = ?2", params![item.source, item.key.0], |r| r.get(0))?;
        user::push_history(c, &HistoryEntry { item, at: now - 7200 + i as i64 * 900, title })?;
    }

    // Continue Watching: two movies and an episode
    for (src, name, pos, dur, ago) in [(north, "The Phantom of the Opera", 2830.0, 6420.0, 3600), (aurora, "Sunrise", 4100.0, 5700.0, 26 * 3600)] {
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
             WHERE s.source_id = ?1 AND s.name LIKE '%Beverly Hillbillies%' AND e.season = 1 AND e.episode = 6",
            [north],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((src, key, show)) = ep {
        let item = ItemRef { source: SourceId(src), kind: ItemKind::Episode, key: ItemKey(key) };
        let title = format!("{show} · S01E06");
        user::save_progress(
            c,
            &WatchProgress {
                item,
                position_s: 812.0,
                duration_s: Some(f64::from(SERIES[0].minutes * 60)),
                updated_at: now - 5400,
                finished: false,
                title,
                fingerprint: Default::default(),
            },
        )?;
    }

    // reminders: a notification for the next sports programme starting in 90 min or more, an
    // automatic switch (into the pop-out) and a film night; none fires within the hour
    let remind = |src: i64, name: &str, brand: &str, after: i64, lead_s: i64, switch: bool| -> tuner::error::Result<()> {
        let ch = need(channel(c, src, name)?, name)?;
        let Some(p) = progs.iter().filter(|p| p.channel == slug(brand) && p.start >= now + after).min_by_key(|p| p.start) else { return Ok(()) };
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
                lead_s: if switch { Reminder::SWITCH_LEAD } else { lead_s },
                state: ReminderState::Pending,
                mode: if switch { ReminderMode::Switch } else { ReminderMode::Notify },
                offset_s: 0,
                open_in: switch.then_some(OpenIn::PopOut),
                rule_id: None,
            },
        )?;
        Ok(())
    };
    remind(north, "Bluefield Sports", "Bluefield Sports", 90 * 60, 300, false)?;
    remind(north, "Lumen Docs", "Lumen Docs", 2 * 3600, 0, true)?;
    remind(aurora, "Northstar Cinema", "Northstar Cinema", 3 * 3600, 600, false)?;

    // Up next: the trilogy's first film played, an episode, then the other two films
    let mut rows = Vec::new();
    for Film { title, year, minutes, .. } in UP_NEXT.iter().filter_map(|t| MOVIES.iter().find(|m| m.title == *t)) {
        // the plain title could also find a longer one
        let item = need(
            one(c, "SELECT source_id, key FROM movie WHERE source_id = ?1 AND name LIKE ?2 ORDER BY id", north, &format!("{title} ("))?
                .map(|i| ItemRef { kind: ItemKind::Movie, ..i }),
            title,
        )?;
        rows.push(NewRow {
            item,
            any_copy: true,
            programme: None,
            title: format!("{title} ({year})"),
            subtitle: None,
            duration_s: Some(f64::from(minutes * 60)),
            from_start: false,
            by: AddedBy::App,
        });
    }
    let ep: (i64, String, String, i64, i64) = c.query_row(
        "SELECT e.source_id, e.key, e.title, e.season, e.episode FROM episode e JOIN series s ON s.id = e.series_id
         WHERE s.source_id = ?1 AND s.name LIKE '%Beverly Hillbillies%' AND e.season = 1 AND e.episode = 13",
        [north],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    rows.insert(
        2,
        NewRow {
            item: ItemRef { source: SourceId(ep.0), kind: ItemKind::Episode, key: ItemKey(ep.1) },
            any_copy: true,
            programme: None,
            title: SERIES[0].title.into(),
            subtitle: Some(format!("S{:02}E{:02} · {}", ep.3, ep.4, ep.2)),
            duration_s: Some(f64::from(SERIES[0].minutes * 60)),
            from_start: false,
            by: AddedBy::App,
        },
    );
    let added = tuner::store::queue::add(c, &rows, Insert::End, now - 4 * 3600)?;
    if let Some(&first) = added.ids.first() {
        tuner::store::queue::set_state(c, first, RowState::Played, now - 2 * 3600)?;
    }

    // one connection per playlist (multi-view then takes the other picture from the other
    // playlist), as on the accounts multi-view was designed for
    for src in [north, aurora] {
        let account = AccountInfo { status: AccountStatus::Active, max_connections: Some(1), ..Default::default() };
        tuner::store::sources::set_account(c, SourceId(src), &account)?;
    }
    Ok(())
}

/// Downloads and recordings, planned by the app's own `downloads::planning::new_job`. Nothing
/// fetches anything: the finished ones are links to the demo clip (`media`; an empty file
/// without one), the others are paused or scheduled. Two series' episodes (grouped per series),
/// a film, a documentary recorded earlier today and tomorrow's match scheduled.
fn downloads(c: &mut Connection, now: Timestamp, progs: &[Prog], cfg: &Config, media: Option<&Path>) -> tuner::error::Result<()> {
    use tuner::downloads::{State, Target, planning};
    use tuner::error::Error;
    use tuner::store::downloads as dl;
    let lib = tuner::store::library::load(c)?;
    let view = tuner::shared::ParentalView { unlocked: true, hides_explicit: false, pin_active: false };
    let root = dl::root(c, &cfg.downloads)?;
    std::fs::create_dir_all(&root).map_err(|e| Error::io(&root, e))?;
    let north: i64 = c.query_row("SELECT id FROM source WHERE name = 'Northwind'", [], |r| r.get(0))?;
    let item = |c: &Connection, kind: ItemKind, sql: &str, args: &[&dyn rusqlite::ToSql]| -> tuner::error::Result<ItemRef> {
        let (src, key): (i64, String) = c.query_row(sql, args, |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(ItemRef { source: SourceId(src), kind, key: ItemKey(key) })
    };
    let episode = |c: &Connection, show: &str, season: i64, ep: i64| {
        item(
            c,
            ItemKind::Episode,
            "SELECT e.source_id, e.key FROM episode e JOIN series s ON s.id = e.series_id
             WHERE s.source_id = ?1 AND s.name LIKE '%' || ?2 || '%' AND e.season = ?3 AND e.episode = ?4",
            &[&north, &show, &season, &ep],
        )
    };
    let movie = |c: &Connection, name: &str| {
        item(c, ItemKind::Movie, "SELECT source_id, key FROM movie WHERE source_id = ?1 AND name LIKE '%' || ?2 || '%' ORDER BY id", &[&north, &name])
    };
    let channel = |c: &Connection, name: &str| {
        item(c, ItemKind::Channel, "SELECT source_id, key FROM channel WHERE source_id = ?1 AND name LIKE ?2 || '%' ORDER BY id", &[&north, &name])
    };
    let slot = |brand: &str, pick: &dyn Fn(&Prog) -> bool| {
        progs.iter().filter(|p| p.channel == slug(brand) && pick(p)).min_by_key(|p| (now - p.start).abs()).map(|p| tuner::queue::Slot {
            start: p.start,
            stop: p.stop,
            catchup_id: None,
        })
    };
    const GB: u64 = 1_000_000_000;
    // (what, programme, state, bytes, added this long ago, watched)
    let last_night = slot("Polar Nature", &|p| p.stop <= now - 6 * 3600 && p.start >= now - 30 * 3600 && p.title != "Wild Frontiers");
    let match_day = slot("Bluefield Sports", &|p| p.title.contains(" v ") && p.start > now);
    let plan = [
        (episode(c, "One Step Beyond", 1, 1)?, None, State::Done, 6 * GB / 10, 5 * 3600, true),
        (episode(c, "One Step Beyond", 1, 2)?, None, State::Done, 5 * GB / 10, 5 * 3600 - 600, false),
        (episode(c, "One Step Beyond", 1, 9)?, None, State::Done, 6 * GB / 10, 5 * 3600 - 1200, false),
        (episode(c, "One Step Beyond", 1, 17)?, None, State::Paused, 6 * GB / 10, 1800, false),
        (episode(c, "Petticoat Junction", 1, 1)?, None, State::Done, 5 * GB / 10, 2 * 86_400, false),
        (episode(c, "Petticoat Junction", 1, 2)?, None, State::Done, 5 * GB / 10, 2 * 86_400 - 600, false),
        (movie(c, "D.O.A.")?, None, State::Done, 21 * GB / 10, 86_400, false),
        (movie(c, "Wings")?, None, State::Paused, 34 * GB / 10, 1200, false),
        (channel(c, "Polar Nature")?, last_night, State::Done, 27 * GB / 10, 20 * 3600, false),
        (channel(c, "Bluefield Sports")?, match_day, State::Scheduled, 0, 600, false),
    ];
    for (item, programme, state, bytes, ago, watched) in plan {
        if programme.is_none() && item.kind == ItemKind::Channel {
            continue;
        }
        let target = Target { item, programme, minutes: None, any_copy: true };
        let job = planning::new_job(c, &lib, view, cfg, &target, false, now - ago)?;
        let id = dl::add(c, &job)?;
        let mut job = dl::job(c, id)?.ok_or_else(|| Error::other("download vanished"))?;
        job.state = state;
        match state {
            State::Done => {
                let name = format!("{id}.mp4");
                let path = root.join(&name);
                let linked = media.filter(|m| m.is_file()).map(|m| std::os::unix::fs::symlink(m, &path));
                if !matches!(linked, Some(Ok(()))) {
                    std::fs::write(&path, b"").map_err(|e| Error::io(&path, e))?;
                }
                job.store_name = Some(name);
                job.bytes_done = bytes;
                job.bytes_total = Some(bytes);
                let done = now - ago + 1200;
                job.done_at = Some(done);
                job.watched_at = watched.then_some(done + (now - done) / 2);
            }
            // paused part of the way through
            State::Paused => {
                job.bytes_done = bytes * 38 / 100;
                job.bytes_total = Some(bytes);
            }
            _ => {}
        }
        dl::put(c, &job)?;
    }
    Ok(())
}
