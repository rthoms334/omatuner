//! The reminder engine's UI (B6). OWNER: rui.
//!
//! * **Programmes:** "Switch to this when it starts…" (or "Edit reminder…") in a programme's
//!   menus — the guide's Ctrl+Enter menu and the programme popover's "More…" — opens the
//!   reminder sheet (`ui::overlays::reminders::ReminderSheet`): notify or switch, how long
//!   before, the start offset ("skip the build-up"), where a switch opens, and the clashes.
//!   `r` keeps setting a plain notification at once.
//! * **The strip** ([`ReminderUi`]): an automatic switch's countdown ("Switching to ‹channel›
//!   in 8 s · ‹title›" with Cancel and, when it opens in the pop-out, Open full), the sleep
//!   timer's warning (+15 min / Turn off) and a running news flash (Open full / End). It is
//!   drawn above everything, in the full window and in the pop-out, and never takes focus.
//! * **Where scheduled playback opens** ([`placement`]): the pop-out (pinned, every workspace,
//!   no focus taken) unless Tuner is in the foreground playing already; the full window; or
//!   wherever Tuner is.
//! * **Sleep timer:** the menu (Ctrl+Shift+T, the player menu, the status-bar chip), the
//!   warning, the chip text ([`sleep_chip`]).

use std::time::Duration;

use eframe::egui::{self, RichText};

use crate::config::ReminderConfig;
use crate::keymap::{Action, KeyContext};
use crate::model::{
    ChannelId, ChannelSummary, MediaRef, OpenIn, PlayOrigin, ProgrammeId, Reminder, ReminderId, ReminderMode, ReminderState, Timestamp,
};
use crate::reminders::sleep::{SleepAction, SleepStatus, SleepWhen};
use crate::store::user::ReminderEdit;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::{CardKind, card_frame};
use crate::ui::widgets::glyphs;
use crate::ui::widgets::time::hhmm;

// ------------------------------------------------------------------ the sheet's choices

/// "Notify me … before" (minutes). `lead_s = 0` means "the setting" in the engine, so there is
/// no "at the start".
pub const LEADS_MIN: [u32; 6] = [1, 2, 5, 10, 15, 30];
/// "Start" (minutes after the programme's start; negative = early).
pub const OFFSETS_MIN: [i64; 6] = [-5, 0, 5, 10, 15, 30];
/// "Open in", in display order.
pub const OPEN_IN: [OpenIn; 3] = [OpenIn::PopOut, OpenIn::FullWindow, OpenIn::WhereIAm];
/// The sleep warning's "+15 min".
pub const EXTEND_S: i64 = 15 * 60;

pub fn open_in_label(o: OpenIn) -> &'static str {
    match o {
        OpenIn::PopOut => "Pop-out",
        OpenIn::FullWindow => "Full window",
        OpenIn::WhereIAm => "Where I am",
    }
}

/// "5 min early" / "On time" / "10 min in".
pub fn offset_label(min: i64) -> String {
    match min {
        0 => "On time".into(),
        m if m < 0 => format!("{} min early", -m),
        m => format!("{m} min in"),
    }
}

/// What the reminder sheet sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SheetChoice {
    pub switch: bool,
    pub lead_min: u32,
    pub offset_min: i64,
    pub open_in: OpenIn,
}

impl SheetChoice {
    /// The sheet's starting values: the reminder's own, else a notification `lead_min` before
    /// (or a switch, `switch`), on time, opening where the setting says.
    pub fn of(existing: Option<&Reminder>, cfg: &ReminderConfig, switch: bool) -> Self {
        match existing {
            Some(r) => Self {
                switch: r.switches(),
                lead_min: if r.switches() || r.lead_s <= 0 { cfg.lead_min.max(1) } else { ((r.lead_s + 59) / 60) as u32 },
                offset_min: r.offset_s / 60,
                open_in: crate::reminders::open_in(r, cfg.open_in),
            },
            None => Self { switch, lead_min: cfg.lead_min.max(1), offset_min: 0, open_in: cfg.open_in },
        }
    }

    /// The lead choices: [`LEADS_MIN`] and the current one.
    pub fn leads(&self) -> Vec<u32> {
        let mut v = LEADS_MIN.to_vec();
        if !v.contains(&self.lead_min) {
            v.push(self.lead_min);
            v.sort_unstable();
        }
        v
    }

