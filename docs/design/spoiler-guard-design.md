# Spoiler guard: design

> The design as approved on 2026-10-11, kept for its user stories, mockups and numbered rules (code comments cite them as "DESIGN §34 §x.y"). The answers to its open questions and what was built differently are in DESIGN.md §34 (Spoiler guard).

I based this on `origin/main` (013f897) and read-only looks at the in-flight worktrees `feat/mcp` (p21, uncommitted), `feat/p3-copies` and `feat/pane-arrows`. Nothing was modified. The section number is §3x; the MCP pass is §30, so the number gets assigned at merge.

## 0. What the code allows and constrains (verified)

- **Text that can spoil, and where it is painted today:**
  - `programme.sub_title`: in `widgets/rows.rs` programme rows (`"title · sub"`), `epg_grid.rs:752`, `overlays/programme.rs:209` and `search.rs` hit matching.
  - `programme.descr`: in `overlays/programme.rs:250`, `screens/guide.rs:789` (tooltip and preview) and `screens/live.rs:433/1894` (the details pane).
  - Episode `title` and `plot`: in `series_detail.rs` / `rows::episode_row`.
  - VOD names and plots: in the movie and series pages and the poster cards.
  - Outside the window: MPRIS `xesam:title` (`mpris.rs:189`, the programme title shown in the Omarchy bar) and reminder notifications (`notify.rs::note_for`).
- **Pictures:** episode `still` and programme `icon` are stored but never painted. The one exception is `Playable.logo` for the episode that is playing, which is not a spoiler. So "thumbnails" is a hook to keep future still painters honest, not current work.
- **Watch data:** there is no record of watching at programme level. `history` is per item/channel (`at` only) and `watch_progress` covers VOD and catch-up. The guard needs a small "seen" table of its own.
- **Patterns to copy:**
  - `ExplicitGate` and `AgentShared` (an `RwLock<Arc<…>>` snapshot read by the UI, the scheduler and the MCP handler off the UI thread).
  - `Library::conceals`, which fails closed before the first build.
  - `ProgrammeCache` (6 h buckets, LRU 4 000).
  - The `DataScope` invalidation.
- **Dependencies:** `aho-corasick` and `regex` are already in the lock file, so adding them directly brings in no new crate.
- **Free keys:**
  - Ctrl+B and Ctrl+Shift+B are unbound in every table: `GLOBAL`, `ITEM_ACTIONS`, `SEARCH*`, the player tables, and the in-flight branches.
  - Ctrl+W is free in `GUIDE`.
  - `feat/pane-arrows` moves Settings steppers to Shift+←/→ (H/L), and ← goes back to the page list.
- **Migration number:** `0007_title_join.sql` is taken by P3, and the MCP pass 2 may take 0008. The guard's migration is **`0009_spoiler_guard.sql`, renumbered at merge**.

## 1. User stories

1. "Arsenal play at 17:30 and I'll watch on catch-up at 21:00. Until then nothing in Tuner tells me the score: not the guide, search, Home, notifications, the Omarchy bar or Claude."
2. "I'm on S2E4 of Severance. The series page shows E04's title and plot, but E05 and later show no titles, plots or stills. That applies on TV and TV 2, in the guide's S/E airings, and in Claude's answers."
3. "When I zap past Sky Sports News on match day, Tuner stops before tuning and asks."
4. "One key shows the hidden text on the thing I'm looking at, and moving on hides it again. Another key shows everything for 15 minutes."
5. "When I watch the end of the match live or on catch-up, the guard lifts by itself and a toast offers Undo."
6. "When I follow a team (a follow rule), the guard is ticked for sport by default."
7. "'Guard spoilers about…' on a guide cell suggests the teams from its title."
8. "Claude's `plan` and `watch` skills respect the guard. They reveal only when I ask, in that turn."
9. "With the parental lock on, the guard never lists, counts or reveals anything explicit."

## 2. UX

### 2.1 Concepts (what the user sees in Settings)

- **Topic rule.** Words plus aliases (for example Arsenal = "arsenal", "gunners"), with an optional category scope (Sport / any) and a mode:
  - **Until seen**, the default. Each matching **event**, a programme whose title matches, opens an *embargo* at its first airing. The embargo lasts until the user has seen any airing of it, or until `window_h` after it ends (default 48 h). While it is open, any text that mentions the topic is guarded, and with "sport descriptions" on, so are the sub-titles and descriptions of all sport-category programmes.
  - **Always.** Every mention is always guarded, for example a reality-show final.
