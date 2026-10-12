# fcore: the Following and news-flash engine

Track notes for the wave-4 DESIGN pass (DESIGN §33). The engine is headless; `follow::ENABLED` stays `false` until the fui track flips it.

## Pipeline

1. **Trigger.** A guide import, a rematch or a mapping change (B7 `jobs::after_guide`), a rule change, a new library or the daily tick submits `JobKind::EvaluateFollows`. All of them run on the "features" lane and coalesce under the "follows" key.
2. **Read** (`jobs::follow::prepare`, blocking pool):
   - A library snapshot is loaded with explicit content hidden. JobContext carries no `AppShared`.
   - `follow::plan::Pool` builds the followable channels: visible, supported, mapped to a guide channel and never explicit, whatever the parental state.
   - One `store::follow::scan` reads the active guide generations through `follow::matcher::Matcher`. The matcher builds one Aho-Corasick over every rule's folded terms, plus an ASCII prefilter so most rows are never folded.
   - `follow::plan::build` turns the hits into a `Plan`.
3. **Write** (`jobs::follow::commit`, writer, one transaction). `store::follow::apply_plan` writes ledger rows and the rules' reminders:
   - reminders are created with `store::user::add_reminder` and carry `rule_id`;
   - reminders are cancelled with `remove_reminder(…, RemoveWhy::Rule)`;
   - a reminder that already exists at that channel and start time is adopted, never touched.
4. **After the write:**
   - notifications for `DataScope::{Reminders, Follow}`;
   - one toast per moved match (at most 3);
   - a summary at most every 12 h: a desktop notification, or `FollowEvent::Summary` when no notification server answers.

## Plan rules

- **Events.** Hits of one rule that start within 10 minutes of each other are one event, which merges simulcasts. The event key is `textmatch::event_key`; a fixture key is preferred over an episode key, and an episode key over a dated title.
- **Channel.** Candidates are the pool's channels on the event's guide channels, inside the rule's scope. Only the smallest effective time shift is kept, so a "+1" copy is never the live match. They are ordered by `store::copies::rank_channels` (B5) with the rule's pin and quality cap. Alternates are the next 3. Start and stop include the chosen channel's effective shift.
- **Airings** (per rule and event key):

  | Setting | Kept |
  |---|---|
  | `LiveOnly` | live-marked airings; if none is marked, the first airing |
  | `LiveOrNew` | live or new; otherwise the first airing |
  | `NewOnly` | first airings only, so a ledger row that already aired makes it "shown before" |
  | `Any` | everything |

  Highlights are always left out of spoiler-safe rules.
- **Days and hours** (`spec.days`, `spec.between`) use the local time of the displayed start.
- **Ledger matching:**
  - the same key within ±36 h;
  - otherwise the same channel within 10 minutes;
  - only rows that haven't aired, or `gone` rows, which revive.
- **States:**
  - `skipped` stays skipped;
  - `adopted` stays adopted while its start is unchanged;
  - a list rule gives `listed`;
  - the weekly cap is per ISO week and counts aired rows of that week;
  - the global `max_pending` cap applies;
  - an event that has already started gets no new reminder.
- **Switches** (mode switch or flash):
  - a locked target while a PIN is set becomes notify with `note=locked`;
  - clashes are resolved in this order: your own or Claude's pending switches, then rule priority, then the earlier start. The loser becomes notify with `note=clash:<winner>`, where `clash:0` means your own switch won.
- **Gone.** An unclaimed future row becomes `gone` only while its guide still covers its start (`epg_source.max_stop`), or when its rule is paused or deleted. Its owned reminder is cancelled.

## Deviations from the design

- **No description-hash event key.** Generic sports descriptions would fold every week's game into one event. A show without season, episode or episode title gets one key per day.
- **`PreviewService::start` takes the `SharedConfig`** (pin state, `max_pending`).
- **The library is loaded per evaluation.** A loaded copy replaces the published snapshot until JobContext carries `AppShared`.

## Performance (`cargo bench --bench follow`, this machine)

The dataset is 760,466 programmes on 9,300 channels (454k still to come), with 100 rules × 10 terms.

| Measurement | Result | Gate |
|---|---|---|
| Full evaluation | 0.80 s | < 1.5 s |
| Single-rule preview | 0.51 s | < 1 s |
| Cancel | 16 ms | < 50 ms |

Without the ASCII prefilter the same run took 2.3 s and 1.6 s.
