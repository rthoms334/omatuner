//! Smart reconnect policy (pure). Owner: WP-07. DESIGN §11.4.
//!
//! mpv's `stream-lavf-o reconnect*` handles TCP-level drops inside one file; this policy
//! handles everything that ends or stalls the file (HLS segment 404 storms, token expiry,
//! server-side kicks, EOF on live, decoder stalls).
//!
//! Two regimes (field report 2026-10-11: hour-long provider outages and connection-limit races
//! ended live playback for good):
//! * [`ReconnectPolicy::delay`]: a first play, VOD and catch-up. Bounded: a channel that never
//!   started is reported quickly (and the zap skips it later).
//! * [`ReconnectPolicy::sticky_delay`]: a live channel that already played since it was asked
//!   for keeps reconnecting until the user stops it — only DRM, resolve, budget and
//!   unsupported-format failures end it.

use std::time::Duration;

use super::PlayerErrorKind;

/// Retries of a provider message on a first play (the restreamer may get the channel back).
pub const PROVIDER_RETRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectPolicy {
    /// Delay before attempt n (1-based) = backoff[min(n-1, len-1)].
    pub backoff: Vec<Duration>,
    /// Max attempts for live streams (config `player.reconnect_max_attempts`).
    pub max_attempts_live: u32,
    /// Max attempts for VOD/catch-up (resumes at last position).
    pub max_attempts_vod: u32,
    /// Playing this long without a failure resets the attempt counter.
    pub stable_after: Duration,
    /// Sticky reconnects ([`ReconnectPolicy::sticky_delay`]) keep the `backoff` cadence (its
    /// last step repeats) for this long after the first failure, then slow to `sticky_slow`.
    pub sticky_fast_for: Duration,
    pub sticky_slow: Duration,
    /// Connection-limit race (`state` module docs): a 403 within this long after a different
    /// stream of the same source closed is retried every `race_retry`. Xtream panels keep
    /// counting a closed connection for a while (30 s HLS sessions reproduced).
    pub race_window: Duration,
    pub race_retry: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            backoff: [500, 1000, 2000, 4000, 8000, 15000].into_iter().map(Duration::from_millis).collect(),
            max_attempts_live: 8,
            max_attempts_vod: 3,
            stable_after: Duration::from_secs(30),
            sticky_fast_for: Duration::from_secs(5 * 60),
            sticky_slow: Duration::from_secs(30),
            race_window: Duration::from_secs(45),
            race_retry: Duration::from_secs(3),
        }
    }
}

impl ReconnectPolicy {
    /// Delay before retry number `attempt` (1-based) after a failure of `kind`, or `None` to
    /// give up and enter `PlayerState::Error`.
    ///
    /// | kind                         | live                     | vod/catch-up |
    /// |------------------------------|--------------------------|--------------|
    /// | Network/Timeout/Stalled/EOF  | backoff, ≤ max_live      | backoff, ≤ max_vod |
    /// | HttpForbidden (401/403)      | 1 retry after 3 s (connection-limit race after a zap) | same |
    /// | HttpNotFound (404/410)       | 1 retry after 2 s        | none |
    /// | ProviderError (message body) | 3 retries, 3 s apart     | same |
    /// | HttpOther(5xx)               | backoff                  | backoff |
    /// | Unsupported / Drm / Resolve / Budget | none             | none |
    pub fn delay(&self, attempt: u32, kind: PlayerErrorKind, live: bool) -> Option<Duration> {
        use PlayerErrorKind as K;
        let max = if live { self.max_attempts_live } else { self.max_attempts_vod };
        let backoff = |n: u32| self.backoff.get((n.saturating_sub(1) as usize).min(self.backoff.len().saturating_sub(1))).copied();
        match kind {
            K::Unsupported | K::Drm | K::Resolve | K::Budget => None,
            K::HttpForbidden => (attempt <= 1).then(|| Duration::from_secs(3)),
            K::HttpNotFound => (live && attempt <= 1).then(|| Duration::from_secs(2)),
            K::HttpOther(code) if (400..500).contains(&code) => None,
            K::ProviderError => (attempt <= PROVIDER_RETRIES).then(|| Duration::from_secs(3)),
            K::Network | K::Timeout | K::Stalled | K::StreamEnded | K::HttpOther(_) | K::Mpv => {
                if attempt > max {
                    None
                } else {
                    backoff(attempt)
                }
            }
        }
    }

