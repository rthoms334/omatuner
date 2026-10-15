# Multi-view design for Tuner (2-up, PiP; 2×2 later)

> The design as approved on 2026-10-11, kept for its user stories, mockups and numbered rules (code comments cite them as "DESIGN §35 §x.y"). The answers to its open questions and what was built differently are in DESIGN.md §35 (Multi-view).

Based on `origin/main` at 013f897. It builds on the work still in flight:
- P3 copies, which adds Live "Also on"
- `feat/pane-arrows`
- the MCP second pass: reminders, sleep timer and the 6 skills. MCP is DESIGN §30, so this proposes **§31 Multi-view** (or the next free number when it merges).

---

## 0. Decisions made here (not open questions)

1. **At most 2 pictures in v1. Each picture is live TV or catch-up.**
   - Movies and episodes stay single-view, because of resume, the next-episode card and progress prompts. Trying one gives the toast "Multi-view is for live TV and catch-up".
   - 2×2 uses the same engine later (§4.7). With the user's two accounts (one stream each), 4 pictures can't be fed anyway.
2. **No SQL migration. The second picture is not restored at startup**, because that would open two streams nobody asked for. Settings live in `config.toml`.
3. **Pictures are named A and B.** The names stay fixed through swaps and layout changes. Logs, MCP and the key help all use these names. On screen each picture has a small corner letter.
4. **Focus and sound are separate.**
   - Focus is where keys go: zap, channel list, pause, tracks, seek.
   - Sound is the one picture that is audible.
   - Volume and mute keys always act on the audible picture, because there is only one listening volume.
5. **Esc never closes a picture.** The Esc ladder is unchanged and ends with "leave the player", after which multi-view carries on in the mini player. `q` closes the focused picture. `Ctrl+Shift+S` stops everything.
6. **Plays from outside go to the silent picture and take the sound.** This covers the notification Watch button, `tuner --play` over IPC, MCP `play`, a reminder's auto-switch, and a follow-rule switch. The picture you were listening to stays on screen, and the countdown toast says "Switching picture B to … · Cancel". The sleep timer stops both pictures.
7. **Live preview autoplay is off while multi-view is on.** The Live preview pane shows the focused picture. Enter retunes the focused picture and `Ctrl+M` retunes the other one. Live preview never opens a third stream.

---

## 1. User stories

1. **Two semi-finals, two playlists.** Both semis kick off at 20:00. One is on Sky Sports Main Event and the other on Sky Sports Football. I press `Ctrl+M` on the second channel in Live. Both play side by side with sound on the first. `M` moves the sound when a goal goes in.
2. **Same playlist, automatic copy.** Both channels are in "TV", which allows one stream. Tuner plays the second from its copy on "TV 2" with no question asked. The info strip says "from TV 2 — TV is busy with picture A".
3. **Moving the playing picture.** Only "TV" has the channel I want, and the picture already playing comes from "TV". Tuner offers "Move picture A to its TV 2 copy so B can use TV? (Enter = Move)". It never does this silently.
4. **Zapping the silent picture.** With focus on B, ↑/↓ zap only B. If the next channel's playlist is held by A, B plays that channel's copy on a free playlist. If there is none, B skips the channel and the banner says why.
5. **Background game.** I watch a film-length match in "Big + small" with the second game small and silent. A goal alert from my phone, then `w`, then `W`, and the second game is big.
6. **Half-screen tile.** In a 950 pt tiled half, I get Big + small by default. `t` cycles to PiP.
7. **Pop-out.** `o` pops multi-view out as PiP on every workspace. Keys work there. Pop-in restores the layout I had.
8. **Claude.** "Put both semis on, sound on Arsenal." The skill finds both programmes and calls `multiview_open`. If they can't both play (one playlist, limit 1), it says so and offers catch-up of the other from the start.
9. **Parental.** A child can't get a locked channel into picture B without the PIN. Explicit channels are never offered, even as an automatic copy. Relocking stops only the picture that shows locked or explicit content.
10. **Hidden workspace.** With both pictures playing, I switch workspace. Sound continues and both decoders stop after 10 s. Both resume when I come back.

---

## 2. UX

### 2.1 Layouts

| Layout | Geometry (inside the video area) | Default when |
|---|---|---|
| **Side by side** | Two equal 16:9 tiles centred vertically. The spare height holds a permanent **info strip** under each tile: logo, number, name, `● playlist`, programme, progress. | Wide (≥ 1200 pt × rem) |
| **Big + small** | Focused tile at about 2/3 of the width. The other is 1/3 wide at the top right, with its info strip under it. | Medium (700–1199) |
| **Picture in picture** | One tile fills the area. The inset is 25 % of the width, in the chosen corner (default bottom-right) with a 12 pt × rem margin. | Compact, pop-out, mini player |

- `t` cycles the three layouts. The choice is saved as `player.multiview.layout`. `auto` means the defaults above.
- `W` swaps the two places. In Big + small and PiP, that swaps which picture is big. No stream reloads.

