# Guide fixes: making guide data easy to fix at scale

> The design as approved on 2026-10-11, kept for its user stories, mockups and numbered rules (code comments cite them as "DESIGN §31 §x.y"). The answers to its open questions and what was built differently are in DESIGN.md §31 (Guide fixes).

Everything below was read from `origin/main` (013f897), plus the in-flight branches `feat/pane-arrows`, `feat/mcp` (DESIGN §30 draft in `.claude/worktrees/p21`) and the job plans. No files were modified.

## 0. Decisions in one screen

1. **One screen with three jobs.** The existing EPG mapping screen (`src/ui/screens/epg_mapping.rs`, `ScreenId::EpgMapping`) becomes **Guide fixes**:
   - a per-playlist coverage dashboard;
   - an issue list with a proposed fix on every row;
   - a detail pane with a before/after preview, guide-channel candidates, time shift and catch-up offset.
   
   Ctrl+E, the "Not in your guide — Ctrl+E maps it" copy and Settings ▸ EPG keep pointing here.
2. **Issues are computed, not stored.** Seven kinds:
   - No guide
   - Suggested
   - +1 without shift
   - Copy has guide
   - Guide ended
   - Gone from guide
   - Looks wrong
   
   One background job (`jobs::guide_check`) classifies every shown channel after each EPG or playlist import. It persists only the top 3 fuzzy suggestions per channel, as a derived cache.
3. **Rules persist; fixes reattach.**
   - The two bulk patterns are per-playlist **rules**. "+1/+2 follow their base channel" and "Copies share a guide" are re-derived as automatic mapping steps on every refresh, so channels added later are covered too.
   - One-off fixes are Manual mappings plus a per-channel time-shift override. Both already sit in user-data tables with `fp` + `orphaned_since`, so they reattach through `store::import::reattach_user_data` for free.
4. **Every write is a logged batch that can be undone.** This covers the app (Ctrl+Z, toast Undo, the "Recent changes" view) and Claude (`apply_mapping` returns `undo_id`).
5. **The preview is cheap.** `store::epg::now_next(&[(epg_channel, shift)])` already answers before/after for any (guide channel, shift) pair in under 1 ms each.
6. **Claude gets three tools on the MCP IPC v2 handler:** `guide_issues`, `propose_mapping` and `apply_mapping`. Proposals are bound, expiring and capped. Applying needs the proposal id plus `confirm: true`. The app toasts "Claude fixed the guide for N channels · Undo".

---

## 1. User stories

