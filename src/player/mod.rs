//! The player: one libmpv instance, a controller thread with the state machine, an event pump
//! thread, and the GL render surface (UI thread). Owner: WP-07. DESIGN §11.
//!
//! ```text
//!  UI thread ──PlayerCommand──▶ controller thread ◀──MpvEvent── mpv-events thread
//!  MPRIS (tokio) ─────────────▶   (PlayerMachine,                 (mpv_wait_event)
//!  reminders ─────────────────▶    reconnect, zap,
//!                                  progress saving)
//!        ◀── watch<Arc<PlayerSnapshot>> + AppEvent::Player ──┘
//!  UI thread: VideoSurface::render in App::logic (GL), paints texture where the layout says.
//! ```
//! Commands may be sent from any thread; nothing in the controller depends on the UI running.
//!
//! THREADING RULE (mpv `render.h`): the UI thread is the render thread. It must not call any
//! libmpv function that is not marked render-safe, and must never wait on a thread that does.
//! UI code therefore talks to mpv ONLY through [`PlayerHandle`] (a non-blocking channel) and
//! reads [`PlayerSnapshot`]s; it never calls `Mpv::*` and never blocks on the controller. The
//! only mpv access the UI gets is [`Player::render_core`] (raw handle for
//! `mpv_render_context_create`, used once by `VideoSurface::new`). The `Arc<Mpv>` stays
//! private: libmpv2 marks `Mpv` `Send + Sync` and `wait_event(&self)` would allow a second
//! concurrent waiter (unsound with mpv); the `mpv-events` thread is the only waiter. Never use
//! libmpv2's `Mpv::create_client` (edition-2024 temporary-lifetime bug: dangling name pointer);
//! if a second handle is ever needed use `sys::mpv_create_weak_client` with a local `CString`.
//!
//! Hidden windows (DESIGN §11.1): `App::logic` only runs when the compositor sends frame
//! callbacks. Hyprland sends none to windows on hidden workspaces unless the window has the
//! `render_unfocused` rule (shipped in `packaging/hypr/tuner.lua`). As a safety net the
//! controller watches the [`RenderClock`]: while Playing with no rendered frame for 10 s it sets
//! `vid=no` (audio continues, no decode cost, no VO stalls) and publishes
//! `video_suspended = true`; the app sends [`PlayerCommand::ResumeVideo`] as soon as it runs
//! `logic` again.

pub mod budget;
pub mod controller;
pub mod events;
pub mod mpv;
pub mod mpv_log;
pub mod reconnect;
pub mod render;
pub mod state;
pub mod tiles;
pub mod tracks;

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crossbeam_channel::Sender;
use libmpv2::Mpv;
use tokio::sync::watch;

pub use render::VideoSurface;
pub use tracks::{AspectMode, StreamInfo, Track, TrackKind, Tracks};

use crate::config::Config;
use crate::error::Result;
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, PlayOrigin, Playable, WatchProgress, ZapList};
use crate::msg::EventBus;

/// Request to play something. Resolution (URL building, DB reads) happens on the controller
/// thread via the injected [`MediaResolver`], never on the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayRequest {
    pub media: MediaRef,
    /// Zap order for Up/Down / MPRIS Next/Previous (live only).
    pub zap: Option<ZapList>,
    pub origin: PlayOrigin,
    /// Apply saved VOD progress (the UI asked Resume vs Start over beforehand).
    pub resume: bool,
    /// Skip the zapping delay (explicit Enter); zaps/preview autoplay set false.
    pub immediate: bool,
}

/// Play one copy of a title that several playlists have (DESIGN §29.15).
#[derive(Debug, Clone, PartialEq)]
pub struct CopyPlay {
    pub req: PlayRequest,
    /// The next copy in the play order (`store::copies::CopyPlan::fallback`): tried ONCE, at
    /// the same position, when `req` can't be resolved or fails before its first frame
    /// ([`PlayerEvent::FellBack`]). A failure after the first frame never switches copies.
    pub fallback: Option<MediaRef>,
    /// When `fallback` is in the same playlist as `req`: the first copy in another playlist,
    /// tried instead when the failure was the playlist's (HTTP 401/403, our connection budget
    /// for it) — a copy in the same playlist would fail the same way.
    pub elsewhere: Option<MediaRef>,
    /// Start here (seconds) instead of the copy's own saved progress: "Play from TV 2 at
    /// 41:20" after the first copy failed mid-film. Overrides `req.resume`.
    pub start_at: Option<f64>,
}

