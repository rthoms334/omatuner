//! Which playlist a screen shows (DESIGN §29 "Several playlists"): the scope of Movies,
//! Series, Live TV and the guide. Each of Movies, Series and Live TV remembers its own
//! (`interface.playlist_scope`); the guide follows Live TV.
//!
//! * Only playlists that are *eligible* for the screen count ([`Library::eligible`]: enabled
//!   and with movies / series / channels). With fewer than two there is no choice: no bar, no
//!   picker, no marks, and the stored choice is ignored.
//! * "All playlists" is every eligible playlist except those whose "Show in All playlists" is
//!   off (`SourceSettings::in_all`) — unless that would leave nothing. That only narrows the
//!   provider lists (All, categories / groups); the user's own lists (favourites, history,
//!   reminders, continue watching) show every playlist in All ([`ScopeFilter::for_user_list`]).
//! * The choice is applied through `ctx.edit_config` (it lands at the end of the frame); a
//!   playlist that disappears falls back to All with a toast (`TunerApp`, on the library swap).

use eframe::egui::Color32;

use super::{OverlayRequest, ScreenId, UiCtx, data::SourceMarks};
use crate::config::PlaylistScopes;
use crate::keymap::Action;
use crate::model::{ItemKind, SourceColor, SourceId};
use crate::msg::Toast;
use crate::store::library::{Library, SourceMeta};

/// The item kind of a movie or series (playlist marks).
pub fn vod_item_kind(k: crate::model::VodKind) -> ItemKind {
    match k {
        crate::model::VodKind::Movie(_) => ItemKind::Movie,
        crate::model::VodKind::Series(_) => ItemKind::Series,
    }
}

/// A screen with a playlist scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    Movies,
    Series,
    /// Live TV and the guide.
    Live,
}

impl ScopeKind {
    pub const ALL: [ScopeKind; 3] = [ScopeKind::Movies, ScopeKind::Series, ScopeKind::Live];

    /// The scope a screen shows (the guide follows Live TV).
    pub fn of_screen(id: ScreenId) -> Option<Self> {
        match id {
            ScreenId::Movies => Some(ScopeKind::Movies),
            ScreenId::Series => Some(ScopeKind::Series),
            ScreenId::Live | ScreenId::Guide => Some(ScopeKind::Live),
            _ => None,
        }
    }

    pub fn item_kind(self) -> ItemKind {
        match self {
            ScopeKind::Movies => ItemKind::Movie,
            ScopeKind::Series => ItemKind::Series,
            ScopeKind::Live => ItemKind::Channel,
        }
    }

    /// "movies", "series", "channels" (toasts).
    pub fn noun(self) -> &'static str {
        match self {
            ScopeKind::Movies => "movies",
            ScopeKind::Series => "series",
            ScopeKind::Live => "channels",
        }
    }

    pub fn stored(self, s: &PlaylistScopes) -> Option<SourceId> {
        *self.slot_ref(s)
    }

    fn slot_ref(self, s: &PlaylistScopes) -> &Option<SourceId> {
        match self {
            ScopeKind::Movies => &s.movies,
            ScopeKind::Series => &s.series,
            ScopeKind::Live => &s.live,
        }
    }

    pub fn slot(self, s: &mut PlaylistScopes) -> &mut Option<SourceId> {
        match self {
            ScopeKind::Movies => &mut s.movies,
            ScopeKind::Series => &mut s.series,
            ScopeKind::Live => &mut s.live,
        }
    }
}

/// What a scope lets through, as a value (rebuild keys, closures).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ScopeFilter {
    #[default]
    Everything,
    /// One playlist picked.
    Only(SourceId),
    /// All playlists minus the ones whose "Show in All playlists" is off.
    Except(Vec<SourceId>),
}

impl ScopeFilter {
    pub fn admits(&self, source: SourceId) -> bool {
        match self {
            ScopeFilter::Everything => true,
            ScopeFilter::Only(s) => *s == source,
            ScopeFilter::Except(out) => !out.contains(&source),
        }
    }

    /// The filter of the user's own lists (favourites, history, reminders, continue watching):
    /// "Show in All playlists" only keeps a playlist out of All's provider lists, never out of
    /// what the user saved — so in All those lists show every playlist.
    pub fn for_user_list(&self) -> ScopeFilter {
        match self {
            ScopeFilter::Except(_) => ScopeFilter::Everything,
            f => f.clone(),
        }
    }
}

