//! Idle inhibit while playing. Owner: WP-08. DESIGN §21.2.
//!
//! How Omarchy 4 decides (read from /usr/share/omarchy/shell/plugins/services/idle/Service.qml):
//! the Quickshell `IdleMonitor` (ext-idle-notify-v1) is created with `respectInhibitors: true`,
//! so the compositor's *Wayland idle inhibitors* are honoured: `zwp_idle_inhibit_manager_v1`
//! inhibitors on visible surfaces and Hyprland `idle_inhibit` window rules (Omarchy already
//! ships `o.window({tag="noidle"}, {idle_inhibit="always"})` and per-app `fullscreen` rules).
//! There is NO `org.freedesktop.ScreenSaver` service on the session bus and
//! xdg-desktop-portal-hyprland does not implement `Inhibit`, so D-Bus inhibition would be
//! silently ignored; `systemd-inhibit` does not affect ext-idle-notify either. The user's
//! "stay awake" toggle is a separate state file (`~/.local/state/omarchy/indicators/stay-awake`)
//! that we must not touch.
//!
//! Mechanism: create `zwp_idle_inhibitor_v1` on OUR `wl_surface`, using a wayland-client
//! connection built from eframe's foreign `wl_display` (`Backend::from_foreign_display`) and
//! the surface pointer from the window handle (`ObjectId::from_ptr`). Own event queue; no
//! interference with winit's queue. Inhibitor exists while `PlayerState::is_active()` and the
//! config mode allows (WhilePlaying / Fullscreen); destroyed otherwise. The compositor only
//! honours it while the surface is visible — correct behaviour for a video player.
//! Fallback (documented in the Hyprland snippet): `idle_inhibit = "fullscreen"` window rule.
//! `hyprctl clients` shows `"inhibitingIdle": true` on the window while it is held.

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::{self, ZwpIdleInhibitManagerV1};
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::{self, ZwpIdleInhibitorV1};

use crate::config::IdleInhibitMode;
use crate::error::{Error, Result};

pub struct IdleInhibitor {
    conn: Connection,
    queue: EventQueue<QueueState>,
    manager: ZwpIdleInhibitManagerV1,
    /// The window's surface (owned by winit; we never destroy it).
    surface: WlSurface,
    inhibitor: Option<ZwpIdleInhibitorV1>,
    active: bool,
}

impl IdleInhibitor {
    /// Bind `zwp_idle_inhibit_manager_v1` from the window's display/surface handles.
    /// Returns Err when not on Wayland or the protocol is missing (caller logs, continues).
    /// One registry round trip on our own queue (called once, at window creation).
    pub fn new(display: raw_window_handle::RawDisplayHandle, window: raw_window_handle::RawWindowHandle) -> Result<Self> {
        use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
        let (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) = (display, window) else {
            return Err(Error::other("not a Wayland window (idle inhibit needs zwp_idle_inhibit_manager_v1)"));
        };
        // SAFETY: eframe/winit keep the wl_display alive for the whole event loop, which
        // outlives the app (and so this inhibitor). A foreign display is never closed by us.
        let backend = unsafe { Backend::from_foreign_display(d.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<QueueState>(&conn).map_err(|e| Error::other(format!("wayland registry: {e}")))?;
        let manager: ZwpIdleInhibitManagerV1 =
            globals.bind(&queue.handle(), 1..=1, ()).map_err(|e| Error::other(format!("zwp_idle_inhibit_manager_v1 unavailable: {e}")))?;
        // SAFETY: the pointer is the live wl_surface of our window (same lifetime argument as
        // the display); we only use it as the argument of `create_inhibitor`.
        let id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }.map_err(|e| Error::other(format!("wl_surface: {e}")))?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| Error::other(format!("wl_surface: {e}")))?;
        Ok(Self { conn, queue, manager, surface, inhibitor: None, active: false })
    }