### 2.2 Full window, 1600 pt (Wide, side by side; the player has no sidebar)

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│ 󰁍  Multi-view                                              󰕾 A · Sky Sports Main Event   20:26 │ ← top bar (banner)
│                                                                                                  │
│ ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓ ┌──────────────────────────────────────────────┐ │
│ ┃A 󰕾                                  󰕾  ⇄  ✕ ┃ │B                                             │ │
│ ┃                                              ┃ │                                              │ │
│ ┃          VIDEO A  (focused, audible)         ┃ │            VIDEO B  (muted)                  │ │
│ ┃               792 × 446 pt                   ┃ │                                              │ │
│ ┃                                              ┃ │       ┌ Reconnecting… attempt 2 · 1 s ┐      │ │
│ ┃                                              ┃ │       └ r Retry now  ·  q Close picture ┘    │ │
│ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ └──────────────────────────────────────────────┘ │
│ ▣ 401 Sky Sports Main Event   ● TV     [UHD]    ▣ 406 Sky Sports Football   ● TV 2  [HD]          │
│ Semi-final 1   19:45–22:00   ▕██████░░░░░▏ 1h34  Semi-final 2   20:00–22:15   ▕████░░░░░▏ 1h49    │ ← info strips
│ Next 22:00 Post-match                            Next 22:15 Highlights                            │
│                                                                                                  │
│ 󰒮 󰐊 󰒭  󰗅 English  󰨖 Off  󰐑 Channels  󰃰 Guide  󰋚 History │ ▦ Layout  󰕾 Sound here  ⇄ Swap  ✕ Close │ 󰕾 ━━━●─┊─ 80 % │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
 status hint (when the banner is hidden it fades): w next picture · M sound here · W swap · t layout · q close picture
```

- The focused tile has a 2 pt `osd_accent` border. It drops to 40 % alpha when the banner hides, but stays visible because keys act on it.
- The corner letter shows 󰕾 on the audible tile.
- Hover chrome (top right of each tile, OSD colours): 󰕾 "Sound from this picture (M)", ⇄ "Swap places (W)", ✕ "Close this picture (q)".
- The bottom controls row belongs to the **focused** picture, plus a multi-view group: Layout, Sound here, Swap, Close.
- At the end of the row, the volume belongs to the **audible** picture.
- Side panels (`c`, `g`, `h`) open over the right edge. Their header reads "Channels — picture B".

### 2.3 Half screen, about 950 pt (Medium, Big + small)

```
┌──────────────────────────────────────────────────────────────┐
│ 󰁍 A · 401 Sky Sports Main Event · ● TV               20:26 │
│ ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓ ┌────────────────┐ │
│ ┃A 󰕾                                     ┃ │B               │ │
│ ┃                                        ┃ │   VIDEO B      │ │
│ ┃        VIDEO A (focused, audible)      ┃ │  300 × 169 pt  │ │
│ ┃            612 × 344 pt                ┃ └────────────────┘ │
│ ┃                                        ┃ 406 Sky Sp. Football│
│ ┃                                        ┃ ● TV 2  Semi-final 2│
│ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ ▕████░░░░▏ 1h49    │
│ Semi-final 1  19:45–22:00 ▕██████░░░░░▏ 1h34                  │
│ 󰒮 󰐊 󰒭 󰗅 󰨖 󰐑 󰋽 │ ▦ 󰕾 ⇄ ✕ │ 󰕾 ━━●─ 80 %                         │ ← below 700 pt: glyph-only rule (§13.8)
└──────────────────────────────────────────────────────────────┘
```

**PiP at 950 pt** (after `t`). The inset is 237 × 133 pt. The inset's info shows only in its banner for 3.5 s after a zap or focus change.

```
┌──────────────────────────────────────────────────────────────┐
│ 󰁍 A · 401 Sky Sports Main Event · ● TV               20:26 │
│ ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓ │
│ ┃A 󰕾                                                        ┃ │
│ ┃                 VIDEO A                                  ┃ │
│ ┃                                      ┌─────────────────┐ ┃ │
│ ┃                                      │B  VIDEO B       │ ┃ │
│ ┃                                      │ 406 · Semi 2    │ ┃ │
│ ┃                                      └─────────────────┘ ┃ │
│ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ │
│ Semi-final 1 ▕██████░░░░░▏ 1h34 · 󰒮 󰐊 󰒭 󰐑 ▦ 󰕾 ⇄ ✕  󰕾 ━━●─     │
└──────────────────────────────────────────────────────────────┘
```

### 2.4 Live TV with multi-view on (1600 pt, Wide)

```
┌side┬ groups ───────────┬ channels ─────────────────────────────────┬ preview ─────────────────────────────┐
│ 󰋜  │ ★ Favourites      │ 401 Sky Sports Main Event   ● TV   [UHD] A│ [A 󰕾 Sky Sp Main Ev] [B Sky Sp Footb] │ ← picture chips (click = focus)
│ 󰔂▸ │▸UK Sports         │▸402 Sky Sports Premier Lge  ● TV   [FHD]  │ ┌──────────────────────────────────┐ │
│ 󰃰  │ UK Entertainment  │ 406 Sky Sports Football     ● TV 2 [HD]  B│ │ VIDEO A (the focused picture)    │ │
│ 󰎁  │ UK News           │ 408 Sky Sports F1           ● TV          │ └──────────────────────────────────┘ │
│    │                   │                                            │ 402 Sky Sports Premier League        │
│    │                   │                                            │ ↵ plays in A  ·  Ctrl+M plays in B   │
│    │                   │                                            │ ⓘ B would play it from TV 2 (TV is   │
│    │                   │                                            │   busy with picture A)               │
├────┴───────────────────┴────────────────────────────────────────────┴──────────────────────────────────────┤
│ ↵ play in A · Ctrl+M play in B · ⇧↵ fullscreen · Ctrl+F fav · ? help                    Guide updated 2 h ago│
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- Channel rows that are playing carry `A` or `B` where the "playing" glyph goes today.
- The note under the preview is computed for the cursor row by the planner (§4.4). It reads "B would play it from TV 2", "Needs picture A to move to TV 2", or "TV allows one stream: close A or pick another channel".
- **Mini player** (when you leave the player): PiP of the two. The focused picture is big. The inset is 1/3 of the width at the top right, outlined. A click opens the player and ✕ stops both.
- **Compact:** the status chip reads "▶ 2 pictures · Sky Sports Main Event".

