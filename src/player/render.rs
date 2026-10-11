//! libmpv render API → OpenGL FBO → egui texture. PROVEN BY THE SPIKE — implemented for real.
//! Owner: WP-07 (changes need a re-run of the spike checks: upright, colours, resize, exit 0).
//!
//! Recipe (docs/DESIGN.md §11.1, research/spike.md):
//! * Own `mpv_render_context` via `libmpv2-sys` (libmpv2's `render` module has a 3-field,
//!   non-`repr(C)` FBO struct — out-of-bounds read).
//! * `FLIP_Y = 0` when rendering into an FBO that egui samples (uv 0,0 = top-left).
//! * No `report_swap` (eframe has no post-swap hook; a late call halves throughput).
//! * `BLOCK_FOR_TARGET_TIME = 0` + mpv `video-timing-offset=0`: never block the UI thread.
//! * RGBA8 texture registered once with `frame.register_native_glow_texture`; on resize the same
//!   texture's storage is re-specified (eframe owns the GL name).
//! * After each mpv render, restore GL state egui_glow does not reset itself.
//! * Teardown order: detach update callback → free render ctx (GL current) → delete FBO →
//!   drop `Mpv`. `MpvGl` frees the context only in the explicit [`MpvGl::destroy`] (GL
//!   current); a plain `Drop` (panic/unwind path) detaches the callback and LEAKS the context,
//!   and the liveness flag makes `Player` leak mpv too — never UB.
//! * Rendering happens in `App::logic`, which runs ONLY when the compositor sends frame
//!   callbacks (vsync is off and `App::ui` requests them; on Hyprland: not on hidden
//!   workspaces unless the window has the `render_unfocused` rule; see DESIGN §11.1). Each render stamps the player's
//!   `RenderClock`, which the controller uses to suspend video decoding when nothing renders.
//! * Until a screen places the video, frames are rendered into a 640×360 sink so the VO never
//!   waits (CLI `--play`, MPRIS, Compact without mini-player). `mpv_render_context_update`
//!   alone does NOT consume frames.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use eframe::egui;
use eframe::glow::{self, HasContext};
use libmpv2_sys as sys;

use crate::error::{Error, Result};

/// eframe's `CreationContext::get_proc_address`.
pub type GetProcAddress = Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

/// mpv calls this (on the GL thread, during mpv_render_context_create) to resolve GL symbols.
unsafe extern "C" fn gpa_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let gpa = unsafe { &*(ctx as *const GetProcAddress) };
    gpa(unsafe { CStr::from_ptr(name) }) as *mut c_void
}

/// State touched from mpv's internal thread. Must not call any mpv API in here.
struct UpdateShared {
    egui: egui::Context,
    dirty: AtomicBool,
    callbacks: AtomicU64,
}

unsafe extern "C" fn update_trampoline(ctx: *mut c_void) {
    let s = unsafe { &*(ctx as *const UpdateShared) };
    s.dirty.store(true, Ordering::Release);
    s.callbacks.fetch_add(1, Ordering::Relaxed);
    s.egui.request_repaint();
}

/// Owns an `mpv_render_context`.
/// INVARIANTS: drop it (a) with the GL context current and (b) BEFORE the `Mpv` handle is
/// destroyed (the owning `Player` keeps an `Arc<Mpv>` alive until after `VideoSurface::destroy`).
pub struct MpvGl {
    ctx: *mut sys::mpv_render_context,
    _gpa: Box<GetProcAddress>,
    shared: Box<UpdateShared>,
    freed: bool,
}

