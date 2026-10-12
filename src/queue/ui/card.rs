//! The card between Up next's items (DESIGN §32 §2.4), as the engine's snapshot describes it:
//! the countdown to the next item (or to the next episode, the empty-queue case), the waits
//! (PIN, a slot that hasn't started, three failures in a row, multi-view) and the card after
//! the last item. The player draws it over the video, the pop-out as a strip, the Up next screen
//! as a banner and the mini player as a chip; the keys and buttons are the same everywhere.

use crate::keymap::Action;
use crate::model::{EpisodeId, QueueId, Timestamp};
use crate::queue::{Blocked, CountdownWhy, FinishedWhy, QueueCommand, QueueSnapshot, RowState};

/// What the card shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Card {
    /// "Up next in 8 s · 3 of 7" (`left` = `None`: Settings ▸ "Ask", no countdown).
    Countdown {
        why: CountdownWhy,
        /// The item (or episode) that plays next; `None` when a live slot ended with nothing
        /// after it.
        next: Option<String>,
        detail: Option<String>,
        left: Option<i64>,
        place: Option<(usize, usize)>,
        /// The live slot that ended / the item that failed.
        before: Option<String>,
    },
    /// "Up next is locked · You Only Live Twice".
    Locked { title: String },
    /// "Up next: Bake Off at 20:00 on BBC One — in 25 min".
    NotStarted { title: String, detail: Option<String>, start: Timestamp },
    /// "Up next paused: 3 items failed in a row".
    Paused,
    /// "Up next waits while multi-view is on".
    MultiView,
    /// The card after the last item; `offer`: the next episode, without a countdown.
    Finished { text: String, offer: Option<(EpisodeId, String)> },
}

/// A card button (each also has a key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    PlayNow,
    List,
    Cancel,
    KeepWatching,
    EnterPin,
    Skip,
    SkipForNow,
    CarryOn,
    PlayOffer,
    Close,
}

impl Button {
    /// Glyph, label, key.
    pub fn parts(self) -> (&'static str, &'static str, &'static str) {
        use crate::ui::widgets::glyphs as g;
        match self {
            Button::PlayNow => (g::PLAY, "Play now", "Enter"),
            Button::List => (g::UP_NEXT, "List", "u"),
            Button::Cancel => (g::CLOSE, "Cancel", "Esc"),
            Button::KeepWatching => (g::CLOSE, "Keep watching", "Esc"),
            Button::EnterPin => (g::LOCK, "Enter PIN", "Enter"),
            Button::Skip => (g::NEXT, "Skip", "n"),
            Button::SkipForNow => (g::NEXT, "Skip for now", "n"),
            Button::CarryOn => (g::PLAY, "Carry on", "Enter"),
            Button::PlayOffer => (g::PLAY, "Play", "Enter"),
            Button::Close => (g::CLOSE, "Close", "Esc"),
        }
    }
}

/// What a card key or button asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Send(QueueCommand),
    /// Ask for the PIN, then `Proceed { unlocked: true }`.
    Pin(String),
    /// Open the Up next list (`u`).
    List,
    /// Play the offered episode and close the card.
    PlayEpisode(EpisodeId),
}

fn title_of(s: &QueueSnapshot, id: Option<QueueId>) -> Option<String> {
    s.row(id?).map(|r| super::row_title(&r.entry))
}

impl Card {
    /// The card the snapshot asks for, if any (a countdown first, then a wait, then the end).
    pub fn of(s: &QueueSnapshot, now: Timestamp) -> Option<Card> {
        if let Some(c) = &s.countdown {
            let left = c.at.map(|at| (at - now).max(0));
            let (next, detail) = match c.why {
                CountdownWhy::NextEpisode => (s.next_episode.as_ref().map(|e| e.label.clone()), None),
                _ => {
                    let row = c.id.and_then(|id| s.row(id));
                    (row.map(|r| r.entry.title.clone()), row.and_then(|r| r.entry.subtitle.clone()))
                }
            };
            let before = match c.why {
                CountdownWhy::SlotEnded => title_of(s, s.current),
                // the latest failed row before the next one
                CountdownWhy::Failed => {
                    let end = c.id.and_then(|id| s.rows.iter().position(|r| r.entry.id == id)).unwrap_or(s.rows.len());
                    s.rows[..end].iter().rev().find(|r| r.entry.state == RowState::Failed).map(|r| r.entry.title.clone())
                }
                _ => None,
            };
            return Some(Card::Countdown { why: c.why, next, detail, left, place: c.id.and_then(|id| s.place(id)), before });
        }
        if let Some(b) = s.blocked {
            let row = s.blocked_id.and_then(|id| s.row(id));
            return Some(match b {
                Blocked::NeedsPin => Card::Locked { title: row.map(|r| super::row_title(&r.entry)).unwrap_or_default() },
                Blocked::NotStarted => Card::NotStarted {
                    title: row.map(|r| r.entry.title.clone()).unwrap_or_default(),
                    detail: row.and_then(|r| r.entry.subtitle.clone()),
                    start: row.and_then(|r| r.entry.programme.as_ref()).map_or(now, |p| p.start),
                },
                Blocked::FailedStreak => Card::Paused,
                Blocked::MultiView => Card::MultiView,
            });
        }
        let f = s.finished?;
        let text = match f {
            FinishedWhy::EndOfQueue => "That was Up next.".to_owned(),
            FinishedWhy::StopAfter(id) => match title_of(s, Some(id)) {
                Some(t) => format!("Stopped after {t}, as asked"),
                None => "Stopped, as asked".to_owned(),
            },
            FinishedWhy::Sleep => "Up next stopped for the sleep timer".to_owned(),
        };
        Some(Card::Finished { text, offer: s.next_episode.as_ref().map(|e| (e.id, e.label.clone())) })
    }