impl CopyPlay {
    /// The copy to fall back to after a failure of `kind` before the first frame.
    pub fn fallback_for(fallback: Option<MediaRef>, elsewhere: Option<MediaRef>, kind: PlayerErrorKind) -> Option<MediaRef> {
        match kind {
            PlayerErrorKind::HttpForbidden | PlayerErrorKind::Budget => elsewhere.or(fallback),
            _ => fallback,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    Play(PlayRequest),
    /// [`PlayerCommand::Play`] of a title's copy, with the copy to fall back on.
    PlayCopy(CopyPlay),
    Stop,
    TogglePause,
    SetPause(bool),
    /// Relative seek in seconds (negative = back). Ignored for non-seekable live.
    SeekBy(f64),
    /// Absolute position in seconds.
    SeekTo(f64),
    /// Zap within the current ZapList (wraps; skips channels for which
    /// `MediaResolver::skip_on_zap` is true: ≥ 2 recent failures, DRM, resolve failures). Keeps
    /// the current `PlayOrigin`.
    Zap {
        forward: bool,
        /// The sender's parental session is unlocked (`ParentalSession::is_unlocked`). When
        /// false and a PIN is set, PIN-locked channels are skipped too (MPRIS always sends
        /// false: it can't ask for the PIN); explicit channels are skipped whenever
        /// `ParentalConfig::hides_explicit(unlocked)`.
        unlocked: bool,
    },
    /// Tune a channel by id keeping the current ZapList and origin (numeric entry, history).
    ZapTo(ChannelId),
    /// Recall the previously watched live channel (Backspace / UHF ←). Ignored when that
    /// channel is PIN-locked, a PIN is set and `unlocked` is false (the session re-locked since).
    LastChannel {
        unlocked: bool,
    },
    /// From catch-up back to the live stream of the same channel.
    JumpToLive,
    SetVolume(f64),
    AdjustVolume(f64),
    ToggleMute,
    /// `None` = off.
    SetAudioTrack(Option<i64>),
    SetSubtitleTrack(Option<i64>),
    CycleAudio,
    CycleSubtitle,
    AdjustSubDelay(f64),
    ResetSubDelay,
    AdjustSubScale(f64),
    SetAspect(AspectMode),
    CycleAspect,
    /// `video-zoom` (log2 scale; 0 = none).
    SetZoom(f64),
    /// User-initiated retry from the error state.
    Reconnect,
    /// Refresh `PlayerSnapshot.stream_info` (stream info panel open).
    RefreshStreamInfo,
    SetZapList(ZapList),
    /// Provider limits from account info (budget). Sent by the app at startup for every source
    /// (from stored `AccountInfo`) and after every `JobOutcome::SourceRefreshed`.
    SetConnectionLimit {
        source: crate::model::SourceId,
        max: Option<u32>,
    },
    /// Settings changed: re-derive `MachineConfig` and set the runtime-settable mpv properties
    /// (`hwdec`, `cache-secs`, `demuxer-readahead-secs`, `demuxer-max-bytes`,
    /// `network-timeout`, `alang`, `slang`, subtitle auto mode, `sub-scale`, user-agent).
    /// Every v1 setting applies without restart (raw `mpv_options` apply to the next file).
    ApplyConfig(Box<Config>),
    /// The UI renders frames again after `video_suspended` (see module docs).
    ResumeVideo,
    Shutdown,
}

/// Why playback failed (drives reconnect policy and the error overlay copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayerErrorKind {
    Network,
    /// No first frame within the load timeout.
    Timeout,
    /// Playing but time-pos frozen / buffering too long.
    Stalled,
    /// EOF on a live stream.
    StreamEnded,
    /// 401/403 — expired account or connection limit.
    HttpForbidden,
    /// 404/410 — channel offline.
    HttpNotFound,
    HttpOther(u16),
    /// mpv can't demux/decode (unknown format).
    Unsupported,
    /// DRM-protected (from M3U KODIPROP).
    Drm,
    /// URL could not be built (missing source, catch-up outside window...).
    Resolve,
    /// Connection budget exceeded.
    Budget,
    /// The provider answered the stream request with a message instead of a stream (its
    /// restreamer lost the channel: "Cannot read /home/…/streams/123_.m3u8"), usually for a
    /// while. The message is in [`PlayerError::message`].
    ProviderError,
    /// Other mpv error.
    Mpv,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerError {
    pub kind: PlayerErrorKind,
    /// Redacted, user-presentable.
    pub message: String,
}

/// Player lifecycle (DESIGN §11.3 state diagram).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerState {
    Idle,
    /// Waiting out the zapping delay before connecting.
    Zapping {
        until: Instant,
    },
    /// loadfile issued, waiting for the first frame.
    Loading {
        since: Instant,
        attempt: u32,
    },
    Playing,
    Paused,
    /// Cache underrun (`paused-for-cache`), `percent` from `cache-buffering-state`.
    Buffering {
        since: Instant,
        percent: Option<i64>,
    },
    /// Waiting to retry.
    Reconnecting {
        attempt: u32,
        /// Attempts allowed before giving up; 0 = no limit (a live channel that already played
        /// keeps reconnecting until stopped, and the connection-limit race window is bounded by
        /// time — the overlay reads "attempt n").
        max: u32,
        at: Instant,
        reason: PlayerErrorKind,
    },
    /// VOD reached the end.
    Ended,
    Error(PlayerError),
}

impl PlayerState {
    /// Video is (about to be) on screen: idle inhibit, MPRIS "Playing".
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            PlayerState::Playing
                | PlayerState::Buffering { .. }
                | PlayerState::Loading { .. }
                | PlayerState::Reconnecting { .. }
                | PlayerState::Zapping { .. }
        )
    }
}