    /// The offset choices: [`OFFSETS_MIN`] and the current one.
    pub fn offsets(&self) -> Vec<i64> {
        let mut v = OFFSETS_MIN.to_vec();
        if !v.contains(&self.offset_min) {
            v.push(self.offset_min);
            v.sort_unstable();
        }
        v
    }

    fn mode(&self) -> ReminderMode {
        if self.switch { ReminderMode::Switch } else { ReminderMode::Notify }
    }

    /// The row's `open_in`: `None` when it is what the setting says (it then follows the
    /// setting), and always `None` for a notification.
    fn own_open_in(&self, default: OpenIn) -> Option<OpenIn> {
        (self.switch && self.open_in != default).then_some(self.open_in)
    }

    /// `r` as the sheet says (a new row).
    pub fn apply(&self, r: &mut Reminder, default: OpenIn) {
        r.mode = self.mode();
        r.lead_s = if self.switch { Reminder::SWITCH_LEAD } else { i64::from(self.lead_min) * 60 };
        r.offset_s = self.offset_min * 60;
        r.open_in = self.own_open_in(default);
    }

    /// The change to an existing row.
    pub fn edit(&self, default: OpenIn) -> ReminderEdit {
        ReminderEdit {
            lead_s: (!self.switch).then_some(i64::from(self.lead_min) * 60),
            mode: Some(self.mode()),
            offset_s: Some(self.offset_min * 60),
            open_in: Some(self.own_open_in(default)),
        }
    }
}

/// A programme as a reminder sees it: on `channel` (its time shift applied), with the pending
/// reminder of that programme if there is one. One indexed read each (menus and the sheet,
/// never per frame).
pub struct ProgrammeOn {
    pub channel: ChannelSummary,
    pub title: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub existing: Option<Reminder>,
}

pub fn programme_on(
    db: &rusqlite::Connection,
    lib: &crate::store::library::Library,
    programme: ProgrammeId,
    channel: ChannelId,
) -> Option<ProgrammeOn> {
    let ch = lib.live.get(channel)?.clone();
    let p = crate::store::epg::programme(db, programme).map_err(|e| tracing::debug!("programme {programme}: {e}")).ok()?;
    let d = i64::from(ch.tvg_shift_min) * 60;
    let (start, stop) = (p.start + d, p.stop + d);
    let item = crate::ui::screens::live::channel_ref(&ch);
    let existing = crate::store::user::reminders_for_channel(db, &item)
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.start == start && r.state == ReminderState::Pending);
    Some(ProgrammeOn { channel: ch, title: p.title, start, stop, existing })
}

impl ProgrammeOn {
    /// A new row for this programme, as `choice` says.
    pub fn draft(&self, choice: &SheetChoice, default: OpenIn) -> Reminder {
        let mut r = Reminder {
            id: self.existing.as_ref().map_or(ReminderId(0), |r| r.id),
            channel: crate::ui::screens::live::channel_ref(&self.channel),
            channel_name: self.channel.name.to_string(),
            programme_title: self.title.clone(),
            start: self.start,
            stop: self.stop,
            lead_s: 0,
            state: ReminderState::Pending,
            mode: ReminderMode::Notify,
            offset_s: 0,
            open_in: None,
            rule_id: self.existing.as_ref().and_then(|r| r.rule_id),
        };
        choice.apply(&mut r, default);
        r
    }
}

/// "Clashes with ‹title› on ‹channel› at 20:30" (+ why two switches matter).
pub fn clash_text(other: &Reminder, kind: crate::reminders::ClashKind, title: &str, h24: bool) -> String {
    let at = hhmm(crate::reminders::target_at(other), h24);
    let mut s = format!("Clashes with {title} on {} at {at}", other.channel_name);
    if kind == crate::reminders::ClashKind::Switches {
        s.push_str(" — both switch: the later one takes the picture");
    }
    s
}

// ------------------------------------------------------------------ item menus