impl MpvGl {
    /// Create the render context. Must run with the GL context current (eframe's
    /// `CreationContext` callback or `App::logic`).
    pub fn new(
        core: std::ptr::NonNull<sys::mpv_handle>,
        gpa: GetProcAddress,
        wl_display: Option<*mut c_void>,
        egui_ctx: egui::Context,
    ) -> Result<Self> {
        let gpa = Box::new(gpa);
        let mut init = sys::mpv_opengl_init_params {
            get_proc_address: Some(gpa_trampoline),
            get_proc_address_ctx: &*gpa as *const GetProcAddress as *mut c_void,
        };
        let mut advanced: i32 = 0; // simple mode; mpv drives decoding itself
        let mut params = vec![
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut init as *mut _ as *mut c_void },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced as *mut _ as *mut c_void,
            },
        ];
        if let Some(d) = wl_display {
            // lets mpv's vaapi/EGL hwdec interop find the Wayland display
            params.push(sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_WL_DISPLAY, data: d });
        }
        params.push(sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() });

        let mut ctx: *mut sys::mpv_render_context = std::ptr::null_mut();
        let err = unsafe { sys::mpv_render_context_create(&mut ctx, core.as_ptr(), params.as_mut_ptr()) };
        if err < 0 {
            return Err(Error::Mpv(format!("mpv_render_context_create failed: {}", mpv_err(err))));
        }
        let shared = Box::new(UpdateShared { egui: egui_ctx, dirty: AtomicBool::new(true), callbacks: AtomicU64::new(0) });
        unsafe {
            sys::mpv_render_context_set_update_callback(ctx, Some(update_trampoline), &*shared as *const UpdateShared as *mut c_void);
        }
        Ok(Self { ctx, _gpa: gpa, shared, freed: false })
    }

    /// Free the render context. MUST run with the GL context current and before mpv is
    /// destroyed (`VideoSurface::destroy` from `App::on_exit`).
    pub fn destroy(mut self) {
        unsafe {
            // detach the callback first so mpv's thread never touches freed `shared`
            sys::mpv_render_context_set_update_callback(self.ctx, None, std::ptr::null_mut());
            sys::mpv_render_context_free(self.ctx);
        }
        self.freed = true;
    }

    /// True if the update callback fired since the last call (consumes it).
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// `mpv_render_context_update()`; returns `MPV_RENDER_UPDATE_*` flags.
    pub fn update(&self) -> u64 {
        unsafe { sys::mpv_render_context_update(self.ctx) }
    }

    /// Render the current frame into `fbo` (w × h). flip_y=false ⇒ FBO row 0 is the TOP of the
    /// picture, which is what egui sampling expects (VERIFIED by the spike).
    pub fn render(&self, fbo: u32, w: i32, h: i32, flip_y: bool) -> Result<()> {
        let mut fbo = sys::mpv_opengl_fbo { fbo: fbo as i32, w, h, internal_format: glow::RGBA8 as i32 };
        let mut flip: i32 = flip_y as i32;
        let mut block: i32 = 0; // never block the UI thread waiting for the frame's target time
        let mut params = [
            sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO, data: &mut fbo as *mut _ as *mut c_void },
            sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut _ as *mut c_void,
            },
            sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() },
        ];
        let err = unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) };
        if err < 0 { Err(Error::Mpv(mpv_err(err))) } else { Ok(()) }
    }

    /// Number of update callbacks so far (diagnostics).
    pub fn callbacks(&self) -> u64 {
        self.shared.callbacks.load(Ordering::Relaxed)
    }
}

impl Drop for MpvGl {
    fn drop(&mut self) {
        if self.freed {
            return;
        }
        // Not destroyed explicitly (panic/unwind): we may not have a current GL context, so
        // freeing would be UB. Detach the callback (so `shared` may be dropped) and leak the
        // context; `Player` sees the liveness flag and leaks mpv as well.
        tracing::error!("mpv render context dropped without destroy(); leaking it");
        unsafe { sys::mpv_render_context_set_update_callback(self.ctx, None, std::ptr::null_mut()) };
    }
}

pub fn mpv_err(code: i32) -> String {
    unsafe { CStr::from_ptr(sys::mpv_error_string(code)) }.to_string_lossy().into_owned()
}

/// Off-screen target: FBO + RGBA8 texture that egui samples like any other image.
pub struct VideoTarget {
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    pub tex_id: egui::TextureId,
    pub w: i32,
    pub h: i32,
}

impl VideoTarget {
    pub fn new(gl: &glow::Context, frame: &mut eframe::Frame, w: i32, h: i32) -> Result<Self> {
        unsafe {
            let tex = gl.create_texture().map_err(Error::Mpv)?;
            let fbo = gl.create_framebuffer().map_err(Error::Mpv)?;
            let mut t = Self { fbo, tex, tex_id: egui::TextureId::default(), w: 0, h: 0 };
            t.resize(gl, w, h)?;
            // eframe takes ownership of the GL name; we keep re-specifying its storage on resize.
            t.tex_id = frame.register_native_glow_texture(tex);
            Ok(t)
        }
    }

    pub fn resize(&mut self, gl: &glow::Context, w: i32, h: i32) -> Result<()> {
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.tex), 0);
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(Error::Mpv(format!("FBO incomplete: {status:#x}")));
            }
        }
        self.w = w;
        self.h = h;
        Ok(())
    }

    /// Fill with opaque black (hides the stale frame of the previous stream; spike gotcha 12).
    pub fn clear(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    pub fn fbo_name(&self) -> u32 {
        self.fbo.0.get()
    }

    /// Delete the FBO. The texture is owned by eframe after registration.
    pub fn destroy(&self, gl: &glow::Context) {
        unsafe { gl.delete_framebuffer(self.fbo) }
    }
}

