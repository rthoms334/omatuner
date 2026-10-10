//! MPRIS (`org.mpris.MediaPlayer2.tuner`) via `mpris-server` 0.10 on the tokio runtime.
//! Owner: WP-08. DESIGN §21.1.
//!
//! * Identity "Tuner", DesktopEntry "tuner", CanRaise/CanQuit true, no tracklist.
//! * Player: PlaybackStatus from `PlayerState` (Playing/Buffering/Loading/Zapping/
//!   Reconnecting → Playing, Paused → Paused, else Stopped); Metadata: `mpris:trackid`
//!   (`/tuner/item/<hash>`), `xesam:title` (programme title if live, else item title),
//!   `xesam:artist` [channel name], `mpris:artUrl` (logo/poster URL, http(s) only),
//!   `mpris:length` (VOD µs). CanGoNext/Previous = live with a ZapList; CanSeek = seekable.
//! * Commands → `PlayerHandle` directly (works while the window is hidden): PlayPause, Play,
//!   Pause, Stop, Next/Previous → Zap, Seek/SetPosition → SeekBy/SeekTo, Volume → SetVolume.
//!   Raise/Quit → `PlatformEvent` (Raise also focuses the window right away: the UI may not
//!   be running while it sits on another workspace).
//! * Subscribes to the player snapshot `watch` channel and emits PropertiesChanged only for
//!   changed fields (position is polled by clients; emit `Seeked` on seeks).
//!
//! Omarchy media keys (`omarchy-shell media playPause|next|previous`) and the bar's media
//! widget then work out of the box.
//! API choice: `mpris_server::Player` wraps a `LocalServer` that is not `Send`, so it cannot be
//! spawned on the runtime. Implement `RootInterface + PlayerInterface` on a `Send` struct
//! (holding the `PlayerHandle` + latest snapshot) and serve it with `mpris_server::Server<T>`.
//! If another Tuner (other profile) already owns the name, we register as
//! `org.mpris.MediaPlayer2.tuner.instance<pid>` like other players do.

use std::sync::Arc;
use std::time::Instant;

use mpris_server::zbus::{self, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface, Server, Signal, Time, TrackId,
    Volume,
};
use tokio::sync::watch;

use crate::error::{Error, Result};
use crate::model::PlayableKind;
use crate::msg::{AppEvent, EventBus};
use crate::platform::PlatformEvent;
use crate::player::{PlayerCommand, PlayerHandle, PlayerSnapshot, PlayerState};

/// mpv's `volume-max` default: MPRIS volume 1.0 = 100 %, up to 1.3.
const MAX_VOLUME: f64 = 1.3;
/// A position jump larger than this (vs. the expected playback progress) is a seek.
const SEEK_JUMP_S: f64 = 2.0;

pub struct MprisService {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl MprisService {
    /// Register the bus name and start serving on `rt`. Failure (no session bus) → Err; the
    /// caller logs and continues without MPRIS.
    ///
    /// Returns without waiting for D-Bus (the caller is the UI thread at startup): the name is
    /// registered by the spawned task, which logs a warning if that fails later.
    pub fn start(rt: &tokio::runtime::Handle, player: PlayerHandle, bus: EventBus) -> Result<Self> {
        let snapshots = player.subscribe();
        Self::start_with(rt, Arc::new(player), snapshots, bus, None)
    }

    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// `suffix = None` → `tuner` (falling back to `tuner.instance<pid>`); tests pass a unique one.
    fn start_with(
        rt: &tokio::runtime::Handle,
        cmds: Arc<dyn Commands>,
        snapshots: watch::Receiver<Arc<PlayerSnapshot>>,
        bus: EventBus,
        suffix: Option<String>,
    ) -> Result<Self> {
        if !session_bus_configured() {
            return Err(Error::other("no D-Bus session bus"));
        }
        let imp = Imp { cmds, snapshots: snapshots.clone(), bus };
        let task = rt.spawn(async move {
            let server = match register(imp, suffix).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("MPRIS: cannot register on the session bus: {e}");
                    return;
                }
            };
            tracing::info!("MPRIS: serving {}", server.bus_name());
            publish_changes(&server, snapshots).await;
        });
        Ok(Self { task: Some(task) })
    }
}

impl Drop for MprisService {
    fn drop(&mut self) { self.stop(); }
}

/// `DBUS_SESSION_BUS_ADDRESS`, or the systemd default socket `$XDG_RUNTIME_DIR/bus`.
fn session_bus_configured() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("bus").exists())
}

