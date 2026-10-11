//! Request limits and the reply check of the agent handler (DESIGN §30.6). Owner: MCP-app
//! track.
//!
//! * At most [`MAX_IN_FLIGHT`] requests at once (`busy`), queries ≤ 20/s (burst 40) and
//!   actions ≤ 2/s (burst 5) (`rate_limited`): a runaway client can't keep the store readers
//!   or the player busy.
//! * Every reply leaves through [`sanitize`]: each string goes through `redact` (a registered
//!   secret never leaves, even inside a title), and a string that still holds `"://"` — a URL —
//!   turns the whole reply into an `internal` error (logged at WARN, without the string).
//!   Result structs carry no URL fields; this is the backstop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::proto::{AgentError, ErrorCode, Reply};

/// Agent requests handled at once.
pub const MAX_IN_FLIGHT: usize = 4;

/// A token bucket.
#[derive(Debug)]
struct Bucket {
    per_s: f64,
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(per_s: f64, burst: f64) -> Self {
        Self { per_s, burst, tokens: burst, at: Instant::now() }
    }

    fn take(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens + now.saturating_duration_since(self.at).as_secs_f64() * self.per_s).min(self.burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The handler's limits (one per handler).
#[derive(Debug)]
pub struct Limits {
    in_flight: Arc<AtomicUsize>,
    buckets: Mutex<(Bucket, Bucket)>,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new((20.0, 40.0), (2.0, 5.0))
    }
}

/// Holds one in-flight slot until dropped.
#[derive(Debug)]
pub struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Limits {
    /// `(per second, burst)` for queries and for actions.
    pub fn new(queries: (f64, f64), actions: (f64, f64)) -> Self {
        Self { in_flight: Arc::default(), buckets: Mutex::new((Bucket::new(queries.0, queries.1), Bucket::new(actions.0, actions.1))) }
    }

    /// Admit a request: a slot, or `busy` / `rate_limited`.
    pub fn admit(&self, action: bool) -> Result<Slot, AgentError> {
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(AgentError { code: ErrorCode::Busy, message: "Tuner is answering other requests; try again in a moment".into() });
        }
        let slot = Slot(self.in_flight.clone());
        let mut b = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = if action { &mut b.1 } else { &mut b.0 };
        if !bucket.take(Instant::now()) {
            let what = if action { "actions" } else { "requests" };
            return Err(AgentError { code: ErrorCode::RateLimited, message: format!("Too many {what} a second; slow down") });
        }
        Ok(slot)
    }
}

/// Redact every string of a reply; a URL anywhere (`"://"`) → `internal` (DESIGN §30.6).
pub fn sanitize(mut reply: Reply, method: &str) -> Reply {
    let mut url = false;
    if let Some(v) = reply.result.as_mut() {
        scrub(v, &mut url);
    }
    if let Some(e) = reply.error.as_mut() {
        e.message = crate::redact::redact(&e.message).into_owned();
        url |= e.message.contains("://");
    }
    if url {
        tracing::warn!("agent: {method}: the reply held an address; withheld");
        return Reply::err(reply.id, ErrorCode::Internal, "Tuner withheld this answer (it would have contained an address); try another request");
    }
    reply
}

fn scrub(v: &mut serde_json::Value, url: &mut bool) {
    match v {
        serde_json::Value::String(s) => {
            if let std::borrow::Cow::Owned(r) = crate::redact::redact(s) {
                *s = r;
            }
            *url |= s.contains("://");
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| scrub(x, url)),
        serde_json::Value::Object(o) => o.iter_mut().for_each(|(k, x)| {
            *url |= k.contains("://");
            scrub(x, url)
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_refill_and_slots_free() {
        let limits = Limits::new((1.0, 2.0), (1.0, 1.0));
        assert!(limits.admit(false).is_ok());
        assert!(limits.admit(false).is_ok());
        assert_eq!(limits.admit(false).unwrap_err().code, ErrorCode::RateLimited, "burst of 2 used up");
        assert!(limits.admit(true).is_ok(), "actions have their own bucket");
        assert_eq!(limits.admit(true).unwrap_err().code, ErrorCode::RateLimited);
        let mut b = Bucket::new(10.0, 1.0);
        let t = Instant::now();
        assert!(b.take(t) && !b.take(t));
        assert!(b.take(t + std::time::Duration::from_millis(150)), "refilled");

        let limits = Limits::new((100.0, 100.0), (1.0, 1.0));
        let held: Vec<Slot> = (0..MAX_IN_FLIGHT).map(|_| limits.admit(false).unwrap()).collect();
        assert_eq!(limits.admit(false).unwrap_err().code, ErrorCode::Busy);
        drop(held);
        assert!(limits.admit(false).is_ok(), "slots come back");
    }

    #[test]
    fn replies_lose_secrets_and_never_carry_an_address() {
        crate::redact::register_secret("hunter2secret");
        let ok = Reply::ok(1, serde_json::json!({"items": [{"name": "News hunter2secret", "n": 3}]}));
        let clean = sanitize(ok, "search");
        assert_eq!(clean.result.unwrap()["items"][0]["name"], "News ***");
        let leaky = Reply::ok(2, serde_json::json!({"items": [{"plot": "see http://h.tv/x"}]}));
        let out = sanitize(leaky, "details");
        assert_eq!((out.ok, out.error.as_ref().map(|e| e.code)), (false, Some(ErrorCode::Internal)));
        assert!(!serde_json::to_string(&out).unwrap().contains("://"));
        let err = sanitize(Reply::err(3, ErrorCode::NotFound, "no rtsp://cam"), "play");
        assert_eq!(err.error.unwrap().code, ErrorCode::Internal);
    }
}
