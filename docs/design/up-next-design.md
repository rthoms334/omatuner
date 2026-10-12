# Play queue ("Up next"): design

> The design as approved on 2026-10-11, kept for its user stories, mockups and numbered rules (code comments cite them as "DESIGN §32 §x.y"). The answers to its open questions and what was built differently are in DESIGN.md §32 (Up next).

Built on origin/main 013f897 and the in-flight work: P3 copies (`feat/p3-copies`, which adds migration 0007), pane arrows (`feat/pane-arrows`) and the MCP pass (`feat/mcp`, DESIGN §30). It is meant to become DESIGN §31.

**What exists today that this builds on**
- **Next-episode card.** It lives in `src/ui/screens/player.rs`:
  - `NextEpisode`, `end_of_playback`, `find_next_episode`, and a 10 s countdown run from `PlayerScreen::ui`.
  - It only runs while the fullscreen player is drawn (`on_event` returns early for other placements).
  - Its countdown never runs in the mini player, in the pop-out, or with the window on a hidden workspace.
- **Player events.**
  - The player reports `PlayerState::Ended` only at the real end of a movie, episode or catch-up (EOF).
  - Stop goes to `Idle`.
  - `PlayerSnapshot` already carries `origin`, `playable.item` and `last_position`.
- **Copies.**
  - `CopyPlay {fallback, elsewhere, start_at}` and `store::copies::plan` already decide which copy plays and handle fallbacks.
  - `same_episode` finds the same episode in another copy.
- **Shared state from the MCP pass.**
  - `AgentShared::{library(), parental()}` reads the library snapshot and the parental state from tokio while the UI isn't running.
  - The existing `ExplicitGate` is shared the same way.
- **Free keys** (checked against `keymap.rs` on main and the three branches):
  - `Ctrl+N`, `Ctrl+Shift+N` and `Alt+6` are unused everywhere.
  - `u`, `n` and `N` are unused in `PlayerLive`, `PlayerVod` and `PopOut`.
  - P3 only adds `Ctrl+O`, and MCP only adds the unbound `CancelSwitch`.

---

## 1. User stories

1. **Marathon.** "Queue S2 E3–E5 of Slow Horses, then stop."
   - The episodes play back to back with the Up next card between them.
   - After E5 playback stops and the usual next-episode countdown does **not** start.
2. **Franchise.** "Bond in release order."
   - The user filters Movies to "bond", then chooses "Add the 25 shown to Up next, by year", or asks Claude to queue them.
   - Each film plays from whichever playlist the copy plan picks at the time it plays.
3. **Kids line-up.** "Three Bluey catch-ups and a film, done by 7:30."
   - The queue shows "ends 19:22".
   - The sleep timer is set to "after the queue".
   - A locked film in the line-up stops and asks for the PIN; it is never skipped silently.
4. **Add from anywhere.**
   - `Ctrl+N` adds to the end and `Ctrl+Shift+N` plays next.
   - Both work on a poster, detail page, search hit, guide programme or channel row.
   - Mouse: right-click › Add to Up next / Play next, or the `Up next` button on detail pages and the guide's programme card.
5. **Rearrange.** Alt+↑/↓ moves items, the mouse drags the handle, Delete removes with Undo, and there is "Shuffle the rest".
6. **Restart-safe.** If Tuner quits halfway through Goldfinger, the next day Home shows "Resume Up next · Goldfinger · 1 h 09 left · 5 more". Enter resumes at 41:20.
7. **Interruption.** If I zap to the news, or a reminder auto-switches, the queue steps aside without losing its place. `Alt+6` › Enter carries on.
8. **Copies.** A queued movie plays from the best copy when its turn comes. "Use another copy…" (`Ctrl+O` on a row) pins the copy for that row.
9. **Claude.** "Queue the Mission Impossibles in order and start them" goes through the `queue` tool. Locked items are refused and explicit items don't exist while hidden.
10. **Hidden window.** Auto-advance works in the mini player, the pop-out, and on a hidden workspace (the engine runs on tokio, not in the frame loop). Media-key Next skips to the next queue item.

---

## 2. UX

### 2.1 Where the queue shows up
| Place | What |
|---|---|
| **Up next screen**, `Alt+6`, sidebar entry "Up next" with a count | The full editor: played, current and next, with ETA, sleep marker and detail pane (Wide) |
| **Player side panel** `u` | Same list widget, contexts `[List\|ListFiltering, PlayerVod\|PlayerLive]`, like `c`/`g`/`h` |
| **Up next card** (replaces the next-episode card) | Countdown between items: Play now / Up next / Cancel |
| **Mini player / pop-out strip** | "Next in 8 s ✕" chip |
| **Home** | "Up next" tile first in the top rail while the queue has pending items and nothing plays |
| **Status bar hint** | `↵ play · Alt+↑↓ move · Del remove · Ctrl+↵ more · ? help` |

