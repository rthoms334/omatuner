//! Guide fixes (DESIGN §31; the EPG mapping screen of §9.4 grown up). One screen, three jobs:
//!
//! * **Dashboard**: per playlist, how many SHOWN channels have guide data
//!   (`guide_fix::coverage`, the numbers Settings ▸ EPG and Live use), how many to fix and how
//!   many are one-key fixes; the Rules card (per playlist: "+1/+2 follow their base", "Copies
//!   share a guide", the catch-up offset); one chip per issue kind (a click filters).
//! * **Issues** (left pane): the channels of the chosen view (an issue kind, every issue, every
//!   channel, or Recent changes) in the Live TV playlist scope (Alt+P, Ctrl+PgDn / Ctrl+PgUp),
//!   each with its proposed fix and confidence; Space / Insert / a click on the box marks,
//!   Ctrl+A marks all shown, Shift+Enter (or a double-click) accepts — one channel at once,
//!   more after a confirm card with the before/after now; Delete resets to automatic; Ctrl+Z
//!   undoes the last batch (every write is a logged batch, `guide_fix::ui::submit_apply`).
//! * **Detail** (right pane): the channel's state and proposal with a before/after now/next
//!   preview (`guide_fix::preview`, recomputed as the shift is nudged), the other guide channels
//!   (lazy fuzzy suggestions for the rows drawn, then every guide channel; type to search; Enter
//!   assigns), and the Adjust rows: time shift (±15 min, Ctrl+Shift+←/→ ±1 h), catch-up offset
//!   (±1 min), Check ▶ (plays the last finished programme from its archive) and "Use for all
//!   ‹playlist› channels…". Shift and offset changes preview at once and are written 400 ms
//!   after the last press (one undo entry), or when the cursor leaves the channel.
//!
//! Keys: contexts `[EpgMapping, List | ListFiltering]` on the lists, `[EpgMapping, Panel]` in
//! Adjust; Tab cycles Issues → Guide channels → Adjust; → / Enter on an issue opens its guide
//! channels, ← returns. Esc: clear the filter → back to Issues → clear the marks → leave.
//! The issue report is read and classified off the UI thread (`guide_fix::ui::load_report`)
//! after each library build and after the guide check (`DataScope::GuideIssues`). Locked
//! channels are listed (dimmed) and editable; Check ▶ and "Watch to check" ask for the PIN.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::guide_fix::catalog::Catalog;
use crate::guide_fix::preview::PreviewRow;
use crate::guide_fix::ui::{self as gf, Report, kind_label, method_label, offset_text, shift_text};
use crate::keymap::{Action, KeyContext};
use crate::model::{
    ChannelId, EpgSuggestion, FixActor, FixChange, GuideRules, GuideTarget, IssueKind, ItemKind, ItemRef, NowNext, Proposal, SourceId,
};
use crate::msg::{AppEvent, DataScope, Toast};
use crate::store::Ticket;
use crate::store::guide_fix::{ApplyReport, GuideOpts, LogEntry, LoggedChange};
use crate::store::library::Library;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::item_menu::entry;
use crate::ui::overlays::{ConfirmSpec, ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::scope::{Scope, ScopeFilter, ScopeKind};
use crate::ui::screens::live::{choice_action, choice_index};
use crate::ui::widgets::button::{ButtonStyle, back_button, button};
use crate::ui::widgets::list::{ListOutcome, ListState, paint_row_state, virtual_list};
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, section};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};

/// Lazy suggestions per channel, and channels per batch.
const SUGGESTIONS: usize = 5;
const BATCH: usize = 40;
/// Shift / offset presses coalesce into one write this long after the last one.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// The screen lets go of its report and catalogue when it hasn't drawn for this long (it
/// repaints every [`ALIVE`] while shown).
const IDLE_RELEASE: Duration = Duration::from_secs(10);
const ALIVE: Duration = Duration::from_secs(4);
/// Changes listed by name in the confirm card.
const CONFIRM_LINES: usize = 6;
/// Steps: time shift (minutes) in Adjust and on the lists, catch-up offset (seconds).
const SHIFT_STEP: i32 = 15;
const SHIFT_HOUR: i32 = 60;
const OFFSET_STEP: i64 = 60;
const SHIFT_LIMIT: i32 = 24 * 60;
const OFFSET_LIMIT: i64 = 24 * 3600;

/// Confirm tokens.
const T_ACCEPT: u64 = 0x6766_0001;
const T_RESET: u64 = 0x6766_0002;
const T_RULE: u64 = 0x6766_0003;
const T_USE_ALL: u64 = 0x6766_0004;
const T_CLAUDE: u64 = 0x6766_0005;

/// Menu choices (`live::choice_action` indexes). `C_KIND + kind` picks an issue kind;
/// `C_RULE + 2·i (+1)` toggles playlist `i`'s +1 (copies) rule.
const C_KIND: usize = 0;
const C_ALL_ISSUES: usize = 7;
const C_ALL_CHANNELS: usize = 8;
const C_RECENT: usize = 9;
const C_HIDDEN: usize = 10;
const C_SETTINGS: usize = 11;
const C_PICK: usize = 12;
const C_CATCHUP: usize = 13;
const C_WATCH: usize = 14;
const C_DISMISS: usize = 15;
const C_PLAYLIST: usize = 16;
const C_CHECK: usize = 17;
const C_USE_ALL: usize = 18;
const C_RULE: usize = 32;

/// What the left pane lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// Channels with an issue (of one kind).
    Issues(Option<IssueKind>),
    /// Every channel (to re-map one that has a guide channel).
    All,
    /// The logged batches ("Recent changes").
    Recent,
}

impl Default for View {
    fn default() -> Self {
        View::Issues(None)
    }
}

impl View {
    fn label(self) -> &'static str {
        match self {
            View::Issues(Some(k)) => kind_label(k),
            View::Issues(None) => "All issues",
            View::All => "All channels",
            View::Recent => "Recent changes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    #[default]
    Issues,
    Guide,
    Adjust,
}

/// The Adjust rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adj {
    Shift,
    Catchup,
    Check,
    UseForAll,
}

/// A row of the guide channels pane.
#[derive(Debug, Clone, PartialEq)]
enum Cand {
    Suggestion(EpgSuggestion),
    Entry(usize),
}

/// The fix a row proposes (from the report, or a lazy suggestion while the guide check hasn't
/// covered the channel yet).
#[derive(Debug, Clone, PartialEq)]
struct Fix {
    change: FixChange,
    confidence: f32,
    why: String,
}

/// Shift / offset presses not written yet.
#[derive(Debug, Clone, PartialEq)]
struct PendingAdjust {
    /// The channels and their effective shift when the first press came.
    targets: Vec<(ChannelId, i32)>,
    delta: i32,
    /// The highlighted channel's new catch-up offset (seconds).
    catchup: Option<i64>,
    due: Instant,
}

/// A change waiting for the confirm card's answer.
struct Pending {
    token: u64,
    proposal: Proposal,
    label: String,
    toast: String,
    /// Claude's batch over 50 channels (`AgentCommand::GuideFixReview`), else the user's.
    actor: FixActor,
}

type PreviewKey = (ChannelId, FixChange);

#[derive(Default)]
pub struct EpgMappingScreen {
    view: View,
    pane: Pane,
    include_hidden: bool,
    list: ListState,
    rows: Vec<ChannelId>,
    recent: Vec<LogEntry>,
    recent_loader: Option<Loader<Vec<LogEntry>>>,
    batch_changes: Option<(i64, Vec<LoggedChange>)>,
    batch_loader: Option<(i64, Loader<Vec<LoggedChange>>)>,
    marks: HashSet<ChannelId>,
    /// The last row clicked or marked (Shift+click marks the range to it).
    anchor: Option<usize>,
    cands: ListState,
    cand_rows: Vec<Cand>,
    adj: usize,
    report: Option<Arc<Report>>,
    /// (library generation, include_hidden, epoch) the loader runs for.
    report_loader: Option<((u64, bool, u64), Loader<Report>)>,
    report_key: Option<(u64, bool, u64)>,
    /// Bumped by `DataScope::GuideIssues` (a guide check finished).
    epoch: u64,
    catalog: Option<Arc<Catalog>>,
    catalog_loader: Option<Loader<Arc<Catalog>>>,
    suggestions: HashMap<ChannelId, Vec<EpgSuggestion>>,
    suggest_rx: Option<tokio::sync::oneshot::Receiver<HashMap<ChannelId, Vec<EpgSuggestion>>>>,
    /// Channel rows drawn last frame (suggestions are computed for these).
    visible: Vec<ChannelId>,
    preview: Option<(PreviewKey, PreviewRow)>,
    preview_loader: Option<(PreviewKey, Loader<Vec<PreviewRow>>)>,
    adjust: Option<PendingAdjust>,
    /// Shifts written by the last flush, for the library build they were written on (the next
    /// presses start from them until the rebuild shows them).
    written: (u64, HashMap<ChannelId, i32>),
    /// The last write; Check ▶ waits for it (the archive must use the offset just set).
    writing: Option<Ticket<ApplyReport>>,
    check_after: Option<ChannelId>,
    confirm: Option<Pending>,
    confirm_loader: Option<(Pending, Loader<Vec<PreviewRow>>)>,
    rows_dirty: bool,
    scope: ScopeFilter,
    preselect: Option<ChannelId>,
    last_frame: Option<Instant>,
}

/// Fuzzy suggestions for (channel, name) pairs across every guide (blocking pool).
fn compute_suggestions(cat: &Catalog, names: Vec<(ChannelId, String)>, threshold: f64) -> HashMap<ChannelId, Vec<EpgSuggestion>> {
    names.into_iter().map(|(id, name)| (id, cat.suggest(&name, SUGGESTIONS, threshold, None))).collect()
}

fn channel_ref(ctx: &UiCtx<'_>, id: ChannelId) -> Option<ItemRef> {
    ctx.lib.live.get(id).map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", rows::grouped(n), if n == 1 { one } else { many })
}

/// "→ E4 · +1 h" (what a fix does, short).
fn fix_text(c: &FixChange) -> String {
    let mut parts = Vec::new();
    match &c.map {
        Some(Some(t)) => parts.push(t.name.clone()),
        Some(None) => parts.push("automatic guide".into()),
        None => {}
    }
    match c.shift_min {
        Some(Some(m)) => parts.push(shift_text(m)),
        Some(None) => parts.push("automatic shift".into()),
        None => {}
    }
    if c.dismiss.is_some() {
        parts.push("it's right".into());
    }
    parts.join(" · ")
}

/// "20:30 Hollyoaks" or "—".
fn slot_text(p: &Option<crate::model::ProgrammeSlim>, h24: bool) -> String {
    match p {
        Some(p) => format!("{} {}", crate::ui::widgets::time::hhmm(p.start, h24), p.title),
        None => "—".into(),
    }
}

/// One line of the confirm card: "BBC Four HD: — no guide — → Timewatch".
fn confirm_line(name: &str, r: &PreviewRow) -> String {
    let now = |nn: &NowNext| nn.now.as_ref().map_or_else(|| "— no guide —".to_owned(), |p| p.title.to_string());
    format!("{name}: {} → {}", now(&r.before), now(&r.after))
}

/// A single-line galley truncated with "…".
fn fit(painter: &egui::Painter, text: &str, font: egui::FontId, color: egui::Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    painter.layout_job(job)
}

/// The fix a channel's row proposes: the report's, else (no guide channel, no cached
/// suggestion yet) the lazy suggestion at or above the threshold — the old fast path.
fn fix_for(
    lib: &Library,
    report: Option<&Report>,
    suggestions: &HashMap<ChannelId, Vec<EpgSuggestion>>,
    threshold: f64,
    ch: ChannelId,
) -> Option<Fix> {
    let row = report.and_then(|r| r.row(ch));
    if let Some(r) = row
        && let Some(p) = &r.proposal
    {
        return Some(Fix { change: p.clone(), confidence: r.confidence, why: r.why.clone() });
    }
    let c = lib.live.get(ch)?;
    let lazy = row.is_none_or(|r| r.kind == IssueKind::NoGuide) && c.epg_channel_id.is_none();
    let s = suggestions.get(&ch)?.first().filter(|s| lazy && f64::from(s.score) >= threshold)?;
    let target = GuideTarget { epg_source: s.epg_source_id, xmltv_id: s.xmltv_id.clone(), name: s.display_name.clone() };
    Some(Fix {
        change: FixChange {
            channel: Some(ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }),
            map: Some(Some(target)),
            ..Default::default()
        },
        confidence: s.score,
        why: "similar name in the guide".into(),
    })
}

/// What a channel's issue row shows (computed for the rows drawn only).
fn row_look(
    lib: &Library,
    report: Option<&Report>,
    suggestions: &HashMap<ChannelId, Vec<EpgSuggestion>>,
    threshold: f64,
    marked: bool,
    multi: bool,
    id: ChannelId,
) -> RowLook {
    let c = lib.live.get(id);
    let issue = report.and_then(|r| r.row(id));
    let fix = fix_for(lib, report, suggestions, threshold, id);
    let (glyph, target, conf) = match (&fix, issue) {
        (Some(f), _) => {
            let conf = if f.confidence >= 1.0 { "✓".to_owned() } else { format!("{:.0} %", f.confidence * 100.0) };
            (glyphs::WARNING, format!("→ {}", fix_text(&f.change)), conf)
        }
        (None, Some(r)) => (glyphs::WARNING, r.why.clone(), String::new()),
        (None, None) => match c.and_then(|c| c.epg_channel_id) {
            Some(_) => (glyphs::CHECK, "has guide data".into(), String::new()),
            None => (glyphs::WARNING, "no guide data".into(), String::new()),
        },
    };
    RowLook {
        name: c.map(|c| c.name.to_string()).unwrap_or_default(),
        glyph,
        target,
        conf,
        marked,
        locked: c.is_some_and(|c| c.locked),
        code: if multi { c.and_then(|c| lib.source(c.source_id)).map(|s| s.code.clone()) } else { None },
    }
}

