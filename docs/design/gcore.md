# Guide fixes engine (G-core)

This is the engine half of DESIGN §31 (Guide fixes). The screen, the Settings entries and the agent tools are separate tracks: gui and agent. Wave 4 folds this into DESIGN.md.

## Rules: "+1/+2 follow their base" and "Copies share a guide"

The rules are derived after every rematch. B7 runs them after a guide import's flip and after `rematch_all`, before `DataScope::Epg`.

Two functions do the work:
- `store::guide_fix::plan_rule_rows` runs on a reader. It sees the mappings without any rule rows (`base_rule_channels`).
- `write_rule_rows` runs on the writer, in one transaction. It replaces the rule rows, resolves, and retimes reminders whose shown time moved (B8).

`derive_rule_rows` does both on one connection. `apply` and `undo` use it when a rule changes.

**+1/+2 follow their base.** The parser is `textmatch::timeshift::marker`, with a strict grammar: `+`/"plus" followed by 1–24 hours at the end of the name, after trailing quality words and brackets. The bases of a channel are the channels of the same playlist whose `norm_name` equals the normalised base name. There are four outcomes:
- **No guide channel of its own.** The rule adds a `sibling` row: the base's guide channel, with `shift_min = N·60`.
- **It resolves to the base's guide channel** (a provider reusing the base's tvg-id). The automatic row keeps its place and gains `shift_min`.
- **Its own guide entry, or the base's guide channel is itself a "+N" entry.** Nothing is derived.
- **Its bases resolve to different guide channels.** Nothing is derived. The issue list says "N bases, pick one".

**Copies share a guide.** This applies to a channel with no guide channel. It takes the guide channel of the first copy that resolves, using:
- the B5 family key (`COALESCE(family, norm_name)`);
- another playlist;
- a usable copy: supported, not user-hidden, playlist enabled;
- no tvg-id veto.

It runs after the siblings, so a "+1" copy passes on its shift.

Rules never touch manual rows. A manual row made after the plan was read still wins (`ON CONFLICT … WHERE method != 'manual'`).

**Resolution.** The order is: manual, then `sibling`, then guide priority, then method (`sibling` and `copy` rank last). The winner's `shift_min` goes to `channel_epg.shift_min`. The effective shift is `COALESCE(item_override.shift_min, channel_epg.shift_min, channel.tvg_shift_min)`, which is the library's B8 select. `sources::resolve` uses the same select for the OSD and catch-up programme lookups.

**Rematch stability.** `replace_auto_mappings` keeps the rule rows, but a matcher row for the same (channel, guide) replaces a rule row. It also carries a rule shift over to an automatic row that is unchanged. As a result, the flip's resolve keeps the shown times, and re-deriving the same rows moves no reminder twice.

## Batches and Undo

`store::guide_fix::apply(conn, actor, label, &Proposal, &GuideOpts, now)` writes one logged batch: `guide_fix_log` plus one `guide_fix_change` row per field, with JSON before and after values. It covers these fields:

| Field | What it writes |
|---|---|
| `map` | A manual row via `epg::set_manual_mapping_as`, carrying `epg_name` and `actor` |
| `shift` | `item_override.shift_min` |
| `catchup` | `item_override.catchup_offset_s` |
| `dismiss` | `item_override.guide_ok \|= IssueKind::bit` |
| `rule` | `SourceSettings::guide` via `sources::set_guide_rules`, then re-derive |

Limits and checks:
- Override rows carry the channel's fingerprint, so they reattach on a re-keying refresh. A row left all-default is deleted.
- A proposal that carries a stamp must match `stamp()` now. If it doesn't, the call fails with `STALE`.
- Claude's batches are capped at 200 changes (`MAX_BATCH`).

When a channel's effective shift changes, `store::retime::channel_shift` runs in the same transaction. It moves reminders, Up next slots and Following rows.

`undo` restores each field only while it still holds the batch's `after`. A field that changed since is skipped as "changed since", and a channel that is gone is skipped as "no longer in the playlist".

The caller emits `DataScope::Epg`. When `ApplyReport::mappings_changed` is set, it also submits `EvaluateFollows { reason: Mapping }` (B7).

## Issue list

`store::guide_fix::facts(conn, opts, now)` is read off the UI thread, in about 0.15–0.3 s at 79k channels. It loads:
- the rule channels;
- overrides and manual rows;
- cached suggestions;
- the probes (guide channels with a programme now or later);
- the resolved guide channels' display names;
- the rules and the stamp.

`guide_fix::issues::classify(lib, &facts, Option<&Catalog>, opts)` is pure. The universe is the library snapshot, so explicit, hidden and filtered channels follow its visibility. Each channel gets at most one row. The kinds are checked in this order, and dismissed kinds are skipped:
1. Gone from guide
2. +1 without shift
3. Looks wrong
4. Guide ended
5. Copy has guide
6. Suggested
7. No guide

A row's `why` is a fixed phrase and never contains a URL.

`guide_fix::preview::preview(conn, changes, now)` returns before and after now/next through one batched `now_next`. A reset previews the non-manual resolution (`epg::resolve_one`).

`guide_fix::catalog::shared(conn)` returns one `Arc<Catalog>` per database. It is keyed by the guides' active generations and dropped by a reaper thread after 2 minutes unused.

## The guide check job

`jobs::guide_check::run` works in this order:
1. It debounces for 30 s. Requests coalesce, because the job hasn't read its inputs yet.
2. It takes a library snapshot.
3. It runs `check_now`:
   - It clears the cache together with the new stamp when the stamp changed.
   - It scores unmapped shown channels without cached rows, in chunks of 256 on 2 threads.
   - It pauses while `player_busy` and can be cancelled between chunks.
   - It writes each chunk in its own transaction.
4. It ends with `DataScope::GuideIssues`.

Suggestions are cached from a score of 0.8 up. The issue list applies the user's threshold.
