//! Controller thread: owns the state machine, executes its effects against mpv, applies direct
//! commands (pause, seek, tracks...), zapping, connection budget (stop-before-start),
//! progress persistence, video suspension while nothing renders, and publishes snapshots.
//! Implemented in the skeleton; owner for changes: WP-07.
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
use super::events::{MpvEvent, PropId, PropValue};
use super::state::{Effect, Input, MachineConfig, PlayerMachine};
use super::tracks::{AspectMode, parse_track_list, query_stream_info};
use super::{PlayRequest, PlayerCommand, PlayerDeps, PlayerError, PlayerErrorKind, PlayerEvent, PlayerSnapshot, PlayerState, RenderClock};
use crate::config::{Config, SubtitleAuto};
use crate::model::{HistoryEntry, MediaRef, PlayOrigin, Playable, PlayableKind, WatchProgress};
use crate::msg::AppEvent;

const PROGRESS_EVERY: Duration = Duration::from_secs(10);
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Max wait for the old stream's END_FILE before loading anyway (stop-before-start).
const PENDING_LOAD_MAX: Duration = Duration::from_millis(1500);
/// No rendered frame for this long while Playing → `vid=no` (hidden window safety net).
const SUSPEND_VIDEO_AFTER: Duration = Duration::from_secs(2);

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
        pending: None,
        loaded_entry: None,
        last_progress: Instant::now(),
        last_publish: Instant::now(),
        dirty: false,
        force_publish: false,
        last_second: None,
        watched_pct: cfg.player.watched_pct,
        render_clock,
        epoch,
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
    pending: Option<PendingLoad>,
    /// Entry id of the last loadfile.
    loaded_entry: Option<i64>,
    last_progress: Instant,
    last_publish: Instant,
    dirty: bool,
    force_publish: bool,
    last_second: Option<i64>,
    watched_pct: u8,
    render_clock: RenderClock,
    epoch: Instant,
}

