# Watchlists (play orders): design

> Draft for review, 2026-10-13. Built on origin/main b000794 (v0.3.0). It is meant to become DESIGN §42. Decisions answered so far and the ones still open are in §11.

**Why not "playlists":** films and series lists work like music playlists, but in Tuner "playlist" already means an IPTV source (TV, TV 2). So the feature is called **Watchlists**.

**The idea in one line:** a watchlist is a named list of films, series (or parts of one) and episodes, like a music playlist. A list is either **In order** (Star Wars: Tuner always knows where you are, so "play the next Star Wars" works whenever you like) or **Any order** (Christmas films: "play something from my Christmas list" picks one you haven't watched). Several lists can be on the go at once.

**It plays like a music playlist.** Start playing from a list and Tuner goes through it, with Up next's countdown between items. Like a music app, anything you add to Up next while a list plays comes first, and then the list carries on. If you play the same film from Movies, it's just that film.

**Favourites become channels only.** Films and series move to watchlists. A default **My list** watchlist takes the place of film favourites, and Ctrl+F still means "save this" everywhere (§4.1).

**How it differs from Up next**
| | Up next (§32) | Watchlist |
|---|---|---|
| Lifetime | Tonight's line-up. Rows are used up as they play and pruned after 7 days | Kept until you delete it. A list can take months |
| How many | One | As many as you like (Star Wars, Marvel, "Films for Sam") |
| Position | A `current` row that the engine moves | **Derived from what you've watched** (any copy, any way it was played) |
| Order | Always the queue's order | Per list: **In order** (next = the next one) or **Any order** (next = a pick among the unwatched) |
| Plays back to back | Yes (countdown between items) | When you start from the list, yes: the list becomes what Up next plays after its own rows (§3, Playing a list) |
| Live programmes | Yes (programme slots) | No (they expire). Films, series and episodes only |
| Titles you don't have yet | No | Yes, as "Not in your playlists" placeholders that attach when a refresh brings them in |

Up next keeps its job: Ctrl+N still adds whatever you're looking at to tonight's queue. A playing list **feeds** Up next one entry at a time, and those rows show which list they came from.

**What exists today that this builds on**
- **Title identity across playlists.** `movie/series.match_hash` (kind + folded title + year), `tmdb_id`, and the user's `title_link` joins and splits (§29.10–§29.16). `store::copies::plan` picks the copy that plays.
- **Watched state.** `watch_progress (source_id, kind, item_key, position, finished, updated_at)`. `store::copies::next_episode` finds a series' next episode across copies. `store::user::mark_watched` marks items watched or unwatched.
- **Favourites lists.** `fav_list` / `fav_item` already hold channels, films and series in order. The discover and pick skills save franchises there ("Trending", "which Mission Impossibles do I have"). Films in favourites show as Home rails (`RailKind::Favourites`), as the ★ on stack cards (`StackMarks.favourite`) and as the Favourite button on detail pages. They are lists **without** a position.
- **Up next.** `QueueCommand::Add {targets, at: Insert::{End,Next,Index}, by}` and the engine (PIN, copies, failures, countdown, B1 placement, `open_in`).
- **Agent.** `favourites`, `queue`, `continue_watching` and `play`; spoiler guard on every answer; RULES.md confirmations.
- **Free keys** (checked against DESIGN §15.2): `Alt+7`. Adding reuses `Ctrl+F` / `Ctrl+Shift+F` (§2.4).

---

## 1. User stories

1. **Two sagas at once.** "Star Wars in story order" (9 films, Rogue One, Solo, Andor S1–S2, The Mandalorian S1–S3) and "Marvel in release order" (33 films). On Monday I watch the next Marvel and on Tuesday the next Star Wars. Neither list waits for the other.
2. **Ask the agent.** "Play me the next Star Wars from my watchlist."
   - The agent finds the list, says "Next in Star Wars: *Rogue One* (2016), 2 h 13. You stopped at 41 min last time", and plays it, resuming.
   - If the next entry is a series range, it plays the next episode in that range ("Andor S1E4").
   - The list carries on after it like a music playlist. The countdown card between items lets me stop, and "just the one" makes the agent stop after it.
3. **Series inside a list.** An entry can be a whole series, one or more seasons ("The Mandalorian S1–S2"), or a single episode. The same series can appear twice ("Mandalorian S1–S2 … Book of Boba Fett … Mandalorian S3").
4. **It just knows.** I watched *A New Hope* last year from Movies, not from the list, and the list already shows it as watched. When I watch *Empire* through the TV 2 copy, the list moves on.
5. **Titles I don't have.** The agent builds "Marvel in release order" and I'm missing *The Incredible Hulk*. The row stays in place, dimmed, as "Not in your playlists", and the list skips it. If a refresh brings it in, the row attaches by itself.
6. **Marathon.** Enter on the Star Wars tile plays the next entry, then the next, with Up next's countdown between them. Each one shows a "Star Wars · 4 of 15" chip.
7. **Something extra mid-list.** While Star Wars plays, I press Ctrl+N on a short. It plays after the current film, and then Star Wars carries on.
8. **Rewatch.** "Start Star Wars again" restarts the list. Films watched before the restart count as unwatched **for this list only**. Nothing in History changes.
9. **Skip.** "Skip the Holiday Special" marks the entry skipped in this list. It isn't marked as watched anywhere.
10. **Add as I browse.** `Ctrl+F` on a film or series saves it to My list, the way it saves a channel to Favourites. `Ctrl+Shift+F` picks another list ("Star Wars", "New list…") and the place in it. Up next stays on Ctrl+N.
11. **A genre list.** "Christmas films" is an Any order list. "Play something from my Christmas list" picks one I haven't watched and carries on shuffling through the unwatched ones. "Something under 90 minutes from it" lets the agent choose by length.
12. **Where am I.** Home shows a "Your watchlists" rail with one tile per active list ("Star Wars · next: Rogue One · 4 of 15"). Enter plays the list from that entry.

---

## 2. UX

### 2.1 Where watchlists show up
| Place | What |
|---|---|
| **Watchlists screen**, `Alt+7`, sidebar entry "Watchlists" under Up next | The lists pane (name, progress, next title) and the selected list's entries |
| **Home** | A "Your watchlists" rail after Continue watching: one tile per list that has a next entry, ordered by most recently played from |
| **Detail pages** (film, series, episode) | A `[󰐕 Watchlist ▾]` button. When the title is in one or more lists, a line reads "In Star Wars (5 of 15) · Marvel" |
| **Up next rows** | A chip with the list's name and the entry's place ("Star Wars · 5/15") |
| **Up next screen and panel** | While a list plays, a "Then from Star Wars" section follows the queue's rows and shows the list's next few entries (greyed; they aren't rows yet) with "Stop after the queue" |
| **Up next card** between list items | "Next in Star Wars in 8 s · *Solo*" [Play now] [Stop the list · Esc]. Any order: "Another from Christmas films: *Elf*". At the end: "That was Star Wars ✓" |
| **Search** (`Ctrl+K`) | Watchlist names are hits: Enter opens the list |

### 2.2 Watchlists screen, Wide
```
┌──────────────┬───────────────────────────────┬──────────────────────────────────────────────────────────────────┐
│ 󰍉 Search      │ WATCHLISTS                 + │ STAR WARS · story order          5 of 15 · ~27 h left            │
│ 󰋜 Home        │ ▸ Star Wars        5/15     │ ─ Watched ──────────────────────────────────────── ✓ 4 · show ▾  │
│ 󰔂 Live TV     │     next Rogue One          │ ─ Next ───────────────────────────────────────────────────────── │
│ 󰃰 Guide       │   Marvel           12/33    │ ▸ 5 ⠿ Rogue One (2016)                ● TV 2   2 h 13  41:20 ▕██░▏│
│ 󰎁 Movies      │     next Black Panther      │   6 ⠿ Andor  S1–S2 · 24 episodes      ● TV     next S1E1           │
│ 󰟴 Series      │   Films for Sam    0/6      │   7 ⠿ A New Hope (1977)               ● TV     2 h 01              │
│ 󰐑 Up next   3 │     next Paddington         │   8 ⠿ Star Wars: The Holiday Special  ⊘ Not in your playlists     │
│ 󰓎 Watchlists  │   ✓ Bond (done)             │   9 ⠿ The Empire Strikes Back (1980)  ● TV     2 h 04   ↷ skipped │
│              │                             │  10 ⠿ The Mandalorian  S1–S2           ● TV     16 episodes  󰌾   │
│ 󰒓 Settings    │                             │  …                                                                │
├──────────────┴───────────────────────────────┴──────────────────────────────────────────────────────────────────┤
│ ↵ play · Ctrl+N add to Up next · Alt+↑↓ move · Del remove · Ctrl+W watched · Ctrl+↵ more · ? help                │
└───────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```
- **List mode.** The header shows `In order ▾` / `Any order ▾` (click or `o`). An Any order list hides the number column, shows unwatched first, and Enter on the header plays a pick.
- **Lists pane.** You arrange the lists yourself (`Alt+↑↓` or drag; stored in `sort_index`). My list is always first. Finished lists sink to the bottom as "✓ … (done)". Only the Home rail sorts by most recently played (decided 2026-10-14). `+` (or Ctrl+↵ › New list…) asks for a name.
- **My list** is always first in the lists pane and can't be deleted or renamed. It starts as Any order.
- **Entries pane.** The number column is the play order. Watched entries fold into one "✓ 4" row by default (the toggle is remembered per list).
- **The "next" entry** is the first unfinished one (§3). The cursor opens on it.
- **Row marks.** The progress bar shows a partly watched entry. Series rows show "next S1E4" or "3 of 8 watched". ⊘ marks a placeholder. ↷ marks a skipped entry. 󰌾 marks a locked entry. Copy marks work as in Up next.
- **Narrower layouts.** Medium shows the lists as a header switcher, "Star Wars ▾" (`Tab` cycles). Compact drops the time and copy columns.

### 2.3 Adding (`Ctrl+F` saves to My list; `Ctrl+Shift+F` on a film, series, episode or search hit opens this)
```
┌ Add to a watchlist ─────────────────────────┐
│ The Mandalorian                             │
│ ▸ My list            ✓                      │
│   Star Wars          (last used)            │
│   Marvel                                    │
│   New list…                                 │
│ ───────────────────────────────────────────── │
│ Series: ◉ Whole series  ○ Seasons [1]–[2]   │   ← series only
│ Place:  ◉ At the end    ○ After the next    │
│         ○ Choose position…                  │
└─────────────────────────────────────────────┘
```
- A film or episode that's already in the chosen list gets the toast "Already in Star Wars (7th)". A series can be added again as a separate range.
- **"Add the N shown to a watchlist, by year"** on the Movies grid menu (≤ 50 shown). It mirrors Up next's "Add the N shown".
- **Ticks toggle.** Lists the title is already in show ✓, and Enter on one removes it (with Undo), the same way the favourites lists dialog works for channels.
- **Up next's menu** gets "Save as a watchlist…".

### 2.4 Keyboard
| Context | Key | Action |
|---|---|---|
| `Global` | **`Alt+7`** | Watchlists screen (`GoWatchlists`) |
| Item contexts on a film, series or episode | **`Ctrl+F`** | Add to / remove from My list (`ToggleFavourite` keeps its key; on a channel it still means Favourites) |
| same | **`Ctrl+Shift+F`** | Watchlists… (the dialog in §2.3; on a channel it is still Favourites lists…) |
| Watchlist entries | `Enter` | Play the list from this entry (the normal Resume prompt applies) |
| | `Ctrl+N` / `Ctrl+Shift+N` | Add this entry to Up next / play it next |
| | `Ctrl+Shift+Enter` | Play just this one (no list after it) |
| | `Alt+↑↓`, drag the handle | Reorder |
| | `Delete` | Remove, with Undo |
| | `Ctrl+W` | Mark the title watched / unwatched (global, as today) |
| | `s` | Skip in this list / unskip |
| | `Ctrl+Enter` / `Shift+F10` | Menu: Play from here · Play just this one · Skip · Make next · Seasons… · Use another copy… · Find in my playlists… (placeholders) · Remove |
| Lists pane | `F2` / `Delete` / `Alt+↑↓` | Rename / delete (Confirm) / reorder. The menu adds "Play", "Start again…" and "Duplicate" |

The mouse gets a menu item or a button for every key, the same as Up next.

---

## 3. Behaviour rules

| Rule | Decision |
|---|---|
| **Entry kinds** | `movie`, `series` (whole series or a season range `season_from..=season_to`), `episode`, plus a **placeholder** for any of them (title, year, kind and optional TMDB id, with no copy yet). Live channels and programmes are refused: "Watchlists hold films and series; use Up next or a reminder for live TV". |
| **Title, not copy** | An entry follows the **title** (its stack: `match_hash`, `title_link` joins), like Up next's `any_copy = 1`. "Use another copy…" pins one copy for that entry. |
| **Done** | Film or episode: `finished` in **any** copy with `updated_at ≥ list.since`. Series range: every episode in the range is done that way (specials, season 0, are left out unless the range names season 0). Skipped entries count as done for the cursor and are shown as ↷. |
| **Any order lists** | "Next" is a random pick among the entries that are not done and are playable (a partly watched entry wins over a random one, so a film you stopped halfway comes back first). The agent can instead choose from `show` by mood, length or genre. Positions still exist so you can arrange the list, but they don't drive play. A series entry plays its own next episode. |
| **Next** (In order lists) | The first entry, in order, that is not done, is visible (not concealed) and is not a placeholder. A placeholder or a locked entry before it is passed over and named ("Skipping The Holiday Special: not in your playlists"). A locked entry is passed over only by the agent. The app shows it and asks for the PIN on play (§6). |
| **Next inside a series entry** | `copies::next_episode` limited to the range: the most recently progressed unfinished episode in the range, else the first unfinished one after the last finished, else the range's first episode. |
| **Resume** | Playing the next entry resumes a partly watched film or episode, as Continue watching does (the agent says "from 41 min"). Enter in the app shows the normal Resume prompt. |
| **Order and duplicates** | The order belongs to the user. Nothing is sorted automatically. A film can't appear twice in one list. A series can appear twice as long as the ranges don't overlap. One title can be in any number of lists. |
| **Watching outside the list** | It counts. Done is derived from `watch_progress` at read time, so there is no "played from the list" bookkeeping. |
| **Unwatching** | Ctrl+W (unwatched) on a title makes it "not done" in every list. The list's cursor moves back. |
| **Start again** | Sets `list.since = now` and clears the list's skips. History is untouched. Undo restores both. |
| **Playing a list** (decided) | Playing **from a list** (Enter on a list, an entry or a Home tile, or the agent's `watchlist play`) makes that list Up next's **context**. The entry goes in as Up next's next row and plays. When Up next has no pending rows left, the engine takes the list's next entry (§3, Next), adds it as a row and counts down as usual. Like a music app: rows you add to Up next play first, then the list carries on. |
| **What ends a list** | The list runs out ("That was Star Wars ✓"), Esc or "Stop the list" on the card, "Play just this one", the agent's `stop_after`, the sleep timer's `end_of_queue`, or starting another list (it replaces the context). Playing something else steps aside as Up next does today ("Up next paused · Alt+6 to carry on"), and the context is kept. |
| **Playing elsewhere** | The same film played from Movies, Search or a detail page has no context, so nothing follows it, exactly as today. |
| **Up next link** | Rows added from a list carry `watchlist_item_id` (the chip). Removing the list ends the context and leaves its rows in Up next without their chip. |
| **Restart** | Nothing auto-plays after a restart (as in §32). The Home tile reads "Resume Star Wars" and carries on with the context. |
| **Finished list** | When nothing is left, the list shows "✓ Done" and leaves the Home rail. It's never deleted automatically. |
| **Caps** | 100 lists, 300 entries per list, 50 per add. |

---

## 4. Data model and migration

`src/store/migrations/0015_watchlists.sql`:
```sql
-- 0015 Watchlists (DESIGN §42). User data: entries carry fp + orphaned_since (store::user
-- HOLDERS "watchlist"); placeholders have no copy and match by title at every refresh.
CREATE TABLE IF NOT EXISTS watchlist (
  id          INTEGER PRIMARY KEY,
  name        TEXT    NOT NULL,
  note        TEXT,                              -- "story order", shown under the name
  play_order  TEXT    NOT NULL DEFAULT 'ordered' CHECK (play_order IN ('ordered', 'any')),
  sort_index  INTEGER NOT NULL DEFAULT 0,
  since       INTEGER NOT NULL DEFAULT 0,        -- Start again: progress before this doesn't count
  is_default  INTEGER NOT NULL DEFAULT 0,        -- My list: one row, can't be deleted or renamed
  created_at  INTEGER NOT NULL,
  created_by  TEXT    NOT NULL DEFAULT 'app',    -- 'app' or the agent's name (§30.12)
  last_played_at INTEGER                         -- Home rail / lists pane order
);
CREATE UNIQUE INDEX IF NOT EXISTS watchlist_name ON watchlist(name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS watchlist_item (
  id           INTEGER PRIMARY KEY,
  list_id      INTEGER NOT NULL REFERENCES watchlist(id) ON DELETE CASCADE,
  position     INTEGER NOT NULL,                 -- dense 0..n-1 per list
  kind         TEXT    NOT NULL CHECK (kind IN ('movie', 'series', 'episode')),
  source_id    INTEGER REFERENCES source(id) ON DELETE SET NULL,  -- NULL = placeholder
  item_key     TEXT,
  any_copy     INTEGER NOT NULL DEFAULT 1,
  season_from  INTEGER,                          -- series ranges; NULL = whole series
  season_to    INTEGER,
  title        TEXT    NOT NULL,                 -- display + placeholder matching; never logged at INFO
  year         INTEGER,
  tmdb_id      INTEGER,
  skipped      INTEGER NOT NULL DEFAULT 0,
  added_at     INTEGER NOT NULL,
  fp           TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  CHECK ((source_id IS NULL) = (item_key IS NULL)),
  CHECK (kind = 'series' OR (season_from IS NULL AND season_to IS NULL))
);
CREATE INDEX IF NOT EXISTS watchlist_item_order ON watchlist_item(list_id, position);
CREATE INDEX IF NOT EXISTS watchlist_item_ref ON watchlist_item(source_id, kind, item_key);

ALTER TABLE queue_item ADD COLUMN watchlist_item_id INTEGER;  -- the Up next chip; no FK (rows outlive lists)
-- Up next's context (the list playing after the queue's rows): `meta` row 'queue:context' =
-- the watchlist id, or absent. Kept across a restart for the "Resume Star Wars" tile.
```
- **Deleting a playlist** sets `source_id` to NULL, which turns the entry into a placeholder (it isn't deleted). The next refresh, or P3's "move to matched copies", reattaches it by title.
- **Placeholder matching** runs after every catalogue import (`titles::refresh_source` already computes `match_hash`). A film or series placeholder matches when its `tmdb_id`, or its (kind, folded title, year), matches. Playlists carry no TMDB id for episodes, so an episode placeholder stores its series (title, year, `tmdb_id`) and its season and episode number: it matches when the series matches that way and the episode with that S/E exists (decided 2026-10-14). If there are several matches, the copy plan picks one. The matching cost is one indexed lookup per placeholder, and there are at most a few hundred.
- **Store module** `src/store/watchlist.rs`: CRUD, dense positions, and `progress(list) -> Vec<EntryState>`. That function makes one query per list, joining `watch_progress` over the copies of each entry; episodes are counted per range with `GROUP BY`. HOLDERS gets a "watchlist" row. `store::backup` exports both tables.
- **Model.** `row_id!(WatchlistId)` and `row_id!(WatchlistItemId)`. `ItemRef` is unchanged.

### 4.1 Favourites become channels only (decided)

**A one-time move**, done in the Rust step of migration 0015 in one transaction, after `store::backup` writes a backup file:
1. Create **My list** (`is_default = 1`, Any order).
2. Films, series and episodes in the default **Favourites** list go to My list, in the same order.
3. Every other favourites list that holds films or series becomes an Any order watchlist with the **same name and order** ("Trending", "Mission Impossible"). If the name is taken, it becomes "Trending (favourites)".
4. The film and series rows are deleted from `fav_item`. A list left with no channels is deleted (the default Favourites list is always kept).
5. `fp` and `orphaned_since` move with the rows, so orphans stay orphans and can still reattach.
6. Once, on the next launch, a card says: "Films and series you'd saved are now in Watchlists: My list, Trending, Mission Impossible. Favourites are for channels." with [Open Watchlists] [OK].

**What changes afterwards**
| Place | Before | After |
|---|---|---|
| `Ctrl+F` on a film, series or episode | Favourites | **My list** (the same key, glyph and toast wording "Saved to My list") |
| `Ctrl+Shift+F` on a film or series | Favourites lists… | Watchlists… (§2.3) |
| Detail page button | ★ Favourite | ★ My list, plus "In Star Wars (5 of 15)" |
| Stack cards | ★ = any copy is a favourite | ★ = in My list. Other lists don't mark cards, to keep posters clean |
| Home | One "Favourites" rail per list, films included | Favourites rails show channels only. Films come from the "Your watchlists" rail (My list has a tile like any list) |
| Settings ▸ Favourites | Lists of everything | Channel lists only. Watchlists are managed on their own screen |
| Agent `favourites` / `favourites_edit` | Channels, films, series | Channels only. A film or series gets `use_watchlist`, pointing at the `watchlist` tool |
| `fav_item` writes | Any kind | `kind = 'channel'` only. `store::user` refuses others; the CHECK isn't rebuilt |

A backup from before 0015 that is restored on a newer Tuner runs the same move.

---

## 5. Architecture

Watchlists don't need an engine of their own. A list's state is derived when it's read, and playing a list is Up next's engine with a **context**:
```
 UI / agent ──▶ store::watchlist (spawn_blocking reader)  ──▶ ListView { entries, states, next }
 play list   ──▶ QueueCommand::PlayList { list, from: Option<entry>, just_one }
                   → set context, Add { at: Next } + PlayAt (resume as §3)
 Up next engine: item Ended, no pending row left, context set
             ──▶ resolve the list's next entry (pre-resolved when the countdown starts)
             ──▶ Add { at: End, watchlist_item_id } → countdown → PlayCopy
             ──▶ nothing left → context cleared, "That was Star Wars ✓"
```
- **`QueueMachine` gains `context: Option<WatchlistId>`.** The new inputs are `PlayList`, `StopList` and a "list next" answer from the store. Pure and unit-tested like the rest of the machine.
- **Everything about playing is reused:** the PIN card, copy fallback, the three-failures pause, B1 placement (`open_in`), the mini player, the pop-out, the hidden window and MPRIS Next.
- **One row at a time.** The list's entries aren't copied into Up next ahead of time, so reordering the list, or watching something elsewhere, changes what plays next straight away.
- **The view is computed per screen open and per player `Ended`, never per frame.** `WatchlistsSnapshot` is cached in the UI and invalidated by a `watch_progress` write, a list edit or a library swap. There are no catalogue scans: progress reads are indexed on `(source_id, kind, item_key)`.
- **The Home rail** reads the same snapshot.

---

## 6. Guardrails

- **Parental lock.** Locked entries show in the app (dimmed) and ask for the PIN when played, as in Up next. Over MCP, a locked entry is never played or queued. "Next" for the agent passes over it and says "The next one, *X*, is locked: it needs your PIN in Tuner." Adding a locked title over MCP is refused, the same as `queue add`.
- **Explicit content while hidden.** Concealed entries are absent from rows, counts ("5 of 15" counts visible entries only), the Home rail, search and MCP. They are never deleted. A list that contains only concealed entries is hidden too.
- **Spoiler guard.** Rows show the title and the S/E only, never a plot. A series entry's "next S2E5" episode title goes through `Subject::Episode`. Watchlists don't add spoiler rules of their own (decided).
- **Privacy.** No titles at INFO (`watchlist: list 3 → entry 12 (movie, source 2)`). No URLs in any result. Watchlists are included in backups.
- **The 3-failures pause** also ends the list's context, so a list never hammers a provider overnight.
- **The agent's writes** follow RULES.md: creating a list, adding more than 3 entries, removing entries, deleting a list and Start again each need the user's yes. Order claims ("story order") are labelled as the agent's own knowledge.

---

## 7. MCP and skills

**One `watchlist` tool** (same action shape as `queue`):
| action | params | result |
|---|---|---|
| `lists` | — | `[{id, name, note, play_order, done, total, next?:{ref, title, kind, s?, e?, position_s?, locked}}]` |
| `show` | `list` (name or id), `include_done?` | `{list, items:[{id, ref?, kind, title, year, seasons?, state: done\|next\|todo\|skipped\|missing\|locked, progress?}]}` |
| `next` | `list` | `{item, episode?, resume_at?, passed_over:[{title, why}]}`. Read-only, so the agent can say what's next before playing it. For an Any order list it returns the pick, and `play` with `id` plays the agent's own choice instead |
| `play` | `list`, `id?`, `just_one?`, `open_in?` | Plays the list from its next entry (or `id`) and carries on through it. `just_one` stops after it. It replaces current playback, so `status` comes first (RULES 2). Returns `{playing, opened_in, then?}` or `{waiting_for_pin}` |
| `stop` | — | Ends the list's context after the current item: `{stopped_after}` |
| `create` | `name`, `note?`, `play_order?`, `refs[]?` + `placeholders[]?` (`{title, year, kind, tmdb_id?}`) | `{list, added, refused}` |
| `add` | `list`, `refs[]` / `placeholders[]`, `at: end\|next\|<index>`, `seasons?` | `{added, refused}` |
| `move` / `remove` / `skip` | `list`, `ids[]`, … | `{items}` |
| `restart` / `rename` / `delete` | `list`, … | `{list}` |

- `list` accepts a name and matches it fuzzily ("star wars" or "SW" → "Star Wars"). An ambiguous name returns `{ambiguous:[names]}`, so the agent asks the user rather than guessing.
- `status` gains `watchlist:{name, position, total, playing_through}` while a list plays.
- `favourites` and `favourites_edit` become channel-only (§4.1).

**Skills**
- **pick:** a franchise check ("which Mission Impossibles do I have, in order?") offers "Save as a watchlist" instead of a favourites list. Titles the user doesn't have go in as placeholders. "Play the next Star Wars" → `watchlist next` → say what it is and where it resumes → `watchlist play` (it carries on through the list; "just the one" → `just_one`). "Something from my Christmas list under 90 min" → `show` → choose → `play` with `id`.
- **series:** "carry on with my Star Wars shows" uses `watchlist next` when the series is in a list; "where am I in Star Wars" reads `show`.
- **discover:** the "Trending" list becomes a watchlist (Any order).
- **watch:** saving a channel to favourites is unchanged.
- **RULES.md:** the confirmation rules in §6, plus "never reorder a user's list without asking".

---

## 8. Tests worth having

**Store (temporary DB)**
1. Done is derived across copies: finishing *Empire* on TV 2 moves the cursor of an entry pinned to TV. Progress older than `since` doesn't count after Start again.
2. A series range: S1–S2 is done only when every S1–S2 episode is finished. S3 progress doesn't count. Two ranges of the same series advance independently.
3. An Any order list never picks a done, placeholder, concealed or (agent) locked entry, and a partly watched entry wins.
4. Next passes over placeholders, skips and (for the agent) locked entries, and reports why.
5. A placeholder attaches after an import that brings in the title (by tmdb_id, and by title + year). Deleting a playlist turns its entries back into placeholders.
6. A refresh that re-keys a copy reattaches the entry through HOLDERS. Backup round trip. Dense positions after move and remove.

**Playing a list (`QueueMachine`, fake clock)**
7. `PlayList` → exactly one row → `Ended` → the countdown → the list's next entry as one new row → `PlayCopy`. The last entry → "That was …" and the context is cleared.
8. A row added with Ctrl+N during a list plays before the list's next entry, and the list resumes after it.
9. `just_one`, Stop the list, Esc on the card, the sleep timer's `end_of_queue` and a second list each end or replace the context. A foreign play steps aside and keeps the context. The same film played without a context is followed by nothing.
10. Reordering the list mid-play changes the next entry. An entry watched elsewhere in the meantime is passed over.

**Favourites move (migration on a copy of a v14 DB)**
11. Default-list films go to My list in order; other lists keep their names; channel-only lists are untouched; a films-only list is deleted after its move; orphans stay orphans. Restoring a pre-0015 backup runs the same move. `fav_item` refuses a film afterwards.

**Guardrails**
12. Concealed entries are absent from counts, the Home rail and MCP, and are restored in place when explicit content is shown again. A locked next entry is refused over MCP. No `http` substring in any result.

**UI (rig):** Ctrl+F on a film saves to My list, and on a channel to Favourites. Ctrl+Shift+F on a series offers the season range. The entries pane opens on the next entry. Alt+↓ and a drag give the same stored order.

---

## 9. Implementation plan

Following the one-feature-per-workflow rule, this ships as **one feature**. It is cut into tracks only so that agents work on disjoint files:

| Track | Files | Size |
|---|---|---|
| **W0 skeleton** | `0015_watchlists.sql`, `src/watchlist/{mod,model}.rs` (types, `WatchlistsSnapshot`), `model/ids.rs`, keymap (`GoWatchlists`; `Ctrl+F` / `Ctrl+Shift+F` routing by kind), `QueueCommand::{PlayList, StopList}` | ~300 |
| **W-core** | `src/store/watchlist.rs` (+ tests), the favourites move (§4.1) in `migrations.rs`, placeholder matching in `titles::refresh_source`, HOLDERS, backup, `queue::machine` context + `queue::service` list resolve | ~1,500 |
| **W-ui** | `src/ui/screens/watchlists.rs`, the Watchlists… dialog, detail-page button and line, Home rails (watchlists in, film favourites out), stack-card ★, sidebar entry, Up next chip and "Then from Star Wars" section, list card texts, the one-time "moved" card, Settings ▸ Favourites channels-only | ~1,400 |
| **W-agent** | `agent/proto.rs`, `agent/watchlist.rs`, `tools.rs`, `favourites` channel-only, skills (pick, series, discover, watch) + RULES.md | ~550 + md |
| **Docs** | DESIGN §42, §15 tables (`keymap_md`), MANUAL_CHECKLIST, README feature line | ~150 |

The total is about 4,200 lines including tests. The order is W0, then W-core and W-ui in parallel, then W-agent. There is one gate and one review at the end.

---

## 10. Risks

1. **The cost of deriving done for long series ranges.** A 10-season range is about 200 episodes × copies. It's mitigated by one grouped query per list and a snapshot cached until something changes. Bench it against the user's real library (302k films) before merging.
2. **Wrong placeholder matches** (remakes, the same title in a different year). Mitigation: match on tmdb_id first, then title + year only, never title alone. Settings ▸ Orphans-style "Find in my playlists…" handles a manual fix.
3. **Two concepts that look alike** (Up next and Watchlists) could confuse. Mitigation: the same model as a music app (the queue first, then the playlist), the "Then from Star Wars" section in Up next, and the list chip on every row it adds.
4. **The favourites move is one-way.** Mitigation: a backup first, one transaction, names and order kept, a test on a copy of a real DB, and the one-time card that says where things went.
5. **Ctrl+F means two things** (Favourites for channels, My list for films). Both mean "save this", the toast always names the destination, and help lists both.

---

## 11. Decisions (all answered, 2026-10-13)

| # | Question | Your answer |
|---|---|---|
| 1 | New feature, or favourites with a play order? | **New Watchlists.** Favourites are for channels; film and series lists work like music playlists |
| 2 | When a list item ends | **It depends where you started.** Started from a list → the list plays through like a music playlist (Up next's countdown, and rows added to Up next go first). Started anywhere else → nothing follows. This replaces the earlier "offer next, per-list Keep going" answer |
| 3 | Titles you don't have | **Placeholders**, which attach on a refresh |
| 4 | Series in lists | **Whole series or a season range**; the same series may appear twice |
| 5 | Lists without an order | **Yes: each list is In order or Any order** |
| 6 | Films already in favourites | **Favourites become channels only.** Films and series move to watchlists (§4.1), with My list as the default |
| 7 | How a list plays | **Through Up next** (decided with 2) |
| 8 | Spoiler guard from watchlists | **No** |
| 9 | Sidebar and keys | **"Watchlists" in the sidebar (always shown), `Alt+7`**. `Ctrl+F` / `Ctrl+Shift+F` save films to watchlists |

**Defaulted (internal or follows from the answers; say if you want otherwise)**
- **The agent's "play the next Star Wars" plays through the list too**, since it starts from the list. "Just the next one" makes it stop after that one.
- My list is Any order and can't be deleted. Only My list puts the ★ on posters.
- Caps: 100 lists, 300 entries per list.
