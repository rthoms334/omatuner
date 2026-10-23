---
name: series
description: Continue, recap or find episodes of a series in Tuner. "Carry on with X", "next episode", "where am I in X", "previously on", "recap season 1 before season 2", "which episode is the one where…", "which shows have I stalled on", "next 3 then stop", "marathon season 2".
when_to_use: Use for anything about a TV series in the user's Tuner library: progress, the next episode, recaps, finding an episode, or queueing episodes. What's new in the user's shows (new episodes in a playlist) is here. For when a new episode airs on live TV, use plan.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__episodes mcp__plugin_omatuner_tuner__new_episodes mcp__plugin_omatuner_tuner__continue_watching mcp__plugin_omatuner_tuner__history mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__queue mcp__plugin_omatuner_tuner__watchlist mcp__plugin_omatuner_tuner__sleep_timer mcp__plugin_omatuner_tuner__spoiler_guard
---

# Series: carry on, recap, find an episode

First read `${CLAUDE_SKILL_DIR}/../../RULES.md` and follow it throughout. A series the user is part-way through is
spoiler territory: use `spoiler_safe: true` until you know where they are.

## Carry on

1. `search` the series (`kinds: ["series"]`), then `episodes` with its `series_ref`. Progress is
   merged across the series' copies in every playlist.
2. Choose: the episode in progress if under about 95% watched, else the next one, moving into
   the next season. `episodes.next_episode` is what `play` on the series would start.
3. Confirm only when you'd skip an unfinished episode or switch to another copy. Then `play`
   the episode ref (or the series ref).

## Series in a watchlist

For “carry on with my Star Wars shows”, use `watchlist lists` / `show` to find the list,
then `watchlist next`; name the episode and resume point before `status` → `watchlist play` with that entry’s
`id`. A series range stays within its seasons. Playing carries on through the list unless
the user says “just the one” (`just_one: true`). “Where am I in Star Wars” reads `show`.
Ask which list when the name is ambiguous; never skip a locked entry without saying why.

## What's new on my shows

"Anything new on my shows?", "what's new in Landman?": `new_episodes` (`list`). Tuner tracks the
series on the user's watchlists and the ones they binged, and tells them itself (a notification,
a New episodes row on Home); this is the same list. Name each show with its new episodes ("Landman:
S2E9 and S2E10") and `next_episode`, the one to play: it can be an earlier unwatched episode, so
say "S1E6 is next" rather than naming the newest. Offer `play` with `next_episode.ref`, or
`downloads add` with the `new[].ref`s (RULES.md §14). The list is spoiler-guarded like
`episodes`: an episode title may be hidden, so give numbers only. "Why am I told about X?" /
"which shows do you watch for?": `new_episodes tracked` (`why` says the list or "binged"; `muted`
shows are silenced). Reading never clears anything, and "what's new" is not "seen": call `seen`
only when the user asks to clear them ("clear my new episodes"), `mute` only when they ask to stop
alerts for one show (RULES.md §15). If `enabled` is false, say the user switched New episodes off
in Tuner (Settings ▸ New episodes).

## Recap

Use the library's plots up to the last finished episode (`episodes` with `spoiler_safe: false`
only for those episodes) plus your own knowledge, labelled. Never go past that boundary. If you
don't know the show, say so. The spoiler guard hides later episodes; that is expected.

## Find an episode ("the Friends one with the football")

Make a season/episode guess from your knowledge, check it with `episodes` (`query` with words of
the title or plot, or `season`), and always confirm with the plot line shown so the user can
check before you `play`.

## Tracker ("which shows have I stalled on?")

From `continue_watching` and `history`, group shows into active (watched this week), stalled
(not for a month) and new since last watch. Personal data: keep it in the chat.

## Line-ups

- "Next 3 then stop": `episodes` → `queue` `add` the three refs in order → `queue` `play` →
  `sleep_timer` `set` with `end_of_queue: true` (or `queue` `stop_after` the last id). Confirm
  the plan once.
- "Marathon season 2": the rest of the season (at most 50), confirmed once.
- "I'll watch the rest later": offer `spoiler_guard` `add_series` (confirm) so the guide and
  answers keep later episodes hidden.

Without the `queue` tool, play one episode and say Tuner offers the next one at the end.

## Downloads

For “download the rest of season 1”, resolve the series or episode with `search` / `details`,
then `downloads list` for space and existing jobs. Use `downloads series` with `scope: season`
and `season`, or `downloads add` with the returned remaining episode refs. Confirm more than
three items as RULES.md §14 requires. `scope: next_n` with `n` chooses unwatched episodes;
`scope: keep` maintains 1–10 unwatched downloads and always needs consent. Report returned
counts and size; explain any refused refs. Finished copies play locally through ordinary Play.
`downloads pause` keeps partial files, `resume` continues them and `cancel` removes the job.