/// The scope in effect on a screen.
#[derive(Debug, Clone)]
pub struct Scope<'a> {
    pub kind: ScopeKind,
    /// Eligible playlists, in playlist order.
    pub eligible: Vec<&'a SourceMeta>,
    /// The playlist picked (`None` = All playlists; always `None` with fewer than two).
    pub selected: Option<&'a SourceMeta>,
}

impl<'a> Scope<'a> {
    pub fn of(lib: &'a Library, scopes: &PlaylistScopes, kind: ScopeKind) -> Self {
        let eligible = lib.eligible(kind.item_kind());
        let selected = if eligible.len() > 1 { kind.stored(scopes).and_then(|id| eligible.iter().copied().find(|s| s.id == id)) } else { None };
        Self { kind, eligible, selected }
    }

    /// The scope of `kind` for this frame.
    pub fn current(ctx: &UiCtx<'a>, kind: ScopeKind) -> Self {
        Self::of(ctx.lib, &ctx.config.interface.playlist_scope, kind)
    }

    /// Two or more eligible playlists: the scope bar, the picker and Auto playlist marks.
    pub fn multi(&self) -> bool {
        self.eligible.len() > 1
    }

    pub fn is_all(&self) -> bool {
        self.selected.is_none()
    }

    pub fn filter(&self) -> ScopeFilter {
        if let Some(s) = self.selected {
            return ScopeFilter::Only(s.id);
        }
        // every playlist switched out of All: All shows them anyway (it never shows nothing);
        // the library's stacks use the same rule
        let out: Vec<SourceId> = crate::store::library::out_of_all_of(&self.eligible);
        if out.is_empty() { ScopeFilter::Everything } else { ScopeFilter::Except(out) }
    }

    /// "TV 2" or "All playlists".
    pub fn label(&self) -> &'a str {
        self.selected.map_or(ALL_PLAYLISTS, |s| s.name.as_str())
    }

    /// The choices in tab / picker order: All, then each eligible playlist.
    pub fn choices(&self) -> Vec<Option<&'a SourceMeta>> {
        std::iter::once(None).chain(self.eligible.iter().copied().map(Some)).collect()
    }

    /// The next (or previous) choice after the current one, wrapping.
    pub fn step(&self, forward: bool) -> Option<SourceId> {
        let choices = self.choices();
        let n = choices.len();
        let at = choices.iter().position(|c| c.map(|s| s.id) == self.selected.map(|s| s.id)).unwrap_or(0);
        let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
        choices[next].map(|s| s.id)
    }

    /// Items of the scope's kind in a choice (`None` = All, i.e. what [`Scope::filter`] lets
    /// through).
    pub fn count(&self, lib: &Library, choice: Option<SourceId>) -> usize {
        let kind = self.kind.item_kind();
        match choice {
            Some(id) => lib.count(kind, id),
            None => {
                let all = Scope { selected: None, ..self.clone() }.filter();
                self.eligible.iter().filter(|s| all.admits(s.id)).map(|s| lib.count(kind, s.id)).sum()
            }
        }
    }

    /// The playlist marks of a list in this scope (Settings ▸ Interface ▸ Playlist on cards:
    /// Auto shows them only in All with two or more playlists).
    pub fn marks(&self, ctx: &UiCtx<'a>) -> SourceMarks<'a> {
        SourceMarks::for_view(ctx.config.interface.playlist_on_cards, &ctx.lib.sources, self.kind.item_kind(), self.is_all())
    }
}

pub const ALL_PLAYLISTS: &str = "All playlists";

/// Show `choice` on the screens of `kind` (saved at the end of the frame). `toast`: say so
/// (keyboard cycling, where nothing else points at the change).
pub fn choose(ctx: &mut UiCtx<'_>, kind: ScopeKind, choice: Option<SourceId>, toast: bool) {
    ctx.edit_config(move |c| *kind.slot(&mut c.interface.playlist_scope) = choice);
    if toast {
        let text = match choice.and_then(|id| ctx.lib.source(id)) {
            Some(s) => format!("Showing {} only", s.name),
            None => "Showing all playlists".to_owned(),
        };
        ctx.toast(Toast::info(text));
    }
}

