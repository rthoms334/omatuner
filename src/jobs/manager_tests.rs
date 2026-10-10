//! JobManager behaviour against a real temp store and a scripted local HTTP server:
//! coalescing (Queued / Connecting / later → rerun), force upgrades, cancellation (no sweep, no
//! lost request), one import per source, follow-up submits, a guide that survives a late cancel.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use super::*;
use crate::config::Config;
use crate::model::{EpgSourceKind, SourceColor, SourceSettings};
use crate::sources::http::HttpOptions;

/// How the scripted server answers one path.
#[derive(Clone)]
enum Script {
    /// Accept, read the request, never answer (the job stays `Connecting`).
    Hang,
    /// Send headers + the first 300 KiB, wait for the gate, then the rest.
    Gated(Arc<Vec<u8>>),
    /// Wait for the gate, then answer 404 (the job stays `Connecting` until then).
    Late404,
}

struct Server {
    base: String,
    hits: Arc<AtomicUsize>,
    gate: watch::Sender<bool>,
}

async fn server(scripts: HashMap<&'static str, Script>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let (gate, gate_rx) = watch::channel(false);
    let scripts = Arc::new(scripts);
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let scripts = scripts.clone();
            let mut gate_rx = gate_rx.clone();
            h.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req);
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                match scripts.get(path.as_str()).cloned() {
                    Some(Script::Hang) => {
                        let _ = gate_rx.wait_for(|_| false).await; // forever (until the server drops)
                    }
                    Some(Script::Gated(body)) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/x-mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let split = body.len().min(300 * 1024);
                        if sock.write_all(head.as_bytes()).await.is_err() || sock.write_all(&body[..split]).await.is_err() {
                            return;
                        }
                        let _ = sock.flush().await;
                        let _ = gate_rx.wait_for(|open| *open).await;
                        let _ = sock.write_all(&body[split..]).await;
                        let _ = sock.shutdown().await;
                    }
                    Some(Script::Late404) => {
                        let _ = gate_rx.wait_for(|open| *open).await;
                        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    }
                    None => {
                        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    }
                }
            });
        }
    });
    Server { base, hits, gate }
}

/// A playlist of `n` channels, padded past 300 KiB so the gate falls mid-download.
fn playlist(n: usize, prefix: &str) -> Vec<u8> {
    let mut s = String::from("#EXTM3U\n");
    for i in 0..n {
        s.push_str(&format!("#EXTINF:-1 tvg-id=\"{prefix}{i}.tv\" group-title=\"G{}\",{prefix} {i}\nhttp://127.0.0.1/{prefix}/{i}.ts\n", i % 7));
    }
    while s.len() < 400 * 1024 {
        s.push_str("# padding comment line ......................................................\n");
    }
    s.into_bytes()
}

/// A `Finished` event: (id, kind, outcome).
type Finished = (JobId, JobKind, Result<JobOutcome, String>);

struct Harness {
    _dir: tempfile::TempDir,
    jobs: JobManager,
    events: crossbeam_channel::Receiver<AppEvent>,
    stash: std::sync::Mutex<Vec<Finished>>,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted(dir.path());
    paths.ensure().unwrap();
    let (bus, events) = EventBus::new(None);
    let store = Store::open_temp("tuner-jobs-test", Some(bus.clone())).unwrap();
    let http = HttpClient::new(&HttpOptions::default()).unwrap();
    let config: SharedConfig = Arc::new(std::sync::RwLock::new(Arc::new(Config::default())));
    let ctx = JobContext::new(store, http, bus, paths, config);
    Harness { _dir: dir, jobs: JobManager::new(ctx, tokio::runtime::Handle::current()), events, stash: Default::default() }
}

impl Harness {
    async fn add_url_source(&self, name: &str, url: &str) -> SourceId {
        let draft = SourceDraft {
            name: name.into(),
            kind: SourceKind::M3uUrl,
            icon: String::new(),
            color: SourceColor::Accent,
            url: Some(url.into()),
            path: None,
            xtream: None,
            settings: SourceSettings::default(),
        };
        self.jobs.context().store.writer().run(move |c| crate::store::sources::insert_source(c, &draft, 0)).await.unwrap()
    }

