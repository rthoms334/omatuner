//! Pop-out player (DESIGN §21.6): Tuner floats, sizes, places, pins and tags its OWN window
//! through Hyprland's IPC, and follows the window's floating / pinned state (also when the user
//! pops it out with Omarchy's Super+O) so the app can switch to its video-only layout. A pinned
//! floating window is shown on every workspace, so the pop-out follows the user around.
//!
//! * Requests go to `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (what
//!   `hyprctl` writes, without a process per command); window events come from
//!   `.socket2.sock` (`changefloatingmode>>ADDR,0|1`, `pin>>ADDR,0|1`, `openwindow`,
//!   `closewindow`).
//! * Dispatch syntax: Omarchy 4 runs Hyprland's Lua config, where `dispatch` evaluates a Lua
//!   dispatcher (`hl.dsp.window.pin({ window = "address:0x…", action = "enable" })`); a classic
//!   hyprlang config takes `pin address:0x…`. Like `omarchy-hyprland-window-pop`, the Lua form
//!   is tried first and the classic one when Hyprland doesn't answer `ok`; the first form that
//!   works is kept for the session (a running config is either Lua or not).
//! * Our window is found by OUR pid in `j/clients`, never by class, so another profile's Tuner
//!   is never touched; its address is validated before it goes into a Lua string.
//! * Pop-out: (un-maximize) → float → size → place → pin → raise → tag `pop` (Omarchy styles
//!   `tag:pop` windows); a step Hyprland refuses undoes the steps before it. Pop-in undoes
//!   only what pop-out changed: unpin, untag, then back to the recorded floating geometry or
//!   tiled. A pop-in without a record (popped out by Super+O) does what Super+O does on its
//!   second press: unpin, untag, tile. A fullscreen window is neither popped out nor in (the
//!   app leaves fullscreen first and the worker waits for Hyprland to see it).
//! * The pop-out's place is remembered only when the user moved or resized it; an unmoved
//!   pop-out keeps following the configured corner (another monitor, another resolution).
//! * Everything here runs on the `hypr-popout` / `hypr-events` threads; the UI sends
//!   [`PopRequest`]s and receives [`PopOutEvent`]s (`PlatformEvent::PopOut`).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::config::{PopoutCorner, PopoutSize};
use crate::msg::{AppEvent, EventBus};
use crate::platform::PlatformEvent;

/// Gap between a new pop-out and the monitor edges (logical px): Omarchy's `gaps_out`, so the
/// pop-out lines up with the edges of tiled windows.
const MARGIN: i32 = 10;
/// A pop-out within this many px of its default place counts as not moved.
const MOVED_SLACK: i32 = 2;
/// Smallest pop-out a remembered geometry is clamped to.
const MIN_SIZE: (i32, i32) = (192, 108);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// A pop-out right after leaving fullscreen, or right after the window was created, waits in
/// steps of this long for Hyprland to see it (fullscreen gone, the window mapped)…
const READY_WAIT: Duration = Duration::from_millis(100);
/// …this many times while it's fullscreen…
const FULLSCREEN_TRIES: u32 = 10;
/// …and this many times while Hyprland doesn't list the window (a launch straight into the
/// pop-out asks on the first frame, before the window is mapped).
const MAP_TRIES: u32 = 30;

/// One request on Hyprland's request socket. `SocketIpc` in the app, a fake in tests.
pub trait HyprIpc: Send {
    /// Send `req` (`j/clients`, `/dispatch …`) and return Hyprland's reply.
    fn request(&mut self, req: &str) -> Result<String, String>;
}

/// Hyprland's sockets of this session.
#[derive(Debug, Clone)]
pub struct SocketIpc {
    dir: PathBuf,
}

impl SocketIpc {
    /// `None` outside Hyprland (no `HYPRLAND_INSTANCE_SIGNATURE`).
    pub fn from_env() -> Option<Self> {
        let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").filter(|s| !s.is_empty())?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
        Some(Self { dir: PathBuf::from(runtime).join("hypr").join(sig) })
    }

    fn events(&self) -> std::io::Result<UnixStream> {
        UnixStream::connect(self.dir.join(".socket2.sock"))
    }
}

impl HyprIpc for SocketIpc {
    fn request(&mut self, req: &str) -> Result<String, String> {
        let io = |e: std::io::Error| format!("Hyprland IPC: {e}");
        let mut s = UnixStream::connect(self.dir.join(".socket.sock")).map_err(io)?;
        s.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        s.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
        s.write_all(req.as_bytes()).map_err(io)?;
        // Hyprland answers and closes the connection
        let mut reply = String::new();
        s.read_to_string(&mut reply).map_err(io)?;
        Ok(reply)
    }
}

/// A window in `j/clients` (the fields we use).
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub address: String,
    pub pid: i64,
    #[serde(default = "mapped_default")]
    pub mapped: bool,
    pub floating: bool,
    pub pinned: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub monitor: i64,
    /// Fullscreen mode (0 = none).
    #[serde(default)]
    pub fullscreen: i64,
    /// `*` marks a tag set by a window rule.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn mapped_default() -> bool {
    true
}

impl Client {
    fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t.trim_end_matches('*') == tag)
    }
}

/// A monitor in `j/monitors` (pixel size, logical position and reserved edges).
#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub id: i64,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    #[serde(default)]
    pub transform: i32,
    /// left, top, right, bottom (logical px; the bar).
    #[serde(default)]
    pub reserved: [i32; 4],
}

