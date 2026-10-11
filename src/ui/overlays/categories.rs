//! "Choose what to show" — the category filter picker (DESIGN §28.6). Owner: P6-ui.
//!
//! One implementation: [`CategoryPicker`] (state, keys and view) is embedded by the onboarding
//! step and wrapped by [`CategoriesOverlay`] (Settings ▸ Playlists ▸ playlist ▸ Categories…,
//! Settings ▸ Hidden ▸ Choose countries…).
//!
//! * It edits the rule **every playlist shares** (DESIGN §28.3), opened from one playlist. Its
//!   first rows are options: "Only for <playlist>" (with two or more playlists, or when the
//!   playlist already has its own rule) gives that playlist its own rule instead, and "Also
//!   hide titles in other languages" (DESIGN §28.12). Space ticks them like any row; they are
//!   listed while the filter is empty.
//! * Rows: the tags of the playlists the rule covers (the shared rule: every playlist that
//!   follows it; "Only for": that playlist) by item count, then "No country tag"; each with a
//!   tick box, the short code, the name, what it covers and a "new" badge for tags the saved
//!   rule didn't know.
//! * Ticks start from the rule edited; without one from the defaults (the user's country and
//!   its language, every English-language tag, untagged — `category_tags::default_keep`); a
//!   playlist's own "show everything" ticks every row.
//! * The shared rule's ticks and the playlist's own are kept apart: "Only for" first ticked
//!   starts the own ticks from the shared ones; unticked, the shared ticks are back as they
//!   were. A playlist that had its own rule and only has "Only for" unticked follows the shared
//!   rule on Enter, which is not rewritten (a playlist with its own rule and no shared one shows
//!   the shared ticks as "everything", what following it shows).
//! * Saving stores the ticked tags — also ticked tags no playlist has (yet), so the English
//!   defaults keep working when such a category appears — and `known` = the tags present.
//!   Everything ticked (and titles in other languages not hidden) stores no rule at all.
//! * Keys (`[Checklist, Modal]`): ↑/↓ move, Space / Insert tick, Ctrl+A all / none of the tag
//!   rows shown, Ctrl+E just my country + English, typing filters, Enter saves, Esc clears the
//!   filter, then cancels. F5 / Ctrl+R on a playlist imported before tags were recognised
//!   (`needs_refresh`) runs a forced refresh of it (an unforced one may get "304 Not Modified"
//!   and never write the raw names).
//! * A line above the buttons says what the ticks show ("6 of 42 ticked · 3 210 of 48 900
//!   channels"). Ticks that show nothing of the playlists warn, and Enter then asks once more.
//! * Data is read off the UI thread and re-read whenever the library changes while the picker
//!   is open (Xtream movie / series categories import after the channels); the ticks survive.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim_guarded};
use super::{Overlay, OverlayOutcome};
use crate::jobs::JobKind;
use crate::keymap::{Action, KeyContext};
use crate::model::{CategoryFilter, CategoryTag, SourceId};
use crate::msg::{DataScope, Toast};
use crate::sources::category_tags::{self, CategorySummary, TagSummary};
use crate::store::Ticket;
use crate::store::library::Library;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::UiCtx;
use crate::ui::screens::home::kit::{self, Loader};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::glyphs;
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::rows::{check_row, grouped, row_height};
use crate::ui::widgets::text_field::search_header;

/// Name of the untagged entry.
pub(crate) const UNTAGGED: &str = "No country tag";

/// What the picker reads for a playlist (off the UI thread).
pub(crate) struct PickerData {
    /// The categories the shared rule covers: every playlist that follows it, and this one.
    pub summary: CategorySummary,
    /// This playlist's own categories ("Only for <playlist>").
    pub own_summary: CategorySummary,
    /// The shared rule (`None` = no filtering).
    pub rule: Option<CategoryFilter>,
    /// The playlist's own rule when it has one (`Some(None)`: it shows everything).
    pub own: Option<Option<CategoryFilter>>,
    pub name: String,
    /// Enabled playlists (two or more offer "Only for <playlist>").
    pub playlists: usize,
    /// The user's country (`presets::detect_country`, which may read a file).
    pub country: Option<String>,
}

/// Read a playlist's category summaries and rules (a `Loader` body).
pub(crate) fn load(c: &rusqlite::Connection, id: SourceId, hide_explicit: bool) -> crate::error::Result<PickerData> {
    let all = crate::store::sources::list_sources(c)?;
    let src = crate::store::sources::get_source(c, id)?;
    let followers: Vec<SourceId> = all.iter().filter(|s| s.id == id || !s.settings.has_own_category_filter()).map(|s| s.id).collect();
    Ok(PickerData {
        summary: crate::store::library::category_summary_of(c, &followers, hide_explicit)?,
        own_summary: crate::store::library::category_summary(c, id, hide_explicit)?,
        rule: crate::store::sources::shared_category_filter(c)?,
        own: src.settings.has_own_category_filter().then_some(src.settings.category_filter),
        name: src.name,
        playlists: all.iter().filter(|s| s.enabled).count(),
        country: crate::sources::presets::detect_country(),
    })
}

/// "United Kingdom", "Arabic", or "No country tag".
pub(crate) fn tag_name(tag: Option<&CategoryTag>) -> String {
    tag.map_or_else(|| UNTAGGED.to_owned(), category_tags::tag_label)
}

/// "38 live groups · 2 140 channels · 12 movie / 9 series categories".
pub(crate) fn counts_text(t: &TagSummary) -> String {
    let plural = |n: u32, one: &str, many: &str| format!("{} {}", grouped(n as usize), if n == 1 { one } else { many });
    let mut parts = Vec::new();
    if t.live_groups > 0 {
        parts.push(plural(t.live_groups, "live group", "live groups"));
        parts.push(plural(t.channels, "channel", "channels"));
    }
    match (t.movie_groups, t.series_groups) {
        (0, 0) => {}
        (m, 0) => parts.push(plural(m, "movie category", "movie categories")),
        (0, s) => parts.push(plural(s, "series category", "series categories")),
        (m, s) => parts.push(format!("{} movie / {} series categories", grouped(m as usize), grouped(s as usize))),
    }
    parts.join(" · ")
}

