//! Controller thread: owns the state machine, executes its effects against mpv, applies direct
//! commands (pause, seek, tracks...), zapping, connection budget (stop-before-start),
//! progress persistence, video suspension while nothing renders, and publishes snapshots.
//! Implemented in the skeleton, hardened by WP-07.
//!
//! Snapshot publishing: state changes and command results are published IMMEDIATELY (with a
//! repaint request, so e.g. a volume change while paused is visible at once); continuous values
//! (time-pos, cache) at most every 100 ms; a repaint for time-pos only when the whole second
//! changes (radio must not repaint the UI at audio-packet rate).
//!
//! Logging (field report 2026-10-11: a day of drop-outs left no trace of what the player did):
//! every state change of the current target at INFO with its source, item key and the time
//! spent in the previous state (no title: explicit titles never reach INFO, DESIGN §20.3; the
//! title is a DEBUG line per new target); recoveries ("recovered after 3 attempts in 41 s"); END_FILE
//! reasons; HTTP statuses (rate-limited); provider messages; and every 10 minutes a health line
//! (cache, drops, FFmpeg's own reconnects, memory). Stream addresses never appear: items are
//! named by their key, mpv's lines are redacted by the event pump.
//!
//! Multi-view (DESIGN §35, MV-1): every picture runs its own controller. Picture B's lines read
//! `player B:` / `health B:`, its events reach the app as `AppEvent::Tile(TileEvent::Player(B,
//! …))` (single-view code only ever sees picture A's), and its claims and leases go to the
//! [`SharedBudget`] both controllers share. Sound: `mute = user mute || !audible`; the snapshot
//! keeps the user's own mute and volume (one listening volume, moved with the sound). Zapping a
//! picture onto a channel whose playlist the other picture holds plays the channel's copy on a
//! free playlist (`zap_anchor` keeps the zap-list position), or skips it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, select};
use libmpv2::Mpv;
use tokio::sync::watch;

use super::budget::{Lease, Released, SharedBudget};
use super::events::{EndReason, MpvEvent, PropId, PropValue};
use super::mpv_log::{self, MpvLog};
use super::state::{Effect, Input, MachineConfig, PlayerMachine};
use super::tracks::{AspectMode, parse_track_list, query_stream_info};
use super::{CopyPlay, PlayRequest, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerEvent, PlayerSnapshot, PlayerState, RenderClock};
use crate::config::Config;
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, PlayOrigin, Playable, PlayableKind, SourceId, TileId, WatchProgress};
use crate::msg::AppEvent;

const PROGRESS_EVERY: Duration = Duration::from_secs(10);
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Max wait for the old stream's END_FILE before loading anyway (stop-before-start).
const PENDING_LOAD_MAX: Duration = Duration::from_millis(1500);
/// Max wait for ANOTHER picture's stream that is closing (its claim already moved on, its
/// END_FILE not seen yet) before the budget's refusal stands: multi-view's "move picture A to
/// its TV 2 copy so B can use TV" (`Tiles::rearrange`).
const OTHER_CLOSE_MAX: Duration = Duration::from_secs(3);
/// No rendered frame for this long while Playing → `vid=no` (hidden window safety net). Long
/// enough that a quick workspace switch never suspends: decoding restarts mid-GOP on resume,
/// and HEVC shows seconds of broken picture until the next keyframe (13 of 14 suspends in the
/// field log were followed by hundreds of HEVC reference errors).
const SUSPEND_VIDEO_AFTER: Duration = Duration::from_secs(10);
/// Period of the health line in the log (stream + memory).
const HEALTH_EVERY: Duration = Duration::from_secs(10 * 60);
/// How long an END_FILE(error) during Loading waits for the log line that explains it:
/// `mpv_wait_event` returns queued events before log messages, so ffmpeg's "HTTP error 403"
/// (logged before the file ended) is delivered just AFTER the END_FILE.
const END_FILE_LOG_GRACE: Duration = Duration::from_millis(200);

/// Which picture a controller drives and the budget it shares (multi-view, DESIGN §35).
#[derive(Debug, Clone)]
pub struct TileSetup {
    pub tile: TileId,
    pub budget: SharedBudget,
    /// Starts audible (picture A) or muted until the app moves the sound (picture B).
    pub audible: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    mpv: Arc<Mpv>,
    cfg: &Config,
    deps: PlayerDeps,
    cmd_rx: Receiver<PlayerCommand>,
    evt_rx: Receiver<MpvEvent>,
    snap_tx: watch::Sender<Arc<PlayerSnapshot>>,
    render_clock: RenderClock,
    epoch: Instant,
    setup: TileSetup,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let snap = (**snap_tx.borrow()).clone();
    let TileSetup { tile, budget, audible } = setup;
    let mut c = Controller {
        machine: PlayerMachine::new(MachineConfig::from_config(cfg)),
        mpv,
        deps,
        snap,
        snap_tx,
        tile,
        log: if tile == TileId::A { "player" } else { "player B" },
        budget,
        claimed: None,
        audible,
        light_decode: false,
        race_armed: false,
        lease: None,
        lease_entry: None,
        lease_redirected: false,
        pending: None,
        waiting: None,
        deferred_end: None,
        on_air: None,
        last_progress: Instant::now(),
        last_publish: Instant::now(),
        dirty: false,
        force_publish: false,
        last_second: None,
        watched_pct: cfg.player.watched_pct,
        applied: Box::new(cfg.clone()),
        render_clock,
        epoch,
        mpv_log: MpvLog::default(),
        recorded_video: None,
        fallback: None,
        elsewhere: None,
        lease_media: None,
        lease_played: false,
        http_log: MpvLog::default(),
        logged: LoggedState { label: state_label(&PlayerState::Idle), who: "nothing".into(), since: Instant::now(), outage: None, provider: None },
        last_health: Instant::now(),
        ffmpeg_reconnects: 0,
    };
    let name = if tile == TileId::A { "player".to_owned() } else { format!("player-{}", tile.letter().to_ascii_lowercase()) };
    std::thread::Builder::new().name(name).spawn(move || c.run(cmd_rx, evt_rx))
}

/// A load waiting for the previous stream of the same source to close.
struct PendingLoad {
    playable: Arc<Playable>,
    deadline: Instant,
}

struct Controller {
    mpv: Arc<Mpv>,
    machine: PlayerMachine,
    deps: PlayerDeps,
    snap: PlayerSnapshot,
    snap_tx: watch::Sender<Arc<PlayerSnapshot>>,
    /// The picture this controller drives.
    tile: TileId,
    /// Log line prefix: `player` (A), `player B`.
    log: &'static str,
    /// Shared by every picture (claims, leases, race window, observed limits).
    budget: SharedBudget,
    /// The claim last written to the budget (kept in step with the machine, `sync_claim`).
    claimed: Option<SourceId>,
    /// This picture has the sound (`mute = user mute || !audible`).
    audible: bool,
    /// Lighter decoding from the next load (`PlayerCommand::SetLightDecode`).
    light_decode: bool,
    /// The current load started inside another stream's race window: its 403 is the race,
    /// not a refusal (`SharedBudget::observe_refusal`).
    race_armed: bool,
    lease: Option<Lease>,
    /// Playlist entry the current lease belongs to (released on its END_FILE).
    lease_entry: Option<i64>,
    /// The leased entry was a playlist that expanded (END_FILE redirect): the lease moves to
    /// the next entry that starts.
    lease_redirected: bool,
    pending: Option<PendingLoad>,
    /// A load waiting for another picture's stream on its playlist to close.
    waiting: Option<PendingLoad>,
    /// END_FILE(error) (reason, error, entry) held for [`END_FILE_LOG_GRACE`], until the instant.
    deferred_end: Option<(EndReason, i32, i64, Instant)>,
    /// Last live channel that actually started playing (Backspace recall target).
    on_air: Option<Arc<Playable>>,
    last_progress: Instant,
    last_publish: Instant,
    dirty: bool,
    force_publish: bool,
    last_second: Option<i64>,
    watched_pct: u8,
    /// Config whose mpv settings are currently applied (ApplyConfig applies only changes).
    applied: Box<Config>,
    render_clock: RenderClock,
    epoch: Instant,
    /// Rate limiter of forwarded mpv log lines.
    mpv_log: MpvLog,
    /// The live channel and picture size last handed to `PlayerStore::record_video`.
    recorded_video: Option<(ItemRef, (i64, i64))>,
    /// The other copy of the current title, for one fallback before the first frame
    /// ([`CopyPlay::fallback`]; the machine is armed while it is set).
    fallback: Option<MediaRef>,
    /// [`CopyPlay::elsewhere`] of the current title.
    elsewhere: Option<MediaRef>,
    /// What the current lease is streaming.
    lease_media: Option<MediaRef>,
    /// The current lease's stream reached Playing (a stream the provider counted for sure).
    lease_played: bool,
    /// Rate limiter of the HTTP status lines.
    http_log: MpvLog,
    /// The state last written to the log.
    logged: LoggedState,
    last_health: Instant,
    /// FFmpeg's own reconnects of dropped transfers since the last health line.
    ffmpeg_reconnects: u32,
}

