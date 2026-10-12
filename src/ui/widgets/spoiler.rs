//! What the painters show of programme and episode text under the spoiler guard (DESIGN §34,
//! B4). OWNER: S-ui. base-B: the API every painter call site goes through, with identity
//! rendering until `spoiler::ENABLED` — S-ui then changes this module (bars, the "Description
//! hidden" block, the dim glyph of a rewritten title, obscured images) and the screens' subjects,
//! not the painters.
//!
//! Call sites (base-B): the programme popover (sub-title, description), the guide's hover
//! tooltip and its description load, Live's details pane, the series page (plot, episode
//! titles and plots), the movie page (plot).

use std::borrow::Cow;

use crate::guard::{Field, Guard, Guarded, Policy, Subject};
use crate::model::{Episode, Programme, Timestamp};
use crate::ui::UiCtx;

/// The guard as a frame's painters see it (cheap: an `Arc` clone).
#[derive(Clone, Default)]
pub struct Spoilers {
    /// `None`: the feature is off — everything shows as is.
    guard: Option<Guard>,
    now: Timestamp,
}

impl Spoilers {
    /// This frame's view.
    pub fn of(ctx: &UiCtx<'_>) -> Self {
        Self { guard: crate::spoiler::ENABLED.then(|| ctx.services.agent.guard()), now: ctx.now }
    }

    /// What to show of `field` of `subject` (the S-ui `RevealSet` decides `Policy::Reveal`).
    pub fn guarded<'a>(&self, subject: &Subject<'_>, field: Field, text: &'a str) -> Guarded<'a> {
        match &self.guard {
            Some(g) => g.text(subject, field, text, Policy::Normal, self.now),
            None => Guarded::Shown(Cow::Borrowed(text)),
        }
    }

    /// The text to paint, `None` when hidden (a painter then shows nothing, or S-ui's bar).
    pub fn text<'a>(&self, subject: &Subject<'_>, field: Field, text: &'a str) -> Option<Cow<'a, str>> {
        match self.guarded(subject, field, text) {
            Guarded::Shown(t) | Guarded::Rewritten(t) => Some(t),
            Guarded::Hidden { .. } => None,
        }
    }

    /// An episode as its row and page may show it (title / plot cleared when guarded; borrowed
    /// when nothing is). `series_norm`: `spoiler::series_key` of the series' name.
    pub fn episode<'e>(&self, series_norm: &str, e: &'e Episode) -> Cow<'e, Episode> {
        if self.guard.is_none() {
            return Cow::Borrowed(e);
        }
        let subj = Subject::Episode { series_norm, season: e.season as i32, episode: e.episode as i32 };
        let title = self.text(&subj, Field::EpisodeTitle, &e.title).map(Cow::into_owned);
        let plot = e.plot.as_deref().map(|p| self.text(&subj, Field::Plot, p).map(Cow::into_owned));
        if title.as_deref() == Some(e.title.as_str()) && plot.as_ref().map(|p| p.as_deref()) == e.plot.as_deref().map(Some) {
            return Cow::Borrowed(e);
        }
        Cow::Owned(Episode { title: title.unwrap_or_default(), plot: plot.flatten(), ..e.clone() })
    }
}

/// The subject of a full guide programme.
pub fn programme_subject(p: &Programme) -> Subject<'_> {
    Subject::Programme {
        title: &p.title,
        sub_title: p.sub_title.as_deref(),
        categories: &p.categories,
        season_episode: crate::spoiler::programme_se(p),
        start: p.start,
    }
}

/// The subject of a grid programme (no categories or S/E in the slim row).
pub fn slim_subject(p: &crate::model::ProgrammeSlim) -> Subject<'_> {
    Subject::Programme { title: &p.title, sub_title: p.sub_title.as_deref(), categories: &[], season_episode: None, start: p.start }
}