async fn register(imp: Imp, suffix: Option<String>) -> zbus::Result<Server<Imp>> {
    match suffix {
        Some(s) => Server::new(&s, imp).await,
        None => match Server::new(crate::APP_ID, imp.clone()).await {
            Err(zbus::Error::NameTaken) => Server::new(&format!("{}.instance{}", crate::APP_ID, std::process::id()), imp).await,
            r => r,
        },
    }
}

/// Where player commands go (the `PlayerHandle` in production, a channel in tests).
trait Commands: Send + Sync + 'static {
    fn send(&self, cmd: PlayerCommand);
}

impl Commands for PlayerHandle {
    fn send(&self, cmd: PlayerCommand) { PlayerHandle::send(self, cmd) }
}

#[derive(Clone)]
struct Imp {
    cmds: Arc<dyn Commands>,
    snapshots: watch::Receiver<Arc<PlayerSnapshot>>,
    bus: EventBus,
}

impl Imp {
    fn snap(&self) -> Arc<PlayerSnapshot> { self.snapshots.borrow().clone() }
}

// ------------------------------------------------------------------ snapshot → MPRIS values

fn is_live(s: &PlayerSnapshot) -> bool { s.playable.as_ref().is_some_and(|p| p.kind.is_live()) }

fn playback_status(s: &PlayerSnapshot) -> PlaybackStatus {
    match &s.state {
        PlayerState::Paused => PlaybackStatus::Paused,
        st if st.is_active() && s.paused => PlaybackStatus::Paused,
        st if st.is_active() => PlaybackStatus::Playing,
        _ => PlaybackStatus::Stopped,
    }
}

fn can_zap(s: &PlayerSnapshot) -> bool { is_live(s) && s.zap.as_ref().is_some_and(|z| z.channels.len() > 1) }

fn can_pause(s: &PlayerSnapshot) -> bool { s.playable.is_some() }

fn volume(s: &PlayerSnapshot) -> Volume { if s.muted { 0.0 } else { (s.volume / 100.0).clamp(0.0, MAX_VOLUME) } }

/// Stable per item: `/tuner/item/<16 hex>` of the item's identity (or its media ref).
fn track_id(s: &PlayerSnapshot) -> TrackId {
    let Some(p) = &s.playable else { return TrackId::NO_TRACK };
    let identity = match &p.item {
        Some(item) => format!("{}:{}:{}", item.source, item.kind.as_str(), item.key),
        None => format!("{:?}", p.media),
    };
    let hash = blake3::hash(identity.as_bytes()).to_hex();
    TrackId::try_from(format!("/tuner/item/{}", &hash[..16])).unwrap_or(TrackId::NO_TRACK)
}

/// Logos are public URLs; anything that would need redaction (credentials) is left out.
fn art_url(logo: Option<&str>) -> Option<String> {
    let logo = logo?.trim();
    let scheme_ok = ["http://", "https://"].iter().any(|p| logo.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)));
    (scheme_ok && crate::redact::redact(logo) == logo).then(|| logo.to_owned())
}

fn metadata(s: &PlayerSnapshot) -> Metadata {
    let mut m = Metadata::new();
    m.set_trackid(Some(track_id(s)));
    let Some(p) = &s.playable else { return m };
    let live = matches!(p.kind, PlayableKind::Live);
    let title = if live { p.subtitle.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&p.title) } else { &p.title };
    m.set_title(Some(title.to_owned()));
    if live || matches!(p.kind, PlayableKind::Catchup { .. }) {
        m.set_artist(Some([p.title.clone()]));
    }
    m.set_art_url(art_url(p.logo.as_deref()));
    if !live && let Some(d) = s.duration.or(p.duration_hint).filter(|d| d.is_finite() && *d > 0.0) {
        m.set_length(Some(Time::from_micros((d * 1e6) as i64)));
    }
    m
}

fn position_us(s: &PlayerSnapshot) -> i64 { s.position.filter(|p| p.is_finite() && *p >= 0.0).map_or(0, |p| (p * 1e6) as i64) }

