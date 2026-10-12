# Follow rules ("Following"): design

> The design as approved on 2026-10-11, kept for its user stories, mockups and numbered rules (code comments cite them as "DESIGN §33 §x.y"). The answers to its open questions and what was built differently are in DESIGN.md §33 (Following).

Built on main @ 013f897 and the work still in progress: P3 copies (P3 adds "Also on" and Ctrl+O; its migration is 0007), pane-arrows, the filter fix, and the MCP PR (`src/agent/*`; pass 2 adds reminder modes, the scheduler, systemd timers and the Settings reminders list).

## 1. What follow rules are for

A follow rule is a standing guide search. After every guide import Tuner checks the guide against each rule and creates, moves or cancels that rule's reminders. Reminders keep using the in-flight reminder engine for notify or auto-switch, lead time, start offset, the Cancel countdown, catching up when late and firing while Tuner is closed. Following adds no new firing mechanism.

## 2. User stories

1. **Arsenal.** In the guide I press `R` on "Premier League Football · Arsenal v Chelsea" and pick "Arsenal: every match". Every live Arsenal game in the guide gets a 2-minute reminder, now and after each refresh. I'm not reminded about Arsenal Women, repeats, highlights or "Arsenal: The Invincibles".
2. **Formula 1, qualifying and races only.** On race days Tuner switches me over at the start. Practice, F1 Academy and The F1 Show are left out.
3. **New episodes of Taskmaster.** I'm reminded once per new episode. A repeat of an episode I've already been reminded about stays quiet, even when the guide doesn't mark it as a repeat.
4. **Every England match.** I want a list, not alarms ("just list it"). The preview shows that it catches rugby and cricket too, so I can narrow it with sport chips.
5. **Kick-off moved.** When a kick-off moves from 15:00 to 17:30, the reminder moves with it and I get one toast. I don't end up with two reminders.
6. **Skipped matches stay skipped.** If I delete a rule-made reminder (Settings ▸ Reminders, or `r` in the guide), it isn't re-created after the next refresh.
7. **Clashes.** If two auto-switch rules clash, the rule higher in my list wins and the other one becomes a notification. I can see this in the Upcoming list.
8. **Weekly summary.** One summary such as "Following: 3 new matches this week", not a stream of notifications.
9. **Spoiler-safe sport.** Notifications and lists show "Arsenal · Sat 12:30 · Sky Sports Main Event", never the stage, the opponent's result or the description. If I'm late, it offers "From the start" first.
10. **From Claude.** "Follow Arsenal for me, switch at kick-off" leads to a preview, one confirmation, the rule, and an Undo toast in Tuner.
11. **Parental.** No rule ever matches explicit channels, whether or not the session is unlocked. A switch onto a locked channel becomes a "needs your PIN" notification.

## 3. UX

### 3.1 Where Following appears

- **Settings ▸ Following**: a new page between Reminders and Parental. It has three parts: the rules list, the Upcoming list and the rule editor.
- **"Follow this…" sheet**: a modal opened from:
  - `R` in the guide;
  - the programme popover's new **Follow…** button;
  - the guide cell or channel right-click menu;
  - the search overlay's programme-hit menu.
- **Toasts**: "Following Arsenal · 4 reminders set [Undo]" uses the in-flight `Action::Undo(u16)` toast. "Arsenal v Chelsea moved to 17:30" appears once per move.
- **Desktop summary notification** (§3.6).
- **Settings ▸ Reminders** (in-flight list): rows made by a rule get the tag `from Following · Arsenal`. Editing such a row detaches it from the rule (§5.4).
- **Guide cells**: the existing 󰂚 bell. Its tooltip adds "From Following: Arsenal". Following does not blur guide cells; that belongs to the spoiler-guard feature (§7).

### 3.2 Wide (≈1600 pt): Settings ▸ Following, list view

