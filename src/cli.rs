//! Command line. Implemented in the skeleton (clap derive).
//!
//! ```text
//! tuner [--view home|live|guide|movies|series|search|settings] [--play <source>:<item-key>]
//!       [--debug] [--profile-dir <dir>] [--refresh] [PLAYLIST_OR_URL...]
//! ```
//! A second invocation hands its request to the running instance over the IPC socket and exits
//! (`omarchy-launch-or-focus` + omarchy-menu entries rely on this).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewArg {
    Home,
    Live,
    Guide,
    Movies,
    Series,
    Search,
    Settings,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "tuner", version, about = "Keyboard-first IPTV player for Omarchy")]
pub struct Cli {
    /// Screen to open (or switch to, if already running).
    #[arg(long, value_enum)]
    pub view: Option<ViewArg>,

    /// Play a channel by stable reference `<source-id>:<item-key>` (e.g. `2:xt:live:1234`).
    #[arg(long)]
    pub play: Option<String>,

    /// Verbose logging (also `RUST_LOG`).
    #[arg(long)]
    pub debug: bool,

    /// Use an isolated profile directory (config/data/cache under it). Testing aid.
    #[arg(long)]
    pub profile_dir: Option<PathBuf>,

    /// Refresh stale sources/EPG headlessly and exit (for a future systemd timer).
    #[arg(long)]
    pub refresh: bool,

    /// M3U/M3U8 files or URLs (desktop MimeType handler `%U`): offered as a new playlist,
    /// or played directly if it is a single stream.
    pub inputs: Vec<String>,
}
