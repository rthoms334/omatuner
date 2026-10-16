//! DESIGN §43 §8.1–§8.2: real service, HTTP sockets and account handoff. Localhost binding
//! is intentionally required; run outside the editing sandbox when it denies sockets.
mod support;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tuner::downloads::{DownloadCommand, State, Target};
use tuner::model::*;
use tuner::player::budget::BudgetExceeded;
use tuner::ui::testing::TestUi;

#[test]
fn pause_acknowledges_transport_close_then_play_and_automatic_resume_then_cancel_unlinks() {
    let mut cfg = tuner::config::Config::default();
    cfg.downloads.reserve_gb = 0;
    cfg.downloads.max_mbps = 0.1;
    let t = TestUi::with_config(cfg).unwrap();
    let s = t.services();
    s.rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("outside-sandbox run: localhost sockets required");
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let budget = s.tiles.budget().clone();
        let server_budget = budget.clone();
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_server, count, seen) = (stop.clone(), active.clone(), requests.clone());
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = tokio::select! { ()=stop_server.cancelled()=>break, r=listener.accept()=>r.unwrap() };
                let count = count.clone();
                let seen = seen.clone();
                let stop = stop_server.clone();
                let budget = server_budget.clone();
                let closed = closed_tx.clone();
                tasks.spawn(async move {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let b = stream.read_u8().await.unwrap();
                        request.push(b);
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let offset = request
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes=").and_then(|s| s.strip_suffix('-')).and_then(|s| s.parse::<usize>().ok()))
                        .unwrap_or(0);
                    const SIZE: usize = 2_000_000;
                    let headers = if offset > 0 {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {offset}-{}/{}\r\n", SIZE - 1, SIZE)
                    } else {
                        "HTTP/1.1 200 OK\r\n".into()
                    };
                    stream
                        .write_all(
                            format!("{headers}Content-Length: {}\r\nAccept-Ranges: bytes\r\nETag: v1\r\nConnection: close\r\n\r\n", SIZE - offset)
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    count.fetch_add(1, Ordering::SeqCst);
                    seen.fetch_add(1, Ordering::SeqCst);
                    let (mut read, mut write) = stream.into_split();
                    let body = async {
                        let mut sent = offset;
                        while sent < SIZE {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            let n = (SIZE - sent).min(16384);
                            if write.write_all(&vec![b'x'; n]).await.is_err() {
                                break;
                            }
                            sent += n;
                        }
                    };
                    let mut one = [0u8; 1];
                    tokio::select! { ()=stop.cancelled()=>{}, _=read.read(&mut one)=>{}, ()=body=>{} }
                    let held = budget.download_holder(SourceId(1));
                    let _ = closed.send((Instant::now(), held));
                    count.fetch_sub(1, Ordering::SeqCst);
                });
            }
            while tasks.join_next().await.is_some() {}
        });
        let item = s
            .store
            .writer()
            .run(move |c| {
                let source = support::seed::seed_source(c, "TV")?;
                let movie = support::seed::seed_movies(c, source, 1)?[0];
                c.execute(
                    "UPDATE movie SET stream=?2 WHERE id=?1",
                    rusqlite::params![movie, serde_json::json!({"t":"url","url":format!("http://{address}/1.mp4")}).to_string()],
                )?;
                let m = tuner::store::vod::movie(c, movie)?;
                Ok(ItemRef { source, kind: ItemKind::Movie, key: m.key })
            })
            .await
            .unwrap();
        let lib = Arc::new(tuner::store::library::load(&s.store.reader().unwrap()).unwrap());
        drop(s.agent.publish_library(&lib));
        budget.set_limit(item.source, Some(1));
        let handle = tuner::downloads::service::spawn(
            s.rt.handle(),
            tuner::downloads::service::Deps {
                store: s.store.clone(),
                config: s.config.clone(),
                shared: s.agent.clone(),
                budget: budget.clone(),
                http: s.http.clone(),
                bus: s.bus.clone(),
                tiles: s.tiles.registry(),
                paths: s.paths.clone(),
            },
        );
        s.agent.set_downloads(Some(handle.clone()));
        let added = handle
            .request(DownloadCommand::Add {
                targets: vec![Target { item: item.clone(), programme: None, minutes: None, any_copy: false }],
                agent: false,
            })
            .await;
        assert_eq!(added.added.len(), 1, "{added:?}");
        let id = added.added[0];
        let mut snapshot = handle.subscribe();
        tokio::time::timeout(Duration::from_secs(8), snapshot.wait_for(|s| s.jobs.iter().any(|j| j.id == id && j.bytes_done > 0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(budget.acquire(TileId::A, item.source), Err(BudgetExceeded::Download { id }));
        let other = budget.acquire(TileId::B, SourceId(999)).unwrap();
        budget.claim(TileId::A, Some(item.source));
        assert!(handle.request(DownloadCommand::PauseForPlay(item.source)).await.error.is_none());
        let acknowledged = Instant::now();
        assert_eq!(active.load(Ordering::SeqCst), 0, "PauseForPlay returned with an open provider socket");
        let (closed_at, lease_held) = closed_rx.try_recv().expect("server observed closure before the pause acknowledgement");
        assert!(closed_at <= acknowledged && lease_held == Some(id), "the socket must close before releasing the lease");
        assert_eq!(budget.download_holder(item.source), None);
        let play = budget.acquire(TileId::A, item.source).unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(tuner::store::downloads::job(&s.store.reader().unwrap(), id).unwrap().unwrap().state, State::Paused);
        budget.release(TileId::A, play, None);
        budget.claim(TileId::A, None);
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("download resumed without a command");
        assert!(handle.request(DownloadCommand::Cancel(vec![id])).await.error.is_none());
        let root = tuner::store::downloads::root(&s.store.reader().unwrap(), &s.config().downloads).unwrap();
        assert!(!root.join(format!("{id}.mp4.part")).exists());
        assert_eq!(budget.download_holder(item.source), None);
        assert!(budget.usage().iter().any(|u| u.source == SourceId(999) && u.in_use == 1));
        budget.release(TileId::B, other, None);
        handle.request(DownloadCommand::Shutdown).await;
        stop.cancel();
        server.await.unwrap();
    });
}

