//! Desktop integration: MPRIS, idle inhibit, notifications/reminders, file picker, Hyprland
//! helpers. Owner: WP-08. Every service degrades gracefully (logs a warning and becomes a
//! no-op) when its D-Bus name / Wayland protocol / binary is unavailable.

pub mod filepicker;
pub mod hypr;
pub mod idle;
pub mod mpris;
pub mod notify;
pub mod signals;

use crate::model::{ItemRef, ReminderId};

/// Events from platform services to the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformEvent {
    /// MPRIS `Raise` — bring the window forward (best effort, `hypr::focus_self`).
    Raise,
    /// MPRIS `Quit`.
    Quit,
    /// A reminder notification fired (the UI may show a toast too).
    ReminderFired(ReminderId),
    /// User clicked "Watch" on a reminder notification → the app resolves the channel by its
    /// stable ref (`LiveIndex::by_ref`), plays it and raises the window.
    ReminderWatch { reminder: ReminderId, channel: ItemRef },
    /// File picker result (None = cancelled). `purpose` echoes the request.
    FilePicked { purpose: PickPurpose, path: Option<std::path::PathBuf> },
}

/// glibc malloc arenas the process may create (default: 8 per core).
const MALLOC_ARENAS: i32 = 2;

/// Cap glibc's malloc arenas at [`MALLOC_ARENAS`]; call first thing in `main`, before any
/// thread starts. libmpv starts and ends threads on every file (demuxer, decoders), each of
/// which may get an arena of its own; the few small allocations that outlive a file stay in
/// those arenas and keep their pages resident, so RSS climbed with every channel change and
/// `malloc_trim` could not give it back (`tests/e2e_soak.rs` on aarch64 with 16 KiB pages:
/// 232 → 281 MiB over 1 000 zaps, +21 %; with two arenas ≈ 137 MiB and +1–2 % over 2 000 zaps,
/// same zap speed). An arena limit the user set (`MALLOC_ARENA_MAX`, or
/// `glibc.malloc.arena_max` in `GLIBC_TUNABLES`) wins.
pub fn limit_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let user_set =
        std::env::var_os("MALLOC_ARENA_MAX").is_some() || std::env::var("GLIBC_TUNABLES").is_ok_and(|t| t.contains("glibc.malloc.arena_max"));
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if !user_set {
        const M_ARENA_MAX: i32 = -8; // <malloc.h>
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        // SAFETY: plain glibc call without pointers; it only sets an allocator limit (runs
        // before logging exists; on failure glibc's default stays, which is merely bigger).
        unsafe { mallopt(M_ARENA_MAX, MALLOC_ARENAS) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickPurpose {
    OpenPlaylist,
    OpenEpg,
    ExportBackup,
    ImportBackup,
    ExportLog,
}