/// "Switch to this when it starts…" / "Edit reminder…" on a programme that hasn't started.
pub fn menu_items(target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    let Target::Programme { programme, channel } = *target else { return Vec::new() };
    if !crate::reminders::ENABLED {
        return Vec::new();
    }
    let Some(p) = programme_on(ctx.db, ctx.lib, programme, channel) else { return Vec::new() };
    match &p.existing {
        Some(_) => vec![crate::ui::item_menu::entry(glyphs::REMINDER, "Edit reminder…", Action::ReminderOptions, keys)],
        None if p.start > ctx.now => {
            vec![crate::ui::item_menu::entry(glyphs::LIVE_TV, "Switch to this when it starts…", Action::ReminderOptions, keys)]
        }
        None => Vec::new(),
    }
}

/// The menu's choice: open the sheet.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    match (action, *target) {
        (Action::ReminderOptions, Target::Programme { programme, channel }) if crate::reminders::ENABLED => {
            ctx.open(crate::ui::OverlayRequest::ReminderSheet(crate::ui::overlays::reminders::ReminderSheetSpec {
                programme,
                channel,
                switch: true,
            }));
            true
        }
        _ => false,
    }
}

/// The player menu's "Sleep timer…" (the menu's choice comes back as `Action::SleepTimer`).
pub fn player_menu_items(keys: KeyContext) -> Vec<MenuItem> {
    if !crate::reminders::ENABLED {
        return Vec::new();
    }
    vec![crate::ui::screens::live::menu_item(glyphs::SLEEP, "Sleep timer…", Action::SleepTimer, true, keys)]
}

// ------------------------------------------------------------------ where it opens

/// The window as a switch finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowNow {
    /// Tuner's window has the keyboard focus.
    pub focused: bool,
    /// The Player screen shows and something plays.
    pub watching: bool,
    /// The window is popped out already.
    pub popped_out: bool,
    /// The pop-out is possible (Hyprland).
    pub can_pop: bool,
}

/// What a switch does with the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Pop the window out (pinned on every workspace, never taking focus) and play there.
    PopOut,
    /// Play in the full window's player (popping it back in first; the scheduler raised it).
    FullWindow,
    /// Play without touching the window or the screen: the pop-out stays, the player screen
    /// stays, elsewhere the corner mini player shows it.
    InPlace,
}

/// Where scheduled playback goes (big-features additions): "Pop-out" pops out unless Tuner is
/// in the foreground playing already (then it switches in place) — outside Hyprland the mini
/// player shows it in the normal window; "Full window" plays in the (raised) full window;
/// "Where I am" changes nothing but what plays. Pure.
pub fn placement(open_in: OpenIn, w: WindowNow) -> Placement {
    match open_in {
        OpenIn::PopOut if w.popped_out || (w.focused && w.watching) || !w.can_pop => Placement::InPlace,
        OpenIn::PopOut => Placement::PopOut,
        OpenIn::FullWindow => Placement::FullWindow,
        OpenIn::WhereIAm => Placement::InPlace,
    }
}

// ------------------------------------------------------------------ sleep timer text

/// "in 30 min" / "in 1 h 5 min".
pub fn in_minutes(secs: i64) -> String {
    let m = (secs.max(0) + 59) / 60;
    match (m / 60, m % 60) {
        (0, m) => format!("in {m} min"),
        (h, 0) => format!("in {h} h"),
        (h, m) => format!("in {h} h {m} min"),
    }
}

/// What a set timer does, for the menu, Settings and toasts: "Stops playback at 23:10 (in 42
/// min)", "Pauses after 2 more items", "Stops at the end of Up next".
pub fn sleep_status_text(s: &SleepStatus, now: Timestamp, h24: bool) -> String {
    let verb = match s.action {
        SleepAction::Stop => "Stops playback",
        SleepAction::Pause => "Pauses",
    };
    let when = match (s.when, s.fires_at, s.items_left) {
        (SleepWhen::EndOfQueue, None, _) => "at the end of Up next".to_owned(),
        (_, Some(t), _) => format!("at {} ({})", hhmm(t, h24), in_minutes(t - now)),
        (_, None, Some(1)) => "when this one ends".to_owned(),
        (_, None, Some(n)) => format!("after {n} more items"),
        (_, None, None) => "later".to_owned(),
    };
    format!("{verb} {when}")
}

