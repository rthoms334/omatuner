//! The single video "screen" shown in different places (FIXED DECISION 3): Live preview pane,
//! fullscreen player, corner mini-player. Owner: WP-11 (render path implemented in the
//! skeleton from the spike).
//!
//! Per frame: screens call [`VideoHost::paint`] for the rect where video belongs (at most one
//! placement per frame; the last call wins). If no screen placed the video while something is
//! playing and the screen allows it, the app draws the mini-player (bottom-right, 16:9, width
//! 320 pt Wide / 240 pt Medium, hidden in Compact — there the status bar shows a now-playing
//! chip instead; click = open player, ✕ = stop). Toasts stack ABOVE the mini-player rect
//! ([`VideoHost::mini_rect`]); screens get it as `UiCtx::mini_rect`.
//! Without a video surface (libmpv/GL failed) every placement shows "Video unavailable —
//! libmpv failed to start (see Logs)"; browsing still works.
//! `App::logic` then calls [`VideoHost::render`] next frame with the requested physical size
//! (one-frame resize lag, invisible in practice; spike-verified resize path).
//! Overlays inside the video rect use the FIXED dark scrim (never theme colours): Loading
//! (channel logo + name + spinner), Buffering (percent), Reconnecting (attempt n/max +
//! countdown), Error (glyph + message + "r Retry · Backspace Last channel"), Radio (logo +
//! now playing instead of black).

use eframe::egui;
use eframe::glow;

use crate::player::{PlayerSnapshot, VideoSurface};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    None,
    Preview,
    Fullscreen,
    Mini,
}

#[derive(Default)]
pub struct VideoHost {
    surface: Option<VideoSurface>,
    placed: Placement,
    placed_this_frame: Placement,
    last_generation: u64,
    mini: Option<egui::Rect>,
}

impl VideoHost {
    pub fn new(surface: Option<VideoSurface>) -> Self { Self { surface, ..Default::default() } }

    /// Start of frame bookkeeping (app).
    pub fn begin_frame(&mut self) {
        self.placed = self.placed_this_frame;
        self.placed_this_frame = Placement::None;
    }

    /// Where the video was shown last frame.
    pub fn placement(&self) -> Placement { self.placed }

    /// False when libmpv/GL failed at startup (no surface).
    pub fn available(&self) -> bool { self.surface.is_some() }

    /// The mini-player rect drawn last frame (toasts and the guide avoid it).
    pub fn mini_rect(&self) -> Option<egui::Rect> { self.mini }

    /// Render mpv into the FBO (from `App::logic`, GL current). Clears to black on new loads.
    pub fn render(&mut self, gl: &glow::Context, frame: &mut eframe::Frame, snap: &PlayerSnapshot) {
        let Some(s) = &mut self.surface else { return };
        if snap.load_generation != self.last_generation {
            self.last_generation = snap.load_generation;
            s.reset_for_new_file();
        }
        s.render(gl, frame);
    }

    /// Paint the video into `rect` (mpv draws black bars), plus the state overlay. Returns
    /// the response over the whole rect (click / double-click handling by the caller).
    pub fn paint(&mut self, ui: &mut egui::Ui, rect: egui::Rect, placement: Placement, theme: &Theme, snap: &PlayerSnapshot) -> egui::Response {
        self.placed_this_frame = placement;
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
        let ppp = ui.ctx().pixels_per_point();
        // The FBO covers the whole rect: mpv letterboxes itself (keepaspect), so aspect
        // override / panscan / zoom (`v`, `V`) work without app-side geometry.
        let target = rect;
        if let Some(s) = &mut self.surface {
            s.request_size((target.width() * ppp).round() as i32, (target.height() * ppp).round() as i32);
            if let (Some((tex, _)), true) = (s.texture(), s.has_frame()) {
                painter.image(tex, target, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Video unavailable — libmpv failed to start (see Logs)",
                egui::FontId::proportional(theme.font(crate::theme::TypeToken::Body)),
                theme.tokens.osd_text,
            );
        }
        self.paint_state_overlay(ui, rect, theme, snap);
        resp
    }

    /// Loading / buffering / reconnecting / error overlay (fixed dark scrim). WP-11.
    fn paint_state_overlay(&self, ui: &mut egui::Ui, rect: egui::Rect, theme: &Theme, snap: &PlayerSnapshot) {
        // WP-11: implement per module docs. Skeleton: nothing.
    }

    /// Corner mini-player (app calls when nothing else placed the video, the screen allows it,
    /// the layout isn't Compact and `interface.mini_player` is on). Returns `Some(true)` = open
    /// fullscreen player, `Some(false)` = stop. Records [`VideoHost::mini_rect`].
    /// SKELETON SAFE DEFAULT: draws nothing, returns `None`.
    pub fn mini_player(&mut self, ctx: &egui::Context, theme: &Theme, snap: &PlayerSnapshot) -> Option<bool> {
        self.mini = None;
        None
    }

    /// Free GL resources (from `App::on_exit`, before the player shuts down).
    pub fn destroy(&mut self, gl: Option<&glow::Context>) {
        if let Some(s) = &mut self.surface {
            s.destroy(gl);
        }
        self.surface = None;
    }
}

/// Fit the video's aspect inside `rect` (centred). Unknown size → whole rect. (Used for the
/// mini-player window shape; the main surface lets mpv letterbox.)
pub fn letterbox(rect: egui::Rect, video: Option<(i64, i64)>) -> egui::Rect {
    let Some((w, h)) = video.filter(|(w, h)| *w > 0 && *h > 0) else { return rect };
    let ar = w as f32 / h as f32;
    let (rw, rh) = (rect.width(), rect.height());
    let (tw, th) = if rw / rh > ar { (rh * ar, rh) } else { (rw, rw / ar) };
    egui::Rect::from_center_size(rect.center(), egui::vec2(tw, th))
}