impl EpgMappingScreen {
    fn highlighted(&self) -> Option<ChannelId> {
        if self.view == View::Recent {
            return None;
        }
        self.rows.get(self.list.cursor).copied()
    }

    fn opts(ctx: &UiCtx<'_>) -> GuideOpts {
        GuideOpts::from_config(ctx.config)
    }

    // ------------------------------------------------------------------ loading

    fn report_key(&self, ctx: &UiCtx<'_>) -> (u64, bool, u64) {
        (ctx.lib.generation, self.include_hidden, self.epoch)
    }

    fn pump_report(&mut self, ctx: &mut UiCtx<'_>) {
        if let Some((key, l)) = &mut self.report_loader {
            match l.poll() {
                Some(Ok(r)) => {
                    let key = *key;
                    self.report_loader = None;
                    if !r.include_hidden {
                        gf::note_to_fix(r.generation, r.to_fix());
                    }
                    // the cache is behind (a playlist refresh, a new threshold…): check now
                    if !r.fresh && crate::guide_fix::ENABLED && !Self::checking(ctx) {
                        ctx.services.jobs.submit(crate::jobs::JobKind::GuideCheck);
                    }
                    self.report = Some(Arc::new(r));
                    self.report_key = Some(key);
                    self.rows_dirty = true;
                    if self.view == View::Recent {
                        self.recent_loader = Some(Loader::start(ctx, |c| crate::store::guide_fix::recent(c, 200)));
                    }
                }
                Some(Err(e)) => {
                    self.report_key = Some(*key);
                    self.report_loader = None;
                    kit::error_toast(ctx, "Couldn't check the guide data", &e);
                }
                None => {}
            }
        }
        let want = self.report_key(ctx);
        let loading = self.report_loader.as_ref().is_some_and(|(k, _)| *k == want);
        if self.report_key != Some(want) && !loading && ctx.lib.built_at != 0 {
            let lib = Arc::clone(ctx.lib);
            let (opts, hidden, now) = (Self::opts(ctx), self.include_hidden, ctx.now);
            self.report_loader = Some((want, Loader::start(ctx, move |c| gf::load_report(c, &lib, opts, hidden, true, now))));
        }
    }

    /// A guide check is running or waiting out its debounce (two cheap reads a frame).
    fn checking(ctx: &UiCtx<'_>) -> bool {
        let state = &ctx.services.jobs.context().guide_check;
        state.pending() || state.running()
    }

    fn reload_catalog(&mut self, ctx: &UiCtx<'_>) {
        self.catalog_loader = Some(Loader::start(ctx, crate::guide_fix::catalog::shared));
        self.suggestions.clear();
    }

