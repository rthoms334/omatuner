# Tuner — manual test checklist

These are the release checks that automated tests can't cover: a real compositor, real video,
real providers, the Omarchy desktop, and long runs. They come from DESIGN §23 and from the
`[manual]` items collected in phases 1–5. Run them on the release build of the tree you are
about to tag. For each item, record the date, the commit, and anything unexpected next to the
checkbox.

Each item lists the steps (**Do**) and what should happen (**Expect**). "Wide" means a
full-width window (≈ 1600 pt on the 2560×1600 @ 1.6 panel). "Tiled" means a half-width tile
(≈ 790 pt). "Text 12/16" means Omarchy's base font size at 12 and at 16 (`[font]` `base-size`
in `~/.config/omarchy/shell.toml`; Tuner applies it live).

## 0. Setup

- [ ] **Release build:** `cargo build --release`. Every launch below uses
  `target/release/tuner` (or `/usr/bin/tuner` once the package is installed).
- [ ] **Isolated profile:** use `--profile-dir /tmp/tuner-check` everywhere unless an item says
  otherwise. Delete it between runs that need a fresh start. The Omarchy theme is still read
  from the real home.
  - Every follow-up command that talks to a running Tuner (`tuner --view`, `--refresh`,
    `--play`, opening a file) needs the same `--profile-dir`. The socket is per profile
    (`/tmp/tuner-check/run/tuner.sock`), so without it the command reaches the default profile.
  - The launcher entry, the Omarchy menu, the Super+Shift+T binding and the media-key items
    always use the **default** profile. Those items say so: start Tuner from the launcher, not
    with `--profile-dir`.
- [ ] **Test media:** run `scripts/gen-test-media.sh` (needs ffmpeg). It writes
  `target/test-media/{testsrc.mp4,bars.ts,hls/,long/}`.
- [ ] **Local "fault server"** for reconnect and error checks:
  1. Terminal 1: `scripts/gen-test-media.sh target/test-media --live` writes a rolling live HLS
     until you stop it.
  2. Terminal 2: `python3 -m http.server 8765 -d target/test-media`.
  3. Write `/tmp/tuner-fault.m3u` (outside the profile, so it survives a fresh start) with three
     entries:
     - `Live` → `http://127.0.0.1:8765/live/index.m3u8`
     - `VOD` → `http://127.0.0.1:8765/hls/index.m3u8`
     - `Missing` → `http://127.0.0.1:8765/missing.m3u8`
  4. Add that file as a playlist.
- [ ] **Big library:**
  1. `scripts/gen-big-fixtures.py --channels 5000 /tmp/tuner-big`.
  2. In its own profile (`--profile-dir /tmp/tuner-5k`), add `/tmp/tuner-big/big.m3u` as a
     playlist file.
  3. The playlist's header guide (`http://127.0.0.1/big.xml.gz`) is not served, so it fails:
     ignore it. Settings ▸ EPG ▸ *Add a guide file…* → `/tmp/tuner-big/big.xml.gz`, and move it
     first.
  - The fixture guide covers 2026-10-09 plus 7 days. After that its rows show *Guide data
    ended*, which is still useful for §4 "Stale or ended guide".
  - If you can, also use a real provider with about 5 000 channels.
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
    - `prepare`, `build`, `check` (runs `tuner --version`) and `package` all succeed.
    - It writes `packaging/omatuner-<version>-1-<arch>.pkg.tar.zst`.
    - The package contains `/usr/bin/tuner`, `/usr/share/applications/tuner.desktop`,
      `/usr/share/icons/hicolor/scalable/apps/tuner.svg`,
      `/usr/share/tuner/{hypr/tuner.lua,omarchy/omarchy-menu.jsonc}`,
      `/usr/share/doc/tuner/{README.md,DESIGN.md,MANUAL_CHECKLIST.md}` and
      `/usr/share/licenses/omatuner/LICENSE` (`pacman -Qlp omatuner-*.pkg.tar.zst`).
    - `pkgver` matches `Cargo.toml`.
