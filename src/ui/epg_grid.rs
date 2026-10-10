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
//!
//! The guide screen feeds a few per-frame extras through crate-visible fields of
//! [`EpgGridState`] (the reminder set, the mini-player rect to keep the focus clear of, the
//! clock format and a hovered programme's description) — the pub API stays as frozen.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect};

use crate::images::ImageCache;
use crate::keymap::Action;
use crate::model::{ChannelId, EpgChannelId, ProgrammeId, ProgrammeSlim, Timestamp};
use crate::store::library::LiveIndex;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::LayoutClass;
use crate::ui::data::{EpgReader, FrameBudget};
use crate::ui::widgets::list::paint_row_state;
use crate::ui::widgets::{RowState, glyphs};

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

/// A cached bucket: programmes + last-use stamp.
type Bucket = (Arc<[ProgrammeSlim]>, u64);

/// Bucketed programme cache (see module docs). LRU by a use stamp per bucket; eviction drops
/// the least recently used tenth once the cache is over [`ProgrammeCache::MAX_BUCKETS`].
#[derive(Default)]
pub struct ProgrammeCache {
    buckets: HashMap<(EpgChannelId, i64), Bucket>,
    /// Monotonic use stamp (one tick per `window` call).
    clock: u64,
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
        let shift = i64::from(shift_min) * 60;
        let (efrom, eto) = (from - shift, to - shift);
        if eto <= efrom {
            return Some(Vec::new());
        }
        self.clock += 1;
        let stamp = self.clock;
        let mut complete = true;
        let mut out: Vec<ProgrammeSlim> = Vec::new();
        let overlaps = |p: &&ProgrammeSlim| p.start < eto && p.stop > efrom;
        for b in efrom.div_euclid(Self::BUCKET_SECS)..=(eto - 1).div_euclid(Self::BUCKET_SECS) {
            let key = (ch, b);
            if let Some((v, used)) = self.buckets.get_mut(&key) {
                *used = stamp;
                out.extend(v.iter().filter(overlaps).cloned());
                continue;
            }
            if self.loads_this_frame >= Self::LOADS_PER_FRAME {
                complete = false;
                continue;
            }
            let (b0, b1) = (b * Self::BUCKET_SECS, (b + 1) * Self::BUCKET_SECS);
            match budget.run(|| reader.window(ch, b0, b1)) {
                None => complete = false,
                Some(r) => {
                    self.loads_this_frame += 1;
                    let v: Arc<[ProgrammeSlim]> = match r {
                        Ok(v) => v.into(),
                        Err(e) => {
                            // cached as empty until the next `DataScope::Epg` (no retry storm)
                            tracing::debug!("guide window: {e}");
                            Arc::from([])
                        }
                    };
                    out.extend(v.iter().filter(overlaps).cloned());
                    self.buckets.insert(key, (v, stamp));
                }
            }
        }
        self.evict();
        if !complete {
            return None;
        }
        // a programme spanning a bucket boundary comes back from both buckets
        out.sort_by_key(|p| (p.start, p.id));
        out.dedup_by_key(|p| p.id);
        if shift_min != 0 {
            for p in &mut out {
                *p = p.shifted(shift_min);
            }
        }
        Some(out)
    }
    pub fn invalidate(&mut self) { self.buckets.clear(); }

    /// Buckets held.
    #[cfg(test)]
    fn len(&self) -> usize { self.buckets.len() }

    /// Drop the least recently used tenth once over capacity (amortised O(1) per insert).
    fn evict(&mut self) {
        if self.buckets.len() <= Self::MAX_BUCKETS {
            return;
        }
        let keep = Self::MAX_BUCKETS * 9 / 10;
        let mut stamps: Vec<u64> = self.buckets.values().map(|(_, s)| *s).collect();
        let drop_n = stamps.len() - keep;
        let (_, cut, _) = stamps.select_nth_unstable(drop_n - 1);
        let cut = *cut;
        let mut dropped = 0;
        self.buckets.retain(|_, (_, s)| {
            if *s <= cut && dropped < drop_n {
                dropped += 1;
                false
            } else {
                true
            }
        });
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

/// How far back / ahead a Left/Right step looks for the neighbouring programme.
const NEIGHBOUR_SPAN: i64 = 12 * 3600;
/// Window around T searched for the programme to focus after a row change.
const REFOCUS_SPAN: i64 = 3 * 3600;
/// Rows kept between the focused row and the edge of the view.
const ROW_MARGIN: usize = 2;
/// Time scroll step when a row has no programme next to the focus.
const EMPTY_STEP: i64 = 3600;

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
    /// The focused programme must be re-derived from (focus_row, focus_time) once data loads.
    refocus: bool,
    /// Visible time span and rows of the last frame (keyboard scroll math before a paint).
    view_secs: i64,
    page_rows: usize,
    /// Rows whose focus stays clear of the mini-player (≤ `page_rows`).
    usable_rows: usize,
    /// Set by the guide screen each frame: the mini-player rect to keep the focus clear of.
    pub(crate) avoid: Option<Rect>,
    /// Displayed start times of pending reminders per channel (bell glyph).
    pub(crate) reminders: HashMap<ChannelId, HashSet<Timestamp>>,
    pub(crate) clock_24h: bool,
    /// Programme under the pointer (the screen loads its description into `hover_detail`).
    pub(crate) hovered: Option<ProgrammeId>,
    pub(crate) hover_detail: Option<(ProgrammeId, String)>,
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
            refocus: true,
            view_secs: 3 * 3600,
            page_rows: 10,
            usable_rows: 10,
            avoid: None,
            reminders: HashMap::new(),
            clock_24h: true,
            hovered: None,
            hover_detail: None,
        }
    }
    /// Replace rows (collection/filter changed), keeping focus on the same channel if present.
    pub fn set_rows(&mut self, rows: Arc<[GuideRow]>) {
        let current = self.rows.get(self.focus_row).map(|r| r.channel);
        self.focus_row = current.and_then(|c| rows.iter().position(|r| r.channel == c)).unwrap_or(0);
        let kept = current.is_some() && rows.get(self.focus_row).map(|r| r.channel) == current;
        self.rows = rows;
        if !kept {
            self.focused = None;
            self.refocus = true;
            self.scroll_rows = 0.0;
        }
        self.ensure_row_visible();
    }
    /// Focus a channel at a time (e.g. returning from the player: the channel just watched).
    pub fn jump_to(&mut self, channel: ChannelId, t: Timestamp) {
        if let Some(i) = self.rows.iter().position(|r| r.channel == channel) {
            self.focus_row = i;
        }
        self.focus_time = t;
        if t < self.view_start || t >= self.view_start + self.view_secs {
            self.view_start = t - t.rem_euclid(1800) - 1800;
        }
        self.focused = None;
        self.refocus = true;
        self.ensure_row_visible();
    }
    /// Keyboard actions (Guide context). Neighbour programme lookup goes through `reader`.
    pub fn handle(&mut self, action: Action, reader: &mut dyn EpgReader, budget: &mut FrameBudget, now: Timestamp) -> GridOutcome {
        if self.rows.is_empty() {
            return GridOutcome::None;
        }
        self.focus_row = self.focus_row.min(self.rows.len() - 1);
        let before = (self.focus_row, self.focused.as_ref().map(|p| p.id), self.focus_time);
        let last = self.rows.len() - 1;
        match action {
            Action::Up => self.move_rows(self.focus_row.saturating_sub(1), reader, budget),
            Action::Down => self.move_rows((self.focus_row + 1).min(last), reader, budget),
            Action::PageUp => self.move_rows(self.focus_row.saturating_sub(self.usable_rows.max(1)), reader, budget),
            Action::PageDown => self.move_rows((self.focus_row + self.usable_rows.max(1)).min(last), reader, budget),
            Action::Home => self.move_rows(0, reader, budget),
            Action::End => self.move_rows(last, reader, budget),
            Action::Left => self.step(false, reader, budget),
            Action::Right => self.step(true, reader, budget),
            Action::GuidePrevDay => self.shift_time(-86_400, reader, budget),
            Action::GuideNextDay => self.shift_time(86_400, reader, budget),
            Action::GuideBackHours => self.shift_time(-3 * 3600, reader, budget),
            Action::GuideForwardHours => self.shift_time(3 * 3600, reader, budget),
            Action::GuideNow => {
                self.focus_time = now;
                self.view_start = now - now.rem_euclid(1800) - 1800;
                self.refocus_now(reader, budget);
            }
            Action::Activate => {
                let channel = self.rows[self.focus_row].channel;
                return match &self.focused {
                    Some(p) => GridOutcome::Open { programme: p.id, channel },
                    // a gap or a channel without guide data: the only thing to do is watch it
                    None => GridOutcome::Watch(channel),
                };
            }
            Action::GuideWatch => return GridOutcome::Watch(self.rows[self.focus_row].channel),
            Action::GuideCatchup => {
                let row = &self.rows[self.focus_row];
                return match &self.focused {
                    Some(p) if catchup_available(row, p, now) => GridOutcome::Catchup { channel: row.channel, programme: p.clone() },
                    _ => GridOutcome::None,
                };
            }
            Action::GuideRemind => {
                let channel = self.rows[self.focus_row].channel;
                return match &self.focused {
                    Some(p) if p.start > now => GridOutcome::Remind { channel, programme: p.clone() },
                    _ => GridOutcome::None,
                };
            }
            _ => return GridOutcome::None,
        }
        let after = (self.focus_row, self.focused.as_ref().map(|p| p.id), self.focus_time);
        if after != before { GridOutcome::FocusChanged } else { GridOutcome::None }
    }
    /// Focused channel + programme (for the preview pane / OSD).
    pub fn focused(&self) -> Option<(ChannelId, Option<&ProgrammeSlim>)> {
        self.rows.get(self.focus_row).map(|r| (r.channel, self.focused.as_ref()))
    }

    // ------------------------------------------------------------------ focus model

    /// Programmes of `row` overlapping [from, to) (displayed time); a row without guide data
    /// has none. `None` = still loading.
    fn programmes(&mut self, row: usize, from: Timestamp, to: Timestamp, reader: &mut dyn EpgReader, budget: &mut FrameBudget) -> Option<Vec<ProgrammeSlim>> {
        let r = self.rows.get(row)?;
        let (epg, shift) = (r.epg, r.tvg_shift_min);
        match epg {
            Some(ch) => self.cache.window(reader, budget, ch, shift, from, to),
            None => Some(Vec::new()),
        }
    }

    fn move_rows(&mut self, row: usize, reader: &mut dyn EpgReader, budget: &mut FrameBudget) {
        self.focus_row = row;
        self.refocus_now(reader, budget);
        self.ensure_row_visible();
    }

    fn shift_time(&mut self, secs: i64, reader: &mut dyn EpgReader, budget: &mut FrameBudget) {
        self.focus_time += secs;
        self.view_start += secs;
        self.refocus_now(reader, budget);
    }

    /// Focus the programme covering T on the focus row (else the nearest); deferred while the
    /// row's data isn't loaded.
    fn refocus_now(&mut self, reader: &mut dyn EpgReader, budget: &mut FrameBudget) {
        self.refocus = true;
        self.resolve_focus(reader, budget);
    }

    fn resolve_focus(&mut self, reader: &mut dyn EpgReader, budget: &mut FrameBudget) {
        if !self.refocus {
            return;
        }
        let t = self.focus_time;
        let Some(progs) = self.programmes(self.focus_row, t - REFOCUS_SPAN, t + REFOCUS_SPAN, reader, budget) else {
            self.focused = None;
            return;
        };
        self.refocus = false;
        let distance = |p: &ProgrammeSlim| if p.contains(t) { 0 } else if p.start > t { p.start - t } else { t - p.stop + 1 };
        self.focused = progs.into_iter().min_by_key(distance);
        if let Some(p) = &self.focused {
            let (s, e) = (p.start, p.stop);
            self.ensure_time_visible(s, e);
        }
    }

    /// Left/Right: the previous/next programme on the focus row; across a stretch without
    /// data the focus becomes the gap and the view scrolls by an hour.
    fn step(&mut self, forward: bool, reader: &mut dyn EpgReader, budget: &mut FrameBudget) {
        let (cur_start, cur_stop) = match &self.focused {
            Some(p) => (p.start, p.stop),
            None => (self.focus_time, self.focus_time),
        };
        let (from, to) = if forward { (cur_start, cur_stop.max(cur_start) + NEIGHBOUR_SPAN) } else { (cur_start - NEIGHBOUR_SPAN, cur_start) };
        let Some(progs) = self.programmes(self.focus_row, from, to.max(from + 1), reader, budget) else { return };
        let next = if forward {
            progs.into_iter().filter(|p| p.start > cur_start && p.stop > cur_stop.max(cur_start)).min_by_key(|p| p.start)
        } else {
            progs.into_iter().filter(|p| p.start < cur_start).max_by_key(|p| p.start)
        };
        match next {
            Some(p) => {
                self.ensure_time_visible(p.start, p.stop);
                self.focus_time = p.start.max(self.view_start);
                self.focused = Some(p);
            }
            None => {
                // no programme that way: walk through the gap
                let t = if forward { cur_stop.max(self.focus_time) + EMPTY_STEP } else { cur_start.min(self.focus_time) - EMPTY_STEP };
                self.focused = None;
                self.focus_time = t;
                self.ensure_time_visible(t, t + 1);
            }
        }
        self.refocus = false;
    }

    /// Scroll horizontally so `[start, stop)` is visible (its start when longer than the view).
    fn ensure_time_visible(&mut self, start: Timestamp, stop: Timestamp) {
        let span = self.view_secs.max(1800);
        let end = self.view_start + span;
        if start < self.view_start {
            self.view_start = start;
        } else if stop > end {
            self.view_start = if stop - start > span { start } else { stop - span };
        }
    }

    /// Keep the focus row inside the usable rows with a margin of `ROW_MARGIN`.
    fn ensure_row_visible(&mut self) {
        let usable = self.usable_rows.max(1);
        let margin = ROW_MARGIN.min(usable.saturating_sub(1) / 2);
        let row = self.focus_row as f32;
        let top = self.scroll_rows;
        if row < top + margin as f32 {
            self.scroll_rows = (row - margin as f32).max(0.0);
        } else if row > top + (usable - 1 - margin) as f32 {
            self.scroll_rows = row - (usable - 1 - margin) as f32;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let max = self.rows.len().saturating_sub(self.usable_rows.max(1)) as f32;
        self.scroll_rows = self.scroll_rows.clamp(0.0, max.max(0.0));
    }

    /// Focus a cell by pointer: row + the programme under `t` (or the gap).
    fn focus_at(&mut self, row: usize, t: Timestamp, p: Option<ProgrammeSlim>) {
        self.focus_row = row;
        self.focus_time = p.as_ref().map_or(t, |p| p.start.max(self.view_start));
        self.focused = p;
        self.refocus = false;
    }
}

