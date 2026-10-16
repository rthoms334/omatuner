---
name: fix
description: Troubleshoot Tuner. Buffering, a dead or SD channel, no sound, is it me or the provider, find a better copy, why can't I find X, no guide, the guide is an hour off, a +1 channel shows the wrong programme, catch-up starts late, fix my guide, how are my playlists doing, when does my subscription expire.
when_to_use: Use when playback misbehaves, a channel, title or guide entry is missing or wrong, or the user asks about their playlists' or guides' health in Tuner.
allowed-tools: Read mcp__plugin_omatuner_tuner__status mcp__plugin_omatuner_tuner__diagnose mcp__plugin_omatuner_tuner__cast_devices mcp__plugin_omatuner_tuner__sources mcp__plugin_omatuner_tuner__details mcp__plugin_omatuner_tuner__search mcp__plugin_omatuner_tuner__play mcp__plugin_omatuner_tuner__control mcp__plugin_omatuner_tuner__guide_search mcp__plugin_omatuner_tuner__guide_fix mcp__plugin_omatuner_tuner__follow
---

# Fix: playback trouble, missing things, guide and playlist health

First read `${CLAUDE_SKILL_DIR}/../../RULES.md` and follow it throughout. Settings advice is text only: you don't
change Tuner's settings yourself (guide fixes go through `guide_fix`, with the user's yes).

## Playback ("this keeps buffering", "channel's dead", "is it me or the provider?")

1. `diagnose` (with the channel's ref, or none for what plays). Read `likely`:
   - `provider_outage`: failures across one playlist → it's the provider; try another playlist.
   - `connection_limit`: the account's streams are in use (another device, or both pictures on
     one playlist) → stop the other stream or use the other playlist.
   - `account`: expired or disabled → check the subscription (`sources`).
   - `dead_channel`: this channel keeps failing → use a copy.
   - `local_decode` / `network`: dropped frames or the connection here.
2. Better copy: `details` on the channel lists `copies` with quality, `failures_7d`,
   `has_guide`, catch-up. Rank them (fewest failures, then quality, then guide), tell the
   user, and `play` the copy's ref.
3. Call `status` again after about 20 seconds and report whether it improved.
4. If every copy fails, say so and suggest the same programme on another channel or on
   catch-up later.
"No sound": `status` (muted? volume?) then `control` `unmute` / `volume`.

## Casting ("it won't play on the TV", "the TV went blank")

1. `status`: `casting` has the TV, its `phase` and, after a failure, `error`, a typed reason
   (Tuner may be on another workspace, so it can't be seen). `diagnose` has a `cast` section
   (the TV's model and tvOS version, the pairing variant, the firewall state, the last error).
   `cast_devices` says whether the TV is `paired` and `available`.
2. Say what the `error` means and what the user does:
   - `cast-firewall`: the firewall blocks the TV from reaching Tuner. Tell the user to press
     **Allow on firewall** in Tuner (Settings ▸ Cast ▸ Network). No tool changes the firewall;
     never run `sudo`, `pkexec` or `ufw`.
   - `cast-unreachable`: the TV is off, asleep or on another network: turn it on, then try again.
   - `cast-pairing-required` / `cast-pairing-lost` / `cast-pairing-refused`: pair the TV again in
     Tuner (Settings ▸ Cast). For `cast-pairing-refused` (and often after a tvOS update) the
     Apple TV needs Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network.
   - `cast-stopped-on-tv`: the TV's remote stopped it, or another device took over. Not a fault;
     offer `play` with `device` again, or `control` `play_here` (the last item here, from where
     the TV was).
   - `cast-unsupported` / `cast-needs-ffmpeg` / `cast-cpu-too-slow` / `cast-receiver-failed`: that
     channel's format doesn't play on the TV (Tuner converts it when it can); offer another copy
     (`details` `copies`) or `control` `play_here`.
3. Offer to try again with `play` and `device` only after the user says the cause is fixed.

## "Why can't I find X?"

`search` with `explain_filtered: true`. `filtered` names the user's own settings that hide it
(a hidden category, the category filter, hidden by them, title languages, a playlist out of
"All playlists"); say where to change that in Tuner. A locked item shows as locked. Something
explicit while locked is simply not found: say it isn't in Tuner, nothing more.

## Guide problems ("no guide", "an hour off", "+1 shows the wrong programme", "fix my guide")

1. One channel: `guide_fix` `issues` with its `ref` (`candidates` lists likely guide channels).
   Overview: `guide_fix` `issues` (coverage per playlist and the issue list).
2. Explain the cause in one line from `kind` and `why`.
3. `guide_fix` `propose`: explicit `changes` (`guide` from a candidate, `shift_min`,
   `catchup_offset_min`, `reset`, `dismiss`), `accept` Tuner's own fixes by kind and
   `min_confidence` (0.95 for "fix my guide"), and/or `rules` per playlist (they change every
   future refresh: say so). For "fix my guide" propose the rules first, then the accepted batch.
4. Show the summary and up to 5 before/after now-next rows, then ask.
5. On yes: `guide_fix` `apply` with the `proposal_id` and `confirm: true`. Report `applied`
   and `skipped`, and the undo ("say 'undo', or Ctrl+Z in Guide fixes"; `guide_fix` `undo`
   with `undo_id`). More than 50 channels: `pending_review`; Tuner shows them in a confirm
   card, so tell the user to press Enter there (Esc cancels). Don't apply it again; if they
   cancel, propose again or a narrower batch.
6. Catch-up offsets: don't guess; suggest Tuner's Check, or use the user's "starts N minutes
   late".
"Why wasn't I reminded about X?" → `follow` `preview` with the rule's `id` (left-out reasons)
and `sources` for guide coverage.

## Playlist health ("how are my playlists doing", "when does my sub expire?")

`sources`: per playlist the account (status, expiry, connections), last refresh and its error,
guide coverage and failures in 24 hours; per guide its age and when it ends. Rank the problems,
most urgent first. A regular check can be set up with Claude Code's /loop or /schedule if the
user wants (opt-in).

## Downloads and recordings

Read `downloads list` and `status` once. Explain the job's reason before proposing a change:
waiting for space needs deletion or a Settings cap/reserve change; waiting for an account
means playback or another job holds it; waiting for the window resumes at its start. A
non-resumable server restarts a paused file. Three failed jobs pause that account's queue;
use Retry only after checking the cause, without repeated probing.

A Partial recording retains what was captured. Finishing remuxes HLS into a seekable file;
a failed finish keeps the source folder and can retry on restart or with `downloads retry`.
Missing media is not included in backups. Ask before deleting files or interrupting another
job, and never print private paths, credentials or stream addresses. See RULES.md §14.