- [ ] **CI and release workflows:**
  - **Do:** check that the last CI run on `main` is green for both x86_64 and aarch64
    (<https://github.com/rthoms334/omatuner/actions/workflows/ci.yml>). Before tagging, run the
    release workflow by hand with `publish` off (`gh workflow run release.yml --ref main -f
    publish=false`) and download its artifacts.
  - **Expect:**
    - Both architectures produce `omatuner-<version>-1-<arch>.pkg.tar.zst`,
      `omatuner-<version>-<arch>.tar.gz` and their `.sha256` files.
    - The tarball holds `tuner`, `tuner.desktop`, `tuner.svg`, the snippets, `README.md` and
      `LICENSE`.
- [ ] **Release package install** (after `git tag vX.Y.Z && git push origin vX.Y.Z`):
  - **Do:** download the package for this machine and `SHA256SUMS` from
    <https://github.com/rthoms334/omatuner/releases>, run
    `sha256sum -c --ignore-missing SHA256SUMS`, then
    `sudo pacman -U omatuner-<pkgver>-1-$(uname -m).pkg.tar.zst` (`<pkgver>` is the tag without
    its `v` and the suffix's dash: `0.1.0` for `v0.1.0`, `0.1.0rc4` for `v0.1.0-rc4`).
  - **Expect:** the checksum is OK, pacman installs it with its dependencies (a final release
    over its pre-release is an upgrade, not a reinstall), and `tuner --version` prints the
    tag's version without any pre-release suffix.
- [ ] **Desktop file:**
  - **Do:** `desktop-file-validate packaging/tuner.desktop`.
  - **Expect:** no output and exit status 0.
- [ ] **Launcher entry** (default profile):
  - **Do:** once installed, open Super+Space → Apps.
  - **Expect:** *Tuner* is listed with its icon. The right-click actions *TV Guide* and
    *Live TV* open those screens.
- [ ] **Launch-and-navigate smoke test:**
  - **Do:**
    1. Launch Tuner.
    2. Visit every screen with the sidebar and with Alt+1…5 and Alt+0.
    3. Open help (`?`), then Jobs (Ctrl+J).
    4. Switch the Omarchy theme once.
    5. Play `target/test-media/testsrc.mp4`
       (`tuner --profile-dir /tmp/tuner-check target/test-media/testsrc.mp4`).
  - **Expect:**
    - Each screen draws. Help and Jobs close when requested again, and switching screens closes
      them.
    - The theme recolours live.
    - The file plays fullscreen, and Esc returns to the screen you came from.
    - The first frame appears in well under a second.
- [ ] **Single instance:**
  - **Do:** with Tuner running on `/tmp/tuner-check`, run
    `tuner --profile-dir /tmp/tuner-check --view guide`, then
    `tuner --profile-dir /tmp/tuner-check --refresh`.
  - **Expect:**
    - The first command switches the running window to the guide and exits 0.
    - `--refresh` logs that Tuner is running and exits 0 without touching anything.
- [ ] **Background refresh holds the profile:**
  - **Do:** quit Tuner; make a playlist stale and start
    `tuner --profile-dir /tmp/tuner-check --refresh`; while it runs, launch
    `tuner --profile-dir /tmp/tuner-check`.
  - **Expect:**
    - The launch waits about 10 s, then exits saying a background refresh is running (unless
      the refresh finished first, in which case the window opens).
    - If the refresh upgraded a playlist to its Xtream login, the next window shows
      "<name>: upgraded to Xtream login — movies & series now available" once.

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
- [ ] **Omarchy config created after start** (throwaway home, so the desktop is untouched):
  - **Do:**
    1. `H=$(mktemp -d); HOME=$H target/release/tuner --profile-dir /tmp/tuner-check`
    2. `mkdir -p $H/.config/omarchy`, then write `$H/.config/omarchy/shell.toml` containing
       `[font]` and `base-size = 16`.
  - **Expect:**
    - The fallback palette until then (the throwaway home has no Omarchy theme).
    - Within about 5 s the log says "… exists now; watching it instead of polling", and the
      text grows. Later edits to `shell.toml` apply immediately.

## 3. Layouts and text sizes

Run each item at **Wide** and **Tiled**, at **text 12** and **text 16**, in a **dark** and a
**light** theme (8 combinations; note any that fail). The width class follows from both:
**Wide** is Wide (3 panes) at either text size; **Tiled at text 12** is Medium (icon rail,
2 panes, categories drawer); **Tiled at text 16** is Compact (a single stack).

- [ ] **Live TV:**
  - **Expect:**
    - Wide shows 3 panes. Medium shows 2 panes with the categories drawer, which opens, filters
      and closes from the keyboard. Compact shows a stack.
    - Nothing is clipped or overlapping. Rows and the preview caption (channel · number ·
      badges · now/next) fit or ellipsize cleanly.
- [ ] **TV guide:**
  - **Expect:**
    - The channel column, time ruler and now-line line up.
    - The focused cell stays clear of the mini-player.
    - Cells too short for their title ellipsize.
- [ ] **Player:**
  - **Expect:**
    - At Medium and Compact the OSD controls row shows glyph-only buttons, with nothing cut off.
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
  - **Do:** play a series episode, seek into its last minute and let it end. Then do the same
    with a movie.
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
    1. Lock a channel (Ctrl+L), then relock the session with Settings ▸ Parental ▸ *Lock now*.
    2. Find the channel's reference:
       `sqlite3 -readonly /tmp/tuner-check/data/tuner.db "SELECT source_id||':'||key FROM channel WHERE name LIKE '%…%'"`.
    3. Run `tuner --profile-dir /tmp/tuner-check --play <ref>`.
    4. Let a reminder for a locked channel fire, then press *Watch*.
  - **Expect:**
    - Both ask for the PIN before playing, and a wrong PIN plays nothing.
    - Three wrong PINs start a 30 s cooldown.
    - While locked, zapping with ↑/↓ skips the locked channel and recently failing channels.

## 8. Desktop integration

These items use the **default** profile: start Tuner from the launcher, not with
`--profile-dir`.

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
  - **Do:** play a video, run `omarchy debug idle` and read its *Idle inhibitors* section.
    Pause, then run it again. If you want to watch the screensaver itself, lower the idle timeout
    temporarily, leave the video playing untouched for twice that long, then pause for the same
    time, and restore the timeout afterwards.
  - **Expect:**
    - While video plays, class `tuner` is listed there and no screensaver starts.
    - Once paused, the inhibitor is gone and the screensaver starts after the timeout.
- [ ] **Applying the Hyprland snippet:**
  - **Do:** copy `packaging/hypr/tuner.lua` to `~/.config/hypr/tuner.lua` and add
    `require("hypr.tuner")` below `require("default.hypr.toggles")`. Reload Hyprland.
  - **Expect:**
    - The config loads without errors.
    - `hyprctl clients` shows class `tuner`.
    - Tuner's video is fully opaque (no wallpaper bleeding through, unlike a terminal).
    - The optional Super+Shift+T binding, when uncommented, launches or focuses Tuner.
- [ ] **Playback on another workspace (60 s), with the Hyprland snippet:**
  - **Do:** play a live channel, switch to another workspace for 60 s, then come back. Watch
    `top -p $(pgrep -x tuner)`.
  - **Expect:**
    - The audio never stops, and the video is live (not frozen) when you come back.
    - CPU stays at the playback level. Record the dropped frames from stream info.
- [ ] **Playback on another workspace (60 s), without the snippet:**
  - **Do:** comment out the snippet's `render_unfocused` rule, reload Hyprland, and repeat.
    Restore the rule afterwards.
  - **Expect:**
    - The audio continues. Video decoding is suspended after about 2 s (CPU drops).
    - The video resumes within a moment of coming back.
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
- [ ] **Drag and drop (Hyprland):**
  - **Do:** drag an `.m3u` from the file manager onto the window, then an `.xml.gz` guide, then
    the same guide again, then a `.txt`.
  - **Expect:** the playlist opens the onboarding card at Details; the guide is added and
    imports ("Added the guide …"); the second drop says "… is already in your guides"; the
    `.txt` gets the "Drop an .m3u / .m3u8 playlist or an XMLTV guide" hint. If nothing arrives
    at all, record it: drops may be inert under Wayland with this eframe build (known gap).
- [ ] **MPRIS never zaps onto a locked channel:**
  - **Do:** set a PIN, lock a channel between two others, play the one before it and press the
    media key *next* (or `playerctl -p tuner next`), also after unlocking the session in-app.
  - **Expect:** the locked channel is skipped both times (deliberate: MPRIS always counts as
    locked while a PIN is set).
- [ ] **Single instance, quick double launch:**
  - **Do:** run `tuner --profile-dir /tmp/tuner-check & tuner --profile-dir /tmp/tuner-check`
    (both within a second), then a third launch a moment later.
  - **Expect:** one window opens; `/tmp/tuner-check/run/tuner.sock` still exists afterwards;
    the third launch hands off (the window comes forward) and exits.

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
    file:///path/list.m3u` with Tuner running (add `--profile-dir /tmp/tuner-check` to both
    when that is the running profile).
  - **Expect:** the onboarding card opens at Details with the name filled in, in the running
    window.
- [ ] **Free channels against the real internet:**
  - **Do:** in a fresh profile with a GB time zone, choose *Try free channels*. Pick
    *iptv-org · United Kingdom*; in another fresh profile pick *Free-TV*, and later add
    *iptv-org · All countries* from Settings ▸ Playlists ▸ *Add free channels…*.
  - **Expect:**
    - Nothing is downloaded before a pick; the first row reads "iptv-org · United Kingdom" and
      the subtitle warns that some channels may not play where you are.
    - UK: about 300 channels; Free-TV: about 2 000 with its epgshare01 guide added; all
      countries: about 11 000. All are live channels only (no movies or series).
    - Playlists already added read *Added* and can't be picked again.
- [ ] **A preset that fails:** start with `TZ=` set to a zone whose country has no iptv-org
  list (check that its `countries/<cc>.m3u` URL returns 404) and pick the country row.
  **Expect:** the error, and Back returns to the presets with that row focused, never to Welcome.
- [ ] **Provider M3U is offered its Xtream login:**
  - **Do:** add a provider's `get.php?username=…&password=…` URL; in another fresh profile its
    downloaded `.m3u` file; in a third a file whose live section uses the short form
    `http://host:port/<u>/<p>/<id>` and whose VOD uses `/movie/<u>/<p>/…`.
  - **Expect:** the Test step shows *Xtream login found* each time; *Continue* imports live,
    movies, series and the provider guide with catch-up and account info.
- [ ] **Keep as M3U:**
  - **Do:** repeat the previous item and choose *Keep as M3U*.
  - **Expect:** the Test step then reads "Looks good — M3U playlist". The import puts the
    provider's `/movie/` entries under Movies and `/series/` entries under Series (grouped into
    seasons and episodes); no `player_api.php` request is made (provider or proxy log).

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
- [ ] **A dead guide backs off:** add a source whose guide URL is dead. **Expect:** after the
  first failure the guide is retried about an hour later (not after 12 h, not every 15 min),
  with one toast per failure.
