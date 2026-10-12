//! SQLite persistence. FROZEN (skeleton-implemented: `open`, migrations, writer actor,
//! readers). Repository modules (`sources live vod epg user import library backup sql`) are
//! WP-02's.
//!
//! # Connections & threading (DESIGN §5.3)
//! * ONE writer: a dedicated OS thread ("db-writer") owns the only read-write connection and
//!   executes closures submitted through [`Writer`] in FIFO order. Imports submit one closure
//!   per batch (≈2 000 rows, one transaction each) so user actions (toggle favourite) interleave
//!   with long imports and never wait more than one batch.
//! * Every job runs inside `catch_unwind`: a panicking closure is rolled back, its caller gets
//!   `Err("database writer job panicked")`, and the writer keeps serving later jobs.
//! * Shutdown: [`Writer::shutdown`] (called last by `Services::shutdown`) enqueues a sentinel,
//!   so every write submitted before it (e.g. the final progress save on Stop) runs, then the
//!   thread checkpoints the WAL (`TRUNCATE`) and exits; the call joins with a timeout.
//! * Readers: any thread may open its own read-only connection with [`Store::reader`]
//!   (WAL ⇒ readers never block the writer and see the last committed state). Readers must not
//!   keep statements open across frames (WAL growth). The UI thread keeps one reader for cheap
//!   indexed queries (visible rows, now/next, grid windows) within the per-frame budget
//!   (`ui::data::FrameBudget`, 2 ms).
//! * Repo functions are free functions over `&Connection` (`store::live::channel(conn, id)`),
//!   so they are usable from the writer closure, a reader, or a test connection.
//! * Tests: [`Store::open_temp`] gives a real WAL database in a private temp dir (concurrent
//!   readers behave exactly like production; shared-cache in-memory DBs return SQLITE_LOCKED,
//!   which `busy_timeout` ignores, so they are not offered).
//!
//! # File security
//! The DB holds credentials: it is created with mode 0600 inside a 0700 directory, before
//! SQLite opens it (SQLite copies the mode to `-wal`/`-shm`).

pub mod backup;
pub mod copies;
pub mod epg;
pub mod follow;
pub mod guide_fix;
pub mod import;
pub mod library;
pub mod live;
pub mod migrations;
pub mod queue;
pub mod retime;
pub mod sources;
pub mod spoiler;
pub mod sql;
pub mod stacks;
pub mod titles;
pub mod user;
pub mod vod;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::msg::{DataScope, EventBus};

/// Opened database: path + writer handle. Cheap to clone (`Writer` is a channel sender).
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    writer: Writer,
    /// Keeps a test database's temp dir alive (removed when the last clone drops).
    _temp: Option<Arc<TempDir>>,
}

impl Store {
    /// Create/open the DB at `path` (0600), apply PRAGMAs and migrations, start the writer
    /// thread. `bus` (optional) receives `AppEvent::Data` after `submit_notify` writes.
    pub fn open(path: &Path, bus: Option<EventBus>) -> Result<Self> {
        create_private_file(path)?;
        let mut conn = Connection::open(path)?;
        configure(&conn, false)?;
        migrations::migrate(&mut conn)?;
        // rules saved per playlist before the category rule was shared (DESIGN §28.3)
        sources::adopt_legacy_category_filters(&conn)?;
        let writer = Writer::spawn(conn, bus)?;
        Ok(Self { path: path.to_path_buf(), writer, _temp: None })
    }

    /// A fresh WAL database in a private temp directory (tests, benches). `name` prefixes the
    /// directory for debugging; the directory is deleted when the last `Store` clone drops.
    pub fn open_temp(name: &str, bus: Option<EventBus>) -> Result<Self> {
        let dir = TempDir::new(name)?;
        let mut s = Self::open(&dir.0.join("tuner.db"), bus)?;
        s._temp = Some(Arc::new(dir));
        Ok(s)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The single writer.
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Open a new read-only connection (`query_only=ON`). Each thread keeps its own.
    pub fn reader(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        configure(&conn, true)?;
        Ok(conn)
    }

    /// Drain the writer and join it (see [`Writer::shutdown`]).
    pub fn close(&self, timeout: Duration) -> bool {
        self.writer.shutdown(timeout)
    }
}

/// Private temp dir (0700) without the `tempfile` crate (dev-only dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let p = std::env::temp_dir().join(format!("{prefix}-{}-{}-{nanos}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).map_err(|e| Error::io(&p, e))?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(&p, e))?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path).map_err(|e| Error::io(path, e))?;
    // tighten pre-existing files too
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| Error::io(path, e))
}

fn configure(conn: &Connection, read_only: bool) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    if read_only {
        conn.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-16000;")?;
    } else {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; \
             PRAGMA temp_store=MEMORY; PRAGMA cache_size=-32000; PRAGMA wal_autocheckpoint=4000; \
             PRAGMA journal_size_limit=67108864;",
        )?;
    }
    Ok(())
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