/// A past (ended) programme inside the channel's archive window.
pub(crate) fn catchup_available(row: &GuideRow, p: &ProgrammeSlim, now: Timestamp) -> bool {
    row.catchup_days > 0 && p.stop <= now && p.start >= now - i64::from(row.catchup_days) * 86_400
}

// ------------------------------------------------------------------ time formatting

/// Local wall-clock time: "20:00" (24 h) or "8:00 pm".
pub(crate) fn hhmm(ts: Timestamp, h24: bool) -> String {
    match jiff::Timestamp::from_second(ts) {
        Ok(t) => {
            let z = t.to_zoned(jiff::tz::TimeZone::system());
            if h24 { z.strftime("%H:%M").to_string() } else { z.strftime("%-I:%M %P").to_string() }
        }
        Err(_) => "--:--".into(),
    }
}

/// "Thu 9 Oct" in local time.
pub(crate) fn day_label(ts: Timestamp) -> String {
    match jiff::Timestamp::from_second(ts) {
        Ok(t) => t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %-d %b").to_string(),
        Err(_) => String::new(),
    }
}

/// "20:00 – 21:00".
pub(crate) fn time_range(start: Timestamp, stop: Timestamp, h24: bool) -> String { format!("{} – {}", hhmm(start, h24), hhmm(stop, h24)) }

