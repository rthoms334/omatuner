//! Settings ▸ Interface ▸ Playback: "Play the next item after" (Up next's countdown, DESIGN §32).
//! OWNER: Q-ui.

use super::*;

/// The countdown's choices (s); the last option is "Ask" (no countdown: the card waits).
const AFTER_S: [u32; 5] = [0, 5, 10, 20, 30];

static AFTER: CfgChoice = CfgChoice {
    label: "Play the next item after",
    options: &["At once", "5 s", "10 s", "20 s", "30 s", "Ask"],
    get: |c| {
        if c.queue.ask {
            AFTER_S.len()
        } else {
            // a hand-edited value shows as the nearest choice
            (0..AFTER_S.len()).min_by_key(|i| AFTER_S[*i].abs_diff(c.queue.countdown_s)).unwrap_or(2)
        }
    },
    set: |c, i| match AFTER_S.get(i) {
        Some(s) => {
            c.queue.ask = false;
            c.queue.countdown_s = *s;
        }
        None => c.queue.ask = true,
    },
};

/// Rows in the Interface page's Playback section.
pub(super) fn rows(_s: &SettingsScreen, _ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    if !crate::queue::ENABLED {
        return;
    }
    v.push(Row::Choice(&AFTER));
    v.push(Row::Note("Between Up next's items, and before the next episode when Up next is empty.".into(), false));
}

/// Its `Act::Feature(Feature::Queue, n)` buttons (none).
pub(super) fn act(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _n: u16) {}

/// Enter on its `Item::Feature(Feature::Queue, id)` rows (none).
pub(super) fn item(_s: &mut SettingsScreen, _ctx: &mut UiCtx<'_>, _id: i64) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_countdown_choice_round_trips_and_ask_stops_the_countdown() {
        let mut c = Config::default();
        assert_eq!((AFTER.get)(&c), 2, "10 s by default");
        (AFTER.set)(&mut c, 5);
        assert!(c.queue.ask);
        assert_eq!((AFTER.get)(&c), 5);
        (AFTER.set)(&mut c, 0);
        assert_eq!((c.queue.ask, c.queue.countdown_s), (false, 0));
        c.queue.countdown_s = 12;
        assert_eq!((AFTER.get)(&c), 2, "a hand-edited 12 s shows as 10 s");
    }
}
