//! Wall-clock decisions, independent of the timer which calls them (DESIGN §43 §3, §8.5).
//! A suspended runtime re-evaluates UTC times; it never replays a stale delay as a due event.

use crate::downloads::{Job, Kind, State};
use crate::model::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Wait,
    Start,
    Ask,
    Catchup,
    Missed,
}

pub fn due(job: &Job, now: Timestamp, busy: bool, catchup: bool) -> Due {
    if job.kind != Kind::Live {
        return Due::Start;
    }
    let (Some(start), Some(stop)) = (job.start(), job.prog_stop) else { return Due::Missed };
    if now < start {
        return Due::Wait;
    }
    if job.state == State::Waiting && job.bytes_done > 0 && job.stop().is_some_and(|end| now < end) {
        return if busy { Due::Ask } else { Due::Start };
    }
    if now >= stop {
        return if catchup { Due::Catchup } else { Due::Missed };
    }
    if job.reason.as_deref() == Some("watching") {
        return Due::Wait;
    }
    if busy {
        if let Some(asked) = job.fp.get("download_clash_at").and_then(serde_json::Value::as_i64) {
            return if now >= asked + 60 { Due::Wait } else { Due::Ask };
        }
        return Due::Ask;
    }
    if job.state == State::Scheduled && now > start + 60 && job.fp.get("download_clash_at").and_then(serde_json::Value::as_i64).is_none() {
        // The start was missed while Tuner was closed/asleep. Wait for the whole archive.
        return Due::Wait;
    }
    Due::Start
}

/// A window may cross midnight. Recordings deliberately bypass it at the call site.
pub fn window_allows(window: &str, minute: u32) -> bool {
    if window.trim().is_empty() {
        return true;
    }
    let parse = |s: &str| -> Option<u32> {
        let (h, m) = s.trim().split_once(':')?;
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let Some((start, end)) = window.split_once('-').and_then(|(a, b)| Some((parse(a)?, parse(b)?))) else { return false };
    if start == end {
        true
    } else if start < end {
        minute >= start && minute < end
    } else {
        minute >= start || minute < end
    }
}

pub fn space_allows(used: u64, free: u64, cap: u64, reserve: u64, size: u64) -> bool {
    used.saturating_add(size) <= cap && free.saturating_sub(reserve) >= size
}

pub fn retry_delay(attempt: u32) -> Option<i64> {
    [30, 120, 600].get(attempt.saturating_sub(1) as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overnight_window_and_space_reserve() {
        assert!(window_allows("", 0));
        assert!(window_allows("23:00-07:00", 30));
        assert!(!window_allows("23:00-07:00", 420));
        assert!(!window_allows("bad", 30));
        assert!(!space_allows(190, 50, 200, 10, 11));
        assert!(!space_allows(0, 20, 200, 10, 11));
        assert!(space_allows(190, 20, 200, 10, 10));
    }
}
