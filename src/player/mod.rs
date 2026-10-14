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
//!
//! Multi-view (DESIGN §35, MV-1): picture B is a second `Player` ([`Player::start_tile`]) — its
//! own mpv, event pump ("mpv-events-b"), controller ("player-b"), render clock and liveness
//! flag — sharing the [`budget::SharedBudget`] with picture A. The whole single-player stack
//! (state machine, reconnects, stop-before-start, the provider-message trap, logging, the
//! hidden-window suspension above) runs per picture; each surface stamps only its own clock, so
//! a picture that isn't placed or rendered suspends on its own. B starts muted
//! ([`PlayerCommand::SetAudible`]) with smaller demuxer caches (`mpv::create`).

pub mod audio_output;
pub mod budget;
pub mod controller;
pub mod events;
pub mod mpv;
pub mod mpv_log;
pub mod reconnect;
pub mod remote;
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
use crate::model::{ChannelId, HistoryEntry, ItemRef, MediaRef, PlayOrigin, Playable, SourceId, TileId, WatchProgress, ZapList};
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

impl From<PlayRequest> for CopyPlay {
    /// A title with no other copy: `req` alone.
    fn from(req: PlayRequest) -> Self {
        Self { req, fallback: None, elsewhere: None, start_at: None }
    }
}

