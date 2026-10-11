//! Who gets the screen (B1): every play request from outside the current screen goes through
//! [`route::route`] — the queue's "steps aside", Following's "never interrupt a film", multi-view's
//! "outside plays go to the silent picture", the zap shield. Implemented in the batch base;
//! OWNER afterwards: MV-1 (it extends the multi-view rows) with the integrator.

pub mod route;

pub use route::{Downgrade, Kind, NotifyReason, PlayIntent, PlaySource, Route, RouteState, TilesState, route};