/// Everything the UI/MPRIS need, published through a `watch` channel.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    /// Current (or zapping-to) item.
    pub playable: Option<Arc<Playable>>,
    /// Previous live item (Backspace recall).
    pub previous_live: Option<Arc<Playable>>,
    pub zap: Option<ZapList>,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    /// Seconds buffered ahead.
    pub cache_secs: Option<f64>,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub seekable: bool,
    pub tracks: Tracks,
    pub sub_delay: f64,
    pub sub_scale: f64,
    pub aspect: AspectMode,
    pub zoom: f64,
    pub video_size: Option<(i64, i64)>,
    pub hwdec: Option<String>,
    pub stream_info: Option<StreamInfo>,
    pub last_error: Option<PlayerError>,
    /// Incremented on every new load; the video surface clears to black when it changes.
    pub load_generation: u64,
    /// Where the current item was started from (preview autoplay may only replace a stream it
    /// started itself; zaps keep the origin).
    pub origin: Option<PlayOrigin>,
    /// Video decoding is paused because nothing rendered for 10 s (hidden window, see module
    /// docs). The app sends `ResumeVideo` when it renders again.
    pub video_suspended: bool,
    /// Last position of the current target, kept across reconnects (`position` is cleared by
    /// every load): where "Play from TV 2 at 41:20" starts after a failure mid-film.
    pub last_position: Option<f64>,
}

impl Default for PlayerSnapshot {
    fn default() -> Self {
        Self {
            state: PlayerState::Idle,
            playable: None,
            previous_live: None,
            zap: None,
            position: None,
            duration: None,
            cache_secs: None,
            paused: false,
            volume: 100.0,
            muted: false,
            seekable: false,
            tracks: Tracks::default(),
            sub_delay: 0.0,
            sub_scale: 1.0,
            aspect: AspectMode::Auto,
            zoom: 0.0,
            video_size: None,
            hwdec: None,
            stream_info: None,
            last_error: None,
            load_generation: 0,
            origin: None,
            video_suspended: false,
            last_position: None,
        }
    }
}

/// Discrete notifications (the snapshot carries continuous state).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    StateChanged(PlayerState),
    /// A new item became current (also during Zapping, so the OSD shows it immediately).
    NowPlaying(Arc<Playable>),
    /// VOD ended (`finished` = reached watched threshold).
    Ended {
        item: Option<ItemRef>,
        finished: bool,
    },
    Failed(PlayerError),
    TracksChanged,
    /// The first copy of a title failed before its first frame (`error`) and the player went
    /// on with the next one ([`CopyPlay::fallback`]): the app toasts "TV couldn't play Dune —
    /// playing from TV 2".
    FellBack {
        title: String,
        from: Option<crate::model::SourceId>,
        to: Option<crate::model::SourceId>,
        error: PlayerErrorKind,
    },
}

