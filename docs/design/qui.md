# Up next UI (track qui)

Built on `batch/mid`. It turns on `queue::ENABLED`, so the engine (`queue::service::spawn`) now runs in the app.

## What there is

- **`src/queue/ui/`** (was `queue/ui.rs`)
  - `mod.rs`:
    - Adding from anywhere: `menu_items`, `popover_button`, and `on_action` for Ctrl+N / Ctrl+Shift+N on a film, an episode, a series, a programme or a channel. A channel adds its programme on now. A channel with no guide gets a toast and adds nothing.
    - `SeriesPicker`: next episode, next 3, the rest of the season, or the next 50.
    - `move_before`: reordering stays exact even when rows are hidden from the snapshot (see below).
    - Remove with Undo: `remove`, plus `undo` and `undo_in`.
    - Ctrl+O: `copy_choices` pins a copy for a row.
    - `mini_chip` for the mini player.
    - `snapshot`: reads the snapshot through the spoiler guard. It also starts one repaint watcher per app, so a countdown that starts while the window is idle still gets drawn.
  - `card.rs`: the card between items, built from the snapshot.
    - Cases: the countdown, a failed item, the end of a live slot, the next-episode countdown, the PIN wait, a programme that hasn't started, three failures in a row, multi-view, and the finished card (which offers the next episode).
    - The player, the pop-out strip, the screen's banner and the mini chip all show it with the same buttons and keys.
  - `list.rs`: the list the Up next screen and the player's `u` panel share.
    - Sections Played / Now / Next, and the sleep marker with "late" rows after it. The cursor never lands on a section line.
    - Marks, type-to-filter (kept in play order), Alt+↑/↓, a drag handle with an insertion line and auto-scroll near the edges, and a ✕ on hover.
- **`ui/screens/queue.rs`** (Alt+6): the summary, the card banner (or "Carry on" after Up next stepped aside), the list, and at Wide a detail pane. The row menu has:
  - play now, play from the start, play next, move to the top or the bottom, stop after this one;
  - use another copy, remove, remove played, shuffle the rest, clear (with a confirm).
- **The player.**
  - `cards.rs`:
    - Draws the Up next card from the snapshot. Its Esc comes after the panel and number entry and before the OSD steps; that includes `LeaveOsdControls`, and the pop-in key when popped out.
    - Handles `u`, `n` and `N` (with flashes when they can't act), the PIN flow, and Undo.
    - The old next-episode card is only a fallback now, for when the engine isn't running.
  - `panels.rs`: `PanelKind::Queue`, the same list in OSD colours with an "n to go · ends ~hh:mm" footer.
- **Sidebar**: "Up next" with its count, both expanded and in the rail.
- **Home**: an "Up next" rail first, while items are still to play and nothing is playing. Its tile reads "Resume / Start Up next · title · n more"; Enter carries on in the player.
- **Settings ▸ Interface ▸ Playback**: "Play the next item after" with At once, 5, 10, 20 or 30 s, or Ask.

## Decisions

- **Moves.** `Insert::Index` counts concealed rows, which the snapshot never shows. So a move is "the rows to the end", then "everything that must follow them to the end", and the visible order comes out exact whatever is concealed. This is tested against the store's move.
- **Undo.** Undo adds the rows again at the end, then moves them before the row that used to follow them. Undo numbers start at `0xF000`, far above the copies' numbers.
- **The mini chip.** It has no `UiCtx`, so its ✕ sends through the engine of the app whose UI it saw last.