/// The state last logged (see [`Controller::log_transition`]).
struct LoggedState {
    label: String,
    /// The target it was about (kept for the line after a stop forgot the target).
    who: String,
    since: Instant,
    /// A reconnect run in progress: (first failure, attempts so far).
    outage: Option<(Instant, u32)>,
    /// The provider message last logged at WARN, with the target it was about: a long outage
    /// repeats it every attempt (DEBUG after the first).
    provider: Option<(String, String)>,
}

impl Controller {
    fn run(&mut self, cmd_rx: Receiver<PlayerCommand>, evt_rx: Receiver<MpvEvent>) {
        let ticker = crossbeam_channel::tick(Duration::from_millis(250));
        loop {
            let mut state_changed = false;
            let deferred = match &self.deferred_end {
                Some((.., at)) => crossbeam_channel::at(*at),
                None => crossbeam_channel::never(),
            };
            select! {
                recv(cmd_rx) -> cmd => match cmd {
                    Ok(PlayerCommand::Shutdown) | Err(_) => {
                        // the final progress save reaches the store before its writer closes
                        self.feed(Input::Stop, Instant::now());
                        self.publish(true);
                        self.budget.forget(self.tile);
                        return;
                    }
                    Ok(cmd) => {
                        let retargets = retargets(&cmd);
                        state_changed |= self.command(cmd);
                        if retargets {
                            // whatever was claimed for this picture meanwhile (`Tiles::rearrange`),
                            // the play's outcome decides
                            self.sync_claim(true);
                        }
                        self.force_publish = true;
                    }
                },
                recv(evt_rx) -> ev => match ev {
                    Ok(MpvEvent::Shutdown) | Err(_) => return,
                    Ok(ev) => state_changed |= self.mpv_event(ev),
                },
                recv(deferred) -> _ => state_changed |= self.flush_deferred_end(),
                recv(ticker) -> _ => {
                    let now = Instant::now();
                    state_changed |= self.feed(Input::Tick, now);
                    self.mpv_log.expire(now).iter().for_each(mpv_log::emit_summary);
                    self.http_log.expire(now).iter().for_each(log_http_summary);
                    if now.duration_since(self.last_health) >= HEALTH_EVERY {
                        self.last_health = now;
                        self.log_health();
                    }
                    self.pending_tick(now);
                    self.waiting_tick();
                    self.video_watchdog(now);
                    if now.duration_since(self.last_progress) >= PROGRESS_EVERY
                        && matches!(self.machine.state(), PlayerState::Playing)
                    {
                        self.last_progress = now;
                        self.save_progress(self.machine.last_position());
                    }
                },
            }
            self.publish(state_changed);
        }
    }

    /// Feed the machine and apply effects; returns true if the state changed.
    fn feed(&mut self, input: Input, now: Instant) -> bool {
        let before = self.machine.state().clone();
        // the machine forgets the old target's position on Play; its final save needs it
        let pos = self.machine.last_position();
        let fx = self.machine.handle(input, now);
        self.apply(fx, pos);
        if self.lease.is_some() && matches!(self.machine.state(), PlayerState::Playing) {
            self.lease_played = true;
        }
        self.sync_claim(false);
        self.log_transition(now);
        *self.machine.state() != before
    }

    /// Keep this picture's claim in the shared budget in step with the machine: the target's
    /// playlist from the play (Zapping) through Reconnecting; nothing once stopped, ended or
    /// failed for good. Written when it changes (a claim moved ahead by `Tiles::rearrange`
    /// isn't undone by the old stream's events), or always (`force`) after a play command.
    fn sync_claim(&mut self, force: bool) {
        let want = match self.machine.state() {
            PlayerState::Idle | PlayerState::Ended | PlayerState::Error(_) => None,
            _ => self.machine.target().and_then(|t| t.source_id),
        };
        if force || want != self.claimed {
            self.budget.claim(self.tile, want);
            self.claimed = want;
            self.snap.claim = want;
        }
    }

    /// Send a player event: picture A's as `AppEvent::Player` (what single view always got),
    /// another picture's tagged with its tile.
    fn emit(&self, ev: PlayerEvent) {
        if self.tile == TileId::A {
            self.deps.bus.send(AppEvent::Player(ev));
        } else {
            self.deps.bus.send(AppEvent::Tile(super::tiles::TileEvent::Player(self.tile, ev)));
        }
    }

    /// `source 2 xt:live:717661`: the current target, without its address or title.
    fn target_label(&self) -> String {
        self.machine.target().map_or_else(|| "nothing".into(), |p| playable_label(p))
    }

    /// Log a state change of the current target at INFO (once, however deeply `feed` nested):
    /// `player: source 2 xt:live:717661 "ITV 4" Playing → Reconnecting(attempt 1, StreamEnded)
    /// after 1h12m03s; retry in 0.5 s; cache 0.0 s`, and the end of a reconnect run
    /// ("recovered after …", "gave up after …"). The start of each reconnect attempt
    /// (Reconnecting → Loading) is DEBUG: its outcome follows.
    fn log_transition(&mut self, now: Instant) {
        let state = self.machine.state().clone();
        let label = state_label(&state);
        if label == self.logged.label {
            return;
        }
        let took = fmt_duration(now.saturating_duration_since(self.logged.since));
        let from = std::mem::replace(&mut self.logged.label, label);
        self.logged.since = now;
        if let Some(p) = self.machine.target() {
            let who = playable_label(p);
            if who != self.logged.who {
                tracing::debug!("{}: {who} is {:?}", self.log, p.title);
                self.logged.who = who;
            }
        }
        let line = format!("{}: {} {from} → {} after {took}", self.log, self.logged.who, self.logged.label);
        match &state {
            PlayerState::Loading { .. } if from.starts_with("Reconnecting") => tracing::debug!("{line}"),
            PlayerState::Reconnecting { at, .. } => {
                // attempts of THIS outage (the machine's counter carries over a short recovery)
                let (started, n) = self.logged.outage.unwrap_or((now, 0));
                self.logged.outage = Some((started, n + 1));
                let wait = at.saturating_duration_since(now).as_secs_f32();
                tracing::info!("{line}; retry in {wait:.1} s; cache {}", self.snap.cache_secs.map_or("-".into(), |c| format!("{c:.1} s")));
            }
            PlayerState::Playing => {
                tracing::info!("{line}");
                if let Some((started, attempts)) = self.logged.outage.take() {
                    let s = if attempts == 1 { "" } else { "s" };
                    tracing::info!("{}: recovered after {attempts} attempt{s} in {}", self.log, fmt_duration(now.saturating_duration_since(started)));
                }
            }
            PlayerState::Error(_) | PlayerState::Idle | PlayerState::Ended | PlayerState::Zapping { .. } => {
                tracing::info!("{line}");
                if let Some((started, attempts)) = self.logged.outage.take()
                    && matches!(state, PlayerState::Error(_))
                {
                    tracing::info!("{}: gave up after {attempts} attempts in {}", self.log, fmt_duration(now.saturating_duration_since(started)));
                }
            }
            _ => tracing::info!("{line}"),
        }
    }