- **Blackout ("I'll watch it later").** A topic plus an end time. It also shields the live event itself, because its score bug is on screen.
- **Series rule.** Episodes after the *next-up* episode are guarded: title, plot, still, sub-title and description. This works across copies (merged progress from P3), VOD episodes, and EPG airings that carry S/E numbers. EPG airings of a guarded series with no S/E get their sub-title and description hidden.
  - Series you are part-way through are guarded automatically: at least one episode finished, one not, and activity within 180 days.
  - Rows only store your exceptions and explicit additions.
- **Before an event starts, nothing is guarded.** Fixtures stay plannable.
- **Display only.** Stored data, FTS and sort order never change.

### 2.2 Visual treatment (Omarchy tokens)

| What | Treatment |
|---|---|
| Score in a title | Rewritten inline, then a dim glyph: `Arsenal 2–1 Chelsea` → `Arsenal v Chelsea 󰈉` (󰈉 in `text_disabled`) |
| Title with a result verb plus a topic hit ("Hamilton wins…") | `‹rule label› · title hidden 󰈉` in `text_secondary` |
| Sub-title or episode title | A redaction bar: a rounded rect with `separator` fill (fg @ 0.12) at `rounding`, glyph-height high. **Fixed width per field kind, never the text's length** (no length leak). The 󰈉 glyph follows the bar. |
| Description or plot | A block with a `card_border` 1 px outline and `fill_field`: "󰈉 Description hidden — mentions Arsenal · until you watch *Arsenal v Chelsea* or Mon 19:30 · Ctrl+B or click to show" |
| Revealed | The real text, plus 󰈈 in `accent` and a 2 px `accent` left rule, so you know it's peeking |
| Image (future painters) | `images::Variant::Obscured`: downscale to 12 px wide, upscale with linear filtering, add `scrim` and a centred 󰈉. No shader, and the variant gets its own cache key. |
| Guide cells | No bars, because cells are too small. Titles are rewritten plus 󰈉, the tooltip says "Description hidden (Arsenal)", and the cell is never coloured differently. |
| Status bar (right) | While paused: chip `󰈈 Spoilers shown · 14 min ✕`. Otherwise nothing (no permanent chrome). |
| AccessKit names | "Hidden by spoiler guard (Arsenal)", never the text |

### 2.3 Mockups at 1600 pt (Wide)