impl CopyPlay {
    /// The copy to fall back to after a failure of `kind` before the first frame.
    pub fn fallback_for(fallback: Option<MediaRef>, elsewhere: Option<MediaRef>, kind: PlayerErrorKind) -> Option<MediaRef> {
        match kind {
            PlayerErrorKind::HttpForbidden | PlayerErrorKind::Budget => elsewhere.or(fallback),
            // the TV's problem: another copy would fail the same way (DESIGN §40 §3.4)
            PlayerErrorKind::Cast(_) => None,
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
    /// `unlocked` as for [`PlayerCommand::Zap`]: while false (and a PIN is set) a PIN-locked
    /// copy never stands in for it when the other picture holds its playlist.
    ZapTo {
        id: ChannelId,
        unlocked: bool,
    },
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
    /// The user's mute (moving the sound carries it to the other picture).
    SetMute(bool),
    /// Multi-view: this picture has the sound (`false`: mpv muted whatever the user's mute;
    /// the snapshot keeps the user's mute and volume). Picture B starts without it.
    SetAudible(bool),
    /// Multi-view: decode the next loads lighter (`vd-lavc-skiploopfilter=all`) — for the
    /// small picture, set when it loads (swapping places doesn't reload).
    SetLightDecode(bool),
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
    /// Cast to `device` (DESIGN §40 §5.5): `play` = what to play there (the agent's `play
    /// {device}`), `None` = move what plays now (at its position; live: the same channel), or,
    /// when nothing plays, the last item (the alerts' **Try again** and **Cast again**). The
    /// local picture stops first, so the relay takes over picture A's lease. Refused in
    /// multi-view. Picture A only.
    CastTo {
        device: crate::model::DeviceId,
        play: Option<PlayRequest>,
    },
    /// [`PlayerCommand::CastTo`] of a title's copy: "Cast to…" on a film or an episode (DESIGN
    /// §40 §3.1) and an agent's `play {device}` of one (§12). It plays on that TV as
    /// `PlayCopy` plays here: with its fallback copy and its start point.
    CastCopy {
        device: crate::model::DeviceId,
        copy: CopyPlay,
    },
    /// The Play on switch (DESIGN §40 §3.9): while it names a TV, picture A's `Play` and
    /// `PlayCopy` go there (`CastTo`) instead of playing here — not the Live preview's (moving
    /// the cursor never casts), not in multi-view (two pictures are this computer's), and on a
    /// TV already they land on that TV anyway. `None` = this computer. Changing it moves nothing
    /// (the chooser sends the move itself). Picture A only.
    SetPlayOn(Option<crate::model::DeviceId>),
    /// End casting: the TV stops, the output goes back to local, then `play_here` resumes here
    /// (VOD and catch-up at the TV's position, live at the live point), else Idle.
    StopCasting {
        play_here: bool,
    },
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
    /// Casting failed (DESIGN §40 §3.4: one sentence and an action per kind). The TV's or the
    /// network's problem, not the item's: never recorded as its stream failure
    /// (`PlayerStore::record_failure`), never a reason to try another copy.
    Cast(crate::model::CastErrorKind),
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
    /// The local audio output and its saved correction (negative = sound earlier).
    pub audio_output: audio_output::AudioOutput,
    pub lip_sync_ms: i32,
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
    /// The picture this player is (A in single view).
    pub tile: TileId,
    /// This picture has the sound (multi-view; always true in single view). `muted` and
    /// `volume` are the user's, whatever this says.
    pub audible: bool,
    /// The playlist this picture holds in the shared connection budget (play through
    /// Reconnecting; `None` once stopped, ended or failed for good).
    pub claim: Option<SourceId>,
    /// A copy plays in place of this zap-list channel (its playlist was held by the other
    /// picture): the next zap continues from here.
    pub zap_anchor: Option<ChannelId>,
    /// Casting (DESIGN §40 §11.1): the picture plays on a TV; `None` = local. While casting the
    /// mpv-only fields (`tracks`, `stream_info` unless the info panel asked, `hwdec`,
    /// `video_size`, `cache_secs`, `video_suspended`, `sub_*`, `aspect`, `zoom`) stay at their
    /// defaults.
    pub cast: Option<crate::model::CastView>,
    /// The Play on switch (DESIGN §40 §3.9, [`PlayerCommand::SetPlayOn`]): the TV picture A's
    /// plays go to; `None` = this computer (and always for picture B).
    pub play_on: Option<crate::model::DeviceId>,
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
            audio_output: audio_output::AudioOutput::default(),
            lip_sync_ms: 0,
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
            tile: TileId::A,
            audible: true,
            claim: None,
            zap_anchor: None,
            cast: None,
            play_on: None,
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
    /// The local output was identified or changed; remember its label and show its offset.
    AudioOutputChanged {
        output: audio_output::AudioOutput,
        offset_ms: i32,
    },
    /// The first copy of a title failed before its first frame (`error`) and the player went
    /// on with the next one ([`CopyPlay::fallback`]): the app toasts "TV couldn't play Dune —
    /// playing from TV 2".
    FellBack {
        title: String,
        from: Option<crate::model::SourceId>,
        to: Option<crate::model::SourceId>,
        error: PlayerErrorKind,
    },
    /// Multi-view: a zap skipped `channel` because its playlist is held by another picture
    /// (`holder`) and it has no copy on a free playlist — the banner's "BBC One skipped — TV
    /// is in use by picture A". Once per zap.
    ZapSkipped {
        channel: ChannelId,
        source: SourceId,
        holder: Option<TileId>,
    },
    /// Casting: a catch-up seek past what the relay has loaded stopped at its edge (DESIGN §40
    /// §8.5); the app says "Catch-up can only seek as far as it has loaded".
    SeekClamped,
    /// Casting ended without the user asking in Tuner (DESIGN §40 §3.4, §3.8): `kind` is
    /// `StoppedOnTv` (the TV's remote stopped it, or another device took over). The output is
    /// local again and nothing plays; no error state. The app alerts where the user is (the
    /// casting card's place, a toast, or a desktop notification when the window isn't seen)
    /// with Play here · Cast again. Failures while casting still come as `Failed(Cast(kind))`.
    /// Sent by the controller (CAST-5), shown by the app (CAST-9).
    CastEnded {
        device: crate::model::DeviceId,
        /// Sanitized device name ("Living Room").
        name: String,
        kind: crate::model::CastErrorKind,
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
    /// The playlist a channel belongs to (multi-view zaps check the shared budget before
    /// resolving; `None` = unknown, just try it).
    fn channel_source(&mut self, _ch: ChannelId) -> Option<SourceId> {
        None
    }
    /// The channel's copies on other playlists (`store::copies::channel_copies`, B5), in the
    /// order to try. Default: none.
    fn channel_copies(&mut self, _ch: ChannelId) -> Vec<crate::store::copies::ChannelCopy> {
        Vec::new()
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
    /// Opens cast sessions (picture A; DESIGN §40 §5.5). `None` in tests and for picture B:
    /// `CastTo` then fails as "Casting isn't available".
    pub cast: Option<Arc<dyn remote::CastConnector>>,
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
    /// A handle not attached to a player: what it is sent arrives on the receiver, its
    /// snapshots come from the sender (routers' tests).
    #[cfg(test)]
    pub(crate) fn detached(snapshot: PlayerSnapshot) -> (Self, crossbeam_channel::Receiver<PlayerCommand>, watch::Sender<Arc<PlayerSnapshot>>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (snap_tx, snap_rx) = watch::channel(Arc::new(snapshot));
        (Self { tx, snapshot: snap_rx }, rx, snap_tx)
    }

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
    tile: TileId,
    budget: budget::SharedBudget,
    handle: PlayerHandle,
    render_alive: Arc<AtomicBool>,
    render_clock: RenderClock,
    epoch: Instant,
    pump: Option<events::EventPump>,
    controller: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    /// Create mpv with options from `cfg`, start the event pump and controller threads: the
    /// single-view player (picture A with a budget of its own).
    pub fn start(cfg: &Config, deps: PlayerDeps) -> Result<Self> {
        Self::start_tile(cfg, deps, TileId::A, budget::SharedBudget::default())
    }

    /// Start the player of picture `tile`, sharing `budget` with the other pictures. Picture A
    /// starts audible; any other starts muted (mpv `mute=yes` from creation, so it is never
    /// heard before the app gives it the sound) with smaller caches. mpv init takes 30–80 ms:
    /// call it off the UI thread for a picture started while the app runs.
    pub fn start_tile(cfg: &Config, deps: PlayerDeps, tile: TileId, budget: budget::SharedBudget) -> Result<Self> {
        let audible = tile == TileId::A;
        let mpv = Arc::new(mpv::create(cfg, tile)?);
        if tile == TileId::A {
            let version = |p: &str| mpv.get_property::<String>(p).unwrap_or_else(|_| "?".into());
            tracing::info!("Tuner {}, {}, FFmpeg {}", env!("CARGO_PKG_VERSION"), version("mpv-version"), version("ffmpeg-version"));
        } else {
            tracing::info!("player {tile}: started");
        }
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded();
        let (snap_tx, snap_rx) = watch::channel(Arc::new(PlayerSnapshot {
            volume: cfg.player.volume,
            muted: cfg.player.muted,
            sub_scale: cfg.player.sub_scale,
            tile,
            audible,
            ..Default::default()
        }));
        let pump_name = if tile == TileId::A { "mpv-events".to_owned() } else { format!("mpv-events-{}", tile.letter().to_ascii_lowercase()) };
        let pump = events::EventPump::start(mpv.clone(), evt_tx, pump_name).map_err(crate::error::Error::other)?;
        let render_clock: RenderClock = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let setup = controller::TileSetup { tile, budget: budget.clone(), audible };
        let controller = controller::spawn(mpv.clone(), cfg, deps, cmd_rx, evt_rx, snap_tx, render_clock.clone(), epoch, setup)
            .map_err(crate::error::Error::other)?;
        Ok(Self {
            mpv: Some(mpv),
            tile,
            budget,
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

    /// The picture this player is.
    pub fn tile(&self) -> TileId {
        self.tile
    }

    /// The connection budget it shares.
    pub fn budget(&self) -> &budget::SharedBudget {
        &self.budget
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
            tracing::error!("player {}: dropped while the video surface is alive; leaking mpv", self.tile);
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

    use super::tiles::Tiles;
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
        let deps = PlayerDeps { bus: EventBus::new(None).0, store: Box::new(NullPlayerStore), resolver: Box::new(resolver), cast: None };
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

    /// Real mpv properties, headless output: every local picture uses the saved offset on
    /// load, follows device changes, and never carries speakers' correction onto a new output.
    #[test]
    fn lip_sync_applies_on_play_and_follows_the_output_in_each_picture() {
        use crate::config::LipSync;
        let mut cfg = headless_config();
        cfg.player.mpv_options.insert("audio-device".into(), "null/speakers".into());
        cfg.player.lip_sync.insert("null/speakers".into(), LipSync { offset_ms: -150, label: "Speakers".into() });
        cfg.player.lip_sync.insert("null/headphones".into(), LipSync { offset_ms: 100, label: "Headphones".into() });
        let budget = budget::SharedBudget::default();
        let players: Vec<_> = [TileId::A, TileId::B]
            .into_iter()
            .map(|tile| {
                let deps = PlayerDeps {
                    bus: EventBus::new(None).0,
                    store: Box::new(NullPlayerStore),
                    resolver: Box::new(Lavfi(Vec::new(), Vec::new())),
                    cast: None,
                };
                Player::start_tile(&cfg, deps, tile, budget.clone()).unwrap()
            })
            .collect();
        for p in &players {
            let mpv = p.mpv.as_ref().unwrap();
            // mpv stores this option as a float; compare at the millisecond precision we save.
            let delay_ms = || (mpv.get_property::<f64>("audio-delay").unwrap() * 1000.0).round() as i32;
            let h = p.handle();
            play(p);
            assert!(wait_until(Duration::from_secs(5), || h.snapshot().state == PlayerState::Playing && h.snapshot().lip_sync_ms == -150));
            assert_eq!(delay_ms(), -150);
            for (name, ms) in [("null/headphones", 100), ("null/hdmi", 0), ("null/speakers", -150)] {
                mpv.set_property("audio-device", name).unwrap();
                assert!(
                    wait_until(Duration::from_secs(5), || {
                        let s = h.snapshot();
                        s.audio_output.name == name && s.lip_sync_ms == ms
                    }),
                    "output {name}"
                );
                assert_eq!(delay_ms(), ms);
            }
            // A fresh load restores the saved correction, even if mpv's value was changed.
            mpv.set_property("audio-delay", 1.0).unwrap();
            let generation = h.snapshot().load_generation;
            play(p);
            assert!(wait_until(Duration::from_secs(5), || h.snapshot().load_generation > generation && h.snapshot().state == PlayerState::Playing));
            assert_eq!(delay_ms(), -150);
        }
        // Settings applies live to each picture; repeated presses add up and clamp.
        let saved = cfg.player.lip_sync.get_mut("null/speakers").unwrap();
        saved.adjust(-1);
        saved.adjust(-1);
        assert_eq!(saved.offset_ms, -250);
        saved.adjust(i32::MIN);
        assert_eq!(saved.offset_ms, -2000);
        saved.adjust(i32::MAX);
        assert_eq!(saved.offset_ms, 2000);
        for p in &players {
            p.handle().send(PlayerCommand::ApplyConfig(Box::new(cfg.clone())));
            assert!(wait_until(Duration::from_secs(5), || p.handle().snapshot().lip_sync_ms == 2000));
            assert_eq!(p.mpv.as_ref().unwrap().get_property::<f64>("audio-delay").unwrap(), 2.0);
        }
    }

    /// Also per picture (multi-view): picture B, still rendered, keeps decoding while A — not
    /// placed or not rendered — is suspended.
    #[test]
    fn video_is_suspended_while_nothing_renders_and_resumed_on_request() {
        let p = start();
        let mpv = p.mpv.clone().expect("mpv");
        let h = p.handle();
        let deps = PlayerDeps {
            bus: EventBus::new(None).0,
            store: Box::new(NullPlayerStore),
            resolver: Box::new(Lavfi(Vec::new(), Vec::new())),
            cast: None,
        };
        let b = Player::start_tile(&headless_config(), deps, TileId::B, p.budget().clone()).expect("picture B");
        play(&p);
        play(&b);
        for (who, h) in [("A", &h), ("B", &b.handle())] {
            assert!(
                wait_until(Duration::from_secs(20), || {
                    let s = h.snapshot();
                    s.state == PlayerState::Playing && s.video_size.is_some()
                }),
                "lavfi playback {who}: {:?}",
                h.snapshot().state
            );
        }
        // no surface yet (clock 0): never suspended
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!h.snapshot().video_suspended);

        // one frame rendered "now", then nothing (hidden window); B keeps rendering
        let core = p.render_core().expect("core");
        core.clock.store(core.epoch.elapsed().as_millis().max(1) as u64, Ordering::Relaxed);
        let b_core = b.render_core().expect("core B");
        let rendering = Arc::new(AtomicBool::new(true));
        let stamper = {
            let rendering = rendering.clone();
            std::thread::spawn(move || {
                while rendering.load(Ordering::Relaxed) {
                    b_core.clock.store(b_core.epoch.elapsed().as_millis().max(1) as u64, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        // a quick workspace switch (< 10 s) never suspends: decoding would restart mid-GOP
        std::thread::sleep(Duration::from_secs(8));
        assert!(!h.snapshot().video_suspended, "not after 8 s");
        assert!(wait_until(Duration::from_secs(6), || h.snapshot().video_suspended), "suspended after 10 s");
        assert_eq!(mpv.get_property::<String>("vid").unwrap(), "no");
        assert_eq!(h.snapshot().state, PlayerState::Playing, "audio keeps playing");
        assert!(!b.handle().snapshot().video_suspended, "the picture still rendered keeps its video");
        assert_ne!(b.mpv.as_ref().expect("mpv B").get_property::<String>("vid").unwrap(), "no");
        rendering.store(false, Ordering::Relaxed);
        stamper.join().unwrap();

        h.send(PlayerCommand::ResumeVideo);
        assert!(wait_until(Duration::from_secs(5), || !h.snapshot().video_suspended));
        assert_ne!(mpv.get_property::<String>("vid").unwrap(), "no");
    }

    /// Two playlists of three channels each (1–3 on playlist 1, 4–6 on playlist 2), all
    /// playing lavfi's test source; channel 4's copy is channel 2, channel 1's is channel 5.
    /// Channel 2 is PIN-locked. Like the user's "TV" and "TV 2" with one connection each.
    struct TwoPlaylists;

    impl TwoPlaylists {
        fn source(ch: ChannelId) -> SourceId {
            SourceId(if ch.0 <= 3 { 1 } else { 2 })
        }
    }

    impl MediaResolver for TwoPlaylists {
        fn resolve(&mut self, media: &MediaRef, _resume: bool) -> std::result::Result<Playable, PlayerError> {
            use crate::model::{ItemKey, ItemKind};
            let MediaRef::Channel(ch) = media else { return Err(PlayerError { kind: PlayerErrorKind::Resolve, message: "channels only".into() }) };
            let source = Self::source(*ch);
            Ok(Playable {
                media: media.clone(),
                kind: PlayableKind::Live,
                item: Some(ItemRef { source, kind: ItemKind::Channel, key: ItemKey(format!("ch{}", ch.0)) }),
                source_id: Some(source),
                title: format!("Channel {}", ch.0),
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
            ch.0 == 2
        }
        fn channel_source(&mut self, ch: ChannelId) -> Option<SourceId> {
            Some(Self::source(ch))
        }
        fn channel_copies(&mut self, ch: ChannelId) -> Vec<crate::store::copies::ChannelCopy> {
            let copy = |id: i64| crate::store::copies::ChannelCopy {
                id: ChannelId(id),
                source: Self::source(ChannelId(id)),
                quality: None,
                locked: false,
                explicit: false,
            };
            match ch.0 {
                4 => vec![copy(2)],
                1 => vec![copy(5)],
                _ => Vec::new(),
            }
        }
    }

    /// Pictures A and B on one budget with the two one-connection playlists (B's events on
    /// `events`).
    fn two_pictures() -> (Tiles, crossbeam_channel::Receiver<crate::msg::AppEvent>) {
        let (bus, events) = EventBus::new(None);
        let deps = |bus: EventBus| PlayerDeps { bus, store: Box::new(NullPlayerStore), resolver: Box::new(TwoPlaylists), cast: None };
        let a = Player::start(&headless_config(), deps(EventBus::new(None).0)).expect("picture A");
        a.budget().set_limit(SourceId(1), Some(1));
        a.budget().set_limit(SourceId(2), Some(1));
        let mut tiles = Tiles::single(a);
        let slot = tiles.begin_b().expect("B starts");
        assert!(tiles.begin_b().is_none(), "one start at a time");
        assert_eq!(tiles.adopt_b(), None, "not started yet");
        slot.fill(Player::start_tile(&headless_config(), deps(bus), TileId::B, tiles.budget().clone()).map_err(|e| e.to_string()));
        assert_eq!(tiles.adopt_b(), Some(Ok(())));
        (tiles, events)
    }

    fn on(h: &PlayerHandle, ch: i64) -> bool {
        wait_until(Duration::from_secs(20), || {
            let s = h.snapshot();
            s.state == PlayerState::Playing && s.playable.as_ref().is_some_and(|p| p.media == MediaRef::Channel(ChannelId(ch)))
        })
    }

    fn channel(ch: i64, zap: Option<&[i64]>) -> PlayRequest {
        PlayRequest {
            media: MediaRef::Channel(ChannelId(ch)),
            zap: zap.map(|z| ZapList { collection: crate::model::Collection::AllChannels, channels: z.iter().map(|c| ChannelId(*c)).collect() }),
            origin: PlayOrigin::MultiView,
            resume: false,
            immediate: true,
        }
    }

    fn tile_events(events: &crossbeam_channel::Receiver<crate::msg::AppEvent>) -> Vec<PlayerEvent> {
        events
            .try_iter()
            .filter_map(|e| match e {
                crate::msg::AppEvent::Tile(tiles::TileEvent::Player(TileId::B, ev)) => Some(ev),
                crate::msg::AppEvent::Player(ev) => panic!("picture B's event reached single view: {ev:?}"),
                _ => None,
            })
            .collect()
    }

    /// The user's setup (DESIGN §35 §4.4): picture A plays from TV; picture B asked for a TV
    /// channel can't open a second TV stream and plays its TV 2 copy instead, saying so. A is
    /// untouched, and B's events never reach single-view code. A channel only TV has is refused
    /// — until the user agrees to move A to its TV 2 copy, after which both play.
    #[test]
    fn a_second_picture_on_a_busy_playlist_plays_the_copy_elsewhere() {
        let (tiles, events) = two_pictures();
        let (a, b) = (tiles.handle(TileId::A), tiles.handle(TileId::B));
        a.send(PlayerCommand::Play(channel(1, None)));
        assert!(on(&a, 1));
        let generation = a.snapshot().load_generation;
        assert_eq!(a.snapshot().claim, Some(SourceId(1)));

        b.send(PlayerCommand::PlayCopy(CopyPlay {
            req: channel(3, None),
            fallback: Some(MediaRef::Channel(ChannelId(5))),
            elsewhere: None,
            start_at: None,
        }));
        assert!(on(&b, 5), "B on the copy: {:?}", b.snapshot().state);
        assert_eq!(b.snapshot().claim, Some(SourceId(2)));
        let fell = tile_events(&events).into_iter().find_map(|e| match e {
            PlayerEvent::FellBack { from, to, error, .. } => Some((from, to, error)),
            _ => None,
        });
        assert_eq!(fell, Some((Some(SourceId(1)), Some(SourceId(2)), PlayerErrorKind::Budget)));
        let s = a.snapshot();
        assert_eq!((s.state.clone(), s.load_generation, s.claim), (PlayerState::Playing, generation, Some(SourceId(1))), "A untouched");

        // without a copy the refusal is B's error, never A's stream
        b.send(PlayerCommand::Play(channel(2, None)));
        assert!(wait_until(Duration::from_secs(10), || matches!(&b.snapshot().state, PlayerState::Error(e) if e.kind == PlayerErrorKind::Budget)));
        assert_eq!(b.snapshot().claim, None, "a final error lets the playlist go");
        assert_eq!(a.snapshot().load_generation, generation);
        assert_eq!(tiles.budget().usage().iter().map(|u| u.in_use).sum::<u32>(), 1);

        // "Move picture A to its TV 2 copy so B can use TV?" — Enter
        let zap_to = |id: i64| PlayerCommand::ZapTo { id: ChannelId(id), unlocked: true };
        tiles.rearrange(TileId::A, SourceId(2), zap_to(5), TileId::B, Some(SourceId(1)), PlayerCommand::Play(channel(2, None)));
        assert!(on(&a, 5) && on(&b, 2), "both play: A {:?}, B {:?}", a.snapshot().state, b.snapshot().state);
        assert_eq!((a.snapshot().claim, b.snapshot().claim), (Some(SourceId(2)), Some(SourceId(1))));
        let usage = tiles.budget().usage();
        assert!(usage.iter().all(|u| u.in_use == 1 && u.limit == Some(1)), "never two streams on one playlist: {usage:?}");

        // the two swap playlists (B wants a TV 2-only channel; A goes back to TV): whichever
        // player takes its command first waits for the other's old stream, never refused
        tiles.rearrange(TileId::A, SourceId(1), zap_to(1), TileId::B, Some(SourceId(2)), PlayerCommand::Play(channel(6, None)));
        assert!(on(&a, 1) && on(&b, 6), "both play after the swap: A {:?}, B {:?}", a.snapshot().state, b.snapshot().state);
        assert_eq!((a.snapshot().claim, b.snapshot().claim), (Some(SourceId(1)), Some(SourceId(2))));
        let usage = tiles.budget().usage();
        assert!(usage.iter().all(|u| u.in_use == 1), "never two streams on one playlist: {usage:?}");
    }

    /// Zapping picture B onto a channel whose playlist A holds plays its copy on the free
    /// playlist and keeps the zap-list position; a channel without a copy is skipped, once
    /// announced ("BBC One skipped — TV is in use by picture A").
    #[test]
    fn zapping_a_picture_past_the_other_pictures_playlist() {
        let (tiles, events) = two_pictures();
        let (a, b) = (tiles.handle(TileId::A), tiles.handle(TileId::B));
        a.send(PlayerCommand::Play(channel(5, None)));
        assert!(on(&a, 5));
        b.send(PlayerCommand::Play(channel(1, Some(&[1, 4, 6]))));
        assert!(on(&b, 1));

        b.send(PlayerCommand::Zap { forward: true, unlocked: true });
        assert!(on(&b, 2), "channel 4's copy: {:?}", b.snapshot().playable);
        assert_eq!(b.snapshot().zap_anchor, Some(ChannelId(4)));

        b.send(PlayerCommand::Zap { forward: true, unlocked: true });
        assert!(on(&b, 1), "6 skipped (no copy), wrapped to 1: {:?}", b.snapshot().playable);
        assert_eq!(b.snapshot().zap_anchor, None);
        let skipped: Vec<_> = tile_events(&events).into_iter().filter(|e| matches!(e, PlayerEvent::ZapSkipped { .. })).collect();
        assert_eq!(skipped, [PlayerEvent::ZapSkipped { channel: ChannelId(6), source: SourceId(2), holder: Some(TileId::A) }]);
        assert_eq!(a.snapshot().playable.as_ref().map(|p| p.media.clone()), Some(MediaRef::Channel(ChannelId(5))), "A untouched");

        // numeric entry while the session is locked: channel 4's copy (2) is PIN-locked, so it
        // never stands in — the budget's refusal stands — until the sender is unlocked
        let mut cfg = headless_config();
        cfg.parental = crate::config::ParentalConfig {
            show_explicit: true,
            enabled: true,
            pin_hash: Some("h".into()),
            pin_salt: Some("s".into()),
            relock_min: 0,
        };
        b.send(PlayerCommand::ApplyConfig(Box::new(cfg)));
        b.send(PlayerCommand::ZapTo { id: ChannelId(4), unlocked: false });
        assert!(
            wait_until(Duration::from_secs(10), || matches!(&b.snapshot().state, PlayerState::Error(e) if e.kind == PlayerErrorKind::Budget)),
            "{:?}",
            b.snapshot().state
        );
        assert_ne!(b.snapshot().playable.as_ref().map(|p| p.media.clone()), Some(MediaRef::Channel(ChannelId(2))), "the locked copy never played");
        b.send(PlayerCommand::ZapTo { id: ChannelId(4), unlocked: true });
        assert!(on(&b, 2), "an unlocked sender gets the copy: {:?}", b.snapshot().playable);
    }

    /// One listening volume (DESIGN §35 §4.3): B is silent from its creation; moving the sound
    /// mutes the old picture and gives the new one the listening volume and the user's mute,
    /// which survives two moves.
    #[test]
    fn the_sound_moves_between_pictures_with_the_listening_volume() {
        let (mut tiles, _events) = two_pictures();
        let mpv = |t: TileId| tiles.player(t).and_then(|p| p.mpv.clone()).expect("mpv");
        let (mpv_a, mpv_b) = (mpv(TileId::A), mpv(TileId::B));
        let muted = |m: &Mpv| m.get_property::<bool>("mute").unwrap();
        let (a, b) = (tiles.handle(TileId::A), tiles.handle(TileId::B));
        assert!(muted(&mpv_b) && !b.snapshot().audible && !b.snapshot().muted, "B silent, the user's mute untouched");
        assert!(!muted(&mpv_a) && a.snapshot().audible);

        a.send(PlayerCommand::SetVolume(70.0));
        assert!(wait_until(Duration::from_secs(5), || a.snapshot().volume == 70.0));
        tiles.move_sound(TileId::B);
        assert_eq!(tiles.audible(), TileId::B);
        assert!(wait_until(Duration::from_secs(5), || b.snapshot().audible && b.snapshot().volume == 70.0));
        assert!(wait_until(Duration::from_secs(5), || muted(&mpv_a) && !muted(&mpv_b)));
        assert!(!a.snapshot().audible && !a.snapshot().muted);

        // the user mutes, then the sound moves twice: still muted, wherever it is
        tiles.audible_handle().send(PlayerCommand::ToggleMute);
        assert!(wait_until(Duration::from_secs(5), || b.snapshot().muted && muted(&mpv_b)));
        tiles.move_sound(TileId::A);
        assert!(wait_until(Duration::from_secs(5), || a.snapshot().audible && a.snapshot().muted && muted(&mpv_a) && muted(&mpv_b)));
        tiles.move_sound(TileId::B);
        assert!(wait_until(Duration::from_secs(5), || b.snapshot().audible && b.snapshot().muted && muted(&mpv_b)));
        tiles.audible_handle().send(PlayerCommand::ToggleMute);
        assert!(wait_until(Duration::from_secs(5), || !b.snapshot().muted && !muted(&mpv_b) && muted(&mpv_a)));
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
        let deps = PlayerDeps { bus, store: Box::new(NullPlayerStore), resolver: Box::new(Copies), cast: None };
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

    /// The MV-1 measurement spike (DESIGN §35 §4.6, `docs/design/multiview-spike.md`): CPU and
    /// memory of one and two 4K HEVC pictures decoded in software, with and without
    /// `light_decode_small`, and with both pictures' video suspended (hidden workspace).
    /// `TUNER_SPIKE_MEDIA=<a 4K HEVC file> cargo test --release --lib spike_ -- --ignored
    /// --nocapture`. Whole-process CPU from `/proc/self/stat`, so run it on a quiet machine.
    #[test]
    #[ignore = "measurement spike: needs TUNER_SPIKE_MEDIA (a 4K HEVC file)"]
    fn spike_two_uhd_hevc_decodes() {
        let Some(media) = std::env::var_os("TUNER_SPIKE_MEDIA") else {
            eprintln!("skipped: set TUNER_SPIKE_MEDIA");
            return;
        };
        let secs: u64 = std::env::var("TUNER_SPIKE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
        struct File(String);
        impl MediaResolver for File {
            fn resolve(&mut self, media: &MediaRef, _resume: bool) -> std::result::Result<Playable, PlayerError> {
                Ok(Playable {
                    media: media.clone(),
                    kind: PlayableKind::Movie,
                    item: None,
                    source_id: None,
                    title: "spike".into(),
                    subtitle: None,
                    logo: None,
                    number: None,
                    url: Secret::new(self.0.clone()),
                    headers: Default::default(),
                    start_at: None,
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
        let mut cfg = headless_config();
        cfg.player.hwdec = "no".into();
        cfg.player.mpv_options.insert("loop-file".into(), "inf".into());
        let path = media.to_string_lossy().into_owned();
        let deps = || PlayerDeps { bus: EventBus::new(None).0, store: Box::new(NullPlayerStore), resolver: Box::new(File(path.clone())), cast: None };
        let cpu_s = || {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let f: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split_whitespace().collect();
            (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
        };
        let status = |field: &str| {
            let s = std::fs::read_to_string("/proc/self/status").unwrap();
            s.lines().find_map(|l| l.strip_prefix(field)).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                >> 10
        };
        let mem = |what: &str| eprintln!("MEM {what}: VmRSS {} MiB, RssAnon {} MiB", status("VmRSS:"), status("RssAnon:"));
        let drops = |p: &Player| {
            let m = p.mpv.as_ref().unwrap();
            (m.get_property::<i64>("decoder-frame-drop-count").unwrap_or(0), m.get_property::<i64>("frame-drop-count").unwrap_or(0))
        };
        let measure = |what: &str, players: &[&Player]| {
            std::thread::sleep(Duration::from_secs(3)); // settle
            let d0: Vec<_> = players.iter().map(|p| drops(p)).collect();
            let (c0, t0) = (cpu_s(), Instant::now());
            std::thread::sleep(Duration::from_secs(secs));
            let cores = (cpu_s() - c0) / t0.elapsed().as_secs_f64();
            let d: Vec<_> = players.iter().zip(d0).map(|(p, (a, b))| (drops(p).0 - a, drops(p).1 - b)).collect();
            let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
            eprintln!("CPU {what}: {cores:.2} cores; drops (decoder, vo) per picture {d:?}; loadavg {}", load.trim());
            cores
        };
        // (re)load the file; returns the load generation to wait past
        let play_file = |p: &Player| {
            let generation = p.handle().snapshot().load_generation;
            p.handle().send(PlayerCommand::Play(PlayRequest {
                media: MediaRef::Url("spike".into()),
                zap: None,
                origin: PlayOrigin::Cli,
                resume: false,
                immediate: true,
            }));
            generation
        };
        let playing = |p: &Player, after: u64| {
            wait_until(Duration::from_secs(30), || {
                let s = p.handle().snapshot();
                s.state == PlayerState::Playing && s.load_generation > after
            })
        };

        mem("A idle");
        let a = Player::start(&cfg, deps()).expect("A");
        let g = play_file(&a);
        assert!(playing(&a, g));
        let one = measure("one picture", &[&a]);
        mem("A playing");
        let b = Player::start_tile(&cfg, deps(), TileId::B, a.budget().clone()).expect("B");
        std::thread::sleep(Duration::from_secs(2));
        mem("A playing, B started idle");
        let g = play_file(&b);
        assert!(playing(&b, g));
        let two = measure("two pictures", &[&a, &b]);
        mem("A and B playing");
        b.handle().send(PlayerCommand::SetLightDecode(true));
        let g = play_file(&b);
        assert!(playing(&b, g));
        let skip = |p: &Player| p.mpv.as_ref().unwrap().get_property::<String>("vd-lavc-skiploopfilter").unwrap_or_default();
        assert_eq!((skip(&a), skip(&b)), ("default".into(), "all".into()), "the per-file option reached B only");
        let two_light = measure("two pictures, B light", &[&a, &b]);
        b.handle().send(PlayerCommand::Stop);
        std::thread::sleep(Duration::from_secs(3));
        mem("B stopped");
        crate::platform::memory::trim();
        mem("B stopped, trimmed");
        a.handle().send(PlayerCommand::SetLightDecode(true));
        let g = play_file(&a);
        assert!(playing(&a, g));
        let one_light = measure("one picture, light", &[&a]);
        a.handle().send(PlayerCommand::SetLightDecode(false));
        let (ga, gb) = (play_file(&a), play_file(&b));
        assert!(playing(&a, ga) && playing(&b, gb));
        // hidden workspace: one frame "rendered", then none; both suspend after 10 s
        for p in [&a, &b] {
            let core = p.render_core().unwrap();
            core.clock.store(core.epoch.elapsed().as_millis().max(1) as u64, Ordering::Relaxed);
        }
        assert!(wait_until(Duration::from_secs(20), || a.handle().snapshot().video_suspended && b.handle().snapshot().video_suspended));
        let hidden = measure("two pictures, video suspended (hidden)", &[&a, &b]);
        crate::platform::memory::trim();
        mem("both suspended, trimmed");
        eprintln!(
            "SUMMARY one {one:.2} · two {two:.2} · two with B light {two_light:.2} · one light {one_light:.2} · light saves {:.0} % of a decode · hidden {hidden:.2}",
            (one - one_light) / one * 100.0
        );
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