    /// The 10-minute health line: what plays and how well, and the process's memory.
    fn log_health(&mut self) {
        let mem = crate::platform::memstats::MemStats::sample().line();
        let state = self.machine.state();
        let reconnects = std::mem::take(&mut self.ffmpeg_reconnects);
        let health = if self.tile == TileId::A { "health".to_owned() } else { format!("health {}", self.tile) };
        if matches!(state, PlayerState::Playing | PlayerState::Buffering { .. } | PlayerState::Paused) {
            let int = |name: &str| self.mpv.get_property::<i64>(name).ok();
            let speed = int("cache-speed").map_or("-".into(), |b| format!("{} KiB/s", b / 1024));
            let (vo, dec) = (int("frame-drop-count").unwrap_or(0), int("decoder-frame-drop-count").unwrap_or(0));
            tracing::info!(
                "{health}: {} {}; cache {} at {speed}; {vo} frames dropped ({dec} by the decoder); {reconnects} FFmpeg reconnects; hwdec {}; {mem}",
                self.target_label(),
                state_label(state),
                self.snap.cache_secs.map_or("-".into(), |c| format!("{c:.1} s")),
                self.snap.hwdec.as_deref().unwrap_or("no"),
            );
        } else {
            tracing::info!("{health}: {}; {mem}", state_label(state));
        }
    }

    /// Execute effects in order. `pos` = position of the target that was current when the
    /// input arrived (`SaveProgress` / `Ended` of a target that is being replaced or stopped).
    fn apply(&mut self, fx: Vec<Effect>, pos: Option<f64>) {
        for e in fx {
            match e {
                Effect::Load(p) => self.load(p),
                Effect::StopMpv => {
                    self.pending = None;
                    self.waiting = None;
                    let _ = self.mpv.command("stop", &[]);
                    // the lease is released when the stopped entry's END_FILE arrives
                    if self.lease_entry.is_none() {
                        self.release_lease();
                    }
                }
                Effect::Emit(mut ev) => {
                    if let PlayerEvent::Ended { finished, .. } = &mut ev {
                        *finished = self.watched(pos.or(self.machine.last_position()));
                    }
                    if let PlayerEvent::NowPlaying(p) = &ev {
                        // recall = the last channel that really played, not one zapped over
                        if let Some(prev) = &self.on_air
                            && prev.media != p.media
                        {
                            self.snap.previous_live = Some(prev.clone());
                        }
                        self.snap.playable = Some(p.clone());
                    }
                    self.emit(ev);
                }
                Effect::SaveProgress { .. } => self.save_progress(pos.or(self.machine.last_position())),
                Effect::PushHistory(p) => {
                    if p.kind.is_live() {
                        self.on_air = Some(p.clone());
                    }
                    self.push_history(&p)
                }
                Effect::RecordFailure(p, kind) => self.record_failure(&p, Some(kind)),
                Effect::ClearFailure(p) => self.record_failure(&p, None),
                Effect::Fallback(err) => self.fall_back(err),
            }
        }
        self.dirty = true;
    }

    fn release_lease(&mut self) {
        if let Some(l) = self.lease.take() {
            let played = std::mem::take(&mut self.lease_played);
            let closed = self.lease_media.take().map(|m| (m, played));
            self.budget.release(self.tile, l, closed);
        }
        self.lease_entry = None;
        self.lease_redirected = false;
    }

    /// Stop-before-start (see [`must_stop_first`]): `stop`, wait for the old entry's END_FILE
    /// (or 1.5 s), then load.
    fn load(&mut self, p: Arc<Playable>) {
        self.waiting = None;
        if let Some(old) = must_stop_first(self.lease.as_ref(), self.lease_entry, p.source_id, &self.budget) {
            tracing::debug!("stop-before-start: stopping entry {old} and waiting for its END_FILE");
            let _ = self.mpv.command("stop", &[]);
            self.pending = Some(PendingLoad { playable: p, deadline: Instant::now() + PENDING_LOAD_MAX });
            return;
        }
        self.pending = None;
        self.load_now(p);
    }

    fn pending_tick(&mut self, now: Instant) {
        if self.pending.as_ref().is_some_and(|pl| now >= pl.deadline) {
            tracing::debug!("old stream did not report END_FILE in time; loading anyway");
            self.release_lease();
            if let Some(pl) = self.pending.take() {
                self.load_now(pl.playable);
            }
        }
    }

    /// A load waiting for another picture's closing stream ([`OTHER_CLOSE_MAX`]): try again.
    fn waiting_tick(&mut self) {
        if let Some(w) = self.waiting.take() {
            self.load_now_by(w.playable, w.deadline);
        }
    }

    fn load_now(&mut self, p: Arc<Playable>) {
        self.load_now_by(p, Instant::now() + OTHER_CLOSE_MAX);
    }

