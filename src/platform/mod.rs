//! Desktop integration: MPRIS, idle inhibit, notifications/reminders, file picker, Hyprland
//! helpers, the pop-out player's window control, memory upkeep (glibc trims, memory stats).
//! Owner: WP-08. Every service degrades
//! gracefully (logs a warning and becomes a no-op) when its D-Bus name / Wayland protocol /
//! binary is unavailable.

pub mod filepicker;
pub mod hypr;
pub mod idle;
pub mod memory;
pub mod memstats;
pub mod mpris;
pub mod notify;
pub mod popout;
pub mod signals;
pub mod systemd;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::{ItemRef, ReminderId};

static OFF_DESKTOP: AtomicBool = AtomicBool::new(false);

/// Keep this process off the user's desktop from now on: no window focus requests
/// (`hypr::focus_self`) and no notifications to the desktop's server (`notify`: only an
/// explicit bus answers). The headless test UI (`ui::testing::TestUi`) sets it, so integration
/// tests never touch a live session.
pub fn keep_off_desktop() {
    OFF_DESKTOP.store(true, Ordering::Relaxed);
}

/// Off the desktop: unit tests, a process that called [`keep_off_desktop`], or
/// `TUNER_NO_DESKTOP` set.
pub fn off_desktop() -> bool {
    cfg!(test) || OFF_DESKTOP.load(Ordering::Relaxed) || std::env::var_os("TUNER_NO_DESKTOP").is_some()
}

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
    /// An automatic switch (`Reminder::switches`, made by an MCP client's `schedule_switch`)
    /// is due in `SWITCH_WARN_S`: the app toasts "Switching to ‹channel› in 10 s" with Cancel
    /// (`Action::CancelSwitch`). DESIGN §30.8.
    SwitchSoon { reminder: ReminderId, channel: ItemRef, start: crate::model::Timestamp },
    /// The programme starts: the app plays the channel (PIN rules apply) unless the switch was
    /// cancelled; the scheduler has already raised the window.
    SwitchNow { reminder: ReminderId, channel: ItemRef },
    /// File picker result (None = cancelled). `purpose` echoes the request.
    FilePicked { purpose: PickPurpose, path: Option<std::path::PathBuf> },
    /// Pop-out player: Hyprland window state and request results (`platform::popout`).
    PopOut(popout::PopOutEvent),
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
