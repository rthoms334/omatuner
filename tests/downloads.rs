//! Downloads persistence, local resolution, scheduler and rule behavior (DESIGN §43 §8).
mod support;
use rusqlite::params;
use tuner::config::Config;
use tuner::downloads::{Job, Kind, State, Target, maintenance, planning, scheduler};
use tuner::model::*;
use tuner::shared::ParentalView;
use tuner::store::{Store, downloads as db, library, user};

const NOW: i64 = 1_791_900_000;
fn view() -> ParentalView {
    ParentalView { unlocked: true, hides_explicit: false, pin_active: false }
}
fn fixture() -> (Store, ItemRef, ItemRef, ItemRef, ChannelId) {
    let s = Store::open_temp("downloads", None).unwrap();
    let (movie, copy, series, channel) = s
        .writer()
        .blocking(|c| {
            let a = support::seed::seed_source(c, "A")?;
            let b = support::seed::seed_source(c, "B")?;
            let m = support::seed::seed_movies(c, a, 1)?[0];
            let n = support::seed::seed_movies(c, b, 1)?[0];
            let series = support::seed::seed_series_with_episodes(c, a, 1, 1, 5)?[0];
            let channel = support::seed::seed_channels(c, a, 1)?[0];
            support::seed::seed_epg(c, &[channel], NOW + 3600, 4)?;
            let item = |m| -> tuner::error::Result<ItemRef> {
                let m = tuner::store::vod::movie(c, m)?;
                Ok(ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key })
            };
            let se = tuner::store::vod::series(c, series)?;
            Ok((item(m)?, item(n)?, ItemRef { source: a, kind: ItemKind::Series, key: se.key }, channel))
        })
        .unwrap();
    (s, movie, copy, series, channel)
}
fn job(c: &rusqlite::Connection, item: ItemRef) -> Job {
    let lib = library::load(c).unwrap();
    planning::new_job(c, &lib, view(), &Config::default(), &Target { item, programme: None, minutes: Some(30), any_copy: true }, false, NOW).unwrap()
}

#[test]
fn finished_copy_is_local_partial_is_explicit_and_deleted_playlist_still_plays() {
    let (s, item, copy, _, _) = fixture();
    let c = s.reader().unwrap();
    let movie = tuner::store::vod::resolve_movie(&c, &copy).unwrap().unwrap();
    let mut j = job(&c, item.clone());
    j.fp["download_copies"] = serde_json::json!([item, copy]);
    j.state = State::Done;
    let id = s.writer().blocking(move |c| db::add(c, &j)).unwrap();
    let root = db::root(&c, &Config::default().downloads).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(format!("{id}.mp4")), b"private media").unwrap();
    s.writer()
        .blocking(move |c| {
            c.execute("UPDATE download SET store_name=?2 WHERE id=?1", params![id, format!("{id}.mp4")])?;
            Ok(())
        })
        .unwrap();
    let resolve = |media| tuner::sources::resolve::resolve(&c, &Config::default(), &media, false, NOW).unwrap();
    let local = resolve(MediaRef::Movie(movie));
    assert!(local.from_download && local.source_id.is_none());
    assert_eq!(local.item, Some(item.clone()), "progress belongs to the downloaded copy");
    for state in [State::Running, State::Partial] {
        s.writer().blocking(move |c| db::set_state(c, id, state, None)).unwrap();
        assert!(!resolve(MediaRef::Movie(movie)).from_download);
    }
    assert!(resolve(MediaRef::Download(id)).from_download);
    s.writer().blocking(move |c| tuner::store::sources::delete_source(c, item.source)).unwrap();
    assert!(resolve(MediaRef::Download(id)).from_download);
    assert!(db::job(&c, id).unwrap().unwrap().orphaned_since.is_some());
    assert!(user::orphans(&c).unwrap().iter().any(|o| o.holders.contains(&"download")));
}

