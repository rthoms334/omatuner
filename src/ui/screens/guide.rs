//! TV Guide (DESIGN §13.4): header (clickable collection picker "Favourites ▾", ‹ day ›, Now) +
//! `epg_grid` + optional preview strip (Wide, config `guide_preview`). Default collection: the
//! first non-empty favourites list, else the last-used group. Enter → programme popover; `r`
//! toggles a reminder; digits jump to a channel number (`[NumericEntry, Guide]`); click on the
//! channel column = watch; returning from the player focuses the channel just watched
//! (UHF 1.96). Keeps the focused cell clear of `ctx.mini_rect`. Owner: WP-11.
//!
//! "Last-used group": the collection picked in the guide this session; without one, the group
//! of the channel playing (or All channels).
//!
//! Returning from the player: when the guide started the playback itself (Watch, catch-up, or
//! the programme popover's Watch) and the same guide route is entered again after a new load,
//! the focus follows the channel playing now (zaps included) and the collection the user
//! picked stays; any other entry applies the route's collection / focus.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Rect, RichText};

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, Collection, MediaRef, PlayOrigin, Reminder, SortMode, Timestamp};
use crate::msg::{AppEvent, DataScope, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerSnapshot, PlayerState};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::epg_grid::{self, EpgGridState, GridMetrics, GridOutcome, GuideRow, day_label};
use crate::ui::overlays::{ContextMenuSpec, MenuItem, OverlayResult};
use crate::ui::screens::live::{self, GuideStatus, NoGuide, choice_action, choice_index, playing_channel};
use crate::ui::video_host::Placement;
use crate::ui::widgets::button::{ButtonStyle, button, icon_button};
use crate::ui::widgets::rows::grouped;
use crate::ui::widgets::{empty_state::empty_state, glyphs, section};
use crate::ui::{LayoutClass, OverlayRequest, Route, Screen, ScreenId, SettingsPage, UiCtx};

/// Numeric entry auto-commits after this long without a digit.
const NUMERIC_COMMIT: Duration = Duration::from_millis(1500);

/// The guide data counts as stale after this long without an import (the status bar's rule).
const GUIDE_STALE_S: i64 = 2 * 86_400;

/// The channel of what plays (live or catch-up), whatever its state.
fn played_channel(snap: &PlayerSnapshot) -> Option<ChannelId> {
    match snap.playable.as_ref().map(|p| &p.media) {
        Some(MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. }) => Some(*id),
        _ => None,
    }
}

#[derive(Default)]
pub struct GuideScreen {
    grid: Option<EpgGridState>,
    collection: Option<Collection>,
    built: Option<(u64, u64, SortMode, Option<Collection>)>,
    dirty: bool,
    reminders: Vec<Reminder>,
    reminders_dirty: bool,
    numeric: Option<(String, Instant)>,
    /// Collections offered by the open picker (choice index → collection).
    choices: Vec<Collection>,
    /// The guide started (or offered, through the popover) a playback: the player's load
    /// generation at that moment. Coming back after a newer load re-focuses what plays.
    launched: Option<u64>,
    /// The route the guide was last entered with (a return re-enters the same one).
    entered: Option<Route>,
    /// Keyboard focus rest (optional guide preview autoplay).
    rest: Option<(ChannelId, Instant)>,
    guide: GuideStatus,
}

fn fav_signature(ctx: &UiCtx<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    for l in &ctx.favs.lists {
        (l.id, ctx.favs.items(l.id)).hash(&mut h);
    }
    h.finish()
}

impl GuideScreen {
    fn default_collection(&self, ctx: &UiCtx<'_>) -> Collection {
        if let Some(c) = self.collection.clone().filter(|c| self.valid(ctx, c)) {
            return c;
        }
        if let Some(l) = ctx.favs.non_empty_lists().next() {
            return Collection::Favourites(l.id);
        }
        // the group of what plays, else everything
        playing_channel(ctx.player)
            .and_then(|id| ctx.lib.live.get(id))
            .and_then(|c| c.group_ids.first().copied())
            .map_or(Collection::AllChannels, Collection::Group)
    }

