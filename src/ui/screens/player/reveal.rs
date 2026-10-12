//! The spoiler guard in the player (DESIGN §34.3, §34.5): what the banner says of the
//! programme (its title and sub-title through the guard, like the guide's), Ctrl+B on it, and
//! the banner's "Ctrl+B show spoilers" hint. OWNER: S-ui.
//!
//! Ctrl+B shows the hidden text of the banner's programmes (now and next, or the catch-up
//! airing) until the picture changes — a zap, another channel — then hides it again, like a
//! list item's reveal when focus moves on. Ctrl+Shift+B opens the guard's menu about the
//! channel's programme.

use std::borrow::Cow;

use super::*;
use crate::guard::{Field, Subject};
use crate::model::{ProgrammeSlim, Timestamp};
use crate::spoiler::ui::RevealKey;
use crate::ui::item_menu::Target;
use crate::ui::widgets::spoiler::Spoilers;

/// A programme the banner shows (owned: the now / next rows, or the catch-up airing).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Shown {
    title: String,
    sub_title: Option<String>,
    season_episode: Option<(i32, i32)>,
    start: Timestamp,
    is_new: bool,
}

impl Shown {
    pub(super) fn of(p: &ProgrammeSlim) -> Self {
        Self {
            title: p.title.to_string(),
            sub_title: p.sub_title.as_deref().map(str::to_owned),
            season_episode: p.episode.and_then(crate::spoiler::episode_se),
            start: p.start,
            is_new: p.flags.is_new,
        }
    }

    /// The catch-up airing (its title is the playable's sub-title).
    fn catchup(title: &str, start: Timestamp) -> Self {
        Self { title: title.to_owned(), sub_title: None, season_episode: None, start, is_new: false }
    }

    fn subject(&self) -> Subject<'_> {
        Subject::Programme {
            title: &self.title,
            sub_title: self.sub_title.as_deref(),
            categories: &[],
            season_episode: self.season_episode,
            start: self.start,
            is_new: self.is_new,
        }
    }

    /// The title as the guard lets the banner say it ("Arsenal v Chelsea 󰈉").
    pub(super) fn title(&self, sp: &Spoilers) -> String {
        sp.text(&self.subject(), Field::Title, &self.title).map(Cow::into_owned).unwrap_or_default()
    }

    /// "Title · sub-title" as the guard lets the banner say it (a hidden sub-title is a bar).
    pub(super) fn line(&self, sp: &Spoilers) -> String {
        let title = self.title(sp);
        match self.sub_title.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(s) => format!("{title} · {}", sp.text(&self.subject(), Field::SubTitle, s).unwrap_or_default()),
            None => title,
        }
    }

    /// The guard hides or rewrites some of what the banner says of it (shown on purpose or not).
    fn guarded(&self, sp: &Spoilers) -> bool {
        let subj = self.subject();
        sp.would_guard(&subj, Field::Title, &self.title) || self.sub_title.as_deref().is_some_and(|s| sp.would_guard(&subj, Field::SubTitle, s))
    }
}

/// The channel of the picture (live or its catch-up).
pub(super) fn picture_channel(snap: &PlayerSnapshot) -> Option<ChannelId> {
    match snap.playable.as_ref().map(|p| &p.media) {
        Some(MediaRef::Catchup { channel, .. }) if snap.state != PlayerState::Idle => Some(*channel),
        _ => playing_channel(snap),
    }
}

/// The programmes the banner is about: the channel's now and next (live), or the catch-up
/// airing.
pub(super) fn banner_programmes(ctx: &mut UiCtx<'_>) -> Vec<Shown> {
    let snap = ctx.player.clone();
    let Some(p) = snap.playable.as_ref() else { return Vec::new() };
    match p.kind {
        PlayableKind::Catchup { start, .. } => p.subtitle.as_deref().map(|t| vec![Shown::catchup(t, start)]).unwrap_or_default(),
        PlayableKind::Live => {
            let Some(ch) = playing_channel(&snap).and_then(|id| ctx.lib.live.get(id).cloned()) else { return Vec::new() };
            live::now_next_of(ctx, &ch).map(|n| n.now.iter().chain(n.next.iter()).map(Shown::of).collect()).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The banner's hint: "Ctrl+B show spoilers" while it hides or rewrites something, "Ctrl+B
/// hide spoilers" while that is shown on purpose; `None` when it guards nothing.
pub(super) fn spoiler_hint(shown: &[Shown], sp: &Spoilers) -> Option<String> {
    if !crate::spoiler::ENABLED {
        return None;
    }
    let guarded: Vec<&Shown> = shown.iter().filter(|s| s.guarded(sp)).collect();
    let first = guarded.first()?;
    let key = crate::spoiler::ui::reveal_key_label();
    let verb = if sp.is_revealed(&first.subject()) { "hide spoilers" } else { "show spoilers" };
    Some(format!("{key} {verb}"))
}

impl PlayerScreen {
    /// Ctrl+B / Ctrl+Shift+B in the player (`None`: not one of them).
    pub(super) fn spoiler_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if !crate::spoiler::ENABLED || !matches!(action, Action::RevealSpoilers | Action::SpoilerMenu) {
            return None;
        }
        let Some(ch) = picture_channel(ctx.player) else {
            if action == Action::RevealSpoilers {
                ctx.toast(Toast::info("Nothing hidden here"));
                return Some(true);
            }
            return None; // the app's plain menu
        };
        let target = Target::Channel(ch);
        if action == Action::SpoilerMenu {
            return Some(crate::spoiler::ui::on_action(ctx, action, &target));
        }
        let sp = Spoilers::of(ctx);
        let keys: Vec<RevealKey> = banner_programmes(ctx).iter().filter(|s| s.guarded(&sp)).filter_map(|s| RevealKey::of(&s.subject())).collect();
        crate::spoiler::ui::toggle_reveal_keys(ctx, target, keys);
        self.show_banner(ctx);
        Some(true)
    }

    /// A reveal belongs to the picture it was made on: another channel (a zap) hides it again.
    /// (A reveal made in an overlay over the player is another item's: it stays.)
    pub(super) fn settle_reveal(&self, ctx: &UiCtx<'_>) {
        thread_local! {
            static PICTURE: std::cell::Cell<Option<ChannelId>> = const { std::cell::Cell::new(None) };
        }
        let now = picture_channel(ctx.player);
        let before = PICTURE.with(|p| p.replace(now));
        if crate::spoiler::ENABLED
            && before != now
            && let Some(before) = before
        {
            crate::spoiler::ui::hide_reveal_of(Target::Channel(before));
        }
    }
}
