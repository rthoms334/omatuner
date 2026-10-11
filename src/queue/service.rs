//! The Up next engine task (tokio): commands in, `AppShared::publish_queue` out, timers
//! (countdown, slot start / stop, ≤ 60 s wall-clock re-check), play-time resolution on the
//! blocking pool. OWNER: Q-core.

use std::sync::Arc;

use crate::queue::model::QueueCommand;

/// The UI's / MPRIS's / the agent's way in (cheap to clone).
#[derive(Debug, Clone)]
pub struct QueueHandle {
    tx: tokio::sync::mpsc::UnboundedSender<QueueCommand>,
    shared: Arc<crate::shared::AppShared>,
}

impl QueueHandle {
    /// Send a command (never blocks; dropped when the engine isn't running).
    pub fn send(&self, cmd: QueueCommand) {
        let _ = self.tx.send(cmd);
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> Arc<crate::queue::QueueSnapshot> {
        self.shared.queue()
    }
}

/// Start the engine (`Services::start_integrations`, after the player). Placeholder until
/// Q-core: commands are drained and ignored; the snapshot stays empty.
pub fn start(rt: &tokio::runtime::Handle, shared: Arc<crate::shared::AppShared>) -> QueueHandle {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<QueueCommand>();
    rt.spawn(async move { while rx.recv().await.is_some() {} });
    QueueHandle { tx, shared }
}