/// "34 min left" / "1 h 05 left".
pub(crate) fn time_left(secs: i64) -> String {
    let m = (secs.max(0) + 59) / 60;
    if m >= 60 { format!("{} h {:02} left", m / 60, m % 60) } else { format!("{m} min left") }
}

// ------------------------------------------------------------------ painting

/// One line elided with "…" at `max_width`; returns the galley.
fn galley(painter: &egui::Painter, text: impl Into<String>, font: FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.into(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    painter.layout_job(job)
}

/// Paint one elided line anchored at `pos`; returns its rect.
fn text(painter: &egui::Painter, pos: Pos2, anchor: Align2, s: impl Into<String>, font: FontId, color: Color32, max_width: f32) -> Rect {
    let g = galley(painter, s, font, color, max_width);
    let r = anchor.anchor_size(pos, g.size());
    painter.galley(r.min, g, color);
    r
}

/// What a frame of the grid needs besides the state (keeps the paint helpers' signatures short).
struct Frame<'a> {
    theme: &'a Theme,
    m: GridMetrics,
    grid: Rect,
    now: Timestamp,
    view_start: Timestamp,
    view_end: Timestamp,
}

impl Frame<'_> {
    fn x_of(&self, t: Timestamp) -> f32 { self.grid.left() + (t - self.view_start) as f32 / 60.0 * self.m.px_per_min }
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
    let t = &theme.tokens;
    let rect = ui.available_rect_before_wrap();
    let id = ui.id().with("epg-grid");
    let resp = ui.interact(rect, id, egui::Sense::click());
    ui.allocate_rect(rect, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let m = metrics;
    let row_h = m.row_height.max(8.0);
    let ruler = Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.top() + m.ruler_height));
    let header_w = m.header_width.min(rect.width() * 0.5);
    let grid = Rect::from_min_max(egui::pos2(rect.left() + header_w, ruler.bottom()), rect.max);
    let px_per_s = (m.px_per_min / 60.0).max(1e-3);

    // geometry of this frame feeds the keyboard model
    let grid_h = grid.height().max(row_h);
    state.page_rows = ((grid_h / row_h).floor() as usize).max(1);
    state.usable_rows = match state.avoid.filter(|a| a.intersects(grid) && a.top() > grid.top()) {
        Some(a) => (((a.top() - grid.top()) / row_h).floor() as usize).clamp(1, state.page_rows),
        None => state.page_rows,
    };
    state.view_secs = ((grid.width().max(1.0) / px_per_s) as i64).max(600);
    state.focus_row = state.focus_row.min(state.rows.len().saturating_sub(1));
    state.clamp_scroll();

    let mut outcome = GridOutcome::None;

    // ---- mouse: wheel = rows, horizontal (or Shift+wheel, which egui reports as x) = time
    if resp.hovered() {
        let d = ui.input(|i| i.smooth_scroll_delta);
        if d.y != 0.0 {
            state.scroll_rows -= d.y / row_h;
            state.clamp_scroll();
        }
        if d.x != 0.0 {
            state.view_start -= (d.x / px_per_s) as i64;
        }
    }
    let view_end = state.view_start + state.view_secs;
    let f = Frame { theme, m, grid, now, view_start: state.view_start, view_end };

    // the focus row first so a deferred refocus resolves before its cell is painted
    state.resolve_focus(reader, budget);

    let first = state.scroll_rows.floor().max(0.0) as usize;
    let visible = (grid_h / row_h).ceil() as usize + 1;
    let last = (first + visible).min(state.rows.len());
    // prefetch ±2 rows (only queried, never painted)
    let (q_from, q_to) = (state.view_start - 3600, view_end + 3600);
    for i in first.saturating_sub(2)..(last + 2).min(state.rows.len()) {
        if !(first..last).contains(&i) {
            let _ = state.programmes(i, q_from, q_to, reader, budget);
        }
    }

    let pointer = resp.hover_pos();
    let mut hovered: Option<(usize, Timestamp, Option<ProgrammeSlim>)> = None;
    let mut header_hit: Option<ChannelId> = None;
    let mut loading = false;
    let y0 = grid.top() - (state.scroll_rows - first as f32) * row_h;
    let grid_painter = painter.with_clip_rect(grid);
    let header_painter = painter.with_clip_rect(Rect::from_min_max(egui::pos2(rect.left(), grid.top()), egui::pos2(grid.left(), rect.bottom())));
    for (k, i) in (first..last).enumerate() {
        let y = y0 + k as f32 * row_h;
        let row_rect = Rect::from_min_max(egui::pos2(grid.left(), y), egui::pos2(grid.right(), y + row_h));
        let focus_row = i == state.focus_row;
        let progs = state.programmes(i, q_from, q_to, reader, budget);
        loading |= progs.is_none();
        let row = state.rows[i].clone();
        // header (sticky channel column)
        let header = Rect::from_min_max(egui::pos2(rect.left(), y), egui::pos2(grid.left(), y + row_h));
        paint_header(ui, &header_painter, theme, images, live, &row, header, focus_row);
        if pointer.is_some_and(|p| header.contains(p) && grid.y_range().contains(p.y)) {
            header_hit = Some(row.channel);
        }
        // cells
        let cells = cells_of(progs.as_deref(), state.view_start - 3600, view_end + 3600);
        for cell in &cells {
            let x0 = f.x_of(cell.start).max(grid.left() - 2.0);
            let x1 = f.x_of(cell.stop).min(grid.right() + 2.0);
            if x1 <= grid.left() || x0 >= grid.right() {
                continue;
            }
            let r = Rect::from_min_max(egui::pos2(x0, row_rect.top()), egui::pos2(x1, row_rect.bottom()));
            let focused = focus_row
                && match (&cell.programme, &state.focused) {
                    (Some(p), Some(fp)) => p.id == fp.id,
                    (None, None) => !state.refocus && cell.start <= state.focus_time && state.focus_time < cell.stop,
                    _ => false,
                };
            let reminder = cell.programme.as_ref().is_some_and(|p| state.reminders.get(&row.channel).is_some_and(|s| s.contains(&p.start)));
            paint_cell(&grid_painter, &f, &row, cell, r, focused, reminder, progs.is_none(), state.clock_24h);
            if let Some(pos) = pointer
                && r.contains(pos)
                && grid.contains(pos)
            {
                let tp = state.view_start + ((pos.x - grid.left()) / px_per_s) as i64;
                hovered = Some((i, tp, cell.programme.clone()));
            }
        }
        // row separator
        painter.hline(rect.x_range(), row_rect.bottom() - 0.5, egui::Stroke::new(1.0, t.separator));
    }
    // ---- ruler (sticky) + now-line
    paint_ruler(&painter, &f, Rect::from_min_max(ruler.min, egui::pos2(grid.left(), ruler.bottom())), ruler, state.clock_24h);
    painter.vline(grid.left() - 0.5, egui::Rangef::new(rect.top(), rect.bottom()), egui::Stroke::new(1.0, t.pane_divider));
    if (state.view_start..view_end).contains(&now) {
        let x = f.x_of(now);
        painter.vline(x, egui::Rangef::new(ruler.bottom(), rect.bottom()), egui::Stroke::new(2.0, t.accent));
        let marker = Rect::from_center_size(egui::pos2(x, ruler.bottom() - 3.0), egui::vec2(8.0, 6.0));
        painter.rect_filled(marker, 0.0, t.accent);
    }

    // ---- clicks: the channel column watches, cells focus / open
    if let Some(ch) = header_hit
        && resp.clicked()
    {
        outcome = GridOutcome::Watch(ch);
    }
    if let Some((row, tp, prog)) = hovered.clone() {
        state.hovered = prog.as_ref().map(|p| p.id);
        if resp.double_clicked() {
            state.focus_at(row, tp, prog.clone());
            outcome = match prog {
                Some(p) => GridOutcome::Open { programme: p.id, channel: state.rows[row].channel },
                None => GridOutcome::Watch(state.rows[row].channel),
            };
        } else if resp.clicked() {
            state.focus_at(row, tp, prog);
            if outcome == GridOutcome::None {
                outcome = GridOutcome::FocusChanged;
            }
        }
    } else {
        state.hovered = None;
    }

    // ---- hover tooltip: title, time, sub-title / description excerpt
    if let Some((row, _, Some(p))) = hovered {
        let h24 = state.clock_24h;
        let detail = state.hover_detail.as_ref().filter(|(id, _)| *id == p.id).map(|(_, d)| d.clone());
        let catchup = state.rows.get(row).is_some_and(|r| catchup_available(r, &p, now));
        resp.clone().on_hover_ui_at_pointer(|ui| {
            ui.set_max_width(320.0 * theme.rem());
            ui.label(egui::RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Body)).color(t.fg));
            let mut when = time_range(p.start, p.stop, h24);
            if catchup {
                when.push_str("  ");
                when.push_str(glyphs::CATCHUP);
            }
            ui.label(egui::RichText::new(when).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            let extra = detail.or_else(|| p.sub_title.as_ref().map(|s| s.to_string()));
            if let Some(d) = extra.filter(|d| !d.is_empty()) {
                ui.add(egui::Label::new(egui::RichText::new(excerpt(&d, 220)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).wrap());
            }
        });
    }

    if loading {
        ui.ctx().request_repaint();
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(30));
    // the per-frame load budget covers this paint and the next frame's key handling
    state.cache.begin_frame();
    outcome
}

