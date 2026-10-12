//! Remote output: the controller's side of casting (DESIGN §40 §5.1, §5.5, §11.2). Casting is an
//! output of picture A's controller, not a new player: `load_now_by` hands the `Playable` to a
//! [`RemoteOutput`] instead of `loadfile`, transport commands are forwarded, and receiver /
//! relay status comes back as [`RemoteEvent`]s that the controller turns into the existing
//! `state::Input`s, so `PlayerMachine` stays the single source of truth.
//!
//! The player defines this contract; `crate::cast` implements it (dependencies point from `cast`
//! to `player`, never back). OWNERS: the contract is frozen (CAST-0); CAST-5 wrote the pure
//! `RemoteEvent → state::Input` adapter ([`RemoteAdapter`]) and the controller's use of it.

use std::sync::Arc;
use std::time::Instant;

use super::events::EndReason;
use super::state::Input;
use super::{PlayerError, PlayerErrorKind};
use crate::model::{CastErrorKind, CastHints, CastLevel, CastPhase, CastView, DeviceId, Playable};

/// Opens cast sessions for the controller (`PlayerDeps::cast`). Implemented by the cast service
/// (through `AppShared`, so the service can start after the player and restart on settings
/// changes); tests inject fakes.
pub trait CastConnector: Send + Sync {
    /// Non-blocking: returns at once; the session connects on the runtime and reports `Phase` /
    /// `Lost`. `Err` when casting is off or the device is unknown or not paired.
    fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError>;
}

/// A session as the controller holds it while the output is remote.
#[derive(Debug)]
pub struct RemoteOutput {
    /// To the session task (never blocks the controller thread).
    pub cmd: tokio::sync::mpsc::UnboundedSender<RemoteCmd>,
    /// From the session (a fifth `select!` arm in `Controller::run`).
    pub events: crossbeam_channel::Receiver<RemoteEvent>,
    /// The initial view (phase `Connecting`) for `PlayerSnapshot::cast`.
    pub view: CastView,
}

/// Controller → session.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteCmd {
    /// Play `playable` as load generation `generation` (first load, zap, reconnect, VOD restart).
    Load {
        generation: u64,
        playable: Arc<Playable>,
        hints: CastHints,
    },
    SetPaused(bool),
    /// Absolute seconds into the item (a replace with a playlist view, or a restart).
    SeekTo(f64),
    /// 0–130 (Tuner's scale); AirPlay ignores it ("Use the TV remote for volume").
    SetVolume(f64),
    /// Stop playback on the TV (the session stays for the next `Load`).
    Stop,
    /// End the session; `fast` at shutdown (≤ 300 ms, no waiting for the upstream close).
    Close {
        fast: bool,
    },
}

/// Session → controller (DESIGN §40 §11.2 maps each to `state::Input`s).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    Phase(CastPhase),
    Level(CastLevel),
    UpstreamOpened {
        generation: u64,
    },
    /// The relay's upstream connection closed: the controller releases A's lease and records
    /// when (the 45 s race window covers relay reconnects too).
    UpstreamClosed {
        generation: u64,
        at: Instant,
    },
    /// A real HTTP status from the provider.
    Http {
        generation: u64,
        status: u16,
    },
    /// The provider's text where the stream should be (redacted).
    ProviderText {
        generation: u64,
        text: String,
    },
    /// The upstream ended (EOF): `EndFile { reason: Eof }`. Live: `StreamEnded`, then the
    /// reconnect policy; VOD: wait for the receiver's `Ended`.
    UpstreamEof {
        generation: u64,
    },
    /// The relay's probe of this generation (codec, size, fps, audio): the controller keeps it
    /// for `snapshot.stream_info` while the info panel is open (`RefreshStreamInfo`, §11.3).
    StreamInfo {
        generation: u64,
        info: Box<super::tracks::StreamInfo>,
    },
    /// Live: roughly how far the TV runs behind the live point (`CastView::behind_live_s`);
    /// `None` for catch-up and VOD.
    BehindLive(Option<u16>),
    /// The first is the first picture.
    Playing {
        generation: u64,
    },
    Paused(bool),
    Buffering(bool),
    /// Absolute (the session added the playlist's offset).
    Position {
        generation: u64,
        pos: f64,
        dur: Option<f64>,
    },
    /// The item played to its end (VOD: `Ended`, then Up next).
    Ended {
        generation: u64,
    },
    Failed {
        generation: u64,
        kind: PlayerErrorKind,
    },
    /// The session is gone; `StoppedOnTv` ends casting quietly.
    Lost(CastErrorKind),
}

