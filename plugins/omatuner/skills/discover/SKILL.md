---
name: discover
description: What's trending, Netflix top 10 this week, box office this weekend, best-reviewed films I've got, is X any good — from the web, matched against the user's Tuner library, then play, queue or save the ones they have to a "Trending" list.
when_to_use: Use when the user asks about trending, popular, new-release or well-reviewed titles, or a review score, and wants to know which of them they can watch in Tuner.
allowed-tools: Read WebSearch WebFetch mcp__plugin_omatuner_tuner__match_titles mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__watchlist mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__queue mcp__plugin_omatuner_tuner__status
---

# Discover: trending and well-reviewed titles the user has

First read `${CLAUDE_SKILL_DIR}/../../RULES.md` and follow it throughout.

## Steps

1. Get the list from the web with WebSearch / WebFetch: the chart or review site the user named,
   else a reputable current one. Note the source and its date. Search with titles and chart
   names only, never personal data or viewing history.
2. `match_titles` with up to 100 `{title, year, kind}` (add `tmdb` or `imdb` ids when the page
   gives them). Titles the user hid or filtered out, and explicit ones while the session is
   locked, come back `missing`; that is expected, don't explain it. A locked title comes back
   matched with `locked: true` (rule 5).
3. Answer with what the library has first, in the chart's order: title, year, the copy that
   plays (playlist), and the web data labelled with its source and date ("#3 on Netflix's top 10,
   week of 5 October, per netflix.com"). Then, only if useful, what's missing.
4. Offer: play one (`play`, after `status`), queue several (`queue` `add`, confirm when more
   than 3), or save them to a “Trending” watchlist (`watchlist create`, `name: "Trending"`,
   `play_order: "any"`; `add` when it exists, confirm first). Missing titles can be kept as
   placeholders with title, year, kind and TMDB id when known. Locked titles are never added.
5. Refreshing the list later: on request, or weekly with Claude Code's /loop or /schedule if the
   user asks for it.

## Rules for web data

- Never present web data as Tuner's: Tuner's tools say what the user has, the web says what is
  popular or how it was reviewed.
- Spoiler-safe applies: no plot spoilers from reviews; for a series the user is part-way through,
  no episode details.
- "Is X any good?": give the scores with their sources and dates, then `match_titles` or
  `search` to say whether the user has it.