    async fn add_file_source(&self, name: &str, path: &std::path::Path) -> SourceId {
        let draft = SourceDraft {
            name: name.into(),
            kind: SourceKind::M3uFile,
            icon: String::new(),
            color: SourceColor::Accent,
            url: None,
            path: Some(path.to_path_buf()),
            xtream: None,
            settings: SourceSettings::default(),
        };
        self.jobs.context().store.writer().run(move |c| crate::store::sources::insert_source(c, &draft, 0)).await.unwrap()
    }

    fn status(&self, id: JobId) -> Option<JobStatus> {
        self.jobs.active().into_iter().find(|s| s.id == id)
    }

    async fn wait_phase(&self, id: JobId, pred: impl Fn(&JobPhase) -> bool) -> JobStatus {
        for _ in 0..2000 {
            if let Some(s) = self.status(id).filter(|s| pred(&s.phase)) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("job {id:?} never reached the phase; now {:?}", self.status(id));
    }

    /// Wait for the `Finished` event of a job matching `pred`; returns it. Other `Finished`
    /// events are kept for later calls (jobs may finish in any order).
    async fn finished(&self, pred: impl Fn(&JobKind) -> bool) -> (JobId, Result<JobOutcome, String>) {
        for _ in 0..4000 {
            let hit = {
                let mut stash = self.stash.lock().unwrap();
                while let Ok(ev) = self.events.try_recv() {
                    if let AppEvent::Job(JobEvent::Finished { id, kind, outcome, .. }) = ev {
                        stash.push((id, kind, outcome));
                    }
                }
                stash.iter().position(|(_, k, _)| pred(k)).map(|i| stash.remove(i))
            };
            if let Some((id, _, outcome)) = hit {
                return (id, outcome);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no matching job finished; active: {:?}", self.jobs.active());
    }

    fn count(&self, sql: &'static str, source: SourceId) -> i64 {
        let c = self.jobs.context().store.reader().unwrap();
        c.query_row(sql, [source], |r| r.get(0)).unwrap()
    }
}

fn refresh(source: SourceId, force: bool) -> JobKind {
    JobKind::RefreshSource { source, force }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn coalesces_while_queued_or_connecting_and_upgrades_force() {
    let srv = server(HashMap::from([("/hang.m3u", Script::Hang)])).await;
    let h = harness();
    let url = format!("{}/hang.m3u", srv.base);
    let (a, b, c) = (h.add_url_source("A", &url).await, h.add_url_source("B", &url).await, h.add_url_source("C", &url).await);

    let ja = h.jobs.submit(refresh(a, false));
    let jb = h.jobs.submit(refresh(b, false));
    h.wait_phase(ja, |p| *p == JobPhase::Connecting).await;
    h.wait_phase(jb, |p| *p == JobPhase::Connecting).await;
    // both playlist permits are taken: C waits
    let jc = h.jobs.submit(refresh(c, false));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.status(jc).unwrap().phase, JobPhase::Queued);
    // queued: same id, and a force request upgrades the waiting job
    assert_eq!(h.jobs.submit(refresh(c, true)), jc);
    assert_eq!(h.status(jc).unwrap().kind, refresh(c, true));
    // A is still connecting but has read its source row: same id, a rerun is flagged (and
    // dropped by the cancel below — requests made before a cancel go with it)
    assert_eq!(h.jobs.submit(refresh(a, false)), ja);
    assert_eq!(h.jobs.active().len(), 3);
    assert!(h.status(ja).unwrap().label.starts_with("Refreshing A"), "{:?}", h.status(ja));

    // cancelling A frees a permit: C starts, with the upgraded kind
    h.jobs.cancel(ja);
    let (id, outcome) = h.finished(|k| *k == refresh(a, false)).await;
    assert_eq!((id, outcome.unwrap_err()), (ja, "cancelled".to_owned()));
    h.wait_phase(jc, |p| *p == JobPhase::Connecting).await;
    assert_eq!(h.status(jc).unwrap().kind, refresh(c, true));

    // a cancelled QUEUED job finishes without ever starting
    let d = h.add_url_source("D", &url).await;
    let jd = h.jobs.submit(refresh(d, false));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(h.status(jd).unwrap().phase, JobPhase::Queued);
    h.jobs.cancel(jd);
    let (id, outcome) = h.finished(|k| *k == refresh(d, false)).await;
    assert_eq!((id, outcome.unwrap_err()), (jd, "cancelled".to_owned()));

    h.jobs.cancel_all();
    assert!(h.finished(|k| *k == refresh(b, false)).await.1.is_err());
    assert!(h.finished(|k| *k == refresh(c, true)).await.1.is_err());
    assert!(h.jobs.active().is_empty());
    // nothing was rerun after the cancellations
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(h.jobs.active().is_empty());
    assert_eq!(srv.hits.load(Ordering::SeqCst), 3, "A, B and C connected once each");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_request_arriving_mid_download_reruns_the_job_afterwards() {
    let body = Arc::new(playlist(50, "Ch"));
    let srv = server(HashMap::from([("/slow.m3u", Script::Gated(body))])).await;
    let h = harness();
    let s = h.add_url_source("Slow", &format!("{}/slow.m3u", srv.base)).await;

    let first = h.jobs.submit(refresh(s, false));
    h.wait_phase(first, |p| matches!(p, JobPhase::Downloading { .. })).await;
    // too late to coalesce: same id returned, rerun flagged, still one job
    assert_eq!(h.jobs.submit(refresh(s, false)), first);
    assert_eq!(h.jobs.active().len(), 1);
    srv.gate.send(true).unwrap();

    let (id, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    assert_eq!(id, first);
    match outcome {
        Ok(JobOutcome::SourceRefreshed { source, summary, not_modified: false, account: None }) => {
            assert_eq!(source, s);
            assert_eq!(summary.channels.total, 50);
        }
        other => panic!("{other:?}"),
    }
    let (second, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    assert_ne!(second, first, "the rerun is a new job");
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
    // follow-up: every refresh submits RebuildLibrary
    match h.finished(|k| *k == JobKind::RebuildLibrary).await.1 {
        Ok(JobOutcome::Library(lib)) => assert_eq!(lib.live.channels.len(), 50),
        other => panic!("{other:?}"),
    }
    // the downloaded playlist (may carry credentials) is gone
    let cache = h.jobs.context().paths.playlist_cache();
    assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_cancelled_import_never_sweeps() {
    let h = harness();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("list.m3u");
    std::fs::write(&file, playlist(100, "Old")).unwrap();
    let s = h.add_file_source("File", &file).await;
    h.jobs.submit(refresh(s, false));
    assert!(h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await.1.is_ok());
    let gen0 = h.count("SELECT generation FROM source WHERE id = ?1", s);
    assert_eq!(h.count("SELECT count(*) FROM channel WHERE source_id = ?1", s), 100);

    // a much bigger list without any of the old channels; cancel once writing started
    std::fs::write(&file, playlist(150_000, "New")).unwrap();
    let job = h.jobs.submit(refresh(s, false));
    h.wait_phase(job, |p| matches!(p, JobPhase::Writing { done, .. } if *done > 0)).await;
    h.jobs.cancel(job);
    let (id, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    assert_eq!((id, outcome.unwrap_err()), (job, "cancelled".to_owned()));

    let old = h.count("SELECT count(*) FROM channel WHERE source_id = ?1 AND name LIKE 'Old %'", s);
    let new = h.count("SELECT count(*) FROM channel WHERE source_id = ?1 AND name LIKE 'New %'", s);
    assert_eq!(old, 100, "no sweep after a cancelled import");
    assert!(new > 0 && new < 150_000, "{new} new rows");
    assert_eq!(h.count("SELECT generation FROM source WHERE id = ?1", s), gen0, "generation not bumped");
    let last_error: Option<String> =
        h.jobs.context().store.reader().unwrap().query_row("SELECT last_error FROM source WHERE id = ?1", [s], |r| r.get(0)).unwrap();
    assert_eq!(last_error, None, "a cancellation is not a failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn failures_finish_with_a_redacted_message_and_the_manager_keeps_going() {
    let h = harness();
    let secret = "s3cr3t-jobs-token-81723";
    crate::redact::register_secret(secret);
    let s = h.add_url_source("Dead", &format!("http://127.0.0.1:9/list.m3u?token={secret}")).await;
    h.jobs.submit(refresh(s, false));
    let (_, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    let msg = outcome.unwrap_err();
    assert!(!msg.contains(secret), "{msg}");
    let stored: Option<String> =
        h.jobs.context().store.reader().unwrap().query_row("SELECT last_error FROM source WHERE id = ?1", [s], |r| r.get(0)).unwrap();
    let stored = stored.expect("failure recorded");
    assert!(!stored.contains(secret), "{stored}");
    // a missing source is an error, not a panic; the manager still serves jobs
    h.jobs.submit(refresh(SourceId(999), false));
    assert!(h.finished(|k| *k == refresh(SourceId(999), false)).await.1.is_err());
    h.jobs.submit(JobKind::RebuildLibrary);
    assert!(matches!(h.finished(|k| *k == JobKind::RebuildLibrary).await.1, Ok(JobOutcome::Library(_))));
}

fn probe_draft(url: &str) -> SourceDraft {
    SourceDraft {
        name: "Probe".into(),
        kind: SourceKind::M3uUrl,
        icon: String::new(),
        color: SourceColor::Accent,
        url: Some(url.into()),
        path: None,
        xtream: None,
        settings: SourceSettings::default(),
    }
}

/// Adds of one playlist (a double Enter, a retry under another name) while the first is still
/// importing join it: one job, one source — never a second row once the first add refreshed.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn adds_of_one_locator_join_the_running_add() {
    let body = Arc::new(playlist(20, "Ch"));
    let srv = server(HashMap::from([("/new.m3u", Script::Gated(body))])).await;
    let h = harness();
    let url = format!("{}/new.m3u", srv.base);
    let draft = |name: &str| SourceDraft { name: name.into(), ..probe_draft(&url) };
    let first = h.jobs.submit(JobKind::AddSource(draft("Mine")));
    h.wait_phase(first, |p| matches!(p, JobPhase::Downloading { .. })).await;
    assert_eq!(h.jobs.submit(JobKind::AddSource(draft("Mine"))), first, "identical request");
    assert_eq!(h.jobs.submit(JobKind::AddSource(draft("Renamed"))), first, "same locator, other name");
    assert_eq!(h.jobs.active().len(), 1);
    srv.gate.send(true).unwrap();
    let (id, outcome) = h.finished(|k| matches!(k, JobKind::AddSource(_))).await;
    assert_eq!(id, first);
    assert!(matches!(outcome, Ok(JobOutcome::SourceAdded(_))), "{outcome:?}");
    // no rerun inserting a second source after the first one refreshed
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.jobs.active().iter().all(|s| !matches!(s.kind, JobKind::AddSource(_))), "{:?}", h.jobs.active());
    let c = h.jobs.context().store.reader().unwrap();
    assert_eq!(c.query_row("SELECT count(*) FROM source", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_connecting_job_that_has_not_read_inputs_absorbs_identical_requests() {
    let srv = server(HashMap::from([("/late.m3u", Script::Late404)])).await;
    let h = harness();
    let kind = JobKind::ProbeSource(probe_draft(&format!("{}/late.m3u", srv.base)));
    let job = h.jobs.submit(kind.clone());
    h.wait_phase(job, |p| *p == JobPhase::Connecting).await;
    assert_eq!(h.jobs.submit(kind.clone()), job);
    assert_eq!(h.jobs.active().len(), 1);
    srv.gate.send(true).unwrap();
    let (id, outcome) = h.finished(|k| *k == kind).await;
    assert_eq!(id, job);
    assert!(outcome.is_err());
    // merged, not rerun: one request reached the server
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.jobs.active().is_empty(), "{:?}", h.jobs.active());
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_request_after_a_cancel_runs_once_the_cancelled_job_is_gone() {
    let srv = server(HashMap::from([("/hang.m3u", Script::Hang)])).await;
    let h = harness();
    let a = h.add_url_source("A", &format!("{}/hang.m3u", srv.base)).await;
    let first = h.jobs.submit(refresh(a, false));
    h.wait_phase(first, |p| *p == JobPhase::Connecting).await;
    h.jobs.cancel(first);
    // "Refresh" clicked again right away, while the cancelled run still winds down
    h.jobs.submit(refresh(a, true));
    let (id, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    assert_eq!((id, outcome.unwrap_err()), (first, "cancelled".to_owned()));

    let mut second = None;
    for _ in 0..2000 {
        second = h.jobs.active().into_iter().find(|s| s.id != first && s.kind == refresh(a, true) && s.phase == JobPhase::Connecting);
        if second.is_some() && srv.hits.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = second.expect("the newer request runs as a new job");
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
    h.jobs.cancel_all();
    let (id, outcome) = h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await;
    assert_eq!((id, outcome.unwrap_err()), (second.id, "cancelled".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn imports_of_one_source_never_overlap() {
    let body = Arc::new(playlist(50, "Ch"));
    let srv = server(HashMap::from([("/slow.m3u", Script::Gated(body))])).await;
    let h = harness();
    let s = h.add_url_source("Slow", &format!("{}/slow.m3u", srv.base)).await;
    let first = h.jobs.submit(refresh(s, false));
    h.wait_phase(first, |p| matches!(p, JobPhase::Downloading { .. })).await;

    // a second import of the same row under another job key (AddSource's inline refresh)
    let ctx = h.jobs.context().clone();
    let second = tokio::spawn(async move {
        import_source::refresh_source(&ctx, s, true, &Progress::detached(), &CancellationToken::new()).await.map(|r| r.summary.channels.total)
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(srv.hits.load(Ordering::SeqCst), 1, "the second import waits for the first");
    srv.gate.send(true).unwrap();

    match h.finished(|k| matches!(k, JobKind::RefreshSource { .. })).await.1 {
        Ok(JobOutcome::SourceRefreshed { summary, .. }) => assert_eq!(summary.channels.total, 50),
        other => panic!("{other:?}"),
    }
    assert_eq!(second.await.unwrap().unwrap(), 50);
    assert_eq!(srv.hits.load(Ordering::SeqCst), 2);
    assert_eq!(h.count("SELECT count(*) FROM channel WHERE source_id = ?1", s), 50);
    let cache = h.jobs.context().paths.playlist_cache();
    assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
}

/// An XMLTV guide of `channels` × `per_channel` one-hour programmes starting an hour ago.
fn guide(channels: usize, per_channel: usize, title: &str) -> String {
    let start = crate::model::now() - 3_600;
    let fmt = |t: i64| format!("{} +0000", jiff::Timestamp::from_second(t).unwrap().strftime("%Y%m%d%H%M%S"));
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv>\n");
    for c in 0..channels {
        s.push_str(&format!("<channel id=\"c{c}.tv\"><display-name>Channel {c}</display-name></channel>\n"));
    }
    for c in 0..channels {
        for i in 0..per_channel {
            let t = start + i as i64 * 3_600;
            s.push_str(&format!(
                "<programme start=\"{}\" stop=\"{}\" channel=\"c{c}.tv\"><title>{title} {i}</title></programme>\n",
                fmt(t),
                fmt(t + 3_600)
            ));
        }
    }
    s.push_str("</tv>\n");
    s
}

/// App exit: `shutdown` cancels and refuses new jobs; `wait_idle` returns only once the
/// cancelled guide import has discarded its partial generation (its wind-down), so the
/// runtime shutdown that follows never cuts it short.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shutdown_waits_for_the_wind_down_of_cancelled_jobs() {
    let h = harness();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("guide.xml");
    std::fs::write(&file, guide(200, 400, "Big")).unwrap();
    let kind = EpgSourceKind::File { path: file.clone() };
    let g = h.jobs.context().store.writer().run(move |c| crate::store::sources::insert_epg_source(c, "Guide", &kind, 0)).await.unwrap();
    let job = h.jobs.submit(JobKind::RefreshEpg { epg_source: g, force: true });
    h.wait_phase(job, |p| matches!(p, JobPhase::Writing { done, .. } if *done > 0)).await;

    h.jobs.shutdown();
    assert_eq!(h.jobs.submit(JobKind::RebuildLibrary), JobId(0), "nothing new starts while closing");
    let jobs = h.jobs.clone();
    assert!(tokio::task::spawn_blocking(move || jobs.wait_idle(Duration::from_secs(20))).await.unwrap());
    assert!(h.jobs.active().is_empty());
    let (id, outcome) = h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await;
    assert_eq!((id, outcome.unwrap_err()), (job, "cancelled".to_owned()));
    // the partial guide is gone already: nothing left for a later sweep
    let partial: i64 =
        h.jobs.context().store.reader().unwrap().query_row("SELECT count(*) FROM programme WHERE epg_source_id = ?1", [g], |r| r.get(0)).unwrap();
    assert_eq!(partial, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_cancel_after_the_flip_keeps_the_new_guide() {
    let h = harness();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("guide.xml");
    let (channels, per_channel) = (100, 300);
    let total = (channels * per_channel) as i64;
    std::fs::write(&file, guide(channels, per_channel, "Old")).unwrap();
    let kind = EpgSourceKind::File { path: file.clone() };
    let g = h.jobs.context().store.writer().run(move |c| crate::store::sources::insert_epg_source(c, "Guide", &kind, 0)).await.unwrap();
    let epg = |force| JobKind::RefreshEpg { epg_source: g, force };
    h.jobs.submit(epg(false));
    assert!(h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await.1.is_ok());
    let scalar = |sql: &str| -> i64 { h.jobs.context().store.reader().unwrap().query_row(sql, [g], |r| r.get(0)).unwrap() };
    let gen_sql = "SELECT active_gen FROM epg_source WHERE id = ?1";
    let active_sql = "SELECT count(*) FROM programme p JOIN epg_source es ON es.id = p.epg_source_id
                      WHERE es.id = ?1 AND p.gen = es.active_gen";
    let gen0 = scalar(gen_sql);
    assert_eq!(scalar(active_sql), total);

    // the next import is cancelled the moment it flipped, while the old guide is being swept
    std::fs::write(&file, guide(channels, per_channel, "New")).unwrap();
    let job = h.jobs.submit(epg(true));
    let mut cancelled_after_flip = false;
    for _ in 0..20_000 {
        if scalar(gen_sql) != gen0 {
            cancelled_after_flip = h.status(job).is_some();
            h.jobs.cancel(job);
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(cancelled_after_flip, "the import finished before the test saw the flip");
    match h.finished(|k| matches!(k, JobKind::RefreshEpg { .. })).await {
        (id, Ok(JobOutcome::EpgRefreshed { summary, not_modified: false, .. })) => {
            assert_eq!(id, job);
            assert_eq!(i64::from(summary.programmes), total);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(scalar(active_sql), total, "the new guide is complete and active");
    let new_titles = scalar(
        "SELECT count(*) FROM programme p JOIN epg_source es ON es.id = p.epg_source_id
         WHERE es.id = ?1 AND p.gen = es.active_gen AND p.title LIKE 'New %'",
    );
    assert_eq!(new_titles, total);

    // whatever the cancel left of the old generation, maintenance sweeps it
    h.jobs.submit(JobKind::Maintenance);
    assert!(h.finished(|k| *k == JobKind::Maintenance).await.1.is_ok());
    assert_eq!(scalar("SELECT count(*) FROM programme WHERE epg_source_id = ?1"), total);
}
