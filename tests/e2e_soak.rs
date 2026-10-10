//! Memory-stability probe (WP-13; the automated stand-in for the "1-hour soak, RSS ± 10 %"
//! manual item). The real `TunerApp` (real services in a temp profile, headless mpv
//! `vo=null`/`ao=null`, no window) sits on the TV guide of a seeded profile while the test zaps
//! through local test streams and scrolls the guide one row per frame. RSS is sampled every
//! 100 zaps (after `malloc_trim`, so allocator retention is not counted); after a warm-up
//! (caches, mpv demuxer buffers and egui galleys reach their steady size) it must stay within
//! +10 % of the post-warm-up value. SQLite's heap is reported next to it and left out of the
//! comparison: its page caches (up to 16 MB per reader, 32 MB for the writer) are bounded, so
//! filling them is not a leak (measured: ≈ 1 MiB in this probe).
//!
//! Too slow for the normal gate (≈ 2 000 real stream loads), so it is ignored:
//! `cargo test --release --test e2e_soak -- --ignored --nocapture`
//! (`TUNER_SOAK_ZAPS=n` changes the number of zaps; default 2 000, warm-up = 20 %).
//! Skips with a note when ffmpeg is missing (`support::media::ensure_media`).

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::App as _;
use eframe::egui;
use support::media::{Faults, TestServer, ensure_media};
use tuner::cli::ViewArg;
use tuner::config::Config;
use tuner::ipc::IpcRequest;
use tuner::model::{Collection, MediaRef, PlayOrigin, ZapList};
use tuner::msg::EventBus;
use tuner::player::{PlayRequest, PlayerCommand, PlayerState};
use tuner::services::Services;
use tuner::theme::Theme;

const CHANNELS: usize = 2_000;
const STREAMS: usize = 40;

unsafe extern "C" {
    /// glibc: return free heap memory to the OS.
    fn malloc_trim(pad: usize) -> i32;
}

