//! Agent control: use Tuner from Claude (or any MCP client) — DESIGN §30.
//!
//! ```text
//!  Claude Code ──stdio JSON-RPC (MCP)──▶ `tuner mcp` ──IPC v2 (unix socket)──▶ running Tuner
//!                                         agent::{mcp, tools, client}        ipc.rs → agent::server
//!                                         (bridge track)                     (app track)
//! ```
//! * [`proto`] — the IPC v2 wire types, params, results, refs and time formats (shared).
//! * [`shared`] — what the app publishes for the handler (library snapshot, parental unlock).
//! * [`server`] — the running app's handler (queries, play, switches, control, guardrails).
//! * [`tools`] — the MCP tool list: names, descriptions, input schemas (the model's contract).
//! * [`client`] — `tuner mcp`'s IPC v2 client: connect, launch Tuner when needed, retry while
//!   it loads.
//! * [`mcp`] — the MCP stdio server (`tuner mcp`).

pub mod act;
pub mod client;
pub mod guard;
pub mod mcp;
pub mod proto;
pub mod query;
pub mod server;
pub mod shared;
pub mod tools;

use crate::model::{MediaRef, SeriesId};

/// What the handler asks the UI thread to do (`AppEvent::Agent`). The handler validated the
/// target (it exists and is visible); the app re-applies the PIN rules and raises the window.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    Play(AgentPlay),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentPlay {
    pub target: PlayTarget,
    /// Movies / episodes: ignore saved progress (else resume unless finished).
    pub from_start: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    /// A channel (live, zapping through its primary group), a catch-up, a movie or an episode.
    Media(MediaRef),
    /// A series: its next episode to watch (`store::copies::next_episode` over its copies).
    Series(SeriesId),
}
