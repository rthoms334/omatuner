//! Add or edit a spoiler topic or an "I'll watch it later" (DESIGN §34 §2.3). OWNER: S-ui.
//!
//! ```text
//! ┌ Guard spoilers about ──────────────────────────────┐
//! │ From “PL: Arsenal v Chelsea”                        │
//! │ [x] Arsenal   [ ] Chelsea                           │   (teams parsed from A v B / A vs B / A - B)
//! │ Also called   [ Gunners, AFC              ]         │
//! │ Only in       ‹ Sport ›                             │
//! │ Hide until    ‹ I've watched it, at most 48 h ›     │   (24 h after it ends · 3 days · always)
//! │                               [ Save ]  [ Cancel ]  │
//! └─────────────────────────────────────────────────────┘
//! ```
//! Keyboard: ↑/↓ (Tab / Shift+Tab) move between rows, Space / Enter tick a team or step a
//! choice, ←/→ step a choice, Enter in a text field or on Save saves, Esc cancels. Every row is
//! clickable. The terms come from the focused programme only (never from hidden channels);
//! terms are folded by the store.

use eframe::egui::{self, RichText};

use super::dialogs::{buttons, card_area, scrim_guarded, title};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{Action, KeyContext};
use crate::model::{SpoilerMode, SpoilerRule, SpoilerRuleId, SpoilerRuleKind, Timestamp};
use crate::msg::{DataScope, Toast};
use crate::store::Ticket;
use crate::theme::{SpaceToken, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::text_field::text_field;
use crate::ui::widgets::toggle::toggle;

/// What the editor opens on.
#[derive(Debug, Clone, PartialEq)]
pub struct SpoilerRuleSpec {
    /// An existing rule to edit (`None`: a new one).
    pub rule: Option<SpoilerRuleId>,
    /// A new rule's source: the focused programme's title (teams are suggested from it), or
    /// words to start from.
    pub initial: String,
    pub sub_title: Option<String>,
    /// "I'll watch it later" from a programme ending then: a blackout by default.
    pub later: Option<Timestamp>,
}

/// How long the guard holds.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Until {
    /// Until seen, at most `h` hours after each event.
    Watched(u32),
    /// Until then, the live event included.
    Blackout(Timestamp),
    Always,
}

/// The rows (top to bottom).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Row {
    Pick(usize),
    Words,
    Aliases,
    Scope,
    Until,
    Save,
    Cancel,
}

impl Row {
    fn is_text(self) -> bool {
        matches!(self, Row::Words | Row::Aliases)
    }
}

/// The editable fields (ticks, words, aliases).
type Fields = (Vec<(String, bool)>, String, String);

pub struct SpoilerRuleEditor {
    spec: SpoilerRuleSpec,
    loaded: bool,
    existing: Option<SpoilerRule>,
    /// Suggested terms (teams from the title) and whether each is ticked.
    picks: Vec<(String, bool)>,
    /// The topic's own words (no suggestions, or editing).
    words: String,
    aliases: String,
    sport: bool,
    untils: Vec<Until>,
    until: usize,
    cursor: usize,
    pending: Option<Ticket<()>>,
    error: Option<String>,
    /// Enter was pressed (acted on after this frame's text field took its typing).
    submit: bool,
    /// The fields as opened (`is_dirty`).
    opened: Option<Fields>,
}

impl SpoilerRuleEditor {
    pub fn new(spec: SpoilerRuleSpec) -> Self {
        Self {
            spec,
            loaded: false,
            existing: None,
            picks: Vec::new(),
            words: String::new(),
            aliases: String::new(),
            sport: true,
            untils: Vec::new(),
            until: 0,
            cursor: 0,
            pending: None,
            error: None,
            submit: false,
            opened: None,
        }
    }

