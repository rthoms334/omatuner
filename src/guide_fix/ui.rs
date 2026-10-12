//! Guide fixes' UI pieces (DESIGN §31). OWNER: G-ui.
//!
//! * [`Report`] / [`load_report`]: what the Guide fixes screen shows, read and classified on
//!   the blocking pool (`store::guide_fix::facts` + `issues::classify` over the library
//!   snapshot, so explicit channels are absent while hidden).
//! * [`submit_apply`] / [`submit_undo`]: every write is one logged batch on the store writer;
//!   it emits `DataScope::Epg`, toasts what it did (with Undo), and submits
//!   `EvaluateFollows { reason: Mapping }` when a mapping changed (B7), and a guide check
//!   (`jobs::guide_check::request_from`) when rules or mappings changed. The app's global
//!   `Action::UndoGuideFix` arm can call [`submit_undo`] too (a toast's Undo off the screen).
//! * [`to_fix`]: "Fix guide data… (N to fix)" for Settings ▸ EPG, cached per library build.
//! * Item menus: "Time shift…" on a channel opens Guide fixes on it with Adjust focused.
//! * [`offer_review`] / [`take_review`]: a batch of Claude's over 50 channels
//!   (`AgentCommand::GuideFixReview`) waits here for the Guide fixes screen, which opens its
//!   confirm card prefilled; Enter applies it as Claude's batch ([`claude_label`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::issues::{ClassifyOpts, IssueReport, classify};
use crate::error::Result;
use crate::jobs::JobKind;
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, EpgMatchMethod, FixActor, GuideFixLogId, GuideRules, IssueKind, Proposal, SourceId, Timestamp};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::services::Services;
use crate::store::Ticket;
use crate::store::guide_fix::{ApplyReport, ChannelExtra, GuideOpts, UndoReport};
use crate::store::library::Library;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;
use crate::ui::widgets::glyphs;
use crate::ui::{Route, UiCtx};

/// How long a guide fix toast (and its Undo) stays.
pub const UNDO_TTL_MS: u32 = 8000;

/// The chips' and the view menu's name of an issue kind.
pub fn kind_label(k: IssueKind) -> &'static str {
    match k {
        IssueKind::NoGuide => "No guide",
        IssueKind::Suggested => "Suggested",
        IssueKind::PlusNoShift => "+1 without shift",
        IssueKind::CopyHasGuide => "Copy has guide",
        IssueKind::GuideEnded => "Guide ended",
        IssueKind::GoneFromGuide => "Gone from guide",
        IssueKind::LooksWrong => "Looks wrong",
    }
}

/// "How the channel got its guide" in the detail pane.
pub fn method_label(m: EpgMatchMethod) -> &'static str {
    match m {
        EpgMatchMethod::Manual => "picked by you",
        EpgMatchMethod::TvgIdExact | EpgMatchMethod::TvgIdCaseless => "tvg-id",
        EpgMatchMethod::XtreamEpgId => "provider guide id",
        EpgMatchMethod::FeedStripped => "id without feed",
        EpgMatchMethod::NormalizedName => "name",
        EpgMatchMethod::Fuzzy => "similar name",
        EpgMatchMethod::Sibling => "rule: +1/+2 follow their base",
        EpgMatchMethod::Copy => "rule: copies share a guide",
    }
}

/// A time shift: "0", "+1 h", "−15 min", "+1 h 15".
pub fn shift_text(min: i32) -> String {
    if min == 0 {
        return "0".into();
    }
    let sign = if min < 0 { "−" } else { "+" };
    let (h, m) = (min.unsigned_abs() / 60, min.unsigned_abs() % 60);
    match (h, m) {
        (0, m) => format!("{sign}{m} min"),
        (h, 0) => format!("{sign}{h} h"),
        (h, m) => format!("{sign}{h} h {m:02}"),
    }
}

/// A catch-up offset in seconds: "0 min", "+2 min", "−1 min".
pub fn offset_text(s: i64) -> String {
    let m = s / 60;
    match m {
        0 => "0 min".into(),
        m if m < 0 => format!("−{} min", m.unsigned_abs()),
        m => format!("+{m} min"),
    }
}