### 2.5 Keyboard

There is a new context, **`KeyContext::MultiView`** ("Player — multi-view"). It sits directly above `PlayerLive` or `PlayerVod`, whichever matches the **focused** picture's kind. Possible stacks:
- `[MultiView, PlayerLive]`
- `[PlayerOsd, MultiView, PlayerLive]`
- `[List|ListFiltering, MultiView, PlayerLive]` (side panel)
- `[NumericEntry, MultiView, PlayerLive]`
- `[PopOut, MultiView, PlayerLive]`

Unbound keys fall through, as with `PlayerOsd`. Inside side panels, letters still filter, because `List` is innermost and filterable.

I checked these keys as free in `src/keymap.rs` on main and in the `feat/pane-arrows` and `feat/mcp` diffs.

**`MultiView` table**

| Keys | Action | Notes |
|---|---|---|
| `w` | Next picture (focus) | Vim's window key. Free in `PLAYER_COMMON`, `PLAYER_LIVE` and `PLAYER_VOD`. |
| `Ctrl+←` / `Ctrl+→` / `Ctrl+↑` / `Ctrl+↓` | Focus the picture in that direction | Spatial, matching the pane-arrows idea. Plain ←/→ skip in both live and VOD; H opens history and Backspace recalls the last channel. |
| `M` | Sound from this picture | Paired with `m` (mute). |
| `W` | Swap the pictures' places | |
| `t` | Layout: side by side → big + small → picture in picture | |
| `q` | Close this picture (the other one carries on) | Shadows `PLAYER_COMMON`'s `q` only in multi-view. |
| `Ctrl+M` | Choose the channel for the other picture… | Opens the `c` panel aimed at the other picture. |

**Existing keys, with routing**

| Key | Goes to |
|---|---|
| ↑ ↓ j k PgUp PgDn, digits, Backspace/←, →/h, `c`, `g`, Space, `a` `A` `s` `S` `z` `x` `Z` `{` `}` `v` `V` `I` `r`, `Ctrl+F`, `Ctrl+O`, `L`, `Home`, VOD seeks | the **focused** picture |
| `m`, `+` `=` `-`, wheel | the **audible** picture |
| `p` | leave the player (multi-view continues in the mini player) |
| `o` / `Ctrl+Shift+O` | pop out (forces PiP; §4.8) |
| `f` / F11 | window fullscreen |
| `Ctrl+Shift+P` | pause or play **both** |
| `Ctrl+Shift+S` | stop **both** |
| Esc | unchanged ladder: leave controls → hide banner → leave fullscreen → leave player |

**Entry points outside the player.** The verb is `Ctrl+M` everywhere, and the help text is "Watch in the other picture (multi-view)":
- `ITEM_ACTIONS` (Live lists, Home and grid channel tiles), `GUIDE` and `SEARCH_ITEM_ACTIONS`.
- With nothing playing it simply plays.
- With one picture playing it opens multi-view, with the new channel as B, focus unchanged and sound staying on A.
- With multi-view on it retunes the **unfocused** picture. Enter keeps retuning the focused one.
- On a past programme in the guide or search, it opens catch-up in the other picture.
- On a movie or episode it gives the toast "Multi-view is for live TV and catch-up".
- `PLAYER_COMMON` gets `Ctrl+M` = "Watch something alongside…", which opens the channel panel aimed at the new picture B.

Why `Ctrl+M`:
- Lists are filterable, so only Ctrl/Alt chords are possible there.
- It isn't bound in any table and isn't an egui default (unlike Ctrl+0 / Ctrl+±).
- Omarchy owns Super, and fcitx owns Ctrl+Space.
- It must not be `Global`, because TextInput lets Global chords through.

**Hint line in the multi-view player:** `w next picture · M sound here · W swap · t layout · q close picture`. Help and §15.2 are regenerated with `examples/keymap_md`.

### 2.6 Mouse

| Gesture | Effect |
|---|---|
| Click a tile | Focus it. It also closes an open side panel or leaves controls focus, as the click does today. It never changes sound. |
| Double-click a tile | Window fullscreen on/off, as today. In the popped-out window: pop back in, as today. |
| Drag a tile onto the other | Swap places. The drop target lights up. Needs a 6 pt threshold, so a click is never a drag. |
| Drag the PiP inset | Snaps to the nearest corner. That corner is saved as `pip_corner`. Dropping on the big tile's centre third swaps. |
| Hover a tile | Chrome: 󰕾 sound here, ⇄ swap, ✕ close. The pointing hand and tooltip with the key show, as for OSD buttons. |
| Wheel over video | Volume of the audible picture. |
| Right-click a tile | Player menu with a multi-view section on top: Sound from this picture, Swap places, Layout ▸ (3 options), Close this picture, Close the other picture, then the usual items for **that** tile. |
| Live preview chips `[A …] [B …]` | Click = focus that picture, so Enter targets it. |
| Mini player | Click = open the player. ✕ = stop both. The pop-out button is unchanged. |
| Popped-out window | Drag still moves the window, so there is no drag-swap there. A click focuses. Swap and sound use the hover chrome. |

### 2.7 OSD per tile

- **Per-tile state overlays.** `VideoHost::paint_state_overlay` already scales by rect: Loading, Buffering %, Reconnecting "attempt n" with countdown, Error, Radio.
  - Recovery buttons are drawn inside the tile when the tile is at least 360 pt wide. Otherwise the text reads "r Retry · q Close", and `r` acts on the focused tile.
