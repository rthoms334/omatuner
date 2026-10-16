# Downloads and recordings: design

> Approved for build, 2026-10-16. Built on `feat/netflix-player` 8b6218c, because live recording reuses its HLS recorder (`player/hls`, `docs/design/timeshift-design.md`). It is meant to become DESIGN §43. The user's answers are in §11.

**The idea in one line:** save films, episodes, whole seasons, live programmes and catch-up to disk, so they play instantly, offline and with free seeking. Files are private to Tuner. A playlist's single connection is never used behind your back: when watching and downloading or recording want the same account, Tuner asks.

**What exists today that this builds on**
- **HTTP.** `HttpClient::download` streams to a 0600 `.part` file and renames it at the end. It has no `Range` resume yet.
- **Connection budget.** `player::budget::SharedBudget` gives out per-source leases (`acquire` / `release`, `TileId`), with limits from `AccountInfo.max_connections` (1 on both of the user's accounts). Casting and multi-view already hold leases the same way.
- **Playback.** `sources::resolve::resolve(conn, cfg, media, resume, now)` is the one place a `MediaRef` becomes a URL. `store::copies::plan` picks the copy, and `same_episode` finds an episode in another copy.
- **Live rewind** (`player/hls`). This is a serialized HLS fetcher (playlists, segments, AES keys, init maps) with a 0700 private store, counter file names and no titles or URLs on disk or in logs. It also holds a lease while it runs and closes the socket before the lease is released. Raw TS live keeps mpv's cache.
- **Scheduling while Tuner is closed** (§36.2): transient `systemd-run --user` timers and `tuner --rearm` at login. **Sleep inhibit:** `cast/inhibit.rs`.
- **Watched state, parental lock and concealment, the spoiler guard, Up next and watchlists** (§32, §42).
- **Free keys:** `Ctrl+D` and `Alt+8` (checked on `feat/netflix-player`). **Free migration number:** 0016.

---

## 1. User stories

1. **A season for the weekend.** On Andor's page, Ctrl+D › "Season 1 (12 episodes, ~28 GB)". The episodes download one after another from TV. The badge `⤓ 4/12` shows progress, and each finished episode shows ⤓.
2. **Keep me topped up.** Ctrl+D › "Keep the next 3 unwatched downloaded". When I finish one, the next one starts downloading.
3. **A film.** Ctrl+D on *Dune* downloads it from the best copy. When it's done, Enter plays it from disk: instant start, instant seeking, nothing used on the account.
4. **Record a programme.** In the guide, Ctrl+D on *Bake Off*, Tue 20:00, records it with 1 min of padding before and 5 min after. It works with Tuner closed (Tuner is started for it) and keeps the machine awake while recording.
5. **Record what I'm watching.** In the live player, Ctrl+D records the current programme to its end. Recording starts from the live rewind buffer, so it includes the part I've already seen.
6. **A catch-up.** Ctrl+D on last night's *Newsnight* (⟲) saves it from the archive before it expires.
7. **A missed recording.** The laptop was asleep at 20:00. If the channel has catch-up, Tuner records it from the archive once it can, and says "Recorded from catch-up".
8. **It asks.** TV is downloading Andor S1E4 (62 %) and I press Enter on a TV channel. Tuner asks: "TV is downloading *Andor S1E4* (62 %). [Pause it and play] [Play from TV 2] [Cancel]".
9. **Ask the agent.** "Download the rest of Andor season 1" or "record Bake Off on Tuesday". The agent confirms, then reports progress, free space and clashes.
10. **Tidy up.** Downloads stay until I delete them. Settings has "Delete downloads [7] days after watching" (off) and a size cap.

---

## 2. UX

### 2.1 Where downloads show up
| Place | What |
|---|---|
| **Downloads screen**, `Alt+8`, sidebar entry "Downloads" with a live count | Sections: Now (progress bar, speed, ETA, account) · Waiting (queue, reorderable) · Scheduled recordings · Downloaded (grouped by series) · Failed / Missed |
| **Cards, episode rows, guide cells** | ⤓ downloaded, ⤓ 62 % in progress, ⏺ recording scheduled or running (guide cells get a red dot) |
| **Detail pages** | A `[⤓ Download ▾]` button. Series pages show "8 of 12 downloaded · 19 GB" |
| **Player** | A "⤓ Playing from your download" line in stream info. While recording live, the OSD shows ⏺ and Ctrl+D stops recording |
| **Status bar** | "⤓ 2 downloading · 4.1 MB/s" while anything runs |
| **Settings ▸ Downloads** | Space cap, the free-space reserve, delete after watching, recording padding, folder, and "Pause downloads" |

### 2.2 Downloads screen, Wide
```
┌──────────────┬──────────────────────────────────────────────────────────────────────────────┐
│ 󰓎 Watchlists  │ DOWNLOADS   38 GB used of 200 GB · 312 GB free           [⏸ Pause downloads]     │
│ ⤓ Downloads 2 │ ─ Now ──────────────────────────────────────────────────────────────────────── │
│ 󰒓 Settings    │  ⤓ Andor · S1E4 · Aldhani            ● TV    ▕██████░░░▏ 62 % · 6.2 MB/s · 4 min │
│              │  ⏺ Bake Off · Tue 20:00–21:05           ● TV 2  recording · 21 min left          │
│              │ ─ Waiting (7) ───────────────────────────────────────────────────────────────── │
│              │ ▸⠿ Andor · S1E5 … S1E12               ● TV    8 episodes · ~19 GB              │
│              │  ⠿ Dune (2021)                       ● TV 2  waiting for TV 2 (watching)      │
│              │ ─ Scheduled ─────────────────────────────────────────────────────────────────── │
│              │  ⏺ Newsnight · Wed 22:30             ● TV    ⚠ clashes with Andor downloads    │
│              │ ─ Downloaded ────────────────────────────────────────────────────────────────── │
│              │  ▸ Andor  3 episodes · 7.1 GB · 1 watched                                      │
│              │    Slow Horses S2E1  · 2.4 GB · ✓ watched · deletes in 5 days                  │
├──────────────┴──────────────────────────────────────────────────────────────────────────────┤
│ ↵ play · Space pause/resume · Alt+↑↓ move · Del delete/cancel · Ctrl+↵ more · ? help        │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.3 The download menu (`Ctrl+D`)
| On | Choices |
|---|---|
| Film | Download (best copy) · Download from TV 2 |
| Episode | This episode · From here to the end of the season |
| Series page | Next episode · Next 3 unwatched · Season n · All seasons · **Keep the next [3] unwatched downloaded** |
| Guide programme (future) | Record · Record every new episode (a Following rule that records, §3) |
| Guide programme (ended, ⟲) | Save from catch-up |
| Live player | Record this programme to its end · Record for [30 min ▾] |
| Something already downloaded | Play · Delete download · Show in Downloads |

Every choice shows its size (or "~2 GB per hour" for a recording) and the free space before it starts. If it won't fit, the menu says so and adds nothing.

### 2.4 The "Ask each time" prompt (decided)
**Watching on a busy account:**
```
┌ TV is busy ────────────────────────────────────────────────┐
│ TV is downloading Andor S1E4 (62 %).                       │
│ [Pause it and play · Enter]  [Play from TV 2 · 2]  [Cancel · Esc] │
└────────────────────────────────────────────────────────────┘
```
- The "Play from …" button shows only when another playlist has a copy (or a ranked channel) that isn't busy.
- **If TV is recording:** the prompt offers [Stop recording and play] [Play from TV 2] [Cancel]. When you play the **same channel** that's being recorded, there's no prompt: it plays from the recording as it's written (a live picture with free rewind), using no second connection.
- **A paused download resumes on its own** once the account is free again (playback stopped, the player closed, or a zap to another playlist).

**A download or recording is due on an account you're watching**
- A toast appears with a 60 s countdown: "*Bake Off* recording starts on TV, which you're watching. [Record (stops this)] [Record from TV 2] [Skip]". When the window is hidden, it's a notification with the same actions.
- **A download holds the account:** a due recording pauses it automatically, closing its socket before taking the lease. Only a picture holding the account gets this prompt. **Skip** cancels that recording with reason “skipped”.
- **No answer:** downloads wait. A recording keeps your picture, and after the programme ends Tuner saves it from **catch-up** if the channel has it; otherwise it's marked **Missed (you were watching)**. See "Defaulted" in §11.

**The agent:** `play` on a busy account returns `busy {what, progress}` and never pauses on its own. The agent asks you in chat and then calls `play` with `pause_downloads: true`.

### 2.5 Keyboard
| Context | Key | Action |
|---|---|---|
| `Global` | **`Alt+8`** | Downloads screen (`GoDownloads`) |
| Item contexts, guide, live player | **`Ctrl+D`** | Download / record… (`Download`) |
| Downloads list | `Enter` | Play the download (in progress: play from the provider as usual) |
| | `Space` | Pause / resume this download |
| | `Alt+↑↓`, drag the handle | Reorder the queue |
| | `Delete` | Cancel (waiting or running) or delete the file (Confirm, then Undo for 10 s before the file is unlinked) |
| | `Ctrl+Enter` / `Shift+F10` | Menu: Play · Pause · Retry · Move to top · Download from another copy · Delete · Delete watched |

---

## 3. Behaviour rules

| Rule | Decision |
|---|---|
| **What can be saved** (decided) | Films, episodes, series (seasons, all, or "keep N"), live programmes (scheduled, or current until it ends), and catch-up programmes. A bare channel with no guide can be recorded for a set time ("Record for 30 min"). |
| **One at a time per account** | Each download or recording holds one budget lease, like a player tile. With `max_connections = 1`, a playlist runs one job at a time. TV and TV 2 run in parallel. Jobs waiting on an account show "waiting for TV". |
| **Which copy** | Decided when the job **starts**, by `copies::plan` (latest progress → trouble last → language → playlist order), skipping a busy account when another copy is free. "Download from TV 2" pins it. |
| **Resume** | Film and episode files use HTTP `Range` from the `.part` length when the server sent `Accept-Ranges: bytes` and the ETag / Last-Modified / length still match; otherwise they start again. Pause keeps the `.part` file. |
| **Failures** | Retries 3 times with backoff (30 s, 2 min, 10 min), then **Failed** (Retry is in the menu). 403 counts as busy only when other holders fill the account limit; the failing job’s own lease is excluded. Three failed jobs in a row on one account pause that account's queue with a toast, so it never hammers a provider overnight. |
| **Playing a download** | `resolve` checks first for a **finished** download of this item, or of **any copy** of the title (for episodes, `same_episode`), and plays the local file: no lease and no network. Progress and watched state are stored against the downloaded copy's item, as today. Up next and watchlists use the download in the same way. |
| **In-progress downloads** | They play from the provider as usual (through the Ask prompt when it's the downloading account). Playing a half-finished file isn't supported. |
| **Live recording, HLS** (decided 2026-10-16) | A **keeping** variant of the `player/hls` recorder: no 1,800 s cut; segments, keys and maps are kept in the job's 0700 folder while recording. Saved HLS can't be played directly (mpv's clock resets at provider discontinuities; Codex's `downloaded_hls` probes in `tests/timeshift_transport.rs`). So when the recording stops it is **finished into one file**: each discontinuity run is remuxed with the cast relay's ffmpeg (`-c copy`, no re-encode), then joined with the concat demuxer, which offsets each run's timestamps, into `<id>.mkv`. The folder is deleted once the file is verified (duration within 2 s of the recorded total). The state is `finishing` ("Finishing… 40 %"), which takes about a minute per hour. A failed finish keeps the folder and retries on the next start. The finished file plays, seeks and casts like a downloaded film. |
| **Live recording, raw TS** | One streamed HTTP GET into `<id>.ts` until the stop time. Reconnects inside the window append, and a gap is recorded in the job ("2 gaps, 40 s"). |
| **Recording what's playing** | When the picture is an HLS live rewind of the same channel, recording **adopts** the live recorder's store (the past 30 min up to the programme start is kept, and segments from then on stop being deleted), so no second connection is used. |
| **Catch-up** | Downloaded like a file when the catch-up URL is a file. When it's a stream, it's recorded for the programme's duration plus padding. Saving starts at once, because the archive is already there. |
| **Timing** | Padding defaults to 1 min before and 5 min after (Settings). A scheduler on tokio rechecks the wall clock at least every 60 s and is safe across a suspend. While a recording or download runs, sleep is inhibited (`cast/inhibit.rs`). |
| **Tuner closed** | Recordings arm `tuner-record-<id>` transient units (§36.2 pattern, re-armed at login), which start Tuner headless (no window, the session counts as locked) to record. Downloads don't wake Tuner: they continue the next time it runs. |
| **Guide changes** | At each EPG import, a scheduled recording follows its programme by (channel, title, start ±30 min). If the programme disappears, the recording keeps its time and is labelled "Not in the guide any more". |
| **Missed** | If Tuner or the machine was off at the time: catch-up when the channel has it and the programme is inside the window, otherwise "Missed". |
| **Space** | Before starting, there must be room for the job's size (or, for a recording, ~bitrate × duration) within the cap and above the reserve. Defaults: a cap of 200 GB and 10 GB kept free. A recording that runs out of space or stops early (including quit, crash, pause or Delete then Undo) keeps what it has and is marked **Partial**. Raw-TS parts recovered after their window are promoted to playable Partial files. A download waits as "Waiting for space". |
| **Cleanup** (decided) | Kept until you delete them. "Delete downloads [n] days after watching" is off by default. Deleting a watched title in Tuner deletes its file too, after a Confirm. Deleting a playlist keeps its downloads (they still play) and lists them as orphans. |
| **Series recording** (decided) | "Record every new episode" is a Following rule (§33) whose action is **record** instead of remind or switch. After every guide refresh, Following's evaluation schedules recordings for new matches (new = an S/E or event key not already recorded by this rule or downloaded; repeats within the rule are skipped. Event and fixture keys retain their dates, so daily programmes and subsequent matches remain new. Missed, Failed and Cancelled jobs do not count as recorded). It has the same clash handling, and the rules are listed and edited in Settings ▸ Following. The agent's `follow` tool gains `action: record`. |
| **Speed and window** (decided) | Full speed, one at a time per account, by default. Settings ▸ Downloads has an optional speed cap (MB/s) and an optional "Only download between [01:00] and [07:00]", both off. Recordings ignore both the speed cap and window and spend none of the downloads’ shared rate budget. When the window closes, active downloads pause and resume when it opens. “Pause downloads” persists across restarts; recordings and finishing jobs continue. |
| **Keep N** | A series rule: when an episode in it is watched (or deleted), the next unwatched episode is queued, up to N downloaded and unwatched at any time. Rules appear under “Kept topped up” in Downloads and Settings ▸ Downloads; Delete removes a rule, with Undo for 10 seconds, keeping its files. |

---

## 4. Data model and storage

`src/store/migrations/0016_downloads.sql`:
```sql
-- 0016 Downloads and recordings (DESIGN §43). User data: fp + orphaned_since (HOLDERS "download").
CREATE TABLE IF NOT EXISTS download (
  id          INTEGER PRIMARY KEY,
  position    INTEGER NOT NULL,                 -- queue order among waiting jobs
  kind        TEXT NOT NULL CHECK (kind IN ('movie', 'episode', 'live', 'catchup')),
  source_id   INTEGER REFERENCES source(id) ON DELETE SET NULL,   -- NULL after a playlist delete: still plays
  item_key    TEXT,                             -- movie / episode key; channel key for live / catchup
  any_copy    INTEGER NOT NULL DEFAULT 1,
  prog_start  INTEGER, prog_stop INTEGER,       -- live / catchup: the programme (or the set time)
  pad_before_s INTEGER NOT NULL DEFAULT 0, pad_after_s INTEGER NOT NULL DEFAULT 0,
  catchup_id  TEXT,
  title       TEXT NOT NULL, subtitle TEXT,     -- display only; never logged at INFO, never in file names
  state       TEXT NOT NULL CHECK (state IN ('scheduled', 'waiting', 'running', 'finishing', 'paused', 'done',
                                             'partial', 'failed', 'missed', 'cancelled')),
  reason      TEXT,                             -- 'busy', 'space', 'http-403', 'watching', 'gaps:2:40' …
  store_name  TEXT,                             -- '<id>.mkv' or '<id>/' (HLS folder); counters only
  bytes_done  INTEGER NOT NULL DEFAULT 0, bytes_total INTEGER,
  etag        TEXT, last_modified TEXT,         -- Range resume validation
  attempts    INTEGER NOT NULL DEFAULT 0,
  added_at    INTEGER NOT NULL, added_by TEXT NOT NULL DEFAULT 'app',  -- or the agent's name
  done_at     INTEGER, watched_at INTEGER,
  fp          TEXT NOT NULL DEFAULT '{}', orphaned_since INTEGER
);
CREATE INDEX IF NOT EXISTS download_item ON download(source_id, kind, item_key);
CREATE INDEX IF NOT EXISTS download_state ON download(state, position);

CREATE TABLE IF NOT EXISTS download_rule (     -- "Keep the next N unwatched downloaded"
  id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  series_key TEXT NOT NULL, keep INTEGER NOT NULL CHECK (keep BETWEEN 1 AND 10),
  added_at INTEGER NOT NULL, fp TEXT NOT NULL DEFAULT '{}', orphaned_since INTEGER
);
-- Series recording: follow_rule gains action 'record' (a Rust rebuild step if its CHECK lists the actions).
```
- **Folder** (decided: private). `paths.downloads()` = `~/.local/share/tuner/downloads/`, 0700. Files are 0600 and named only by counters (`17.mkv`, `18/index.m3u8`, `18/000042.ts`). Settings can move it to another folder (an external disk), which is created 0700 and keeps the same layout. Folder copying runs off the async runtime while the scheduler continues ticking. Active recordings keep writing in the old folder; the final sync and switch wait for them. Nothing on disk names a title, a URL or a credential.
- **Container extension** comes from the provider's `container_extension` (it's still a counter name).
- **Startup reconcile:** interrupted live HLS recordings resume their kept folder while their stop is still in the future, otherwise finish as Partial; other `running` jobs → `waiting`; `.part` files without a row are deleted; rows whose file is missing → `failed (file missing)`.
- **Backup** exports the rows and rules, not the media, and says so.
- **Model.** `row_id!(DownloadId)`. `sources::resolve` gets a `local: Option<PathBuf>` check before building any URL. `Playable` carries `from_download: bool` for stream info.
- **Config.** `[downloads] cap_gb = 200`, `reserve_gb = 10`, `delete_after_watched_days = 0`, `pad_before_s = 60`, `pad_after_s = 300`, `dir = ""` (default), `max_mbps = 0` (off), `window = ""` (off, e.g. "01:00-07:00").
- **HTTP.** `DownloadRequest` gains `resume_from: Option<(u64, validators)>`, and `stream_to_file` can open the `.part` file for append.

---

## 5. Architecture

```
 UI / agent ──DownloadCommand──▶ downloads service (tokio)  ──acquire lease──▶ SharedBudget
                                   ├ per-account worker (1 job each at max_connections = 1)
                                   │   file:   HttpClient::download (+ Range) → .part → rename
                                   │   hls:    player::hls recorder in keep mode → <id>/
                                   │   ts:     streamed GET → <id>.ts until stop
                                   ├ scheduler (recordings, ≤60 s wall-clock re-check, suspend-safe,
                                   │            systemd timers while closed, inhibit while running)
                                   └ watch<Arc<DownloadsSnapshot>> ──▶ UI, status bar, agent, resolve
 player controller ── before acquire on a busy account ──▶ Busy { job } ──▶ UI "Ask" prompt
                       answer: PauseAndPlay → service pauses the job, releases the lease → play
```
- **Busy check.** `SharedBudget` tells a **download holder** apart from a picture holder (a new `Holder::Download(DownloadId)` next to `TileId`). The player's `acquire` on an account held by a download returns `BudgetExceeded::Download { id }`, and the UI shows the prompt instead of the usual "connection limit" error. Multi-view tiles and casting get the same prompt.
- **Pausing** releases the lease only after the socket has closed (the same rule as the live recorder), so the play that follows never races into a 403.
- **Keep mode** in `player/hls::store` is a flag on the store (no time-based deletion; ENDLIST is written at stop). The fetcher is unchanged.
- **Kill switch** `downloads::ENABLED`, as with `queue::ENABLED`.
- **Scale.** The snapshot holds ≤ 500 rows. There is no per-frame DB work, and progress is published at most 4 times a second.

---

## 6. Guardrails

- **Parental lock.** You can download a locked item in the app; the PIN is asked when you **play** it (the file is private, so it can't be watched without Tuner). Over MCP, locked items are refused (`locked: needs your PIN in Tuner`).
- **Explicit while hidden.** Concealed downloads are absent from the screen, the counts, the space totals per title and MCP. Their bytes still count in "used", shown as one plain total with no titles. They are never auto-deleted while hidden.
- **Privacy.** Counter names only. No titles at INFO (`download: #17 running (episode, source 2) 62 %`). No URLs in logs, rows or MCP results. Files are 0600 and folders 0700.
- **The provider.** One connection per account, never more than `max_connections`. No parallel range requests, and backoff on errors. The 3-failures pause applies per account. A pause for an at-limit account shows as busy.
- **The spoiler guard** applies to episode titles in the list and in notifications, as everywhere.
- **The agent's writes:** downloading more than 3 items, any recording, Keep N rules, deleting files and pausing for playback each need the user's yes (RULES.md).

---

## 7. MCP and skills

**A `downloads` tool:**
| action | params | result |
|---|---|---|
| `list` | `state?` | `{used_gb, cap_gb, free_gb, items:[{id, ref, kind, series?, season?, episode?, title?, state, progress, eta_s?, account, reason?, when?}], rules:[{id, ref, title, keep}]}` |
| `add` | `refs[]` (≤ 50: items, programmes, catch-ups), `copy?: best\|this` | `{added, refused:[{ref, error: locked\|not_found\|no_space\|duplicate}], size_gb}` |
| `series` | `ref`, `scope: next\|next_n\|season\|all\|keep`, `n?`, `season?` | `{added \| rule}` (`scope=keep,n=0` removes the rule) |
| `record` | programme `ref`, or `channel` + `minutes` | `{id, start, stop, clash?}` |
| `pause` / `resume` / `cancel` / `delete` / `retry` | `ids[]` | `{items}` |

- `play` gains `pause_downloads?` and returns `busy:{what, title?, progress}` without it.
- `status` gains `downloads:{running, waiting, used_gb}`.
- **Skills:** `series` ("download the rest of season 1", "keep 3 downloaded"), `plan` (recordings next to reminders; clash checks against downloads), `watch` ("record this", "save last night's Newsnight"), `fix` (failed or partial jobs, at-limit diagnosis). RULES.md gets the confirmations in §6.

---

## 8. Tests worth having

1. **Range resume:** a fake server with `Accept-Ranges` resumes from the `.part` length. A changed ETag restarts. A server without ranges restarts. Cancel keeps the `.part` file and delete removes it.
2. **Budget:** a download holds TV's lease. The player's acquire on TV returns `Download {id}`; PauseAndPlay releases only after the socket closes, then the play is admitted; when the play ends the download resumes. TV 2 is unaffected.
3. **Resolve:** a finished download of any copy plays locally with no lease. In-progress and partial downloads don't (partial ones do play from the Downloads screen).
4. **HLS keep mode and finishing** (the existing `player_mpv` / `timeshift_transport` rigs; turn Codex's `downloaded_hls` probes into these): a 3-minute recording with rotating keys and a timestamp-resetting discontinuity is finished into one file that plays in real mpv from the start to the end with a monotonic timeline and seeks past the discontinuity. Adopting a live store keeps the earlier segments. A killed finish resumes on the next start.
5. **Scheduler:** padding, a wall-clock jump across suspend (→ catch-up or missed), an EPG re-import moving the programme, and an unanswered clash → watching kept → catch-up fallback.
6. **Space:** cap and reserve block a start. A recording that hits the reserve ends as Partial.
7. **Series recording:** a Following rule with action record schedules each new episode once after a guide refresh, skips ones already recorded or downloaded, and is cancelled with the rule.
8. **Window and speed cap:** outside the window downloads wait (recordings don't); the cap holds the average rate.
9. **Keep N:** marking an episode watched queues the next. N is never exceeded.
10. **Guardrails:** a locked ref is refused over MCP. A concealed download is absent everywhere. No title or `http` substring in file names or logs at INFO; MCP includes visible, spoiler-guarded titles but no stream addresses.
11. **Startup reconcile** for `running` rows, stray `.part` files and missing files.

---

## 9. Implementation plan (one feature, one gate and one review at the end)

| Track | Files | Size |
|---|---|---|
| **D0 skeleton** | `0016_downloads.sql`, `src/downloads/{mod,model}.rs`, `paths.downloads()`, config, `DownloadId`, keymap (`GoDownloads`, `Download`), `Holder::Download` in budget | ~350 |
| **D-core** | `src/downloads/{service,worker,scheduler}.rs`, `store/downloads.rs`, `sources/http.rs` Range resume, `player/hls` keep mode + adopt, `resolve` local check, systemd units + rearm, inhibit, reconcile, backup | ~2,200 |
| **D-ui** | `ui/screens/downloads.rs`, the Ctrl+D menus (detail pages, series, guide, live player), the Ask prompt and the due toast, badges on cards, rows and guide cells, the status-bar item, Settings ▸ Downloads | ~1,600 |
| **D-follow** | `follow` action `record` (store, evaluation, Settings ▸ Following, the `follow` tool) | ~400 |
| **D-agent** | `agent/downloads.rs`, `proto`, `tools`, `play.pause_downloads` / `busy`, skills + RULES.md | ~500 + md |
| **Docs** | DESIGN §43, §15 tables, MANUAL_CHECKLIST (a real overnight recording, Tuner closed, suspend) | ~150 |

The total is about 5,200 lines including tests.

---

## 10. Risks

1. **The provider's terms.** Some IPTV providers flag heavy downloading. Mitigation: one connection per account, never parallel, backoff, and the optional speed cap and window.
2. **Servers without `Range`.** Mitigation: they restart, and the "Waiting" row says "can't resume on this playlist".
3. **Long HLS recordings with rotating keys or tokens.** The live recorder already refreshes playlists on 403/404. Gaps are recorded, never hidden.
4. **Headless recording with Tuner closed.** This is new (§36.2 only launched the UI). Mitigation: a headless mode that starts only the store, budget and downloads service, plus a MANUAL_CHECKLIST run.
5. **The branch base.** This builds on unmerged `feat/netflix-player`, so that branch merges first.

---

## 11. Decisions

**Answered (2026-10-16)**
| # | Question | Answer |
|---|---|---|
| 1 | What can be saved | **Films and series, plus live and catch-up recording** |
| 2 | Watching against downloading on one account | **Ask each time** (§2.4) |
| 3 | After watching | **Keep, with an option** to delete n days after watching (off), plus a size cap |
| 4 | Files | **Private to Tuner** (0700 folder, counter names; plays only in Tuner) |

| 5 | Series recording | **Include it**, as a Following rule action |
| 6 | A recording due while you watch, no answer in 60 s | **Keep watching**, then save from catch-up after the programme, else Missed |
| 8 | Finished HLS recordings | **Finished into one file** with ffmpeg remux and concat (no re-encode), shown as "Finishing…" |
| 7 | Speed and time limits | **None by default**; an optional speed cap and download window in Settings |

**Defaulted (internal or minor; say if you want otherwise)**
- Recording padding: 1 min before, 5 min after. Cap 200 GB, 10 GB kept free.
- Downloads don't start Tuner when it's closed; only recordings do.
