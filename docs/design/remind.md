# Reminder engine and sleep timer (MCP pass 2, remind track)

This is the engine half of pass 2. The UI half (toasts, the pop-out countdown, the Settings
page, the sleep-timer menu) belongs to rui. DESIGN.md takes this in during wave 4.

## Times
- A reminder is about its **target**: `start + offset_s`. The offset can be −1 h to +12 h; a
  positive offset skips the build-up.
- A **notification** goes out `lead` before the target. The lead is the reminder's own `lead_s`
  (0 to 24 h), otherwise `reminders.lead_min`.
- A **switch** shows its Cancel countdown `SWITCH_WARN_S` (10 s) before the target and switches
  at the target.
- **Storage:** a switch row always has both `mode = 'switch'` and `lead_s = SWITCH_LEAD` (−1). A
  notification row has neither. Readers of either column therefore agree. Migration 0008
  converts the pass-1 rows.
- **Late reminders:**
  - A notification still goes out while its programme airs ("Started 5 min ago").
  - A switch still happens up to 15 min late. If it is 60 s or more late, `SwitchNow.late`
    says by how much and whether the channel has catch-up, so the app can start it from the
    beginning.
  - Anything older is dismissed silently.

## Fire policy (`reminders::fire_policy`, decided again at the switch)
| Reminder | Result |
|---|---|
| notification | the note with Watch (titles through `Guard::outside_title`) |
| switch, the channel already plays (single view) | nothing (marked fired) |
| news flash in quiet hours / another app fullscreen (`FireCtx.flash_skip`) | nothing |
| Following switch, target locked and session locked | note "… · needs your PIN" |
| any switch | `playback::route` (B1): Tile → switch; Downgrade/Shield → note with Watch; Refuse → note |

- Your own switches interrupt films, with Cancel.
- Following and news-flash switches don't interrupt a film: they become a note, unless
  `follow.interrupt_vod` is set.
- In multi-view, a switch goes to the silent picture and takes the sound.

## Where it opens
- Where a switch opens is the reminder's `open_in`, otherwise `reminders.open_in` (pop-out by
  default).
- The scheduler raises the window only for Full window. The pop-out and "Where I am" never take
  focus.

## Events (`AppEvent::Reminder`)
With `reminders::ENABLED` on, the scheduler sends these events:
- `SwitchSoon{reminder, channel, channel_name, title (guarded), at, open_in, route}`
- `SwitchNow{reminder, channel, open_in, route, rule, late}`
- `NotSwitched{reminder, why}`
- `SleepWarn{at}`, `SleepNow{action}` and `SleepChanged(status)`

Until then it sends pass 1's `PlatformEvent::{SwitchSoon, SwitchNow}`, so nothing changes before
rui handles the new events.

## While Tuner is closed
- **Units** (`platform::systemd`, `reminders::arming`):
  - Each unit is `tuner-reminder-<id>`, described "Tuner reminder", runs `tuner --reminder
    <id>`, and fires at `OnCalendar=@epoch` with 1 s accuracy.
  - At most 30 units are armed, all within 48 h.
  - A switch's unit fires 30 s before its countdown.
  - Units are transient, so nothing is written in `$HOME`.
  - Only the default profile arms units.
- **Keeping units in step:** the running scheduler re-syncs the units on every reload, and only
  when the plan changes (`ArmSync`). Units left from an earlier run are re-armed once. If
  systemd fails, arming turns off for the session, logged once.
- **`tuner --reminder <id>` with no Tuner running** (`reminders::closed::fire`, called by
  `main`):
  - A due notification is shown from that short-lived process, with no window. Watch runs
    `tuner --play <src>:<key>`.
  - A switch starts Tuner, popped out unless the reminder says otherwise.
  - The session counts as locked:
    - reminders on explicit channels wait untouched;
    - a Following switch to a locked channel becomes "needs your PIN".
  - A unit that fires early (because of a clock change) does nothing. Missed reminders are
    dismissed.

## Sleep timer (`reminders::sleep`)
- **Options:**
  - at a time, or after N min;
  - end of programme: the guide's end for live; the item's end for a film, episode or catch-up;
  - after N items: Up next's rows while it drives playback, otherwise films and episodes. The
    item playing now counts as the first;
  - end of Up next.
- **Counting items:** an item counts when it ends, or when the next one starts within 90 s of
  its end (credits). Stopping or skipping mid-way doesn't count. Live channels are never items.
- **Firing:**
  - It warns `reminders.sleep_warn_s` before.
  - It then stops (or pauses) every picture it was given and sends `SleepNow` so the app stops
    the rest.
- **The service:**
  - `SleepService` runs on tokio, so it works while the window is hidden.
  - It follows `Tiles::main_watch()` and `AppShared::subscribe_queue()`.
  - `SleepHandle::{set, cancel, status}` can be called from any thread.

## Clashes
- `reminders::clashes` and `reminders::clashes_with` look for pending reminders whose windows
  (target to programme end) overlap.
- When both reminders switch, the clash is `Switches`: only one picture can switch.
- `store::user::reminders_overlapping(from, to)` loads the candidates.