#[test]
fn startup_reconciles_running_missing_stray_and_kept_finishes() {
    let (s, item, _, _, _) = fixture();
    let c = s.reader().unwrap();
    let root = db::root(&c, &Config::default().downloads).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let mut j = job(&c, item);
    j.state = State::Running;
    let id = s.writer().blocking(move |c| db::add(c, &j)).unwrap();
    std::fs::write(root.join(format!("{id}.mp4.part")), b"kept").unwrap();
    std::fs::write(root.join("999.mp4.part"), b"stray").unwrap();
    let r = root.clone();
    s.writer().blocking(move |c| db::reconcile(c, &r)).unwrap();
    assert_eq!(db::job(&c, id).unwrap().unwrap().state, State::Waiting);
    assert!(root.join(format!("{id}.mp4.part")).exists());
    assert!(!root.join("999.mp4.part").exists());
    s.writer().blocking(move |c| db::set_state(c, id, State::Done, None)).unwrap();
    let r = root.clone();
    s.writer().blocking(move |c| db::reconcile(c, &r)).unwrap();
    assert_eq!(db::job(&c, id).unwrap().unwrap().reason.as_deref(), Some("file missing"));
    let kept = root.join(id.to_string());
    std::fs::create_dir(&kept).unwrap();
    std::fs::write(kept.join("index.json"), b"{}").unwrap();
    s.writer()
        .blocking(move |c| {
            c.execute("UPDATE download SET state='running',kind='live',store_name=?2 WHERE id=?1", params![id, format!("{id}/")])?;
            Ok(())
        })
        .unwrap();
    s.writer().blocking(move |c| db::reconcile(c, &root)).unwrap();
    assert_eq!(db::job(&c, id).unwrap().unwrap().state, State::Finishing);
}

#[test]
fn scheduler_padding_suspend_and_unanswered_clash_keep_watching() {
    let (s, _, _, _, channel) = fixture();
    let c = s.reader().unwrap();
    let ch = tuner::store::live::channel(&c, channel).unwrap();
    let mut j = job(&c, ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key });
    j.prog_start = Some(NOW + 3600);
    j.prog_stop = Some(NOW + 7200);
    j.pad_before_s = 60;
    j.pad_after_s = 300;
    assert_eq!(j.start(), Some(NOW + 3540));
    assert_eq!(j.stop(), Some(NOW + 7500));
    assert_eq!(scheduler::due(&j, NOW + 3539, false, true), scheduler::Due::Wait);
    assert_eq!(scheduler::due(&j, NOW + 3540, false, true), scheduler::Due::Start);
    assert_eq!(scheduler::due(&j, NOW + 3540, true, true), scheduler::Due::Ask);
    j.fp["download_clash_at"] = (NOW + 3540).into();
    assert_eq!(scheduler::due(&j, NOW + 3600, true, true), scheduler::Due::Wait);
    j.reason = Some("watching".into());
    assert_eq!(scheduler::due(&j, NOW + 3700, false, true), scheduler::Due::Wait);
    assert_eq!(scheduler::due(&j, NOW + 7300, false, true), scheduler::Due::Catchup);
    assert_eq!(scheduler::due(&j, NOW + 7300, false, false), scheduler::Due::Missed);
    j.kind = Kind::Catchup;
    assert_eq!(scheduler::due(&j, NOW + 7300, true, true), scheduler::Due::Start);
}

#[test]
fn keep_n_refills_after_watching_or_deleting_and_never_exceeds_n() {
    let (s, _, _, series, _) = fixture();
    s.writer()
        .blocking(move |c| {
            db::keep(c, &series, 3, NOW)?;
            let lib = library::load(c)?;
            maintenance::refill(c, &lib, &Config::default(), view(), NOW)?;
            maintenance::refill(c, &lib, &Config::default(), view(), NOW)?;
            let rows = db::jobs(c)?;
            assert_eq!(rows.len(), 3);
            let first = rows[0].item().unwrap();
            user::save_progress(
                c,
                &WatchProgress {
                    item: first,
                    position_s: 2700.0,
                    duration_s: Some(2700.0),
                    finished: true,
                    updated_at: NOW + 1,
                    fingerprint: Default::default(),
                    title: "Episode".into(),
                },
            )?;
            c.execute("UPDATE download SET watched_at=?2 WHERE id=?1", params![rows[0].id, NOW + 1])?;
            maintenance::refill(c, &lib, &Config::default(), view(), NOW + 2)?;
            assert_eq!(db::jobs(c)?.len(), 4);
            db::set_state(c, rows[1].id, State::Cancelled, Some("deleted"))?;
            maintenance::refill(c, &lib, &Config::default(), view(), NOW + 3)?;
            maintenance::refill(c, &lib, &Config::default(), view(), NOW + 4)?;
            let rows = db::jobs(c)?;
            assert_eq!(rows.len(), 5);
            assert_eq!(rows.iter().filter(|j| j.watched_at.is_none() && j.state != State::Cancelled).count(), 3);
            Ok(())
        })
        .unwrap();
}

