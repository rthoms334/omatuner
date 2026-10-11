//! `?` / F1 help overlay, GENERATED from the keymap tables (never hand-written). Owner: WP-10a.
//!
//! Shows the contexts relevant to where the user is first (`with_contexts`, passed by the app),
//! then all others, `Global` last; every context of `KeyContext::ALL` appears, so the overlay is
//! complete by construction. Two columns from 760 pt of card width.
//!
//! Type to filter, like Omarchy's keybindings menu: the `Help` key context turns every printable
//! key into `FilterInput`, so typing narrows the list at once and no letter shortcut can fire.
//! A row matches when every whitespace-separated term of the filter is found (case-insensitive
//! substring) in its keys ("ctrl+h", "f5", "alt"; arrows also answer to "up"/"down"…), its
//! description or its section's title ("guide" keeps the whole TV guide section). Results stay
//! grouped by section in the same order (current contexts first); empty sections are hidden and
//! matches are highlighted. Keys: ↑/↓ scroll, PgUp/PgDn page, Home/End top/bottom, Backspace /
//! Ctrl+Backspace / Ctrl+U edit the filter, Esc and F1 clear it and then close, `?` closes only
//! while the filter is empty (otherwise it is typed), Enter closes.

use eframe::egui::{self, RichText, text::LayoutJob};

use super::dialogs::{card_area, scrim};
use super::{Overlay, OverlayOutcome};
use crate::keymap::{self, Action, KeyContext};
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::UiCtx;
use crate::ui::widgets::card::CardKind;
use crate::ui::widgets::glyphs;
use crate::ui::widgets::text_field::search_header;

/// Card width from which the contexts flow into two columns.
const TWO_COLUMNS_FROM: f32 = 760.0;

/// Search-only words of key labels that have no letters to type.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("↑", "up arrow"),
    ("↓", "down arrow"),
    ("←", "left arrow"),
    ("→", "right arrow"),
    ("Esc", "escape"),
    ("Enter", "return"),
    ("PgUp", "page up"),
    ("PgDn", "page down"),
    ("Comma", ","),
];