### 2.2 Up next screen, Wide (1600 pt, sidebar expanded)
```
┌──────────────┬────────────────────────────────────────────────────────────────┬──────────────────────────────────────┐
│ 󰋜 Home        │ UP NEXT   5 to go · 9 h 41 · ends ~03:12        bond▏ (type to filter)│ ┌───────┐ Thunderball (1965)         │
│ 󰔂 Live TV     │ ─ Played (2) ─────────────────────────────────── Ctrl+↵ Remove played │ │       │ ● TV · 2 h 10 · ★ 7.0 · Action │
│ 󰃰 Guide       │  ✓ Dr. No (1962)                         ● TV     1 h 50              │ │poster │ Bond heads to the Bahamas to   │
│ 󰎁 Movies      │  ✓ From Russia with Love (1963)          ● TV 2   1 h 55              │ │       │ recover two stolen atomic …    │
│ 󰟴 Series      │ ─ Now ─────────────────────────────────────────────────────────────── │ └───────┘                              │
│ 󰐑 Up next   5 │  ▶ Goldfinger (1964)                     ● TV 2   41:20 / 1:50 ▕███░░▏│ Plays from   TV (best copy)   Ctrl+O   │
│              │ ─ Next ────────────────────────────────────────────────────────────── │ Starts       from the start           │
│ 󰍉 Search      │ ▸⠿ Thunderball (1965)                     ● TV     2 h 10   ~00:45     │ Copies       TV · TV 2 (FHD)           │
│ 󰒓 Settings    │  ⠿ You Only Live Twice (1967)        󰌾   ● TV     1 h 57   ~02:42     │                                        │
│              │  ⠿ Bluey · Keepy Uppy  ⟲ CBeebies Sat 07:10  ● TV 2  7 min   ~02:49    │ [▶ Play now] [Play next] [✕ Remove]    │
│              │ ─ 󰒲 Sleep timer 03:00 ─────────────────────────────────────────────── │                                        │
│              │  ⠿ On Her Majesty's Secret Service (1969) ● TV  2 h 22   ~05:11 ⚠ late │                                        │
│              │  ⠿ Diamonds Are Forever (1971)            ● TV  2 h      ~07:11        │                                        │
├──────────────┴────────────────────────────────────────────────────────────────┴──────────────────────────────────────┤
│ ↵ play · ⇧↵ fullscreen · Alt+↑↓ move · Del remove · Ctrl+O copy · Ctrl+↵ more · ? help        Guide updated 2 h ago│
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```
- **⠿ drag handle.** It shows on hover and on the cursor row only.
- **Row marks.**
  - 󰌾 means the item is locked: the row is dimmed while the session is locked.
  - ⟲ means a catch-up programme: the row shows its channel and air time, plus "expires Thu" when it is within 24 h of expiry.
- **Times.** The right column is the ETA. `?` means unknown duration, and the header then reads "ends ≥ 03:12".
- **Sleep marker.** It is drawn where the in-app sleep timer (MCP pass) falls. Items after it are flagged "late".
- **Copy mark.** `● TV` shows the copy the plan would pick now, or the pinned copy in bold.

### 2.3 Up next screen, Medium (~950 pt half screen, 48 px rail, no detail pane)
```
┌───┬──────────────────────────────────────────────────────────────────────┐
│ 󰋜 │ UP NEXT  5 to go · ends ~03:12                      type to filter │
│ 󰔂 │ ─ Now ────────────────────────────────────────────────────────────── │
│ 󰃰 │  ▶ Goldfinger (1964)                    ● TV 2   41:20 / 1:50       │
│ 󰎁 │ ─ Next ───────────────────────────────────────────────────────────── │
│ 󰟴 │ ▸⠿ Thunderball (1965)                   ● TV     2 h 10   ~00:45   │
│ 󰐑5│     from the start · best copy (TV) · Ctrl+O copy · Ctrl+I details │ ← cursor row grows a 2nd line
│   │  ⠿ You Only Live Twice (1967)      󰌾   ● TV     1 h 57   ~02:42   │
│ 󰍉 │  ⠿ Bluey · Keepy Uppy   ⟲ Sat 07:10    ● TV 2   7 min    ~02:49   │
│ 󰒓 │ ─ 󰒲 03:00 ────────────────────────────────────────────────────────── │
│   │  ⠿ On Her Majesty's Secret Service      ● TV     2 h 22   ⚠ late   │
├───┴──────────────────────────────────────────────────────────────────────┤
│ ↵ play · Alt+↑↓ move · Del remove · Ctrl+↵ more                         │
└──────────────────────────────────────────────────────────────────────────┘
```
Compact (under 700 pt) is the same list without the ETA column. "Played" collapses to one row, "✓ 2 played".