/// Resolves `MediaRef`s on the controller thread (owns its own DB reader). Built by
/// `Services` around `sources::resolve::resolve`; tests inject fakes (WP-07 needs no store).
pub trait MediaResolver: Send {
    /// Maps DRM channels to `PlayerErrorKind::Drm`, everything else to `Resolve`.
    fn resolve(&mut self, media: &MediaRef, resume: bool) -> std::result::Result<Playable, PlayerError>;
    /// Should zapping skip this channel? (≥ 2 failures in 24 h from `stream_failure`, DRM, or
    /// a resolve failure earlier in this session.)
    fn skip_on_zap(&mut self, ch: ChannelId) -> bool;
    /// Is this channel PIN-locked (its own lock or any of its groups')? A channel whose lock
    /// can't be read counts as locked.
    fn is_locked(&mut self, ch: ChannelId) -> bool;
    /// Is this channel explicit content (DESIGN §20.3)? Zapping and recall skip it while
    /// parental control hides explicit content. A channel that can't be read counts as explicit.
    fn is_explicit(&mut self, _ch: ChannelId) -> bool {
        false
    }
}

/// Persistence the controller needs. `Services` adapts it to the store `Writer`
/// (fire-and-forget `submit_notify`); tests use a recording fake. Implementations must not
/// block.
pub trait PlayerStore: Send {
    /// VOD/catch-up progress (fingerprint left empty — the store computes it).
    fn save_progress(&mut self, p: WatchProgress);
    fn push_history(&mut self, e: HistoryEntry);
    /// `Some(kind)` = record a failure, `None` = clear (successful play).
    fn record_failure(&mut self, item: ItemRef, kind: Option<PlayerErrorKind>);
    /// The picture size a live channel decodes at (its measured quality badge).
    fn record_video(&mut self, item: ItemRef, width: i64, height: i64);
}

/// A `PlayerStore` that drops everything (headless tools, tests that don't care).
pub struct NullPlayerStore;

impl PlayerStore for NullPlayerStore {
    fn save_progress(&mut self, _p: WatchProgress) {}
    fn push_history(&mut self, _e: HistoryEntry) {}
    fn record_failure(&mut self, _item: ItemRef, _kind: Option<PlayerErrorKind>) {}
    fn record_video(&mut self, _item: ItemRef, _width: i64, _height: i64) {}
}

/// Dependencies injected into the controller.
pub struct PlayerDeps {
    pub bus: EventBus,
    pub store: Box<dyn PlayerStore>,
    pub resolver: Box<dyn MediaResolver>,
}

/// Milliseconds since [`Player::start`] of the last frame the UI rendered (0 = never / no
/// surface). Written by `VideoSurface::render`, read by the controller tick.
pub type RenderClock = Arc<AtomicU64>;

/// What `VideoSurface::new` needs from the player: the raw core handle (render-context
/// creation only), a liveness flag (while set, the player never destroys mpv — the render
/// context must be freed first) and the render clock.
pub struct RenderCore {
    pub(crate) handle: NonNull<libmpv2_sys::mpv_handle>,
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) clock: RenderClock,
    pub(crate) epoch: Instant,
}

/// Cloneable handle for issuing commands and reading state. `Send + Sync`.
#[derive(Clone)]
pub struct PlayerHandle {
    tx: Sender<PlayerCommand>,
    snapshot: watch::Receiver<Arc<PlayerSnapshot>>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCommand) {
        let _ = self.tx.send(cmd);
    }
    /// Latest snapshot (cheap Arc clone).
    pub fn snapshot(&self) -> Arc<PlayerSnapshot> {
        self.snapshot.borrow().clone()
    }
    /// For async consumers (MPRIS) that await changes.
    pub fn subscribe(&self) -> watch::Receiver<Arc<PlayerSnapshot>> {
        self.snapshot.clone()
    }
}

