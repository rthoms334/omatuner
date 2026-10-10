//! Responsive breakpoints (DESIGN §12.1). Owner: WP-10a.
//!
//! Logical px (egui points) scaled by `rem` = Omarchy base text size / 12 × UI scale
//! (`Theme::rem`), so a larger Omarchy text size moves the breakpoints with it. On this machine
//! (rem 1): full screen ≈ 1600 wide (2560 / 1.6), tiled half ≈ 790, quarter ≈ 790×490.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayoutClass {
    /// < 700 pt: single column, stack navigation, sidebar hidden (Alt+digits still work).
    Compact,
    /// 700–1199 pt (tiled half ≈ 800): icon rail sidebar, two panes, preview collapses.
    Medium,
    /// ≥ 1200 pt (full ≈ 1600): expanded sidebar, three panes.
    Wide,
}

impl LayoutClass {
    pub fn from_width(w: f32, rem: f32) -> Self {
        let rem = rem.max(0.5);
        if w < 700.0 * rem {
            LayoutClass::Compact
        } else if w < 1200.0 * rem {
            LayoutClass::Medium
        } else {
            LayoutClass::Wide
        }
    }
}

/// Sidebar width for a window width + config mode (0 = hidden). `Auto`: expanded 184 at Wide,
/// 48 rail at Medium and Compact down to 480·rem; below that hidden — WP-10a then draws a top
/// glyph strip so Compact still has mouse navigation.
pub fn sidebar_width(width: f32, rem: f32, mode: crate::config::SidebarMode) -> f32 {
    use crate::config::SidebarMode as M;
    let class = LayoutClass::from_width(width, rem);
    let w = match (class, mode) {
        (LayoutClass::Compact, M::Auto) if width < 480.0 * rem.max(0.5) => 0.0,
        (LayoutClass::Compact, M::Auto) | (_, M::Rail) | (LayoutClass::Medium, M::Auto) => 48.0,
        (_, M::Expanded) | (LayoutClass::Wide, M::Auto) => 184.0,
    };
    (w * rem).round()
}

/// OSD scale `s` for the fullscreen player: readable from a distance on big windows. OSD type
/// tokens: title = Display (24)·s, body = Title (14)·s, badges = Caption·s. Below 700 pt the
/// controls row collapses to play/pause · audio · subtitles · info.
pub fn osd_scale(window_width: f32) -> f32 {
    (window_width / 1280.0).clamp(1.0, 1.6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SidebarMode;

    #[test]
    fn breakpoints_scale_with_text_size() {
        assert_eq!(LayoutClass::from_width(790.0, 1.0), LayoutClass::Medium);
        assert_eq!(LayoutClass::from_width(790.0, 16.0 / 12.0), LayoutClass::Compact);
        assert_eq!(sidebar_width(600.0, 1.0, SidebarMode::Auto), 48.0);
        assert_eq!(sidebar_width(400.0, 1.0, SidebarMode::Auto), 0.0);
        assert_eq!(sidebar_width(1600.0, 1.0, SidebarMode::Auto), 184.0);
    }

    #[test]
    fn class_edges_and_rem_floor() {
        assert_eq!(LayoutClass::from_width(699.9, 1.0), LayoutClass::Compact);
        assert_eq!(LayoutClass::from_width(700.0, 1.0), LayoutClass::Medium);
        assert_eq!(LayoutClass::from_width(1199.9, 1.0), LayoutClass::Medium);
        assert_eq!(LayoutClass::from_width(1200.0, 1.0), LayoutClass::Wide);
        // a broken/zero rem never collapses every window into Wide
        assert_eq!(LayoutClass::from_width(500.0, 0.0), LayoutClass::Medium);
        assert!(LayoutClass::Compact < LayoutClass::Medium && LayoutClass::Medium < LayoutClass::Wide);
    }

    #[test]
    fn sidebar_modes_and_the_compact_strip() {
        // Auto: 184 at Wide, 48 at Medium and Compact down to 480·rem, then hidden (top glyph
        // strip, drawn by `widgets::sidebar` in a horizontal layout)
        assert_eq!(sidebar_width(790.0, 1.0, SidebarMode::Auto), 48.0);
        assert_eq!(sidebar_width(480.0, 1.0, SidebarMode::Auto), 48.0);
        assert_eq!(sidebar_width(479.0, 1.0, SidebarMode::Auto), 0.0);
        assert_eq!(sidebar_width(600.0, 4.0 / 3.0, SidebarMode::Auto), 0.0, "480·rem moves with the text size");
        // explicit modes are honoured at any width, scaled by rem
        assert_eq!(sidebar_width(400.0, 1.0, SidebarMode::Rail), 48.0);
        assert_eq!(sidebar_width(800.0, 1.0, SidebarMode::Expanded), 184.0);
        assert_eq!(sidebar_width(1600.0, 1.0, SidebarMode::Rail), 48.0);
        assert_eq!(sidebar_width(2000.0, 1.25, SidebarMode::Auto), 230.0);
    }

    #[test]
    fn osd_scale_clamps() {
        assert_eq!(osd_scale(640.0), 1.0);
        assert_eq!(osd_scale(1600.0), 1.25);
        assert_eq!(osd_scale(4000.0), 1.6);
    }
}