/// Alt+P (picker), Ctrl+PgDn / Ctrl+PgUp (next / previous playlist) on a screen of `kind`.
/// Returns false for other actions.
pub fn on_action(ctx: &mut UiCtx<'_>, kind: ScopeKind, action: Action) -> bool {
    if !matches!(action, Action::PlaylistPicker | Action::NextPlaylist | Action::PrevPlaylist) {
        return false;
    }
    let scope = Scope::current(ctx, kind);
    if !scope.multi() {
        let n = if scope.eligible.is_empty() { "No playlist has" } else { "Only one playlist has" };
        ctx.toast(Toast::info(format!("{n} {}", kind.noun())));
        return true;
    }
    match action {
        Action::PlaylistPicker => ctx.open(OverlayRequest::PlaylistPicker(kind)),
        _ => {
            let next = scope.step(action == Action::NextPlaylist);
            choose(ctx, kind, next, true);
        }
    }
    true
}

/// The colours a new playlist may take, in order (the source editor's order).
pub const COLOR_ORDER: [SourceColor; 10] = [
    SourceColor::Accent,
    SourceColor::Red,
    SourceColor::Yellow,
    SourceColor::Orange,
    SourceColor::Green,
    SourceColor::Cyan,
    SourceColor::Blue,
    SourceColor::Magenta,
    SourceColor::Brown,
    SourceColor::Muted,
];

/// The first colour no playlist uses (compared as the theme draws them: Accent is often the
/// theme's blue). Accent when every colour is taken.
pub fn first_free_color(used: &[SourceColor], resolve: impl Fn(SourceColor) -> Color32) -> SourceColor {
    let taken: Vec<Color32> = used.iter().map(|c| resolve(*c)).collect();
    COLOR_ORDER.into_iter().find(|c| !taken.contains(&resolve(*c))).unwrap_or_default()
}

/// Playlists that look the same — same glyph and same colour as drawn (DESIGN §29 "telling
/// playlists apart"): groups of indices into `looks`, each in input order.
pub fn look_alikes(looks: &[(&str, SourceColor)], resolve: impl Fn(SourceColor) -> Color32) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, (glyph, color)) in looks.iter().enumerate() {
        let same = |j: &usize| looks[*j].0 == *glyph && resolve(looks[*j].1) == resolve(*color);
        match groups.iter_mut().find(|g| g.first().is_some_and(same)) {
            Some(g) => g.push(i),
            None => groups.push(vec![i]),
        }
    }
    groups.retain(|g| g.len() > 1);
    groups
}

/// "Give each its own colour": new colours for every look-alike but the first of each group,
/// each the first colour nobody uses yet. (index, colour) pairs to save.
pub fn distinct_colors(looks: &[(&str, SourceColor)], resolve: impl Fn(SourceColor) -> Color32 + Copy) -> Vec<(usize, SourceColor)> {
    let mut used: Vec<SourceColor> = looks.iter().map(|l| l.1).collect();
    let mut out = Vec::new();
    for g in look_alikes(looks, resolve) {
        for &i in &g[1..] {
            let c = first_free_color(&used, resolve);
            used.push(c);
            out.push((i, c));
        }
    }
    out
}