### 2.4 Player: Up next panel (`u`) and the card (1600 pt fullscreen)
```
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│ 󰁍  Goldfinger (1964)                                                   ● TV 2   23:51   │
│                                                                    ┌ UP NEXT (u) ──── ✕ ┐│
│                                                                    │ ▶ Goldfinger  1:50 ││
│                               VIDEO                                │▸ Thunderball  2:10 ││
│                                                                    │  You Only… 󰌾 1:57 ││
│                                                                    │  Bluey ⟲     0:07 ││
│                                                                    │  4 more · ~03:12   ││
│                                                                    └────────────────────┘│
│ ▕██████████████████████████████████████████████████████████████████▏  1:49:40 / 1:50:02│
│ 󰒮 󰐊 󰒭  −10 +10  󰗅 English  󰨖 Off  󰐑 Up next  󰒭 Next   󰋽 Info   󰕾 ━━━●─┊─ 80 %         │
└──────────────────────────────────────────────────────────────────────────────────────────┘

After EOF (the card that replaces "Next episode in 10 s", same place, same OSD button style):
                                                         ┌─────────────────────────────────┐
                                                         │ Up next in 8 s · 3 of 7         │
                                                         │ Thunderball (1965) · 2 h 10     │
                                                         │ ● TV · ends ~02:05              │
                                                         │ [▶ Play now · Enter] [󰐑 List · u] [✕ Cancel · Esc] │
                                                         └─────────────────────────────────┘
Locked next:  "Up next is locked · You Only Live Twice"  [󰌾 Enter PIN · Enter] [Skip · n] [Cancel · Esc]
Queue done:   "That was Up next."  (series ended mid-season: [▶ S02E06 · Enter] — no countdown)
Failed item:  "Couldn't play Thunderball (TV and TV 2) — next in 10 s"  [Play now] [Cancel]
Live slot end:"Bake Off has ended — up next in 30 s"  [Play now] [Keep watching · Esc]
Future slot:  "Up next: Bake Off at 20:00 on BBC One — in 25 min"  [Skip for now · n] [Cancel · Esc]
Mini player:  ┌ video ─────────┐   Pop-out strip:  Goldfinger · Next in 8 s  [▶] [✕]
              └ Next in 8 s  ✕ ┘
```

