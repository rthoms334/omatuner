//! UI-thread data access primitives. IMPLEMENTED IN THE SKELETON and frozen (every UI WP uses
//! them every frame). DESIGN §5.3, §13.4.
//!
//! * [`FrameBudget`]: hard cap on synchronous SQLite time per frame on the UI thread (2 ms).
//!   The UI thread is mpv's render thread (25–60 Hz while video shows), so DB time directly
//!   costs video frames. Work that doesn't fit is deferred to the next frame (callers paint a
//!   skeleton and request a repaint).
//! * [`EpgReader`]: the seam between UI code and EPG queries. Production uses [`DbEpg`]
//!   (`store::epg::{window, now_next}` on the UI connection); tests use fakes, so the guide
//!   grid and rows are testable without WP-02.
//! * [`NowNextCache`]: `(epg channel, tvg-shift) → NowNext` valid until the current programme
//!   ends; at most 8 misses per frame; invalidated on `DataScope::Epg`.
//! * [`MeasuredQuality`]: quality badges measured this session, ahead of the library.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::PlaylistOnCards;
use crate::error::Result;
use crate::model::{
    ChannelId, ChannelSummary, EpgChannelId, ItemKey, ItemKind, MediaRef, NowNext, ProgrammeSlim, Quality, QualityBadge, SourceId, Timestamp,
};
use crate::player::{PlayerSnapshot, PlayerState};
use crate::store::library::SourceMeta;

/// Per-frame time budget for synchronous UI-thread DB work.
#[derive(Debug)]
pub struct FrameBudget {
    limit: Duration,
    spent: Duration,
    deferred: bool,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new(Self::UI_DB)
    }
}

impl FrameBudget {
    /// The UI-thread DB budget per frame.
    pub const UI_DB: Duration = Duration::from_millis(2);

    pub fn new(limit: Duration) -> Self {
        Self { limit, spent: Duration::ZERO, deferred: false }
    }

    /// Reset (app, start of every frame).
    pub fn begin_frame(&mut self) {
        self.spent = Duration::ZERO;
        self.deferred = false;
    }

    pub fn exhausted(&self) -> bool {
        self.spent >= self.limit
    }

    /// Run `f` if budget remains (timing it); `None` = deferred to a later frame.
    pub fn run<R>(&mut self, f: impl FnOnce() -> R) -> Option<R> {
        if self.exhausted() {
            self.deferred = true;
            return None;
        }
        let t = Instant::now();
        let r = f();
        self.spent += t.elapsed();
        Some(r)
    }

    /// Something was deferred this frame: the app requests a repaint.
    pub fn deferred(&self) -> bool {
        self.deferred
    }
    pub fn spent(&self) -> Duration {
        self.spent
    }
}

/// EPG reads used by UI code. Times are EPG time except where noted (see `store::epg`).
pub trait EpgReader {
    /// Programmes of `ch` overlapping `[from, to)` (EPG time, unshifted).
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>>;
    /// Now/next per (channel, tvg-shift minutes), ALREADY shifted.
    fn now_next(&mut self, keys: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>>;
}

/// Production reader over the UI connection.
pub struct DbEpg<'a>(pub &'a rusqlite::Connection);

impl EpgReader for DbEpg<'_> {
    fn window(&mut self, ch: EpgChannelId, from: Timestamp, to: Timestamp) -> Result<Vec<ProgrammeSlim>> {
        crate::store::epg::window(self.0, ch, from, to)
    }
    fn now_next(&mut self, keys: &[(EpgChannelId, i32)], now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
        crate::store::epg::now_next(self.0, keys, now)
    }
}

/// Now/next per (epg channel, tvg-shift), valid until the current programme ends.
#[derive(Debug, Default)]
pub struct NowNextCache {
    map: HashMap<(EpgChannelId, i32), (NowNext, Timestamp)>,
    misses: u32,
    pending: bool,
}

impl NowNextCache {
    pub const MAX_MISSES_PER_FRAME: u32 = 8;
    /// Re-check channels without a current programme this often (seconds).
    pub const EMPTY_TTL: i64 = 300;

