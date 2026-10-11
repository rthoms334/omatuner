//! Controller thread: owns the state machine, executes its effects against mpv, applies direct
//! commands (pause, seek, tracks...), zapping, connection budget (stop-before-start),
//! progress persistence, video suspension while nothing renders, and publishes snapshots.
//! Implemented in the skeleton, hardened by WP-07.
//!
//! Snapshot publishing: state changes and command results are published IMMEDIATELY (with a
//! repaint request, so e.g. a volume change while paused is visible at once); continuous values
//! (time-pos, cache) at most every 100 ms; a repaint for time-pos only when the whole second
//! changes (radio must not repaint the UI at audio-packet rate).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, select};
use libmpv2::Mpv;
use tokio::sync::watch;

use super::budget::{ConnectionBudget, Lease};
use super::events::{EndReason, MpvEvent, PropId, PropValue};
use super::mpv_log::{self, MpvLog};
use super::state::{Effect, Input, MachineConfig, PlayerMachine};
use super::tracks::{AspectMode, parse_track_list, query_stream_info};
use super::{PlayRequest, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerEvent, PlayerSnapshot, PlayerState, RenderClock};
use crate::config::Config;
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, PlayOrigin, Playable, PlayableKind, SourceId, WatchProgress};
use crate::msg::AppEvent;

