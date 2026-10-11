//! "Choose what to show" — the per-playlist category filter picker (DESIGN §28.6). Owner: P6-ui.
//!
//! One implementation: [`CategoryPicker`] (state, keys and view) is embedded by the onboarding
//! step and wrapped by [`CategoriesOverlay`] (Settings ▸ Playlists ▸ playlist ▸ Categories…,
//! Settings ▸ Hidden ▸ Choose countries…).
//!
//! * Rows: the playlist's tags by item count, then "No country tag"; each with a tick box, the
//!   short code, the name, what it covers and a "new" badge for tags the saved rule didn't know.
//! * Ticks start from the saved rule; without one from the defaults (the user's country and
//!   its language, every English-language tag, untagged — `category_tags::default_keep`).
//! * Saving stores the ticked tags — also ticked tags the playlist doesn't have (yet), so the
//!   English defaults keep working when such a category appears — and `known` = the tags
//!   present. Everything ticked stores no rule at all.
//! * Keys (`[Checklist, Modal]`): ↑/↓ move, Space / Insert tick, Ctrl+A all / none of the rows
//!   shown, Ctrl+E just my country + English, typing filters, Enter saves, Esc clears the
//!   filter, then cancels. F5 / Ctrl+R on a playlist imported before tags were recognised
//!   (`needs_refresh`) runs a forced refresh of it (an unforced one may get "304 Not Modified"
//!   and never write the raw names).
//! * A line above the buttons says what the ticks show ("6 of 42 ticked · 3 210 of 48 900
//!   channels"). Ticks that show nothing of the playlist warn, and Enter then asks once more.
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
    pub summary: CategorySummary,
    /// The rule stored now (`None` = no filtering).
    pub rule: Option<CategoryFilter>,
    pub name: String,
    /// The user's country (`presets::detect_country`, which may read a file).
    pub country: Option<String>,
}

