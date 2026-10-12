# Spoiler guard engine (track "score", S1)

This is the engine half of DESIGN §34 (the spoiler guard), as amended by big-features-plan §1 B3/B4 and §5. The user's decision is that the guard is on but narrow. On its own it acts only on:
- series you're part-way through;
- sport you follow (Following rules marked spoiler-safe);
- topics the user adds.

Nothing is guarded before an event starts. The guard is display-only. It runs after `Visibility`: it never builds events from explicit content, never lists or counts it, and never reveals it. The UI flag `spoiler::ENABLED` stays false until S-ui flips it.

## Pieces

| File | What it holds |
|---|---|
| `textmatch/scores.rs` | `neutralise_scores` is a token scanner. A score is rewritten only between two words. Clock times, chains ("9-1-1"), "Catch-22", "S2-E5" and seasons ("2023-24") are left alone. `has_result_verb` uses whole words. |
| `model/spoiler.rs` | `SeenRow` and `SEEN_SPAN_S` (7 days: one "seen" covers repeats, +1 channels and both playlists). |
| migration `0012` | `spoiler_seen` gains `title` (for Settings ▸ Seen recently) and `series_norm` (a seen series airing moves the series boundary on). |
| `store/spoiler.rs` | Rules: `list_rules`, `add_rule`, `update_rule`, `set_enabled`, `remove_rule`, `set_series_guard`. Terms are folded and capped. Seen rows: `mark_seen`, `unmark_seen`, `seen_covering`, `seen_recent`. Also `follow_topics` (B4: spoiler-safe Following rules, read by data), the builder's reads, `prune`, `export` and `import`. |
| `spoiler/snapshot.rs` | `build(conn, cfg, lib, now)` builds the `GuardSnapshot`: topics with embargo windows (merged into disjoint spans, so `active` is one binary search), series marks, and zap-shield spans per channel. |
| `spoiler/verdict.rs` | `judge(snapshot, subject, now)` and `tracks(...)`. Fails closed before the first build. |
| `spoiler/tracker.rs` | `start` installs a live `Guard` in `AppShared` and runs two tasks. The builder (debounced 1 s) runs on `Guard::rebuild`, `Guard::configure`, a new library build (not a zap's copy) and hourly. The seen tracker polls every 30 s. `Tracker::observe` is the pure seen rule: live counts at ≥ 50 % watched or when present in the last 10 minutes; catch-up counts at ≥ 90 %. |
| `spoiler/mod.rs` | `event_key_of`, `seen_row_for(&Programme, how, now)` (Ctrl+W, MCP `mark_seen`), `series_key`, `is_sport`, `local_date`. |
| `guard/mod.rs` | The B4 API over a live gate. It provides `text`, `judge`, `shield`, `outside_title` and `topics` (Settings / MCP status). Pause and resume are session-only ("show everything for 15 min"). `generation()` lets UI caches know when to refresh; `take_seen()` feeds the Undo toasts. `Policy::SpoilerSafe` strips even when the guard is off or is the identity. |
| `guard/reply.rs` | `guard_json(value, guard, policy, reveal, now)` is the MCP choke point. It works by field name: `title`, `name` (when `kind` is movie, series or programme), `now`, `subtitle`/`sub_title`, `description`, `plot`, and episodes under a series. Guarded fields are removed and the object is annotated `spoiler: {hidden, rule, reason, until}`. `reveal(ref)` unguards that object only. |

## Rules (the verdict)

- **Topic mention.** A programme whose title or sub-title mentions an *active* topic within its category scope loses:
  - its sub-title, description and image;
  - the scores in its title (they are rewritten);
  - the title itself, when the title also carries a result verb.
- **Sport descriptions.** While a topic with "sport descriptions" is active, every programme in a sport category loses its sub-title and description.
- **What "active" means:**
  - **Until seen:** active inside a window. A window opens at the event's first airing (a title match) and closes `window_h` after that airing ends, or when the event is seen.
  - **Always:** always active.
  - **Blackout:** active until its end time.
- **Series.** For a guarded series, episodes after the next-up one lose their title, plot and still. Guide airings with S/E after next-up are guarded the same way. An airing without S/E loses its sub-title and description.
  - **Part-way series** are guarded automatically: at least one episode done (or 90 % watched, or seen on TV), activity within 180 days, and another episode in some playlist. Progress is merged across playlists through the episode fingerprint's series key.
  - **Next-up** is the started episode, or the catalogue's next episode after the last one done.
  - **The user's own row:** "don't guard" wins over the automatic rule.
- **Zap shield.** It stops a zap onto a channel whose current programme mentions an active topic, unless that programme *is* the until-seen event itself. A blackout shields the event too. The shield never acts before the first build, while paused, or with `shield_zap` off.

## Measurements (release; this machine was busy with other builds)

- **Snapshot build, realistic.** 5 000 channels, 480 000 programmes, 10 narrow topics, 2 000 series, 60 part-way: **3.2 ms**.
- **Snapshot build, pathological.** 10 topics that each hit the 4 000-candidate cap (40 000 windows): about 0.3 s, off the UI thread.
- **Judging 200 guide cells.** About 0.3–1.3 ms per frame with the *placeholder* `TermSet`, which allocates per term. The real aho-corasick set removes most of that. `normalize_name` costs about 50–160 µs per 200 titles and `fold` about 30–70 µs. S-ui should cache verdicts per (programme, `Guard::generation()`, minute).
- **200 shield checks:** about 2–5 µs.

## Cross-track needs

1. **F-AB.** `textmatch::{fold, terms::TermSet, event::event_key}` must be real. Five of the seven tests in `tests/spoiler_guard.rs` match topics, so they fail on this branch alone. They pass with reference bodies (fold → lower-case words; TermSet → whole-word contains), which were used locally and not committed.
2. **Integrator / mv2 (`app.rs`).**
   - Call `services.agent.guard().configure(config)` at start and after every Settings edit.
   - Call `guard().rebuild()` on `DataScope::{Epg, Live, Vod, Progress, Spoiler, Follow}`. A new library is already noticed by polling.
   - Without `configure`, the builder uses `Config::default()`.
3. **Integrator (`shared.rs`).** The tracker watches only `AppShared::main_player()`. Tile B needs an AppShared accessor, for example `tile_player(TileId)`.
4. **Agent.**
   - `agent::guard::guard_reply` should call `crate::guard::reply::guard_json(result, guard, policy, &reveal, now)`, with `policy = SpoilerSafe` for `spoiler_safe=true` and `reveal` from `reveal_spoilers`.
   - The `spoiler_guard` handler uses `store::spoiler::{add_rule, remove_rule, set_series_guard, mark_seen}`, `spoiler::seen_row_for`, `Guard::{topics, paused_until, rebuild}`.
5. **S-ui.**
   - Undo needs a keymap `Action` (integrator) and `store::spoiler::unmark_seen` + `Guard::rebuild`. `Guard::take_seen()` lists what the tracker marked.
   - `Subject::{Vod, Episode}.series_norm` is `spoiler::series_key(series name)`, or the catalogue's `series.norm_name`.
6. **remind / mv1.** `notify.rs` and `mpris.rs` call `Guard::outside_title` (the identity until a live guard is installed).
