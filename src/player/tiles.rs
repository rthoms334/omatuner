//! The pictures behind one façade (DESIGN §35, batch plan §0.3): `Services::tiles` holds tile A
//! (the player that always exists) and, once multi-view starts it, tile B. Single view goes
//! through tile A exactly as before. OWNER: MV-1 (the app's focus / layout logic is MV-2's).
//!
//! * **Lazy B.** The first multi-view request calls `Services::start_tile_b`, which reserves a
//!   [`TileSlot`] ([`Tiles::begin_b`]) and runs `Player::start_tile` on the blocking pool (mpv
//!   init takes 30–80 ms). The app hears `AppEvent::Tile(TileEvent::Ready(B))`, adopts the
//!   player (`Services::adopt_tile_b` → [`Tiles::adopt_b`]) and, GL current, makes B's surface
//!   (`VideoSurface::from_parts`). B then stays alive until exit: after a stop and a trim it
//!   holds ~25 MiB, and recreating it would bring back the risky teardown order (render
//!   context → FBO → mpv) at runtime.
//! * **Focus and sound are separate.** Focus is where keys go ([`Tiles::focused`]); the sound is
//!   one picture's ([`Tiles::move_sound`]), with ONE listening volume and mute that move with
//!   it. Plays from outside go to the silent picture (B1, `playback::route`).
//! * **Routing off the UI thread.** [`TileRegistry`] holds every picture's handle for the MPRIS
//!   router and the agent, which read focus and sound from `AppShared::tiles` (the UI
//!   publishes them; they route while the window is hidden).
//! * **Events.** Picture B's player events arrive as [`TileEvent::Player`], never as
//!   `AppEvent::Player` (single-view code only ever sees picture A's).

use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::watch;

use super::budget::SharedBudget;
use super::{Player, PlayerCommand, PlayerEvent, PlayerHandle, PlayerSnapshot};
use crate::model::{SourceId, TileId};

/// What multi-view's second player tells the app.
#[derive(Debug, Clone, PartialEq)]
pub enum TileEvent {
    /// Tile B's player is up: adopt it (`Services::adopt_tile_b`); its surface is made on the
    /// next frame, GL current.
    Ready(TileId),
    /// Starting tile B failed (the reason, for a toast).
    Failed(TileId, String),
    /// A player event of a picture other than A.
    Player(TileId, PlayerEvent),
}

/// Every picture's handle, for routers off the UI thread (MPRIS, the agent). Cheap to clone.
#[derive(Clone, Default)]
pub struct TileRegistry(Arc<RwLock<[Option<PlayerHandle>; 2]>>);

impl TileRegistry {
    fn slot(tile: TileId) -> usize {
        usize::from(tile.0).min(1)
    }

    /// A registry of picture A alone.
    pub fn single(a: PlayerHandle) -> Self {
        let r = Self::default();
        r.set(TileId::A, Some(a));
        r
    }

    pub(crate) fn set(&self, tile: TileId, h: Option<PlayerHandle>) {
        self.0.write().unwrap_or_else(|e| e.into_inner())[Self::slot(tile)] = h;
    }

    /// The handle of `tile`, if that picture's player exists.
    pub fn get(&self, tile: TileId) -> Option<PlayerHandle> {
        self.0.read().unwrap_or_else(|e| e.into_inner())[Self::slot(tile)].clone()
    }

    /// `tile`'s handle, else picture A's.
    pub fn get_or_a(&self, tile: TileId) -> Option<PlayerHandle> {
        self.get(tile).or_else(|| self.get(TileId::A))
    }

    /// Every picture's handle, A first.
    pub fn all(&self) -> Vec<PlayerHandle> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect()
    }
}

/// Where a picture started on the blocking pool lands until the app adopts it.
#[derive(Clone, Default)]
pub struct TileSlot(Arc<Mutex<Option<Result<Player, String>>>>);

impl TileSlot {
    /// The start finished (the app is told separately, through the event bus).
    pub fn fill(&self, r: Result<Player, String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    }

    fn take(&self) -> Option<Result<Player, String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The pictures.
pub struct Tiles {
    a: Player,
    b: Option<Player>,
    /// Picture B is starting (`begin_b` → `adopt_b`).
    starting: Option<TileSlot>,
    /// Where keys go (UI state mirrored here so non-UI callers agree).
    focus: TileId,
    /// The picture with the sound.
    audible: TileId,
    budget: SharedBudget,
    registry: TileRegistry,
}

impl Tiles {
    /// Single view: tile A only (its budget is the one every picture shares).
    pub fn single(a: Player) -> Self {
        let budget = a.budget().clone();
        let registry = TileRegistry::single(a.handle());
        Self { a, b: None, starting: None, focus: TileId::A, audible: TileId::A, budget, registry }
    }

    /// Tile A's player (the video surface's core, shutdown order).
    pub fn a(&self) -> &Player {
        &self.a
    }

    /// Tile B's player, once started and adopted.
    pub fn b(&self) -> Option<&Player> {
        self.b.as_ref()
    }