- [ ] **Ctrl+R online:** **Expect:** an unchanged playlist comes back not modified (304); an M3U
  whose Xtream upgrade failed recently is not retried; the "Refreshing N playlists and M guides"
  toast still shows.
- [ ] **Ctrl+R offline with several playlists and guides:** **Expect:** exactly one error toast,
  and the status bar says "Offline — showing saved data".
- [ ] **Backspace onto a locked channel:** with a PIN set, unlock, watch a locked channel, zap
  away and let the session relock. **Expect:** Backspace opens the PIN prompt; the right PIN goes
  back to that channel.
- [ ] **Restore while leaving Settings:** start a backup restore with **Restore settings** on
  and leave Settings before it finishes. **Expect:** the restored preferences are applied and
  saved to `config.toml`.
- [ ] **Restore settings on Merge vs Replace:**
  - **Do:** back up, then change a preference (e.g. the layout) and set a PIN. In Settings ▸
    Backup choose *Merge*, then *Replace everything*, then *Merge* again, watching **Restore
    settings**. Restore with *Merge* and the toggle off; then with *Merge* and the toggle on;
    then with *Replace everything* (toggle on).
  - **Expect:**
    - The toggle reads off for Merge and on for Replace each time the mode changes.
    - Merge with it off keeps your current layout; with it on, and with Replace, the backup's
      layout comes back and `config.toml` is rewritten.
    - Your PIN is never replaced by the backup's.

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
- [ ] **Malloc arenas:**
  - **Do:** repeat the soak once as usual and once with `MALLOC_ARENA_MAX=8` set; also play a
    1080p channel for 10 minutes with the default (Tuner caps glibc at 2 arenas).
  - **Expect:** RSS about 100–140 MiB with the default against 250 MiB or more with 8 arenas on
    16 KiB-page machines (record x86_64 too, it was never measured); no dropped frames at 1080p
    from CPU or lock contention (stream info). With `GLIBC_TUNABLES=glibc.malloc.arena_max=8`
    the user's limit is kept.

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