### 2.5 Keyboard model
| Context | Key | Action (new `Action` variant) |
|---|---|---|
| `List`, `ListFiltering`, `PosterGrid(Filtering)`, `Detail` (all `ITEM_ACTIONS`), `Search` (`SEARCH_ITEM_ACTIONS`), `Guide` | **`Ctrl+N`** | Add to Up next (end), `QueueAdd` |
| same | **`Ctrl+Shift+N`** | Play next (after current), `QueuePlayNext` |
| `Global` | **`Alt+6`** | Up next screen, `GoQueue` (fits the Alt+digit screens) |
| `PlayerVod`, `PlayerLive` | **`u`** | Up next panel, `QueuePanel` |
| `PlayerVod`, `PlayerLive` | **`n`** / **`N`** | Next / previous queue item, `QueueNext` / `QueuePrev` (no-op plus a flash "Up next is empty" when the queue is empty) |
| Up next list (`List` + `ITEM_ACTIONS`) | ↑↓ PgUp PgDn Home End | Move |
| | `Enter` / `→` | Play from here (playing → fullscreen); `Shift+Enter` plays fullscreen |
| | `Alt+↑` / `Alt+↓` | Move the item (existing `MoveUp`/`MoveDown`). Refused while filtering, with the toast "Clear the filter to reorder" |
| | `Ctrl+Shift+N` | Make this the next item |
| | `Space` / `Insert` | Mark rows. Delete and Ctrl+↵ › Move to top then act on all marked rows |
| | `Delete` | Remove, with an Undo toast (the existing `UndoStash` pattern) |
| | `Ctrl+O` | Use another copy… (pins the row's copy) |
| | `Ctrl+I`, `Ctrl+W`, `Ctrl+L` | Details, watched, lock (existing) |
| | `Ctrl+Enter` / `Shift+F10` | Menu: Play now · Play from the start ✓ · Play next · Move to top / bottom · Stop after this one · Use another copy… · Remove · Remove played · Shuffle the rest · Save as favourites list… · Clear Up next… |
| | `Tab` / `Shift+Tab` | List ↔ detail-pane buttons (Wide) |
| | `←` / `Backspace` | Nothing to the left on the screen. In the player panel, close it (pane arrows) |
| | letters | Filter (launcher idiom) |

- **Esc ladder (inserted step).** clear filter → close overlay / side panel → **cancel an Up next countdown or a drag in progress** → leave OSD controls → hide banner → leave fullscreen → leave player → route back.
  - The player already checks number entry and the side panel before the card, so the order matches the code.
  - **Pop-out:** "Esc pops in once a side panel, number entry or controls focus has had it" now also lists the countdown.
- **Card keys:** `Enter` / `i` play now (as today), `n` plays now or skips the locked item, `u` opens the panel, and `Esc` cancels.
- **Mouse.**
  - Every key above has a menu item or a button.
  - Rows show the ⠿ handle on hover. Dragging the **handle** reorders the row:
    - an accent insertion line shows where it will land, and the list auto-scrolls within 24 px of an edge;
    - Esc or a drop outside the list cancels the drag.
  - A click selects and a double-click plays. Under the existing double-click safety rule, a reorder never comes from a body click.
  - A ✕ on hover removes the row. Right-click opens the row menu.
  - The OSD row gets `󰐑 Up next` and, when the queue has a next item, `󰒭 Next` (tooltips show the keys).
  - Detail pages get a `[󰐑 Up next]` button after Resume / Start over. The guide programme card gets `Up next`.
  - The Movies grid menu has "Add the N shown to Up next, by year" (only when ≤ 50 are shown).
  - Favourites lists have "Queue this list" (in list order).
- **MPRIS.** While the queue drives playback (`origin == Queue`), Next and Previous go to the queue and CanGoNext = "the queue has a next item". Live zapping is unchanged otherwise.

---

## 3. Behaviour rules

| Rule | Decision |
|---|---|
| **What can be queued** | Movies, episodes, catch-up programmes, and live **programme slots** (a channel plus a guide programme). A bare channel with no guide is refused with the toast "Live channels join Up next by programme; this one has no guide". |
| **Programme slot at its turn** | Airing → play live (from the start through catch-up if the row says "from the start" and catch-up exists); advance at EPG stop with a 30 s card ("Keep watching · Esc"). Ended and inside the catch-up window → catch-up. Expired → skipped, reason on the row. Future → wait card, "Skip for now" leaves it pending. Timed switches ("at 20:00 regardless") stay with the reminder auto-switch. |
| **Series** | "Add to Up next" on a series asks: Next episode · Next 3 · Rest of season · Next 50. Rows are concrete episodes (predictable, editable). |
| **Duplicates** | A title already pending gets the toast "Already in Up next (4th) · Play next?". A played title can be queued again. |
| **Order of play** | Next = the first *visible*, pending row after the current row. Concealed rows stay pending and are never counted. |
| **Resume** | When advancing, a partly watched item resumes silently (above `resume_min_s` and below `watched_pct`, with a 3 s flash "Resuming at 12:30 · Home = start"). A watched item starts from 0. A row's "Play from the start" setting wins. No resume dialog mid-queue. Enter on a row uses the normal Resume prompt. |
| **Which copy** | Decided **at play time** by `store::copies::plan` (pin → latest progress → trouble last → language → playlist order), with `CopyPlay.fallback`/`elsewhere` as today. A row with a pinned copy (`any_copy = 0`) plays that copy, then falls back. Episodes use `same_episode` for the fallback. |
| **Played rows** | Stay greyed under "Played (n)". "Remove played" clears them. They are pruned after 7 days, and the oldest go first when a new item would pass the 200-row cap. |
| **End of queue** | Stop. The old next-episode countdown does **not** start. The card offers the next episode without a countdown. |
| **Empty queue + episode ends** | Exactly today's 10 s next-episode card, now run by the engine (so it also works in the mini player, pop-out and hidden window). |
| **Interruption** | The user plays something else, or a reminder auto-switch fires → the queue steps aside (the current row keeps state `current` and its progress). Stop (`Idle`) never advances. Toast: "Up next paused · Alt+6 to carry on". |
| **Stop after this one** | Session-only flag. The card says "Stopped after Goldfinger, as asked". |
| **Sleep timer** (MCP pass) | Adds `end_of_queue` and `after_items(n)` (counted in queue items while the queue runs). A timer at a fixed time stops mid-item and keeps the queue's place. |
| **Failures** | Copy fallbacks first (player). If a whole item fails, the row is marked `failed` and the card counts down to the next item. **3 failed items in a row pause the queue** ("Up next paused: 3 items failed in a row; provider trouble?"), so it never hammers a provider overnight. |
| **Restart** | The queue is restored and never auto-plays. Home tile: "Resume Up next". |

---

## 4. Data model and migration

`src/store/migrations/0008_play_queue.sql`. P3 owns 0007. The follow-rules and guide-fix features are designed in parallel, so the number is assigned at merge.
```sql
-- Play queue (DESIGN §31). User data: fp + orphaned_since like favourites (store::user HOLDERS).
CREATE TABLE IF NOT EXISTS queue_item (
  id          INTEGER PRIMARY KEY,
  position    INTEGER NOT NULL,                 -- dense 0..n-1, rewritten in one tx on move (≤ 200 rows)
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL CHECK (kind IN ('movie', 'episode', 'channel')),
  item_key    TEXT    NOT NULL,
  any_copy    INTEGER NOT NULL DEFAULT 1,       -- 1: the title's copy plan picks at play time; 0: this copy
  prog_start  INTEGER,                          -- channel rows: the programme (displayed UTC times;
  prog_stop   INTEGER,                          --   programme.id changes every EPG import)
  catchup_id  TEXT,
  title       TEXT    NOT NULL,                 -- captured for display if the item vanishes; never logged at INFO
  subtitle    TEXT,                             -- "S02E05 · …" / "BBC One · Sat 07:10"
  duration_s  REAL,                             -- ETA; NULL = unknown
  from_start  INTEGER NOT NULL DEFAULT 0,
  state       TEXT    NOT NULL DEFAULT 'pending'
              CHECK (state IN ('pending', 'current', 'played', 'skipped', 'failed')),
  added_at    INTEGER NOT NULL,
  added_by    TEXT    NOT NULL DEFAULT 'app' CHECK (added_by IN ('app', 'agent')),
  done_at     INTEGER,
  fp          TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  CHECK ((kind = 'channel') = (prog_start IS NOT NULL AND prog_stop IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS queue_item_order ON queue_item(position);
CREATE UNIQUE INDEX IF NOT EXISTS queue_item_one_current ON queue_item(state) WHERE state = 'current';
```
- **Other store changes.**
  - Add `("queue", "queue_item", "kind", "item_key", "")` to `store::user::HOLDERS`, so re-keyed items reattach and orphans list under Settings ▸ Orphans.
  - Orphaned rows show "Unavailable" and are skipped.
  - `store::backup` exports and restores the table.
- **Deleting a playlist.** Rows cascade. P3's "move favourites and progress to matched copies" offer also moves `any_copy = 1` rows.
- **Model.** `model/ids.rs` gets `row_id!(QueueId)` (append). `model/playable.rs` gets `PlayOrigin::Queue` (append; a frozen file, so it needs architect sign-off).
- **Config.** New `[queue] countdown_s = 10` (0 = immediate; Settings ▸ Playback "Play the next item after [10 s ▾]", with an "Ask" option). No other settings.
- **Rust types** (`src/queue/model.rs`):
  - `QueueEntry { id, item: ItemRef, any_copy, programme: Option<Slot{start, stop, catchup_id}>, title, subtitle, duration_s, from_start, state, added_by }`
  - `QueueSnapshot { rows: Arc<[QueueRow]> /* visible only */, current, next, countdown: Option<Countdown{id, at, why}>, blocked: Option<Blocked>, stop_after, eta_end: Option<(Timestamp, exact: bool)>, interrupted, failed_streak }`
  - `QueueCommand { Add{targets, at: Insert::{End,Next,Index}, by}, Move{ids, to}, Remove(ids), Clear{played_only}, PlayAt(id), Next, Prev, Now, Cancel, StopAfter(Option<id>), FromStart(id, bool), PinCopy(id, ItemRef), Proceed{unlocked}, Resume }`

---

## 5. Engine and architecture

```
 UI thread ──QueueCommand (non-blocking mpsc)──▶ queue-engine task (tokio) ──PlayerCommand::PlayCopy──▶ controller
 MPRIS (Next/Prev when origin==Queue) ─────────▶   QueueMachine (pure)        ◀── watch<PlayerSnapshot> ──┘
 IPC v2 / MCP handler ──cmd + oneshot reply────▶   timers (countdown, slot     ── Writer::submit ──▶ store writer
                                                    stop/start, ≤60 s wall-clock re-check, suspend-safe)
        ◀────────── watch<Arc<QueueSnapshot>> (UI panel/screen/card/mini chip, MPRIS, Home, agent) ────
 reads per operation: AgentShared::library() (never held across awaits), parental(), ExplicitGate
```
- **`QueueMachine`** (`src/queue/machine.rs`) is pure and unit-tested like `PlayerMachine`:
  - Inputs are player snapshot transitions, commands, ticks, parental/explicit changes and a library swap.
  - Effects are `Play(CopyPlay)`, `Persist(op)`, `Publish`, `Toast/Notify`.
  - **Ownership test:** `snap.origin == Some(Queue)` and `snap.playable.item` is the current row's item or one of its copies.
  - `Ended` → mark played and start the countdown (or stop: stop-after, sleep, end of queue).
  - `Error` once reconnects are exhausted → mark failed.
  - A foreign `NowPlaying` or `Idle` → interrupted.
- **The next-episode card moves from `PlayerScreen` into the engine** (as the empty-queue case).
  - `PlayerScreen` only draws `QueueSnapshot.countdown`, `blocked` and the "done" state, and sends commands.
  - `next_episode`, `next_lookup` and `find_next_episode` are deleted.
- **PIN.**
  - The engine never asks for the PIN. A locked next item with the session locked → `blocked = NeedsPin`, and no `Play` is emitted.
  - The UI's PIN dialog → `Proceed{unlocked: true}`.
  - With the window hidden, a notification "Up next is waiting for your PIN" (title only when not explicit, which a locked item isn't by definition).
- **Play-time work** runs on `spawn_blocking` with a store reader:
  - one indexed `ItemRef` → id lookup;
  - `copies::plan` (a few indexed reads per copy);
  - the catch-up window check.
  - It happens once per advance, pre-resolved when the countdown starts. There is **no stream prefetch**: max_connections = 1 per account, and a second decoder would cost about 0.86 core per 4K HEVC stream on the M2.
- **Scale** (79k channels, 302k movies):
  - The queue never scans a catalogue: add is ≤ 50 indexed lookups, and ETA and filtering run over ≤ 200 rows in memory.
  - The snapshot is ≤ 200 small rows. The UI draws visible rows only, with posters through the image cache, and does no DB work per frame.
  - The engine takes `Arc<Library>` per operation and drops it, so it never pins an old 302k-movie snapshot (keeps the memory field fixes intact).
  - Each state change is one tiny write transaction.
- **Connection budget.** Exactly one `PlayCopy` per advance. Controller stop-before-start already covers a TV → TV transition. TV → TV 2 needs nothing. An at-limit playlist sorts last in the plan.
- **Startup and shutdown.** Start after the player, inside `start_integrations`. On stop, save nothing extra (the rows are written through).

---

## 6. Guardrails

- **Parental lock.**
  - Locked items can be queued in the app (they are visible and dimmed) but need the PIN at play time.
  - `n` skips a locked item, and Esc stops.
  - Over MCP, locked items can never be added or played: the reply is "locked: needs your PIN in Tuner".
- **Explicit while hidden.**
  - Concealed rows (`Library::conceals`) are absent everywhere:
    - rows, counts, "3 of 7", ETA, the Home tile;
    - MPRIS next metadata, the MCP `list` and toasts.
    - There is no "1 hidden".
  - The engine skips them. A relock that stops explicit playback reads as `Idle`, so there is no advance and nothing is named.
  - Rows stay pending and reappear once explicit content is shown again.
- **Privacy.**
  - No titles at INFO. The log reads `queue: #12 → #13 (episode, source 2)`.
  - No URLs in any queue result.
  - The backup includes the queue, under the same "keep backups private" note as reminders.
- **Connection limits.** One stream at a time, no prefetch, one fallback per copy, and a pause after 3 failures in a row.
- **Caps.**
  - 200 rows, 50 per add.
  - MCP adds count as actions (rate-limited, logged at INFO without titles).
- **Spoilers** (for the later app-wide spoiler guard): rows only use `title` and `subtitle`. The guard hook blanks `subtitle` for matched sport and reality rows, and the queue renders no plot.

---

## 7. MCP and skills

**One `queue` tool**, with the same action shape as `reminders` and `sleep_timer` in the in-flight pass. If §30 prefers flat names, it maps 1:1 onto `queue_add/list/move/clear/play`.

| action | params | result |
|---|---|---|
| `list` | `include_played?` | `{items:[{id, ref, kind, title, s?, e?, duration_s?, state, position_s?, locked, from_start, copy:{playlist, pinned}, slot?:{channel, start, stop, catchup:{available, expires_at}}}], current_id, next_id, eta_end, exact, countdown_s?, stop_after_id?, sleep_at?, interrupted}` |
| `add` | `refs[]` (≤ 50, item or programme refs), `at: end\|next\|<index>`, `from_start?`, `copy: best\|this` | `{added:[{id, ref}], refused:[{ref, error:"locked: needs your PIN in Tuner" \| "not_found" \| "duplicate" \| "full"}]}` |
| `move` | `ids[]`, `to: <index>\|top\|bottom\|next` | `{items}` |
| `remove` | `ids[]` | `{removed}` |
| `clear` | `played_only? = true` | `{removed}` |
| `play` | `id?` (default next) | `{playing \| waiting_for_pin:true}`. It replaces the current playback, so `status` is called first (RULES 2) |
| `skip` / `stop_after` | `—` / `id?` | `{current}` / `{stop_after_id}` |

- `status` gains `queue:{current_id, next_title, left, eta_end}`.
- `sleep_timer` gains `end_of_queue` and `after_items`.
- An explicit ref while hidden returns `not_found`, so it is indistinguishable from a missing item.

**RULES.md additions:**
- Adding more than 3 items, `clear`, and `play` (which replaces playback) each take one confirmation.
- Claude never clears unplayed items without asking.
- Order claims like "release order" are labelled as Claude's own knowledge.
- Spoiler-safe mode lists titles only.

**Skill changes:**
- **pick:**
  - "Which Mission Impossibles, in order" offers "Queue them: release or story order?".
  - It runs `search` per title and `details.copies`, then `queue add` (confirmed once, numbered list).
  - "Surprise me ×3" queues three picks.
- **series:**
  - "Next 3 then stop" → `episodes` → `queue add` ×3 → `play`.
  - "Marathon season 2" → the rest of the season, capped at 50.
  - The tracker shows queued episodes.
- **kids:**
  - Bedtime line-up: fit items to the time left minus 5 min (`duration_s`) → `queue add` → `sleep_timer end_of_queue`.
  - Locked items are left out. Claude tells the parent that a child can skip or cancel in the app.
- **plan:**
  - The VOD and catch-up parts of an evening plan go into the queue. Live anchors stay reminders.
  - Clash check against `queue.eta_end`: "the 21:00 switch interrupts Goldfinger at 41 min".
- **watch:** "Catch up on the last 3 Bake Offs" → `guide_search catchup_only` → `queue add` in air order → `play`.
- **fix:** reads `failed` rows and `failed_streak` in `diagnose`.

---

## 8. Tests worth having

These test real behaviour, not getters.

**Engine** (pure `QueueMachine`, fake clock and snapshots):
1. EOF → countdown → exactly one `PlayCopy` for the next row. `Now` skips the wait. `Cancel` keeps the next row pending and emits nothing.
2. A foreign play mid-item interrupts the queue (current row kept). `Resume` replays at `last_position`. `Idle` from Stop never advances.
3. A locked next item with the session locked → `Blocked` and no `Play`. `Proceed{unlocked}` → `Play`. `n` skips the row as `skipped`.
4. An explicit row while hidden is absent from the snapshot rows, the counts and the ETA, and is skipped. After it is shown again it is still pending and in its place.
5. `stop_after`, the last item and the sleep timer's `end_of_queue` → no advance and no implicit next-episode countdown. With an empty queue, an episode end gives today's next-episode countdown.
6. Programme slots:
   - airing plays live and advances at stop with a 30 s card;
   - ended plays catch-up;
   - expired is skipped with a reason;
   - future waits until start.
   All of this runs across a simulated suspend (wall-clock jump).
7. Three failures in a row → paused. A success resets the streak.
8. Copies:
   - `any_copy` uses the plan order (pin, then latest progress);
   - a pinned row plays its copy;
   - an episode falls back through `same_episode`;
   - with TV at its limit (403 recently), the plan picks TV 2.

**Store** (temporary DB):
- Migration on a v7 DB.
- Move, remove and add keep positions dense.
- At most one `current` row (enforced by the unique index).
- A refresh that re-keys an item reattaches its row through HOLDERS. An item that vanishes becomes an orphan and is skipped.
- A backup round trip keeps the queue.
- Cap and prune order.

**UI** (existing `ui::testing` rig):
- Alt+↓ and a drag on the handle give the same stored order. A drag cancelled with Esc changes nothing.
- The Esc ladder cancels the countdown before it leaves OSD focus. In the pop-out, Esc cancels the countdown before it pops in.
- The countdown card, mini chip and pop-out strip are drawn from the snapshot with the player not fullscreen.
- Ctrl+N on a channel with no guide toasts and adds nothing.

**Keymap:**
- `Ctrl+N` and `Ctrl+Shift+N` resolve in every item context and in the guide. `u`, `n` and `N` resolve in both player contexts.
- The existing no-conflict test still passes. TextInput still owns Ctrl+N? No: Ctrl chords reach Global by rule, so a test pins that `Ctrl+N` is **not** in Global.

**MCP:**
- A locked ref is refused. An explicit ref returns `not_found` while hidden. The 50 and 200 caps hold.
- No `http` substring appears in any `queue` result (scan).
- Every schema example parses (the existing proto test).

**MPRIS:** with `origin == Queue`, Next sends a `QueueCommand::Next` and not a `Zap`.

---

## 9. Implementation plan

| Track | Files (ownership) | Size |
|---|---|---|
| **Q0 skeleton** (frozen API, ~0.5 day) | `src/queue/{mod,model}.rs` (types, `QueueHandle` = cmd sender + snapshot watch); `0008_play_queue.sql`; `model/ids.rs` (`QueueId`); `model/playable.rs` (`PlayOrigin::Queue`); `keymap.rs` actions and bindings | ~350 |
| **Q-core** | `src/queue/machine.rs` (+ tests), `src/queue/service.rs` (tokio task, timers, play-time resolve), `src/store/queue.rs` (+ tests), `store/user.rs` HOLDERS line, `store/backup.rs`, `services.rs` wiring, `platform/mpris.rs` Next/Prev routing, `config.rs` `[queue]` | ~1,500 |
| **Q-ui** | `src/ui/queue_list.rs` (shared virtualised list: drag handle, marks, filter), `src/ui/screens/queue.rs` (screen, detail pane), `ui/screens/player.rs` (card from snapshot, `u` panel, `n`/`N`, OSD buttons; remove the NextEpisode code), `ui/video_host.rs` + `ui/popout.rs` (chip and strip), `widgets/sidebar.rs` (entry + count), `screens/home.rs` (Resume tile), add-to-queue menu items and buttons in `movie_detail`, `series_detail` (episode-count picker), `movies` (add the N shown, by year), `live`, `guide` / `overlays/programme.rs`, `overlays/search.rs`, `ui/stacks.rs` (Add this copy) | ~1,700 |
| **Q-agent** (after MCP merges) | `agent/proto.rs` (`Call::Queue`), `agent/server.rs`, `agent/tools.rs`, `plugins/omatuner/skills/{pick,series,kids,plan,watch}/SKILL.md` + `RULES.md` | ~450 + ~120 md |
| **Docs** | DESIGN §31, §13.8 card text, §15 tables (`cargo run --example keymap_md`), MANUAL_CHECKLIST (hidden-workspace advance, PIN mid-queue) | ~250 |

- **Total:** about 4,000 lines including tests.
- **Order:** Q0, then Q-core and Q-ui in parallel (disjoint files; Q-ui codes against `QueueHandle` with a fake engine), then Q-agent once `feat/mcp` lands.
- **Prerequisites:**
  - `feat/pane-arrows`: ← semantics in the panel.
  - `feat/p3-copies`: per-copy plan and "move to matched copies".
  - `feat/mcp`: `AgentShared` parental and library sharing, `sleep_timer`. If MCP slips, Q-core lifts `AgentShared` into `services` itself.

---

## 10. Risks

1. **Moving the next-episode card into the engine changes behaviour.** The card now also shows in the mini player, the pop-out and a hidden window. Mitigation: same timings and keys, and a rig test for each placement.
2. **Merge conflicts.**
   - `PlayOrigin` (frozen) and `proto::Call` are shared with in-flight branches.
   - Migration numbers will collide with the parallel follow-rule and guide-fix designs.
   - Mitigation: append-only changes, and numbers assigned at merge.
3. **Drag-reorder in a virtualised egui list** (auto-scroll, a drop between unrendered rows) is the riskiest UI piece. Keyboard reorder ships first and the drag is behind the same `Move` command.
4. **EPG stop times for live slots overrun.** Mitigation: the 30 s "Keep watching" card, and Esc keeps the channel.
5. **A provider outage at night.** Mitigation: the pause after 3 failures in a row, and no prefetch.

## 11. Questions for you (each has a default I'd go with)

1. **Live TV in the queue?**
   - **Default:** yes, as programme slots only: live while it airs, catch-up after it ends, and a wait card if it hasn't started.
   - "Switch at exactly 20:00" stays a reminder auto-switch.
   - The alternative is movies, episodes and catch-up only.
2. **When the queue runs out after an episode, stop or carry on with the series?**
   - **Default:** stop, and offer the next episode with no countdown. "Play these 3 then stop" then means exactly that.
3. **Sidebar entry.**
   - **Default:** "Up next" appears in the sidebar only while the queue has items.
   - `Alt+6` and the player's `u` panel always work.
   - The alternatives are always shown, or never shown (keys and Home tile only).