    /// Apply the desired state (idempotent; called every `logic` frame — must be cheap).
    pub fn set(&mut self, mode: IdleInhibitMode, playing: bool, fullscreen: bool) {
        let want = wanted(mode, playing, fullscreen);
        if want == self.active {
            return;
        }
        if want {
            self.inhibitor = Some(self.manager.create_inhibitor(&self.surface, &self.queue.handle(), ()));
        } else if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.active = want;
        tracing::debug!("idle inhibitor {}", if want { "held" } else { "released" });
        // Nothing is ever sent to our queue, but keep it drained; flushing is non-blocking.
        let _ = self.queue.dispatch_pending(&mut QueueState);
        if let Err(e) = self.conn.flush() {
            tracing::debug!("idle inhibitor flush: {e}");
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for IdleInhibitor {
    fn drop(&mut self) {
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.manager.destroy();
        let _ = self.conn.flush();
    }
}

/// Should the inhibitor be held? `playing` = active and not paused (the app's definition).
fn wanted(mode: IdleInhibitMode, playing: bool, fullscreen: bool) -> bool {
    match mode {
        IdleInhibitMode::WhilePlaying => playing,
        IdleInhibitMode::Fullscreen => playing && fullscreen,
        IdleInhibitMode::Never => false,
    }
}

/// Dispatch target of our private queue (none of these objects send events we care about).
struct QueueState;

impl Dispatch<WlRegistry, GlobalListContents> for QueueState {
    fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for QueueState {
    fn event(_: &mut Self, _: &ZwpIdleInhibitManagerV1, _: zwp_idle_inhibit_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for QueueState {
    fn event(_: &mut Self, _: &ZwpIdleInhibitorV1, _: zwp_idle_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, XlibDisplayHandle, XlibWindowHandle};
    use wayland_client::protocol::wl_compositor::{self, WlCompositor};
    use wayland_client::protocol::wl_surface;

    use super::*;

    #[test]
    fn mode_table() {
        use IdleInhibitMode::*;
        for (mode, playing, fs, want) in [
            (WhilePlaying, true, false, true),
            (WhilePlaying, true, true, true),
            (WhilePlaying, false, true, false),
            (Fullscreen, true, false, false),
            (Fullscreen, true, true, true),
            (Fullscreen, false, true, false),
            (Never, true, true, false),
        ] {
            assert_eq!(wanted(mode, playing, fs), want, "{mode:?} playing={playing} fullscreen={fs}");
        }
    }

    #[test]
    fn non_wayland_handles_are_rejected() {
        let d = RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0));
        let w = RawWindowHandle::Xlib(XlibWindowHandle::new(1));
        assert!(IdleInhibitor::new(d, w).is_err());
    }

    /// Test-side client: owns a connection and an (unmapped, role-less, invisible) surface.
    struct Client;
    impl Dispatch<WlRegistry, GlobalListContents> for Client {
        fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<WlCompositor, ()> for Client {
        fn event(_: &mut Self, _: &WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<WlSurface, ()> for Client {
        fn event(_: &mut Self, _: &WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }

    /// Against the running compositor (skipped without a Wayland session): bind through a
    /// foreign display exactly like the app does, toggle the inhibitor and check the
    /// compositor accepted every request (a protocol error would kill the connection).
    #[test]
    fn toggles_an_inhibitor_on_a_foreign_display() {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            eprintln!("skipped: no Wayland session");
            return;
        }
        let Ok(conn) = Connection::connect_to_env() else {
            eprintln!("skipped: cannot connect to the compositor");
            return;
        };
        let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let display = WaylandDisplayHandle::new(NonNull::new(conn.backend().display_ptr().cast()).unwrap());
        let window = WaylandWindowHandle::new(NonNull::new(surface.id().as_ptr().cast()).unwrap());
        let mut inhibitor = match IdleInhibitor::new(RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) {
            Ok(i) => i,
            Err(e) if e.to_string().contains("unavailable") => {
                eprintln!("skipped: compositor has no idle-inhibit protocol");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert!(!inhibitor.is_active());
        inhibitor.set(IdleInhibitMode::WhilePlaying, true, false);
        assert!(inhibitor.is_active());
        queue.roundtrip(&mut Client).expect("compositor accepted create_inhibitor");
        inhibitor.set(IdleInhibitMode::WhilePlaying, true, false); // idempotent
        inhibitor.set(IdleInhibitMode::Fullscreen, true, false);
        assert!(!inhibitor.is_active());
        queue.roundtrip(&mut Client).expect("compositor accepted destroy");
        inhibitor.set(IdleInhibitMode::Never, true, true);
        assert!(!inhibitor.is_active());
        inhibitor.set(IdleInhibitMode::WhilePlaying, true, true);
        drop(inhibitor); // destroys the held inhibitor and the manager
        queue.roundtrip(&mut Client).expect("clean teardown");
        surface.destroy();
        queue.roundtrip(&mut Client).unwrap();
    }
}
