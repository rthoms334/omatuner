//! Memory upkeep: give freed heap back to the OS, off the UI thread.
//!
//! glibc's allocator returns memory to the OS only from the top of a heap; what is freed in the
//! middle of an arena stays resident (field report: ~1.1 GB after a day of 4K HEVC in software
//! with a 460k-item library). The big frees of this app are bursts:
//! * a library swap (the old snapshot is ~2 M allocations, 240 MiB here),
//! * the global search index (~110 MiB) when the search card closes,
//! * mpv's decoder memory (hundreds of MiB at 4K 10-bit) when playback stops, video decoding is
//!   switched off (`vid=no`, hidden window), or a stream ends or is reloaded.
//!
//! [`Maintenance`] owns one thread ("tuner-mem"). [`Maintenance::release`] drops a value there
//! (dropping a library snapshot took 20–395 ms on the UI thread: a frame hitch at every swap)
//! and then trims; [`Maintenance::watch_player`] trims after the player events above; and it
//! trims every [`PERIOD`] regardless. Triggers are coalesced: a trim runs [`SETTLE`] after the
//! first trigger (mpv frees a decoder on its own threads, a moment after the event), and at
//! most once per [`MIN_GAP`] (zapping through channels reloads every second or so).
//! `malloc_trim(0)` takes 1–30 ms here (once 213 ms); it locks one arena at a time, so a thread
//! allocating in that arena meanwhile waits for it.
//!
//! The memory figures of the periodic health line come from [`super::memstats`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::player::{PlayerSnapshot, PlayerState};

/// Wait after the first trigger before trimming (let mpv's threads free what they hold).
pub const SETTLE: Duration = Duration::from_secs(2);
/// Shortest interval between two trims.
pub const MIN_GAP: Duration = Duration::from_secs(10);
/// Trim at least this often.
pub const PERIOD: Duration = Duration::from_secs(5 * 60);

/// Hand freed heap back to the OS (`malloc_trim(0)`; glibc only, a no-op elsewhere). Returns
/// whether memory was released. Blocking for up to tens of ms: never on the UI thread.
pub fn trim() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: plain glibc call without pointers; it only releases free pages.
        unsafe { malloc_trim(0) != 0 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    false
}

/// A `/proc/self/status` field in KiB (`VmRSS:   12345 kB`).
fn status_kib(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|l| l.strip_prefix(field)).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse().ok())
}

/// `RssAnon` in MiB (0 where unknown): the trim log line, without `mallinfo2`'s arena walk.
fn anon_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|s| status_kib(&s, "RssAnon:")).map_or(0, |k| k >> 10)
}

/// What the player did that freed memory worth trimming (`None`: nothing). Pure, for tests.
pub fn player_trigger(prev: &PlayerSnapshot, next: &PlayerSnapshot) -> Option<&'static str> {
    let stopped = |s: &PlayerState| matches!(s, PlayerState::Idle | PlayerState::Ended | PlayerState::Error(_));
    if stopped(&next.state) && !stopped(&prev.state) {
        Some("playback stopped")
    } else if next.video_suspended && !prev.video_suspended {
        Some("video off")
    } else if next.load_generation != prev.load_generation {
        // a new load: the previous stream ended (or is being reloaded) and freed its decoder
        Some("stream reload")
    } else {
        None
    }
}