/// A rectangle in Hyprland's logical layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Monitor {
    /// The logical area windows may use (minus the bar).
    fn usable(&self) -> Rect {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let (mut w, mut h) = ((f64::from(self.width) / scale).round() as i32, (f64::from(self.height) / scale).round() as i32);
        if self.transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        let [l, t, r, b] = self.reserved;
        Rect { x: self.x + l, y: self.y + t, w: (w - l - r).max(1), h: (h - t - b).max(1) }
    }
}

/// Where a pop-out goes on monitor `m`: the remembered geometry (relative to the monitor,
/// clamped into its usable area), else `size` in `corner` with a margin.
fn place(m: &Monitor, size: PopoutSize, corner: PopoutCorner, last: Option<[i32; 4]>) -> Rect {
    let area = m.usable();
    let fit = |w: i32, h: i32| (w.clamp(MIN_SIZE.0.min(area.w), area.w), h.clamp(MIN_SIZE.1.min(area.h), area.h));
    let clamp_pos = |x: i32, y: i32, w: i32, h: i32| Rect { x: x.clamp(area.x, area.x + area.w - w), y: y.clamp(area.y, area.y + area.h - h), w, h };
    if let Some([x, y, w, h]) = last {
        let (w, h) = fit(w, h);
        return clamp_pos(m.x + x, m.y + y, w, h);
    }
    let (w, h) = size.logical();
    let (w, h) = fit(w, h);
    let left = area.x + MARGIN;
    let right = area.x + area.w - MARGIN - w;
    let top = area.y + MARGIN;
    let bottom = area.y + area.h - MARGIN - h;
    let (x, y) = match corner {
        PopoutCorner::BottomRight => (right, bottom),
        PopoutCorner::BottomLeft => (left, bottom),
        PopoutCorner::TopRight => (right, top),
        PopoutCorner::TopLeft => (left, top),
    };
    clamp_pos(x, y, w, h)
}

/// One window dispatcher, rendered in Lua or classic syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Float(bool),
    /// Only sent when the state differs (classic `pin` toggles).
    Pin(bool),
    Size(i32, i32),
    Move(i32, i32),
    /// `+pop` / `-pop`.
    Tag(bool),
    Raise,
    /// Leave maximized (Hyprland reports it as fullscreen mode 1; a maximized window can't be
    /// pinned). Lua only: the classic `fullscreenstate` acts on the focused window.
    Unmaximize,
}

impl Cmd {
    fn lua(self, addr: &str) -> String {
        let w = format!("window = \"address:{addr}\"");
        let on = |b: bool| if b { "enable" } else { "disable" };
        match self {
            Cmd::Float(b) => format!("hl.dsp.window.float({{ {w}, action = \"{}\" }})", on(b)),
            Cmd::Pin(b) => format!("hl.dsp.window.pin({{ {w}, action = \"{}\" }})", on(b)),
            Cmd::Size(x, y) => format!("hl.dsp.window.resize({{ {w}, x = {x}, y = {y} }})"),
            Cmd::Move(x, y) => format!("hl.dsp.window.move({{ {w}, x = {x}, y = {y} }})"),
            Cmd::Tag(b) => format!("hl.dsp.window.tag({{ {w}, tag = \"{}pop\" }})", if b { '+' } else { '-' }),
            Cmd::Raise => format!("hl.dsp.window.alter_zorder({{ {w}, mode = \"top\" }})"),
            Cmd::Unmaximize => format!("hl.dsp.window.fullscreen({{ {w}, mode = \"maximized\", action = \"unset\" }})"),
        }
    }

    /// `None`: no classic dispatcher takes a window for this.
    fn classic(self, addr: &str) -> Option<String> {
        Some(match self {
            Cmd::Float(true) => format!("setfloating address:{addr}"),
            Cmd::Float(false) => format!("settiled address:{addr}"),
            Cmd::Pin(_) => format!("pin address:{addr}"),
            Cmd::Size(x, y) => format!("resizewindowpixel exact {x} {y},address:{addr}"),
            Cmd::Move(x, y) => format!("movewindowpixel exact {x} {y},address:{addr}"),
            Cmd::Tag(b) => format!("tagwindow {}pop address:{addr}", if b { '+' } else { '-' }),
            Cmd::Raise => format!("alterzorder top,address:{addr}"),
            Cmd::Unmaximize => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Unknown,
    Lua,
    Classic,
}

/// What pop-out changed, to undo on pop-in, and the default place it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    was_floating: bool,
    was_pinned: bool,
    had_tag: bool,
    /// Floating geometry before (only used when it was floating).
    before: Rect,
    size: PopoutSize,
    corner: PopoutCorner,
}

/// What to do with the remembered pop-out place (`PlayerConfig::popout_last`) after a pop-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// The user moved or resized our pop-out: reuse this (`[x, y, w, h]`, monitor-relative).
    Remember([i32; 4]),
    /// Our pop-out sat at its default place: follow the corner next time.
    Forget,
    /// Not our pop-out (Super+O), or no longer floating + pinned: leave it as it is.
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopError {
    /// Our window is not (yet) mapped.
    NoWindow,
    /// Hyprland can't pin or unpin a fullscreen window.
    Fullscreen,
    /// Maximized, and Hyprland's config (classic syntax) can't un-maximize it by address.
    Maximized,
    Ipc(String),
}

impl std::fmt::Display for PopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PopError::NoWindow => f.write_str("Hyprland doesn't list Tuner's window"),
            PopError::Fullscreen => f.write_str("the window is still fullscreen"),
            PopError::Maximized => f.write_str("the window is maximized (Super+F)"),
            PopError::Ipc(e) => f.write_str(e),
        }
    }
}

impl From<String> for PopError {
    fn from(e: String) -> Self {
        PopError::Ipc(e)
    }
}