```
┌────────────────┬────────────────────┬─────────────────────────────────────────────────────────────────────────────────┐
│ 󰋜  Home         │ SETTINGS           │ FOLLOWING                                  3 new this week · checked 08:12      │
│ 󰐹  Live TV      │   Playlists        │ Tuner reads the guide after every refresh and sets these reminders for you.     │
│ 󰃭  TV guide     │   EPG              │ [+ Follow…]                                                                     │
│ 󰎁  Movies       │   Player           │ RULES · higher wins a clash                                                     │
│ 󰻍  Series       │   Interface        │ 󰇙 ● Arsenal              Team · live · remind 2 min · spoiler-safe      4 coming │
│                │   Favourites       │ 󰇙 ● Formula 1 sessions   Event · live · switch at start                 3 coming │
│                │   Reminders        │ 󰇙 ● Taskmaster           Show · new episodes · remind 2 min             1 coming │
│                │ ▸ Following        │ 󰇙 ○ England              Team · live · just list · paused                   —    │
│                │   Parental         │ UPCOMING · next 14 days                                                         │
│                │   Backup           │ Sat 18 Oct 12:30  Arsenal match        Sky Sports Main Event · FHD     󰂚 2 min   │
│                │   Hidden items     │ Sat 18 Oct 14:00  Formula 1 sessions   Sky Sports F1 · FHD             ⇄ switch  │
│                │   Orphans          │ Sun 19 Oct 07:00  Formula 1 sessions   Sky Sports F1 · FHD  ! clash    󰂚 notify  │
│                │   About & logs     │ Tue 21 Oct 21:00  Taskmaster · S20 E4  Channel 4 HD · TV 2             󰂚 2 min   │
│                │                    │ Wed 22 Oct 19:45  England (listed)     ITV1 HD                         · list    │
├────────────────┴────────────────────┴─────────────────────────────────────────────────────────────────────────────────┤
│ ↑↓ move · Enter edit · K/J priority · x more · ← pages                                         Guide updated 08:10 │
└───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Spoiler-safe rows show only the rule name, the time and the channel ("Arsenal match"). The episode title and fixture appear only for rules without spoiler-safe.

### 3.3 Wide: rule editor

The editor is a sub-view with the same pattern as the source editor: an explicit Save and an "Unsaved rule changes" guard.

```
│ ‹ Following   ARSENAL                                                       [Delete]  [Save]     │
│ What          Team                                                                               │
│ Names         [Arsenal ✕] [Gunners ✕] [+ type a name, Enter]                                     │
│ Leave out     [Women ✕] [U21 ✕] [U18 ✕] [Fan TV ✕] [Highlights ✕] [+]                             │
│ Match         ◂ Fixtures only ("Arsenal v …") ▸        Look in  ◂ title, episode, description ▸  │
│ Sport         [Any ✓] Football  Rugby  Cricket                                                   │
│ Airings       ◂ Live only ▸           (Live only · Live or new · New only · Every airing)        │
│ Channels      ◂ Any I can watch ▸     (Any · Favourites · a list… · TV · TV 2 · these channels…) │
│ Prefer        Favourites, then best picture (FHD, 4K), then TV before TV 2                        │
│ When          ◂ Remind me ▸  2 min before · start offset 0      (Remind · Switch for me · List)  │
│ Spoilers      [✓] Spoiler-safe: no episode titles, descriptions or results; late = from start    │
│ Limit         ◂ 10 ▸ a week                                                                      │
│ ── IN THE GUIDE NOW · 4 match · 2 left out ───────────────────────────── updating… ──            │
│ Sat 18 Oct 12:30–14:45  Arsenal v Chelsea    Sky Sports Main Event FHD · also TNT 1 (TV 2)  ✓   │
│ Tue 21 Oct 20:00–22:15  Arsenal v PSG        TNT Sports 1 HD                                ✓   │
│ Wed 22 Oct 19:00        Arsenal Women v …    left out: Women                                ✗   │
│ Thu 23 Oct 22:30        Arsenal v Chelsea    left out: repeat                               ✗   │
```

The "In the guide now" preview always shows the full fixture, because the user asked for it inside the editor. Spoiler-safe applies to everything shown passively.

### 3.4 Medium (≈950 pt, tiled half; icon rail 48)

List view:

```
┌──┬─────────────────┬────────────────────────────────────────────────────────────────┐
│󰋜 │ SETTINGS        │ FOLLOWING                         3 new this week  [+ Follow…] │
│󰐹 │   Playlists     │ 󰇙 ● Arsenal                                         4 coming    │
│󰃭 │   EPG           │     Team · live · remind 2 min · spoiler-safe                  │
│󰎁 │   …             │ 󰇙 ● Formula 1 sessions                              3 coming    │
│󰻍 │   Reminders     │     Event · live · switch at start                             │
│  │ ▸ Following     │ 󰇙 ● Taskmaster                                      1 coming    │
│  │   Parental      │     Show · new episodes · remind 2 min                         │
│  │   …             │ UPCOMING                                                       │
│  │                 │ Sat 12:30  Arsenal match        Sky Sports ME · FHD   󰂚         │
│  │                 │ Sat 14:00  Formula 1 sessions   Sky Sports F1         ⇄         │
│  │                 │ Sun 07:00  Formula 1 sessions   ! clash               󰂚 notify  │
└──┴─────────────────┴────────────────────────────────────────────────────────────────┘
```

Editor at Medium: one column of label/value rows. Chips wrap. The preview drops the "also on" text, which stays in the row's tooltip and menu.

### 3.5 "Follow this…" sheet

The card is 560 wide at every size; at Compact it is full width.

```
┌ Follow ─────────────────────────────────────────────────────┐
│ Premier League Football · Arsenal v Chelsea                 │
│ Sat 18 Oct 12:30 · Sky Sports Main Event                    │
│                                                             │
│ ▸ Arsenal: every match                              Team    │
│   Chelsea: every match                              Team    │
│   Premier League Football: every live game          Event   │
│   Every airing of "Premier League Football"         Show    │
│ ─────────────────────────────────────────────────────────── │
│ When     ◂ Remind me 2 min before ▸                         │
│ 4 in the guide · next Sat 12:30 · spoiler-safe on           │
│                     [More options…]  [Cancel]  [ Follow ]   │
└─────────────────────────────────────────────────────────────┘
```

How the suggestions are built (`follow::suggest`, a pure function):

- A fixture "A v B" in the title or episode title gives one Team suggestion per side.
- A known competition prefix (the text before ":" or the title itself) gives an Event suggestion.
- S/E data, or an episode title, gives "New episodes of X" (Show, new only).
- "Every airing of X" is always offered.

The count line comes from a preview run on the blocking pool, typically 0.7 s, with "counting…" shown until it finishes. **More options…** opens the Settings editor with this draft, unsaved.

### 3.6 Summary notification

```
Following: 3 new this week
Arsenal · Sat 12:30 · Sky Sports Main Event
Formula 1 sessions · Sat 14:00 · Sky Sports F1
+1 more                                                     [Open]
```

- At most one summary every 12 h, and only when there are additions. Moves get their own one-time toast.
- Spoiler-safe rules show only the rule name.
- With no notification server, or while the window is focused, the summary is an in-app toast instead.
- **Open** goes to Settings ▸ Following ▸ Upcoming.
- Config: `follow.summary = true`.

### 3.7 Keyboard model

I checked these against `src/keymap.rs` on main and the pane-arrows and P3 diffs.

| Where (context) | Key | Action |
|---|---|---|
| Guide (`Guide`) | `R` (free; next to `r` "Remind me") | Follow… for the focused programme (new `Action::FollowProgramme`). Help text: "r remind once · R follow (every time)". Added to `hint_line`. |
| Programme popover (`Modal`) | ↑↓/←→ to the new **Follow…** button, Enter | Opens the sheet in place of the popover. Esc returns to the guide; the Esc ladder is unchanged. |
| Search overlay | menu key (Shift+F10 / Ctrl+Enter) on a programme hit | Menu item "Follow…" |
| Follow sheet (`Modal`) | ↑/↓ | Move between suggestions and the When row |
| | ←/→ | Change the value on the When row (the overlay interprets Left/Right, as the popover already does) |
| | Enter | Follow (the focused suggestion) |
| | Tab | Reach the buttons |
| | Esc | Cancel |
| Settings ▸ Following (`Panel`) | ↑↓ / j k | Move |
| | Enter | Edit the rule, or "+ Follow…" opens a template chooser (Team · Show · Event · Custom) |
| | Alt+↑/↓ or K/J | Reorder (priority) |
| | `x` / Delete | Menu: Pause/Resume · Duplicate · Delete…. On an Upcoming row: Skip this one · Watch on another channel · Show in guide. |
| | Tab | Rules ↔ Upcoming |
| | ← / h | Page list (pane-arrows rule) |
| Rule editor (`Panel`, fields push `TextInput`) | → / l / Shift+→ / L, Shift+← / H | Change values (pane-arrows idiom) |
| | Chip fields | Type, then Enter adds a chip; Backspace on an empty field removes the last chip |
| | ← / h | Page list (the editor stays open, like the source editor) |
| | Enter on [Save] | Save |
| | Delete… | Asks "Also cancel its 4 upcoming reminders?" (default Yes) |

Esc ladder in the editor:

1. leave the field;
2. close the editor. If there are unsaved changes, the source-editor guard applies: "Unsaved rule changes: Enter on Save, or Esc to discard". A second Esc discards.
3. page list;
4. back.

There are no new Global chords, and nothing is bound in the player: following from the player is done through the guide or search.

### 3.8 Mouse model

- **Guide:** right-click a cell → the channel menu gains "Follow this programme…". A click on a cell does what Enter does: it opens the popover, which has the Follow… button.
- **Rule rows:**
  - a click on a rule row opens the editor;
  - the ● dot toggles pause;
  - drag the 󰇙 grip to reorder;
  - right-click for the menu.
- **Upcoming rows:** a click opens the programme popover; right-click opens the row menu.
- **Chips** have ✕ buttons. **[+ Follow…]** is a button.
- **Sheet:** click a suggestion to select it, then [Follow]. A click outside closes the sheet, because it holds no typed edit (`is_dirty` = false). The editor follows the source-editor dirty rule: the border flashes.
- **Toast:** [Undo]. **Notification:** [Open].

## 4. Data model and migration

The file is `src/store/migrations/00NN_follow.sql`. I expect NN = 0009, after P3's 0007 and the MCP pass-2 reminder columns at 0008. `user_version` is positional, so the number is fixed at merge time. The script is idempotent and any `ALTER` happens in the Rust `backfill` step through `add_column_if_missing`, following the `reconcile_renumbered` pattern.

```sql
CREATE TABLE IF NOT EXISTS follow_rule (
  id           INTEGER PRIMARY KEY,
  name         TEXT    NOT NULL,                       -- "Arsenal"
  kind         TEXT    NOT NULL CHECK (kind IN ('team','show','event','custom')),
  spec         TEXT    NOT NULL,                       -- JSON FollowSpec {"v":1,…}
  mode         TEXT    NOT NULL DEFAULT 'notify' CHECK (mode IN ('notify','switch','list')),
  lead_s       INTEGER NOT NULL DEFAULT 120,
  offset_s     INTEGER NOT NULL DEFAULT 0,             -- e.g. +900 = skip build-up
  spoiler_safe INTEGER NOT NULL DEFAULT 0,
  max_per_week INTEGER NOT NULL DEFAULT 10,
  priority     INTEGER NOT NULL,                       -- list order; lower wins clashes
  enabled      INTEGER NOT NULL DEFAULT 1,
  origin       TEXT    NOT NULL DEFAULT 'user' CHECK (origin IN ('user','agent')),
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

-- The ledger: one row per airing a rule matched. It survives guide generations
-- (programme ids don't), drives dedupe, dismissals, moves, new-only and Upcoming.
CREATE TABLE IF NOT EXISTS follow_match (
  id          INTEGER PRIMARY KEY,
  rule_id     INTEGER NOT NULL REFERENCES follow_rule(id) ON DELETE CASCADE,
  event_key   TEXT    NOT NULL,   -- fixture "arsenal|chelsea@2026-10-18" · "taskmaster#s20e4" · norm(title|sub)
  start       INTEGER NOT NULL,   -- displayed UTC (tvg-shift applied for the chosen channel)
  stop        INTEGER NOT NULL,
  title       TEXT    NOT NULL,
  sub_title   TEXT,
  source_id   INTEGER,            -- chosen channel; NULL = nothing watchable
  channel_key TEXT,
  alternates  TEXT    NOT NULL DEFAULT '[]',   -- ≤ 3 fallback ItemRefs
  state       TEXT    NOT NULL CHECK (state IN
              ('scheduled','adopted','listed','over-cap','skipped','gone','aired')),
  note        TEXT,               -- 'clash:<rule id>' | 'locked' | 'no-interrupt' (downgraded to notify)
  reminder_id INTEGER REFERENCES reminder(id) ON DELETE SET NULL,
  seen_at     INTEGER NOT NULL,
  announced   INTEGER NOT NULL DEFAULT 0,      -- included in a summary already
  UNIQUE (rule_id, event_key, start)
);
CREATE INDEX IF NOT EXISTS follow_match_start    ON follow_match(start);
CREATE INDEX IF NOT EXISTS follow_match_reminder ON follow_match(reminder_id);
-- Rust step: ALTER TABLE reminder ADD COLUMN rule_id INTEGER REFERENCES follow_rule(id) ON DELETE SET NULL
```

`FollowSpec` (in `src/follow/spec.rs`, serde, `v: 1`):

- `terms` (any-of: names and aliases)
- `with_any` (also needs one of these, e.g. "Qualifying", "Grand Prix", "Sprint", "Race")
- `without`
- `fields {title, sub_title, description, categories}`
- `shape: Fixture | TitleIs { spin_offs } | Anywhere`
- `categories` (any-of: Sport, Football…)
- `airings: LiveOnly | LiveOrNew | NewOnly | Any`
- `include_highlights`
- `days` (weekday mask), `between` (local minutes)
- `channels: Any | Favourites | List(FavListId) | Playlist(SourceId) | Channels(Vec<ItemRef>)`
- `prefer {pin: Option<ItemRef>, max_quality}`

A template fills the spec's defaults:

| Kind | Shape | Fields | Airings | Other defaults |
|---|---|---|---|---|
| team | Fixture | title, sub_title, description | LiveOnly | `without` = Women, Ladies, U18–U23, Legends, Fan TV, Highlights |
| show | TitleIs | title | NewOnly | — |
| event | Anywhere | title, sub_title | LiveOnly | — |

Other storage:

- **Config:** `[follow] enabled=true summary=true interrupt_vod=false max_rules=100 max_pending=300`.
- **Retention** (`jobs::maintenance`): ledger rows for show rules are kept for 2 years, because new-only depends on remembering episodes. All other rows are deleted 30 days after `stop`.
- **Backup:** rules are exported and imported with user data (merged by name + spec); the ledger is not.
- **New `DataScope::Follow`.**

## 5. Engine and architecture

### 5.1 Modules

| File | Contents |
|---|---|
| `src/follow/mod.rs`, `spec.rs` | Types and serde |
| `src/follow/matcher.rs` | Pure. `fold()` (ASCII fast path; `deunicode` only for bytes ≥ 0x80; "&" becomes "and"; "vs" and "vs." become "v"). One Aho-Corasick automaton over every enabled rule's terms (`aho-corasick 1.1.5` is already in Cargo.lock through `regex`; add it as a direct dependency pinned `=1.1.5`). Whole-token verification, the fixture check (`T SEP X` / `X SEP T`, SEP ∈ v/vs/versus/-/–/@, the team-suffix exclusions after T or at the end of the fixture), `airing_kind()` (Live/New/Repeat/Highlights/Unknown from flags bits 0–2 plus UK markers: "Live", "(R)", "[R]", "Repeat", "Replay", "Rewind", "Highlights", "Extended Highlights", "Match of the Day"), and `event_key()`. |
| `src/follow/plan.rs` | Pure. Groups hits into events, chooses the channel, applies caps and clash handling, and reconciles against the ledger to produce a `Plan`. |
| `src/store/follow.rs` | Rule CRUD, ledger reads, the streaming scan, and `apply_plan` (one transaction). |
| `src/jobs/follow.rs` | `JobKind::EvaluateFollows { reason }` plus the preview function. |

### 5.2 Threads

No new thread.

- **Evaluation is a job.** It runs on `tuner-rt`, scans on the blocking pool with the job's own read connection (cache `-4000`, matching memory fix #7), and writes through the db-writer in one transaction. It then calls `notify(DataScope::Reminders)` and `notify(DataScope::Follow)`.
- **Dedupe key `"follows"`.** It coalesces while queued. The existing "submit when finished" path queues one more run when a request arrives while a run is in progress, which covers the TV and TV 2 guides finishing back to back.
- **Preview** is a `FollowPreview` service in `services.rs`: a blocking-pool task with a `CancellationToken`, 350 ms debounce, newest request wins. Results come back as `AppEvent::Follow(PreviewReady{rev, …})`. It is not a job, so typing doesn't clutter the jobs overlay.
- **The UI** reads only the ledger for Upcoming (≤ 300 rows, an indexed read within the frame budget, cached until `DataScope::Follow`).

### 5.3 Hooks: what triggers an evaluation

The background rule applies: correctness must never depend on the UI running.

1. **`jobs::import_epg::run`**, success branch, after `notify(ctx, DataScope::Epg)`:
   `if rules exist { ctx.submit(JobKind::EvaluateFollows { reason: Guide }) }`.
   It is skipped when the download was NotModified. It also runs inside `tuner --refresh`, because the headless runner waits for follow-up jobs (`REFRESH_SETTLE`).
2. **`rematch_all`** (RematchEpg): same submit. This covers the separate guide-mapping fixes feature: mapping writes go through `RematchEpg` and `resolve_mappings`.
3. **App side:** a new library (visibility, favourites, locks, playlists changed), a manual EPG mapping change, a rule save, delete or reorder, or `follow_add`. All of these coalesce into one job.
4. **The daily `Maintenance` tick** marks past rows `aired` and prunes.

Early exit: `meta follow:inputs` stores a hash of (active generation per EPG source, library generation, the rules' maximum `updated_at`). If the hash is unchanged and the last run was under 6 h ago, the job exits without scanning.

**Library access.** The job needs the visibility snapshot. MCP PR pass 1's `AgentShared` already publishes it. I suggest lifting that slot into `jobs::LibrarySlot`, shared by the agent handler and this job. Headless `--refresh` has no published snapshot, so it uses `Library::load` (about 0.4 s) with the session treated as locked.

### 5.4 One evaluation step by step

1. **Followable channels.** From the library: channels that are visible (not hidden, supported, from an enabled and eligible playlist), **never explicit** (`Library::explicit` covers both states), inside the rule's channel scope, and mapped through `channel_epg`. Build a bitset over `epg_channel_id` (about 9.3k ids).
2. **Scan.**
   ```sql
   SELECT p.epg_channel_id, p.id, p.start, p.stop, p.flags, p.title, p.sub_title, p.categories, p.descr, p.season, p.episode
   FROM programme p JOIN epg_source s ON s.id = p.epg_source_id AND p.gen = s.active_gen
   WHERE p.stop > :now
   ```
   The `programme_stop` index drives it.
   - Column 0 is checked against the bitset before any text is read.
   - Text is read as borrowed `ValueRef`, folded into a reusable buffer and run through the automaton.
   - Only the rules whose terms hit are evaluated.
   - Cancellation is checked every 8 192 rows.
3. **Events.** Hits are grouped by `(rule, event_key)`, plus a time-overlap fallback (|Δstart| ≤ 10 min) that merges simulcasts on different EPG channels, such as Sky Sports Main Event and Sky Sports Football. Event keys:
   - fixture: the sorted team pair and the local date;
   - show: the folded series title plus `sNNeNN`, else the folded episode title, else a hash of the description's first sentence;
   - otherwise: folded title, episode title and local date.
4. **Airings filter.** "Live only" means marked live, or, when the guide marks nothing, the first airing of that `event_key` seen in the ledger with no repeat or highlights marker. "New only" checks the ledger for an earlier row with the same key and skips it if one exists.
5. **Channel choice.** Candidates are every followable channel on any of the event's EPG channels (`channel_epg_rev`). They are ranked lexicographically:
   1. the rule's pinned channel;
   2. unlocked;
   3. no `stream_failure` in the last 24 h;
   4. in a favourites list (earlier list and position first);
   5. quality: the measured class, else a *trusted* label (§13.0.1), capped by `prefer.max_quality`;
   6. playlist order (TV before TV 2);
   7. channel number.

   The top three are stored in `alternates`, which back "Watch on another channel" and match P3's "Also on". Start and stop are shifted by the chosen channel's tvg-shift.
6. **Caps.** `max_per_week` per ISO week, counting rows already scheduled. Extra events become `over-cap` and are shown greyed out. Global limits: 300 pending reminders made by rules, 100 rules.
7. **Clashes** apply to switch mode only:
   - priority order: a manual or agent one-off switch, then rule priority, then the earlier start;
   - the losers become notify reminders with `note='clash:<id>'`;
   - clashes between notify reminders are only flagged, through the in-flight clash detection.

   Two further cases also downgrade switch to notify:
   - the target is locked (`note='locked'`; the notification text says "needs your PIN");
   - playback at fire time is a film, episode or catch-up and `interrupt_vod` is false. This is checked at fire time by the in-flight scheduler: one predicate it calls for any reminder whose `rule_id` is set.
8. **Reconcile** (pure, `plan::reconcile`). Each fresh event is compared with the ledger rows of the same key whose start lies within ±36 h:

   | Case | What happens |
   |---|---|
   | Unchanged | Nothing |
   | Moved | Update the reminder's start, stop and channel; one toast |
   | Fresh event with no ledger row | Create the reminder through `store::user::add_reminder` with `rule_id`. If a manual reminder already exists at that `UNIQUE(source, channel, start)`, adopt it (`adopted`, never deleted by Following). |
   | Ledger row with no fresh event | Becomes `gone` and its owned pending reminder is cancelled, **only if the guide still covers that time** (`start < epg_source.max_stop`), so a shorter guide doesn't cancel anything. |
   | Ledger row `skipped` (user dismissed it) | Never re-created |

9. **Apply.** One writer transaction, then the summary. A notification for rows not yet `announced` is sent from the job (blocking pool); the in-app toast is the fallback.

**In-flight code this touches:**

- `store::user::remove_reminder` and the `r` toggle must set `follow_match.state='skipped'` in the same transaction before deleting.
- Editing a rule reminder's lead or mode in Settings ▸ Reminders sets `reminder.rule_id = NULL` and the ledger row to `adopted`. The user's edit wins and the rule no longer owns that reminder.
- Systemd timers: rule-made reminders arm the pass-2 `systemd-run --user` timers only when they fire within 48 h, with at most 30 transient units. Each evaluation and the daily tick re-arm them. Manual reminders keep their current behaviour.
- When a spoiler-safe rule fires late, catch-up "From the start" is offered first. If auto-switch is late and catch-up exists, it plays from the start; otherwise it plays live with "joined 23 min in".

### 5.5 Performance at the user's scale

The user has 760,466 programmes on 9,303 EPG channels, of which roughly 450k are in the future.

- Each row costs one SQLite step plus about 300 B of folding and an Aho-Corasick pass, roughly 1.5 µs.
- **Bench gate:** a full evaluation under 1.5 s, and a single-rule preview under 1 s, at 760k programmes and 100 rules × 10 terms, on the M2 release build. Cancel within 50 ms. Extra heap under 8 MB, because only hits are kept.
- An evaluation runs about twice a day, after an EPG import that itself takes over 8 s.
- **Fallback if the gate fails:** match inside the import stream. `write_programmes` already has every batch in memory, so it can record hits by `(epg_channel_id, gen, start)` and the scan becomes a no-op. Don't build this unless the bench fails.

## 6. Guardrails

- **Explicit:** never matched, counted, previewed or listed, in any parental state. MCP refs to explicit items come back "not found".
- **Locked:** notify is allowed. A switch is downgraded with "needs your PIN". `--play` from a timer keeps the in-flight PIN rule.
- **Privacy:**
  - INFO logs carry only counts and timing: "follow: 4 rules, 0.71 s, +2 ~1 −0".
  - Terms, titles and channels go to `debug` only, matching the "no titles at INFO" fix.
  - Rules are local user data and are included in backups.
- **Connection limits:**
  - Evaluation never touches the network: no probes and no refresh of its own.
  - A switch is one stream through the existing budget (stop-before-start on the same account).
  - Multi-view, later: a switch could target a tile, preferring the playlist that isn't in use. I've left a `FireTarget` enum placeholder; nothing is built for it.
- **Interruptions:**
  - live TV switches with the in-flight 10 s Cancel toast;
  - films are never interrupted by default;
  - two rule switches never overlap.
- **Caps:** per rule per week, 300 pending, 100 rules, 30 transient timers, and the MCP limits in §8.

## 7. Spoiler-safe and the app-wide spoiler guard

- **Inside Following:** notifications, the summary, Upcoming, toasts and MCP results show only the rule name, the time, the channel and Live. No episode title, description, stage words ("Final", "Semi") or opponent. Highlights and results programmes are always left out. Late fires offer "from the start" first.
- **For the separate spoiler-guard feature:** `follow::spoiler_terms(&rules) -> Vec<GuardTerm>` exposes the folded terms of spoiler-safe rules. The guard owns blurring of guide, search and notification text and the "until watched" state.

## 8. MCP tools and skill changes

These go in the in-flight `src/agent/{proto,tools,server}.rs`.

| Tool | Params | Returns | Rules |
|---|---|---|---|
| `follow_preview` (read) | `rule: FollowRuleInput` or `id`, `days` 1–14 (default 14), `limit` ≤ 50, `spoiler_safe` (default from the rule) | `matches[{programme_ref, start, stop, title, sub_title?, channel{ref, name, playlist, quality}, also_on[], airing, state: would_remind \| would_switch \| over_cap \| clash_with(id) \| left_out(reason)}]`, `counts`, `warnings` (e.g. "'England' also matches rugby and cricket") | Explicit items absent, counts included. Spoiler-safe strips sub_title and description. |
| `follow_add` (action) | `rule: FollowRuleInput`, `replace_id?` | `{id, upcoming, reminders_set}` | One rule per call, 10 per hour, at most 20 rules made by the agent. Saved with `origin='agent'` (marked in Settings). The app shows "Claude added Arsenal · 4 reminders [Undo]". |
| `follow_list` (read) | — | rules with mode, status and the next 3 matches (spoiler-safe applied) | — |
| `follow_remove` (action) | `id`, `keep_reminders` (default false) | `{removed, reminders_cancelled}` | — |

`FollowRuleInput` = `{name, kind, terms[], with_any[], without[], look_in?, sports?/categories[], airings, channels: "any" | "favourites" | {list} | {playlist} | {channels: [refs]}, mode, lead_min, offset_min, spoiler_safe, max_per_week}`.

**`plan` skill:**

- "Remind me for every Arsenal game" becomes a persistent rule, replacing the one-off pass:
  1. expand aliases and sessions from Claude's knowledge, labelled as such;
  2. `follow_preview`;
  3. show the matches and what was left out, then tune `without` and sports;
  4. confirm once (mode, lead, spoiler-safe);
  5. `follow_add`;
  6. say "I'll keep checking after every guide refresh".
- New triggers: "what am I following", "stop following X", "why wasn't I reminded about X". The last one uses `follow_preview(id)` with its left-out reasons, plus `sources` for guide coverage.

**RULES.md additions:**

- a rule is a standing order: always preview and confirm, and confirm switch mode explicitly;
- never create a rule from an inferred interest;
- spoiler-safe defaults on for sport;
- don't repeat rule terms back into logs.

**`fix`** gains the "why no reminder" path. **`watch`** hands "every time X is on" to `plan`.

## 9. Tests worth having

1. **`matcher` with real UK guide strings:**
   - "Premier League Football" with episode title "Arsenal v Chelsea" matches; "Live: Arsenal vs. Chelsea" matches.
   - "Arsenal Women v Chelsea Women" and "Arsenal v Chelsea Women" don't.
   - "Arsenal: The Invincibles" doesn't match the Fixture shape.
   - "Chelsea v Arsenal (R)" is a repeat; "Match of the Day" with Arsenal in the description is highlights.
   - "Formula 1: Live Japanese Grand Prix Qualifying" matches; "F1 Academy", "The F1 Show" and "Japanese GP Practice 3" are left out.
   - "Taskmaster NZ" is left out under TitleIs, and included with spin-offs on.
   - Diacritics and full-width letters fold.
2. **`airing_kind` table test** over the flag bits and markers.
3. **`reconcile`:**
   - a moved kick-off updates the reminder and doesn't duplicate it;
   - a dismissed match stays dismissed across two evaluations;
   - `gone` happens only inside guide coverage;
   - a manual reminder is adopted and survives rule deletion;
   - the ISO-week cap holds;
   - a clash downgrades by priority, with manual winning;
   - new-only skips an unflagged repeat.
4. **Channel choice:**
   - favourites beat quality, quality beats playlist order;
   - an explicit channel is never chosen, even when unlocked;
   - hidden and unsupported channels are excluded;
   - a locked channel turns switch into notify with `note=locked`;
   - tvg-shift is applied to the start time.
5. **Job integration** in `tests/`, with a temporary DB and a small XMLTV fixture through the real `import_epg`:
   - reminders are created with `rule_id`;
   - a re-import with a moved time updates them;
   - NotModified submits nothing;
   - `tuner --refresh` runs the follow-up.
6. **Store:** deleting a rule reminder through `remove_reminder` marks the ledger row `skipped`.
7. **Migration:** a database from the previous version with reminders gains `rule_id`, the existing rows are untouched, and re-running is harmless.
8. **`benches/follow.rs`:** 760k programmes and 100 rules under 1.5 s; preview cancel under 50 ms.
9. **MCP:**
   - spoiler-safe strips text;
   - explicit is absent, counts included;
   - `follow_add` is rate-limited and saved with `origin='agent'`.
10. **UI** (the `ui::testing` harness):
    - `R` resolves in the Guide context;
    - a dirty editor follows the Esc ladder (guard, then discard);
    - the sheet's Enter follows the focused suggestion;
    - the help table is generated.

## 10. Implementation plan

**Prerequisites:**

- Merge the MCP PR's pass 2 first, or at least its reminder columns, scheduler hook and Settings reminders list.
- P3 and pane-arrows touch `settings.rs`, so the Following page lives in its own file.

**Tracks:**

| Track | Files (ownership is separate per track) | Approx. lines |
|---|---|---|
| **A. Engine (pure)** | `src/follow/{mod,spec,matcher,plan,suggest}.rs` + unit tests | 1 300 + 800 tests |
| **B. Storage and job** | `store/migrations/00NN_follow.sql`, `store/migrations.rs` (entry + backfill), `store/follow.rs`, `jobs/follow.rs`, `jobs/mod.rs` (JobKind, dedupe, execute), `jobs/import_epg.rs` (hook, ~10), `jobs/maintenance.rs`, `store/user.rs` (dismiss/detach, ~20), `store/backup.rs`, `msg.rs` (`DataScope::Follow`), `config.rs` (`FollowConfig`), `benches/follow.rs`, `tests/follow_job.rs` | 900 + 400 tests |
| **C. UI** | `ui/screens/following.rs` (page, editor, Upcoming), `ui/overlays/follow_sheet.rs`, `ui/overlays/programme.rs` (+Follow…), `ui/overlays/search.rs` (menu item), `ui/screens/guide.rs` (`R`, menu item), `keymap.rs` (`FollowProgramme`, `R`, hint), `ui/epg_grid.rs` (tooltip), `ui/screens/settings.rs` (page entry + Reminders tag, ~40), `app.rs` (submits, toasts, `AppEvent::Follow`), `services.rs` (preview service) | 1 500 |
| **D. Agent and skill** | `agent/proto.rs`, `agent/tools.rs`, `agent/server.rs` (4 tools); plugin `skills/plan/SKILL.md`, `skills/fix/SKILL.md`, `RULES.md`; `docs/DESIGN.md` §31; `docs/MANUAL_CHECKLIST.md` | 600 |

Total: about 4.3k lines plus about 1.2k lines of tests.

**Order:**

1. Tag a skeleton `follow-base` with the types, the store API signatures and the JobKind (half a day).
2. Build A and B in parallel.
3. Build C on B's API.
4. Build D last.

**Fastest useful slice ("STAT"):**

- A, plus B without the summary, plus a Following list with the Team/Show/Event editor, the sheet and `R`.
- Then the summary notification, `follow_*` MCP tools and skill, and alias suggestions.

## 11. Risks

- **Guide data quality.** Missing `<live/>` marks, teams named only in the description, and duplicate simulcasts. Mitigated by the first-airing rule, searching descriptions for team rules, the time-overlap merge, and a preview that shows what was left out and why.
- **Event identity for shows without S/E or episode titles.** Falls back to a hash of the description's first sentence; new-only may then remind twice. The tests cover it, and the user can skip.
- **Migration numbering across three in-flight branches.** Idempotent SQL, a Rust `add_column_if_missing`, and the number assigned at merge.
- **Systemd unit spam and stale closed-app timers.** The 48 h arming window and the 30-unit cap. A move that happens while Tuner is closed isn't seen until the next refresh.
- **Coupling to MCP pass 2's reminder code** (`remove_reminder`, the fire-time predicate, the timer arming). This needs a short interface agreement before Track B starts.
- **Spoiler leaks outside Following** (guide cells, search) until the spoiler-guard feature lands. This is documented in the editor's spoiler-safe help text.

## 12. Open questions (each with a recommended default)

1. **Should a rule's auto-switch interrupt a film, episode or catch-up you're watching?** Recommended: **no**. It becomes a notification with Watch. Live TV still switches, with the 10 s Cancel toast. A setting (`follow.interrupt_vod`) can turn interrupting on.
2. **What should "every England match" cover by default?** Recommended: **any sport, live only, men's senior teams** (Women and U21 left out). The editor shows sport chips, and the preview warns when several sports are mixed.
3. **Should spoiler-safe be on by default?** Recommended: **yes for Team and Event rules, off for Show rules.** In spoiler-safe mode, notifications and the Upcoming list show only "Arsenal · Sat 12:30 · channel", and a late fire offers "From the start" first.
