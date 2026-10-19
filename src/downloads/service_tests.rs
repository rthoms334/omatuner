//! Tick-level regressions: real saved state, controlled workers and wall-clock changes (§43).
use super::*;
use crate::model::{ItemKey, TileId};
use crate::ui::testing::TestUi;

fn fixture() -> (TestUi, Engine) {
    let mut cfg = crate::config::Config::default();
    cfg.downloads.reserve_gb = 0;
    let ui = TestUi::with_config(cfg).unwrap();
    let s = ui.services();
    s.store.writer().blocking(|c| {
        c.execute("INSERT INTO source(id,name,kind,url,created_at,generation) VALUES (1,'TV','m3u-url','http://127.0.0.1/list',0,1)", [])?;
        for id in 1..=8 {
            c.execute("INSERT INTO movie(id,source_id,key,name,clean_name,norm_name,stream,provider_order,added_at,seen_gen) VALUES (?1,1,?2,?2,?2,?2,?3,0,0,1)",
                rusqlite::params![id,format!("m{id}"),r#"{"t":"url","url":"http://127.0.0.1/1.mp4"}"#])?;
        }
        for id in 1..=3 {
            c.execute("INSERT INTO channel(id,source_id,key,name,clean_name,norm_name,stream,provider_order,added_at,seen_gen,catchup) VALUES (?1,1,?2,?2,?2,?2,?3,0,0,1,?4)",
                rusqlite::params![id,format!("c{id}"),r#"{"t":"url","url":"http://127.0.0.1/1.ts"}"#,
                (id == 1).then_some(r#"{"kind":"default","template":null,"days":7,"correction_s":0}"#)])?;
        }
        Ok(())
    }).unwrap();
    let lib = Arc::new(crate::store::library::load(&s.store.reader().unwrap()).unwrap());
    drop(s.agent.publish_library(&lib));
    let (snapshot, _) = watch::channel(Arc::default());
    let (command_tx, _) = mpsc::unbounded_channel();
    let engine = Engine {
        deps: Deps {
            store: s.store.clone(),
            config: s.config.clone(),
            shared: s.agent.clone(),
            budget: s.tiles.budget().clone(),
            http: s.http.clone(),
            bus: s.bus.clone(),
            tiles: s.tiles.registry(),
            paths: s.paths.clone(),
        },
        resources: Default::default(),
        arming: None,
        command_tx,
        running: HashMap::new(),
        saved: HashMap::new(),
        visibility: Mutex::new(Visibility::default()),
        paused: false,
        failed: HashMap::new(),
        paused_accounts: HashSet::new(),
        undo: HashMap::new(),
        undo_rules: HashMap::new(),
        moving: None,
        snapshot,
        last_progress: (Instant::now(), 0),
        closing: false,
        maintenance_at: Instant::now(),
    };
    s.rt.block_on(engine.reconcile()).unwrap();
    engine.deps.budget.set_limit(SourceId(1), Some(1));
    (ui, engine)
}

async fn add(e: &mut Engine, kind: ItemKind, key: &str) -> Job {
    let c = e.deps.store.reader().unwrap();
    let target = Target { item: ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) }, programme: None, minutes: Some(30), any_copy: true };
    let mut job = planning::new_job(
        &c,
        &e.deps.shared.library(),
        crate::shared::ParentalView { unlocked: true, hides_explicit: false, pin_active: false },
        &current(&e.deps.config),
        &target,
        false,
        crate::model::now(),
    )
    .unwrap();
    let write = job.clone();
    job.id = e.deps.store.writer().run(move |c| db::add(c, &write)).await.unwrap();
    job.position = job.id.0;
    e.save(job.clone()).await.unwrap();
    job
}

fn row(e: &Engine, id: DownloadId) -> Job {
    db::job(&e.deps.store.reader().unwrap(), id).unwrap().unwrap()
}

async fn running(e: &mut Engine, mut job: Job, result: Option<Result<()>>) {
    job.state = State::Running;
    e.save(job.clone()).await.unwrap();
    let id = job.id;
    let lease = e.deps.budget.acquire(id, job.source_id.unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let kill = cancel.clone();
    let task = tokio::spawn(async move {
        let result = if let Some(result) = result {
            result
        } else {
            kill.cancelled().await;
            Err(Error::Cancelled)
        };
        (job, result)
    });
    e.running.insert(id, Running { cancel, stop: CancellationToken::new(), progress: Arc::default(), task, lease: Some(lease) });
    tokio::task::yield_now().await;
}

async fn complete(e: &Engine) {
    while !e.running.values().all(|r| r.task.is_finished()) {
        tokio::task::yield_now().await;
    }
}

#[test]
fn forbidden_counts_other_holders_and_three_failed_jobs_pause_the_account() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let job = add(&mut e, ItemKind::Movie, "m1").await;
        let id = job.id;
        running(&mut e, job, Some(Err(Error::Http { status: 403, url: String::new() }))).await;
        complete(&e).await;
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(row(&e, id).attempts, 1);
        assert_eq!(row(&e, id).reason.as_deref(), Some("status-403"));
        let job = add(&mut e, ItemKind::Movie, "m2").await;
        let id = job.id;
        running(&mut e, job, Some(Err(Error::Http { status: 403, url: String::new() }))).await;
        e.deps.budget.claim(TileId::A, Some(SourceId(1)));
        complete(&e).await;
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(row(&e, id).attempts, 0);
        assert_eq!(row(&e, id).reason.as_deref(), Some("busy"));
        e.deps.budget.claim(TileId::A, None);
        for key in ["m3", "m4", "m5"] {
            let mut job = add(&mut e, ItemKind::Movie, key).await;
            job.attempts = 3;
            let id = job.id;
            running(&mut e, job, Some(Err(Error::Http { status: 403, url: String::new() }))).await;
            complete(&e).await;
            e.tick_at(crate::model::now(), 0).await.unwrap();
            assert_eq!(row(&e, id).state, State::Failed);
        }
        assert!(e.paused_accounts.contains(&SourceId(1)));
    });
}

#[test]
fn stale_job_does_not_block_start_or_publish_and_clash_clears() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let stale = add(&mut e, ItemKind::Movie, "m1").await;
        let mut good = add(&mut e, ItemKind::Movie, "m2").await;
        good.reason = Some("clash".into());
        good.fp["download_clash_at"] = crate::model::now().into();
        e.save(good.clone()).await.unwrap();
        e.deps
            .store
            .writer()
            .run(|c| {
                c.execute("DELETE FROM movie WHERE key='m1'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(row(&e, stale.id).reason.as_deref(), Some("not in your playlists"));
        assert!(e.running.contains_key(&good.id));
        let started = row(&e, good.id);
        assert!(started.reason.is_none() && started.fp["download_clash_at"].is_null());
        assert!(e.snapshot.borrow().jobs.iter().any(|j| j.id == stale.id && j.state == State::Failed));
        e.command(DownloadCommand::Clash { id: good.id, record: false, alternate: false }).await.unwrap();
        assert_eq!(row(&e, good.id).state, State::Running);
        e.command(DownloadCommand::Shutdown).await.unwrap();
    });
}

#[test]
fn clock_jump_uses_catchup_or_missed_and_promotes_raw_parts() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        e.reconcile().await.unwrap();
        let now = crate::model::now();
        let mut ids = vec![];
        for key in ["c1", "c2", "c3"] {
            let mut job = add(&mut e, ItemKind::Channel, key).await;
            job.state = State::Scheduled;
            job.prog_start = Some(now + 100);
            job.prog_stop = Some(now + 200);
            job.pad_before_s = 0;
            job.pad_after_s = 0;
            if key == "c3" {
                job.bytes_done = 7;
                job.store_name = Some(format!("{}.ts", job.id));
                std::fs::write(e.root().unwrap().join(format!("{}.ts.part", job.id)), b"partial").unwrap();
            }
            ids.push(job.id);
            e.save(job).await.unwrap();
        }
        e.deps.budget.claim(TileId::A, Some(SourceId(1)));
        e.tick_at(now, 0).await.unwrap();
        assert!(ids.iter().all(|id| row(&e, *id).state == State::Scheduled));
        e.tick_at(now + 300, 0).await.unwrap();
        assert_eq!(row(&e, ids[0]).kind, Kind::Catchup);
        assert_eq!(row(&e, ids[1]).state, State::Missed);
        let partial = row(&e, ids[2]);
        assert_eq!(partial.state, State::Partial);
        assert_eq!(std::fs::read(partial.path(&e.root().unwrap()).unwrap()).unwrap(), b"partial");
    });
}