```
┌──────────────┬───────────────────────────────────────────────────────────────────────────────────────────────┐
│ 󰋜 Home       │ TV GUIDE  ● TV ▾  [Sports ▾]  [‹] Sat 18 Oct [›]  [Now]                    f collection · n now │
│ 󰔂 Live TV    ├──────────────────────┬──────────────────────────┬──────────────┃┬────────────────────┬───────────┤
│ 󰃰 TV guide   │                      │ 17:00                    │ 18:00        ┃│ 19:00              │ 20:00     │
│ 󰎁 Movies     ├──────────────────────┼──────────────────────────┴──────────────┃┴────────────────────┼───────────┤
│ 󰟴 Series     │ ▣ 401 Sky Sp Main Ev │ PL: Arsenal v Chelsea                 LIVE ┃ │ Football Reaction 󰈉 │ …         │
│ 󰍉 Search     │ ▣ 402 Sky Sp PL      │ ⟲ Classic: Arsenal v Spurs 󰈉 │ ┌Goals Express 󰈉 (focus)┐ │ The Debate│
│ 󰒓 Settings   │ ▣ 230 Sky Sp News    │ Sky Sports News 󰈉 (shielded while Arsenal is guarded)    ┃           │
│              │ ▣ 101 BBC One        │ Final Score             │ News             ┃ │ Match of the Day 󰈉  │
│              │ ▣ 102 BBC Two        │ ⟲ Severance · S2E4      │ Severance · S2E5 ▆▆▆▆▆ 󰈉                      │
├──────────────┴──────────────────────┴──────────────────────────────────────────────────────────────────────────┤
│ ↵ details · w watch · Ctrl+B show spoilers · Ctrl+W mark seen · ? help                    Guide updated 2 h ago │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘

Programme popover (Enter)                               Series page (Severance, next-up = S2E4)
┌──────────────────────────────────────────────────┐    ┌───────────────────────────────────────────────────┐
│ Match of the Day                                 │    │ Severance (2022) · Drama · 8.7                    │
│ ▆▆▆▆▆▆▆▆▆▆▆▆▆ 󰈉                                  │    │ [Season 1] [Season 2]                             │
│ Sat 18 Oct · 22:20 – 23:40 · BBC One             │    │ E03  Who Is Alive? · 50 min                    ✓  │
│ [Football]                                       │    │▸E04  Woe's Hollow · 52 min          ▕███░░░▏      │
│ ┌──────────────────────────────────────────────┐ │    │ E05  ▆▆▆▆▆▆▆▆▆▆ 󰈉 · 54 min                        │
│ │ 󰈉 Description hidden — mentions Arsenal       │ │    │ E06  ▆▆▆▆▆▆▆▆▆▆ 󰈉 · 50 min                        │
│ │ Until you watch Arsenal v Chelsea, or Mon     │ │    │ E07  ▆▆▆▆▆▆▆▆▆▆ 󰈉 · 49 min                        │
│ │ 19:30. Ctrl+B or click to show.               │ │    │  (plot under the header: E04's; later plots hidden)│
│ └──────────────────────────────────────────────┘ │    └───────────────────────────────────────────────────┘
│ [󰂚 Remind me] [󰈈 Show spoilers] [✓ Mark seen] [Close]│
└──────────────────────────────────────────────────┘

Settings ▸ Spoiler guard
┌──────────────┬─────────────────────────┬──────────────────────────────────────────────────────────────────────┐
│ sidebar      │ General                 │ SPOILER GUARD                                                        │
│              │ Playlists               │ Spoiler guard                                          [ On  ]       │
│              │ Guide                   │ Series I'm part-way through                            [ On  ]       │
│              │ Parental                │   titles, plots and stills after the next episode                    │
│              │▸Spoiler guard           │ Sport I follow (new follow rules guard spoilers)       [ On  ]       │
│              │ Reminders               │ Hide scores in every sport title                       [ Off ]       │
│              │ …                       │ Don't tune guarded channels when zapping               [ On  ]       │
│              │                         │ Guard time after it airs                               ‹ 48 h ›      │
│              │                         │ RULES ─────────────────────────────────────────── [+ Guard a topic]  │
│              │                         │ 󰈉 Arsenal      topic · Sport · until seen ≤ 48 h · ★ followed        │
│              │                         │                guarding now: Arsenal v Chelsea (Sat 17:30)           │
│              │                         │ 󰈉 Severance    series · automatic · you're on S2E4                   │
│              │                         │ 󰈉 Strictly     topic · always                                        │
│              │                         │ 󰈈 The Traitors series · not guarded (your choice)                    │
│              │                         │ SEEN RECENTLY ── Arsenal v Chelsea · watched live Sat 19:28  [Undo]  │
└──────────────┴─────────────────────────┴──────────────────────────────────────────────────────────────────────┘
```

**Zap shield** (fullscreen player, the same at any size). The stream is **not opened**, which also saves the single connection.