    /// Start a suggestion batch for drawn channels without guide data or cached suggestions.
    fn pump_suggestions(&mut self, ctx: &UiCtx<'_>) {
        if let Some(rx) = &mut self.suggest_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.suggestions.extend(r);
                    self.suggest_rx = None;
                    self.refilter_cands();
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                Err(_) => self.suggest_rx = None,
            }
        }
        let Some(cat) = self.catalog.clone() else { return };
        if cat.indexes.is_empty() {
            return;
        }
        let names: Vec<(ChannelId, String)> = self
            .visible
            .iter()
            .chain(self.highlighted().iter())
            .filter(|id| !self.suggestions.contains_key(id))
            .filter_map(|id| ctx.lib.live.get(*id).map(|c| (*id, c.name.to_string())))
            .collect::<HashSet<_>>()
            .into_iter()
            .take(BATCH)
            .collect();
        if names.is_empty() {
            return;
        }
        let threshold = ctx.config.epg.fuzzy_threshold;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let bus = ctx.services.bus.clone();
        ctx.services.rt.spawn_blocking(move || {
            let _ = tx.send(compute_suggestions(&cat, names, threshold));
            bus.request_repaint();
        });
        self.suggest_rx = Some(rx);
    }

    /// The before/after of the highlighted channel's change (the pending shift, else its
    /// proposal), else its state now.
    fn pump_preview(&mut self, ctx: &UiCtx<'_>) {
        if let Some((key, l)) = &mut self.preview_loader
            && let Some(r) = l.poll()
        {
            let key = key.clone();
            self.preview_loader = None;
            match r {
                Ok(mut v) if !v.is_empty() => self.preview = Some((key, v.swap_remove(0))),
                Ok(_) => {}
                Err(e) => tracing::debug!("guide fix preview: {e}"),
            }
        }
        let Some(key) = self.preview_key(ctx) else { return };
        let have = self.preview.as_ref().is_some_and(|(k, _)| *k == key);
        let loading = self.preview_loader.as_ref().is_some_and(|(k, _)| *k == key);
        if !have && !loading {
            let (change, now) = (key.1.clone(), ctx.now);
            self.preview_loader = Some((key, Loader::start(ctx, move |c| crate::guide_fix::preview::preview(c, &[change], now))));
        }
    }

    fn preview_key(&self, ctx: &UiCtx<'_>) -> Option<PreviewKey> {
        let ch = self.highlighted()?;
        let r = channel_ref(ctx, ch)?;
        let change = match self.pending_change(ch, ctx) {
            Some(c) => c,
            None => self.fix_of(ctx, ch).map(|f| f.change).unwrap_or_else(|| FixChange { channel: Some(r), ..Default::default() }),
        };
        Some((ch, change))
    }

    fn pump_recent(&mut self, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.recent_loader) {
            Some(Ok(v)) => {
                self.recent = v;
                self.list.set_len(self.recent.len());
                self.batch_changes = None;
            }
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read the recent guide fixes", &e),
            None => {}
        }
        if self.view != View::Recent {
            return;
        }
        if let Some((id, l)) = &mut self.batch_loader
            && let Some(r) = l.poll()
        {
            let id = *id;
            self.batch_loader = None;
            match r {
                Ok(v) => self.batch_changes = Some((id, v)),
                Err(e) => tracing::debug!("guide fix changes: {e}"),
            }
        }
        let Some(e) = self.recent.get(self.list.cursor) else { return };
        let id = e.id.0;
        if self.batch_changes.as_ref().is_none_or(|(b, _)| *b != id) && self.batch_loader.as_ref().is_none_or(|(b, _)| *b != id) {
            let log = e.id;
            self.batch_loader = Some((id, Loader::start(ctx, move |c| crate::store::guide_fix::changes_of(c, log))));
        }
    }

    /// The last write finished: Check ▶ may play now.
    fn pump_writing(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(t) = &mut self.writing else { return };
        let Some(r) = t.try_take() else { return };
        self.writing = None;
        if let Err(e) = r {
            kit::error_toast(ctx, "Couldn't save the guide fix", &e);
            self.check_after = None;
            return;
        }
        if let Some(ch) = self.check_after.take() {
            self.check(ctx, ch);
        }
    }

    fn pump_confirm(&mut self, ctx: &mut UiCtx<'_>) {
        let Some((_, l)) = &mut self.confirm_loader else { return };
        let Some(r) = l.poll() else { return };
        let Some((pending, _)) = self.confirm_loader.take() else { return };
        let previews = r.unwrap_or_else(|e| {
            tracing::debug!("guide fix preview: {e}");
            Vec::new()
        });
        let changes = &pending.proposal.changes;
        let maps = changes.iter().filter(|c| matches!(c.map, Some(Some(_)))).count();
        let shifts = changes.iter().filter(|c| matches!(c.shift_min, Some(Some(_)))).count();
        let locked = changes
            .iter()
            .filter_map(|c| c.channel.as_ref())
            .filter(|r| ctx.lib.live.by_ref(r).and_then(|id| ctx.lib.live.get(id)).is_some_and(|c| c.locked))
            .count();
        let claude = pending.actor == FixActor::Claude;
        let mut body = String::new();
        if claude {
            body.push_str("Claude proposed these in chat.\n");
        }
        body.push_str(&format!(
            "{} · {} · {} locked",
            plural(maps, "guide channel", "guide channels"),
            plural(shifts, "time shift", "time shifts"),
            locked
        ));
        if !pending.proposal.rules.is_empty() {
            body.push_str(&format!(" · {}", plural(pending.proposal.rules.len(), "playlist rule", "playlist rules")));
        }
        body.push_str("\n\n");
        for (c, p) in changes.iter().zip(previews.iter()).take(CONFIRM_LINES) {
            let name = c.channel.as_ref().and_then(|r| ctx.lib.live.by_ref(r)).and_then(|id| ctx.lib.live.get(id)).map(|c| c.name.to_string());
            body.push_str(&confirm_line(name.as_deref().unwrap_or("?"), p));
            body.push('\n');
        }
        if changes.len() > CONFIRM_LINES {
            body.push_str(&format!("… {} more\n", rows::grouped(changes.len() - CONFIRM_LINES)));
        }
        body.push_str("\nUndo: Ctrl+Z in Guide fixes");
        let n = changes.len();
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: if claude { format!("Accept Claude's {} fixes?", rows::grouped(n)) } else { format!("Accept {} fixes?", rows::grouped(n)) },
            body,
            confirm_label: format!("Accept {}", rows::grouped(n)),
            destructive: false,
            token: pending.token,
        }));
        self.confirm = Some(pending);
    }

    // ------------------------------------------------------------------ rows

    /// Recompute the rows (view, scope, report, filter).
    fn refilter(&mut self, ctx: &UiCtx<'_>) {
        self.rows_dirty = false;
        if self.view == View::Recent {
            self.rows.clear();
            self.list.set_len(self.recent.len());
            return;
        }
        let keep = self.highlighted();
        // Ctrl+E on a channel: its issue, else every channel
        if let Some(p) = self.preselect
            && let Some(rep) = &self.report
        {
            match rep.row(p) {
                Some(r) if !matches!(self.view, View::Issues(Some(k)) if k == r.kind) => self.view = View::Issues(None),
                None if self.view != View::All => self.view = View::All,
                _ => {}
            }
            self.list.filter.clear();
        }
        let scope = &self.scope;
        let lib = ctx.lib;
        let ids: Vec<ChannelId> = match (self.view, &self.report) {
            (View::Issues(kind), Some(rep)) => rep
                .issues
                .rows
                .iter()
                .filter(|r| kind.is_none_or(|k| r.kind == k))
                .map(|r| r.channel)
                .filter(|id| lib.live.get(*id).is_some_and(|c| scope.admits(c.source_id)))
                .collect(),
            (View::Issues(_), None) => Vec::new(),
            (View::All, _) => lib
                .live
                .channels
                .iter()
                .filter(|c| !c.unsupported && (self.include_hidden || !lib.live.hidden.contains(&c.id)) && scope.admits(c.source_id))
                .map(|c| c.id)
                .collect(),
            (View::Recent, _) => Vec::new(),
        };
        let hay: Vec<String> = ids
            .iter()
            .map(|id| {
                let c = lib.live.get(*id);
                let fix = self.fix_of(ctx, *id).map(|f| fix_text(&f.change)).unwrap_or_default();
                c.map(|c| format!("{} {} {fix}", c.number.map(|n| n.to_string()).unwrap_or_default(), c.name)).unwrap_or_default()
            })
            .collect();
        self.rows = self.list.filter.apply(hay.iter().map(String::as_str)).into_iter().map(|i| ids[i as usize]).collect();
        self.list.set_len(self.rows.len());
        let shown: HashSet<ChannelId> = self.rows.iter().copied().collect();
        self.marks.retain(|m| shown.contains(m));
        let want = if self.report.is_some() { self.preselect.take() } else { None }.or(keep);
        if let Some(i) = want.and_then(|w| self.rows.iter().position(|c| *c == w)) {
            self.list.select(i);
        }
        self.refilter_cands();
    }

    /// Guide channels: the highlighted channel's suggestions, then every guide channel.
    fn refilter_cands(&mut self) {
        let Some(cat) = &self.catalog else {
            self.cand_rows.clear();
            self.cands.set_len(0);
            return;
        };
        let mut v = Vec::new();
        if !self.cands.filter.is_active()
            && let Some(s) = self.highlighted().and_then(|h| self.suggestions.get(&h))
        {
            v.extend(s.iter().cloned().map(Cand::Suggestion));
        }
        v.extend(self.cands.filter.apply(cat.hay.iter().map(String::as_str)).into_iter().map(|i| Cand::Entry(i as usize)));
        self.cand_rows = v;
        self.cands.set_len(self.cand_rows.len());
    }

    /// The fix a channel's row proposes.
    fn fix_of(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> Option<Fix> {
        fix_for(ctx.lib, self.report.as_deref(), &self.suggestions, ctx.config.epg.fuzzy_threshold, ch)
    }

    /// The marked channels in list order, else the highlighted one.
    fn targets(&self) -> Vec<ChannelId> {
        if self.marks.is_empty() {
            self.highlighted().into_iter().collect()
        } else {
            self.rows.iter().copied().filter(|c| self.marks.contains(c)).collect()
        }
    }

    fn name(ctx: &UiCtx<'_>, ch: ChannelId) -> String {
        ctx.lib.live.get(ch).map(|c| c.name.to_string()).unwrap_or_default()
    }

    // ------------------------------------------------------------------ writes

    fn apply(&mut self, ctx: &mut UiCtx<'_>, proposal: Proposal, label: String, toast: String) {
        self.apply_as(ctx, FixActor::You, proposal, label, toast);
    }

    fn apply_as(&mut self, ctx: &mut UiCtx<'_>, actor: FixActor, proposal: Proposal, label: String, toast: String) {
        self.flush_adjust(ctx);
        // this write may change the shifts the last flush wrote
        self.written.1.clear();
        self.writing = Some(gf::submit_apply(ctx.services, actor, label, toast, proposal, Self::opts(ctx), ctx.now));
    }

    /// Claude's batch over 50 channels (`AgentCommand::GuideFixReview`): the confirm card,
    /// prefilled like Shift+Enter's (DESIGN §31.5). It waits while another card is open.
    fn pump_review(&mut self, ctx: &mut UiCtx<'_>) {
        if self.confirm.is_some() || self.confirm_loader.is_some() {
            return;
        }
        let Some(r) = gf::take_review() else { return };
        if r.expires <= ctx.now {
            ctx.toast(Toast::info("Claude's guide fixes waited too long — ask Claude to propose them again"));
            return;
        }
        let n = r.proposal.changes.len();
        let changes = r.proposal.changes.clone();
        let pending =
            Pending { token: T_CLAUDE, proposal: r.proposal, label: gf::claude_label(n), toast: gf::claude_fixed_text(n), actor: FixActor::Claude };
        let now = ctx.now;
        self.confirm_loader = Some((pending, Loader::start(ctx, move |c| crate::guide_fix::preview::preview(c, &changes, now))));
    }

    /// Shift+Enter / double-click / "Accept": one channel at once, more after the card.
    fn accept(&mut self, ctx: &mut UiCtx<'_>) {
        let fixes: Vec<(ChannelId, Fix)> = self.targets().into_iter().filter_map(|ch| self.fix_of(ctx, ch).map(|f| (ch, f))).collect();
        match fixes.as_slice() {
            [] => ctx.toast(Toast::info("No proposed fix — pick a guide channel (Enter)")),
            [(ch, f)] => {
                let toast = format!("{}: {}", Self::name(ctx, *ch), fix_text(&f.change));
                self.apply(ctx, Proposal { changes: vec![f.change.clone()], ..Default::default() }, "Accepted 1 fix".into(), toast);
                self.after_write();
            }
            many => {
                let n = many.len();
                let changes: Vec<FixChange> = many.iter().map(|(_, f)| f.change.clone()).collect();
                let pending = Pending {
                    token: T_ACCEPT,
                    proposal: Proposal { changes: changes.clone(), ..Default::default() },
                    label: format!("Accepted {n} fixes"),
                    toast: format!("Fixed the guide of {} channels", rows::grouped(n)),
                    actor: FixActor::You,
                };
                let now = ctx.now;
                self.confirm_loader = Some((pending, Loader::start(ctx, move |c| crate::guide_fix::preview::preview(c, &changes, now))));
            }
        }
    }

    /// A write landed: marks go, the cursor moves on (the report reloads after the rebuild).
    fn after_write(&mut self) {
        self.marks.clear();
        self.preview = None;
        if self.list.cursor + 1 < self.rows.len() && self.view != View::All {
            self.list.select(self.list.cursor + 1);
        }
    }

    /// Delete: back to automatic (guide channel and time shift).
    fn reset(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(rep) = self.report.clone() else { return };
        let changes: Vec<FixChange> = self
            .targets()
            .into_iter()
            .filter_map(|ch| {
                let e = rep.extras.get(&ch)?;
                let change = FixChange {
                    channel: channel_ref(ctx, ch),
                    map: e.manual.as_ref().map(|_| None),
                    shift_min: e.shift_min.map(|_| None),
                    ..Default::default()
                };
                (change.map.is_some() || change.shift_min.is_some()).then_some(change)
            })
            .collect();
        match changes.len() {
            0 => ctx.toast(Toast::info("Nothing to reset — no guide channel or time shift of yours")),
            1 => {
                let name = changes[0].channel.as_ref().and_then(|r| ctx.lib.live.by_ref(r)).map(|id| Self::name(ctx, id)).unwrap_or_default();
                let toast = format!("{name}: automatic matching applies again");
                self.apply(ctx, Proposal { changes, ..Default::default() }, "Reset to automatic".into(), toast);
                self.preview = None;
            }
            n => {
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Reset {} channels?", rows::grouped(n)),
                    body: "Their guide channel and time shift go back to automatic.\n\nUndo: Ctrl+Z in Guide fixes".into(),
                    confirm_label: "Reset".into(),
                    destructive: true,
                    token: T_RESET,
                }));
                self.confirm = Some(Pending {
                    token: T_RESET,
                    proposal: Proposal { changes, ..Default::default() },
                    label: format!("Reset {n} channels"),
                    toast: format!("{} channels back to automatic", rows::grouped(n)),
                    actor: FixActor::You,
                });
            }
        }
    }

    /// "It's right": dismiss the issue of the marked / highlighted channels.
    fn dismiss(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(rep) = self.report.clone() else { return };
        let changes: Vec<FixChange> = self
            .targets()
            .into_iter()
            .filter_map(|ch| rep.row(ch).map(|r| FixChange { channel: channel_ref(ctx, ch), dismiss: Some(r.kind), ..Default::default() }))
            .collect();
        if changes.is_empty() {
            ctx.toast(Toast::info("Nothing to dismiss here"));
            return;
        }
        let n = changes.len();
        let toast = if n == 1 { "Dismissed — it stays as it is".to_owned() } else { format!("Dismissed {} issues", rows::grouped(n)) };
        self.apply(ctx, Proposal { changes, ..Default::default() }, format!("Dismissed {n} issues"), toast);
        self.after_write();
    }

    /// Assign the highlighted guide channel (Enter in the guide channels pane).
    fn assign(&mut self, ctx: &mut UiCtx<'_>) {
        let (Some(ch), Some(cand)) = (self.highlighted(), self.cand_rows.get(self.cands.cursor).cloned()) else { return };
        let target = match cand {
            Cand::Suggestion(s) => GuideTarget { epg_source: s.epg_source_id, xmltv_id: s.xmltv_id, name: s.display_name },
            Cand::Entry(i) => match self.catalog.as_ref().and_then(|c| c.entries.get(i)) {
                Some(e) => GuideTarget { epg_source: e.source, xmltv_id: e.xmltv_id.clone(), name: e.name.clone() },
                None => return,
            },
        };
        let toast = format!("{} now uses the guide of {}", Self::name(ctx, ch), target.name);
        let change = FixChange { channel: channel_ref(ctx, ch), map: Some(Some(target)), ..Default::default() };
        self.apply(ctx, Proposal { changes: vec![change], ..Default::default() }, "Picked a guide channel".into(), toast);
        self.pane = Pane::Issues;
        self.after_write();
    }

    // ------------------------------------------------------------------ adjust

    /// A channel's effective shift: what the last flush wrote while the library doesn't show it
    /// yet, else the snapshot's.
    fn shift_base(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> i32 {
        match self.written.1.get(&ch) {
            Some(v) if self.written.0 == ctx.lib.generation => *v,
            _ => ctx.lib.live.get(ch).map_or(0, |c| c.tvg_shift_min),
        }
    }

    /// The highlighted channel's shift / offset as shown (pending presses included).
    fn shift_shown(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> i32 {
        let base = self.shift_base(ctx, ch);
        match &self.adjust {
            Some(a) => a.targets.iter().find(|(c, _)| *c == ch).map_or(base, |(_, b)| b + a.delta),
            None => base,
        }
    }

    /// The catch-up offset the channel uses now: its own, else its playlist's, else 0.
    fn offset_now(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> i64 {
        let rep = self.report.as_ref();
        let own = rep.and_then(|r| r.extras.get(&ch)).and_then(|e| e.catchup_offset_s);
        own.or_else(|| self.playlist_offset(ctx, ch)).unwrap_or(0)
    }

    fn playlist_offset(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> Option<i64> {
        let src = ctx.lib.live.get(ch)?.source_id;
        self.report.as_ref()?.rules.get(&src)?.catchup_offset_s
    }

    fn offset_shown(&self, ctx: &UiCtx<'_>, ch: ChannelId) -> i64 {
        self.adjust
            .as_ref()
            .filter(|a| a.targets.first().is_some_and(|(c, _)| *c == ch))
            .and_then(|a| a.catchup)
            .unwrap_or_else(|| self.offset_now(ctx, ch))
    }

    /// The pending change of `ch` as a fix (the preview's "after").
    fn pending_change(&self, ch: ChannelId, ctx: &UiCtx<'_>) -> Option<FixChange> {
        let a = self.adjust.as_ref()?;
        let (_, base) = a.targets.iter().find(|(c, _)| *c == ch)?;
        let mut change = FixChange { channel: channel_ref(ctx, ch), ..Default::default() };
        if a.delta != 0 {
            change.shift_min = Some(Some(base + a.delta));
        }
        if a.targets.first().is_some_and(|(c, _)| *c == ch) {
            change.catchup_offset_s = a.catchup.map(Some);
        }
        Some(change)
    }

    /// Nudge the time shift of `targets` by `delta` minutes (written after the debounce).
    fn nudge_shift(&mut self, ctx: &mut UiCtx<'_>, targets: Vec<ChannelId>, delta: i32) {
        if targets.is_empty() {
            return;
        }
        if self.adjust.as_ref().is_some_and(|a| a.targets.iter().map(|(c, _)| *c).ne(targets.iter().copied())) {
            self.flush_adjust(ctx);
        }
        let bases: Vec<(ChannelId, i32)> = targets.iter().map(|c| (*c, self.shift_base(ctx, *c))).collect();
        let a = self.adjust.get_or_insert_with(|| PendingAdjust { targets: bases, delta: 0, catchup: None, due: Instant::now() });
        let lo = a.targets.iter().map(|(_, b)| -SHIFT_LIMIT - b).max().unwrap_or(-SHIFT_LIMIT);
        let hi = a.targets.iter().map(|(_, b)| SHIFT_LIMIT - b).min().unwrap_or(SHIFT_LIMIT);
        a.delta = (a.delta + delta).clamp(lo, hi);
        a.due = Instant::now() + DEBOUNCE;
    }

    /// Nudge the highlighted channel's catch-up offset by `delta` seconds.
    fn nudge_offset(&mut self, ctx: &mut UiCtx<'_>, delta: i64) {
        let Some(ch) = self.highlighted() else { return };
        if self.adjust.as_ref().is_some_and(|a| a.targets.len() != 1 || a.targets[0].0 != ch) {
            self.flush_adjust(ctx);
        }
        let now_off = self.offset_now(ctx, ch);
        let base = self.shift_base(ctx, ch);
        let a = self.adjust.get_or_insert_with(|| PendingAdjust { targets: vec![(ch, base)], delta: 0, catchup: None, due: Instant::now() });
        a.catchup = Some((a.catchup.unwrap_or(now_off) + delta).clamp(-OFFSET_LIMIT, OFFSET_LIMIT));
        a.due = Instant::now() + DEBOUNCE;
    }

    /// Write the pending presses as one batch.
    fn flush_adjust(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(a) = self.adjust.take() else { return };
        let mut changes = Vec::new();
        for (i, (ch, base)) in a.targets.iter().enumerate() {
            let mut change = FixChange { channel: channel_ref(ctx, *ch), ..Default::default() };
            if a.delta != 0 {
                change.shift_min = Some(Some(base + a.delta));
            }
            if i == 0
                && let Some(off) = a.catchup
            {
                // the playlist's offset (or none) is what "automatic" gives
                let auto = self.playlist_offset(ctx, *ch).unwrap_or(0);
                change.catchup_offset_s = Some((off != auto).then_some(off));
            }
            if change.shift_min.is_some() || change.catchup_offset_s.is_some() {
                changes.push(change);
            }
        }
        if changes.is_empty() {
            return;
        }
        let n = changes.len();
        let (label, toast) = match (a.delta, a.catchup) {
            (0, Some(off)) => ("Catch-up offset".to_owned(), format!("{}: catch-up offset {}", Self::name(ctx, a.targets[0].0), offset_text(off))),
            (d, _) if n == 1 => (
                format!("Time shift {}", shift_text(d)),
                format!("{}: time shift {}", Self::name(ctx, a.targets[0].0), shift_text(a.targets[0].1 + d)),
            ),
            (d, _) => {
                (format!("Time shift {} ({n} channels)", shift_text(d)), format!("Time shift {} for {} channels", shift_text(d), rows::grouped(n)))
            }
        };
        if a.delta != 0 {
            self.written = (ctx.lib.generation, a.targets.iter().map(|(c, b)| (*c, b + a.delta)).collect());
        }
        self.writing =
            Some(gf::submit_apply(ctx.services, FixActor::You, label, toast, Proposal { changes, ..Default::default() }, Self::opts(ctx), ctx.now));
    }

    fn adj_rows(&self, ctx: &UiCtx<'_>) -> Vec<Adj> {
        let Some(c) = self.highlighted().and_then(|h| ctx.lib.live.get(h)) else { return Vec::new() };
        let mut v = vec![Adj::Shift];
        if c.has_catchup {
            v.extend([Adj::Catchup, Adj::Check, Adj::UseForAll]);
        }
        v
    }

    /// Check ▶: the last finished programme from its archive (after the pending write).
    fn start_check(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(ch) = self.highlighted() else { return };
        self.flush_adjust(ctx);
        if self.writing.is_some() {
            self.check_after = Some(ch);
        } else {
            self.check(ctx, ch);
        }
    }

    fn check(&mut self, ctx: &mut UiCtx<'_>, ch: ChannelId) {
        let Some(c) = ctx.lib.live.get(ch) else { return };
        let (Some(epg), shift, days) = (c.epg_channel_id, c.tvg_shift_min, c.catchup_days.max(1)) else {
            ctx.toast(Toast::info("This channel has no guide data to check with"));
            return;
        };
        let now = ctx.now;
        let from = now - 12 * 3600 - i64::from(shift) * 60;
        let db = ctx.db;
        // one indexed read on a key press
        let window = crate::store::epg::window(db, epg, from, now).unwrap_or_default();
        let last =
            window.iter().map(|p| p.shifted(shift)).filter(|p| p.stop <= now && p.start >= now - i64::from(days) * 86_400).max_by_key(|p| p.stop);
        match last {
            Some(p) => crate::ui::screens::live::play_catchup(ctx, ch, &p, choice_action(C_CHECK)),
            None => ctx.toast(Toast::info("No finished programme in the guide to check with")),
        }
    }

    fn use_for_all(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(ch) = self.highlighted() else { return };
        let Some(c) = ctx.lib.live.get(ch) else { return };
        let off = self.offset_shown(ctx, ch);
        let src = c.source_id;
        let name = ctx.lib.source(src).map(|s| s.name.clone()).unwrap_or_else(|| "this playlist".into());
        let mut rules = self.report.as_ref().and_then(|r| r.rules.get(&src).copied()).unwrap_or_default();
        rules.catchup_offset_s = (off != 0).then_some(off);
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: format!("Use {} for all {name} channels?", offset_text(off)),
            body: "Catch-up starts this much later on every channel of the playlist. A channel's own offset still wins.".into(),
            confirm_label: "Use for all".into(),
            destructive: false,
            token: T_USE_ALL,
        }));
        self.confirm = Some(Pending {
            token: T_USE_ALL,
            proposal: Proposal { rules: vec![(src, rules)], ..Default::default() },
            label: "Playlist catch-up offset".into(),
            toast: format!("{name}: catch-up offset {}", offset_text(off)),
            actor: FixActor::You,
        });
    }

    /// Turn a playlist's rule on or off (after the card naming what it changes).
    fn toggle_rule(&mut self, ctx: &mut UiCtx<'_>, src: SourceId, copies: bool) {
        let Some(rep) = self.report.clone() else { return };
        let rules = rep.rules.get(&src).copied().unwrap_or_default();
        let default = ctx.config.epg.rules_default;
        let on = if copies { rules.copies_on(default) } else { rules.siblings_on(default) };
        let mut new = rules;
        if copies {
            new.share_copies = Some(!on);
        } else {
            new.timeshift_siblings = Some(!on);
        }
        let name = ctx.lib.source(src).map(|s| s.name.clone()).unwrap_or_else(|| "this playlist".into());
        let rule = if copies { "Copies share a guide" } else { "+1/+2 follow their base" };
        let kind = if copies { IssueKind::CopyHasGuide } else { IssueKind::PlusNoShift };
        let affected = if on {
            let (s, c) = rep.by_rules.get(&src).copied().unwrap_or_default();
            if copies { c } else { s }
        } else {
            rep.issues
                .rows
                .iter()
                .filter(|r| r.kind == kind && r.proposal.is_some() && ctx.lib.live.get(r.channel).is_some_and(|c| c.source_id == src))
                .count()
        };
        let body = if on {
            format!("{} the rule fixes now lose its guide data. Channels added later aren't fixed either.", plural(affected, "channel", "channels"))
        } else {
            format!(
                "Affects {} now, and channels added later on every refresh. It only fills gaps: your own choices stay.",
                plural(affected, "channel", "channels")
            )
        };
        ctx.open(OverlayRequest::Confirm(ConfirmSpec {
            title: format!("Turn {} “{rule}” for {name}?", if on { "off" } else { "on" }),
            body,
            confirm_label: if on { "Turn off".into() } else { "Turn on".into() },
            destructive: on,
            token: T_RULE,
        }));
        self.confirm = Some(Pending {
            token: T_RULE,
            proposal: Proposal { rules: vec![(src, new)], ..Default::default() },
            label: format!("Rule “{rule}” {}", if on { "off" } else { "on" }),
            toast: format!("{name}: “{rule}” {}", if on { "off" } else { "on" }),
            actor: FixActor::You,
        });
    }

    // ------------------------------------------------------------------ menus

    fn view_menu(&self, ctx: &mut UiCtx<'_>) {
        let item = |icon, label: String, i: usize, on: bool| MenuItem {
            icon: if on { glyphs::CHECK } else { icon },
            label,
            action: choice_action(i),
            enabled: true,
            shortcut: None,
        };
        let counts = self.kind_counts(ctx);
        let mut items = vec![item("", format!("All issues · {}", rows::grouped(counts.iter().sum())), C_ALL_ISSUES, self.view == View::Issues(None))];
        for k in IssueKind::ALL {
            items.push(item(
                "",
                format!("{} · {}", kind_label(k), rows::grouped(counts[k as usize])),
                C_KIND + k as usize,
                self.view == View::Issues(Some(k)),
            ));
        }
        items.push(item("", "All channels".into(), C_ALL_CHANNELS, self.view == View::All));
        items.push(item(glyphs::HISTORY, "Recent changes".into(), C_RECENT, self.view == View::Recent));
        items.push(item(glyphs::HIDDEN, "Include hidden channels".into(), C_HIDDEN, self.include_hidden));
        let scope = Scope::current(ctx, ScopeKind::Live);
        if scope.multi() {
            items.push(MenuItem {
                icon: glyphs::PLAYLIST,
                label: format!("Playlist: {}…", scope.label()),
                action: choice_action(C_PLAYLIST),
                enabled: true,
                shortcut: Some("Alt+P".into()),
            });
        }
        if let Some(rep) = &self.report {
            let default = ctx.config.epg.rules_default;
            for (i, s) in scope.eligible.iter().enumerate().filter(|(_, s)| scope.selected.is_none_or(|x| x.id == s.id)) {
                let r = rep.rules.get(&s.id).copied().unwrap_or_default();
                items.push(item("", format!("{} · +1/+2 follow their base", s.name), C_RULE + 2 * i, r.siblings_on(default)));
                items.push(item("", format!("{} · Copies share a guide", s.name), C_RULE + 2 * i + 1, r.copies_on(default)));
            }
        }
        items.push(item(glyphs::SETTINGS, "Guide settings…".into(), C_SETTINGS, false));
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "View".into(), items }));
    }

    fn channel_menu(&self, ctx: &mut UiCtx<'_>) {
        let Some(ch) = self.highlighted() else { return };
        let k = KeyContext::EpgMapping;
        let has_fix = self.fix_of(ctx, ch).is_some();
        let n = self.marks.len();
        let mut items = vec![
            MenuItem {
                enabled: has_fix || n > 0,
                ..entry(glyphs::CHECK, if n > 1 { format!("Accept {n} fixes") } else { "Accept".into() }, Action::AcceptFix, k)
            },
            entry(glyphs::GUIDE, "Pick guide channel…", choice_action(C_PICK), k),
            entry(glyphs::HISTORY, "Time shift +1 h", Action::ShiftLater, k),
            entry(glyphs::HISTORY, "Time shift −1 h", Action::ShiftEarlier, k),
        ];
        if ctx.lib.live.get(ch).is_some_and(|c| c.has_catchup) {
            items.push(entry(glyphs::CATCHUP, "Catch-up offset…", choice_action(C_CATCHUP), k));
        }
        items.push(entry(glyphs::PLAY, "Watch to check", choice_action(C_WATCH), k));
        items.push(entry(glyphs::GUIDE, "Show in TV guide", Action::OpenInGuide, KeyContext::List));
        if self.report.as_ref().is_some_and(|r| r.row(ch).is_some()) {
            items.push(entry(glyphs::CHECK, "It's right (dismiss)", choice_action(C_DISMISS), k));
        }
        items.push(entry(glyphs::REFRESH, "Reset to automatic", Action::ClearEpg, k));
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: Self::name(ctx, ch), items }));
    }

    fn set_view(&mut self, ctx: &mut UiCtx<'_>, v: View) {
        if self.view == v {
            return;
        }
        self.flush_adjust(ctx);
        self.view = v;
        self.pane = Pane::Issues;
        self.marks.clear();
        self.list.filter.clear();
        self.list.select(0);
        if v == View::Recent {
            self.recent_loader = Some(Loader::start(ctx, |c| crate::store::guide_fix::recent(c, 200)));
        }
        self.refilter(ctx);
    }

    /// Issue rows per kind in the playlist scope.
    fn kind_counts(&self, ctx: &UiCtx<'_>) -> [usize; 7] {
        let mut out = [0; 7];
        if let Some(rep) = &self.report {
            for r in &rep.issues.rows {
                if ctx.lib.live.get(r.channel).is_some_and(|c| self.scope.admits(c.source_id)) {
                    out[r.kind as usize] += 1;
                }
            }
        }
        out
    }

    fn on_choice(&mut self, ctx: &mut UiCtx<'_>, i: usize) {
        match i {
            _ if (C_KIND..C_KIND + 7).contains(&i) => self.set_view(ctx, View::Issues(Some(IssueKind::ALL[i - C_KIND]))),
            C_ALL_ISSUES => self.set_view(ctx, View::Issues(None)),
            C_ALL_CHANNELS => self.set_view(ctx, View::All),
            C_RECENT => self.set_view(ctx, View::Recent),
            C_HIDDEN => {
                self.include_hidden = !self.include_hidden;
                ctx.toast(Toast::info(if self.include_hidden { "Hidden channels included" } else { "Hidden channels left out" }));
            }
            C_SETTINGS => ctx.push(Route::Settings(crate::ui::SettingsPage::Epg)),
            C_PLAYLIST => {
                crate::ui::scope::on_action(ctx, ScopeKind::Live, Action::PlaylistPicker);
            }
            C_PICK => {
                self.pane = Pane::Guide;
                self.cands.select(0);
            }
            C_CATCHUP => {
                self.pane = Pane::Adjust;
                self.adj = 1;
            }
            C_WATCH => {
                if let Some(ch) = self.highlighted()
                    && !kit::play_channel(ctx, ch, None, crate::model::PlayOrigin::Guide)
                {
                    kit::ask_pin(ctx, choice_action(C_WATCH), &format!("“{}” is locked", Self::name(ctx, ch)));
                }
            }
            C_DISMISS => self.dismiss(ctx),
            C_CHECK => self.start_check(ctx),
            C_USE_ALL => self.use_for_all(ctx),
            _ if i >= C_RULE => {
                let scope = Scope::current(ctx, ScopeKind::Live);
                if let Some(s) = scope.eligible.get((i - C_RULE) / 2) {
                    let id = s.id;
                    self.toggle_rule(ctx, id, (i - C_RULE) % 2 == 1);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ marks

    fn toggle_mark(&mut self, i: usize) {
        if let Some(ch) = self.rows.get(i).copied()
            && !self.marks.remove(&ch)
        {
            self.marks.insert(ch);
        }
        self.anchor = Some(i);
    }

    fn mark_range(&mut self, to: usize) {
        let from = self.anchor.unwrap_or(to);
        let (a, b) = (from.min(to), from.max(to));
        self.marks.extend(self.rows[a..=b.min(self.rows.len().saturating_sub(1))].iter().copied());
        self.anchor = Some(to);
    }

    fn mark_all(&mut self) {
        if !self.rows.is_empty() && self.rows.iter().all(|c| self.marks.contains(c)) {
            self.marks.clear();
        } else {
            self.marks.extend(self.rows.iter().copied());
        }
    }

    /// Release the report and catalogue (the screen stays in memory for the whole session).
    fn release(&mut self) {
        self.report = None;
        self.report_key = None;
        self.report_loader = None;
        self.catalog = None;
        self.catalog_loader = None;
        self.suggestions.clear();
        self.cand_rows.clear();
        self.rows.clear();
        self.preview = None;
        self.last_frame = None;
    }
}

impl EpgMappingScreen {
    /// A key action (`Screen::on_action` without the flush).
    fn act(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        // "No guide data": Enter is the "Open EPG settings" button
        if action == Action::Activate && self.catalog.as_ref().is_some_and(|c| c.entries.is_empty()) && !ctx.lib.live.channels.is_empty() {
            ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
            return true;
        }
        if crate::ui::scope::on_action(ctx, ScopeKind::Live, action) {
            return true;
        }
        match action {
            Action::SortMenu => {
                self.view_menu(ctx);
                return true;
            }
            Action::UndoGuideFix(log) => {
                self.flush_adjust(ctx);
                self.written.1.clear();
                let _ = gf::submit_undo(ctx.services, log, Self::opts(ctx), ctx.now);
                self.preview = None;
                return true;
            }
            _ => {}
        }
        if self.view == View::Recent {
            return match self.list.handle(action) {
                ListOutcome::Activated(i) | ListOutcome::ActivatedAlt(i) => {
                    if let Some(e) = self.recent.get(i).filter(|e| e.undone_at.is_none()) {
                        self.written.1.clear();
                        let _ = gf::submit_undo(ctx.services, Some(e.id), Self::opts(ctx), ctx.now);
                    }
                    true
                }
                ListOutcome::Parent => false,
                ListOutcome::None => matches!(action, Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End),
                _ => true,
            };
        }
        match action {
            Action::AcceptFix => {
                self.accept(ctx);
                return true;
            }
            Action::ClearEpg | Action::Remove => {
                self.flush_adjust(ctx);
                self.reset(ctx);
                return true;
            }
            Action::MarkAll => {
                self.mark_all();
                return true;
            }
            Action::ShiftLater | Action::ShiftEarlier => {
                let t = if self.pane == Pane::Adjust { self.highlighted().into_iter().collect() } else { self.targets() };
                self.nudge_shift(ctx, t, if action == Action::ShiftLater { SHIFT_HOUR } else { -SHIFT_HOUR });
                return true;
            }
            Action::FocusNext | Action::FocusPrev => {
                let order = [Pane::Issues, Pane::Guide, Pane::Adjust];
                let n = if self.adj_rows(ctx).is_empty() { 2 } else { 3 };
                let at = order.iter().position(|p| *p == self.pane).unwrap_or(0);
                self.pane = order[if action == Action::FocusNext { (at + 1) % n } else { (at + n - 1) % n }];
                return true;
            }
            Action::OpenInGuide => {
                if let Some(ch) = self.highlighted() {
                    ctx.push(Route::Guide { collection: None, focus: Some(ch) });
                }
                return true;
            }
            Action::ShowDetails => {
                if let Some(r) = self.highlighted().and_then(|ch| channel_ref(ctx, ch)) {
                    ctx.open(OverlayRequest::EditItem(r));
                }
                return true;
            }
            Action::AssignEpg => {
                self.pane = Pane::Guide;
                return true;
            }
            // the Esc ladder: back to Issues, then the marks
            Action::Back if self.pane != Pane::Issues => {
                self.pane = Pane::Issues;
                return true;
            }
            Action::Back if !self.marks.is_empty() => {
                let n = self.marks.len();
                self.marks.clear();
                ctx.toast(Toast::info(format!("{} cleared", plural(n, "mark", "marks"))));
                return true;
            }
            _ => {}
        }
        match self.pane {
            Pane::Issues => match self.list.handle(action) {
                ListOutcome::Moved => {
                    self.flush_adjust(ctx);
                    self.refilter_cands();
                    true
                }
                ListOutcome::FilterChanged => {
                    self.refilter(ctx);
                    true
                }
                ListOutcome::Activated(_) => {
                    self.pane = Pane::Guide;
                    self.cands.select(0);
                    true
                }
                ListOutcome::ActivatedAlt(_) => {
                    self.accept(ctx);
                    true
                }
                ListOutcome::ContextMenu(_) => {
                    self.channel_menu(ctx);
                    true
                }
                ListOutcome::Parent => false,
                ListOutcome::None => match action {
                    Action::ToggleMark => {
                        self.toggle_mark(self.list.cursor);
                        true
                    }
                    a => matches!(
                        a,
                        Action::Up
                            | Action::Down
                            | Action::PageUp
                            | Action::PageDown
                            | Action::Home
                            | Action::End
                            | Action::FilterInput(_)
                            | Action::Activate
                    ),
                },
            },
            // no candidate to pick: Enter is the empty state's "Open EPG settings"
            Pane::Guide if action == Action::Activate && self.catalog.is_some() && self.cand_rows.is_empty() => {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
                true
            }
            Pane::Guide => match self.cands.handle(action) {
                ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                    self.assign(ctx);
                    true
                }
                ListOutcome::FilterChanged => {
                    self.refilter_cands();
                    true
                }
                ListOutcome::Parent => {
                    self.pane = Pane::Issues;
                    true
                }
                ListOutcome::Moved | ListOutcome::ContextMenu(_) => true,
                ListOutcome::None => {
                    matches!(
                        action,
                        Action::Up
                            | Action::Down
                            | Action::PageUp
                            | Action::PageDown
                            | Action::Home
                            | Action::End
                            | Action::FilterInput(_)
                            | Action::Activate
                    )
                }
            },
            Pane::Adjust => {
                let rows = self.adj_rows(ctx);
                if rows.is_empty() {
                    self.pane = Pane::Issues;
                    return false;
                }
                self.adj = self.adj.min(rows.len() - 1);
                let row = rows[self.adj];
                match action {
                    Action::Up => self.adj = self.adj.saturating_sub(1),
                    Action::Down => self.adj = (self.adj + 1).min(rows.len() - 1),
                    Action::Left | Action::Parent => self.pane = Pane::Issues,
                    Action::Right | Action::Increase | Action::Decrease => {
                        let up = action != Action::Decrease;
                        match row {
                            Adj::Shift => {
                                let t = self.highlighted().into_iter().collect();
                                self.nudge_shift(ctx, t, if up { SHIFT_STEP } else { -SHIFT_STEP });
                            }
                            Adj::Catchup => self.nudge_offset(ctx, if up { OFFSET_STEP } else { -OFFSET_STEP }),
                            Adj::Check | Adj::UseForAll => {}
                        }
                    }
                    Action::Activate => match row {
                        Adj::Check => self.start_check(ctx),
                        Adj::UseForAll => self.use_for_all(ctx),
                        Adj::Shift | Adj::Catchup => {}
                    },
                    _ => return false,
                }
                true
            }
        }
    }
}

impl Screen for EpgMappingScreen {
    fn id(&self) -> ScreenId {
        ScreenId::EpgMapping
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        let inner = match self.pane {
            Pane::Issues => self.list.key_context(),
            Pane::Guide => self.cands.key_context(),
            Pane::Adjust => KeyContext::Panel,
        };
        vec![KeyContext::EpgMapping, inner]
    }

    fn hint_extras(&self) -> Vec<(Action, &'static str)> {
        match self.pane {
            Pane::Issues if self.view != View::Recent => {
                vec![(Action::AcceptFix, "accept"), (Action::ToggleMark, "mark"), (Action::UndoGuideFix(None), "undo"), (Action::SortMenu, "view")]
            }
            Pane::Adjust => vec![(Action::ShiftLater, "±1 h"), (Action::UndoGuideFix(None), "undo")],
            _ => vec![(Action::UndoGuideFix(None), "undo")],
        }
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        self.flush_adjust(ctx);
        self.pane = Pane::Issues;
        if let Route::EpgMapping { channel: Some(ch) } = route {
            self.preselect = Some(*ch);
            if gf::take_focus_adjust() {
                self.pane = Pane::Adjust;
                self.adj = 0;
            }
        }
        self.rows_dirty = true;
        self.preview = None;
        if self.view == View::Recent {
            self.recent_loader = Some(Loader::start(ctx, |c| crate::store::guide_fix::recent(c, 200)));
        }
        self.reload_catalog(ctx);
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        // presses whose debounce passed are written even when the screen isn't drawn (left by
        // a click elsewhere)
        if self.adjust.as_ref().is_some_and(|a| Instant::now() >= a.due) {
            self.flush_adjust(ctx);
        }
        // the screen isn't shown: let go of the big things (they're rebuilt on the way back)
        if self.last_frame.is_some_and(|t| t.elapsed() > IDLE_RELEASE) {
            self.flush_adjust(ctx);
            self.release();
            return;
        }
        match ev {
            AppEvent::Data(DataScope::Epg | DataScope::All) => {
                self.preview = None;
                if self.catalog.is_some() {
                    self.reload_catalog(ctx);
                }
                if self.view == View::Recent {
                    self.recent_loader = Some(Loader::start(ctx, |c| crate::store::guide_fix::recent(c, 200)));
                }
            }
            AppEvent::Data(DataScope::GuideIssues) => self.epoch += 1,
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let handled = self.act(ctx, action);
        // a key the app takes (Esc out, Alt+digits…) may leave the screen: write pending presses
        if !handled {
            self.flush_adjust(ctx);
        }
        handled
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Chosen(a) | OverlayResult::Unlocked(a) => match choice_index(a) {
                Some(i) => self.on_choice(ctx, i),
                None => {
                    self.on_action(ctx, a);
                }
            },
            OverlayResult::Confirmed(token) => {
                if let Some(p) = self.confirm.take().filter(|p| p.token == token) {
                    self.apply_as(ctx, p.actor, p.proposal, p.label, p.toast);
                    if token == T_ACCEPT || token == T_RESET {
                        self.after_write();
                    } else if token == T_CLAUDE {
                        self.marks.clear();
                        self.preview = None;
                    }
                }
            }
            OverlayResult::Cancelled(_) => self.confirm = None,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.last_frame = Some(Instant::now());
        match kit::poll(&mut self.catalog_loader) {
            Some(Ok(c)) => {
                self.catalog = Some(c);
                self.refilter_cands();
            }
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read the guides", &e),
            None => {}
        }
        self.pump_report(ctx);
        self.pump_recent(ctx);
        self.pump_writing(ctx);
        self.pump_review(ctx);
        self.pump_confirm(ctx);
        let scope = Scope::current(ctx, ScopeKind::Live).filter();
        if scope != self.scope {
            self.scope = scope;
            self.rows_dirty = true;
        }
        if self.rows_dirty {
            self.refilter(ctx);
        }
        if self.adjust.as_ref().is_some_and(|a| Instant::now() >= a.due) {
            self.flush_adjust(ctx);
        }
        self.pump_suggestions(ctx);
        self.pump_preview(ctx);
        let theme = ctx.theme;
        let has_guides = !self.catalog.as_ref().is_some_and(|c| c.entries.is_empty());
        let no_channels = ctx.lib.built_at != 0 && ctx.lib.live.channels.is_empty();
        let full_page = !no_channels && has_guides;
        let compact = ctx.layout == LayoutClass::Compact;
        let (mut menu, mut undo) = (false, false);
        let view = self.view.label();
        let can_undo = self.report.as_ref().is_some_and(|r| r.undoable.is_some());
        section::page_header(ui, ctx, "Guide fixes", |ui| {
            if full_page {
                undo = ui
                    .add_enabled_ui(can_undo, |ui| {
                        button(ui, theme, Some(glyphs::HISTORY), if compact { "" } else { "Undo" }, ButtonStyle::default())
                    })
                    .inner
                    .on_hover_text("Undo the last guide fix (Ctrl+Z)")
                    .clicked();
                menu = button(ui, theme, None, &format!("{view} ▾"), ButtonStyle::default()).on_hover_text("View (Ctrl+S)").clicked();
            }
        });
        if no_channels {
            empty_state::empty_state(ui, theme, glyphs::GUIDE, "No channels to fix", Some("Add a playlist first."), None);
            return;
        }
        if !has_guides {
            if empty_state::empty_state(
                ui,
                theme,
                glyphs::GUIDE,
                "No guide data",
                Some("Add an XMLTV guide in Settings ▸ EPG, then fix channels here."),
                Some("Open EPG settings"),
            )
            .is_some_and(|r| r.clicked())
            {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
            }
            return;
        }
        if menu {
            self.view_menu(ctx);
        }
        if undo {
            self.on_action(ctx, Action::UndoGuideFix(None));
        }
        self.dashboard(ui, ctx);
        ui.add_space(theme.space(SpaceToken::Sm));
        let full = ui.available_rect_before_wrap();
        let gap = theme.space(SpaceToken::PanelPadding);
        let left_w = if compact { full.width() } else { (full.width() * 0.48).round() };
        let left = egui::Rect::from_min_size(full.min, egui::vec2(left_w, full.height()));
        let right = if compact { full } else { egui::Rect::from_min_max(egui::pos2(left.right() + gap, full.top()), full.max) };
        if !compact || self.pane == Pane::Issues {
            let mut ui_l = ui.new_child(egui::UiBuilder::new().id_salt("fix-issues").max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
            self.issues_pane(&mut ui_l, ctx);
            if !compact {
                section::pane_divider(ui.painter(), left.right() + gap / 2.0, full.y_range(), theme);
            }
        }
        if !compact || self.pane != Pane::Issues {
            let mut ui_r =
                ui.new_child(egui::UiBuilder::new().id_salt("fix-detail").max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
            if compact && back_button(&mut ui_r, theme, "Issues").clicked() {
                self.pane = Pane::Issues;
            }
            if self.view == View::Recent {
                self.batch_pane(&mut ui_r, ctx);
            } else {
                self.detail_pane(&mut ui_r, ctx);
            }
        }
        ui.allocate_rect(full, egui::Sense::hover());
        let busy = self.suggest_rx.is_some()
            || self.catalog_loader.is_some()
            || self.report_loader.is_some()
            || self.preview_loader.is_some()
            || self.confirm_loader.is_some()
            || self.writing.is_some()
            || self.recent_loader.is_some();
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else if let Some(a) = &self.adjust {
            ui.ctx().request_repaint_after(a.due.saturating_duration_since(Instant::now()));
        } else {
            // a slow tick while shown, so the idle release (`on_event`) only happens off screen
            ui.ctx().request_repaint_after(ALIVE);
        }
    }
}

// ------------------------------------------------------------------ drawing

impl EpgMappingScreen {
    /// The coverage cards (Wide), lines (Medium) or one line (Compact), the rules and the chips.
    fn dashboard(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(rep) = self.report.clone() else {
            ui.label(RichText::new("Checking the guide data…").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            return;
        };
        let scope = Scope::current(ctx, ScopeKind::Live);
        let selected = scope.selected.map(|s| s.id);
        let mut pick: Option<Option<SourceId>> = None;
        let covs: Vec<_> = rep.issues.coverage.iter().filter(|c| ctx.lib.source(c.source).is_some()).collect();
        let pct = |c: &crate::guide_fix::SourceCoverage| (c.with_guide * 100).checked_div(c.shown).unwrap_or(0) as u32;
        let small = |s: String| RichText::new(s).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary);
        match ctx.layout {
            LayoutClass::Compact => {
                let mut parts: Vec<String> =
                    covs.iter().map(|c| format!("{} {} %", ctx.lib.source(c.source).map_or("", |s| s.code.as_str()), pct(c))).collect();
                parts.push(format!("{} to fix ▾", rows::grouped(self.kind_counts(ctx).iter().sum())));
                if ui.add(egui::Label::new(small(parts.join(" · "))).truncate().sense(egui::Sense::click())).on_hover_text("View (Ctrl+S)").clicked()
                {
                    self.view_menu(ctx);
                }
            }
            LayoutClass::Medium => {
                for c in &covs {
                    let (n, k) = rep.issues.to_fix(c.source, ctx.lib);
                    let name = ctx.lib.source(c.source).map_or("", |s| s.name.as_str());
                    let text = format!("{name} · {} % · {} to fix · {} one-key", pct(c), rows::grouped(n), rows::grouped(k));
                    let lit = selected == Some(c.source);
                    let r = ui.add(
                        egui::Label::new(RichText::new(text).size(theme.font(TypeToken::Body)).color(if lit { t.accent } else { t.fg }))
                            .truncate()
                            .sense(egui::Sense::click()),
                    );
                    if r.clicked() {
                        pick = Some(if lit { None } else { Some(c.source) });
                    }
                }
                if let Some(line) = self.rules_line(ctx, &rep, selected) {
                    ui.add(egui::Label::new(small(line)).truncate());
                }
            }
            LayoutClass::Wide => {
                let card_w = (300.0 * theme.rem()).min(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Md), theme.space(SpaceToken::Sm));
                    for c in &covs {
                        let (n, k) = rep.issues.to_fix(c.source, ctx.lib);
                        let name = ctx.lib.source(c.source).map_or("", |s| s.name.as_str());
                        let lines = [
                            format!("{} of {} shown have guide", rows::grouped(c.with_guide), rows::grouped(c.shown)),
                            format!("{} to fix · {} one-key", rows::grouped(n), rows::grouped(k)),
                        ];
                        let lit = selected == Some(c.source);
                        if coverage_card(ui, theme, card_w, name, pct(c), &lines, lit).clicked() {
                            pick = Some(if lit { None } else { Some(c.source) });
                        }
                    }
                    if let Some(src) = selected.or_else(|| (covs.len() == 1).then(|| covs[0].source)) {
                        self.rules_card(ui, ctx, &rep, src, card_w);
                    } else if let Some(line) = self.rules_line(ctx, &rep, None) {
                        ui.add_sized([card_w, theme.space(SpaceToken::ControlHeight)], egui::Label::new(small(line)).truncate());
                    }
                });
            }
        }
        if let Some(choice) = pick
            && scope.multi()
        {
            crate::ui::scope::choose(ctx, ScopeKind::Live, choice, false);
        }
        if !rep.fresh || Self::checking(ctx) {
            ui.label(small("Checking the guide data for new suggestions…".into()));
        }
        if ctx.layout != LayoutClass::Compact {
            self.chips(ui, ctx);
        }
    }

    /// "Rules: +1 ✓ · Copies ✓ · catch-up +2 min" (Medium), or what they fix in All.
    fn rules_line(&self, ctx: &UiCtx<'_>, rep: &Report, src: Option<SourceId>) -> Option<String> {
        let default = ctx.config.epg.rules_default;
        let mark = |on: bool| if on { "✓" } else { "off" };
        match src {
            Some(s) => {
                let r = rep.rules.get(&s).copied().unwrap_or_default();
                let mut line =
                    format!("Rules: +1/+2 follow base {} · Copies share guide {}", mark(r.siblings_on(default)), mark(r.copies_on(default)));
                if let Some(off) = r.catchup_offset_s {
                    line.push_str(&format!(" · catch-up {}", offset_text(off)));
                }
                Some(line)
            }
            None => {
                let (a, b) = rep.by_rules.values().fold((0, 0), |(a, b), (x, y)| (a + x, b + y));
                Some(format!("Rules fix {} · Ctrl+S to change them", plural(a + b, "channel", "channels")))
            }
        }
    }

    /// The Rules card of one playlist (Wide): its two toggles and its catch-up offset.
    fn rules_card(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rep: &Report, src: SourceId, w: f32) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let default = ctx.config.epg.rules_default;
        let r: GuideRules = rep.rules.get(&src).copied().unwrap_or_default();
        let name = ctx.lib.source(src).map(|s| s.name.clone()).unwrap_or_default();
        let mut toggle: Option<bool> = None;
        crate::ui::widgets::card::card(ui, theme, crate::ui::widgets::card::CardKind::Popup, |ui| {
            ui.set_width(w - 2.0 * theme.space(SpaceToken::PopupPadding));
            ui.add(egui::Label::new(RichText::new(format!("Rules · {name}")).size(theme.font(TypeToken::Body)).color(t.fg)).truncate());
            for (copies, label, on) in
                [(false, "+1/+2 follow their base", r.siblings_on(default)), (true, "Copies share a guide", r.copies_on(default))]
            {
                let text = format!("{}  {label}", if on { glyphs::CHECKBOX_ON } else { glyphs::CHECKBOX_OFF });
                let resp = ui.add(
                    egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(if on { t.fg } else { t.text_secondary }))
                        .truncate()
                        .sense(egui::Sense::click()),
                );
                if resp.on_hover_text("Change this rule (asks first)").clicked() {
                    toggle = Some(copies);
                }
            }
            let off = r.catchup_offset_s.unwrap_or(0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Catch-up offset {}", offset_text(off))).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary),
                )
                .truncate(),
            );
        });
        if let Some(copies) = toggle {
            self.toggle_rule(ctx, src, copies);
        }
    }

    /// The issue chips (a click picks the view; the lit chip is the view).
    fn chips(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let counts = self.kind_counts(ctx);
        let mut pick = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(theme.space(SpaceToken::Sm), theme.space(SpaceToken::Xs));
            let all = counts.iter().sum::<usize>();
            let style = |on: bool| ButtonStyle { bordered: true, selected: on, ..Default::default() };
            if button(ui, theme, None, &format!("All issues {}", rows::grouped(all)), style(self.view == View::Issues(None))).clicked() {
                pick = Some(View::Issues(None));
            }
            for k in IssueKind::ALL.into_iter().filter(|k| counts[*k as usize] > 0) {
                let on = self.view == View::Issues(Some(k));
                if button(ui, theme, None, &format!("{} {}", kind_label(k), rows::grouped(counts[k as usize])), style(on)).clicked() {
                    pick = Some(if on { View::Issues(None) } else { View::Issues(Some(k)) });
                }
            }
        });
        if let Some(v) = pick {
            self.set_view(ctx, v);
        }
    }

    fn issues_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let scope_label = Scope::current(ctx, ScopeKind::Live).label();
        let n = if self.view == View::Recent { self.recent.len() } else { self.rows.len() };
        let header = format!("{} · {scope_label} · {}  (type to filter)", self.view.label(), rows::grouped(n));
        search_header(ui, theme, self.list.filter.query(), &header, Some(glyphs::SEARCH));
        ui.add_space(theme.space(SpaceToken::Md));
        let row_h = rows::row_height(theme, ctx.config.interface.density, false);
        let footer_h = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Sm);
        let focused = self.pane == Pane::Issues;
        if self.view == View::Recent {
            if self.recent.is_empty() {
                empty_state::empty_state(ui, theme, glyphs::HISTORY, "No guide fixes yet", None, None);
                return;
            }
            let (recent, h24, now) = (&self.recent, ctx.config.interface.clock_24h, ctx.now);
            let out = virtual_list(ui, theme, "fix-recent", &mut self.list, rows::row_height(theme, ctx.config.interface.density, true), |ui, rc| {
                let e = &recent[rc.index];
                let who = match e.actor {
                    crate::model::FixActor::You => "you",
                    crate::model::FixActor::Claude => "Claude",
                };
                let mut detail = format!("{} · {} · {}", plural(e.n, "change", "changes"), who, kit::ago(now, e.at));
                if e.undone_at.is_some() {
                    detail.push_str(" · undone");
                }
                let _ = h24;
                let state = if focused { rc.state } else { RowState::from_flags(false, rc.state.is_cursor()) };
                rows::menu_row(
                    ui,
                    theme,
                    Some(if e.undone_at.is_some() { glyphs::HISTORY } else { glyphs::CHECK }),
                    &e.label,
                    Some(&detail),
                    false,
                    state,
                )
            });
            if matches!(out, ListOutcome::Moved) {
                self.pane = Pane::Issues;
            }
            return;
        }
        let list_rect = {
            let r = ui.available_rect_before_wrap();
            egui::Rect::from_min_max(r.min, egui::pos2(r.right(), (r.bottom() - footer_h).max(r.top())))
        };
        let mut ui_list =
            ui.new_child(egui::UiBuilder::new().id_salt("fix-issue-list").max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        if self.rows.is_empty() {
            let (glyph, msg) = match (self.view, &self.report) {
                (View::Issues(_), None) => (glyphs::BUSY, "Checking the guide data…"),
                _ if self.list.filter.is_active() => (glyphs::SEARCH, "No matches"),
                (View::Issues(_), Some(_)) => (glyphs::CHECK, "Nothing to fix here"),
                _ => (glyphs::CHECK, "No channels"),
            };
            empty_state::empty_state(&mut ui_list, theme, glyph, msg, None, None);
        } else {
            let mut visible = Vec::new();
            let mut box_click: Option<(usize, egui::Modifiers)> = None;
            let multi = ctx.lib.eligible(ItemKind::Channel).len() > 1;
            let painted = self.paint_rows(&mut ui_list, ctx, row_h, focused, multi, &mut visible, &mut box_click);
            self.visible = visible;
            if let Some((i, m)) = box_click {
                if m.shift {
                    self.mark_range(i);
                } else {
                    self.toggle_mark(i);
                }
            }
            match painted {
                ListOutcome::Moved => {
                    self.pane = Pane::Issues;
                    self.flush_adjust(ctx);
                    self.refilter_cands();
                }
                // a double-click accepts the row's fix
                ListOutcome::Activated(_) => {
                    self.pane = Pane::Issues;
                    self.accept(ctx);
                }
                ListOutcome::ContextMenu(_) => {
                    self.pane = Pane::Issues;
                    self.refilter_cands();
                    self.channel_menu(ctx);
                }
                _ => {}
            }
        }
        // footer: marks and the accept button
        let footer =
            egui::Rect::from_min_max(egui::pos2(list_rect.left(), list_rect.bottom()), egui::pos2(list_rect.right(), list_rect.bottom() + footer_h));
        let mut ui_f =
            ui.new_child(egui::UiBuilder::new().id_salt("fix-footer").max_rect(footer).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let n = self.marks.len();
        let text = if n > 0 {
            format!("{} · ⇧Enter accepts · Ctrl+A all", plural(n, "marked", "marked"))
        } else {
            "Space marks · ⇧Enter accepts · Ctrl+A all".to_owned()
        };
        let mut accept = false;
        ui_f.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if n > 0 {
                accept =
                    button(ui, theme, None, &format!("Accept {}", rows::grouped(n)), ButtonStyle { primary: true, ..Default::default() }).clicked();
            }
            ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary)).truncate());
        });
        if accept {
            self.accept(ctx);
        }
    }
}

