//! Tuner — a keyboard-first IPTV player for Omarchy.
//!
//! Crate layout mirrors `docs/DESIGN.md` §4 ("Module map"). Every module is a unit that one
//! engineer can implement in parallel with the others; cross-module contracts (types and
//! signatures) are frozen by the skeleton. Bodies marked `todo!()` belong to the work package
//! (WP) named in the module docs.
//!
//! FROZEN FILES: the single authoritative list is DESIGN §24.1 (architect sign-off to change;
//! every change re-tags `skeleton-v1.N`).
//!
//! Lints: every WP body has landed, so no module carries a stub `allow(dead_code, …)` any
//! more; the whole crate is warning-clean and must stay so (`cargo clippy --all-targets -- -D
//! warnings` gate).
#![allow(clippy::too_many_arguments)]

pub mod cli;
pub mod config;
pub mod error;
pub mod logging;
pub mod model;
pub mod msg;
pub mod paths;
pub mod redact;

pub mod ipc;
pub mod jobs;
pub mod player;
pub mod sources;
pub mod store;

pub mod fonts;
pub mod images;
pub mod keymap;
pub mod search;
pub mod theme;

pub mod platform;

pub mod agent;
pub mod app;
pub mod cast;
pub mod devshot;
pub mod services;
pub mod shared;
pub mod ui;

// the big-features batch (DESIGN §31–§35): shared blocks and feature modules
pub mod follow;
pub mod guard;
pub mod guide_fix;
pub mod playback;
pub mod queue;
pub mod reminders;
pub mod spoiler;
pub mod textmatch;

// Tuner's own updates (DESIGN §41)
pub mod update;
pub mod watchlist;

/// Application id: binary name, Wayland `app_id` (= Hyprland `class`), MPRIS bus suffix,
/// `.desktop` basename, XDG directory name.
pub const APP_ID: &str = "tuner";
/// Wayland `app_id` (Hyprland `class`) of a window started for the pop-out — a scheduled
/// switch that starts Tuner, `tuner --play <ref> --popout` — so Hyprland's window rules
/// (`packaging/hypr/tuner.lua`: float, pin, `no_initial_focus`) apply as it maps and the
/// keyboard stays where it was (DESIGN §21.6). It stays for the whole session (winit sets
/// the app id once, when it creates the window).
pub const POPOUT_APP_ID: &str = "tuner-popout";

/// The Wayland `app_id` for this launch: [`POPOUT_APP_ID`] when it starts straight into the
/// pop-out under Hyprland (elsewhere there is no pop-out, and no rule to apply), else
/// [`APP_ID`].
pub fn window_app_id(popout_launch: bool, hyprland: bool) -> &'static str {
    if popout_launch && hyprland { POPOUT_APP_ID } else { APP_ID }
}

/// Human-readable name (window title, MPRIS `Identity`, notifications).
pub const APP_NAME: &str = "Tuner";