- **Per-tile banner** (3.5 s after a zap or focus change): logo, number, name, `● playlist`, programme, progress. In Side by side this is the permanent info strip. In the other layouts it overlays the tile's bottom edge.
- **The global top bar** shows `󰕾 A · <audible channel>` and the clock, once, never per tile.
- **Spoilers:** strips and banners go through the same spoiler-guard helper as the single OSD (sibling design). Multi-view adds no new place that shows programme text.

---

## 3. Data model

### 3.1 Migrations

**None.**
- Channel copies come from tables that already exist: `channel_epg` → `epg_channel.xmltv_id`, with a fallback to `channel.norm_name` via the `channel_norm` index.
- History, progress and failures are written per picture through the existing `PlayerStore`.

If P3's Live "Also on" lands first, multi-view uses its query. If not, MV-1 adds it with the semantics in §4.4, and "Also on" reuses it.

### 3.2 Config (`config.toml`)

```toml
[player.multiview]
layout = "auto"              # auto | side-by-side | big-small | pip
pip_corner = "bottom-right"  # top-left | top-right | bottom-left | bottom-right
light_decode_small = true    # §4.6: lighter decoding for the small picture, set when it loads
```

Settings ▸ Player gets a "Multi-view" block with these 3 controls and one line of explanation about connection limits.

### 3.3 Runtime types

```rust
// player/mod.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId(pub u8);                  // A = 0 (the player that exists today), B = 1
impl TileId { pub const A: Self = Self(0); pub const B: Self = Self(1); pub fn letter(self) -> char }

PlayerCommand::SetAudible(bool)             // effective mute = user mute || !audible
PlayRequest { …, zap_anchor: Option<ChannelId> }   // copy played in place of a zap-list channel
PlayOrigin::MultiView                        // new variant (autoplay never replaces it)
PlayerSnapshot { …, tile: TileId, audible: bool, claim: Option<SourceId>, zap_anchor: Option<ChannelId> }
MediaResolver::channel_copies(&mut self, ch: ChannelId) -> Vec<ChannelCopy>   // default: empty

// player/budget.rs — shared by every tile's controller
#[derive(Clone)] pub struct SharedBudget(Arc<Mutex<BudgetState>>);
struct BudgetState {
    limits: HashMap<SourceId, u32>,
    open: HashMap<(TileId, SourceId), u32>,        // leases (stream not yet END_FILE'd)
    claims: HashMap<TileId, SourceId>,             // source a tile is targeting, kept through Reconnecting
    released: HashMap<SourceId, Released>,         // race window, moved here from the controller (§11.4)
    observed: HashMap<SourceId, u32>,              // a second stream got 403 on an "unknown" limit: 1 for the session
}
// acquire(tile, s) is allowed iff |{t ≠ tile : claims[t] == s or open[(t, s)] > 0}| < limit(s)

// store/live.rs (or P3's)
pub struct ChannelCopy { pub id: ChannelId, pub source: SourceId, pub quality: Option<Quality>, pub locked: bool, pub explicit: bool }
pub fn channel_copies(conn: &Connection, ch: ChannelId) -> Result<Vec<ChannelCopy>>

// ui/multiview.rs (UI-owned, not persisted except layout / pip_corner)
pub struct MultiView { pub on: bool, pub layout: Layout, pub slots: [TileId; 2], pub focus: TileId, pub audible: TileId, pub pip_corner: Corner }
pub enum Layout { SideBySide, BigSmall, Pip }          // Grid2x2 later
pub fn geometry(layout, area: Rect, slots, corner, rem) -> ArrayVec<(TileId, Rect, InfoStrip?), 2>   // pure
pub fn plan(req: &PlaceRequest, limits: &LimitsView) -> Result<PlacementPlan, Unplaceable>          // pure, §4.4
```

---

## 4. Engine and architecture

### 4.1 Threads

```
UI thread (GL) ── VideoHost{ surfaces: [A, B?] } ── renders each placed surface (A also into the sink when nothing is placed)
   │  PlayerCommand per tile (Tiles::handle(t)) · focus/sound are UI state · ctx.play() → focused tile
   ▼
"player"   controller A ─┐                               "player-b"  controller B ─┐
"mpv-events"   pump A    ├─ SharedBudget (Arc<Mutex>) ─┤ "mpv-events-b" pump B    │
libmpv A (own threads)  ─┘   limits·claims·leases·released └ libmpv B (own threads) ┘
        │ watch<Arc<PlayerSnapshot>> ×2 ──▶ MPRIS focus mux (tokio task) ──▶ org.mpris.MediaPlayer2.tuner
        │                                ──▶ tuner-mem (watch_player ×2 → malloc_trim)
        ▼ PlayerStore → db-writer (history / failures / progress from both)
```

- **Picture B is a second `Player`:** a full controller, event pump and mpv, created by `Player::start_tile(cfg, deps, TileId::B, budget)`. The whole tested single-player stack runs twice unchanged: state machine, reconnect policy, stop-before-start, the provider-message trap, logging and the hidden-window suspension.
- **Lazy start.** The first multi-view request makes `Services::tiles.ensure_b()` run `Player::start_tile` on the blocking pool. mpv init takes about 30–80 ms and must not run on the UI thread. It answers with `AppEvent::TileReady(Player)`.
  - In the next `App::logic` (GL current), `VideoHost` creates B's surface with `VideoSurface::from_parts(core, gpa, wl_display, egui_ctx)`. The `GetProcAddress` and the Wayland display are saved from `CreationContext` at startup.
  - Until then tile B shows "Starting…" with the logo.