/// One context's rows: (merged key labels, description).
type Section = (KeyContext, Vec<(String, &'static str)>);

pub struct HelpOverlay {
    first: Vec<KeyContext>,
    query: String,
    scroll: f32,
    /// Viewport height and maximum offset of the last frame (page step, clamping).
    viewport: f32,
    max_scroll: f32,
    /// Card top edge, frozen once typing starts (the card shrinks and grows downward only).
    top: Option<f32>,
    last_top: Option<f32>,
}

impl HelpOverlay {
    pub fn new() -> Self {
        Self::with_contexts(Vec::new())
    }
    /// Put the given contexts (current screen's stack) at the top.
    pub fn with_contexts(contexts: Vec<KeyContext>) -> Self {
        Self { first: contexts, query: String::new(), scroll: 0.0, viewport: 0.0, max_scroll: f32::INFINITY, top: None, last_top: None }
    }

    /// Every context exactly once: the current ones first (innermost first), then the rest in
    /// documentation order, `Global` last.
    fn ordered(&self) -> Vec<KeyContext> {
        let mut v: Vec<KeyContext> = Vec::with_capacity(KeyContext::ALL.len());
        let rest = KeyContext::ALL.into_iter().filter(|c| *c != KeyContext::Global).chain([KeyContext::Global]);
        for c in self.first.iter().copied().chain(rest) {
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    /// The sections to show (in order, empty ones dropped) and the total row count.
    fn sections(&self) -> (Vec<Section>, usize) {
        let terms = terms(&self.query);
        let mut total = 0;
        let sections = self
            .ordered()
            .into_iter()
            .filter_map(|ctx| {
                let rows = keymap::help_rows(ctx);
                total += rows.len();
                let title = fold(ctx.title());
                let rows: Vec<_> = rows.into_iter().filter(|(keys, help)| row_matches(&terms, &title, keys, help)).collect();
                (!rows.is_empty()).then_some((ctx, rows))
            })
            .collect();
        (sections, total)
    }

    fn edit(&mut self, f: impl FnOnce(&mut String)) {
        f(&mut self.query);
        self.scroll = 0.0;
        if self.query.is_empty() {
            self.top = None; // re-centre (a resize or text-scale change may have moved it)
        } else if self.top.is_none() {
            self.top = self.last_top;
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll = (self.scroll + dy).clamp(0.0, self.max_scroll.max(0.0));
    }
}

impl Default for HelpOverlay {
    fn default() -> Self {
        Self::new()
    }
}

/// Case folding that keeps one char per char (highlight indices stay valid).
fn fold(s: &str) -> Vec<char> {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// The filter's terms, folded; every one must match.
fn terms(query: &str) -> Vec<Vec<char>> {
    query.split_whitespace().map(fold).collect()
}

fn contains(hay: &[char], term: &[char]) -> bool {
    hay.windows(term.len()).any(|w| w == term)
}

fn row_matches(terms: &[Vec<char>], title: &[char], keys: &str, help: &str) -> bool {
    if terms.is_empty() {
        return true;
    }
    let aliases: String = KEY_ALIASES.iter().filter(|(k, _)| keys.contains(k)).map(|(_, a)| format!(" {a}")).collect();
    let (keys, aliases, help) = (fold(keys), fold(&aliases), fold(help));
    terms.iter().all(|t| [&keys, &aliases, &help].into_iter().any(|h| contains(h, t)) || contains(title, t))
}

/// `text` with every occurrence of a term on a text-selection background.
fn highlighted(text: &str, terms: &[Vec<char>], theme: &Theme, size: f32, color: egui::Color32) -> LayoutJob {
    let hay = fold(text);
    let mut hit = vec![false; hay.len()];
    for t in terms.iter().filter(|t| !t.is_empty()) {
        for start in 0..hay.len().saturating_sub(t.len() - 1) {
            if hay[start..start + t.len()] == t[..] {
                hit[start..start + t.len()].fill(true);
            }
        }
    }
    let plain = egui::TextFormat::simple(egui::FontId::proportional(size), color);
    let marked = egui::TextFormat { background: theme.tokens.fill_text_selection, ..plain.clone() };
    let mut job = LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let on = hit[i];
        let end = (i..chars.len()).find(|&j| hit[j] != on).unwrap_or(chars.len());
        job.append(&chars[i..end].iter().collect::<String>(), 0.0, if on { marked.clone() } else { plain.clone() });
        i = end;
    }
    job
}

/// Split contexts into `cols` reading-order columns of roughly equal row counts.
fn split_columns(contexts: &[Section], cols: usize) -> Vec<Vec<usize>> {
    let total: usize = contexts.iter().map(|(_, r)| r.len() + 2).sum();
    let per = total.div_ceil(cols.max(1));
    let mut out = vec![Vec::new(); cols.max(1)];
    let (mut col, mut acc) = (0, 0);
    for (i, (_, rows)) in contexts.iter().enumerate() {
        let n = rows.len() + 2;
        // next column once this section would sit mostly past the column's share
        if acc > 0 && acc + n / 2 >= per && col + 1 < out.len() {
            col += 1;
            acc = 0;
        }
        out[col].push(i);
        acc += n;
    }
    out
}

fn context_block(ui: &mut egui::Ui, theme: &Theme, (ctx, rows): &Section, terms: &[Vec<char>], keys_w: f32) {
    let t = &theme.tokens;
    let body = theme.font(TypeToken::Body);
    ui.add_space(theme.space(SpaceToken::Lg));
    // the section header's look (caption, uppercase, text_section), with the matches marked
    ui.add_space((theme.font(TypeToken::Caption) * 0.15).ceil());
    ui.label(highlighted(&ctx.title().to_uppercase(), terms, theme, theme.font(TypeToken::Caption), t.text_section));
    egui::Grid::new(("help-grid", format!("{ctx:?}")))
        .num_columns(2)
        .min_col_width(keys_w)
        .spacing([theme.space(SpaceToken::Lg), theme.space(SpaceToken::Xs)])
        .show(ui, |ui| {
            for (keys, help) in rows {
                ui.label(highlighted(keys, terms, theme, body, t.accent));
                ui.add(egui::Label::new(highlighted(help, terms, theme, body, t.fg)).wrap());
                ui.end_row();
            }
        });
}

impl Overlay for HelpOverlay {
    fn key_contexts(&self) -> Vec<KeyContext> {
        vec![KeyContext::Help]
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> OverlayOutcome {
        let line = ctx.theme.font(TypeToken::Body) * 3.0;
        let page = (self.viewport * 0.85).max(line);
        match action {
            Action::FilterInput('?') if self.query.is_empty() => return OverlayOutcome::Close,
            Action::FilterInput(c) if self.query.is_empty() && c.is_whitespace() => {}
            // any whitespace (AltGr+Space types NBSP) becomes a plain space: word deletion cuts at ASCII spaces
            Action::FilterInput(c) => self.edit(|q| q.push(if c.is_whitespace() { ' ' } else { c })),
            Action::FilterBackspace => self.edit(|q| {
                q.pop();
            }),
            Action::FilterDeleteWord => self.edit(|q| {
                let cut = q.trim_end().rfind(' ').map_or(0, |i| i + 1);
                q.truncate(cut);
            }),
            // Esc ladder: clear the filter, then close (F1 too: it can't be typed)
            Action::FilterClear => self.edit(String::clear),
            Action::Back | Action::ToggleHelp if !self.query.is_empty() => self.edit(String::clear),
            Action::Back | Action::ToggleHelp | Action::Activate => return OverlayOutcome::Close,
            Action::Down => self.scroll_by(line),
            Action::Up => self.scroll_by(-line),
            Action::PageDown => self.scroll_by(page),
            Action::PageUp => self.scroll_by(-page),
            Action::Home => self.scroll = 0.0,
            Action::End => self.scroll_by(f32::INFINITY),
            _ => return OverlayOutcome::Ignored,
        }
        OverlayOutcome::Keep
    }

    fn ui(&mut self, egui_ctx: &egui::Context, ctx: &mut UiCtx<'_>) -> OverlayOutcome {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if scrim(egui_ctx, theme, "help") {
            return OverlayOutcome::Close;
        }
        let screen = egui_ctx.content_rect();
        let (contexts, total) = self.sections();
        let terms = terms(&self.query);
        let filtering = !terms.is_empty();
        let matched: usize = contexts.iter().map(|(_, r)| r.len()).sum();
        let small = |s: String| RichText::new(s).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary);
        let r = card_area(egui_ctx, theme, "help", CardKind::Menu, 1040.0 * theme.rem(), self.top, |ui| {
            search_header(ui, theme, &self.query, "Type to filter keyboard shortcuts", Some(glyphs::SEARCH));
            ui.add_space(theme.space(SpaceToken::Xs));
            ui.horizontal(|ui| {
                let count = if filtering { format!("{matched} of {total} shortcuts") } else { format!("{total} shortcuts") };
                ui.label(small(count));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(small(format!("↑/↓ scroll · Esc {}", if self.query.is_empty() { "close" } else { "clear" })));
                });
            });
            if contexts.is_empty() {
                ui.add_space(theme.space(SpaceToken::Lg));
                ui.label(RichText::new("No matching shortcuts").size(theme.font(TypeToken::Title)).color(t.text_secondary));
                return None;
            }
            let width = ui.available_width();
            let cols = if width >= TWO_COLUMNS_FROM { 2 } else { 1 };
            let keys_w = (150.0 * theme.rem()).min(width / cols as f32 * 0.4);
            let out = egui::ScrollArea::vertical()
                .max_height(screen.height() * 0.8)
                .vertical_scroll_offset(self.scroll)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let split = split_columns(&contexts, cols);
                    ui.columns(cols, |columns| {
                        for (col, idx) in columns.iter_mut().zip(&split) {
                            for &i in idx {
                                context_block(col, theme, &contexts[i], &terms, keys_w);
                            }
                        }
                    });
                });
            Some(out)
        });
        self.last_top = Some(r.response.rect.top());
        match r.inner {
            Some(out) => {
                self.viewport = out.inner_rect.height();
                self.max_scroll = (out.content_size.y - out.inner_rect.height()).max(0.0);
                // follow wheel scrolling too
                self.scroll = out.state.offset.y.min(self.max_scroll);
            }
            None => (self.scroll, self.max_scroll) = (0.0, 0.0),
        }
        OverlayOutcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_context_once_current_first_global_last() {
        let h = HelpOverlay::with_contexts(vec![KeyContext::PlayerOsd, KeyContext::PlayerLive, KeyContext::PlayerOsd]);
        let v = h.ordered();
        assert_eq!(v.len(), KeyContext::ALL.len());
        assert_eq!(&v[..2], &[KeyContext::PlayerOsd, KeyContext::PlayerLive]);
        assert_eq!(*v.last().unwrap(), KeyContext::Global);
        for c in KeyContext::ALL {
            assert!(v.contains(&c), "{c:?} missing");
        }
    }

    #[test]
    fn columns_keep_reading_order_and_balance() {
        let contexts: Vec<_> = KeyContext::ALL.into_iter().map(|c| (c, keymap::help_rows(c))).collect();
        let split = split_columns(&contexts, 2);
        assert_eq!(split.len(), 2);
        let flat: Vec<usize> = split.concat();
        assert_eq!(flat, (0..contexts.len()).collect::<Vec<_>>());
        let rows = |col: &Vec<usize>| col.iter().map(|&i| contexts[i].1.len() + 2).sum::<usize>();
        let (a, b) = (rows(&split[0]), rows(&split[1]));
        assert!(a.abs_diff(b) <= 30, "{a} vs {b}");
        assert_eq!(split_columns(&contexts, 1)[0].len(), contexts.len());
    }

    fn filtered(first: Vec<KeyContext>, query: &str) -> Vec<Section> {
        let mut h = HelpOverlay::with_contexts(first);
        h.query = query.into();
        h.sections().0
    }

    /// The filter matches key combos, descriptions and section titles, case-insensitively,
    /// every term must match, and sections without a match disappear.
    #[test]
    fn filter_matches_keys_descriptions_and_sections() {
        let (all, total) = HelpOverlay::new().sections();
        assert_eq!(all.len(), KeyContext::ALL.len());
        assert_eq!(all.iter().map(|(_, r)| r.len()).sum::<usize>(), total);

        // key combo
        let hide = filtered(vec![], "ctrl+h");
        // (the bulk-hide "Mark group (Ctrl+H then hides all marked)" rows match by description)
        assert!(hide.iter().all(|(_, rows)| rows.iter().all(|(k, h)| k.contains("Ctrl+H") || h.contains("Ctrl+H"))));
        assert!(hide.iter().any(|(c, rows)| *c == KeyContext::List && rows.iter().any(|(k, _)| k.contains("Space"))));
        assert!(hide.iter().any(|(c, rows)| *c == KeyContext::List && rows.iter().any(|(_, h)| *h == "Hide / unhide")));
        assert!(filtered(vec![], "f5").iter().any(|(c, rows)| *c == KeyContext::Global && rows[0].1 == "Refresh playlists & guide"));
        assert!(filtered(vec![], "down").iter().any(|(c, rows)| *c == KeyContext::Guide && rows.iter().any(|(k, _)| k.contains('↓'))));

        // description, in the order of the current contexts
        let mute = filtered(vec![KeyContext::PlayerVod], "MUTE");
        assert_eq!(mute.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [KeyContext::PlayerVod, KeyContext::PlayerLive]);
        assert!(mute.iter().all(|(_, rows)| rows == &[("m".to_string(), "Mute")]));

        // section title keeps the whole section; another term narrows it
        let guide = filtered(vec![], "tv guide");
        let rows = |s: &[Section], c| s.iter().find(|(x, _)| *x == c).map(|(_, r)| r.len());
        assert_eq!(rows(&guide, KeyContext::Guide), Some(keymap::help_rows(KeyContext::Guide).len()));
        assert!(rows(&guide, KeyContext::List).is_some(), "“Show in TV guide” rows elsewhere");
        let day = filtered(vec![], "guide  DAY");
        assert_eq!(day.iter().find(|(c, _)| *c == KeyContext::Guide).unwrap().1, [("[ / ]".to_string(), "Previous / next day")]);

        assert!(filtered(vec![], "zzqx").is_empty());
    }
}