const PROGRESS_EVERY: Duration = Duration::from_secs(10);
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Max wait for the old stream's END_FILE before loading anyway (stop-before-start).
const PENDING_LOAD_MAX: Duration = Duration::from_millis(1500);
/// No rendered frame for this long while Playing → `vid=no` (hidden window safety net).
const SUSPEND_VIDEO_AFTER: Duration = Duration::from_secs(2);
/// How long an END_FILE(error) during Loading waits for the log line that explains it:
/// `mpv_wait_event` returns queued events before log messages, so ffmpeg's "HTTP error 403"
/// (logged before the file ended) is delivered just AFTER the END_FILE.
const END_FILE_LOG_GRACE: Duration = Duration::from_millis(200);

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
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let snap = (**snap_tx.borrow()).clone();
    let mut c = Controller {
        machine: PlayerMachine::new(MachineConfig::from_config(cfg)),
        mpv,
        deps,
        snap,
        snap_tx,
        budget: ConnectionBudget::default(),
        lease: None,
        lease_entry: None,
        lease_redirected: false,
        pending: None,
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
    };
    std::thread::Builder::new().name("player".into()).spawn(move || c.run(cmd_rx, evt_rx))
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
    budget: ConnectionBudget,
    lease: Option<Lease>,
    /// Playlist entry the current lease belongs to (released on its END_FILE).
    lease_entry: Option<i64>,
    /// The leased entry was a playlist that expanded (END_FILE redirect): the lease moves to
    /// the next entry that starts.
    lease_redirected: bool,
    pending: Option<PendingLoad>,
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
                        return;
                    }
                    Ok(cmd) => {
                        state_changed |= self.command(cmd);
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
                    self.pending_tick(now);
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
        *self.machine.state() != before
    }

    /// Execute effects in order. `pos` = position of the target that was current when the
    /// input arrived (`SaveProgress` / `Ended` of a target that is being replaced or stopped).
    fn apply(&mut self, fx: Vec<Effect>, pos: Option<f64>) {
        for e in fx {
            match e {
                Effect::Load(p) => self.load(p),
                Effect::StopMpv => {
                    self.pending = None;
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
                    self.deps.bus.send(AppEvent::Player(ev));
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
            }
        }
        self.dirty = true;
    }

    fn release_lease(&mut self) {
        if let Some(l) = self.lease.take() {
            self.budget.release(l);
        }
        self.lease_entry = None;
        self.lease_redirected = false;
    }

    /// Stop-before-start (see [`must_stop_first`]): `stop`, wait for the old entry's END_FILE
    /// (or 1.5 s), then load.
    fn load(&mut self, p: Arc<Playable>) {
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

    fn load_now(&mut self, p: Arc<Playable>) {
        self.release_lease();
        if let Some(src) = p.source_id {
            match self.budget.try_acquire(src) {
                Ok(l) => self.lease = Some(l),
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
        let entry = match super::mpv::loadfile(&self.mpv, &super::mpv::loadfile_args(&p)) {
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
    }

    fn resolve_and_play(&mut self, req: PlayRequest) -> bool {
        if let Some(z) = req.zap.clone() {
            self.snap.zap = Some(z);
        }
        match self.deps.resolver.resolve(&req.media, req.resume) {
            Ok(p) => {
                self.snap.origin = Some(req.origin);
                self.feed(Input::Play { target: Arc::new(p), immediate: req.immediate }, Instant::now())
            }
            Err(err) => {
                self.snap.last_error = Some(err.clone());
                self.feed(Input::ResolveFailed(err), Instant::now())
            }
        }
    }

    fn zap_request(&self, media: MediaRef, immediate: bool) -> PlayRequest {
        PlayRequest { media, zap: None, origin: self.snap.origin.unwrap_or(PlayOrigin::Fullscreen), resume: false, immediate }
    }

    fn command(&mut self, cmd: PlayerCommand) -> bool {
        let m = self.mpv.clone();
        match cmd {
            PlayerCommand::Play(req) => return self.resolve_and_play(req),
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
                if let Some(next) = self.zap_neighbour(forward, unlocked) {
                    let req = self.zap_request(MediaRef::Channel(next), false);
                    return self.resolve_and_play(req);
                }
            }
            PlayerCommand::ZapTo(id) => {
                let req = self.zap_request(MediaRef::Channel(id), false);
                return self.resolve_and_play(req);
            }
            PlayerCommand::LastChannel { unlocked } => {
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
                let _ = m.command("cycle", &["mute"]);
            }
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
            PlayerCommand::Shutdown => {}
        }
        self.dirty = true;
        false
    }

    /// Settings changed (DESIGN §13.9): everything applies at runtime — the changed entries of
    /// the setting list shared with init (`mpv::runtime_settings`) plus changed raw
    /// `mpv_options`. Unchanged settings are left alone, so a subtitle track or sub scale the
    /// user picked for this stream survives an unrelated settings change.
    fn apply_config(&mut self, cfg: &Config) {
        self.machine.set_config(MachineConfig::from_config(cfg));
        self.watched_pct = cfg.player.watched_pct;
        super::mpv::apply_runtime(&self.mpv, cfg, &self.applied);
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
    /// a PIN set and the sender's session locked, a PIN-locked channel is never tuned.
    fn locked_out(&mut self, ch: ChannelId, unlocked: bool) -> bool {
        let p = &self.applied.parental;
        !unlocked && p.enabled && p.pin_hash.is_some() && self.deps.resolver.is_locked(ch)
    }

    /// Next/previous channel of the ZapList, skipping what the resolver says to skip and, while
    /// the parental session is locked, locked channels.
    fn zap_neighbour(&mut self, forward: bool, unlocked: bool) -> Option<ChannelId> {
        let ids = self.snap.zap.as_ref()?.channels.clone();
        let cur = self.snap.playable.as_ref().and_then(|p| channel_of(&p.media)).unwrap_or(ChannelId(i64::MIN));
        let this = std::cell::RefCell::new(self);
        crate::store::library::LiveIndex::neighbour(&ids, cur, forward, &|c| {
            let mut this = this.borrow_mut();
            this.deps.resolver.skip_on_zap(c) || this.locked_out(c, unlocked)
        })
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
            tracing::info!("no frames rendered for {idle:?}; suspending video decoding");
            let _ = self.mpv.set_property("vid", "no");
            self.snap.video_suspended = true;
            self.force_publish = true;
        }
    }

    /// Feed an END_FILE and release the lease of the stream that just closed (starting a load
    /// that waited for it, see [`Controller::load`]).
    fn end_file(&mut self, reason: EndReason, error: i32, entry_id: i64, inserted: Option<i64>) -> bool {
        tracing::debug!("END_FILE entry {entry_id}: {reason:?} ({error}) in {:?}", self.machine.state());
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
                self.end_file(reason, error, entry_id, inserted) || changed
            }
            MpvEvent::Log { level, prefix, text } => {
                if let Some(code) = http_status_in(&text) {
                    tracing::debug!("HTTP {code} in {:?}", self.machine.state());
                    self.feed(Input::HttpStatus(code), now);
                }
                let (verdict, summary) = self.mpv_log.admit(&prefix, mpv_log::level_of(&level), now);
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
            PropId::Mute => {
                self.snap.muted = b(&v);
                self.force_publish = true;
            }
            PropId::TrackList => {
                if let PropValue::Str(s) = &v {
                    // track-list re-fires on sub-property changes (demux-bitrate…): only
                    // announce real changes
                    let t = parse_track_list(s);
                    if t != self.snap.tracks {
                        self.snap.tracks = t;
                        self.deps.bus.send(AppEvent::Player(PlayerEvent::TracksChanged));
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
fn must_stop_first(lease: Option<&Lease>, lease_entry: Option<i64>, next: Option<SourceId>, budget: &ConnectionBudget) -> Option<i64> {
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
        let mut budget = ConnectionBudget::default();
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
