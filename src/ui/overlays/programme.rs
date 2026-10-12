//! Guide programme popover: title, time range, channel, description, episode, categories;
//! actions Watch (live now) / Catch up (past + archive) / Remind me (future) / Assign EPG.
//! Owner: WP-11.
//!
//! The popover performs its actions itself (it can be opened from the guide, and from search
//! results that show programmes). Watch zaps through the zap list of the request
//! (`OverlayRequest::Programme::zap` — the guide passes its collection, DESIGN §13.0), else
//! through the channel's primary group. A locked channel opens the PIN prompt with `GuideWatch` / `GuideCatchup`
//! as the action to retry, which the guide (the opener) re-runs on its focused programme.
//! The programme row is read once (one indexed read inside the frame budget).
//!
//! The batch features (base-B, plan §2.2): at most 4 primary buttons — Watch / Catch up ·
//! Remind me · Up next · Follow… (`ui::item_menu::popover_buttons`) — and everything else under
//! "More…" (Ctrl+Enter; `ui::item_menu::extend`): Watch alongside, Mark seen, Guard spoilers
//! about…. Their choices go to `ui::item_menu::dispatch` on the programme.

use eframe::egui::{self, RichText};

use super::dialogs::{card_area, scrim};
use super::{ContextMenuSpec, MenuItem, Overlay, OverlayOutcome, OverlayResult};
use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, ChannelSummary, PlayOrigin, Programme, ProgrammeFlags, ProgrammeId, ProgrammeSlim, ReminderId, ZapList};
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::epg_grid::{day_label, time_left, time_range};
use crate::ui::screens::live;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::{badge, glyphs, progress};
use crate::ui::{Route, UiCtx};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Watch,
    Catchup,
    Remind,
    AssignEpg,
    /// A feature's primary button (`ProgrammeOverlay::slots`).
    Slot(usize),
    /// The features' other entries.
    More,
    Close,
}

/// What the popover shows, read once.
struct Loaded {
    /// `None` when the programme vanished (a newer guide import).
    programme: Option<Programme>,
    /// The programme as displayed on the channel (tvg-shift applied).
    slim: Option<ProgrammeSlim>,
    channel: Option<ChannelSummary>,
    reminder: Option<ReminderId>,
}

pub struct ProgrammeOverlay {
    pub programme: ProgrammeId,
    pub channel: ChannelId,
    cursor: usize,
    loaded: Option<Loaded>,
    zap: Option<Option<ZapList>>,
    /// The features' primary buttons and "More…" entries (read with the programme).
    slots: Vec<MenuItem>,
    more: Vec<MenuItem>,
}

impl ProgrammeOverlay {
    /// `zap`: the opener's zap list for Watch (`None` = the channel's primary group).
    pub fn new(programme: ProgrammeId, channel: ChannelId, zap: Option<ZapList>) -> Self {
        Self { programme, channel, cursor: 0, loaded: None, zap: zap.map(Some), slots: Vec::new(), more: Vec::new() }
    }

    fn load(&mut self, ctx: &mut UiCtx<'_>) {
        if self.zap.is_none() {
            self.zap = Some(live::primary_zap(ctx, self.channel));
        }
        if self.loaded.is_some() {
            return;
        }
        let (db, id) = (ctx.db, self.programme);
        let Some(r) = ctx.budget.run(|| crate::store::epg::programme(db, id)) else { return };
        let channel = ctx.lib.live.get(self.channel).cloned();
        let programme = r.map_err(|e| tracing::debug!("programme {id}: {e}")).ok();
        let slim = programme.as_ref().map(|p| {
            let s = ProgrammeSlim {
                id: p.id,
                start: p.start,
                stop: p.stop,
                title: p.title.as_str().into(),
                sub_title: p.sub_title.as_deref().map(Into::into),
                flags: ProgrammeFlags { is_new: p.is_new, is_live: p.is_live, has_catchup_id: p.catchup_id.is_some() },
            };
            s.shifted(channel.as_ref().map_or(0, |c| c.tvg_shift_min))
        });
        let reminder = match (&channel, &slim) {
            (Some(c), Some(s)) => crate::store::user::reminders_for_channel(db, &live::channel_ref(c))
                .unwrap_or_default()
                .into_iter()
                .find(|r| r.start == s.start && r.state == crate::model::ReminderState::Pending)
                .map(|r| r.id),
            _ => None,
        };
        self.loaded = Some(Loaded { programme, slim, channel, reminder });
        let target = self.target();
        self.slots = crate::ui::item_menu::popover_buttons(&target, ctx);
        let mut more = Vec::new();
        crate::ui::item_menu::extend(&mut more, &target, ctx, KeyContext::Guide);
        self.more = more;
    }