/// Playlists whose categories carry two or more different tags: their group rows show the
/// tag's short code (DESIGN §28.7). One pass over the library's groups.
pub(crate) fn multi_tag_sources(lib: &Library) -> HashSet<SourceId> {
    let mut first: std::collections::HashMap<SourceId, &CategoryTag> = std::collections::HashMap::new();
    let mut multi = HashSet::new();
    for g in lib.live.groups.iter().chain(&lib.vod.movie_groups).chain(&lib.vod.series_groups) {
        let Some(tag) = &g.tag else { continue };
        match first.get(&g.source_id) {
            None => {
                first.insert(g.source_id, tag);
            }
            Some(t) if *t != tag => {
                multi.insert(g.source_id);
            }
            Some(_) => {}
        }
    }
    multi
}

/// "All countries and languages" / "4 of 41 countries and languages" for a playlist (its tags
/// in the library).
pub(crate) fn showing_text(lib: &Library, source: SourceId, rule: Option<&CategoryFilter>) -> String {
    let present: BTreeSet<&CategoryTag> = lib
        .live
        .groups
        .iter()
        .chain(&lib.vod.movie_groups)
        .chain(&lib.vod.series_groups)
        .filter(|g| g.source_id == source)
        .filter_map(|g| g.tag.as_ref())
        .collect();
    match rule {
        None => "All countries and languages".into(),
        Some(r) => format!("{} of {} countries and languages", present.iter().filter(|t| r.keep.contains(**t)).count(), present.len()),
    }
}

/// Settings ▸ Hidden ▸ Enter on a tag: add it to the rule that hides it — the playlist's own,
/// else the shared one (read in the write, so a picker save in between isn't lost). Untagged
/// (`None`) turns `keep_untagged` on.
pub(crate) fn show_tag(ctx: &mut UiCtx<'_>, source: SourceId, tag: Option<CategoryTag>) {
    let name = tag_name(tag.as_ref());
    let shared = ctx.lib.source(source).is_some_and(|s| !s.own_category_filter) && ctx.lib.sources.len() > 1;
    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::keep_tag(c, source, tag).map(|_| ()));
    ctx.toast(Toast::info(if shared { format!("{name} shown in every playlist") } else { format!("{name} shown") }));
}

/// What a key or click did to the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Handled; the picker stays.
    None,
    /// Not a picker key (the app's global keys go on).
    Ignored,
    /// The rule was stored.
    Saved,
    /// Esc / Cancel: nothing stored.
    Cancelled,
}

/// A row of the picker's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// "Only for <playlist>".
    OnlyThis,
    /// "Also hide titles in other languages".
    OtherLanguages,
    /// Index into the summary's tags.
    Tag(usize),
}

/// What Enter stores.
#[derive(Debug, Clone, PartialEq)]
enum Save {
    /// The shared rule (and the playlist follows it).
    Shared(Option<CategoryFilter>),
    /// The playlist's own rule.
    Own(Option<CategoryFilter>),
    /// The playlist had its own rule and follows the shared one again, which stays as it is
    /// (its ticks weren't changed).
    Follow(Option<CategoryFilter>),
}

impl Save {
    fn rule(&self) -> Option<&CategoryFilter> {
        match self {
            Save::Shared(f) | Save::Own(f) | Save::Follow(f) => f.as_ref(),
        }
    }
}

/// One rule's ticks in the picker: the shared rule's and the playlist's own are kept apart, so
/// "Only for <playlist>" on and off again never carries one into the other.
#[derive(Debug, Clone, PartialEq, Default)]
struct Ticks {
    keep: BTreeSet<CategoryTag>,
    keep_untagged: bool,
    /// "Also hide titles in other languages": `None` = automatic (English-only ticks).
    title_langs: Option<bool>,
}

impl Ticks {
    fn of(rule: &CategoryFilter) -> Self {
        Ticks { keep: rule.keep.clone(), keep_untagged: rule.keep_untagged, title_langs: rule.hide_other_languages }
    }

    /// Every row ticked, titles in other languages shown: what no rule shows.
    fn everything(summary: &CategorySummary) -> Self {
        Ticks { keep: summary.present(), keep_untagged: true, title_langs: Some(false) }
    }
}

/// The picker's state and view (see the module docs).
pub struct CategoryPicker {
    source: SourceId,
    name: String,
    country: Option<String>,
    /// The categories the rule edited covers (shared or "Only for"), and the other ones.
    summary: Option<CategorySummary>,
    other: Option<CategorySummary>,
    /// Two or more playlists, or this one has its own rule: "Only for <playlist>" is offered.
    can_split: bool,
    /// "Only for <playlist>" is ticked.
    only_this: bool,
    /// The playlist had its own rule when the picker opened.
    was_own: bool,
    /// The rule shown: the playlist's own with "Only for" ticked, else the shared one.
    ticks: Ticks,
    /// The other rule's ticks (`None`: the own rule before "Only for" was first ticked).
    parked: Option<Ticks>,
    /// The shared rule's ticks as they were set up (unchanged: unticking "Only for" only makes
    /// the playlist follow the shared rule, which isn't rewritten).
    shared_initial: Option<Ticks>,
    /// Re-read the data whenever the library generation changes.
    follow: bool,
    /// Library generation the data was (or is being) read for.
    loaded_for: Option<u64>,
    loader: Option<Loader<PickerData>>,
    /// The ticks were set up (from the stored rule or the defaults); later reads keep them.
    started: bool,
    /// The shared rule and the playlist's own (when it has one) as stored: their `known` tags
    /// badge the others "new".
    stored_shared: Option<CategoryFilter>,
    stored_own: Option<CategoryFilter>,
    list: ListState,
    rows: Vec<Row>,
    /// The write in flight and what it stores.
    saving: Option<(Ticket<()>, Save)>,
    error: Option<String>,
    /// The button Esc mirrors ("Cancel"; onboarding: "Show everything").
    cancel: &'static str,
    /// Enter was pressed on ticks that show nothing of the playlists: the next Enter on these
    /// same ticks saves them.
    armed_empty: Option<(BTreeSet<CategoryTag>, bool)>,
    /// The choice as it was set up (unsaved changes = it differs).
    initial: Option<Choice>,
}

/// What [`CategoryPicker::is_edited`] compares: "Only for", then the shared and own ticks.
type Choice = (bool, Option<Ticks>, Option<Ticks>);

