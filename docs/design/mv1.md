# MV-1: the second picture (engine)

MV-1 is the multi-view engine (DESIGN §35, design `multiview.md` §4). It runs headless, and nothing changes in single view: every user-visible piece is MV-2's.

The spike's measurements are in `multiview-spike.md`.

## What exists now

**Picture B.**
- `Player::start_tile(cfg, deps, TileId::B, budget)` builds a second full player:
  - its own mpv, created with `mute=yes`, a 64 MiB forward demuxer cache cap and an 8 MiB back cap;
  - event pump `mpv-events-b` and controller `player-b`;
  - its own render clock and liveness flag.
- B's log lines read `player B:` and `health B:`.
- B's events reach the app as `AppEvent::Tile(TileEvent::Player(B, ev))`. Single-view code keeps seeing only A's events (`AppEvent::Player`), so no screen reacts to B's `Ended` or `NowPlaying` by mistake.

**Lazy start.**
1. `Services::start_tile_b()` reserves a `TileSlot` and starts B on the blocking pool.
2. B then sends `AppEvent::Tile(TileEvent::Ready(B))`, or `Failed(B, why)` if it couldn't start.
3. On that event, the app calls `Services::adopt_tile_b()`. This adds B to the `TileRegistry` and the memory thread.
4. The app then makes B's surface with GL current: `SurfaceFactory::make(tiles.b().render_core())`. The `SurfaceFactory` is saved from the `CreationContext` at startup (`SurfaceFactory::new(cc)`).
5. B stays alive until exit. `Tiles::shutdown` stops B first, then A. Both surfaces must be destroyed before that.

**The `Tiles` façade** (`player/tiles.rs`):

| Call | What it does |
|---|---|
| `focused()`, `set_focus(t)` | The focused picture (keys go here). Focus never moves the sound. |
| `audible()`, `audible_handle()` | The picture with the sound. |
| `move_sound(t)` | Mutes the old picture, then gives `t` the listening volume and the user's mute, then `SetAudible(true)`. There is one listening volume. |
| `rearrange(mover, to, mover_cmd, target, target_cmd)` | The planner's "move picture A to its TV 2 copy so B can use TV". Call it **only after the user agrees**. The mover's claim moves at once. The target's load then waits up to 3 s for the mover's old stream to close instead of being refused, and the shared race window retries a provider that still counts the old stream. |
| `budget()` | The shared `SharedBudget`. |
| `registry()` | Every picture's handle, for routers off the UI thread. |
| `player(t)`, `handles()`, `main_watch()` | As before. |

**The shared connection budget** (`player/budget.rs`, `SharedBudget`):
- **Limits.** Known limits come from the stored account info. A limit can also be **observed**: when a second stream gets a 403 on an unknown-limit playlist that another picture holds, and the race window doesn't explain the 403, the budget learns "one stream at a time" for the session.
- **Claims.** A claim is set on play or zap and kept through Reconnecting. It is cleared on stop, end or final error, and moved on a retarget.
- **Leases.** A lease is released only on the stream's END_FILE.
- **The race window.** The `released` record is shared, so a picture starting on a playlist the other picture left moments ago retries its 403s.
- A tile may open a stream when the *other* tiles holding the playlist, plus its own open streams, stay under the limit. Nothing ever exceeds a known or observed limit.
- `usage()` returns `StreamUse{source, limit, in_use}`, for the planner and MCP `status.streams`.
- `learned(source)` drives the error card's "This playlist seems to allow one stream at a time".

