# fui: the Following and news-flash UI

Track notes for the wave-4 DESIGN pass (DESIGN §33, the news-flash preset of the big-features additions). This track flips `follow::ENABLED`.

## Where Following shows

- **Guide.** `R` on a programme opens the Follow sheet. The programme popover's third button is "Follow…", or "News flash this daily…" when the programme is a news bulletin (`follow::suggest` puts the news flash first). The guide's channel menu (Shift+F10, right-click) and search's programme-hit menu have "Follow this programme…". On a gap, `R` explains that a programme is needed.
- **Guide tooltip.** "From Following: Arsenal", "Listed by Following: …", "(over its weekly limit)" or "(skipped)". It is read from the ledger in the background into a per-thread index keyed by (displayed start, title), refreshed at most once a minute and at once after a save here. The hook signature has no `UiCtx`, so the index remembers the store from the first Following hook that had one (every guide key passes through `follow::ui::on_action`).
- **Settings ▸ Following.** From top to bottom:
  - a note with "N coming · checked Sat 08:12";
  - "+ Follow…", which opens the sheet on the templates;
  - the rules in list order (higher wins a clash), each with its summary ("Team · live · remind 2 min · spoiler-safe · 4 coming"); Enter or a click opens the rule in the editor;
  - Upcoming for the next 14 days, from `store::follow::upcoming`, so spoiler-safe rules show only their name; Enter or a click opens that airing's programme popover (Watch, "Cancel reminder" skips it, Follow…);
  - the `[follow]` switches;
  - the news flash's quiet hours (typed "22:00–07:00"; empty means none; anything unreadable keeps the stored value), "not while another app is fullscreen" and the headlines length.
- **Summary toast.** `AppEvent::Follow(Summary)` becomes a toast in `TunerApp::handle_event`. It only arrives when no notification server answered.

## The sheet (`ui/overlays/follow_sheet.rs`)

- **Pick view** (opened on a programme): the suggestions, a When row and a count line from the live preview ("1 in the guide · next Sat 12:30 · spoiler-safe on", or "counting…").
  - ↑↓ move, and moving onto a suggestion makes it the draft (the When choice is kept when it fits the kind).
  - ←/→ change When; Enter follows the focused suggestion; Tab jumps to the buttons.
  - "More options…" opens the editor on the draft, unsaved.
  - A click selects a suggestion. Nothing typed lives here, so a click outside closes the sheet.
- **Templates** (Settings "+ Follow…"): Team, Show, Event, News flash and Custom. Choosing one opens the editor with the Names field ready for typing. The first name typed also becomes the rule's name.
- **Edit view:** Name, Names / Leave out / Also needs (chips: Enter adds one, Backspace on the empty field removes the last), Look in, Match, Airings, Days (every day / weekdays / weekends), Channels (any / favourites / one playlist / what Claude set), When, Before / Start, the news flash's Length, Headlines and Sound, Spoiler-safe, Limit and On / Paused, followed by "In the guide now" with its matches, what was left out and why, and warnings.
  - Buttons: Delete… (a menu: cancel its N reminders, or keep them as your own), Duplicate, ↑ / ↓ (reorder now, then re-evaluate), Cancel and Save.
  - Esc ladder: leave the field → (unsaved) a warning "Unsaved rule changes: Enter on Save, or Esc to discard" with the focus on Save → discard. A click outside never throws an edit away.
- **Preview.** The sheet starts `follow::preview::PreviewService` with an `EventBus` of its own, so `PreviewReady` comes straight back to the sheet and never reaches the app. A preview answers only for the draft it was asked about: until the draft and the asked rule match, the count says "counting…", which keeps a stale count out of the toast.
- **Saving** goes through `follow::ui::save_rule` (writer, `DataScope::Follow`, then `EvaluateFollows { Rules }`). The toast reads "Following: Chelsea · 1 in the guide", or "News flash: …".
- **Spoilers.** The header shows the programme through the spoiler guard's `Spoilers::text`. The preview shows full fixtures, as the design asks.
- **Layout.** The card is 560 rem wide (Pick) or 680 rem (Edit), clamped to the window. The scrolling part leaves room for the buttons as measured on the last frame. A test checks that Follow, Save and Delete… stay on screen at 790 × 700 with 16-pt text and at 1600 × 1000 with 12-pt text.

## Deviations

- **The rule editor lives in the sheet, not in a Settings sub-view.** Settings' Feature hooks don't receive K/J, x/Delete or drag for feature rows, so priority, pause, duplicate and delete are all in the editor (see the integrator list).
- **No Undo button on the "Following X" toast and no Open button on the summary toast.** Both need new `Action` variants. `follow::ui::undo_follow(services, id)` is ready for the first.
- **No hours-of-day setting in the editor** (`spec.between`). Days and the bulletin's title cover the news-flash cases. Claude can still set hours through MCP, and the editor keeps them.
