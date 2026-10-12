# MV-2: multi-view in the UI

This track builds DESIGN §35 (designs/multiview.md) on MV-1's second player. `ui::multiview::ENABLED` is on.

## What the user gets

- **Opening.** `Ctrl+M` on a channel or programme in Live, the guide, search or Home opens multi-view. It works the same from the item menus ("Watch alongside"), and in the player it opens a channel picker for the other picture.
  - With nothing playing, it simply plays.
  - Next to a film or episode, it is refused: "Multi-view is for live TV and catch-up".
  - A past programme opens catch-up in the other picture. A future one says to use Remind me.
- **Layouts.**
  - `auto` gives side by side at 1200 pt ÷ rem and wider, big + small at 700 or wider, and picture in picture below that.
  - The pop-out and the mini player always show picture in picture.
  - `t` cycles the layout and saves it as `player.multiview.layout`. `W` swaps the places.
  - Dragging a picture onto the other swaps them. Dragging the inset snaps it to a corner (saved as `pip_corner`), and dropping it on the big picture's centre third swaps.
- **Focus and sound.**
  - Focus is where keys go: `w`, Ctrl+arrows, or a click (a click never moves the sound).
  - `M` moves the sound to the focused picture, as do the 󰕾 hover button and "Sound here".
  - Volume and mute act on the picture with the sound (`UiCtx::play` routes them).
  - The focused picture has a 2 pt accent ring, dimmed when the banner hides. Each picture has a corner letter, with 󰕾 on the audible one.
- **Chrome.**
  - Hovering a picture shows sound here, swap and close.
  - Side by side, each picture has a three-line info strip: number, name and playlist; the programme with its time and progress; next. In big + small, only the small picture has a strip.
  - Elsewhere, the small picture gets a caption for 3.5 s after a zap or focus change.
  - With the banner up, the group Layout · Sound here · Swap · Close sits above the bottom bar.
  - The right-click menu starts with the multi-view items, including "Close picture B".
  - Programme titles go through the spoiler guard's `Spoilers`.
- **Closing.** `q` closes the focused picture and the other carries on, focused and with the sound. Esc never closes a picture. Ctrl+Shift+S, and the ✕ of the status bar and mini player, stop both. Ctrl+Shift+P pauses or plays both.
- **Live.**
  - Picture chips over the preview: a click chooses which picture the preview shows and Enter retunes.
  - The rows that play show A or B.
  - The note under the preview reads "↵ plays in A · Ctrl+M plays in B", plus what the planner would do with the cursor channel in the other picture: "B would play it from TV 2 (TV is busy with picture A)", "Needs picture A to move to TV 2", or why it can't play.
  - Preview autoplay is off while two pictures show.
- **Settings ▸ Player ▸ Multi-view:** layout, picture-in-picture corner, and lighter decoding for the small picture (off by default, per the MV-1 spike), with one line about connection limits.

## Rules kept

- **Two playlists, one stream each.** The planner (`ui::multiview::plan`) never exceeds a limit. It prefers the pick, then a healthy copy, then quality fit: a small picture takes HD over UHD.
  - It never picks a locked copy while locked, nor one whose explicit flag differs.
  - Moving the playing picture is never silent: the confirm card reads "Move picture A…? Enter = Move", and only then does `Tiles::rearrange` run.
  - A copy keeps the pick's zap-list position (the pick's id is replaced in the zap list).
- **Plays from outside take the silent picture and the sound.** This covers a reminder's switch or Watch, `--play`, and MCP `play` of a channel or catch-up (`play_channel` and `on_agent_play`).
- **Films, episodes and files play in picture A only,** where resume and the next-episode card live. Picking one while two pictures show stops B and ends multi-view (`send_player`, `UiCtx::play`).
- **Single view may go on in picture B** after A is closed (no reload). If picture A then starts something (Up next, MPRIS, a film), B stops.
- **Parental.**
  - A locked channel asks for the PIN "for picture B".
  - Relocking stops every picture that shows explicit content. With two pictures, it also stops every picture that shows a locked channel.
- **The routers see the pictures.** `AppShared::publish_tiles` runs every frame, which covers MPRIS, the reminder scheduler, the queue's "waits while multi-view is on" and the agent.

## Code

| File | What |
|---|---|
| `src/ui/multiview.rs` | State, layouts, geometry, the planner, `MvRequest`, `route_command`, the menu hooks, Live's chips and note, the channel picker |
| `src/app/multiview.rs` (new) | The app side: placing, B's start and surface, consent and PIN, reconcile, relock, publishing |
| `src/app.rs` | Key stack (`MultiView` above the player's base context, `PopOut` above it), event routing per picture, `send_player`, `play_channel`, `on_agent_play`, logic (both snapshots, resume, render, idle inhibit), status bar and mini player |
| `src/ui/mod.rs` | `UiCtx::multiview`, `play` (routing), `play_in`, `mv_request`, `audible_player`, and `NavRequest::MultiView` |
| `src/ui/video_host.rs` | Two surfaces, `paint_tile`, per-picture render and painted flags, and the picture-in-picture mini player (`mini_player_with`) |
| `src/ui/screens/player/{mod,multiview}.rs` | Layout, chrome, strips, keys, mouse and menu |
| `src/ui/popout.rs` | `strip` hook for rui's countdown strip; the player lays the pictures out below it |
| `src/ui/screens/live.rs` | Chips, row letters, note, autoplay off, hint |
| `src/ui/screens/settings/multiview.rs` | The Settings rows |

## Tests

- `ui::multiview`:
  - layouts fit at 790, 950 and 1600 pt;
  - auto layout by width and rem, and the pop-out is PiP;
  - Ctrl+arrows and corner snapping;
  - the planner on two one-stream playlists (a copy, the move offered and never chosen, both only on TV, two at once, an unknown limit);
  - never escalate (locked, explicit), trouble and quality fit;
  - command routing.
- `app` rig tests (real players, B started on the blocking pool):
  - Ctrl+M opens B on TV 2's copy with focus and sound unchanged;
  - a reminder's Watch goes to the silent picture and takes the sound;
  - a film ends multi-view in A;
  - the player keys: `MultiView` in the stack, `w`, Esc never closes, `q` closes and the other carries on;
  - a move asks (Esc moves nothing; Enter moves A and A's claim moves at once);
  - the multi-view player fits at 790 and 1600 pt with text size 12 and 16.