/// Resident set size of this process in KiB (`VmRSS`), after `malloc_trim`: freed memory that
/// glibc keeps in its per-thread arenas (mpv starts threads on every load) would otherwise show
/// up as 20 MiB steps that are not leaks.
fn rss_kib() -> u64 {
    // SAFETY: plain glibc call without pointers; safe at any time.
    unsafe { malloc_trim(0) };
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Memory held by SQLite's allocator (page caches, statements) in KiB.
fn sqlite_kib() -> u64 {
    // SAFETY: reads SQLite's global memory counter; safe from any thread.
    (unsafe { rusqlite::ffi::sqlite3_memory_used() }).max(0) as u64 / 1024
}

#[test]
#[ignore = "slow (≈ 2 000 stream loads); run explicitly, see module docs"]
fn memory_stays_flat_over_2000_zaps_and_guide_scrolls() {
    let Some(media) = ensure_media() else { return };
    let zaps: usize = std::env::var("TUNER_SOAK_ZAPS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000);
    let warmup = (zaps / 5).max(1);
    let server = TestServer::start(media, Faults::default());
    let files = ["bars.ts", "hls/index.m3u8", "testsrc.mp4"];

    // profile: channels with a guide; the first STREAMS point at local test media
    let dir = tempfile::tempdir().expect("tempdir");
    let mut paths = tuner::paths::AppPaths::rooted(dir.path());
    paths.home = dir.path().join("home");
    paths.ensure().expect("dirs");
    let now = tuner::model::now();
    let urls: Vec<String> = (0..STREAMS).map(|i| server.url(files[i % files.len()])).collect();
    let ids = {
        let store = tuner::store::Store::open(&paths.db_file(), None).expect("store");
        let ids = store
            .writer()
            .blocking(move |c| {
                let src = support::seed::seed_source(c, "Soak")?;
                let ids = support::seed::seed_channels(c, src, CHANNELS)?;
                support::seed::seed_epg(c, &ids, now - now.rem_euclid(1800) - 6 * 3600, 48)?;
                for (id, url) in ids.iter().zip(&urls) {
                    let stream = serde_json::json!({ "t": "url", "url": url }).to_string();
                    c.execute("UPDATE channel SET stream = ?1 WHERE id = ?2", rusqlite::params![stream, id.0])?;
                }
                Ok(ids)
            })
            .expect("seed");
        assert!(store.close(Duration::from_secs(10)));
        ids
    };

    let mut cfg = Config::default();
    cfg.player.mpv_options.insert("vo".into(), "null".into());
    cfg.player.mpv_options.insert("ao".into(), "null".into());
    cfg.player.zap_delay_ms = 0;
    cfg.integration.mpris = false;
    cfg.integration.follow_omarchy_theme = false;
    cfg.reminders.enabled = false;
    cfg.general.refresh_on_launch_after_h = 0;
    let (bus, events) = EventBus::new(None);
    let services = Services::start(paths, cfg, tuner::logging::LogBuffer::new(1000), bus).expect("services");
    let player = services.player.handle();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let initial = IpcRequest { view: Some(ViewArg::Guide), ..Default::default() };
    let mut app = tuner::app::TunerApp::new(&cc, services, events, Arc::new(Theme::fallback()), Default::default(), initial);
    let mut frame = eframe::Frame::_new_kittest();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0));
    let mut run_frame = |app: &mut tuner::app::TunerApp, key: Option<egui::Key>| {
        let events = key
            .map(|key| {
                let ev = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE };
                vec![ev(true), ev(false)]
            })
            .unwrap_or_default();
        let mut raw = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
        app.raw_input_hook(&ctx, &mut raw);
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
        out.textures_delta.clear(); // no renderer (epaint debug-asserts on unapplied deltas)
        std::hint::black_box(ctx.tessellate(out.shapes, out.pixels_per_point));
    };
    // let the startup library load land
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        run_frame(&mut app, None);
        std::thread::sleep(Duration::from_millis(10));
    }

    let zap_list = ZapList { collection: Collection::AllChannels, channels: Arc::from(&ids[..STREAMS]) };
    player.send(PlayerCommand::Play(PlayRequest {
        media: MediaRef::Channel(ids[0]),
        zap: Some(zap_list),
        origin: PlayOrigin::Fullscreen,
        resume: false,
        immediate: true,
    }));
    let started = Instant::now();
    let (mut baseline, mut samples, mut failed, mut scrolls) = (None, Vec::new(), 0usize, 0usize);
    for z in 1..=zaps {
        let generation = player.snapshot().load_generation;
        player.send(PlayerCommand::Zap { forward: true });
        // frames run meanwhile: one guide row down per frame (back up at the end of the list)
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let key = if (scrolls / CHANNELS).is_multiple_of(2) { egui::Key::ArrowDown } else { egui::Key::ArrowUp };
            run_frame(&mut app, Some(key));
            scrolls += 1;
            let s = player.snapshot();
            let loaded = s.load_generation > generation;
            match &s.state {
                PlayerState::Playing if loaded => break,
                PlayerState::Error(_) if loaded => {
                    failed += 1;
                    break;
                }
                _ if Instant::now() > deadline => {
                    failed += 1;
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        if z == warmup {
            baseline = Some(rss_kib().saturating_sub(sqlite_kib()));
        }
        if z % 100 == 0 || z == zaps {
            let (rss, sqlite) = (rss_kib(), sqlite_kib());
            samples.push((z, rss.saturating_sub(sqlite)));
            println!(
                "zap {z:>5}: RSS {:>7.1} MiB, of which SQLite {:>5.1} MiB ({scrolls} guide scrolls/frames, {failed} failed loads, {:.0} s)",
                rss as f64 / 1024.0,
                sqlite as f64 / 1024.0,
                started.elapsed().as_secs_f64()
            );
        }
    }
    app.on_exit(None);
    drop(app);

    let baseline = baseline.expect("warm-up reached");
    let (_, last) = *samples.last().expect("samples");
    let peak = samples.iter().filter(|(z, _)| *z >= warmup).map(|(_, r)| *r).max().unwrap_or(last);
    let growth = (last as f64 / baseline as f64 - 1.0) * 100.0;
    println!(
        "soak: {zaps} zaps, {scrolls} guide scrolls in {:.0} s; RSS without SQLite after warm-up {:.1} MiB, end {:.1} MiB ({growth:+.1} %), peak {:.1} MiB; {failed} loads failed",
        started.elapsed().as_secs_f64(),
        baseline as f64 / 1024.0,
        last as f64 / 1024.0,
        peak as f64 / 1024.0
    );
    assert!(failed * 20 <= zaps, "too many failed loads ({failed}) — the probe did not exercise playback");
    assert!(last as f64 <= baseline as f64 * 1.10, "RSS without SQLite grew {growth:+.1} % after warm-up (limit +10 %)");
}