    /// Reset the per-frame miss counter (app, start of every frame).
    pub fn begin_frame(&mut self) {
        self.misses = 0;
        self.pending = false;
    }

    /// Cached value, loading it if allowed (≤ 8 misses per frame, within `budget`). A stale
    /// value is returned while a reload is deferred; `None` = not loaded yet (draw a skeleton
    /// and request a repaint — see [`NowNextCache::pending`]).
    pub fn get(&mut self, key: (EpgChannelId, i32), now: Timestamp, reader: &mut dyn EpgReader, budget: &mut FrameBudget) -> Option<&NowNext> {
        let fresh = self.map.get(&key).is_some_and(|(_, until)| now < *until);
        if !fresh {
            if self.misses < Self::MAX_MISSES_PER_FRAME {
                self.misses += 1;
                match budget.run(|| reader.now_next(&[key], now)) {
                    Some(Ok(mut m)) => {
                        let nn = m.remove(&key).unwrap_or_default();
                        let until = nn
                            .now
                            .as_ref()
                            .map(|p| p.stop)
                            .or(nn.next.as_ref().map(|p| p.start))
                            .unwrap_or(now + Self::EMPTY_TTL)
                            .min(now + Self::EMPTY_TTL * 12)
                            .max(now + 1);
                        self.map.insert(key, (nn, until));
                    }
                    Some(Err(e)) => {
                        tracing::debug!("now/next: {e}");
                        self.map.insert(key, (NowNext::default(), now + Self::EMPTY_TTL));
                    }
                    None => self.pending = true,
                }
            } else {
                self.pending = true;
            }
        }
        self.map.get(&key).map(|(nn, _)| nn)
    }

    /// Some lookups were deferred this frame (request a repaint).
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Drop everything (`DataScope::Epg`).
    pub fn invalidate(&mut self) {
        self.map.clear();
    }
}

/// The quality class each channel played this session decoded at (DESIGN §13.0 badges). The
/// player stores it (`channel_video`) without a library rebuild, so until the next build the
/// rows read it from here: a badge follows what mpv decoded as soon as the channel plays.
/// Owned by the app, fed every frame from the player snapshot.
#[derive(Debug, Default)]
pub struct MeasuredQuality {
    /// With the channel's identity: a later library may give the id to another channel.
    by_channel: HashMap<ChannelId, (SourceId, ItemKey, Quality)>,
}

impl MeasuredQuality {
    /// Note the playing live channel's decoded size (same rule as the controller's
    /// `record_video`: live, Playing, a known size).
    pub fn observe(&mut self, snap: &PlayerSnapshot) {
        let Some(p) = snap.playable.as_ref().filter(|p| p.kind.is_live()) else { return };
        let (MediaRef::Channel(id), Some(item), Some((w, h))) = (&p.media, &p.item, snap.video_size) else { return };
        let Some(q) = Quality::of_video(w, h).filter(|_| snap.state == PlayerState::Playing) else { return };
        if self.by_channel.get(id).is_none_or(|(_, _, old)| *old != q) {
            self.by_channel.insert(*id, (item.source, item.key.clone(), q));
        }
    }

    /// Measured this session, if this channel (same identity) played; never for radio (a
    /// cover picture is no video).
    pub fn get(&self, ch: &ChannelSummary) -> Option<Quality> {
        self.by_channel.get(&ch.id).filter(|(s, k, _)| !ch.radio && *s == ch.source_id && *k == ch.key).map(|e| e.2)
    }

    /// A new library: its own values include everything measured so far, and a refresh may
    /// have moved a key between the quality duplicates of a channel (`store::import`).
    pub fn clear(&mut self) {
        self.by_channel.clear();
    }
}

/// What a channel's quality badge says, copied out of the `UiCtx` for painter-level code that
/// can't hold it (the guide grid): `None` inside while Settings ▸ Interface ▸ Quality badges
/// is off.
#[derive(Debug, Clone, Copy)]
pub struct QualityBadges<'a>(pub Option<&'a MeasuredQuality>);

