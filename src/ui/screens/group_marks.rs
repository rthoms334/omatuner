//! Bulk hide in group panes (DESIGN §28.7), shared by Live TV's groups and the Movies / Series
//! categories. Owner: P6-ui.
//!
//! * Space (empty filter) / Insert marks the focused provider group (virtual rows can't be
//!   marked); Ctrl+H then hides every marked group in one write; Esc clears the marks first.
//! * While the pane is filtered, Ctrl+Shift+H hides the provider groups the filter shows whose
//!   names have a word starting with each word typed ([`word_match`]): the fuzzy filter also
//!   shows "Cartoons" for "ar", which a bulk hide must not take. The confirmation (several
//!   groups) names the first few.
//! * Marks outlive library rebuilds (group ids are stable) but drop groups that are gone.
//! * Hiding goes through `store::user::hide_groups`, which also clears a "show anyway"
//!   exemption from the category filter (DESIGN §28.3).

use eframe::egui;

use crate::keymap::{Action, KeyContext};
use crate::model::{Group, GroupId, GroupRef};
use crate::msg::{DataScope, Toast};
use crate::theme::{Theme, TypeToken};
use crate::ui::overlays::ConfirmSpec;
use crate::ui::{OverlayRequest, UiCtx};

/// Confirm tokens of "Hide these N groups" (other confirmations of the panes use other ranges).
const TOKEN_BASE: u64 = 0x4800_0000;

/// What the pane calls its groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Noun {
    /// Live TV.
    #[default]
    Group,
    /// Movies / Series.
    Category,
}

impl Noun {
    pub(crate) fn of(self, n: usize) -> &'static str {
        match (self, n) {
            (Noun::Group, 1) => "group",
            (Noun::Group, _) => "groups",
            (Noun::Category, 1) => "category",
            (Noun::Category, _) => "categories",
        }
    }
}

/// Lower-case words of `s` (split at anything not a letter or digit).
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Whether every word typed (split at anything not a letter or digit, case-insensitive) starts
/// a word of `haystack`: "ar" takes "AR | Kids" and "Arabic News", never "Cartoons". A query
/// without words matches nothing.
pub(crate) fn word_match(query: &str, haystack: &str) -> bool {
    let (q, hay) = (words(query), words(haystack));
    !q.is_empty() && q.iter().all(|q| hay.iter().any(|w| w.starts_with(q.as_str())))
}

/// Whether `name` already shows `code` as words ("|UK| Sports" has "UK"; "Ukraine" doesn't):
/// the row then needs no tag badge.
pub(crate) fn has_words(name: &str, code: &str) -> bool {
    let (name, code) = (words(name), words(code));
    !code.is_empty() && code.iter().all(|c| name.contains(c))
}

/// The key that does `action` in a group pane, for footers ("Ctrl+H").
fn key(ctx: KeyContext, action: Action) -> String {
    super::live::shortcut(ctx, action).unwrap_or_default()
}

#[derive(Default)]
pub(crate) struct GroupMarks {
    noun: Noun,
    /// Marked groups, in marking order.
    marked: Vec<(GroupId, GroupRef)>,
    /// "Hide these N groups" waiting for its confirmation.
    confirm: Option<(u64, Vec<GroupRef>)>,
    next: u64,
}

/// Hide `groups` in one write; the toast names the group when there is one.
pub(crate) fn hide(ctx: &mut UiCtx<'_>, noun: Noun, groups: Vec<GroupRef>, name: Option<&str>) {
    if groups.is_empty() {
        return;
    }
    let n = groups.len();
    let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::hide_groups(c, &groups));
    ctx.toast(Toast::info(match (n, name) {
        (1, Some(name)) => format!("Hid “{name}” — Settings ▸ Hidden items brings it back"),
        (1, None) => format!("Hid 1 {} — Settings ▸ Hidden items brings it back", noun.of(1)),
        _ => format!("Hid {n} {} · Settings ▸ Hidden items shows them again", noun.of(n)),
    }));
}

impl GroupMarks {
    /// What the pane calls its groups (Live: groups, the default).
    pub(crate) fn set_noun(&mut self, noun: Noun) {
        self.noun = noun;
    }