/// A displayed cell: a programme or a gap without data.
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    start: Timestamp,
    stop: Timestamp,
    programme: Option<ProgrammeSlim>,
}

/// Programmes + gap cells covering [from, to). `None` (loading) = one skeleton cell.
fn cells_of(progs: Option<&[ProgrammeSlim]>, from: Timestamp, to: Timestamp) -> Vec<Cell> {
    let Some(progs) = progs else { return vec![Cell { start: from, stop: to, programme: None }] };
    let mut out = Vec::with_capacity(progs.len() * 2 + 1);
    let mut t = from;
    for p in progs.iter().filter(|p| p.stop > p.start) {
        if p.start > t {
            out.push(Cell { start: t, stop: p.start, programme: None });
        }
        out.push(Cell { start: p.start, stop: p.stop, programme: Some(p.clone()) });
        t = t.max(p.stop);
    }
    if t < to {
        out.push(Cell { start: t, stop: to, programme: None });
    }
    out
}

/// First `n` characters at a word boundary, with "…".
fn excerpt(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let cut: String = s.chars().take(n).collect();
    // cut at a word boundary unless the text breaks right after the cut anyway
    let at_boundary = s.chars().nth(n).is_some_and(char::is_whitespace);
    let cut = if at_boundary { cut.as_str() } else { cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a) };
    format!("{}…", cut.trim_end())
}