/// The single video "screen": render context + target, drawn wherever the current layout puts
/// it (Live preview pane, fullscreen player, corner mini-player). One instance per app.
pub struct VideoSurface {
    render: Option<MpvGl>,
    core: crate::player::RenderCore,
    target: Option<VideoTarget>,
    /// Size (physical px) requested by the last `request_size` call (from the previous frame's
    /// layout); applied at the next `render`.
    want: (i32, i32),
    /// A frame of the *current* file has been rendered since the last `clear`.
    has_frame: bool,
    pending_clear: bool,
    pub renders: u64,
}

impl VideoSurface {
    /// Size used until a screen places the video (frames must be consumed anyway).
    pub const SINK_SIZE: (i32, i32) = (640, 360);

    /// Create the render context. Call from the eframe `CreationContext` callback (GL current)
    /// with `Player::render_core()`.
    pub fn new(core: crate::player::RenderCore, cc: &eframe::CreationContext<'_>) -> Result<Self> {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let gpa = cc.get_proc_address.clone().ok_or_else(|| Error::Mpv("glow backend required".into()))?;
        let wl = match cc.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(h)) => Some(h.display.as_ptr()),
            _ => None,
        };
        let render = MpvGl::new(core.handle, gpa, wl, cc.egui_ctx.clone())?;
        core.alive.store(true, Ordering::Release);
        Ok(Self { render: Some(render), core, target: None, want: (0, 0), has_frame: false, pending_clear: false, renders: 0 })
    }

    /// Record the physical-pixel size the video occupies this frame (applied next render).
    pub fn request_size(&mut self, w_px: i32, h_px: i32) {
        if w_px > 0 && h_px > 0 {
            self.want = (w_px, h_px);
        }
    }

    /// A new file is loading: show black until its first frame (call on `PlayerState::Loading`).
    pub fn reset_for_new_file(&mut self) {
        self.has_frame = false;
        self.pending_clear = true;
    }

    /// Render if mpv has a new frame or the size changed. Call once per frame from
    /// `App::logic` (GL current). Returns true if a frame was rendered.
    pub fn render(&mut self, gl: &glow::Context, frame: &mut eframe::Frame) -> bool {
        let Some(render) = &self.render else { return false };
        // stamp the render clock: the controller suspends video decoding when this stops
        let ms = self.core.epoch.elapsed().as_millis().max(1) as u64;
        self.core.clock.store(ms, Ordering::Relaxed);
        // nobody placed the video yet: render into a small sink so the VO never stalls
        let (w, h) = if self.want.0 > 0 && self.want.1 > 0 { self.want } else { Self::SINK_SIZE };
        let mut resized = false;
        match &mut self.target {
            None => match VideoTarget::new(gl, frame, w, h) {
                Ok(t) => {
                    t.clear(gl);
                    self.target = Some(t);
                    resized = true;
                }
                Err(e) => {
                    tracing::error!("video target: {e}");
                    return false;
                }
            },
            Some(t) if t.w != w || t.h != h => {
                if let Err(e) = t.resize(gl, w, h) {
                    tracing::error!("video target resize: {e}");
                    return false;
                }
                resized = true;
            }
            _ => {}
        }
        let t = self.target.as_ref().expect("target");
        if self.pending_clear {
            t.clear(gl);
            self.pending_clear = false;
        }
        let dirty = render.take_dirty();
        let flags = if dirty { render.update() } else { 0 };
        let new_frame = (flags & sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64) != 0;
        if !(new_frame || (resized && self.has_frame)) {
            return false;
        }
        if let Err(e) = render.render(t.fbo_name(), w, h, false) {
            tracing::warn!("mpv render: {e}");
        }
        self.renders += 1;
        if new_frame {
            self.has_frame = true;
        }
        // restore the bits of GL state egui_glow does NOT reset itself
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.active_texture(glow::TEXTURE0);
            gl.disable(glow::FRAMEBUFFER_SRGB);
        }
        true
    }

    /// Texture to paint (uv (0,0)–(1,1)) and its pixel size; `None` before the first render.
    pub fn texture(&self) -> Option<(egui::TextureId, [i32; 2])> {
        self.target.as_ref().map(|t| (t.tex_id, [t.w, t.h]))
    }

    pub fn has_frame(&self) -> bool {
        self.has_frame
    }

    /// Shutdown (from `App::on_exit`, GL current): render ctx first, then FBO. The `Mpv`
    /// handle must outlive this call.
    pub fn destroy(&mut self, gl: Option<&glow::Context>) {
        if let Some(r) = self.render.take()
            && gl.is_some()
        {
            r.destroy();
            self.core.alive.store(false, Ordering::Release);
        } // else: dropped → leaked (see MpvGl::drop); `alive` stays set so mpv is leaked too
        if let (Some(t), Some(gl)) = (self.target.take(), gl) {
            t.destroy(gl);
        }
    }
}
