//! Smart reconnect policy (pure). Owner: WP-07. DESIGN §11.4.
//!
//! mpv's `stream-lavf-o reconnect*` handles TCP-level drops inside one file; this policy
//! handles everything that ends or stalls the file (HLS segment 404 storms, token expiry,
//! server-side kicks, EOF on live, decoder stalls).

use std::time::Duration;

use super::PlayerErrorKind;

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
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            backoff: [500, 1000, 2000, 4000, 8000, 15000].into_iter().map(Duration::from_millis).collect(),
            max_attempts_live: 8,
            max_attempts_vod: 3,
            stable_after: Duration::from_secs(30),
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
            K::Network | K::Timeout | K::Stalled | K::StreamEnded | K::HttpOther(_) | K::Mpv => {
                if attempt > max {
                    None
                } else {
                    backoff(attempt)
                }
            }
        }
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
    }
}