/// mpv's `MPV_ERROR_LOADING_FAILED`: an upstream that closed before its end is the END_FILE
/// error mpv gives a dropped connection (`state::classify` → `Network`, or the HTTP status
/// seen while loading).
const LOADING_FAILED: i32 = -13;

/// The pure `RemoteEvent → state::Input` adapter (DESIGN §40 §11.2): the session's events
/// become the inputs mpv produces today, so `PlayerMachine` stays the single source of truth.
/// Entry ids are load generations. It keeps what the table needs per generation: whether the
/// upstream opened (`StartFile`), whether the TV showed its first picture (the first `Playing`
/// is `FileLoaded` + `PlaybackRestart`, later ones end a pause or a buffering), whether the
/// upstream reached its end (a close after EOF is not a failure), and whether an end was
/// already delivered (one `EndFile` or failure per generation).
///
/// | `RemoteEvent` | Input(s) |
/// |---|---|
/// | `UpstreamOpened` | `StartFile` |
/// | first `Playing` | (`StartFile` if the upstream wasn't reported) `FileLoaded`, `PlaybackRestart` |
/// | later `Playing` | `Pause(false)`, `PausedForCache(false)`: the TV plays (its own remote resumes with `playing`) |
/// | `Buffering(b)` / `Paused(b)` | `PausedForCache(b)` / `Pause(b)` |
/// | `Position` | `TimePos` (absolute) |
/// | `Http` / `ProviderText` | `HttpStatus` / `ProviderMessage` |
/// | `UpstreamEof` | live: `EndFile(Eof)` (`StreamEnded`, reconnect); VOD: none (the TV plays on to `Ended`) |
/// | `UpstreamClosed` before EOF | `EndFile(Error)`: with the `Http` status seen while loading, the same classification as mpv |
/// | `Ended` | `EndFile(Eof)` |
/// | `Failed { kind }`, `Lost(kind)` | `Failed { kind }` / `Failed { Cast(kind) }` (the reconnect policy) |
/// | `Lost(StoppedOnTv)` | none: the controller ends casting quietly |
///
/// `Phase`, `Level`, `StreamInfo` and `BehindLive` only change the view (the controller), and
/// events of an older generation (a zap's late replies) give nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteAdapter {
    /// The generation loaded last (`None` before the first load).
    current: Option<u64>,
    live: bool,
    opened: bool,
    playing: bool,
    eof: bool,
    ended: bool,
}

impl RemoteAdapter {
    /// The controller sent `RemoteCmd::Load { generation }`; `live`: a sliding-window item,
    /// whose upstream EOF is the stream's end. Events of earlier generations are stale now.
    pub fn loaded(&mut self, generation: u64, live: bool) {
        *self = Self { current: Some(generation), live, ..Self::default() };
    }

    /// The generation loaded last.
    pub fn generation(&self) -> Option<u64> {
        self.current
    }

    /// The generation's upstream reported its end (`UpstreamEof`).
    pub fn at_eof(&self) -> bool {
        self.eof
    }

