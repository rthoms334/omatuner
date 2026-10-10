//! EPG mapping (DESIGN §9.4): channel list filtered to Unmapped / Fuzzy suggestions / All;
//! right pane shows suggestions (`epg_match::suggest`, computed lazily for VISIBLE rows on a
//! worker with a token prefilter — never for all unmapped channels at once) + searchable list
//! of EPG channels; Enter assigns (Manual), Delete clears. Key contexts
//! `[EpgMapping, List|ListFiltering]`. Owner: WP-12.
//!
//! * The EPG catalogue (channels of every enabled guide + one candidate index per guide) is
//!   read and indexed on the blocking pool when the screen opens and after `DataScope::Epg`.
//! * Suggestions are computed in batches for the channel rows drawn in the last frame that
//!   have none cached yet (one batch in flight at a time); "Has suggestions" lists the
//!   channels whose cached suggestions are non-empty (the prefilter's results so far).
//! * Ctrl+S picks the channel filter; Tab switches between the channel list and the EPG
//!   list (which has its own type-to-filter); Enter on a channel moves to its candidates.
//! * Writes: `store::epg::set_manual_mapping` / `clear_manual_mapping` through the writer,
//!   then `DataScope::Epg` (the app flushes guide caches and rebuilds the library).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, EpgChannelId, EpgMapping, EpgMatchMethod, EpgSourceId, EpgSuggestion, ItemKind, ItemRef, Timestamp};
use crate::msg::{AppEvent, DataScope};
use crate::sources::epg_match::{EpgCandidateIndex, build_index, suggest};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::text_field::search_header;
use crate::ui::widgets::{RowState, empty_state, glyphs, rows, section};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, UiCtx};

/// Suggestions kept per channel, and channels per suggestion batch.
const SUGGESTIONS: usize = 5;
const BATCH: usize = 40;
/// The mode menu's "Guide settings…" (after the `Mode::ALL` digits).
const MENU_GUIDE_SETTINGS: u8 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Unmapped,
    Suggested,
    All,
}

impl Mode {
    const ALL: [(Mode, &'static str); 3] = [(Mode::Unmapped, "Unmapped"), (Mode::Suggested, "Has suggestions"), (Mode::All, "All channels")];
    fn label(self) -> &'static str { Self::ALL.iter().find(|(m, _)| *m == self).map(|(_, l)| *l).unwrap_or("All") }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    #[default]
    Channels,
    Candidates,
}

#[derive(Debug, Clone, PartialEq)]
struct EpgEntry {
    id: EpgChannelId,
    source: EpgSourceId,
    xmltv_id: String,
    name: String,
    source_name: String,
}

/// Every EPG channel of the enabled guides, plus the fuzzy candidate indexes.
#[derive(Debug, Default)]
struct Catalog {
    entries: Vec<EpgEntry>,
    /// Filter haystacks of `entries` ("name xmltv-id guide").
    hay: Vec<String>,
    by_id: HashMap<EpgChannelId, usize>,
    indexes: Vec<EpgCandidateIndex>,
}

fn load_catalog(c: &rusqlite::Connection) -> crate::error::Result<Arc<Catalog>> {
    let mut cat = Catalog::default();
    for src in crate::store::sources::list_epg_sources(c)?.into_iter().filter(|s| s.enabled) {
        let chans = crate::store::epg::epg_channels(c, src.id)?;
        for ch in &chans {
            let name = ch.display_names.first().cloned().unwrap_or_else(|| ch.xmltv_id.clone());
            cat.by_id.insert(ch.id, cat.entries.len());
            cat.entries.push(EpgEntry { id: ch.id, source: src.id, xmltv_id: ch.xmltv_id.clone(), name, source_name: src.name.clone() });
        }
        cat.indexes.push(build_index(src.id, &chans));
    }
    cat.hay = cat.entries.iter().map(|e| format!("{} {} {}", e.name, e.xmltv_id, e.source_name)).collect();
    Ok(Arc::new(cat))
}

/// Fuzzy suggestions for (channel, name) pairs across every guide (blocking pool).
fn compute_suggestions(cat: &Catalog, names: Vec<(ChannelId, String)>, threshold: f64) -> HashMap<ChannelId, Vec<EpgSuggestion>> {
    names
        .into_iter()
        .map(|(id, name)| {
            let mut all: Vec<EpgSuggestion> = cat.indexes.iter().flat_map(|idx| suggest(idx, &name, SUGGESTIONS, threshold)).collect();
            all.sort_by(|a, b| b.score.total_cmp(&a.score));
            all.truncate(SUGGESTIONS);
            (id, all)
        })
        .collect()
}

/// A row of the candidates pane.
#[derive(Debug, Clone, PartialEq)]
enum Cand {
    Suggestion(EpgSuggestion),
    Entry(usize),
}

#[derive(Default)]
pub struct EpgMappingScreen {
    mode: Mode,
    pane: Pane,
    channels: ListState,
    chan_rows: Vec<ChannelId>,
    cands: ListState,
    cand_rows: Vec<Cand>,
    catalog: Option<Arc<Catalog>>,
    catalog_loader: Option<Loader<Arc<Catalog>>>,
    suggestions: HashMap<ChannelId, Vec<EpgSuggestion>>,
    suggest_rx: Option<tokio::sync::oneshot::Receiver<HashMap<ChannelId, Vec<EpgSuggestion>>>>,
    /// Channel rows drawn last frame (suggestions are computed for these).
    visible: Vec<ChannelId>,
    /// Mappings of the highlighted channel.
    mappings: Option<(ChannelId, Vec<EpgMapping>)>,
    mappings_loader: Option<(ChannelId, Loader<Vec<EpgMapping>>)>,
    /// What `chan_rows` was computed from.
    rows_key: Option<(Mode, Timestamp, usize)>,
    preselect: Option<ChannelId>,
}

impl EpgMappingScreen {
    fn highlighted(&self) -> Option<ChannelId> { self.chan_rows.get(self.channels.cursor).copied() }

    /// What the channel rows depend on (new suggestions matter only for "Has suggestions").
    fn rows_key(&self, ctx: &UiCtx<'_>) -> (Mode, Timestamp, usize) {
        (self.mode, ctx.lib.built_at, if self.mode == Mode::Suggested { self.suggestions.len() } else { 0 })
    }

    fn channel_ref(ctx: &UiCtx<'_>, id: ChannelId) -> Option<ItemRef> {
        ctx.lib.live.get(id).map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() })
    }

