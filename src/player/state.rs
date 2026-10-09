//! Player lifecycle state machine — pure (no mpv, no threads, injected `now`), so it is unit
//! testable with scripted inputs. Owner: WP-07. DESIGN §11.3.
//!
//! ```text
//!            Play(delay)              tick ≥ until
//!  Idle ─────────────────▶ Zapping ───────────────▶ Loading ◀──────────────┐
//!   ▲   Play(immediate) ───────────────────────────▶   │ PlaybackRestart   │ tick ≥ at
//!   │                                                  ▼                   │
//!   │ Stop            Pause(true)               ┌── Playing ──┐       Reconnecting
//!   ├──────────────── Paused ◀─────────────────▶│             │ fail ─────▲  │ give up
//!   │                                           └─ Buffering ◀┘ (EndFile err/EOF live,
//!   │                                         paused-for-cache   load timeout, stall)  ▼
//!   │                     EndFile EOF (VOD) ──▶ Ended                              Error
//!   └──────────────────────────────────────────────────────────── Retry ◀──────────┘
//! ```
//! Entry tracking: after every `loadfile` the controller reads the new entry's id
//! (`playlist/0/id`, synchronous — the controller is the only playlist writer) and feeds
//! `Input::Loaded { entry }`. START_FILE / END_FILE are matched against that id, and
//! PLAYBACK_RESTART is accepted only after START_FILE of the expected entry — so a fast
//! A→B switch can never bind A's events to B.
//! HTTP statuses from log lines classify failures only while Loading (a 404 storm of HLS
//! segments while Playing is a network problem, not "channel offline"); cleared on
//! PlaybackRestart.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::events::EndReason;
use super::reconnect::ReconnectPolicy;
use super::{PlayerError, PlayerErrorKind, PlayerEvent, PlayerState};
use crate::config::Config;
use crate::model::Playable;

#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub zap_delay: Duration,
    /// No first frame within this → Timeout failure.
    pub load_timeout: Duration,
    pub buffer_timeout_live: Duration,
    pub buffer_timeout_vod: Duration,
    /// Live: time-pos frozen this long while "playing" → Stalled.
    pub stall_timeout: Duration,
    pub policy: ReconnectPolicy,
}

