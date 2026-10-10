# Tuner — manual test checklist

These are the release checks that automated tests can't cover: a real compositor, real video,
real providers, the Omarchy desktop, and long runs. They come from DESIGN §23 and from the
`[manual]` items collected in phases 1–4. Run them on the release build of the tree you are
about to tag. For each item, record the date, the commit, and anything unexpected next to the
checkbox.

Each item lists the steps (**Do**) and what should happen (**Expect**). "Wide" means a
full-width window (≈ 1600 pt on the 2560×1600 @ 1.6 panel). "Tiled" means a half-width tile
(≈ 790 pt). "Text 12/16" means Omarchy's base font size at 12 and at 16.

## 0. Setup

- [ ] **Release build:** `cargo build --release`. Every launch below uses
  `target/release/tuner` (or `/usr/bin/tuner` once the package is installed).
- [ ] **Isolated profile:** use `--profile-dir /tmp/tuner-check` everywhere unless an item says
  otherwise. Delete it between runs that need a fresh start. The Omarchy theme is still read
  from the real home.
- [ ] **Test media:** run `scripts/gen-test-media.sh` (needs ffmpeg). It writes
  `target/test-media/{testsrc.mp4,bars.ts,hls/,long/}`.
- [ ] **Local "fault server"** for reconnect and error checks:
  1. Terminal 1: `scripts/gen-test-media.sh target/test-media --live` writes a rolling live HLS
     until you stop it.
  2. Terminal 2: `python3 -m http.server 8765 -d target/test-media`.
  3. Write `/tmp/tuner-check/fault.m3u` with three entries:
     - `Live` → `http://127.0.0.1:8765/live/index.m3u8`
     - `VOD` → `http://127.0.0.1:8765/hls/index.m3u8`
     - `Missing` → `http://127.0.0.1:8765/missing.m3u8`
  4. Add that file as a playlist.
- [ ] **Big library:** `scripts/gen-big-fixtures.py --channels 5000 /tmp/tuner-big`, then add
  `/tmp/tuner-big/big.m3u` as a playlist file in its own profile (`--profile-dir
  /tmp/tuner-5k`). If you can, also use a real provider with about 5 000 channels.
- [ ] **Real providers at hand:**
  - an Xtream account (with catch-up channels and VOD)
  - the iptv-org playlist (`https://iptv-org.github.io/iptv/index.m3u`)
  - a Pluto TV M3U with the i.mjh.nz XMLTV guide
- [ ] **Themes to switch between:** a dark theme (e.g. tokyo-night), `catppuccin-latte`
  (light) and `white`.

## 1. Build, package, launch

- [ ] **makepkg:**
  - **Do:** in a temporary copy of the checkout, run `cd packaging && makepkg -f` (no install).
  - **Expect:**
    - `prepare`, `build`, `check` (library tests) and `package` all succeed.
    - The package contains `/usr/bin/tuner`, `/usr/share/applications/tuner.desktop`,
      `/usr/share/icons/hicolor/scalable/apps/tuner.svg`,
      `/usr/share/tuner/{hypr/tuner.lua,omarchy/omarchy-menu.jsonc}` and
      `/usr/share/doc/tuner/{README.md,DESIGN.md,MANUAL_CHECKLIST.md}`.
    - `pkgver` matches `Cargo.toml`.
- [ ] **Desktop file:**
  - **Do:** `desktop-file-validate packaging/tuner.desktop`.
  - **Expect:** no output and exit status 0.
- [ ] **Launcher entry:**
  - **Do:** once installed, open Super+Space → Apps.
  - **Expect:** *Tuner* is listed with its icon. The right-click actions *TV Guide* and
    *Live TV* open those screens.
- [ ] **Launch-and-navigate smoke test:**
  - **Do:**
    1. Launch Tuner.
    2. Visit every screen with the sidebar and with Alt+1…5 and Alt+0.
    3. Open help (`?`), then Jobs (Ctrl+J).
    4. Switch the Omarchy theme once.
    5. Play `target/test-media/testsrc.mp4` (`tuner target/test-media/testsrc.mp4`).
  - **Expect:**
    - Each screen draws. Help and Jobs close when requested again, and switching screens closes
      them.
    - The theme recolours live.
    - The file plays fullscreen, and Esc returns to the screen you came from.
    - The first frame appears in well under a second.
