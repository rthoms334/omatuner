//! The firewall card (DESIGN §40 §3.4, §9.4). OWNER: CAST-9.
//!
//! "Living Room couldn't connect back to Tuner. Your firewall is blocking it." with **Allow on
//! firewall** (only when `Detection::can_allow`), **Show command** (`firewall::copy_text`, the
//! only remedy for firewalld, nftables, `NoAgent` and `NotAuthorized`), **Check again** and Play
//! here. While polkit's dialog is up (`FirewallJob::Waiting`): "Waiting for your password…" with
//! Cancel (`CastHandle::firewall_cancel`). Each `AllowOutcome` has its copy; ufw's redacted
//! message shows only here.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// CAST-9: the firewall card. Until then it closes at once (casting is off: `cast::ENABLED`).
#[derive(Debug, Default)]
pub struct CastFirewall {}

impl CastFirewall {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Overlay for CastFirewall {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }
    fn on_action(&mut self, _ctx: &mut UiCtx<'_>, _action: Action) -> OverlayOutcome {
        OverlayOutcome::Close
    }
    fn ui(&mut self, _egui_ctx: &egui::Context, _ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        OverlayOutcome::Close
    }
}
