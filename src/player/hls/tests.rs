use super::*;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

#[allow(dead_code)]
#[path = "../../../tests/support/media.rs"]
mod media;

/// Same store and playlist renderer as the HTTP recorder, using the filesystem solely to
/// make the decoder/seek proof runnable where socket(2) is prohibited. Not an HTTP zap gate.
#[test]
#[ignore = "65 second real-mpv filesystem proof; run explicitly"]
fn rolling_playlist_real_mpv() {
    let fixture = media::ensure_rolling_media();
    let dir = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(Store::new(dir.path().to_owned(), 20.0)));
    let stopped = Arc::new(AtomicBool::new(false));
    let view_from = Arc::new(Mutex::new(None));
    let writer = {
        let shared = shared.clone();
        let stopped = stopped.clone();
        let view_from = view_from.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut next = 0;
            while !stopped.load(Ordering::Relaxed) {
                let mut s = shared.lock().unwrap();
                let end = started.elapsed().as_secs() / 2 + 1;
                while next < end {
                    s.prepare(0, 2.0);
                    let bytes = fs::read(fixture.join(format!("rolling/seg{next:03}.ts"))).unwrap();
                    let id = s.insert(Resource { uri: format!("http://fixture/{next}"), range: None }, bytes.into(), "ts").unwrap();
                    let t = &mut s.tracks[0];
                    t.segments.push_back(store::Segment { id, seq: next, start: t.edge, end: t.edge + 2.0, disc: 0, key: None, map: None });
                    t.edge += 2.0;
                    t.target = 2.0;
                    next += 1;
                }
                assert!(s.blobs.len() <= 10);
                let playlist = s.playlist(0, None).unwrap();
                fs::write(s.dir.join("next.m3u8"), playlist).unwrap();
                fs::rename(s.dir.join("next.m3u8"), s.dir.join("index.m3u8")).unwrap();
                if let Some(from) = *view_from.lock().unwrap() {
                    fs::write(s.dir.join("next.m3u8"), s.playlist(0, Some(from)).unwrap()).unwrap();
                    fs::rename(s.dir.join("next.m3u8"), s.dir.join("view.m3u8")).unwrap();
                }
                drop(s);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    struct WriterGuard(Arc<AtomicBool>, Option<JoinHandle<()>>);
    impl Drop for WriterGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
            let _ = self.1.take().unwrap().join();
        }
    }
    let guard = WriterGuard(stopped, Some(writer));
    let mpv = libmpv2::Mpv::with_initializer(|m| {
        for (k, v) in [
            ("config", "no"),
            ("terminal", "no"),
            ("vo", "null"),
            ("ao", "null"),
            ("hwdec", "no"),
            ("cache", "yes"),
            ("cache-on-disk", "no"),
            ("cache-secs", "1"),
            ("demuxer-readahead-secs", "1"),
            ("demuxer-max-back-bytes", "0"),
            ("cache-pause-initial", "no"),
            ("force-seekable", "yes"),
        ] {
            m.set_option(k, v)?;
        }
        Ok(())
    })
    .unwrap();
    let started = Instant::now();
    while !dir.path().join("index.m3u8").exists() {
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    mpv.command("loadfile", &[dir.path().join("index.m3u8").to_str().unwrap(), "replace"]).unwrap();
    while started.elapsed() < Duration::from_secs(64) {
        let _ = mpv.wait_event(0.1);
    }
    let before = mpv.get_property::<f64>("time-pos").unwrap();
    let range = shared.lock().unwrap().range().unwrap();
    assert!(range.start >= 44.0 && before >= 62.0, "window {range:?}, position {before}");
    let target = before - 15.0;
    assert!(target >= range.start, "seek must be inside the recorded window");
    let origin = shared.lock().unwrap().tracks[0].segments.iter().find(|seg| seg.end > target).unwrap().start;
    *view_from.lock().unwrap() = Some(origin);
    while !dir.path().join("view.m3u8").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    while mpv.wait_event(0.0).is_some() {}
    let opts = format!(
        "{MPV_OPTIONS},demuxer-lavf-o=live_start_index=0,start={},cache-secs=1,demuxer-readahead-secs=1,demuxer-max-back-bytes=0",
        target - origin
    );
    mpv.command("loadfile", &[dir.path().join("view.m3u8").to_str().unwrap(), "replace", "-1", &opts]).unwrap();
    let start = Instant::now();
    loop {
        let event = mpv.wait_event(0.01);
        let pos = mpv.get_property::<f64>("time-pos").unwrap_or(-1.0) + origin;
        if matches!(event, Some(Ok(libmpv2::events::Event::PlaybackRestart))) && (pos - target).abs() < 0.6 {
            assert!(mpv.get_property::<String>("video-frame-info/picture-type").is_ok());
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(8), "seek target {target}, actual {pos}");
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(mpv.get_property::<f64>("time-pos").unwrap() + origin > target + 0.2);
    eprintln!(
        "mpv {}: at {before:.2}s, seek back to {target:.2}s decoded video; oldest {:.2}s, {} files",
        mpv.get_property::<String>("mpv-version").unwrap(),
        range.start,
        shared.lock().unwrap().blobs.len()
    );
    drop(mpv);
    drop(guard);
}

#[test]
fn raw_ts_and_non_live_hls_do_not_enter_the_recorder() {
    use crate::model::*;
    let mut p = Playable {
        media: MediaRef::Channel(ChannelId(1)),
        kind: PlayableKind::Live,
        item: None,
        source_id: None,
        title: String::new(),
        subtitle: None,
        logo: None,
        number: None,
        url: Secret::new("https://example.test/stream.ts?token=secret".into()),
        headers: HttpHeaders::default(),
        start_at: None,
        duration_hint: None,
    };
    assert!(!eligible(&p));
    p.url = Secret::new("https://example.test/stream.m3u8?token=secret".into());
    assert!(eligible(&p));
    p.kind = PlayableKind::Movie;
    assert!(!eligible(&p));
}

#[test]
fn low_disk_is_a_one_way_switch_and_unlinks_recording() {
    let dir = tempfile::tempdir().unwrap();
    let state = Shared {
        store: Mutex::new(Store::new(dir.path().to_owned(), 20.0)),
        changed: Notify::new(),
        token: Token::mint(),
        audio: Mutex::new(false),
        view: Mutex::new(View::default()),
        errors: crossbeam_channel::unbounded().0,
    };
    {
        let mut s = state.store();
        let id = s.insert(Resource { uri: "https://fixture/segment".into(), range: None }, Bytes::from_static(b"packets"), "ts").unwrap();
        s.tracks[0].segments.push_back(store::Segment { id, seq: 0, start: 0.0, end: 2.0, disc: 0, key: None, map: None });
    }
    check_space(&state, Some(RESERVE));
    assert!(!state.store().memory);
    check_space(&state, Some(RESERVE - 1));
    assert!(state.store().memory);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    check_space(&state, Some(RESERVE * 2));
    assert!(state.store().memory, "freeing space never restarts disk within the channel");
    assert_eq!(state.store().blobs[&0].data.as_deref(), Some(b"packets".as_slice()));
}

#[test]
#[ignore = "real-mpv local seek views, discontinuity and pause; run explicitly"]
fn local_view_real_mpv() {
    let fixture = media::ensure_rolling_media();
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::new(dir.path().to_owned(), 20.0);
    // Real TS timestamps reset between the fifth and sixth segment. Seeking opens a
    // view at the desired segment, so it does not depend on FFmpeg seeking across that reset.
    for (seq, source) in (25..30).chain(0..5).enumerate() {
        store.prepare(0, 2.0);
        let id = store
            .insert(
                Resource { uri: format!("http://fixture/{seq}"), range: None },
                fs::read(fixture.join(format!("rolling/seg{source:03}.ts"))).unwrap().into(),
                "ts",
            )
            .unwrap();
        let track = &mut store.tracks[0];
        track.segments.push_back(store::Segment {
            id,
            seq: seq as u64,
            start: track.edge,
            end: track.edge + 2.0,
            disc: u64::from(seq >= 5),
            key: None,
            map: None,
        });
        track.edge += 2.0;
    }
    let mpv = libmpv2::Mpv::with_initializer(|m| {
        for (key, value) in [("config", "no"), ("terminal", "no"), ("vo", "null"), ("ao", "null"), ("hwdec", "no"), ("cache-pause-initial", "no")] {
            m.set_option(key, value)?;
        }
        Ok(())
    })
    .unwrap();
    for (target, paused) in [(7.3_f64, false), (3.3, false), (15.3, false), (5.5, true)] {
        while mpv.wait_event(0.0).is_some() {}
        mpv.set_property("pause", paused).unwrap();
        let origin = store.tracks[0].segments.iter().find(|seg| seg.end > target).unwrap().start;
        fs::write(dir.path().join("view.m3u8"), store.playlist(0, Some(origin)).unwrap()).unwrap();
        let options = format!(
            "{MPV_OPTIONS},demuxer-lavf-o=live_start_index=0,start={},cache-secs=1,demuxer-readahead-secs=1,demuxer-max-back-bytes=0",
            target - origin
        );
        mpv.command("loadfile", &[dir.path().join("view.m3u8").to_str().unwrap(), "replace", "-1", &options]).unwrap();
        let started = Instant::now();
        loop {
            let restarted = matches!(mpv.wait_event(0.01), Some(Ok(libmpv2::events::Event::PlaybackRestart)));
            let pos = mpv.get_property::<f64>("time-pos").unwrap_or(-1.0) + origin;
            if restarted && (pos - target).abs() < 0.15 {
                assert!(mpv.get_property::<String>("video-frame-info/picture-type").is_ok());
                assert_eq!(mpv.get_property::<bool>("pause").unwrap(), paused);
                eprintln!("local seek view decoded {pos:.3}s (requested {target:.3}s, paused={paused})");
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(8), "target {target}, actual {pos}");
        }
        if target == 7.3 {
            assert_eq!(store.boundary(origin), Some(10.0));
            assert!(store.playlist(0, Some(origin)).unwrap().ends_with("#EXT-X-ENDLIST\n"));
            let began = Instant::now();
            loop {
                if matches!(mpv.wait_event(0.1), Some(Ok(libmpv2::events::Event::EndFile(_)))) {
                    break;
                }
                assert!(began.elapsed() < Duration::from_secs(6), "local view must finish before the timestamp reset");
            }
            // The controller performs this local handoff on EOF (or its preceding tick).
            fs::write(dir.path().join("view.m3u8"), store.playlist(0, Some(10.0)).unwrap()).unwrap();
            let options = format!("{MPV_OPTIONS},demuxer-lavf-o=live_start_index=0,start=0");
            mpv.command("loadfile", &[dir.path().join("view.m3u8").to_str().unwrap(), "replace", "-1", &options]).unwrap();
            let began = Instant::now();
            loop {
                let restarted = matches!(mpv.wait_event(0.01), Some(Ok(libmpv2::events::Event::PlaybackRestart)));
                if restarted {
                    assert!(mpv.get_property::<f64>("time-pos").unwrap() < 0.2);
                    break;
                }
                assert!(began.elapsed() < Duration::from_secs(6));
            }
        }
    }
}

#[test]
fn unavailable_directory_keeps_the_single_fetcher_in_memory() {
    let mut store = Store::memory(20.0);
    let id = store.insert(Resource { uri: "https://fixture/segment".into(), range: None }, Bytes::from_static(b"packets"), "ts").unwrap();
    assert!(store.memory && store.dir.as_os_str().is_empty());
    assert_eq!(store.blobs[&id].data.as_deref(), Some(b"packets".as_slice()));
}

#[tokio::test]
async fn recorded_view_reload_does_not_wait_for_publication() {
    let state = Arc::new(Shared {
        store: Mutex::new(Store::memory(20.0)),
        changed: Notify::new(),
        token: Token::mint(),
        audio: Mutex::new(false),
        view: Mutex::new(View { from: Some(0.0), ..View::default() }),
        errors: crossbeam_channel::unbounded().0,
    });
    {
        let mut s = state.store();
        for seq in 0..3 {
            let id = s.insert(Resource { uri: format!("https://fixture/{seq}"), range: None }, Bytes::from_static(b"packets"), "ts").unwrap();
            let t = &mut s.tracks[0];
            t.segments.push_back(store::Segment { id, seq, start: t.edge, end: t.edge + 2.0, disc: 0, key: None, map: None });
            t.edge += 2.0;
            t.next_seq += 1;
            t.ready = true;
        }
    }
    let request = || Request::builder().uri(format!("/{}/0.m3u8?view=0", state.token.expose())).body(Body::empty()).unwrap();
    assert_eq!(serve(State(state.clone()), request()).await.status(), StatusCode::OK);
    // FFmpeg may refresh before opening an already-listed segment. Elapsed time since
    // opening the view says nothing about whether it has exhausted the recording.
    state.view.lock().unwrap().opened = Some(Instant::now() - Duration::from_secs(1));
    for _ in 0..2 {
        let r = tokio::time::timeout(Duration::from_millis(100), serve(State(state.clone()), request()))
            .await
            .expect("recorded playlists must reload without another provider publication");
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let playlist = std::str::from_utf8(&body).unwrap();
        assert_eq!(playlist.matches("#EXTINF:").count(), 3);
        assert!(!playlist.contains("#EXT-X-ENDLIST"), "the view must remain live");
    }
}

#[tokio::test]
async fn live_reload_waits_for_publication_and_releases_replaced_reader() {
    let state = Arc::new(Shared {
        store: Mutex::new(Store::memory(20.0)),
        changed: Notify::new(),
        token: Token::mint(),
        audio: Mutex::new(false),
        view: Mutex::new(View::default()),
        errors: crossbeam_channel::unbounded().0,
    });
    {
        let mut s = state.store();
        let id = s.insert(Resource { uri: "https://fixture/segment".into(), range: None }, Bytes::from_static(b"packets"), "ts").unwrap();
        let t = &mut s.tracks[0];
        t.segments.push_back(store::Segment { id, seq: 0, start: 0.0, end: 2.0, disc: 0, key: None, map: None });
        t.edge = 2.0;
        t.next_seq = 1;
        t.ready = true;
    }
    let request = || Request::builder().uri(format!("/{}/0.m3u8?view=0", state.token.expose())).body(Body::empty()).unwrap();
    for _ in 0..2 {
        assert_eq!(serve(State(state.clone()), request()).await.status(), StatusCode::OK);
    }
    state.view.lock().unwrap().opened = Some(Instant::now() - Duration::from_secs(1));
    let mut waiting = tokio::spawn(serve(State(state.clone()), request()));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut waiting).await.is_err());
    state.store().tracks[0].next_seq += 1;
    state.changed.notify_waiters();
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap().status(), StatusCode::OK);

    let mut waiting = tokio::spawn(serve(State(state.clone()), request()));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut waiting).await.is_err());
    let buffer = Buffer {
        shared: state.clone(),
        url: Secret::new("http://127.0.0.1/unused/0.m3u8".into()),
        stop: CancellationToken::new(),
        thread: None,
        errors: crossbeam_channel::unbounded().1,
    };
    buffer.view(Some(0.5));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap().status(), StatusCode::NOT_FOUND);
}
