//! Agent control: use Tuner from an AI agent (any MCP client) — DESIGN §30.
//!
//! ```text
//!  MCP client  ──stdio JSON-RPC (MCP)──▶ `tuner mcp` ──IPC v2 (unix socket)──▶ running Tuner
//!                                         agent::{mcp, tools, client}        ipc.rs → agent::server
//!                                         (bridge track)                     (app track)
//! ```
//! * [`proto`] — the IPC v2 wire types, params, results, refs and time formats (shared).
//! * [`crate::shared`] — what the app publishes for the handler (library snapshot, parental
//!   unlock, the guard, the pictures, Up next).
//! * [`server`] — the running app's handler (queries, play, switches, control, guardrails).
//! * [`tools`] — the MCP tool list: names, descriptions, input schemas (the model's contract).
//! * [`client`] — `tuner mcp`'s IPC v2 client: connect, launch Tuner when needed, retry while
//!   it loads.
//! * [`mcp`] — the MCP stdio server (`tuner mcp`).
//! * [`connect`] — connecting AI agents to Tuner from Tuner (the first-run card, Settings ▸ AI
//!   agents; DESIGN §39).

pub mod act;
pub mod actions;
pub mod client;
pub mod connect;
pub mod follow;
pub mod guard;
pub mod guide;
pub mod health;
pub mod library;
pub mod mcp;
pub mod multiview;
pub mod params;
pub mod proto;
pub mod query;
pub mod queue;
pub mod reminders;
pub mod server;
pub mod setup;
pub mod spoiler;
pub mod tools;
pub mod tools_batch;

use crate::model::{MediaRef, SeriesId};

/// What the handler asks the UI thread to do (`AppEvent::Agent`). The handler validated the
/// target (it exists and is visible) and raised the window when the play shows there; the app
/// re-applies the PIN rules.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    Play(AgentPlay),
    /// Up next started playing for an agent (`queue play` / `skip`), or `control window`: show
    /// it where the handler resolved (`act::agent_show`; the engine already sent the play to
    /// the main picture).
    Show(act::AgentShow),
    /// An agent's play failed while Tuner shows an empty pop-out (one started for that play,
    /// `--agent-popout`): back into the window, as the old launch was (DESIGN §30.13).
    PopIn,
    /// `control popout`: move what plays to the pop-out without taking the keyboard (MV-2).
    PopOut,
    /// `lock session_now`: lock the parental session now (the app's `ParentalSession::lock`).
    /// An agent can only ever lock: nothing here unlocks.
    LockSession,
    /// `multiview` (validated: visible live channels or catch-up, the PIN rules re-applied by
    /// the app; MV-2).
    MultiView(AgentMultiView),
    /// `guide_fix apply` of more than `guide::CHAT_APPLY_MAX` channels after the user's yes:
    /// Tuner opens Guide fixes with its confirm card prefilled; Enter applies the batch as the
    /// agent's (DESIGN §31.5).
    GuideFixReview(GuideFixReview),
}

/// An agent's proposal handed to Tuner's confirm card (taken out of the handler's proposals, so
/// it is applied there or not at all).
#[derive(Debug, Clone, PartialEq)]
pub struct GuideFixReview {
    /// Bound to the guide check it was made from (`store::guide_fix::STALE` otherwise).
    pub proposal: crate::model::Proposal,
    /// The card isn't opened after this (the screen wasn't shown in time).
    pub expires: crate::model::Timestamp,
    /// Who proposed it ("Accept Codex's 55 fixes?").
    pub agent: crate::model::AgentName,
}

/// A multi-view request (`multiview` tool, DESIGN §35 §6).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMultiView {
    /// One or two pictures (one = alongside what plays). `sound`: the index into `targets` of
    /// the picture with the sound (`None`: the sound stays where it is). `rearrange`: the user
    /// agreed that the playing picture may move to its copy in another playlist.
    Open {
        targets: Vec<MediaRef>,
        layout: Option<crate::config::MultiViewLayout>,
        sound: Option<usize>,
        rearrange: bool,
    },
    Swap(MultiViewSwap),
    /// Back to one picture: the one kept.
    Close {
        keep: MultiViewKeep,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiViewSwap {
    Sound,
    Places,
    Focus,
    Layout(crate::config::MultiViewLayout),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiViewKeep {
    A,
    B,
    /// The picture with the sound.
    Sound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentPlay {
    pub target: PlayTarget,
    /// Movies / episodes: ignore saved progress (else resume unless finished).
    pub from_start: bool,
    /// Where it shows, resolved by the handler (`act::agent_show`, DESIGN §30.13).
    pub show: act::AgentShow,
    /// `play {device}` (DESIGN §40 §3.8, §12): play it on this TV (`PlayerCommand::CastTo`
    /// with the request) instead of here. The window stays where it is (no pop-out, no raise,
    /// whatever `show` says; a popped-out window goes back in, unraised); only a PIN prompt
    /// raises it, and nothing is cast before the unlock. Set by CAST-10, played by CAST-9.
    pub device: Option<crate::model::DeviceId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    /// A channel (live, zapping through its primary group), a catch-up, a movie or an episode.
    Media(MediaRef),
    /// A series: its next episode to watch (`store::copies::next_episode` over its copies).
    Series(SeriesId),
}
