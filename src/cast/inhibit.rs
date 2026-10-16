//! The logind sleep inhibitor while casting (DESIGN §40 §11.5): `Inhibit("sleep", "Tuner",
//! "Casting to <TV>", "block")` through zbus, held only while a session exists and
//! `cast.keep_awake` is on; lid close still suspends. OWNER: CAST-6.
//!
//! Without it a suspend kills the relay (Tuner *is* the TV's source). The Wayland idle
//! inhibitor (`platform::idle`) is a surface inhibitor and is released while casting, so the
//! laptop's screen may lock and blank; this one holds whatever workspace Tuner is on and with the
//! screen locked. logind keeps the lock while the returned file descriptor is open: dropping
//! [`SleepInhibitor`] closes it, which releases the lock.

use std::time::Duration;

/// zbus calls have no timeout of their own: a wedged system bus must not stall a cast.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// A held sleep lock (released on drop).
pub struct SleepInhibitor {
    _fd: zbus::zvariant::OwnedFd,
}

impl std::fmt::Debug for SleepInhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SleepInhibitor")
    }
}

impl SleepInhibitor {
    /// Block sleep while casting to `device_name` (sanitized; shown by `systemd-inhibit
    /// --list`). `None` off the desktop (tests never touch the system bus) or when logind
    /// isn't there: casting goes on without it.
    pub async fn take(device_name: &str) -> Option<Self> {
        Self::take_reason(&format!("Casting to {device_name}")).await
    }

    /// Background media transfers keep sleep blocked, including a headless recording.
    pub async fn for_downloads() -> Option<Self> {
        Self::take_reason("Downloading or recording").await
    }

    async fn take_reason(why: &str) -> Option<Self> {
        if crate::platform::off_desktop() {
            return None;
        }
        match tokio::time::timeout(CALL_TIMEOUT, inhibit(why)).await {
            Ok(Ok(fd)) => {
                tracing::debug!("cast: sleep inhibited while casting");
                Some(Self { _fd: fd })
            }
            Ok(Err(e)) => {
                tracing::info!("cast: no sleep inhibitor (logind): {e}");
                None
            }
            Err(_) => {
                tracing::info!("cast: no sleep inhibitor (logind didn't answer)");
                None
            }
        }
    }
}

async fn inhibit(why: &str) -> zbus::Result<zbus::zvariant::OwnedFd> {
    let conn = zbus::Connection::system().await?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &("sleep", "Tuner", why, "block"),
        )
        .await?;
    reply.body().deserialize::<zbus::zvariant::OwnedFd>()
}
