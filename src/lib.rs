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
//! Lints: modules that still contain WP stub bodies carry a scoped
//! `allow(dead_code, unused_variables, unused_imports)` below. A WP removes the allow of its
//! module when its bodies land; WP-13 removes whatever is left. Skeleton-only modules are
//! warning-clean and must stay so (`cargo clippy --all-targets -- -D warnings` gate).
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

pub mod app;
pub mod services;
#[allow(dead_code, unused_variables, unused_imports)] // WP stubs (see crate docs)
pub mod ui;

/// Application id: binary name, Wayland `app_id` (= Hyprland `class`), MPRIS bus suffix,
/// `.desktop` basename, XDG directory name.
pub const APP_ID: &str = "tuner";
/// Human-readable name (window title, MPRIS `Identity`, notifications).
pub const APP_NAME: &str = "Tuner";