    /// Load `p` now. Refused by the budget only because another picture's stream is still
    /// closing (its claim moved on): wait for it until `deadline`, then the refusal stands.
    fn load_now_by(&mut self, p: Arc<Playable>, deadline: Instant) {
        self.release_lease();
        if let Some(src) = p.source_id {
            match self.budget.acquire(self.tile, src) {
                Ok(l) => {
                    self.lease = Some(l);
                    self.lease_media = Some(p.media.clone());
                    self.lease_played = false;
                }
                Err(e) if e.closing && Instant::now() < deadline => {
                    tracing::debug!("{}: source {} is still closing in picture {:?}; waiting", self.log, src.0, e.holder);
                    // the lease went above: the old stream must not run on unaccounted
                    let _ = self.mpv.command("stop", &[]);
                    self.waiting = Some(PendingLoad { playable: p, deadline });
                    return;
                }
                Err(e) => {
                    self.feed(Input::Fail(PlayerError { kind: PlayerErrorKind::Budget, message: e.to_string() }), Instant::now());
                    return;
                }
            }
        }
        // extra header values are credentials (cookies, tokens): keep them out of mpv logs
        for (_, v) in &p.headers.extra {
            crate::redact::register_secret(v);
        }
        if self.snap.paused {
            // a new play always plays (mpv's `pause` survives loadfile)
            let _ = self.mpv.set_property("pause", false);
        }
        self.snap.load_generation += 1;
        // periodic progress saves count from the start of each file
        self.last_progress = Instant::now();
        self.snap.position = None;
        self.snap.duration = p.duration_hint;
        self.snap.stream_info = None;
        self.snap.tracks = Default::default();
        self.snap.last_error = None;
        let mut args = super::mpv::loadfile_args(&p);
        if self.light_decode {
            super::mpv::add_light_decode(&mut args);
        }
        let entry = match super::mpv::loadfile(&self.mpv, &args) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("loadfile failed: {e}");
                self.release_lease();
                let err = PlayerError { kind: PlayerErrorKind::Mpv, message: format!("Playback failed ({e})") };
                self.feed(Input::Fail(err), Instant::now());
                return;
            }
        };
        tracing::debug!("loadfile: entry {entry:?}");
        self.lease_entry = entry;
        self.feed(Input::Loaded { entry }, Instant::now());
        let closed = p.source_id.and_then(|s| self.other_stream_closed(s, &p.media));
        self.race_armed = closed.is_some();
        if let Some(at) = closed {
            tracing::debug!("another stream of this source closed {:?} ago: a 403 is the connection-limit race", at.elapsed());
        }
        self.feed(Input::SourceClosed(closed), Instant::now());
    }

    /// When a stream of `source` closed within the race window, on a source whose limit is one
    /// connection or unknown (the provider may still count it): see [`race_close`].
    fn other_stream_closed(&self, source: SourceId, media: &MediaRef) -> Option<Instant> {
        let one = self.budget.limit(source).is_none_or(|m| m <= 1);
        race_close(self.budget.released(source).as_ref().filter(|_| one), media, Instant::now(), self.machine.policy().race_window)
    }

    /// The provider's message behind a playlist redirect, if the "playlist" was prose: mpv's
    /// playlist parser reads a headerless `.m3u8` body line by line and makes each line an
    /// entry (see [`provider_text`]). Only entries the redirect inserted (`inserted` = the
    /// first one's id) are read.
    fn provider_message(&self, inserted: Option<i64>) -> Option<String> {
        let n = self.mpv.get_property::<i64>("playlist-count").ok()?;
        let lines: Vec<String> = (0..n.min(16))
            .filter(|i| inserted.is_none_or(|first| self.mpv.get_property::<i64>(&format!("playlist/{i}/id")).is_ok_and(|id| id >= first)))
            .filter_map(|i| self.mpv.get_property::<String>(&format!("playlist/{i}/filename")).ok())
            .filter_map(|f| provider_text(&f))
            .collect();
        (!lines.is_empty()).then(|| crate::redact::redact(&lines.join(" ")).into_owned())
    }

    fn resolve_and_play(&mut self, req: PlayRequest) -> bool {
        self.play_copy(CopyPlay { req, fallback: None, elsewhere: None, start_at: None })
    }

    /// Resolve and play; a copy with a fallback that can't be resolved goes straight to the
    /// fallback (once). The machine is armed only for the target that actually plays.
    fn play_copy(&mut self, c: CopyPlay) -> bool {
        let CopyPlay { req, fallback, elsewhere, start_at } = c;
        if let Some(z) = req.zap.clone() {
            self.snap.zap = Some(z);
        }
        let resume = req.resume && start_at.is_none();
        let resolved = self.deps.resolver.resolve(&req.media, resume).map(|mut p| {
            if start_at.is_some() {
                p.start_at = start_at.filter(|s| *s > 0.0);
            }
            p
        });
        match (resolved, fallback) {
            (Ok(p), fallback) => {
                self.snap.origin = Some(req.origin);
                self.fallback = fallback;
                self.elsewhere = elsewhere;
                self.machine.arm_fallback(self.fallback.is_some());
                self.feed(Input::Play { target: Arc::new(p), immediate: req.immediate }, Instant::now())
            }
            (Err(err), Some(other)) => match self.deps.resolver.resolve(&other, resume) {
                Ok(mut p) => {
                    tracing::info!("the first copy can't be resolved ({}); playing the next one", err.message);
                    if start_at.is_some() {
                        p.start_at = start_at.filter(|s| *s > 0.0);
                    }
                    // nothing played to name the first copy by: the toast names the next one
                    let ev = PlayerEvent::FellBack { title: p.title.clone(), from: None, to: p.source_id, error: err.kind };
                    self.snap.origin = Some(req.origin);
                    self.fallback = None;
                    self.elsewhere = None;
                    self.machine.arm_fallback(false);
                    let changed = self.feed(Input::Play { target: Arc::new(p), immediate: req.immediate }, Instant::now());
                    self.emit(ev);
                    changed
                }
                Err(_) => {
                    self.snap.last_error = Some(err.clone());
                    self.feed(Input::ResolveFailed(err), Instant::now())
                }
            },
            (Err(err), None) => {
                self.snap.last_error = Some(err.clone());
                self.feed(Input::ResolveFailed(err), Instant::now())
            }
        }
    }

    /// The armed target failed before its first frame ([`Effect::Fallback`]): play the other
    /// copy once, at the position the first one was asked to start at; if that can't even be
    /// resolved, the first failure stands.
    fn fall_back(&mut self, err: PlayerError) {
        let failed = self.machine.target().cloned();
        // a playlist that refused (403, its connection budget): a copy elsewhere
        let next = CopyPlay::fallback_for(self.fallback.take(), self.elsewhere.take(), err.kind).map(|m| (self.deps.resolver.resolve(&m, false), m));
        match next {
            Some((Ok(mut p), _)) => {
                let failed = failed.as_deref();
                p.start_at = failed.and_then(|f| f.start_at);
                tracing::info!(
                    "{}: {} failed before its first frame ({:?}); playing the next copy",
                    self.log,
                    failed.map_or_else(|| "nothing".into(), playable_label),
                    err.kind
                );
                let ev = PlayerEvent::FellBack {
                    title: failed.map_or_else(|| p.title.clone(), |f| f.title.clone()),
                    from: failed.and_then(|f| f.source_id),
                    to: p.source_id,
                    error: err.kind,
                };
                self.machine.arm_fallback(false);
                self.feed(Input::Play { target: Arc::new(p), immediate: true }, Instant::now());
                self.emit(ev);
            }
            Some((Err(e), m)) => {
                tracing::warn!("the next copy {m:?} can't be resolved either: {}", e.message);
                self.feed(Input::GiveUp(err), Instant::now());
            }
            None => {
                self.feed(Input::GiveUp(err), Instant::now());
            }
        }
    }

    fn zap_request(&self, media: MediaRef, immediate: bool) -> PlayRequest {
        PlayRequest { media, zap: None, origin: self.snap.origin.unwrap_or(PlayOrigin::Fullscreen), resume: false, immediate }
    }

    fn command(&mut self, cmd: PlayerCommand) -> bool {
        let m = self.mpv.clone();
        match cmd {
            PlayerCommand::Play(req) => {
                self.snap.zap_anchor = None;
                return self.resolve_and_play(req);
            }
            PlayerCommand::PlayCopy(c) => {
                self.snap.zap_anchor = None;
                return self.play_copy(c);
            }
            PlayerCommand::Stop => return self.feed(Input::Stop, Instant::now()),
            PlayerCommand::TogglePause => {
                let _ = m.command("cycle", &["pause"]);
            }
            PlayerCommand::SetPause(b) => {
                let _ = m.set_property("pause", b);
            }
            PlayerCommand::SeekBy(s) => {
                if self.snap.seekable {
                    let _ = m.command("seek", &[&format!("{s}"), "relative"]);
                }
            }
            PlayerCommand::SeekTo(t) => {
                if self.snap.seekable {
                    let _ = m.command("seek", &[&format!("{t}"), "absolute"]);
                }
            }
            PlayerCommand::Zap { forward, unlocked } => {
                if let Some((next, copy)) = self.zap_neighbour(forward, unlocked) {
                    return self.zap_onto(next, copy);
                }
            }
            PlayerCommand::ZapTo { id, unlocked } => {
                // numeric entry / history: the copy when the other picture holds the playlist
                // (none: the budget refuses it and the error card says why); a locked copy only
                // for an unlocked sender, or for a pick that is locked itself (its PIN was typed)
                let unlocked = unlocked || self.deps.resolver.is_locked(id);
                let copy = match self.placement(id, unlocked) {
                    Placement::Copy(c) => Some(c),
                    _ => None,
                };
                return self.zap_onto(id, copy);
            }
            PlayerCommand::LastChannel { unlocked } => {
                self.snap.zap_anchor = None;
                if let Some(prev) = self.snap.previous_live.clone() {
                    if let Some(ch) = channel_of(&prev.media)
                        && self.locked_out(ch, unlocked)
                    {
                        tracing::debug!("last channel {ch:?} is locked; not recalled");
                        return false;
                    }
                    let req = self.zap_request(prev.media.clone(), true);
                    return self.resolve_and_play(req);
                }
            }
            PlayerCommand::JumpToLive => {
                self.snap.zap_anchor = None;
                if let Some(MediaRef::Catchup { channel, .. }) = self.snap.playable.as_ref().map(|p| p.media.clone()) {
                    let req = self.zap_request(MediaRef::Channel(channel), true);
                    return self.resolve_and_play(req);
                }
            }
            PlayerCommand::SetVolume(v) => {
                let _ = m.set_property("volume", v.clamp(0.0, 130.0));
            }
            PlayerCommand::AdjustVolume(d) => {
                let v = (self.snap.volume + d).clamp(0.0, 130.0);
                let _ = m.set_property("volume", v);
            }
            PlayerCommand::ToggleMute => {
                self.snap.muted = !self.snap.muted;
                self.apply_mute();
            }
            PlayerCommand::SetMute(on) => {
                self.snap.muted = on;
                self.apply_mute();
            }
            PlayerCommand::SetAudible(on) => {
                self.audible = on;
                self.snap.audible = on;
                self.apply_mute();
            }
            PlayerCommand::SetLightDecode(on) => self.light_decode = on,
            PlayerCommand::SetAudioTrack(id) => {
                let _ = m.set_property("aid", id.map(|i| i.to_string()).unwrap_or_else(|| "no".into()).as_str());
            }
            PlayerCommand::SetSubtitleTrack(id) => {
                let _ = m.set_property("sid", id.map(|i| i.to_string()).unwrap_or_else(|| "no".into()).as_str());
            }
            PlayerCommand::CycleAudio => {
                let _ = m.command("cycle", &["audio"]);
            }
            PlayerCommand::CycleSubtitle => {
                let _ = m.command("cycle", &["sub"]);
            }
            PlayerCommand::AdjustSubDelay(d) => {
                let _ = m.command("add", &["sub-delay", &format!("{d}")]);
            }
            PlayerCommand::ResetSubDelay => {
                let _ = m.set_property("sub-delay", 0.0);
            }
            PlayerCommand::AdjustSubScale(d) => {
                let v = (self.snap.sub_scale + d).clamp(0.3, 3.0);
                let _ = m.set_property("sub-scale", v);
            }
            PlayerCommand::SetAspect(a) => self.set_aspect(a),
            PlayerCommand::CycleAspect => self.set_aspect(self.snap.aspect.next()),
            PlayerCommand::SetZoom(z) => {
                let _ = m.set_property("video-zoom", z);
                self.snap.zoom = z;
            }
            PlayerCommand::Reconnect => return self.feed(Input::Retry, Instant::now()),
            PlayerCommand::RefreshStreamInfo => self.snap.stream_info = Some(query_stream_info(&self.mpv)),
            PlayerCommand::SetZapList(z) => self.snap.zap = Some(z),
            PlayerCommand::SetConnectionLimit { source, max } => self.budget.set_limit(source, max),
            PlayerCommand::ApplyConfig(cfg) => self.apply_config(&cfg),
            PlayerCommand::ResumeVideo => {
                if self.snap.video_suspended {
                    let _ = m.set_property("vid", "auto");
                    self.snap.video_suspended = false;
                }
            }
            // CAST-5: the remote output (DESIGN §40 §5.5); inert until then (casting is off)
            PlayerCommand::CastTo { .. } | PlayerCommand::StopCasting { .. } => {}
            PlayerCommand::Shutdown => {}
        }
        self.dirty = true;
        false
    }

    /// mpv's mute: the user's mute, or this picture doesn't have the sound.
    fn apply_mute(&self) {
        let _ = self.mpv.set_property("mute", self.snap.muted || !self.audible);
    }

    /// Tune `ch` (or its `copy`, keeping `ch` as the zap-list position) as a zap.
    fn zap_onto(&mut self, ch: ChannelId, copy: Option<ChannelId>) -> bool {
        let req = self.zap_request(MediaRef::Channel(copy.unwrap_or(ch)), false);
        let changed = self.resolve_and_play(req);
        self.snap.zap_anchor = copy.map(|_| ch);
        if let Some(c) = copy {
            tracing::info!("{}: channel {} is on a playlist another picture holds; playing its copy {}", self.log, ch.0, c.0);
        }
        changed
    }

    /// Where `ch` can play for this picture (multi-view, DESIGN §35 §4.4): on its own playlist
    /// when the budget admits it, else on the first copy (same explicit flag as `ch`, not
    /// locked out, not failing) on a playlist that admits it. Single view never asks the
    /// database: nobody else holds anything.
    fn placement(&mut self, ch: ChannelId, unlocked: bool) -> Placement {
        if !self.budget.others_active(self.tile) {
            return Placement::Here;
        }
        let Some(source) = self.deps.resolver.channel_source(ch) else { return Placement::Here };
        if self.budget.admits(self.tile, source) {
            return Placement::Here;
        }
        let explicit = self.deps.resolver.is_explicit(ch);
        for c in self.deps.resolver.channel_copies(ch) {
            if c.explicit != explicit || self.deps.resolver.skip_on_zap(c.id) || self.locked_out(c.id, unlocked) {
                continue;
            }
            if self.budget.admits(self.tile, c.source) {
                return Placement::Copy(c.id);
            }
        }
        Placement::Blocked { source, holder: self.budget.holder(self.tile, source) }
    }

    /// Settings changed (DESIGN §13.9): everything applies at runtime — the changed entries of
    /// the setting list shared with init (`mpv::runtime_settings`) plus changed raw
    /// `mpv_options`. Unchanged settings are left alone, so a subtitle track or sub scale the
    /// user picked for this stream survives an unrelated settings change.
    fn apply_config(&mut self, cfg: &Config) {
        self.machine.set_config(MachineConfig::from_config(cfg));
        self.watched_pct = cfg.player.watched_pct;
        super::mpv::apply_runtime(&self.mpv, cfg, &self.applied, self.tile);
        *self.applied = cfg.clone();
    }

    fn set_aspect(&mut self, a: AspectMode) {
        let (ovr, panscan, keep) = a.mpv_values();
        let _ = self.mpv.set_property("video-aspect-override", ovr);
        let _ = self.mpv.set_property("panscan", panscan);
        let _ = self.mpv.set_property("keepaspect", keep);
        self.snap.aspect = a;
    }

    /// Parental gate of zapping and recall (DESIGN §20.3), authoritative for every sender: with
    /// a PIN set and the sender's session locked, a PIN-locked channel is never tuned, and
    /// explicit content is never tuned while it is hidden (`ParentalConfig::hides_explicit`).
    fn locked_out(&mut self, ch: ChannelId, unlocked: bool) -> bool {
        let p = &self.applied.parental;
        let (pin_locks, hides_explicit) = (!unlocked && p.pin_active(), p.hides_explicit(unlocked));
        (pin_locks && self.deps.resolver.is_locked(ch)) || (hides_explicit && self.deps.resolver.is_explicit(ch))
    }

    /// Next/previous channel of the ZapList, skipping what the resolver says to skip, locked
    /// channels while the parental session is locked, and (multi-view) channels whose playlist
    /// another picture holds and that have no copy on a free one. Returns the channel and the
    /// copy to play in its place. The position counts from `zap_anchor` when a copy plays.
    /// The first channel skipped for the budget is announced ([`PlayerEvent::ZapSkipped`]).
    fn zap_neighbour(&mut self, forward: bool, unlocked: bool) -> Option<(ChannelId, Option<ChannelId>)> {
        let ids = self.snap.zap.as_ref()?.channels.clone();
        let playing = self.snap.playable.as_ref().and_then(|p| channel_of(&p.media));
        let cur = self.snap.zap_anchor.or(playing).unwrap_or(ChannelId(i64::MIN));
        let copy = std::cell::Cell::new(None);
        let skipped = std::cell::Cell::new(None);
        let this = std::cell::RefCell::new(&mut *self);
        let next = crate::store::library::LiveIndex::neighbour(&ids, cur, forward, &|c| {
            let mut this = this.borrow_mut();
            if this.deps.resolver.skip_on_zap(c) || this.locked_out(c, unlocked) {
                return true;
            }
            match this.placement(c, unlocked) {
                Placement::Here => {
                    copy.set(None);
                    false
                }
                Placement::Copy(id) => {
                    copy.set(Some(id));
                    false
                }
                Placement::Blocked { source, holder } => {
                    if skipped.get().is_none() {
                        skipped.set(Some((c, source, holder)));
                    }
                    true
                }
            }
        });
        if let Some((channel, source, holder)) = skipped.get() {
            tracing::info!("{}: channel {} skipped: source {} is in use by another picture", self.log, channel.0, source.0);
            self.emit(PlayerEvent::ZapSkipped { channel, source, holder });
        }
        next.map(|n| (n, copy.get()))
    }

    /// Hidden-window safety net (module docs of `player`).
    fn video_watchdog(&mut self, now: Instant) {
        let last = self.render_clock.load(Ordering::Relaxed);
        if last == 0 || self.snap.video_suspended || self.snap.video_size.is_none() {
            return; // no surface yet / nothing to suspend
        }
        let since_epoch = now.duration_since(self.epoch).as_millis() as u64;
        let idle = Duration::from_millis(since_epoch.saturating_sub(last));
        if idle > SUSPEND_VIDEO_AFTER && matches!(self.machine.state(), PlayerState::Playing) {
            // the clock is stamped per UI frame: a visible window over a stream that stopped
            // advancing stops repainting too
            let advancing = self.machine.position_changed_at().is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(3));
            let why = if advancing { "the window is hidden; playback continues" } else { "playback isn't advancing either" };
            tracing::info!("{}: no frames rendered for {idle:?} ({why}); suspending video decoding", self.log);
            let _ = self.mpv.set_property("vid", "no");
            self.snap.video_suspended = true;
            self.force_publish = true;
        }
    }

    /// Feed an END_FILE and release the lease of the stream that just closed (starting a load
    /// that waited for it, see [`Controller::load`]).
    fn end_file(&mut self, reason: EndReason, error: i32, entry_id: i64, inserted: Option<i64>) -> bool {
        let line = format!("END_FILE entry {entry_id}: {reason:?} ({error}) in {}", state_label(self.machine.state()));
        if self.machine.owns_entry(entry_id) && matches!(reason, EndReason::Eof | EndReason::Error | EndReason::Unknown) {
            tracing::info!("{line}");
        } else {
            tracing::debug!("{line}");
        }
        let changed = self.feed(Input::EndFile { reason, error, entry: entry_id }, Instant::now());
        match lease_on_end(self.lease_entry, entry_id, reason, inserted) {
            LeaseOnEnd::Unrelated => {}
            LeaseOnEnd::MoveTo(next) => {
                // the playlist URL expanded into new entries that keep using the source: the
                // lease moves to the first of them right away (no window in which a same-source
                // play would skip stop-before-start); without mpv's insert id it follows the
                // next entry that starts (`mpv_event`, START_FILE)
                self.lease_entry = next;
                self.lease_redirected = next.is_none();
            }
            LeaseOnEnd::Release => {
                // the leased stream is really closed now
                self.release_lease();
                if let Some(pl) = self.pending.take() {
                    self.load_now(pl.playable);
                }
            }
        }
        changed
    }

    /// Process a held END_FILE (see [`END_FILE_LOG_GRACE`]); true if the state changed.
    fn flush_deferred_end(&mut self) -> bool {
        match self.deferred_end.take() {
            Some((reason, error, entry_id, _)) => self.end_file(reason, error, entry_id, None),
            None => false,
        }
    }

    fn mpv_event(&mut self, ev: MpvEvent) -> bool {
        let now = Instant::now();
        match ev {
            MpvEvent::StartFile { entry_id } => {
                if self.lease_redirected && self.lease.is_some() {
                    // the controller is the only playlist writer: the entry starting after a
                    // redirect is the expansion of our leased playlist
                    self.lease_redirected = false;
                    self.lease_entry = Some(entry_id);
                }
                self.feed(Input::StartFile { entry: entry_id }, now)
            }
            MpvEvent::FileLoaded => self.feed(Input::FileLoaded, now),
            MpvEvent::PlaybackRestart => {
                // the new stream's size before the Playing publish records it (`record_video`),
                // whatever order mpv delivers the `video-params` change in
                self.read_video_size();
                self.feed(Input::PlaybackRestart, now)
            }
            MpvEvent::EndFile { reason, error, entry_id, inserted } => {
                let changed = self.flush_deferred_end();
                if reason == EndReason::Error && matches!(self.machine.state(), PlayerState::Loading { .. }) {
                    self.deferred_end = Some((reason, error, entry_id, now + END_FILE_LOG_GRACE));
                    return changed;
                }
                // a live channel's address answered with the provider's message instead of a
                // playlist: stop before mpv requests the "address" made of it
                if reason == EndReason::Redirect
                    && self.machine.owns_entry(entry_id)
                    && self.machine.target().is_some_and(|t| t.kind.is_live())
                    && let Some(text) = self.provider_message(inserted)
                {
                    let about = (self.target_label(), text.clone());
                    if self.logged.provider.as_ref() == Some(&about) {
                        tracing::debug!("{}: the provider answered with the same message again", self.log);
                    } else {
                        tracing::warn!("{}: the provider answered with a message instead of a stream: {text:?}", self.log);
                        self.logged.provider = Some(about);
                    }
                    let changed = self.end_file(reason, error, entry_id, inserted) || changed;
                    return self.feed(Input::ProviderMessage(text), now) || changed;
                }
                self.end_file(reason, error, entry_id, inserted) || changed
            }
            MpvEvent::Log { level, prefix, text } => {
                if let Some(code) = http_status_in(&text) {
                    let (verdict, summary) = self.http_log.admit(&format!("HTTP {code}"), tracing::Level::INFO, now);
                    if let Some(s) = summary {
                        log_http_summary(&s);
                    }
                    if verdict != mpv_log::Verdict::Suppress {
                        tracing::info!("HTTP {code} in {}", state_label(self.machine.state()));
                    }
                    self.feed(Input::HttpStatus(code), now);
                    self.observe_refusal(code);
                }
                if mpv_log::is_internal_reconnect(&text) {
                    self.ffmpeg_reconnects += 1;
                }
                let (verdict, summary) = self.mpv_log.admit(&prefix, mpv_log::level_for(&level, &text), now);
                if let Some(s) = summary {
                    mpv_log::emit_summary(&s);
                }
                if let mpv_log::Verdict::Show(l) = verdict {
                    mpv_log::emit(l, &prefix, &text);
                }
                false
            }
            MpvEvent::Prop { id, value } => self.prop(id, value, now),
            MpvEvent::VideoReconfig | MpvEvent::AudioReconfig | MpvEvent::Seek | MpvEvent::Idle => false,
            MpvEvent::QueueOverflow => {
                tracing::warn!("mpv event queue overflow");
                false
            }
            MpvEvent::Shutdown => false,
        }
    }

    /// A 403 while loading, outside the race window, on a playlist with an unknown limit that
    /// another picture holds: the playlist allows one stream at a time (learned for the
    /// session, so the planner and zaps stop offering it twice).
    fn observe_refusal(&mut self, code: u16) {
        if code != 403 || self.race_armed || !matches!(self.machine.state(), PlayerState::Loading { .. }) {
            return;
        }
        if let Some(src) = self.machine.target().and_then(|t| t.source_id)
            && self.budget.observe_refusal(self.tile, src)
        {
            tracing::info!("{}: source {} refused a second stream; one stream at a time from now on", self.log, src.0);
        }
    }

    fn prop(&mut self, id: PropId, v: PropValue, now: Instant) -> bool {
        self.dirty = true;
        let f = |v: &PropValue| match v {
            PropValue::Double(d) => Some(*d),
            PropValue::Int(i) => Some(*i as f64),
            _ => None,
        };
        let b = |v: &PropValue| matches!(v, PropValue::Flag(true));
        match id {
            PropId::Pause => {
                self.snap.paused = b(&v);
                self.force_publish = true;
                return self.feed(Input::Pause(b(&v)), now);
            }
            PropId::PausedForCache => return self.feed(Input::PausedForCache(b(&v)), now),
            PropId::CacheBufferingState => {
                if let PropValue::Int(p) = v {
                    self.feed(Input::BufferingPercent(p), now);
                }
            }
            PropId::TimePos => {
                self.snap.position = f(&v);
                if let Some(t) = f(&v) {
                    self.feed(Input::TimePos(t), now);
                    let sec = t.floor() as i64;
                    if self.last_second != Some(sec) {
                        self.last_second = Some(sec);
                        self.deps.bus.request_repaint();
                    }
                }
            }
            PropId::Duration => self.snap.duration = f(&v).or(self.snap.duration),
            PropId::DemuxerCacheDuration => self.snap.cache_secs = f(&v),
            PropId::Volume => {
                self.snap.volume = f(&v).unwrap_or(self.snap.volume);
                self.force_publish = true;
            }
            // the controller is mute's only writer (`apply_mute`): the snapshot keeps the user's
            // own mute, mpv's also follows the sound
            PropId::Mute => self.force_publish = true,
            PropId::TrackList => {
                if let PropValue::Str(s) = &v {
                    // track-list re-fires on sub-property changes (demux-bitrate…): only
                    // announce real changes
                    let t = parse_track_list(s);
                    if t != self.snap.tracks {
                        self.snap.tracks = t;
                        self.emit(PlayerEvent::TracksChanged);
                    }
                }
            }
            PropId::Seekable => {
                // live streams are not seekable in v1 even if the cache is (no live rewind)
                let live = self.snap.playable.as_ref().map(|p| p.kind.is_live()).unwrap_or(true);
                self.snap.seekable = b(&v) && !live;
            }
            PropId::SubDelay => self.snap.sub_delay = f(&v).unwrap_or(0.0),
            PropId::SubScale => self.snap.sub_scale = f(&v).unwrap_or(1.0),
            PropId::VideoParamsW | PropId::VideoParamsH => self.read_video_size(),
            PropId::HwdecCurrent => {
                self.snap.hwdec = match v {
                    PropValue::Str(s) if !s.is_empty() => Some(s),
                    _ => None,
                }
            }
            PropId::CoreIdle | PropId::Aid | PropId::Sid | PropId::EofReached => {}
        }
        false
    }

    /// Persist progress of the current movie/episode at `pos` (catch-up items are channel refs
    /// and ad-hoc URLs have none, so neither gets a resume point).
    fn save_progress(&mut self, pos: Option<f64>) {
        let Some(p) = self.snap.playable.clone() else { return };
        if !matches!(p.kind, PlayableKind::Movie | PlayableKind::Episode { .. }) {
            return;
        }
        let (Some(item), Some(pos)) = (p.item.clone(), pos) else { return };
        self.deps.store.save_progress(WatchProgress {
            item,
            position_s: pos,
            duration_s: self.snap.duration.filter(|d| *d > 0.0),
            updated_at: crate::model::now(),
            finished: self.watched(Some(pos)),
            title: p.title.clone(),
            fingerprint: Default::default(), // computed by the store
        });
    }

    /// `pos` reached the watched threshold (`watched_pct` of a known duration).
    fn watched(&self, pos: Option<f64>) -> bool {
        match (pos, self.snap.duration) {
            (Some(pos), Some(d)) if d > 0.0 => pos / d * 100.0 >= self.watched_pct as f64,
            _ => false,
        }
    }

    fn push_history(&mut self, p: &Playable) {
        let Some(item) = p.item.clone() else { return };
        self.deps.store.push_history(HistoryEntry { item, at: crate::model::now(), title: p.title.clone() });
    }

    fn record_failure(&mut self, p: &Playable, kind: Option<PlayerErrorKind>) {
        let Some(item) = p.item.clone() else { return };
        if let Some(k) = kind {
            self.snap.last_error = Some(PlayerError { kind: k, message: super::state::describe(k).into() });
        }
        self.deps.store.record_failure(item, kind);
    }

    fn read_video_size(&mut self) {
        let w = self.mpv.get_property::<i64>("video-params/w").ok();
        let h = self.mpv.get_property::<i64>("video-params/h").ok();
        if self.snap.video_size != w.zip(h) {
            self.snap.video_size = w.zip(h);
            self.dirty = true;
        }
    }

    /// A live channel that is Playing records the size it decodes at, once per channel and
    /// size. Only while Playing: during a zap or a reconnect the snapshot can still hold the
    /// previous stream's size. Catch-up archives may be encoded differently and are skipped.
    fn record_video(&mut self) {
        let Some(p) = self.snap.playable.as_ref().filter(|p| p.kind.is_live()) else { return };
        let (Some(item), Some(size)) = (p.item.as_ref(), self.snap.video_size) else { return };
        if self.snap.state != PlayerState::Playing || self.recorded_video.as_ref().is_some_and(|(i, s)| i == item && *s == size) {
            return;
        }
        self.recorded_video = Some((item.clone(), size));
        self.deps.store.record_video(item.clone(), size.0, size.1);
    }

    fn publish(&mut self, state_changed: bool) {
        if !self.dirty && !state_changed && !self.force_publish {
            return;
        }
        let urgent = state_changed || self.force_publish;
        if !urgent && self.last_publish.elapsed() < PUBLISH_EVERY {
            return;
        }
        self.snap.state = self.machine.state().clone();
        self.snap.last_position = self.machine.last_position();
        if let PlayerState::Error(e) = &self.snap.state {
            self.snap.last_error = Some(e.clone());
        }
        self.record_video();
        let _ = self.snap_tx.send(Arc::new(self.snap.clone()));
        if urgent {
            self.deps.bus.request_repaint();
        }
        self.last_publish = Instant::now();
        self.dirty = false;
        self.force_publish = false;
    }
}