/// Everything the Guide fixes screen shows (one read + classification off the UI thread).
#[derive(Debug, Default)]
pub struct Report {
    pub issues: IssueReport,
    /// Row index in `issues.rows` per channel.
    pub by_channel: HashMap<ChannelId, u32>,
    /// The channels' own time shift, catch-up offset, dismissals and manual rows.
    pub extras: HashMap<ChannelId, ChannelExtra>,
    /// How each resolved channel got its guide channel.
    pub methods: HashMap<ChannelId, EpgMatchMethod>,
    /// The rules each playlist set (`None` fields: `rules_default`).
    pub rules: HashMap<SourceId, GuideRules>,
    /// Shown channels each playlist's rules fix now: (+N follow their base, copies).
    pub by_rules: HashMap<SourceId, (usize, usize)>,
    /// The suggestion cache matches the guides (else "Checking…").
    pub fresh: bool,
    /// A batch Ctrl+Z can undo.
    pub undoable: Option<GuideFixLogId>,
    /// The library build (`Library::generation`) and the options it was made for.
    pub generation: u64,
    pub include_hidden: bool,
}

impl Report {
    pub fn row(&self, ch: ChannelId) -> Option<&super::issues::IssueRow> {
        self.by_channel.get(&ch).map(|&i| &self.issues.rows[i as usize])
    }

    /// Issues of shown channels (hidden ones only count with "Include hidden").
    pub fn to_fix(&self) -> usize {
        self.issues.rows.len()
    }
}

/// Read and classify (blocking pool; about 0.3 s + < 0.1 s at 79k channels). `with_catalog`:
/// also find proposals for "Gone from guide" and "Guide ended" (the catalogue takes 0.2–1 s to
/// build; a count doesn't need them).
pub fn load_report(
    conn: &rusqlite::Connection,
    lib: &Library,
    opts: GuideOpts,
    include_hidden: bool,
    with_catalog: bool,
    now: Timestamp,
) -> Result<Report> {
    let facts = crate::store::guide_fix::facts(conn, &opts, now)?;
    let catalog = if with_catalog { super::catalog::shared(conn).ok() } else { None };
    let issues = classify(lib, &facts, catalog.as_deref(), ClassifyOpts { include_hidden });
    let by_channel = issues.rows.iter().enumerate().map(|(i, r)| (r.channel, i as u32)).collect();
    let mut methods = HashMap::new();
    let mut by_rules: HashMap<SourceId, (usize, usize)> = HashMap::new();
    for c in &facts.chans {
        let Some(r) = &c.resolved else { continue };
        // the snapshot's channels only: concealed ones never count
        if lib.live.get(c.id).is_none() {
            continue;
        }
        methods.insert(c.id, r.method);
        if c.usable {
            let e = by_rules.entry(c.source).or_default();
            if r.method == EpgMatchMethod::Sibling || (r.method != EpgMatchMethod::Manual && r.shift_min.is_some()) {
                e.0 += 1;
            } else if r.method == EpgMatchMethod::Copy {
                e.1 += 1;
            }
        }
    }
    let undoable = crate::store::guide_fix::last_undoable(conn)?;
    Ok(Report {
        issues,
        by_channel,
        extras: facts.extras.clone(),
        methods,
        rules: facts.rules.clone(),
        by_rules,
        fresh: facts.fresh(),
        undoable,
        generation: lib.generation,
        include_hidden,
    })
}

// ------------------------------------------------------------------ writes

/// What a batch toasts (no programme titles; a channel name only for a one-channel change).
fn applied_text(what: &str, r: &ApplyReport) -> String {
    let mut s = what.to_owned();
    if r.reminders_moved > 0 {
        s.push_str(&format!(" · {} reminder{} moved", r.reminders_moved, if r.reminders_moved == 1 { "" } else { "s" }));
    }
    if !r.skipped.is_empty() {
        s.push_str(&format!(" · {} skipped (no longer in the playlist)", r.skipped.len()));
    }
    s
}