/// Owns mpv + threads. Lives in `Services`; shut down via [`Player::shutdown`] AFTER the video
/// surface was destroyed (render context must be freed before mpv).
pub struct Player {
    mpv: Option<Arc<Mpv>>,
    handle: PlayerHandle,
    render_alive: Arc<AtomicBool>,
    render_clock: RenderClock,
    epoch: Instant,
    pump: Option<events::EventPump>,
    controller: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    /// Create mpv with options from `cfg`, start the event pump and controller threads.
    pub fn start(cfg: &Config, deps: PlayerDeps) -> Result<Self> {
        let mpv = Arc::new(mpv::create(cfg)?);
        let version = |p: &str| mpv.get_property::<String>(p).unwrap_or_else(|_| "?".into());
        tracing::info!("Tuner {}, {}, FFmpeg {}", env!("CARGO_PKG_VERSION"), version("mpv-version"), version("ffmpeg-version"));
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded();
        let (snap_tx, snap_rx) = watch::channel(Arc::new(PlayerSnapshot {
            volume: cfg.player.volume,
            muted: cfg.player.muted,
            sub_scale: cfg.player.sub_scale,
            ..Default::default()
        }));
        let pump = events::EventPump::start(mpv.clone(), evt_tx).map_err(crate::error::Error::other)?;
        let render_clock: RenderClock = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let controller =
            controller::spawn(mpv.clone(), cfg, deps, cmd_rx, evt_rx, snap_tx, render_clock.clone(), epoch).map_err(crate::error::Error::other)?;
        Ok(Self {
            mpv: Some(mpv),
            handle: PlayerHandle { tx: cmd_tx, snapshot: snap_rx },
            render_alive: Arc::new(AtomicBool::new(false)),
            render_clock,
            epoch,
            pump: Some(pump),
            controller: Some(controller),
        })
    }

    pub fn handle(&self) -> PlayerHandle {
        self.handle.clone()
    }

    /// Raw core for `VideoSurface::new` ONLY (see module docs: no other mpv access from the UI).
    pub(crate) fn render_core(&self) -> Option<RenderCore> {
        let mpv = self.mpv.as_ref()?;
        Some(RenderCore { handle: mpv.ctx, alive: self.render_alive.clone(), clock: self.render_clock.clone(), epoch: self.epoch })
    }