/// The status-bar chip: "Sleep 23:10", "Sleep · 2 left", "Sleep · Up next".
pub fn sleep_chip(s: &SleepStatus, h24: bool) -> String {
    match (s.fires_at, s.items_left, s.when) {
        (Some(t), _, _) => format!("{} Sleep {}", glyphs::SLEEP, hhmm(t, h24)),
        (None, Some(n), _) => format!("{} Sleep · {n} left", glyphs::SLEEP),
        (None, None, SleepWhen::EndOfQueue) => format!("{} Sleep · Up next", glyphs::SLEEP),
        _ => format!("{} Sleep", glyphs::SLEEP),
    }
}

// ------------------------------------------------------------------ the strip

/// An automatic switch counting down.
#[derive(Debug, Clone, PartialEq)]
pub struct Countdown {
    pub reminder: ReminderId,
    pub channel_name: String,
    /// Guarded already (`Guard::outside_title`); may be empty.
    pub title: String,
    pub at: Timestamp,
    pub open_in: OpenIn,
}

impl Countdown {
    /// "Switching to BBC One in 8 s · News at Six".
    pub fn text(&self, now: Timestamp) -> String {
        let secs = (self.at - now).max(0);
        let mut s =
            if secs > 0 { format!("Switching to {} in {secs} s", self.channel_name) } else { format!("Switching to {}…", self.channel_name) };
        if !self.title.trim().is_empty() {
            s.push_str(" · ");
            s.push_str(self.title.trim());
        }
        s
    }
}

/// What played before a news flash, to put back after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Previous {
    pub media: MediaRef,
    pub origin: PlayOrigin,
    pub paused: bool,
}

/// A news flash on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct FlashRun {
    pub reminder: ReminderId,
    /// The rule's name ("BBC News at One").
    pub name: String,
    pub channel: ChannelId,
    /// When it closes by itself (headlines only: N minutes; else the programme's end).
    pub ends_at: Timestamp,
    pub previous: Option<Previous>,
    /// The window was popped out for it (it goes back in after).
    pub popped: bool,
    /// The Player screen showed before (else Back leaves it after).
    pub was_on_player: bool,
    /// Muted for it ("silent flash").
    pub muted: bool,
    /// Multi-view: the picture it plays in (it closes there; the other one is left alone).
    pub tile: Option<crate::model::TileId>,
}

/// What to do when a flash ends. Pure.
#[derive(Debug, Clone, PartialEq)]
pub enum FlashRestore {
    /// The user moved on during the flash: leave things as they are.
    Nothing,
    /// Stop (nothing played before).
    Stop,
    /// Put back what played.
    Play(Previous),
}

impl FlashRun {
    /// `now_playing`: the main picture's media now.
    pub fn restore(&self, now_playing: Option<&MediaRef>) -> FlashRestore {
        if now_playing != Some(&MediaRef::Channel(self.channel)) {
            return FlashRestore::Nothing;
        }
        match &self.previous {
            Some(p) => FlashRestore::Play(p.clone()),
            None => FlashRestore::Stop,
        }
    }
}

/// A button on the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripAction {
    CancelSwitch(ReminderId),
    /// Show this switch in the full window (now and when it switches).
    OpenFull(ReminderId),
    SleepExtend,
    SleepOff,
    /// End the news flash and put back what played.
    EndFlash,
    /// Keep watching the news in the full window (it no longer closes by itself).
    FlashOpenFull,
}

/// The app's reminder state: countdowns, the sleep warning, the news flash.
#[derive(Debug, Default)]
pub struct ReminderUi {
    pub countdowns: Vec<Countdown>,
    /// The sleep timer warned: it fires at this time.
    pub sleep_warn: Option<Timestamp>,
    pub flash: Option<FlashRun>,
    /// Switches the user asked to see in the full window ("Open full").
    pub open_full: std::collections::HashSet<ReminderId>,
    /// `--popout` (or a scheduled launch in the pop-out) asked for the pop-out before the
    /// Hyprland worker ran: popped out on the first frame that can.
    pub pop_out_pending: bool,
}

impl ReminderUi {
    pub fn add_countdown(&mut self, c: Countdown) {
        self.countdowns.retain(|x| x.reminder != c.reminder);
        self.countdowns.push(c);
        self.countdowns.sort_by_key(|c| (c.at, c.reminder));
    }

    pub fn drop_countdown(&mut self, id: ReminderId) -> Option<Countdown> {
        let i = self.countdowns.iter().position(|c| c.reminder == id)?;
        Some(self.countdowns.remove(i))
    }

