//! What the app shares with the agent handler (DESIGN §30.4): the library snapshot it shows and
//! when the parental session was unlocked. Implemented in the skeleton; owned by the app track.
//!
//! The handler answers on the tokio runtime, never on the UI thread (which may not run while the
//! window sits on a hidden workspace). It reads:
//! * the snapshot the app last published ([`AgentShared::publish_library`], called by the app
//!   whenever `TunerApp::lib` is replaced). The previous snapshot comes back to the caller so the
//!   app drops it on the memory maintenance thread, never on the UI thread;
//! * the parental state, computed at request time from the published unlock instant and the
//!   live config ([`AgentShared::parental`]) — so a session that relocked by its timer while the
//!   UI wasn't running counts as locked, and explicit content in a snapshot built while unlocked
//!   is concealed by the handler itself (`ParentalView::hides_explicit`).

use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::config::ParentalConfig;
use crate::store::library::Library;
use crate::ui::ParentalSession;

#[derive(Debug, Default)]
pub struct AgentShared {
    lib: RwLock<Arc<Library>>,
    unlocked_at: Mutex<Option<Instant>>,
}

/// The parental state at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentalView {
    /// No PIN, or the PIN was entered and the session hasn't relocked.
    pub unlocked: bool,
    /// Explicit content is hidden right now (`ParentalConfig::hides_explicit`).
    pub hides_explicit: bool,
    /// A PIN is set (locked items need it).
    pub pin_active: bool,
}

impl AgentShared {
    /// Publish the app's snapshot; returns the one it replaces (the caller releases it off the
    /// UI thread), `None` when `lib` is already the published one.
    #[must_use = "drop the old snapshot through `TunerApp::release`"]
    pub fn publish_library(&self, lib: &Arc<Library>) -> Option<Arc<Library>> {
        let mut cur = self.lib.write().unwrap_or_else(|e| e.into_inner());
        if Arc::ptr_eq(&cur, lib) {
            return None;
        }
        Some(std::mem::replace(&mut *cur, lib.clone()))
    }

    /// The last published snapshot (`Library::default()` — `built_at` 0 — before the first).
    pub fn library(&self) -> Arc<Library> {
        self.lib.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The app's parental session changed (`ParentalSession::unlocked_at`).
    pub fn set_unlocked_at(&self, at: Option<Instant>) {
        *self.unlocked_at.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// The parental state now, under `cfg` (the live config).
    pub fn parental(&self, cfg: &ParentalConfig) -> ParentalView {
        let at = *self.unlocked_at.lock().unwrap_or_else(|e| e.into_inner());
        let session = ParentalSession::with_unlocked_at(at);
        ParentalView { unlocked: session.is_unlocked(cfg), hides_explicit: session.hides_explicit(cfg), pin_active: cfg.pin_active() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_hands_back_the_old_snapshot_once() {
        let shared = AgentShared::default();
        assert_eq!(shared.library().built_at, 0);
        let a = Arc::new(Library { built_at: 1, ..Default::default() });
        let old = shared.publish_library(&a).expect("replaced the default");
        assert_eq!(old.built_at, 0);
        assert!(shared.publish_library(&a).is_none(), "same snapshot");
        assert_eq!(shared.library().built_at, 1);
    }

    #[test]
    fn the_parental_view_follows_the_unlock_and_the_relock_timer() {
        let salt = crate::ui::parental::new_salt();
        let cfg = ParentalConfig {
            enabled: true,
            pin_hash: Some(crate::ui::parental::hash_pin("1234", &salt)),
            pin_salt: Some(salt),
            relock_min: 1,
            ..Default::default()
        };
        let shared = AgentShared::default();
        assert_eq!(shared.parental(&cfg), ParentalView { unlocked: false, hides_explicit: true, pin_active: true });
        shared.set_unlocked_at(Some(Instant::now()));
        assert!(shared.parental(&cfg).unlocked && !shared.parental(&cfg).hides_explicit);
        // unlocked two minutes ago with a one-minute relock: locked again, UI running or not
        shared.set_unlocked_at(Instant::now().checked_sub(std::time::Duration::from_secs(120)));
        assert!(!shared.parental(&cfg).unlocked);
        let open = ParentalConfig::default();
        assert_eq!(shared.parental(&open), ParentalView { unlocked: true, hides_explicit: true, pin_active: false });
    }
}