impl RootInterface for Imp {
    async fn raise(&self) -> fdo::Result<()> {
        self.bus.send(AppEvent::Platform(PlatformEvent::Raise));
        crate::platform::hypr::focus_self();
        Ok(())
    }
    async fn quit(&self) -> fdo::Result<()> {
        self.bus.send(AppEvent::Platform(PlatformEvent::Quit));
        Ok(())
    }
    async fn can_quit(&self) -> fdo::Result<bool> { Ok(true) }
    async fn fullscreen(&self) -> fdo::Result<bool> { Ok(false) }
    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> { Ok(()) }
    async fn can_set_fullscreen(&self) -> fdo::Result<bool> { Ok(false) }
    async fn can_raise(&self) -> fdo::Result<bool> { Ok(true) }
    async fn has_track_list(&self) -> fdo::Result<bool> { Ok(false) }
    async fn identity(&self) -> fdo::Result<String> { Ok(crate::APP_NAME.to_owned()) }
    async fn desktop_entry(&self) -> fdo::Result<String> { Ok(crate::APP_ID.to_owned()) }
    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> { Ok(Vec::new()) }
    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> { Ok(Vec::new()) }
}

impl PlayerInterface for Imp {
    async fn next(&self) -> fdo::Result<()> {
        if can_zap(&self.snap()) {
            self.cmds.send(PlayerCommand::Zap { forward: true });
        }
        Ok(())
    }
    async fn previous(&self) -> fdo::Result<()> {
        if can_zap(&self.snap()) {
            self.cmds.send(PlayerCommand::Zap { forward: false });
        }
        Ok(())
    }
    async fn pause(&self) -> fdo::Result<()> {
        self.cmds.send(PlayerCommand::SetPause(true));
        Ok(())
    }
    async fn play_pause(&self) -> fdo::Result<()> {
        let cmd = match self.snap().state {
            PlayerState::Error(_) => PlayerCommand::Reconnect,
            _ => PlayerCommand::TogglePause,
        };
        self.cmds.send(cmd);
        Ok(())
    }
    async fn stop(&self) -> fdo::Result<()> {
        self.cmds.send(PlayerCommand::Stop);
        Ok(())
    }
    async fn play(&self) -> fdo::Result<()> {
        let cmd = match self.snap().state {
            PlayerState::Error(_) => PlayerCommand::Reconnect,
            _ => PlayerCommand::SetPause(false),
        };
        self.cmds.send(cmd);
        Ok(())
    }
    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        if self.snap().seekable {
            self.cmds.send(PlayerCommand::SeekBy(offset.as_micros() as f64 / 1e6));
        }
        Ok(())
    }
    async fn set_position(&self, track: TrackId, position: Time) -> fdo::Result<()> {
        let s = self.snap();
        // Spec: ignore stale track ids and positions outside the track.
        let in_range = position.as_micros() >= 0 && s.duration.is_none_or(|d| position.as_micros() as f64 / 1e6 <= d);
        if s.seekable && track == track_id(&s) && in_range {
            self.cmds.send(PlayerCommand::SeekTo(position.as_micros() as f64 / 1e6));
        }
        Ok(())
    }
    async fn open_uri(&self, _uri: String) -> fdo::Result<()> { Err(fdo::Error::NotSupported("OpenUri is not supported".into())) }
    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> { Ok(playback_status(&self.snap())) }
    async fn loop_status(&self) -> fdo::Result<LoopStatus> { Ok(LoopStatus::None) }
    async fn set_loop_status(&self, _loop_status: LoopStatus) -> zbus::Result<()> { Ok(()) }
    async fn rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> { Ok(()) }
    async fn shuffle(&self) -> fdo::Result<bool> { Ok(false) }
    async fn set_shuffle(&self, _shuffle: bool) -> zbus::Result<()> { Ok(()) }
    async fn metadata(&self) -> fdo::Result<Metadata> { Ok(metadata(&self.snap())) }
    async fn volume(&self) -> fdo::Result<Volume> { Ok(volume(&self.snap())) }
    async fn set_volume(&self, volume: Volume) -> zbus::Result<()> {
        if volume.is_finite() {
            self.cmds.send(PlayerCommand::SetVolume(volume.clamp(0.0, MAX_VOLUME) * 100.0));
        }
        Ok(())
    }
    async fn position(&self) -> fdo::Result<Time> { Ok(Time::from_micros(position_us(&self.snap()))) }
    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn can_go_next(&self) -> fdo::Result<bool> { Ok(can_zap(&self.snap())) }
    async fn can_go_previous(&self) -> fdo::Result<bool> { Ok(can_zap(&self.snap())) }
    async fn can_play(&self) -> fdo::Result<bool> { Ok(can_pause(&self.snap())) }
    async fn can_pause(&self) -> fdo::Result<bool> { Ok(can_pause(&self.snap())) }
    async fn can_seek(&self) -> fdo::Result<bool> { Ok(self.snap().seekable) }
    async fn can_control(&self) -> fdo::Result<bool> { Ok(true) }
}