#[test]
fn locked_agent_add_and_hidden_jobs_fail_closed_and_backup_excludes_media() {
    let (s, item, _, _, _) = fixture();
    s.writer()
        .blocking(move |c| {
            user::set_item_override(c, &item, &ItemOverride { locked: true, ..Default::default() })?;
            let lib = library::load(c)?;
            let target = Target { item: item.clone(), programme: None, minutes: None, any_copy: false };
            assert!(planning::new_job(c, &lib, view(), &Config::default(), &target, true, NOW).is_err());
            let mut j = planning::new_job(c, &lib, view(), &Config::default(), &target, false, NOW)?;
            j.fp["download_explicit"] = true.into();
            j.source_id = None;
            assert!(planning::concealed(c, &lib, ParentalView { hides_explicit: true, ..view() }, &j));
            let id = db::add(c, &j)?;
            let backup = db::export(c)?.unwrap();
            assert_eq!(backup["media_included"], false);
            let encoded = serde_json::to_string(&backup).unwrap();
            assert!(!encoded.contains("http"));
            assert_eq!(db::job(c, id)?.unwrap().id, id);
            Ok(())
        })
        .unwrap();
}

#[test]
fn guide_move_follows_title_and_disappearance_keeps_the_last_time() {
    let (s, _, _, _, channel) = fixture();
    s.writer()
        .blocking(move |c| {
            let ch = tuner::store::live::channel(c, channel)?;
            let epg = ch.epg_channel_id.unwrap();
            let p = tuner::store::epg::window(c, epg, NOW + 3600, NOW + 3601)?.remove(0);
            let target = Target {
                item: ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key },
                programme: Some(tuner::queue::Slot { start: p.start, stop: p.stop, catchup_id: None }),
                minutes: None,
                any_copy: false,
            };
            let lib = library::load(c)?;
            let j = planning::new_job(c, &lib, view(), &Default::default(), &target, false, NOW)?;
            let id = db::add(c, &j)?;
            c.execute("UPDATE programme SET start=start+600,stop=stop+600 WHERE id=?1", [p.id])?;
            maintenance::retime(c, NOW + 1)?;
            let moved = db::job(c, id)?.unwrap();
            assert_eq!(moved.prog_start, Some(p.start + 600));
            c.execute("DELETE FROM programme WHERE id=?1", [p.id])?;
            maintenance::retime(c, NOW + 2)?;
            let gone = db::job(c, id)?.unwrap();
            assert_eq!(gone.prog_start, moved.prog_start);
            assert_eq!(gone.reason.as_deref(), Some("guide missing"));
            Ok(())
        })
        .unwrap();
}

#[test]
fn verified_finish_recovers_after_folder_was_deleted_before_database_commit() {
    for expected in [State::Done, State::Partial, State::Failed] {
        let (s, item, _, _, _) = fixture();
        let c = s.reader().unwrap();
        let mut j = job(&c, item);
        j.state = State::Finishing;
        j.fp["download_partial"] = (expected == State::Partial).into();
        j.fp["download_capture_failed"] = (expected == State::Failed).into();
        let id = s.writer().blocking(move |c| db::add(c, &j)).unwrap();
        let root = db::root(&c, &Default::default()).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(format!("{id}.mkv")), b"verified media").unwrap();
        std::fs::write(root.join(format!("{id}.mkv.ok")), b"180").unwrap();
        s.writer().blocking(move |c| db::reconcile(c, &root)).unwrap();
        let recovered = db::job(&c, id).unwrap().unwrap();
        assert_eq!(recovered.state, expected);
        assert_eq!(recovered.fp["download_duration_s"], 180.0);
        assert_eq!(recovered.store_name, Some(format!("{id}.mkv")));
    }
}

