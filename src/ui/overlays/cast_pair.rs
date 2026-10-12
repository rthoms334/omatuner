//! PIN pairing with a TV (DESIGN §40 §3.2). OWNER: CAST-8.
//!
//! "Enter the code shown on the TV." with four digit boxes; the 600 ms `ARMING` guard of
//! `agent_connect.rs`; digits and Backspace edit, Enter pairs once 4 digits are in; **Show a new
//! code** asks for the next variant (`CastHandle::pair_new_code`). It starts the job with
//! `CastHandle::pair_start`, polls `pair_state` every frame (the connection stays open while the
//! user types; the overlay waits if the user leaves its workspace) and maps each `PairError` to
//! its copy (wrong code, backoff, Allow Access hint, "The code expired.", not answering). The PIN
//! goes to the service once (`pair_submit`) and is dropped: never logged, stored or shown.
//! Success: the toast "Paired with <TV>" and the cast starts.

use eframe::egui;

use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::DeviceId;
use crate::ui::UiCtx;

/// CAST-8: the PIN overlay. Until then it closes at once (casting is off: `cast::ENABLED`).
#[derive(Debug)]
pub struct CastPair {
    pub device: DeviceId,
}

impl CastPair {
    pub fn new(device: DeviceId) -> Self {
        Self { device }
    }
}

impl Overlay for CastPair {
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
