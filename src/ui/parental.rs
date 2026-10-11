//! Parental PIN session. DESIGN §20.3. Session logic implemented in the skeleton (frozen;
//! reached every frame by WP-10a/11/12 code); WP-12 owns the PIN UI (Settings ▸ Parental,
//! `PinOverlay` behaviour) and `tests/parental.rs`.
//! PIN = 4–8 digits; stored as hex(blake3(salt || pin)) in config; unlock lasts until
//! `relock_min` elapses (0 = until restart). Locked groups/channels/VOD show a lock glyph and
//! require the PIN before browsing into / playing them; search excludes them while locked.
//! (A 4-digit PIN is a speed bump for children, not security — documented.)

use std::time::Instant;

use crate::config::ParentalConfig;

#[derive(Debug, Default)]
pub struct ParentalSession {
    unlocked_at: Option<Instant>,
}

impl ParentalSession {
    /// True when no PIN is configured / parental disabled, or unlocked and not expired.
    pub fn is_unlocked(&self, cfg: &ParentalConfig) -> bool {
        if !cfg.enabled || cfg.pin_hash.is_none() {
            return true;
        }
        match self.unlocked_at {
            None => false,
            Some(_) if cfg.relock_min == 0 => true,
            Some(t) => t.elapsed().as_secs() < cfg.relock_min as u64 * 60,
        }
    }

    /// Verify `pin`; unlock on success. (Attempt limiting / cooldown is the overlay's job.)
    pub fn unlock(&mut self, pin: &str, cfg: &ParentalConfig) -> bool {
        let ok = match (&cfg.pin_hash, &cfg.pin_salt) {
            (Some(h), Some(salt)) => hash_pin(pin, salt) == *h,
            (None, _) => true,
            _ => false,
        };
        if ok {
            self.unlocked_at = Some(Instant::now());
        }
        ok
    }

    pub fn lock(&mut self) {
        self.unlocked_at = None;
    }
}

/// hex(blake3(salt_hex_bytes || pin)).
pub fn hash_pin(pin: &str, salt_hex: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(salt_hex.as_bytes());
    h.update(pin.as_bytes());
    h.finalize().to_hex().to_string()
}

/// New random salt (32 hex chars): blake3 of time + pid + a stack and a heap address (no RNG
/// crate in the tree; a PIN salt only needs uniqueness).
pub fn new_salt() -> String {
    let mut h = blake3::Hasher::new();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    h.update(&nanos.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    let local = 0u8;
    h.update(&(&local as *const u8 as usize).to_le_bytes());
    let heap = Box::new(0u8);
    h.update(&(&*heap as *const u8 as usize).to_le_bytes());
    h.finalize().to_hex()[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_unlock_and_relock() {
        let salt = new_salt();
        assert_eq!(salt.len(), 32);
        let cfg = ParentalConfig { enabled: true, pin_hash: Some(hash_pin("1234", &salt)), pin_salt: Some(salt), relock_min: 0 };
        let mut s = ParentalSession::default();
        assert!(!s.is_unlocked(&cfg));
        assert!(!s.unlock("0000", &cfg));
        assert!(s.unlock("1234", &cfg));
        assert!(s.is_unlocked(&cfg));
        s.lock();
        assert!(!s.is_unlocked(&cfg));
        assert!(ParentalSession::default().is_unlocked(&ParentalConfig::default()));
    }
}