/// What one issue row shows.
struct RowLook {
    name: String,
    glyph: &'static str,
    /// "→ E4 · +1 h", or why there is no proposal.
    target: String,
    /// "97 %", "✓" or empty.
    conf: String,
    marked: bool,
    locked: bool,
    /// The playlist's short code (two or more playlists).
    code: Option<String>,
}

impl EpgMappingScreen {
    /// The issue rows: ☐ mark · glyph · name · → fix · confidence · playlist code.
    #[allow(clippy::too_many_arguments)]
    fn paint_rows(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut UiCtx<'_>,
        row_h: f32,
        focused: bool,
        multi: bool,
        visible: &mut Vec<ChannelId>,
        box_click: &mut Option<(usize, egui::Modifiers)>,
    ) -> ListOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let (lib, threshold) = (ctx.lib, ctx.config.epg.fuzzy_threshold);
        let (rows_ids, report, suggestions, marks) = (&self.rows, self.report.as_deref(), &self.suggestions, &self.marks);
        virtual_list(ui, theme, "fix-issues", &mut self.list, row_h, |ui, rc| {
            let id = rows_ids[rc.index];
            visible.push(id);
            let look = row_look(lib, report, suggestions, threshold, marks.contains(&id), multi, id);
            let RowLook { name, glyph, target, conf, marked, locked, code } = &look;
            let state = if focused { rc.state } else { RowState::from_flags(false, rc.state.is_cursor()) };
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
            let label = format!("{name} {target}");
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, *marked, &label));
            if !ui.is_rect_visible(rect) {
                return resp;
            }
            let p = ui.painter();
            paint_row_state(p, rect, theme, state);
            let fg = if state.is_cursor() && focused {
                t.list_cursor_text
            } else if *locked {
                t.text_secondary
            } else {
                t.fg
            };
            let body = egui::FontId::proportional(theme.font(TypeToken::Body));
            let icon = egui::FontId::proportional(theme.font(TypeToken::Icon));
            let inset = theme.space(SpaceToken::Md);
            let col = (28.0 * theme.rem()).round();
            let y = rect.center().y;
            let box_rect = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.top()), egui::vec2(inset + col, rect.height()));
            let (tick, tick_c) = if *marked { (glyphs::CHECKBOX_ON, t.accent) } else { (glyphs::CHECKBOX_OFF, t.text_secondary) };
            p.text(egui::pos2(rect.left() + inset + col / 2.0, y), egui::Align2::CENTER_CENTER, tick, icon.clone(), tick_c);
            let mut left = rect.left() + inset + col;
            let g = if *locked { glyphs::LOCK } else { *glyph };
            let gc = if *glyph == glyphs::CHECK {
                t.success
            } else if *locked {
                t.text_secondary
            } else {
                t.warning
            };
            p.text(egui::pos2(left + col / 2.0, y), egui::Align2::CENTER_CENTER, g, icon, gc);
            left += col + theme.space(SpaceToken::Xs);
            let mut right = rect.right() - inset;
            if let Some(code) = code {
                let g = fit(p, code, egui::FontId::proportional(theme.font(TypeToken::Caption)), t.text_secondary, 64.0 * theme.rem());
                let r = egui::Rect::from_min_size(egui::pos2(right - g.size().x, y - g.size().y / 2.0), g.size());
                p.galley(r.min, g, t.text_secondary);
                right = r.left() - theme.space(SpaceToken::Sm);
            }
            if !conf.is_empty() {
                let g = fit(p, conf, body.clone(), if conf == "✓" { t.success } else { t.text_secondary }, 64.0 * theme.rem());
                let x = right - g.size().x;
                p.galley(egui::pos2(x, y - g.size().y / 2.0), g, t.text_secondary);
                right = x - theme.space(SpaceToken::Sm);
            }
            let w = (right - left).max(0.0);
            let name_w = (w * 0.45).round();
            let ng = fit(p, name, body.clone(), fg, name_w);
            p.galley(egui::pos2(left, y - ng.size().y / 2.0), ng, fg);
            let tg = fit(
                p,
                target,
                egui::FontId::proportional(theme.font(TypeToken::BodySmall)),
                t.text_secondary,
                w - name_w - theme.space(SpaceToken::Sm),
            );
            p.galley(egui::pos2(left + name_w + theme.space(SpaceToken::Sm), y - tg.size().y / 2.0), tg, t.text_secondary);
            if resp.clicked() {
                let m = ui.input(|i| i.modifiers);
                let on_box = resp.interact_pointer_pos().is_some_and(|pos| box_rect.contains(pos));
                if on_box || m.command || m.shift {
                    *box_click = Some((rc.index, m));
                }
            }
            resp
        })
    }

    /// The detail of the highlighted channel: Fix, Other guide channels, Adjust.
    fn detail_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(ch) = self.highlighted() else {
            ui.label(RichText::new("Pick a channel on the left").size(theme.font(TypeToken::Body)).color(t.text_secondary));
            return;
        };
        let Some(c) = ctx.lib.live.get(ch) else { return };
        let body = |s: String, color| RichText::new(s).size(theme.font(TypeToken::Body)).color(color);
        let small = |s: String| RichText::new(s).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary);
        // ---- Fix
        let src = ctx.lib.source(c.source_id).map(|s| s.name.clone()).unwrap_or_default();
        let title = if c.locked { format!("{} · {src} · locked", c.name) } else { format!("{} · {src}", c.name) };
        ui.add(egui::Label::new(RichText::new(title).size(theme.font(TypeToken::Title)).color(t.fg)).truncate());
        let current = match c.epg_channel_id {
            Some(e) => {
                let name = self
                    .catalog
                    .as_ref()
                    .and_then(|cat| cat.by_id.get(&e).map(|i| cat.entries[*i].name.clone()))
                    .unwrap_or_else(|| "a guide channel".into());
                let how = self.report.as_ref().and_then(|r| r.methods.get(&ch)).map(|m| format!(" ({})", method_label(*m))).unwrap_or_default();
                format!("Guide: {name}{how}")
            }
            None => "No guide data".into(),
        };
        let shift_now = self.shift_shown(ctx, ch);
        let mut state_line = current;
        if shift_now != 0 {
            state_line.push_str(&format!(" · time shift {}", shift_text(shift_now)));
        }
        ui.add(egui::Label::new(small(state_line)).truncate());
        let fix = self.fix_of(ctx, ch);
        let issue = self.report.as_ref().and_then(|r| r.row(ch)).cloned();
        match (&fix, &issue) {
            (Some(f), Some(r)) => {
                ui.add(egui::Label::new(body(format!("{} → {}", kind_label(r.kind), fix_text(&f.change)), t.fg)).truncate());
                ui.add(egui::Label::new(small(f.why.clone())).truncate());
            }
            (Some(f), None) => {
                ui.add(egui::Label::new(body(format!("Fix → {}", fix_text(&f.change)), t.fg)).truncate());
                ui.add(egui::Label::new(small(f.why.clone())).truncate());
            }
            (None, Some(r)) => {
                ui.add(egui::Label::new(body(kind_label(r.kind).to_owned(), t.fg)).truncate());
                ui.add(egui::Label::new(small(format!("{} — pick a guide channel below (Enter)", r.why))).truncate());
            }
            (None, None) => {}
        }
        // before / after now and next
        let h24 = ctx.config.interface.clock_24h;
        let preview = self.preview.as_ref().filter(|(k, _)| Some(k) == self.preview_key(ctx).as_ref()).map(|(_, p)| p.clone());
        let changes = self.adjust.as_ref().is_some_and(|a| a.targets.iter().any(|(c, _)| *c == ch)) || fix.is_some();
        let label_w = (64.0 * theme.rem()).round();
        let col_w = ((ui.available_width() - label_w) / 2.0).max(0.0);
        let row_h = theme.font(TypeToken::Body) * 1.5;
        let cell = |ui: &mut egui::Ui, w: f32, s: String, color| {
            ui.add_sized([w, row_h], egui::Label::new(RichText::new(s).size(theme.font(TypeToken::BodySmall)).color(color)).truncate());
        };
        ui.add_space(theme.space(SpaceToken::Xs));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            cell(ui, label_w, String::new(), t.text_secondary);
            cell(ui, col_w, "now".into(), t.text_secondary);
            cell(ui, col_w, "next".into(), t.text_secondary);
        });
        let table: Vec<(&str, Option<&NowNext>)> = match &preview {
            Some(p) if changes => vec![("Before", Some(&p.before)), ("After", Some(&p.after))],
            Some(p) => vec![("Now", Some(&p.before))],
            None => vec![("…", None)],
        };
        for (label, nn) in table {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                cell(ui, label_w, label.to_owned(), t.text_secondary);
                match nn {
                    Some(nn) if nn.now.is_none() && nn.next.is_none() => cell(ui, 2.0 * col_w, "— no guide data —".into(), t.text_secondary),
                    Some(nn) => {
                        cell(ui, col_w, slot_text(&nn.now, h24), t.fg);
                        cell(ui, col_w, slot_text(&nn.next, h24), t.fg);
                    }
                    None => cell(ui, 2.0 * col_w, String::new(), t.text_secondary),
                }
            });
        }
        if let Some(w) = preview.as_ref().and_then(|p| p.warnings.first()) {
            ui.add(egui::Label::new(RichText::new(w.clone()).size(theme.font(TypeToken::BodySmall)).color(t.warning)).truncate());
        }
        let manual = self.report.as_ref().and_then(|r| r.extras.get(&ch)).is_some_and(|e| e.manual.is_some() || e.shift_min.is_some());
        let (mut accept, mut reset, mut dismiss) = (false, false, false);
        ui.add_space(theme.space(SpaceToken::Xs));
        ui.horizontal_wrapped(|ui| {
            accept = ui
                .add_enabled_ui(fix.is_some(), |ui| {
                    button(ui, theme, Some(glyphs::CHECK), "Accept ⇧Enter", ButtonStyle { primary: true, ..Default::default() })
                })
                .inner
                .clicked();
            reset = ui
                .add_enabled_ui(manual, |ui| button(ui, theme, None, "Reset Del", ButtonStyle { bordered: true, ..Default::default() }))
                .inner
                .clicked();
            if issue.is_some() {
                dismiss = button(ui, theme, None, "It's right", ButtonStyle { bordered: true, ..Default::default() })
                    .on_hover_text("Dismiss this issue for the channel")
                    .clicked();
            }
        });
        if accept {
            let keep = std::mem::take(&mut self.marks);
            self.accept(ctx);
            if self.confirm_loader.is_none() {
                self.marks = keep.into_iter().filter(|m| *m != ch).collect();
            }
        }
        if reset {
            let keep = std::mem::take(&mut self.marks);
            self.reset(ctx);
            self.marks = keep;
        }
        if dismiss {
            let keep = std::mem::take(&mut self.marks);
            self.dismiss(ctx);
            self.marks = keep;
        }
        ui.add_space(theme.space(SpaceToken::Sm));
        section::separator(ui, theme);
        // ---- Adjust (at the bottom), then the guide channels in between
        let adj_rows = self.adj_rows(ctx);
        let line_h = theme.space(SpaceToken::ControlHeight) + theme.space(SpaceToken::Xs);
        let adjust_h = line_h * (if adj_rows.len() > 1 { 3.0 } else { 1.0 }) + theme.space(SpaceToken::Md);
        let rest = ui.available_rect_before_wrap();
        let adjust_rect = egui::Rect::from_min_max(egui::pos2(rest.left(), (rest.bottom() - adjust_h).max(rest.top())), rest.max);
        let cands_rect = egui::Rect::from_min_max(rest.min, egui::pos2(rest.right(), adjust_rect.top()));
        let mut ui_c =
            ui.new_child(egui::UiBuilder::new().id_salt("fix-cands").max_rect(cands_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        self.cands_pane(&mut ui_c, ctx);
        let mut ui_a =
            ui.new_child(egui::UiBuilder::new().id_salt("fix-adjust").max_rect(adjust_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        self.adjust_rows(&mut ui_a, ctx, ch, &adj_rows);
        ui.allocate_rect(rest, egui::Sense::hover());
    }

    fn cands_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        search_header(ui, theme, self.cands.filter.query(), "Other guide channels (Tab, type to search)", Some(glyphs::GUIDE));
        ui.add_space(theme.space(SpaceToken::Xs));
        let row_h = rows::row_height(theme, ctx.config.interface.density, true);
        if self.catalog.is_none() {
            for _ in 0..3 {
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h - 6.0), egui::Sense::hover());
                ui.painter().rect_filled(r, t.rounding, t.fill_field);
                ui.add_space(6.0);
            }
            return;
        }
        if self.cand_rows.is_empty() {
            let detail = "Search for another name, or add a guide that lists this channel in Settings ▸ EPG.";
            if empty_state::empty_state(ui, theme, glyphs::SEARCH, "No matching guide channels", Some(detail), Some("Open EPG settings"))
                .is_some_and(|r| r.clicked())
            {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
            }
            return;
        }
        let focused = self.pane == Pane::Guide;
        let (cand_rows, catalog) = (&self.cand_rows, &self.catalog);
        let out = virtual_list(ui, theme, "fix-candidates", &mut self.cands, row_h, |ui, rc| {
            let state = if focused { rc.state } else { RowState::Idle };
            match &cand_rows[rc.index] {
                Cand::Suggestion(s) => {
                    let src =
                        catalog.as_ref().and_then(|c| c.by_id.get(&s.epg_channel_id).map(|i| c.entries[*i].source_name.clone())).unwrap_or_default();
                    let detail = format!("Suggested · {:.0} % · {} · {src}", s.score * 100.0, s.xmltv_id);
                    rows::menu_row(ui, theme, Some(glyphs::TAG), &s.display_name, Some(&detail), false, state)
                }
                Cand::Entry(i) => match catalog.as_ref().and_then(|c| c.entries.get(*i)) {
                    Some(e) => {
                        rows::menu_row(ui, theme, Some(glyphs::GUIDE), &e.name, Some(&format!("{} · {}", e.xmltv_id, e.source_name)), false, state)
                    }
                    None => ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover()).1,
                },
            }
        });
        match out {
            ListOutcome::Moved => self.pane = Pane::Guide,
            ListOutcome::Activated(_) => {
                self.pane = Pane::Guide;
                self.assign(ctx);
            }
            _ => {}
        }
    }

    /// Time shift, catch-up offset, Check ▶, "Use for all ‹playlist› channels…".
    fn adjust_rows(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, ch: ChannelId, rows_: &[Adj]) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        ui.add_space(theme.space(SpaceToken::Sm));
        let focused = |a: Adj| self.pane == Pane::Adjust && rows_.get(self.adj) == Some(&a);
        let label_w = (120.0 * theme.rem()).round().min(ui.available_width() * 0.35);
        let shift = self.shift_shown(ctx, ch);
        let offset = self.offset_shown(ctx, ch);
        let mut shift_delta = 0;
        let mut offset_delta = 0;
        let (mut check, mut use_all) = (false, false);
        let style = |f: bool| ButtonStyle { bordered: true, focused: f, ..Default::default() };
        ui.horizontal_wrapped(|ui| {
            let f = focused(Adj::Shift);
            ui.add_sized(
                [label_w, theme.space(SpaceToken::ControlHeight)],
                egui::Label::new(RichText::new("Time shift").size(theme.font(TypeToken::Body)).color(if f {
                    t.list_cursor_text
                } else {
                    t.text_secondary
                }))
                .truncate(),
            );
            for (l, d) in [("−1 h", -SHIFT_HOUR), ("−15", -SHIFT_STEP)] {
                if button(ui, theme, None, l, style(false)).clicked() {
                    shift_delta = d;
                }
            }
            ui.label(RichText::new(shift_text(shift)).size(theme.font(TypeToken::Body)).color(if f { t.list_cursor_text } else { t.fg }));
            for (l, d) in [("+15", SHIFT_STEP), ("+1 h", SHIFT_HOUR)] {
                if button(ui, theme, None, l, style(false)).clicked() {
                    shift_delta = d;
                }
            }
        });
        if rows_.contains(&Adj::Catchup) {
            ui.horizontal_wrapped(|ui| {
                let f = focused(Adj::Catchup);
                ui.add_sized(
                    [label_w, theme.space(SpaceToken::ControlHeight)],
                    egui::Label::new(RichText::new("Catch-up offset").size(theme.font(TypeToken::Body)).color(if f {
                        t.list_cursor_text
                    } else {
                        t.text_secondary
                    }))
                    .truncate(),
                );
                if button(ui, theme, None, "−1", style(false)).clicked() {
                    offset_delta = -OFFSET_STEP;
                }
                ui.label(RichText::new(offset_text(offset)).size(theme.font(TypeToken::Body)).color(if f { t.list_cursor_text } else { t.fg }));
                if button(ui, theme, None, "+1", style(false)).clicked() {
                    offset_delta = OFFSET_STEP;
                }
                check = button(ui, theme, Some(glyphs::PLAY), "Check", style(focused(Adj::Check)))
                    .on_hover_text("Play the last finished programme from its start")
                    .clicked();
            });
            let name = ctx.lib.live.get(ch).and_then(|c| ctx.lib.source(c.source_id)).map(|s| s.name.clone()).unwrap_or_default();
            use_all = button(ui, theme, None, &format!("Use for all {name} channels…"), style(focused(Adj::UseForAll))).clicked();
        }
        if shift_delta != 0 {
            self.nudge_shift(ctx, vec![ch], shift_delta);
        }
        if offset_delta != 0 {
            self.nudge_offset(ctx, offset_delta);
        }
        if check {
            self.start_check(ctx);
        }
        if use_all {
            self.use_for_all(ctx);
        }
    }

    /// Recent changes: what the highlighted batch changed, and its Undo.
    fn batch_pane(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let Some(e) = self.recent.get(self.list.cursor).cloned() else { return };
        ui.add(egui::Label::new(RichText::new(e.label.clone()).size(theme.font(TypeToken::Title)).color(t.fg)).truncate());
        let mut undo = false;
        if e.undone_at.is_none() {
            undo = button(ui, theme, Some(glyphs::HISTORY), "Undo this (Enter)", ButtonStyle { bordered: true, ..Default::default() }).clicked();
        } else {
            ui.label(RichText::new("Undone").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        }
        ui.add_space(theme.space(SpaceToken::Sm));
        if let Some((_, changes)) = self.batch_changes.as_ref().filter(|(id, _)| *id == e.id.0) {
            egui::ScrollArea::vertical().id_salt("fix-batch").auto_shrink([false, false]).show(ui, |ui| {
                for c in changes {
                    let who = if c.channel_key.is_empty() {
                        ctx.lib.source(c.source).map(|s| s.name.clone()).unwrap_or_else(|| "a playlist".into())
                    } else {
                        let r = ItemRef { source: c.source, kind: ItemKind::Channel, key: crate::model::ItemKey(c.channel_key.clone()) };
                        ctx.lib.live.by_ref(&r).map(|id| Self::name(ctx, id)).unwrap_or_else(|| "a channel no longer listed".into())
                    };
                    let what = match c.field.as_str() {
                        "map" => "guide channel",
                        "shift" => "time shift",
                        "catchup" => "catch-up offset",
                        "dismiss" => "issue dismissed",
                        "rule" => "rules",
                        _ => "",
                    };
                    ui.add(egui::Label::new(RichText::new(format!("{who} · {what}")).size(theme.font(TypeToken::BodySmall)).color(t.fg)).truncate());
                }
            });
        }
        if undo {
            self.written.1.clear();
            let _ = gf::submit_undo(ctx.services, Some(e.id), Self::opts(ctx), ctx.now);
        }
    }
}

