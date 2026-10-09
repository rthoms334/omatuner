//! The EPG grid widget — 2-D virtualized (channels × time). Owner: WP-11. DESIGN §13.4.
//!
//! Geometry: sticky time ruler (height = control-height) with 30-min ticks and day label;
//! sticky channel column (Wide 200 pt: logo+number+name, Medium 140 pt: logo+name, Compact
//! 56 pt: logo only); rows = `row_height` (48 pt comfortable / 36 compact); time scale
//! `px_per_min` (Wide 4.0, Medium 3.0, Compact 2.5) → Wide shows ≈ 5 h, Medium ≈ 3.5 h.
//!
//! Virtualization: only rows in [first_visible − 2, last_visible + 2] and time in
//! [view_start − 1 h, view_end + 1 h] are queried/painted. Programme data come from
//! [`ProgrammeCache`], keyed by (epg channel, 6-hour bucket) in EPG time; a row with tvg-shift
//! `s` asks for `[from − s, to − s)` and shifts the results (`ProgrammeSlim::shifted`) AFTER the
//! cache. A miss loads in THIS frame while the frame budget allows (`FrameBudget`, 2 ms total
//! UI DB time; at most 24 bucket loads), the rest next frame (cells show a neutral skeleton and
//! a repaint is requested). Cache: LRU 4 000 buckets, fully invalidated on `DataScope::Epg`.
//! All reads go through `&mut dyn EpgReader` (tests pass a fake; production `DbEpg`).
//! Geometry derives from the theme (`GridMetrics::for_layout`): rows/ruler scale with
//! `Tokens::rem` and control-height.
//!
//! Painting per cell: 1 px separator right edge; title (body, clipped with …) + time
//! (body-small @0.52) when width ≥ 60 pt; past = text @0.52; airing = progress fill fg@0.08
//! up to now; catch-up-able past = ⟲ glyph; reminder set = bell glyph; focus = list-cursor
//! (fg@0.08 fill, accent text, 1 px fg@0.25 border). Now-line: 2 px accent vertical line
//! across rows + a marker on the ruler; repaint scheduled every 30 s (request_repaint_after).
//! Gaps without data: a single "No information" cell (text @0.36).
//!
//! Keyboard focus (Kodi-style "focus time" model): `focus_time` T persists across Up/Down —
//! moving rows selects the programme covering T on the new row (or the nearest one). Left/Right
//! select the previous/next programme on the row and set T = max(start, view_start) of the new
//! one. The view auto-scrolls horizontally to keep the focused cell fully visible (or its start
//! when longer than the view) and vertically with a 2-row margin. PgUp/PgDn move by visible
//! rows; `[`/`]` ±24 h; Shift+←/→ ±3 h; `n`/Home → now. Mouse: wheel = rows, Shift+wheel /
//! horizontal trackpad = time, click = focus, double-click = open programme, hover tooltip.

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;

use crate::images::ImageCache;
use crate::keymap::Action;
use crate::model::{ChannelId, EpgChannelId, ProgrammeId, ProgrammeSlim, Timestamp};
use crate::store::library::LiveIndex;
use crate::theme::{SpaceToken, Theme};
use crate::ui::LayoutClass;
use crate::ui::data::{EpgReader, FrameBudget};

/// One guide row.
#[derive(Debug, Clone, PartialEq)]
pub struct GuideRow {
    pub channel: ChannelId,
    pub epg: Option<EpgChannelId>,
    pub tvg_shift_min: i32,
    pub catchup_days: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridMetrics {
    pub px_per_min: f32,
    pub row_height: f32,
    pub header_width: f32,
    pub ruler_height: f32,
}

impl GridMetrics {
    /// Base values at rem = 1 (Omarchy text size 12): Wide 4 px/min + 200 pt column, Medium 3 +
    /// 140, Compact 2.5 + 56; rows 48 (36 compact density); ruler = control-height.
    pub fn for_layout(layout: LayoutClass, compact_rows: bool, theme: &Theme) -> Self {
        let rem = theme.rem();
        let (px, hw) = match layout {
            LayoutClass::Wide => (4.0, 200.0),
            LayoutClass::Medium => (3.0, 140.0),
            LayoutClass::Compact => (2.5, 56.0),
        };
        let ch = theme.space(SpaceToken::ControlHeight);
        let row = if compact_rows { (36.0 * rem).max(ch + 4.0) } else { (48.0 * rem).max(ch * 1.5) };
        Self { px_per_min: px * rem, row_height: row.round(), header_width: (hw * rem).round(), ruler_height: ch }
    }
}

/// Bucketed programme cache (see module docs).
#[derive(Default)]
pub struct ProgrammeCache {
    buckets: HashMap<(EpgChannelId, i64), Arc<[ProgrammeSlim]>>,
    lru: std::collections::VecDeque<(EpgChannelId, i64)>,
    loads_this_frame: u32,
}

impl ProgrammeCache {
    pub const BUCKET_SECS: i64 = 6 * 3600;
    pub const MAX_BUCKETS: usize = 4000;
    pub const LOADS_PER_FRAME: u32 = 24;

