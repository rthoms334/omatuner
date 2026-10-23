# New episodes: design

> Approved and built, 2026-10-23. Built on origin/main c36f13d (v0.3.6); it became DESIGN §44, which is the contract as built. Every question in §11 is answered.

**The idea in one line:** when a new episode of a series you care about appears in a playlist's series catalogue (on demand, not live TV), Tuner tells you once: a desktop notification with Play, a "New episodes" row on Home, and a "New" badge in Watchlists. The series you care about are the ones on any watchlist plus the ones you've binged.

**Nothing to set up.** There are no rules to write. Tuner already knows what's on your lists and what you've watched, and it learns about new episodes from the playlist refreshes it already does. The first run records what exists today and says nothing about it.

**How it differs from Following**
| | Following (§33) | New episodes |
|---|---|---|
| Watches | The **TV guide**: airings on live channels | The **series catalogue** of each playlist (VOD) |
| Learns from | Guide imports (`jobs::after_guide`) | Playlist refreshes (`JobKind::RefreshSource`) |
| You set up | A rule per show, team or event | Nothing: your watchlists and what you've binged |
| One alert is about | An airing: a time on a channel ("Taskmaster · Tue 21:00 · Channel 4") | An episode you can play now ("Landman: 2 new episodes") |
| Identity | `follow_match.event_key`, e.g. `taskmaster#s20e04` (`textmatch::event::event_key`) | The title (its copies across playlists) + season + episode |
| What it can do | Remind, switch, list, record (`FollowMode`) | Notify (Play, Download), the Home row, badges |
| Lifetime | Until the airing has passed | Until you watch or dismiss it |

The two coexist without double alerts: one episode alerts once, through whichever feature saw it first. The rules are in §3.3.

