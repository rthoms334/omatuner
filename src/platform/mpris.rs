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
//!   Raise/Quit → `PlatformEvent`.
//! * Subscribes to the player snapshot `watch` channel and emits PropertiesChanged only for
//!   changed fields (position is polled by clients; emit `Seeked` on seeks).
//!
//! Omarchy media keys (`omarchy-shell media playPause|next|previous`) and the bar's media
//! widget then work out of the box.
//! API choice: `mpris_server::Player` wraps a `LocalServer` that is not `Send`, so it cannot be
//! spawned on the runtime. Implement `RootInterface + PlayerInterface` on a `Send` struct
//! (holding the `PlayerHandle` + latest snapshot) and serve it with `mpris_server::Server<T>`.

use crate::error::Result;
use crate::msg::EventBus;
use crate::player::PlayerHandle;

pub struct MprisService {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl MprisService {
    /// Register the bus name and start serving on `rt`. Failure (no session bus) → Err; the
    /// caller logs and continues without MPRIS.
    /// SKELETON SAFE DEFAULT: `Err` (logged; the app runs without MPRIS).
    pub fn start(rt: &tokio::runtime::Handle, player: PlayerHandle, bus: EventBus) -> Result<Self> {
        Err(crate::error::Error::other("MPRIS not implemented yet (WP-08)"))
    }

    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}