    /// Reset the per-frame load budget (call once per frame).
    pub fn begin_frame(&mut self) { self.loads_this_frame = 0; }
    /// Programmes of `ch` overlapping [from, to) as DISPLAYED on a channel with tvg-shift
    /// `shift_min` (queries `[from − shift, to − shift)` in EPG time, shifts after the cache).
    /// `None` while some bucket is still unloaded (budget exhausted) — caller paints skeleton
    /// cells and requests a repaint.
    pub fn window(
        &mut self,
        reader: &mut dyn EpgReader,
        budget: &mut FrameBudget,
        ch: EpgChannelId,
        shift_min: i32,
        from: Timestamp,
        to: Timestamp,
    ) -> Option<Vec<ProgrammeSlim>> {
        todo!("WP-11")
    }
    pub fn invalidate(&mut self) {
        self.buckets.clear();
        self.lru.clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GridOutcome {
    None,
    FocusChanged,
    /// Enter / double-click on a programme.
    Open { programme: ProgrammeId, channel: ChannelId },
    Watch(ChannelId),
    Catchup { channel: ChannelId, programme: ProgrammeSlim },
    Remind { channel: ChannelId, programme: ProgrammeSlim },
}

pub struct EpgGridState {
    pub rows: Arc<[GuideRow]>,
    pub focus_row: usize,
    /// "Focus time" T (see module docs).
    pub focus_time: Timestamp,
    /// Left edge of the visible time window.
    pub view_start: Timestamp,
    /// Fractional first visible row (vertical scroll).
    pub scroll_rows: f32,
    pub cache: ProgrammeCache,
    focused: Option<ProgrammeSlim>,
}

impl EpgGridState {
    pub fn new(rows: Arc<[GuideRow]>, now: Timestamp) -> Self {
        Self {
            rows,
            focus_row: 0,
            focus_time: now,
            view_start: now - (now % 1800) - 1800,
            scroll_rows: 0.0,
            cache: ProgrammeCache::default(),
            focused: None,
        }
    }
    /// Replace rows (collection/filter changed), keeping focus on the same channel if present.
    pub fn set_rows(&mut self, rows: Arc<[GuideRow]>) { todo!("WP-11") }
    /// Focus a channel at a time (e.g. returning from the player: the channel just watched).
    pub fn jump_to(&mut self, channel: ChannelId, t: Timestamp) { todo!("WP-11") }
    /// Keyboard actions (Guide context). Neighbour programme lookup goes through `reader`.
    pub fn handle(&mut self, action: Action, reader: &mut dyn EpgReader, budget: &mut FrameBudget, now: Timestamp) -> GridOutcome {
        todo!("WP-11")
    }
    /// Focused channel + programme (for the preview pane / OSD).
    pub fn focused(&self) -> Option<(ChannelId, Option<&ProgrammeSlim>)> { todo!("WP-11") }
}

/// Draw the grid into the available rect.
#[allow(clippy::too_many_arguments)]
pub fn epg_grid(
    ui: &mut egui::Ui,
    theme: &Theme,
    images: &mut ImageCache,
    reader: &mut dyn EpgReader,
    budget: &mut FrameBudget,
    live: &LiveIndex,
    state: &mut EpgGridState,
    metrics: GridMetrics,
    now: Timestamp,
) -> GridOutcome {
    todo!("WP-11")
}