    fn reload_catalog(&mut self, ctx: &UiCtx<'_>) {
        self.catalog_loader = Some(Loader::start(ctx, load_catalog));
        self.suggestions.clear();
    }

    /// Recompute the channel rows (mode, filter, library).
    fn refilter_channels(&mut self, ctx: &UiCtx<'_>) {
        let keep = self.highlighted();
        let ids: Vec<ChannelId> = ctx
            .lib
            .live
            .channels
            .iter()
            .filter(|c| !ctx.lib.live.hidden.contains(&c.id))
            .filter(|c| match self.mode {
                Mode::All => true,
                Mode::Unmapped => c.epg_channel_id.is_none(),
                Mode::Suggested => c.epg_channel_id.is_none() && self.suggestions.get(&c.id).is_some_and(|s| !s.is_empty()),
            })
            .map(|c| c.id)
            .collect();
        let hay: Vec<String> = ids
            .iter()
            .map(|id| ctx.lib.live.get(*id).map(|c| format!("{} {}", c.number.map(|n| n.to_string()).unwrap_or_default(), c.name)).unwrap_or_default())
            .collect();
        self.chan_rows = self.channels.filter.apply(hay.iter().map(String::as_str)).into_iter().map(|i| ids[i as usize]).collect();
        self.channels.set_len(self.chan_rows.len());
        let want = self.preselect.take().or(keep);
        if let Some(i) = want.and_then(|w| self.chan_rows.iter().position(|c| *c == w)) {
            self.channels.select(i);
        }
        self.rows_key = Some(self.rows_key(ctx));
    }

    /// Candidates: the highlighted channel's suggestions, then every EPG channel (filtered).
    fn refilter_cands(&mut self) {
        let Some(cat) = &self.catalog else {
            self.cand_rows.clear();
            self.cands.set_len(0);
            return;
        };
        let mut rows = Vec::new();
        if !self.cands.filter.is_active()
            && let Some(s) = self.highlighted().and_then(|h| self.suggestions.get(&h))
        {
            rows.extend(s.iter().cloned().map(Cand::Suggestion));
        }
        rows.extend(self.cands.filter.apply(cat.hay.iter().map(String::as_str)).into_iter().map(|i| Cand::Entry(i as usize)));
        self.cand_rows = rows;
        self.cands.set_len(self.cand_rows.len());
    }