// ------------------------------------------------------------------ change notification

/// The MPRIS-visible part of a snapshot, compared between updates.
#[derive(Debug, Clone, PartialEq)]
struct View {
    status: PlaybackStatus,
    metadata: Metadata,
    can_zap: bool,
    can_pause: bool,
    can_seek: bool,
    volume: Volume,
    generation: u64,
    /// Position is expected to advance (Playing, not paused or buffering).
    advancing: bool,
    position_us: i64,
    at: Instant,
}

impl View {
    fn of(s: &PlayerSnapshot, at: Instant) -> Self {
        Self {
            status: playback_status(s),
            metadata: metadata(s),
            can_zap: can_zap(s),
            can_pause: can_pause(s),
            can_seek: s.seekable,
            volume: volume(s),
            generation: s.load_generation,
            advancing: s.state == PlayerState::Playing && !s.paused,
            position_us: position_us(s),
            at,
        }
    }

    /// Properties whose value differs in `next`.
    fn changes(&self, next: &View) -> Vec<Property> {
        let mut p = Vec::new();
        if self.status != next.status {
            p.push(Property::PlaybackStatus(next.status));
        }
        if self.metadata != next.metadata {
            p.push(Property::Metadata(next.metadata.clone()));
        }
        if self.can_zap != next.can_zap {
            p.push(Property::CanGoNext(next.can_zap));
            p.push(Property::CanGoPrevious(next.can_zap));
        }
        if self.can_pause != next.can_pause {
            p.push(Property::CanPlay(next.can_pause));
            p.push(Property::CanPause(next.can_pause));
        }
        if self.can_seek != next.can_seek {
            p.push(Property::CanSeek(next.can_seek));
        }
        if self.volume != next.volume {
            p.push(Property::Volume(next.volume));
        }
        p
    }

    /// `Some(position)` when `next` jumped instead of progressing (same item, seekable).
    fn seeked(&self, next: &View) -> Option<Time> {
        if !next.can_seek || self.generation != next.generation || self.metadata.trackid() != next.metadata.trackid() {
            return None;
        }
        let elapsed = if self.advancing { next.at.saturating_duration_since(self.at).as_secs_f64() } else { 0.0 };
        let expected = self.position_us as f64 / 1e6 + elapsed;
        ((next.position_us as f64 / 1e6 - expected).abs() > SEEK_JUMP_S).then(|| Time::from_micros(next.position_us))
    }
}