impl MachineConfig {
    pub fn from_config(cfg: &Config) -> Self {
        let p = &cfg.player;
        Self {
            zap_delay: Duration::from_millis(p.zap_delay_ms as u64),
            load_timeout: Duration::from_secs(p.network_timeout_s as u64 + 5),
            buffer_timeout_live: Duration::from_secs(15),
            buffer_timeout_vod: Duration::from_secs(30),
            stall_timeout: Duration::from_secs(12),
            policy: ReconnectPolicy { max_attempts_live: p.reconnect_max_attempts, ..ReconnectPolicy::default() },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Play { target: Arc<Playable>, immediate: bool },
    Stop,
    /// User retry from Error.
    Retry,
    /// Budget/mpv-command failure for the CURRENT target, reported by the controller.
    Fail(PlayerError),
    /// The requested media could not be resolved. Does NOT touch the current target: whatever
    /// plays keeps playing (or Idle → Error); nothing is recorded in the failure cache.
    ResolveFailed(PlayerError),
    /// `loadfile` issued; `entry` = its playlist entry id (`None` if it couldn't be read →
    /// the first START_FILE is accepted).
    Loaded { entry: Option<i64> },
    StartFile { entry: i64 },
    FileLoaded,
    PlaybackRestart,
    EndFile { reason: EndReason, error: i32, entry: i64 },
    Pause(bool),
    PausedForCache(bool),
    BufferingPercent(i64),
    TimePos(f64),
    /// From mpv/ffmpeg log lines ("HTTP error 403 Forbidden").
    HttpStatus(u16),
    Tick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// `loadfile` this (its `start_at` already adjusted for VOD resume-after-reconnect).
    Load(Arc<Playable>),
    StopMpv,
    Emit(PlayerEvent),
    /// Persist VOD/catch-up progress (`final_` = on stop/switch/end).
    SaveProgress { final_: bool },
    PushHistory(Arc<Playable>),
    RecordFailure(Arc<Playable>, PlayerErrorKind),
    ClearFailure(Arc<Playable>),
}

#[derive(Debug)]
pub struct PlayerMachine {
    cfg: MachineConfig,
    state: PlayerState,
    target: Option<Arc<Playable>>,
    /// Entry id of the file we loaded last (from `Input::Loaded`).
    expected_entry: Option<i64>,
    /// Set once START_FILE of the expected entry arrived.
    current_entry: Option<i64>,
    awaiting_start: bool,
    /// `Loaded` arrived without an id: accept the next START_FILE.
    accept_any_start: bool,
    attempt: u32,
    playing_since: Option<Instant>,
    last_pos: Option<f64>,
    last_pos_change: Option<Instant>,
    last_http: Option<u16>,
    history_pushed: bool,
    stop_requested: bool,
}

impl PlayerMachine {
    pub fn new(cfg: MachineConfig) -> Self {
        Self {
            cfg,
            state: PlayerState::Idle,
            target: None,
            expected_entry: None,
            current_entry: None,
            awaiting_start: false,
            accept_any_start: false,
            attempt: 0,
            playing_since: None,
            last_pos: None,
            last_pos_change: None,
            last_http: None,
            history_pushed: false,
            stop_requested: false,
        }
    }

    pub fn state(&self) -> &PlayerState { &self.state }
    pub fn target(&self) -> Option<&Arc<Playable>> { self.target.as_ref() }
    pub fn last_position(&self) -> Option<f64> { self.last_pos }
    pub fn set_config(&mut self, cfg: MachineConfig) { self.cfg = cfg; }

    /// Feed one input; returns effects for the controller to execute in order.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        match input {
            Input::Play { target, immediate } => {
                if self.is_seekable_active() {
                    fx.push(Effect::SaveProgress { final_: true });
                }
                self.target = Some(target.clone());
                self.attempt = 0;
                self.history_pushed = false;
                self.last_http = None;
                self.last_pos = None;
                self.last_pos_change = None;
                self.stop_requested = false;
                fx.push(Effect::Emit(PlayerEvent::NowPlaying(target)));
                if !immediate && !self.cfg.zap_delay.is_zero() {
                    self.set_state(PlayerState::Zapping { until: now + self.cfg.zap_delay }, &mut fx);
                } else {
                    self.begin_load(now, &mut fx);
                }
            }
            Input::Stop => {
                if self.is_seekable_active() {
                    fx.push(Effect::SaveProgress { final_: true });
                }
                self.stop_requested = true;
                self.clear_entries();
                fx.push(Effect::StopMpv);
                self.target = None;
                self.set_state(PlayerState::Idle, &mut fx);
            }
            Input::Retry => {
                if matches!(self.state, PlayerState::Error(_) | PlayerState::Reconnecting { .. }) && self.target.is_some() {
                    self.attempt = 0;
                    self.begin_load(now, &mut fx);
                }
            }
            Input::ResolveFailed(err) => {
                fx.push(Effect::Emit(PlayerEvent::Failed(err.clone())));
                if matches!(self.state, PlayerState::Idle | PlayerState::Ended | PlayerState::Error(_)) {
                    self.target = None;
                    self.set_state(PlayerState::Error(err), &mut fx);
                }
            }
            Input::Loaded { entry } => {
                self.expected_entry = entry;
                self.accept_any_start = entry.is_none();
            }
            Input::Fail(err) => {
                fx.push(Effect::StopMpv);
                self.clear_entries();
                if let Some(t) = &self.target {
                    fx.push(Effect::RecordFailure(t.clone(), err.kind));
                }
                fx.push(Effect::Emit(PlayerEvent::Failed(err.clone())));
                self.set_state(PlayerState::Error(err), &mut fx);
            }
            Input::StartFile { entry } => {
                if self.awaiting_start && (self.expected_entry == Some(entry) || self.accept_any_start) {
                    self.current_entry = Some(entry);
                    self.expected_entry = Some(entry);
                    self.awaiting_start = false;
                    self.accept_any_start = false;
                }
            }
            Input::FileLoaded => {}
            Input::PlaybackRestart => match self.state {
                // only once START_FILE of the file WE loaded arrived (a late restart of the
                // replaced file must not mark the new one as playing)
                PlayerState::Loading { .. } if self.current_entry.is_some() => {
                    self.last_http = None;
                    self.playing_since = Some(now);
                    self.last_pos_change = Some(now);
                    if let Some(t) = self.target.clone() {
                        fx.push(Effect::ClearFailure(t.clone()));
                        if !self.history_pushed {
                            self.history_pushed = true;
                            fx.push(Effect::PushHistory(t));
                        }
                    }
                    self.set_state(PlayerState::Playing, &mut fx);
                }
                PlayerState::Buffering { .. } => self.set_state(PlayerState::Playing, &mut fx),
                _ => {}
            },
            Input::EndFile { reason, error, entry } => {
                if self.current_entry != Some(entry) && self.expected_entry != Some(entry) {
                    return fx; // the file we replaced, or a stop we issued ourselves
                }
                let loading = matches!(self.state, PlayerState::Loading { .. });
                self.clear_entries();
                let live = self.target.as_ref().map(|t| t.kind.is_live()).unwrap_or(true);
                match reason {
                    EndReason::Stop | EndReason::Quit | EndReason::Redirect => {}
                    EndReason::Eof if !live => {
                        fx.push(Effect::SaveProgress { final_: true });
                        let item = self.target.as_ref().and_then(|t| t.item.clone());
                        fx.push(Effect::Emit(PlayerEvent::Ended { item, finished: true }));
                        self.set_state(PlayerState::Ended, &mut fx);
                    }
                    EndReason::Eof => self.fail(PlayerErrorKind::StreamEnded, now, &mut fx),
                    EndReason::Error => {
                        let kind = classify(error, if loading { self.last_http } else { None });
                        self.fail(kind, now, &mut fx)
                    }
                    EndReason::Unknown => self.fail(PlayerErrorKind::Mpv, now, &mut fx),
                }
            }
            Input::Pause(p) => match (&self.state, p) {
                (PlayerState::Playing, true) => self.set_state(PlayerState::Paused, &mut fx),
                (PlayerState::Paused, false) => {
                    self.last_pos_change = Some(now);
                    self.set_state(PlayerState::Playing, &mut fx)
                }
                _ => {}
            },
            Input::PausedForCache(b) => match (&self.state, b) {
                (PlayerState::Playing, true) => self.set_state(PlayerState::Buffering { since: now, percent: None }, &mut fx),
                (PlayerState::Buffering { .. }, false) => {
                    self.last_pos_change = Some(now);
                    self.set_state(PlayerState::Playing, &mut fx)
                }
                _ => {}
            },
            Input::BufferingPercent(p) => {
                if let PlayerState::Buffering { since, .. } = self.state {
                    self.state = PlayerState::Buffering { since, percent: Some(p) };
                }
            }
            Input::TimePos(t) => {
                if self.last_pos.map(|p| (p - t).abs() > 0.01).unwrap_or(true) {
                    self.last_pos_change = Some(now);
                }
                self.last_pos = Some(t);
            }
            Input::HttpStatus(code) => {
                if matches!(self.state, PlayerState::Loading { .. }) {
                    self.last_http = Some(code);
                }
            }
            Input::Tick => self.tick(now, &mut fx),
        }
        fx
    }