/// A batch's toast with its Undo.
fn undo_toast(text: String, log: GuideFixLogId) -> Toast {
    Toast { kind: ToastKind::Info, text, action: Some(("Undo".into(), Action::UndoGuideFix(Some(log)))), ttl_ms: UNDO_TTL_MS }
}

/// "Claude fixed the guide for 140 channels" (a batch of Claude's, applied from chat or from
/// the confirm card).
pub fn claude_fixed_text(n: usize) -> String {
    format!("Claude fixed the guide for {} channel{}", crate::ui::widgets::rows::grouped(n), if n == 1 { "" } else { "s" })
}

/// The toast of a batch Claude applied from chat (`None`: nothing changed).
pub fn claude_toast(r: &ApplyReport) -> Option<Toast> {
    r.log.map(|log| undo_toast(applied_text(&claude_fixed_text(r.applied), r), log))
}

/// What the log calls a batch of Claude's.
pub fn claude_label(n: usize) -> String {
    format!("Claude: {n} channel{}", if n == 1 { "" } else { "s" })
}

/// Apply `p` as one logged batch of `actor`'s (`label` goes in the log, `toast` on screen).
/// The toast offers Undo; Following re-evaluates after a mapping change (B7). A proposal bound
/// to an older guide check (Claude's, `store::guide_fix::STALE`) applies nothing and says so.
pub fn submit_apply(
    services: &Services,
    actor: FixActor,
    label: String,
    toast: String,
    p: Proposal,
    opts: GuideOpts,
    now: Timestamp,
) -> Ticket<ApplyReport> {
    let (bus, jobs) = (services.bus.clone(), services.jobs.clone());
    services.store.writer().submit_notify(DataScope::Epg, move |c| {
        let r = match crate::store::guide_fix::apply(c, actor, &label, &p, &opts, now) {
            Ok(r) => r,
            Err(crate::error::Error::Other(m)) if m == crate::store::guide_fix::STALE => {
                let again = if actor == FixActor::Claude { "ask Claude to propose them again" } else { "try again" };
                bus.toast(Toast::info(format!("The guide data changed since these fixes were proposed — nothing changed; {again}")));
                return Ok(ApplyReport::default());
            }
            Err(e) => return Err(e),
        };
        match r.log {
            Some(log) => bus.toast(undo_toast(applied_text(&toast, &r), log)),
            None if !r.skipped.is_empty() => bus.toast(Toast::info("That channel is no longer in its playlist")),
            None => {}
        }
        if r.mappings_changed && crate::follow::ENABLED {
            jobs.submit(JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Mapping });
        }
        // new rules are a new stamp; an unmapped channel needs suggestions
        if r.rules_changed || r.mappings_changed {
            crate::jobs::guide_check::request_from(&jobs);
        }
        Ok(r)
    })
}

