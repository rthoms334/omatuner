//! Wall-clock formatting shared by every screen, row and overlay, so Settings ▸ Interface ▸
//! Clock (`interface.clock_24h`) applies wherever a time is shown: "21:00" or "9:00 PM".

use crate::model::Timestamp;

/// `ts` in local time with a strftime format (`fallback` for an out-of-range stamp).
pub fn local(ts: Timestamp, format: &str, fallback: &str) -> String {
    match jiff::Timestamp::from_second(ts) {
        Ok(t) => t.to_zoned(jiff::tz::TimeZone::system()).strftime(format).to_string(),
        Err(_) => fallback.into(),
    }
}

/// Local "21:00", or "9:00 PM" with the 12-hour clock.
pub fn hhmm(ts: Timestamp, h24: bool) -> String {
    local(ts, if h24 { "%H:%M" } else { "%-I:%M %p" }, "--:--")
}

/// Local "Thu 21:00" / "Thu 9:00 PM".
pub fn day_time(ts: Timestamp, h24: bool) -> String {
    local(ts, if h24 { "%a %H:%M" } else { "%a %-I:%M %p" }, "--")
}

/// "21:00–22:00" / "9:00 PM–10:00 PM" (compact form for row detail lines).
pub fn span(start: Timestamp, stop: Timestamp, h24: bool) -> String {
    format!("{}–{}", hhmm(start, h24), hhmm(stop, h24))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings ▸ Interface ▸ Clock: 24-hour "21:00", or a 12-hour clock with no leading zero
    /// and an AM/PM.
    #[test]
    fn both_clocks() {
        for h in 0..24 {
            let ts = 1_791_576_000 + h * 3600;
            assert_eq!(hhmm(ts, true).len(), 5);
            let twelve = hhmm(ts, false);
            assert!(twelve.ends_with(" AM") || twelve.ends_with(" PM"), "{twelve}");
            assert!(!twelve.starts_with('0') && twelve.len() <= 8, "{twelve}");
            let (day24, day12) = (day_time(ts, true), day_time(ts, false));
            assert_eq!(day24.len(), 9, "{day24}");
            assert!(day12.ends_with('M') && day12[..3] == day24[..3], "{day12} / {day24}");
        }
        assert_eq!(hhmm(i64::MAX, true), "--:--");
    }
}