/// A playlist's coverage card: name and percentage, the bar, two lines.
fn coverage_card(ui: &mut egui::Ui, theme: &crate::theme::Theme, w: f32, name: &str, pct: u32, lines: &[String; 2], lit: bool) -> egui::Response {
    let t = &theme.tokens;
    let pad = theme.space(SpaceToken::Md);
    let line = theme.font(TypeToken::Body) * 1.4;
    let bar_h = (6.0 * theme.rem()).round();
    let h = 2.0 * pad + line * 3.0 + bar_h + theme.space(SpaceToken::Xs) * 2.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let label = format!("{name} {pct} % · {} · {}", lines[0], lines[1]);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, lit, &label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    p.rect_filled(rect, t.rounding, if resp.hovered() { t.fill_hover } else { t.fill_field });
    let border = if lit { t.accent } else { t.card_border };
    p.rect_stroke(rect, t.rounding, egui::Stroke::new(t.card_border_width.max(1.0), border), egui::StrokeKind::Inside);
    let inner = rect.shrink(pad);
    let body = egui::FontId::proportional(theme.font(TypeToken::Body));
    let small = egui::FontId::proportional(theme.font(TypeToken::BodySmall));
    let pg = fit(p, &format!("{pct} %"), body.clone(), t.fg, inner.width() * 0.3);
    let ng = fit(p, name, body, if lit { t.accent } else { t.fg }, inner.width() - pg.size().x - pad);
    p.galley(inner.left_top(), ng, t.fg);
    p.galley(egui::pos2(inner.right() - pg.size().x, inner.top()), pg, t.fg);
    let bar = egui::Rect::from_min_size(egui::pos2(inner.left(), inner.top() + line + theme.space(SpaceToken::Xs)), egui::vec2(inner.width(), bar_h));
    crate::ui::widgets::progress::paint_progress(p, bar, theme, pct as f32 / 100.0, true);
    for (i, l) in lines.iter().enumerate() {
        let g = fit(p, l, small.clone(), t.text_secondary, inner.width());
        p.galley(egui::pos2(inner.left(), bar.bottom() + theme.space(SpaceToken::Xs) + line * i as f32), g, t.text_secondary);
    }
    resp
}