impl CategoryPicker {
    /// A picker that reads the playlist's data itself (and follows library changes).
    pub fn new(source: SourceId) -> Self {
        Self {
            source,
            name: String::new(),
            country: None,
            summary: None,
            other: None,
            can_split: false,
            only_this: false,
            was_own: false,
            ticks: Ticks { keep_untagged: true, ..Default::default() },
            parked: None,
            shared_initial: None,
            follow: true,
            loaded_for: None,
            loader: None,
            started: false,
            stored_shared: None,
            stored_own: None,
            list: ListState::default(),
            rows: Vec::new(),
            saving: None,
            error: None,
            cancel: "Cancel",
            armed_empty: None,
            initial: None,
        }
    }

    /// The shared rule's ticks and the playlist's own (`None` when it has none).
    fn sets(&self) -> (Option<&Ticks>, Option<&Ticks>) {
        if self.only_this { (self.parked.as_ref(), Some(&self.ticks)) } else { (Some(&self.ticks), self.parked.as_ref()) }
    }

    fn choice(&self) -> Choice {
        let (shared, own) = self.sets();
        // the own ticks only count while they're used
        (self.only_this, shared.cloned(), own.filter(|_| self.only_this).cloned())
    }

    /// Ticks changed and not saved.
    pub fn is_edited(&self) -> bool {
        self.initial.as_ref().is_some_and(|i| *i != self.choice())
    }

    /// Name the button that Esc mirrors.
    pub fn cancel_label(mut self, label: &'static str) -> Self {
        self.cancel = label;
        self
    }

    /// A picker over data already read (the onboarding step decides on it first). It re-reads
    /// when the library generation differs from `generation`; `None` = never (tests).
    pub(crate) fn from_data(source: SourceId, data: PickerData, generation: Option<u64>) -> Self {
        let mut p = Self::new(source);
        p.follow = generation.is_some();
        p.loaded_for = generation;
        p.set_data(data);
        p
    }

    pub fn source(&self) -> SourceId {
        self.source
    }

    /// The summary once read (of the playlists the edited rule covers).
    pub fn summary(&self) -> Option<&CategorySummary> {
        self.summary.as_ref()
    }

    /// The rule edits only this playlist ("Only for <playlist>").
    pub fn only_this(&self) -> bool {
        self.only_this
    }

