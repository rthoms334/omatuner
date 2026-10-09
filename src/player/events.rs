//! Dedicated mpv event pump thread (FIXED DECISION 2). Implemented in the skeleton.
//! Owner for changes: WP-07.
//!
//! The thread blocks in `mpv_wait_event(-1)` on the raw handle (it is the ONLY caller of
//! `mpv_wait_event`; all other mpv calls are thread-safe and may come from any thread),
//! converts each event into an owned [`MpvEvent`] (so no borrowed mpv memory escapes), and
//! sends it to the controller. It never touches the UI, so reconnect logic keeps running while
//! the window is hidden. Shutdown: set the stop flag and call `mpv_wakeup`.
//! We read events through `libmpv2-sys` (not `Mpv::wait_event`) to keep full `END_FILE`
//! fidelity (reason + error + playlist_entry_id), which libmpv2 collapses into `Err`.

use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use libmpv2::Mpv;
use libmpv2_sys as sys;

/// Observed properties. The u64 is the `reply_userdata` used with `mpv_observe_property`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum PropId {
    Pause = 1,
    PausedForCache = 2,
    CacheBufferingState = 3,
    CoreIdle = 4,
    TimePos = 5,
    Duration = 6,
    DemuxerCacheDuration = 7,
    Volume = 8,
    Mute = 9,
    /// JSON string (mpv formats node properties as JSON when read as STRING).
    TrackList = 10,
    Seekable = 11,
    SubDelay = 12,
    SubScale = 13,
    VideoParamsW = 14,
    VideoParamsH = 15,
    HwdecCurrent = 16,
    Aid = 17,
    Sid = 18,
    EofReached = 19,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Flag,
    Int,
    Double,
    Str,
}

/// (id, mpv property name, format). Registered once at pump start.
const OBSERVED: &[(PropId, &str, Fmt)] = &[
    (PropId::Pause, "pause", Fmt::Flag),
    (PropId::PausedForCache, "paused-for-cache", Fmt::Flag),
    (PropId::CacheBufferingState, "cache-buffering-state", Fmt::Int),
    (PropId::CoreIdle, "core-idle", Fmt::Flag),
    (PropId::TimePos, "time-pos", Fmt::Double),
    (PropId::Duration, "duration", Fmt::Double),
    (PropId::DemuxerCacheDuration, "demuxer-cache-duration", Fmt::Double),
    (PropId::Volume, "volume", Fmt::Double),
    (PropId::Mute, "mute", Fmt::Flag),
    (PropId::TrackList, "track-list", Fmt::Str),
    (PropId::Seekable, "seekable", Fmt::Flag),
    (PropId::SubDelay, "sub-delay", Fmt::Double),
    (PropId::SubScale, "sub-scale", Fmt::Double),
    (PropId::VideoParamsW, "video-params/w", Fmt::Int),
    (PropId::VideoParamsH, "video-params/h", Fmt::Int),
    (PropId::HwdecCurrent, "hwdec-current", Fmt::Str),
    (PropId::Aid, "aid", Fmt::Str),
    (PropId::Sid, "sid", Fmt::Str),
    (PropId::EofReached, "eof-reached", Fmt::Flag),
];

