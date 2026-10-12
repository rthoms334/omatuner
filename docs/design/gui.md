# Guide fixes UI (G-ui)

This is the screen half of DESIGN §31 (Guide fixes). The engine is in `docs/design/gcore.md`. Wave 4 folds both into DESIGN.md. `guide_fix::ENABLED` is on.

## The screen

`ui/screens/epg_mapping.rs` (`ScreenId::EpgMapping`) becomes **Guide fixes**. Ctrl+E, "Assign EPG channel…" and Settings ▸ EPG still lead here.

**Dashboard.**
- Wide: a card per playlist (coverage bar, "N of M shown have guide", "X to fix · Y one-key"), then the Rules card of the playlist in scope (its two rule toggles and its catch-up offset).
- Medium: one line per playlist and a rules line.
- Compact: one line, which opens the view menu.
- A card click scopes the screen to that playlist (a second click goes back to All). The scope is Live TV's: Alt+P and Ctrl+PgDn / Ctrl+PgUp work here too.
- Issue chips (Wide and Medium) pick the view; the lit chip is the view, and clicking it again shows every issue.

**Issues (left pane).** The view is one issue kind, all issues, every channel, or Recent changes (Ctrl+S, or the header's `View ▾`). Hidden and category-filtered channels are left out unless "Include hidden channels" is on.
- Each row: mark box, glyph (lock when locked: dimmed, still editable), name, `→ proposed fix` or why there is none, confidence (`97 %` or `✓`), the playlist's short code with two or more playlists.
- Only the rows drawn are computed (79k channels in "All channels").
- A row without a cached suggestion gets the old lazy suggestion (batches of 40 drawn rows) as its proposal while the guide check hasn't covered it.
- Footer: "N marked · ⇧Enter accepts · Ctrl+A all" and `[Accept N]`.

**Detail (right pane).**
1. Fix: the channel's guide channel and how it got it, the issue and its "why", the proposal, a before/after now/next table (`guide_fix::preview`, one channel per read), then `Accept ⇧Enter`, `Reset Del` and `It's right`.
2. Other guide channels: the lazy suggestions, then every guide channel (type to search; Enter assigns as a logged one-change batch).
3. Adjust: Time shift (`[−1 h] [−15] value [+15] [+1 h]`). Channels with an archive also get Catch-up offset (`[−1] value [+1]`), Check ▶ and "Use for all ‹playlist› channels…".

**Recent changes.** The logged batches (label, actor, age, undone), with the channels and fields of the highlighted one. Enter or "Undo this" undoes that batch.

## Writes

Every write goes through `guide_fix::ui::submit_apply` / `submit_undo`. Each is one logged batch on the writer, followed by `DataScope::Epg`, then:
- a toast with `Undo` (`Action::UndoGuideFix(Some(log))`);
- `EvaluateFollows { reason: Mapping }` when a mapping changed and Following is on.

| What | When it asks |
|---|---|
| Accept | One channel at once. More than one opens a card: counts (guide channels, time shifts, locked), up to 6 "name: now before → now after" lines, "Undo: Ctrl+Z in Guide fixes". |
| Reset (Delete) | One channel at once; more asks (destructive). It resets the manual guide channel and the user's time shift. |
| It's right | Never; it dismisses the row's issue kind. |
| Rule toggle | Always. The card names how many channels it affects: fixed by the rule now (off), or the proposals of that kind in the playlist (on). |
| Use for all | Always. Writes the playlist's `GuideRules::catchup_offset_s`. |

**Shift and offset presses are debounced.** The preview shows the result at once. One batch is written 400 ms after the last press, or when the cursor leaves the row, the view changes, a key the screen doesn't take (Esc out, Alt+digits) arrives, or Check ▶ is pressed. Repeated presses make one undo entry. Ctrl+Shift+←/→ (±1 h) acts on the marked channels, or the highlighted one; the Adjust rows act on the highlighted channel only.

**Check ▶** writes the pending offset, waits for the write, then plays the last finished programme (shown times) from its archive through `live::play_catchup`. A locked channel asks for the PIN, and the PIN's answer retries.

## Keys

Contexts are `[EpgMapping, List | ListFiltering]` on both lists and `[EpgMapping, Panel]` in Adjust. Tab cycles Issues → Guide channels → Adjust. Enter or → on an issue opens its guide channels; ← returns.

| Key | Does |
|---|---|
| Space / Insert, Ctrl+A | Mark / all shown or none |
| Shift+Enter | Accept (the marked channels, or the highlighted one) |
| Delete | Reset to automatic |
| Ctrl+Z | Undo the last batch (`last_undoable`) |
| Ctrl+Shift+→ / ← | Time shift ±1 h |
| Ctrl+S | View menu: kinds with counts, all channels, recent changes, include hidden, playlist, each playlist's two rules, guide settings |
| Shift+F10 / Ctrl+Enter / right-click | Channel menu: Accept, Pick guide channel…, Time shift ±1 h, Catch-up offset…, Watch to check, Show in TV guide, It's right, Reset |
| Ctrl+G, Ctrl+I | Show in TV guide, Edit information |
| Esc | Clear the filter, then back to Issues, then clear the marks (toast), then leave |

The hint line adds `accept · mark · undo · view` on the issues, and `±1 h · undo` in Adjust (`Screen::hint_extras`).

**Mouse.**
- Clicking the mark box marks a row. Ctrl+click toggles a mark; Shift+click marks a range from the last mark.
- One click on a row does what Enter does (on to its guide channels); Shift+Enter or the Accept button accepts. Right-click opens the channel menu.
- The stepper buttons act like the keys. Compact has `‹ Issues`.

## Elsewhere

- **Edit information (Ctrl+I).** A Time shift row on channels (±15 min, `+1 h`). It is saved as a logged guide fix ("Time shift +1 h", with Undo in the toast), so the channel's reminders move with it.
- **Item menus.** "Time shift…" on a channel (`guide_fix::ui::menu_items`) opens Guide fixes on that channel with Adjust focused.
- **Settings ▸ EPG.** "EPG mapping…" reads "Fix guide data… (N to fix)". `guide_fix::ui::to_fix` counts once per library build off the UI thread; the screen refreshes the count whenever it classifies.
- **Onboarding Done.** "Only X of Y shown channels have a guide", from `guide_fix::coverage`.

## Memory and threads

- The report (`guide_fix::ui::load_report`: facts, shared catalogue, classify) is read on the blocking pool. It is reloaded per library generation, per "Include hidden", and after each `DataScope::GuideIssues`.
- The catalogue is the shared `guide_fix::catalog::shared` one.
- While shown, the screen repaints every 4 s. An event arriving more than 10 s after its last frame releases the report, the catalogue and the lazy suggestions.