- **B stays alive until exit.** After `Stop` and a trim, it holds about 25 MiB. Recreating it would bring back the risky teardown order (render ctx with GL current → FBO → mpv) at runtime for no real gain.
- **Exit order:** `on_exit` destroys **both** surfaces with GL current, then `Services` shuts down both players. B's `render_alive` flag gives B the same "leak, never UB" safety net that A already has.
- **Two render contexts on one GL context.** mpv supports one render context per mpv handle.
  - `VideoSurface::render` already restores the GL state egui needs, and runs per surface.
  - Each surface renders only when its own update callback flagged a new frame.
  - Each surface stamps only its own `RenderClock`.

### 4.2 Where it hooks in

| Area | Change |
|---|---|
| `Services` | `player: Player` becomes `tiles: Tiles { a: Player, b: Option<Player>, budget: SharedBudget }`. `SetConnectionLimit` goes to `budget` directly (startup and after refresh). `ApplyConfig` goes to both. Shutdown covers both. `memory.watch_player` runs per tile. |
| `UiCtx::play` / `App::send_player` | Go to `tiles.handle(mv.focus)`. Volume and mute go to `tiles.handle(mv.audible)`. A new `UiCtx::play_in(tile, cmd)` and `UiCtx::multiview()` give a read-only view. `UiCtx.player` stays the **focused** snapshot, so `player.rs`, `live.rs` and `guide.rs` keep working unchanged in single view. |
| `App::logic` | Reads both snapshots. ResumeVideo goes to each `video_suspended` tile **that was placed last frame**. `player_busy` = any tile Loading or Zapping. Idle inhibit = any tile active and not paused. `measured.observe` runs for both. Relock (`sync_explicit`) stops each tile showing explicit or locked content, and multi-view collapses if one tile is left. |
| Key stack | `key_stack()` inserts `MultiView` above `PlayerLive`/`PlayerVod` when `mv.on`, and `PopOut` above that when video-only. |
| `VideoHost` | `surfaces: [Option<VideoSurface>; 2]`, plus per-tile `placed`, `last_generation` and logo. `paint_tile(ui, rect, tile, …)`. `render()` renders placed tiles. When nothing is placed, only the focused tile goes into the 640×360 sink. `mini_player` draws the PiP composition. |
| Controller | Owns its `TileId` and `SharedBudget`. Sets its claim on Play/Zap/ZapTo and keeps it through Reconnecting. Clears it on Stop, Ended, final Error, or a retarget to another source. Moves `released` into the shared budget. `SetAudible` and effective mute. Zap substitution (§4.4). Log prefix `player B:`. Health line per tile. |
| MPRIS | `Commands` routes through a `TileRouter` (focus held in an `AtomicU8` set by the app). The snapshot stream is a tokio task that republishes the focused tile's `watch` and switches on focus change. Next/Previous/Seek go to the focused tile. Play/Pause/PlayPause/Stop go to both. Volume goes to the audible tile. It works while hidden, with no UI involved. |
| `platform::memory` | `player_trigger` gains "picture closed" (a tile went Idle while the other plays). It already fires on stop, video off and reload per tile. |

### 4.3 Sound

- B's mpv is created with `mute=yes`, so it is never audible before the app decides.
- The controller computes `mute = user_muted || !audible` and keeps the user's mute and volume in the snapshot.
- **Swapping sound:** `SetAudible(false)` to the old picture, then `SetAudible(true)` with the listening volume to the new one. It is instant, with no track reselection. The muted picture still decodes audio, which costs about 1 % of a core for AAC/AC-3 and keeps the swap gapless.
- **One listening volume:** `+`/`-`/`m` go to the audible tile. On a swap the new picture gets the same level.
- PipeWire sees two streams named `tuner`, which is fine.

### 4.4 Connection budget and automatic copy placement

The user's setup is "TV" and "TV 2" with `max_connections=1` each. **Two pictures must come from two playlists.**

**Channel copies (`channel_copies`, shared with P3 "Also on"):**
- Channels on *other* playlists whose resolved EPG channel has the same `xmltv_id` (case-insensitive) count as copies.
- Otherwise, channels with the same `norm_name` after quality tokens are dropped (`name_tokens`) count as copies.
- There is no fuzzy matching. "BBC One London" and "BBC One Scotland" stay different.
- Copies hidden by `Visibility` (explicit while hidden) are never returned. Locked copies come back flagged.
- Cost: about 6 index lookups (`channel_epg` PK → `epg_channel` → `epg_channel_nocase` → `channel_epg_rev`, plus `channel_norm` per other source), about 0.2 ms on 79k channels. That fits the UI's 2 ms `FrameBudget` and runs on the controller's own reader.

**The planner (`ui::multiview::plan`, pure):**
- **Input:**
  - each wanted picture: the user's pick plus its copies
  - the picture that keeps playing, with its current copy and its copies
  - limits and current use from a `SharedBudget` snapshot
  - each wanted picture's target pixel height
- **Search:** brute force over copy combinations (≤ 8 × 8). It picks the lowest cost:
  1. moving a playing picture: +100 (only with consent)
  2. not the user's pick: +10
  3. a copy that `skip_on_zap` marks (≥ 2 failures in 24 h): +20
  4. a locked copy while locked, when the pick wasn't locked: excluded
  5. a copy whose explicit flag differs from the pick's: excluded (never escalate)
  6. **quality fit:** for a picture smaller than 1080 physical px tall, prefer the lowest quality ≥ its height. Example: a 300 pt inset takes the HD copy over UHD, which saves most of a core in software HEVC. Otherwise prefer the best.
