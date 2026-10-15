//! Tuner's updates in the app (DESIGN §41.4, §41.6): every frame, what only the UI knows goes to
//! the updater (the window has the keyboard, a dialog or text field is open, multi-view, the
//! screen); the passwordless switch's result is saved into the config; the password dialog of
//! an install waits for its moment; and the status-bar chip says an update waits for the
//! restart ("Updated to 0.2.6 — restarts when idle (Restart now)").
//!
//! The moment for the password dialog: Tuner's window has the keyboard (so the dialog makes
//! sense to whoever is at it), nothing plays in either picture, nothing casts (a session, Test
//! on TV, a pairing, the firewall's dialog: `Updater::prompt_due`), nothing is open, no text
//! field is being typed in, not popped out. The restart itself is decided off the UI thread
//! (`update::service`), from what this publishes.

use eframe::egui;

use super::TunerApp;
use crate::cli::ViewArg;
use crate::keymap::{Action, KeyContext};
use crate::player::PlayerState;
use crate::ui::Route;
use crate::ui::widgets::status_bar::StatusChip;
use crate::update::service::UiNow;

impl TunerApp {
    /// The updater's part of the frame (after the first-run card's).
    pub(super) fn update_frame(&mut self, egui_ctx: &egui::Context) {
        let focused = egui_ctx.input(|i| i.viewport().focused).unwrap_or(false);
        let typing = egui_ctx.egui_wants_keyboard_input() || self.key_stack().contains(&KeyContext::TextInput);
        let onboarding = matches!(self.current_route(), Route::Onboarding(_));
        let busy = !self.overlays.is_empty() || !self.nav.is_empty() || typing || onboarding;
        let ui = UiNow { focused, busy, multiview: self.mv.view.on, view: view_of(self.current_route()) };
        let updater = self.services().updater.clone();
        updater.note_ui(ui);
        if let Some((on, input)) = updater.take_config_edit() {
            self.config_edits.push(crate::config::audit::edit(input, move |c| c.updates.passwordless = on));
        }
        let playing = self.snapshot.state != PlayerState::Idle || self.mv.view.on;
        if focused && !busy && !playing && !self.popout.is_out() && !self.video_only && updater.prompt_due() {
            updater.install_now();
        }
    }

    /// The status bar's chip while an installed update waits for the restart (a click restarts).
    pub(super) fn update_chip(&self) -> Option<StatusChip> {
        let version = self.services().updater.restart_pending()?;
        Some(StatusChip {
            text: format!("Updated to {version} — restarts when idle (Restart now)"),
            label: format!("Tuner {version} is installed and restarts when idle: restart now"),
            action: Action::RestartToUpdate,
            warning: false,
            close: None,
        })
    }
}

/// The screen a restart opens on (`--view`): the tab of the route, when it has one.
fn view_of(route: &Route) -> Option<ViewArg> {
    Some(match route {
        Route::Home => ViewArg::Home,
        Route::Live { .. } | Route::Player => ViewArg::Live,
        Route::Guide { .. } => ViewArg::Guide,
        Route::Movies { .. } | Route::MovieDetail(_) => ViewArg::Movies,
        Route::Series { .. } | Route::SeriesDetail(_) => ViewArg::Series,
        Route::Settings(_) | Route::Setting(_) | Route::EditPlaylist(_) | Route::Logs | Route::EpgMapping { .. } => ViewArg::Settings,
        Route::Onboarding(_) | Route::Queue | Route::Watchlists { .. } => return None,
    })
}