    /// Whether a live channel that already played keeps reconnecting after a failure of
    /// `kind` (see [`ReconnectPolicy::sticky_delay`]).
    pub fn sticky(kind: PlayerErrorKind) -> bool {
        !matches!(kind, PlayerErrorKind::Unsupported | PlayerErrorKind::Drm | PlayerErrorKind::Resolve | PlayerErrorKind::Budget)
    }

    /// Delay before reconnect `attempt` (1-based) of a live channel that already played, `since`
    /// = time since the first failure of this run of reconnects; `None` for the failures that
    /// end it anyway ([`ReconnectPolicy::sticky`]). The usual backoff (0.5 s … 15 s), its last
    /// step repeating for [`sticky_fast_for`](Self::sticky_fast_for) (≈ 5 min), then every
    /// [`sticky_slow`](Self::sticky_slow) (30 s) — about 120 requests an hour while a channel
    /// stays down. A refusal, "offline" or provider message waits at least 3 s (the provider
    /// may still count the dropped connection).
    pub fn sticky_delay(&self, attempt: u32, kind: PlayerErrorKind, since: Duration) -> Option<Duration> {
        use PlayerErrorKind as K;
        if !Self::sticky(kind) {
            return None;
        }
        let d = if since >= self.sticky_fast_for {
            self.sticky_slow
        } else {
            let i = (attempt.saturating_sub(1) as usize).min(self.backoff.len().saturating_sub(1));
            self.backoff.get(i).copied().unwrap_or(self.sticky_slow)
        };
        let floor = if matches!(kind, K::HttpForbidden | K::HttpNotFound | K::HttpOther(_) | K::ProviderError) {
            Duration::from_secs(3)
        } else {
            Duration::ZERO
        };
        Some(d.max(floor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_network_backs_off_then_gives_up() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.delay(1, PlayerErrorKind::Network, true), Some(Duration::from_millis(500)));
        assert_eq!(p.delay(7, PlayerErrorKind::Network, true), Some(Duration::from_secs(15)));
        assert_eq!(p.delay(9, PlayerErrorKind::Network, true), None);
        assert_eq!(p.delay(1, PlayerErrorKind::Drm, true), None);
        assert_eq!(p.delay(2, PlayerErrorKind::HttpForbidden, true), None);
        assert_eq!(p.delay(3, PlayerErrorKind::ProviderError, true), Some(Duration::from_secs(3)));
        assert_eq!(p.delay(4, PlayerErrorKind::ProviderError, true), None, "3 retries on a first play");
    }

    /// A channel that played: the backoff, its 15 s step for about 5 min, then every 30 s;
    /// never `None` for a transient failure; refusals and provider messages wait ≥ 3 s.
    #[test]
    fn sticky_schedule_eases_off_and_never_gives_up() {
        use PlayerErrorKind as K;
        let p = ReconnectPolicy::default();
        let s = |n: u32, since: u64| p.sticky_delay(n, K::Network, Duration::from_secs(since)).map(|d| d.as_millis() as u64);
        let early: Vec<_> = (1..=8).map(|n| s(n, 10).unwrap()).collect();
        assert_eq!(early, [500, 1000, 2000, 4000, 8000, 15000, 15000, 15000]);
        assert_eq!(s(20, 299), Some(15_000), "15 s cadence for the first minutes");
        assert_eq!(s(21, 300), Some(30_000), "then every 30 s");
        assert_eq!(s(10_000, 86_400), Some(30_000), "for as long as it takes");
        for k in [K::HttpForbidden, K::HttpNotFound, K::HttpOther(502), K::ProviderError] {
            assert_eq!(p.sticky_delay(1, k, Duration::ZERO), Some(Duration::from_secs(3)), "{k:?}");
            assert_eq!(p.sticky_delay(6, k, Duration::ZERO), Some(Duration::from_secs(15)), "{k:?}");
        }
        for k in [K::Timeout, K::Stalled, K::StreamEnded, K::Mpv] {
            assert_eq!(p.sticky_delay(1, k, Duration::ZERO), Some(Duration::from_millis(500)), "{k:?}");
        }
        for k in [K::Drm, K::Resolve, K::Budget, K::Unsupported] {
            assert_eq!(p.sticky_delay(1, k, Duration::ZERO), None, "{k:?} still ends playback");
        }
    }
}