    /// Fill the fields (first frame: needs the context).
    fn load(&mut self, ctx: &UiCtx<'_>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let now = ctx.now;
        let window = ctx.config.spoiler.default_window_h;
        let later = self.spec.later.map(|stop| super::spoiler_menu::later_until(stop, now));
        self.untils =
            vec![Until::Watched(window), Until::Blackout(later.unwrap_or(now + 24 * 3600)), Until::Blackout(now + 72 * 3600), Until::Always];
        if let Some(id) = self.spec.rule {
            match crate::store::spoiler::rule(ctx.db, id) {
                Ok(r) => {
                    self.words = r.label.clone();
                    let label = r.label.to_lowercase();
                    self.aliases = r.terms.iter().filter(|t| **t != label).cloned().collect::<Vec<_>>().join(", ");
                    self.sport = r.categories.iter().any(|c| c.to_lowercase().starts_with("sport"));
                    self.until = match (r.kind, r.mode) {
                        (SpoilerRuleKind::Blackout, _) => {
                            self.untils[1] = Until::Blackout(r.until_at.unwrap_or(now + 24 * 3600));
                            1
                        }
                        (_, SpoilerMode::Always) => 3,
                        _ => {
                            self.untils[0] = Until::Watched(r.window_h);
                            0
                        }
                    };
                    self.existing = Some(r);
                }
                Err(e) => self.error = Some(format!("Couldn't open the rule: {}", crate::redact::redact(&e.to_string()))),
            }
        } else {
            match crate::textmatch::fixture::parse_fixture(&self.spec.initial, self.spec.sub_title.as_deref()) {
                Some((home, away)) => self.picks = vec![(home, true), (away, false)],
                None => self.words = self.spec.initial.trim().to_owned(),
            }
            if later.is_some() {
                self.until = 1;
            }
        }
        self.opened = Some((self.picks.clone(), self.words.clone(), self.aliases.clone()));
    }

    fn rows(&self) -> Vec<Row> {
        let mut v: Vec<Row> = (0..self.picks.len()).map(Row::Pick).collect();
        if self.picks.is_empty() {
            v.push(Row::Words);
        }
        v.extend([Row::Aliases, Row::Scope, Row::Until, Row::Save, Row::Cancel]);
        v
    }

    fn current(&self) -> Row {
        let rows = self.rows();
        rows[self.cursor.min(rows.len() - 1)]
    }

    fn step(&mut self, row: Row, forward: bool) {
        match row {
            Row::Scope => self.sport = !self.sport,
            Row::Until => {
                let n = self.untils.len();
                self.until = if forward { (self.until + 1) % n } else { (self.until + n - 1) % n };
            }
            Row::Pick(i) => {
                if let Some(p) = self.picks.get_mut(i) {
                    p.1 = !p.1;
                }
            }
            _ => {}
        }
    }