    /// The inputs `ev` means for the machine, in order.
    pub fn inputs(&mut self, ev: &RemoteEvent) -> Vec<Input> {
        use RemoteEvent as E;
        let Some(cur) = self.current else { return Vec::new() };
        let entry = cur as i64;
        let ours = |g: &u64| *g == cur;
        match ev {
            E::UpstreamOpened { generation } if ours(generation) => {
                self.opened = true;
                vec![Input::StartFile { entry }]
            }
            E::Http { generation, status } if ours(generation) => vec![Input::HttpStatus(*status)],
            E::ProviderText { generation, text } if ours(generation) => vec![Input::ProviderMessage(text.clone())],
            E::UpstreamEof { generation } if ours(generation) => {
                self.eof = true;
                if self.live { self.end(Input::EndFile { reason: EndReason::Eof, error: 0, entry }) } else { Vec::new() }
            }
            E::UpstreamClosed { generation, .. } if ours(generation) && !self.eof => {
                self.end(Input::EndFile { reason: EndReason::Error, error: LOADING_FAILED, entry })
            }
            E::Playing { generation } if ours(generation) => {
                if std::mem::replace(&mut self.playing, true) {
                    return vec![Input::Pause(false), Input::PausedForCache(false)];
                }
                let mut v = Vec::with_capacity(3);
                if !std::mem::replace(&mut self.opened, true) {
                    v.push(Input::StartFile { entry });
                }
                v.extend([Input::FileLoaded, Input::PlaybackRestart]);
                v
            }
            E::Paused(p) => vec![Input::Pause(*p)],
            E::Buffering(b) => vec![Input::PausedForCache(*b)],
            E::Position { generation, pos, .. } if ours(generation) => vec![Input::TimePos(*pos)],
            E::Ended { generation } if ours(generation) => self.end(Input::EndFile { reason: EndReason::Eof, error: 0, entry }),
            E::Failed { generation, kind } if ours(generation) => self.end(Input::Failed { entry, kind: *kind }),
            E::Lost(CastErrorKind::StoppedOnTv) => Vec::new(),
            E::Lost(kind) => self.end(Input::Failed { entry, kind: PlayerErrorKind::Cast(*kind) }),
            _ => Vec::new(),
        }
    }

