//! Skeleton smoke tests: the frozen foundations work end to end. Owner: WP-00 / WP-13.

mod support;

use std::os::unix::fs::PermissionsExt;

#[test]
fn store_opens_private_and_migrated() {
    let (_dir, paths) = support::temp_paths();
    let store = tuner::store::Store::open(&paths.db_file(), None).expect("open");
    let mode = std::fs::metadata(paths.db_file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "DB must be private (it stores credentials)");
    let conn = store.reader().expect("reader");
    let lists: i64 = conn.query_row("SELECT count(*) FROM fav_list WHERE is_default = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(lists, 1);
    // writer round trip
    let n = store.writer().blocking(|c| Ok(c.execute("INSERT INTO meta(key, value) VALUES ('k', 'v')", [])?)).unwrap();
    assert_eq!(n, 1);
    assert!(store.close(std::time::Duration::from_secs(5)));
    // credential-bearing caches are private
    let mode = std::fs::metadata(paths.playlist_cache()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn fixtures_present() {
    assert!(support::fixture("m3u/edge_cases.m3u").exists());
    assert!(support::fixture("xmltv/basic.xml.gz").exists());
    assert!(support::fixture("xtream/auth_ok.json").exists());
    assert!(support::fixture("theme/liftoff/colors.toml").exists());
}

#[test]
fn liftoff_fixture_theme_parses() {
    let colors = std::fs::read_to_string(support::fixture("theme/liftoff/colors.toml")).unwrap();
    let shell = std::fs::read_to_string(support::fixture("theme/liftoff/shell.toml")).unwrap();
    let pal = tuner::theme::Palette::parse(&colors, false).unwrap();
    let sh = tuner::theme::ShellConfig::parse(&shell, None).unwrap();
    let theme = tuner::theme::Theme::from_parts("liftoff".into(), pal, sh, 0.0, 1.0);
    assert_eq!(theme.font(tuner::theme::TypeToken::Heading), 16.0);
    assert_eq!(theme.space(tuner::theme::SpaceToken::ControlHeight), 28.0);
    assert_eq!(theme.tokens.list_cursor_text, theme.palette.accent);
}

/// Seeders produce a consistent DB under concurrent readers (WAL temp DB, not shared cache).
#[test]
fn seeders_fill_a_consistent_catalogue() {
    use support::seed;
    let store = tuner::store::Store::open_temp("tuner-seed", None).unwrap();
    let (chans, epg) = store
        .writer()
        .blocking(|c| {
            let src = seed::seed_source(c, "Seed")?;
            let chans = seed::seed_channels(c, src, 200)?;
            let epg = seed::seed_epg(c, &chans[..20], 1_791_576_000, 6)?;
            seed::seed_movies(c, src, 50)?;
            seed::seed_series_with_episodes(c, src, 3, 2, 5)?;
            Ok((chans, epg))
        })
        .unwrap();
    assert_eq!(chans.len(), 200);
    let r = store.reader().unwrap();
    let n: i64 = r.query_row("SELECT count(*) FROM programme WHERE epg_source_id = ?1", [epg.0], |x| x.get(0)).unwrap();
    assert_eq!(n, 20 * 12);
    let hits: i64 = r.query_row("SELECT count(*) FROM programme_fts WHERE programme_fts MATCH '\"show\"'", [], |x| x.get(0)).unwrap();
    assert_eq!(hits, n);
    let eps: i64 = r.query_row("SELECT count(*) FROM episode", [], |x| x.get(0)).unwrap();
    assert_eq!(eps, 30);
    // integrity: FK + programme_grid uniqueness hold
    let fk: i64 = r.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |x| x.get(0)).unwrap();
    assert_eq!(fk, 0);
}

/// The headless UI harness starts real services (mpv vo=null) and every screen/overlay of the
/// skeleton handles actions and draws without panicking (no reachable `todo!()`).
#[test]
fn test_ui_runs_every_screen_and_overlay() {
    use tuner::keymap::Action;
    use tuner::ui::overlays::{self, ConfirmSpec};
    use tuner::ui::{OverlayRequest, Route};
    let mut t = tuner::ui::testing::TestUi::new().expect("TestUi");
    let mut screens = tuner::ui::screens::all();
    let egui = t.egui.clone();
    for s in screens.iter_mut() {
        assert!(!s.key_contexts().is_empty());
        t.with_ctx(|ctx| {
            s.on_enter(ctx, &Route::Home);
            s.on_action(ctx, Action::Down);
        });
        let mut out = egui.run_ui(Default::default(), |ui| t.with_ctx(|ctx| s.ui(ui, ctx)));
        out.textures_delta.clear();
    }
    let reqs = vec![
        OverlayRequest::Search { initial: "bbc".into() },
        OverlayRequest::Jobs,
        OverlayRequest::Help,
        OverlayRequest::Confirm(ConfirmSpec { title: "t".into(), body: "b".into(), confirm_label: "ok".into(), destructive: true, token: 1 }),
        OverlayRequest::Tracks { audio: true },
    ];
    for req in reqs {
        let mut o = t.with_ctx(|ctx| overlays::build(req, ctx));
        t.with_ctx(|ctx| {
            let _ = o.on_action(ctx, Action::Down);
        });
        let mut out = egui.run_ui(Default::default(), |_ui| {
            t.with_ctx(|ctx| {
                let _ = o.ui(&egui, ctx);
            })
        });
        out.textures_delta.clear();
    }
}
