//! The first-run card "Watch on your TV" and the setup wizard (DESIGN §40 §3.5). OWNER: CAST-9.
//!
//! Opened by the app (`start_cast_pass`, the `CardGate`, one card at a time) when the pass found
//! an Apple-made, video-capable, unpaired TV. **Set up <TV>** runs the wizard in the card's
//! place: Pair (§3.2) → Network (only when detection says ufw blocks: Allow on firewall, Show
//! command, Check again) → Test on TV (`CastHandle::test_on_tv`), ending "Ready. Press C in the
//! player to cast." Every answer writes `cast.checked_version`; "Don't ask again" also
//! `cast.offer_setup = false`.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::cast::SetupOffer;
use crate::keymap::{Action, KeyContext};
use crate::ui::UiCtx;

/// CAST-9: the card and wizard. Until then it closes at once (casting is off: `cast::ENABLED`).
#[derive(Debug)]
pub struct CastOffer {
    pub offer: SetupOffer,
}

impl CastOffer {
    pub fn new(offer: SetupOffer) -> Self {
        Self { offer }
    }
}

impl Overlay for CastOffer {
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