    /// The playlist's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn set_data(&mut self, d: PickerData) {
        if !self.started {
            self.started = true;
            self.was_own = d.own.is_some();
            self.only_this = self.was_own;
            // the shared rule; without one the defaults, or — for a playlist with its own rule,
            // which may go back to following the shared one — everything, as that is what it
            // would show
            let shared = match &d.rule {
                Some(r) => Ticks::of(r),
                None if self.was_own => Ticks::everything(&d.summary),
                None => Ticks { keep: category_tags::default_keep(d.country.as_deref()), keep_untagged: true, title_langs: None },
            };
            self.shared_initial = Some(shared.clone());
            self.stored_shared = d.rule.clone();
            match &d.own {
                Some(own) => {
                    self.ticks = own.as_ref().map_or_else(|| Ticks::everything(&d.own_summary), Ticks::of);
                    self.parked = Some(shared);
                    self.stored_own = own.clone();
                }
                None => self.ticks = shared,
            }
            self.initial = Some(self.choice());
        }
        self.can_split = d.playlists > 1 || d.own.is_some();
        // the cursor stays on its entry when a re-read reorders the rows
        let at = self.cursor_row();
        self.name = d.name;
        self.country = d.country;
        let (active, other) = if self.only_this { (d.own_summary, d.summary) } else { (d.summary, d.own_summary) };
        self.summary = Some(active);
        self.other = Some(other);
        self.refilter();
        self.reselect(at);
    }

    /// The row under the cursor, as a stable key (the option, or the tag).
    fn cursor_row(&self) -> Option<Result<Row, Option<CategoryTag>>> {
        match *self.rows.get(self.list.cursor)? {
            Row::Tag(i) => Some(Err(self.summary.as_ref()?.tags.get(i)?.tag.clone())),
            r => Some(Ok(r)),
        }
    }

    fn reselect(&mut self, at: Option<Result<Row, Option<CategoryTag>>>) {
        let Some(at) = at else { return };
        let s = self.summary.as_ref();
        if let Some(i) = self.rows.iter().position(|r| match (r, &at) {
            (Row::Tag(i), Err(tag)) => s.is_some_and(|s| s.tags[*i].tag == *tag),
            (r, Ok(o)) => r == o,
            _ => false,
        }) {
            self.list.select(i);
        }
    }

    fn has_vod(&self) -> bool {
        [&self.summary, &self.other].iter().any(|s| s.as_ref().is_some_and(|s| s.tags.iter().any(|t| t.movies + t.series > 0)))
    }

    fn refilter(&mut self) {
        let Some(s) = &self.summary else { return };
        let hay: Vec<String> = s
            .tags
            .iter()
            .map(|t| match &t.tag {
                Some(tag) => format!("{} {}", category_tags::tag_short(tag), category_tags::tag_label(tag)),
                None => UNTAGGED.to_owned(),
            })
            .collect();
        let mut rows = Vec::new();
        if !self.list.filter.is_active() {
            if self.can_split {
                rows.push(Row::OnlyThis);
            }
            if self.has_vod() {
                rows.push(Row::OtherLanguages);
            }
        }
        rows.extend(self.list.filter.apply(hay.iter().map(String::as_str)).into_iter().map(|i| Row::Tag(i as usize)));
        self.rows = rows;
        self.list.set_len(self.rows.len());
    }

    fn tag_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().filter_map(|r| match r {
            Row::Tag(i) => Some(*i),
            _ => None,
        })
    }

    fn ticked(&self, tag: Option<&CategoryTag>) -> bool {
        match tag {
            Some(t) => self.ticks.keep.contains(t),
            None => self.ticks.keep_untagged,
        }
    }

    fn set_tick(&mut self, tag: Option<&CategoryTag>, on: bool) {
        match tag {
            Some(t) if on => {
                self.ticks.keep.insert(t.clone());
            }
            Some(t) => {
                self.ticks.keep.remove(t);
            }
            None => self.ticks.keep_untagged = on,
        }
    }

    /// "Also hide titles in other languages" as it stands (automatic: English-only ticks).
    fn hides_other_languages(&self) -> bool {
        self.ticks.title_langs.unwrap_or_else(|| category_tags::keeps_only_english(&self.ticks.keep))
    }

    /// "Only for <playlist>" on / off: the rows become that playlist's (or the shared) tags,
    /// and the ticks that rule's. The first time on, the own ticks start from the shared ones;
    /// off again, the shared ticks are back as they were left.
    fn set_only_this(&mut self, on: bool) {
        if on == self.only_this || !self.can_split {
            return;
        }
        self.only_this = on;
        let parked = self.parked.take().unwrap_or_else(|| self.ticks.clone());
        self.parked = Some(std::mem::replace(&mut self.ticks, parked));
        self.armed_empty = None;
        std::mem::swap(&mut self.summary, &mut self.other);
        self.refilter();
        self.reselect(Some(Ok(Row::OnlyThis)));
    }

    fn toggle_row(&mut self, row: usize) {
        match self.rows.get(row).copied() {
            Some(Row::OnlyThis) => self.set_only_this(!self.only_this),
            Some(Row::OtherLanguages) => self.ticks.title_langs = Some(!self.hides_other_languages()),
            Some(Row::Tag(i)) => {
                let Some(tag) = self.summary.as_ref().and_then(|s| s.tags.get(i)).map(|t| t.tag.clone()) else { return };
                let on = !self.ticked(tag.as_ref());
                self.set_tick(tag.as_ref(), on);
            }
            None => {}
        }
    }

    /// Tick every tag row the filter shows, or none when they all are.
    fn set_shown(&mut self, on: Option<bool>) {
        let Some(s) = &self.summary else { return };
        let tags: Vec<Option<CategoryTag>> = self.tag_rows().map(|i| s.tags[i].tag.clone()).collect();
        let on = on.unwrap_or_else(|| !tags.iter().all(|t| self.ticked(t.as_ref())));
        for t in &tags {
            self.set_tick(t.as_ref(), on);
        }
    }

    fn defaults(&mut self) {
        self.ticks = Ticks { keep: category_tags::default_keep(self.country.as_deref()), keep_untagged: true, title_langs: None };
    }

    /// The rule Enter stores: `None` when every entry is ticked and titles in other languages
    /// aren't hidden (no filtering).
    fn to_save(&self) -> Option<CategoryFilter> {
        let s = self.summary.as_ref()?;
        let present = s.present();
        let untagged_present = s.tags.iter().any(|t| t.tag.is_none());
        let everything = present.iter().all(|t| self.ticks.keep.contains(t)) && (self.ticks.keep_untagged || !untagged_present);
        (!everything || self.hides_other_languages()).then(|| CategoryFilter {
            keep: self.ticks.keep.clone(),
            keep_untagged: self.ticks.keep_untagged,
            known: present,
            hide_other_languages: self.ticks.title_langs,
        })
    }

    fn to_store(&self) -> Save {
        if self.only_this {
            Save::Own(self.to_save())
        } else if self.was_own && self.shared_initial.as_ref() == Some(&self.ticks) {
            // "Only for" unticked, the shared ticks untouched: follow the shared rule as it is
            Save::Follow(self.stored_shared.clone())
        } else {
            Save::Shared(self.to_save())
        }
    }

    /// The ticks show nothing of the playlists (no present tag ticked, untagged off or absent).
    fn shows_nothing(&self) -> bool {
        let Some(s) = &self.summary else { return false };
        !s.tags.is_empty() && s.tags.iter().all(|t| !self.ticked(t.tag.as_ref()))
    }

    /// "6 of 42 ticked · 3 210 of 48 900 channels" for the ticks as they are.
    fn effect_text(&self) -> Option<String> {
        let s = self.summary.as_ref()?;
        let kept: Vec<&TagSummary> = s.tags.iter().filter(|t| self.ticked(t.tag.as_ref())).collect();
        let sum = |v: &mut dyn Iterator<Item = &TagSummary>| v.map(|t| t.items() as usize).sum::<usize>();
        let vod = s.tags.iter().any(|t| t.movies + t.series > 0);
        Some(format!(
            "{} of {} ticked · {} of {} {}{}",
            kept.len(),
            s.tags.len(),
            grouped(sum(&mut kept.iter().copied())),
            grouped(sum(&mut s.tags.iter())),
            if vod { "channels and titles" } else { "channels" },
            if vod && self.hides_other_languages() { " (fewer titles: other languages hidden)" } else { "" }
        ))
    }

    /// Who the choice applies to: "Applies to every playlist" / "Only TV 2".
    fn scope_text(&self) -> Option<String> {
        if !self.can_split {
            return None;
        }
        Some(if self.only_this {
            format!("Only {} — your other playlists keep the shared choice.", self.name)
        } else {
            "Applies to every playlist, also ones you add later (unless one has its own).".to_owned()
        })
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) {
        if self.saving.is_some() || self.summary.is_none() {
            return;
        }
        // nothing of the playlists would show: say so, and save on a second Enter
        if self.shows_nothing() {
            let ticks = (self.ticks.keep.clone(), self.ticks.keep_untagged);
            if self.armed_empty.as_ref() != Some(&ticks) {
                self.armed_empty = Some(ticks);
                return;
            }
        }
        self.armed_empty = None;
        let save = self.to_store();
        let (id, write, was_own) = (self.source, save.clone(), self.was_own);
        let ticket = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
            use crate::store::sources::{CategoryRule, set_category_rule, set_shared_category_filter};
            match write {
                Save::Own(f) => set_category_rule(c, id, &CategoryRule::Own(f)),
                Save::Follow(_) => set_category_rule(c, id, &CategoryRule::Shared),
                Save::Shared(f) => {
                    set_shared_category_filter(c, f.as_ref())?;
                    if was_own { set_category_rule(c, id, &CategoryRule::Shared) } else { Ok(()) }
                }
            }
        });
        self.saving = Some((ticket, save));
        self.error = None;
    }

    /// A key (see the module docs).
    pub fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> PickerOutcome {
        match action {
            Action::Activate => self.save(ctx),
            Action::ToggleMark => self.toggle_row(self.list.cursor),
            Action::MarkAll => self.set_shown(None),
            Action::MarkDefaults => self.defaults(),
            Action::Back if self.list.filter.is_active() => {
                self.list.filter.clear();
                self.refilter();
            }
            Action::Back => return PickerOutcome::Cancelled,
            // the Text twin of Space (tag names are one word)
            Action::FilterInput(' ') => {}
            Action::Up
            | Action::Down
            | Action::PageUp
            | Action::PageDown
            | Action::Home
            | Action::End
            | Action::FilterInput(_)
            | Action::FilterBackspace
            | Action::FilterDeleteWord
            | Action::FilterClear => {
                if self.list.handle(action) == ListOutcome::FilterChanged {
                    self.refilter();
                }
            }
            // a playlist imported before tags were recognised: refresh it for real (an unforced
            // refresh may get "304 Not Modified" and never write the raw names)
            Action::RefreshSource if self.summary.as_ref().is_some_and(|s| s.needs_refresh) => self.refresh(ctx),
            Action::Left | Action::Right | Action::FocusNext | Action::FocusPrev | Action::ActivateAlt => {}
            _ => return PickerOutcome::Ignored,
        }
        PickerOutcome::None
    }

    fn refresh(&mut self, ctx: &mut UiCtx<'_>) {
        ctx.services.jobs.submit(JobKind::RefreshSource { source: self.source, force: true });
        ctx.toast(Toast::info("Refreshing the playlist…"));
    }

    /// Reads and the pending write; `Saved` once the write answered.
    fn poll(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> PickerOutcome {
        let generation = ctx.lib.generation;
        if self.follow && self.loader.is_none() && self.loaded_for != Some(generation) {
            self.loaded_for = Some(generation);
            let id = self.source;
            let hide_explicit = ctx.parental.hides_explicit(&ctx.config.parental);
            self.loader = Some(Loader::start(ctx, move |c| load(c, id, hide_explicit)));
        }
        match kit::poll(&mut self.loader) {
            Some(Ok(d)) => self.set_data(d),
            Some(Err(e)) => self.error = Some(format!("Couldn't read the categories: {}", crate::redact::redact(&e.to_string()))),
            None => {}
        }
        if let Some((ticket, save)) = &mut self.saving {
            match ticket.try_take() {
                Some(Ok(())) => {
                    let whom = match (&*save, self.can_split) {
                        (Save::Own(_), _) => format!(" in {}", self.name),
                        (Save::Follow(_), _) => format!(" in {} (the choice every playlist shares)", self.name),
                        (Save::Shared(_), true) => " in every playlist".to_owned(),
                        (Save::Shared(_), false) => String::new(),
                    };
                    let text = match (save.rule(), &self.summary) {
                        (Some(f), Some(s)) => {
                            let present = s.present();
                            format!(
                                "Showing {} of {} countries and languages{whom} · Settings ▸ Hidden items shows the rest",
                                present.iter().filter(|t| f.keep.contains(*t)).count(),
                                present.len()
                            )
                        }
                        _ => format!("Showing every country and language{whom}"),
                    };
                    self.saving = None;
                    ctx.toast(Toast::info(text));
                    return PickerOutcome::Saved;
                }
                Some(Err(e)) => {
                    self.saving = None;
                    self.error = Some(format!("Couldn't save: {}", crate::redact::redact(&e.to_string())));
                }
                None => egui_ctx.request_repaint_after(Duration::from_millis(30)),
            }
        }
        PickerOutcome::None
    }

    /// Draw the picker into `ui` (the caller draws the title above it).
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> PickerOutcome {
        let egui_ctx = ui.ctx().clone();
        if self.poll(&egui_ctx, ctx) == PickerOutcome::Saved {
            return PickerOutcome::Saved;
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        search_header(ui, theme, self.list.filter.query(), "Type to filter countries", Some(glyphs::SEARCH));
        ui.add_space(theme.space(SpaceToken::Sm));
        let small = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
            ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(color)).wrap());
        };
        let mut toggled = None;
        let mut refresh = false;
        match &self.summary {
            None if self.error.is_none() => small(ui, "Reading the categories…", t.text_secondary),
            None => {}
            Some(s) => {
                if s.needs_refresh {
                    ui.horizontal_wrapped(|ui| {
                        let key = crate::ui::screens::live::shortcut(KeyContext::Global, Action::RefreshSource).unwrap_or_default();
                        let note = format!("Some categories were imported before countries were recognised — refresh to find them all ({key}).");
                        small(ui, &note, t.warning);
                        refresh =
                            button(ui, theme, Some(glyphs::REFRESH), "Refresh now", ButtonStyle { bordered: true, ..Default::default() }).clicked();
                    });
                    ui.add_space(theme.space(SpaceToken::Sm));
                }
                if !self.rows.iter().any(|r| matches!(r, Row::Tag(_))) && self.list.filter.is_active() {
                    small(ui, &format!("No matches for “{}”", self.list.filter.query()), t.text_secondary);
                } else {
                    if s.tags.is_empty() {
                        small(ui, "This playlist has no categories yet", t.text_secondary);
                    }
                    let row_h = row_height(theme, crate::config::Density::Comfortable, true);
                    let max_h = (egui_ctx.content_rect().height() * 0.45).max(row_h * 3.0);
                    let h = (self.rows.len() as f32 * row_h).min(max_h);
                    let (rows, keep, untagged) = (&self.rows, &self.ticks.keep, self.ticks.keep_untagged);
                    // the stored rule of the ticks shown (field borrows: the list is borrowed below)
                    let stored = if self.only_this { self.stored_own.as_ref() } else { self.stored_shared.as_ref() };
                    let known = stored.map(|r| &r.known).filter(|k| !k.is_empty());
                    let only_label = format!("Only for {}", self.name);
                    let only_detail = if self.only_this {
                        "This playlist has its own choice — untick to use the one every playlist shares"
                    } else {
                        "Tick to give this playlist its own choice; the others keep the shared one"
                    };
                    let (only_this, hides) = (self.only_this, self.hides_other_languages());
                    let list = &mut self.list;
                    ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
                        virtual_list(ui, theme, ("category-picker", self.source), list, row_h, |ui, rc| {
                            let resp = match rows[rc.index] {
                                Row::OnlyThis => check_row(ui, theme, only_this, None, &only_label, Some(only_detail), false, rc.state),
                                Row::OtherLanguages => check_row(
                                    ui,
                                    theme,
                                    hides,
                                    None,
                                    "Also hide titles in other languages",
                                    Some("Movies and series whose name marks another language (“DE - …”, “AR-SUBS - …”), also in the countries ticked. Favourites and titles you started stay."),
                                    false,
                                    rc.state,
                                ),
                                Row::Tag(i) => {
                                    let e = &s.tags[i];
                                    let ticked = e.tag.as_ref().map_or(untagged, |tag| keep.contains(tag));
                                    let code = e.tag.as_ref().map(category_tags::tag_short);
                                    let new = e.tag.as_ref().is_some_and(|tag| known.is_some_and(|k| !k.contains(tag)));
                                    let label = tag_name(e.tag.as_ref());
                                    check_row(ui, theme, ticked, code.as_deref(), &label, Some(&counts_text(e)), new, rc.state)
                                }
                            };
                            if resp.clicked() {
                                toggled = Some(rc.index);
                            }
                            resp
                        });
                    });
                }
            }
        }
        if let Some(row) = toggled {
            self.list.select(row);
            self.toggle_row(row);
        }
        if refresh {
            self.refresh(ctx);
        }
        ui.add_space(theme.space(SpaceToken::Sm));
        if let Some(text) = self.scope_text() {
            small(ui, &text, t.text_secondary);
        }
        if let Some(text) = self.effect_text() {
            small(ui, &text, t.text_secondary);
        }
        if self.shows_nothing() {
            let armed = self.armed_empty.as_ref() == Some(&(self.ticks.keep.clone(), self.ticks.keep_untagged));
            let text =
                if armed { "Nothing from this playlist will show — save again to save anyway." } else { "Nothing from this playlist will show." };
            small(ui, text, t.warning);
        }
        small(ui, "Favourites in hidden countries are hidden too. Settings ▸ Hidden items shows any of them again.", t.text_disabled);
        if let Some(e) = &self.error {
            small(ui, e, t.urgent);
        }
        ui.add_space(theme.space(SpaceToken::Md));
        let saving = self.saving.is_some();
        let mut out = PickerOutcome::None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
            let plain = ButtonStyle { bordered: true, ..Default::default() };
            if button(ui, theme, None, "Select all", plain).clicked() {
                self.set_shown(Some(true));
            }
            if button(ui, theme, None, "None", plain).clicked() {
                self.set_shown(Some(false));
            }
            if button(ui, theme, None, "My country + English", plain).clicked() {
                self.defaults();
            }
            ui.add_enabled_ui(self.summary.is_some() && !saving, |ui| {
                if button(
                    ui,
                    theme,
                    Some(glyphs::CHECK),
                    if saving { "Saving…" } else { "Save" },
                    ButtonStyle { primary: true, ..Default::default() },
                )
                .clicked()
                {
                    self.save(ctx);
                }
            });
            if button(ui, theme, None, self.cancel, plain).clicked() {
                out = PickerOutcome::Cancelled;
            }
        });
        ui.add_space(theme.space(SpaceToken::Sm));
        let keys = format!("{} · Esc {}", crate::keymap::hint_line(&[KeyContext::Checklist, KeyContext::Modal]), self.cancel.to_lowercase());
        small(ui, &keys, t.text_disabled);
        out
    }
}

