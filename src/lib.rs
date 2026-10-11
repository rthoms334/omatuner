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
pub mod devshot;
pub mod services;
pub mod ui;

/// Application id: binary name, Wayland `app_id` (= Hyprland `class`), MPRIS bus suffix,
/// `.desktop` basename, XDG directory name.
pub const APP_ID: &str = "tuner";
/// Human-readable name (window title, MPRIS `Identity`, notifications).
pub const APP_NAME: &str = "Tuner";