impl QualityBadges<'_> {
    /// Measured this session, else the library's (measured earlier, else from the name —
    /// `4K?` when the source's claims of that class were often wrong).
    pub fn of(&self, ch: &ChannelSummary) -> Option<QualityBadge> {
        self.0.and_then(|m| m.get(ch).map(|q| QualityBadge::measured(q, ch.quality.and_then(|b| b.claimed))).or(ch.quality))
    }
}

/// Which playlist an item comes from, copied out of the `UiCtx` for painters and closures that
/// can't hold it (Settings ▸ Interface ▸ Playlist on cards, DESIGN §13.0 / §29): `None` inside
/// while the playlist isn't shown.
#[derive(Debug, Clone, Copy)]
pub struct SourceMarks<'a>(pub Option<&'a [SourceMeta]>);

impl<'a> SourceMarks<'a> {
    /// The marks of a view of `kind` items. Auto shows the playlist only while two or more
    /// playlists are eligible for that kind (enabled, with such items) AND the view shows all
    /// of them (`all`: the scope is All playlists, or the view has no scope — Home, Search,
    /// detail pages, the player); inside one playlist the mark is noise.
    pub fn for_view(mode: PlaylistOnCards, sources: &'a [SourceMeta], kind: ItemKind, all: bool) -> Self {
        let shown = match mode {
            PlaylistOnCards::Auto => all && sources.iter().filter(|s| s.enabled && s.has(kind)).nth(1).is_some(),
            PlaylistOnCards::Always => true,
            PlaylistOnCards::Never => false,
        };
        Self(shown.then_some(sources))
    }

