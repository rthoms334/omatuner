//! Pop-out state of the window and when the app shows the video-only player (DESIGN §12.3).
//! Pure: the app feeds it Hyprland's window flags and request results
//! (`platform::popout::PopOutEvent`) and asks it what a toggle should request.
//!
//! Video-only layout:
//! * while our own pop-out is requested or active (whatever its size: the user may resize it);
//! * otherwise while Hyprland reports the window floating AND pinned AND it is no bigger than
//!   Omarchy's Super+O pop size (1300 × 900 logical) — a pinned big window keeps the full UI;
//! * never while a pop-in is in flight, nor right after one until Hyprland's events caught up
//!   (no flash of the video-only layout between "popped in" and the unpin event).
//!
//! Someone else unpinning our pop-out (Super+O pressed again, a window rule) ends it.

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
}

impl PopOut {
    /// Show the video-only player in a window of `size` (logical px).
    pub fn video_only(&self, size: egui::Vec2) -> bool {
        match self.phase {
            Phase::PoppingOut | Phase::Out => true,
            Phase::PoppingIn => false,
            Phase::Docked => !self.flags_stale && self.flags.floating && self.flags.pinned && size.x <= UNDOCKED_MAX.x && size.y <= UNDOCKED_MAX.y,
        }
    }

    /// The pop-out key: what to request (`None` while a request is in flight).
    pub fn toggle(&mut self, size: egui::Vec2) -> Option<Toggle> {
        let t = match self.phase {
            Phase::PoppingOut | Phase::PoppingIn => return None,
            Phase::Out => Toggle::In { ours: true },
            Phase::Docked if self.video_only(size) => Toggle::In { ours: false },
            Phase::Docked => Toggle::Out,
        };
        self.phase = if t == Toggle::Out { Phase::PoppingOut } else { Phase::PoppingIn };
        Some(t)
    }

    /// Our own pop-out is requested or active.
    pub fn is_out(&self) -> bool {
        matches!(self.phase, Phase::PoppingOut | Phase::Out)
    }

    pub fn on_event(&mut self, ev: &PopOutEvent) {
        match ev {
            PopOutEvent::Flags(f) => {
                // unpinned by someone else (Super+O again): our pop-out is over
                if self.phase == Phase::Out && self.flags.pinned && !f.pinned {
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
            PopOutEvent::Failed(_) => self.phase = Phase::Docked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!p.video_only(BIG));
        assert_eq!(p.toggle(BIG), Some(Toggle::Out));
        assert!(p.video_only(BIG), "immediately, before Hyprland answered");
        assert_eq!(p.toggle(BIG), None, "busy");
        p.on_event(&flags(true, false));
        p.on_event(&flags(true, true));
        p.on_event(&PopOutEvent::PoppedOut);
        assert!(p.video_only(BIG) && p.video_only(SMALL));
        assert_eq!(p.toggle(SMALL), Some(Toggle::In { ours: true }));
        assert!(!p.video_only(SMALL));
        p.on_event(&PopOutEvent::PoppedIn { remember: Some([1, 2, 480, 270]) });
        assert!(!p.video_only(SMALL), "Hyprland's unpin event hasn't arrived yet");
        p.on_event(&flags(true, false));
        p.on_event(&flags(false, false));
        assert!(!p.video_only(SMALL));
        assert_eq!(p.toggle(SMALL), Some(Toggle::Out));
    }

    /// Super+O (floating + pinned, 1300 × 900): video-only up to that size only; the pop-out key
    /// then undoes Super+O's pop (not ours), and a failed request leaves the window docked.
    #[test]
    fn super_o_triggers_video_only_when_small() {
        let mut p = PopOut::default();
        p.on_event(&flags(true, false));
        assert!(!p.video_only(SMALL), "floating alone isn't a pop-out");
        p.on_event(&flags(true, true));
        assert!(p.video_only(SUPER_O) && p.video_only(SMALL));
        assert!(!p.video_only(BIG), "a big pinned window keeps the full UI");
        assert_eq!(p.toggle(SUPER_O), Some(Toggle::In { ours: false }));
        p.on_event(&PopOutEvent::Failed("refused".into()));
        assert!(p.video_only(SUPER_O), "still pinned: still video-only");
        assert_eq!(p.toggle(BIG), Some(Toggle::Out), "big pinned window: the key pops it out properly");
        p.on_event(&PopOutEvent::Failed("refused".into()));
        assert!(!p.video_only(BIG));
    }

    /// Super+O pressed on our pop-out unpins it: our pop-out ends, the key pops out again.
    /// Events from our own pop-out arriving after its result don't end it.
    #[test]
    fn unpinned_by_someone_else_ends_our_pop_out() {
        let mut p = PopOut::default();
        p.toggle(SMALL);
        p.on_event(&PopOutEvent::PoppedOut);
        p.on_event(&flags(true, false)); // late event of our own float step
        assert!(p.is_out());
        p.on_event(&flags(true, true));
        p.on_event(&flags(true, false)); // Super+O: unpin…
        p.on_event(&flags(false, false)); // …and tile
        assert!(!p.is_out() && !p.video_only(SMALL));
        assert_eq!(p.toggle(SMALL), Some(Toggle::Out));
    }
}