#[test]
fn window_closes_mid_download_and_pause_downloads_preserves_recordings_and_finishing() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let mut cfg = (*current(&e.deps.config)).clone();
        cfg.downloads.window = "01:00-07:00".into();
        *e.deps.config.write().unwrap() = Arc::new(cfg);
        let job = add(&mut e, ItemKind::Movie, "m1").await;
        let id = job.id;
        running(&mut e, job, None).await;
        e.tick_at(crate::model::now(), 419).await.unwrap();
        assert!(e.running.contains_key(&id));
        e.tick_at(crate::model::now(), 420).await.unwrap();
        assert_eq!(row(&e, id).state, State::Paused);
        assert_eq!(row(&e, id).reason.as_deref(), Some("window"));
        assert!(e.deps.budget.admits(TileId::A, SourceId(1)));
        e.tick_at(crate::model::now(), 60).await.unwrap();
        assert!(e.running.contains_key(&id));
        e.stop(id, State::Paused, None).await.unwrap();
        let recording = add(&mut e, ItemKind::Channel, "c2").await;
        let rid = recording.id;
        running(&mut e, recording, None).await;
        let mut finish = add(&mut e, ItemKind::Movie, "m2").await;
        finish.state = State::Finishing;
        let fid = finish.id;
        e.save(finish.clone()).await.unwrap();
        let cancel = CancellationToken::new();
        let kill = cancel.clone();
        e.running.insert(
            fid,
            Running {
                cancel,
                stop: CancellationToken::new(),
                progress: Arc::default(),
                lease: None,
                task: tokio::spawn(async move {
                    kill.cancelled().await;
                    (finish, Err(Error::Cancelled))
                }),
            },
        );
        e.command(DownloadCommand::PauseAll(true)).await.unwrap();
        assert!(e.running.contains_key(&rid) && e.running.contains_key(&fid));
        assert!(crate::config::Config::load(&e.deps.paths.config_file()).unwrap().downloads.paused);
        e.tick_at(crate::model::now(), 420).await.unwrap();
        assert!(e.running.contains_key(&rid) && e.running.contains_key(&fid));
        e.command(DownloadCommand::Shutdown).await.unwrap();
    });
}