/// The picker as a dialog (Settings).
pub struct CategoriesOverlay {
    picker: CategoryPicker,
}

impl CategoriesOverlay {
    pub fn new(source: SourceId) -> Self {
        Self { picker: CategoryPicker::new(source) }
    }
}

fn outcome(o: PickerOutcome) -> OverlayOutcome {
    match o {
        PickerOutcome::None => OverlayOutcome::Keep,
        PickerOutcome::Ignored => OverlayOutcome::Ignored,
        PickerOutcome::Saved | PickerOutcome::Cancelled => OverlayOutcome::Close,
    }
}

impl Overlay for CategoriesOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Checklist, KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        outcome(self.picker.on_action(ctx, action))
    }

    fn is_dirty(&self) -> bool {
        self.picker.is_edited()
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        if scrim_guarded(egui_ctx, theme, "categories", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        let picker = &mut self.picker;
        let r = card_area(egui_ctx, theme, "categories", CardKind::Menu, 600.0 * theme.rem(), None, |ui| {
            let t = &theme.tokens;
            ui.label(RichText::new("Choose what to show").size(theme.font(TypeToken::Heading)).color(t.fg));
            ui.add_space(theme.space(SpaceToken::Xs));
            let sub = match picker.summary() {
                Some(s) if picker.only_this() => {
                    format!("{}: categories in {} countries and languages. Untick what you don't want to see.", picker.name(), s.tag_count())
                }
                Some(s) => format!("Your playlists: categories in {} countries and languages. Untick what you don't want to see.", s.tag_count()),
                None => picker.name().to_owned(),
            };
            ui.add(egui::Label::new(RichText::new(sub).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
            ui.add_space(theme.space(SpaceToken::Lg));
            picker.ui(ui, ctx)
        });
        outcome(r.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::home::testkit::{self, rig, seed};

    fn tag(s: &str) -> CategoryTag {
        CategoryTag(s.to_owned())
    }

    fn entry(t: Option<&str>, channels: u32) -> TagSummary {
        TagSummary { tag: t.map(tag), live_groups: 1, channels, ..Default::default() }
    }

    /// A playlist with UK, US, French, Arabic and English-language categories plus untagged ones.
    fn data(rule: Option<CategoryFilter>) -> PickerData {
        let summary = CategorySummary {
            tags: vec![
                entry(Some("lang:ar"), 900),
                entry(Some("FR"), 500),
                entry(Some("GB"), 400),
                entry(Some("US"), 300),
                entry(Some("lang:en"), 200),
                entry(None, 50),
            ],
            needs_refresh: false,
        };
        PickerData { own_summary: summary.clone(), summary, rule, own: None, name: "My IPTV".into(), playlists: 1, country: Some("GB".into()) }
    }

    fn ticks(p: &CategoryPicker) -> Vec<String> {
        let s = p.summary().unwrap();
        s.tags.iter().filter(|t| p.ticked(t.tag.as_ref())).map(|t| t.tag.as_ref().map_or("-".to_owned(), |t| t.0.clone())).collect()
    }

    /// DESIGN §28.1 decision 3 and §28.6: the defaults, the keys, and what Enter stores.
    #[test]
    fn defaults_keys_and_the_stored_rule() {
        let (mut t, one) = rig();
        let mut p = CategoryPicker::from_data(SourceId(1), data(None), None);
        // no rule yet: the user's country + English + untagged, nothing else
        assert_eq!(ticks(&p), ["GB", "US", "lang:en", "-"]);
        t.with_ctx(|ctx| {
            // Space on the first row (Arabic) ticks it, again unticks it
            assert_eq!(p.on_action(ctx, Action::ToggleMark), PickerOutcome::None);
            assert_eq!(ticks(&p)[0], "lang:ar");
            p.on_action(ctx, Action::ToggleMark);
            // the Text twin of Space doesn't filter
            p.on_action(ctx, Action::FilterInput(' '));
            assert!(!p.list.filter.is_active());
            // filter "fr" → Ctrl+A ticks only the rows shown, again unticks them
            p.on_action(ctx, Action::FilterInput('f'));
            p.on_action(ctx, Action::FilterInput('r'));
            assert_eq!(p.rows.len(), 1, "{:?}", p.rows);
            p.on_action(ctx, Action::MarkAll);
            assert_eq!(ticks(&p), ["FR", "GB", "US", "lang:en", "-"]);
            // Esc clears the filter first, then cancels
            assert_eq!(p.on_action(ctx, Action::Back), PickerOutcome::None);
            assert_eq!(p.rows.len(), 6);
            // Ctrl+A with some unticked ticks all; again: none
            p.on_action(ctx, Action::MarkAll);
            assert_eq!(ticks(&p).len(), 6);
            assert_eq!(p.to_save(), None, "everything ticked stores no rule");
            p.on_action(ctx, Action::MarkAll);
            assert!(ticks(&p).is_empty());
            let none = p.to_save().unwrap();
            assert!(!none.keep_untagged && none.keep.iter().all(|t| !p.summary().unwrap().present().contains(t)));
            // ticks that show nothing of the playlist: the first Enter only warns, the second saves
            p.on_action(ctx, Action::Activate);
            assert!(p.saving.is_none() && p.shows_nothing());
            p.on_action(ctx, Action::Activate);
            assert!(p.saving.take().is_some());
            // Ctrl+E: back to my country + English
            p.on_action(ctx, Action::MarkDefaults);
            assert_eq!(ticks(&p), ["GB", "US", "lang:en", "-"]);
            assert_eq!(p.on_action(ctx, Action::Back), PickerOutcome::Cancelled);
            assert_eq!(p.on_action(ctx, Action::GoHome), PickerOutcome::Ignored);
        });
        // the stored rule keeps the English tags the playlist doesn't have (yet), and knows
        // which tags it had
        let rule = p.to_save().unwrap();
        for absent in ["CA", "AU", "IE", "NZ"] {
            assert!(rule.keep.contains(&tag(absent)), "{absent}");
        }
        assert!(!rule.keep.contains(&tag("FR")) && !rule.keep.contains(&tag("lang:ar")));
        assert!(rule.keep_untagged);
        assert_eq!(rule.known, ["lang:ar", "FR", "GB", "US", "lang:en"].into_iter().map(tag).collect::<BTreeSet<_>>());
        // a saved rule is where the ticks start (not the defaults); tags it didn't know are "new"
        let saved = CategoryFilter { keep: [tag("FR")].into(), keep_untagged: false, known: [tag("FR"), tag("GB")].into(), ..Default::default() };
        let mut p2 = CategoryPicker::from_data(SourceId(1), data(Some(saved)), None);
        assert_eq!(ticks(&p2), ["FR"]);
        // F5 on a playlist imported before tags were recognised: a forced refresh of it (an
        // unforced one may be "304 Not Modified" and never write the raw names); otherwise the
        // app's own refresh
        let mut stale = data(None);
        stale.summary.needs_refresh = true;
        let mut p3 = CategoryPicker::from_data(SourceId(7), stale, None);
        t.with_ctx(|ctx| {
            assert_eq!(p2.on_action(ctx, Action::RefreshSource), PickerOutcome::Ignored);
            assert_eq!(p3.on_action(ctx, Action::RefreshSource), PickerOutcome::None);
        });
        let forced = t.events.try_iter().any(|e| {
            matches!(e, crate::msg::AppEvent::Job(crate::jobs::JobEvent::Updated(st)) if st.kind == JobKind::RefreshSource { source: SourceId(7), force: true })
        });
        assert!(forced);
        drop((t, one));
    }

    /// DESIGN §28.3 / §28.12: the picker edits the rule every playlist shares; "Only for
    /// <playlist>" (a row, ticked with Space) edits that playlist's own instead, over its own
    /// categories; "Also hide titles in other languages" is on by itself for English-only ticks;
    /// Ctrl+A ticks tags only; a playlist's own "show everything" ticks every row.
    #[test]
    fn shared_rule_only_for_this_playlist_and_other_languages() {
        let (mut t, one) = rig();
        let (tv, tv2) = seed(&mut t, |c| Ok((testkit::source(c, "TV")?, testkit::source(c, "TV 2")?)));
        let movies = |t: &str, n: u32| TagSummary { tag: Some(tag(t)), movie_groups: 1, movies: n, ..Default::default() };
        let shared = CategorySummary { tags: vec![movies("DE", 900), movies("GB", 500), movies("FR", 100)], needs_refresh: false };
        let own = CategorySummary { tags: vec![movies("GB", 500), movies("FR", 100)], needs_refresh: false };
        let data = |own_rule: Option<Option<CategoryFilter>>| PickerData {
            summary: shared.clone(),
            own_summary: own.clone(),
            rule: None,
            own: own_rule,
            name: "TV 2".into(),
            playlists: 2,
            country: Some("GB".into()),
        };
        let mut p = CategoryPicker::from_data(tv2, data(None), None);
        assert_eq!(p.rows[..2], [Row::OnlyThis, Row::OtherLanguages]);
        assert_eq!(ticks(&p), ["GB"], "the shared rule's rows: every playlist's tags");
        assert!(p.hides_other_languages(), "automatic: English-only ticks");
        assert!(matches!(p.to_store(), Save::Shared(Some(ref f)) if f.hide_other_languages.is_none() && f.known.len() == 3));
        t.with_ctx(|ctx| {
            // Space on "Also hide titles in other languages" turns it off for good
            p.list.select(1);
            p.on_action(ctx, Action::ToggleMark);
            assert!(!p.hides_other_languages());
            // Ctrl+A ticks the tags only
            p.on_action(ctx, Action::MarkAll);
            assert_eq!(ticks(&p), ["DE", "GB", "FR"]);
            assert!(!p.only_this && !p.hides_other_languages());
            assert_eq!(p.to_store(), Save::Shared(None), "everything shown: no rule");
            p.on_action(ctx, Action::MarkDefaults);
            // Space on "Only for TV 2": TV 2's own categories
            p.list.select(0);
            p.on_action(ctx, Action::ToggleMark);
            assert!(p.only_this);
            assert_eq!(p.summary().unwrap().tags.len(), 2);
            assert_eq!(p.list.cursor, 0, "the cursor stays on the option");
            p.on_action(ctx, Action::Activate);
        });
        let (ticket, save) = p.saving.take().expect("saving");
        assert!(matches!(save, Save::Own(Some(_))));
        ticket.wait().unwrap();
        let r = t.services().store.reader().unwrap();
        let st = crate::store::sources::get_source(&r, tv2).unwrap().settings;
        assert_eq!(st.own_category_filter, Some(true));
        assert!(st.category_filter.unwrap().keep.contains(&tag("GB")));
        assert_eq!(crate::store::sources::shared_category_filter(&r).unwrap(), None, "the shared rule untouched");
        assert!(!crate::store::sources::get_source(&r, tv).unwrap().settings.has_own_category_filter());
        // a playlist that shows everything of its own: every row ticked, "Only for" on; unticking
        // it and saving makes the playlist follow the shared rule again
        let mut p = CategoryPicker::from_data(tv2, data(Some(None)), None);
        assert!(p.only_this && !p.hides_other_languages());
        assert_eq!(ticks(&p), ["GB", "FR"]);
        t.with_ctx(|ctx| {
            p.list.select(0);
            p.on_action(ctx, Action::ToggleMark);
            p.on_action(ctx, Action::Activate);
        });
        let (ticket, save) = p.saving.take().expect("saving");
        assert_eq!(save, Save::Follow(None), "only \"Only for\" unticked: follow, the shared rule untouched");
        ticket.wait().unwrap();
        assert_eq!(crate::store::sources::get_source(&r, tv2).unwrap().settings.own_category_filter, Some(false));
        assert_eq!(crate::store::sources::shared_category_filter(&r).unwrap(), None, "None stays None");
        // an own French-only rule: unticking "Only for" shows the shared ticks, never the own
        // ones, and saves nothing over the shared rule; ticking it again brings the own ticks back
        let fr = CategoryFilter {
            keep: [tag("FR")].into(),
            keep_untagged: false,
            known: [tag("GB"), tag("FR")].into(),
            hide_other_languages: Some(false),
        };
        let gb = CategoryFilter {
            keep: [tag("GB")].into(),
            keep_untagged: true,
            known: [tag("DE"), tag("GB"), tag("FR")].into(),
            hide_other_languages: None,
        };
        let mut d = data(Some(Some(fr.clone())));
        d.rule = Some(gb.clone());
        let mut p = CategoryPicker::from_data(tv2, d, None);
        assert_eq!(ticks(&p), ["FR"]);
        assert!(!p.is_edited());
        p.set_only_this(false);
        assert_eq!(ticks(&p), ["GB"], "the shared rule's ticks");
        assert!(p.hides_other_languages(), "the shared rule's option (automatic)");
        assert!(p.is_edited());
        assert_eq!(p.to_store(), Save::Follow(Some(gb.clone())));
        p.set_only_this(true);
        assert_eq!(ticks(&p), ["FR"], "the own ticks as they were");
        assert!(!p.is_edited());
        // the shared ticks changed after unticking: the shared rule is written, from them
        p.set_only_this(false);
        p.set_tick(Some(&tag("DE")), true);
        match p.to_store() {
            Save::Shared(Some(f)) => assert!(f.keep == [tag("DE"), tag("GB")].into() && f.keep_untagged && f.hide_other_languages.is_none()),
            other => panic!("{other:?}"),
        }
        // with no shared rule, a playlist with its own one sees "everything" as the shared ticks
        let mut p = CategoryPicker::from_data(tv2, data(Some(Some(fr))), None);
        p.set_only_this(false);
        assert_eq!(ticks(&p), ["DE", "GB", "FR"]);
        assert!(!p.hides_other_languages());
        assert_eq!(p.to_store(), Save::Follow(None));
        drop((t, one));
    }

    /// The dialog (Settings ▸ Playlists ▸ Categories…): it reads the playlist's categories off
    /// the UI thread, Space ticks the row the filter shows, Enter stores the shared rule (the
    /// library rebuild applies it) and closes.
    #[test]
    fn the_dialog_reads_the_playlist_and_enter_stores_the_rule() {
        let (mut t, one) = rig();
        let src = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            // EX-YU: never anyone's default, whatever country the machine is in
            for (i, raw) in ["UK | News", "EX-YU | Sport", "Sports"].iter().enumerate() {
                c.execute(
                    "INSERT INTO grp(source_id, kind, key, name, raw_name, provider_order, item_count, seen_gen) VALUES (?1,'channel',?2,?3,?3,?4,3,1)",
                    rusqlite::params![s, format!("channel:g{i}"), raw, i as i64],
                )?;
            }
            Ok(s)
        });
        let mut h = testkit::overlay_harness(t, one, CategoriesOverlay::new(src), egui::vec2(1200.0, 900.0));
        let (uk, yu) = (tag_name(Some(&tag("GB"))), tag_name(Some(&tag("region:exyu"))));
        testkit::until_ov(&mut h, "the categories", |h| testkit::has_label(h, &yu));
        assert!(testkit::has_label(&h, "Choose what to show") && testkit::has_label(&h, &uk) && testkit::has_label(&h, UNTAGGED));
        assert_eq!(h.state().o.key_contexts(), [KeyContext::Checklist, KeyContext::Modal]);
        // the UK is ticked by default (English); filter to it and untick it
        for c in ['u', 'k'] {
            assert_eq!(h.state_mut().act(Action::FilterInput(c)), "Keep");
        }
        h.state_mut().act(Action::ToggleMark);
        assert_eq!(h.state_mut().act(Action::Activate), "Keep", "the write runs");
        testkit::until_ov(&mut h, "the dialog to close", |h| h.state().last == "Close");
        let r = h.state().t.services().store.reader().unwrap();
        let rule = crate::store::sources::shared_category_filter(&r).unwrap().expect("a shared rule");
        assert!(!crate::store::sources::get_source(&r, src).unwrap().settings.has_own_category_filter(), "the playlist follows it");
        assert!(!rule.keep.contains(&tag("region:exyu")) && !rule.keep.contains(&tag("GB")) && rule.keep_untagged);
        assert_eq!(rule.known, [tag("GB"), tag("region:exyu")].into());
        assert!(h.state().t.toasts.iter().any(|x| x.text.starts_with("Showing 0 of 2 countries and languages")), "{:?}", h.state().t.toasts);
    }
}
