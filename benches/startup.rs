//! startup benchmark. Owner: WP-13. Gates (DESIGN §5.1, §5.4, §24.2 WP-13; release build):
//! first frame < 300 ms with a 100k-channel database; `library::load` < 400 ms.
//! Run: `cargo bench --bench startup` (harness = false). Prints one PASS/FAIL line per gate and
//! exits non-zero when one fails.
//!
//! The profile is generated here, deterministically, into a temp dir under the target dir
//! (real disk, like a user's profile): 100k channels, 50k movies, 20k series (the §5.4 budget),
//! 24 h of guide for 5 000 channels, a few favourites and history (so `StartScreen::Auto`
//! opens Home, the busiest start screen), quality words on most channels and a measured
//! picture size on one in five; movies and series come in pairs of copies of one title, keyed
//! like an import keys them, so the build stacks them (DESIGN §29.11).
//!
//! "First frame" follows `main` minus the window: from before the font/theme helper threads
//! are spawned, through `Services::start` (store open + migrate, mpv init, …), the creation
//! callback (join helpers, `TunerApp::new`, which submits the async library load) and the
//! first `logic` + `ui` + tessellation on a headless egui context. Not included: creating the
//! Wayland window and GL context (winit/EGL — independent of the library size) and the GPU
//! upload of the first frame. Each run uses a fresh copy of the profile. Also reported: how
//! long until the library lands in the app, and frame times once it has.
//!
//! Then the library build of a real-size two-playlist catalogue (`support/catalogue.rs`: 80k
//! channels, 300k movies, 40k series, 2.5k categories, an English-only shared rule) with and
//! without the category rule — see [`bench_catalogue`]. `cargo bench --bench startup --
//! catalogue` runs that part alone; `TUNER_BENCH_CATALOGUE=<file>` keeps its database.

#[path = "support/catalogue.rs"]
mod catalogue;
#[path = "../tests/support/seed.rs"]
mod seed;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::App as _;
use eframe::egui;
use tuner::cli::ViewArg;
use tuner::config::Config;
use tuner::fonts::FontSet;
use tuner::ipc::IpcRequest;
use tuner::model::{ItemKey, ItemKind, ItemRef};
use tuner::msg::EventBus;
use tuner::paths::AppPaths;
use tuner::services::Services;
use tuner::store::{Store, library};
use tuner::theme::Theme;

const CHANNELS: usize = 100_000;
const MOVIES: usize = 50_000;
const SERIES: usize = 20_000;
const EPG_CHANNELS: usize = 5_000;
const RUNS: usize = 3;
const FIRST_FRAME_GATE: Duration = Duration::from_millis(300);
const LIBRARY_GATE: Duration = Duration::from_millis(400);

fn profile(root: &Path) -> AppPaths {
    let mut paths = AppPaths::rooted(root);
    paths.home = root.join("home"); // the fallback theme; never the user's Omarchy files
    paths
}

/// What `main` would load from the profile, minus integrations that reach the session bus.
fn config() -> Config {
    let mut cfg = Config::default();
    cfg.integration.mpris = false;
    cfg.reminders.enabled = false;
    cfg.general.refresh_on_launch_after_h = 0; // the seeded 127.0.0.1 sources are never refreshed
    cfg
}

