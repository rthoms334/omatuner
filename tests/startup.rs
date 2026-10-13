//! Fatal startup errors (DESIGN §21.7): the real binary, started on a profile whose database a
//! newer Tuner wrote, refuses it, exits 1 and says why on stderr AND in `tuner.log` — the 0.2.6
//! incident, where only stderr had it and a launcher start showed nothing. The notification
//! itself is checked in `platform::startup` (fake sink); here `TUNER_NO_DESKTOP` keeps it off
//! the desktop and shows that a missing notification server changes nothing else.

use std::process::{Command, Stdio};

use tuner::paths::AppPaths;

#[test]
fn a_newer_database_is_refused_visibly_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted(dir.path());
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    {
        let c = rusqlite::Connection::open(paths.db_file()).unwrap();
        c.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO meta VALUES ('app_version', '9.9.9'); PRAGMA user_version = 99;")
            .unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_tuner"))
        .arg("--profile-dir")
        .arg(dir.path())
        .env("TUNER_NO_DESKTOP", "1")
        .env("WAYLAND_DISPLAY", "tuner-test-no-such-display")
        .env_remove("DISPLAY")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let latest = tuner::store::migrations::latest_version();
    let why = format!("database schema v99 is newer than this build (v{latest}), last opened by Tuner 9.9.9; refusing to open");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("Error: {why}")), "{stderr}");
    let log = std::fs::read_to_string(paths.log_file()).unwrap();
    assert!(log.lines().any(|l| l.contains(" ERROR tuner: Tuner exits: ") && l.contains(&why)), "{log}");
    // the refusal protects the data: nothing was migrated or re-stamped
    let c = rusqlite::Connection::open(paths.db_file()).unwrap();
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    let by: String = c.query_row("SELECT value FROM meta WHERE key = 'app_version'", [], |r| r.get(0)).unwrap();
    assert_eq!((v, by.as_str()), (99, "9.9.9"));
}

/// Every build from 0.2.7 on records itself, so an older one can name it (`meta.app_version`).
#[test]
fn opening_the_store_records_this_version() {
    let store = tuner::store::Store::open_temp("startup", None).unwrap();
    let c = store.reader().unwrap();
    let by: String = c.query_row("SELECT value FROM meta WHERE key = ?1", [tuner::store::migrations::APP_VERSION_KEY], |r| r.get(0)).unwrap();
    assert_eq!(by, env!("CARGO_PKG_VERSION"));
    assert!(store.close(std::time::Duration::from_secs(3)));
}