**What exists today that this builds on**
- **Series catalogue and `last_modified`.** Xtream `get_series` returns `last_modified` per series (`XtSeries.last_modified`, `src/sources/xtream/types.rs:403`). The import stores it in `series.last_modified` and also uses it as `added_at` (`jobs::import_source`, `positive(s.last_modified)`). `store::import::upsert_series` **clears `details_at`** when a non-null `last_modified` differs from the stored one, so the next detail load fetches the episodes again. Home's "Recently updated series" rail already sorts by it (`ui/screens/home.rs`, `SortMode::RecentlyAdded`), library-wide and without knowing which episodes are new.
- **Episodes load lazily.** `JobKind::LoadSeriesDetails(SeriesId)` → `jobs::details::load_series` → `XtreamClient::series_info` → `store::import::store_series_details` → `replace_episodes`, which upserts by `(source_id, key)`, **deletes the series' other episodes** and sets `details_at`. Details younger than `DETAILS_TTL_S` (7 days) aren't fetched again. The jobs run on the "details" lane (4 at once, `jobs::Limits`).
- **M3U series are complete at import.** `store::import::write_series_episodes` replaces each M3U series' episodes on every import and sets `details_at` to the import's start (DESIGN §27.5). There is no `last_modified` and no per-series API.
- **Copies.** A title's copies across playlists form a stack (`series.match_hash`, `tmdb_id`, the user's `title_link` joins and splits; `lib.vod.stacks(false)`). `store::watchlist::title_copies` lists a title's copies. `store::copies::{episodes, seasons, episode_progress, next_episode, same_episode}` match episodes across copies by `(season, episode)`; `episode_progress` says "any copy watched → watched".
- **Watched state.** `watch_progress (source_id, kind, item_key, position_s, duration_s, finished, updated_at, …)`. `finished` is set at `player.watched_pct` (92 %) and by Ctrl+W / "Mark season watched" (`store::user::mark_watched`, `copies::mark_episodes`).
- **Watchlists.** `watchlist_item` holds `series` entries, whole (`season_from`/`season_to` NULL) or a closed range (`valid_range` requires both ends). `progress_units` reads a whole-series entry as seasons ≥ 1, so specials are left out.
- **Notifications.** `platform::notify` sends freedesktop notifications with **one** action plus a click on the body (`show_with_action`). It falls back to an in-app toast without a notification server. Following's summary (`jobs::follow::announce`, `notify_with_open`) is the model for a summary with Open.
- **Scheduling.** `jobs::scheduler`: a refresh check at launch (`refresh_on_launch_after_h` = 12), then `RefreshStale` every 15 min, honouring `playlist_refresh_h` (24 by default). Ticks are skipped while `player_busy`. `guide_check::request` shows how to debounce a follow-up job after imports.
- **Connection budget.** `player::budget::SharedBudget` leases are taken by pictures and downloads (`Holder::{Picture, Download}`) for **streams**. API calls (`player_api.php`, including `get_series_info`) take no lease. The app already makes this call while a stream plays (a series page opened during playback, the agent's `episodes`).
- **Downloads** (§43): `DownloadCommand::Add { targets: [Target { item, any_copy: true, .. }] }`, Keep N rules (`downloads::maintenance::refill`), Ctrl+D.
- **Guards.** `Library::conceals` (explicit while hidden), `VodSummary.locked`, hidden titles are absent from `lib.vod` (`store::library`). Spoiler guard: `Guard::text(&Subject::Episode { series_norm, season, episode }, Field::EpisodeTitle, …)` and `Guard::outside_title`.
- **Free migration number:** 0017 is taken by `fix/guide-freshness` (`0017_failing_since.sql`), so this uses **0018** (check again at build time). **Free keys:** none are needed.

---

## 1. User stories

1. **A show on my list.** *Landman* is in My list. On Tuesday TV adds S2E9 and S2E10. After the next refresh I get one notification: "Landman: 2 new episodes · S2E9, S2E10". Clicking it (or Play) plays S2E9 in Tuner.
2. **A show I binged.** I watched 7 of the 8 episodes of *Slow Horses* season 4 and never put it on a list. When S4E8 isn't new but S5E1 appears, I'm told. A show I tried for one episode and dropped stays quiet.
3. **The same episode twice.** TV adds *The Bear* S3E1 on Monday and TV 2 adds it on Wednesday. I'm told on Monday only.
4. **A busy night.** A refresh brings new episodes for six of my shows. I get one notification, "New episodes in 6 series", and it opens Home's New episodes row.
5. **Home.** Home has a "New episodes" row: one tile per show, "2 new · S2E9–E10". A tile leaves the row when I've watched its new episodes or pressed Delete on it.
6. **Watchlists.** The sidebar's Watchlists item shows a "New" dot. In the list, *Landman*'s entry reads "● New · S2E9–E10" until I open it.
7. **First run.** I install the update. Tuner records the episodes that exist today and doesn't alert on any of them. A card on Home says what it will watch.
8. **Adding an old show.** I add *The Wire* (5 seasons, finished years ago) to My list. Nothing happens. If the provider ever adds an episode, I'm told.
9. **No spoilers.** I'm halfway through season 2 when S2E10 arrives. The notification says "S2E10", never its title.
10. **Download it.** The notification also offers Download, which queues the new episodes in Downloads. Nothing downloads unless I ask.
11. **Ask the agent.** "Anything new on my shows?" The agent lists the shows with new episodes and offers to play the next one.
12. **The kids' profile.** Locked, hidden and explicit series never alert, not even as a count.

---

## 2. UX

### 2.1 Where new episodes show up
| Place | What |
|---|---|
| **Desktop notification** | One per series ("Landman: 2 new episodes"), or one summary when 4 or more series changed in one refresh ("New episodes in 6 series"). Actions: **Play** (also a click on the body) and **Download**; the summary has **Open** |
| **Home** | A "New episodes" row after Continue watching: one tile per series with new episodes, newest first |
| **Watchlists screen** | A "● New · S2E9–E10" chip on each entry with new episodes, until you open the entry |
| **Sidebar** | A "New" dot on Watchlists while a list series has news you haven't seen since you last opened the Watchlists screen |
| **Series page** | A "New" chip on the new episode rows and a header line "2 new episodes since Tue" (defaulted, §11) |
| **Settings ▸ New episodes** | The master switch, notifications, "Also series I've binged", the tracked series and why, the recent arrivals |
| **First-run card** | Once, after the first baseline: what it will watch, with [Settings] [Turn off] [OK] |

### 2.2 Notifications
```
┌ Tuner ─────────────────────────────────────┐   ┌ Tuner ─────────────────────────────────────┐
│ Landman: 2 new episodes                    │   │ New episodes in 6 series                   │
│ S2E9, S2E10 · on TV                        │   │ Landman, Slow Horses, The Bear and 3 more  │
│ [Play S2E9]  [Download]                    │   │ [Open]                                     │
└────────────────────────────────────────────┘   └────────────────────────────────────────────┘
```
- **One new episode** may show its title when the spoiler guard allows it: "The Bear: new episode · S3E1 · Tomorrow". Two or more list only S/E.
- **The series name** goes through `Guard::outside_title`. The episode title goes through `Guard::text(Subject::Episode { series_norm: series.norm_name, .. }, Field::EpisodeTitle, …)`. A hidden title is left out, never replaced with something guessed.
- **Play** plays the episode named on the button (§3.4) and raises Tuner, the way a reminder's Watch does (`hypr::focus_self`). **Download** sends `DownloadCommand::Add` for the new episodes (any copy). It is left out when `downloads::ENABLED` is off, and the body says "Downloading" instead when a Keep N rule already queued them.
- **Open** on the summary raises Tuner on Home with the New episodes row focused.
- **No notification server:** the same text as an in-app toast with Play (or Open), like Following's summary.

### 2.3 Home, Wide (≈1600 pt)
```
┌────────────────┬─────────────────────────────────────────────────────────────────────────────────────────┐
│ 󰍉 Search        │ CONTINUE WATCHING                                                                       │
│ 󰋜 Home          │ [poster▁▁▁] [poster▁▁ ] [S2E5▁▁]                                                          │
│ 󰔂 Live TV       │ NEW EPISODES                                                   since Tue · Del dismisses │
│ 󰃰 Guide         │ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐                                              │
│ 󰎁 Movies        │ │ poster │ │ poster │ │ poster │ │ poster │                                              │
│ 󰟴 Series        │ │  2 NEW │ │  NEW   │ │  NEW   │ │  3 NEW │                                              │
│ 󰓎 Watchlists  ● │ └────────┘ └────────┘ └────────┘ └────────┘                                              │
│ ⤓ Downloads     │ Landman    Slow Horses The Bear    Severance                                              │
│                │ S2E9–E10   S5E1        S3E1 ▕██░▏  S2E8–E10                                               │
│                │ My list    binged      My list     Sci-fi                                                 │
│                │ YOUR WATCHLISTS                                                                         │
│ 󰒓 Settings      │ [Star Wars · next: Rogue One · 5 of 15] [My list · …]                                    │
├────────────────┴─────────────────────────────────────────────────────────────────────────────────────────┤
│ ↵ open · Shift+↵ play next · Del dismiss · Ctrl+D download new · Ctrl+W watched · Shift+F10 more          │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```
- **One tile per title** (its copies are one tile, as everywhere on Home, §29.14). The badge counts the new episodes not yet watched or dismissed. The second line is the S/E span ("S2E9–E10", or "S2E9 · S3E1" across seasons). The third line says why it's tracked: a list name (the first, "+1" for more) or "binged". A started new episode shows its progress bar.
- **Order:** the newest arrival first. At most `RAIL_MAX` (30) tiles.
- **Enter** opens the series page on the season of the first new episode, with that row selected. **Shift+Enter** plays what the notification's Play would. **Delete** dismisses the tile's new episodes, with Undo. **Ctrl+D** downloads them. **Ctrl+W** is the existing mark-watched. The menu (Shift+F10, right-click) offers Play S2E9 · Download the new episodes · Dismiss · Open series · Stop alerts for this series (§11 Q1).

### 2.4 Home, Medium (≈950 pt, icon rail 48)
```
┌──┬──────────────────────────────────────────────────────────────────┐
│󰍉 │ CONTINUE WATCHING                                                 │
│󰋜 │ [poster▁▁▁] [poster▁▁ ] [S2E5▁▁]                                    │
│󰔂 │ NEW EPISODES                                         Del dismisses │
│󰃰 │ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐                                │
│󰎁 │ │ 2 NEW│ │ NEW  │ │ NEW  │ │ 3 NEW│  →                             │
│󰟴 │ └──────┘ └──────┘ └──────┘ └──────┘                                │
│󰓎●│ Landman  Slow Hor… The Bear Severan…                               │
│⤓ │ S2E9–E10 S5E1      S3E1     S2E8–E10                               │
│  │ YOUR WATCHLISTS                                                   │
│󰒓 │ [Star Wars · next: Rogue One] [My list · …]                        │
└──┴──────────────────────────────────────────────────────────────────┘
```
Medium drops the "why" line (it stays in the tooltip and the menu). The rail's "New" dot sits on the Watchlists glyph. Compact shows the same row with smaller posters and no header hint.

### 2.5 Watchlists screen, Wide
```
┌────────────────┬───────────────────────────────┬──────────────────────────────────────────────────────────────────┐
│ 󰍉 Search        │ WATCHLISTS                 + │ MY LIST · any order                    14 of 31 watched            │
│ 󰋜 Home          │ ▸ My list   ● New   14/31   │ ▸ ⠿ Landman  whole series      ● TV    ● New · S2E9–E10            │
│ 󰔂 Live TV       │     next Landman S2E9       │   ⠿ The Bear  S1–S3            ● TV 2  ● New · S3E1   3 of 28      │
│ 󰃰 Guide         │   Star Wars          5/15   │   ⠿ The Wire  whole series     ● TV    ✓ done                      │
│ 󰎁 Movies        │     next Rogue One          │   ⠿ Dune (2021)                ● TV 2  2 h 35                      │
│ 󰟴 Series        │   Sci-fi    ● New    2/9    │   …                                                                │
│ 󰓎 Watchlists  ● │     next Severance S2E8     │                                                                    │
│ ⤓ Downloads     │                             │                                                                    │
│ 󰒓 Settings      │                             │                                                                    │
├────────────────┴───────────────────────────────┴──────────────────────────────────────────────────────────────────┤
│ ↵ open · Shift+↵ play from here · Ctrl+N add to Up next · Del remove · Ctrl+W watched · Ctrl+↵ more              │
└───────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```
- **The entry chip** shows while the entry has new episodes inside its coverage (§3.2) that you haven't opened. Opening the entry (Enter, its series page from anywhere) or playing one of its new episodes clears the chip for that series in every list. A "done" entry that gets a new episode becomes not done (as today: done is derived), and the chip says why.
- **The lists pane** marks a list with "● New" while any of its entries has the chip.
- **The sidebar dot** shows while any list series has an arrival newer than the last time the Watchlists screen was opened. Opening the screen clears it. Binged series that are on no list don't light it: they have the Home row and the notification.
- **Medium** shows the lists as the header switcher ("My list ● ▾"), with the same chips on the entries.

### 2.6 Settings ▸ New episodes, Wide (a page after Following)
```
│ ‹ Settings   NEW EPISODES                                       checked 08:12 · tracking 14 series │
│ Tuner checks your playlists' series at each refresh (every 24 h, Settings ▸ Playlists) and tells  │
│ you when new episodes arrive. Airings on live TV are Following's job.                              │
│ [✓] New episode alerts                                                                              │
│ [✓] Desktop notifications            The Home row and the badges stay when this is off             │
│ [✓] Also series I've binged          Over 60 % of a season watched                                 │
│ TRACKING · 14 series                                                                               │
│   Landman            My list                                    TV          last new Tue 21 Oct    │
│   Severance          Sci-fi · My list                           TV · TV 2   last new Mon 20 Oct    │
│   Slow Horses        binged (S4: 7 of 8)                        TV 2        —                      │
│   The Bear           My list (S1–S3, follows new seasons)       TV 2        last new Mon 20 Oct    │
│   …                                                                                                │
│ RECENT · 30 days                                                                                   │
│   Tue 21 Oct   Landman        S2E9, S2E10     TV      notified                                     │
│   Mon 20 Oct   The Bear       S3E1            TV 2    notified                                     │
│   Mon 20 Oct   Taskmaster     S20E4           TV      quiet: aired on TV (Following)               │
```
- The tracked list explains every alert ("why am I told about this?"). Enter on a row opens the series page. Concealed and locked series are absent, not counted.
- **Medium:** one column; each tracked row wraps its playlists and "last new" under the name.

### 2.7 The first-run card
```
┌ New episode alerts ────────────────────────────────────────────────────┐
│ Tuner will tell you when new episodes arrive for 14 series:            │
│ 9 on your watchlists and 5 you've binged.                              │
│ Nothing that exists today counts as new.                               │
│ [OK · Enter]   [Settings…]   [Turn off]                                │
└────────────────────────────────────────────────────────────────────────┘
```
It shows once on Home after the first baseline (`meta` row `newep:card`), like the watchlists' "moved" card. It's acknowledged by any button or Esc. With nothing tracked it says "Add series to a watchlist and Tuner will tell you when new episodes arrive".

---

## 3. Behaviour rules

### 3.1 Which series are tracked
| Rule | Decision |
|---|---|
| **On a list** (decided) | A title is tracked when a `series` entry of **any** watchlist, My list included, holds one of its copies (`title_copies`). Episode entries (one episode in a list) don't track the series. Placeholders track nothing until they attach. |
| **Binged** (decided) | A title is tracked when, in at least one season ≥ 1, **more than 60 %** of its episodes are finished. Precisely, for the title's copies `C` (`title_copies`, visible ones only): `Known_s` = the distinct episode numbers of season `s` in any copy's `episode` rows (the union `copies::episodes` / `copies::seasons` use). `Finished_s` = the numbers in `Known_s` where some copy's episode `(s, e)` has `watch_progress.finished = 1` (`copies::episode_progress`: any copy watched → watched). Binged ⇔ ∃ s ≥ 1 with `Known_s` ≠ ∅ and `|Finished_s| / |Known_s| > 0.6`. Part-watched episodes don't count; Ctrl+W marks do. For a tracked title, `Known_s` holds only the episodes its baseline had (`newep_known.first_seen` no later than its earliest copy's `baselined_at`) plus finished ones: whatever came later (announced, muted, started when it arrived, dismissed and pruned) never lowers the ratio, so an abandoned or muted show stays tracked (Q1, Q2). |
| **"Also series I've binged" off** | Only list series are tracked. |
| **Season ranges** | A whole-series entry covers seasons ≥ 1. A range `S a–b` covers seasons `a..=b`. A range is **open-ended** when `b` reaches the newest season the title had when the entry started tracking it (the newest season in the ledger, §4, at the later of the entry's `added_at` and the title's baseline): it then also covers every later season. This is decided once, so what arrives later never closes it. So "The Bear S1–S3" with S3 as the newest season gets S4E1, S4E2 and S5. "Mandalorian S1–S2" while S3 exists is closed: only a gap filled inside S1–S2 alerts. A binged title covers seasons ≥ 1. A title covered by several entries or reasons gets the union. |
| **Specials** | Season 0 never alerts unless a range names season 0 (as watchlists leave specials out, §42 §3). |
| **Becoming tracked** | A title is baselined the moment it becomes tracked: added to a list, attached from a placeholder, or crossing the binge line. Its existing episodes are recorded silently (§3.2, Baseline). |
| **No longer tracked** | Leaving every list (and not binged) stops tracking. Its ledger rows are deleted, so a later add baselines again and never alerts on what arrived meanwhile. Arrivals already on Home stay until watched or dismissed. |
| **Guards** | A title with any locked copy, a concealed copy (`Library::conceals`), an explicit copy (`item_explicit`, in **every** parental state) or no visible copy (hidden titles are absent from `lib.vod`) is never tracked, never counted and never named. |

### 3.2 What counts as a new episode
| Rule | Decision |
|---|---|
| **Unit** | A **title episode**: the title (its stack of copies) + season + episode, the key `copies::same_episode` and `copies::episodes` use. Never an episode key: `replace_episodes` can delete and re-insert episodes, and providers re-key re-uploads. |
| **Arrival** | A coordinate `(s, e)` present in a tracked copy's `episode` rows that is in **no** copy's ledger (`newep_known`) and is covered by §3.1. Every arrival alerts (decided): list series and binged series alike. |
| **Baseline** (required) | The first run of the feature, a newly tracked title, a copy seen for the first time and the feature switched back on all record the current coordinates **silently**. A playlist's first import (`AddSource`, or `last_refresh` empty) is always silent. |
| **A new copy of a tracked title** | Its coordinates are recorded silently, except ones **beyond the title's frontier** (later than the newest `(season, episode)` in the ledger of its other copies). Those are arrivals: a second playlist that has S3 when the first stops at S2 brings genuinely new episodes. |
| **Once per title, whatever the copies** (decided) | When TV adds S3E1 on Monday it alerts and is recorded for every copy that has it. TV 2 adding S3E1 on Wednesday finds it in the ledger: no alert, recorded for TV 2. |
| **Comes and goes** | The ledger keeps every coordinate a tracked copy ever had. An episode that a provider drops and restores (an empty `series_info`, a re-upload, a re-key) never alerts twice. Nor does one a copy brought before it left the library and came back (its ledger went, §3.1): a coordinate with an arrival row for any copy of the title is never an arrival again. |
| **Already watched** | An arrival that is finished or in progress in any copy when it's found (progress moved with a re-key, or watched on another copy) is recorded, not alerted. |
| **Found by a page visit** | Episodes a series page loaded since the last check are diffed at the next check like any others. Watched ones are skipped by the rule above. |

### 3.3 Coexisting with Following
| Rule | Decision |
|---|---|
| **Different jobs** | Following watches airings in the guide. New episodes watches the playlists' catalogue. Neither reads the other's triggers. A show can be in both. |
| **One alert per episode** | When an arrival's `(s, e)` matches a Following ledger row from the last 30 days (`follow_match.event_key` = `core_title(series name)#sNNeMM`, the format of `textmatch::event::event_key`) whose rule reminded, switched or recorded it (state `scheduled`, `adopted` or `aired`), the arrival is recorded **quiet**: it's on Home and in Recent ("quiet: aired on TV"), with no notification and not counted in a summary. A Following **Record** rule's finished recording of that airing counts the same way. |
| **Seen on TV** | An arrival whose `(series_norm, season, episode)` is in `spoiler_seen` (seen live, on catch-up or marked) is recorded as watched. `spoiler_seen` only holds guarded subjects, so this is best effort. |
| **The other way round** | Following is unchanged in this feature: a guide airing of an episode announced here, or already watched on demand, still reminds. |
| **Matching titles** | The guide's title and the catalogue's series name are matched by `core_title` over the series' `clean_name`. When they don't match ("Landman (2024)" vs "Landman"), both features alert, about different things (a time; a file). |

### 3.4 Alerts
| Rule | Decision |
|---|---|
| **When** | After a refresh: one check per refresh wave (§5), never on a timer of its own. |
| **Grouping** (decided) | One notification per series ("Landman: 2 new episodes"). If **4 or more** series have arrivals in one check, a single summary instead ("New episodes in 6 series"), whose Open shows Home's row. Quiet arrivals don't count towards the 4. |
| **Play** | Plays the series' next episode by `copies::next_episode` (resume the newest unfinished, else the one after the newest watched), through the copy plan, the way Continue watching does. When that is the first new episode, the button says "Play S2E9"; when you're behind, it says "Play S1E6". |
| **Download** (decided: no automatic download) | Only when you pick it: `DownloadCommand::Add` with every new episode of the series (any copy; the downloads service picks the copy at start, §43 §3). A Keep N rule (§43) works as before and may already have queued them. |
| **Desktop notifications off** | Arrivals still reach Home, the badges, Settings ▸ Recent and the agent. |
| **The master switch off** | Nothing is checked, fetched or shown; a check still running when it goes off writes nothing (it reads the switch again inside its write). Switching it off or on forgets the ledger, so switching it on again baselines silently. |
| **No notification server** | An in-app toast with the same text and Play (or Open). |

### 3.5 Clearing
| Rule | Decision |
|---|---|
| **Home row** (decided) | An arrival leaves the row when it's **finished** in any copy (derived at read time, like watchlists' done) or **dismissed** (Delete on the tile, or the menu; Undo for 10 s). A tile leaves when all its arrivals have. Expiry after 30 days (Q3). |
| **Entry chip** (decided: until opened) | Cleared for a series when its series page is opened from anywhere, its watchlist entry is opened, or one of its new episodes is played. |
| **Sidebar dot** (decided: until opened) | Cleared when the Watchlists screen is opened (`meta` row `newep:watchlists_opened_at`). |
| **Agent** | Reading the list over MCP clears nothing. Q5: a `seen` action clears only when the user asks. |

---

## 4. Data model and storage

`src/store/migrations/0018_new_episodes.sql`:
```sql
-- 0018 New episodes (DESIGN §44). The ledger is per series COPY; alerts are per TITLE episode.
-- Rows exist only for tracked titles (watchlists + binged), so the tables stay small.
CREATE TABLE IF NOT EXISTS newep_copy (
  source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  series_key   TEXT    NOT NULL,
  baselined_at INTEGER,               -- NULL: tracked, episodes not loaded yet (baseline at first load)
  lm_seen      INTEGER,               -- series.last_modified at the last diff
  checked_at   INTEGER,               -- the last details fetch this feature made
  PRIMARY KEY (source_id, series_key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS newep_known (  -- every (season, episode) a tracked copy has had
  source_id  INTEGER NOT NULL,
  series_key TEXT    NOT NULL,
  season     INTEGER NOT NULL,
  episode    INTEGER NOT NULL,
  first_seen INTEGER NOT NULL,
  PRIMARY KEY (source_id, series_key, season, episode),
  FOREIGN KEY (source_id, series_key) REFERENCES newep_copy(source_id, series_key) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS newep_arrival (  -- one row per title episode found new
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER REFERENCES source(id) ON DELETE SET NULL,  -- the copy it arrived in first
  series_key   TEXT,
  season       INTEGER NOT NULL,
  episode      INTEGER NOT NULL,
  series_title TEXT    NOT NULL,      -- display only; never logged at INFO
  why          TEXT    NOT NULL DEFAULT '[]',  -- JSON: ["list:3", "binge"]
  arrived_at   INTEGER NOT NULL,
  notified_at  INTEGER,               -- NULL: not announced (notifications off, quiet, headless)
  quiet        TEXT,                  -- 'following' | 'seen' | NULL
  opened_at    INTEGER,               -- series page / entry opened: the chip goes
  cleared_at   INTEGER,               -- off the Home row
  cleared_how  TEXT CHECK (cleared_how IN ('watched', 'dismissed', 'expired')),
  fp           TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER
);
CREATE INDEX IF NOT EXISTS newep_arrival_open ON newep_arrival(cleared_at, arrived_at DESC);
CREATE INDEX IF NOT EXISTS newep_arrival_ref  ON newep_arrival(source_id, series_key, season, episode);
-- meta rows: 'newep:baselined_at', 'newep:card', 'newep:watchlists_opened_at',
-- 'newep:lm_unreliable:<source id>' (§5, Fallback).
-- Q1: newep_mute(source_id, series_key, muted_at, fp, orphaned_since).
```
- **Size.** About 50 tracked titles × 2 copies × 100 episodes ≈ 10k `newep_known` rows. The diff reads `episode` through its `episode_series (series_id, season, episode)` index.
- **Re-keys.** `newep_arrival` joins `store::user` HOLDERS ("new episode"), so a Home tile survives an M3U → Xtream upgrade (`apply_key_moves`) and lists under Orphans otherwise. `newep_copy` / `newep_known` aren't moved: a re-keyed copy is a new copy and baselines silently, with the frontier rule over its other copies.
- **Deleting a playlist** cascades its ledger rows. Its arrivals keep their row with `source_id` NULL and show through the title's other copies, or are cleared when there are none.
- **Backup** exports `newep_arrival` (Home state) and the switches. It doesn't export the ledger: a restore re-baselines silently.
- **Model.** `row_id!(ArrivalId)`. `src/new_episodes/model.rs`: `Tracked { title, copies, why: Vec<Why>, coverage }`, `Arrival`, `NewEpisodesSnapshot { series: Vec<SeriesNews>, list_dot: bool, checked_at }`.
- **Config** `[new_episodes]`: `enabled = true`, `notify = true`, `binged = true`, `binge_pct = 60` (decided; not shown in Settings), plus `home_days` if Q3 is decided. **Kill switch** `new_episodes::ENABLED`, as with `downloads::ENABLED`.

---

## 5. Architecture

```
 RefreshSource (import done) ──▶ new_episodes::request(ctx, source)   debounced like guide_check::request;
                                                                     starts when no RefreshSource or
                                                                     RebuildLibrary is queued or running
 watchlist edit / progress write ──▶ new_episodes::track(ctx)        local only: no network
 app launch, first library ──▶ request(Launch)                       picks up a headless `tuner --refresh`

 JobKind::CheckNewEpisodes { reason, sources }   dedupe "new-episodes"; sources merge (merged_with)
   1 tracked    = store::new_episodes::tracked(conn, lib)     lists + binge (§3.1), guards applied
                  → baseline the newly tracked (silent)
   2 fetch      = Xtream copies on the refreshed sources whose details_at is NULL (last_modified
                  changed) or whose fallback is due → details::load_series_checked(id), one at a
                  time per playlist, ≤ 40 per playlist per run, paused while player_busy
   3 diff       = per title: current (s, e) over its copies − the ledger → arrivals (§3.2)
                  → coverage, watched, Following-quiet filters → newep_known + newep_arrival
                  (one write transaction)
   4 announce   = group per title; ≥ 4 titles → one summary; notify::show_with_actions
   5 publish    = DataScope::NewEpisodes → watch<Arc<NewEpisodesSnapshot>> → Home, Watchlists,
                  sidebar, series page, Settings, agent
```
- **Triggers.** Import completion calls `new_episodes::request` next to the existing `guide_check::request` in `jobs::import_source` (Xtream when the series kind finished; M3U unless `NotModified`). The job waits for the `RebuildLibrary` the refresh submitted, because stacks decide what a title is. `RefreshStale` refreshes playlists two at a time, so a wave of refreshes is one check and one round of notifications.
- **Tracking on progress and list changes.** `DataScope::Progress` and `DataScope::Watchlists` call `track`, debounced 60 s, off the UI thread. It recomputes binge only for the series whose episode progress changed, and baselines titles that became tracked. This matters for M3U: its import writes the new episodes before any check runs, so a title has to be tracked (and baselined) **before** the refresh that brings its next episode.
- **Fetching Xtream episodes.** `last_modified` is the cheap trigger: `upsert_series` already clears `details_at` when it changes, and only tracked copies are fetched. `details::load_series` skips details younger than 7 days, so the check uses a variant, `load_series_checked(ctx, id, max_age)`, with the same parsing and `store_series_details`. It never fetches untracked series.
- **Fallback for providers that don't update `last_modified`.** Tracked copies whose details are older than 7 days are fetched anyway, oldest first, within the cap. When such a fetch finds an arrival while `series.last_modified` equals `lm_seen`, the playlist is marked `newep:lm_unreliable:<id>`, and from then on its tracked copies are fetched at every refresh. A provider that bumps `last_modified` on every sync just means the tracked copies are fetched each refresh, bounded by the cap.
- **M3U.** No fetch: every import replaces the episodes (`write_series_episodes`), so step 3 diffs straight after it.
- **The connection budget.** `get_series_info` is an API call, not a stream: `SharedBudget` isn't involved and nothing takes a lease. A one-connection account keeps playing while the check runs, as it does today when a series page loads during playback. The check still pauses between fetches while `player_busy` (Loading or Zapping), and fetches serially per playlist. It runs in a job lane of its own (not Following's "features" lane), so those minutes never delay Following, and its notifications are sent after the job ends.
- **Headless `tuner --refresh`** does nothing here, like the guide check. The app's launch check finds what it changed (cleared `details_at`, new M3U rows).
- **Notifications with two actions.** `platform::notify` gains `show_with_actions(summary, body, &[("play", "Play S2E9"), ("download", "Download")], on_action)`. The body click stays the default action (Play). New `PlatformEvent::{NewEpisodePlay { series, episode }, NewEpisodeDownload { episodes }, NewEpisodesOpen}`; the UI resolves refs as `ReminderWatch` does.
- **Reads are never per frame.** The snapshot is rebuilt on `DataScope::{NewEpisodes, Progress, Watchlists, Vod, VodDetails}` and on a library swap, except for the player's periodic progress saves while a film or episode plays (the save at stop rebuilds it, as with the spoiler guard). It computes "why" only for the titles it shows, without the binge pass over every title. "Watched" clearing is derived when the snapshot is built and written back lazily as `cleared_how = 'watched'`.