fn seed_profile(paths: &AppPaths) {
    paths.ensure().expect("profile dirs");
    let store = Store::open(&paths.db_file(), None).expect("store");
    let now = tuner::model::now();
    store
        .writer()
        .blocking(move |c| {
            let src = seed::seed_source(c, "Bench")?;
            let ids = seed::seed_channels(c, src, CHANNELS)?;
            seed::seed_epg(c, &ids[..EPG_CHANNELS], now - now.rem_euclid(1800) - 6 * 3600, 24)?;
            seed::seed_movies(c, src, MOVIES)?;
            seed::seed_series_with_episodes(c, src, SERIES, 0, 0)?;
            let list = tuner::store::user::default_fav_list(c)?;
            for i in [0usize, 7, 42, 1_000, 4_999] {
                let item = ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
                tuner::store::user::set_favourite(c, list, &item, true, now)?;
                let entry = tuner::model::HistoryEntry { item, at: now - i as i64, title: format!("Channel {i}") };
                tuner::store::user::push_history(c, &entry)?;
            }
            // quality badges: a word in 4 names out of 5, and one channel in 5 measured
            c.execute_batch(
                "UPDATE channel SET quality = NULLIF(id % 5, 0), family = CASE WHEN id % 5 <> 0 THEN norm_name END;
                 INSERT INTO channel_video(source_id, item_key, name, url_key, width, height, at)
                   SELECT source_id, key, name, url_key, 1920, 1080, 0 FROM channel WHERE id % 5 = 1;",
            )?;
            // P2 stacks (DESIGN §29.11): titles as a provider names them, in pairs (an HD and a 4K
            // copy of each), keyed like an import does
            c.execute_batch(
                "UPDATE movie SET name = 'EN ★ Title ' || (id / 2) || ' - 2001' || CASE WHEN id % 2 = 1 THEN ' [4K]' ELSE '' END, year = 2001;
                 UPDATE series SET name = 'EN - Show ' || (id / 2) || ' (2010)', year = 2010;",
            )?;
            tuner::store::titles::refresh_all(c)?;
            c.execute_batch("ANALYZE")?;
            Ok(())
        })
        .expect("seed");
    assert!(store.close(Duration::from_secs(30)), "seed writer did not finish");
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn gate(name: &str, value: Duration, limit: Duration) -> bool {
    let ok = value < limit;
    println!("gate {name:<24} {:>8.1} ms   (< {} ms)   {}", value.as_secs_f64() * 1e3, limit.as_millis(), if ok { "PASS" } else { "FAIL" });
    ok
}

struct Run {
    first_frame: Duration,
    services: Duration,
    library_ready: Duration,
    frames_with_library: Duration,
}

/// One launch on a fresh copy of the seeded database.
fn launch(seeded_db: &Path, root: &Path, view: Option<ViewArg>) -> Run {
    let paths = profile(root);
    paths.ensure().expect("profile dirs");
    std::fs::copy(seeded_db, paths.db_file()).expect("copy profile");
    let cfg = config();

    let t0 = Instant::now();
    let (bus, events) = EventBus::new(None);
    let font_cfg = cfg.fonts.clone();
    let fonts_thread = std::thread::spawn(move || FontSet::resolve(&font_cfg).load());
    let (theme_paths, scale) = (paths.clone(), cfg.interface.ui_scale);
    let theme_thread = std::thread::spawn(move || Theme::load(&theme_paths, scale));
    let services = Services::start(paths, cfg, tuner::logging::LogBuffer::new(1000), bus.clone()).expect("services");
    let services_done = t0.elapsed();
    let jobs = services.jobs.clone();

    // eframe's creation callback
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    bus.attach_egui(ctx.clone());
    let fonts = fonts_thread.join().unwrap_or_default();
    let theme = Arc::new(theme_thread.join().unwrap_or_else(|_| Theme::fallback()));
    let mut app = tuner::app::TunerApp::new(&cc, services, events, theme, fonts, IpcRequest { view, ..Default::default() });
    let mut frame = eframe::Frame::_new_kittest();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0));
    let mut run_frame = |app: &mut tuner::app::TunerApp| {
        let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
        out.textures_delta.clear(); // no renderer (epaint debug-asserts on unapplied deltas)
        std::hint::black_box(ctx.tessellate(out.shapes, out.pixels_per_point));
    };
    run_frame(&mut app);
    let first_frame = t0.elapsed();

    // the async library load (RebuildLibrary), then a few frames with the full library
    let deadline = Instant::now() + Duration::from_secs(30);
    while !jobs.active().is_empty() {
        assert!(Instant::now() < deadline, "startup jobs never finished: {:?}", jobs.active());
        std::thread::sleep(Duration::from_millis(2));
    }
    run_frame(&mut app); // applies the Library event
    let library_ready = t0.elapsed();
    let mut frames: Vec<Duration> = (0..30)
        .map(|_| {
            let t = Instant::now();
            run_frame(&mut app);
            t.elapsed()
        })
        .collect();
    let frames_with_library = median(&mut frames);
    // eframe's on_exit: video surface (none here), then the services
    app.on_exit(None);
    drop(app);
    Run { first_frame, services: services_done, library_ready, frames_with_library }
}