- **Output:** `PlacementPlan { per tile: (ChannelId, from_copy: bool), moves: Option<TileId> }` or `Unplaceable { reason }`. Reasons are plain English and name playlists only, never addresses: "Both are only on TV, which allows one stream at a time." / "TV is held by picture B (reconnecting)."
- `moves.is_some()` → the confirm card "Move picture A to its TV 2 copy so B can use TV? · Enter Move · Esc Cancel". The card warns that it can take up to 30 s while TV frees up (the race window in §11.4).
- The play is sent as `PlayCopy { req, fallback, elsewhere }`. `elsewhere` holds the next placement-valid copy on another free playlist, so a 403 or `Budget` before the first frame falls back once. This uses the existing `CopyPlay::fallback_for`.

**Inside the controllers:**
- `SharedBudget::acquire(tile, s)` counts the *other* tile's claim and open leases. So a picture that is reconnecting keeps its playlist, and the other picture can't take it in between attempts.
- Stop-before-start within a tile is unchanged.
- `released` (race window) is shared. When B starts on a playlist A left less than 45 s ago, B's 403s are retried as race refusals, not counted as final.
- **Zapping a picture** (keys, MPRIS Next, hidden window) when the next channel's source is blocked for this tile:
  - `resolver.channel_copies(ch)` → the first copy on an unblocked, admitted source plays, with `zap_anchor = ch` so the next zap continues from the zap-list position.
  - Otherwise the channel is skipped, like `skip_on_zap`.
  - The banner says "BBC One skipped — TV is in use by picture A".
- **Unknown limit** (M3U or free presets):
  - Both pictures may share the playlist.
  - If the second stream gets a 403 that isn't explained by the race window, `observed[s] = 1` for the session. The error card then reads "This playlist seems to allow one stream at a time" and offers the copy elsewhere.
- **Never exceeded:** no code path opens a lease beyond `limits`/`observed`. Settings' "another device is using this account" warning stays as it is.

### 4.5 Reconnects per tile

Each picture has its own `PlayerMachine` and runs the §11.4 policy independently: unlimited reconnects for a live channel that played, provider-message trap, 30 s steady-state backoff. The shared parts are:
- **Claims:** a down picture keeps its playlist until the user closes or retunes it (`q`, or `r` on the focused tile).
- **The race window:** cross-tile handovers get the 403 grace.

Two pictures on two accounts make at most about 2 × 120 requests per hour during a long outage. That is within the existing "don't hammer the provider" policy.

### 4.6 Performance, CPU and memory

All figures are for the user's M2 under Asahi, with software decoding.

| Item | Cost | Mitigation |
|---|---|---|
| Two 4K HEVC decodes | about 1.7 cores of 8 (0.86 each) | Quality fit (§4.4) puts the small picture on an HD copy when one exists (about 0.25 core). `light_decode_small`: when the small picture *loads*, `vd-lavc-skiploopfilter=all` goes into its `loadfile` options. Swapping places doesn't reload, so it applies from the next zap. Ships only if the MV-1 measurement shows ≥ 15 % saved. |
| Picture not placed (Live shows the focused one, Compact, …) | it isn't rendered, so its clock stops, and the existing 10 s safety net sets `vid=no` | Same mechanism and the same 10 s HEVC keyframe reasoning as §11.1. ResumeVideo when placed again. |
| Hidden workspace | both clocks stop → both `vid=no` after 10 s; the audible picture keeps sound; the muted one keeps only demux and audio decode | Instant return; the `render_unfocused` rule renders both at 15 fps. |
| Memory | idle B: about 25 MiB. A 4K 10-bit HEVC B: about 0.4–0.6 GiB (decoder frame pool plus cache) | B's mpv is created with `demuxer-max-bytes=min(cfg, 64MiB)` and `demuxer-max-back-bytes=8MiB`. `tuner-mem` watches both snapshots: trim on picture closed, video off, or reload, coalesced by the existing SETTLE/MIN_GAP. |
| UI | 2 textures and about 20 rects. Repaints come from both mpv callbacks, paced by frame callbacks (≤ 60 Hz). Per-frame DB: now/next for 2 channels (cached). | — |
| Library scale (79k channels / 302k movies) | Nothing is scanned. Copies are point queries. The planner is ≤ 64 combinations. | — |

### 4.7 2×2 later

`TileId(u8)`, `Tiles` as a small vec, `geometry()` for N, and the planner over N pictures all generalise. They are blocked only by streams: with two accounts at one connection each, more than 2 pictures needs another playlist or free presets. CPU for four 4K streams is about 3.4 cores. The `MultiView` key context gains `Ctrl+↑/↓`, which is already reserved.

### 4.8 Pop-out

`o` (or Super+O, pinned and floating) while multi-view is on:
- The video-only player draws the PiP composition at pop size, with a 160 × 90 inset at Medium.
- The layout is saved, and restored on pop-in.
- The stack is `[PopOut, MultiView, Player*]`: `w`, `M`, `W` and `q` work. Esc and `o` pop in.
- The compact one-strip OSD shows the focused picture, with the 󰕾 letter.
- A popped-out window is always visible, so no suspension happens.

---

## 5. Guardrails

- **Parental:**
  - Every way a picture starts goes through the existing play gate. A locked channel gets the PIN prompt titled "for picture B". Explicit content isn't in the snapshot while hidden, so it can't be picked.
  - Zaps per picture pass `unlocked`. MPRIS always counts as locked.
  - The planner never substitutes a locked copy while locked, and never a copy whose explicit flag differs.
  - Relock stops only the offending picture(s).
  - MCP gets `pin_required` per picture and can never unlock.
- **Explicit privacy:** no titles at INFO. Log lines are `player B: source 2 xt:live:… Playing → …`. Toasts and planner reasons never name concealed content.
- **Connection limits:** shared budget, claims kept through Reconnecting, the race window shared, `observed` limits learned per session, no automatic moving of a playing picture without consent, and no stream probing.
- **Privacy:** nothing new is stored. History shows both channels. MPRIS exposes only the focused picture.