/// The actor applies the tiny download cap, but raw recordings keep taking every chunk.
#[test]
fn recording_ignores_rate_cap_and_delete_undo_keeps_partial_bytes() {
    let mut cfg = tuner::config::Config::default();
    cfg.downloads.reserve_gb = 0;
    cfg.downloads.max_mbps = 0.000001;
    let t = TestUi::with_config(cfg).unwrap();
    let s = t.services();
    s.rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("outside-sandbox run: localhost sockets required");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n").await.unwrap();
            loop {
                if stream.write_all(&vec![b'x'; 32_768]).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let item = s
            .store
            .writer()
            .run(move |c| {
                let source = support::seed::seed_source(c, "TV")?;
                let id = support::seed::seed_channels(c, source, 1)?[0];
                c.execute(
                    "UPDATE channel SET stream=?2 WHERE id=?1",
                    rusqlite::params![id, serde_json::json!({"t":"url","url":format!("http://{address}/1.ts")}).to_string()],
                )?;
                let ch = tuner::store::live::channel(c, id)?;
                Ok(ItemRef { source, kind: ItemKind::Channel, key: ch.key })
            })
            .await
            .unwrap();
        let lib = Arc::new(tuner::store::library::load(&s.store.reader().unwrap()).unwrap());
        drop(s.agent.publish_library(&lib));
        let budget = s.tiles.budget().clone();
        budget.set_limit(item.source, Some(1));
        let handle = tuner::downloads::service::spawn(
            s.rt.handle(),
            tuner::downloads::service::Deps {
                store: s.store.clone(),
                config: s.config.clone(),
                shared: s.agent.clone(),
                budget,
                http: s.http.clone(),
                bus: s.bus.clone(),
                tiles: s.tiles.registry(),
                paths: s.paths.clone(),
            },
        );
        let added = handle
            .request(DownloadCommand::Add { targets: vec![Target { item, programme: None, minutes: Some(1), any_copy: false }], agent: false })
            .await;
        let id = added.added[0];
        let mut snapshot = handle.subscribe();
        tokio::time::timeout(Duration::from_secs(5), snapshot.wait_for(|s| s.jobs.iter().any(|j| j.id == id && j.bytes_done >= 100_000)))
            .await
            .expect("a recording must not wait on the download rate budget")
            .unwrap();
        handle.request(DownloadCommand::Delete(vec![id])).await;
        let root = tuner::store::downloads::root(&s.store.reader().unwrap(), &s.config().downloads).unwrap();
        let path = root.join(format!("{id}.ts"));
        let kept = std::fs::read(&path).unwrap();
        assert!(kept.len() >= 100_000);
        handle.request(DownloadCommand::Undo(vec![id])).await;
        let job = tuner::store::downloads::job(&s.store.reader().unwrap(), id).unwrap().unwrap();
        assert_eq!(job.state, State::Partial);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(std::fs::read(&path).unwrap(), kept, "Undo must not overwrite the recording");
        handle.request(DownloadCommand::Shutdown).await;
        server.abort();
    });
}
