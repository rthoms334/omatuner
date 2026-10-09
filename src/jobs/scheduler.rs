//! Refresh scheduling. Owner: WP-06. DESIGN §9.5.
//!
//! * On launch: if any source/EPG is older than `general.refresh_on_launch_after_h` → submit
//!   `RefreshStale` 5 s after start (so first paint isn't competing for the disk).
//! * While running: tick every 15 min → `RefreshStale` (honours per-source `refresh_h`, config
//!   intervals, and "guide runs out in < 12 h"). Ticks are skipped while a stream is in the
//!   Loading/Zapping state (`JobContext::player_busy`; don't compete with zapping).
//! * Daily: `Maintenance`.
//! * No background refresh while the app is closed in v1 (documented; a systemd user timer
//!   calling `tuner --refresh` is a v2 option).

use super::JobManager;

/// Spawn the scheduler task on the manager's runtime. Returns its abort handle.
/// SKELETON SAFE DEFAULT: an idle task (called by `Services::start_integrations`).
pub fn spawn(manager: JobManager) -> tokio::task::AbortHandle {
    tracing::info!("refresh scheduler not implemented yet (WP-06)");
    manager.runtime().spawn(async {}).abort_handle()
}
