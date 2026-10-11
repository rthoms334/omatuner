//! The pictures behind one façade (DESIGN §35, batch plan §0.3): `Services::tiles` holds tile A
//! (the player that always exists) and, once multi-view starts it, tile B. Single view goes
//! through tile A exactly as before. Batch base: tile A only; MV-1 adds B (`start_tile` on the
//! blocking pool, `AppEvent::Tile(TileEvent::Ready)`), MV-2 the focus / sound logic in the app.
//! OWNER: MV-1.

use std::sync::Arc;

use tokio::sync::watch;

use super::budget::SharedBudget;
use super::{Player, PlayerHandle, PlayerSnapshot};
use crate::model::TileId;

/// What multi-view's lazy second player tells the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TileEvent {
    /// Tile B's player is up (its surface is made on the next frame, GL current).
    Ready(TileId),
    /// Starting tile B failed (the reason, for a toast).
    Failed(TileId, String),
}

/// The pictures.
pub struct Tiles {
    a: Player,
    b: Option<Player>,
    /// Where keys go (UI state mirrored here so non-UI callers agree).
    focus: TileId,
    budget: SharedBudget,
}

impl Tiles {
    /// Single view: tile A only.
    pub fn single(a: Player) -> Self {
        Self { a, b: None, focus: TileId::A, budget: SharedBudget::default() }
    }

    /// Tile A's player (the video surface's core, shutdown order).
    pub fn a(&self) -> &Player {
        &self.a
    }

    /// Tile B's player, once started.
    pub fn b(&self) -> Option<&Player> {
        self.b.as_ref()
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

    /// Focus a picture (ignored for one that doesn't exist).
    pub fn set_focus(&mut self, tile: TileId) {
        if tile == TileId::A || self.b.is_some() {
            self.focus = tile;
        }
    }

    /// The main picture's snapshots (Up next binds to these; the only picture in single view).
    pub fn main_watch(&self) -> watch::Receiver<Arc<PlayerSnapshot>> {
        self.a.handle().subscribe()
    }

    /// The connection budget every picture shares.
    pub fn budget(&self) -> &SharedBudget {
        &self.budget
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
        self.a.shutdown();
    }
}
