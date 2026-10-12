---
name: watch
description: Play, find or catch up on live TV in Tuner, the user's IPTV player. "Is X on", "put X on", "I missed X, play it from the start", "no spoilers", the news, background TV, what's on now, what's this and how long is left, switch me over at kick-off, put both semis on side by side.
when_to_use: Use when the user wants something on the TV now or soon in Tuner, asks what is on, wants to catch up on a programme that aired, or wants two live things at once. Hand standing "every time X is on" requests to the plan skill.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__guide_search mcp__plugin_omatuner_tuner__whats_on mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__categories mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__control mcp__plugin_omatuner_tuner__reminders mcp__plugin_omatuner_tuner__schedule_switch mcp__plugin_omatuner_tuner__cancel_switch mcp__plugin_omatuner_tuner__queue mcp__plugin_omatuner_tuner__multiview mcp__plugin_omatuner_tuner__spoiler_guard
---

# Watch: find it, play it now, or catch up

First read `${CLAUDE_SKILL_DIR}/../../RULES.md` (this plugin's shared rules) and follow it throughout.

## Steps

1. Call `status`: what plays now, whether Up next or multi-view is on, `streams` per playlist.
2. Find it.
   - A named programme: `guide_search` with its title words (`include_past` for "I missed it").
   - A kind of thing ("football", "news", "something in Spanish"): `guide_search` without
     `query`, with `categories` and a window (`from: "now"`, `to` a few hours ahead), or
     `whats_on` with `categories`.
   - A channel ("what's on BBC One at 9"): `whats_on` with `channel` and `at`.
   - Sport, reality TV, a part-watched series, or "no spoilers": add `spoiler_safe: true`.
3. Resolve by `airing`:
   - `live` → `play` the programme ref (say "this replaces …" when something plays). Add
     `from_start: true` for "from the start" when `catchup` is true.
   - `past` with `catchup: true` → `play` it; it plays from the archive. Mention
     `catchup_until` if it is soon.
   - `past` without catch-up → say plainly that it can't be replayed; offer the next airing
     from `guide_search`.
   - `upcoming` → say when and where, then offer a reminder: `reminders` `add` with
     `mode: "notify"`, or `mode: "switch"` when the user wants Tuner to switch by itself (that
     interrupts playback: confirm). Without the `reminders` tool, use `schedule_switch`.
4. Confirm in one line what plays, on which channel, where (the pop-out or Tuner's window:
   `opened_in`), and how to stop or undo it.

## Cases

- **"Catch me up on the news, 10 minutes max"**: find the newest news bulletin with catch-up
  (`guide_search` with `categories: ["news"]`, `include_past: true`) and play it from the start.
  Don't summarise the news yourself.
- **Background TV**: rank by low attention: news, music, rolling sport, nature, cooking
  (`categories` tags help). Avoid films, dramas and programmes about to end
  (`remaining_min`). Offer a lower volume with `control` `volume`.
- **"What's this / how long's left / what's after?"**: `status`, then `details` on the playing
  channel (its `schedule` has now and next).
- **"Switch me over at kick-off, skip the build-up"**: the programme's start is often the
  build-up. Use the description (keep it private in spoiler-safe mode) or the usual build-up
  length, say it is an assumption, and add `reminders` `add` with `mode: "switch"` and
  `offset_min`. Confirm first.
- **Better copy / another playlist**: `details` lists `copies`; `play` a copy's ref.
- **"Put both semis on" / "X and Y side by side"**: `guide_search` each, check
  `status.streams` (each playlist usually carries one stream), then `multiview` `open` with
  both refs and `sound` on the one named first or asked for. On `needs_rearrange` ask before
  resending with `rearrange: true`. On `unavailable`, offer "X now, Y from the start on
  catch-up later (spoiler-safe)".
- **"I'll watch it later" / "don't tell me the score"**: offer `spoiler_guard` `blackout` for
  that programme (confirm), and `mark_seen` once they say they've watched it.
- **On the TV / full screen / the big screen**: `play` with `open_in: "full_window"`; for what
  already plays, `control` `window` (it carries on, full screen in Tuner's window).
- **"Pop it out" / mini player**: `control` `popout`.
- **"Go back to the last channel"**: `control` `previous_channel`.

Never zap through channels in a loop to look for something; search the guide instead.