/// The close that opens the connection-limit race window for a load of `media`: a stream that
/// closed less than `window` ago and was either another stream (a zap) or this one after it
/// played (stop, then restart the same channel). Our own failed attempts never played, so a
/// refused channel never re-arms its own window.
fn race_close(closed: Option<&Released>, media: &MediaRef, now: Instant, window: Duration) -> Option<Instant> {
    let c = closed?;
    ((c.media != *media || c.played) && now.saturating_duration_since(c.at) < window).then_some(c.at)
}

/// `source 2 xt:live:717661`: a target for the log (no address, no title).
fn playable_label(p: &Playable) -> String {
    let source = p.source_id.map_or_else(|| "no source".to_owned(), |s| format!("source {}", s.0));
    let item = p.item.as_ref().map_or("ad-hoc", |i| i.key.0.as_str());
    format!("{source} {item}")
}

fn state_label(s: &PlayerState) -> String {
    match s {
        PlayerState::Idle => "Idle".into(),
        PlayerState::Zapping { .. } => "Zapping".into(),
        PlayerState::Loading { attempt: 0, .. } => "Loading".into(),
        PlayerState::Loading { attempt, .. } => format!("Loading(attempt {attempt})"),
        PlayerState::Playing => "Playing".into(),
        PlayerState::Paused => "Paused".into(),
        PlayerState::Buffering { .. } => "Buffering".into(),
        PlayerState::Reconnecting { attempt, max, reason, .. } => {
            let of = if *max == 0 { String::new() } else { format!("/{max}") };
            format!("Reconnecting(attempt {attempt}{of}, {reason:?})")
        }
        PlayerState::Ended => "Ended".into(),
        PlayerState::Error(e) => format!("Error({:?}: {})", e.kind, crate::redact::redact(&e.message)),
    }
}

