//! guide benchmark. Owner: WP-11. Gate: epg_grid layout+paint of 5 000 seeded channels into a headless egui::Context with a fake EpgReader, median of 200 frames < 8 ms (release).
//! Run: `cargo bench --bench guide` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
//!
//! Simulates holding ↓ in the guide (DESIGN §13.4 "Perf"): every frame handles one `Down`
//! (focus model + neighbour lookup) and then lays out, paints and tessellates the whole grid
//! at 1600×900 (Wide, ≈ 5 h visible) with the production 2 ms DB frame budget. The fake reader
//! allocates 30-minute programmes per 6-hour bucket like `store::epg::window` would return
//! them, so cache misses cost what they cost in the app minus SQLite. Prints the median and
//! the 95th percentile and exits non-zero when the median misses the gate.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use tuner::keymap::Action;
use tuner::model::{
    ChannelId, ChannelSummary, EpgChannelId, GroupId, ItemKey, NowNext, ProgrammeFlags, ProgrammeId, ProgrammeSlim, SourceId, Timestamp,
};
use tuner::store::library::LiveIndex;
use tuner::theme::Theme;
use tuner::ui::LayoutClass;
use tuner::ui::data::{EpgReader, FrameBudget};
use tuner::ui::epg_grid::{EpgGridState, GridMetrics, GuideRow, epg_grid};

const CHANNELS: i64 = 5_000;
const FRAMES: usize = 200;
const WARMUP: usize = 20;
const GATE: Duration = Duration::from_millis(8);
const T0: Timestamp = 1_791_576_000;

const TITLES: [&str; 8] =
    ["News at Eight", "Gardeners' World", "Film: The Matrix", "Live Football", "The Simpsons", "Panorama", "Weather", "Big Bang Theory"];

struct Fake;

impl EpgReader for Fake {
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> tuner::error::Result<Vec<ProgrammeSlim>> {
        let mut out = Vec::with_capacity(16);
        let mut s = from - from.rem_euclid(1800);
        while s < to {
            // vary lengths a little: 30 or 60 minutes
            let len = if (s / 1800 + ch.0) % 3 == 0 { 3600 } else { 1800 };
            let i = ((s / 1800 + ch.0) % TITLES.len() as i64) as usize;
            out.push(ProgrammeSlim {
                id: ProgrammeId(ch.0 * 1_000_000 + s / 60),
                start: s,
                stop: s + len,
                title: Arc::from(TITLES[i]),
                sub_title: None,
                flags: ProgrammeFlags::default(),
            });
            s += len;
        }
        Ok(out)
    }
    fn now_next(&mut self, _: &[(EpgChannelId, i32)], _: Timestamp) -> tuner::error::Result<HashMap<(EpgChannelId, i32), NowNext>> {
        Ok(HashMap::new())
    }
}

fn live() -> LiveIndex {
    let chans: Vec<ChannelSummary> = (1..=CHANNELS)
        .map(|i| ChannelSummary {
            id: ChannelId(i),
            source_id: SourceId(1),
            key: ItemKey(format!("c{i}")),
            name: Arc::from(format!("Channel {i} HD")),
            number: Some(i as u32),
            logo: None,
            group_ids: vec![GroupId(1)],
            epg_channel_id: Some(EpgChannelId(i)),
            tvg_shift_min: 0,
            has_catchup: i % 4 == 0,
            catchup_days: if i % 4 == 0 { 7 } else { 0 },
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: i as i32,
            user_order: None,
            added_at: None,
        })
        .collect();
    let members = HashMap::from([(GroupId(1), chans.iter().map(|c| c.id).collect())]);
    LiveIndex::from_parts(chans, vec![], members, HashSet::new())
}

fn main() {
    let dir = std::env::temp_dir().join(format!("tuner-bench-guide-{}", std::process::id()));
    let paths = tuner::paths::AppPaths::rooted(&dir);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let http = tuner::sources::http::HttpClient::new(&Default::default()).expect("http");
    let ctx = egui::Context::default();
    let mut images = tuner::images::ImageCache::new(&paths, http, rt.handle().clone(), ctx.clone(), 16 << 20);
    let theme = Theme::fallback();
    tuner::theme::egui_style::apply(&ctx, &theme);
    let live = live();
    let rows: Arc<[GuideRow]> =
        live.channels.iter().map(|c| GuideRow { channel: c.id, epg: c.epg_channel_id, tvg_shift_min: 0, catchup_days: c.catchup_days }).collect();
    let now = T0 + 1234;
    let mut state = EpgGridState::new(rows, now);
    let metrics = GridMetrics::for_layout(LayoutClass::Wide, false, &theme);
    let mut reader = Fake;
    let mut budget = FrameBudget::default();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));

    let mut times = Vec::with_capacity(FRAMES);
    for frame in 0..WARMUP + FRAMES {
        let t = Instant::now();
        budget.begin_frame();
        // holding ↓: one row per frame
        state.handle(Action::Down, &mut reader, &mut budget, now);
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
            epg_grid(ui, &theme, &mut images, &mut reader, &mut budget, &live, &mut state, metrics, now);
        });
        out.textures_delta.clear(); // no renderer (epaint debug-asserts on unapplied deltas)
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        std::hint::black_box(prims);
        if frame >= WARMUP {
            times.push(t.elapsed());
        }
    }
    times.sort();
    let median = times[times.len() / 2];
    let p95 = times[times.len() * 95 / 100];
    let focus = state.focused().map(|(c, _)| c.0).unwrap_or_default();
    println!(
        "guide: {CHANNELS} channels, {FRAMES} frames holding ↓ (focus now on channel {focus}): median {median:?}, p95 {p95:?}, max {:?}",
        times[times.len() - 1]
    );
    let verdict = if median < GATE { "PASS" } else { "FAIL" };
    println!("gate {:<24} {:>8.2} ms   (< {} ms)   {verdict}", "guide frame (median)", median.as_secs_f64() * 1e3, GATE.as_millis());
    let _ = std::fs::remove_dir_all(&dir);
    if median >= GATE {
        eprintln!("guide bench FAILED the gate");
        std::process::exit(1);
    }
}