---

## 6. Guardrails

- **Parental lock, hidden and explicit** (§3.1): never tracked, never alerted, never counted (not even "6 series"), absent from Home, Settings, the badges and MCP. An explicit or locked copy excludes the whole title, failing closed, as Following never matches explicit channels in any parental state. A title hidden after an arrival drops off Home at the next snapshot; its rows aren't deleted.
- **Spoiler guard.** Notification text, toasts, tile captions, Settings ▸ Recent and MCP replies show S/E always and episode titles only through `Guard::text(Subject::Episode)`. Series names go through `Guard::outside_title`. Plots and stills are never shown in alerts. The series page's "New" chips follow the page's existing guard.
- **The provider.** Only tracked copies are fetched, serially per playlist, at most 40 per playlist per check, only after a refresh. API calls only; no stream is opened. `series_info`'s second spelling (`XtreamClient::series_info`) counts as part of the same fetch. A failed fetch is retried at the next refresh, never in a loop.
- **Privacy.** No titles at INFO: `newep: 3 arrivals in 2 series (source 2), 1 quiet`. No URLs anywhere. The ledger holds keys and numbers only.
- **Noise caps.** At most 1 notification per series per check and at most 3 per-series notifications in all (the 4th makes it a summary). A provider that re-numbers a whole season can't produce more than one notification per series.