async fn publish_changes(server: &Server<Imp>, mut rx: watch::Receiver<Arc<PlayerSnapshot>>) {
    let mut last = View::of(&rx.borrow_and_update(), Instant::now());
    while rx.changed().await.is_ok() {
        let snap = rx.borrow_and_update().clone();
        let next = View::of(&snap, Instant::now());
        let changed = last.changes(&next);
        if !changed.is_empty()
            && let Err(e) = server.properties_changed(changed).await
        {
            tracing::debug!("MPRIS PropertiesChanged: {e}");
        }
        if let Some(position) = last.seeked(&next)
            && let Err(e) = server.emit(Signal::Seeked { position }).await
        {
            tracing::debug!("MPRIS Seeked: {e}");
        }
        last = next;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::model::{ChannelId, Collection, ItemKey, ItemKind, ItemRef, MediaRef, Playable, SourceId, ZapList};
    use crate::redact::Secret;

    struct Recorder(Mutex<Vec<PlayerCommand>>);

    impl Commands for Recorder {
        fn send(&self, cmd: PlayerCommand) { self.0.lock().unwrap().push(cmd) }
    }

    fn live(programme: Option<&str>) -> PlayerSnapshot {
        let p = Playable {
            media: MediaRef::Channel(ChannelId(7)),
            kind: PlayableKind::Live,
            item: Some(ItemRef { source: SourceId(2), kind: ItemKind::Channel, key: ItemKey("xt:live:1234".into()) }),
            source_id: Some(SourceId(2)),
            title: "BBC One".into(),
            subtitle: programme.map(Into::into),
            logo: Some("https://logos.example/bbc1.png".into()),
            number: Some(1),
            url: Secret::new("http://h.tv/live/u/p/1234.ts".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        };
        PlayerSnapshot {
            state: PlayerState::Playing,
            playable: Some(Arc::new(p)),
            zap: Some(ZapList { collection: Collection::AllChannels, channels: Arc::from(vec![ChannelId(7), ChannelId(8)]) }),
            load_generation: 1,
            ..Default::default()
        }
    }

    fn movie() -> PlayerSnapshot {
        let mut s = live(None);
        let mut p = (**s.playable.as_ref().unwrap()).clone();
        p.kind = PlayableKind::Movie;
        p.media = MediaRef::Movie(crate::model::MovieId(3));
        p.item = Some(ItemRef { source: SourceId(2), kind: ItemKind::Movie, key: ItemKey("xt:vod:3".into()) });
        p.title = "A Film".into();
        p.logo = Some("http://bob:hunter2@h.tv/images/x.png".into());
        s.playable = Some(Arc::new(p));
        s.zap = None;
        s.seekable = true;
        s.duration = Some(5400.0);
        s.position = Some(60.0);
        s
    }

    #[test]
    fn status_mapping() {
        let mut s = live(None);
        assert_eq!(playback_status(&s), PlaybackStatus::Playing);
        s.state = PlayerState::Buffering { since: Instant::now(), percent: None };
        assert_eq!(playback_status(&s), PlaybackStatus::Playing);
        s.paused = true;
        assert_eq!(playback_status(&s), PlaybackStatus::Paused);
        s.paused = false;
        s.state = PlayerState::Paused;
        assert_eq!(playback_status(&s), PlaybackStatus::Paused);
        for st in [PlayerState::Idle, PlayerState::Ended] {
            s.state = st;
            assert_eq!(playback_status(&s), PlaybackStatus::Stopped);
        }
    }

    #[test]
    fn live_metadata_uses_programme_and_channel() {
        let m = metadata(&live(Some("News at Six")));
        assert_eq!(m.title(), Some("News at Six"));
        assert_eq!(m.artist(), Some(vec!["BBC One".to_owned()]));
        assert_eq!(m.art_url().as_deref(), Some("https://logos.example/bbc1.png"));
        assert_eq!(m.length(), None);
        let id = m.trackid().unwrap();
        assert!(id.as_str().starts_with("/tuner/item/") && id.as_str().len() == "/tuner/item/".len() + 16, "{id}");
        assert_eq!(metadata(&live(None)).title(), Some("BBC One"));
        // same item → same track id regardless of the programme
        assert_eq!(metadata(&live(None)).trackid(), Some(id));
        let empty = metadata(&PlayerSnapshot::default());
        assert_eq!(empty.trackid(), Some(TrackId::NO_TRACK));
        assert_eq!(empty.title(), None);
    }

    #[test]
    fn vod_metadata_has_length_and_drops_credential_logos() {
        let m = metadata(&movie());
        assert_eq!(m.title(), Some("A Film"));
        assert_eq!(m.artist(), None);
        assert_eq!(m.length(), Some(Time::from_secs(5400)));
        assert_eq!(m.art_url(), None, "a logo URL with userinfo is not exported");
        assert_eq!(art_url(Some("data:image/png;base64,xx")), None);
        assert_eq!(art_url(Some("file:///home/u/logos/bbc1.png")), None, "local paths stay private");
        assert_eq!(art_url(Some("HTTPS://x.tv/l.png")).as_deref(), Some("HTTPS://x.tv/l.png"));
    }

    #[test]
    fn changes_and_seeks() {
        let t0 = Instant::now();
        let a = View::of(&movie(), t0);
        assert!(a.changes(&a.clone()).is_empty());
        let mut paused = movie();
        paused.state = PlayerState::Paused;
        let b = View::of(&paused, t0);
        assert_eq!(a.changes(&b), vec![Property::PlaybackStatus(PlaybackStatus::Paused)]);
        // 1 s of playback is progress, not a seek
        let mut later = movie();
        later.position = Some(61.0);
        assert_eq!(a.seeked(&View::of(&later, t0 + Duration::from_secs(1))), None);
        later.position = Some(300.0);
        assert_eq!(a.seeked(&View::of(&later, t0 + Duration::from_secs(1))), Some(Time::from_secs(300)));
        // buffering doesn't advance the position: 5 s stuck at 60 s is not a seek
        let mut stuck = movie();
        stuck.state = PlayerState::Buffering { since: Instant::now(), percent: Some(10) };
        let s0 = View::of(&stuck, t0);
        assert_eq!(s0.seeked(&View::of(&stuck, t0 + Duration::from_secs(5))), None);
        // new item: metadata change, no Seeked
        let live_view = View::of(&live(Some("x")), t0);
        assert_eq!(a.seeked(&live_view), None);
        let ch = a.changes(&live_view);
        assert!(ch.iter().any(|p| matches!(p, Property::Metadata(_))));
        assert!(ch.contains(&Property::CanGoNext(true)) && ch.contains(&Property::CanSeek(false)));
    }

    fn session_bus_available() -> bool { std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() }

    async fn call(conn: &zbus::Connection, dest: &str, iface: &str, method: &str) {
        conn.call_method(Some(dest), "/org/mpris/MediaPlayer2", Some(iface), method, &()).await.unwrap();
    }

    async fn get(conn: &zbus::Connection, dest: &str, prop: &str) -> zbus::zvariant::OwnedValue {
        let reply = conn
            .call_method(Some(dest), "/org/mpris/MediaPlayer2", Some("org.freedesktop.DBus.Properties"), "Get", &("org.mpris.MediaPlayer2.Player", prop))
            .await
            .unwrap();
        reply.body().deserialize::<zbus::zvariant::OwnedValue>().unwrap()
    }

    /// Registers under a unique name on the real session bus and drives it like playerctl /
    /// Omarchy's media keys would (zbus client + `busctl` when installed).
    #[test]
    fn serves_mpris_on_the_session_bus() {
        if !session_bus_available() {
            eprintln!("skipped: no session bus");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let suffix = format!("tuner_test_{}", std::process::id());
        let dest = format!("org.mpris.MediaPlayer2.{suffix}");
        let rec = Arc::new(Recorder(Mutex::new(Vec::new())));
        let (snap_tx, snap_rx) = watch::channel(Arc::new(live(Some("News at Six"))));
        let (bus, events) = EventBus::new(None);
        let svc = MprisService::start_with(rt.handle(), rec.clone(), snap_rx, bus, Some(suffix)).unwrap();
        rt.block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            let dbus = fdo::DBusProxy::new(&conn).await.unwrap();
            let name = zbus::names::BusName::try_from(dest.as_str()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !dbus.name_has_owner(name.clone()).await.unwrap() {
                assert!(Instant::now() < deadline, "name never appeared");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let player = "org.mpris.MediaPlayer2.Player";
            call(&conn, &dest, player, "PlayPause").await;
            call(&conn, &dest, player, "Next").await;
            call(&conn, &dest, player, "Previous").await;
            call(&conn, &dest, player, "Stop").await;
            call(&conn, &dest, "org.mpris.MediaPlayer2", "Raise").await;
            let status: String = get(&conn, &dest, "PlaybackStatus").await.try_into().unwrap();
            assert_eq!(status, "Playing");
            let can_next: bool = get(&conn, &dest, "CanGoNext").await.try_into().unwrap();
            assert!(can_next);
            // a state change is visible to the next Get
            let mut paused = live(Some("News at Six"));
            paused.state = PlayerState::Paused;
            snap_tx.send(Arc::new(paused)).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let status: String = get(&conn, &dest, "PlaybackStatus").await.try_into().unwrap();
            assert_eq!(status, "Paused");
        });
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![PlayerCommand::TogglePause, PlayerCommand::Zap { forward: true }, PlayerCommand::Zap { forward: false }, PlayerCommand::Stop]
        );
        assert!(matches!(events.try_recv(), Ok(AppEvent::Platform(PlatformEvent::Raise))));

        // The same through busctl, as a user would check it (skipped when busctl is missing).
        let busctl = |args: &[&str]| std::process::Command::new("busctl").arg("--user").args(args).output();
        if let Ok(out) = busctl(&["get-property", &dest, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", "Metadata"]) {
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert!(text.contains("\"xesam:title\" s \"News at Six\"") && text.contains("\"BBC One\""), "{text}");
            let out = busctl(&["call", &dest, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", "Pause"]).unwrap();
            assert!(out.status.success());
            assert_eq!(rec.0.lock().unwrap().last(), Some(&PlayerCommand::SetPause(true)));
            let out = busctl(&["get-property", &dest, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2", "Identity"]).unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "s \"Tuner\"");
        }
        drop(svc);
        // the name is released once the task is gone
        rt.block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            let dbus = fdo::DBusProxy::new(&conn).await.unwrap();
            let name = zbus::names::BusName::try_from(dest.as_str()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while dbus.name_has_owner(name.clone()).await.unwrap() {
                assert!(Instant::now() < deadline, "name not released");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
}