    fn target(&self) -> crate::ui::item_menu::Target {
        crate::ui::item_menu::Target::Programme { programme: self.programme, channel: self.channel }
    }

    /// Available actions, in display order.
    fn actions(&self, now: crate::model::Timestamp) -> Vec<Act> {
        let mut v = Vec::new();
        let Some(l) = &self.loaded else { return vec![Act::Close] };
        let Some(ch) = &l.channel else { return vec![Act::Close] };
        match &l.slim {
            Some(p) => {
                if p.contains(now) {
                    v.push(Act::Watch);
                }
                // the same archive rule as the guide's rows
                let catchup_days = if ch.has_catchup { ch.catchup_days } else { 0 };
                let row = crate::ui::epg_grid::GuideRow { channel: ch.id, epg: ch.epg_channel_id, tvg_shift_min: ch.tvg_shift_min, catchup_days };
                if crate::ui::epg_grid::catchup_available(&row, p, now) {
                    v.push(Act::Catchup);
                }
                if p.start > now {
                    v.push(Act::Remind);
                }
                if !v.contains(&Act::Watch) {
                    v.push(Act::Watch);
                }
            }
            None => v.push(Act::Watch),
        }
        v.extend((0..self.slots.len()).map(Act::Slot));
        if !self.more.is_empty() {
            v.push(Act::More);
        }
        v.push(Act::AssignEpg);
        v.push(Act::Close);
        v
    }

    fn label(&self, a: Act, now: crate::model::Timestamp) -> (&'static str, String) {
        match a {
            Act::Watch => {
                let live_now = self.loaded.as_ref().and_then(|l| l.slim.as_ref()).is_none_or(|p| p.contains(now));
                (glyphs::PLAY, if live_now { "Watch".into() } else { "Watch channel".into() })
            }
            Act::Catchup => (glyphs::CATCHUP, "Catch up".into()),
            Act::Remind => {
                let set = self.loaded.as_ref().is_some_and(|l| l.reminder.is_some());
                (glyphs::REMINDER, if set { "Cancel reminder".into() } else { "Remind me".into() })
            }
            Act::AssignEpg => (glyphs::GUIDE, "Assign guide channel…".into()),
            Act::Slot(i) => self.slots.get(i).map_or(("", String::new()), |m| (m.icon, m.label.clone())),
            Act::More => (glyphs::MORE, "More…".into()),
            Act::Close => ("", "Close".into()),
        }
    }

    fn run(&mut self, ctx: &mut UiCtx<'_>, a: Act) -> OverlayOutcome {
        let Some(l) = &self.loaded else { return OverlayOutcome::Keep };
        match a {
            Act::Watch => {
                let zap = self.zap.clone().flatten();
                live::activate_channel(ctx, self.channel, zap, PlayOrigin::Guide, false, Action::GuideWatch);
            }
            Act::Catchup => {
                if let Some(p) = l.slim.clone() {
                    live::play_catchup(ctx, self.channel, &p, Action::GuideCatchup);
                }
            }
            Act::Remind => {
                if let (Some(ch), Some(p)) = (l.channel.clone(), l.slim.clone()) {
                    live::toggle_reminder(ctx, &ch, &p, l.reminder);
                }
            }
            Act::AssignEpg => ctx.push(Route::EpgMapping { channel: Some(self.channel) }),
            Act::Slot(i) => {
                if let Some(a) = self.slots.get(i).map(|m| m.action) {
                    crate::ui::item_menu::dispatch(ctx, a, &self.target());
                }
            }
            Act::More => {
                self.open_more(ctx);
                return OverlayOutcome::Keep;
            }
            Act::Close => {}
        }
        OverlayOutcome::Close
    }

    fn open_more(&self, ctx: &mut UiCtx<'_>) {
        let title = self.loaded.as_ref().and_then(|l| l.programme.as_ref()).map_or_else(String::new, |p| p.title.clone());
        ctx.open(crate::ui::OverlayRequest::ContextMenu(ContextMenuSpec { title, items: self.more.clone() }));
    }
}

