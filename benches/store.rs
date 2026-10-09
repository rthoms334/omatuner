//! store benchmark. Owner: WP-02. Gate: 100k-channel import < 3 s; window query < 1 ms; library::load 100k < 400 ms (seeded DB).
//! Run: `cargo bench --bench store` (criterion, harness = false). Datasets: `scripts/gen-big-fixtures.py`.
fn main() {}
