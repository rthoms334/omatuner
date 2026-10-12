---
name: pick
description: Recommend and play a film, series or live show from the user's Tuner library. Moods, "like X", "surprise me", what can I finish in 40 minutes, for two people or the family, what's new, a film in a language, which films of a franchise I have and in what order.
when_to_use: Use when the user wants Claude to choose something to watch from their own library, or to line several titles up in Tuner's Up next. Use discover for trending or top-10 lists from the web.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__categories mcp__plugin_omatuner_tuner__browse mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__history mcp__plugin_omatuner_tuner__continue_watching mcp__plugin_omatuner_tuner__favourites mcp__plugin_omatuner_tuner__favourites_edit mcp__plugin_omatuner_tuner__match_titles mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__queue
---

# Pick: choose something for me

First read `../../RULES.md` and follow it throughout.

## Steps

1. Turn the request into filters.
   - Mood or genre → `categories` (movie or series; `tag` for kids, family, music, doc), then
     `browse` with `group`, or `genre`.
   - Time ("I've got 40 minutes", "under 2 hours") → `max_minutes` (movies). For "what can I
     finish", also `continue_watching` (`remaining_min`).
   - "Something new" → `added_since` (for example 7 days ago) and `sort: "recent"`.
   - A person → `person`; years → `year_from`/`year_to`; quality bar → `min_rating`.
   - Never seen → `unwatched: true`. "Surprise me" → `sort: "random"`.
2. Build a taste anchor when it helps: "like the last thing I watched" → `history`; "like
   Arrival" → your knowledge of it (labelled), checked against `browse` results.
3. Offer three options, one line each: title, year, length, and why it fits. Use `details` for
   the plot when needed (spoiler-safe for part-watched series). Prefer the copy `details.copies`
   ranks best.
4. Play the chosen one with `play` (say "this replaces …" when something plays).

## Modes

- **Surprise me**: skip the three options, `play` one at once and add one line: "not this?
  say 'again'".
- **Two people / family**: apply every exclusion ("no horror, no rom-coms"). For a family, the
  youngest viewer sets the limit; give labelled content notes (see the kids skill).
- **In a language** ("a Korean film", "easy French for practice"): `categories` shows each
  category's country or language tag; browse those.
- **Franchise check** ("which Mission Impossible films do I have, in order?"): `search` each
  title (or `match_titles` with the list from your knowledge), report which are there and
  which are missing, and label the order as yours. Offer to save them as a favourites list
  (`favourites_edit`, confirm) or queue them.
- **Line-ups** ("queue three", "a Bond marathon in release order"): `queue` `add` with the refs
  in order (more than 3 needs one confirmation; say the order is from your knowledge). Then
  `queue` `play` only if asked.

Never present a title the library doesn't have as available; `search` or `match_titles` first.