    /// Stop playback, join threads. Call after `VideoSurface::destroy`.
    pub fn shutdown(&mut self) {
        self.handle.send(PlayerCommand::Shutdown);
        if let Some(j) = self.controller.take() {
            let _ = j.join();
        }
        if let Some(mut p) = self.pump.take() {
            p.stop();
        }
        // A render context still alive here means `VideoSurface::destroy` was skipped (panic /
        // unwind). Destroying mpv now would be UB (render ctx must be freed first), so leak it.
        if self.render_alive.load(Ordering::Acquire)
            && let Some(m) = self.mpv.take()
        {
            tracing::error!("player dropped while the video surface is alive; leaking mpv");
            std::mem::forget(m);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Headless controller tests against a real libmpv (`vo=null`, `ao=null`) playing libavdevice's
/// `lavfi` test source (no files, no network). File/HTTP scenarios live in `tests/player_mpv.rs`.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{ChannelId, PlayableKind};
    use crate::redact::Secret;

    /// Plays lavfi's test source for any media; the listed channels are PIN-locked (`.0`) or
    /// explicit content (`.1`).
    struct Lavfi(Vec<ChannelId>, Vec<ChannelId>);

    impl MediaResolver for Lavfi {
        fn resolve(&mut self, media: &MediaRef, _resume: bool) -> std::result::Result<Playable, PlayerError> {
            Ok(Playable {
                media: media.clone(),
                kind: PlayableKind::Live,
                item: None,
                source_id: None,
                title: "lavfi".into(),
                subtitle: None,
                logo: None,
                number: None,
                url: Secret::new("av://lavfi:testsrc2=size=160x90:rate=25".into()),
                headers: Default::default(),
                start_at: None,
                duration_hint: None,
            })
        }
        fn skip_on_zap(&mut self, _ch: ChannelId) -> bool {
            false
        }
        fn is_locked(&mut self, ch: ChannelId) -> bool {
            self.0.contains(&ch)
        }
        fn is_explicit(&mut self, ch: ChannelId) -> bool {
            self.1.contains(&ch)
        }
    }

    fn headless_config() -> Config {
        let mut cfg = Config::default();
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        cfg
    }

    fn start() -> Player {
        start_with(&headless_config(), Lavfi(Vec::new(), Vec::new()))
    }

    fn start_with(cfg: &Config, resolver: Lavfi) -> Player {
        let deps = PlayerDeps { bus: EventBus::new(None).0, store: Box::new(NullPlayerStore), resolver: Box::new(resolver) };
        Player::start(cfg, deps).expect("headless mpv")
    }

    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        f()
    }

    fn play(p: &Player) {
        p.handle().send(PlayerCommand::Play(PlayRequest {
            media: MediaRef::Channel(ChannelId(1)),
            zap: None,
            origin: PlayOrigin::Cli,
            resume: false,
            immediate: true,
        }));
    }

    #[test]
    fn apply_config_reaches_mpv() {
        let p = start();
        let mpv = p.mpv.clone().expect("mpv");
        let mut cfg = headless_config();
        cfg.player.cache_secs = 4;
        cfg.player.network_timeout_s = 7;
        cfg.player.user_agent = "ApplyConfig/1".into();
        cfg.player.mpv_options.insert("video-sync".into(), "display-resample".into());
        p.handle().send(PlayerCommand::ApplyConfig(Box::new(cfg)));
        // the controller sets the properties one by one: wait for the last one it applies
        assert!(wait_until(Duration::from_secs(5), || { mpv.get_property::<String>("video-sync").ok().as_deref() == Some("display-resample") }));
        assert_eq!(mpv.get_property::<f64>("cache-secs").unwrap(), 4.0);
        assert_eq!(mpv.get_property::<f64>("demuxer-readahead-secs").unwrap(), 4.0);
        assert_eq!(mpv.get_property::<f64>("network-timeout").unwrap(), 7.0);
        assert_eq!(mpv.get_property::<String>("user-agent").unwrap(), "ApplyConfig/1");
    }

    #[test]
    fn video_is_suspended_while_nothing_renders_and_resumed_on_request() {
        let p = start();
        let mpv = p.mpv.clone().expect("mpv");
        let h = p.handle();
        play(&p);
        assert!(
            wait_until(Duration::from_secs(20), || {
                let s = h.snapshot();
                s.state == PlayerState::Playing && s.video_size.is_some()
            }),
            "lavfi playback: {:?}",
            h.snapshot().state
        );
        // no surface yet (clock 0): never suspended
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!h.snapshot().video_suspended);

        // one frame rendered "now", then nothing (hidden window)
        let core = p.render_core().expect("core");
        core.clock.store(core.epoch.elapsed().as_millis().max(1) as u64, Ordering::Relaxed);
        // a quick workspace switch (< 10 s) never suspends: decoding would restart mid-GOP
        std::thread::sleep(Duration::from_secs(8));
        assert!(!h.snapshot().video_suspended, "not after 8 s");
        assert!(wait_until(Duration::from_secs(6), || h.snapshot().video_suspended), "suspended after 10 s");
        assert_eq!(mpv.get_property::<String>("vid").unwrap(), "no");
        assert_eq!(h.snapshot().state, PlayerState::Playing, "audio keeps playing");

        h.send(PlayerCommand::ResumeVideo);
        assert!(wait_until(Duration::from_secs(5), || !h.snapshot().video_suspended));
        assert_ne!(mpv.get_property::<String>("vid").unwrap(), "no");
    }

    /// Audit fix: MPRIS Next/Previous (which can't ask for the PIN) and a Backspace recall after
    /// the session re-locked must never tune a PIN-locked channel; an unlocked UI still can.
    #[test]
    fn zap_and_recall_skip_locked_channels_while_locked() {
        let mut cfg = headless_config();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: false,
            enabled: true,
            pin_hash: Some("h".into()),
            pin_salt: Some("s".into()),
            relock_min: 0,
        };
        let p = start_with(&cfg, Lavfi(vec![ChannelId(2)], Vec::new()));
        let h = p.handle();
        let playing = |id: i64| {
            wait_until(Duration::from_secs(20), || {
                let s = h.snapshot();
                s.state == PlayerState::Playing && s.playable.as_ref().is_some_and(|p| p.media == MediaRef::Channel(ChannelId(id)))
            })
        };
        let zap = ZapList { collection: crate::model::Collection::AllChannels, channels: vec![ChannelId(1), ChannelId(2), ChannelId(3)].into() };
        let media = MediaRef::Channel(ChannelId(1));
        h.send(PlayerCommand::Play(PlayRequest { media, zap: Some(zap), origin: PlayOrigin::Fullscreen, resume: false, immediate: true }));
        assert!(playing(1));

