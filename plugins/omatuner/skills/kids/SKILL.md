---
name: kids
description: Family and parental help in Tuner. Something for the kids (by age), is X OK for a 6-year-old, what finishes before bedtime and then stop, what's on the kids' channels this afternoon, check or tighten the parental locks.
when_to_use: Use when children are watching or a parent asks about suitability, bedtime viewing or Tuner's parental locks. Claude can only add locks, never remove them.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__categories mcp__plugin_omatuner_tuner__browse mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__guide_search mcp__plugin_omatuner_tuner__whats_on mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__queue mcp__plugin_omatuner_tuner__sleep_timer mcp__plugin_omatuner_tuner__parental_status mcp__plugin_omatuner_tuner__lock
---

# Kids: suitable picks, bedtime, parent checks, tighter locks

First read `../../RULES.md` and follow it throughout; rules 5 and 6 matter most here.

## Picks ("something for the kids, 5 and 8, nothing scary")

1. Ask for the ages once if they're missing.
2. Candidates: `categories` with `tag: "kids"` or `"family"` (movie, series or channel), then
   `browse` that category; or `guide_search` / `whats_on` with `categories: ["children"]` for
   live TV.
3. Judge each on its category, `content_rating` (programmes), genre and plot (`details`) and
   your knowledge of the title. The youngest child sets the limit.
4. Offer three picks with one reason each. Mark any title you don't recognise "unverified".
   Never call something simply "safe"; give content notes ("a scary villain at the end").
5. Locked items show as locked; leave them out of picks for children.

## "Is X OK for a 6-year-old?"

`search` + `details`, then answer from its rating, category and your knowledge, labelled, with
the specific concerns. If you don't know it, say so.

## Bedtime ("bed's at 7:30, what finishes in time? then stop")

1. Time left = bedtime − now − 5 minutes.
2. Fit items to it (`browse` `max_minutes`, `episodes` lengths, live `remaining_min`).
3. Line them up (`queue` `add`, confirm when more than 3) and arm the stop: `sleep_timer`
   `set` with `end_of_queue: true` (or `at` the bedtime).
4. Tell the parent the timer and the queue are helpers a child can cancel in the app.

## Lock audit ("make sure the kids can't get to horror")

1. `parental_status`. Warn plainly about: no PIN set, `relock_min` 0 (it relocks only on
   restart), the session unlocked now, explicit content shown.
2. Scan `categories` (movie, series, channel) for unlocked categories and titles that don't fit
   (horror, 18-rated, adult-adjacent names). Locked ones say `locked: true`.
3. Propose the locks with a reason each, confirm once, then `lock` with the refs. Offer
   `lock` with `session_now: true` to lock the session at once.
4. Settings beyond locks (setting a PIN, the relock time) are advice only: tell the parent
   where they are in Tuner (Settings ▸ Parental).

Never suggest unlocking anything or showing explicit content to solve a request.