#[test]
fn recording_preempts_download_and_skip_cancels_only_pending_jobs() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let job = add(&mut e, ItemKind::Movie, "m1").await;
        let id = job.id;
        running(&mut e, job, None).await;
        let mut record = add(&mut e, ItemKind::Channel, "c2").await;
        record.state = State::Scheduled;
        record.prog_start = Some(crate::model::now());
        record.pad_before_s = 0;
        let rid = record.id;
        e.save(record).await.unwrap();
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(row(&e, id).state, State::Paused);
        assert_eq!(row(&e, id).reason.as_deref(), Some("recording"));
        assert!(e.running.contains_key(&rid));
        let mut skipped = add(&mut e, ItemKind::Channel, "c3").await;
        skipped.state = State::Scheduled;
        let sid = skipped.id;
        e.save(skipped).await.unwrap();
        e.command(DownloadCommand::Clash { id: sid, record: false, alternate: false }).await.unwrap();
        assert_eq!(row(&e, sid).state, State::Cancelled);
        assert_eq!(row(&e, sid).reason.as_deref(), Some("skipped"));
        e.command(DownloadCommand::Shutdown).await.unwrap();
    });
}

#[test]
fn unchanged_progress_and_blocked_reason_do_not_write_again() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let job = add(&mut e, ItemKind::Movie, "m1").await;
        e.deps.budget.claim(TileId::A, Some(SourceId(1)));
        e.tick_at(crate::model::now(), 0).await.unwrap();
        let before = e.deps.store.writer().run(|c| Ok(c.total_changes())).await.unwrap();
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(e.deps.store.writer().run(|c| Ok(c.total_changes())).await.unwrap(), before);
        e.deps.budget.claim(TileId::A, None);
        let job = row(&e, job.id);
        running(&mut e, job, None).await;
        e.tick_at(crate::model::now(), 0).await.unwrap();
        let before = e.deps.store.writer().run(|c| Ok(c.total_changes())).await.unwrap();
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert_eq!(e.deps.store.writer().run(|c| Ok(c.total_changes())).await.unwrap(), before);
        e.command(DownloadCommand::Shutdown).await.unwrap();
    });
}