#[cfg(test)]
mod tests {
    use egui_kittest::kittest::Queryable;
    use rusqlite::params;

    use super::*;
    use crate::model::{EpgMapping, EpgMatchMethod, ItemKey};
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, rig, seed, until};

    type H = egui_kittest::Harness<'static, testkit::App<EpgMappingScreen>>;

    fn item(i: usize) -> ItemRef {
        ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) }
    }

    fn manual(h: &H, i: usize) -> Vec<EpgMapping> {
        let c = h.state().t.services().store.reader().unwrap();
        crate::store::epg::mappings_for_channel(&c, &item(i)).unwrap().into_iter().filter(|m| m.method == EpgMatchMethod::Manual).collect()
    }

    fn settle(h: &mut H) {
        let _ = h.state().t.services().store.writer().blocking(|_| Ok(()));
        h.run_steps(2);
    }

    /// 6 channels; a guide airing now lists all six but only 0–2 are mapped (3–5: "No guide",
    /// each with a same-name guide channel the lazy suggestions find).
    fn seeded() -> (H, Vec<ChannelId>) {
        let (mut t, one) = rig();
        let now = t.now;
        let chans = seed(&mut t, move |c| {
            let s = testkit::source(c, "TV")?;
            let ch = testkit::channels(c, s, 6)?;
            testkit::epg(c, &ch[..3], now - 1800, 3, true)?;
            // channel 3 has an archive (the catch-up rows of Adjust)
            c.execute("UPDATE channel SET catchup = '{\"kind\":\"shift\",\"days\":7,\"correction_s\":0}' WHERE number = 4", [])?;
            for i in 3..6 {
                c.execute("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (1, ?1, 1)", [format!("ch{i}.tv")])?;
                let id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO epg_display_name(epg_channel_id, name, norm) VALUES (?1, ?2, ?3)",
                    params![id, format!("Channel {i}"), format!("channel{i}")],
                )?;
            }
            Ok(ch)
        });
        let mut h = harness(t, one, EpgMappingScreen::default(), egui::vec2(1600.0, 900.0));
        h.state_mut().enter(Route::EpgMapping { channel: None });
        until(&mut h, "the report and the guide catalogue", |h| h.state().s.report.is_some() && h.state().s.catalog.is_some());
        h.run_steps(2);
        (h, chans)
    }

    /// Marks, the confirm card for more than one fix, a one-key accept, Ctrl+Z, chip clicks
    /// and the Esc ladder (filter → pane → marks → leave).
    #[test]
    fn accept_marked_fixes_after_the_card_undo_and_the_esc_ladder() {
        let (mut h, chans) = seeded();
        assert_eq!(h.state().s.rows, chans[3..].to_vec(), "the three channels without guide data");
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::EpgMapping, KeyContext::List]);
        until(&mut h, "the lazy suggestions", |h| (3..6).all(|i| h.state().s.suggestions.get(&chans[i]).is_some_and(|s| !s.is_empty())));
        // Space marks, the cursor moves on, Space marks the next
        h.state_mut().act(Action::ToggleMark);
        h.state_mut().act(Action::Down);
        h.state_mut().act(Action::ToggleMark);
        assert_eq!(h.state().s.marks, HashSet::from([chans[3], chans[4]]));
        // Shift+Enter: two fixes ask first, with the before/after now
        h.state_mut().act(Action::AcceptFix);
        until(&mut h, "the confirm card", |h| h.state().s.confirm.is_some());
        let nav = h.state_mut().nav();
        let Some(NavRequest::Open(OverlayRequest::Confirm(spec))) = nav.last() else { panic!("no confirm card: {nav:?}") };
        assert_eq!(spec.title, "Accept 2 fixes?");
        assert!(spec.body.contains("Channel 3: — no guide — →") && spec.body.contains("Undo: Ctrl+Z"), "{}", spec.body);
        assert!(manual(&h, 3).is_empty(), "nothing written before the answer");
        h.state_mut().result(OverlayResult::Confirmed(T_ACCEPT));
        settle(&mut h);
        assert_eq!((manual(&h, 3).len(), manual(&h, 4).len()), (1, 1));
        assert!(h.state().s.marks.is_empty());
        let undo = h.state().t.toasts.iter().rev().find_map(|t| t.action.clone());
        assert!(matches!(undo, Some((_, Action::UndoGuideFix(Some(_))))), "the toast offers Undo: {undo:?}");
        // Ctrl+Z undoes the whole batch
        h.state_mut().act(Action::UndoGuideFix(None));
        settle(&mut h);
        assert_eq!((manual(&h, 3).len(), manual(&h, 4).len()), (0, 0));
        // one channel never asks
        h.state_mut().s.list.select(2);
        h.state_mut().act(Action::AcceptFix);
        settle(&mut h);
        assert_eq!(manual(&h, 5).len(), 1);
        assert!(h.state().s.confirm.is_none() && h.state().s.confirm_loader.is_none());
        // the new library: that guide channel lists nothing now or later (its own issue); the
        // suggestion cache was behind, so the screen checked the guide data and the channels
        // without a guide are suggestions now
        testkit::reload_library(&mut h.state_mut().t);
        until(&mut h, "the checked report after the fix", |h| {
            h.state().s.report.as_ref().is_some_and(|r| r.fresh && r.row(chans[5]).is_some_and(|r| r.kind == IssueKind::GuideEnded))
        });
        // a chip click filters by its kind; clicking the lit chip shows every issue again
        h.get_by_label("Suggested 2").click();
        h.run_steps(2);
        assert_eq!((h.state().s.view, h.state().s.rows.clone()), (View::Issues(Some(IssueKind::Suggested)), chans[3..5].to_vec()));
        h.get_by_label("Suggested 2").click();
        h.run_steps(2);
        assert_eq!((h.state().s.view, h.state().s.rows.len()), (View::Issues(None), 3));
        // Esc: the filter, then the guide channels pane, then the marks, then the screen
        h.state_mut().act(Action::FilterInput('x'));
        assert_eq!((h.state().s.rows.len(), h.state().s.key_contexts()[1]), (0, KeyContext::ListFiltering));
        assert!(h.state_mut().act(Action::FilterClear));
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state().s.pane, Pane::Guide);
        assert!(h.state_mut().act(Action::Back));
        assert_eq!(h.state().s.pane, Pane::Issues);
        h.state_mut().act(Action::MarkAll);
        assert_eq!(h.state().s.marks.len(), 3, "Ctrl+A marks all shown");
        assert!(h.state_mut().act(Action::Back));
        assert!(h.state().s.marks.is_empty());
        assert!(!h.state_mut().act(Action::Back), "then Esc leaves");
    }

    /// Ctrl+Shift+→ shows the shifted now/next at once and writes one logged batch after the
    /// debounce, however many presses; Adjust is a Panel.
    #[test]
    fn a_time_shift_previews_at_once_and_writes_once() {
        let (mut h, chans) = seeded();
        // Ctrl+E on a channel without an issue: every channel, that one highlighted
        h.state_mut().enter(Route::EpgMapping { channel: Some(chans[0]) });
        until(&mut h, "channel 0", |h| h.state().s.highlighted() == Some(chans[0]));
        assert_eq!(h.state().s.view, View::All);
        h.state_mut().act(Action::ShiftLater);
        h.state_mut().act(Action::ShiftLater);
        let epg = h.state().t.lib.live.get(chans[0]).and_then(|c| c.epg_channel_id).expect("mapped");
        let now = h.state().t.now;
        let want = {
            let c = h.state().t.services().store.reader().unwrap();
            crate::store::epg::now_next(&c, &[(epg, 120)], now).unwrap().remove(&(epg, 120)).unwrap_or_default()
        };
        until(&mut h, "the shifted preview", |h| {
            h.state().s.preview.as_ref().is_some_and(|(k, p)| k.1.shift_min == Some(Some(120)) && p.after == want)
        });
        let shift = |h: &H| -> Option<i32> {
            let c = h.state().t.services().store.reader().unwrap();
            c.query_row("SELECT shift_min FROM item_override WHERE item_key = ?1", [item(0).key.as_str()], |r| r.get(0)).ok().flatten()
        };
        assert_eq!(shift(&h), None, "not written yet");
        std::thread::sleep(DEBOUNCE + Duration::from_millis(50));
        h.run_steps(2);
        settle(&mut h);
        assert_eq!(shift(&h), Some(120));
        let batches = crate::store::guide_fix::recent(&h.state().t.services().store.reader().unwrap(), 10).unwrap();
        assert_eq!(batches.len(), 1, "two presses, one undo entry");
        // Tab, Tab: the Adjust rows take ←/→ like a settings panel
        h.state_mut().act(Action::FocusNext);
        h.state_mut().act(Action::FocusNext);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::EpgMapping, KeyContext::Panel]);
        assert!(h.state_mut().act(Action::Left));
        assert_eq!(h.state().s.pane, Pane::Issues);
    }

    /// Half and full screen, with small and large text: nothing drawn past the right edge, in
    /// the dashboard, the issues or the detail with every Adjust row.
    #[test]
    fn fits_half_and_full_width_at_small_and_large_text() {
        let (mut h, chans) = seeded();
        assert_eq!(h.state().s.highlighted(), Some(chans[3]));
        for (w, scale) in [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)] {
            let base = crate::theme::Theme::fallback();
            let theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.set_size(egui::vec2(w, 900.0));
            let app = h.state_mut();
            app.t.layout = LayoutClass::from_width(w, theme.rem());
            app.t.theme = theme;
            // one pane at a time in Compact: show the detail
            app.s.pane = if app.t.layout == LayoutClass::Compact { Pane::Adjust } else { Pane::Issues };
            h.run_steps(3);
            assert!(testkit::has_label(&h, "Time shift") && testkit::has_label_containing(&h, "Use for all TV"), "{w} × {scale}: the Adjust rows");
            for node in h.query_all_by_label_contains("") {
                let r = node.rect();
                assert!(r.right() <= w + 1.0 && r.left() >= -1.0, "{w} pt × {scale}: {node:?} spans {}–{}", r.left(), r.right());
            }
        }
    }
}
