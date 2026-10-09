//! Idle inhibit while playing. Owner: WP-08. DESIGN §21.2.
//!
//! How Omarchy 4 decides (read from /usr/share/omarchy/shell/plugins/services/idle/Service.qml):
//! the Quickshell `IdleMonitor` (ext-idle-notify-v1) is created with `respectInhibitors: true`,
//! so the compositor's *Wayland idle inhibitors* are honoured: `zwp_idle_inhibit_manager_v1`
//! inhibitors on visible surfaces and Hyprland `idle_inhibit` window rules (Omarchy already
//! ships `o.window({tag="noidle"}, {idle_inhibit="always"})` and per-app `fullscreen` rules).
//! There is NO `org.freedesktop.ScreenSaver` service on the session bus and
//! xdg-desktop-portal-hyprland does not implement `Inhibit`, so D-Bus inhibition would be
//! silently ignored; `systemd-inhibit` does not affect ext-idle-notify either. The user's
//! "stay awake" toggle is a separate state file (`~/.local/state/omarchy/indicators/stay-awake`)
//! that we must not touch.
//!
//! Mechanism: create `zwp_idle_inhibitor_v1` on OUR `wl_surface`, using a wayland-client
//! connection built from eframe's foreign `wl_display` (`Backend::from_foreign_display`) and
//! the surface pointer from the window handle (`ObjectId::from_ptr`). Own event queue; no
//! interference with winit's queue. Inhibitor exists while `PlayerState::is_active()` and the
//! config mode allows (WhilePlaying / Fullscreen); destroyed otherwise. The compositor only
//! honours it while the surface is visible — correct behaviour for a video player.
//! Fallback (documented in the Hyprland snippet): `idle_inhibit = "fullscreen"` window rule.

use crate::config::IdleInhibitMode;
use crate::error::Result;

pub struct IdleInhibitor {
    // WP-08: wayland connection, queue, manager proxy, Option<inhibitor proxy>
    active: bool,
}

impl IdleInhibitor {
    /// Bind `zwp_idle_inhibit_manager_v1` from the window's display/surface handles.
    /// Returns Err when not on Wayland or the protocol is missing (caller logs, continues).
    /// SKELETON SAFE DEFAULT: `Err` (logged; no inhibition).
    pub fn new(display: raw_window_handle::RawDisplayHandle, window: raw_window_handle::RawWindowHandle) -> Result<Self> {
        Err(crate::error::Error::other("idle inhibitor not implemented yet (WP-08)"))
    }

    /// Apply the desired state (idempotent; called every `logic` frame — must be cheap).
    pub fn set(&mut self, mode: IdleInhibitMode, playing: bool, fullscreen: bool) {}

    pub fn is_active(&self) -> bool { self.active }
}
