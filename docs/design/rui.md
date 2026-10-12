# Reminders and sleep timer UI (rui track)

This is the UI half of MCP pass 2's reminder engine (`docs/design/remind.md`). It turns on
`reminders::ENABLED`, so the scheduler sends the pass-2 `AppEvent::Reminder` events and arms
systemd timers. DESIGN.md takes this in during wave 4.

## Setting a reminder or a switch from a programme
- **Quick reminder:** `r` in the guide and the popover's "Remind me" set a plain notification
  straight away, as before.
- **The sheet:** "Switch to this when it starts…" (or "Edit reminder…" when the programme has a
  reminder) opens the reminder sheet (`ui::overlays::reminders::ReminderSheet`). It is in a
  programme's menus: the guide's Ctrl+Enter / right-click menu, the programme popover's
  "More…" (Ctrl+Enter) and search's guide hits. It comes through `ui::item_menu` with
  `Action::ReminderOptions`, a menu-only action with no key.
- **What the sheet sets:**
  - **When it starts:** Notify me / Switch to it.
  - **Notify me before** (notify only): 1, 2, 5, 10, 15 or 30 min. A lead of 0 means "the
    setting" to the engine, so the sheet offers no "at the start".
  - **Start offset:** 5 min early, on time, or 5, 10, 15 or 30 min in ("skip the build-up").
  - **Open in** (switch only): Pop-out, Full window or Where I am. The setting's choice is
    marked "(setting)". Picking it stores no value on the row, so the row follows the setting.
- **Clashes:** the sheet lists up to 3 pending reminders whose windows overlap, using
  `reminders::clashes_with`. When both are switches it adds "the later one takes the picture".
- **Keys:** ↑/↓ move between rows, ←/→ change the value, Enter sets it from any value row, and
  Esc closes. Every choice can also be clicked. Saving an existing row goes through
  `store::user::edit_reminder`, which detaches a Following rule's reminder when its lead or mode
  changes.

## The strip (`reminders::ui::ReminderUi`, drawn by the app every frame)
The strip is a popup card at the top centre of the window, spanning the width in the pop-out. It
never takes focus. It shows:
- **A switch's countdown:** "Switching to ‹channel› in 8 s · ‹title›". The title is already
  guarded. Buttons:
  - Cancel removes the row, and a `SwitchNow` already on its way is ignored.
  - Open full (shown when the switch opens in the pop-out) brings the window back in now and
    switches there.
  - Delete in the pop-out cancels the next countdown.
- **The sleep timer's warning:** "Sleep timer: stopping in 1:52", with +15 min and Turn off.
- **A news flash:** "News flash · ‹rule› · until 13:05", with End (also Delete in the pop-out)
  and, in the pop-out, Open full. Open full keeps watching in the full window, and the flash no
  longer closes by itself.

## Where scheduled playback goes (`reminders::ui::placement`)
| Open in | Tuner now | Result |
|---|---|---|
| Pop-out | in front, on the Player, playing | switches in place |
| Pop-out | popped out already | in place |
| Pop-out | anything else (Hyprland) | pops out (pinned, every workspace, no focus taken) |
| Pop-out | no Hyprland | in place: the corner mini player in the normal window |
| Full window | any | the full window's player (popped back in; the scheduler raised it) |
| Where I am | any | in place (the Player stays the Player, elsewhere the mini player) |

More rules:
- When the switch needs a PIN, the window doesn't move: the prompt shows where the window is.
- A switch that comes 60 s or more late plays from the beginning on catch-up. This happens when
  the channel has catch-up and needs no PIN. Otherwise it plays live with "Started N min ago".
- `tuner --play <ref> --popout` pops out on the first frame that has the Hyprland worker.
  Without Hyprland it falls back to the mini player.

## News flash
- **Recognising one:** a `SwitchNow` whose reminder belongs to a Following rule in flash mode
  (`store::follow::flash_of`) is a news flash.
- **When it starts, the app remembers:**
  - what the main picture played (media, origin, paused);
  - whether it popped the window out;
  - whether the Player screen showed.
- **Muting:** when the flash is set to Mute, the app mutes for it.
- **When it ends:** headlines-only ends after N minutes, otherwise at the programme's end. The
  end is fired by a tokio timer (`FlashEnded`), so it works with no frames, and is checked
  every frame too.
- **On end:**
  - the app unmutes;
  - it puts back what played, but only if the flash's channel still plays (if the user moved
    on, nothing changes);
  - it pops the window back in;
  - it leaves the Player screen if that wasn't where the user was.

## Sleep timer
- **The menu** (`SleepTimerMenu`) opens from Ctrl+Shift+T, the player menu's "Sleep timer…",
  the status-bar chip and Settings ▸ Reminders. It offers:
  - Then: stop or pause (←/→);
  - in 15, 30 or 45 min, 1 h, 1½ h or 2 h;
  - at the end of this programme (for live, the guide's end of the programme on now);
  - after N items (←/→ sets N; Up next's rows while Up next drives playback, otherwise episodes
    and films);
  - at the end of Up next;
  - turn off.
- **The status-bar chip:** "Sleep 23:10", in warning colour once the timer has warned.
- **When it fires:** the app stops or pauses every picture, including multi-view's second one.

## Settings ▸ Reminders
- **The pending list** (built by `settings.rs`) gets these additions:
  - the offset ("10 min in");
  - where a switch opens;
  - "from Following · ‹rule›";
  - a warning row under each reminder that clashes with another.
- **Scheduled playback:** "Open scheduled playback in" and "Fire while Tuner is closed".
- **Sleep timer:** its status, "Sleep timer…", "Turn off the sleep timer", and "Warn before the
  sleep timer stops".
