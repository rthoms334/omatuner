//! Command line. Implemented in the skeleton (clap derive).
//!
//! ```text
//! tuner [--view home|live|guide|movies|series|search|settings] [--play <source>:<item-key> [--popout]]
//!       [--reminder <id>] [--debug] [--profile-dir <dir>] [--refresh] [PLAYLIST_OR_URL...]
//! tuner --rearm [--debug]                       # re-arm reminder timers at login (DESIGN §36.2)
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

    /// With --play: open it in the pinned pop-out without taking keyboard focus (scheduled
    /// playback, big-features additions); the in-app mini player outside Hyprland.
    #[arg(long, requires = "play")]
    pub popout: bool,

    /// Started by `tuner mcp` for an agent's play in the pop-out (DESIGN §30.13): the pop-out's
    /// window (pinned, without the keyboard; Hyprland) with nothing on yet; the play arrives
    /// over IPC v2.
    #[arg(long, hide = true)]
    pub agent_popout: bool,

    /// Fire reminder <id> now (what the transient systemd timers run while Tuner is closed,
    /// B6): Tuner looks the reminder up, and guards it, itself.
    #[arg(long, value_name = "ID")]
    pub reminder: Option<i64>,

    /// Verbose logging (also `RUST_LOG`).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Use an isolated profile directory (config/data/cache under it). Testing aid.
    #[arg(long, global = true)]
    pub profile_dir: Option<PathBuf>,

    /// Refresh stale sources/EPG headlessly and exit (for a future systemd timer).
    #[arg(long)]
    pub refresh: bool,

    /// Re-arm the reminder timers and exit: offline, no window (what the packaged
    /// tuner-rearm.service runs at every login, DESIGN §36.2). Does nothing while Tuner runs.
    #[arg(long, conflicts_with_all = ["refresh", "reminder", "play", "view", "inputs"])]
    pub rearm: bool,

    /// M3U/M3U8 files or URLs (desktop MimeType handler `%U`): offered as a new playlist,
    /// or played directly if it is a single stream.
    pub inputs: Vec<String>,
}

impl Cli {
    /// `tuner mcp`. Global flags put first (`tuner --debug mcp`, `tuner --profile-dir D mcp`) make
    /// clap read `mcp` as an input file (`args_conflicts_with_subcommands`); a lone `mcp` input
    /// with no player flag is the bridge too. `tuner ./mcp` opens a file of that name.
    pub fn is_mcp(&self) -> bool {
        self.command == Some(Command::Mcp)
            || (self.command.is_none()
                && self.inputs == ["mcp"]
                && self.view.is_none()
                && self.play.is_none()
                && self.reminder.is_none()
                && !self.refresh
                && !self.rearm)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Serve the Model Context Protocol on stdin/stdout so an AI agent (any MCP client: Claude
    /// Code, Codex, Gemini CLI, Cursor…; e.g. `claude mcp add tuner -- tuner mcp`) can search the
    /// guide and your library and play things in the running Tuner — starting it when needed
    /// (DESIGN §30).
    Mcp,
    /// Show how to connect Claude Code to Tuner (the omatuner plugin: the MCP server and its
    /// skills). `--apply` runs the plugin steps after asking on the terminal.
    ClaudeSetup {
        /// Run the steps (asks first; never without a "y").
        #[arg(long)]
        apply: bool,
    },
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
        assert!(!cli.is_mcp());
        assert_eq!((cli.command, cli.inputs), (None, vec!["mcp".to_owned()]));
        // global flags first: still the bridge
        let cli = Cli::try_parse_from(["tuner", "--debug", "mcp"]).unwrap();
        assert!(cli.is_mcp() && cli.debug);
        let cli = Cli::try_parse_from(["tuner", "--profile-dir", "/p", "mcp"]).unwrap();
        assert!(cli.is_mcp());
        assert_eq!(cli.profile_dir, Some(PathBuf::from("/p")));
        assert!(Cli::try_parse_from(["tuner", "mcp"]).unwrap().is_mcp());
        for not in [&["tuner", "./mcp"][..], &["tuner", "--refresh", "mcp"], &["tuner", "--play", "1:a", "mcp"]] {
            assert!(!Cli::try_parse_from(not).unwrap().is_mcp(), "{not:?}");
        }
    }

    /// What the reminder timers run (B6) and the pop-out launch: `--popout` only goes with
    /// `--play`.
    #[test]
    fn reminder_and_popout_flags() {
        let cli = Cli::try_parse_from(["tuner", "--reminder", "42"]).unwrap();
        assert_eq!((cli.reminder, cli.popout), (Some(42), false));
        let cli = Cli::try_parse_from(["tuner", "--play", "2:xt:live:1", "--popout"]).unwrap();
        assert!(cli.popout && cli.play.is_some());
        assert!(Cli::try_parse_from(["tuner", "--popout"]).is_err(), "nothing to pop out");
        // `tuner mcp`'s launch for an agent's play: the play itself comes over IPC v2
        let cli = Cli::try_parse_from(["tuner", "--agent-popout"]).unwrap();
        assert!(cli.agent_popout && !cli.popout && !cli.is_mcp());
    }

    /// What `tuner-rearm.service` runs at login: on its own (with the global flags), never a
    /// player request or the MCP bridge.
    #[test]
    fn rearm_flag_stands_alone() {
        let cli = Cli::try_parse_from(["tuner", "--rearm"]).unwrap();
        assert!(cli.rearm && !cli.refresh && !cli.is_mcp());
        let cli = Cli::try_parse_from(["tuner", "--rearm", "--debug", "--profile-dir", "/p"]).unwrap();
        assert!(cli.rearm && cli.debug);
        for not in [&["tuner", "--rearm", "--refresh"][..], &["tuner", "--rearm", "--reminder", "7"], &["tuner", "--rearm", "mcp"]] {
            assert!(Cli::try_parse_from(not).is_err(), "{not:?}");
        }
    }
}