**Controller, per picture:**
- **Claims.** The controller keeps its claim in step with the state machine, and re-writes it after every play command.
- **Sound.** `SetAudible`, `SetMute` and `ToggleMute` set `mute = user mute || !audible`. The snapshot keeps the user's mute and volume, plus `audible`, `tile`, `claim` and `zap_anchor`.
- **Zapping past the other picture's playlist.**
  - When the next channel's playlist is held by the other picture, the first eligible copy plays instead. An eligible copy has the same explicit flag, is not locked out and is not failing. `zap_anchor` keeps the zap-list position.
  - With no eligible copy, the channel is skipped and announced once with `PlayerEvent::ZapSkipped{channel, source, holder}` (the banner's "BBC One skipped — TV is in use by picture A").
  - `ZapTo` also takes the copy.
  - Single view never runs this lookup.
- **Copy fallback for live.** `CopyPlay.fallback` now also works for live channels, but only for a playlist refusal: our budget, or a 403 outside the race window. Before, it was VOD only. This is how "B plays its TV 2 copy" happens without a second request.
- **Light decoding.** `SetLightDecode(bool)` adds `vd-lavc-skiploopfilter=all` to the next loads. The spike says to ship it off.
- **Suspension.** The hidden-window suspension runs per picture. Each surface stamps only its own clock, so an unplaced or unrendered picture suspends after 10 s while a rendered one keeps decoding.

**MPRIS** (`platform/mpris.rs`, `TileRouter`):
- Focus and sound come from `AppShared::tiles()`.
- Next, Previous and Seek go to the focused picture. Play, Pause and Stop go to every picture. With two pictures, PlayPause sets one state on both.
- Volume goes to the picture with the sound.
- MPRIS shows the focused picture only. A task follows its snapshots and switches when the focus moves.
- While Up next drives the picture (`origin == Queue`), Next and Previous go to the `QueueHandle` through B1. In multi-view they wait.
- `xesam:title` goes through the guard. A hidden title becomes the channel name for live TV, or "Tuner".

**Memory** (`platform/memory.rs`):
- `watch_player(rt, tile, rx)` watches each picture.
- A picture that stops while the other plays triggers the trim "picture closed".

**Services:**
- One budget for every picture.
- MPRIS starts after Up next, so the router gets the `QueueHandle`.
- `apply_config` forwards `ApplyConfig` to the pictures the app's `send_player` doesn't reach (the app sends it to the focused one).
- `DbResolver` answers `channel_source` and `channel_copies` (B5 `store::copies::channel_copies` over the published library).

## Tests (what each guards)

- **`budget::tests`**
  - A reconnecting picture keeps its playlist.
  - Claims follow retargets, and open streams count; a closing stream is reported as `closing`.
  - An unknown limit is learned only from a refusal while another picture holds the playlist.
  - Closes are shared between pictures (the race window).
  - A shut-down tile holds nothing.
- **`player::tests::a_second_picture_on_a_busy_playlist_plays_the_copy_elsewhere`** (real libmpv)
  - B refused on TV plays its TV 2 copy, with `FellBack` 1→2, and A is untouched.
  - B's events never reach `AppEvent::Player`.
  - Without a copy, the refusal is B's own error.
  - After consent, `rearrange` lets both play, one stream per playlist.
- **`player::tests::zapping_a_picture_past_the_other_pictures_playlist`**
  - A zap plays the copy and sets `zap_anchor`.
  - The next zap continues from the anchor.
  - A channel with no copy is skipped and announced once.
- **`player::tests::the_sound_moves_between_pictures_with_the_listening_volume`**
  - B is silent from creation.
  - Moving the sound carries the listening volume.
  - The user's mute survives two moves.
- **`player::tests::video_is_suspended_while_nothing_renders_and_resumed_on_request`** (extended): only the picture that stopped rendering suspends.
- **`player::mpv::tests::a_second_picture_starts_muted_with_small_caches`**: B starts muted, with its cache caps kept through `ApplyConfig`.
- **`player::state::tests::no_fallback_after_the_first_frame_for_live_or_unarmed`** (extended): live falls back on a budget refusal, but not on a 403 that the race window explains.
- **`platform::mpris::tests::the_router_follows_focus_and_sound`**
  - Routing by focus and sound.
  - The queue's Next, and that it waits in multi-view.
  - The MPRIS view follows a focus change and B's own snapshots.
- **`platform::memory::tests`**: "picture closed", and the per-tile watchers.
- **`player::tests::spike_two_uhd_hevc_decodes`** (ignored): the measurement spike.

## Left to other tracks

- **MV-2 (`app.rs`, `ui/video_host.rs`, `ui/multiview.rs`):**
  - On the first multi-view request, call `start_tile_b`, and on `TileEvent::Ready` call `adopt_tile_b`.
  - Make B's surface with a `SurfaceFactory` saved in `App::new`.
  - Destroy both surfaces in `on_exit`.
  - Handle `TileEvent::Player(B, …)`: the `FellBack` toast, `Failed`, and `ZapSkipped` for the banner.
  - Send volume and mute to `tiles.audible_handle()`, and move the sound with `tiles.move_sound`.
  - Use `rearrange` after the confirm card.
  - Publish `TilesState` to `AppShared` (the MPRIS router reads it).
  - Send `ResumeVideo` to each suspended picture that was placed.
- **agent:** `AgentHandler` is still bound to tile A (`services.rs` passes `tiles.handle(A)`). For multi-view it should route through `Tiles::registry()` + `AppShared::tiles()`, as the MPRIS router does. Its `status.streams` comes from `tiles.budget().usage()`.
- **Integrator (`config.rs`):** set the default of `player.multiview.light_decode_small` to `false` (see the spike).
