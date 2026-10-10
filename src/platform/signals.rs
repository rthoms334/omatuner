//! SIGTERM / SIGINT / SIGHUP → a clean exit (logout, `kill`, Ctrl+C in a terminal).
//!
//! 1. The first signal asks the UI to close the window (`PlatformEvent::Quit`): the normal
//!    `on_exit` path then saves progress, stops mpv, removes `run/tuner.sock` and closes the
//!    database.
//! 2. That path needs a frame. A window on a hidden Hyprland workspace gets none (DESIGN §3),
//!    so if the app has not started shutting down within [`GRACE`] — or a second signal
//!    arrives — the watcher exits by itself: the player stops (its final progress save goes to
//!    the writer), the writer queue is flushed, the socket is removed, `exit(0)`.
//! 3. Once the app is shutting down (`closing`), the watcher keeps running until the runtime
//!    stops: a signal then means that shutdown is stuck (mpv on a dead stream), and it exits
//!    directly the same way.
//!
//! Only `main` installs the handlers (tests never replace the process's signal handling).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::msg::{AppEvent, EventBus};
use crate::platform::PlatformEvent;

/// How long the UI gets to start its own shutdown before the watcher exits by itself.
pub const GRACE: Duration = Duration::from_secs(3);

/// Forward SIGTERM, SIGINT and SIGHUP (by name) into a channel. Must run inside the runtime.
pub fn forward() -> std::io::Result<(mpsc::UnboundedReceiver<&'static str>, tokio::task::JoinHandle<()>)> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                Some(()) = term.recv() => "SIGTERM",
                Some(()) = int.recv() => "SIGINT",
                Some(()) = hup.recv() => "SIGHUP",
                else => return,
            };
            if tx.send(name).is_err() {
                return;
            }
        }
    });
    Ok((rx, task))
}

/// The policy above. `closing` is set when the app's own shutdown has begun; `emergency`
/// performs the direct exit (or, in tests, records that it would).
pub async fn quit_on_signals<F, Fut>(mut signals: mpsc::UnboundedReceiver<&'static str>, bus: EventBus, closing: Arc<AtomicBool>, grace: Duration, emergency: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(name) = signals.recv().await else { return };
    if closing.load(Ordering::SeqCst) {
        tracing::warn!("{name} while shutting down: exiting directly");
        return emergency().await;
    }
    tracing::info!("{name}: quitting");
    bus.send(AppEvent::Platform(PlatformEvent::Quit));
    match tokio::time::timeout(grace, signals.recv()).await {
        // the window closed and the app is shutting down: its own path does the rest, unless
        // another signal says it is stuck
        Err(_) if closing.load(Ordering::SeqCst) => match signals.recv().await {
            Some(again) => tracing::warn!("{again} while shutting down: exiting directly"),
            None => return,
        },
        Err(_) => tracing::warn!("the window did not close within {} s (hidden workspace?); exiting directly", grace.as_secs()),
        Ok(Some(again)) => tracing::warn!("{again} again: exiting directly"),
        Ok(None) => return,
    }
    emergency().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig() -> (mpsc::UnboundedSender<&'static str>, mpsc::UnboundedReceiver<&'static str>, EventBus, crossbeam_channel::Receiver<AppEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (bus, events) = EventBus::new(None);
        (tx, rx, bus, events)
    }

    fn quits(events: &crossbeam_channel::Receiver<AppEvent>) -> usize {
        events.try_iter().filter(|e| matches!(e, AppEvent::Platform(PlatformEvent::Quit))).count()
    }

    #[tokio::test]
    async fn a_signal_closes_the_window_and_the_app_shuts_down_itself() {
        let (tx, rx, bus, events) = rig();
        let closing = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let task = tokio::spawn(quit_on_signals(rx, bus, closing.clone(), Duration::from_millis(100), move || async move { f.store(true, Ordering::SeqCst) }));
        tx.send("SIGTERM").unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(quits(&events), 1);
        closing.store(true, Ordering::SeqCst); // what on_exit → Services::shutdown does
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!fired.load(Ordering::SeqCst), "the normal path does the cleanup");
        // …but a signal while that shutdown hangs still exits directly
        tx.send("SIGINT").unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(fired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_window_that_never_closes_exits_directly_after_the_grace() {
        let (tx, rx, bus, events) = rig();
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let task = tokio::spawn(quit_on_signals(rx, bus, Arc::default(), Duration::from_millis(50), move || async move { f.store(true, Ordering::SeqCst) }));
        tx.send("SIGINT").unwrap();
        task.await.unwrap();
        assert_eq!(quits(&events), 1);
        assert!(fired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_second_signal_exits_at_once() {
        let (tx, rx, bus, _events) = rig();
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let task = tokio::spawn(quit_on_signals(rx, bus, Arc::default(), Duration::from_secs(60), move || async move { f.store(true, Ordering::SeqCst) }));
        tx.send("SIGTERM").unwrap();
        tx.send("SIGTERM").unwrap();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(fired.load(Ordering::SeqCst));
    }
}
