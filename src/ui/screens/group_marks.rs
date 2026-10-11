//! Bulk hide in group panes (DESIGN §28.7), shared by Live TV's groups and the Movies / Series
//! categories. Owner: P6-ui.
//!
//! * Space (empty filter) / Insert marks the focused provider group (virtual rows can't be
//!   marked); Ctrl+H then hides every marked group in one write; Esc clears the marks first.
//! * While the pane is filtered, Ctrl+Shift+H hides the provider groups the filter shows,
//!   after a confirmation when there are several.
//! * Marks outlive library rebuilds (group ids are stable) but drop groups that are gone.
//! * Hiding goes through `store::user::hide_groups`, which also clears a "show anyway"
//!   exemption from the category filter (DESIGN §28.3).

use eframe::egui;

use crate::model::{Group, GroupId, GroupRef};
use crate::msg::{DataScope, Toast};
use crate::theme::{Theme, TypeToken};
use crate::ui::overlays::ConfirmSpec;
use crate::ui::{OverlayRequest, UiCtx};

/// Confirm tokens of "Hide these N groups" (other confirmations of the panes use other ranges).
const TOKEN_BASE: u64 = 0x4800_0000;

#[derive(Default)]
pub(crate) struct GroupMarks {
    /// Marked groups, in marking order.
    marked: Vec<(GroupId, GroupRef)>,
    /// "Hide these N groups" waiting for its confirmation.
    confirm: Option<(u64, Vec<GroupRef>)>,
    next: u64,
}

/// Hide `groups` in one write; the toast names the group when there is one.
pub(crate) fn hide(ctx: &mut UiCtx<'_>, groups: Vec<GroupRef>, name: Option<&str>) {
    if groups.is_empty() {
        return;
    }
    let n = groups.len();
    let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::user::hide_groups(c, &groups));
    ctx.toast(Toast::info(match (n, name) {
        (1, Some(name)) => format!("Hid “{name}” — Settings ▸ Hidden items brings it back"),
        (1, None) => "Hid 1 group — Settings ▸ Hidden items brings it back".to_owned(),
        _ => format!("Hid {n} groups · Settings ▸ Hidden items shows them again"),
    }));
}

impl GroupMarks {
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
        hide(ctx, groups, None);
        true
    }

    /// Ctrl+Shift+H: hide the provider groups the filter shows (`shown`), asking first when
    /// there are several.
    pub(crate) fn hide_shown(&mut self, ctx: &mut UiCtx<'_>, shown: &[Group]) {
        match shown {
            [] => ctx.toast(Toast::info("No playlist groups match the filter")),
            [g] => hide(ctx, vec![GroupRef::from(g)], Some(g.display_name())),
            _ => {
                self.next += 1;
                let token = TOKEN_BASE + self.next;
                let n = shown.len();
                self.confirm = Some((token, shown.iter().map(GroupRef::from).collect()));
                ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                    title: format!("Hide {n} groups?"),
                    body: "Everything in them is hidden too. Settings ▸ Hidden items shows them again.".into(),
                    confirm_label: format!("Hide {n} groups"),
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
                hide(ctx, groups, None);
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

    /// The pane's footer line: the marks, else what Ctrl+Shift+H would hide.
    pub(crate) fn footer(&self, filtering: bool, shown_groups: usize) -> Option<String> {
        if !self.marked.is_empty() {
            Some(format!("{} marked · Ctrl+H hides · Esc clears", self.marked.len()))
        } else if filtering && shown_groups > 0 {
            Some(format!(
                "{shown_groups} {} · Ctrl+Shift+H hides {}",
                if shown_groups == 1 { "group" } else { "groups" },
                if shown_groups == 1 { "it" } else { "them" }
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