    fn valid(&self, ctx: &UiCtx<'_>, c: &Collection) -> bool {
        match c {
            Collection::Favourites(l) => ctx.favs.lists.iter().any(|x| x.id == *l),
            Collection::Group(g) => ctx.lib.live.groups.iter().any(|x| x.id == *g && !x.hidden),
            _ => true,
        }
    }

    fn collection_name(ctx: &UiCtx<'_>, c: &Collection) -> String {
        match c {
            Collection::AllChannels => "All channels".into(),
            Collection::Group(g) => ctx.lib.live.groups.iter().find(|x| x.id == *g).map_or_else(|| "Group".into(), |x| x.display_name().to_owned()),
            Collection::Favourites(l) => ctx.favs.lists.iter().find(|x| x.id == *l).map_or_else(|| "Favourites".into(), |x| x.name.clone()),
            Collection::History => "History".into(),
            Collection::Adhoc(_) => "Channels".into(),
        }
    }

    /// Rebuild rows when the library, favourites, sort or collection changed.
    fn refresh(&mut self, ctx: &mut UiCtx<'_>) {
        let coll = self.default_collection(ctx);
        let sig = (ctx.lib.generation, fav_signature(ctx), ctx.config.interface.live_sort, Some(coll.clone()));
        if self.grid.is_none() {
            self.reminders_dirty = true;
        }
        if self.reminders_dirty
            && let Some(r) = live::load_reminders(ctx)
        {
            self.reminders = r;
            self.reminders_dirty = false;
            self.apply_reminders(ctx);
        }
        if self.built.as_ref() == Some(&sig) && !self.dirty && self.grid.is_some() {
            return;
        }
        let lib_changed = self.built.as_ref().is_none_or(|b| b.0 != sig.0) || self.dirty;
        self.built = Some(sig);
        self.dirty = false;
        let ids = ctx.lib.live.collection(&coll, ctx.favs, &ctx.lib.recent_channels, ctx.config.interface.live_sort);
        let rows: Arc<[GuideRow]> = ids
            .iter()
            .filter_map(|id| ctx.lib.live.get(*id))
            .map(|c| GuideRow { channel: c.id, epg: c.epg_channel_id, tvg_shift_min: c.tvg_shift_min, catchup_days: if c.has_catchup { c.catchup_days } else { 0 } })
            .collect();
        match &mut self.grid {
            Some(g) => {
                if lib_changed {
                    // mappings may have changed with the library (EPG scope rebuilds it)
                    g.cache.invalidate();
                }
                g.set_rows(rows);
            }
            None => {
                let mut g = EpgGridState::new(rows, ctx.now);
                g.clock_24h = ctx.config.interface.clock_24h;
                self.grid = Some(g);
            }
        }
        self.collection = Some(coll);
        self.apply_reminders(ctx);
    }

    fn apply_reminders(&mut self, ctx: &UiCtx<'_>) {
        let Some(g) = &mut self.grid else { return };
        let mut map: HashMap<ChannelId, HashSet<Timestamp>> = HashMap::new();
        for r in &self.reminders {
            if let Some(id) = ctx.lib.live.by_ref(&r.channel) {
                map.entry(id).or_default().insert(r.start);
            }
        }
        g.reminders = map;
    }

