//! The guard text helper (B4): every place that shows programme or episode text outside a
//! plain row asks it what to show. OWNER: S1.
//!
//! | Consumer | Policy |
//! |---|---|
//! | UI painters (`ui/widgets/spoiler.rs`, base-B call sites) | Normal; a revealed item uses Reveal |
//! | `notify.rs`, MPRIS `xesam:title`, switch / reminder toasts | Normal |
//! | MCP `guard_reply` (every read result, through [`reply::guard_json`]) | Normal; `spoiler_safe` → SpoilerSafe; `reveal_spoilers` → Reveal |
//! | Following summary, toasts, Upcoming, MCP for spoiler-safe rules | SpoilerSafe |
//! | Up next rows and card, multi-view strips | Normal |
//!
//! A [`Guard`] is cheap to clone and shared (AppShared, the reminder scheduler, MPRIS, the MCP
//! handler). `Guard::default()` is the identity (nothing guarded; `Policy::SpoilerSafe` still
//! strips): what every caller sees until `spoiler::tracker::start` installs a live one
//! ([`Guard::live`]), which fails closed until its first snapshot arrives.
//!
//! Never consulted for parental decisions: the guard never unlocks or reveals explicit content,
//! and the snapshot never holds any.

pub mod reply;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Config;
use crate::model::{ChannelId, SeenRow, SpoilerRuleId, Timestamp};
use crate::spoiler::snapshot::{GuardSnapshot, TopicSource};

/// The guard's implementation is real (S1). What the UI shows of it is `spoiler::ENABLED`
/// (S-ui); until then no live guard is installed and every `Guard` is the identity.
pub const ENABLED: bool = true;

/// Which text of a subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Title,
    SubTitle,
    Description,
    Plot,
    EpisodeTitle,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Policy {
    /// What the guard's rules say.
    #[default]
    Normal,
    /// Strip sub-titles, descriptions and plots, neutralise scores — for everything (MCP
    /// `spoiler_safe`, spoiler-safe Following rules). Applies with the guard off too.
    SpoilerSafe,
    /// The user asked to see it (Ctrl+B on the item, MCP `reveal_spoilers` for that ref).
    Reveal,
}

/// Why something is hidden (tooltips, the description block, MCP `spoiler` annotations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    /// The rule's label ("Arsenal", "Severance").
    pub rule: String,
    pub kind: ReasonKind,
    /// When it lifts by itself (`None`: when watched / never).
    pub until: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonKind {
    Embargo,
    Series,
    Blackout,
    /// `Policy::SpoilerSafe`.
    SpoilerSafe,
}

impl ReasonKind {
    /// The MCP annotation's `reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasonKind::Embargo => "embargo",
            ReasonKind::Series => "series",
            ReasonKind::Blackout => "blackout",
            ReasonKind::SpoilerSafe => "spoiler_safe",
        }
    }
}

/// What to show for one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guarded<'a> {
    /// As is.
    Shown(Cow<'a, str>),
    /// Rewritten ("Arsenal 2–1 Chelsea" → "Arsenal v Chelsea") — show with the dim glyph.
    Rewritten(Cow<'a, str>),
    /// Not shown (a fixed-width bar / "Description hidden").
    Hidden { reason: Option<Reason> },
}

impl<'a> Guarded<'a> {
    /// The text to show, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Guarded::Shown(t) | Guarded::Rewritten(t) => Some(t),
            Guarded::Hidden { .. } => None,
        }
    }
}

/// What the text belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject<'a> {
    /// A guide programme (title + categories + S/E decide; `start` is the airing's, 0 when
    /// unknown; `is_new`: the guide says it's a first showing).
    Programme {
        title: &'a str,
        sub_title: Option<&'a str>,
        categories: &'a [String],
        season_episode: Option<(i32, i32)>,
        start: Timestamp,
        is_new: bool,
    },
    /// A movie or series of the library (`series_norm`: `spoiler::series_key` of a series).
    Vod { title: &'a str, series_norm: Option<&'a str> },
    /// An episode of a series (`series_norm`: the catalogue's `series.norm_name`, which is
    /// `spoiler::series_key(series name)`).
    Episode { series_norm: &'a str, season: i32, episode: i32 },
    /// Text going outside the window (notifications, MPRIS).
    Notification { title: &'a str },
}

/// The verdict on a subject (all fields at once; painters use [`Guard::text`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Verdict {
    /// The title itself goes (a result verb next to a guarded topic: "Hamilton wins …").
    pub hide_title: bool,
    /// Sub-title / episode title.
    pub hide_sub: bool,
    /// Description / plot.
    pub hide_descr: bool,
    pub obscure_image: bool,
    /// Rewrite scores in the title.
    pub neutralise_title: bool,
    pub reason: Option<Reason>,
}

