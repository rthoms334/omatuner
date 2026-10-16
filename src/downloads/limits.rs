//! Shared disk and transfer limits for all accounts (DESIGN §43 §3, §8.6, §8.8).
//! Reservations include writes in flight, so two accounts cannot each spend the same space.
use crate::model::DownloadId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Resources(Arc<Mutex<Usage>>);
#[derive(Debug)]
struct Usage {
    cap: u64,
    bytes: HashMap<DownloadId, u64>,
    rate: f64,
    next: Instant,
}
impl Default for Usage {
    fn default() -> Self {
        Self { cap: u64::MAX, bytes: HashMap::new(), rate: 0.0, next: Instant::now() }
    }
}
impl Resources {
    pub fn configure(&self, cap: u64, mbps: f64) {
        let mut u = self.0.lock().unwrap_or_else(|e| e.into_inner());
        u.cap = cap;
        if u.rate != mbps {
            u.next = Instant::now();
        }
        u.rate = mbps;
    }
    pub fn job(&self, id: DownloadId) -> Limits {
        Limits { resources: self.clone(), id }
    }
}
#[derive(Debug, Clone)]
pub struct Limits {
    resources: Resources,
    id: DownloadId,
}
impl Limits {
    /// Called only at a transfer boundary, after any prior writer has acknowledged closure.
    pub fn reset(&self, bytes: u64) {
        self.resources.0.lock().unwrap_or_else(|e| e.into_inner()).bytes.insert(self.id, bytes);
    }
    pub fn reserve(&self, bytes: u64) -> Option<Permit> {
        let mut u = self.resources.0.lock().unwrap_or_else(|e| e.into_inner());
        if u.bytes.values().copied().sum::<u64>().saturating_add(bytes) > u.cap {
            return None;
        }
        *u.bytes.entry(self.id).or_default() += bytes;
        Some(Permit { limits: self.clone(), bytes, committed: false })
    }
    pub async fn throttle(&self, bytes: u64) {
        let at = {
            let mut u = self.resources.0.lock().unwrap_or_else(|e| e.into_inner());
            if !u.rate.is_finite() || u.rate <= 0.0 {
                return;
            }
            u.next = u.next.max(Instant::now()) + Duration::from_secs_f64(bytes as f64 / (u.rate * 1_000_000.0));
            u.next
        };
        tokio::time::sleep_until(at.into()).await;
    }
}
pub struct Permit {
    limits: Limits,
    bytes: u64,
    committed: bool,
}
impl Permit {
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.committed {
            let mut u = self.limits.resources.0.lock().unwrap_or_else(|e| e.into_inner());
            let n = u.bytes.entry(self.limits.id).or_default();
            *n = n.saturating_sub(self.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn accounts_share_cap_failed_writes_roll_back_and_rate_is_aggregate() {
        let r = Resources::default();
        r.configure(100, 0.1);
        let a = r.job(DownloadId(1));
        let b = r.job(DownloadId(2));
        a.reset(20);
        let p = a.reserve(60).unwrap();
        assert!(b.reserve(21).is_none(), "pending writes count");
        drop(p);
        b.reserve(80).unwrap().commit();
        assert!(a.reserve(1).is_none());
        b.reset(0);
        assert!(a.reserve(80).is_some(), "deleting frees the cap");
        let start = Instant::now();
        tokio::join!(a.throttle(50_000), b.throttle(50_000));
        assert!(start.elapsed() >= Duration::from_millis(980));
    }
}
