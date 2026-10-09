//! File dialogs through the XDG portal (`rfd` xdg-portal backend → xdg-desktop-portal-gtk,
//! auto-floated by Omarchy, follows prefer-dark/light). Owner: WP-08.
//! Async on the runtime; result delivered as `PlatformEvent::FilePicked`.

use super::PickPurpose;
use crate::msg::EventBus;

/// Open a picker for `purpose` (filters: `.m3u/.m3u8` for playlists, `.xml/.gz/.xz` for EPG,
/// `.json` for backups; save dialog for exports with a dated default name).
pub fn pick(rt: &tokio::runtime::Handle, bus: EventBus, purpose: PickPurpose) { todo!("WP-08") }