```
┌──────────────────────────────────────── (black: not tuned) ─────────────────────────────────────────┐
│                       ┌──────────────────────────────────────────────────────┐                      │
│                       │ 󰈉  230 Sky Sports News                                │                      │
│                       │ This channel may show the Arsenal result —           │                      │
│                       │ you haven't watched Arsenal v Chelsea yet.           │                      │
│                       │ [ Watch anyway ↵ ]  [ Next channel ↓ ]  [ Back ⌫ ]   │                      │
│                       └──────────────────────────────────────────────────────┘                      │
│ 230 Sky Sports News · Sky Sports News 󰈉                         ↑↓ zap · ↵ watch anyway · Esc leave │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Ctrl+Shift+B menu** (launcher card, Modal):

```
┌ Spoiler guard ─────────────────────────────────┐
│▸ Show everything for 15 minutes                │
│  Show everything for 1 hour                    │
│  Show everything until Tuner restarts          │
│  I'll watch "Arsenal v Chelsea" later…         │   ← only when a programme is focused (blackout)
│  Guard a topic or team…                        │
│  Spoiler guard settings…                       │
└────────────────────────────────────────────────┘
(paused: the first row reads "Guard again now")
```

**Rule editor** (Modal, TextInput fields; "Guard spoilers about…" from a cell pre-ticks terms parsed from `A v B` / `A vs B` / `A - B`):

```
┌ Guard spoilers about ──────────────────────────────┐
│ From "PL: Arsenal v Chelsea":  [x] Arsenal  [ ] Chelsea  [ ] Premier League │
│ Also called   [ Gunners, AFC              ]        │
│ Only in       ‹ Sport ›                            │
│ Hide until    ‹ I've watched it, at most 48 h ›    │   (always · until a date)
│ Linked        ★ follows Arsenal (follow rule)      │
│                               [ Save ]  [ Cancel ] │
└────────────────────────────────────────────────────┘
```

### 2.4 About 950 pt (Medium: icon rail, 140 px channel column, 3 px/min)

```
┌──┬──────────────────────────────────────────────────────────────────────┐
│󰋜 │ TV GUIDE ● TV ▾ [Sports ▾] [‹] Sat 18 [›] [Now]                      │
│󰔂 ├────────────┬──────────────────┬───────────┃┬────────────────────────┤
│󰃰 │            │ 17:00            │ 18:00     ┃│ 19:00                  │
│󰎁 ├────────────┼──────────────────┴───────────┃┴────────────────────────┤
│󰟴 │▣ 401 Sky ME│ PL: Arsenal v Chelsea   LIVE ┃ │ Football Reaction 󰈉   │
│󰍉 │▣ 230 SSN   │ Sky Sports News 󰈉            ┃                         │
│󰒓 │▣ 102 BBC 2 │ ⟲ Severance·S2E4 │ Severance · ▆▆▆ 󰈉                     │
├──┴────────────┴──────────────────────────────────────────────────────────┤
│ ↵ details · Ctrl+B show spoilers · ? help                     󰈈 shown 14m│
└──────────────────────────────────────────────────────────────────────────┘
```

- Live TV Medium preview pane: the description block becomes a one-line "󰈉 Description hidden (Arsenal) · Ctrl+B".
- The Settings page shows content only, with `‹ Settings` back; rules show two lines each.
- The popover keeps its 520·rem width, capped to the window.
- Compact: bars shrink and the description block becomes the glyph plus "hidden".

### 2.5 Keyboard and mouse model

| Input | Where | Does |
|---|---|---|
| **Ctrl+B** (`Global`, `Action::RevealSpoilers`) | Every screen and overlay: list row, grid card, guide cell, episode, search hit, popover, player banner, mini guide, channel list | Show or hide the guarded fields of the **focused** item. Revealed items stay shown until focus leaves the item or the overlay or screen closes (`RevealSet`, memory only), then hide again. On an unguarded item: toast "Nothing hidden here". A Ctrl chord, so it works in filter lists (letters type) and in `TextInput` (Ctrl chords reach Global). |
| **Ctrl+Shift+B** (`Global`, `Action::SpoilerMenu`) | Anywhere | The menu above: pause for 15 min / 1 h / session, blackout the focused programme, add a topic, settings. |
| **Ctrl+W** (added to `GUIDE`, popover, search guide hits; `MarkWatched`) | Programme | Mark seen, which lifts its event or episode. It already means "mark watched" on VOD, so the meaning stays consistent. |
| Menu key / Shift+F10 / Ctrl+Enter / right-click | Any guarded item | Adds "Show spoilers (Ctrl+B)", "Mark seen (Ctrl+W)", "Guard spoilers about…", "Stop guarding ‹label›", and on a programme "I'll watch this later…" |
| Shield card | Player (live) | Enter = watch anyway (reveals and tunes; `FocusOsdControls` is taken by the screen while the card shows, the same pattern as "Next episode in 10 s"). ↑/↓/PgUp/PgDn zap on (the next channel is judged again). Backspace = last channel. Esc = normal ladder (hide banner → leave the player; nothing is playing). |
| Settings page | `Panel` | ↑/↓ rows, Enter/Space toggle, `x`/Delete = rule menu (delete or disable), Shift+←/→ (H/L) = steppers after pane-arrows, ← = back to the page list. |

- **Esc ladder:** unchanged. Esc never reveals or hides.
- **Arrow-key pane navigation:** no new focus stops. Bars are not focusable; reveal acts on the row's cursor.
- **Mouse:**
  - A click on a redaction bar or description block reveals that item. The bar has its own interact rect that consumes the click, so the row doesn't activate (the same idea as the reorder grips on top of their rows).
  - A click on 󰈈 hides it again.
  - Hover shows the reason tooltip.
  - The status chip: click opens the menu, ✕ guards again.
  - Shield and popover buttons are clickable.
  - Guide cells: right-click opens the menu with "Show spoilers".
- **Hint line:** when the focused item is guarded, `keymap::hint_line` adds "Ctrl+B show spoilers".

## 3. Data model and migration

### `0009_spoiler_guard.sql` (renumber at merge)

```sql
CREATE TABLE spoiler_rule (
  id             INTEGER PRIMARY KEY,
  kind           TEXT NOT NULL CHECK (kind IN ('topic','series','blackout')),
  label          TEXT NOT NULL,                 -- "Arsenal" (Settings, tooltips)
  terms          TEXT NOT NULL DEFAULT '[]',    -- JSON folded phrases incl. aliases (topic/blackout)
  series_norm    TEXT,                          -- title-stack norm name (series)
  categories     TEXT NOT NULL DEFAULT '[]',    -- JSON guide-category scope; [] = any
  mode           TEXT NOT NULL DEFAULT 'until-seen' CHECK (mode IN ('until-seen','always')),
  window_h       INTEGER NOT NULL DEFAULT 48,   -- cap after the event ends; 0 = none
  until_at       INTEGER,                       -- blackout end
  sport_descr    INTEGER NOT NULL DEFAULT 1,    -- also hide sport sub/descr while embargoed
  shield         INTEGER NOT NULL DEFAULT 1,    -- zap shield for this rule
  enabled        INTEGER NOT NULL DEFAULT 1,    -- 0 on a series row = "don't guard" exception
  origin         TEXT NOT NULL DEFAULT 'user' CHECK (origin IN ('user','follow','agent')),
  follow_rule_id INTEGER,                       -- no FK: follow rules may land later; app cleans up
  created_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX spoiler_rule_series ON spoiler_rule(series_norm) WHERE kind = 'series';

-- Only guarded subjects are ever recorded (no general programme watch log).
CREATE TABLE spoiler_seen (
  title_core  TEXT    NOT NULL,                 -- spoiler::text::title_core (channel-agnostic)
  first_start INTEGER NOT NULL,                 -- EPG time of the first airing
  season      INTEGER, episode INTEGER,         -- series airings seen on TV / catch-up
  seen_at     INTEGER NOT NULL,
  how         TEXT NOT NULL CHECK (how IN ('live','catchup','vod','marked','agent')),
  PRIMARY KEY (title_core, first_start)
) WITHOUT ROWID;
CREATE INDEX spoiler_seen_at ON spoiler_seen(seen_at);
```

- **Keys:** rules hold no `source_id`, so they survive refreshes, re-keys and the move between TV and TV 2 with no reattach work.
- **Events:** `title_core` strips "LIVE", "Live:", "(R)", "Replay", quality and +1 tags, then folds through `name_tokens`. An airing matches a seen row when the cores are equal and the starts are within 7 days. One "seen" therefore covers repeats, +1 channels and both playlists.
- **Pruning:** seen rows are pruned 30 days after their window closes (`jobs::maintenance`).
- **Backup:** both tables go into the backup (user data).
- **Config** (`config.rs`, new `SpoilerConfig` under `[spoiler]`): `enabled`, `series_auto`, `follow_sport_default`, `hide_scores_everywhere`, `shield_zap`, `default_window_h`. Paused-until is session-only and never persisted.
- **`msg.rs`:** add `DataScope::Spoiler`.

## 4. Engine and architecture

### `src/spoiler/` (pure except `tracker`)

- **`text.rs`**
  - `fold` reuses `name_tokens`.
  - `TopicMatcher`: aho-corasick, LeftmostLongest, over folded text with a word-boundary check. "Arsenal's" matches, "Arsenale" doesn't.
  - `title_core`.
  - `neutralise_scores(&str) -> Cow<str>`:
    - Matches `‹letters› N[-–—:]M ‹letters›` with N up to 3 digits, an optional `/w`, and optional `(aet|pens|n-m pens)`.
    - Excludes chains (`9-1-1`), times (`:` with a two-digit M and N ≤ 23), season ranges and `S2-E5` (letters must sit on both sides, with a space).
    - Replaces with `v`.
    - Applied to titles in sport categories, or titles with a topic hit, or everywhere when `hide_scores_everywhere` is on.
  - Result-verb list (beat/beats/wins/won/draw/thrash/edge/stun/crowned/knocked out…). With a topic hit, this redacts the whole title.
- **`snapshot.rs`:** `GuardSnapshot { gen, on, matcher, rules, embargo: Vec<SmallVec<Window{open, close, title_core}>> per topic, series: HashMap<SeriesNorm, Boundary(s,e)>, vod_hits: bitset over VodIndex + topic ids, built_at }`.
  - `build(reader, cfg, &Library, now)` runs on the **tokio blocking pool**, never the UI thread.
  - Events come from an FTS `programme_fts MATCH` per term, joined on `start BETWEEN now−max_window−7d AND now+36h`. Explicit channels are skipped while hidden.
  - Series boundaries come from merged progress (P3 copies API) plus `spoiler_seen` S/E.
  - VOD hits: one aho-corasick pass over the snapshot's movie and series names.
- **`verdict.rs`:** `Subject::{Programme{title, sub, descr?, cats, s_e, start}, Vod{idx}, Episode{series_norm, s, e}, Notification{title}}`, and `judge(&Subject, now) -> Verdict { title: Shown, sub/descr: Keep|Hide, image: Keep|Obscure, reason: Option<Reason{rule, until}> }`.
  - Time only enters as a binary search over a topic's windows. That is why windows don't need a rebuild when time passes.
- **`mod.rs`:** `SpoilerGate`: an `RwLock<Arc<GuardSnapshot>>` plus `AtomicI64 paused_until`, cloned into the app, `ReminderScheduler`, MPRIS and `AgentShared`.
  - Before the first build it **fails closed**: series after next-up are hidden and every topic term counts as embargoed. The first build arrives in under 1 s.
- **`tracker.rs`:** a tokio task polls `PlayerHandle::snapshot()` every 30 s (so it works while the window is hidden).
  - Live: it accumulates minutes for the channel's current programme from a cached `now_next`, **only if the snapshot guards it**.
  - Seen when watched ≥ 50 % of the programme, or present during its last 10 minutes.
  - Catch-up and VOD: seen at ≥ 90 % or `finished`.
  - On seen it writes `spoiler_seen` through the store writer, raises `DataScope::Spoiler`, and shows the toast "Arsenal v Chelsea marked seen — guard lifted [Undo]".

### Hook points

- **Grid:** `ProgrammeCache` entries gain a `GuardMark` (topic ids plus a series flag), computed from title and sub-title at bucket load (about 200 ns each). On a gate-generation mismatch the marks are recomputed lazily with no DB reload.
- **Descriptions:** judged where a full `Programme` is loaded: popover `load`, `live.rs` details, the guide tooltip and preview.
- **Painters:** `rows.rs` (programme_row, episode_row), `epg_grid.rs`, `overlays/{programme,search}.rs`, `screens/{guide,live,home,movies,series,series_detail,movie_detail,player}.rs`. All go through `ui/widgets/spoiler.rs` (bar, block, guarded label, obscured image).
- **Outside the window:**
  - `notify.rs::note_for` takes the gate and neutralises the title; sub-titles and descriptions are never included.
  - `mpris.rs::metadata` neutralises `xesam:title` for live programmes.
  - The auto-switch toast and the play queue panel (sibling feature) use the same `judge`.
- **Zap shield:** in `screens/player.rs` before `PlayerHandle::play` for live. It applies when `judge` says shield: the programme text matches an embargoed topic and is not the event itself, or a blackout covers it. Each multi-view tile, when that feature lands, calls the same check.
- **Rebuild triggers:** `DataScope::{Epg, Live, Vod, Progress, Spoiler}`, config change, and an hourly tick that extends the 36 h lookahead. Builds are debounced to 1 s.

### Performance at the user's scale (79 k channels, 302 k movies)

- **Off the UI thread:** about 10 topics × an FTS query of under 20 ms each, plus an aho-corasick pass over about 380 k names (estimated 15–25 ms on M2, release). Under 300 ms in total.
- **Memory:** the snapshot is under 300 KB; the bitset is 38 KB.
- **Per frame:** about 200 visible cells × (mark lookup + binary search) is under 0.05 ms. The `benches/guide.rs` budget (< 8 ms while holding ↓ over 5 000 channels) must still hold.

## 5. Guardrails

- **Parental and explicit:** the guard runs **after** `Visibility`. It never builds events from, suggests terms from, counts or lists explicit items while they are hidden. "Show spoilers" never bypasses a PIN; locked items still ask for it. The rule editor's term suggestions never come from hidden channels.
- **Privacy:**
  - Only guarded subjects are recorded as seen.
  - No titles at INFO, consistent with the field-fix rule; rules and seen rows are logged by id.
  - AccessKit names don't carry hidden text.
  - Reveals are memory only.
  - **The in-flight transient `systemd-run --user` reminder timers must carry only a reminder id (`tuner --reminder <id>`), never titles.** Unit names and command lines show in `systemctl --user list-timers` and would bypass the guard. Tuner judges at fire time.
- **Connection limits:** the shield avoids opening streams. There are no probes and no network lookups (the guard never fetches fixtures or results).
- **Failure:** fails closed before the first build. A broken rule (bad JSON) is skipped with a WARN, never a crash.

## 6. MCP tools and skill changes (after the MCP PR merges)

- **One choke point:** `agent/server.rs::guard_reply(&mut Reply, &GuardSnapshot)` runs on every read tool (`guide_search`, `whats_on`, `search`, `browse`, `details`, `episodes`, `continue_watching`, `history`, `status`).
  - Titles are neutralised, and guarded `sub_title`, `description`, `plot` and episode `title` are removed.
  - Each item gains `spoiler: {hidden: ["sub_title","description"], rule: "Arsenal", reason: "embargo|series|blackout", until}`.
  - The planned `spoiler_safe=true` keeps its meaning (strip sub-titles and descriptions for everything, ad hoc).
  - New `reveal_spoilers: [refs]` returns the raw fields for those refs only. The skill rule allows it only when the user asks in that turn.
- **New tool `spoiler_guard`:** `action: status|list|add_topic{label, terms, categories?, mode?, window_h?}|add_series{series_ref}|blackout{programme_ref|topic, until}|remove{id}|mark_seen{programme_ref|episode_ref}`.
  - Writes are confirmed by the skill, logged at INFO by id, and capped at 10 per call.
  - Pausing stays in the app.
  - `status` returns `{enabled, paused_until?, active: [{rule, until}]}`.
- **Skills:**
  - `RULES.md` rule 4 becomes: "Tuner's spoiler guard is authoritative and tools apply it. Say 'hidden by your spoiler guard' rather than guessing. Never pass `reveal_spoilers` unless the user asks to see it now. Never web-search an embargoed event."
  - `watch`: "I'll watch it later / don't tell me the score" → offer `blackout`, then `mark_seen` when they say they've watched it.
  - `plan`: following a team (and later a `follow_rule`) asks "Guard spoilers too?" (default yes for sport).
  - `series`: `episodes` is already guarded after next-up.
  - No new skill.

## 7. Tests worth having (real behaviour)

1. **`neutralise_scores`:**
   - Rewrites: "Arsenal 2–1 Chelsea" → "Arsenal v Chelsea"; "(aet)", "4-3 pens", "Eng 245/6 v Aus", and multiple scores ("6-4 6-3").
   - Untouched: "20:00 News", "Premier League 2023-24", "9-1-1: Lone Star", "Catch-22", "S2-E5", "Mission: Impossible 2".
2. **Matcher:** "ARSENAL", "Arsenal's" and the alias "Man Utd" ↔ "Manchester United" match; "Arsenale" and "arsenal of democracy" outside the Sport scope don't.
3. **Embargo:**
   - Event 17:30–19:30, 48 h: nothing is guarded before 17:30.
   - A news sub-title mentioning Arsenal is hidden at 18:00.
   - A repeat on another channel or on TV 2 is lifted by a single seen.
   - Lifted at 19:30 + 48 h.
   - "Always" mode never lifts.
4. **Series:** with S2E3 finished, S2E4 is visible and S2E5 and later are hidden in VOD rows and in EPG S/E airings, with progress merged across TV and TV 2. An EPG airing with no S/E has its sub-title and description hidden. A "don't guard" exception wins.
5. **Tracker** (fake clock and player):
   - The last 10 minutes live → seen.
   - 2 minutes mid-match → not seen.
   - Catch-up to 90 % → seen.
   - An unguarded programme writes no row.
6. **Notifications and MPRIS:** a reminder for "Arsenal 2-1 Chelsea highlights" during an embargo produces a neutralised body, and the toast fallback is the same. The MPRIS title is neutralised.
7. **MCP:** during an embargo, the serialized `guide_search` reply contains neither "2-1" nor the description. `reveal_spoilers` returns the description for that ref only. Explicit items stay absent while locked.
8. **UI (`ui/testing.rs` harness):**
   - A guide cell shows the rewritten title.
   - Ctrl+B reveals the focused cell, and moving focus hides it again.
   - Ctrl+Shift+B → 15 min shows everything and the chip appears; ✕ guards again.
   - A click on a bar reveals without activating the row.
9. **Zap shield:** zapping onto a shielded channel makes zero `play` calls (fake `PlayerHandle`); Enter tunes it; ↓ judges the next channel.
10. **Persistence:** rules survive a source refresh and re-key, and a backup round trip.
11. **Benchmarks:** `benches/guide.rs` with 10 active topics stays under 8 ms; a snapshot build over 302 k synthetic names stays under 100 ms in release.

## 8. Implementation plan (file ownership is disjoint)

| WP | Owns | Size (incl. tests) |
|---|---|---|
| **S1 core** | `src/spoiler/{mod,text,snapshot,verdict}.rs`, `src/model/spoiler.rs`, `src/store/spoiler.rs`, migration `0009`, `config.rs` `[spoiler]`, `msg.rs` scope, DESIGN §3x | ~1 600 |
| **S2 painters** | `ui/widgets/spoiler.rs`, `widgets/rows.rs`, `ui/epg_grid.rs`, `overlays/{programme,search}.rs`, `screens/{guide,live,home,movies,series,series_detail,movie_detail}.rs`, `images.rs` Obscured variant | ~1 200 |
| **S3 control** | `keymap.rs`, `app.rs` (gate wiring, `RevealSet`, chip, menu routing), `widgets/status_bar.rs`, `overlays/{spoiler_menu,spoiler_rule}.rs`, `screens/settings.rs` page | ~1 300 |
| **S4 seen and shield** | `spoiler/tracker.rs`, `screens/player.rs` shield card, `platform/{notify,mpris}.rs`, `jobs/maintenance.rs` prune | ~700 |
| **S5 agent** | `agent/{server,tools,proto}.rs` guard_reply and `spoiler_guard`, plugin `RULES.md`, `watch`/`plan`/`series` SKILL.md | ~500 |

**Order:**

1. S1 (it defines the API, `judge` and `GuardMark`).
2. S2, S3 and S4 in parallel.
3. S5 once the MCP PR is on main.

**Fastest path:** a first PR of S1 plus the series half of S2, plus Ctrl+B, the Settings toggles and the notification/MPRIS hooks. The topic embargo, shield and agent work follow in a second PR.

**Coordination:**
- `screens/player.rs` is also touched by the queue and multi-view features, so S4 lands before them or rebases onto them.
- Follow rules link by `follow_rule_id`; whichever lands second adds the delete clean-up dialog.
- S1 should take the P3 copies API for merged series progress.

## 9. Risks

- **False positives:** "Arsenal" in a documentary. The category scope defaults to Sport for teams and follow rules.
- **False negatives:** EPG text with no team names (Sky Sports News), and results in video or score bugs. The sport-description option and the shield help; documented as best effort.
- **Confusion when text is hidden:** "where's my description?". The reason tooltip and the block copy always name the rule and the end time.
- **EPG mapping errors** (sibling fix): a wrong mapping guards or misses the wrong programmes.
- **Score regex edge cases:** covered by a large table of test cases.
- **The systemd timer leak:** the guardrail in §5 must be applied in the MCP pass 2 PR now.

## 10. Open questions for the user (each with a recommended default)

1. **What should be on by default?** Recommended: the guard is **on**, but it only acts automatically on series you're part-way through and on sport you follow. Nothing else is guarded until you add a topic.
2. **Should the next episode's title and plot be shown?** Recommended: **yes**. Guarding starts after the next-up episode, so you can see what you're about to play.
3. **Should zapping stop before tuning a guarded channel?** Recommended: **on**, but only where the programme text mentions an embargoed topic or a blackout is active. Plain sport news channels are not shielded unless you tick them on the rule.