/// `4.2 s`, `3m05s`, `1h12m03s`.
fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{:.1} s", d.as_secs_f32()),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m{:02}s", s / 3600, s / 60 % 60, s % 60),
    }
}

fn log_http_summary((what, n, _): &mpv_log::Summary) {
    tracing::info!("{what}: {n} more in the last 10 s");
}

/// The text of a playlist "entry" that was a line of the provider's prose: mpv joins each line
/// of a headerless `.m3u8` body onto the playlist's directory, so the provider's "Cannot read
/// /home/nxt/storage/streams/717661_.m3u8" becomes `http://host/live/u/p/Cannot read
/// /home/nxt/storage/streams/717661_.m3u8`. A real address has no whitespace; the text starts
/// after the last `/` before the first whitespace. At most 200 characters.
fn provider_text(entry: &str) -> Option<String> {
    let ws = entry.find(char::is_whitespace)?;
    let start = entry[..ws].rfind('/').map_or(0, |i| i + 1);
    let text = entry[start..].split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(200).collect())
}

/// Commands that (may) change what the picture plays: its claim is re-written after them.
fn retargets(cmd: &PlayerCommand) -> bool {
    matches!(
        cmd,
        PlayerCommand::Play(_)
            | PlayerCommand::PlayCopy(_)
            | PlayerCommand::Zap { .. }
            | PlayerCommand::ZapTo { .. }
            | PlayerCommand::LastChannel { .. }
            | PlayerCommand::JumpToLive
            | PlayerCommand::Stop
    )
}