enum Msg {
    /// Drop this here, then trim.
    Release(Box<dyn Send>, &'static str),
    Trim(&'static str),
    Shutdown,
}

/// When the thread trims (tests shorten them).
#[derive(Debug, Clone, Copy)]
struct Timing {
    settle: Duration,
    min_gap: Duration,
    period: Duration,
}

/// Called on the maintenance thread after every trim with its reason (tests observe trims).
type TrimHook = Box<dyn Fn(&'static str) + Send>;

/// The maintenance thread (module docs). Dropping it stops the thread (after it finished
/// dropping what it was given).
pub struct Maintenance {
    tx: Sender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
    watcher: Option<tokio::task::AbortHandle>,
}

impl Maintenance {
    pub fn start() -> Self {
        Self::start_with(Timing { settle: SETTLE, min_gap: MIN_GAP, period: PERIOD }, None)
    }

    fn start_with(timing: Timing, hook: Option<TrimHook>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("tuner-mem".into())
            .spawn(move || run(rx, timing, hook))
            .map_err(|e| tracing::warn!("memory maintenance thread: {e}"))
            .ok();
        Self { tx, thread, watcher: None }
    }

    /// Drop `what` on the maintenance thread, then trim (the module docs say why). Without the
    /// thread it is dropped right here.
    pub fn release<T: Send + 'static>(&self, what: T, why: &'static str) {
        if self.thread.is_some() {
            let _ = self.tx.send(Msg::Release(Box::new(what), why));
        }
    }

    /// Trim soon (coalesced with other triggers).
    pub fn trim_soon(&self, why: &'static str) {
        let _ = self.tx.send(Msg::Trim(why));
    }

    /// Trim after the player stops, switches video off or (re)loads a stream
    /// ([`player_trigger`]). Runs on `rt` until this is dropped.
    pub fn watch_player(&mut self, rt: &tokio::runtime::Handle, mut snapshots: tokio::sync::watch::Receiver<Arc<PlayerSnapshot>>) {
        let tx = self.tx.clone();
        // the state at subscription is the baseline (not whatever the task first sees)
        let mut prev = snapshots.borrow_and_update().clone();
        let task = rt.spawn(async move {
            while snapshots.changed().await.is_ok() {
                let next = snapshots.borrow_and_update().clone();
                if let Some(why) = player_trigger(&prev, &next)
                    && tx.send(Msg::Trim(why)).is_err()
                {
                    break;
                }
                prev = next;
            }
        });
        if let Some(old) = self.watcher.replace(task.abort_handle()) {
            old.abort();
        }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        if let Some(w) = self.watcher.take() {
            w.abort();
        }
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The maintenance thread's loop: drop what arrives, trim when due.
fn run(rx: Receiver<Msg>, timing: Timing, hook: Option<TrimHook>) {
    let mut last_trim = Instant::now();
    // (when, why) of the pending trim
    let mut due: Option<(Instant, &'static str)> = None;
    loop {
        let (deadline, why) = due.unwrap_or((last_trim + timing.period, "periodic"));
        let msg = match rx.recv_deadline(deadline) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let trigger = match msg {
            Some(Msg::Shutdown) => return,
            Some(Msg::Release(what, why)) => {
                let t = Instant::now();
                drop(what);
                tracing::debug!("memory: released {why} in {:?}", t.elapsed());
                Some(why)
            }
            Some(Msg::Trim(why)) => Some(why),
            None => {
                let before = anon_mib();
                let t = Instant::now();
                let released = trim();
                let took = t.elapsed();
                last_trim = Instant::now();
                due = None;
                tracing::debug!("memory: trimmed after {why} in {took:?} (released: {released}); RssAnon {before} → {} MiB", anon_mib());
                if let Some(h) = &hook {
                    h(why);
                }
                None
            }
        };
        if let Some(why) = trigger
            && due.is_none()
        {
            due = Some(((Instant::now() + timing.settle).max(last_trim + timing.min_gap), why));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread::ThreadId;

    use super::*;
    use crate::player::{PlayerError, PlayerErrorKind};

    fn quick(settle_ms: u64, gap_ms: u64, period_ms: u64) -> (Maintenance, Receiver<(&'static str, ThreadId)>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let hook: TrimHook = Box::new(move |why| {
            let _ = tx.send((why, std::thread::current().id()));
        });
        let timing =
            Timing { settle: Duration::from_millis(settle_ms), min_gap: Duration::from_millis(gap_ms), period: Duration::from_millis(period_ms) };
        (Maintenance::start_with(timing, Some(hook)), rx)
    }

    /// Records the thread it is dropped on.
    struct DropProbe(Sender<ThreadId>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    /// A released value (the old library after a swap) is dropped on the maintenance thread,
    /// not the caller's (the UI thread), and a trim follows on that thread.
    #[test]
    fn released_values_drop_on_the_maintenance_thread_then_trim() {
        let (m, trims) = quick(20, 0, 60_000);
        let (tx, dropped) = crossbeam_channel::unbounded();
        let caller = std::thread::current().id();
        m.release(DropProbe(tx), "library swap");
        let on = dropped.recv_timeout(Duration::from_secs(5)).expect("dropped");
        assert_ne!(on, caller, "dropped off the calling thread");
        let (why, trim_on) = trims.recv_timeout(Duration::from_secs(5)).expect("trimmed");
        assert_eq!((why, trim_on), ("library swap", on));
    }

    /// Triggers within the settle time (a zap burst, a swap that also frees the search index)
    /// make one trim, and trims keep the minimum gap.
    #[test]
    fn triggers_coalesce_and_keep_the_minimum_gap() {
        let (m, trims) = quick(150, 600, 60_000);
        let t0 = Instant::now();
        for why in ["stream reload", "stream reload", "video off"] {
            m.trim_soon(why);
        }
        assert_eq!(trims.recv_timeout(Duration::from_secs(5)).unwrap().0, "stream reload");
        let first = t0.elapsed();
        assert!(first >= Duration::from_millis(150), "waited for the settle time: {first:?}");
        m.trim_soon("playback stopped");
        assert_eq!(trims.recv_timeout(Duration::from_secs(5)).unwrap().0, "playback stopped");
        assert!(t0.elapsed() >= first + Duration::from_millis(550), "kept the gap: {:?}", t0.elapsed());
        assert!(trims.recv_timeout(Duration::from_millis(300)).is_err(), "one trim per burst");
    }

    #[test]
    fn trims_periodically_without_triggers() {
        let (_m, trims) = quick(10, 0, 100);
        assert_eq!(trims.recv_timeout(Duration::from_secs(5)).unwrap().0, "periodic");
        assert_eq!(trims.recv_timeout(Duration::from_secs(5)).unwrap().0, "periodic");
    }

    fn snap(state: PlayerState, suspended: bool, generation: u64) -> PlayerSnapshot {
        PlayerSnapshot { state, video_suspended: suspended, load_generation: generation, ..Default::default() }
    }

    #[test]
    fn player_events_that_free_decoder_memory() {
        let err = PlayerState::Error(PlayerError { kind: PlayerErrorKind::Network, message: String::new() });
        let playing = snap(PlayerState::Playing, false, 1);
        assert_eq!(player_trigger(&playing, &snap(PlayerState::Idle, false, 1)), Some("playback stopped"));
        assert_eq!(player_trigger(&playing, &snap(PlayerState::Ended, false, 1)), Some("playback stopped"));
        assert_eq!(player_trigger(&playing, &snap(err.clone(), false, 1)), Some("playback stopped"));
        assert_eq!(player_trigger(&playing, &snap(PlayerState::Playing, true, 1)), Some("video off"));
        assert_eq!(player_trigger(&playing, &snap(PlayerState::Loading { since: Instant::now(), attempt: 1 }, false, 2)), Some("stream reload"));
        // nothing freed: position ticks, pause, resume, staying stopped
        assert_eq!(player_trigger(&playing, &playing), None);
        assert_eq!(player_trigger(&playing, &snap(PlayerState::Paused, false, 1)), None);
        assert_eq!(player_trigger(&snap(PlayerState::Playing, true, 1), &playing), None);
        assert_eq!(player_trigger(&snap(PlayerState::Idle, false, 1), &snap(err, false, 1)), None);
    }

    /// The player's snapshot channel drives trims: video switched off (hidden window) and stop.
    #[test]
    fn player_snapshots_trigger_trims() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (mut m, trims) = quick(10, 0, 60_000);
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(snap(PlayerState::Playing, false, 1)));
        m.watch_player(rt.handle(), rx);
        let spin = |want: &str| {
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let Ok((why, _)) = trims.try_recv() {
                            return why;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("no trim for {want}"))
            })
        };
        tx.send(Arc::new(snap(PlayerState::Playing, true, 1))).unwrap();
        assert_eq!(spin("video off"), "video off");
        tx.send(Arc::new(snap(PlayerState::Idle, true, 1))).unwrap();
        assert_eq!(spin("stop"), "playback stopped");
        drop(m);
    }

    #[test]
    fn status_fields_are_read_in_kib() {
        assert_eq!(status_kib("VmRSS:\t  2048 kB\nVmSwap:\t 0 kB\n", "VmRSS:"), Some(2048));
        assert_eq!(status_kib("VmRSS:\t  2048 kB\n", "VmSwap:"), None);
        assert!(anon_mib() > 0, "this process has anonymous memory");
    }
}
