//! The cast service (DESIGN §40 §5.6): started by `Services::start_integrations` (a failure means
//! no casting, never a crash), restarted by `Services::apply_config` on `[cast]` changes (the
//! MPRIS pattern: `.take().stop()`, then start if enabled), stopped first at shutdown (a
//! fire-and-forget stop to the TV within 300 ms; children, a waiting `pkexec`, listeners, the
//! inhibitor and the session directories go; all inside the runtime's 2 s budget). It implements
//! [`super::CastApi`] (published with `AppShared::set_cast`), which also serves picture A's
//! `player::remote::CastConnector`. OWNER: CAST-6.

use std::sync::Arc;

use super::{CastApi, CastHandle};

/// The running service (`Integrations::cast`).
pub struct CastService {
    // CAST-6: the runtime handle, the registry, discovery, the firewall cache and job, the
    // pairing jobs, the current session.
    api: Arc<dyn CastApi>,
}

impl CastService {
    /// The facade the app and the agent use.
    pub fn handle(&self) -> CastHandle {
        CastHandle::new(self.api.clone())
    }

    /// Stop everything (see the module docs); idempotent.
    pub fn stop(&mut self) {
        // CAST-6
    }
}

impl Drop for CastService {
    fn drop(&mut self) {
        self.stop();
    }
}
