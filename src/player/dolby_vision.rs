//! Dolby Vision profile 5 (see [`DolbyVision`](super::DolbyVision)): the libplacebo filter
//! that converts it to HDR10, and the watch on dropped frames that says when the conversion
//! can't keep up.
//!
//! The controller owns one [`DvState`] and does the mpv side: it adds [`FILTER`] when the
//! decoder reports the `dolbyvision` colour matrix, removes it when [`DvState::file_started`]
//! says so, and feeds [`DvState::tick`] the drop count while the converted picture plays.

use std::time::{Duration, Instant};

use crate::model::MediaRef;

/// mpv's `vf add` argument: FFmpeg's libplacebo filter, labelled so it can be removed again
/// without touching a user's own `vf`. Reshapes the picture to HDR10 (BT.2020, PQ), which
/// mpv's OpenGL renderer tone-maps like any HDR10 copy. Planar 10-bit (what the HEVC decoder
/// itself hands over): a P010 picture comes out as near-black noise through libmpv's render
/// API here, though mpv's own window shows it right (field report 2026-11-11, mpv 0.41).
pub(super) const FILTER: &str = "@tuner-dv:libplacebo=format=yuv420p10le:colorspace=bt2020nc:color_primaries=bt2020:color_trc=smpte2084";
/// The `vf remove` argument for [`FILTER`].
pub(super) const LABEL: &str = "@tuner-dv";

/// Frames are counted over windows this long of uninterrupted playback.
const WINDOW: Duration = Duration::from_secs(10);
/// More dropped frames than this in one window (2 a second at 24 fps) is a visible stutter.
const SLOW_DROPS: i64 = 20;

#[derive(Debug, Default)]
pub(super) struct DvState {
    /// The filter is in mpv's chain.
    pub filter: bool,
    /// The current window: when it started and the drop count then.
    window: Option<(Instant, i64)>,
    /// The title last warned about: a reconnect of the same title doesn't toast again.
    warned: Option<MediaRef>,
}

impl DvState {
    /// mpv started a file: the filter (if any) comes out, it is added again if this file is
    /// Dolby Vision too. True when it has to be removed.
    pub fn file_started(&mut self) -> bool {
        self.window = None;
        std::mem::take(&mut self.filter)
    }

    /// The window restarts (a seek, a pause, buffering: drops around them aren't the GPU's).
    pub fn restart_window(&mut self) {
        self.window = None;
    }

    /// Whether to tell the user about `state` for `media` (once per title).
    pub fn should_warn(&mut self, media: Option<&MediaRef>) -> bool {
        if media.is_some() && self.warned.as_ref() == media {
            return false;
        }
        self.warned = media.cloned();
        true
    }

    /// One tick of uninterrupted playback with the filter in, `drops` the frames dropped so
    /// far in this file: the count of the window that just closed when it dropped too many.
    pub fn tick(&mut self, now: Instant, drops: i64) -> Option<i64> {
        let Some((start, base)) = self.window else {
            self.window = Some((now, drops));
            return None;
        };
        if now.duration_since(start) < WINDOW {
            return None;
        }
        self.window = Some((now, drops));
        Some(drops - base).filter(|n| *n > SLOW_DROPS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verdict is per window of playback: a few drops (the filter's start, a hiccup) pass,
    /// a steady stutter is slow; the count restarts after a seek or pause.
    #[test]
    fn slow_only_after_a_window_with_steady_drops() {
        let mut s = DvState { filter: true, ..Default::default() };
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        assert_eq!(s.tick(at(0), 5), None); // opens the window
        assert_eq!(s.tick(at(5), 40), None); // still open
        assert_eq!(s.tick(at(10), 15), None); // 10 dropped in 10 s: fine
        s.restart_window(); // a seek
        assert_eq!(s.tick(at(11), 100), None);
        assert_eq!(s.tick(at(21), 130), Some(30)); // 30 in 10 s: slow
        assert!(s.file_started());
        assert!(!s.filter);
    }

    #[test]
    fn warns_once_per_title() {
        let mut s = DvState::default();
        let a = MediaRef::Movie(crate::model::MovieId(1));
        assert!(s.should_warn(Some(&a)));
        assert!(!s.should_warn(Some(&a)));
        assert!(s.should_warn(Some(&MediaRef::Movie(crate::model::MovieId(2)))));
    }
}