    fn assign(&mut self, ctx: &mut UiCtx<'_>) {
        let (Some(ch), Some(cand)) = (self.highlighted(), self.cand_rows.get(self.cands.cursor).cloned()) else { return };
        let Some(item) = Self::channel_ref(ctx, ch) else { return };
        let (source, xmltv_id, name) = match cand {
            Cand::Suggestion(s) => (s.epg_source_id, s.xmltv_id, s.display_name),
            Cand::Entry(i) => match self.catalog.as_ref().and_then(|c| c.entries.get(i)) {
                Some(e) => (e.source, e.xmltv_id.clone(), e.name.clone()),
                None => return,
            },
        };
        let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| crate::store::epg::set_manual_mapping(c, &item, source, &xmltv_id));
        let channel = ctx.lib.live.get(ch).map(|c| c.name.to_string()).unwrap_or_default();
        ctx.toast(crate::msg::Toast::info(format!("{channel} now uses the guide of {name}")));
        self.mappings = None;
        self.pane = Pane::Channels;
        // the next unmapped channel is what the user works on next
        if self.mode != Mode::All && self.channels.cursor + 1 < self.chan_rows.len() {
            self.channels.select(self.channels.cursor + 1);
        }
    }

    fn clear(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(ch) = self.highlighted() else { return };
        let Some(item) = Self::channel_ref(ctx, ch) else { return };
        let manual = self.mappings.as_ref().is_some_and(|(c, m)| *c == ch && m.iter().any(|m| m.method == EpgMatchMethod::Manual));
        if !manual {
            ctx.toast(crate::msg::Toast::info("This channel has no manual assignment"));
            return;
        }
        let _ = ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| crate::store::epg::clear_manual_mapping(c, &item));
        ctx.toast(crate::msg::Toast::info("Manual assignment cleared — automatic matching applies again"));
        self.mappings = None;
    }

    fn mode_menu(&self, ctx: &mut UiCtx<'_>) {
        let mut items: Vec<MenuItem> = Mode::ALL
            .iter()
            .enumerate()
            .map(|(i, (m, l))| MenuItem { icon: if *m == self.mode { glyphs::CHECK } else { "" }, label: (*l).into(), action: Action::Digit(i as u8), enabled: true, shortcut: None })
            .collect();
        // the way to a guide that lists more channels, by keyboard
        items.push(MenuItem { icon: glyphs::SETTINGS, label: "Guide settings…".into(), action: Action::Digit(MENU_GUIDE_SETTINGS), enabled: true, shortcut: None });
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Show channels".into(), items }));
    }

    /// "Your guides list 2 channels. Add one…" when the guides list under half as many
    /// channels as the playlists (mapping can't cover the rest).
    fn few_note(&self, ctx: &UiCtx<'_>) -> Option<String> {
        let n = self.catalog.as_ref()?.entries.len();
        (n * 2 < ctx.lib.live.channels.len())
            .then(|| format!("Your guides list {} channel{}. Add one that lists yours in Settings ▸ EPG.", rows::grouped(n), if n == 1 { "" } else { "s" }))
    }

    /// Start a suggestion batch for visible channels without cached suggestions.
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
            .filter_map(|id| ctx.lib.live.get(*id).filter(|c| c.epg_channel_id.is_none() || self.mode == Mode::All).map(|c| (*id, c.name.to_string())))
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

    fn pump_mappings(&mut self, ctx: &UiCtx<'_>) {
        if let Some((ch, l)) = &mut self.mappings_loader {
            match l.poll() {
                Some(Ok(m)) => {
                    self.mappings = Some((*ch, m));
                    self.mappings_loader = None;
                }
                Some(Err(e)) => {
                    tracing::debug!("mappings: {e}");
                    self.mappings_loader = None;
                }
                None => return,
            }
        }
        let Some(h) = self.highlighted() else { return };
        if self.mappings.as_ref().is_some_and(|(c, _)| *c == h) {
            return;
        }
        if let Some(item) = Self::channel_ref(ctx, h) {
            self.mappings_loader = Some((h, Loader::start(ctx, move |c| crate::store::epg::mappings_for_channel(c, &item))));
        }
    }
}