    /// The player of `tile`, if it exists.
    pub fn player(&self, tile: TileId) -> Option<&Player> {
        if tile == TileId::A { Some(&self.a) } else { self.b.as_ref() }
    }

    /// The handle of `tile` (A when `tile` has no player).
    pub fn handle(&self, tile: TileId) -> PlayerHandle {
        match (tile, &self.b) {
            (TileId::B, Some(b)) => b.handle(),
            _ => self.a.handle(),
        }
    }

    /// The focused picture's handle (`UiCtx::play`, `App::send_player`).
    pub fn focused(&self) -> PlayerHandle {
        self.handle(self.focus)
    }

    pub fn focus(&self) -> TileId {
        self.focus
    }

    /// Focus a picture (ignored for one that doesn't exist). Never moves the sound.
    pub fn set_focus(&mut self, tile: TileId) {
        if tile == TileId::A || self.b.is_some() {
            self.focus = tile;
        }
    }

    /// The picture with the sound (volume and mute keys go there).
    pub fn audible(&self) -> TileId {
        self.audible
    }

    /// The audible picture's handle.
    pub fn audible_handle(&self) -> PlayerHandle {
        self.handle(self.audible)
    }

    /// Move the sound to `to` (`M`, an outside play that takes the sound): the old picture is
    /// muted first, then `to` gets the listening volume and the user's mute and is unmuted —
    /// one listening volume, whichever picture speaks. Ignored for a picture that doesn't
    /// exist or already has the sound.
    pub fn move_sound(&mut self, to: TileId) {
        if to == self.audible || self.player(to).is_none() {
            return;
        }
        let from = self.handle(self.audible);
        let listening = from.snapshot();
        from.send(PlayerCommand::SetAudible(false));
        let to_h = self.handle(to);
        to_h.send(PlayerCommand::SetVolume(listening.volume));
        to_h.send(PlayerCommand::SetMute(listening.muted));
        to_h.send(PlayerCommand::SetAudible(true));
        self.audible = to;
    }

    /// The planner's move, ONLY after the user said yes ("Move picture A to its TV 2 copy so B
    /// can use TV? · Enter Move"): `mover` retunes with `mover_cmd` (its copy, on playlist
    /// `mover_to`), then `target` plays with `target_cmd` (on playlist `target_to`, when
    /// known). Both claims move at once, so each load waits for the other picture's old stream
    /// to close instead of being refused (a few seconds at most) — whichever player gets its
    /// command first, and also when the two swap playlists — and the shared race window then
    /// retries the 403s of a provider that still counts the old stream (up to ~45 s).
    pub fn rearrange(
        &self,
        mover: TileId,
        mover_to: SourceId,
        mover_cmd: PlayerCommand,
        target: TileId,
        target_to: Option<SourceId>,
        target_cmd: PlayerCommand,
    ) {
        if self.player(mover).is_none() || self.player(target).is_none() || mover == target {
            return;
        }
        tracing::info!("player: moving picture {mover} to source {} so picture {target} can use its playlist", mover_to.0);
        self.budget.claim(mover, Some(mover_to));
        if target_to.is_some() {
            self.budget.claim(target, target_to);
        }
        self.handle(mover).send(mover_cmd);
        self.handle(target).send(target_cmd);
    }

    /// Reserve picture B's start: `Some(slot)` to fill on the blocking pool, `None` when B
    /// exists or is already starting.
    pub fn begin_b(&mut self) -> Option<TileSlot> {
        if self.b.is_some() || self.starting.is_some() {
            return None;
        }
        let slot = TileSlot::default();
        self.starting = Some(slot.clone());
        Some(slot)
    }

    /// Take picture B's player once its start finished: `Some(Ok(()))` adopted, `Some(Err)`
    /// failed (a later `begin_b` may try again), `None` not finished (or nothing started).
    pub fn adopt_b(&mut self) -> Option<Result<(), String>> {
        let r = self.starting.as_ref()?.take()?;
        self.starting = None;
        Some(r.map(|p| {
            self.registry.set(TileId::B, Some(p.handle()));
            self.b = Some(p);
        }))
    }

    /// The main picture's snapshots (Up next binds to these; the only picture in single view).
    pub fn main_watch(&self) -> watch::Receiver<Arc<PlayerSnapshot>> {
        self.a.handle().subscribe()
    }

    /// The connection budget every picture shares.
    pub fn budget(&self) -> &SharedBudget {
        &self.budget
    }

    /// Every picture's handle for routers off the UI thread.
    pub fn registry(&self) -> TileRegistry {
        self.registry.clone()
    }

    /// Every picture's handle (ApplyConfig, connection limits, stop all).
    pub fn handles(&self) -> Vec<PlayerHandle> {
        std::iter::once(self.a.handle()).chain(self.b.as_ref().map(Player::handle)).collect()
    }

    /// Stop every player, B first (after the video surfaces were destroyed).
    pub fn shutdown(&mut self) {
        if let Some(b) = &mut self.b {
            b.shutdown();
        }
        self.registry.set(TileId::B, None);
        self.a.shutdown();
    }
}