        h.send(PlayerCommand::Zap { forward: true, unlocked: false });
        assert!(playing(3), "locked channel 2 skipped: {:?}", h.snapshot().playable);
        h.send(PlayerCommand::Zap { forward: false, unlocked: true });
        assert!(playing(2), "an unlocked session zaps onto it");
        h.send(PlayerCommand::Zap { forward: true, unlocked: false });
        assert!(playing(3));

        // the recall target is the locked channel: refused while locked
        assert_eq!(h.snapshot().previous_live.as_ref().map(|p| p.media.clone()), Some(MediaRef::Channel(ChannelId(2))));
        h.send(PlayerCommand::LastChannel { unlocked: false });
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(h.snapshot().playable.as_ref().map(|p| p.media.clone()), Some(MediaRef::Channel(ChannelId(3))));
        h.send(PlayerCommand::LastChannel { unlocked: true });
        assert!(playing(2));
    }

    /// Copies of one movie for the fallback tests: `Movie(1)` is a file that doesn't exist (in
    /// playlist 1), `Movie(2)` plays lavfi's test source (playlist 2), `Movie(3)` can't be
    /// resolved.
    struct Copies;

    impl MediaResolver for Copies {
        fn resolve(&mut self, media: &MediaRef, _resume: bool) -> std::result::Result<Playable, PlayerError> {
            use crate::model::{ItemKey, ItemKind, MovieId, SourceId};
            let (id, url) = match media {
                MediaRef::Movie(MovieId(1)) => (1, "/nonexistent/tuner-test/dune.mkv"),
                MediaRef::Movie(MovieId(2)) => (2, "av://lavfi:testsrc2=size=160x90:rate=25"),
                _ => return Err(PlayerError { kind: PlayerErrorKind::Resolve, message: "no such copy".into() }),
            };
            Ok(Playable {
                media: media.clone(),
                kind: PlayableKind::Movie,
                item: Some(ItemRef { source: SourceId(id), kind: ItemKind::Movie, key: ItemKey(format!("m{id}")) }),
                source_id: Some(SourceId(id)),
                title: "Dune".into(),
                subtitle: None,
                logo: None,
                number: None,
                url: Secret::new(url.into()),
                headers: Default::default(),
                start_at: Some(12.0),
                duration_hint: None,
            })
        }
        fn skip_on_zap(&mut self, _ch: ChannelId) -> bool {
            false
        }
        fn is_locked(&mut self, _ch: ChannelId) -> bool {
            false
        }
    }

    /// The first copy fails before its first frame: the player plays the next copy once, at the
    /// position the first was asked to start at, and says so (`FellBack`, the app's toast). A
    /// first copy that can't even be resolved goes to the next one directly. A copy asked to
    /// start elsewhere ("Play from TV 2 at 41:20") starts there, not at its own progress.
    #[test]
    fn a_copy_that_fails_before_its_first_frame_falls_back_to_the_next_once() {
        use crate::model::{MovieId, SourceId};
        let (bus, events) = EventBus::new(None);
        let deps = PlayerDeps { bus, store: Box::new(NullPlayerStore), resolver: Box::new(Copies) };
        let p = Player::start(&headless_config(), deps).expect("headless mpv");
        let h = p.handle();
        let copy = |media: i64, fallback: Option<i64>, start_at: Option<f64>| {
            PlayerCommand::PlayCopy(CopyPlay {
                req: PlayRequest { media: MediaRef::Movie(MovieId(media)), zap: None, origin: PlayOrigin::Fullscreen, resume: true, immediate: true },
                fallback: fallback.map(|f| MediaRef::Movie(MovieId(f))),
                elsewhere: None,
                start_at,
            })
        };
        let fell_back = || {
            events.try_iter().find_map(|e| match e {
                crate::msg::AppEvent::Player(PlayerEvent::FellBack { from, to, .. }) => Some((from, to)),
                _ => None,
            })
        };
        let on = |id: i64| {
            let s = h.snapshot();
            s.playable.as_ref().is_some_and(|p| p.media == MediaRef::Movie(MovieId(id)))
        };

        h.send(copy(1, Some(2), None));
        assert!(wait_until(Duration::from_secs(20), || on(2)), "fell back: {:?}", h.snapshot().state);
        assert_eq!(h.snapshot().playable.as_ref().and_then(|p| p.start_at), Some(12.0), "at the first copy's position");
        assert!(!matches!(h.snapshot().state, PlayerState::Error(_)));
        assert_eq!(fell_back(), Some((Some(SourceId(1)), Some(SourceId(2)))));

        h.send(PlayerCommand::Stop);
        h.send(copy(3, Some(2), Some(2480.0)));
        assert!(wait_until(Duration::from_secs(5), || on(2) && h.snapshot().playable.as_ref().and_then(|p| p.start_at) == Some(2480.0)));
        assert_eq!(fell_back(), Some((None, Some(SourceId(2)))), "unresolvable: the next copy at once");

        // without a fallback the failure is the usual error (after the VOD retries)
        h.send(copy(3, None, None));
        assert!(wait_until(Duration::from_secs(5), || matches!(h.snapshot().last_error, Some(PlayerError { kind: PlayerErrorKind::Resolve, .. }))));
        assert_eq!(fell_back(), None);
    }

    /// Explicit channels (DESIGN §20.3): zapping and recall skip them while parental control
    /// hides explicit content — with a PIN while the sender's session is locked (MPRIS always),
    /// without one unless "Show explicit content" is on.
    #[test]
    fn zap_and_recall_skip_explicit_channels_while_hidden() {
        let mut cfg = headless_config();
        cfg.parental = crate::config::ParentalConfig { enabled: true, pin_hash: Some("h".into()), pin_salt: Some("s".into()), ..Default::default() };
        let p = start_with(&cfg, Lavfi(Vec::new(), vec![ChannelId(2)]));
        let h = p.handle();
        let playing = |id: i64| {
            wait_until(Duration::from_secs(20), || {
                let s = h.snapshot();
                s.state == PlayerState::Playing && s.playable.as_ref().is_some_and(|p| p.media == MediaRef::Channel(ChannelId(id)))
            })
        };
        let zap = ZapList { collection: crate::model::Collection::AllChannels, channels: vec![ChannelId(1), ChannelId(2), ChannelId(3)].into() };
        let media = MediaRef::Channel(ChannelId(1));
        h.send(PlayerCommand::Play(PlayRequest { media, zap: Some(zap), origin: PlayOrigin::Fullscreen, resume: false, immediate: true }));
        assert!(playing(1));
        h.send(PlayerCommand::Zap { forward: true, unlocked: false });
        assert!(playing(3), "explicit channel 2 skipped while locked: {:?}", h.snapshot().playable);
        h.send(PlayerCommand::Zap { forward: false, unlocked: true });
        assert!(playing(2), "an unlocked session zaps onto it");
        h.send(PlayerCommand::Zap { forward: true, unlocked: true });
        assert!(playing(3));
        h.send(PlayerCommand::LastChannel { unlocked: false });
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(h.snapshot().playable.as_ref().map(|p| p.media.clone()), Some(MediaRef::Channel(ChannelId(3))), "no recall while locked");

        // no PIN: the "Show explicit content" switch decides, whatever the sender says
        let mut none = headless_config();
        none.parental = crate::config::ParentalConfig::default();
        h.send(PlayerCommand::ApplyConfig(Box::new(none.clone())));
        h.send(PlayerCommand::Zap { forward: false, unlocked: true });
        assert!(playing(1), "hidden by default without a PIN: {:?}", h.snapshot().playable);
        none.parental.show_explicit = true;
        h.send(PlayerCommand::ApplyConfig(Box::new(none)));
        h.send(PlayerCommand::Zap { forward: true, unlocked: false });
        assert!(playing(2));
    }

    #[test]
    fn shutdown_stops_and_joins() {
        let mut p = start();
        play(&p);
        let h = p.handle();
        assert!(wait_until(Duration::from_secs(20), || h.snapshot().state == PlayerState::Playing));
        p.shutdown();
        assert_eq!(h.snapshot().state, PlayerState::Idle, "final snapshot published on shutdown");
        p.shutdown(); // idempotent
    }
}
