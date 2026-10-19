//! Real library stacks and screen actions over a fixed service snapshot (DESIGN §43 §8).
use super::*;
use crate::downloads::{DownloadsHandle, test_support};
use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};
use crate::ui::{NavRequest, testing::TestUi};
use std::sync::Arc;

fn fixture() -> (TestUi, Vec<Job>) {
    let mut ui = TestUi::new().unwrap();
    ui.services().store.writer().blocking(test_support::seed).unwrap();
    ui.set_library(crate::store::library::load(&ui.db).unwrap());
    let mut jobs = Vec::new();
    for (source, season, episode, bytes, done, watched, state) in [
        (1, 2, 7, 2_000_000_000, 120, false, State::Done),
        (1, 1, 10, 3_000_000_000, 130, true, State::Done),
        (3, 1, 2, 1_000_000_000, 140, false, State::Done),
        (3, 2, 7, 4_000_000_000, 150, true, State::Done),
        (1, 1, 2, 500_000_000, 160, false, State::Partial),
    ] {
        let mut job = test_support::job(&ui.db, source, season, episode);
        job.id = DownloadId(jobs.len() as i64 + 1);
        job.position = job.id.0;
        job.state = state;
        job.bytes_done = bytes;
        job.done_at = Some(done);
        job.watched_at = watched.then_some(done);
        jobs.push(job);
    }
    snapshot(&ui, &jobs);
    (ui, jobs)
}

fn snapshot(ui: &TestUi, jobs: &[Job]) {
    ui.services().agent.set_downloads(Some(DownloadsHandle::with_snapshot(DownloadsSnapshot {
        jobs: jobs.to_vec(),
        used_bytes: jobs.iter().map(|j| j.bytes_done).sum(),
        ..Default::default()
    })));
}

#[test]
fn different_copy_names_share_one_title_with_unique_counts_and_actual_file_size() {
    let (mut ui, _) = fixture();
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        let snap = shared::snapshot(ctx);
        let rows = screen.rows(ctx, &snap);
        assert_eq!(rows.len(), 1);
        let Row::Title(title) = &rows[0] else { panic!("expected the series row") };
        assert_eq!(title.name, "Landman");
        assert_eq!(title.summary(), "3 episodes · 10.5 GB · 1 unwatched");
        assert_eq!(title.jobs.len(), 5, "the stack joins match_hash copies and the explicit title_link");
    });
}

#[test]
fn series_orders_seasons_and_episodes_and_prefers_complete_then_newer_copies() {
    let (mut ui, jobs) = fixture();
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        assert!(screen.on_action(ctx, Action::Activate));
        let rows = screen.rows(ctx, &shared::snapshot(ctx));
        assert_eq!(rows.iter().map(|r| grouping::coordinates(r.job().unwrap())).collect::<Vec<_>>(), [(1, 2), (1, 10), (2, 7)]);
        assert_eq!(rows[0].job().unwrap().id, jobs[2].id, "complete beats newer partial");
        assert_eq!(rows[2].job().unwrap().id, jobs[3].id, "newer complete wins");
        screen.cursor = 2;
        screen.selected = Some(rows[2].key());
        screen.on_action(ctx, Action::ContextMenu);
    });
    assert!(ui.nav.iter().any(|n| matches!(n, NavRequest::Open(OverlayRequest::ContextMenu(menu))
        if menu.items.iter().any(|i| i.label == "Delete the other copy's file"))));
    ui.with_ctx(|ctx| screen.on_overlay_result(ctx, OverlayResult::Chosen(Action::Digit(207))));
    assert_eq!(screen.delete, [jobs[0].id], "delete only the other copy's file");
}

#[test]
fn concealed_copy_disappears_from_rows_counts_and_sizes() {
    let (mut ui, jobs) = fixture();
    ui.services()
        .store
        .writer()
        .blocking(|c| {
            c.execute("UPDATE series SET explicit=1 WHERE source_id=3", [])?;
            Ok(())
        })
        .unwrap();
    ui.set_library(crate::store::library::load_visible(&ui.db, crate::store::library::Visibility::NO_EXPLICIT).unwrap());
    snapshot(&ui, &jobs);
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        let snap = shared::snapshot(ctx);
        assert_eq!(snap.used_bytes, 5_500_000_000);
        let rows = screen.rows(ctx, &snap);
        let Row::Title(title) = &rows[0] else { panic!("expected a title") };
        assert_eq!(title.summary(), "3 episodes · 5.5 GB · 2 unwatched");
        assert!(title.jobs.iter().all(|j| j.source_id != Some(SourceId(3))));
        screen.on_action(ctx, Action::Activate);
        assert!(screen.rows(ctx, &snap).iter().all(|r| r.job().unwrap().source_id != Some(SourceId(3))));
    });
}