/// "TV and TV 2", "A, B and C".
pub fn names_and(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceKind;

    fn meta(id: i64, name: &str, enabled: bool, vod: bool, live: bool, in_all: bool) -> SourceMeta {
        SourceMeta {
            id: SourceId(id),
            name: name.into(),
            icon: String::new(),
            color: SourceColor::Accent,
            kind: SourceKind::Xtream,
            enabled,
            last_refresh: None,
            has_vod: vod,
            has_series: false,
            has_live: live,
            in_all,
            category_filter: None,
        }
    }

    /// Eligible = enabled AND has that kind of item: a disabled playlist and a free-channel
    /// preset (channels only) don't make Movies a two-playlist screen; a stored choice that
    /// isn't eligible (or with one playlist) reads as All; "Show in All playlists" off keeps a
    /// playlist out of All but not out of its own tab, and never empties All.
    #[test]
    fn eligibility_choice_and_the_all_filter() {
        let lib = Library {
            sources: vec![
                meta(1, "TV", true, true, true, true),
                meta(2, "TV 2", true, true, true, true),
                meta(3, "Old", false, true, true, true),
                meta(4, "Free-TV", true, false, true, false),
            ],
            ..Default::default()
        };
        let names = |k: ItemKind| lib.eligible(k).iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(ItemKind::Movie), ["TV", "TV 2"]);
        assert_eq!(names(ItemKind::Channel), ["TV", "TV 2", "Free-TV"]);
        assert!(names(ItemKind::Series).is_empty());

        let pick = |kind: ScopeKind, id: Option<i64>| {
            let mut s = PlaylistScopes::default();
            *kind.slot(&mut s) = id.map(SourceId);
            s
        };
        let movies = Scope::of(&lib, &pick(ScopeKind::Movies, Some(2)), ScopeKind::Movies);
        assert_eq!(movies.label(), "TV 2");
        assert_eq!(movies.filter(), ScopeFilter::Only(SourceId(2)));
        assert!(Scope::of(&lib, &pick(ScopeKind::Movies, Some(3)), ScopeKind::Movies).is_all(), "disabled: All");
        assert!(Scope::of(&lib, &pick(ScopeKind::Movies, Some(4)), ScopeKind::Movies).is_all(), "no movies: All");
        // each screen keeps its own choice
        assert!(Scope::of(&lib, &pick(ScopeKind::Movies, Some(2)), ScopeKind::Live).is_all());

        let live = Scope::of(&lib, &PlaylistScopes::default(), ScopeKind::Live);
        let all = live.filter();
        assert!(all.admits(SourceId(1)) && !all.admits(SourceId(4)), "Free-TV is out of All");
        assert!(all.for_user_list().admits(SourceId(4)), "but not out of your favourites");
        assert_eq!(ScopeFilter::Only(SourceId(2)).for_user_list(), ScopeFilter::Only(SourceId(2)));
        assert_eq!(Scope::of(&lib, &pick(ScopeKind::Live, Some(4)), ScopeKind::Live).filter(), ScopeFilter::Only(SourceId(4)));
        // Ctrl+PgDn walks All → TV → TV 2 → Free-TV → All
        assert_eq!(live.step(true), Some(SourceId(1)));
        assert_eq!(live.step(false), Some(SourceId(4)));
        assert_eq!(Scope::of(&lib, &pick(ScopeKind::Live, Some(4)), ScopeKind::Live).step(true), None);

        let only_out =
            Library { sources: vec![meta(1, "A", true, false, true, false), meta(2, "B", true, false, true, false)], ..Default::default() };
        assert_eq!(Scope::of(&only_out, &PlaylistScopes::default(), ScopeKind::Live).filter(), ScopeFilter::Everything, "never empty");
    }

    /// Two Xtream playlists with the default glyph and Accent look the same; Accent and the
    /// theme's identical blue do too. The fix keeps the first and gives the others the first
    /// colours nobody uses; a new playlist gets the first free colour.
    #[test]
    fn look_alikes_and_free_colours() {
        let resolve = |c: SourceColor| match c {
            SourceColor::Accent | SourceColor::Blue => Color32::BLUE,
            SourceColor::Red => Color32::RED,
            other => Color32::from_gray(COLOR_ORDER.iter().position(|x| *x == other).unwrap_or(0) as u8),
        };
        let looks = [("X", SourceColor::Accent), ("X", SourceColor::Blue), ("M", SourceColor::Accent), ("X", SourceColor::Accent)];
        assert_eq!(look_alikes(&looks, resolve), vec![vec![0, 1, 3]]);
        assert_eq!(distinct_colors(&looks, resolve), vec![(1, SourceColor::Red), (3, SourceColor::Yellow)]);
        assert_eq!(first_free_color(&[SourceColor::Accent], resolve), SourceColor::Red);
        assert_eq!(first_free_color(&[], resolve), SourceColor::Accent);
        assert_eq!(first_free_color(&COLOR_ORDER, resolve), SourceColor::Accent, "all taken");
        assert_eq!(names_and(&["TV", "TV 2", "Free"]), "TV, TV 2 and Free");
    }
}