    /// The playlist to show for an item of `source` (`None` = don't show one).
    pub fn of(&self, source: SourceId) -> Option<&'a SourceMeta> {
        self.0.and_then(|all| all.iter().find(|s| s.id == source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgrammeFlags, ProgrammeId};
    use std::sync::Arc;

    struct Fake(u32);
    impl EpgReader for Fake {
        fn window(&mut self, _: EpgChannelId, _: Timestamp, _: Timestamp) -> Result<Vec<ProgrammeSlim>> {
            Ok(vec![])
        }
        fn now_next(&mut self, keys: &[(EpgChannelId, i32)], _now: Timestamp) -> Result<HashMap<(EpgChannelId, i32), NowNext>> {
            self.0 += 1;
            let p = ProgrammeSlim {
                id: ProgrammeId(1),
                start: 0,
                stop: 100,
                title: Arc::from("News"),
                sub_title: None,
                flags: ProgrammeFlags::default(),
                episode: None,
            };
            Ok(keys.iter().map(|k| (*k, NowNext { now: Some(p.shifted(k.1)), next: None })).collect())
        }
    }

    #[test]
    fn caches_until_programme_end_and_limits_misses() {
        let mut c = NowNextCache::default();
        let mut r = Fake(0);
        let mut b = FrameBudget::new(Duration::from_secs(1));
        assert!(c.get((EpgChannelId(1), 0), 10, &mut r, &mut b).is_some());
        assert!(c.get((EpgChannelId(1), 0), 50, &mut r, &mut b).is_some());
        assert_eq!(r.0, 1, "cached until stop");
        for i in 2..20 {
            c.get((EpgChannelId(i), 0), 10, &mut r, &mut b);
        }
        assert_eq!(r.0, NowNextCache::MAX_MISSES_PER_FRAME);
        assert!(c.pending());
        // shift is part of the key and applied by the reader
        c.begin_frame();
        let nn = c.get((EpgChannelId(1), 60), 10, &mut r, &mut b).unwrap();
        assert_eq!(nn.now.as_ref().unwrap().start, 3600);
    }

    /// The session's measurement beats the library's value, only once the channel is really
    /// Playing (a zap still shows the old picture), and never for another channel that got the
    /// same row id in a later library.
    #[test]
    fn measured_quality_waits_for_playing_and_checks_identity() {
        let ch = |key: &str| ChannelSummary {
            id: ChannelId(4),
            source_id: SourceId(1),
            key: ItemKey(key.into()),
            name: Arc::from("BBC One"),
            provider_name: None,
            number: None,
            logo: None,
            group_ids: vec![],
            epg_channel_id: None,
            tvg_shift_min: 0,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: 0,
            user_order: None,
            added_at: None,
            quality: Some(QualityBadge::named(Quality::Uhd, false)),
            family: 0,
        };
        let mut snap = PlayerSnapshot { state: PlayerState::Zapping { until: Instant::now() }, video_size: Some((1920, 1080)), ..Default::default() };
        snap.playable = Some(Arc::new(crate::model::Playable {
            from_download: false,
            media: MediaRef::Channel(ChannelId(4)),
            kind: crate::model::PlayableKind::Live,
            item: Some(crate::model::ItemRef { source: SourceId(1), kind: crate::model::ItemKind::Channel, key: ItemKey("a".into()) }),
            source_id: Some(SourceId(1)),
            title: "BBC One".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: crate::redact::Secret::new(String::new()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        }));
        let mut m = MeasuredQuality::default();
        m.observe(&snap);
        assert_eq!(m.get(&ch("a")), None, "not while zapping");
        snap.state = PlayerState::Playing;
        m.observe(&snap);
        let on = QualityBadges(Some(&m));
        assert_eq!(on.of(&ch("a")), Some(QualityBadge::measured(Quality::Fhd, Some(Quality::Uhd))), "measured beats the name");
        assert_eq!(on.of(&ch("b")).map(|b| b.label()), Some("4K"), "same id, another channel");
        assert_eq!(QualityBadges(None).of(&ch("a")), None, "badges off");
    }

    /// Settings ▸ Interface ▸ Playlist on cards: Auto shows the playlist only with two or more
    /// ELIGIBLE playlists for the view's kind (enabled, with such items) and only in All;
    /// Always / Never override; an item gets its own playlist.
    #[test]
    fn playlist_marks_follow_the_setting_eligibility_and_the_scope() {
        let src = |id: i64, enabled: bool, vod: bool| SourceMeta {
            id: SourceId(id),
            name: format!("P{id}"),
            icon: String::new(),
            color: Default::default(),
            kind: crate::model::SourceKind::M3uUrl,
            enabled,
            last_refresh: None,
            has_vod: vod,
            has_series: false,
            has_live: true,
            in_all: true,
            category_filter: None,
            own_category_filter: false,
            language_hidden: 0,
            code: String::new(),
        };
        let one_on = [src(1, true, true), src(2, false, true)];
        let two_on = [src(1, true, true), src(2, true, true)];
        // a free-channel preset: enabled, channels only
        let preset = [src(1, true, true), src(2, true, false)];
        let (auto, movie, live) = (PlaylistOnCards::Auto, ItemKind::Movie, ItemKind::Channel);
        let name = |m: SourceMarks<'_>, id: i64| m.of(SourceId(id)).map(|s| s.name.clone());
        assert_eq!(name(SourceMarks::for_view(auto, &one_on, movie, true), 1), None, "one enabled playlist = noise");
        assert_eq!(name(SourceMarks::for_view(auto, &two_on, movie, true), 2), Some("P2".into()), "the item's own playlist");
        assert_eq!(name(SourceMarks::for_view(auto, &two_on, movie, false), 2), None, "inside one playlist");
        assert_eq!(name(SourceMarks::for_view(auto, &preset, movie, true), 1), None, "the preset has no movies");
        assert_eq!(name(SourceMarks::for_view(auto, &preset, live, true), 1), Some("P1".into()), "but channels");
        assert_eq!(name(SourceMarks::for_view(PlaylistOnCards::Always, &one_on[..1], movie, false), 1), Some("P1".into()));
        assert_eq!(name(SourceMarks::for_view(PlaylistOnCards::Never, &two_on, movie, true), 1), None);
        assert_eq!(name(SourceMarks::for_view(PlaylistOnCards::Always, &two_on, movie, true), 9), None, "unknown playlist");
    }
}