    /// The rule the fields describe, or what's missing.
    fn rule(&self, ctx: &UiCtx<'_>) -> Result<SpoilerRule, String> {
        let mut terms: Vec<String> = self.picks.iter().filter(|p| p.1).map(|p| p.0.trim().to_owned()).collect();
        let words = self.words.trim();
        if !words.is_empty() {
            terms.push(words.to_owned());
        }
        let named = terms.clone();
        terms.extend(self.aliases.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned));
        if named.is_empty() {
            return Err(if self.picks.is_empty() { "Type what to guard (a team, a show…)".into() } else { "Tick at least one".into() });
        }
        let label = named.join(" & ");
        let until = self.untils.get(self.until).copied().unwrap_or(Until::Watched(48));
        let (kind, mode, window_h, until_at) = match until {
            Until::Watched(h) => (SpoilerRuleKind::Topic, SpoilerMode::UntilSeen, h, None),
            Until::Blackout(t) => (SpoilerRuleKind::Blackout, SpoilerMode::UntilSeen, ctx.config.spoiler.default_window_h, Some(t)),
            Until::Always => (SpoilerRuleKind::Topic, SpoilerMode::Always, ctx.config.spoiler.default_window_h, None),
        };
        let base = self.existing.clone();
        Ok(SpoilerRule {
            id: base.as_ref().map_or(SpoilerRuleId(0), |r| r.id),
            kind,
            label,
            terms,
            series_norm: None,
            categories: if self.sport { vec!["Sport".into()] } else { Vec::new() },
            mode,
            window_h,
            until_at,
            sport_descr: base.as_ref().map_or(self.sport, |r| r.sport_descr),
            shield: base.as_ref().is_none_or(|r| r.shield),
            enabled: true,
            by_agent: false,
            agent: Default::default(),
            created_at: base.as_ref().map_or(ctx.now, |r| r.created_at),
        })
    }

    fn save(&mut self, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        if self.pending.is_some() {
            return OverlayOutcome::Keep;
        }
        let r = match self.rule(ctx) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return OverlayOutcome::Keep;
            }
        };
        let now = ctx.now;
        let existing = self.existing.clone();
        self.pending = Some(ctx.services.store.writer().submit_notify(DataScope::Spoiler, move |c| match existing {
            // the kind is fixed once stored: a topic that became a blackout is a new row
            Some(old) if old.kind == r.kind => crate::store::spoiler::update_rule(c, &r),
            Some(old) => {
                crate::store::spoiler::remove_rule(c, old.id)?;
                crate::store::spoiler::add_rule(c, &r, now).map(|_| ())
            }
            None => crate::store::spoiler::add_rule(c, &r, now).map(|_| ()),
        }));
        OverlayOutcome::Keep
    }

    fn until_label(&self, u: Until, ctx: &UiCtx<'_>) -> String {
        let h24 = ctx.config.interface.clock_24h;
        match u {
            Until::Watched(0) => "I've watched it".into(),
            Until::Watched(h) => format!("I've watched it, at most {h} h"),
            Until::Blackout(t) => format!("{} (live too)", crate::ui::widgets::time::day_time(t, h24)),
            Until::Always => "Always".into(),
        }
    }
}