/// Undo batch `log` (`None`: the newest one not undone yet). `Ok(None)`: nothing to undo.
pub fn submit_undo(services: &Services, log: Option<GuideFixLogId>, opts: GuideOpts, now: Timestamp) -> Ticket<Option<UndoReport>> {
    let (bus, jobs) = (services.bus.clone(), services.jobs.clone());
    services.store.writer().submit_notify(DataScope::Epg, move |c| {
        let id = match log {
            Some(id) => Some(id),
            None => crate::store::guide_fix::last_undoable(c)?,
        };
        let Some(id) = id else {
            bus.toast(Toast::info("No guide fix to undo"));
            return Ok(None);
        };
        let r = match crate::store::guide_fix::undo(c, id, &opts, now) {
            Ok(r) => r,
            // a toast's Undo pressed twice: say so rather than failing loudly
            Err(crate::error::Error::Other(m)) if m.contains("already undone") => {
                bus.toast(Toast::info("That guide fix was already undone"));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let mut text = format!("Guide fix undone · {} channel{}", r.reverted, if r.reverted == 1 { "" } else { "s" });
        if r.reminders_moved > 0 {
            text.push_str(&format!(" · {} reminder{} moved back", r.reminders_moved, if r.reminders_moved == 1 { "" } else { "s" }));
        }
        if !r.skipped.is_empty() {
            text.push_str(&format!(" · {} skipped (changed since)", r.skipped.len()));
        }
        bus.toast(Toast::info(text));
        if r.reverted > 0 && crate::follow::ENABLED {
            jobs.submit(JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Mapping });
        }
        if r.rules_changed || r.reverted > 0 {
            crate::jobs::guide_check::request_from(&jobs);
        }
        Ok(Some(r))
    })
}

// ------------------------------------------------------------------ "N to fix" (Settings ▸ EPG)

/// The last count per library build: (`Library::generation`, count without hidden channels).
static TO_FIX: Mutex<Option<(u64, Option<usize>)>> = Mutex::new(None);

fn to_fix_slot() -> std::sync::MutexGuard<'static, Option<(u64, Option<usize>)>> {
    TO_FIX.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remember a count the screen computed (shown channels only).
pub(crate) fn note_to_fix(generation: u64, n: usize) {
    *to_fix_slot() = Some((generation, Some(n)));
}

/// "N to fix" for the current library build; `None` while it is being counted (one count at a
/// time, off the UI thread).
pub fn to_fix(ctx: &UiCtx<'_>) -> Option<usize> {
    let generation = ctx.lib.generation;
    {
        let mut slot = to_fix_slot();
        match *slot {
            Some((g, n)) if g == generation => return n,
            _ => *slot = Some((generation, None)),
        }
    }
    let (store, bus, lib) = (ctx.services.store.clone(), ctx.services.bus.clone(), Arc::clone(ctx.lib));
    let (opts, now) = (GuideOpts::from_config(ctx.config), ctx.now);
    ctx.services.rt.spawn_blocking(move || {
        let n = store.reader().and_then(|c| load_report(&c, &lib, opts, false, false, now)).map(|r| r.to_fix());
        match n {
            Ok(n) => {
                let mut slot = to_fix_slot();
                if slot.is_some_and(|(g, _)| g == generation) {
                    *slot = Some((generation, Some(n)));
                }
            }
            Err(e) => tracing::debug!("guide fixes count: {e}"),
        }
        bus.request_repaint();
    });
    None
}

// ------------------------------------------------------------------ item menus

thread_local! {
    /// "Time shift…" opened the screen: focus its Adjust rows (taken by the screen's
    /// `on_enter`; the UI is one thread).
    static FOCUS_ADJUST: Cell<bool> = const { Cell::new(false) };
}

/// The screen opened from "Time shift…" (once).
pub(crate) fn take_focus_adjust() -> bool {
    FOCUS_ADJUST.with(|f| f.replace(false))
}

// ------------------------------------------------------------------ Claude's big batches

thread_local! {
    /// A batch of Claude's too big to apply from chat, waiting for the Guide fixes screen to
    /// open its confirm card (the app sets it on `AgentCommand::GuideFixReview`; the UI is one
    /// thread). A newer one replaces it.
    static REVIEW: RefCell<Option<crate::agent::GuideFixReview>> = const { RefCell::new(None) };
}

/// Hand Claude's batch to the Guide fixes screen (the app opens the screen). True when it
/// replaced an earlier batch still waiting for its card (the app says so).
pub(crate) fn offer_review(r: crate::agent::GuideFixReview) -> bool {
    REVIEW.with(|s| s.borrow_mut().replace(r).is_some())
}

/// Claude's batch for the confirm card (once).
pub(crate) fn take_review() -> Option<crate::agent::GuideFixReview> {
    REVIEW.with(|s| s.borrow_mut().take())
}

/// Its entries in an item menu (`ui::item_menu::extend`): "Time shift…" on a channel.
pub fn menu_items(target: &Target, _ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    match target {
        Target::Channel(_) if super::ENABLED => vec![crate::ui::item_menu::entry(glyphs::HISTORY, "Time shift…", Action::ShiftLater, keys)],
        _ => Vec::new(),
    }
}

/// Its keys and menu choices on an item (`ui::item_menu::dispatch`).
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    match (action, target) {
        (Action::ShiftLater, Target::Channel(id)) if super::ENABLED => {
            FOCUS_ADJUST.with(|f| f.set(true));
            ctx.push(Route::EpgMapping { channel: Some(*id) });
            true
        }
        _ => false,
    }
}