/// Sticky channel column: logo (+ number + name at Wide, name at Medium, logo only Compact).
fn paint_header(ui: &mut egui::Ui, p: &egui::Painter, theme: &Theme, images: &mut ImageCache, live: &LiveIndex, row: &GuideRow, r: Rect, focus_row: bool) {
    let t = &theme.tokens;
    if !ui.is_rect_visible(r) {
        return;
    }
    if focus_row {
        paint_row_state(p, r.shrink2(egui::vec2(0.0, 1.0)), theme, RowState::Selected);
    }
    let Some(ch) = live.get(row.channel) else { return };
    let pad = theme.space(SpaceToken::Lg);
    let size = (r.height() - 2.0 * theme.space(SpaceToken::Md)).clamp(8.0, 36.0 * theme.rem());
    let compact = r.width() < 100.0 * theme.rem();
    let logo_x = if compact { r.center().x - size / 2.0 } else { r.left() + pad };
    let logo_rect = Rect::from_min_size(egui::pos2(logo_x, r.center().y - size / 2.0), egui::vec2(size, size));
    if logo_rect.max.y <= p.clip_rect().max.y && logo_rect.min.y >= p.clip_rect().min.y {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(logo_rect));
        crate::ui::widgets::logo::logo(&mut child, theme, images, ch.logo.as_deref(), &ch.name, size);
    }
    if compact {
        return;
    }
    let mut x = logo_rect.right() + pad;
    let color = if focus_row { t.list_cursor_text } else { t.fg };
    let wide = r.width() >= 180.0 * theme.rem();
    if wide && let Some(n) = ch.number {
        let nr = text(p, egui::pos2(x, r.center().y), Align2::LEFT_CENTER, n.to_string(), FontId::proportional(theme.font(TypeToken::Body)), t.text_secondary, f32::INFINITY);
        x = nr.right() + theme.space(SpaceToken::Md);
    }
    text(p, egui::pos2(x, r.center().y), Align2::LEFT_CENTER, ch.name.to_string(), FontId::proportional(theme.font(TypeToken::Body)), color, (r.right() - pad - x).max(0.0));
}