## 13. Xtream upgrade and M3U movies & series (real provider)

- [ ] **Existing M3U upgrades silently:**
  - **Do:** with the previous release, add a provider's `get.php` URL as M3U; favourite a few
    channels, watch part of a `/movie/` entry, lock a group "Adult". Install this build and
    refresh (or wait for the scheduler).
  - **Expect:**
    - Toast "Upgraded to Xtream login — movies & series now available" (with several
      playlists it is prefixed with the playlist's name). Settings ▸ Playlists reads *Xtream
      (from M3U)*.
    - Favourites, history and progress are still there; the movie group "Adult" is locked.
- [ ] **Quit during the first import:** lock the channel group "Adult", quit while the first
  import after the update runs, start again and refresh. **Expect:** the movie group "Adult" is
  locked.
- [ ] **Switching back and forth:**
  - **Do:** open the upgraded playlist's editor and turn **Use the Xtream login** off; Cancel the
    confirm, then do it again and choose *Use playlist*. Then turn it back on.
  - **Expect:** Cancel changes nothing. *Use playlist* goes back to the M3U (favourites kept
    where the playlist has them). Turning it on signs in again with the upgrade toast.
- [ ] **Turning the login on fails:** on a playlist kept as M3U whose login is rejected, turn
  **Use the Xtream login** on. **Expect:** the "Couldn't sign in…" toast; the row reads off again
  and the playlist stays kept (no automatic retries afterwards).
- [ ] **Silent upgrade under an open editor:** open the playlist's editor and start editing
  *Playlist URL*, then let a refresh upgrade it. **Expect:** editing ends, the cursor goes to
  Name, and Save does not undo the upgrade.
- [ ] **Sparse M3U pages:** on a playlist kept as M3U, open a movie and a series. **Expect:** a
  deliberate short page (title, poster, group, and the playlist when there are several) with no
  loading skeleton and no request to the provider; an untitled episode reads
  "S01E03 · Episode 3", also on the next-episode card.
- [ ] **Offline at launch:**
  - **Do:** add a provider's `get.php` URL as M3U with the previous release (not upgraded yet).
    Install this build, disconnect the network and start Tuner. Reconnect a few minutes later and
    press Ctrl+R; press Ctrl+R again more than an hour later.
  - **Expect:**
    - At launch the refresh fails offline (one toast, "Offline — showing saved data"), and the
      log says "Couldn't use the Xtream login of … (network error …)".
    - The first Ctrl+R imports the playlist as M3U without signing in again.
    - The Ctrl+R an hour later signs in and upgrades, with the toast — not a week later.
- [ ] **Quit right after the switch:** on a playlist not upgraded yet, refresh and quit as soon
  as Live TV has its new channels (while movies and series still import). **Expect:** the next
  start shows the upgrade toast once.
- [ ] **Launch from the desktop during a background refresh:** run `tuner --refresh` on a slow
  provider and start Tuner from the app launcher. **Expect:** after about 10 s a desktop
  notification says a background refresh is running.
- [ ] **Login refused:** use a playlist whose panel rejects the API login. **Expect:** it stays
  M3U with movies and series sorted out of the playlist, and the next automatic check is a week
  later (*Refresh now* in its editor retries at once).

## 14. Countries and languages (category filter) and bulk hide (real provider)

Use a real Xtream account whose categories cover many countries (≈ 60 tags: "UK | …", "|DE| …",
"AR: …", "EX-YU …", "4K | …", "VIP | UK | …"), with the time zone set to Europe/London.

- [ ] **Onboarding defaults:**
  - **Do:** fresh profile; add the Xtream account. When **Open Live TV** turns on, press Enter
    at once, then look at the **Choose what to show** step.
  - **Expect:**
    - The immediate Enter does not save (the step ignores Enter for its first moment).
    - Ticked: UK, English and the other English-speaking countries present (US, CA, AU, IE, NZ),
      and **No country tag**; everything else unticked. "N of M ticked · X of Y channels" is
      right. Rows read "UK · United Kingdom", not "GB".
    - Movie and series categories join the counts while they import, without the list jumping.
    - "4K | Sports", "BT Sport", "FA Cup", "Premier League"-style categories are under **No
      country tag**; `UK-…`, `UK.…`, `[🇬🇧] …`, `EX YU …` sit under the right tag.
    - Enter saves; Live TV, the guide, Movies, Series, Home rails, search (Ctrl+K) and zapping
      (↑/↓ in the player) show nothing from unticked countries; group and channel counts match.
- [ ] **Picker keys:** in Settings ▸ Playlists ▸ (playlist) ▸ *Countries & languages…*: type
  "fr" to filter, Space ticks, Ctrl+A all / none of the rows shown, Ctrl+E back to the defaults,
  Esc clears the filter then cancels. Untick everything + Enter: the warning line, and only a
  second Enter saves. Tick everything + Enter: the rule is removed (everything shows).
- [ ] **New categories after a refresh stay hidden:**
  - **Do:** keep the defaults. Wait for the provider to add categories (or use a panel where
    you can add "DE | New Test" and "NZ | New Test"), then Ctrl+R.
  - **Expect:** "DE | New Test" never appears; "NZ | New Test" does (NZ is in the defaults even
    when the playlist didn't have it). The picker marks newly seen tags *new*.
- [ ] **Exceptions and Settings ▸ Hidden:**
  - **Expect:** Settings ▸ Hidden lists each hidden tag with its counts. Enter on "Arabic" shows
    all Arabic categories again (toast "Arabic shown"). → expands a tag (down chevron); Enter on
    one category shows just that one, and it stays shown after a refresh. Ctrl+H on a kept
    group hides it and it is listed under *Hidden groups*.
- [ ] **Playlist added before this version:** with a profile from v0.1.0-rc3, open the picker.
  **Expect:** the note "… refresh to find them all (Ctrl+R)"; Ctrl+R refreshes and the note
  goes away, and most categories now have tags.
- [ ] **Bulk hide (Live groups):**
  - **Do:** Space on three groups (check marks, footer "3 marked · Ctrl+H hides · Esc clears"),
    Ctrl+H. Then type "ar" and press Ctrl+Shift+H.
  - **Expect:** the three groups go in one step with "Hid 3 groups · Settings ▸ Hidden items shows
    them again". The footer reads "N groups with “ar” · Ctrl+Shift+H hides them"; the confirm
    names the first five; "AR | …" and "Arabic …" groups go, "Cartoons" and "Documentary" stay.
    Choosing a menu item with Space never marks the group behind the menu.
- [ ] **Bulk hide (Movies / Series):** the same with categories. **Expect:** the toast, confirm
  and footer say "categories".
- [ ] **Badges:** with smart names on, "UK | Sports" and "US | Sports" read "Sports" with a
  muted UK / US badge; with smart names off, "|UK| Sports" has no extra badge.

## 15. Quality badges (real provider)

Use a provider that lists the same channel several times in different qualities (e.g.
"UK: BBC One SD / HD / FHD / 4K", often also "ᴴᴰ" or "1080p" names), at **Wide** and **Tiled**,
**text 12** and **text 16**.

- [ ] **Duplicates show distinct badges:**
  - **Do:** after this build's first start (the migration fills the badges of the existing
    library without a refresh), open the group with the duplicates in Live TV, the guide, Search
    (`/` + the channel name), the player's channel list (`c`) and mini guide (`g`), and a Home
    favourites rail holding two of them.
  - **Expect:**
    - Each copy shows its badge (SD, HD, FHD, 4K) right after the same name, in the muted badge
      look (monospace, thin border), never clipped. A channel with no quality word has none.
    - The copies are listed best first (4K, FHD, HD, SD) in their group; other channels keep
      the provider's order (also "2160p" / "576p" names). ↑/↓ in the player zaps in that order,
      and the zap banner shows the badge of the copy it tunes to until the picture comes up
      (then only the decoded badge on the right, no second "FHD").
    - In a narrow list (Home tiles, the guide column at Tiled, the player's list) a long name
      gets the ellipsis and the badge stays; only in a very narrow cell does the name keep the
      room. Radio channels have no badge.
- [ ] **The measured value replaces a wrong name tag after playback:**
  - **Do:** play a copy whose name claims more than it delivers (a "4K" that plays at 1080p;
    stream info, `I`, shows the real size), and a channel without a quality word.
    Leave the player and look at their rows; then restart Tuner and refresh the playlist.
  - **Expect:** as soon as each plays, its badge shows the real class (FHD for the 1080p "4K";
    SD/HD/FHD for the untagged one) in every list, and it stays so after the restart and the
    refresh. A 1440p stream reads FHD. If the provider reorders its copies (or adds one), each
    copy keeps its own measured badge after the next refresh; a copy the provider renames
    ("HD" → "FHD") shows its new name's badge until it plays again.
- [ ] **Setting:**
  - **Do:** Settings ▸ Interface ▸ Quality badges off, then on.
  - **Expect:** the badges disappear from every list at once and come back; the Live preview
    and the player OSD still show the stream's quality.