---

## 6. MCP tools and skill changes

These build on the MCP PR's `proto.rs` patterns: `deny_unknown_fields`, refs and `ErrorCode`. They are action calls (`is_action`), so they are rate-limited as `play` is.

**`multiview_open`**
```jsonc
{ "refs": ["programme:1:xt:live:401@2026-10-11T20:00+01:00", "channel:2:xt:live:406"], // 1–2; one ref = add alongside what plays
  "layout": "side_by_side|big_small|pip",   // optional (default: the user's setting)
  "sound": 0,                               // index into refs (default 0; with one ref: sound stays put)
  "from_start": false,                      // programmes with catch-up: start over
  "rearrange": false }                      // allow moving the playing picture to another copy
→ { "outcome": "playing|pin_required|needs_rearrange",
    "tiles": [{ "tile":"a","ref":"…","title":"Sky Sports Main Event","playlist":"TV","sound":true,"focused":true,
                "from_copy_of":"channel:1:xt:live:406?","state":"loading" }, …],
    "message": "Both on: Semi-final 1 (sound) and Semi-final 2 from TV 2." }
```
- **Errors:**
  - `unavailable`: the planner's reason, e.g. "Both are only on TV, which allows one stream at a time."
  - `not_found`: hidden or missing; indistinguishable on purpose.
  - `unsupported`: movies and episodes.
  - `not_started`: a future programme; use `schedule_switch`.

**`multiview_swap`** `{ "what": "sound|places|focus|layout", "layout"?: … }` → `{ tiles }`.

**`multiview_close`** `{ "keep": "a|b|sound" }` (default `"sound"`) → `{ status }`.

**Extensions to in-flight tools:**
- `status` adds `multiview: { on, layout, tiles[…] }` and `streams: [{ playlist, limit, in_use }]`, so the skill can tell whether two pictures are possible before offering.
- `control` adds an optional `tile`. Pause, resume and stop apply to both unless `tile` is given. Volume and mute go to the sound picture.
- `sleep_timer` stops both.
- An auto-switch from `schedule_switch` or reminders follows the "outside play" rule (§0.6), and its `SwitchOut` gets `tile`.

**Skills.** The plugin keeps 6 skills, as the skill plan decided (no 14+ skills). There is no separate sports skill: sports lives in `watch` and `plan`.
- **RULES.md:** replace "multi-view (the app has one player today…)" with this rule: multi-view shows up to 2 pictures of live TV or catch-up. Each playlist carries as many streams as its limit (usually 1), so check `status.streams` before offering. Open it only when the user asked for two things at once, or accepted an offer. On `needs_rearrange`, ask before resending with `rearrange=true`, and say that the playing picture will pause for up to 30 s. Spoiler-safe mode is unchanged; the app's spoiler guard covers the tile strips.
- **`watch`:** add the prompts "put both semis on" and "show X and Y side by side":
  1. `guide_search` each.
  2. `multiview_open([a, b], sound: <the one named first or asked for>)`.
  3. On `unavailable`, offer "play X now, Y from the start on catch-up later (spoiler-safe)".
- **`plan`:** step 8 (clashes) offers multi-view first when `status.streams` shows two free playlists, else falls back to "play one, catch up on the other". Auto-switch reminders mention the silent-picture rule.
- **`fix`:** `diagnose` reports per picture (`tiles[].error`) and names a refused second stream as the playlist's limit.

---

## 7. Tests worth having (real behaviour only)

1. **`budget` unit tests:**
   - B can't acquire TV while A is Reconnecting on TV, and can once A stops.
   - Claims follow retargets.
   - An unknown limit allows both until `observe(1)`.
   - The race window is armed for B by A's close less than 45 s ago, and isn't armed for A's own failed attempts.
2. **Headless two-player test** (real libmpv, `vo=null`, lavfi, as in `player/mod.rs` tests), with the shared budget at limit 1 on source 1:
   - A plays source 1.
   - `PlayCopy(B: source-1 channel, elsewhere: source-2 copy)` → B plays the copy, `FellBack` names 1→2, and A is untouched (still Playing, same `load_generation`).
3. **Sound:**
   - B starts with mpv `mute=yes`.
   - After a swap, A has `mute=yes`, and B has the listening volume with `mute=no`.
   - User mute survives two swaps.
4. **Zap substitution:** with a fake resolver returning copies, B zaps onto a channel whose source A holds → B plays the copy, `zap_anchor` is set, and the next zap continues from the anchor. With no copy, the channel is skipped.
5. **Per-tile suspension** (controller clock test, as in the existing one): only the tile whose clock stops gets `vid=no` after 10 s; `ResumeVideo` brings it back.
6. **Planner table tests on the user's setup:**
   - both semis each on a different playlist
   - both on both playlists
   - both only on TV → `Unplaceable`
   - the move needed is offered, never chosen without consent
   - a locked copy is excluded while locked
   - an explicit copy is never chosen
   - the small picture prefers HD over UHD
   - a copy with recent failures loses
7. **Keymap tests:**
   - The new stacks resolve as specified (`[MultiView, PlayerLive]`, `[List, MultiView, PlayerLive]`, `[PopOut, MultiView, PlayerVod]`).
   - Esc in multi-view resolves to `Back`, not close.
   - `w` in a side panel types into the filter.
   - `Ctrl+M` is in `ITEM_ACTIONS`, `GUIDE` and `SEARCH_ITEM_ACTIONS`, with no duplicates.
8. **App rig tests** (recorded commands):
   - `Ctrl+M` in Live opens B with the planned copy, focus unchanged.
   - Enter retunes the focused picture.
   - `q` closes the focused picture, and the other becomes single view with sound.
   - Relock stops only the explicit or locked picture.
   - An outside play goes to the silent picture and moves the sound.
   - The MPRIS router follows focus (the existing `Commands` channel fake).
