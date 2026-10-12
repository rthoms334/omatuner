//! Pop-out state of the window and when the app shows the video-only player (DESIGN §12.3).
//! Pure: the app feeds it Hyprland's window flags and request results
//! (`platform::popout::PopOutEvent`) and asks it what a toggle should request.
//!
//! Video-only layout:
//! * while our own pop-out is requested or active (whatever its size: the user may resize it);
//! * otherwise while something is playing (or paused) AND Hyprland reports the window floating
//!   AND pinned AND it is no bigger than Omarchy's Super+O pop size (1300 × 900 logical) — a
//!   pinned big window, or Super+O on the browsing UI with nothing playing, keeps the full UI;
//! * never while a pop-in is in flight, nor right after one until Hyprland's events caught up
//!   (no flash of the video-only layout between "popped in" and the unpin event).
//!
//! Someone else unpinning or tiling our pop-out (Super+O pressed again, Super+V) ends it.

use eframe::egui;

use crate::platform::popout::{PopOutEvent, WindowFlags};

/// Omarchy's `omarchy-hyprland-window-pop` default size (+1 px for rounding).
pub const UNDOCKED_MAX: egui::Vec2 = egui::vec2(1301.0, 901.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Docked,
    PoppingOut,
    Out,
    PoppingIn,
}

/// What a toggle asks Hyprland to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Pop the window out (float, size, place, pin).
    Out,
    /// Pop it back in; `ours` = undo our own pop-out (else undo Super+O's).
    In { ours: bool },
}

#[derive(Debug, Default)]
pub struct PopOut {
    phase: Phase,
    flags: WindowFlags,
    /// A pop-in finished but Hyprland's events about it may still be on their way.
    flags_stale: bool,
    /// The pop-in in flight undoes our own pop-out (a refusal leaves it out).
    pop_in_ours: bool,
}

impl PopOut {
    /// Show the video-only player in a window of `size` (logical px); `has_media`: something
    /// is playing or paused.
    pub fn video_only(&self, size: egui::Vec2, has_media: bool) -> bool {
        match self.phase {
            Phase::PoppingOut | Phase::Out => true,
            Phase::PoppingIn => false,
            Phase::Docked => {
                has_media && !self.flags_stale && self.flags.floating && self.flags.pinned && size.x <= UNDOCKED_MAX.x && size.y <= UNDOCKED_MAX.y
            }
        }
    }

    /// The pop-out key: what to request (`None` while a request is in flight).
    pub fn toggle(&mut self, size: egui::Vec2, has_media: bool) -> Option<Toggle> {
        let t = match self.phase {
            Phase::PoppingOut | Phase::PoppingIn => return None,
            Phase::Out => Toggle::In { ours: true },
            Phase::Docked if self.video_only(size, has_media) => Toggle::In { ours: false },
            Phase::Docked => Toggle::Out,
        };
        self.phase = if t == Toggle::Out { Phase::PoppingOut } else { Phase::PoppingIn };
        self.pop_in_ours = t == Toggle::In { ours: true };
        Some(t)
    }

    /// Our own pop-out is requested or active.
    pub fn is_out(&self) -> bool {
        matches!(self.phase, Phase::PoppingOut | Phase::Out)
    }