---

## 7. MCP and skills

**A `new_episodes` tool** (read-only; plain JSON, opaque refs, self-describing, so it works with any MCP client, not only the Claude plugin):
| action | params | result |
|---|---|---|
| `list` (default) | `since?` (a time or "7d"; default: everything not cleared), `include_cleared?`, `limit?` (≤ 50 series), `spoiler_safe?` | `{checked_at, series:[{series_ref, title, year, why:["list:My list","binged"], playlists:["TV"], new:[{ref, season, episode, title?, spoiler?, arrived_at, state: new\|opened\|watched\|dismissed, quiet?, downloaded?}], next_episode?:{ref, season, episode, resume_at_s?}}]}` |
| `tracked` | `limit?` | `{enabled, notify, binged, series:[{series_ref, title, why, playlists, last_new_at?}]}`, so an agent can answer "why do I get alerts for X?" |

- Locked, hidden and explicit series are absent, as everywhere (§30). Spoiler-guarded titles are dropped with `spoiler: {hidden, rule, reason}`; `reveal_spoilers` works as elsewhere.
- Playing and downloading use the existing tools: `play` with `next_episode.ref`, `downloads add` with the `new[].ref`s (RULES.md confirmations apply).
- `status` gains `new_episodes: {series, episodes}` (counts of uncleared arrivals).
- Writes (`seen`, `mute`) follow Q1 and Q5.