9. **MCP:**
   - Wire round trip of the 3 tools.
   - Server guardrails: a hidden ref gives `not_found`; one playlist gives `unavailable` with no URL or login in the message; `needs_rearrange` doesn't touch the player until `rearrange=true`.
10. **Memory:** `player_trigger` fires on "picture closed".
11. **Manual checklist (`docs/MANUAL_CHECKLIST.md`):**
    - Two 4K HEVC streams on the M2: CPU, dropped frames, a 1-hour soak with RSS.
    - 60 s on another workspace with and without `render_unfocused`.
    - Pop-out PiP.
    - Real TV + TV 2 handover through the race window.
    - Exit with two surfaces: exit code 0, no leak line in the log.

---

## 8. Implementation plan

This follows the earlier phases' approach: a skeleton tag `mv-base` (≈150 lines) commits the types and signatures first. That covers `TileId`, `SharedBudget` API, `Tiles`, `ui::multiview::{MultiView, Layout, geometry, plan}` signatures, `KeyContext::MultiView` and `PlayOrigin::MultiView`. After that, the tracks can run in parallel with disjoint file ownership.

| Track | Owns | Size |
|---|---|---|
| **MV-1 engine** | `player/budget.rs` (SharedBudget, claims, released, observed; +200), `player/controller.rs` (tile, shared budget, audible/mute, claim lifecycle, zap substitution, log prefix, per-tile health; +260), `player/mod.rs` (TileId, `start_tile`, `SetAudible`, snapshot fields, headless 2-player tests; +260), `player/mpv.rs` (tile-B cache caps, `light_decode_small` load opts; +40), `player/render.rs` (`VideoSurface::from_parts`; +40), `services.rs` (Tiles, lazy B on the blocking pool, limits to the shared budget, shutdown both; +160), `store/live.rs` `channel_copies` + test (+90, or reuse P3's), `platform/mpris.rs` (TileRouter + snapshot mux + test; +120), `platform/memory.rs` (+25), spike measurement of `light_decode_small` | ≈ 1.2k |
| **MV-2 UI** | `ui/multiview.rs` (state, geometry, planner, chrome, drag; +650 incl. tests), `ui/video_host.rs` (two surfaces, lazy surface, per-tile paint and overlays, PiP mini; +330), `ui/screens/player.rs` (multi-view layout, info strips, routing, panels aimed at a picture, menu; +380), `ui/screens/live.rs` (chips, Ctrl+M, Enter target, cursor-row note; +130), `ui/screens/guide.rs` and `ui/overlays/search.rs` (Ctrl+M; +50), `keymap.rs` (+70), `app.rs` (Tiles routing, logic per tile, key stack, relock, outside-play rule, rig tests; +320), `ui/mod.rs` (+40), `ui/popout.rs` (+30), `config.rs` + `ui/screens/settings.rs` (+110), DESIGN §31 + §15.2 regeneration + checklist (+300) | ≈ 2.4k |
| **MV-3 agent** | `agent/proto.rs`, `agent/tools.rs`, `agent/server.rs`, `agent/mod.rs` (`AgentCommand::MultiView`), status/control/sleep_timer extensions, plugin `RULES.md` + `watch`/`plan`/`fix` skill text | ≈ 0.45k |

**Order:**
1. `mv-base`.
2. MV-1, which can start now: it touches no in-flight file except `services.rs`, and the MCP PR only adds lines there.
3. MV-2 after `feat/pane-arrows` and P3 merge. Both touch `keymap.rs`, lists, `player.rs` and "Also on".
4. MV-3 after the MCP PR merges.

Shipping MV-1 and MV-2 gives the whole feature in the app. MV-3 only adds Claude control.

**Coordination with sibling designs:**
- **Play queue:** advances only in single view. Opening multi-view keeps it waiting.
- **Follow rules:** their switches use the outside-play rule.
- **Spoiler guard:** strips and banners call its helper.
- **Guide-mapping fixes:** better mappings mean better channel copies, through `channel_epg`, with no extra work.

---

## 9. Risks

1. **`app.rs` assumes one snapshot in about 32 places.** Mitigation: the focused-tile indirection keeps single view byte-for-byte the same path through tile A. Rig tests cover both modes.
2. **Heat and battery with two software 4K decodes** on a fanless M2. Mitigation: quality-fit copies, `light_decode_small`, and suspending pictures that aren't shown. All are measured in MV-1 before MV-2 builds on them.
3. **Providers that limit per IP across accounts.** TV 2 may refuse while TV streams. Mitigation: a clear per-picture error, the `observed` limit, and no retry storm. Only real use will show whether this happens.
4. **Key collisions with sibling designs.** `t`, `w`, `W`, `M` and `Ctrl+M` are claimed here. The keymap test catches duplicates at merge, and siblings should check this list.

## 10. Open questions for you (each with a recommended default)

1. **Should sound follow focus?** **Recommended: no.** Sound stays where it is until `M` or a click on 🔊, so you can zap the silent game without losing the commentary. Outside plays (reminder, Claude) do move the sound to what they switched.
2. **Default layout at full width?** **Recommended:** Side by side (with the info strips) at ≥ 1200 pt, Big + small at half screen, PiP in the pop-out and the mini player. `t` changes it, and Tuner remembers the choice.
3. **When the only way is to move the playing picture to its other playlist, should Tuner just do it?** **Recommended: ask each time** with a one-key card (Enter = Move), because the move can take up to 30 s while the playlist frees up. Claude gets `needs_rearrange` and asks you.