---
name: plan
description: Plan TV with Tuner's guide and set reminders. Tonight, this weekend, sport fixtures, follow a team or a show, a daily news flash, clashes, list or cancel reminders, a sleep timer ("stop at 11", "turn off when this ends").
when_to_use: Use when the user wants an evening or weekend planned, reminders or automatic switches set, things followed after every guide refresh ("every Arsenal game", "pop up the 1 o'clock news on weekdays"), their reminders listed or cancelled, or a sleep timer.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__guide_search mcp__plugin_omatuner_tuner__whats_on mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__categories mcp__plugin_omatuner_tuner__continue_watching mcp__plugin_omatuner_tuner__browse mcp__plugin_omatuner_tuner__reminders mcp__plugin_omatuner_tuner__schedule_switch mcp__plugin_omatuner_tuner__cancel_switch mcp__plugin_omatuner_tuner__follow mcp__plugin_omatuner_tuner__sleep_timer mcp__plugin_omatuner_tuner__queue mcp__plugin_omatuner_tuner__multiview mcp__plugin_omatuner_tuner__spoiler_guard mcp__plugin_omatuner_tuner__sources
---

# Plan: tonight, this week, sport, reminders, sleep timer

First read `${CLAUDE_SKILL_DIR}/../../RULES.md` and follow it throughout.

## An evening or a weekend

1. Turn the request into windows and categories. Expand with your knowledge (team aliases,
   "X v" / "v X", an event's sessions), labelled.
2. `guide_search` per window: with `query`, or without it using `categories`; `live_only` for
   sport, `exclude_repeats` / `new_only` for shows. Drop previews, magazines, replays and films
   that only share a word.
3. Merge simulcasts into one entry (the best channel is first; `also_on` lists the rest).
4. Build a timeline by day: start, channel, live / repeat / catch-up (`catchup_until`), and
   mark clashes.
5. Fill weak slots from `continue_watching` or short films (`browse` `max_minutes`); VOD and
   catch-up parts can go into Up next (`queue` `add`), live anchors stay reminders.
6. Before writing, `reminders` `list` so nothing is duplicated. Confirm the whole batch once.
   Reminders notify by default; `mode: "switch"` only for the main event or on request (it
   interrupts what plays, opens in the pop-out by default; `open_in` changes that).

## Clashes

Both at 8: if `status.streams` shows two playlists free, offer multi-view (`multiview` `open`);
otherwise play one and offer the other later from the start on catch-up, spoiler-safe. Never
zap back and forth by yourself. Up next: check `status.queue.eta_end` ("the 21:00 switch
interrupts Goldfinger at 41 min").

## Following ("every Arsenal game", "new episodes of Taskmaster")

A rule is a standing order: Tuner re-reads the guide after every refresh and sets reminders for
each match.
1. Expand aliases and sessions from your knowledge (labelled).
2. `follow` `preview` with the rule (`kind`: team, show, event, custom). Show the matches and
   what was left out (`left_out` with reasons, `warnings` such as "matches several sports"),
   then tune `without`, `categories`, `airings`.
3. Confirm once: mode (notify by default; switch only when asked, explicitly), lead,
   spoiler-safe (on by default for sport).
4. `follow` `add`, then say "Tuner checks the guide for this after every refresh".
Never create a rule from an inferred interest. "What am I following" → `follow` `list`;
"stop following X" → `follow` `remove` (confirm whether to keep its reminders).
"Why wasn't I reminded about X?" → `follow` `preview` with its `id` (left-out reasons) and
`sources` for guide coverage. Without the `follow` tool, do a one-off `guide_search` pass and
offer reminders for what it finds.

## News flash ("pop up the 1 o'clock news every weekday, headlines only")

`follow` `preview` then `add` a rule with `kind: "news_flash"`, `name` / `terms` the bulletin's
title, `days` (for example `["mon","tue","wed","thu","fri"]`), `headlines_min` (default 5) and
`volume` (`normal`, `mute`). It pops up in the pop-out without taking the keyboard,
never interrupts a film or episode (a notification with Watch instead), and closes after the
headlines.

## Reminders

"What reminders have I got?" → `reminders` `list`. "Cancel the Sunday ones" → list, show which,
confirm, `cancel` each. Rule reminders name their rule; removing one stops that match only.

## Sleep timer ("stop at 11", "when this episode ends", "after two episodes")

`sleep_timer` `set` with exactly one of `at`, `after_min`, `end_of_programme`, `after_items`,
`end_of_queue`, and `then: "stop"` or `"pause"`. It runs in Tuner (it outlives this chat) and
warns with a toast and Cancel first. Read back the stop time from the answer.

## Recording plans

Include `downloads list` alongside reminders when planning a viewing day: scheduled jobs,
account use and free space matter. Resolve programmes with the guide, obtain consent, then
`downloads record` with the exact programme ref. A bare channel needs `minutes`. State the
start, stop and any clash. An unanswered clash keeps watching; Tuner saves from catch-up
when available or marks the recording Missed. Recordings work with Tuner closed while the
computer is awake; don't promise that a timer wakes a suspended machine.

For recurring programmes, preview a Following rule, show its matches, then use `follow` with
`action: record` after consent. It records new episodes and skips repeats. The rule can be
removed with `follow remove`; individual jobs can be cancelled with `downloads cancel`.
