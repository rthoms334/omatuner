//! Marks guarded events seen while they play (every picture, through `AppShared`), polling
//! every 30 s so it works while the window is hidden: live ≥ 50 % or the last 10 minutes,
//! catch-up / VOD ≥ 90 %. Only guarded subjects are ever recorded. OWNER: S1.

use std::sync::Arc;

/// Start the tracker (`Services::start_integrations`). Placeholder until S1: nothing runs.
pub fn start(_rt: &tokio::runtime::Handle, _shared: Arc<crate::shared::AppShared>, _store: crate::store::Store) {}