    /// A countdown is running (the card repaints every quarter second).
    pub fn counts_down(&self) -> bool {
        matches!(self, Card::Countdown { left: Some(_), .. })
    }

    /// The headline and the line under it.
    pub fn lines(&self, now: Timestamp, h24: bool) -> (String, Option<String>) {
        let secs = |left: Option<i64>| left.map(|l| format!(" in {l} s")).unwrap_or_default();
        match self {
            Card::Countdown { why, next, detail, left, place, before } => {
                let of = place.map(|(k, n)| format!(" · {k} of {n}")).unwrap_or_default();
                let head = match why {
                    CountdownWhy::NextEpisode => format!("Next episode{}", secs(*left)),
                    CountdownWhy::SlotEnded => match (before, next) {
                        (Some(b), Some(_)) => format!("{b} has ended — up next{}", secs(*left)),
                        (Some(b), None) => format!("{b} has ended"),
                        (None, _) => format!("Up next{}", secs(*left)),
                    },
                    CountdownWhy::Failed => match before {
                        Some(b) => format!("Couldn't play {b} — next{}", secs(*left)),
                        None => format!("Couldn't play that — next{}", secs(*left)),
                    },
                    CountdownWhy::Ended => format!("Up next{}{of}", secs(*left)),
                };
                let line = match (next, detail) {
                    (Some(n), Some(d)) => Some(format!("{n} · {d}")),
                    (Some(n), None) => Some(n.clone()),
                    _ => None,
                };
                (head, line)
            }
            Card::Locked { title } => (format!("Up next is locked · {title}"), None),
            Card::NotStarted { title, detail, start } => {
                let at = crate::ui::widgets::time::hhmm(*start, h24);
                let wait = crate::ui::epg_grid::time_left((*start - now).max(0));
                (format!("Up next: {title} at {at} — {wait}"), detail.clone())
            }
            Card::Paused => ("Up next paused: 3 items failed in a row".into(), Some("Provider trouble? It carries on when you say.".into())),
            Card::MultiView => ("Up next waits while multi-view is on".into(), None),
            Card::Finished { text, offer } => (text.clone(), offer.as_ref().map(|(_, l)| format!("Next episode: {l}"))),
        }
    }

    /// The mini player's chip ("Next in 8 s"); `None` for the cards it doesn't show.
    pub fn short(&self, _now: Timestamp) -> Option<String> {
        Some(match self {
            Card::Countdown { why: CountdownWhy::NextEpisode, left: Some(l), .. } => format!("Next episode in {l} s"),
            Card::Countdown { next: None, .. } => return None,
            Card::Countdown { left: Some(l), .. } => format!("Next in {l} s"),
            Card::Countdown { left: None, .. } => "Up next is waiting".into(),
            Card::Locked { .. } => "Up next is locked".into(),
            Card::NotStarted { .. } => "Up next is waiting".into(),
            Card::Paused => "Up next paused".into(),
            Card::MultiView | Card::Finished { .. } => return None,
        })
    }

    pub fn buttons(&self) -> Vec<Button> {
        match self {
            Card::Countdown { why: CountdownWhy::SlotEnded, next: Some(_), .. } => vec![Button::PlayNow, Button::KeepWatching],
            Card::Countdown { why: CountdownWhy::SlotEnded, next: None, .. } => vec![Button::KeepWatching],
            Card::Countdown { why: CountdownWhy::NextEpisode, .. } => vec![Button::PlayNow, Button::Cancel],
            Card::Countdown { .. } => vec![Button::PlayNow, Button::List, Button::Cancel],
            Card::Locked { .. } => vec![Button::EnterPin, Button::Skip, Button::Cancel],
            Card::NotStarted { .. } => vec![Button::SkipForNow, Button::Cancel],
            Card::Paused => vec![Button::CarryOn, Button::Cancel],
            Card::MultiView => vec![Button::Cancel],
            Card::Finished { offer: Some(_), .. } => vec![Button::PlayOffer, Button::Close],
            Card::Finished { offer: None, .. } => vec![Button::Close],
        }
    }