impl PropId {
    fn from_u64(v: u64) -> Option<Self> {
        OBSERVED.iter().map(|(id, _, _)| *id).find(|id| *id as u64 == v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    /// Property unavailable (e.g. `time-pos` while idle).
    None,
    Flag(bool),
    Int(i64),
    Double(f64),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Unknown,
}

/// Owned mpv event.
#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    StartFile { entry_id: i64 },
    FileLoaded,
    /// `error` is an mpv error code (< 0) when `reason == Error`.
    EndFile { reason: EndReason, error: i32, entry_id: i64 },
    PlaybackRestart,
    VideoReconfig,
    AudioReconfig,
    Seek,
    Idle,
    Prop { id: PropId, value: PropValue },
    /// Already redacted. Only `warn` and above are requested.
    Log { level: String, prefix: String, text: String },
    QueueOverflow,
    Shutdown,
}

/// Running pump. Drop or call [`EventPump::stop`] before destroying mpv.
pub struct EventPump {
    stop: Arc<AtomicBool>,
    mpv: Arc<Mpv>,
    join: Option<JoinHandle<()>>,
}

impl EventPump {
    /// Observe properties, request warn-level log messages, spawn the thread.
    pub fn start(mpv: Arc<Mpv>, tx: Sender<MpvEvent>) -> std::io::Result<Self> {
        let h = mpv.ctx.as_ptr();
        unsafe {
            let lvl = CString::new("warn").unwrap();
            sys::mpv_request_log_messages(h, lvl.as_ptr());
            for (id, name, fmt) in OBSERVED {
                let n = CString::new(*name).unwrap();
                let f = match fmt {
                    Fmt::Flag => sys::mpv_format_MPV_FORMAT_FLAG,
                    Fmt::Int => sys::mpv_format_MPV_FORMAT_INT64,
                    Fmt::Double => sys::mpv_format_MPV_FORMAT_DOUBLE,
                    Fmt::Str => sys::mpv_format_MPV_FORMAT_STRING,
                };
                sys::mpv_observe_property(h, *id as u64, n.as_ptr(), f);
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, mpv2) = (stop.clone(), mpv.clone());
        let join = std::thread::Builder::new().name("mpv-events".into()).spawn(move || pump(mpv2, tx, stop2))?;
        Ok(Self { stop, mpv, join: Some(join) })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe { sys::mpv_wakeup(self.mpv.ctx.as_ptr()) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for EventPump {
    fn drop(&mut self) { self.stop(); }
}

fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn pump(mpv: Arc<Mpv>, tx: Sender<MpvEvent>, stop: Arc<AtomicBool>) {
    let h = mpv.ctx.as_ptr();
    while !stop.load(Ordering::Acquire) {
        let ev = unsafe { &*sys::mpv_wait_event(h, -1.0) };
        let out = match ev.event_id {
            sys::mpv_event_id_MPV_EVENT_NONE => continue,
            sys::mpv_event_id_MPV_EVENT_SHUTDOWN => {
                let _ = tx.send(MpvEvent::Shutdown);
                break;
            }
            sys::mpv_event_id_MPV_EVENT_START_FILE => {
                let d = unsafe { &*(ev.data as *const sys::mpv_event_start_file) };
                MpvEvent::StartFile { entry_id: d.playlist_entry_id }
            }
            sys::mpv_event_id_MPV_EVENT_FILE_LOADED => MpvEvent::FileLoaded,
            sys::mpv_event_id_MPV_EVENT_END_FILE => {
                let d = unsafe { &*(ev.data as *const sys::mpv_event_end_file) };
                let reason = match d.reason {
                    sys::mpv_end_file_reason_MPV_END_FILE_REASON_EOF => EndReason::Eof,
                    sys::mpv_end_file_reason_MPV_END_FILE_REASON_STOP => EndReason::Stop,
                    sys::mpv_end_file_reason_MPV_END_FILE_REASON_QUIT => EndReason::Quit,
                    sys::mpv_end_file_reason_MPV_END_FILE_REASON_ERROR => EndReason::Error,
                    sys::mpv_end_file_reason_MPV_END_FILE_REASON_REDIRECT => EndReason::Redirect,
                    _ => EndReason::Unknown,
                };
                MpvEvent::EndFile { reason, error: d.error, entry_id: d.playlist_entry_id }
            }
            sys::mpv_event_id_MPV_EVENT_PLAYBACK_RESTART => MpvEvent::PlaybackRestart,
            sys::mpv_event_id_MPV_EVENT_VIDEO_RECONFIG => MpvEvent::VideoReconfig,
            sys::mpv_event_id_MPV_EVENT_AUDIO_RECONFIG => MpvEvent::AudioReconfig,
            sys::mpv_event_id_MPV_EVENT_SEEK => MpvEvent::Seek,
            sys::mpv_event_id_MPV_EVENT_IDLE => MpvEvent::Idle,
            sys::mpv_event_id_MPV_EVENT_QUEUE_OVERFLOW => MpvEvent::QueueOverflow,
            sys::mpv_event_id_MPV_EVENT_LOG_MESSAGE => {
                let d = unsafe { &*(ev.data as *const sys::mpv_event_log_message) };
                MpvEvent::Log {
                    level: cstr(d.level),
                    prefix: cstr(d.prefix),
                    text: crate::redact::redact(cstr(d.text).trim_end()).into_owned(),
                }
            }
            sys::mpv_event_id_MPV_EVENT_PROPERTY_CHANGE => {
                let Some(id) = PropId::from_u64(ev.reply_userdata) else { continue };
                let d = unsafe { &*(ev.data as *const sys::mpv_event_property) };
                let value = unsafe {
                    match d.format {
                        sys::mpv_format_MPV_FORMAT_FLAG => PropValue::Flag(*(d.data as *const i32) != 0),
                        sys::mpv_format_MPV_FORMAT_INT64 => PropValue::Int(*(d.data as *const i64)),
                        sys::mpv_format_MPV_FORMAT_DOUBLE => PropValue::Double(*(d.data as *const f64)),
                        sys::mpv_format_MPV_FORMAT_STRING => PropValue::Str(cstr(*(d.data as *const *const std::os::raw::c_char))),
                        _ => PropValue::None,
                    }
                };
                MpvEvent::Prop { id, value }
            }
            _ => continue,
        };
        if tx.send(out).is_err() {
            break;
        }
    }
}