/// Hyprland commands for our window.
pub struct Hypr<I: HyprIpc> {
    ipc: I,
    syntax: Syntax,
    /// Until the first pop-out: the window was started for the pop-out (`POPOUT_APP_ID`), so a
    /// floating, pinned window is the launch rule's doing, not the user's.
    launch_rule: bool,
}

impl<I: HyprIpc> Hypr<I> {
    pub fn new(ipc: I) -> Self {
        Self { ipc, syntax: Syntax::Unknown, launch_rule: false }
    }

    /// The window was started for the pop-out under [`crate::POPOUT_APP_ID`]: the packaged
    /// rule (`packaging/hypr/tuner.lua`) maps it floating and pinned, without the keyboard. The
    /// first pop-out takes that state as its own, so popping in tiles the window (instead of
    /// keeping it floating and pinned as if the user had done that).
    pub fn with_launch_rule(mut self) -> Self {
        self.launch_rule = true;
        self
    }

    fn json<T: serde::de::DeserializeOwned>(&mut self, what: &str) -> Result<T, String> {
        let reply = self.ipc.request(&format!("j/{what}"))?;
        serde_json::from_str(&reply).map_err(|e| format!("Hyprland {what}: {e}"))
    }

    /// Our mapped window (by pid), with a well-formed address.
    pub fn our_client(&mut self, pid: u32) -> Result<Option<Client>, String> {
        let clients: Vec<Client> = self.json("clients")?;
        Ok(clients.into_iter().find(|c| c.pid == i64::from(pid) && c.mapped && valid_address(&c.address)))
    }

    fn monitor(&mut self, id: i64) -> Result<Monitor, String> {
        let monitors: Vec<Monitor> = self.json("monitors")?;
        let first = monitors.first().cloned();
        monitors.into_iter().find(|m| m.id == id).or(first).ok_or_else(|| "Hyprland lists no monitor".to_owned())
    }

    /// One dispatcher: Lua first, then classic, until one syntax has worked (then only it).
    fn dispatch(&mut self, cmd: Cmd, addr: &str) -> Result<(), String> {
        let (lua, classic) = (cmd.lua(addr), cmd.classic(addr));
        let mut send = |req: &str| -> Result<bool, String> { Ok(self.ipc.request(&format!("/dispatch {req}"))?.trim() == "ok") };
        let refused = |req: &str| format!("Hyprland refused {req}");
        match (self.syntax, classic) {
            (Syntax::Lua, _) => send(&lua)?.then_some(()).ok_or_else(|| refused(&lua)),
            (Syntax::Classic, Some(c)) => send(&c)?.then_some(()).ok_or_else(|| refused(&c)),
            (Syntax::Classic, None) => Err(format!("no classic dispatcher for {cmd:?}")),
            (Syntax::Unknown, classic) => {
                if send(&lua)? {
                    self.syntax = Syntax::Lua;
                } else if let Some(c) = classic {
                    if !send(&c)? {
                        return Err(refused(&c));
                    }
                    self.syntax = Syntax::Classic;
                } else {
                    return Err(refused(&lua));
                }
                Ok(())
            }
        }
    }

    /// Float, size, place, pin, raise and tag our window; returns what to undo. A refused
    /// step undoes the steps before it (no half popped-out window, no wrong record).
    pub fn pop_out(&mut self, pid: u32, size: PopoutSize, corner: PopoutCorner, last: Option<[i32; 4]>) -> Result<Restore, PopError> {
        let mut c = self.our_client(pid)?.ok_or(PopError::NoWindow)?;
        match c.fullscreen {
            0 => {}
            1 => {
                self.dispatch(Cmd::Unmaximize, &c.address).map_err(|_| PopError::Maximized)?;
                c = self.our_client(pid)?.ok_or(PopError::NoWindow)?;
            }
            _ => return Err(PopError::Fullscreen),
        }
        // floating + pinned by the launch rule (never tagged by it): popped in, it tiles
        let launched = std::mem::take(&mut self.launch_rule) && c.floating && c.pinned && !c.has_tag("pop");
        let m = self.monitor(c.monitor)?;
        let r = place(&m, size, corner, last);
        let restore = Restore {
            was_floating: c.floating && !launched,
            was_pinned: c.pinned && !launched,
            had_tag: c.has_tag("pop"),
            before: Rect { x: c.at[0], y: c.at[1], w: c.size[0], h: c.size[1] },
            size,
            corner,
        };
        if let Err(e) = self.float_place_pin(&c, r, !launched) {
            if let Err(undo) = self.pop_in(pid, Some(&restore)) {
                tracing::warn!("pop-out: undoing a refused pop-out: {undo}");
            }
            return Err(e.into());
        }
        Ok(restore)
    }

    /// None of these steps moves the keyboard focus (Hyprland 0.56's `Actions::floatWindow`,
    /// `pinWindow`, `resize`, `move`, `tag`). `alter_zorder` alone may: after raising it
    /// simulates a pointer motion, and with `follow_mouse = 1` (Omarchy's) a pointer that just
    /// moved over the raised window gives it the keyboard. So it is sent only when needed: a
    /// window Hyprland floats is raised by that already (0.56 `floatWindow`, and
    /// `changeWindowFloatingMode` before it), and a freshly mapped window (`raise` = false: the
    /// launch rule floated it) is on top anyway.
    fn float_place_pin(&mut self, c: &Client, r: Rect, raise: bool) -> Result<(), String> {
        let a = c.address.as_str();
        if !c.floating {
            self.dispatch(Cmd::Float(true), a)?;
        }
        self.dispatch(Cmd::Size(r.w, r.h), a)?;
        self.dispatch(Cmd::Move(r.x, r.y), a)?;
        if !c.pinned {
            self.dispatch(Cmd::Pin(true), a)?;
        }
        if c.floating && raise {
            self.dispatch(Cmd::Raise, a)?;
        }
        if !c.has_tag("pop") {
            self.dispatch(Cmd::Tag(true), a)?;
        }
        Ok(())
    }

