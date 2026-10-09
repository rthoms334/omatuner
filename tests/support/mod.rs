//! Shared integration-test helpers. Include with `mod support;` from a test file.
//! FROZEN module list (skeleton); bodies owned as noted. `seed` is skeleton-implemented so no
//! test depends on WP-02's writers. UI tests use `tuner::ui::testing::TestUi`.
#![allow(dead_code, unused_variables)]

pub mod media;
pub mod mock_xtream;
pub mod seed;

use std::path::PathBuf;

/// `tests/fixtures/<rel>`. Fixtures are append-only for every WP; only the owner of a fixture
/// directory may modify existing files.
pub fn fixture(rel: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel) }

/// Isolated app paths in a temp dir (real home kept for theme reads; override if needed).
pub fn temp_paths() -> (tempfile::TempDir, tuner::paths::AppPaths) {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = tuner::paths::AppPaths::rooted(dir.path());
    paths.ensure().expect("ensure dirs");
    (dir, paths)
}