#[test]
fn ask_prompt_offers_a_free_copy_and_cancel_keeps_the_download_lease() {
    use egui_kittest::{Harness, kittest::Queryable};
    use tuner::ui::overlays::{Overlay, OverlayOutcome, downloads::Ask};
    use tuner::ui::testing::TestUi;
    let mut t = TestUi::new().unwrap();
    let (movie, source) = t
        .services()
        .store
        .writer()
        .blocking(|c| {
            let a = support::seed::seed_source(c, "TV")?;
            let b = support::seed::seed_source(c, "TV 2")?;
            let movie = support::seed::seed_movies(c, a, 1)?[0];
            support::seed::seed_movies(c, b, 1)?;
            c.execute("UPDATE movie SET tmdb_id='123'", [])?;
            Ok((movie, a))
        })
        .unwrap();
    let lib = library::load(&t.db).unwrap();
    t.set_library(lib);
    drop(t.services().agent.publish_library(&t.lib));
    let budget = t.services().tiles.budget().clone();
    budget.set_limit(source, Some(1));
    let id = DownloadId(17);
    let lease = budget.acquire(id, source).unwrap();
    let command = tuner::player::PlayerCommand::Play(tuner::player::PlayRequest {
        media: MediaRef::Movie(movie),
        zap: None,
        origin: PlayOrigin::Home,
        resume: true,
        immediate: true,
    });
    let ask = Ask::new(tuner::ui::downloads::Busy { id, source, tile: TileId::A, command: Box::new(command) });
    struct App {
        t: TestUi,
        ask: Ask,
    }
    let mut h = Harness::builder().with_size(eframe::egui::vec2(1000.0, 800.0)).build_ui_state(
        |ui, app: &mut App| {
            let ask = &mut app.ask;
            app.t.with_ctx(|ctx| {
                ask.ui(ui.ctx(), ctx);
            });
        },
        App { t, ask },
    );
    h.run_steps(3);
    assert!(h.query_all_by_label("TV is busy").next().is_some());
    assert!(h.query_all_by_label("Play from TV 2 · 2").next().is_some());
    let app = h.state_mut();
    let ask = &mut app.ask;
    assert!(matches!(app.t.with_ctx(|ctx| ask.on_action(ctx, tuner::keymap::Action::Back)), OverlayOutcome::Close));
    assert_eq!(budget.download_holder(source), Some(id));
    assert_eq!(budget.claim_of(TileId::A), None, "cancel must not reserve the account or send Play");
    budget.release(id, lease, None);
}

#[test]
fn backup_restore_is_metadata_only_idempotent_and_preserves_future_schedules() {
    let (source, item, _, _, channel) = fixture();
    source
        .writer()
        .blocking(move |c| {
            let mut film = job(c, item);
            film.state = State::Done;
            film.store_name = Some("1.mp4".into());
            film.bytes_done = 1234;
            db::add(c, &film)?;
            let ch = tuner::store::live::channel(c, channel)?;
            let mut recording = job(c, ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key });
            recording.prog_start = Some(NOW + 3600);
            recording.prog_stop = Some(NOW + 7200);
            recording.state = State::Scheduled;
            db::add(c, &recording)?;
            Ok(())
        })
        .unwrap();
    let backup = db::export(&source.reader().unwrap()).unwrap().unwrap();
    let (restored, _, _, _, _) = fixture();
    restored
        .writer()
        .blocking(move |c| {
            assert_eq!(db::import(c, &backup, &|i| Some(i.clone()))?, 2);
            assert_eq!(db::import(c, &backup, &|i| Some(i.clone()))?, 0);
            let rows = db::jobs(c)?;
            assert_eq!(rows[0].state, State::Failed);
            assert_eq!(rows[0].reason.as_deref(), Some("file missing"));
            assert!(rows[0].store_name.is_none());
            assert_eq!(rows[0].bytes_done, 0);
            assert_eq!(rows[1].state, State::Scheduled);
            assert_eq!(rows[1].prog_start, Some(NOW + 3600));
            Ok(())
        })
        .unwrap();
}