    /// Developer screenshots (`devshot`): float our window at `w`×`h` logical px, centred in
    /// its monitor's usable area.
    pub fn float_centred(&mut self, pid: u32, w: i32, h: i32) -> Result<(), PopError> {
        let c = self.our_client(pid)?.ok_or(PopError::NoWindow)?;
        let area = self.monitor(c.monitor)?.usable();
        let a = c.address.as_str();
        if !c.floating {
            self.dispatch(Cmd::Float(true), a)?;
        }
        self.dispatch(Cmd::Size(w, h), a)?;
        self.dispatch(Cmd::Move(area.x + (area.w - w).max(0) / 2, area.y + (area.h - h).max(0) / 2), a)?;
        Ok(())
    }

    /// Undo a pop-out (`restore` = our record; `None` = undo Super+O); says what to do with
    /// the remembered place.
    pub fn pop_in(&mut self, pid: u32, restore: Option<&Restore>) -> Result<Place, PopError> {
        let Some(c) = self.our_client(pid)? else { return Ok(Place::Keep) };
        match c.fullscreen {
            0 => {}
            1 => self.dispatch(Cmd::Unmaximize, &c.address).map_err(|_| PopError::Maximized)?,
            _ => return Err(PopError::Fullscreen),
        }
        let a = c.address.as_str();
        let remember = match restore {
            Some(r) if c.floating && c.pinned => {
                let m = self.monitor(c.monitor)?;
                let home = place(&m, r.size, r.corner, None);
                let now = Rect { x: c.at[0], y: c.at[1], w: c.size[0], h: c.size[1] };
                let moved = [now.x - home.x, now.y - home.y, now.w - home.w, now.h - home.h].iter().any(|d| d.abs() > MOVED_SLACK);
                if moved { Place::Remember([now.x - m.x, now.y - m.y, now.w, now.h]) } else { Place::Forget }
            }
            _ => Place::Keep,
        };
        if c.pinned && !restore.is_some_and(|r| r.was_pinned) {
            self.dispatch(Cmd::Pin(false), a)?;
        }
        if c.has_tag("pop") && !restore.is_some_and(|r| r.had_tag) {
            self.dispatch(Cmd::Tag(false), a)?;
        }
        match restore {
            Some(r) if r.was_floating => {
                self.dispatch(Cmd::Size(r.before.w, r.before.h), a)?;
                self.dispatch(Cmd::Move(r.before.x, r.before.y), a)?;
            }
            _ if c.floating => self.dispatch(Cmd::Float(false), a)?,
            _ => {}
        }
        Ok(remember)
    }
}