    fn tick(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let live = self.target.as_ref().map(|t| t.kind.is_live()).unwrap_or(true);
        match self.state.clone() {
            PlayerState::Zapping { until } if now >= until => self.begin_load(now, fx),
            PlayerState::Loading { since, .. } if now.duration_since(since) > self.cfg.load_timeout => {
                self.fail(PlayerErrorKind::Timeout, now, fx)
            }
            PlayerState::Buffering { since, .. } => {
                let limit = if live { self.cfg.buffer_timeout_live } else { self.cfg.buffer_timeout_vod };
                if now.duration_since(since) > limit {
                    self.fail(PlayerErrorKind::Stalled, now, fx);
                }
            }
            PlayerState::Playing => {
                if live
                    && let Some(at) = self.last_pos_change
                        && self.last_pos.is_some() && now.duration_since(at) > self.cfg.stall_timeout {
                            self.fail(PlayerErrorKind::Stalled, now, fx);
                            return;
                        }
                if self.attempt > 0
                    && let Some(s) = self.playing_since
                        && now.duration_since(s) >= self.cfg.policy.stable_after {
                            self.attempt = 0;
                        }
            }
            PlayerState::Reconnecting { at, .. } if now >= at => self.begin_load(now, fx),
            _ => {}
        }
    }

