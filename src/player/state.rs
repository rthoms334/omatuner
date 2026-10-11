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
    Play {
        target: Arc<Playable>,
        immediate: bool,
    },
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
    Loaded {
        entry: Option<i64>,
    },
    StartFile {
        entry: i64,
    },
    FileLoaded,
    PlaybackRestart,
    EndFile {
        reason: EndReason,
        error: i32,
        entry: i64,
    },
    Pause(bool),
    PausedForCache(bool),
    BufferingPercent(i64),
    TimePos(f64),
    /// From mpv/ffmpeg log lines ("HTTP error 403 Forbidden").
    HttpStatus(u16),
    Tick,
    /// The controller could not start the other copy an [`Effect::Fallback`] asked for: the
    /// failure stands (Error, nothing recorded again).
    GiveUp(PlayerError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// `loadfile` this (its `start_at` already adjusted for VOD resume-after-reconnect).
    Load(Arc<Playable>),
    StopMpv,
    Emit(PlayerEvent),
    /// Persist VOD/catch-up progress (`final_` = on stop/switch/end).
    SaveProgress {
        final_: bool,
    },
    PushHistory(Arc<Playable>),
    RecordFailure(Arc<Playable>, PlayerErrorKind),
    ClearFailure(Arc<Playable>),
    /// The armed target ([`PlayerMachine::arm_fallback`]) failed before its first frame: play
    /// the next copy of the title instead of entering Error (DESIGN §29.14). Always the last
    /// effect of its input; the machine is disarmed (the fallback is tried once).
    Fallback(PlayerError),
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
    /// `Loaded` arrived without an id (or our entry redirected): accept the next START_FILE.
    accept_any_start: bool,
    /// Backoff attempts (Network/Timeout/Stalled/EOF/5xx…); reset by stable playback.
    attempt: u32,
    /// The one 401/403/404 retry of §11.4 was spent. Counted apart from `attempt`: a 403 right
    /// after a network reconnect (the provider still counts the dropped connection) is exactly
    /// the connection-limit race that retry exists for.
    http_retry_used: bool,
    playing_since: Option<Instant>,
    /// Last `time-pos` of the CURRENT target (only accepted after START_FILE of our entry, so
    /// the replaced file's positions never leak into resume/progress).
    last_pos: Option<f64>,
    last_pos_change: Option<Instant>,
    last_http: Option<u16>,
    history_pushed: bool,
    /// Mirrors of mpv's `pause` / `paused-for-cache` (both persist across files and only fire
    /// on change), so PLAYBACK_RESTART and unpause land in the right state.
    paused: bool,
    paused_for_cache: bool,
    /// Another copy of this VOD target can be played instead when it fails before its first
    /// frame ([`Effect::Fallback`]). Set by the controller for every play.
    fallback: bool,
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
            http_retry_used: false,
            playing_since: None,
            last_pos: None,
            last_pos_change: None,
            last_http: None,
            history_pushed: false,
            paused: false,
            paused_for_cache: false,
            fallback: false,
        }
    }

    pub fn state(&self) -> &PlayerState {
        &self.state
    }
    pub fn target(&self) -> Option<&Arc<Playable>> {
        self.target.as_ref()
    }
    pub fn last_position(&self) -> Option<f64> {
        self.last_pos
    }
    pub fn set_config(&mut self, cfg: MachineConfig) {
        self.cfg = cfg;
    }
    /// Whether another copy can stand in for the NEXT target if it fails before its first
    /// frame (the controller calls it right before every `Input::Play`). Armed, a VOD target
    /// gives up sooner — a load timeout at once, other retryable failures after one retry (the
    /// 403 retry stays) — and asks for [`Effect::Fallback`] instead of entering Error.
    pub fn arm_fallback(&mut self, on: bool) {
        self.fallback = on;
    }
    /// The current target showed its first frame (a failure now is mid-playback: the error
    /// card offers the other copy, nothing switches by itself).
    pub fn started(&self) -> bool {
        self.history_pushed
    }

    /// Feed one input; returns effects for the controller to execute in order.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        match input {
            Input::Play { target, immediate } => {
                if self.has_progress() {
                    fx.push(Effect::SaveProgress { final_: true });
                }
                self.target = Some(target.clone());
                self.attempt = 0;
                self.http_retry_used = false;
                self.history_pushed = false;
                self.last_http = None;
                self.last_pos = None;
                self.last_pos_change = None;
                self.playing_since = None;
                // whatever still plays during the zapping window is no longer ours: its
                // END_FILE / PLAYBACK_RESTART must not move the new target's state
                self.clear_entries();
                fx.push(Effect::Emit(PlayerEvent::NowPlaying(target)));
                if !immediate && !self.cfg.zap_delay.is_zero() {
                    // repeated zaps inside the window only move the target and restart the
                    // window; the connection is made once the user pauses
                    self.set_state(PlayerState::Zapping { until: now + self.cfg.zap_delay }, &mut fx);
                } else {
                    self.begin_load(now, &mut fx);
                }
            }
            Input::Stop => {
                if self.has_progress() {
                    fx.push(Effect::SaveProgress { final_: true });
                }
                self.clear_entries();
                fx.push(Effect::StopMpv);
                self.target = None;
                self.attempt = 0;
                self.http_retry_used = false;
                self.set_state(PlayerState::Idle, &mut fx);
            }
            Input::Retry => {
                if matches!(self.state, PlayerState::Error(_) | PlayerState::Reconnecting { .. }) && self.target.is_some() {
                    self.attempt = 0;
                    self.http_retry_used = false;
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
                if self.awaiting_start {
                    self.expected_entry = entry;
                    self.accept_any_start = entry.is_none();
                }
            }
            Input::Fail(err) => {
                fx.push(Effect::StopMpv);
                self.clear_entries();
                // the budget is ours, not the channel's: never dim a channel for it
                if err.kind != PlayerErrorKind::Budget
                    && let Some(t) = &self.target
                {
                    fx.push(Effect::RecordFailure(t.clone(), err.kind));
                }
                if self.fallback_due() {
                    self.fallback = false;
                    fx.push(Effect::Fallback(err));
                    return fx;
                }
                fx.push(Effect::Emit(PlayerEvent::Failed(err.clone())));
                self.set_state(PlayerState::Error(err), &mut fx);
            }
            Input::GiveUp(err) => {
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
                    let next = if self.paused {
                        PlayerState::Paused
                    } else if self.paused_for_cache {
                        PlayerState::Buffering { since: now, percent: None }
                    } else {
                        PlayerState::Playing
                    };
                    self.set_state(next, &mut fx);
                }
                // a seek inside a buffering stream: only paused-for-cache ends buffering
                PlayerState::Buffering { .. } if !self.paused_for_cache => {
                    self.last_pos_change = Some(now);
                    self.set_state(PlayerState::Playing, &mut fx)
                }
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
                    // a playlist URL expanded into new entries: the next START_FILE is ours
                    EndReason::Redirect => {
                        self.awaiting_start = true;
                        self.accept_any_start = true;
                    }
                    // nobody but us stops our file; if mpv did, the file is gone
                    EndReason::Stop | EndReason::Quit => {
                        if self.state.is_active() || matches!(self.state, PlayerState::Paused) {
                            if self.has_progress() {
                                fx.push(Effect::SaveProgress { final_: true });
                            }
                            self.set_state(PlayerState::Idle, &mut fx);
                        }
                    }
                    EndReason::Eof if !live => {
                        fx.push(Effect::SaveProgress { final_: true });
                        let item = self.target.as_ref().and_then(|t| t.item.clone());
                        // `finished` is recomputed by the controller (watched threshold)
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
            Input::Pause(p) => {
                self.paused = p;
                match (&self.state, p) {
                    (PlayerState::Playing | PlayerState::Buffering { .. }, true) => self.set_state(PlayerState::Paused, &mut fx),
                    (PlayerState::Paused, false) => {
                        self.last_pos_change = Some(now);
                        let next = if self.paused_for_cache { PlayerState::Buffering { since: now, percent: None } } else { PlayerState::Playing };
                        self.set_state(next, &mut fx)
                    }
                    _ => {}
                }
            }
            Input::PausedForCache(b) => {
                self.paused_for_cache = b;
                match (&self.state, b) {
                    (PlayerState::Playing, true) => self.set_state(PlayerState::Buffering { since: now, percent: None }, &mut fx),
                    (PlayerState::Buffering { .. }, false) => {
                        self.last_pos_change = Some(now);
                        self.set_state(PlayerState::Playing, &mut fx)
                    }
                    _ => {}
                }
            }
            Input::BufferingPercent(p) => {
                if let PlayerState::Buffering { since, .. } = self.state {
                    self.state = PlayerState::Buffering { since, percent: Some(p) };
                }
            }
            Input::TimePos(t) => {
                if self.current_entry.is_some() {
                    if self.last_pos.is_none_or(|p| (p - t).abs() > 0.01) {
                        self.last_pos_change = Some(now);
                    }
                    self.last_pos = Some(t);
                }
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
            PlayerState::Loading { since, .. } if now.duration_since(since) > self.cfg.load_timeout => self.fail(PlayerErrorKind::Timeout, now, fx),
            PlayerState::Buffering { since, .. } => {
                let limit = if live { self.cfg.buffer_timeout_live } else { self.cfg.buffer_timeout_vod };
                if now.duration_since(since) > limit {
                    self.fail(PlayerErrorKind::Stalled, now, fx);
                }
            }
            PlayerState::Playing => {
                if live
                    && self.last_pos.is_some()
                    && let Some(at) = self.last_pos_change
                    && now.duration_since(at) > self.cfg.stall_timeout
                {
                    self.fail(PlayerErrorKind::Stalled, now, fx);
                    return;
                }
                if (self.attempt > 0 || self.http_retry_used)
                    && let Some(s) = self.playing_since
                    && now.duration_since(s) >= self.cfg.policy.stable_after
                {
                    self.attempt = 0;
                    self.http_retry_used = false;
                }
            }
            PlayerState::Reconnecting { at, .. } if now >= at => self.begin_load(now, fx),
            _ => {}
        }
    }

    fn begin_load(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let Some(t) = self.target.clone() else { return };
        // VOD/catch-up reloads of the same target (reconnect or user retry) resume where
        // playback stopped; `last_pos` is reset on every new Play, so a first load never does
        let t = match (t.kind.is_live(), self.last_pos) {
            (false, Some(pos)) => Arc::new(Playable { start_at: Some((pos - 2.0).max(0.0)), ..(*t).clone() }),
            _ => t,
        };
        self.clear_entries();
        self.awaiting_start = true;
        self.last_http = None;
        self.playing_since = None;
        // state first: the controller may report a failure of the load itself (budget,
        // loadfile error) while executing `Load`, and that must be the last state announced
        self.set_state(PlayerState::Loading { since: now, attempt: self.attempt }, fx);
        fx.push(Effect::Load(t));
    }

    /// An armed VOD target that never showed a frame: its failure plays the other copy.
    fn fallback_due(&self) -> bool {
        self.fallback && !self.history_pushed && self.target.as_ref().is_some_and(|t| !t.kind.is_live())
    }

    fn fail(&mut self, kind: PlayerErrorKind, now: Instant, fx: &mut Vec<Effect>) {
        let live = self.target.as_ref().map(|t| t.kind.is_live()).unwrap_or(true);
        let http_once = matches!(kind, PlayerErrorKind::HttpForbidden | PlayerErrorKind::HttpNotFound);
        // the one-shot HTTP retry has its own counter (see `http_retry_used`)
        let next = if http_once { 1 + u32::from(self.http_retry_used) } else { self.attempt + 1 };
        fx.push(Effect::StopMpv);
        self.clear_entries();
        let early = self.fallback_due();
        // with another copy waiting, a copy that never started isn't retried for long: a load
        // timeout already waited, other failures get one quick retry (the 403 retry stays: a
        // connection-limit race right after a stop)
        let delay = match kind {
            PlayerErrorKind::Timeout if early => None,
            _ if early && !http_once && next > 1 => None,
            _ => self.cfg.policy.delay(next, kind, live),
        };
        match delay {
            Some(d) => {
                let max = if http_once {
                    self.http_retry_used = true;
                    1
                } else {
                    self.attempt = next;
                    if live { self.cfg.policy.max_attempts_live } else { self.cfg.policy.max_attempts_vod }
                };
                self.set_state(PlayerState::Reconnecting { attempt: next, max, at: now + d, reason: kind }, fx);
            }
            None => {
                let err = PlayerError { kind, message: describe(kind).to_owned() };
                if let Some(t) = &self.target {
                    fx.push(Effect::RecordFailure(t.clone(), kind));
                }
                if early {
                    self.fallback = false;
                    fx.push(Effect::Fallback(err));
                    return;
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
        if s != self.state {
            self.state = s.clone();
            fx.push(Effect::Emit(PlayerEvent::StateChanged(s)));
        }
    }

    /// The current target is seekable content that actually played (has a position), so a
    /// final progress save is due when it is stopped or replaced.
    fn has_progress(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.kind.is_seekable())
            && self.last_pos.is_some()
            && !matches!(self.state, PlayerState::Idle | PlayerState::Ended | PlayerState::Zapping { .. })
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
        -13 => PlayerErrorKind::Network,           // LOADING_FAILED
        -18..=-16 => PlayerErrorKind::Unsupported, // NOTHING_TO_PLAY / UNKNOWN_FORMAT / UNSUPPORTED
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
    use crate::model::{ItemKey, ItemKind, ItemRef, MediaRef, MovieId, PlayableKind, SourceId};
    use crate::redact::Secret;

    fn playable(kind: PlayableKind, media: MediaRef) -> Arc<Playable> {
        Arc::new(Playable {
            media,
            kind,
            item: Some(ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: ItemKey("k".into()) }),
            source_id: Some(SourceId(1)),
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

    fn live() -> Arc<Playable> {
        playable(PlayableKind::Live, MediaRef::Url("x".into()))
    }
    fn vod() -> Arc<Playable> {
        playable(PlayableKind::Movie, MediaRef::Movie(MovieId(1)))
    }

    fn machine() -> PlayerMachine {
        PlayerMachine::new(MachineConfig::from_config(&Config::default()))
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn has_load(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Load(_)))
    }

    fn loads(fx: &[Effect]) -> Vec<Arc<Playable>> {
        fx.iter().filter_map(|e| if let Effect::Load(p) = e { Some(p.clone()) } else { None }).collect()
    }

    fn recorded(fx: &[Effect]) -> Option<PlayerErrorKind> {
        fx.iter().find_map(|e| if let Effect::RecordFailure(_, k) = e { Some(*k) } else { None })
    }

    /// Immediate play of `p` that reaches Playing with entry `entry`.
    fn start_playing(m: &mut PlayerMachine, p: Arc<Playable>, entry: i64, t: Instant) -> Vec<Effect> {
        let mut fx = m.handle(Input::Play { target: p, immediate: true }, t);
        fx.extend(m.handle(Input::Loaded { entry: Some(entry) }, t));
        fx.extend(m.handle(Input::StartFile { entry }, t));
        fx.extend(m.handle(Input::PlaybackRestart, t));
        assert_eq!(m.state(), &PlayerState::Playing);
        fx
    }

    /// After a reconnect `Load`, the new entry reaches Playing.
    fn reload_playing(m: &mut PlayerMachine, entry: i64, t: Instant) {
        m.handle(Input::Loaded { entry: Some(entry) }, t);
        m.handle(Input::StartFile { entry }, t);
        m.handle(Input::PlaybackRestart, t);
        assert_eq!(m.state(), &PlayerState::Playing);
    }

    #[test]
    fn zap_delay_then_load_then_play() {
        let mut m = machine();
        let t0 = Instant::now();
        let fx = m.handle(Input::Play { target: live(), immediate: false }, t0);
        assert!(matches!(m.state(), PlayerState::Zapping { .. }));
        assert!(!has_load(&fx));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::NowPlaying(_)))), "OSD shows the target at once");
        let fx = m.handle(Input::Tick, t0 + Duration::from_millis(400));
        assert!(has_load(&fx));
        m.handle(Input::Loaded { entry: Some(7) }, t0);
        m.handle(Input::StartFile { entry: 7 }, t0);
        // END_FILE of the replaced file must be ignored
        m.handle(Input::EndFile { reason: EndReason::Stop, error: 0, entry: 6 }, t0);
        assert!(matches!(m.state(), PlayerState::Loading { .. }));
        let fx = m.handle(Input::PlaybackRestart, t0);
        assert_eq!(m.state(), &PlayerState::Playing);
        assert!(fx.iter().any(|e| matches!(e, Effect::PushHistory(_))));
        assert!(fx.iter().any(|e| matches!(e, Effect::ClearFailure(_))), "a successful play clears the failure row");
    }

    #[test]
    fn repeated_zaps_coalesce_into_one_load_of_the_last_target() {
        let mut m = machine();
        let t0 = Instant::now();
        let a = playable(PlayableKind::Live, MediaRef::Url("a".into()));
        let b = playable(PlayableKind::Live, MediaRef::Url("b".into()));
        m.handle(Input::Play { target: a, immediate: false }, t0);
        m.handle(Input::Play { target: b.clone(), immediate: false }, t0 + Duration::from_millis(200));
        // the window restarted with the second zap: nothing loads at 400 ms
        assert!(!has_load(&m.handle(Input::Tick, t0 + Duration::from_millis(400))));
        assert!(matches!(m.state(), PlayerState::Zapping { .. }));
        let fx = m.handle(Input::Tick, t0 + Duration::from_millis(520));
        assert_eq!(loads(&fx), vec![b.clone()]);
        assert!(!has_load(&m.handle(Input::Tick, t0 + Duration::from_millis(800))), "exactly one load");
        assert_eq!(m.target(), Some(&b));
    }

    #[test]
    fn immediate_play_and_zero_zap_delay_skip_zapping() {
        let mut m = machine();
        let t0 = Instant::now();
        assert!(has_load(&m.handle(Input::Play { target: live(), immediate: true }, t0)));
        assert!(matches!(m.state(), PlayerState::Loading { attempt: 0, .. }));

        let mut cfg = Config::default();
        cfg.player.zap_delay_ms = 0;
        let mut m = PlayerMachine::new(MachineConfig::from_config(&cfg));
        assert!(has_load(&m.handle(Input::Play { target: live(), immediate: false }, t0)));
    }

    #[test]
    fn set_config_changes_the_zap_delay() {
        let mut m = machine();
        let t0 = Instant::now();
        let mut cfg = Config::default();
        cfg.player.zap_delay_ms = 2000;
        m.set_config(MachineConfig::from_config(&cfg));
        m.handle(Input::Play { target: live(), immediate: false }, t0);
        assert!(!has_load(&m.handle(Input::Tick, t0 + Duration::from_millis(1500))));
        assert!(has_load(&m.handle(Input::Tick, t0 + Duration::from_millis(2001))));
    }

    #[test]
    fn live_error_reconnects_with_backoff() {
        let mut m = machine();
        let t0 = Instant::now();
        start_playing(&mut m, live(), 1, t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(fx.iter().any(|e| matches!(e, Effect::StopMpv)));
        assert!(recorded(&fx).is_none(), "no failure recorded while retrying");
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, max: 8, reason: PlayerErrorKind::Network, .. }));
        assert!(!has_load(&m.handle(Input::Tick, t0 + Duration::from_millis(400))), "backoff 0.5 s");
        let fx = m.handle(Input::Tick, t0 + Duration::from_secs(1));
        assert!(has_load(&fx));
        assert!(matches!(m.state(), PlayerState::Loading { attempt: 1, .. }));
    }

    #[test]
    fn live_gives_up_after_max_attempts_and_records_the_failure() {
        let mut m = machine();
        let mut t = Instant::now();
        start_playing(&mut m, live(), 1, t);
        let mut delays = Vec::new();
        for entry in 1..=9 {
            let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry }, t);
            if let PlayerState::Reconnecting { at, attempt, .. } = m.state().clone() {
                assert_eq!(attempt, entry as u32);
                delays.push(at - t);
                t = at;
                assert!(has_load(&m.handle(Input::Tick, t)));
                m.handle(Input::Loaded { entry: Some(entry + 1) }, t);
                m.handle(Input::StartFile { entry: entry + 1 }, t);
            } else {
                assert_eq!(entry, 9, "8 attempts, the 9th failure gives up");
                assert_eq!(recorded(&fx), Some(PlayerErrorKind::Network));
                assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Failed(_)))));
            }
        }
        let ms: Vec<u64> = delays.iter().map(|d| d.as_millis() as u64).collect();
        assert_eq!(ms, vec![500, 1000, 2000, 4000, 8000, 15000, 15000, 15000]);
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::Network, .. })));
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
            if i == 1 {
                assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, max: 1, reason: PlayerErrorKind::HttpForbidden, .. }));
            }
        }
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpForbidden, .. })));
    }

    /// Review fix: the 3 s retry of a 403 is not used up by earlier network reconnects (the
    /// provider may still count the dropped connection), and a 404 likewise keeps its retry.
    #[test]
    fn http_retry_survives_earlier_network_reconnects() {
        for (code, kind) in [(403, PlayerErrorKind::HttpForbidden), (404, PlayerErrorKind::HttpNotFound)] {
            let mut m = machine();
            let mut t = Instant::now();
            start_playing(&mut m, live(), 1, t);
            // two network drops → backoff attempts 1 and 2
            for entry in 1..=2 {
                m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry }, t);
                let PlayerState::Reconnecting { at, attempt, reason: PlayerErrorKind::Network, .. } = m.state().clone() else {
                    panic!("{:?}", m.state())
                };
                assert_eq!(attempt, entry as u32);
                t = at;
                assert!(has_load(&m.handle(Input::Tick, t)));
                m.handle(Input::Loaded { entry: Some(entry + 1) }, t);
                m.handle(Input::StartFile { entry: entry + 1 }, t);
            }
            // the reconnect is refused: one HTTP retry, then Error
            m.handle(Input::HttpStatus(code), t);
            let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 3 }, t);
            assert!(recorded(&fx).is_none(), "{code}: no failure recorded while retrying");
            let PlayerState::Reconnecting { at, attempt: 1, max: 1, reason, .. } = m.state().clone() else { panic!("{code}: {:?}", m.state()) };
            assert_eq!(reason, kind);
            t = at;
            assert!(has_load(&m.handle(Input::Tick, t)));
            m.handle(Input::Loaded { entry: Some(4) }, t);
            m.handle(Input::StartFile { entry: 4 }, t);
            m.handle(Input::HttpStatus(code), t);
            let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 4 }, t);
            assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: k, .. }) if *k == kind), "{code}: {:?}", m.state());
            assert_eq!(recorded(&fx), Some(kind));
            m.handle(Input::Retry, t);
            assert!(matches!(m.state(), PlayerState::Loading { attempt: 0, .. }), "user retry resets both counters");
        }
    }

    #[test]
    fn http_retry_is_restored_by_stable_playback_and_by_a_new_play() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::HttpStatus(403), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, .. }));
        m.handle(Input::Tick, t0 + secs(3));
        reload_playing(&mut m, 2, t0 + secs(3));
        // ≥ 30 s of stable playback: the retry is available again
        for s in (4..=34).step_by(2) {
            m.handle(Input::TimePos(s as f64), t0 + secs(s));
            m.handle(Input::Tick, t0 + secs(s));
        }
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 2 }, t0 + secs(34));
        m.handle(Input::Tick, t0 + secs(35));
        m.handle(Input::Loaded { entry: Some(3) }, t0 + secs(35));
        m.handle(Input::StartFile { entry: 3 }, t0 + secs(35));
        m.handle(Input::HttpStatus(403), t0 + secs(35));
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 3 }, t0 + secs(35));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, .. }), "{:?}", m.state());

        // spent retry, then a new Play: the new target gets its own retry
        m.handle(Input::Tick, t0 + secs(38));
        m.handle(Input::Loaded { entry: Some(4) }, t0 + secs(38));
        m.handle(Input::Play { target: live(), immediate: true }, t0 + secs(38));
        m.handle(Input::Loaded { entry: Some(5) }, t0 + secs(38));
        m.handle(Input::StartFile { entry: 5 }, t0 + secs(38));
        m.handle(Input::HttpStatus(403), t0 + secs(38));
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 5 }, t0 + secs(38));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, .. }), "{:?}", m.state());
    }

    #[test]
    fn not_found_retries_once_on_live_and_fails_vod_at_once() {
        let t0 = Instant::now();
        let mut m = machine();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::HttpStatus(404), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        let PlayerState::Reconnecting { at, reason: PlayerErrorKind::HttpNotFound, .. } = m.state().clone() else {
            panic!("live 404 retries: {:?}", m.state())
        };
        assert_eq!(at - t0, secs(2));

        let mut m = machine();
        m.handle(Input::Play { target: vod(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::HttpStatus(410), t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, .. })));
        assert_eq!(recorded(&fx), Some(PlayerErrorKind::HttpNotFound));
    }

    #[test]
    fn other_4xx_and_unsupported_fail_immediately_5xx_backs_off() {
        let t0 = Instant::now();
        for (http, err, expect_retry) in [(Some(400), -13, false), (None, -17, false), (Some(503), -13, true)] {
            let mut m = machine();
            m.handle(Input::Play { target: live(), immediate: true }, t0);
            m.handle(Input::Loaded { entry: Some(1) }, t0);
            m.handle(Input::StartFile { entry: 1 }, t0);
            if let Some(c) = http {
                m.handle(Input::HttpStatus(c), t0);
            }
            m.handle(Input::EndFile { reason: EndReason::Error, error: err, entry: 1 }, t0);
            assert_eq!(matches!(m.state(), PlayerState::Reconnecting { .. }), expect_retry, "{http:?} {err}: {:?}", m.state());
        }
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

    #[test]
    fn old_stream_events_during_the_zapping_window_are_ignored() {
        let mut m = machine();
        let t0 = Instant::now();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::TimePos(50.0), t0);
        m.handle(Input::Play { target: vod(), immediate: false }, t0);
        // the old stream still plays (and may fail) while we wait out the delay
        m.handle(Input::TimePos(51.0), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Zapping { .. }));
        assert_eq!(m.last_position(), None, "old positions never become the new target's");
        let fx = m.handle(Input::Tick, t0 + secs(1));
        assert_eq!(loads(&fx)[0].start_at, None, "first load of a VOD does not 'resume'");
    }

    #[test]
    fn loaded_without_an_id_accepts_the_next_start() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: None }, t0);
        m.handle(Input::StartFile { entry: 42 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        assert_eq!(m.state(), &PlayerState::Playing);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 42 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { .. }));
    }

    #[test]
    fn redirect_keeps_loading_and_follows_the_new_entry() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Redirect, error: 0, entry: 1 }, t0);
        assert!(fx.is_empty(), "{fx:?}");
        assert!(matches!(m.state(), PlayerState::Loading { .. }));
        m.handle(Input::StartFile { entry: 2 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        assert_eq!(m.state(), &PlayerState::Playing);
    }

    #[test]
    fn load_timeout_fails_with_timeout() {
        let mut m = machine();
        let t0 = Instant::now();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        // default network_timeout_s = 15 → load timeout 20 s
        assert!(m.handle(Input::Tick, t0 + secs(20)).is_empty());
        let fx = m.handle(Input::Tick, t0 + secs(21));
        assert!(fx.iter().any(|e| matches!(e, Effect::StopMpv)));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Timeout, .. }));
    }

    #[test]
    fn live_stall_detected_vod_not() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::TimePos(1.0), t0);
        m.handle(Input::TimePos(2.0), t0 + secs(5));
        m.handle(Input::Tick, t0 + secs(16));
        assert_eq!(m.state(), &PlayerState::Playing, "12 s since the last change not reached");
        m.handle(Input::Tick, t0 + secs(18));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Stalled, .. }));

        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(1.0), t0);
        m.handle(Input::Tick, t0 + secs(60));
        assert_eq!(m.state(), &PlayerState::Playing);
    }

    #[test]
    fn paused_live_is_not_a_stall() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::TimePos(1.0), t0);
        m.handle(Input::Pause(true), t0);
        assert_eq!(m.state(), &PlayerState::Paused);
        m.handle(Input::Tick, t0 + secs(60));
        m.handle(Input::Pause(false), t0 + secs(60));
        assert_eq!(m.state(), &PlayerState::Playing);
        m.handle(Input::Tick, t0 + secs(65));
        assert_eq!(m.state(), &PlayerState::Playing, "the stall clock restarts on unpause");
    }

    #[test]
    fn buffering_transitions_and_timeouts() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::PausedForCache(true), t0);
        assert!(matches!(m.state(), PlayerState::Buffering { percent: None, .. }));
        m.handle(Input::BufferingPercent(40), t0);
        assert!(matches!(m.state(), PlayerState::Buffering { percent: Some(40), .. }));
        m.handle(Input::PausedForCache(false), t0 + secs(1));
        assert_eq!(m.state(), &PlayerState::Playing);
        // live: 15 s
        m.handle(Input::PausedForCache(true), t0 + secs(2));
        m.handle(Input::Tick, t0 + secs(17));
        assert!(matches!(m.state(), PlayerState::Buffering { .. }));
        m.handle(Input::Tick, t0 + secs(18));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Stalled, .. }));
        // VOD: 30 s
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::PausedForCache(true), t0);
        m.handle(Input::Tick, t0 + secs(20));
        assert!(matches!(m.state(), PlayerState::Buffering { .. }));
        m.handle(Input::Tick, t0 + secs(31));
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Stalled, .. }));
    }

    #[test]
    fn pause_while_buffering_and_restart_during_initial_cache_pause() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::PausedForCache(true), t0);
        m.handle(Input::Pause(true), t0);
        assert_eq!(m.state(), &PlayerState::Paused, "user pause wins over buffering");
        m.handle(Input::Tick, t0 + secs(60));
        assert_eq!(m.state(), &PlayerState::Paused, "no buffering timeout while paused");
        m.handle(Input::Pause(false), t0 + secs(60));
        assert!(matches!(m.state(), PlayerState::Buffering { .. }), "still waiting for the cache");
        // a seek's PLAYBACK_RESTART does not end buffering while paused-for-cache is set
        m.handle(Input::PlaybackRestart, t0 + secs(61));
        assert!(matches!(m.state(), PlayerState::Buffering { .. }));
        m.handle(Input::PausedForCache(false), t0 + secs(62));
        assert_eq!(m.state(), &PlayerState::Playing);

        // cache pause reported before the first frame: PLAYBACK_RESTART → Buffering
        let mut m = machine();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PausedForCache(true), t0);
        assert!(matches!(m.state(), PlayerState::Loading { .. }));
        let fx = m.handle(Input::PlaybackRestart, t0);
        assert!(matches!(m.state(), PlayerState::Buffering { .. }));
        assert!(fx.iter().any(|e| matches!(e, Effect::PushHistory(_))));

        // paused before the first frame: PLAYBACK_RESTART → Paused
        let mut m = machine();
        m.handle(Input::Pause(true), t0);
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::PlaybackRestart, t0);
        assert_eq!(m.state(), &PlayerState::Paused);
    }

    #[test]
    fn vod_eof_ends_and_saves_progress() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(599.0), t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Eof, error: 0, entry: 1 }, t0);
        assert_eq!(m.state(), &PlayerState::Ended);
        let save = fx.iter().position(|e| matches!(e, Effect::SaveProgress { final_: true })).expect("final save");
        let ended = fx.iter().position(|e| matches!(e, Effect::Emit(PlayerEvent::Ended { item: Some(_), .. }))).expect("Ended");
        assert!(save < ended);
        assert!(!fx.iter().any(|e| matches!(e, Effect::StopMpv)));
        // Stop after the end: nothing more to save
        let fx = m.handle(Input::Stop, t0);
        assert!(!fx.iter().any(|e| matches!(e, Effect::SaveProgress { .. })));
    }

    #[test]
    fn live_eof_is_a_stream_end_and_reconnects() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::EndFile { reason: EndReason::Eof, error: 0, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::StreamEnded, .. }));
    }

    #[test]
    fn vod_reconnect_resumes_two_seconds_back_and_gives_up_after_three() {
        let mut t = Instant::now();
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t);
        m.handle(Input::TimePos(100.0), t);
        for attempt in 1..=3u32 {
            m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: attempt as i64 }, t);
            let PlayerState::Reconnecting { at, max: 3, .. } = m.state().clone() else { panic!("{:?}", m.state()) };
            t = at;
            let fx = m.handle(Input::Tick, t);
            assert_eq!(loads(&fx)[0].start_at, Some(98.0 + attempt as f64 - 1.0));
            reload_playing(&mut m, attempt as i64 + 1, t);
            m.handle(Input::TimePos(100.0 + attempt as f64), t);
        }
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 4 }, t);
        assert!(matches!(m.state(), PlayerState::Error(_)));
        assert_eq!(recorded(&fx), Some(PlayerErrorKind::Network));
    }

    #[test]
    fn stable_playback_resets_the_attempt_counter() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        m.handle(Input::Tick, t0 + secs(1));
        reload_playing(&mut m, 2, t0 + secs(1));
        m.handle(Input::TimePos(1.0), t0 + secs(1));
        // 29 s later: not yet stable → the next failure is attempt 2 (1 s backoff)
        let t1 = t0 + secs(30);
        m.handle(Input::TimePos(29.0), t1);
        m.handle(Input::Tick, t1);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 2 }, t1);
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 2, .. }));
        m.handle(Input::Tick, t1 + secs(1));
        reload_playing(&mut m, 3, t1 + secs(1));
        // ≥ 30 s of stable playback → counter reset → attempt 1 again
        for s in (2..=32).step_by(2) {
            m.handle(Input::TimePos(s as f64), t1 + secs(s));
            m.handle(Input::Tick, t1 + secs(s));
        }
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 3 }, t1 + secs(32));
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, .. }), "{:?}", m.state());
    }

    /// Architecture review M4: segment 404s while Playing are a network problem.
    #[test]
    fn http_status_only_classifies_while_loading() {
        let mut m = machine();
        let t0 = Instant::now();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::HttpStatus(404), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Network, .. }));
        // a status seen during a load is forgotten once that load plays
        let mut m = machine();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::HttpStatus(403), t0);
        m.handle(Input::PlaybackRestart, t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Network, .. }));
    }

    /// Architecture review M3: a resolve failure neither stops nor blames the current stream.
    #[test]
    fn resolve_failure_keeps_current_stream() {
        let mut m = machine();
        let t0 = Instant::now();
        start_playing(&mut m, live(), 1, t0);
        let err = PlayerError { kind: PlayerErrorKind::Resolve, message: "x".into() };
        let fx = m.handle(Input::ResolveFailed(err.clone()), t0);
        assert_eq!(m.state(), &PlayerState::Playing);
        assert!(!fx.iter().any(|e| matches!(e, Effect::StopMpv | Effect::RecordFailure(..))));
        assert!(fx.contains(&Effect::Emit(PlayerEvent::Failed(err.clone()))));
        // from Idle: Error, still nothing stopped or recorded
        let mut m = machine();
        let fx = m.handle(Input::ResolveFailed(err.clone()), t0);
        assert_eq!(m.state(), &PlayerState::Error(err));
        assert!(!fx.iter().any(|e| matches!(e, Effect::StopMpv | Effect::RecordFailure(..))));
        assert!(m.target().is_none());
        assert!(m.handle(Input::Retry, t0).is_empty(), "nothing to retry");
    }

    #[test]
    fn stop_and_switch_save_vod_progress() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(42.0), t0);
        let fx = m.handle(Input::Play { target: live(), immediate: true }, t0);
        assert!(matches!(fx[0], Effect::SaveProgress { final_: true }), "save before the switch: {fx:?}");

        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(42.0), t0);
        m.handle(Input::Pause(true), t0);
        let fx = m.handle(Input::Stop, t0);
        assert!(matches!(fx[0], Effect::SaveProgress { final_: true }));
        assert!(fx.contains(&Effect::StopMpv));
        assert_eq!(m.state(), &PlayerState::Idle);
        assert!(m.target().is_none());

        // live never saves progress; a VOD that never played neither
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::TimePos(42.0), t0);
        assert!(!m.handle(Input::Stop, t0).iter().any(|e| matches!(e, Effect::SaveProgress { .. })));
        let mut m = machine();
        m.handle(Input::Play { target: vod(), immediate: false }, t0);
        assert!(!m.handle(Input::Stop, t0).iter().any(|e| matches!(e, Effect::SaveProgress { .. })));
    }

    #[test]
    fn stop_while_reconnecting_vod_saves_the_last_position() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(42.0), t0);
        m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Reconnecting { .. }));
        assert!(matches!(m.handle(Input::Stop, t0)[0], Effect::SaveProgress { final_: true }));
    }

    #[test]
    fn user_retry_reloads_now_and_vod_resumes() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, vod(), 1, t0);
        m.handle(Input::TimePos(300.0), t0);
        m.handle(Input::HttpStatus(410), t0); // ignored: not loading
        m.handle(Input::EndFile { reason: EndReason::Error, error: -17, entry: 1 }, t0);
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::Unsupported, .. })));
        let fx = m.handle(Input::Retry, t0);
        assert_eq!(loads(&fx)[0].start_at, Some(298.0));
        assert!(matches!(m.state(), PlayerState::Loading { attempt: 0, .. }));

        // `r` while Reconnecting retries now instead of waiting out the backoff
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::EndFile { reason: EndReason::Eof, error: 0, entry: 1 }, t0);
        assert!(has_load(&m.handle(Input::Retry, t0)));
    }

    fn fell_back(fx: &[Effect]) -> Option<PlayerErrorKind> {
        fx.iter().find_map(|e| if let Effect::Fallback(err) = e { Some(err.kind) } else { None })
    }

    /// An armed VOD copy (another copy of the title waits, DESIGN §29.14) that fails before
    /// its first frame asks for the fallback instead of Error: a load timeout at once, a network
    /// error after one quick retry, a 403 after its 3 s retry, our connection budget at once
    /// (not blamed on the copy). The failure is recorded (the copy reads "failed 2 h ago"), the
    /// fallback is the last effect, no error is announced, and it is asked for once.
    #[test]
    fn an_armed_copy_that_fails_before_its_first_frame_falls_back_once() {
        let t0 = Instant::now();
        let armed = || {
            let mut m = machine();
            m.arm_fallback(true);
            m.handle(Input::Play { target: vod(), immediate: true }, t0);
            m.handle(Input::Loaded { entry: Some(1) }, t0);
            m
        };
        // load timeout: no retry
        let mut m = armed();
        let fx = m.handle(Input::Tick, t0 + secs(60));
        assert_eq!(fell_back(&fx), Some(PlayerErrorKind::Timeout));
        assert!(matches!(fx.last(), Some(Effect::Fallback(_))));
        assert_eq!(recorded(&fx), Some(PlayerErrorKind::Timeout));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Failed(_)))), "no error card");
        assert!(!matches!(m.state(), PlayerState::Error(_)));
        // the controller couldn't start the other copy: the failure stands, nothing recorded twice
        let fx = m.handle(Input::GiveUp(PlayerError { kind: PlayerErrorKind::Timeout, message: "x".into() }), t0);
        assert!(matches!(m.state(), PlayerState::Error(PlayerError { kind: PlayerErrorKind::Timeout, .. })));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Failed(_)))));
        assert_eq!(recorded(&fx), None);

        // network error: one quick retry, then the fallback
        let mut m = armed();
        m.handle(Input::StartFile { entry: 1 }, t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert_eq!(fell_back(&fx), None);
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, .. }));
        assert!(has_load(&m.handle(Input::Tick, t0 + secs(1))));
        m.handle(Input::Loaded { entry: Some(2) }, t0 + secs(1));
        m.handle(Input::StartFile { entry: 2 }, t0 + secs(1));
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 2 }, t0 + secs(1));
        assert_eq!(fell_back(&fx), Some(PlayerErrorKind::Network));

        // 403: its retry first (a connection-limit race right after a stop)
        let mut m = armed();
        m.handle(Input::StartFile { entry: 1 }, t0);
        m.handle(Input::HttpStatus(403), t0);
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 1 }, t0);
        assert_eq!(fell_back(&fx), None);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::HttpForbidden, .. }));
        m.handle(Input::Tick, t0 + secs(4));
        m.handle(Input::Loaded { entry: Some(2) }, t0 + secs(4));
        m.handle(Input::StartFile { entry: 2 }, t0 + secs(4));
        m.handle(Input::HttpStatus(403), t0 + secs(4));
        let fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry: 2 }, t0 + secs(4));
        assert_eq!(fell_back(&fx), Some(PlayerErrorKind::HttpForbidden));

        // our connection budget: straight to the other copy, not blamed on this one
        let mut m = armed();
        let fx = m.handle(Input::Fail(PlayerError { kind: PlayerErrorKind::Budget, message: "limit".into() }), t0);
        assert_eq!(fell_back(&fx), Some(PlayerErrorKind::Budget));
        assert_eq!(recorded(&fx), None);

        // once: the machine is disarmed, so the next copy's own failure is an ordinary one
        m.handle(Input::Play { target: vod(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(3) }, t0);
        let fx = m.handle(Input::Tick, t0 + secs(60));
        assert_eq!(fell_back(&fx), None);
        assert!(matches!(m.state(), PlayerState::Reconnecting { reason: PlayerErrorKind::Timeout, .. }));
    }

    /// After the first frame nothing falls back by itself (the normal reconnect runs, then the
    /// error card offers the other copy at the last position); live streams never fall back;
    /// an unarmed VOD keeps its three retries.
    #[test]
    fn no_fallback_after_the_first_frame_for_live_or_unarmed() {
        let t0 = Instant::now();
        let mut m = machine();
        m.arm_fallback(true);
        start_playing(&mut m, vod(), 1, t0);
        assert!(m.started());
        m.handle(Input::TimePos(2480.0), t0);
        let mut t = t0;
        let mut fx = Vec::new();
        for entry in 1..=4 {
            fx = m.handle(Input::EndFile { reason: EndReason::Error, error: -13, entry }, t);
            assert_eq!(fell_back(&fx), None);
            if let PlayerState::Reconnecting { at, .. } = m.state().clone() {
                t = at;
                m.handle(Input::Tick, t);
                m.handle(Input::Loaded { entry: Some(entry + 1) }, t);
                m.handle(Input::StartFile { entry: entry + 1 }, t);
            }
        }
        assert!(matches!(m.state(), PlayerState::Error(_)), "three reconnects, then the error card");
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Failed(_)))));
        assert_eq!(m.last_position(), Some(2480.0), "where the card's other copy starts");

        let mut m = machine();
        m.arm_fallback(true);
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        assert_eq!(fell_back(&m.handle(Input::Tick, t0 + secs(60))), None);

        let mut m = machine();
        m.handle(Input::Play { target: vod(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(1) }, t0);
        assert_eq!(fell_back(&m.handle(Input::Tick, t0 + secs(60))), None);
        assert!(matches!(m.state(), PlayerState::Reconnecting { attempt: 1, max: 3, .. }));
    }

    #[test]
    fn budget_failure_is_not_blamed_on_the_channel() {
        let t0 = Instant::now();
        let mut m = machine();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        let err = PlayerError { kind: PlayerErrorKind::Budget, message: "limit".into() };
        let fx = m.handle(Input::Fail(err.clone()), t0);
        assert_eq!(recorded(&fx), None);
        assert_eq!(m.state(), &PlayerState::Error(err));
        // an mpv command failure is
        let mut m = machine();
        m.handle(Input::Play { target: live(), immediate: true }, t0);
        let fx = m.handle(Input::Fail(PlayerError { kind: PlayerErrorKind::Mpv, message: "x".into() }), t0);
        assert_eq!(recorded(&fx), Some(PlayerErrorKind::Mpv));
    }

    #[test]
    fn unrequested_stop_of_our_file_goes_idle() {
        let t0 = Instant::now();
        let mut m = machine();
        start_playing(&mut m, live(), 1, t0);
        m.handle(Input::EndFile { reason: EndReason::Stop, error: 0, entry: 1 }, t0);
        assert_eq!(m.state(), &PlayerState::Idle);
    }

    #[test]
    fn time_pos_before_our_start_file_is_ignored() {
        let t0 = Instant::now();
        let mut m = machine();
        m.handle(Input::Play { target: vod(), immediate: true }, t0);
        m.handle(Input::Loaded { entry: Some(2) }, t0);
        m.handle(Input::TimePos(77.0), t0); // still the replaced file
        assert_eq!(m.last_position(), None);
        m.handle(Input::StartFile { entry: 2 }, t0);
        m.handle(Input::TimePos(1.0), t0);
        assert_eq!(m.last_position(), Some(1.0));
    }

    #[test]
    fn classify_table() {
        use PlayerErrorKind as K;
        assert_eq!(classify(-13, None), K::Network);
        assert_eq!(classify(-13, Some(401)), K::HttpForbidden);
        assert_eq!(classify(-13, Some(403)), K::HttpForbidden);
        assert_eq!(classify(-13, Some(404)), K::HttpNotFound);
        assert_eq!(classify(-13, Some(410)), K::HttpNotFound);
        assert_eq!(classify(-13, Some(429)), K::HttpOther(429));
        assert_eq!(classify(-13, Some(502)), K::HttpOther(502));
        assert_eq!(classify(-13, Some(200)), K::Network);
        for e in [-16, -17, -18] {
            assert_eq!(classify(e, None), K::Unsupported);
        }
        assert_eq!(classify(-20, None), K::Mpv);
    }
}
