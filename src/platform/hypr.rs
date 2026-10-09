//! Hyprland helpers (best effort, never required). Owner: WP-08.

/// Focus our own window. Omarchy 4 (Hyprland Lua config) first tries
/// `hyprctl dispatch 'hl.dsp.focus({ window = "class:^(tuner)$" })'`, then falls back to the
/// legacy `hyprctl dispatch focuswindow class:^(tuner)$` (same as `omarchy-launch-or-focus`).
/// Used by IPC handoff, MPRIS Raise and reminder "Watch"; Wayland clients cannot raise
/// themselves otherwise.
/// SKELETON SAFE DEFAULT: no-op (logged).
pub fn focus_self() {
    tracing::debug!("focus_self not implemented yet (WP-08)");
}

/// True when running under Hyprland (`HYPRLAND_INSTANCE_SIGNATURE` set).
pub fn is_hyprland() -> bool { std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() }
