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
        failed: Mutex::new(false),
        retry: Notify::new(),
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

#[tokio::test]
async fn recorded_view_reload_does_not_wait_for_publication() {
    let state = Arc::new(Shared {
        store: Mutex::new(Store::memory(20.0)),
        changed: Notify::new(),
        token: Token::mint(),
        audio: Mutex::new(false),
        view: Mutex::new(View { from: Some(0.0), targeted: true, ..View::default() }),
        errors: crossbeam_channel::unbounded().0,
        failed: Mutex::new(false),
        retry: Notify::new(),
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
        failed: Mutex::new(false),
        retry: Notify::new(),
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

fn test_buffer(store: Store) -> Buffer {
    Buffer {
        shared: Arc::new(Shared {
            store: Mutex::new(store),
            changed: Notify::new(),
            token: Token::mint(),
            audio: Mutex::new(false),
            view: Mutex::new(View::default()),
            errors: crossbeam_channel::unbounded().0,
            failed: Mutex::new(false),
            retry: Notify::new(),
        }),
        url: Secret::new("http://127.0.0.1/unused/0.m3u8".into()),
        stop: CancellationToken::new(),
        thread: None,
        errors: crossbeam_channel::unbounded().1,
    }
}

fn append_segment(s: &mut Store, duration: f64, disc: u64) {
    s.prepare(0, duration);
    let id = s.insert(Resource { uri: format!("https://fixture/{}", s.next), range: None }, Bytes::from_static(b"packets"), "ts").unwrap();
    let t = &mut s.tracks[0];
    t.segments.push_back(store::Segment { id, seq: t.next_seq, start: t.edge, end: t.edge + duration, disc, key: None, map: None });
    t.edge += duration;
    t.next_seq += 1;
    t.target = duration;
    t.ready = true;
}

#[tokio::test]
async fn targeted_reload_waits_at_the_edge_but_not_while_reading_history() {
    let mut store = Store::memory(20.0);
    for _ in 0..3 {
        append_segment(&mut store, 2.0, 0);
    }
    let buffer = test_buffer(store);
    buffer.view(Some(0.5));
    let state = buffer.shared.clone();
    let request = |name| Request::builder().uri(format!("/{}/{name}", state.token.expose())).body(Body::empty()).unwrap();
    assert_eq!(serve(State(state.clone()), request("0.m3u8?view=1")).await.status(), StatusCode::OK);
    state.view.lock().unwrap().opened = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(serve(State(state.clone()), request("0.ts")).await.status(), StatusCode::OK);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), serve(State(state.clone()), request("0.m3u8?view=1"))).await.unwrap().status(),
        StatusCode::OK,
        "historical playback must not wait for the provider"
    );
    assert_eq!(serve(State(state.clone()), request("2.ts")).await.status(), StatusCode::OK);
    let mut waiting = tokio::spawn(serve(State(state.clone()), request("0.m3u8?view=1")));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut waiting).await.is_err(), "do not send a stale playlist at the edge");
    append_segment(&mut state.store(), 2.0, 0);
    state.changed.notify_waiters();
    let response = tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(std::str::from_utf8(&bytes).unwrap().contains("3.ts"), "the waiting reload receives the newly published segment");
}

#[test]
fn live_seek_headroom_stays_in_the_current_epoch_and_retained_window() {
    let dir = tempfile::tempdir().unwrap();
    let buffer = test_buffer(Store::new(dir.path().into(), 30.0));
    assert_eq!(buffer.live_target(), None);
    append_segment(&mut buffer.shared.store(), 6.0, 0);
    assert_eq!(buffer.live_target(), Some(0.0), "first tune needs only one segment");
    append_segment(&mut buffer.shared.store(), 6.0, 0);
    assert_eq!(buffer.live_target(), Some(5.0), "seven seconds of packets before the six-second refresh");
    append_segment(&mut buffer.shared.store(), 6.0, 1);
    assert_eq!(buffer.live_target(), Some(12.0), "never replay the old timestamp epoch");
    for _ in 0..6 {
        append_segment(&mut buffer.shared.store(), 6.0, 1);
    }
    let range = buffer.snapshot().range.unwrap();
    let target = buffer.live_target().unwrap();
    assert!(target >= range.start && !range.behind(target));
    assert_eq!(target, range.end - 1.0);
}