impl Verdict {
    /// Something is hidden (a rewrite alone doesn't count: it shows as `Guarded::Rewritten`).
    pub fn guarded(&self) -> bool {
        self.hide_title || self.hide_sub || self.hide_descr || self.obscure_image
    }
}

/// A topic term set the guard treats as a rule (Following's spoiler-safe rules, B4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardTerm {
    pub label: String,
    pub terms: Vec<String>,
    /// Scope: guide category Sport only (Team and Event rules).
    pub sport_only: bool,
}

/// One topic as Settings ("guarding now: Arsenal v Chelsea (Sat 17:30)") and MCP `status` show
/// it. Explicit content never appears (the snapshot holds none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicStatus {
    pub label: String,
    /// `None` for a Following rule's topic.
    pub rule: Option<SpoilerRuleId>,
    pub kind: ReasonKind,
    /// Guarding now.
    pub active: bool,
    pub until: Option<Timestamp>,
    /// The event it guards now (or next): title and first airing.
    pub event: Option<(String, Timestamp)>,
}

/// The live part shared by a guard's clones.
struct Gate {
    snap: RwLock<Arc<GuardSnapshot>>,
    /// Session-only pause ("show everything for 15 min"): 0 = not paused, `MAX` = until restart.
    paused_until: AtomicI64,
    /// Changes on every new snapshot and every pause change (UI caches compare it).
    epoch: AtomicU64,
    config: RwLock<Option<Arc<Config>>>,
    wake: tokio::sync::Notify,
    /// Events the tracker marked seen, for the UI's "guard lifted · Undo" (drained).
    seen: Mutex<VecDeque<SeenRow>>,
}

/// The guard, cheap to clone.
#[derive(Clone, Default)]
pub struct Guard {
    gate: Option<Arc<Gate>>,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.gate {
            None => f.write_str("Guard(identity)"),
            Some(g) => write!(f, "Guard(generation {})", g.snap.read().unwrap_or_else(|e| e.into_inner()).generation),
        }
    }
}

impl Guard {
    /// A live guard that fails closed until [`Guard::publish`] gives it a snapshot.
    pub fn live() -> Self {
        Self::with_snapshot(GuardSnapshot::fail_closed())
    }

    /// A live guard over a given snapshot (tests, benches).
    pub fn with_snapshot(snap: GuardSnapshot) -> Self {
        Self {
            gate: Some(Arc::new(Gate {
                snap: RwLock::new(Arc::new(snap)),
                paused_until: AtomicI64::new(0),
                epoch: AtomicU64::new(1),
                config: RwLock::default(),
                wake: tokio::sync::Notify::new(),
                seen: Mutex::default(),
            })),
        }
    }

    /// Not the identity.
    pub fn is_live(&self) -> bool {
        self.gate.is_some()
    }

    /// The current snapshot (the identity's: an empty "off" one).
    pub fn snapshot(&self) -> Arc<GuardSnapshot> {
        match &self.gate {
            Some(g) => g.snap.read().unwrap_or_else(|e| e.into_inner()).clone(),
            None => Arc::new(GuardSnapshot::default()),
        }
    }