    pub(crate) fn is_marked(&self, id: GroupId) -> bool {
        self.marked.iter().any(|(g, _)| *g == id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }

    /// Mark / unmark a provider group.
    pub(crate) fn toggle(&mut self, g: &Group) {
        match self.marked.iter().position(|(id, _)| *id == g.id) {
            Some(i) => {
                self.marked.remove(i);
            }
            None => self.marked.push((g.id, GroupRef::from(g))),
        }
    }

    /// Esc: drop the marks; false when there were none (Esc goes on).
    pub(crate) fn clear(&mut self) -> bool {
        !std::mem::take(&mut self.marked).is_empty()
    }

    /// Keep the marks of groups the pane still lists (after a library rebuild).
    pub(crate) fn retain(&mut self, listed: impl Fn(GroupId) -> bool) {
        self.marked.retain(|(id, _)| listed(*id));
    }

    /// Ctrl+H with marks: hide them all. False when nothing is marked.
    pub(crate) fn hide_marked(&mut self, ctx: &mut UiCtx<'_>) -> bool {
        if self.marked.is_empty() {
            return false;
        }
        let groups = std::mem::take(&mut self.marked).into_iter().map(|(_, r)| r).collect();
        hide(ctx, self.noun, groups, None);
        true
    }

    /// Ctrl+Shift+H: hide `shown` — the provider groups the filter shows that [`word_match`]
    /// the query — asking first when there are several (the first few named).
    pub(crate) fn hide_shown(&mut self, ctx: &mut UiCtx<'_>, query: &str, shown: &[&Group]) {
        let noun = self.noun;
        match shown {
            [] => ctx.toast(Toast::info(format!("No playlist {} have a word starting “{}”", noun.of(2), query.trim()))),
            [g] => hide(ctx, noun, vec![GroupRef::from(*g)], Some(g.display_name())),
            _ => {
                self.next += 1;
                let token = TOKEN_BASE + self.next;
                let n = shown.len();
                self.confirm = Some((token, shown.iter().map(|g| GroupRef::from(*g)).collect()));
                const NAMED: usize = 5;
                let mut names = shown.iter().take(NAMED).map(|g| g.display_name()).collect::<Vec<_>>().join(", ");
                if n > NAMED {
                    names.push_str(&format!(" and {} more", n - NAMED));
                }
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Hide {n} {}?", noun.of(n)),
                    body: format!("{names}. Everything in them is hidden too; Settings ▸ Hidden items shows them again."),
                    confirm_label: format!("Hide {n} {}", noun.of(n)),
                    destructive: false,
                    token,
                }));
            }
        }
    }

    /// A confirmation answered; true when it was ours (and the groups were hidden).
    pub(crate) fn confirmed(&mut self, ctx: &mut UiCtx<'_>, token: u64) -> bool {
        match self.confirm.take() {
            Some((t, groups)) if t == token => {
                self.marked.retain(|(_, r)| !groups.contains(r));
                hide(ctx, self.noun, groups, None);
                true
            }
            other => {
                self.confirm = other;
                false
            }
        }
    }

    pub(crate) fn cancelled(&mut self, token: u64) {
        if self.confirm.as_ref().is_some_and(|(t, _)| *t == token) {
            self.confirm = None;
        }
    }

    /// The pane's footer line: the marks, else what Ctrl+Shift+H would hide (`matching`: the
    /// shown groups that [`word_match`] the query).
    pub(crate) fn footer(&self, query: &str, matching: usize) -> Option<String> {
        if !self.marked.is_empty() {
            Some(format!("{} marked · {} hides · Esc clears", self.marked.len(), key(KeyContext::List, Action::ToggleHidden)))
        } else if !query.is_empty() && matching > 0 {
            Some(format!(
                "{matching} {} with “{}” · {} hides {}",
                self.noun.of(matching),
                query.trim(),
                key(KeyContext::ListFiltering, Action::HideShown),
                if matching == 1 { "it" } else { "them" }
            ))
        } else {
            None
        }
    }
}

/// Run `body` above a one-line footer at the bottom of `ui` (none: the whole area).
pub(crate) fn with_footer<R>(ui: &mut egui::Ui, theme: &Theme, footer: Option<&str>, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let Some(text) = footer else { return body(ui) };
    let area = ui.available_rect_before_wrap();
    let h = (theme.font(TypeToken::BodySmall) * 2.0).round();
    let list = egui::Rect::from_min_max(area.min, egui::pos2(area.right(), (area.bottom() - h).max(area.top())));
    let r = ui.scope_builder(egui::UiBuilder::new().max_rect(list), body).inner;
    let line = egui::Rect::from_min_max(egui::pos2(area.left(), list.bottom()), area.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(line).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
        ui.add(egui::Label::new(egui::RichText::new(text).size(theme.font(TypeToken::BodySmall)).color(theme.tokens.text_secondary)).truncate());
    });
    r
}