**Skills**
- **series:** "anything new on my shows?" / "what's new in Landman?" → `new_episodes list` → name the shows and the next episode for each → offer `play`. "Why am I told about X?" → `tracked`.
- **pick:** "what's new" first offers new episodes of the user's shows, then `browse` with `added_since` for the rest of the library.
- **plan:** unchanged; its `when_to_use` already sends live airings there. It says that on-demand arrivals are automatic and need no `follow` rule.
- **RULES.md:** `new_episodes` is read-only. Never call a series "new" from `browse`'s `added_since` alone: the provider's `last_modified` moves for many reasons.

---

## 8. Tests worth having

**Store (temporary DB, fake library)**
1. **Baseline:** the first check records every coordinate and creates no arrival. A title added to My list later baselines silently, and so does switching the feature off and on.
2. **Every new episode:** S2E9 and S2E10 added to a tracked copy give two arrivals and one notification group. A gap filled inside a covered season is an arrival.
3. **Copies:** S3E1 on TV, then on TV 2 two checks later, gives one arrival, recorded for both copies. A new copy with S3 while the old one stops at S2 alerts S3 only (the frontier). A playlist's first import is silent.
4. **Comes and goes:** an empty `series_info` deletes the episodes (`replace_episodes`) and the next fetch restores them: no arrival. A re-keyed episode with the same S/E: no arrival.
5. **Binge** (table-driven): 5 of 8 (62.5 %) is binged and 4 of 8 (50 %) isn't. Copies combine (3 finished on TV + 2 on TV 2 of 8). Part-watched doesn't count. Season 0 doesn't count. A new arrival doesn't lower the ratio.
6. **Ranges:** S1–S3 with S3 newest follows S4. S1–S2 with S3 existing alerts only inside S1–S2. Two entries of one title give the union. An episode entry tracks nothing.
7. **Guards:** a locked copy, an explicit copy (with explicit shown and hidden), a concealed copy and a hidden title never produce arrivals or counts.
8. **Following:** a `follow_match` row `taskmaster#s20e04` (state `aired`) makes the VOD arrival of Taskmaster S20E4 quiet. A `spoiler_seen` row marks it watched.