/// Where a channel can play for a picture ([`Controller::placement`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Here,
    Copy(ChannelId),
    /// Its playlist is held by another picture and no copy is free.
    Blocked {
        source: SourceId,
        holder: Option<TileId>,
    },
}

/// What an END_FILE does to the connection lease.
#[derive(Debug, PartialEq)]
enum LeaseOnEnd {
    /// Not the leased entry.
    Unrelated,
    /// The leased stream closed.
    Release,
    /// The leased playlist expanded (redirect): the lease belongs to this inserted entry now
    /// (`None`: mpv didn't say which — the next START_FILE).
    MoveTo(Option<i64>),
}

/// The channel a live or catch-up media plays.
fn channel_of(media: &MediaRef) -> Option<ChannelId> {
    match media {
        MediaRef::Channel(c) | MediaRef::Catchup { channel: c, .. } => Some(*c),
        _ => None,
    }
}

fn lease_on_end(lease_entry: Option<i64>, entry_id: i64, reason: EndReason, inserted: Option<i64>) -> LeaseOnEnd {
    match (lease_entry == Some(entry_id), reason) {
        (false, _) => LeaseOnEnd::Unrelated,
        (true, EndReason::Redirect) => LeaseOnEnd::MoveTo(inserted),
        (true, _) => LeaseOnEnd::Release,
    }
}