impl Overlay for ProgrammeOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Modal]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.load(ctx);
        let acts = self.actions(ctx.now);
        let n = acts.len();
        match action {
            Action::Up | Action::Left | Action::FocusPrev => {
                self.cursor = self.cursor.saturating_sub(1);
                OverlayOutcome::Keep
            }
            Action::Down | Action::Right | Action::FocusNext => {
                self.cursor = (self.cursor + 1).min(n.saturating_sub(1));
                OverlayOutcome::Keep
            }
            Action::Activate => match acts.get(self.cursor.min(n.saturating_sub(1))) {
                Some(a) => self.run(ctx, *a),
                None => OverlayOutcome::Close,
            },
            Action::Back => OverlayOutcome::Close,
            Action::ContextMenu if !self.more.is_empty() => {
                self.open_more(ctx);
                OverlayOutcome::Keep
            }
            _ => OverlayOutcome::Ignored,
        }
    }

    /// A "More…" choice: the feature acts on the programme.
    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) -> OverlayOutcome {
        match result {
            OverlayResult::Chosen(a) if crate::ui::item_menu::dispatch(ctx, a, &self.target()) => OverlayOutcome::Close,
            _ => OverlayOutcome::Keep,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.load(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "programme") {
            return OverlayOutcome::Close;
        }
        let now = ctx.now;
        let h24 = ctx.config.interface.clock_24h;
        let acts = self.actions(now);
        self.cursor = self.cursor.min(acts.len().saturating_sub(1));
        let spoilers = crate::ui::widgets::spoiler::Spoilers::of(ctx);
        let mut clicked = None;
        card_area(egui_ctx, theme, "programme", CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            let Some(l) = &self.loaded else {
                ui.label(RichText::new("Loading…").size(theme.font(TypeToken::Body)).color(t.text_secondary));
                return;
            };
            match (&l.programme, &l.slim) {
                (Some(p), Some(s)) => {
                    // the spoiler guard's view of the text (S-ui draws what replaces hidden text)
                    let subj = crate::ui::widgets::spoiler::programme_subject(p);
                    let title = spoilers.text(&subj, crate::guard::Field::Title, &p.title).unwrap_or_default();
                    ui.add(egui::Label::new(RichText::new(title.as_ref()).size(theme.font(TypeToken::Heading)).color(t.fg)).wrap());
                    let sub = p.sub_title.as_deref().filter(|x| !x.is_empty()).and_then(|x| spoilers.text(&subj, crate::guard::Field::SubTitle, x));
                    if let Some(sub) = sub {
                        ui.label(RichText::new(sub.as_ref()).size(theme.font(TypeToken::Title)).color(t.text_secondary));
                    }
                    let mut when = format!("{} · {}", day_label(s.start), time_range(s.start, s.stop, h24));
                    if let Some(ch) = &l.channel {
                        when.push_str(&format!(" · {}", ch.name));
                    }
                    ui.label(RichText::new(when).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                    ui.add_space(theme.space(SpaceToken::Sm));
                    ui.horizontal_wrapped(|ui| {
                        if let Some(e) = p.episode {
                            let label = match (e.season, e.episode) {
                                (Some(s), Some(n)) => format!("S{s:02}E{n:02}"),
                                (None, Some(n)) => format!("E{n:02}"),
                                (Some(s), None) => format!("S{s:02}"),
                                (None, None) => String::new(),
                            };
                            if !label.is_empty() {
                                badge::badge(ui, theme, &label, t.fg);
                            }
                        }
                        if p.is_live {
                            badge::live_badge(ui, theme);
                        }
                        if p.is_new {
                            badge::badge(ui, theme, "NEW", t.accent);
                        }
                        for c in p.categories.iter().take(4) {
                            badge::badge(ui, theme, c, t.text_secondary);
                        }
                    });
                    if s.contains(now) {
                        ui.add_space(theme.space(SpaceToken::Sm));
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() * 0.7).max(60.0);
                            ui.allocate_ui(egui::vec2(w, (8.0 * theme.rem()).round()), |ui| {
                                progress::progress_bar(ui, theme, s.progress(now), (3.0 * theme.rem()).round())
                            });
                            ui.label(RichText::new(time_left(s.stop - now)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                        });
                    }
                    let descr =
                        p.description.as_deref().filter(|d| !d.is_empty()).and_then(|d| spoilers.text(&subj, crate::guard::Field::Description, d));
                    if let Some(d) = descr {
                        ui.add_space(theme.space(SpaceToken::Md));
                        egui::ScrollArea::vertical().max_height(egui_ctx.content_rect().height() * 0.35).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(d.as_ref()).size(theme.font(TypeToken::Body)).color(t.fg)).wrap());
                        });
                    }
                }
                _ => {
                    ui.label(RichText::new("This programme is no longer in the guide").size(theme.font(TypeToken::Title)).color(t.text_secondary));
                }
            }
            ui.add_space(theme.space(SpaceToken::Lg));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
                for (i, a) in acts.iter().enumerate() {
                    let (icon, label) = self.label(*a, now);
                    let style = ButtonStyle {
                        focused: i == self.cursor,
                        primary: i == 0 && *a != Act::Close,
                        bordered: *a == Act::Close,
                        ..Default::default()
                    };
                    if button(ui, theme, (!icon.is_empty()).then_some(icon), &label, style).clicked() {
                        clicked = Some(*a);
                    }
                }
            });
        });
        match clicked {
            Some(a) => self.run(ctx, a),
            None => OverlayOutcome::Keep,
        }
    }
}