    /// The builder's new snapshot.
    pub fn publish(&self, snap: GuardSnapshot) {
        if let Some(g) = &self.gate {
            *g.snap.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snap);
            g.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Changes whenever what the guard says may have changed (a new snapshot, a pause): UI
    /// caches of verdicts compare it. Time passing doesn't change it — judge again each minute.
    pub fn generation(&self) -> u64 {
        self.gate.as_ref().map_or(0, |g| g.epoch.load(Ordering::Acquire))
    }

    /// Ask the builder for a new snapshot (debounced 1 s): the app on `DataScope::{Epg, Live,
    /// Vod, Progress, Spoiler, Follow}`, a rule edit, a mark seen.
    pub fn rebuild(&self) {
        if let Some(g) = &self.gate {
            g.wake.notify_one();
        }
    }

    /// The live settings (the app at start and after every Settings edit); a change of
    /// `[spoiler]` or `[follow]` rebuilds.
    pub fn configure(&self, cfg: Arc<Config>) {
        let Some(g) = &self.gate else { return };
        let mut cur = g.config.write().unwrap_or_else(|e| e.into_inner());
        let changed = cur.as_ref().is_none_or(|c| c.spoiler != cfg.spoiler || c.follow != cfg.follow);
        *cur = Some(cfg);
        if changed {
            g.wake.notify_one();
        }
    }

    /// The settings the builder uses (`None` until the app configured the guard: defaults).
    pub fn config(&self) -> Option<Arc<Config>> {
        self.gate.as_ref().and_then(|g| g.config.read().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Wait for a [`Guard::rebuild`] (the builder). The identity never wakes.
    pub async fn rebuild_requested(&self) {
        match &self.gate {
            Some(g) => g.wake.notified().await,
            None => std::future::pending().await,
        }
    }

    /// Show everything until `until` (`None`: until Tuner restarts). Session only.
    pub fn pause(&self, until: Option<Timestamp>) {
        if let Some(g) = &self.gate {
            g.paused_until.store(until.unwrap_or(Timestamp::MAX), Ordering::Release);
            g.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Guard again now.
    pub fn resume(&self) {
        if let Some(g) = &self.gate
            && g.paused_until.swap(0, Ordering::AcqRel) != 0
        {
            g.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Paused until (`Timestamp::MAX`: until restart), or `None`.
    pub fn paused_until(&self, now: Timestamp) -> Option<Timestamp> {
        let t = self.gate.as_ref()?.paused_until.load(Ordering::Acquire);
        (t > now).then_some(t)
    }

    /// The guard is on (live, its settings on).
    pub fn on(&self) -> bool {
        self.gate.is_some() && self.snapshot().on
    }

    /// The tracker marked an event seen (the UI drains these into "guard lifted · Undo" toasts).
    pub fn note_seen(&self, row: SeenRow) {
        if let Some(g) = &self.gate {
            let mut q = g.seen.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= 16 {
                q.pop_front();
            }
            q.push_back(row);
        }
    }

    /// Seen events since the last call (oldest first).
    pub fn take_seen(&self) -> Vec<SeenRow> {
        self.gate.as_ref().map(|g| g.seen.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()).unwrap_or_default()
    }

    /// The rules' verdict now (`Verdict::default()` while paused or for the identity).
    fn normal_verdict(&self, subj: &Subject<'_>, now: Timestamp) -> Verdict {
        if self.gate.is_none() || self.paused_until(now).is_some() {
            return Verdict::default();
        }
        crate::spoiler::verdict::judge(&self.snapshot(), subj, now)
    }

    /// What to show for `f` of `subj`.
    pub fn text<'a>(&self, subj: &Subject<'_>, f: Field, text: &'a str, p: Policy, now: Timestamp) -> Guarded<'a> {
        if p == Policy::Reveal {
            return Guarded::Shown(Cow::Borrowed(text));
        }
        let mut v = self.normal_verdict(subj, now);
        let episode = matches!(subj, Subject::Episode { .. });
        if p == Policy::SpoilerSafe {
            let safe = Reason { rule: "Spoiler-safe".into(), kind: ReasonKind::SpoilerSafe, until: None };
            match f {
                Field::SubTitle | Field::Description | Field::Plot | Field::Image => {
                    return Guarded::Hidden { reason: v.reason.or(Some(safe)) };
                }
                Field::Title if !episode => {
                    v.neutralise_title = true;
                    if let Subject::Programme { categories, .. } = subj
                        && crate::spoiler::is_sport(categories)
                        && crate::textmatch::scores::has_result_verb(&crate::spoiler::snapshot::folded(text))
                    {
                        v.hide_title = true;
                        v.reason.get_or_insert(safe);
                    }
                }
                _ => {}
            }
        }
        apply(&v, f, episode, text)
    }

    /// The verdict on a subject now (all fields; `Verdict::default()` while paused).
    pub fn judge(&self, subj: &Subject<'_>, now: Timestamp) -> Verdict {
        self.normal_verdict(subj, now)
    }

    /// A title that leaves the window (notification summary, MPRIS `xesam:title`, a switch
    /// toast): rewritten when guarded, "Reminder" when it must not show at all.
    pub fn outside_title<'a>(&self, title: &'a str, now: Timestamp) -> Cow<'a, str> {
        match self.text(&Subject::Notification { title }, Field::Title, title, Policy::Normal, now) {
            Guarded::Shown(t) | Guarded::Rewritten(t) => t,
            Guarded::Hidden { .. } => Cow::Borrowed("Reminder"),
        }
    }

    /// The zap shield (B1: `UserZap` only): `Some` when tuning `channel` now could show a
    /// guarded result — its programme mentions an active topic and isn't the "until seen" event
    /// itself, or a blackout covers it. Never before the first build, never while paused or
    /// with "Don't tune guarded channels when zapping" off.
    pub fn shield(&self, channel: ChannelId, now: Timestamp) -> Option<Reason> {
        if self.gate.is_none() || self.paused_until(now).is_some() {
            return None;
        }
        let snap = self.snapshot();
        if !(snap.on && snap.ready && snap.settings.shield_zap) {
            return None;
        }
        snap.shield_spans(channel).iter().filter(|s| s.start <= now && now < s.stop && !s.is_event).find_map(|s| {
            let t = snap.topics.get(s.topic as usize)?;
            let until = t.active(now)?;
            t.shield.then(|| Reason { rule: t.label.clone(), kind: t.reason_kind(), until })
        })
    }

    /// Every topic and what it guards now or next (Settings, MCP `spoiler_guard status`).
    pub fn topics(&self, now: Timestamp) -> Vec<TopicStatus> {
        let snap = self.snapshot();
        snap.topics
            .iter()
            .map(|t| {
                let until = t.active(now);
                let event = t.open_window(now).or_else(|| t.windows.iter().find(|w| w.open > now)).map(|w| (w.title.clone(), w.open));
                TopicStatus {
                    label: t.label.clone(),
                    rule: match t.source {
                        TopicSource::Rule(id) => Some(id),
                        TopicSource::Follow => None,
                    },
                    kind: t.reason_kind(),
                    active: until.is_some(),
                    until: until.flatten(),
                    event,
                }
            })
            .collect()
    }
}

/// One field under a verdict.
fn apply<'a>(v: &Verdict, f: Field, episode: bool, text: &'a str) -> Guarded<'a> {
    let hidden = || Guarded::Hidden { reason: v.reason.clone() };
    match f {
        Field::Title if episode => {
            if v.hide_sub {
                hidden()
            } else {
                Guarded::Shown(Cow::Borrowed(text))
            }
        }
        Field::Title => {
            if v.hide_title {
                return hidden();
            }
            if v.neutralise_title
                && let Cow::Owned(t) = crate::textmatch::scores::neutralise_scores(text)
            {
                return Guarded::Rewritten(Cow::Owned(t));
            }
            Guarded::Shown(Cow::Borrowed(text))
        }
        Field::SubTitle | Field::EpisodeTitle if v.hide_sub => hidden(),
        Field::Description | Field::Plot if v.hide_descr => hidden(),
        Field::Image if v.obscure_image => hidden(),
        _ => Guarded::Shown(Cow::Borrowed(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_guards_nothing_but_spoiler_safe_still_strips() {
        let g = Guard::default();
        let subj = Subject::Programme {
            title: "Arsenal 2-1 Chelsea",
            sub_title: Some("Goals"),
            categories: &[],
            season_episode: None,
            start: 0,
            is_new: false,
        };
        assert_eq!(g.text(&subj, Field::Title, "Arsenal 2-1 Chelsea", Policy::Normal, 0).text(), Some("Arsenal 2-1 Chelsea"));
        assert_eq!(
            g.text(&subj, Field::Title, "Arsenal 2-1 Chelsea", Policy::SpoilerSafe, 0),
            Guarded::Rewritten(Cow::Borrowed("Arsenal v Chelsea"))
        );
        assert!(matches!(g.text(&subj, Field::Description, "Saka scores twice", Policy::SpoilerSafe, 0), Guarded::Hidden { .. }));
        assert_eq!(g.outside_title("Arsenal 2-1 Chelsea", 0), "Arsenal 2-1 Chelsea");
        assert!(g.shield(ChannelId(1), 0).is_none());
    }

    #[test]
    fn before_the_first_build_it_fails_closed() {
        let g = Guard::live();
        let ep = Subject::Episode { series_norm: "severance", season: 1, episode: 1 };
        assert!(matches!(g.text(&ep, Field::EpisodeTitle, "Good News About Hell", Policy::Normal, 0), Guarded::Hidden { .. }));
        assert_eq!(g.outside_title("Arsenal 2-1 Chelsea", 0), "Arsenal v Chelsea");
        // pausing shows everything, resuming guards again, and both change the generation
        let before = g.generation();
        g.pause(Some(100));
        assert!(g.generation() != before && g.paused_until(50) == Some(100));
        assert_eq!(g.text(&ep, Field::EpisodeTitle, "Good News About Hell", Policy::Normal, 50).text(), Some("Good News About Hell"));
        assert!(matches!(g.text(&ep, Field::EpisodeTitle, "x", Policy::Normal, 100), Guarded::Hidden { .. }), "the pause ran out");
        g.pause(None);
        g.resume();
        assert!(g.paused_until(0).is_none());
    }
}