/// One programme / gap / skeleton cell.
#[allow(clippy::too_many_arguments)]
fn paint_cell(p: &egui::Painter, f: &Frame<'_>, row: &GuideRow, cell: &Cell, r: Rect, focused: bool, reminder: bool, skeleton: bool, h24: bool) {
    let theme = f.theme;
    let t = &theme.tokens;
    let inner = Rect::from_min_max(egui::pos2(r.left(), r.top() + 1.0), egui::pos2(r.right() - 1.0, r.bottom() - 1.0));
    if skeleton {
        let bar = Rect::from_center_size(egui::pos2(f.grid.center().x, inner.center().y), egui::vec2(f.grid.width() * 0.8, (theme.font(TypeToken::Body) * 0.8).round()));
        p.rect_filled(bar, t.rounding, t.fill_field);
        return;
    }
    let pad = theme.space(SpaceToken::Lg);
    let text_left = inner.left().max(f.grid.left()) + pad;
    let text_w = (inner.right().min(f.grid.right()) - pad - text_left).max(0.0);
    let Some(prog) = &cell.programme else {
        if focused {
            paint_row_state(p, inner, theme, RowState::Cursor);
            p.rect_stroke(inner, t.rounding, egui::Stroke::new(1.0, t.list_cursor_border), egui::StrokeKind::Inside);
        }
        let label = if row.epg.is_none() { "No guide data" } else { "No information" };
        let color = if focused { t.list_cursor_text } else { t.text_disabled };
        if text_w > 12.0 {
            text(p, egui::pos2(text_left, inner.center().y), Align2::LEFT_CENTER, label, FontId::proportional(theme.font(TypeToken::Body)), color, text_w);
        }
        p.vline(inner.right() + 0.5, inner.y_range(), egui::Stroke::new(1.0, t.separator));
        return;
    };
    let airing = prog.contains(f.now);
    let past = prog.stop <= f.now;
    if airing {
        let x_now = f.x_of(f.now);
        let fill = Rect::from_min_max(inner.min, egui::pos2(x_now.clamp(inner.left(), inner.right()), inner.bottom()));
        p.rect_filled(fill, 0.0, t.fill_hover);
    }
    if focused {
        paint_row_state(p, inner, theme, RowState::Cursor);
        p.rect_stroke(inner, t.rounding, egui::Stroke::new(1.0, t.list_cursor_border), egui::StrokeKind::Inside);
    }
    p.vline(inner.right() + 0.5, inner.y_range(), egui::Stroke::new(1.0, t.separator));
    if text_w < 8.0 {
        return;
    }
    let title_color = if focused {
        t.list_cursor_text
    } else if past {
        t.text_secondary
    } else {
        t.fg
    };
    let catchup = catchup_available(row, prog, f.now);
    let icon_font = FontId::proportional(theme.font(TypeToken::BodySmall));
    let mut left = text_left;
    let mut right = text_left + text_w;
    let show_time = inner.width() >= 60.0 * theme.rem();
    let title_y = if show_time { inner.top() + inner.height() * 0.36 } else { inner.center().y };
    if catchup {
        let g = text(p, egui::pos2(left, title_y), Align2::LEFT_CENTER, glyphs::CATCHUP, icon_font.clone(), t.text_secondary, f32::INFINITY);
        left = g.right() + theme.space(SpaceToken::Sm);
    }
    if reminder && right - left > 24.0 {
        let g = text(p, egui::pos2(right, title_y), Align2::RIGHT_CENTER, glyphs::REMINDER, icon_font, if focused { t.list_cursor_text } else { t.accent }, f32::INFINITY);
        right = g.left() - theme.space(SpaceToken::Sm);
    }
    text(p, egui::pos2(left, title_y), Align2::LEFT_CENTER, prog.title.to_string(), FontId::proportional(theme.font(TypeToken::Body)), title_color, (right - left).max(0.0));
    if show_time {
        let y = inner.top() + inner.height() * 0.70;
        text(p, egui::pos2(text_left, y), Align2::LEFT_CENTER, time_range(prog.start, prog.stop, h24), FontId::proportional(theme.font(TypeToken::BodySmall)), t.text_secondary, text_w);
    }
}