    fn begin_load(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let Some(t) = self.target.clone() else { return };
        // VOD/catch-up reconnects resume where playback stopped
        let t = match (self.attempt > 0 && !t.kind.is_live(), self.last_pos) {
            (true, Some(pos)) => Arc::new(Playable { start_at: Some((pos - 2.0).max(0.0)), ..(*t).clone() }),
            _ => t,
        };
        self.clear_entries();
        self.awaiting_start = true;
        self.last_http = None;
        self.playing_since = None;
        fx.push(Effect::Load(t));
        self.set_state(PlayerState::Loading { since: now, attempt: self.attempt }, fx);
    }

    fn fail(&mut self, kind: PlayerErrorKind, now: Instant, fx: &mut Vec<Effect>) {
        let live = self.target.as_ref().map(|t| t.kind.is_live()).unwrap_or(true);
        let next = self.attempt + 1;
        fx.push(Effect::StopMpv);
        self.clear_entries();
        match self.cfg.policy.delay(next, kind, live) {
            Some(d) => {
                self.attempt = next;
                let max = if live { self.cfg.policy.max_attempts_live } else { self.cfg.policy.max_attempts_vod };
                self.set_state(PlayerState::Reconnecting { attempt: next, max, at: now + d, reason: kind }, fx);
            }
            None => {
                let err = PlayerError { kind, message: describe(kind).to_owned() };
                if let Some(t) = &self.target {
                    fx.push(Effect::RecordFailure(t.clone(), kind));
                }
                fx.push(Effect::Emit(PlayerEvent::Failed(err.clone())));
                self.set_state(PlayerState::Error(err), fx);
            }
        }
    }

    fn clear_entries(&mut self) {
        self.expected_entry = None;
        self.current_entry = None;
        self.awaiting_start = false;
        self.accept_any_start = false;
    }

    fn set_state(&mut self, s: PlayerState, fx: &mut Vec<Effect>) {
        if std::mem::discriminant(&s) != std::mem::discriminant(&self.state) || s != self.state {
            self.state = s.clone();
            fx.push(Effect::Emit(PlayerEvent::StateChanged(s)));
        }
    }

    fn is_seekable_active(&self) -> bool {
        self.target.as_ref().map(|t| t.kind.is_seekable()).unwrap_or(false)
            && matches!(self.state, PlayerState::Playing | PlayerState::Paused | PlayerState::Buffering { .. })
    }
}

/// Map an mpv END_FILE error (+ last HTTP status seen in logs) to a failure kind.
pub fn classify(mpv_error: i32, http: Option<u16>) -> PlayerErrorKind {
    match http {
        Some(401) | Some(403) => return PlayerErrorKind::HttpForbidden,
        Some(404) | Some(410) => return PlayerErrorKind::HttpNotFound,
        Some(c) if c >= 400 => return PlayerErrorKind::HttpOther(c),
        _ => {}
    }
    match mpv_error {
        -13 => PlayerErrorKind::Network,                            // LOADING_FAILED
        -18..=-16 => PlayerErrorKind::Unsupported,            // NOTHING_TO_PLAY / UNKNOWN_FORMAT / UNSUPPORTED
        _ => PlayerErrorKind::Mpv,
    }
}