- [ ] **Single instance:**
  - **Do:** with Tuner running, run `tuner --view guide`, then `tuner --refresh`.
  - **Expect:**
    - The first command switches the running window to the guide and exits 0.
    - `--refresh` logs that Tuner is running and exits 0 without touching anything.

## 2. Theme, fonts, look

- [ ] **Live theme switch:**
  - **Do:** while video plays in the preview, switch dark → light (`catppuccin-latte`) → dark
    with `omarchy-theme-set`.
  - **Expect:**
    - Everything recolours within about half a second.
    - The fallback palette never flashes.
    - State tints (cursor, hover) stay readable in the light theme.
- [ ] **OSD on the `white` theme:**
  - **Do:** play something and show the OSD.
  - **Expect:** the OSD text and accent are readable over video. The accent falls back to near
    white when the theme accent is too dark.
- [ ] **Source badges on `catppuccin-latte`:**
  - **Do:** look at Live rows, search results and Settings ▸ Playlists with two playlists that
    have different colours.
  - **Expect:** the badge glyphs and colours are distinguishable and readable on the light
    background.
- [ ] **Font change:**
  - **Do:** change the monospace font in `~/.config/fontconfig/fonts.conf` (or with Omarchy's
    font menu), then switch back.
  - **Expect:** the UI font changes live. CJK and Arabic names still render through the
    fallbacks.
- [ ] **Omarchy config created after start:**
  - **Do:**
    1. Start with `~/.config/omarchy/` temporarily renamed away.
    2. Restore it, then edit `shell.toml`.
  - **Expect:** the log says "exists now; watching it", and the change applies live (no more 5 s
    polling).

## 3. Layouts and text sizes

Run each item at **Wide** and **Tiled**, at **text 12** and **text 16**, in a **dark** and a
**light** theme (8 combinations; note any that fail).

- [ ] **Live TV:**
  - **Expect:**
    - Wide shows 3 panes. Medium shows 2 panes with the categories drawer, which opens, filters
      and closes from the keyboard. Narrow shows a stack.
    - Nothing is clipped or overlapping. Rows and the preview caption (channel · number ·
      badges · now/next) fit or ellipsize cleanly.
- [ ] **TV guide:**
  - **Expect:**
    - The channel column, time ruler and now-line line up.
    - The focused cell stays clear of the mini-player.
    - Cells too short for their title ellipsize.
- [ ] **Player:**
  - **Expect:**
    - At narrow widths the OSD controls row shows glyph-only buttons, with nothing cut off.
    - The tracks panel (audio, subtitles, delay, size) fits and scrolls.
    - The next-episode card does not cover the controls.
- [ ] **Settings:**
  - **Do:** hover rows with the mouse, then drag an EPG source and a favourites list by their
    grips.
  - **Expect:**
    - Hover moves the cursor only after real pointer motion.
    - Dragging reorders and persists after a restart. Alt+↑/↓ does the same from the keyboard.

## 4. Live TV and preview

- [ ] **Real video in the preview:**
  - **Do:** on Live, press Enter on a channel. Then turn on preview autoplay and rest the
    cursor on a channel.
  - **Expect:**
    - Video plays in the preview with sound.
    - Autoplay starts only after the delay and never replaces a stream you started yourself.
- [ ] **Mini-player:**
  - **Do:** play a channel fullscreen, press Esc, then go to another screen.
  - **Expect:**
    - The corner mini-player keeps playing the same stream, with no reconnect.
    - Click opens the player. ✕ stops playback. Double-click toggles window fullscreen. The
      mouse wheel changes volume only in the fullscreen player.
    - Toasts stack above the mini-player.
- [ ] **Error and reconnect hints in preview and mini-player:**
  - **Do:** play `Live` from the fault server, then stop the HTTP server for 10 s and start it
    again.
  - **Expect:** a *Reconnecting…* hint appears in the preview or mini-player and clears when the
    stream recovers. `Missing` shows an error hint (404), not a spinner.
- [ ] **Radio card:**
  - **Do:** play an audio-only channel (a radio entry from a real playlist).
  - **Expect:** the video area shows the radio card (logo or monogram, name, programme) instead
    of black.