impl Controller {
    fn run(&mut self, cmd_rx: Receiver<PlayerCommand>, evt_rx: Receiver<MpvEvent>) {
        let ticker = crossbeam_channel::tick(Duration::from_millis(250));
        loop {
            let mut state_changed = false;
            select! {
                recv(cmd_rx) -> cmd => match cmd {
                    Ok(PlayerCommand::Shutdown) | Err(_) => {
                        let fx = self.machine.handle(Input::Stop, Instant::now());
                        self.apply(fx);
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
                recv(ticker) -> _ => {
                    let now = Instant::now();
                    state_changed |= self.feed(Input::Tick, now);
                    self.pending_tick(now);
                    self.video_watchdog(now);
                    if now.duration_since(self.last_progress) >= PROGRESS_EVERY
                        && matches!(self.machine.state(), PlayerState::Playing)
                    {
                        self.last_progress = now;
                        self.save_progress();
                    }
                },
            }
            self.publish(state_changed);
        }
    }

    /// Feed the machine and apply effects; returns true if the state changed.
    fn feed(&mut self, input: Input, now: Instant) -> bool {
        let before = self.machine.state().clone();
        let fx = self.machine.handle(input, now);
        self.apply(fx);
        *self.machine.state() != before
    }

    fn apply(&mut self, fx: Vec<Effect>) {
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
                Effect::Emit(ev) => {
                    if let PlayerEvent::NowPlaying(p) = &ev {
                        let prev = self.snap.playable.clone();
                        if let Some(prev) = prev
                            && prev.kind.is_live() && prev.media != p.media {
                                self.snap.previous_live = Some(prev);
                            }
                        self.snap.playable = Some(p.clone());
                    }
                    self.deps.bus.send(AppEvent::Player(ev));
                }
                Effect::SaveProgress { .. } => self.save_progress(),
                Effect::PushHistory(p) => self.push_history(&p),
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
    }

    /// Stop-before-start: same source with a limit ≤ 1 (or unknown) and a stream still open →
    /// `stop`, wait for its END_FILE (or 1.5 s), then load.
    fn load(&mut self, p: Arc<Playable>) {
        let must_wait = match (&self.lease, p.source_id) {
            (Some(l), Some(src)) => l.source == src && self.lease_entry.is_some() && self.budget.limit(src).is_none_or(|m| m <= 1),
            _ => false,
        };
        if must_wait {
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
                    let fx = self.machine.handle(
                        Input::Fail(PlayerError { kind: PlayerErrorKind::Budget, message: e.to_string() }),
                        Instant::now(),
                    );
                    self.apply(fx);
                    return;
                }
            }
        }
        self.snap.load_generation += 1;
        self.snap.position = None;
        self.snap.duration = p.duration_hint;
        self.snap.stream_info = None;
        self.snap.tracks = Default::default();
        self.snap.last_error = None;
        let args = super::mpv::loadfile_args(&p);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = self.mpv.command("loadfile", &refs) {
            let err = PlayerError { kind: PlayerErrorKind::Mpv, message: crate::error::Error::from(e).to_string() };
            let fx = self.machine.handle(Input::Fail(err), Instant::now());
            self.apply(fx);
            return;
        }
        // `loadfile … replace` rebuilt the playlist synchronously; this thread is its only writer.
        let entry = self.mpv.get_property::<i64>("playlist/0/id").ok();
        self.loaded_entry = entry;
        self.lease_entry = entry;
        let fx = self.machine.handle(Input::Loaded { entry }, Instant::now());
        self.apply(fx);
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
            PlayerCommand::Zap { forward } => {
                if let Some(next) = self.zap_neighbour(forward) {
                    let req = self.zap_request(MediaRef::Channel(next), false);
                    return self.resolve_and_play(req);
                }
            }
            PlayerCommand::ZapTo(id) => {
                let req = self.zap_request(MediaRef::Channel(id), false);
                return self.resolve_and_play(req);
            }
            PlayerCommand::LastChannel => {
                if let Some(prev) = self.snap.previous_live.clone() {
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

    /// Settings changed (DESIGN §13.9): everything applies at runtime.
    fn apply_config(&mut self, cfg: &Config) {
        self.machine.set_config(MachineConfig::from_config(cfg));
        self.watched_pct = cfg.player.watched_pct;
        let p = &cfg.player;
        let m = &self.mpv;
        let readahead = if p.instant_load { 1 } else { p.cache_secs.max(1) } as i64;
        let _ = m.set_property("hwdec", cfg.effective_hwdec().as_str());
        let _ = m.set_property("cache-secs", readahead);
        let _ = m.set_property("demuxer-readahead-secs", readahead);
        let _ = m.set_property("demuxer-max-bytes", format!("{}MiB", p.demuxer_max_mib).as_str());
        let _ = m.set_property("network-timeout", p.network_timeout_s as i64);
        let _ = m.set_property("cache-pause-initial", if p.instant_load { "no" } else { "yes" });
        let _ = m.set_property("alang", p.audio_langs.join(",").as_str());
        let _ = m.set_property("slang", p.sub_langs.join(",").as_str());
        let _ = m.set_property("subs-fallback-forced", if p.sub_auto == SubtitleAuto::Forced { "always" } else { "no" });
        let _ = m.set_property("subs-fallback", if p.sub_auto == SubtitleAuto::Always { "yes" } else { "default" });
        let _ = m.set_property("sub-scale", p.sub_scale);
        let _ = m.set_property("user-agent", p.user_agent.as_str());
    }

    fn set_aspect(&mut self, a: AspectMode) {
        let (ovr, panscan, keep) = a.mpv_values();
        let _ = self.mpv.set_property("video-aspect-override", ovr);
        let _ = self.mpv.set_property("panscan", panscan);
        let _ = self.mpv.set_property("keepaspect", keep);
        self.snap.aspect = a;
    }

    /// Next/previous channel of the ZapList, skipping what the resolver says to skip.
    fn zap_neighbour(&mut self, forward: bool) -> Option<crate::model::ChannelId> {
        let ids = self.snap.zap.as_ref()?.channels.clone();
        let cur = match self.snap.playable.as_ref().map(|p| &p.media) {
            Some(MediaRef::Channel(c)) | Some(MediaRef::Catchup { channel: c, .. }) => *c,
            _ => crate::model::ChannelId(i64::MIN),
        };
        let resolver = &mut self.deps.resolver;
        let skip = std::cell::RefCell::new(|c| resolver.skip_on_zap(c));
        crate::store::library::LiveIndex::neighbour(&ids, cur, forward, &|c| (skip.borrow_mut())(c))
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

    fn mpv_event(&mut self, ev: MpvEvent) -> bool {
        let now = Instant::now();
        match ev {
            MpvEvent::StartFile { entry_id } => self.feed(Input::StartFile { entry: entry_id }, now),
            MpvEvent::FileLoaded => self.feed(Input::FileLoaded, now),
            MpvEvent::PlaybackRestart => self.feed(Input::PlaybackRestart, now),
            MpvEvent::EndFile { reason, error, entry_id } => {
                let changed = self.feed(Input::EndFile { reason, error, entry: entry_id }, now);
                if self.lease_entry == Some(entry_id) {
                    // the leased stream is really closed now
                    self.release_lease();
                    if let Some(pl) = self.pending.take() {
                        self.load_now(pl.playable);
                    }
                }
                changed
            }
            MpvEvent::Log { level, prefix, text } => {
                if let Some(code) = http_status_in(&text) {
                    self.feed(Input::HttpStatus(code), now);
                }
                tracing::warn!(target: "mpv", "[{level}] {prefix}: {text}");
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
            PropId::VideoParamsW | PropId::VideoParamsH => {
                let w = self.mpv.get_property::<i64>("video-params/w").ok();
                let h = self.mpv.get_property::<i64>("video-params/h").ok();
                self.snap.video_size = w.zip(h);
            }
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

    fn save_progress(&mut self) {
        let Some(p) = self.snap.playable.clone() else { return };
        if !matches!(p.kind, PlayableKind::Movie | PlayableKind::Episode { .. }) {
            return;
        }
        let (Some(item), Some(pos)) = (p.item.clone(), self.machine.last_position().or(self.snap.position)) else { return };
        let dur = self.snap.duration;
        let finished = dur.map(|d| d > 0.0 && pos / d * 100.0 >= self.watched_pct as f64).unwrap_or(false);
        self.deps.store.save_progress(WatchProgress {
            item,
            position_s: pos,
            duration_s: dur,
            updated_at: crate::model::now(),
            finished,
            title: p.title.clone(),
            fingerprint: Default::default(), // computed by the store
        });
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
        let _ = self.snap_tx.send(Arc::new(self.snap.clone()));
        if urgent {
            self.deps.bus.request_repaint();
        }
        self.last_publish = Instant::now();
        self.dirty = false;
        self.force_publish = false;
    }
}

/// Extract an HTTP status from an ffmpeg/mpv log line ("HTTP error 404 Not Found").
pub fn http_status_in(text: &str) -> Option<u16> {
    let i = text.find("HTTP error ")?;
    text[i + 11..].get(..3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn http_status_parse() {
        assert_eq!(super::http_status_in("http: HTTP error 403 Forbidden"), Some(403));
        assert_eq!(super::http_status_in("nothing"), None);
    }
}