**Jobs (fake Xtream server)**
9. A refresh that changes `last_modified` of one tracked and one untracked series fetches only the tracked one. A provider that never changes `last_modified` is caught by the fallback and marked unreliable. The per-playlist cap holds.
10. A wave of two refreshes gives one check and one round of notifications. Six series with arrivals give one summary; three give three notifications.
11. No call ever touches `SharedBudget`. The check pauses while `player_busy`.

**Notify (notify-rust test bus, as the existing `notify_at(Some(bus), …)` tests in `platform/notify.rs`)**
12. Two actions: Play and the body click send `NewEpisodePlay`; Download sends `NewEpisodeDownload`. Without a server, the toast fallback is used.

**UI (rig):** the Home row shows one tile per stack; Delete dismisses with Undo; finishing the episode removes the tile. The entry chip clears on Enter; the sidebar dot clears when the Watchlists screen opens. A guarded episode title never appears in the notification or the tile.

---

## 9. Implementation plan

One feature, one gate and one review at the end. The tracks only keep agents on disjoint files:

| Track | Files | Size |
|---|---|---|
| **N0 skeleton** | `0018_new_episodes.sql`, `src/new_episodes/{mod,model}.rs`, config `[new_episodes]`, `JobKind::CheckNewEpisodes`, `DataScope::NewEpisodes`, `PlatformEvent` variants, `ArrivalId`, HOLDERS row | ~250 |
| **N-core** | `src/store/new_episodes.rs` (tracked, binge, baseline, diff, Following and `spoiler_seen` checks, clearing, backup) + tests; `src/jobs/new_episodes.rs` (request/track debounce, fetch with cap and fallback, announce); `details::load_series_checked`; the hook in `jobs::import_source`; `platform::notify::show_with_actions` | ~1,300 |
| **N-ui** | Home row (`RailKind::NewEpisodes`), the watchlist entry chip and lists-pane mark, the sidebar dot (`sidebar()` gains a flag beside `up_next` / `downloads`), series page chips, Settings ▸ New episodes, the first-run card, the toast fallback | ~900 |
| **N-agent** | `agent/new_episodes.rs`, `proto`, `tools_batch`, `status`, skills (series, pick, plan) + RULES.md | ~350 + md |
| **Docs** | DESIGN §44, MANUAL_CHECKLIST (a real provider adding an episode overnight; the notification's two actions on Omarchy), README feature line | ~120 |

The total is about 2,900 lines including tests. The order is N0, then N-core and N-ui in parallel, then N-agent.

---

## 10. Risks

1. **Providers' `last_modified` is unreliable.** Some never change it; some change it for everything. Mitigation: the fallback learns per playlist (§5), and the fetch is bounded by the tracked set and the cap, never the catalogue.
2. **Re-numbered or re-uploaded episodes look new.** `details::series_details` numbers an episode without a number by its position, and M3U numbers unmarked entries by order, so an insertion can shift numbers. Mitigation: the ledger keeps every coordinate, so only coordinates never seen alert; one notification per series per check caps the noise; Settings ▸ Recent shows what happened.
3. **Wrong stacks.** Two different series stacked as one would share a ledger; one split would alert twice. Mitigation: the stacks are the ones the user already sees and fixes (Same title as… / Not the same title).
4. **Notification actions.** A second action (Download) needs a notification server that shows action buttons. Omarchy's must be checked on the machine (MANUAL_CHECKLIST). The body click (Play) works everywhere, and Download is also on the Home tile and the series page.
5. **Late alerts.** The playlist refresh runs every 24 h by default, so an episode can be a day late. That's the cost of no extra polling (Q9).
6. **Binge from Ctrl+W.** Marking a season watched to tidy up makes a show binged. Mitigation: "why" is visible in Settings and on the tile, and Q1 (mute) covers it.

---

## 11. Decisions

**Answered (the user, 2026-10-22)**
| # | Question | Answer |
|---|---|---|
| 1 | Where alerts show | **All three.** A desktop notification, like reminders (`src/platform/notify.rs`), with a Play action; a "New episodes" row on Home, listing what arrived since you last looked, cleared once watched or dismissed; a "New" badge on the watchlist entry and on the sidebar's Watchlists item until opened. |
| 2 | Which series | **Series on ANY watchlist, including My list** (for season-range entries, define sensibly what an open-ended range means: §3.1). **Also series the user has binged even if they're on no list.** "Binged" = **over 60% of a season watched**, in at least one season, defined precisely against `watch_progress` (finished flags) and combined copies (§3.1). |
| 3 | Trigger | **Every new episode**, for both list series and binged series. |
| 4 | Grouping | **One notification per series** ("Landman: 2 new episodes"). If 4 or more series change in one refresh, **a single summary notification** ("New episodes in 6 series") that opens the Home row. |
| 5 | Download | **No automatic download.** Alerts only; the notification may offer a Download action that uses the existing downloads (`src/downloads`, `docs/design/downloads-design.md`). |

**Defaulted (follows from the answers or is internal; say if you want otherwise)**
- Episode entries in a list don't track their series; placeholders track nothing until attached.
- A playlist's first import is silent; a new copy alerts only beyond the title's frontier (§3.2).
- An episode Following already reminded about, switched to or recorded is quiet here (Home only, §3.3).
- Settings live on their own page, Settings ▸ New episodes, after Following, with a first-run card. The feature is on by default (the user asked for it), and its first run is a silent baseline.
- The series page marks new episodes with a "New" chip.
- Locked, explicit and concealed copies exclude the whole title.

**Answered (the user, 2026-10-23): every open question as recommended**
| # | Question | Decision |
|---|---|---|
| Q1 | **Per-series mute** ("Stop alerts for this series") from the notification and the series page? | Yes, on the series page, the Home tile menu and Settings ▸ New episodes, with Undo. **Not** on the notification: a third action is too much for a toast, and some servers hide extra actions. A muted title keeps its ledger, so unmuting doesn't replay what arrived meanwhile. |
| Q2 | Does a series that **falls below the binge threshold**, or **comes off all lists**, keep alerting? | Off a list: stop (unless binged), as §3.1 drafts. Binge: arrivals never lower the ratio (§3.1), so it only falls when you unmark episodes; then stop. A show you abandoned stays tracked; Q1 covers it. |
| Q3 | **How long "New" stays on Home** | Until watched or dismissed, plus an expiry after **30 days** (`cleared_how = 'expired'`), so a show you've stopped watching doesn't sit on Home forever. |
| Q4 | **What Play plays**: the first new episode, or the next unwatched? | **The next unwatched** (`copies::next_episode`), named on the button ("Play S1E6"). It never skips past episodes you haven't seen, so it can't spoil, and it matches Continue watching. When you're up to date, that is the first new episode anyway. |
| Q5 | **Can the MCP tool also mark items seen?** | Yes, a `seen` action (ids, or a `series_ref`) that only clears the Home row and badges, never watched state. Use it only when the user asks ("clear my new episodes"); it's undoable in Tuner. Reading never clears. |
| Q6 | **A minimum season size for binge.** "Over 60 %" makes a one-episode season (1 of 1) binged. | Require at least **3 finished episodes** in that season as well as over 60 %. |
| Q7 | **Hold notifications while you watch, and at night?** A refresh can land during a film or at 3 a.m. | Hold desktop notifications while Tuner plays a film or episode full screen, and during the news-flash quiet hours when they are set (`[follow.news_flash] quiet_from_min..quiet_to_min`, off by default), then send them; Home updates at once. |
| Q8 | Should **Following stay quiet** for an airing of an episode you've already watched on demand (or that this feature announced)? | Already watched: yes, skip the reminder (a small change in `follow::plan`, separate PR). Only announced: keep the reminder, because it's about a time on a channel. |
| Q9 | **Check tracked series more often than the playlist refresh** (24 h by default)? | No for now. `get_series` is the whole catalogue, and per-series polling would be the "extra polling" we avoid. Revisit if day-late alerts annoy; a 6-hour check of tracked Xtream series only would be about 50 small API calls. |
| Q10 | **Specials (season 0)**: alert? | No, unless a list range names season 0 (§3.1); specials are often trailers and extras. |
| Q11 | **A "Check now"** in Settings ▸ New episodes (and for the agent)? | Yes in Settings: it runs the same check on the tracked series, without a full playlist refresh, at most once every 10 minutes. Not over MCP. |
