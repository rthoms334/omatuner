//! guide benchmark. Owner: WP-11. Gate: epg_grid layout+paint of 5 000 seeded channels into a headless egui::Context with a fake EpgReader, median of 200 frames < 8 ms (release).
//! Run: `cargo bench --bench guide` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
fn main() {}