    pub fn is_empty(&self) -> bool {
        self.countdowns.is_empty() && self.sleep_warn.is_none() && self.flash.is_none()
    }

    /// Draw the strip (top centre of the window, or across the pop-out). Never takes focus.
    pub fn draw(
        &self,
        egui_ctx: &egui::Context,
        theme: &Theme,
        now: Timestamp,
        video_only: bool,
        h24: bool,
        sleep: Option<SleepAction>,
    ) -> Option<StripAction> {
        if self.is_empty() {
            return None;
        }
        let t = &theme.tokens;
        let screen = egui_ctx.content_rect();
        let gap = theme.space(if video_only { SpaceToken::Sm } else { SpaceToken::PanelPadding });
        let width = if video_only { screen.width() - 2.0 * gap } else { (560.0 * theme.rem()).min(screen.width() - 2.0 * gap) };
        let frame = card_frame(theme, CardKind::Popup);
        let chrome = frame.total_margin().sum().x;
        let mut out = None;
        egui::Area::new(egui::Id::new("reminder-strip"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_TOP)
            .fixed_pos(egui::pos2(screen.center().x, screen.top() + gap))
            .interactable(true)
            .show(egui_ctx, |ui| {
                frame.show(ui, |ui| {
                    ui.set_width(width - chrome);
                    let line = |ui: &mut egui::Ui, icon: &str, text: String, color: egui::Color32| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(icon).size(theme.font(TypeToken::Icon)).color(color));
                            ui.add(egui::Label::new(RichText::new(text).size(theme.font(TypeToken::Body)).color(t.fg)).wrap());
                        });
                    };
                    let buttons = |ui: &mut egui::Ui, list: &[(&str, StripAction, bool)]| -> Option<StripAction> {
                        let mut hit = None;
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Md);
                            for (label, a, primary) in list {
                                let style = ButtonStyle { primary: *primary, bordered: !*primary, ..Default::default() };
                                if button(ui, theme, None, label, style).clicked() {
                                    hit = Some(*a);
                                }
                            }
                        });
                        hit
                    };
                    let mut first = true;
                    let mut sep = |ui: &mut egui::Ui| {
                        if !std::mem::replace(&mut first, false) {
                            ui.add_space(theme.space(SpaceToken::Sm));
                        }
                    };
                    for c in &self.countdowns {
                        sep(ui);
                        line(ui, glyphs::LIVE_TV, c.text(now), t.accent);
                        let mut list = vec![("Cancel", StripAction::CancelSwitch(c.reminder), true)];
                        if (c.open_in == OpenIn::PopOut || video_only) && !self.open_full.contains(&c.reminder) {
                            list.push(("Open full", StripAction::OpenFull(c.reminder), false));
                        }
                        out = out.or(buttons(ui, &list));
                    }
                    if let Some(at) = self.sleep_warn {
                        sep(ui);
                        let verb = if sleep == Some(SleepAction::Pause) { "pausing" } else { "stopping" };
                        let left = (at - now).max(0);
                        line(ui, glyphs::SLEEP, format!("Sleep timer: {verb} in {}:{:02}", left / 60, left % 60), t.warning);
                        out = out.or(buttons(ui, &[("+15 min", StripAction::SleepExtend, true), ("Turn off", StripAction::SleepOff, false)]));
                    }
                    if let Some(f) = &self.flash {
                        sep(ui);
                        line(ui, glyphs::LIVE_DOT, format!("News flash · {} · until {}", f.name, hhmm(f.ends_at, h24)), t.accent);
                        let mut list = vec![("End (Del)", StripAction::EndFlash, true)];
                        if video_only {
                            list.push(("Open full", StripAction::FlashOpenFull, false));
                        }
                        out = out.or(buttons(ui, &list));
                    }
                });
            });
        // the seconds tick
        egui_ctx.request_repaint_after(Duration::from_millis(250));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};

    /// The user's rule (2026-10-11): scheduled playback opens in the pop-out without taking
    /// focus — unless Tuner is the window in front and already playing, which switches in
    /// place; outside Hyprland the mini player shows it; "Where I am" never moves the window.
    #[test]
    fn scheduled_playback_pops_out_unless_tuner_is_in_front_playing() {
        let away = WindowNow { can_pop: true, ..Default::default() };
        assert_eq!(placement(OpenIn::PopOut, away), Placement::PopOut);
        assert_eq!(placement(OpenIn::PopOut, WindowNow { focused: true, ..away }), Placement::PopOut, "browsing: still the pop-out");
        assert_eq!(placement(OpenIn::PopOut, WindowNow { focused: true, watching: true, ..away }), Placement::InPlace);
        assert_eq!(placement(OpenIn::PopOut, WindowNow { watching: true, ..away }), Placement::PopOut, "playing on another workspace");
        assert_eq!(placement(OpenIn::PopOut, WindowNow { popped_out: true, ..away }), Placement::InPlace);
        assert_eq!(placement(OpenIn::PopOut, WindowNow::default()), Placement::InPlace, "no Hyprland: the mini player");
        assert_eq!(placement(OpenIn::FullWindow, WindowNow { popped_out: true, ..away }), Placement::FullWindow);
        assert_eq!(placement(OpenIn::WhereIAm, away), Placement::InPlace);
    }

    fn row(mode: ReminderMode, lead_s: i64) -> Reminder {
        let mut r = Reminder::notify(
            ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("xt:live:1".into()) },
            "BBC One".into(),
            "News".into(),
            1000,
            2000,
            lead_s,
        );
        r.mode = mode;
        if mode == ReminderMode::Switch {
            r.lead_s = Reminder::SWITCH_LEAD;
        }
        r
    }

    /// The sheet writes rows the engine reads the same way: a switch carries the switch
    /// marker, a note its own lead; "Open in" equal to the setting follows the setting (no
    /// own value), and a notification never keeps one.
    #[test]
    fn sheet_choices_become_engine_rows() {
        let cfg = ReminderConfig::default();
        let mut c = SheetChoice::of(None, &cfg, true);
        assert_eq!((c.switch, c.lead_min, c.offset_min, c.open_in), (true, 2, 0, OpenIn::PopOut));
        c.offset_min = 10;
        let mut r = row(ReminderMode::Notify, 120);
        c.apply(&mut r, cfg.open_in);
        assert!(r.switches() && r.offset_s == 600 && r.open_in.is_none());
        c.open_in = OpenIn::FullWindow;
        c.apply(&mut r, cfg.open_in);
        assert_eq!(r.open_in, Some(OpenIn::FullWindow));
        c.switch = false;
        c.lead_min = 15;
        c.apply(&mut r, cfg.open_in);
        assert_eq!((r.mode, r.lead_s, r.open_in), (ReminderMode::Notify, 900, None));
        assert_eq!(
            c.edit(cfg.open_in),
            ReminderEdit { lead_s: Some(900), mode: Some(ReminderMode::Notify), offset_s: Some(600), open_in: Some(None) }
        );
        // an existing switch opens as a switch, with the setting's lead ready for a note
        let back = SheetChoice::of(Some(&row(ReminderMode::Switch, 0)), &cfg, false);
        assert_eq!((back.switch, back.lead_min), (true, 2));
        let note = SheetChoice::of(Some(&row(ReminderMode::Notify, 7 * 60)), &cfg, true);
        assert_eq!((note.switch, note.lead_min), (false, 7));
        assert!(note.leads().contains(&7), "its own lead stays choosable");
    }

    /// A flash puts back what played — unless the user moved on during it.
    #[test]
    fn a_flash_restores_only_what_it_replaced() {
        let prev = Previous { media: MediaRef::Channel(ChannelId(4)), origin: PlayOrigin::Fullscreen, paused: false };
        let f = FlashRun {
            reminder: ReminderId(1),
            name: "News at One".into(),
            channel: ChannelId(9),
            ends_at: 100,
            previous: Some(prev.clone()),
            popped: true,
            was_on_player: false,
            muted: false,
            tile: None,
        };
        assert_eq!(f.restore(Some(&MediaRef::Channel(ChannelId(9)))), FlashRestore::Play(prev));
        assert_eq!(f.restore(Some(&MediaRef::Channel(ChannelId(5)))), FlashRestore::Nothing);
        assert_eq!(f.restore(None), FlashRestore::Nothing);
        let none = FlashRun { previous: None, ..f };
        assert_eq!(none.restore(Some(&MediaRef::Channel(ChannelId(9)))), FlashRestore::Stop);
    }
}