/// Time ruler: day label over the channel column, 30-min ticks with times over the grid.
fn paint_ruler(p: &egui::Painter, f: &Frame<'_>, corner: Rect, ruler: Rect, h24: bool) {
    let view_start = f.view_start;
    let theme = f.theme;
    let t = &theme.tokens;
    let font = FontId::proportional(theme.font(TypeToken::BodySmall));
    let pad = theme.space(SpaceToken::Lg);
    text(p, egui::pos2(corner.left() + pad, corner.center().y), Align2::LEFT_CENTER, day_label(view_start.max(f.now.min(f.view_end))), FontId::proportional(theme.font(TypeToken::Body)), t.fg, (corner.width() - 2.0 * pad).max(0.0));
    let rp = p.with_clip_rect(Rect::from_min_max(egui::pos2(f.grid.left(), ruler.top()), ruler.max));
    let mut tick = view_start - view_start.rem_euclid(1800);
    while tick < f.view_end {
        let x = f.x_of(tick);
        if x >= f.grid.left() - 1.0 {
            rp.vline(x, egui::Rangef::new(ruler.bottom() - theme.space(SpaceToken::Md), ruler.bottom()), egui::Stroke::new(1.0, t.separator));
            text(&rp, egui::pos2(x + theme.space(SpaceToken::Sm), ruler.center().y), Align2::LEFT_CENTER, hhmm(tick, h24), font.clone(), t.text_secondary, f32::INFINITY);
        }
        tick += 1800;
    }
    p.hline(ruler.x_range(), ruler.bottom() - 0.5, egui::Stroke::new(1.0, t.separator));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProgrammeFlags;
    use std::collections::HashMap;

    /// Every channel shows 30-minute programmes, except channel 2 which has an hour-long gap
    /// at 01:00 (relative to T0), and channel 3 which has nothing.
    struct Fake {
        calls: u32,
    }

    const T0: Timestamp = 1_791_576_000; // multiple of 1800

    impl EpgReader for Fake {
        fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> crate::error::Result<Vec<ProgrammeSlim>> {
            self.calls += 1;
            if ch.0 == 3 {
                return Ok(vec![]);
            }
            let mut out = Vec::new();
            let mut s = from - from.rem_euclid(1800);
            while s < to {
                let gap = ch.0 == 2 && (T0 + 3600..T0 + 7200).contains(&s);
                if !gap {
                    out.push(ProgrammeSlim { id: ProgrammeId(ch.0 * 1_000_000 + (s - T0) / 1800 + 500_000), start: s, stop: s + 1800, title: format!("P{}", (s - T0) / 1800).into(), sub_title: None, flags: ProgrammeFlags::default() });
                }
                s += 1800;
            }
            Ok(out)
        }
        fn now_next(&mut self, _: &[(EpgChannelId, i32)], _: Timestamp) -> crate::error::Result<HashMap<(EpgChannelId, i32), crate::model::NowNext>> { Ok(HashMap::new()) }
    }

    fn rows(n: i64) -> Arc<[GuideRow]> {
        (1..=n).map(|i| GuideRow { channel: ChannelId(i), epg: Some(EpgChannelId(i)), tvg_shift_min: 0, catchup_days: 7 }).collect()
    }

    fn big() -> FrameBudget { FrameBudget::new(std::time::Duration::from_secs(10)) }

    #[test]
    fn cache_buckets_dedupe_shift_and_limit_loads() {
        let mut c = ProgrammeCache::default();
        let mut r = Fake { calls: 0 };
        let mut b = big();
        // 12 h across bucket boundaries
        let v = c.window(&mut r, &mut b, EpgChannelId(1), 0, T0, T0 + 12 * 3600).unwrap();
        assert_eq!(v.len(), 24);
        assert!(v.windows(2).all(|w| w[0].stop == w[1].start), "sorted and deduplicated");
        let calls = r.calls;
        c.window(&mut r, &mut b, EpgChannelId(1), 0, T0 + 3600, T0 + 7200).unwrap();
        assert_eq!(r.calls, calls, "served from the cache");
        // tvg-shift +60: the 20:00 programme shows at 21:00
        let s = c.window(&mut r, &mut b, EpgChannelId(1), 60, T0 + 3600, T0 + 5400).unwrap();
        assert_eq!(s[0].start, T0 + 3600);
        assert_eq!(s[0].title.as_ref(), "P0");
        // per-frame load limit
        c.begin_frame();
        let mut misses = 0;
        for ch in 100..140 {
            if c.window(&mut r, &mut b, EpgChannelId(ch), 0, T0, T0 + 3600).is_none() {
                misses += 1;
            }
        }
        assert_eq!(misses, 40 - ProgrammeCache::LOADS_PER_FRAME as usize);
        c.begin_frame();
        assert!(c.window(&mut r, &mut b, EpgChannelId(139), 0, T0, T0 + 3600).is_some());
        // an exhausted frame budget defers too
        let mut none = FrameBudget::new(std::time::Duration::ZERO);
        assert!(c.window(&mut r, &mut none, EpgChannelId(999), 0, T0, T0 + 3600).is_none());
        assert!(none.deferred());
        c.invalidate();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut c = ProgrammeCache::default();
        let mut r = Fake { calls: 0 };
        let mut b = big();
        for ch in 0..(ProgrammeCache::MAX_BUCKETS as i64 + 10) {
            c.begin_frame();
            c.window(&mut r, &mut b, EpgChannelId(10 + ch), 0, T0, T0 + 60).unwrap();
        }
        assert!(c.len() <= ProgrammeCache::MAX_BUCKETS);
        let calls = r.calls;
        // the most recent survives
        c.window(&mut r, &mut b, EpgChannelId(10 + ProgrammeCache::MAX_BUCKETS as i64 + 9), 0, T0, T0 + 60).unwrap();
        assert_eq!(r.calls, calls);
        // the oldest was evicted
        c.window(&mut r, &mut b, EpgChannelId(10), 0, T0, T0 + 60).unwrap();
        assert_eq!(r.calls, calls + 1);
    }

    #[test]
    fn focus_time_persists_across_rows_and_left_right_set_it() {
        let mut s = EpgGridState::new(rows(5), T0 + 600);
        let mut r = Fake { calls: 0 };
        let mut b = big();
        let now = T0 + 600;
        s.handle(Action::GuideNow, &mut r, &mut b, now);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0);
        // → next programme: T = its start
        assert_eq!(s.handle(Action::Right, &mut r, &mut b, now), GridOutcome::FocusChanged);
        assert_eq!(s.focus_time, T0 + 1800);
        // ↓ keeps T: the programme covering T on the next row
        s.handle(Action::Down, &mut r, &mut b, now);
        assert_eq!(s.focus_row, 1);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0 + 1800);
        // row 2 (index 1 → channel 2) has a gap at T0+3600..T0+7200: Right walks into it
        s.handle(Action::Right, &mut r, &mut b, now);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0 + 7200, "the next programme after the gap");
        s.handle(Action::Left, &mut r, &mut b, now);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0 + 1800, "back across the gap");
        // a channel without any data: focus is the gap, Left/Right scroll time
        s.handle(Action::Down, &mut r, &mut b, now);
        assert_eq!(s.focus_row, 2);
        assert!(s.focused().unwrap().1.is_none());
        let t = s.focus_time;
        s.handle(Action::Right, &mut r, &mut b, now);
        assert_eq!(s.focus_time, t + EMPTY_STEP);
        assert!(matches!(s.handle(Action::Activate, &mut r, &mut b, now), GridOutcome::Watch(ChannelId(3))));
    }

    #[test]
    fn up_down_pick_the_programme_covering_t() {
        let mut s = EpgGridState::new(rows(3), T0);
        let mut r = Fake { calls: 0 };
        let mut b = big();
        s.focus_time = T0 + 4000; // inside P2 (T0+3600..T0+5400)
        s.refocus = true;
        s.handle(Action::Down, &mut r, &mut b, T0);
        let p = s.focused().unwrap().1.unwrap().clone();
        // channel 2 has a gap there: the nearest programme is chosen
        assert!(p.start == T0 + 1800 || p.start == T0 + 7200, "{p:?}");
        s.handle(Action::Down, &mut r, &mut b, T0);
        s.handle(Action::Up, &mut r, &mut b, T0);
        assert_eq!(s.focus_time, T0 + 4000, "T persists across rows");
    }

    #[test]
    fn day_hours_and_now_move_the_view() {
        let now = T0 + 900;
        let mut s = EpgGridState::new(rows(2), now);
        let mut r = Fake { calls: 0 };
        let mut b = big();
        let v = s.view_start;
        s.handle(Action::GuideNextDay, &mut r, &mut b, now);
        assert_eq!(s.view_start, v + 86_400);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0 + 86_400);
        s.handle(Action::GuideBackHours, &mut r, &mut b, now);
        assert_eq!(s.view_start, v + 86_400 - 3 * 3600);
        s.handle(Action::GuideNow, &mut r, &mut b, now);
        assert_eq!(s.view_start, T0 - 1800);
        assert_eq!(s.focus_time, now);
        s.handle(Action::GuideForwardHours, &mut r, &mut b, now);
        assert_eq!(s.focus_time, now + 3 * 3600);
    }

    #[test]
    fn paging_and_first_last_keep_a_margin() {
        let mut s = EpgGridState::new(rows(100), T0);
        let mut r = Fake { calls: 0 };
        let mut b = big();
        s.page_rows = 10;
        s.usable_rows = 10;
        s.handle(Action::PageDown, &mut r, &mut b, T0);
        assert_eq!(s.focus_row, 10);
        assert!(s.focus_row as f32 <= s.scroll_rows + 7.0, "2-row margin: {}", s.scroll_rows);
        s.handle(Action::End, &mut r, &mut b, T0);
        assert_eq!(s.focus_row, 99);
        assert_eq!(s.scroll_rows, 90.0);
        s.handle(Action::Home, &mut r, &mut b, T0);
        assert_eq!((s.focus_row, s.scroll_rows), (0, 0.0));
        // the mini-player covers the bottom rows: fewer usable rows
        s.usable_rows = 4;
        for _ in 0..6 {
            s.handle(Action::Down, &mut r, &mut b, T0);
        }
        assert!(s.focus_row as f32 <= s.scroll_rows + 3.0 - 1.0);
    }

    #[test]
    fn actions_on_the_focused_programme() {
        let now = T0 + 4000;
        let mut s = EpgGridState::new(rows(2), now);
        let mut r = Fake { calls: 0 };
        let mut b = big();
        s.handle(Action::GuideNow, &mut r, &mut b, now);
        // airing: no catch-up, no reminder; Enter opens
        assert_eq!(s.handle(Action::GuideCatchup, &mut r, &mut b, now), GridOutcome::None);
        assert_eq!(s.handle(Action::GuideRemind, &mut r, &mut b, now), GridOutcome::None);
        assert!(matches!(s.handle(Action::Activate, &mut r, &mut b, now), GridOutcome::Open { channel: ChannelId(1), .. }));
        s.handle(Action::Left, &mut r, &mut b, now);
        assert!(matches!(s.handle(Action::GuideCatchup, &mut r, &mut b, now), GridOutcome::Catchup { channel: ChannelId(1), .. }));
        s.handle(Action::Right, &mut r, &mut b, now);
        s.handle(Action::Right, &mut r, &mut b, now);
        assert!(matches!(s.handle(Action::GuideRemind, &mut r, &mut b, now), GridOutcome::Remind { channel: ChannelId(1), .. }));
        assert_eq!(s.handle(Action::GuideWatch, &mut r, &mut b, now), GridOutcome::Watch(ChannelId(1)));
        // no catch-up without archive
        let mut s = EpgGridState::new(Arc::from(vec![GuideRow { channel: ChannelId(1), epg: Some(EpgChannelId(1)), tvg_shift_min: 0, catchup_days: 0 }]), now);
        s.handle(Action::GuideNow, &mut r, &mut b, now);
        s.handle(Action::Left, &mut r, &mut b, now);
        assert_eq!(s.handle(Action::GuideCatchup, &mut r, &mut b, now), GridOutcome::None);
    }

    #[test]
    fn deferred_focus_resolves_once_data_arrives() {
        let mut s = EpgGridState::new(rows(2), T0 + 60);
        let mut r = Fake { calls: 0 };
        let mut none = FrameBudget::new(std::time::Duration::ZERO);
        s.handle(Action::Down, &mut r, &mut none, T0);
        assert_eq!(s.focus_row, 1);
        assert!(s.focused().unwrap().1.is_none());
        let mut b = big();
        s.resolve_focus(&mut r, &mut b);
        assert_eq!(s.focused().unwrap().1.unwrap().start, T0);
    }

    #[test]
    fn set_rows_keeps_the_focused_channel_and_jump_to_scrolls() {
        let mut s = EpgGridState::new(rows(50), T0);
        s.usable_rows = 10;
        s.jump_to(ChannelId(40), T0 + 86_400);
        assert_eq!(s.focus_row, 39);
        assert!(s.scroll_rows > 25.0);
        assert!(s.view_start <= T0 + 86_400 && T0 + 86_400 < s.view_start + s.view_secs);
        let sub: Arc<[GuideRow]> = rows(50).iter().skip(30).cloned().collect();
        s.set_rows(sub);
        assert_eq!(s.focused().unwrap().0, ChannelId(40));
        assert_eq!(s.focus_row, 9);
        let other: Arc<[GuideRow]> = rows(5);
        s.set_rows(other);
        assert_eq!(s.focus_row, 0);
        let mut empty = EpgGridState::new(Arc::from(Vec::new()), T0);
        let mut r = Fake { calls: 0 };
        assert_eq!(empty.handle(Action::Down, &mut r, &mut big(), T0), GridOutcome::None);
        assert!(empty.focused().is_none());
    }

    #[test]
    fn cells_cover_gaps() {
        let p = |s: i64, e: i64| ProgrammeSlim { id: ProgrammeId(s), start: s, stop: e, title: "x".into(), sub_title: None, flags: ProgrammeFlags::default() };
        let cells = cells_of(Some(&[p(10, 20), p(30, 40)]), 0, 50);
        let spans: Vec<(i64, i64, bool)> = cells.iter().map(|c| (c.start, c.stop, c.programme.is_some())).collect();
        assert_eq!(spans, [(0, 10, false), (10, 20, true), (20, 30, false), (30, 40, true), (40, 50, false)]);
        assert_eq!(cells_of(Some(&[]), 0, 50).len(), 1);
        assert_eq!(cells_of(None, 0, 50).len(), 1);
    }

    #[test]
    fn formatting() {
        assert_eq!(hhmm(T0, true).len(), 5);
        assert!(hhmm(T0, false).ends_with('m'));
        assert_eq!(time_left(34 * 60), "34 min left");
        assert_eq!(time_left(65 * 60), "1 h 05 left");
        assert_eq!(excerpt("one two three", 7), "one two…");
        assert!(day_label(T0).split(' ').count() == 3, "{}", day_label(T0));
    }
}
