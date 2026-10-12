# Up next engine (Q-core): notes for DESIGN §32

This file covers what the engine does and the contract the UI (Q-ui), MPRIS (MV-1), the sleep timer (remind) and the agent build on. The design is `designs/queue.md` with the batch plan's amendments. Where the build differs from the design, the difference is marked **(deviation)**.

## Files

| File | Contents |
|---|---|
| `src/queue/model.rs` | `QueueSnapshot`, `QueueRow`, `QueueEntry`, `QueueCommand`, `QueueReply`, `Refusal` |
| `src/queue/machine.rs` | `QueueMachine`, the pure state machine, with its unit tests |
| `src/queue/service.rs` | The tokio engine: `spawn`, `QueueHandle`, `EngineDeps`, `PlayerPort`, and play-time resolution |
| `src/store/queue.rs` | `queue_item`: load, add, move, remove, clear, set_state, from-start, pin copy, retime, prune, export and import |
| `tests/queue_engine.rs` | Storage tests, and an end-to-end engine test |

## Order of play

The next row is the first row in list order that is all of these:
- **pending**;
- **visible**: not concealed;
- **available**: not orphaned and still in the catalogue;
- **not deferred**: not a future slot the user chose to skip for now.

Played, skipped and failed rows stay where they are, so the UI's "Next" section is exactly the play order.

**Play next** inserts a row before the first pending row. **(deviation)** The design said "after current". The two only differ once rows have been moved around the current row. "Before the first pending row" keeps the rule "Next shows the play order" true.

## Ownership

Up next drives the main picture when both of these hold:
- `origin == Queue`;
- the playable's `ItemRef` is one of the refs the row was sent with: every copy of the title, or every ranked channel of a slot.

So these count as foreign, and Up next steps aside:
- a zap or `ZapTo` that keeps the Queue origin but moves to another channel;
- any other pick.

Stepping aside keeps the current row as `current` and shows the toast "Up next paused · Alt+6 to carry on", whose action is `GoQueue`.

`Idle` (Stop) never advances, and it cancels any countdown.

## Timers

All timers use wall-clock seconds:
- the countdown (`[queue] countdown_s`: `0` plays at once, and `ask` makes the card wait);
- the 30 s card at the end of a live slot;
- the wait for a future slot to start.

The engine sleeps until the next deadline, and at most 60 s. Each 60 s wake also re-reads the rows, which picks up outside changes: retime, prune, a refresh's orphans and a backup restore.

A suspend is caught on the next tick:
- a slot whose start has passed starts;
- a slot that ended within its catch-up window plays from the archive;
- a slot that ended outside the window is marked skipped with the note "Expired".

## Play-time resolution

Resolution runs on the blocking pool with a reader. It never holds the `Library` across an await.

| Row | How it resolves |
|---|---|
| Movie | `copies::plan` → `ui::copies::movie_command(…, PlayOrigin::Queue)`. `any_copy = 0` pins the row's copy. |
| Episode | The plan's first copy plays the same episode when that copy has it (`same_episode`), through `episode_command`. Another copy's progress gives `start_at`. |
| Slot | The row's channel plus its B5 copies (`channel_copies`, when `any_copy`), ranked by `rank_channels`. A row-pinned copy goes first, and channels with trouble in the last 24 h go last. Archive plays only consider channels whose catch-up reaches the programme. The fallback is the next channel, and `elsewhere` is the first one in another playlist. |

- Every play goes through B1 with `route(PlaySource::Queue, …)`. A refusal (multi-view) gives `Blocked::MultiView`, and the row plays when multi-view ends.
- **(deviation)** The design resumed silently "above `resume_min_s`". That is done by the resolver's own rule (`resume: true`), except when the row asks for "from the start". **Resume** of an interrupted row always resumes.

## Guardrails

- **Locked rows.** A locked row with the session locked gives `Blocked::NeedsPin`, and nothing plays.
  - `Proceed{unlocked: true}` re-resolves the row with the parental state read **again** from `AppShared`. The engine never unlocks anything.
  - When the window is hidden (the last snapshot's `video_suspended`), the engine sends the notification "Up next is waiting for your PIN". The title goes through `Guard::outside_title`.
  - `n` skips the row and marks it skipped with the note "Locked".
- **Concealed rows** (explicit content while hidden) are left out of `rows`, the counts, the ETA, `current`, `stop_after` and the cards. They are never played and stay pending. A card whose row becomes concealed closes.
- **Agent adds** (`AddedBy::Agent`) of locked items are refused with `locked: needs your PIN in Tuner`. Explicit items while hidden are refused with `not_found`, which looks the same as a missing item.
- **Logging.** Plays are logged without titles: `queue: #12 → #13 (episode, source 2)`. Adds are logged as `queue: added n (k refused, by app|agent)`.
- **Failures.** Three failed rows in a row give `Blocked::FailedStreak` and a toast. A row that reaches Playing resets the streak. `Resume` clears it.
- **Restart.** The rows load, and a row left `current` stays current for Home's "Resume Up next". Nothing plays.

## Empty queue and the end of the queue

- **An episode ends while Up next isn't driving playback.** The engine runs today's next-episode countdown. The countdown's `why` is `NextEpisode`, and `QueueSnapshot::next_episode` names the episode. When it runs out the engine sends `PlayerCommand::Play` with the ended item's origin. This works in the mini player, the pop-out and a hidden window. Q-ui deletes `PlayerScreen`'s own card (`next_episode`, `next_lookup`, `find_next_episode`) in the same commit that sets `ENABLED`.
- **The queue runs out.** `finished = EndOfQueue`. When the last row was an episode, `next_episode` offers its successor with no countdown.
- **Stop after this one.** `finished = StopAfter(id)`.

## Sleep timer (B6 hook 6)

The engine has no sleep timer of its own **(deviation)**: `reminders::sleep::SleepService` reads this snapshot and stops the pictures itself. Between two items (`QueueSnapshot::between_items`: a countdown to the next row, a PIN wait, a future slot, the failure pause) Up next hasn't run out, so "end of Up next" must not fire there. The list's sleep marker comes from the timer's status (`queue::ui::sleep_mark`).

## Measurements

These were measured with a debug-optimised test build on the dev machine, with 200 rows.

| Operation | Time |
|---|---|
| Add 4 × 50 rows (with fingerprints) | 4.2 ms |
| Load 200 rows | 0.27 ms |
| Move one row (rewrites the dense positions) | 0.41 ms |
| `QueueMachine::step` with a snapshot rebuild | 50 µs |
| A player snapshot with nothing new for Up next (outside the 30 s ETA step) | 17 ns, no rebuild |
