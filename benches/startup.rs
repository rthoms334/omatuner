//! startup benchmark. Owner: WP-13. Gates (DESIGN §5.1, §5.4, §24.2 WP-13; release build):
//! first frame < 300 ms with a 100k-channel database; `library::load` < 400 ms.
//! Run: `cargo bench --bench startup` (harness = false). Prints one PASS/FAIL line per gate and
//! exits non-zero when one fails.
//!
//! The profile is generated here, deterministically, into a temp dir under the target dir
//! (real disk, like a user's profile): 100k channels, 50k movies, 20k series (the §5.4 budget),
//! 24 h of guide for 5 000 channels, a few favourites and history (so `StartScreen::Auto`
//! opens Home, the busiest start screen).
//!
//! "First frame" follows `main` minus the window: from before the font/theme helper threads
//! are spawned, through `Services::start` (store open + migrate, mpv init, …), the creation
//! callback (join helpers, `TunerApp::new`, which submits the async library load) and the
//! first `logic` + `ui` + tessellation on a headless egui context. Not included: creating the
//! Wayland window and GL context (winit/EGL — independent of the library size) and the GPU
//! upload of the first frame. Each run uses a fresh copy of the profile. Also reported: how
//! long until the library lands in the app, and frame times once it has.

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
        let out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
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

fn main() {
    let tmp = tempfile::Builder::new().prefix("tuner-bench-startup").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
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
            let r = launch(&db, &tmp.path().join(format!("run-{label}-{i}").replace(' ', "")), view);
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
    let ok = [gate("first frame (100k ch)", worst_median, FIRST_FRAME_GATE), gate("library load (100k ch)", load, LIBRARY_GATE)];
    if ok.contains(&false) {
        std::process::exit(1);
    }
}