1. As the user, I open Guide fixes and see per playlist: "TV · 8 412 of 10 233 shown channels have guide data · 1 821 to fix · 640 one-key fixes". The numbers match Settings ▸ EPG, Settings ▸ Playlists and the guide header.
2. "Channel 4 +1 shows the 8 pm programme at 8 pm." The row is flagged **+1 without shift**. Shift+Enter applies +1 h. The preview showed Now changing from "Gogglebox (20:00)" to "Hughie Green (20:00, was 19:00)" before I pressed it.
3. "All my +1s are wrong in TV 2." I turn on the rule **+1/+2 follow their base** for TV 2. The preview lists the 52 channels it affects. After next week's refresh, a newly added "Dave +1" is already right.
4. "BBC One has guide data in TV but not in TV 2." The issue is **Copy has guide**. One rule ("Copies share a guide") fixes 410 channels across both playlists.
5. Bulk accept: I filter "Suggested ≥ 95 %", press Ctrl+A, then Shift+Enter, and a confirm card shows the 140 changes. Ctrl+Z undoes the whole batch.
6. A guide renames `bbcfour.uk` to `BBCFour.uk`. My manual mapping now points at nothing. The issue is **Gone from guide**, with a one-key fix that finds the renamed entry by the stored guide-channel name.
7. Catch-up on a channel starts 2 minutes late. In the detail pane I press **Check ▶**, which plays the last finished programme from its start. I nudge the offset to +2 min and press Check again. Then I choose "Use for all TV channels".
8. A provider reuses tvg-ids, so "BBC Two" shows Dave's guide. The issue is **Looks wrong**. I either pick the right one or press "It's right", which dismisses it permanently (the dismissal reattaches like other user data).
9. With Claude: "Why does Sky Mix have no guide?" Claude explains (not in any guide; TV's guide lists "Sky Mix HD" at 96 %), shows before/after now/next, asks, applies, and tells me "Undo: say 'undo' or press Ctrl+Z in Guide fixes".
10. With Claude: "Fix my guide." Claude summarises the dashboard and proposes the two rules plus the top-confidence batch, applies only after I confirm, and never touches locked channels while the session is locked.
11. Kids are using the app while it is locked. Explicit channels are absent from issues, counts, previews and MCP results. "1 821 to fix" is computed after they are removed.
12. Reminders follow shifts. If I shift a channel by +1 h, its pending reminders move by +1 h in the same transaction, and the toast says so.

---

## 2. UX

### 2.1 Layout model

- **Header:** `‹ Settings  Guide fixes` · view button `Suggested ▾` (Ctrl+S) · `⟲ Undo` (Ctrl+Z, greyed out when the log is empty).
- **Dashboard strip:**
  - One card per eligible playlist (scope rules of §29), with a coverage bar, "N of M shown channels", "X to fix · Y one-key".
  - A Rules card: the per-playlist toggles for the playlist in scope, or "2 playlists" in All.
  - Issue **chips** with counts. A click filters. Keyboard: Ctrl+S menu, and digits inside that menu as today.
- **Left pane, Issues:** a virtual list of channels for the chosen issue kind and playlist scope.
  - Each row has a mark box, a glyph and the name, then `→ proposed fix`, confidence or ✓, and a playlist badge.
  - Hidden and category-filtered channels are excluded unless "Include hidden channels" is on in the Ctrl+S menu.
  - The view "Recent changes" lists logged batches and channels instead of issues.
- **Right pane, Detail** of the highlighted channel, with three stacked sections:
  1. **Fix:** current state, the proposal and "why", then the Before/After now and next table, then [Accept ⇧Enter] [Reset Del].
  2. **Other guide channels:** the existing suggestions plus a searchable list of every guide channel.
  3. **Adjust:** a Panel-idiom row each for Time shift and Catch-up offset, with Check ▶ and "Use for all ‹playlist› channels…".
- **Footer of the left pane:** "N marked · ⇧Enter accepts · Ctrl+A all".

Playlist scope uses the global keys (Alt+P picker, Ctrl+PgDn / Ctrl+PgUp), extended to this screen, and it follows the filter-for-all-playlists fix.

### 2.2 Wide, 1600 pt (expanded sidebar 184)

```
┌─────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────┐
│ ◉ Tuner     │ ‹ Settings   Guide fixes                                           Suggested ▾     ⟲ Undo  Ctrl+Z │
│             │ ┌ TV ───────────────────────────────┐ ┌ TV 2 ─────────────────────────────┐ ┌ Rules · TV ─────────┐ │
│ ⌂ Home      │ │ ██████████████████████░░░░░  82 % │ │ ████████████████░░░░░░░░░░░  61 % │ │ ✓ +1/+2 follow base │ │
│ ▶ Live TV   │ │ 8 412 of 10 233 shown have guide  │ │ 3 870 of 6 344 shown have guide   │ │ ✓ Copies share guide│ │
│ ▦ Guide     │ │ 1 821 to fix · 640 one-key        │ │ 2 474 to fix · 1 310 one-key      │ │ Catch-up offset 0   │ │
│ ▣ Movies    │ └───────────────────────────────────┘ └───────────────────────────────────┘ └─────────────────────┘ │
│ ▤ Series    │  [No guide 4 295] [Suggested 1 950] [+1 no shift 37] [Copy has guide 410] [Ended 12] [Gone 3] [Odd 9]│
│             │ ──────────────────────────────────────────────────┬──────────────────────────────────────────────── │
│             │ ⌕ Suggested · All playlists · 1 950  (type to filter)│ E4 +1                      TV · UK Entertainment│
│             │  ☐ ⚠ BBC Four HD      → BBC Four          97 %  TV  │ Fix   E4 (TV guide) · time shift +1 h          │
│             │  ☑ ⚠ Channel 4 +1     → Channel 4 · +1 h    ✓   TV  │       "+1" of E4, which has guide data         │
│             │ ▸☑ ⚠ E4 +1            → E4 · +1 h           ✓   TV  │                 now                next        │
│             │  ☐ ⚠ Dave             → Dave              94 %  TV2 │ Before  — no guide data —                      │
│             │  ☐ ⚠ Sky Mix HD       → Sky Mix           93 %  TV2 │ After   20:30 Hollyoaks       21:00 Gogglebox  │
│             │  ☐ ⚠ Quest Red +1     → 2 bases, pick one       TV2 │         ▓▓▓▓▓▓▓░░░░ 12 min left                │
│             │  ☐ ⚠ 5Action          → 5 ACTION          92 %  TV2 │ [ Accept  ⇧Enter ]   [ Reset  Del ]            │
│             │  …                                               │ ─────────────────────────────────────────────── │
│             │                                                  │ Other guide channels  (Tab, type to search)     │
│             │                                                  │  ◆ E4 +1        e4plus1.uk       88 %  TV guide │
│             │                                                  │  ▦ E4           e4.uk                  TV guide │
│             │                                                  │ ─────────────────────────────────────────────── │
│             │                                                  │ Time shift      [−1 h] [−15]   +1 h  [+15] [+1 h]│
│             │                                                  │ Catch-up offset [−1]   0 min  [+1]    Check ▶   │
│             │ ──────────────────────────────────────────────── │ Use for all TV channels…                        │
│             │  2 marked · ⇧Enter accepts both · Ctrl+A all     │                                                 │
├─────────────┴──────────────────────────────────────────────────┴─────────────────────────────────────────────────┤
│ ▶ BBC One · The One Show        Enter pick · ⇧Enter accept · Space mark · Ctrl+Z undo · Ctrl+S view · ? help      │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

The left pane takes 48 % of the content width, as today.

### 2.3 Medium, about 950 pt (half screen, icon rail 48)

```
┌──┬──────────────────────────────────────────────────────────────────────────┐
│◉ │ ‹ Settings  Guide fixes                         Suggested ▾   ⟲ Ctrl+Z   │
│⌂ │ TV   ███████████████░░░ 82 % · 1 821 to fix · 640 one-key                │
│▶ │ TV 2 ███████████░░░░░░░ 61 % · 2 474 to fix         Rules: +1 ✓ Copies ✓ │
│▦ │ [Suggested 1 950] [+1 no shift 37] [Copy 410] [Ended 12] [More ▾]        │
│▣ │ ───────────────────────────────────┬──────────────────────────────────── │
│▤ │ ⌕ Suggested · 1 950                │ E4 +1 · TV                          │
│  │  ☐ BBC Four HD → BBC Four 97 % TV  │ Fix  E4 · shift +1 h                │
│  │  ☑ Channel 4 +1 → Ch 4 +1 h ✓ TV   │         now            next         │
│  │ ▸☑ E4 +1 → E4 +1 h          ✓ TV   │ Before  — no guide data —           │
│  │  ☐ Dave → Dave           94 % TV2  │ After   20:30 Hollyo… 21:00 Goggl…  │
│  │  ☐ Sky Mix HD → Sky Mix  93 % TV2  │ [Accept ⇧Enter]  [Reset Del]        │
│  │  …                                 │ Other guide channels (Tab)          │
│  │                                    │  ◆ E4 +1  e4plus1.uk  88 %          │
│  │                                    │ Shift  [−1h][−15]  +1 h [+15][+1h]  │
│  │ 2 marked · ⇧Enter accept           │ Catch-up [−1] 0 min [+1]  Check ▶   │
└──┴──────────────────────────────────────────────────────────────────────────┘
```

At Medium, chips past the fourth fold into `More ▾`, which opens the same menu as Ctrl+S. The proposal column truncates the target name before the playlist badge.

**Compact (under 700, rem-scaled):** one pane at a time, as today. The issues list and the detail pane each show `‹ Issues`. The dashboard collapses to one line, "TV 82 % · TV 2 61 % · 4 295 to fix ▾", which opens the dashboard as a sheet.

### 2.4 Keyboard

The free keys were checked against `src/keymap.rs` on main and `feat/pane-arrows`. Ctrl+A, Ctrl+Z and Ctrl+Shift+←/→ are unbound in List, ListFiltering and Global. Shift+Enter in `EPG_MAPPING` shadows List's "Play fullscreen", which this screen never used.

Context stacks:

| Focus | Contexts |
|---|---|
| Issues pane | `[EpgMapping, List \| ListFiltering]` |
| Other guide channels | `[EpgMapping, List \| ListFiltering]` |
| Adjust rows | `[EpgMapping, Panel]` |

New rows in the `EPG_MAPPING` table (consulted first):

| Keys | Action (new unless noted) | Help text |
|---|---|---|
| Enter | `Activate` (exists) | Issues: show its guide channels · Guide channels: assign · Adjust: activate (Check ▶, "Use for all…") |
| Shift+Enter | `AcceptFix` | Accept the proposed fix (all marked channels when any are marked) |
| Space / Insert | `ToggleMark` (exists in List; in ListFiltering only Insert) | Mark channel |
| Ctrl+A | `MarkAll` (exists, Checklist only today) | Mark all shown / none |
| Ctrl+Shift+→ / Ctrl+Shift+← | `ShiftLater` / `ShiftEarlier` | Time shift +1 h / −1 h (highlighted or marked) |
| Ctrl+Z | `UndoGuideFix(None)` | Undo the last guide fix |
| Delete | `ClearEpg` (exists, wider meaning) | Reset to automatic (guide channel and time shift) |
| Ctrl+S | `SortMenu` (exists) | View: issue kind, playlist, include hidden, rules, recent changes |
| Ctrl+G | `OpenInGuide` (exists via ITEM_ACTIONS) | Show the channel in the TV guide (to check) |
| Shift+F10 / Ctrl+Enter | `ContextMenu` (exists) | Channel menu: Accept, Pick guide channel…, Time shift ▸, Catch-up offset…, Apply to its +1/+2, Watch to check, Show in guide, It's right (dismiss), Reset |

Inherited behaviour:

- ↑/↓, PgUp/PgDn, Home/End and type-to-filter work in both lists.
- Tab and Shift+Tab cycle Issues → Guide channels → Adjust.
- With pane-arrows, → or Enter on an issue goes right, and ← goes back left (also while filtering, filter kept).
- In Adjust (Panel), → and Shift+→ / L increase; Shift+← / H decrease; ← steps back to the Issues pane, the pane-arrows Settings idiom.
- The steps are ±15 min for time shift and ±1 min for catch-up. Ctrl+Shift+←/→ gives whole hours on the lists.
- **Writes are debounced.** Shift and offset changes update the preview at once. They are written 400 ms after the last press, or when the cursor leaves the row. Repeated presses coalesce into one undo entry.

**Esc ladder** (§15.1 order):
1. Clear the filter.
2. Leave Adjust or Guide channels for Issues.
3. Clear the marks (toast "2 marks cleared").
4. Go back to Settings (or whatever is below in the route stack).

Auto-repeat never steps out, as today.

### 2.5 Mouse

- A dashboard card click scopes the list to that playlist.
- A chip click picks the issue kind; the lit chip shows the active filter.
- Rules card toggles open the rule preview card (below).
- A row click moves the cursor (pointerGate). A click on the ☐ box marks. Ctrl+click toggles a mark; Shift+click marks a range.
- Double-clicking a row accepts its fix. The second click is dropped if the row changed under the pointer (`widgets::second_click_after`).
- Right-click opens the channel menu.
- The stepper buttons [−1 h] [−15] [+15] [+1 h] and [−1] [+1] behave like the keys.
- Footer button: `[Accept N]`. Toasts carry `[Undo]`.
- A ✕ back button follows Compact rules.

### 2.6 Confirm and rule preview cards (Modal)

```
┌ Accept 140 fixes? ──────────────────────────────────────────┐
│ 132 guide channels · 8 time shifts · 0 locked               │
│                         now before          now after       │
│ BBC Four HD             — no guide —        Timewatch       │
│ Channel 4 +1            Gogglebox 20:00     Hughie… 20:00   │
│ … 138 more (scroll)                                         │
│ Undo: Ctrl+Z in Guide fixes                                 │
│                              [ Cancel ]  [ Accept 140 ]     │
└─────────────────────────────────────────────────────────────┘
```

- Accepting one channel never asks. More than 1 asks once.
- Rule toggles show "Turn on '+1/+2 follow their base' for TV 2? Affects 52 channels (now/next before → after…)".
- In this card, Enter confirms and Esc cancels (Modal).

### 2.7 Entry points elsewhere (small edits)

- The TV guide channel menu (Shift+F10 on a channel) gains "Time shift…" and "Fix guide…" (= Ctrl+E). Live preview and the guide header keep "Ctrl+E maps it".
- Ctrl+I "Edit information" (`ui/overlays/edit_item.rs`) gains a **Time shift** row (±15 min, "+1 h") next to the existing catch-up offset.
- Settings ▸ EPG: rename "EPG mapping…" to "Fix guide data… (1 821 to fix)". Coverage text comes from the shared function (§4.5).

---

## 3. Data model and migration

The migration is the next free number at merge time: `feat/p3-copies` holds 0007, and the MCP branch adds none. File: `src/store/migrations/00NN_guide_fixes.sql`. Columns are added by a Rust step using `add_column_if_missing`, so it is safe to re-run like 0003–0006.

```sql
-- Guide fixes (DESIGN §31). Columns added by migrations::add_guide_fix_columns (idempotent):
--   item_override.shift_min        INTEGER  -- user's time shift for a channel (min); NULL = automatic
--   item_override.guide_ok         INTEGER NOT NULL DEFAULT 0  -- dismissed issue kinds (bit per IssueKind)
--   channel_epg_map.shift_min      INTEGER  -- shift implied by an automatic rule row ('sibling'); NULL otherwise
--   channel_epg_map.epg_name       TEXT     -- guide channel display name when chosen (re-find when it's gone)
--   channel_epg_map.actor          TEXT     -- manual rows: 'you' | 'claude'
--   channel_epg.shift_min          INTEGER  -- resolved: the winning row's shift_min
-- New methods in channel_epg_map.method: 'sibling' (rule: +N of a base), 'copy' (rule: same channel elsewhere).

CREATE TABLE IF NOT EXISTS guide_fix_log (
  id        INTEGER PRIMARY KEY,
  at        INTEGER NOT NULL,
  actor     TEXT    NOT NULL CHECK (actor IN ('you','claude')),
  label     TEXT    NOT NULL,          -- "Accepted 140 suggestions", "Time shift +1 h" (no titles at INFO)
  n         INTEGER NOT NULL,
  undone_at INTEGER
);
CREATE TABLE IF NOT EXISTS guide_fix_change (
  log_id      INTEGER NOT NULL REFERENCES guide_fix_log(id) ON DELETE CASCADE,
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key TEXT    NOT NULL,
  field       TEXT    NOT NULL CHECK (field IN ('map','shift','catchup','dismiss','rule')),
  before      TEXT,                     -- JSON (map: {epg_source,xmltv_id,epg_name}|null; shift: n|null …)
  after       TEXT,
  PRIMARY KEY (log_id, source_id, channel_key, field)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS guide_fix_change_ch ON guide_fix_change(source_id, channel_key);

-- Derived cache (not user data, no fp): top-3 fuzzy suggestions per unmapped shown channel.
CREATE TABLE IF NOT EXISTS epg_suggestion (
  source_id     INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key   TEXT    NOT NULL,
  rank          INTEGER NOT NULL,       -- 0..2
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  display_name  TEXT    NOT NULL,
  score         REAL    NOT NULL,
  PRIMARY KEY (source_id, channel_key, rank)
) WITHOUT ROWID;
-- meta 'guide_check:stamp' = blake3(active_gens of enabled guides, catalogue generations,
-- fuzzy_threshold, rules) — mismatch ⇒ cache is stale (UI shows "Checking…").
```

**Per-playlist rules and catch-up default.** No migration is needed. Add `SourceSettings.guide: GuideRules` (serde default) in `src/model/source.rs`:

```rust
pub struct GuideRules { pub timeshift_siblings: Option<bool>, pub share_copies: Option<bool>, pub catchup_offset_s: Option<i64> }
```

`None` means the config default (`config.epg.rules_default`, see Q1). The rules are written by a dedicated `store::sources::set_guide_rules`, like `set_category_filter`.

**Effective values:**

- **Shift** = `item_override.shift_min` ?? `channel_epg.shift_min` ?? `channel.tvg_shift_min`.
  - It is a single `COALESCE` in `store::library::load_live` and in the full `Channel` load. Every reader that applies `ProgrammeSlim::shifted` then gets it with no other change.
- **Catch-up offset** = `item_override.catchup_offset_s` ?? `source.settings.guide.catchup_offset_s` ?? 0.
  - It changes in one place: `sources/resolve.rs:116`.
- **Resolution** (`store::epg::resolve_select`): `ORDER BY m.method='manual' DESC, m.method='sibling' DESC, es.priority, es.id, METHOD_RANK, confidence`.
  - `METHOD_RANK` gains `'sibling'` and `'copy'`, which rank last.
  - The winning row's `shift_min` is copied into `channel_epg`.

**Reattach:**

- Manual map rows already reattach (`HOLDERS` "epg").
- `shift_min` and `guide_ok` live in `item_override` (holder "override"), so they reattach too.
- `move_user_data` needs no change.
- Rule rows are automatic and re-derived.
- Log rows are keyed by channel key. Undo skips keys that no longer exist and says so. They are not user data, so they never orphan.

**Pruning:** `Maintenance` drops logs older than 90 days or beyond 200 batches, and rebuilds `epg_suggestion` on a stamp mismatch.

**Model additions** (`src/model/epg.rs`):

```rust
pub enum EpgMatchMethod { …, Sibling, Copy }                 // kebab: "sibling", "copy"
pub enum IssueKind { NoGuide, Suggested, PlusNoShift, CopyHasGuide, GuideEnded, GoneFromGuide, LooksWrong }
pub struct GuideTarget { pub epg_source: EpgSourceId, pub xmltv_id: String, pub name: String }
pub struct FixChange { pub channel: ItemRef, pub map: Option<Option<GuideTarget>>, /* Some(None)=reset */ 
                       pub shift_min: Option<Option<i32>>, pub catchup_offset_s: Option<Option<i64>>, pub dismiss: Option<IssueKind> }
pub struct Proposal { pub changes: Vec<FixChange>, pub rules: Vec<(SourceId, GuideRules)>, pub stamp: [u8; 16] }
```

---

## 4. Engine and architecture

### 4.1 Modules

| Module | Role | Kind |
|---|---|---|
| `src/sources/timeshift.rs` (new) | `marker(name) -> Option<(base_norm, minutes)>` over `name_tokens`: `+1`, `+ 1`, `+1h`, `+2`, `+24`, `plus 1`, quality words dropped; not `Sky Sports+`, `ITV4+`, `5USA` | pure |
| `src/sources/epg_match.rs` | `derive_siblings(rows, base_maps) -> Vec<EpgMapping>`: method Sibling, `shift = N·60`, only when the +N channel is unresolved or resolves to the base's guide channel, and that guide channel's display names have no +N marker. Ambiguous bases (several, different guide channels) produce no row, only an issue | pure |
| `src/guide_fix/catalog.rs` (new) | Moves `Catalog` / `load_catalog` out of the screen. One shared, lazily built `Arc<Catalog>` behind `Services::guide_catalog` (keyed by the guides' `active_gen`s, dropped after 2 min unused) | used by screen, job and agent |
| `src/guide_fix/issues.rs` (new) | `classify(lib, cache, probes, opts) -> IssueReport { coverage: Vec<SourceCoverage>, rows: Vec<IssueRow> }` | pure over the snapshot; under 100 ms for 79k channels |
| `src/guide_fix/preview.rs` (new) | `preview(conn, changes, now) -> Vec<PreviewRow{before: NowNext, after: NowNext, warnings}>` | batched `now_next` |
| `src/store/guide_fix.rs` (new) | `apply(conn, actor, label, &Proposal) -> LogId`, `undo(conn, LogId) -> UndoReport`, `recent(conn)`, suggestion cache read/write, rule-row derivation for copies | store |
| `src/jobs/guide_check.rs` (new) | The background job (§4.2) | job |

### 4.2 Threads and flows

```
EPG import / playlist import / rule toggle / threshold change
   └─(debounced 30 s, follow-up via JobContext::submit)─▶ Job GuideCheck  [tokio + blocking pool]
        1. writer: derive rule rows    (siblings via epg_match per guide during rematch;
                                        copies: after resolve, Rust pass over channel/family
                                        → 'copy' rows; then resolve_mappings again)     ≤ 300 ms
        2. reader: probes              (per DISTINCT mapped guide channel: any programme
                                        with stop > now? via programme_grid; ~10k seeks) ≈ 100 ms
        3. blocking, 2 threads:        suggestions for unmapped SHOWN channels lacking a
                                        cached row (prefilter + Jaro-Winkler, chunks of 256,
                                        cancellable, paused while JobContext::player_busy) ≈ 1–3 s
        4. writer: epg_suggestion rows (2 000 per tx), meta stamp
        5. bus: DataScope::GuideIssues (NEW, light: no library rebuild)

UI thread (screen):  IssueReport = classify(lib snapshot, cache) on the blocking pool when
                     lib.generation or GuideIssues changes; rows render from the report.
                     Preview for the highlighted row (≤ 2 now_next calls) and the confirm card
                     (≤ 200 pairs, < 20 ms) on a Loader. The existing lazy 40-row suggestion
                     batches stay as the fast path while the job hasn't covered a row yet.
Writer:              apply/undo in ONE tx per 1 000 changes (one log id): manual rows
                     (set_manual_mapping with epg_name/actor), item_override shift/catch-up/guide_ok,
                     re-time pending reminders of shifted channels (start/stop += Δ), then
                     resolve_channel per channel (or resolve_mappings when > 500) → DataScope::Epg
                     (existing: cache flush + library rebuild < 0.4 s).
Agent handler:       same guide_fix:: functions on its reader pool; writes via the store writer
                     handle; never on the UI thread (§30.1.4).
```

### 4.3 Classification rules (`issues.rs`)

The universe is the library snapshot's channels. Hidden and category-filtered channels are excluded unless `include_hidden` is on. Explicit channels are already absent while locked.

| Kind | Condition | Proposal |
|---|---|---|
| PlusNoShift | `timeshift::marker(name)` matches and the effective shift is 0 and the resolved guide channel has no +N marker (it is the base's) | shift = N·60 (✓ high confidence) |
| CopyHasGuide | unmapped; another shown channel with the same `family` (or same norm across playlists, reusing P3's "Also on" key) resolves | that guide channel |
| Suggested | unmapped, cached suggestion ≥ threshold | top suggestion (score %) |
| NoGuide | unmapped, nothing above | none (search) |
| GuideEnded | resolved, but probe says no programme now or later | another guide that lists it (fuzzy), else "refresh guide" |
| GoneFromGuide | manual row whose `xmltv_id` is absent from the active generation | caseless/feedless id match, else fuzzy on `epg_name` |
| LooksWrong | automatic tvg-id row and **zero** shared trigrams between the channel norm and every display-name norm of the guide channel | none; "It's right" dismisses |

`guide_ok` bits suppress a kind for that channel. "One-key" means the proposal is ✓ or ≥ 95 %.

### 4.4 Performance at the user's scale

The scale is 79k channels, about 30k guide channels, and about 6k shown unmapped channels with the UK + English filter.

- **Classification** is O(channels) with hash lookups: under 100 ms off the UI thread.
- **Suggestions:** about 0.3–0.5 ms per channel per guide (≤ 512 Jaro-Winkler calls plus posting walks). For about 6k shown channels × 2 guides on 2 threads, that is roughly 2–3 s.
  - "Include hidden" for about 40k channels takes about 10–15 s in the background, with progress shown in the Jobs overlay.
  - The job is capped at 2 threads because each 4K HEVC stream costs about 0.86 of an M2 core in software decoding.
  - It pauses while the player is Loading or Zapping.
- **Memory:** the Catalog is about 15–25 MB (60k norms, trigram postings). It is shared, built on demand and dropped when idle. The `epg_suggestion` cache stores at most 3 rows per channel. The screen never holds a second copy.
- **Writes:** about 1k manual rows plus overrides per transaction is roughly 20 ms. `resolve_mappings` takes under 200 ms (existing budget). Library rebuild is existing.
- **Bench:** extend `benches/guide.rs` with `classify_79k` and `suggest_6k_two_threads` against `scripts/gen-big-fixtures.py` data.

### 4.5 One coverage function

`guide_fix::coverage(lib) -> Vec<SourceCoverage{source, shown, with_guide, hidden_with_guide}>` replaces:

- `GuideStatus::coverage` (`ui/screens/live.rs:273`);
- Settings `guide_counts` / `epg_matches` (`settings.rs:2777–2789`);
- the onboarding Done summary.

All of them then say "shown channels". The MCP `sources` tool's `guide.coverage` uses it too.

### 4.6 Catch-up and shift semantics (a decision the tests must lock)

`catchup::effective_start` subtracts the provider's `tvg_shift_min` (the Kodi convention).

A **user** shift or a **sibling-rule** shift means "this stream airs N minutes after the guide". The archive of that stream is therefore at the **displayed** time. So `CatchupRequest` gets `tvg_shift_min = channel.tvg_shift_min` (provider only), not the effective shift. The **Check ▶** flow is how the user verifies this.

---

## 5. Guardrails

**Parental and explicit content:**
- The universe is the snapshot, so explicit channels are absent from rows, counts, chips, coverage, the confirm card and suggestions while hidden.
- `epg_suggestion` rows for explicit channels are written but never read while hidden: reads join the snapshot.
- The agent path re-checks `ParentalView::hides_explicit` per request, like §30.6. Concealed refs return `not_found`.

**Locked channels:**
- In the app, they are shown dimmed and are editable: guide edits can't open content.
- Check ▶ and "Watch to check" go through the normal play path, so they ask for the PIN.
- Over MCP while the session is locked: `locked: true`, no now/next titles in previews, and changes to locked channels are rejected (`locked: change it in Tuner`).

**Privacy:**
- Results carry no URLs, logos or provider ids.
- Guide refs are `guide:<epg_source_id>:<xmltv_id>`.
- A guide name containing "://" is shown as "Guide 2".
- Log lines are at INFO: `guide fix: 140 changes (you) #34`, with no channel or programme titles (the field-fixes rule).

**Connection limits:**
- The engine never opens streams.
- Check ▶ and "Watch to check" use the player and budget (§11.5). With `max_connections=1`, if something is playing from the same playlist, the card says "This stops ‹channel› (TV allows one stream)".
- Claude's guide tools never trigger playback.
- There is no bulk probing.

**Provider load:** the guide check is local. "Refresh guide" in a GuideEnded proposal is the existing rate-limited refresh.

**Blast radius:**
- Every write is logged. Undo checks that the current value still equals `after` and skips anything changed since, with a report.
- Rules are per playlist and can be turned off. Turning one off deletes its rows on the next derivation and is itself logged.
- Pending reminders move with shift changes in the same transaction.

---

## 6. MCP tools and skill changes

The tools live on the app track of §30, in a new `src/agent/guide.rs`. Their params and results are added to `agent/proto.rs` and `agent/tools.rs` (owners must agree, per §30.10). All three are rate-limited as queries or actions (existing).

| Tool | Params | Result | Rules |
|---|---|---|---|
| `guide_issues` (read) | `playlist?`, `kinds?[]`, `ref?` (one channel: adds `candidates[]` top 5 + search), `query?` (guide channel search for `ref`), `include_hidden?` (false), `limit?` (50, ≤ 200), `cursor?` | `{coverage:[{playlist, shown, with_guide, to_fix, one_key, rules:{timeshift_siblings, share_copies, catchup_offset_min}}], issues:[{ref, channel, playlist, kind, locked, current:{guide?, shift_min, catchup_offset_min}, proposal?:{guide?, shift_min?, confidence, why}}], candidates?, scan:{state: fresh\|stale\|running, at}, truncated}` | Counts computed after concealment. `why` is a fixed phrase ("+1 of ‹base›", "same channel in TV") and never a URL |
| `propose_mapping` (read) | `changes:[{ref, guide?: "guide:…", shift_min?, catchup_offset_min?, reset?, dismiss?}]` and/or `accept:{kinds[], min_confidence?, playlist?}` and/or `rules:[{playlist, timeshift_siblings?, share_copies?, catchup_offset_min?}]` | `{proposal_id, expires_at (+10 min), summary:{channels, maps, shifts, offsets, rules, rule_affected}, changes:[{ref, channel, before:{guide, shift_min, now, next}, after:{…}, warnings[]}], rejected:[{ref, reason}]}` | ≤ 200 channel changes. Locked or not-found refs go to `rejected`. At most 5 live proposals. The proposal is bound to the guide-check stamp |
| `apply_mapping` (action) | `proposal_id` + `confirm: true`, **or** `undo_id` | `{applied, skipped:[{ref, reason}], undo_id, reminders_moved}` | Stamp changed means `stale` (re-propose). Batches over 50 open the confirm card in Tuner and return `pending_review` (see Q2). The app toasts "Claude fixed the guide for N channels · Undo". `actor = 'claude'` |

**`fix` skill** (`plugins/omatuner/skills/fix/SKILL.md`):
- Description triggers to add: "no guide", "guide is an hour off", "+1 channel shows the wrong programme", "wrong programme info", "catch-up starts late/early", "fix my guide".
- New **Guide** branch:
  1. `guide_issues{ref}` (single channel) or `guide_issues{}` (overview).
  2. Explain the cause in one line using `kind` and `why`.
  3. `propose_mapping`, then show the summary and up to 5 before/after now-next rows, then ask.
  4. On yes, `apply_mapping{proposal_id, confirm:true}`, then report `applied` and `skipped`, and name the undo ("say 'undo', or Ctrl+Z in Guide fixes").
  5. For "fix my guide", propose the rules first (they persist and have the biggest effect), then the `accept{min_confidence:0.95}` batch.
  6. Catch-up offsets: suggest Tuner's Check ▶. Claude doesn't guess offsets, and only proposes one when the user says "starts N minutes late".

**RULES.md:**
- **Rule 9:** "Guide changes always go propose → show before/after → explicit yes → apply → state the undo. Never more than 200 per batch; never on locked channels; rules (which change future refreshes) are named as such."
- Add "guide fixes" to rule 3's confirmation list.
- Remove "mapping fixes come later" from skill-plan §6.

---

## 7. Tests worth having (real behaviour)

1. `sources::timeshift::marker` table: "Channel 4 +1", "E4+1 HD", "Dave ja vu" (none), "Sky Sports+" (none), "ITV4+" (none), "BBC One +24", "5USA" (none), "Quest Red + 1".
2. The matcher on a seeded store:
   - A +1 channel whose tvg-id is the base id, with the rule on, resolves to the base guide with shift 60 and shows Now at the base's time + 1 h through the library and `now_next`.
   - A guide with its own "+1" entry gives shift 0.
   - Two bases mapped to different guide channels produce no row and a PlusNoShift issue with no proposal.
3. Effective-shift precedence: override > rule row > tvg-shift, visible in `ChannelSummary.tvg_shift_min`, guide grid cells and `now_next`.
4. Persistence:
   - A manual mapping, shift override and dismissal survive an Xtream renumber refresh through reattach.
   - A refresh that adds "Dave +1" gets the sibling mapping with no user action.
   - Turning the rule off removes its rows after the next derivation.
5. GoneFromGuide: re-import a guide with a renamed id. The manual row is flagged and the proposal finds it by `epg_name`.
6. Apply and undo:
   - A batch writes manual rows, overrides and log rows.
   - Undo restores the exact previous rows, with automatic rows intact.
   - Undo skips a channel changed since, and skips a re-keyed channel.
   - A shift change moves pending reminders and undo moves them back.
7. Preview: before and after match `now_next` for (old, shift) and (new, shift) pairs, including a +1 case.
8. Coverage parity: the dashboard, Settings ▸ EPG, Playlists and `GuideStatus` give the same numbers on a seeded library with hidden and filtered groups.
9. Parental:
   - While locked, an explicit group's channels are absent from the report, counts, chips and every MCP tool, and present once unlocked.
   - MCP changes to a locked channel are rejected and its preview withholds titles.
   - No "://" appears in any guide tool output on a library whose every row carries URLs.
10. MCP:
    - propose → apply round trip.
    - Apply without `confirm` is `bad_request`.
    - After an EPG import, the proposal is `stale`.
    - More than 200 changes is rejected.
    - `undo_id` reverts.
    - A batch over 50 returns `pending_review`, and the app's confirm card applies it on Enter.
11. Screen harness (`EpgMappingScreen` with the rig):
    - Space marks, Ctrl+A marks all shown, Shift+Enter opens the confirm card for more than 1 and applies, Ctrl+Z undoes.
    - Ctrl+Shift+→ updates the preview at once and writes once after the debounce.
    - The Esc ladder goes filter → pane → marks → back.
    - Chip clicks filter.
    - The key contexts are `[EpgMapping, Panel]` in Adjust.
12. Catch-up: the playlist default is used when the channel has none, a channel override wins, and the user shift is not subtracted (§4.6) while provider tvg-shift still is.
13. Job: cancelling mid-suggestions leaves a consistent cache; it pauses while `player_busy`; it covers 79k channels in benches (not CI).

---

## 8. Implementation plan

Start from a skeleton branch, `guide/base`, of about 600 lines, owned by the integrator. It contains:
- the migration and Rust step;
- model types (`IssueKind`, `FixChange`, `Proposal`, `GuideRules`, the new `EpgMatchMethod`s);
- `DataScope::GuideIssues`;
- the new `Action`s and `EPG_MAPPING` rows;
- function signatures with `todo!()`;
- `Services::guide_catalog`;
- the `COALESCE` lines in `library.rs`.

Then three parallel tracks with disjoint file ownership:

| Track | Owns | Size (code + tests) |
|---|---|---|
| **core** | `src/sources/timeshift.rs`, `src/sources/epg_match.rs`, `src/store/epg.rs` (resolve, columns, manual row extras), `src/store/guide_fix.rs`, `src/guide_fix/{mod,catalog,issues,preview}.rs`, `src/jobs/guide_check.rs` + `jobs/mod.rs` wiring, `src/jobs/import_epg.rs` (follow-up submit, rule derivation), `src/sources/resolve.rs` (catch-up default, §4.6), `src/store/user.rs` (reminder re-time helper), `benches/guide.rs`, `tests/store_guide_fix.rs` | ~1,900 + ~1,000 |
| **ui** | `src/ui/screens/epg_mapping.rs` (rewrite; ~1,500), `src/keymap.rs` rows + hints, `src/ui/overlays/edit_item.rs` (time shift row), `src/ui/screens/{settings,live,onboarding,guide}.rs` (shared coverage function, menu entries), DESIGN §9.4, §13.11, §15 (regenerate tables), new §31, MANUAL_CHECKLIST §5 | ~1,800 + ~500 |
| **agent** (after the MCP PR merges) | `src/agent/guide.rs`, the `proto.rs`/`tools.rs` additions (coordinated with the bridge owner), `tests/agent_app.rs` cases, `plugins/omatuner/skills/fix/SKILL.md`, `RULES.md` | ~700 + ~400 |

Merge order:
1. pane-arrows, P3 copies, then MCP (in flight).
2. `guide/base`.
3. core, then ui.
4. agent.
5. Full gate (clippy, tests, benches smoke) and manual checks on the real "TV" / "TV 2" profile:
   - +1 channels;
   - copies across playlists;
   - Check ▶ with `max_connections=1`.

**Total:** about 4,400 lines of code plus about 1,900 of tests. That is about 2–3 agent-days in parallel after the skeleton.

---

## 9. Risks

- **Wrong automatic matches from rules.** Examples: regional variants, "Sky Sports+", one family hash shared by different feeds. Mitigations: strict marker grammar, only firing when the base resolves to exactly one guide channel, an "Auto-fixed by rules" view under Ctrl+S, per-playlist off switches, and dismissals.
- **CPU contention on the M2.** Mitigations: 2 threads, pause while playing or zapping, chunked work, and a cache so the job seldom recomputes.
- **Migration number collision with in-flight PRs.** The skeleton picks the number at merge, and the Rust step is idempotent.
- **Catch-up semantics for shifted channels (§4.6).** Provider archives differ. The Check ▶ flow and the per-playlist offset are the escape hatch.
- **Reminder and MCP programme refs shift with time-shift changes.** Reminders are re-timed in the same transaction. The MCP follow-rule feature (later) must re-resolve refs after a shift.
- **Memory.** The Catalog moves from per-screen to shared and idle-dropped, a net improvement. Watch `MemStats` when the job runs during playback.

---

## 10. Open questions for you

1. **Should the two rules ("+1/+2 follow their base", "Copies share a guide") be on by default for TV and TV 2?**
   - Recommended: **on**. They only touch channels with no guide data, or +1s that are an hour early. A Ctrl+S view "Auto-fixed by rules" shows what they did, and one toggle per playlist turns each off.
2. **How big a batch can Claude apply with only a "yes" in chat?**
   - Recommended: **up to 50 channels**. Bigger batches open the confirm card in Tuner for you to press Enter. The hard cap is 200 per proposal.
3. **Does catch-up start early or late across a whole playlist, or only on some channels?**
   - Recommended default: add a per-playlist catch-up offset (0 unless you set it). A channel's own offset still wins, and Check ▶ lets you calibrate one channel and then "Use for all ‹playlist› channels".