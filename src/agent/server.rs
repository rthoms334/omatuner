//! The running Tuner's side of IPC v2 (DESIGN §30.4–§30.6). Owner: MCP-app track.
//!
//! `ipc.rs` hands every v2 request line to an [`AgentService`]; production uses
//! [`AgentHandler`]. The handler runs on the tokio runtime (blocking store reads on the blocking
//! pool, one lazily opened reader per worker), never on the UI thread:
//! * queries read the snapshot the app published (`AgentShared::library`), a store reader (EPG,
//!   progress, favourites, details) and the parental state computed now (`AgentShared::parental`);
//! * `play` validates the target here (exists, visible, catch-up window…) and sends
//!   `AppEvent::Agent(AgentCommand::Play)` + `hypr::focus_self()`: the app applies the PIN rules
//!   (prompt on screen), zap lists and resume rules exactly as for a key press;
//! * `control` / `status` go straight to the `PlayerHandle` (work while the window is hidden);
//! * `schedule_switch` / `cancel_switch` write reminder rows (`Reminder::SWITCH_LEAD`) through
//!   the store writer; the reminder scheduler fires them (`PlatformEvent::Switch*`).
//!
//! Guardrails (§30.6) live here: every result passes the visibility predicate (hidden, category
//! filter, explicit-while-hidden, `in_all` for provider lists), locked items are flagged and
//! their details withheld while locked, results never carry a URL (checked on the serialized
//! reply), requests are rate-limited, and actions are logged at INFO by ref only.

use std::sync::Arc;

use futures_util::future::BoxFuture;

use super::proto::{ErrorCode, Reply, Request};
use super::shared::AgentShared;
use crate::config::SharedConfig;
use crate::jobs::JobManager;
use crate::msg::EventBus;
use crate::player::PlayerHandle;
use crate::store::Store;

/// Answers v2 requests (the IPC server holds one; tests install fakes).
pub trait AgentService: Send + Sync + 'static {
    /// The reply to `req` (always a reply: failures are `Reply::err`). Must finish well within
    /// the IPC connection limit (5 s); the handler's own budget is [`HANDLER_BUDGET`].
    fn handle(&self, req: Request) -> BoxFuture<'static, Reply>;
}

/// Time a request may take inside the handler (the IPC connection allows 5 s in all).
pub const HANDLER_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// The production [`AgentService`].
pub struct AgentHandler {
    #[allow(dead_code)] // SKELETON: the MCP-app track's handler reads them
    inner: Arc<Inner>,
}

#[allow(dead_code)] // SKELETON: the MCP-app track's handler reads them
struct Inner {
    shared: Arc<AgentShared>,
    store: Store,
    config: SharedConfig,
    player: PlayerHandle,
    bus: EventBus,
    jobs: JobManager,
}

impl AgentHandler {
    pub fn new(shared: Arc<AgentShared>, store: Store, config: SharedConfig, player: PlayerHandle, bus: EventBus, jobs: JobManager) -> Self {
        Self { inner: Arc::new(Inner { shared, store, config, player, bus, jobs }) }
    }
}

impl AgentService for AgentHandler {
    fn handle(&self, req: Request) -> BoxFuture<'static, Reply> {
        // SKELETON SAFE DEFAULT: the MCP-app track answers every method
        let reply = Reply::err(req.id, ErrorCode::Unsupported, format!("{} is not available in this build yet", req.call.name()));
        Box::pin(async move { reply })
    }
}