    pub fn on_event(&mut self, ev: &PopOutEvent) {
        match ev {
            PopOutEvent::Flags(f) => {
                // unpinned or tiled by someone else (Super+O again, Super+V): our pop-out is over
                if self.phase == Phase::Out && ((self.flags.pinned && !f.pinned) || (self.flags.floating && !f.floating)) {
                    self.phase = Phase::Docked;
                }
                self.flags = *f;
                self.flags_stale = false;
            }
            PopOutEvent::PoppedOut => {
                if self.phase == Phase::PoppingOut {
                    self.phase = Phase::Out;
                }
            }
            PopOutEvent::PoppedIn { .. } => {
                self.phase = Phase::Docked;
                self.flags_stale = true;
            }
            // a refused pop-in leaves the window popped out
            PopOutEvent::Failed { popping_out: false, .. } if self.phase == Phase::PoppingIn && self.pop_in_ours => self.phase = Phase::Out,
            PopOutEvent::Failed { .. } => self.phase = Phase::Docked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::popout::{Place, PopError};

    const SMALL: egui::Vec2 = egui::vec2(480.0, 270.0);
    const SUPER_O: egui::Vec2 = egui::vec2(1300.0, 900.0);
    const BIG: egui::Vec2 = egui::vec2(1600.0, 1000.0);

    fn flags(floating: bool, pinned: bool) -> PopOutEvent {
        PopOutEvent::Flags(WindowFlags { floating, pinned })
    }

    /// Our pop-out: video-only from the key press on (also resized big), back in on the second
    /// press, and no video-only flash between "popped in" and Hyprland's unpin event.
    #[test]
    fn own_pop_out_round_trip() {
        let mut p = PopOut::default();
        assert!(!p.video_only(BIG, true));
        assert_eq!(p.toggle(BIG, true), Some(Toggle::Out));
        assert!(p.video_only(BIG, true), "immediately, before Hyprland answered");
        assert_eq!(p.toggle(BIG, true), None, "busy");
        p.on_event(&flags(true, false));
        p.on_event(&flags(true, true));
        p.on_event(&PopOutEvent::PoppedOut);
        assert!(p.video_only(BIG, true) && p.video_only(SMALL, true));
        assert_eq!(p.toggle(SMALL, true), Some(Toggle::In { ours: true }));
        assert!(!p.video_only(SMALL, true));
        p.on_event(&PopOutEvent::PoppedIn { place: Place::Forget });
        assert!(!p.video_only(SMALL, true), "Hyprland's unpin event hasn't arrived yet");
        p.on_event(&flags(true, false));
        p.on_event(&flags(false, false));
        assert!(!p.video_only(SMALL, true));
        assert_eq!(p.toggle(SMALL, true), Some(Toggle::Out));
    }

    fn failed(popping_out: bool) -> PopOutEvent {
        PopOutEvent::Failed { popping_out, error: PopError::Ipc("refused".into()) }
    }

    /// Super+O (floating + pinned, 1300 × 900): video-only up to that size only, and only with
    /// something playing; the pop-out key then undoes Super+O's pop (not ours), and a failed
    /// request leaves the window docked.
    #[test]
    fn super_o_triggers_video_only_when_small() {
        let mut p = PopOut::default();
        p.on_event(&flags(true, false));
        assert!(!p.video_only(SMALL, true), "floating alone isn't a pop-out");
        p.on_event(&flags(true, true));
        assert!(p.video_only(SUPER_O, true) && p.video_only(SMALL, true));
        assert!(!p.video_only(SUPER_O, false), "nothing playing: Super+O pins the browsing UI");
        assert!(!p.video_only(BIG, true), "a big pinned window keeps the full UI");
        assert_eq!(p.toggle(SUPER_O, true), Some(Toggle::In { ours: false }));
        p.on_event(&failed(false));
        assert!(p.video_only(SUPER_O, true), "still pinned: still video-only");
        assert_eq!(p.toggle(BIG, true), Some(Toggle::Out), "big pinned window: the key pops it out properly");
        p.on_event(&failed(true));
        assert!(!p.video_only(BIG, true));
    }

    /// Super+O pressed on our pop-out unpins it, Super+V tiles it: either ends our pop-out
    /// and the key pops out again. Events from our own pop-out arriving after its result don't
    /// end it, and a refused pop-in leaves it out.
    #[test]
    fn unpinned_by_someone_else_ends_our_pop_out() {
        let mut p = PopOut::default();
        p.toggle(SMALL, true);
        p.on_event(&PopOutEvent::PoppedOut);
        p.on_event(&flags(true, false)); // late event of our own float step
        assert!(p.is_out());
        p.on_event(&flags(true, true));
        assert_eq!(p.toggle(SMALL, true), Some(Toggle::In { ours: true }));
        p.on_event(&failed(false));
        assert!(p.is_out(), "a refused pop-in: still out");
        p.on_event(&flags(true, false)); // Super+O: unpin…
        p.on_event(&flags(false, false)); // …and tile
        assert!(!p.is_out() && !p.video_only(SMALL, true));
        assert_eq!(p.toggle(SMALL, true), Some(Toggle::Out));
        p.on_event(&PopOutEvent::PoppedOut);
        p.on_event(&flags(true, true));
        p.on_event(&flags(false, true)); // Super+V tiles it (no unpin event)
        assert!(!p.is_out());
    }
}