    fn zap(&self, ctx: &UiCtx<'_>) -> Option<crate::model::ZapList> { self.collection.as_ref().and_then(|c| live::zap_list(ctx, c)) }

    /// The collection is a provider group whose channels are all hidden ("Everything here is
    /// hidden" + Show hidden).
    fn all_hidden(&self, ctx: &UiCtx<'_>) -> bool {
        self.grid.as_ref().is_some_and(|g| g.rows.is_empty())
            && matches!(&self.collection, Some(Collection::Group(id)) if ctx.lib.live.groups.iter().any(|g| g.id == *id && g.item_count > 0))
    }

    fn handle_outcome(&mut self, ctx: &mut UiCtx<'_>, out: GridOutcome) {
        match out {
            GridOutcome::None => {}
            GridOutcome::FocusChanged => {
                self.rest = self.grid.as_ref().and_then(|g| g.focused()).map(|(c, _)| (c, Instant::now()));
            }
            GridOutcome::Open { programme, channel } => {
                let zap = self.zap(ctx);
                ctx.open(OverlayRequest::Programme { programme, channel, zap });
                // its Watch / Catch up play from the guide too
                self.launched = Some(ctx.player.load_generation);
            }
            GridOutcome::Watch(ch) => {
                let zap = self.zap(ctx);
                live::activate_channel(ctx, ch, zap, PlayOrigin::Guide, false, Action::GuideWatch);
                self.launched = Some(ctx.player.load_generation);
            }
            GridOutcome::Catchup { channel, programme } => {
                live::play_catchup(ctx, channel, &programme, Action::GuideCatchup);
                self.launched = Some(ctx.player.load_generation);
            }
            GridOutcome::Remind { channel, programme } => {
                let Some(ch) = ctx.lib.live.get(channel).cloned() else { return };
                let item = live::channel_ref(&ch);
                let existing = self.reminders.iter().find(|r| r.channel == item && r.start == programme.start).map(|r| r.id);
                live::toggle_reminder(ctx, &ch, &programme, existing);
                // optimistic bell; the Reminders scope reloads the real set
                if let Some(g) = &mut self.grid {
                    let set = g.reminders.entry(channel).or_default();
                    if existing.is_some() {
                        set.remove(&programme.start);
                    } else {
                        set.insert(programme.start);
                    }
                }
            }
        }
    }

    fn open_picker(&mut self, ctx: &mut UiCtx<'_>) {
        let mut choices = Vec::new();
        let mut items = Vec::new();
        let current = self.collection.clone();
        let mut add = |c: Collection, icon: &'static str, label: String| {
            let mark = if Some(&c) == current.as_ref() { glyphs::CHECK } else { icon };
            items.push(MenuItem { icon: mark, label, action: choice_action(choices.len()), enabled: true, shortcut: None });
            choices.push(c);
        };
        for l in &ctx.favs.lists {
            add(Collection::Favourites(l.id), glyphs::FAVOURITE, l.name.clone());
        }
        add(Collection::History, glyphs::HISTORY, "History".into());
        add(Collection::AllChannels, glyphs::LIVE_TV, "All channels".into());
        for g in ctx.lib.live.visible_groups(None, ctx.config.interface.hide_empty_groups) {
            add(Collection::Group(g.id), glyphs::FOLDER, g.display_name().to_owned());
        }
        self.choices = choices;
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title: "Show in the guide…".into(), items }));
    }

    fn set_collection(&mut self, ctx: &mut UiCtx<'_>, c: Collection) {
        self.collection = Some(c);
        self.refresh(ctx);
    }

    fn numeric_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        let (digits, _) = self.numeric.as_mut()?;
        match action {
            Action::Digit(d) => {
                if digits.len() < 5 {
                    digits.push(char::from(b'0' + d.min(9)));
                }
                self.numeric.as_mut()?.1 = Instant::now();
                Some(true)
            }
            Action::FilterBackspace => {
                digits.pop();
                if digits.is_empty() {
                    self.numeric = None;
                }
                Some(true)
            }
            Action::Back => {
                self.numeric = None;
                Some(true)
            }
            Action::Activate => {
                self.commit_numeric(ctx);
                Some(true)
            }
            // anything else commits what was typed, then acts
            _ => {
                self.commit_numeric(ctx);
                None
            }
        }
    }

    fn commit_numeric(&mut self, ctx: &mut UiCtx<'_>) {
        let Some((digits, _)) = self.numeric.take() else { return };
        let Ok(n) = digits.parse::<u32>() else { return };
        let Some(g) = &mut self.grid else { return };
        let row = g.rows.iter().find(|r| ctx.lib.live.get(r.channel).is_some_and(|c| c.number == Some(n))).map(|r| r.channel);
        match row {
            Some(ch) => {
                let t = g.focus_time;
                g.jump_to(ch, t);
            }
            None if ctx.lib.live.by_number(n).is_some() => ctx.toast(Toast::info(format!("Channel {n} isn't in this collection — press f to pick another"))),
            None => ctx.toast(Toast::info(format!("No channel {n}"))),
        }
    }

    /// Header: title, collection picker, ‹ day ›, Now, freshness / hints.
    fn header(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<Action> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let mut act = None;
        let name = self.collection.as_ref().map(|c| Self::collection_name(ctx, c)).unwrap_or_default();
        let view_start = self.grid.as_ref().map_or(ctx.now, |g| g.view_start);
        let compact = ctx.layout == LayoutClass::Compact;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
            if !compact {
                section::section_header(ui, theme, "TV guide");
            }
            if button(ui, theme, None, &format!("{name} ▾"), ButtonStyle { bordered: true, ..Default::default() }).on_hover_text("Collection (f)").clicked() {
                act = Some(Action::GuideCategory);
            }
            if icon_button(ui, theme, glyphs::PREVIOUS, "Previous day ([)", Default::default()).clicked() {
                act = Some(Action::GuidePrevDay);
            }
            let mid = view_start + 3600;
            ui.label(RichText::new(day_label(mid)).size(theme.font(TypeToken::Body)).color(t.fg));
            if icon_button(ui, theme, glyphs::NEXT, "Next day (])", Default::default()).clicked() {
                act = Some(Action::GuideNextDay);
            }
            if button(ui, theme, None, "Now", Default::default()).on_hover_text("Jump to now (n)").clicked() {
                act = Some(Action::GuideNow);
            }
            let (text, color) = self.status_line(ctx, view_start);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(color)).truncate());
            });
        });
        act
    }

    /// Right side of the header: loading / missing / stale / no-data notices, why the focused
    /// channel has no programmes, else key hints. Stale and ended use the status bar's wording
    /// (DESIGN §9.5, §13.12).
    fn status_line(&mut self, ctx: &mut UiCtx<'_>, view_start: Timestamp) -> (String, egui::Color32) {
        let t = &ctx.theme.tokens;
        let hints = ("f collection · [ ] day · n now · r remind".to_owned(), t.text_secondary);
        let joined = |r: NoGuide| {
            let (title, detail) = r.copy();
            detail.map_or(title.clone(), |d| format!("{title} · {d}"))
        };
        if let Some(loading) = self.guide.loading_label(ctx) {
            return (loading, t.text_secondary);
        }
        let Some(info) = self.guide.info(ctx) else { return hints };
        if info.sources == 0 {
            return (joined(NoGuide::Missing), t.text_secondary);
        }
        if info.max_stop.is_none() {
            return (joined(NoGuide::NotLoaded), t.warning);
        }
        let refresh = crate::keymap::GLOBAL.iter().find(|b| b.action == Action::RefreshSource).map(|b| b.chord.label()).unwrap_or_default();
        if let Some(stop) = info.max_stop {
            if stop <= ctx.now {
                return (format!("{} Guide data ended · {refresh} refresh", glyphs::WARNING), t.warning);
            }
            if view_start >= stop {
                return (format!("No guide data for {}", day_label(view_start + 3600)), t.text_secondary);
            }
        }
        if let Some(at) = info.last_refresh.filter(|at| ctx.now - at >= GUIDE_STALE_S) {
            return (format!("{} Guide updated {} · {refresh} refresh", glyphs::WARNING, live::age_label(ctx.now, at)), t.warning);
        }
        // the focused channel isn't in the guides, and they cover few channels: say so, the
        // action first (the label truncates); the cells already say it for well-covered ones
        let unmapped = self.grid.as_ref().and_then(|g| g.focused()).and_then(|(ch, _)| ctx.lib.live.get(ch)).is_some_and(|c| c.epg_channel_id.is_none());
        if unmapped {
            let (covered, total) = self.guide.coverage(ctx);
            let why = NoGuide::NotCovered { covered, total };
            if why.poor() {
                return (format!("{} · guides cover {} of {} channels", why.short(), grouped(covered), grouped(total)), t.text_secondary);
            }
        }
        hints
    }

    /// The numeric entry box: digits + the channel they match (like the player's).
    fn draw_numeric(&self, ui: &egui::Ui, ctx: &UiCtx<'_>, area: Rect) {
        let Some((digits, _)) = &self.numeric else { return };
        let theme = ctx.theme;
        let t = &theme.tokens;
        let p = ui.painter();
        let g = p.layout_no_wrap(format!("{digits}_"), egui::FontId::proportional(theme.font(TypeToken::Display)), t.accent);
        let name = digits.parse::<u32>().ok().and_then(|n| ctx.lib.live.by_number(n)).and_then(|id| ctx.lib.live.get(id)).map(|c| c.name.to_string());
        let name_g = name.map(|n| {
            let mut job = egui::text::LayoutJob::simple_singleline(n, egui::FontId::proportional(theme.font(TypeToken::Body)), t.text_secondary);
            job.wrap = egui::text::TextWrapping::truncate_at_width((240.0 * theme.rem()).round());
            p.layout_job(job)
        });
        let pad = theme.space(SpaceToken::PopupPadding);
        let gap = theme.space(SpaceToken::Sm);
        let inner = egui::vec2(g.size().x.max(name_g.as_ref().map_or(0.0, |n| n.size().x)), g.size().y + name_g.as_ref().map_or(0.0, |n| gap + n.size().y));
        let r = Rect::from_min_size(egui::pos2(area.right() - inner.x - 3.0 * pad, area.top() + pad), inner + egui::vec2(2.0 * pad, 2.0 * pad));
        p.rect_filled(r, t.rounding, t.card_bg);
        p.rect_stroke(r, t.rounding, egui::Stroke::new(t.card_border_width, t.popup_border), egui::StrokeKind::Inside);
        let gh = g.size().y;
        p.galley(r.min + egui::vec2(pad, pad), g, t.accent);
        if let Some(n) = name_g {
            p.galley(r.min + egui::vec2(pad, pad + gh + gap), n, t.text_secondary);
        }
    }

    /// Optional preview strip (Wide + `guide_preview`): video + the focused programme.
    fn preview_strip(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let w = ui.available_width();
        let video = Rect::from_min_size(ui.cursor().min, egui::vec2(w, (w * 9.0 / 16.0).round()));
        let snap = ctx.player.clone();
        if snap.state != PlayerState::Idle {
            if ctx.video.paint(ui, video, Placement::Preview, theme, ctx.images, &snap).clicked() {
                ctx.push(Route::Player);
            }
        } else {
            let r = ui.allocate_rect(video, egui::Sense::hover()).rect;
            ui.painter().rect_filled(r, t.rounding, t.fill_field);
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        let Some((ch, prog)) = self.grid.as_ref().and_then(|g| g.focused()).map(|(c, p)| (c, p.cloned())) else { return };
        if let Some(c) = ctx.lib.live.get(ch) {
            ui.label(RichText::new(c.name.as_ref()).size(theme.font(TypeToken::Title)).color(t.fg));
        }
        if let Some(p) = prog {
            ui.label(RichText::new(p.title.as_ref()).size(theme.font(TypeToken::Body)).color(t.fg));
            ui.label(RichText::new(epg_grid::time_range(p.start, p.stop, ctx.config.interface.clock_24h)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
        }
    }

    /// Guide preview autoplay: keyboard rest only, never replacing a user-started stream.
    fn autoplay(&mut self, ctx: &mut UiCtx<'_>, ui: &egui::Ui, strip: bool) {
        let Some((ch, at)) = self.rest else { return };
        if !strip || self.grid.as_ref().and_then(|g| g.focused()).map(|f| f.0) != Some(ch) {
            self.rest = None;
            return;
        }
        let delay = Duration::from_millis(u64::from(ctx.config.interface.preview_delay_ms));
        if at.elapsed() < delay {
            ui.ctx().request_repaint_after(delay - at.elapsed());
            return;
        }
        self.rest = None;
        let snap = ctx.player;
        let allowed = snap.state == PlayerState::Idle || snap.origin == Some(PlayOrigin::LivePreview);
        let Some(c) = ctx.lib.live.get(ch) else { return };
        if !allowed || playing_channel(snap) == Some(ch) || c.unsupported || (c.locked && !ctx.parental.is_unlocked(&ctx.config.parental)) {
            return;
        }
        let zap = self.zap(ctx);
        ctx.play(PlayerCommand::Play(PlayRequest { media: crate::model::MediaRef::Channel(ch), zap, origin: PlayOrigin::LivePreview, resume: false, immediate: true }));
    }
}

impl Screen for GuideScreen {
    fn id(&self) -> ScreenId { ScreenId::Guide }

    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.numeric.is_some() { vec![KeyContext::NumericEntry, KeyContext::Guide] } else { vec![KeyContext::Guide] }
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let Route::Guide { collection, focus } = route else {
            self.refresh(ctx);
            return;
        };
        // back from a playback the guide started (then maybe zapped): the same route again,
        // a newer load, still the guide's stream
        let snap = ctx.player.clone();
        let returning = self.launched.take().is_some_and(|g| g != snap.load_generation) && snap.origin == Some(PlayOrigin::Guide) && self.entered.as_ref() == Some(route);
        self.entered = Some(route.clone());
        if !returning && let Some(c) = collection {
            self.collection = Some(c.clone());
        }
        self.guide = GuideStatus::default();
        self.refresh(ctx);
        let now = ctx.now;
        let target = if returning { played_channel(&snap).or(*focus) } else { *focus };
        if let (Some(ch), Some(g)) = (target, &mut self.grid) {
            g.jump_to(ch, now);
        }
    }

    fn on_event(&mut self, _ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        self.guide.on_event(ev);
        match ev {
            AppEvent::Data(DataScope::Reminders) => self.reminders_dirty = true,
            AppEvent::Data(DataScope::Epg) => {
                self.dirty = true;
                if let Some(g) = &mut self.grid {
                    g.cache.invalidate();
                }
            }
            AppEvent::Data(DataScope::Favourites | DataScope::Sources | DataScope::All) => self.dirty = true,
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.refresh(ctx);
        if ctx.lib.built_at != 0 && ctx.lib.is_empty() {
            // "No playlists yet": Enter is the "Add playlist" button
            if action == Action::Activate {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(crate::ui::OnboardingStart::AddSource)));
                return true;
            }
            return false;
        }
        if let Some(handled) = self.numeric_action(ctx, action) {
            return handled;
        }
        match action {
            Action::Digit(d) => {
                self.numeric = Some((char::from(b'0' + d.min(9)).to_string(), Instant::now()));
                true
            }
            Action::GuideCategory => {
                self.open_picker(ctx);
                true
            }
            Action::ToggleFavourite => {
                if let Some(c) = self.grid.as_ref().and_then(|g| g.focused()).and_then(|(id, _)| ctx.lib.live.get(id).cloned()) {
                    live::toggle_favourite(ctx, live::channel_ref(&c), &c.name);
                }
                true
            }
            Action::AssignEpg => {
                if let Some((ch, _)) = self.grid.as_ref().and_then(|g| g.focused()) {
                    live::assign_epg(ctx, &mut self.guide, ch);
                }
                true
            }
            // the empty state's "Show hidden", from the keyboard
            Action::Activate if self.all_hidden(ctx) => {
                ctx.push(Route::Settings(SettingsPage::Hidden));
                true
            }
            Action::OpenSearch | Action::ToggleHelp | Action::Back => false,
            _ => {
                let Some(g) = &mut self.grid else { return false };
                let focused_prog = g.focused().and_then(|(_, p)| p.cloned());
                let mut reader = ctx.epg_reader();
                let out = g.handle(action, &mut reader, ctx.budget, ctx.now);
                match (action, &out) {
                    (Action::GuideCatchup, GridOutcome::None) if focused_prog.is_some() => {
                        ctx.toast(Toast::info("No catch-up for this programme"));
                    }
                    (Action::GuideRemind, GridOutcome::None) if focused_prog.is_some() => {
                        ctx.toast(Toast::info("Reminders are for programmes that haven't started"));
                    }
                    _ => {}
                }
                self.handle_outcome(ctx, out);
                // the grid's keys never fall through to globals
                matches!(
                    action,
                    Action::Up
                        | Action::Down
                        | Action::Left
                        | Action::Right
                        | Action::PageUp
                        | Action::PageDown
                        | Action::Home
                        | Action::End
                        | Action::Activate
                        | Action::GuidePrevDay
                        | Action::GuideNextDay
                        | Action::GuideNow
                        | Action::GuideBackHours
                        | Action::GuideForwardHours
                        | Action::GuideRemind
                        | Action::GuideWatch
                        | Action::GuideCatchup
                )
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.refresh(ctx);
        let theme = ctx.theme;
        if ctx.lib.built_at != 0 && ctx.lib.is_empty() {
            if empty_state(ui, theme, glyphs::PLAYLIST, "No playlists yet", Some(live::EMPTY_LIBRARY_DETAIL), Some("Add playlist")).is_some_and(|r| r.clicked()) {
                ctx.navigate(crate::ui::NavRequest::Switch(Route::Onboarding(crate::ui::OnboardingStart::AddSource)));
            }
            return;
        }
        if let Some(a) = self.header(ui, ctx) {
            self.on_action(ctx, a);
        }
        ui.add_space(theme.space(SpaceToken::Md));
        if let Some((_, at)) = &self.numeric {
            if at.elapsed() >= NUMERIC_COMMIT {
                self.commit_numeric(ctx);
            } else {
                ui.ctx().request_repaint_after(NUMERIC_COMMIT - at.elapsed());
            }
        }
        if ctx.lib.built_at == 0 {
            live::skeleton_rows(ui, theme, GridMetrics::for_layout(ctx.layout, false, theme).row_height);
            return;
        }
        if self.grid.as_ref().is_some_and(|g| g.rows.is_empty()) {
            if self.all_hidden(ctx) {
                if empty_state(ui, theme, glyphs::HIDDEN, "Everything here is hidden", Some("Enter opens Settings ▸ Hidden items · f shows another collection"), Some("Show hidden"))
                    .is_some_and(|r| r.clicked())
                {
                    ctx.push(Route::Settings(SettingsPage::Hidden));
                }
                return;
            }
            let (glyph, title) = match &self.collection {
                Some(Collection::Favourites(_)) => (glyphs::FAVOURITE, "Press Ctrl+F on any channel to add it here"),
                Some(Collection::History) => (glyphs::HISTORY, "Nothing watched yet"),
                _ => (glyphs::GUIDE, "This group is empty"),
            };
            empty_state(ui, theme, glyph, title, Some("Press f to show another collection"), None);
            return;
        }
        let area = ui.available_rect_before_wrap();
        let strip = ctx.layout == LayoutClass::Wide && ctx.config.interface.guide_preview;
        let gap = theme.space(SpaceToken::PanelGap);
        let strip_w = if strip { (320.0 * theme.rem()).round() } else { 0.0 };
        let grid_rect = Rect::from_min_max(area.min, egui::pos2(area.right() - if strip { strip_w + gap } else { 0.0 }, area.bottom()));
        let compact_rows = ctx.config.interface.density == crate::config::Density::Compact;
        let metrics = GridMetrics::for_layout(ctx.layout, compact_rows, theme);
        let mut out = GridOutcome::None;
        let no_guide = self.guide.no_guide(ctx).map(|r| r.short());
        if let Some(g) = &mut self.grid {
            g.avoid = ctx.mini_rect;
            g.no_guide_label = no_guide;
            g.clock_24h = ctx.config.interface.clock_24h;
            // hovered programme: load its description for the tooltip (one indexed read)
            if let Some(id) = g.hovered.filter(|id| g.hover_detail.as_ref().is_none_or(|(h, _)| h != id)) {
                let db = ctx.db;
                if let Some(r) = ctx.budget.run(|| crate::store::epg::programme(db, id)) {
                    let d = r.ok().and_then(|p| p.description.or(p.sub_title)).unwrap_or_default();
                    g.hover_detail = Some((id, d));
                }
            }
            let mut reader = ctx.epg_reader();
            out = ui
                .scope_builder(egui::UiBuilder::new().max_rect(grid_rect), |ui| epg_grid::epg_grid(ui, theme, ctx.images, &mut reader, ctx.budget, &ctx.lib.live, g, metrics, ctx.now))
                .inner;
        }
        self.draw_numeric(ui, ctx, grid_rect);
        if strip {
            let r = Rect::from_min_max(egui::pos2(area.right() - strip_w, area.top()), area.max);
            section::pane_divider(ui.painter(), r.left() - gap / 2.0, area.y_range(), theme);
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| self.preview_strip(ui, ctx));
        }
        if matches!(out, GridOutcome::FocusChanged) {
            // pointer focus never autoplays
            self.rest = None;
        } else {
            self.handle_outcome(ctx, out);
        }
        self.autoplay(ctx, ui, strip);
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            OverlayResult::Chosen(a) => {
                if let Some(c) = choice_index(a).and_then(|i| self.choices.get(i).cloned()) {
                    self.set_collection(ctx, c);
                }
            }
            OverlayResult::Unlocked(a) => {
                self.on_action(ctx, a);
            }
            _ => {}
        }
    }
}
