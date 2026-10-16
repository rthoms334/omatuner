//! Downloads and recordings (DESIGN §43; docs/design/downloads-design.md).
pub mod model;
pub mod planning;
pub mod scheduler;
pub mod service;
pub use service::DownloadsHandle;
pub mod worker;
pub use model::*;

/// One gate for the service, UI, keys and agent tool.
pub const ENABLED: bool = true;

pub mod arming;
pub mod maintenance;

pub mod limits;

pub mod reader;