- [ ] **Holding ↓ on a 5 000-channel library:**
  - **Do:** in the 5k profile, hold ↓ in the channel list for 10 s, then hold ↑.
  - **Expect:**
    - Scrolling stays smooth with no visible hitches.
    - Now/next text fills in as rows appear.
    - CPU drops back to idle when you let go.
- [ ] **"Loading guide… n %" during the first import:**
  - **Do:** add a playlist with a large guide to a fresh profile, then watch Live rows, the
    preview and the guide while the guide imports.
  - **Expect:** rows say *Loading guide… n %* with a rising number, never *No guide data*.
- [ ] **Stale or ended guide:**
  - **Do:** use a guide whose data has ended, or set the EPG offset so it ends now.
  - **Expect:** the status bar and guide header show the warning glyph with "Guide updated … ago"
    or "Guide data ended — refresh", and *Refresh* works.

## 5. TV guide and guide coverage

- [ ] **Trackpad scroll and tooltips:**
  - **Do:** in the guide, scroll horizontally and vertically with two fingers, then hover
    programme cells.
  - **Expect:**
    - Scrolling follows the fingers in both axes, without jumping a whole day.
    - Tooltips show the full title and time for truncated cells.
- [ ] **Guide coverage with the iptv-org playlist:**
  - **Do:** in a fresh profile, add the iptv-org playlist. Its header guide lists only a couple
    of channels. Let the import finish.
  - **Expect:**
    - The onboarding Done step names the guide and its coverage (e.g. "Matches 3 of 11 249
      channels") and offers *TV guide settings*.
    - Settings ▸ EPG shows "Matches N of M channels · guide lists X channels, Y programmes",
      plus the low-coverage note.
    - Settings ▸ Playlists shows "N ch (M with guide)".
    - A channel the guide doesn't list says *Not in your guide*, with the coverage, in the
      preview and the guide.
- [ ] **Ctrl+E from a channel without guide data:**
  - **Do:**
    1. Press Ctrl+E on an unlisted channel in Live and in the guide.
    2. Press Ctrl+E on a channel in a profile with no guide at all.
  - **Expect:**
    - The first opens EPG mapping for that channel, and assigning a guide channel fills its
      programmes.
    - The second opens Settings ▸ EPG, ready to add a guide.
    - Enter on the mapping screen's "No guide data" state also opens Settings ▸ EPG.
- [ ] **Adding a fuller guide fixes coverage:**
  - **Do:** add a second XMLTV guide that lists more of the channels, and move it first.
  - **Expect:** after its import, the coverage numbers rise and the rows show programmes. Manual
    mappings are kept.

## 6. Player

- [ ] **Reconnecting and Error overlays (fault server):**
  - **Do:** play `Live` fullscreen. Stop the HTTP server for 15 s, then restart it. Next, stop
    it for good. Finally, play `Missing`.
  - **Expect:**
    - The *Reconnecting… (n/8)* overlay appears, and playback recovers on its own after the
      restart.
    - Once attempts run out, an *Error* overlay appears with a retry hint.
    - `Missing` shows a 404 "channel offline" message, and the row is dimmed afterwards.
- [ ] **Esc ladder with compositor fullscreen:**
  - **Do:** play a channel and put the window into fullscreen with Hyprland's own fullscreen
    binding (not F11). Then press Esc repeatedly.
  - **Expect:**
    - The steps run in order: hide the banner or OSD → leave fullscreen (Tuner follows the
      compositor's state) → leave the player (the mini-player keeps playing) → previous screen.
    - Ctrl+P reopens the player only while something plays.
- [ ] **Up-next card and finished-movie return:**
  - **Do:** play the second-to-last minute of a series episode, then the last minutes of a
    movie (seek near the end).
  - **Expect:**
    - The episode shows the up-next card, which plays the next episode or can be dismissed.
    - The movie returns to its details page when finished and is marked watched.
    - Resume positions are kept.
- [ ] **Fullscreen toggle:**
  - **Do:** press F11 and `f` repeatedly during playback, and double-click the video.
  - **Expect:** no black frames that last, no crash, and the video stays sharp and correctly
    scaled.
- [ ] **Frame pacing:**
  - **Do:** play 25, 30, 50 and 60 fps samples (`bars.ts` is 30 fps; use real channels for the
    others) for 60 s each. Read the dropped frames from stream info (`I`).
  - **Expect:** record the numbers. 60 fps should not drop. 25/50 fps judder on a 60 Hz panel
    is a known limitation (DESIGN §11.8).
- [ ] **HDR / 10-bit sample (optional):**
  - **Do:** play a 10-bit HEVC or HDR sample.
  - **Expect:** it plays, with tone-mapped colours that are not washed out.

## 7. Catch-up, reminders, parental lock (real provider)

- [ ] **Catch-up:**
  - **Do:** on an Xtream channel with catch-up, open a past programme in the guide (marked ⟲)
    and press *Watch*. Do the same on an M3U catch-up channel.
  - **Expect:** playback starts at the programme's start, at the right time in both cases.
    Seeking works within the programme.
- [ ] **Reminders:**
  - **Do:** set a reminder (`r` in the guide) on a programme that starts in 3–4 minutes.
  - **Expect:**
    - A desktop notification arrives at start minus the lead time. *Watch* tunes the channel
      and raises Tuner.
    - Settings ▸ Reminders lists the reminder, and Delete cancels it.
- [ ] **Locked `--play` and reminder PIN prompt:**
  - **Do:**
    1. Lock a channel (Ctrl+L) and relock the session.
    2. Run `tuner --play <ref of that channel>`.
    3. Let a reminder for a locked channel fire, then press *Watch*.
  - **Expect:**
    - Both ask for the PIN before playing, and a wrong PIN plays nothing.
    - Three wrong PINs start a 30 s cooldown.
    - While locked, zapping with ↑/↓ skips the locked channel and recently failing channels.

## 8. Desktop integration

- [ ] **Media keys and MPRIS:**
  - **Do:** while playing, check `busctl --user list | grep mpris` and `playerctl -p tuner
    metadata`. Then use the Omarchy media keys (play/pause, next, previous), also with Tuner
    on another workspace.
  - **Expect:**
    - `org.mpris.MediaPlayer2.tuner` is listed, and the metadata shows the title, channel and
      logo.
    - Play/pause works. Next and previous zap on live channels.
    - The keys work while Tuner is hidden, and the bar's media widget follows.
- [ ] **Idle inhibitor:**
  - **Do:** temporarily lower the screensaver/idle timeout to 30 s. Play a video for 2 minutes
    without touching anything, then pause for 2 minutes. Restore the timeout afterwards.
  - **Expect:** no screensaver while video plays. Once paused, the screensaver starts after
    about 30 s.
- [ ] **Playback on another workspace (60 s), with the Hyprland snippet:**
  - **Do:** play a live channel, switch to another workspace for 60 s, then come back. Watch
    `top -p $(pgrep -x tuner)`.
  - **Expect:**
    - The audio never stops, and the video is live (not frozen) when you come back.
    - CPU stays at the playback level. Record the dropped frames from stream info.
- [ ] **Playback on another workspace (60 s), without the snippet:**
  - **Do:** remove the `render_unfocused` rule, reload Hyprland, and repeat.
  - **Expect:**
    - The audio continues. Video decoding is suspended after about 2 s (CPU drops).
    - The video resumes within a moment of coming back.
- [ ] **Applying the Hyprland snippet:**
  - **Do:** copy `packaging/hypr/tuner.lua` to `~/.config/hypr/tuner.lua` and add
    `require("hypr.tuner")` below `require("default.hypr.toggles")`. Reload Hyprland.
  - **Expect:**
    - The config loads without errors.
    - `hyprctl clients` shows class `tuner`.
    - Tuner's video is fully opaque (no wallpaper bleeding through, unlike a terminal).
    - Playback on another workspace behaves as in the "with the snippet" item.
    - The optional Super+Shift+T binding, when uncommented, launches or focuses Tuner.
- [ ] **Omarchy menu:**
  - **Do:** merge `packaging/omarchy/omarchy-menu.jsonc` into
    `~/.config/omarchy/extensions/omarchy-menu.jsonc`, then run `omarchy menu summon tv`.
  - **Expect:** the TV submenu appears. *Open Tuner* launches or focuses it, and the other
    entries switch the running window to that screen.
- [ ] **Real portal file dialogs:**
  - **Do:** use *Add a guide file…*, the M3U file step in onboarding, *Export backup…*,
    *Restore a backup…* and *Export log…*.
  - **Expect:**
    - The GTK portal dialog opens, floating and following the light/dark preference.
    - Cancel changes nothing. The chosen files work.
    - Exported files are mode 0600.

## 9. Onboarding (real providers)

- [ ] **Xtream:**
  - **Do:** in a fresh profile, enter a server without `http://` and with a port, plus valid
    credentials.
  - **Expect:**
    - The server is normalized. *Test* shows the status, expiry and connections.
    - *Open Live TV* becomes available as soon as the channels are in. Movies, series and the
      guide continue in the status bar.
    - Done shows the counts and guide coverage.
- [ ] **Bad credentials and an unreachable server:**
  - **Expect:** a wrong password returns to the password field with the provider's message. A
    wrong host offers *Retry* and *Add anyway*.
- [ ] **M3U URL without a guide:**
  - **Do:** add an M3U URL whose header names no guide.
  - **Expect:** the Guide step appears and accepts an XMLTV URL. *Skip* says it can be added
    later in Settings ▸ EPG.
- [ ] **M3U file from the desktop:**
  - **Do:** open an `.m3u` from the file manager, or run `tuner ~/list.m3u` and `tuner
    file:///path/list.m3u` with Tuner running.
  - **Expect:** the onboarding card opens at Details with the name filled in, in the running
    window.

## 10. Runtime behaviour

- [ ] **User-Agent applies live:**
  - **Do:** change Settings ▸ Player ▸ Default User-Agent, then a playlist's own User-Agent.
    Refresh, and watch the server's access log, or a provider that rejects VLC's agent.
  - **Expect:**
    - The next refresh, logo fetch and stream use the new agent, without a restart.
    - Logos hosted on a playlist's own server use that playlist's agent.
- [ ] **Debug logging applies live:**
  - **Do:** toggle Settings ▸ About & logs ▸ Debug logging while the log viewer is open.
  - **Expect:** DEBUG lines (including other libraries' messages) start and stop appearing
    immediately. `--debug` keeps it on regardless.
- [ ] **Quiet logs in normal use:**
  - **Do:** use the iptv-org playlist for 10 minutes: browse logos, and zap through dead
    channels.
  - **Expect:**
    - No WARN or ERROR lines from other crates (TLS, calloop).
    - Dead streams produce a few aggregated `tuner::mpv` lines, not one per failed fragment.
- [ ] **Clean shutdown on SIGTERM and SIGINT:**
  - **Do:**
    1. Play a movie for a minute.
    2. Run `pkill -TERM -x tuner`. Restart and resume.
    3. Repeat with `pkill -INT -x tuner`.
  - **Expect:**
    - The log says "SIGTERM: quitting" (or SIGINT), and the window closes normally.
    - `$XDG_RUNTIME_DIR/tuner/tuner.sock` (or `run/tuner.sock` in the profile) is gone.
    - The movie resumes where it was.
- [ ] **SIGTERM while hidden:**
  - **Do:** without the `render_unfocused` rule, move a playing Tuner to another workspace and
    send SIGTERM.
  - **Expect:** within about 3 s the log says it is exiting directly. The socket is gone and
    the VOD position is saved.

## 11. Soak

- [ ] **One-hour soak:**
  - **Do:** play one live channel for 60 minutes in the mini-player, with the guide open (it
    keeps loading programmes).
    - Record RSS with `ps -o rss= -p $(pgrep -x tuner)` at 5 minutes and at 60 minutes.
    - Zap a few times along the way.
  - **Expect:**
    - RSS at 60 minutes is within ± 10 % of the 5-minute value.
    - No growing CPU use, and no stalls.
    - Playback is still in sync with the audio.

## 12. Real providers (summary)

- [ ] **Xtream account:**
  - **Expect:**
    - Live, movies, series, the provider guide, catch-up, account info, and the connection
      limit all work. A second stream is never opened while one plays.
    - A refresh with renumbered streams keeps favourites and progress.
- [ ] **iptv-org playlist:**
  - **Expect:** it imports about 11 000 channels quickly. Guide coverage is shown (§5). Dead
    channels fail fast and get dimmed.
- [ ] **Pluto TV with the i.mjh.nz guide:**
  - **Expect:** most channels match the guide. Now/next and the grid are filled, and times are
    correct for the local time zone.