enum Msg {
    Job(WriteJob),
    Shutdown,
}

/// Handle to the writer thread. `Clone + Send + Sync`.
#[derive(Clone)]
pub struct Writer {
    tx: Sender<Msg>,
    bus: Option<EventBus>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
    done: crossbeam_channel::Receiver<()>,
}

impl Writer {
    fn spawn(mut conn: Connection, bus: Option<EventBus>) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
        let handle = std::thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || {
                while let Ok(Msg::Job(job)) = rx.recv() {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut conn)));
                    if r.is_err() {
                        tracing::error!("database writer job panicked; rolled back, writer continues");
                        if !conn.is_autocommit() {
                            let _ = conn.execute_batch("ROLLBACK");
                        }
                    }
                }
                // shutdown sentinel or all senders dropped: checkpoint so the WAL doesn't linger
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                let _ = done_tx.send(());
            })
            .map_err(Error::other)?;
        Ok(Self { tx, bus, thread: Arc::new(Mutex::new(Some(handle))), done: done_rx })
    }

    /// Run `f` on the writer thread. `f` should open its own transaction when it writes more
    /// than one statement (`let tx = conn.transaction()?; ...; tx.commit()`). If `f` panics the
    /// ticket resolves to `Err` (the sender is dropped during unwinding).
    pub fn submit<R, F>(&self, f: F) -> Ticket<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        let (otx, orx) = oneshot::channel();
        let job: WriteJob = Box::new(move |c| {
            let _ = otx.send(f(c));
        });
        if self.tx.send(Msg::Job(job)).is_err() {
            let (otx, orx) = oneshot::channel();
            let _ = otx.send(Err(Error::other("database writer stopped")));
            return Ticket(orx);
        }
        Ticket(orx)
    }

    /// Like [`submit`](Self::submit), then emits `AppEvent::Data(scope)` on success.
    pub fn submit_notify<R, F>(&self, scope: DataScope, f: F) -> Ticket<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        let bus = self.bus.clone();
        self.submit(move |c| {
            let r = f(c);
            if r.is_ok()
                && let Some(b) = &bus
            {
                b.data_changed(scope);
            }
            r
        })
    }

    /// Async convenience for tokio jobs.
    pub async fn run<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        self.submit(f).recv().await
    }

    /// Blocking convenience (tests, non-async threads). Never call from the UI thread.
    pub fn blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        self.submit(f).wait()
    }

    /// Run every job submitted so far, checkpoint, stop the thread and join it. Waits at most
    /// `timeout`; returns false if the writer did not finish in time (the thread is then left
    /// to finish on its own — process exit). Later submits fail with "database writer stopped".
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let _ = self.tx.send(Msg::Shutdown);
        if self.done.recv_timeout(timeout).is_err() {
            return false;
        }
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        true
    }

    pub fn bus(&self) -> Option<&EventBus> {
        self.bus.as_ref()
    }
}

/// Completion handle of a write. Dropping it is fine (fire-and-forget).
pub struct Ticket<R>(oneshot::Receiver<Result<R>>);

impl<R> Ticket<R> {
    /// Non-blocking poll (UI thread): `Some` once finished.
    pub fn try_take(&mut self) -> Option<Result<R>> {
        match self.0.try_recv() {
            Ok(r) => Some(r),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(Error::other("database writer dropped request"))),
        }
    }
    /// Block until done. Not on the UI thread.
    pub fn wait(self) -> Result<R> {
        self.0.blocking_recv().unwrap_or_else(|_| Err(Error::other("database writer dropped request")))
    }
    pub async fn recv(self) -> Result<R> {
        self.0.await.unwrap_or_else(|_| Err(Error::other("database writer dropped request")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_survives_a_panicking_job_and_drains_on_shutdown() {
        let store = Store::open_temp("tuner-writer-test", None).unwrap();
        let w = store.writer().clone();
        let r: Result<()> = w.blocking(|c| {
            c.execute_batch("BEGIN; INSERT INTO meta(key,value) VALUES ('a','1');")?;
            panic!("boom");
        });
        assert!(r.is_err());
        // the open transaction was rolled back and the writer still works
        let n: i64 = w.blocking(|c| Ok(c.query_row("SELECT count(*) FROM meta", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0);
        // a write submitted before shutdown is executed
        let _ = w.submit(|c| Ok(c.execute("INSERT INTO meta(key,value) VALUES ('b','2')", [])?));
        assert!(store.close(Duration::from_secs(5)));
        let conn = store.reader().unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM meta WHERE key='b'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(w.blocking(|_| Ok(())).is_err());
    }
}