    /// A button's step.
    pub fn press(&self, b: Button) -> Step {
        match b {
            Button::PlayNow => Step::Send(QueueCommand::Now),
            Button::List => Step::List,
            Button::Cancel | Button::KeepWatching | Button::Close => Step::Send(QueueCommand::Cancel),
            Button::EnterPin => Step::Pin(match self {
                Card::Locked { title } => format!("“{title}” is locked"),
                _ => "Up next is locked".into(),
            }),
            Button::Skip => Step::Send(QueueCommand::Proceed { unlocked: false }),
            Button::SkipForNow => Step::Send(QueueCommand::Next),
            Button::CarryOn => Step::Send(QueueCommand::Resume),
            Button::PlayOffer => match self {
                Card::Finished { offer: Some((id, _)), .. } => Step::PlayEpisode(*id),
                _ => Step::Send(QueueCommand::Cancel),
            },
        }
    }

    /// A key on the card (Enter / `i` play now, `n` plays now or skips, `u` the list, Esc
    /// cancels); `None` for keys the card leaves alone.
    pub fn key(&self, action: Action) -> Option<Step> {
        let buttons = self.buttons();
        let first = |want: &[Button]| buttons.iter().copied().find(|b| want.contains(b));
        let b = match action {
            Action::Activate | Action::FocusOsdControls => first(&[Button::PlayNow, Button::EnterPin, Button::CarryOn, Button::PlayOffer])?,
            Action::QueueNext => match self {
                Card::Countdown { next: Some(_), .. } => Button::PlayNow,
                _ => first(&[Button::Skip, Button::SkipForNow])?,
            },
            Action::QueuePanel => Button::List,
            // Esc in controls focus mode too: the card is the rung before leaving the controls
            Action::Back | Action::LeaveOsdControls => first(&[Button::Cancel, Button::KeepWatching, Button::Close])?,
            _ => return None,
        };
        Some(self.press(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::ui::tests::entry;
    use crate::queue::{Countdown, NextEpisode, QueueRow};

    fn snap(rows: &[(i64, RowState)]) -> QueueSnapshot {
        let rows: Vec<QueueRow> = rows.iter().map(|(id, s)| QueueRow { entry: entry(*id, *s), locked: false, eta: None, note: None }).collect();
        QueueSnapshot { rows: rows.into(), ..Default::default() }
    }

    /// The card reads the snapshot: the countdown names the next row and its place, Enter plays
    /// it now and Esc cancels; a failed item is named; the empty-queue episode countdown and
    /// the PIN wait have their own keys.
    #[test]
    fn the_card_follows_the_snapshot_and_its_keys_send_the_engine_commands() {
        let mut s = snap(&[(1, RowState::Played), (2, RowState::Failed), (3, RowState::Pending), (4, RowState::Pending)]);
        s.countdown = Some(Countdown { id: Some(QueueId(3)), at: Some(108), why: CountdownWhy::Failed });
        let c = Card::of(&s, 100).unwrap();
        assert_eq!(c.lines(100, true).0, "Couldn't play Movie 2 — next in 8 s");
        assert_eq!(c.lines(100, true).1.as_deref(), Some("Movie 3"));
        assert_eq!(c.key(Action::Activate), Some(Step::Send(QueueCommand::Now)));
        assert_eq!(c.key(Action::QueueNext), Some(Step::Send(QueueCommand::Now)));
        assert_eq!(c.key(Action::Back), Some(Step::Send(QueueCommand::Cancel)));
        assert_eq!(c.key(Action::QueuePanel), Some(Step::List));
        assert_eq!(c.key(Action::PlayPause), None, "other keys stay the player's");
        s.countdown = Some(Countdown { id: Some(QueueId(3)), at: Some(108), why: CountdownWhy::Ended });
        assert_eq!(Card::of(&s, 100).unwrap().lines(100, true).0, "Up next in 8 s · 2 of 3");
        assert_eq!(Card::of(&s, 100).unwrap().short(100).as_deref(), Some("Next in 8 s"));

        s.countdown = Some(Countdown { id: None, at: Some(110), why: CountdownWhy::NextEpisode });
        s.next_episode = Some(NextEpisode { id: EpisodeId(9), label: "S02E06 · Bad Dates".into() });
        let c = Card::of(&s, 100).unwrap();
        assert_eq!(c.lines(100, true), ("Next episode in 10 s".into(), Some("S02E06 · Bad Dates".into())));
        assert_eq!(c.buttons(), vec![Button::PlayNow, Button::Cancel]);

        s.countdown = None;
        s.blocked = Some(Blocked::NeedsPin);
        s.blocked_id = Some(QueueId(4));
        let c = Card::of(&s, 100).unwrap();
        assert_eq!(c.key(Action::Activate), Some(Step::Pin("“Movie 4” is locked".into())));
        assert_eq!(c.key(Action::QueueNext), Some(Step::Send(QueueCommand::Proceed { unlocked: false })), "n skips the locked item");

        s.blocked = None;
        s.finished = Some(FinishedWhy::EndOfQueue);
        let c = Card::of(&s, 100).unwrap();
        assert_eq!(c.key(Action::Activate), Some(Step::PlayEpisode(EpisodeId(9))), "the next episode is offered, no countdown");
        assert!(!c.counts_down());
        assert_eq!(c.short(100), None, "the mini chip leaves the end card to the player");
    }
}