#[test]
fn reconnect_after_forty_minutes_and_discontinuity_starts_at_live() {
    // One second per minute: retain 30 of a 40-minute recording with a timestamp reset.
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::new(dir.path().into(), 30.0);
    for minute in 0..40 {
        append_segment(&mut s, 1.0, u64::from(minute >= 20));
    }
    assert!(s.playlist(0, None).unwrap().ends_with("#EXT-X-ENDLIST\n"));
    let b = test_buffer(s);
    b.view(None);
    assert_eq!(b.snapshot().origin, 39.0);
    let playlist = b.shared.store().playlist(0, b.shared.view.lock().unwrap().from).unwrap();
    assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:39\n"));
    assert!(!playlist.contains("#EXT-X-ENDLIST"));
    b.view(Some(15.25));
    assert_eq!(b.snapshot().origin, 15.0, "rewound reconnect retains its position");
    assert_eq!(b.snapshot().next_view, Some(20.0));
}

#[test]
fn realistic_segments_have_a_full_live_segment_and_a_stable_badge() {
    for duration in [6.0, 10.0] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(dir.path().into(), 60.0);
        for _ in 0..10 {
            append_segment(&mut store, duration, 0);
            let r = store.range().unwrap();
            assert_eq!(store.tracks[0].edge - r.end, duration);
            // Position advances through the newest segment while its publication is static.
            for tick in 0..=(duration * 10.0) as u32 {
                assert_eq!(r.label(r.end + f64::from(tick) / 10.0), "Live");
            }
            assert!(r.behind(r.end - duration - 1.0));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn outage_reload_returns_the_unchanged_playlist_with_200() {
    let b = test_buffer(Store::memory(60.0));
    append_segment(&mut b.shared.store(), 10.0, 0);
    b.view(None);
    let request = || Request::builder().uri(format!("/{}/0.m3u8?view=1", b.shared.token.expose())).body(Body::empty()).unwrap();
    let first = serve(State(b.shared.clone()), request()).await;
    let first = axum::body::to_bytes(first.into_body(), 4096).await.unwrap();
    b.shared.view.lock().unwrap().opened = Some(Instant::now() - Duration::from_secs(1));
    let next = serve(State(b.shared.clone()), request()).await;
    assert_eq!(next.status(), StatusCode::OK);
    assert_eq!(axum::body::to_bytes(next.into_body(), 4096).await.unwrap(), first);
}

#[test]
fn sequence_regression_needs_confirmation_or_more_than_a_window() {
    let make =
        || Source { url: String::new(), initial: None, last: Some(103), first: Some(100), discontinuity: Some(0), gap: false, regressed: false };
    let mut s = make();
    assert!(s.regression(99, 102, 4));
    assert_eq!(s.last, Some(103));
    assert!(!s.regression(101, 104, 4));
    assert_eq!(s.last, Some(103), "a single stale playlist never resets");
    assert!(s.regression(99, 102, 4));
    assert!(!s.regression(99, 102, 4));
    assert_eq!(s.last, None, "two regressed refreshes confirm a restart");
    let mut s = make();
    assert!(!s.regression(0, 3, 4));
    assert_eq!(s.last, None, "a large regression resets immediately");
}

#[test]
fn playlist_sniff_preserves_provider_messages_and_rejects_binary_immediately() {
    crate::redact::register_secret("hls-sniff-secret");
    let text = playlist_head(b"<html>Max connections reached: hls-sniff-secret</html>").unwrap_err().unwrap();
    assert!(text.starts_with("Max connections reached:"));
    assert!(!text.contains("hls-sniff-secret"));
    assert_eq!(playlist_head(b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n"), Ok(()));
    let mut ts = [0; 512];
    ts[0] = 0x47;
    ts[188] = 0x47;
    assert_eq!(playlist_head(&ts), Err(None), "binary takes the mpv cache path after just 512 bytes");
}
