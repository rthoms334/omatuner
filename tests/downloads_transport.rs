//! DESIGN §43 §8.1–§8.2: real service, HTTP sockets and account handoff. Localhost binding
//! is intentionally required; run outside the editing sandbox when it denies sockets.
mod support;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
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
        let (stop_server, count, seen) = (stop.clone(), active.clone(), requests.clone());
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = tokio::select! { ()=stop_server.cancelled()=>break, r=listener.accept()=>r.unwrap() };
                let count = count.clone();
                let seen = seen.clone();
                let stop = stop_server.clone();
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
        let budget = s.tiles.budget().clone();
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
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("pause closed the provider socket");
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