/// `0x` + hex digits only (it is interpolated into Lua and dispatcher arguments).
fn valid_address(a: &str) -> bool {
    a.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Floating / pinned state of our window as Hyprland reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowFlags {
    pub floating: bool,
    pub pinned: bool,
}

/// What the pop-out threads tell the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum PopOutEvent {
    /// Our window's floating / pinned state changed (any cause: us, Super+O, a mouse drag).
    Flags(WindowFlags),
    PoppedOut,
    /// `place`: what to do with the remembered pop-out place (`PlayerConfig::popout_last`).
    PoppedIn {
        place: Place,
    },
    /// `popping_out`: the failed request was a pop-out (else a pop-in).
    Failed {
        popping_out: bool,
        error: PopError,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PopRequest {
    Out {
        size: PopoutSize,
        corner: PopoutCorner,
        last: Option<[i32; 4]>,
    },
    /// `ours`: the window is popped out by us (undo our record) rather than by Super+O.
    In {
        ours: bool,
    },
}

/// Where the app sends pop-out requests (the worker thread; a recorder in tests).
pub trait PopOutBackend {
    fn send(&self, req: PopRequest);
}

/// The `hypr-popout` worker (requests) plus the `hypr-events` watcher (window state).
pub struct PopOutWorker {
    tx: crossbeam_channel::Sender<PopRequest>,
}

impl PopOutWorker {
    /// Under Hyprland: start both threads for our own window. `None` outside Hyprland.
    /// `launched`: the window was started for the pop-out ([`Hypr::with_launch_rule`]).
    pub fn start(bus: EventBus, launched: bool) -> Option<Self> {
        let ipc = SocketIpc::from_env()?;
        let pid = std::process::id();
        let (tx, rx) = crossbeam_channel::unbounded::<PopRequest>();
        let worker_bus = bus.clone();
        let worker_ipc = ipc.clone();
        let spawned = std::thread::Builder::new().name("hypr-popout".into()).spawn(move || {
            let hypr = Hypr::new(worker_ipc);
            serve(if launched { hypr.with_launch_rule() } else { hypr }, &rx, &worker_bus, pid)
        });
        if let Err(e) = spawned {
            tracing::warn!("pop-out worker: {e}");
            return None;
        }
        if let Err(e) = std::thread::Builder::new().name("hypr-events".into()).spawn(move || watch(ipc, &bus, pid)) {
            tracing::warn!("Hyprland event watcher: {e}");
        }
        Some(Self { tx })
    }
}

impl PopOutBackend for PopOutWorker {
    fn send(&self, req: PopRequest) {
        let _ = self.tx.send(req);
    }
}

fn serve<I: HyprIpc>(mut hypr: Hypr<I>, rx: &crossbeam_channel::Receiver<PopRequest>, bus: &EventBus, pid: u32) {
    let mut restore: Option<Restore> = None;
    for req in rx.iter() {
        let ev = match req {
            PopRequest::Out { size, corner, last } => match when_ready(true, || hypr.pop_out(pid, size, corner, last)) {
                Ok(r) => {
                    restore = Some(r);
                    PopOutEvent::PoppedOut
                }
                Err(error) => PopOutEvent::Failed { popping_out: true, error },
            },
            PopRequest::In { ours } => {
                let record = restore.take().filter(|_| ours);
                match when_ready(false, || hypr.pop_in(pid, record.as_ref())) {
                    Ok(place) => PopOutEvent::PoppedIn { place },
                    Err(error) => {
                        // still popped out: the next pop-in undoes the same record
                        restore = record;
                        PopOutEvent::Failed { popping_out: false, error }
                    }
                }
            }
        };
        tracing::debug!("pop-out: {ev:?}");
        bus.send(AppEvent::Platform(PlatformEvent::PopOut(ev)));
    }
}

/// Run `step`, retrying while the window is fullscreen (the app left window fullscreen just
/// before the request and Hyprland may not have seen it yet) and, with `mapping`, while
/// Hyprland doesn't list it yet (a pop-out asked for on the first frame of a new window).
fn when_ready<T>(mapping: bool, mut step: impl FnMut() -> Result<T, PopError>) -> Result<T, PopError> {
    let (mut fullscreen, mut unmapped) = (0, 0);
    loop {
        let result = step();
        let tries = match &result {
            Err(PopError::Fullscreen) => Some((&mut fullscreen, FULLSCREEN_TRIES)),
            Err(PopError::NoWindow) if mapping => Some((&mut unmapped, MAP_TRIES)),
            _ => None,
        };
        match tries {
            Some((n, max)) if *n < max => *n += 1,
            _ => return result,
        }
        std::thread::sleep(READY_WAIT);
    }
}

/// Follows our window through Hyprland's event stream.
#[derive(Debug, Default)]
struct WindowTracker {
    /// Our window's address without `0x` (the event stream's spelling).
    address: Option<String>,
    flags: WindowFlags,
}

enum Track {
    Changed(WindowFlags),
    /// A window opened while ours is unknown: look ours up again.
    Resync,
    Nothing,
}

impl WindowTracker {
    fn on_line(&mut self, line: &str) -> Track {
        let Some((event, data)) = line.split_once(">>") else { return Track::Nothing };
        let ours = |addr: &str| self.address.as_deref() == Some(addr.trim_start_matches("0x"));
        let before = self.flags;
        match event {
            "openwindow" if self.address.is_none() => return Track::Resync,
            "changefloatingmode" | "pin" => {
                let Some((addr, value)) = data.split_once(',') else { return Track::Nothing };
                if !ours(addr) {
                    return Track::Nothing;
                }
                let on = value.trim() == "1";
                if event == "pin" {
                    self.flags.pinned = on;
                } else {
                    self.flags.floating = on;
                }
            }
            "closewindow" if ours(data) => {
                self.address = None;
                self.flags = WindowFlags::default();
            }
            _ => return Track::Nothing,
        }
        if self.flags != before { Track::Changed(self.flags) } else { Track::Nothing }
    }

    /// Adopt a fresh `j/clients` view of our window.
    fn resync(&mut self, client: Option<&Client>) -> Option<WindowFlags> {
        let before = self.flags;
        self.address = client.map(|c| c.address.trim_start_matches("0x").to_owned());
        self.flags = client.map(|c| WindowFlags { floating: c.floating, pinned: c.pinned }).unwrap_or_default();
        (self.flags != before).then_some(self.flags)
    }
}

fn watch(ipc: SocketIpc, bus: &EventBus, pid: u32) {
    let stream = match ipc.events() {
        Ok(s) => s,
        Err(e) => {
            tracing::info!("Hyprland events unavailable ({e}); Super+O won't switch to the video-only player");
            return;
        }
    };
    let mut hypr = Hypr::new(ipc);
    let mut tracker = WindowTracker::default();
    let send = |f: WindowFlags| bus.send(AppEvent::Platform(PlatformEvent::PopOut(PopOutEvent::Flags(f))));
    let mut resync = |tracker: &mut WindowTracker| match hypr.our_client(pid) {
        Ok(c) => {
            if let Some(f) = tracker.resync(c.as_ref()) {
                send(f);
            }
        }
        Err(e) => tracing::debug!("Hyprland clients: {e}"),
    };
    resync(&mut tracker);
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        match tracker.on_line(&line) {
            Track::Changed(f) => send(f),
            Track::Resync => resync(&mut tracker),
            Track::Nothing => {}
        }
    }
    tracing::debug!("Hyprland event stream closed");
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Answers `j/clients` / `j/monitors` from a fake window table and `/dispatch` with `ok`
    /// for the configured syntax (Lua or classic), applying the dispatchers to the table like
    /// Hyprland would, and records every dispatch.
    #[derive(Clone)]
    struct FakeHypr {
        lua: bool,
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        clients: Vec<serde_json::Value>,
        dispatched: Vec<String>,
        /// Replies to force for the next dispatches (e.g. a refusal).
        forced: VecDeque<String>,
        /// Dispatches containing one of these are refused (once each).
        refuse: Vec<&'static str>,
    }

    const MONITORS: &str = r#"[{"id": 0, "x": 0, "y": 0, "width": 2560, "height": 1600, "scale": 1.3333334,
        "transform": 0, "reserved": [0, 26, 0, 0]}]"#;

    impl FakeHypr {
        fn new(lua: bool, ours: serde_json::Value) -> Self {
            let other = serde_json::json!({"address": "0xbbb", "pid": 1, "mapped": true, "floating": false, "pinned": false,
                "at": [0, 0], "size": [10, 10], "monitor": 0, "fullscreen": 0, "tags": []});
            Self {
                lua,
                state: Arc::new(Mutex::new(FakeState { clients: vec![other, ours], dispatched: vec![], forced: VecDeque::new(), refuse: vec![] })),
            }
        }
        fn dispatched(&self) -> Vec<String> {
            self.state.lock().unwrap().dispatched.clone()
        }
        fn ours(&self) -> serde_json::Value {
            self.state.lock().unwrap().clients[1].clone()
        }
    }

    impl HyprIpc for FakeHypr {
        fn request(&mut self, req: &str) -> Result<String, String> {
            let mut st = self.state.lock().unwrap();
            match req {
                "j/clients" => return Ok(serde_json::to_string(&st.clients).unwrap()),
                "j/monitors" => return Ok(MONITORS.into()),
                _ => {}
            }
            let cmd = req.strip_prefix("/dispatch ").expect("only dispatches besides queries").to_owned();
            if let Some(forced) = st.forced.pop_front() {
                st.dispatched.push(cmd);
                return Ok(forced);
            }
            let is_lua = cmd.starts_with("hl.dsp.");
            if is_lua != self.lua {
                return Ok(if is_lua { "Invalid dispatcher".into() } else { "[string]: attempt to call a nil value".into() });
            }
            st.dispatched.push(cmd.clone());
            if let Some(i) = st.refuse.iter().position(|r| cmd.contains(r)) {
                st.refuse.remove(i);
                return Ok("Window does not qualify".into());
            }
            let w = &mut st.clients[1];
            let num = |s: &str, key: &str| -> i32 {
                let rest = &s[s.find(key).unwrap() + key.len()..];
                rest.trim_start().split(|c: char| !(c == '-' || c.is_ascii_digit())).find(|t| !t.is_empty()).unwrap().parse().unwrap()
            };
            let words: Vec<&str> = cmd.split([' ', ',']).collect();
            if cmd.contains("window.fullscreen") {
                w["fullscreen"] = 0.into();
            } else if cmd.contains("window.float") || cmd.starts_with("setfloating") || cmd.starts_with("settiled") {
                w["floating"] = (cmd.contains("enable") || cmd.starts_with("setfloating")).into();
            } else if cmd.contains("window.pin") || cmd.starts_with("pin ") {
                let now = w["pinned"].as_bool().unwrap();
                w["pinned"] = if self.lua { cmd.contains("enable") } else { !now }.into();
            } else if cmd.contains("window.resize") {
                w["size"] = serde_json::json!([num(&cmd, "x ="), num(&cmd, "y =")]);
            } else if cmd.starts_with("resizewindowpixel") {
                w["size"] = serde_json::json!([words[2].parse::<i32>().unwrap(), words[3].parse::<i32>().unwrap()]);
            } else if cmd.contains("window.move") {
                w["at"] = serde_json::json!([num(&cmd, "x ="), num(&cmd, "y =")]);
            } else if cmd.starts_with("movewindowpixel") {
                w["at"] = serde_json::json!([words[2].parse::<i32>().unwrap(), words[3].parse::<i32>().unwrap()]);
            } else if cmd.contains("pop") {
                let add = cmd.contains("+pop");
                let tags = w["tags"].as_array_mut().unwrap();
                tags.retain(|t| t != "pop");
                if add {
                    tags.push("pop".into());
                }
            }
            Ok("ok".into())
        }
    }

    fn tiled(pid: u32) -> serde_json::Value {
        serde_json::json!({"address": "0xaaab07400000", "pid": pid, "mapped": true, "floating": false, "pinned": false,
            "at": [12, 38], "size": [1896, 1150], "monitor": 0, "fullscreen": 0, "tags": ["default-opacity*"]})
    }

    const PID: u32 = 4242;
    const W: &str = "address:0xaaab07400000";

    /// Lua config (Omarchy 4): float, 480×270 at the bottom-right of the 1920×1200 logical
    /// monitor (bar 26 px, margin 10), pin, tag — no `alter_zorder`: floating raised it, and
    /// that dispatcher's simulated pointer motion could hand it the keyboard; pop-in undoes
    /// exactly that. An unmoved pop-out's place isn't remembered (it follows the corner); a
    /// moved one's is.
    #[test]
    fn lua_pop_out_and_back_restores_the_tiled_window() {
        let fake = FakeHypr::new(true, tiled(PID));
        let mut h = Hypr::new(fake.clone());
        let restore = h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        assert_eq!(
            fake.dispatched(),
            [
                format!("hl.dsp.window.float({{ window = \"{W}\", action = \"enable\" }})"),
                format!("hl.dsp.window.resize({{ window = \"{W}\", x = 480, y = 270 }})"),
                format!("hl.dsp.window.move({{ window = \"{W}\", x = 1430, y = 920 }})"),
                format!("hl.dsp.window.pin({{ window = \"{W}\", action = \"enable\" }})"),
                format!("hl.dsp.window.tag({{ window = \"{W}\", tag = \"+pop\" }})"),
            ]
        );
        let w = fake.ours();
        assert_eq!((w["floating"].as_bool(), w["pinned"].as_bool()), (Some(true), Some(true)));

        assert_eq!(h.pop_in(PID, Some(&restore)), Ok(Place::Forget), "unmoved: follow the corner");
        assert_eq!(
            fake.dispatched()[5..],
            [
                format!("hl.dsp.window.pin({{ window = \"{W}\", action = \"disable\" }})"),
                format!("hl.dsp.window.tag({{ window = \"{W}\", tag = \"-pop\" }})"),
                format!("hl.dsp.window.float({{ window = \"{W}\", action = \"disable\" }})"),
            ]
        );
        let w = fake.ours();
        assert_eq!((w["floating"].as_bool(), w["pinned"].as_bool(), w["tags"].as_array().unwrap().len()), (Some(false), Some(false), 1));

        let restore = h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        fake.state.lock().unwrap().clients[1]["at"] = serde_json::json!([100, 500]); // Super+drag
        assert_eq!(h.pop_in(PID, Some(&restore)), Ok(Place::Remember([100, 500, 480, 270])));
    }

    /// Classic hyprlang config: the first Lua attempt is refused, the classic form works and is
    /// used from then on (no Lua attempt per command). The remembered geometry is reused,
    /// clamped into the usable area (below the bar).
    #[test]
    fn classic_fallback_is_learned_once_and_remembered_place_is_clamped() {
        let fake = FakeHypr::new(false, tiled(PID));
        let mut h = Hypr::new(fake.clone());
        h.pop_out(PID, PopoutSize::Medium, PopoutCorner::TopLeft, Some([-50, 0, 640, 360])).unwrap();
        assert_eq!(
            fake.dispatched(),
            [
                format!("setfloating {W}"),
                format!("resizewindowpixel exact 640 360,{W}"),
                format!("movewindowpixel exact 0 26,{W}"),
                format!("pin {W}"),
                format!("tagwindow +pop {W}"),
            ]
        );
    }

    /// A window that was already floating comes back to its own floating geometry (never
    /// tiled); a window popped out by Super+O (no record) is unpinned, untagged and tiled.
    #[test]
    fn pop_in_restores_floating_geometry_or_undoes_super_o() {
        let mut floating = tiled(PID);
        floating["floating"] = true.into();
        floating["at"] = serde_json::json!([300, 200]);
        floating["size"] = serde_json::json!([1000, 700]);
        let fake = FakeHypr::new(true, floating);
        let mut h = Hypr::new(fake.clone());
        let restore = h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomLeft, None).unwrap();
        assert!(!fake.dispatched()[0].contains("window.float"), "already floating: {:?}", fake.dispatched());
        assert!(fake.dispatched().iter().any(|d| d.contains("alter_zorder")), "nothing raised it: raise it");
        h.pop_in(PID, Some(&restore)).unwrap();
        let back = fake.dispatched();
        assert_eq!(
            back[back.len() - 2..],
            [
                format!("hl.dsp.window.resize({{ window = \"{W}\", x = 1000, y = 700 }})"),
                format!("hl.dsp.window.move({{ window = \"{W}\", x = 300, y = 200 }})"),
            ]
        );
        assert_eq!(fake.ours()["floating"].as_bool(), Some(true));

        // Super+O state: floating, pinned, tagged pop — no record of ours
        let mut popped = tiled(PID);
        popped["floating"] = true.into();
        popped["pinned"] = true.into();
        popped["tags"] = serde_json::json!(["default-opacity*", "pop"]);
        let fake = FakeHypr::new(true, popped);
        assert_eq!(Hypr::new(fake.clone()).pop_in(PID, None), Ok(Place::Keep), "only our own pop-out's place is remembered");
        assert_eq!(
            fake.dispatched(),
            [
                format!("hl.dsp.window.pin({{ window = \"{W}\", action = \"disable\" }})"),
                format!("hl.dsp.window.tag({{ window = \"{W}\", tag = \"-pop\" }})"),
                format!("hl.dsp.window.float({{ window = \"{W}\", action = \"disable\" }})"),
            ]
        );
    }

    /// Nothing is dispatched for a fullscreen window, out or in (the worker retries once the
    /// app's unfullscreen landed), without our window, or when Hyprland refuses both syntaxes;
    /// a step refused halfway undoes the steps before it.
    #[test]
    fn refusals_dispatch_nothing_further() {
        let mut fs = tiled(PID);
        fs["fullscreen"] = 2.into();
        let fake = FakeHypr::new(true, fs);
        assert_eq!(Hypr::new(fake.clone()).pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None), Err(PopError::Fullscreen));
        assert_eq!(Hypr::new(fake.clone()).pop_out(7, PopoutSize::Small, PopoutCorner::BottomRight, None), Err(PopError::NoWindow));
        let fake_in = FakeHypr::new(true, tiled(PID));
        let restore = Hypr::new(fake_in.clone()).pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        fake_in.state.lock().unwrap().clients[1]["fullscreen"] = 2.into();
        let sent = fake_in.dispatched().len();
        assert_eq!(Hypr::new(fake_in.clone()).pop_in(PID, Some(&restore)), Err(PopError::Fullscreen));
        assert_eq!(fake_in.dispatched().len(), sent, "no unpin of a fullscreen window");
        assert!(fake.dispatched().is_empty());

        let fake = FakeHypr::new(true, tiled(PID));
        fake.state.lock().unwrap().forced.extend(["Window does not qualify".to_owned(), "Invalid dispatcher".to_owned()]);
        let err = Hypr::new(fake.clone()).pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap_err();
        assert!(matches!(err, PopError::Ipc(_)), "{err:?}");
        assert_eq!(fake.dispatched().len(), 2, "stops at the first refused step: {:?}", fake.dispatched());

        // pin refused after float, size and move: back to tiled, nothing recorded
        let fake = FakeHypr::new(true, tiled(PID));
        fake.state.lock().unwrap().refuse.push("window.pin");
        let mut h = Hypr::new(fake.clone());
        assert!(matches!(h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None), Err(PopError::Ipc(_))));
        assert_eq!(fake.dispatched().last(), Some(&format!("hl.dsp.window.float({{ window = \"{W}\", action = \"disable\" }})")));
        assert_eq!((fake.ours()["floating"].as_bool(), fake.ours()["pinned"].as_bool()), (Some(false), Some(false)));

        // maximized: un-maximized by address (Lua), but classic has no dispatcher for that
        let mut max = tiled(PID);
        max["fullscreen"] = 1.into();
        let fake = FakeHypr::new(true, max.clone());
        Hypr::new(fake.clone()).pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        assert_eq!(fake.dispatched()[0], format!("hl.dsp.window.fullscreen({{ window = \"{W}\", mode = \"maximized\", action = \"unset\" }})"));
        let fake = FakeHypr::new(false, max);
        assert_eq!(Hypr::new(fake.clone()).pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None), Err(PopError::Maximized));
        assert!(fake.dispatched().is_empty());
    }

    /// A window started for the pop-out (`tuner-popout`) is mapped floating and pinned by the
    /// packaged rule. Its first pop-out only sizes, places and tags it (no float, no pin, no
    /// raise: nothing that could move the keyboard) and takes the floating + pinned state as its
    /// own, so popping in tiles it. The flag survives a pop-out asked before the window was
    /// mapped and is used once: the next pop-out of the now tiled window is the usual one.
    /// Without the flag the same window is the user's floating + pinned window, kept as it is.
    #[test]
    fn launch_rule_window_is_adopted_by_its_first_pop_out() {
        let mut ruled = tiled(PID);
        ruled["floating"] = true.into();
        ruled["pinned"] = true.into();
        ruled["at"] = serde_json::json!([720, 465]);
        ruled["size"] = serde_json::json!([480, 270]);
        let fake = FakeHypr::new(true, ruled.clone());
        let mut h = Hypr::new(fake.clone()).with_launch_rule();
        assert_eq!(h.pop_out(7, PopoutSize::Small, PopoutCorner::BottomRight, None), Err(PopError::NoWindow), "not mapped yet");
        let restore = h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        assert_eq!(
            fake.dispatched(),
            [
                format!("hl.dsp.window.resize({{ window = \"{W}\", x = 480, y = 270 }})"),
                format!("hl.dsp.window.move({{ window = \"{W}\", x = 1430, y = 920 }})"),
                format!("hl.dsp.window.tag({{ window = \"{W}\", tag = \"+pop\" }})"),
            ]
        );
        assert_eq!(h.pop_in(PID, Some(&restore)), Ok(Place::Forget));
        assert_eq!(
            fake.dispatched()[3..],
            [
                format!("hl.dsp.window.pin({{ window = \"{W}\", action = \"disable\" }})"),
                format!("hl.dsp.window.tag({{ window = \"{W}\", tag = \"-pop\" }})"),
                format!("hl.dsp.window.float({{ window = \"{W}\", action = \"disable\" }})"),
            ]
        );
        let w = fake.ours();
        assert_eq!((w["floating"].as_bool(), w["pinned"].as_bool()), (Some(false), Some(false)), "tiled like any window");
        h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        let sent = fake.dispatched();
        assert!(sent[6].contains("window.float") && sent[9].contains("window.pin"), "the usual pop-out: {sent:?}");

        // no launch flag: the user's own floating + pinned window stays so when popped in
        let fake = FakeHypr::new(true, ruled);
        let mut h = Hypr::new(fake.clone());
        let restore = h.pop_out(PID, PopoutSize::Small, PopoutCorner::BottomRight, None).unwrap();
        h.pop_in(PID, Some(&restore)).unwrap();
        let w = fake.ours();
        assert_eq!((w["floating"].as_bool(), w["pinned"].as_bool()), (Some(true), Some(true)));
        let sent = fake.dispatched();
        assert!(!sent.iter().any(|d| d.contains("window.pin") || d.contains("action = \"disable\"")), "{sent:?}");
    }

    /// The worker waits for a window that isn't mapped yet only when popping out (a pop-in
    /// without a window has nothing to do), and stops waiting once the step answers.
    #[test]
    fn pop_out_waits_for_the_window_to_map() {
        let mut calls = 0;
        let r = when_ready(true, || {
            calls += 1;
            if calls < 3 { Err(PopError::NoWindow) } else { Ok(calls) }
        });
        assert_eq!(r, Ok(3));
        let mut calls = 0;
        let r: Result<(), _> = when_ready(false, || {
            calls += 1;
            Err(PopError::NoWindow)
        });
        assert_eq!((r, calls), (Err(PopError::NoWindow), 1));
        let mut calls = 0;
        let r: Result<(), _> = when_ready(true, || {
            calls += 1;
            Err(PopError::Ipc("refused".into()))
        });
        assert_eq!(calls, 1, "{r:?}");
    }

    /// The event stream drives the floating/pinned flags of OUR window only.
    #[test]
    fn tracker_follows_only_our_window() {
        let mut t = WindowTracker::default();
        assert!(matches!(t.on_line("openwindow>>aaab07400000,1,tuner,Tuner"), Track::Resync), "ours unknown: look it up");
        let ours: Client = serde_json::from_value(tiled(PID)).unwrap();
        assert_eq!(t.resync(Some(&ours)), None, "tiled = default flags");
        assert!(matches!(t.on_line("openwindow>>bbb,1,foot,foot"), Track::Nothing));
        assert!(matches!(t.on_line("pin>>bbb,1"), Track::Nothing), "another window");
        assert!(matches!(t.on_line("changefloatingmode>>aaab07400000,1"), Track::Changed(WindowFlags { floating: true, pinned: false })));
        assert!(matches!(t.on_line("pin>>aaab07400000,1"), Track::Changed(WindowFlags { floating: true, pinned: true })));
        assert!(matches!(t.on_line("pin>>aaab07400000,1"), Track::Nothing), "no change");
        assert!(matches!(t.on_line("closewindow>>aaab07400000"), Track::Changed(WindowFlags { floating: false, pinned: false })));
    }
}