#[test]
fn folder_copy_keeps_ticking_and_waits_for_recording_before_switching() {
    let (ui, mut e) = fixture();
    ui.services().rt.block_on(async {
        let old = e.root().unwrap();
        std::fs::write(old.join("kept"), b"old bytes").unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = target.path().join("moved");
        let (gate, wait) = oneshot::channel();
        e.moving = Some(FolderMove {
            from: old.clone(),
            to: path.clone(),
            task: Some(tokio::spawn(async move {
                wait.await.map_err(Error::other)?;
                Ok(())
            })),
            dirty: false,
            reply: None,
        });
        let mut recording = add(&mut e, ItemKind::Channel, "c2").await;
        recording.prog_start = Some(crate::model::now());
        recording.pad_before_s = 0;
        let id = recording.id;
        e.save(recording).await.unwrap();
        e.tick_at(crate::model::now(), 0).await.unwrap();
        assert!(e.running.contains_key(&id), "copying must not freeze due recordings");
        assert_eq!(e.root().unwrap(), old);
        gate.send(()).unwrap();
        e.stop(id, State::Paused, None).await.unwrap();
        for _ in 0..100 {
            e.finish_move().await;
            if e.moving.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(e.moving.is_none());
        assert_eq!(e.root().unwrap(), path);
        assert_eq!(std::fs::read(path.join("kept")).unwrap(), b"old bytes");
    });
}

fn episode_fixture() -> (TestUi, Engine) {
    let (ui, e) = fixture();
    e.deps.store.writer().blocking(super::super::test_support::seed).unwrap();
    let lib = Arc::new(crate::store::library::load(&e.deps.store.reader().unwrap()).unwrap());
    drop(e.deps.shared.publish_library(&lib));
    (ui, e)
}

async fn part(e: &mut Engine, pinned: bool) -> Job {
    let mut job = super::super::test_support::job(&e.deps.store.reader().unwrap(), 1, 2, 7);
    job.any_copy = !pinned;
    job.attempts = 3;
    job.bytes_done = 71;
    job.bytes_total = Some(100);
    job.etag = Some("same-file".into());
    job.fp["download_validators"] = serde_json::json!({"accepts_ranges": true, "etag": "same-file", "last_modified": null, "length": 100});
    let write = job.clone();
    job.id = e.deps.store.writer().run(move |c| db::add(c, &write)).await.unwrap();
    job.store_name = Some(format!("{}.mp4", job.id));
    std::fs::write(e.root().unwrap().join(format!("{}.mp4.part", job.id)), [0; 71]).unwrap();
    e.save(job.clone()).await.unwrap();
    job
}

#[test]
fn exhausted_episode_retries_switch_to_a_copy_with_the_episode_and_discard_a_different_part() {
    let (ui, mut e) = episode_fixture();
    ui.services().rt.block_on(async {
        let job = part(&mut e, false).await;
        let id = job.id;
        e.paused = true;
        running(&mut e, job, Some(Err(Error::other("connection failed")))).await;
        complete(&e).await;
        e.tick_at(crate::model::now(), 0).await.unwrap();
        let switched = row(&e, id);
        assert_eq!(switched.source_id, Some(SourceId(3)), "the earlier copy on source 2 lacks S2E7");
        assert_eq!(switched.state, State::Waiting);
        assert_eq!(switched.attempts, 0);
        assert_eq!(switched.bytes_done, 0);
        assert!(switched.bytes_total.is_none() && switched.etag.is_none());
        assert!(!e.root().unwrap().join(format!("{id}.mp4.part")).exists());
        assert!(crate::ui::downloads::state_text(&switched).contains("Switched to TV 2"));
        let cfg = current(&e.deps.config);
        let c = e.deps.store.reader().unwrap();
        assert_eq!(
            planning::pick(&c, &e.deps.shared.library(), &cfg, &e.deps.budget, &switched, |_| false).unwrap(),
            switched.item(),
            "starting must retain the replacement"
        );
        let mut failed_again = switched;
        assert!(!e.recover(&mut failed_again).unwrap(), "do not cycle back to the failing copy or pick the missing episode");
    });
}

#[test]
fn pinned_episode_keeps_its_playlist_and_part_after_exhausted_retries() {
    let (ui, mut e) = episode_fixture();
    ui.services().rt.block_on(async {
        let job = part(&mut e, true).await;
        let id = job.id;
        e.paused = true;
        running(&mut e, job, Some(Err(Error::other("connection failed")))).await;
        complete(&e).await;
        e.tick_at(crate::model::now(), 0).await.unwrap();
        let failed = row(&e, id);
        assert_eq!(failed.source_id, Some(SourceId(1)));
        assert_eq!(failed.state, State::Failed);
        assert_eq!(failed.bytes_done, 71);
        assert!(e.root().unwrap().join(format!("{id}.mp4.part")).exists());
    });
}

#[test]
fn waiting_episode_leaves_a_failed_account_but_pinned_jobs_wait() {
    for pinned in [false, true] {
        let (ui, mut e) = episode_fixture();
        ui.services().rt.block_on(async {
            let job = part(&mut e, pinned).await;
            let id = job.id;
            for _ in 0..3 {
                e.failed_job(SourceId(1));
            }
            let mut cfg = (*current(&e.deps.config)).clone();
            cfg.downloads.window = "01:00-07:00".into();
            e.pending(job, &cfg, crate::model::now(), 0, &mut 0, u64::MAX).await.unwrap();
            assert_eq!(row(&e, id).source_id, Some(SourceId(if pinned { 1 } else { 3 })));
            assert!(e.running.is_empty(), "the closed window still applies");
        });
    }
}

#[test]
fn switching_an_identical_resource_preserves_the_part_and_resume_validators() {
    let (ui, mut e) = episode_fixture();
    ui.services().rt.block_on(async {
        e.deps
            .store
            .writer()
            .run(|c| {
                c.execute(
                    "UPDATE episode SET stream=(SELECT stream FROM episode WHERE source_id=1 AND key='e2-7') WHERE source_id=3 AND key='e2-7'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let mut job = part(&mut e, false).await;
        assert!(e.recover(&mut job).unwrap());
        assert_eq!(job.source_id, Some(SourceId(3)));
        assert_eq!(job.bytes_done, 71);
        assert_eq!(job.fp["download_validators"]["etag"], "same-file");
        assert_eq!(std::fs::metadata(e.root().unwrap().join(format!("{}.mp4.part", job.id))).unwrap().len(), 71);
    });
}

#[test]
fn new_episode_job_skips_a_free_copy_without_the_episode() {
    let (ui, e) = episode_fixture();
    let c = e.deps.store.reader().unwrap();
    let job = super::super::test_support::job(&c, 1, 2, 7);
    e.deps.budget.claim(TileId::A, Some(SourceId(1)));
    let picked = planning::pick(&c, &e.deps.shared.library(), &current(&e.deps.config), &e.deps.budget, &job, |_| false).unwrap().unwrap();
    assert_eq!(picked.source, SourceId(3));
    let media = crate::store::vod::resolve_episode(&c, &picked).unwrap().unwrap();
    let episode = crate::store::vod::episode(&c, media).unwrap();
    assert_eq!((episode.season, episode.episode), (2, 7));
    drop(ui);
}
