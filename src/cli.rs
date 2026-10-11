//! Command line. Implemented in the skeleton (clap derive).
//!
//! ```text
//! tuner [--view home|live|guide|movies|series|search|settings] [--play <source>:<item-key>]
//!       [--debug] [--profile-dir <dir>] [--refresh] [PLAYLIST_OR_URL...]
//! tuner mcp [--debug] [--profile-dir <dir>]     # MCP server on stdio (DESIGN §30)
//! ```
//! A second invocation hands its request to the running instance over the IPC socket and exits
//! (`omarchy-launch-or-focus` + omarchy-menu entries rely on this).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
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
#[command(name = "tuner", version, about = "Keyboard-first IPTV player for Omarchy", args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Screen to open (or switch to, if already running).
    #[arg(long, value_enum)]
    pub view: Option<ViewArg>,

    /// Play a channel by stable reference `<source-id>:<item-key>` (e.g. `2:xt:live:1234`).
    #[arg(long)]
    pub play: Option<String>,

    /// Verbose logging (also `RUST_LOG`).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Use an isolated profile directory (config/data/cache under it). Testing aid.
    #[arg(long, global = true)]
    pub profile_dir: Option<PathBuf>,

    /// Refresh stale sources/EPG headlessly and exit (for a future systemd timer).
    #[arg(long)]
    pub refresh: bool,

    /// M3U/M3U8 files or URLs (desktop MimeType handler `%U`): offered as a new playlist,
    /// or played directly if it is a single stream.
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Serve the Model Context Protocol on stdin/stdout so an AI assistant (Claude Code:
    /// `claude mcp add tuner -- tuner mcp`) can search the guide and your library and play
    /// things in the running Tuner — starting it when needed (DESIGN §30).
    Mcp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_is_a_subcommand_and_files_stay_inputs() {
        let cli = Cli::try_parse_from(["tuner", "mcp"]).unwrap();
        assert_eq!(cli.command, Some(Command::Mcp));
        let cli = Cli::try_parse_from(["tuner", "mcp", "--profile-dir", "/tmp/p", "--debug"]).unwrap();
        assert_eq!((cli.command, cli.profile_dir, cli.debug), (Some(Command::Mcp), Some(PathBuf::from("/tmp/p")), true));
        let cli = Cli::try_parse_from(["tuner", "list.m3u", "./mcp"]).unwrap();
        assert_eq!((cli.command, cli.inputs), (None, vec!["list.m3u".to_owned(), "./mcp".to_owned()]));
        // after a player flag, `mcp` is an input like any other word: never the bridge by accident
        let cli = Cli::try_parse_from(["tuner", "--view", "guide", "mcp"]).unwrap();
        assert_eq!((cli.command, cli.inputs), (None, vec!["mcp".to_owned()]));
    }
}
