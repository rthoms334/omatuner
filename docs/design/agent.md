# Agent track: MCP pass 2 and the Claude Code plugin

Notes for the wave-4 DESIGN pass (§30, plus the feature sections §31–§35). The tool contract is
`src/agent/{tools,tools_batch}.rs` (schemas and descriptions) and `src/agent/{proto,params}.rs`
(the same objects as Rust); `tests/agent_app.rs` and `tests/agent_mcp.rs` pin the behaviour.

## Tools

| Tool | Module | Kill switch |
|---|---|---|
| 13 pass-1 tools (extended) | `query`, `act`, `server` | always |
| `match_titles`, `history`, `episodes`, `favourites_edit`, `parental_status`, `lock` | `library` | always |
| `diagnose`, `sources` | `health` | always |
| `reminders`, `sleep_timer` | `reminders` | `reminders::ENABLED` |
| `queue` | `queue` | `queue::ENABLED` |
| `follow` (news flash included) | `follow` | `follow::ENABLED` |
| `spoiler_guard` | `spoiler` | `spoiler::ENABLED` |
| `guide_fix` | `guide` | `guide_fix::ENABLED` |
| `multiview` | `multiview` | `ui::multiview::ENABLED` |

- Every feature is on, so every tool is listed (28). A tool whose feature's kill switch is
  turned off answers `unavailable` (RULES.md §11).
- `AgentHandler::with_tiles(TileRegistry)` lets `control tile` reach picture B; without it the
  handler reaches picture A only.

## Extensions to pass-1 tools

- `guide_search`: `query` is optional. Without it a window (`from` + `to`, at most 3 days) and
  `categories` or `channel` are required; the scan goes per guide channel over the active
  generation (at most 2000 rows). Filters: `categories`, `channel`, `playlist`,
  `favourites_only`, `live_only`, `new_only`, `exclude_repeats`, `catchup_only`. Rows carry
  `categories`, season/episode, `is_live`/`is_new`/`repeat`, `content_rating`,
  `catchup_until`, `remaining_min`.
- `whats_on`: `categories`, `live_only`; `remaining_min`.
- `details`: `copies` (a channel and its B5 copies with quality, `failures_7d`, `has_guide`,
  catch-up days; a title's stack copies), programme season/episode and `content_rating`.
- `search`: `explain_filtered` names the user's own filters (hidden category, category filter,
  hidden item, title language, playlist out of All). Explicit content is never explained,
  whatever the parental state.
- `browse`: `added_since`, `person`, `genre`. `categories`: `tag` and `tags`
  (kids/family/news/sport/music/doc from the name), `lang`.
- `continue_watching`: `remaining_min`. `status`: `queue`, `multiview`, `streams`
  (limit, in use per playlist), `sleep_timer`.
- `control`: `previous_channel` (the player keeps the PIN rule), `popout`
  (`AgentCommand::PopOut`, MV-2), `tile`; pause / resume / stop reach every picture alike.
- Every read result passes `guard_reply` (`guard::reply::guard_json`) with the call's policy:
  `spoiler_safe` → `Policy::SpoilerSafe`, `reveal_spoilers` refs → `Policy::Reveal`.

## Guardrails added in pass 2

- Locks only add (`lock`; `session_now` sets the shared unlock to none and sends
  `AgentCommand::LockSession`, which the app turns into `ParentalSession::lock`). Locked items
  are never added to favourites or Up next. Explicit items while hidden are `not_found` /
  `missing` everywhere.
- Follow rules made by Claude are marked and capped at 10 an hour and 20 in all.
- Guide fixes: proposals live 10 minutes, are bound to the guide-check stamp (`stale`), at most
  200 channels; more than 50 returns `pending_review` (the app's confirm card; not applied from
  chat). Locked channels are rejected while locked. Claude undoes only its own batches.
- Multi-view: the handler checks the playlists' stream limits (`AccountInfo::max_connections`)
  with each live picture's copies; `needs_rearrange` when only moving the playing picture works.
- No tool returns a log line: `diagnose` gives Tuner's own error kinds, failure counts and the
  pictures' health; playlist errors are the stored, redacted ones.

## The plugin

- `.claude-plugin/marketplace.json` (repo root) → `plugins/omatuner` (`plugin.json`, `.mcp.json`
  with `tuner mcp`, `RULES.md`, seven skills: watch, pick, series, plan, kids, fix, discover).
- Tool names in `allowed-tools` are `mcp__plugin_omatuner_tuner__<tool>`; a test checks each
  is a real tool and that the plugin's version follows the app's.
- `claude plugin validate ./plugins/omatuner --strict` and `claude plugin validate .` pass.
- The Arch package installs the marketplace and the plugin under `/usr/share/tuner/claude/`.
  `tuner claude-setup` prints the steps (the packaged marketplace when present, else GitHub);
  `--apply` asks on the terminal and runs only the plugin steps after a "y". Nothing touches
  the home directory at install time.