    /// One end per generation: the first of EOF, a failure or a lost session decides.
    fn end(&mut self, input: Input) -> Vec<Input> {
        if std::mem::replace(&mut self.ended, true) { Vec::new() } else { vec![input] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(generation: u64, live: bool) -> RemoteAdapter {
        let mut a = RemoteAdapter::default();
        a.loaded(generation, live);
        a
    }

    /// DESIGN §40 §11.2, row by row: a first play, a pause on the TV and its own remote's
    /// resume, buffering and positions.
    #[test]
    fn a_play_on_the_tv_is_the_input_sequence_mpv_gives() {
        let mut a = adapter(7, true);
        assert_eq!(a.inputs(&RemoteEvent::Phase(CastPhase::Starting)), vec![], "the view only");
        assert_eq!(a.inputs(&RemoteEvent::UpstreamOpened { generation: 7 }), vec![Input::StartFile { entry: 7 }]);
        assert_eq!(a.inputs(&RemoteEvent::Playing { generation: 7 }), vec![Input::FileLoaded, Input::PlaybackRestart], "the first picture");
        assert_eq!(a.inputs(&RemoteEvent::Buffering(true)), vec![Input::PausedForCache(true)]);
        assert_eq!(a.inputs(&RemoteEvent::Playing { generation: 7 }), vec![Input::Pause(false), Input::PausedForCache(false)]);
        assert_eq!(a.inputs(&RemoteEvent::Paused(true)), vec![Input::Pause(true)]);
        assert_eq!(a.inputs(&RemoteEvent::Position { generation: 7, pos: 12.5, dur: None }), vec![Input::TimePos(12.5)]);
        // the TV played without the relay reporting its upstream (still bound to the generation)
        let mut a = adapter(8, false);
        assert_eq!(a.inputs(&RemoteEvent::Playing { generation: 8 }), vec![Input::StartFile { entry: 8 }, Input::FileLoaded, Input::PlaybackRestart]);
    }

    /// A refused stream: the status, then the upstream's end, exactly as mpv logs "HTTP error
    /// 403" before its END_FILE (so the 403 retry and the race window apply as they do here).
    #[test]
    fn a_status_then_the_upstream_closing_is_an_end_file_error() {
        let mut a = adapter(3, true);
        assert_eq!(a.inputs(&RemoteEvent::UpstreamOpened { generation: 3 }), vec![Input::StartFile { entry: 3 }]);
        assert_eq!(a.inputs(&RemoteEvent::Http { generation: 3, status: 403 }), vec![Input::HttpStatus(403)]);
        let closed = RemoteEvent::UpstreamClosed { generation: 3, at: Instant::now() };
        assert_eq!(a.inputs(&closed), vec![Input::EndFile { reason: EndReason::Error, error: LOADING_FAILED, entry: 3 }]);
        assert_eq!(super::super::state::classify(LOADING_FAILED, Some(403)), PlayerErrorKind::HttpForbidden);
        assert_eq!(super::super::state::classify(LOADING_FAILED, None), PlayerErrorKind::Network, "a dropped upstream");
        assert_eq!(a.inputs(&RemoteEvent::Failed { generation: 3, kind: PlayerErrorKind::Network }), vec![], "one end per generation");
        let mut a = adapter(4, true);
        let text = RemoteEvent::ProviderText { generation: 4, text: "Max connections reached".into() };
        assert_eq!(a.inputs(&text), vec![Input::ProviderMessage("Max connections reached".into())]);
    }

    /// Live: the upstream's end is the stream's end (`StreamEnded`, then the reconnect policy).
    /// VOD: the relay has read the whole film long before the TV shows its end; the TV's
    /// `itemPlayedToEnd` ends it, and the upstream closing after EOF is no failure.
    #[test]
    fn upstream_eof_ends_live_and_waits_for_the_tv_on_vod() {
        let eof = |g| RemoteEvent::UpstreamEof { generation: g };
        let closed = |g| RemoteEvent::UpstreamClosed { generation: g, at: Instant::now() };
        let mut live = adapter(5, true);
        assert_eq!(live.inputs(&eof(5)), vec![Input::EndFile { reason: EndReason::Eof, error: 0, entry: 5 }]);
        assert_eq!(live.inputs(&closed(5)), vec![]);
        let mut vod = adapter(6, false);
        assert_eq!(vod.inputs(&eof(6)), vec![]);
        assert!(vod.at_eof());
        assert_eq!(vod.inputs(&closed(6)), vec![], "read to the end, not dropped");
        assert_eq!(vod.inputs(&RemoteEvent::Ended { generation: 6 }), vec![Input::EndFile { reason: EndReason::Eof, error: 0, entry: 6 }]);
    }

    /// A zap's late replies never move the new generation; a lost session fails the current
    /// one through the reconnect policy, except `StoppedOnTv`, which ends casting quietly.
    #[test]
    fn stale_generations_are_ignored_and_lost_sessions_fail_the_current_one() {
        let mut a = adapter(9, true);
        a.loaded(10, true);
        assert_eq!(a.generation(), Some(10));
        for ev in [
            RemoteEvent::UpstreamOpened { generation: 9 },
            RemoteEvent::Playing { generation: 9 },
            RemoteEvent::Http { generation: 9, status: 404 },
            RemoteEvent::UpstreamClosed { generation: 9, at: Instant::now() },
            RemoteEvent::Ended { generation: 9 },
            RemoteEvent::Position { generation: 9, pos: 1.0, dur: None },
            RemoteEvent::Failed { generation: 9, kind: PlayerErrorKind::Cast(CastErrorKind::ReceiverFailed) },
        ] {
            assert_eq!(a.inputs(&ev), vec![], "{ev:?}");
        }
        assert_eq!(a.inputs(&RemoteEvent::Lost(CastErrorKind::StoppedOnTv)), vec![]);
        let lost = a.inputs(&RemoteEvent::Lost(CastErrorKind::Unreachable));
        assert_eq!(lost, vec![Input::Failed { entry: 10, kind: PlayerErrorKind::Cast(CastErrorKind::Unreachable) }]);
        let failed = adapter(11, false).inputs(&RemoteEvent::Failed { generation: 11, kind: PlayerErrorKind::Cast(CastErrorKind::Firewall) });
        assert_eq!(failed, vec![Input::Failed { entry: 11, kind: PlayerErrorKind::Cast(CastErrorKind::Firewall) }]);
        assert_eq!(RemoteAdapter::default().inputs(&RemoteEvent::Paused(true)), vec![], "nothing loaded yet");
    }
}