/// User-facing copy for the error overlay.
pub fn describe(kind: PlayerErrorKind) -> &'static str {
    match kind {
        PlayerErrorKind::Network => "Connection lost",
        PlayerErrorKind::Timeout => "The stream didn't start in time",
        PlayerErrorKind::Stalled => "The stream stopped sending data",
        PlayerErrorKind::StreamEnded => "The stream ended",
        PlayerErrorKind::HttpForbidden => "The provider refused the stream (account expired or too many connections)",
        PlayerErrorKind::HttpNotFound => "This channel is offline",
        PlayerErrorKind::HttpOther(_) => "The server returned an error",
        PlayerErrorKind::Unsupported => "This stream format isn't supported",
        PlayerErrorKind::Drm => "This channel is DRM-protected and can't be played",
        PlayerErrorKind::Resolve => "Couldn't build the stream address",
        PlayerErrorKind::Budget => "Connection limit reached for this provider",
        PlayerErrorKind::Mpv => "Playback failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MediaRef, PlayableKind};
    use crate::redact::Secret;

    fn live() -> Arc<Playable> {
        Arc::new(Playable {
            media: MediaRef::Url("x".into()),
            kind: PlayableKind::Live,
            item: None,
            source_id: None,
            title: "Ch".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: Secret::new("http://x".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        })
    }

    fn machine() -> PlayerMachine { PlayerMachine::new(MachineConfig::from_config(&Config::default())) }

    #[test]
    fn zap_delay_then_load_then_play() {
        let mut m = machine();
        let t0 = Instant::now();
        let fx = m.handle(Input::Play { target: live(), immediate: false }, t0);
        assert!(matches!(m.state(), PlayerState::Zapping { .. }));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Load(_))));
        let fx = m.handle(Input::Tick, t0 + Duration::from_millis(400));
        assert!(fx.iter().any(|e| matches!(e, Effect::Load(_))));
        m.handle(Input::Loaded { entry: Some(7) }, t0);
        m.handle(Input::StartFile { entry: 7 }, t0);
        // END_FILE of the replaced file must be ignored
        m.handle(Input::EndFile { reason: EndReason::Stop, error: 0, entry: 6 }, t0);
        assert!(matches!(m.state(), PlayerState::Loading { .. }));
        let fx = m.handle(Input::PlaybackRestart, t0);
        assert_eq!(m.state(), &PlayerState::Playing);
        assert!(fx.iter().any(|e| matches!(e, Effect::PushHistory(_))));
    }

    #[test]
    fn live_error_reconnects_with_backoff() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, reason: PlayerErrorKind::Network, .. }));
        let fx = m.handle(Input::Tick, t0 + Duration::from_secs(1));
        assert!(fx.iter().any(|e| matches!(e, Effect::Load(_))));
    }

    #[test]
    fn forbidden_gives_up_after_one_retry() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        for (i, at) in [(1, 0u64), (2, 10)] {
            m.handle(Input::Tick, t0 + Duration::from_secs(at));
            m.handle(Input::Loaded { entry: Some(i) }, t0);
            m.handle(Input::StartFile { entry: i }, t0);
            m.handle(Input::HttpStatus(403), t0);
            m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: i }, t0 + Duration::from_secs(at));
        }
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. })));
    }

    /// Architecture review M2: Load A, Load B, START_FILE(A), END_FILE(A, stop), START_FILE(B),
    /// END_FILE(B, error) must reconnect B; a late PLAYBACK_RESTART of A must not mark B
    /// playing.
    #[test]
    fn fast_switch_binds_events_to_the_right_entry() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(2) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        assert!(matches!(m.state(), PlayerState::Loading { .. }), "restart of A ignored");
        m.handle(Input::EndFile { reason: EndReason::Stop, error: 0, entry: 1 }, t0);
        m.handle(Input::StartFile { entry: 2 }, t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 2 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, .. }));
    }

    /// Architecture review M4: segment 404s while Playing are a network problem.
    #[test]
    fn http_status_only_classifies_while_loading() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        m.handle(Input::HttpStatus(404), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Network, .. }));
    }

    /// Architecture review M3: a resolve failure neither stops nor blames the current stream.
    #[test]
    fn resolve_failure_keeps_current_stream() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        let err = PlayerError { kind: PlayerErrorKind::Resolve, message: "x".into() };
        let fx = m.handle(Input::ResolveFailed(err), t0);
        assert_eq!(m.state(), &PlayerState::Playing);
        assert!(!fx.iter().any(|e| matches!(e, Effect::StopMpv | Effect::RecordFailure(..))));
    }
}
