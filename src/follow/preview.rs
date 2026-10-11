//! The "In the guide now" preview of the rule editor and the Follow sheet (DESIGN §33 §5.2): a
//! blocking-pool task with a cancellation token, 350 ms debounce, newest request wins; answers
//! with `AppEvent::Follow(FollowEvent::PreviewReady)`. Not a job (typing must not clutter the
//! jobs overlay). OWNER: F-AB (F-C starts it lazily from `follow/ui.rs`).

use std::sync::Arc;

use crate::follow::FollowRule;

/// The preview service (cheap to clone).
#[derive(Clone)]
pub struct PreviewService {
    tx: tokio::sync::mpsc::UnboundedSender<(u64, FollowRule, u32)>,
}

impl PreviewService {
    /// Start it on `rt`. Placeholder until F-AB: requests are dropped.
    pub fn start(
        rt: &tokio::runtime::Handle,
        _store: crate::store::Store,
        _shared: Arc<crate::shared::AppShared>,
        _bus: crate::msg::EventBus,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, FollowRule, u32)>();
        rt.spawn(async move { while rx.recv().await.is_some() {} });
        Self { tx }
    }

    /// Preview `rule` over the next `days` days (`rev`: the editor's revision; older answers
    /// are dropped by the UI).
    pub fn request(&self, rev: u64, rule: FollowRule, days: u32) {
        let _ = self.tx.send((rev, rule, days));
    }
}