/// Read a playlist's category summary and rule (a `Loader` body).
pub(crate) fn load(c: &rusqlite::Connection, id: SourceId, hide_explicit: bool) -> crate::error::Result<PickerData> {
    let summary = crate::store::library::category_summary(c, id, hide_explicit)?;
    let src = crate::store::sources::get_source(c, id)?;
    Ok(PickerData { summary, rule: src.settings.category_filter, name: src.name, country: crate::sources::presets::detect_country() })
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

/// Settings ▸ Hidden ▸ Enter on a tag: add it to the playlist's rule (read in the write, so a
/// picker save in between isn't lost). Untagged (`None`) turns `keep_untagged` on.
pub(crate) fn show_tag(ctx: &mut UiCtx<'_>, source: SourceId, tag: Option<CategoryTag>) {
    let name = tag_name(tag.as_ref());
    let _ = ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| {
        let Some(mut rule) = crate::store::sources::get_source(c, source)?.settings.category_filter else { return Ok(()) };
        match tag {
            Some(t) => {
                rule.keep.insert(t);
            }
            None => rule.keep_untagged = true,
        }
        crate::store::sources::set_category_filter(c, source, Some(&rule))
    });
    ctx.toast(Toast::info(format!("{name} shown")));
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

/// The picker's state and view (see the module docs).
pub struct CategoryPicker {
    source: SourceId,
    name: String,
    country: Option<String>,
    summary: Option<CategorySummary>,
    /// Re-read the data whenever the library generation changes.
    follow: bool,
    /// Library generation the data was (or is being) read for.
    loaded_for: Option<u64>,
    loader: Option<Loader<PickerData>>,
    /// The ticks were set up (from the stored rule or the defaults); later reads keep them.
    started: bool,
    /// The rule stored when the picker opened.
    stored: Option<CategoryFilter>,
    keep: BTreeSet<CategoryTag>,
    keep_untagged: bool,
    list: ListState,
    /// Indices into `summary.tags` that the filter shows.
    rows: Vec<usize>,
    /// The write in flight and what it stores.
    saving: Option<(Ticket<()>, Option<CategoryFilter>)>,
    error: Option<String>,
    /// The button Esc mirrors ("Cancel"; onboarding: "Show everything").
    cancel: &'static str,
    /// Enter was pressed on ticks that show nothing of the playlist: the next Enter on these
    /// same ticks saves them.
    armed_empty: Option<(BTreeSet<CategoryTag>, bool)>,
    /// The ticks as they were set up (unsaved changes = the ticks differ).
    initial: Option<(BTreeSet<CategoryTag>, bool)>,
}

impl CategoryPicker {
    /// A picker that reads the playlist's data itself (and follows library changes).
    pub fn new(source: SourceId) -> Self {
        Self {
            source,
            name: String::new(),
            country: None,
            summary: None,
            follow: true,
            loaded_for: None,
            loader: None,
            started: false,
            stored: None,
            keep: BTreeSet::new(),
            keep_untagged: true,
            list: ListState::default(),
            rows: Vec::new(),
            saving: None,
            error: None,
            cancel: "Cancel",
            armed_empty: None,
            initial: None,
        }
    }

    /// Ticks changed and not saved.
    pub fn is_edited(&self) -> bool {
        self.initial.as_ref().is_some_and(|(keep, untagged)| *keep != self.keep || *untagged != self.keep_untagged)
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

    /// The summary once read.
    pub fn summary(&self) -> Option<&CategorySummary> {
        self.summary.as_ref()
    }

    fn set_data(&mut self, d: PickerData) {
        if !self.started {
            self.started = true;
            match &d.rule {
                Some(r) => {
                    self.keep = r.keep.clone();
                    self.keep_untagged = r.keep_untagged;
                }
                None => {
                    self.keep = category_tags::default_keep(d.country.as_deref());
                    self.keep_untagged = true;
                }
            }
            self.stored = d.rule;
            self.initial = Some((self.keep.clone(), self.keep_untagged));
        }
        // the cursor stays on its entry when a re-read reorders the rows
        let at = self.cursor_entry().map(|t| t.tag.clone());
        self.name = d.name;
        self.country = d.country;
        self.summary = Some(d.summary);
        self.refilter();
        if let Some(tag) = at
            && let Some(i) = self.rows.iter().position(|&i| self.summary.as_ref().is_some_and(|s| s.tags[i].tag == tag))
        {
            self.list.select(i);
        }
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
        self.rows = self.list.filter.apply(hay.iter().map(String::as_str)).into_iter().map(|i| i as usize).collect();
        self.list.set_len(self.rows.len());
    }

    fn cursor_entry(&self) -> Option<&TagSummary> {
        let i = *self.rows.get(self.list.cursor)?;
        self.summary.as_ref()?.tags.get(i)
    }

    fn ticked(&self, tag: Option<&CategoryTag>) -> bool {
        match tag {
            Some(t) => self.keep.contains(t),
            None => self.keep_untagged,
        }
    }

    fn set_tick(&mut self, tag: Option<&CategoryTag>, on: bool) {
        match tag {
            Some(t) if on => {
                self.keep.insert(t.clone());
            }
            Some(t) => {
                self.keep.remove(t);
            }
            None => self.keep_untagged = on,
        }
    }

    fn toggle_row(&mut self, row: usize) {
        let Some(tag) = self.rows.get(row).and_then(|&i| self.summary.as_ref()?.tags.get(i)).map(|t| t.tag.clone()) else { return };
        let on = !self.ticked(tag.as_ref());
        self.set_tick(tag.as_ref(), on);
    }

    /// Tick every row the filter shows, or none when they all are.
    fn set_shown(&mut self, on: Option<bool>) {
        let Some(s) = &self.summary else { return };
        let tags: Vec<Option<CategoryTag>> = self.rows.iter().map(|&i| s.tags[i].tag.clone()).collect();
        let on = on.unwrap_or_else(|| !tags.iter().all(|t| self.ticked(t.as_ref())));
        for t in &tags {
            self.set_tick(t.as_ref(), on);
        }
    }

    fn defaults(&mut self) {
        self.keep = category_tags::default_keep(self.country.as_deref());
        self.keep_untagged = true;
    }

    /// The rule Enter stores: `None` when every entry is ticked (no filtering).
    fn to_save(&self) -> Option<CategoryFilter> {
        let s = self.summary.as_ref()?;
        let present = s.present();
        let untagged_present = s.tags.iter().any(|t| t.tag.is_none());
        let everything = present.iter().all(|t| self.keep.contains(t)) && (self.keep_untagged || !untagged_present);
        (!everything).then(|| CategoryFilter { keep: self.keep.clone(), keep_untagged: self.keep_untagged, known: present })
    }

    /// The ticks show nothing of the playlist (no present tag ticked, untagged off or absent).
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
            "{} of {} ticked · {} of {} {}",
            kept.len(),
            s.tags.len(),
            grouped(sum(&mut kept.iter().copied())),
            grouped(sum(&mut s.tags.iter())),
            if vod { "channels and titles" } else { "channels" }
        ))
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) {
        if self.saving.is_some() || self.summary.is_none() {
            return;
        }
        // nothing of the playlist would show: say so, and save on a second Enter
        if self.shows_nothing() {
            let ticks = (self.keep.clone(), self.keep_untagged);
            if self.armed_empty.as_ref() != Some(&ticks) {
                self.armed_empty = Some(ticks);
                return;
            }
        }
        self.armed_empty = None;
        let filter = self.to_save();
        let (id, f) = (self.source, filter.clone());
        let ticket =
            ctx.services.store.writer().submit_notify(DataScope::Sources, move |c| crate::store::sources::set_category_filter(c, id, f.as_ref()));
        self.saving = Some((ticket, filter));
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
        if let Some((ticket, filter)) = &mut self.saving {
            match ticket.try_take() {
                Some(Ok(())) => {
                    let text = match (filter, &self.summary) {
                        (Some(f), Some(s)) => {
                            let present = s.present();
                            format!(
                                "Showing {} of {} countries and languages · Settings ▸ Hidden items shows the rest",
                                present.iter().filter(|t| f.keep.contains(*t)).count(),
                                present.len()
                            )
                        }
                        _ => "Showing every country and language".to_owned(),
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
                if self.rows.is_empty() {
                    let q = self.list.filter.query();
                    small(
                        ui,
                        &if q.is_empty() { "This playlist has no categories yet".to_owned() } else { format!("No matches for “{q}”") },
                        t.text_secondary,
                    );
                } else {
                    let row_h = row_height(theme, crate::config::Density::Comfortable, true);
                    let max_h = (egui_ctx.content_rect().height() * 0.45).max(row_h * 3.0);
                    let h = (self.rows.len() as f32 * row_h).min(max_h);
                    let (rows, keep, untagged) = (&self.rows, &self.keep, self.keep_untagged);
                    let known = self.stored.as_ref().map(|r| &r.known).filter(|k| !k.is_empty());
                    let list = &mut self.list;
                    ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
                        virtual_list(ui, theme, ("category-picker", self.source), list, row_h, |ui, rc| {
                            let e = &s.tags[rows[rc.index]];
                            let ticked = e.tag.as_ref().map_or(untagged, |tag| keep.contains(tag));
                            let code = e.tag.as_ref().map(category_tags::tag_short);
                            let new = e.tag.as_ref().is_some_and(|tag| known.is_some_and(|k| !k.contains(tag)));
                            let label = tag_name(e.tag.as_ref());
                            let resp = check_row(ui, theme, ticked, code.as_deref(), &label, Some(&counts_text(e)), new, rc.state);
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
        if let Some(text) = self.effect_text() {
            small(ui, &text, t.text_secondary);
        }
        if self.shows_nothing() {
            let armed = self.armed_empty.as_ref() == Some(&(self.keep.clone(), self.keep_untagged));
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
                Some(s) => {
                    format!("{}: categories in {} countries and languages. Untick what you don't want to see.", picker.name, s.tag_count())
                }
                None => picker.name.clone(),
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
        PickerData { summary, rule, name: "My IPTV".into(), country: Some("GB".into()) }
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
        let saved = CategoryFilter { keep: [tag("FR")].into(), keep_untagged: false, known: [tag("FR"), tag("GB")].into() };
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

    /// The dialog (Settings ▸ Playlists ▸ Categories…): it reads the playlist's categories off
    /// the UI thread, Space ticks the row the filter shows, Enter stores the rule through
    /// `set_category_filter` (the library rebuild applies it) and closes.
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
        let rule = crate::store::sources::get_source(&h.state().t.services().store.reader().unwrap(), src).unwrap().settings.category_filter;
        let rule = rule.expect("a rule");
        assert!(!rule.keep.contains(&tag("region:exyu")) && !rule.keep.contains(&tag("GB")) && rule.keep_untagged);
        assert_eq!(rule.known, [tag("GB"), tag("region:exyu")].into());
        assert!(h.state().t.toasts.iter().any(|x| x.text.starts_with("Showing 0 of 2 countries and languages")), "{:?}", h.state().t.toasts);
    }
}