/// The library build on a real-size two-playlist catalogue (`support/catalogue.rs`) with no
/// category rule, with the English rule as it was before titles were hidden by language
/// (`hide_other_languages: Some(false)`), and with the rule as the app applies it by default
/// (automatic title-language hiding). Prints the best and median load and the best of each
/// step (`library::load_profiled`). Gates: [`CATALOGUE_GATE`] (the build read every item
/// table through an SQL sort and a join per row before the perf track: ≈ 650 ms here), and
/// the default rule costs at most [`RULE_OVERHEAD`] × the rule without title languages.
fn bench_catalogue(tmp: &Path) -> bool {
    const RUNS: usize = 7;
    // `TUNER_BENCH_CATALOGUE=<file>`: seed there once and reuse it (profiling sessions)
    let kept = std::env::var_os("TUNER_BENCH_CATALOGUE").map(std::path::PathBuf::from);
    let db = kept.clone().unwrap_or_else(|| tmp.join("catalogue.db"));
    let t = Instant::now();
    if kept.is_none() || !db.exists() {
        catalogue::seed(&db);
    }
    let (ch, mv, se, groups) = catalogue::totals();
    println!(
        "catalogue: {ch} channels, {mv} movies, {se} series, {groups} categories over {} playlists ({:.0} MB, seeded in {:.1} s)",
        catalogue::PLAYLISTS.len(),
        std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0) as f64 / 1e6,
        t.elapsed().as_secs_f64()
    );
    let store = Store::open(&db, None).expect("store");
    let opts = library::LoadOptions { audio_langs: vec!["en".to_owned()], visibility: library::Visibility::ALL };
    let english = catalogue::english_rule();
    let variants = [
        ("no rule", None),
        ("rule, titles kept", Some(tuner::model::CategoryFilter { hide_other_languages: Some(false), ..english.clone() })),
        ("rule (default)", Some(english)),
    ];
    // rounds of every variant in turn (a busy machine slows them alike), each load on a fresh
    // reader like the RebuildLibrary job's
    type Times = (Vec<Duration>, Vec<(&'static str, Vec<Duration>)>, (usize, usize, usize));
    let mut times: Vec<Times> = variants.iter().map(|_| Default::default()).collect();
    for _ in 0..RUNS {
        for ((_, rule), (totals, steps, shown)) in variants.iter().zip(&mut times) {
            catalogue::set_rule(&store, rule.clone());
            let conn = store.reader().expect("reader");
            let t = Instant::now();
            let (lib, profile) = library::load_profiled(&conn, &opts).expect("library");
            totals.push(t.elapsed());
            *shown = (lib.live.channels.len(), lib.vod.movies.len(), lib.vod.series.len());
            for (i, (step, d)) in profile.0.into_iter().enumerate() {
                match steps.get_mut(i) {
                    Some(s) => s.1.push(d),
                    None => steps.push((step, vec![d])),
                }
            }
        }
    }
    assert!(store.close(Duration::from_secs(5)));
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let mut best = Vec::new();
    for ((label, _), (totals, steps, shown)) in variants.iter().zip(&mut times) {
        assert_eq!(shown.0, ch, "every channel is in the snapshot (hidden ones flagged)");
        let min = totals.iter().min().copied().unwrap_or_default();
        let detail: Vec<String> = steps.iter().map(|(s, v)| format!("{s} {:.0}", ms(v.iter().min().copied().unwrap_or_default()))).collect();
        println!(
            "library::load {label:<18} best {:>5.0} ms, median {:>5.0} ms  ({} movies, {} series shown)  best steps ms: {}",
            ms(min),
            ms(median(totals)),
            shown.1,
            shown.2,
            detail.join(", ")
        );
        best.push(min);
    }
    let limit = best[1].mul_f64(RULE_OVERHEAD);
    [gate("catalogue rule overhead", best[2], limit.max(Duration::from_millis(1))), gate("catalogue load (rule)", best[2], CATALOGUE_GATE)]
        .iter()
        .all(|ok| *ok)
}

/// The library build of the real-size catalogue with the default rule (best of the runs).
const CATALOGUE_GATE: Duration = Duration::from_millis(550);
/// What the default rule (with title-language hiding) may cost over the same rule without it.
const RULE_OVERHEAD: f64 = 1.25;

fn main() {
    let tmp = tempfile::Builder::new().prefix("tuner-bench-startup").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    // `cargo bench --bench startup -- catalogue`: the catalogue load only
    if std::env::args().any(|a| a == "catalogue") {
        let ok = bench_catalogue(tmp.path());
        drop(tmp);
        std::process::exit(i32::from(!ok));
    }
    let seed_paths = profile(&tmp.path().join("seed"));
    let t = Instant::now();
    seed_profile(&seed_paths);
    let db = seed_paths.db_file();
    let size_mb = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
    println!(
        "startup: profile with {CHANNELS} channels, {MOVIES} movies, {SERIES} series, 24 h guide for {EPG_CHANNELS} channels ({size_mb:.0} MB, seeded in {:.1} s){}",
        t.elapsed().as_secs_f64(),
        if cfg!(debug_assertions) { " — debug build: numbers not comparable" } else { "" }
    );

    // library::load on its own (the RebuildLibrary job body)
    let store = Store::open(&db, None).expect("store");
    let conn = store.reader().expect("reader");
    let mut loads: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let lib = library::load(&conn).expect("library");
            let d = t.elapsed();
            assert_eq!(lib.live.channels.len(), CHANNELS);
            d
        })
        .collect();
    drop(conn);
    assert!(store.close(Duration::from_secs(5)));
    let load = median(&mut loads);
    println!("library::load (5 runs): {:?}", loads.iter().map(|d| format!("{:.0} ms", d.as_secs_f64() * 1e3)).collect::<Vec<_>>());

    let mut firsts = Vec::new();
    for (label, view) in [("auto (Home)", None), ("live", Some(ViewArg::Live)), ("guide", Some(ViewArg::Guide))] {
        for i in 0..RUNS {
            let run_root = tmp.path().join(format!("run-{label}-{i}").replace(' ', ""));
            let r = launch(&db, &run_root, view);
            // each run copies the seeded database; keep at most one copy on disk
            let _ = std::fs::remove_dir_all(&run_root);
            println!(
                "launch {label:<11} #{i}: services {:>5.0} ms, first frame {:>5.0} ms, library in app {:>5.0} ms, frame with library {:>5.1} ms",
                r.services.as_secs_f64() * 1e3,
                r.first_frame.as_secs_f64() * 1e3,
                r.library_ready.as_secs_f64() * 1e3,
                r.frames_with_library.as_secs_f64() * 1e3,
            );
            firsts.push(r.first_frame);
        }
    }
    let worst_median = firsts.chunks(RUNS).map(|c| median(&mut c.to_vec())).max().unwrap_or_default();
    let ok = [
        gate("first frame (100k ch)", worst_median, FIRST_FRAME_GATE),
        gate("library load (100k ch)", load, LIBRARY_GATE),
        bench_catalogue(tmp.path()),
    ];
    if ok.contains(&false) {
        drop(tmp); // `exit` skips destructors
        std::process::exit(1);
    }
}