impl Overlay for SpoilerRuleEditor {
    fn key_contexts(&self) -> Vec<KeyContext> {
        if self.loaded && self.current().is_text() { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    fn is_dirty(&self) -> bool {
        self.opened.as_ref().is_some_and(|o| *o != (self.picks.clone(), self.words.clone(), self.aliases.clone()))
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        self.load(ctx);
        let n = self.rows().len();
        let row = self.current();
        match action {
            Action::Back => OverlayOutcome::Close,
            Action::Up | Action::FocusPrev => {
                self.cursor = (self.cursor + n - 1) % n;
                OverlayOutcome::Keep
            }
            Action::Down | Action::FocusNext => {
                self.cursor = (self.cursor + 1) % n;
                OverlayOutcome::Keep
            }
            Action::Left | Action::Right => {
                match row {
                    Row::Save | Row::Cancel => {
                        self.cursor =
                            self.rows().iter().position(|r| *r == if row == Row::Save { Row::Cancel } else { Row::Save }).unwrap_or(self.cursor)
                    }
                    _ => self.step(row, action == Action::Right),
                }
                OverlayOutcome::Keep
            }
            Action::Activate => match row {
                Row::Cancel => OverlayOutcome::Close,
                Row::Save | Row::Words | Row::Aliases => {
                    self.submit = true;
                    OverlayOutcome::Keep
                }
                _ => {
                    self.step(row, true);
                    OverlayOutcome::Keep
                }
            },
            _ => OverlayOutcome::Ignored,
        }
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        self.load(ctx);
        if let Some(r) = self.pending.as_mut().and_then(|t| t.try_take()) {
            self.pending = None;
            match r {
                Ok(()) => {
                    let label = self.rule(ctx).map(|r| r.label).unwrap_or_default();
                    ctx.toast(Toast::info(format!("Guarding spoilers about {label}")));
                    return OverlayOutcome::Close;
                }
                Err(e) => self.error = Some(format!("Couldn't save: {}", crate::redact::redact(&e.to_string()))),
            }
        } else if self.pending.is_some() {
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim_guarded(egui_ctx, theme, "spoiler-rule", self.is_dirty()) {
            return OverlayOutcome::Close;
        }
        let rows = self.rows();
        let cur = self.current();
        let saving = self.pending.is_some();
        let heading = if self.existing.is_some() {
            "Edit spoiler topic"
        } else if self.spec.later.is_some() {
            "I'll watch it later"
        } else {
            "Guard spoilers about"
        };
        let detail = (!self.picks.is_empty() || self.spec.later.is_some()).then(|| format!("From “{}”", self.spec.initial));
        let label_w = (110.0 * theme.rem()).round();
        let mut clicked_row: Option<Row> = None;
        let mut stepped: Option<Row> = None;
        let (cancel, ok) = card_area(egui_ctx, theme, "spoiler-rule", crate::ui::widgets::card::CardKind::Popup, 520.0 * theme.rem(), None, |ui| {
            title(ui, theme, heading, detail.as_deref());
            ui.add_space(theme.space(SpaceToken::Lg));
            let label = |ui: &mut egui::Ui, text: &str| {
                ui.allocate_ui(egui::vec2(label_w, ui.spacing().interact_size.y), |ui| {
                    ui.label(RichText::new(text).size(theme.font(TypeToken::Body)).color(t.text_secondary));
                });
            };
            ui.add_enabled_ui(!saving, |ui| {
                if !self.picks.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
                        for (i, (name, on)) in self.picks.iter_mut().enumerate() {
                            if toggle(ui, theme, on, name, cur == Row::Pick(i)).clicked() {
                                clicked_row = Some(Row::Pick(i));
                            }
                        }
                    });
                    ui.add_space(theme.space(SpaceToken::Md));
                }
                for row in rows.iter().copied().filter(|r| matches!(r, Row::Words | Row::Aliases)) {
                    ui.horizontal(|ui| {
                        let (name, text, hint) = match row {
                            Row::Words => ("About", &mut self.words, "A team, a player, a show…"),
                            _ => ("Also called", &mut self.aliases, "Gunners, AFC (commas between)"),
                        };
                        label(ui, name);
                        let resp = text_field(ui, theme, text, hint, false);
                        if resp.changed() {
                            self.error = None;
                        }
                        if resp.clicked() || resp.gained_focus() {
                            clicked_row = Some(row);
                        }
                        if cur == row && !resp.has_focus() && clicked_row.is_none_or(|r| r == row) {
                            resp.request_focus();
                        } else if cur != row && resp.has_focus() && clicked_row != Some(row) {
                            resp.surrender_focus();
                        }
                    });
                    ui.add_space(theme.space(SpaceToken::Sm));
                }
                let choice = |ui: &mut egui::Ui, name: &str, value: String, focused: bool| -> bool {
                    ui.horizontal(|ui| {
                        label(ui, name);
                        button(ui, theme, None, &format!("‹ {value} ›"), ButtonStyle { focused, bordered: true, ..Default::default() }).clicked()
                    })
                    .inner
                };
                if choice(ui, "Only in", if self.sport { "Sport".into() } else { "Anything".into() }, cur == Row::Scope) {
                    stepped = Some(Row::Scope);
                }
                ui.add_space(theme.space(SpaceToken::Sm));
                let until = self.untils.get(self.until).map(|u| self.until_label(*u, ctx)).unwrap_or_default();
                if choice(ui, "Hide until", until, cur == Row::Until) {
                    stepped = Some(Row::Until);
                }
            });
            if let Some(e) = &self.error {
                ui.add_space(theme.space(SpaceToken::Sm));
                ui.add(egui::Label::new(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(t.urgent)).wrap());
            }
            let focus = match cur {
                Row::Save => Some(true),
                Row::Cancel => Some(false),
                _ => None,
            };
            buttons(ui, theme, if saving { "Saving…" } else { "Save" }, ButtonStyle { primary: true, ..Default::default() }, focus)
        })
        .inner;
        if let Some(r) = clicked_row.or(stepped)
            && let Some(i) = rows.iter().position(|x| *x == r)
        {
            self.cursor = i;
        }
        if let Some(r) = stepped {
            self.step(r, true);
        }
        if std::mem::take(&mut self.submit) || ok {
            return self.save(ctx);
        }
        if cancel { OverlayOutcome::Close } else { OverlayOutcome::Keep }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testing::TestUi;

    /// "Guard spoilers about…" on "PL: Arsenal v Chelsea": the home team is ticked, the other
    /// isn't; Save writes one Sport topic with the aliases (folded by the store).
    #[test]
    fn a_new_topic_from_a_fixture_saves_the_ticked_team_and_aliases() {
        let mut t = TestUi::new().unwrap();
        let spec = SpoilerRuleSpec { rule: None, initial: "PL: Arsenal v Chelsea".into(), sub_title: None, later: None };
        let mut ed = SpoilerRuleEditor::new(spec);
        t.with_ctx(|ctx| {
            ed.on_action(ctx, Action::Down); // Chelsea
            ed.on_action(ctx, Action::Down); // Also called
        });
        assert!(ed.current().is_text());
        assert_eq!(ed.picks.iter().map(|p| (p.0.as_str(), p.1)).collect::<Vec<_>>(), [("Arsenal", true), ("Chelsea", false)]);
        ed.aliases = "Gunners, AFC".into();
        let r = t.with_ctx(|ctx| ed.rule(ctx)).unwrap();
        assert_eq!((r.label.as_str(), r.kind, r.mode), ("Arsenal", SpoilerRuleKind::Topic, SpoilerMode::UntilSeen));
        assert_eq!(r.terms, ["Arsenal", "Gunners", "AFC"]);
        assert_eq!(r.categories, ["Sport"]);
        // "Hide until" → the blackout row
        t.with_ctx(|ctx| {
            ed.on_action(ctx, Action::Down); // Only in
            ed.on_action(ctx, Action::Down); // Hide until
            ed.on_action(ctx, Action::Right);
        });
        let r = t.with_ctx(|ctx| ed.rule(ctx)).unwrap();
        assert_eq!(r.kind, SpoilerRuleKind::Blackout);
        assert!(r.until_at.is_some_and(|u| u > t.now));
        // nothing ticked: nothing to save
        ed.picks[0].1 = false;
        assert!(t.with_ctx(|ctx| ed.rule(ctx)).is_err());
    }

    /// The editor fits at 790 and 1600 pt with 12 and 16 px text: nothing ends past the window.
    #[test]
    fn the_editor_fits_narrow_and_wide_windows_at_both_text_sizes() {
        use egui_kittest::kittest::Queryable;
        let (t, one) = crate::ui::screens::home::testkit::rig();
        let spec = SpoilerRuleSpec {
            rule: None,
            initial: "Premier League: Manchester United v Wolverhampton Wanderers".into(),
            sub_title: None,
            later: Some(t.now),
        };
        let mut h = crate::ui::screens::home::testkit::overlay_harness(t, one, SpoilerRuleEditor::new(spec), egui::vec2(790.0, 900.0));
        for (w, scale) in [(790.0, 1.0), (790.0, 4.0 / 3.0), (1600.0, 1.0), (1600.0, 4.0 / 3.0)] {
            let base = crate::theme::Theme::fallback();
            h.state_mut().t.theme = crate::theme::Theme::from_parts(base.name.clone(), base.palette.clone(), base.shell.clone(), 0.0, scale);
            h.set_size(egui::vec2(w, 900.0));
            h.run_steps(3);
            assert!(crate::ui::screens::home::testkit::has_label(&h, "Save"));
            for node in h.query_all_by_label_contains("") {
                let r = node.rect();
                assert!(r.left() >= -1.0 && r.right() <= w + 1.0, "at {w} pt × {scale}: {node:?}");
            }
        }
    }

    /// "I'll watch it later" opens on the blackout that lifts a day after the programme ends.
    #[test]
    fn later_defaults_to_a_blackout_past_the_programme() {
        let mut t = TestUi::new().unwrap();
        let stop = t.now + 3 * 3600;
        let mut ed = SpoilerRuleEditor::new(SpoilerRuleSpec { rule: None, initial: "Arsenal v Chelsea".into(), sub_title: None, later: Some(stop) });
        let r = t.with_ctx(|ctx| {
            ed.load(ctx);
            ed.rule(ctx)
        });
        let r = r.unwrap();
        assert_eq!((r.kind, r.until_at), (SpoilerRuleKind::Blackout, Some(stop + 24 * 3600)));
    }
}