/// Stop-before-start decision (DESIGN §11.5): the next stream comes from the same source as
/// the open leased entry, and that source allows one connection or its limit is unknown —
/// `loadfile replace` would open the new connection before the old one is closed. Returns the
/// entry to stop and wait for.
fn must_stop_first(lease: Option<&Lease>, lease_entry: Option<i64>, next: Option<SourceId>, budget: &SharedBudget) -> Option<i64> {
    let (lease, entry, next) = (lease?, lease_entry?, next?);
    (lease.source == next && budget.limit(next).is_none_or(|m| m <= 1)).then_some(entry)
}

/// Extract an HTTP status from an ffmpeg/mpv log line: ffmpeg's http protocol logs
/// "HTTP error 404 Not Found", and its error strings read "Server returned 403 Forbidden
/// (access denied)" or, for statuses without a dedicated code, "Server returned 4XX Client
/// Error, but not one of 40{0,1,3,4}" / "Server returned 5XX Server Error reply" (mapped to
/// 400 / 500).
pub fn http_status_in(text: &str) -> Option<u16> {
    for marker in ["HTTP error ", "Server returned "] {
        if let Some(i) = text.find(marker) {
            let code = text[i + marker.len()..].get(..3)?;
            return match code {
                "4XX" => Some(400),
                "5XX" => Some(500),
                _ => code.parse().ok().filter(|c| (100..600).contains(c)),
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_first_only_for_the_same_source_with_a_limit_of_one_or_unknown() {
        let (a, b) = (SourceId(1), SourceId(2));
        let lease = Lease { source: a };
        let budget = SharedBudget::default();
        // unknown limit: assume the usual Xtream limit of 1
        assert_eq!(must_stop_first(Some(&lease), Some(7), Some(a), &budget), Some(7));
        budget.set_limit(a, Some(1));
        assert_eq!(must_stop_first(Some(&lease), Some(7), Some(a), &budget), Some(7));
        budget.set_limit(a, Some(2));
        assert_eq!(must_stop_first(Some(&lease), Some(7), Some(a), &budget), None, "room for both: plain replace");
        budget.set_limit(a, Some(1));
        assert_eq!(must_stop_first(Some(&lease), Some(7), Some(b), &budget), None, "another source");
        assert_eq!(must_stop_first(Some(&lease), Some(7), None, &budget), None, "ad-hoc URL without a source");
        assert_eq!(must_stop_first(Some(&lease), None, Some(a), &budget), None, "no open entry");
        assert_eq!(must_stop_first(None, Some(7), Some(a), &budget), None, "nothing leased");
    }

    /// Review fix: stopping a channel and restarting it at once on a one-connection account is
    /// the same race as a zap; a failed attempt of the same channel is not.
    #[test]
    fn race_window_opens_after_a_zap_or_a_restart_of_what_played() {
        let now = Instant::now();
        let w = Duration::from_secs(45);
        let a = MediaRef::Url("a".into());
        let b = MediaRef::Url("b".into());
        let closed =
            |media: &MediaRef, played: bool, ago: u64| Released { at: now - Duration::from_secs(ago), media: media.clone(), played, tile: TileId::A };
        assert_eq!(race_close(Some(&closed(&a, false, 5)), &b, now, w), Some(now - Duration::from_secs(5)), "zap");
        assert_eq!(race_close(Some(&closed(&a, true, 5)), &a, now, w), Some(now - Duration::from_secs(5)), "stop + restart");
        assert_eq!(race_close(Some(&closed(&a, false, 5)), &a, now, w), None, "our own failed attempt");
        assert_eq!(race_close(Some(&closed(&a, true, 50)), &b, now, w), None, "too long ago");
        assert_eq!(race_close(None, &a, now, w), None);
    }

    #[test]
    fn provider_text_is_the_prose_joined_onto_the_playlist_address() {
        assert_eq!(
            provider_text("http://h:8080/live/user/pass/Cannot read /home/nxt/storage/streams/717661_.m3u8").as_deref(),
            Some("Cannot read /home/nxt/storage/streams/717661_.m3u8")
        );
        assert_eq!(provider_text("http://h/live/Max connections  reached\t").as_deref(), Some("Max connections reached"));
        assert_eq!(provider_text("Stream not available"), Some("Stream not available".into()), "not joined");
        assert_eq!(provider_text("http://h/live/u/p/717661.ts"), None, "a real address");
        assert_eq!(provider_text("http://h/s/abc/seg001.ts?token=a%20b"), None);
        assert_eq!(provider_text(&format!("http://h/{}", "x y".repeat(200))).map(|t| t.chars().count()), Some(200));
    }

    #[test]
    fn log_labels_and_durations() {
        let now = Instant::now();
        assert_eq!(state_label(&PlayerState::Buffering { since: now, percent: Some(40) }), "Buffering");
        assert_eq!(state_label(&PlayerState::Loading { since: now, attempt: 2 }), "Loading(attempt 2)");
        let r = PlayerState::Reconnecting { attempt: 4, max: 0, at: now, reason: PlayerErrorKind::ProviderError };
        assert_eq!(state_label(&r), "Reconnecting(attempt 4, ProviderError)");
        let r = PlayerState::Reconnecting { attempt: 1, max: 8, at: now, reason: PlayerErrorKind::Network };
        assert_eq!(state_label(&r), "Reconnecting(attempt 1/8, Network)");
        assert_eq!(fmt_duration(Duration::from_millis(4200)), "4.2 s");
        assert_eq!(fmt_duration(Duration::from_secs(185)), "3m05s");
        assert_eq!(fmt_duration(Duration::from_secs(4323)), "1h12m03s");
    }

    #[test]
    fn http_status_parse() {
        assert_eq!(http_status_in("http: HTTP error 403 Forbidden"), Some(403));
        assert_eq!(http_status_in("Failed to open http://h/x: Server returned 404 Not Found"), Some(404));
        assert_eq!(http_status_in("Server returned 401 Unauthorized (authorization failed)"), Some(401));
        assert_eq!(http_status_in("Server returned 4XX Client Error, but not one of 40{0,1,3,4}"), Some(400));
        assert_eq!(http_status_in("Server returned 5XX Server Error reply"), Some(500));
        assert_eq!(http_status_in("HTTP error 12"), None);
        assert_eq!(http_status_in("HTTP error abc"), None);
        assert_eq!(http_status_in("nothing"), None);
    }
}