fn method_label(m: EpgMatchMethod) -> &'static str {
    match m {
        EpgMatchMethod::Manual => "assigned by you",
        EpgMatchMethod::TvgIdExact | EpgMatchMethod::TvgIdCaseless => "tvg-id",
        EpgMatchMethod::XtreamEpgId => "provider guide id",
        EpgMatchMethod::FeedStripped => "id without feed",
        EpgMatchMethod::NormalizedName => "name",
        EpgMatchMethod::Fuzzy => "similar name",
    }
}

impl Screen for EpgMappingScreen {
    fn id(&self) -> ScreenId { ScreenId::EpgMapping }

    fn key_contexts(&self) -> Vec<KeyContext> {
        let list = match self.pane {
            Pane::Channels => self.channels.key_context(),
            Pane::Candidates => self.cands.key_context(),
        };
        vec![KeyContext::EpgMapping, list]
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        if let Route::EpgMapping { channel: Some(ch) } = route {
            self.mode = Mode::All;
            self.preselect = Some(*ch);
            self.pane = Pane::Channels;
            self.channels.filter.clear();
        }
        self.rows_key = None;
        self.mappings = None;
        self.reload_catalog(ctx);
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        if let AppEvent::Data(DataScope::Epg | DataScope::All) = ev {
            self.mappings = None;
            if self.catalog.is_some() {
                self.reload_catalog(ctx);
            }
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        // "No guide data": Enter is the "Open EPG settings" button
        if action == Action::Activate && self.catalog.as_ref().is_some_and(|c| c.entries.is_empty()) && !ctx.lib.live.channels.is_empty() {
            ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
            return true;
        }
        match action {
            Action::SortMenu => {
                self.mode_menu(ctx);
                return true;
            }
            Action::ClearEpg | Action::Remove => {
                self.clear(ctx);
                return true;
            }
            Action::FocusNext | Action::FocusPrev => {
                self.pane = if self.pane == Pane::Channels { Pane::Candidates } else { Pane::Channels };
                return true;
            }
            _ => {}
        }
        // Esc in the candidates pane returns to the channel list (then leaves the screen)
        if action == Action::Back && self.pane == Pane::Candidates {
            self.pane = Pane::Channels;
            return true;
        }
        match self.pane {
            Pane::Channels => match self.channels.handle(action) {
                ListOutcome::Moved => {
                    self.refilter_cands();
                    true
                }
                ListOutcome::FilterChanged => {
                    self.refilter_channels(ctx);
                    self.refilter_cands();
                    true
                }
                ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                    self.pane = Pane::Candidates;
                    self.cands.select(0);
                    true
                }
                ListOutcome::ContextMenu(_) => {
                    self.mode_menu(ctx);
                    true
                }
                ListOutcome::Parent => false,
                ListOutcome::None => matches!(action, Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) | Action::Activate),
            },
            // no candidate to pick: Enter is the empty state's "Open EPG settings"
            Pane::Candidates if action == Action::Activate && self.catalog.is_some() && self.cand_rows.is_empty() => {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
                true
            }
            Pane::Candidates => match self.cands.handle(action) {
                ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                    self.assign(ctx);
                    true
                }
                ListOutcome::FilterChanged => {
                    self.refilter_cands();
                    true
                }
                ListOutcome::Parent => {
                    self.pane = Pane::Channels;
                    true
                }
                ListOutcome::Moved | ListOutcome::ContextMenu(_) => true,
                ListOutcome::None => matches!(action, Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::FilterInput(_) | Action::Activate),
            },
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        if result == OverlayResult::Chosen(Action::Digit(MENU_GUIDE_SETTINGS)) {
            ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
        } else if let OverlayResult::Chosen(Action::Digit(i)) = result
            && let Some((m, _)) = Mode::ALL.get(i as usize)
        {
            self.mode = *m;
            self.channels.select(0);
            self.refilter_channels(ctx);
            self.refilter_cands();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        match kit::poll(&mut self.catalog_loader) {
            Some(Ok(c)) => {
                self.catalog = Some(c);
                self.refilter_cands();
            }
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read the guides", &e),
            None => {}
        }
        if self.rows_key != Some(self.rows_key(ctx)) {
            self.refilter_channels(ctx);
        }
        self.pump_suggestions(ctx);
        self.pump_mappings(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if ctx.lib.built_at != 0 && ctx.lib.live.channels.is_empty() {
            empty_state::empty_state(ui, theme, glyphs::GUIDE, "No channels to map", Some("Add a playlist first."), None);
            return;
        }
        if self.catalog.as_ref().is_some_and(|c| c.entries.is_empty()) {
            if empty_state::empty_state(ui, theme, glyphs::GUIDE, "No guide data", Some("Add an XMLTV guide in Settings ▸ EPG, then map channels here."), Some("Open EPG settings"))
                .is_some_and(|r| r.clicked())
            {
                ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
            }
            return;
        }
        let compact = ctx.layout == LayoutClass::Compact;
        let full = ui.available_rect_before_wrap();
        let gap = theme.space(SpaceToken::PanelPadding);
        let left_w = if compact { full.width() } else { (full.width() * 0.48).round() };
        let left = egui::Rect::from_min_size(full.min, egui::vec2(left_w, full.height()));
        let right = if compact { full } else { egui::Rect::from_min_max(egui::pos2(left.right() + gap, full.top()), full.max) };
        let row_h = rows::row_height(theme, ctx.config.interface.density, true);
        if !compact || self.pane == Pane::Channels {
            let mut ui_l = ui.new_child(egui::UiBuilder::new().id_salt("map-chans").max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
            let header = format!("{} channels — Ctrl+S changes the view", self.mode.label());
            search_header(&mut ui_l, theme, self.channels.filter.query(), &header, Some(glyphs::SEARCH));
            ui_l.add_space(theme.space(SpaceToken::Md));
            if self.chan_rows.is_empty() {
                let msg = match self.mode {
                    Mode::Unmapped if !self.channels.filter.is_active() => "Every channel has guide data",
                    Mode::Suggested if !self.channels.filter.is_active() => "No suggestions yet — they appear as you scroll the Unmapped list",
                    _ => "No matches",
                };
                empty_state::empty_state(&mut ui_l, theme, glyphs::CHECK, msg, None, None);
            } else {
                let focused = self.pane == Pane::Channels;
                let (chan_rows, suggestions, catalog) = (&self.chan_rows, &self.suggestions, &self.catalog);
                let mut visible = Vec::new();
                let out = virtual_list(&mut ui_l, theme, "map-channels", &mut self.channels, row_h, |ui, rc| {
                    let id = chan_rows[rc.index];
                    visible.push(id);
                    let c = ctx.lib.live.get(id);
                    let label = c.map(|c| match c.number {
                        Some(n) => format!("{n}  {}", c.name),
                        None => c.name.to_string(),
                    }).unwrap_or_default();
                    let (icon, detail) = match c.and_then(|c| c.epg_channel_id) {
                        Some(e) => {
                            let name = catalog.as_ref().and_then(|cat| cat.by_id.get(&e).map(|i| cat.entries[*i].name.clone())).unwrap_or_else(|| "a guide channel".into());
                            (glyphs::CHECK, format!("Guide: {name}"))
                        }
                        None => match suggestions.get(&id) {
                            Some(s) if !s.is_empty() => (glyphs::WARNING, format!("No guide data · {} suggestion{}", s.len(), if s.len() == 1 { "" } else { "s" })),
                            _ => (glyphs::WARNING, "No guide data".to_owned()),
                        },
                    };
                    let state = if focused { rc.state } else { RowState::from_flags(false, rc.state.is_cursor()) };
                    rows::menu_row(ui, theme, Some(icon), &label, Some(&detail), false, state)
                });
                self.visible = visible;
                match out {
                    ListOutcome::Moved => {
                        self.pane = Pane::Channels;
                        self.refilter_cands();
                    }
                    ListOutcome::Activated(_) => self.pane = Pane::Candidates,
                    _ => {}
                }
            }
            if !compact {
                section::pane_divider(ui.painter(), left.right() + gap / 2.0, full.y_range(), theme);
            }
        }
        if !compact || self.pane == Pane::Candidates {
            let mut ui_r = ui.new_child(egui::UiBuilder::new().id_salt("map-cands").max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
            let current = self.highlighted().and_then(|h| ctx.lib.live.get(h)).map(|c| c.name.to_string()).unwrap_or_default();
            ui_r.label(RichText::new(format!("Guide for {current}")).size(theme.font(TypeToken::Title)).color(t.fg));
            match &self.mappings {
                Some((ch, m)) if Some(*ch) == self.highlighted() => match m.first() {
                    Some(best) => {
                        let name = self
                            .catalog
                            .as_ref()
                            .and_then(|cat| cat.entries.iter().find(|e| e.source == best.epg_source_id && e.xmltv_id == best.xmltv_id))
                            .map(|e| format!("{} ({})", e.name, e.source_name))
                            .unwrap_or_else(|| best.xmltv_id.clone());
                        let extra = if best.method == EpgMatchMethod::Manual { " · Delete clears it" } else { "" };
                        ui_r.label(RichText::new(format!("Matched {name} by {}{extra}", method_label(best.method))).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                    }
                    None => {
                        ui_r.label(RichText::new("Not matched — pick a guide channel below (Enter assigns)").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                    }
                },
                _ => {
                    ui_r.label(RichText::new(" ").size(theme.font(TypeToken::BodySmall)));
                }
            }
            let few = self.few_note(ctx);
            if let Some(note) = &few {
                ui_r.label(RichText::new(note).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
            }
            ui_r.add_space(theme.space(SpaceToken::Md));
            search_header(&mut ui_r, theme, self.cands.filter.query(), "Search guide channels", Some(glyphs::GUIDE));
            ui_r.add_space(theme.space(SpaceToken::Md));
            if self.catalog.is_none() {
                for _ in 0..5 {
                    let (r, _) = ui_r.allocate_exact_size(egui::vec2(ui_r.available_width(), row_h - 6.0), egui::Sense::hover());
                    ui_r.painter().rect_filled(r, t.rounding, t.fill_field);
                    ui_r.add_space(6.0);
                }
            } else if self.cand_rows.is_empty() {
                let detail = few.as_deref().unwrap_or("Search for another name, or add a guide that lists this channel in Settings ▸ EPG.");
                if empty_state::empty_state(&mut ui_r, theme, glyphs::SEARCH, "No matching guide channels", Some(detail), Some("Open EPG settings")).is_some_and(|r| r.clicked()) {
                    ctx.push(Route::Settings(crate::ui::SettingsPage::Epg));
                }
            } else {
                let focused = self.pane == Pane::Candidates;
                let (cand_rows, catalog) = (&self.cand_rows, &self.catalog);
                let out = virtual_list(&mut ui_r, theme, "map-candidates", &mut self.cands, row_h, |ui, rc| {
                    let state = if focused { rc.state } else { RowState::Idle };
                    match &cand_rows[rc.index] {
                        Cand::Suggestion(s) => {
                            let src = catalog.as_ref().and_then(|c| c.entries.iter().find(|e| e.id == s.epg_channel_id)).map(|e| e.source_name.clone()).unwrap_or_default();
                            let detail = format!("Suggested · {:.0} % · {} · {src}", s.score * 100.0, s.xmltv_id);
                            rows::menu_row(ui, theme, Some(glyphs::TAG), &s.display_name, Some(&detail), false, state)
                        }
                        Cand::Entry(i) => match catalog.as_ref().and_then(|c| c.entries.get(*i)) {
                            Some(e) => rows::menu_row(ui, theme, Some(glyphs::GUIDE), &e.name, Some(&format!("{} · {}", e.xmltv_id, e.source_name)), false, state),
                            None => ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover()).1,
                        },
                    }
                });
                match out {
                    ListOutcome::Moved => self.pane = Pane::Candidates,
                    ListOutcome::Activated(_) => {
                        self.pane = Pane::Candidates;
                        self.assign(ctx);
                    }
                    _ => {}
                }
            }
        }
        ui.allocate_rect(full, egui::Sense::hover());
        if self.suggest_rx.is_some() || self.catalog_loader.is_some() || self.mappings_loader.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpgChannel, ItemKey, SourceId};
    use crate::ui::NavRequest;
    use crate::ui::screens::home::testkit::{self, harness, has_label, rig, seed, until};

    fn manual(t: &crate::ui::testing::TestUi, i: usize) -> Vec<EpgMapping> {
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
        crate::store::epg::mappings_for_channel(&t.services().store.reader().unwrap(), &item).unwrap().into_iter().filter(|m| m.method == EpgMatchMethod::Manual).collect()
    }

    #[test]
    fn unmapped_channels_get_suggestions_enter_assigns_delete_clears() {
        let (mut t, one) = rig();
        let chans = seed(&mut t, |c| {
            let s = testkit::source(c, "My IPTV")?;
            let ch = testkit::channels(c, s, 5)?;
            testkit::epg(c, &ch[..3], 0, 1, true)?;
            Ok(ch)
        });
        let mut h = harness(t, one, EpgMappingScreen::default(), egui::vec2(1400.0, 900.0));
        h.state_mut().enter(Route::EpgMapping { channel: None });
        until(&mut h, "the guide catalogue", |h| h.state().s.catalog.is_some());
        h.run_steps(2);
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::EpgMapping, KeyContext::List]);
        // Unmapped: channels 3 and 4 only
        assert_eq!(h.state().s.chan_rows, vec![chans[3], chans[4]]);
        until(&mut h, "fuzzy suggestions for the visible rows", |h| h.state().s.suggestions.get(&chans[3]).is_some_and(|s| !s.is_empty()));
        h.run_steps(1);
        assert!(has_label(&h, "Channel 0"), "the EPG channel list");
        // Enter on the channel → the candidates pane; Enter there assigns the best suggestion
        h.state_mut().act(Action::Activate);
        assert_eq!(h.state().s.pane, Pane::Candidates);
        // Esc there returns to the channel list (it doesn't leave the screen)
        assert!(h.state_mut().act(Action::Back));
        assert_eq!(h.state().s.pane, Pane::Channels);
        h.state_mut().act(Action::Activate);
        assert!(matches!(h.state().s.cand_rows.first(), Some(Cand::Suggestion(_))));
        h.state_mut().act(Action::Activate);
        until(&mut h, "the manual mapping", |h| manual(&h.state().t, 3).len() == 1);
        // Delete clears it (once its mappings are known)
        h.state_mut().act(Action::FocusNext);
        h.state_mut().s.channels.select(0);
        until(&mut h, "the mappings of channel 3", |h| h.state().s.mappings.as_ref().is_some_and(|(c, m)| *c == chans[3] && !m.is_empty()));
        h.state_mut().act(Action::ClearEpg);
        until(&mut h, "cleared", |h| manual(&h.state().t, 3).is_empty());
        // Ctrl+S: the channel filter; All lists every channel
        h.state_mut().act(Action::SortMenu);
        assert!(matches!(h.state_mut().nav().as_slice(), [NavRequest::Open(OverlayRequest::ContextMenu(m))] if m.title == "Show channels"));
        h.state_mut().result(OverlayResult::Chosen(Action::Digit(2)));
        assert_eq!(h.state().s.chan_rows.len(), 5);
        // Ctrl+E from a list opens the screen on that channel
        h.state_mut().enter(Route::EpgMapping { channel: Some(chans[4]) });
        h.run_steps(2);
        assert_eq!(h.state().s.highlighted(), Some(chans[4]));
    }

    #[test]
    fn suggestions_merge_guides_best_first() {
        let chans = vec![
            EpgChannel { id: EpgChannelId(1), epg_source_id: EpgSourceId(1), xmltv_id: "bbc1.uk".into(), display_names: vec!["BBC One".into()], icon: None },
            EpgChannel { id: EpgChannelId(2), epg_source_id: EpgSourceId(1), xmltv_id: "bbc2.uk".into(), display_names: vec!["BBC Two".into()], icon: None },
        ];
        let cat = Catalog { entries: vec![], hay: vec![], by_id: HashMap::new(), indexes: vec![build_index(EpgSourceId(1), &chans)] };
        let r = compute_suggestions(&cat, vec![(ChannelId(7), "BBC One HD".into())], 0.8);
        let s = &r[&ChannelId(7)];
        assert!(!s.is_empty());
        assert_eq!(s[0].xmltv_id, "bbc1.uk");
        assert!(s.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(Mode::Suggested.label(), "Has suggestions");
        assert_eq!(method_label(EpgMatchMethod::Manual), "assigned by you");
    }
}