#[test]
fn enter_opens_series_and_each_back_key_restores_the_selected_title() {
    let (mut ui, mut jobs) = fixture();
    let mut queue = jobs[0].clone();
    queue.id = DownloadId(99);
    queue.state = State::Waiting;
    jobs.push(queue);
    snapshot(&ui, &jobs);
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        screen.on_action(ctx, Action::Down);
    });
    let selected = screen.selected.clone();
    for (key, alt) in [(egui::Key::Escape, false), (egui::Key::ArrowLeft, false), (egui::Key::ArrowLeft, true)] {
        ui.with_ctx(|ctx| {
            screen.on_action(ctx, Action::Activate);
        });
        assert!(screen.page.is_some());
        assert_eq!(screen.cursor, 0);
        let event =
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers { alt, ..Default::default() } };
        let action = crate::keymap::resolve(&[KeyContext::Downloads, KeyContext::Global], &event).unwrap();
        ui.with_ctx(|ctx| {
            assert!(screen.on_action(ctx, action));
        });
        assert!(screen.page.is_none());
        assert!(screen.selected == selected);
        assert_eq!(screen.cursor, 1);
    }
    assert!(ui.nav.is_empty(), "opening an episode list never starts playback");
}

#[test]
fn downloaded_titles_sort_by_completion_and_compact_rows_render() {
    let (mut ui, mut jobs) = fixture();
    let mut recording = jobs[0].clone();
    recording.id = DownloadId(90);
    recording.kind = crate::downloads::Kind::Live;
    recording.title = "Newsnight".into();
    recording.done_at = Some(200);
    jobs.push(recording);
    snapshot(&ui, &jobs);
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        let rows = screen.rows(ctx, &shared::snapshot(ctx));
        let Row::Title(title) = &rows[0] else { panic!("expected a title") };
        assert_eq!(title.name, "Newsnight");
    });
    ui.layout = LayoutClass::Compact;
    let egui = ui.egui.clone();
    let mut output = egui.run_ui(
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 600.0))), ..Default::default() },
        |panel| ui.with_ctx(|ctx| screen.ui(panel, ctx)),
    );
    output.textures_delta.clear();
    assert!(!output.shapes.is_empty());
}

#[test]
fn locked_download_still_requests_pin_when_played() {
    let (mut ui, _) = fixture();
    let mut config = (*ui.config).clone();
    config.parental.enabled = true;
    config.parental.pin_hash = Some("set".into());
    ui.config = Arc::new(config);
    let mut screen = DownloadsScreen::default();
    ui.services()
        .store
        .writer()
        .blocking(|c| {
            crate::store::user::set_item_override(
                c,
                &ItemRef { source: SourceId(3), kind: ItemKind::Series, key: ItemKey("landman".into()) },
                &crate::model::ItemOverride { locked: true, ..Default::default() },
            )
        })
        .unwrap();
    ui.set_library(crate::store::library::load(&ui.db).unwrap());
    ui.with_ctx(|ctx| {
        screen.on_action(ctx, Action::Activate);
        screen.on_action(ctx, Action::Activate);
    });
    assert!(ui.nav.iter().any(|n| matches!(n, NavRequest::Open(OverlayRequest::Pin { .. }))));
}

#[test]
fn episode_titles_remain_guarded_in_the_series_page() {
    let (mut ui, _) = fixture();
    ui.services().agent.set_guard(crate::guard::Guard::live());
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        screen.on_action(ctx, Action::Activate);
        let rows = screen.rows(ctx, &shared::snapshot(ctx));
        assert!(rows.iter().all(|r| !r.job().unwrap().subtitle.as_ref().unwrap().contains("A spoiler")));
        assert!(rows[0].job().unwrap().subtitle.as_ref().unwrap().starts_with("S01E02 · "));
    });
}

#[test]
fn clicking_a_series_opens_episodes_and_back_restores_its_row() {
    use crate::ui::testing::clicks;
    let (ui, _) = fixture();
    let mut h = clicks::harness((ui, DownloadsScreen::default()), |panel, (test, screen)| {
        test.with_ctx(|ctx| screen.ui(panel, ctx));
    });
    clicks::click_text(&mut h, "Landman");
    assert!(h.state().1.page.is_some());
    h.run_steps(1);
    clicks::click_text(&mut h, "← Downloads");
    assert!(h.state().1.page.is_none());
    assert!(matches!(h.state().1.selected, Some(RowKey::Title(_))));
}

#[test]
fn enter_on_a_film_plays_the_download() {
    let (mut ui, mut jobs) = fixture();
    let mut film = jobs.remove(0);
    film.kind = crate::downloads::Kind::Movie;
    film.source_id = None;
    film.item_key = None;
    film.title = "Dune".into();
    film.done_at = Some(200);
    jobs.push(film);
    snapshot(&ui, &jobs);
    let mut screen = DownloadsScreen::default();
    ui.with_ctx(|ctx| {
        screen.on_action(ctx, Action::Activate);
    });
    assert!(screen.page.is_none());
    assert!(ui.nav.iter().any(|n| matches!(n, NavRequest::Push(Route::Player))));
}
