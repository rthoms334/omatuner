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
- [ ] **Playlist on cards (two playlists, light and dark):**
  - **Do:** add two playlists with movies, series and channels and give them different glyphs
    and colours in their editors, one of them yellow. Watch something from each, favourite a
    channel and a movie from each. Then, in a dark theme and in `catppuccin-latte`, look at:
    Movies and Series grids; every Home rail (Continue watching, Recently watched,
    Favourites, Recently added movies / series, channel tiles); a movie and a series page;
    Search (`/`) results; Live ▸ Favourites, History, Reminders and All channels; the Live
    preview; the player's channel (c), mini guide (g) and history (h) panels.
  - **Expect:**
    - Each poster has a small chip in its top-left corner with its own playlist's glyph (the
      lock and watched chips stay top-right); hovering the chip shows the playlist's name.
    - The line under each card's title starts with the playlist's name ("Provider A · 2019 ·
      ★ 7.1"); a long name ends in "…" and leaves room for the year.
    - Rows, channel tiles and the player panels show the glyph at the right; detail pages (an
      Xtream movie too) and the preview show glyph + name; the player's top bar shows "● name"
      left of the clock.
    - Group rows in Live and the Movies / Series categories show the playlist's name in its
      colour, so the two playlists' "Action" (or "Sports") rows can be told apart.
    - On the light theme the yellow playlist's glyph and name are drawn in the text colour,
      with a yellow edge on the chip and a yellow underline under row glyphs; nothing is
      unreadable.
    - Disable one playlist (Settings ▸ Playlists): the marks disappear at once (Auto). Settings
      ▸ Interface ▸ Playlist on cards ▸ Always brings them back; Never hides them, with no
      restart.
    - At a tiled half (790 pt) and with text size 16, nothing overlaps or leaves its card.
- [ ] **Choosing a playlist (two playlists with movies, series and channels; DESIGN §29):**
  - **Do:** open Movies at full width.
  - **Expect:** a bar on top reads "All playlists · ● A · ● B" with counts; All is underlined.
  - **Do:** press `Ctrl+PgDn`, then `Ctrl+PgUp` twice; click B's tab; right-click A's tab.
  - **Expect:**
    - A toast "Showing A only", then "Showing all playlists", then "Showing B only".
    - With one playlist: only its categories; All movies / Continue watching / Favourites
      hold only its titles; the grid header reads "B › All movies — type to filter"; a 2 px
      line in B's colour runs under the bar; cards have no playlist marks; the Omarchy bar
      shows "Tuner — Movies · B".
    - The right-click menu offers "Edit playlist…" (opens B's editor in Settings ▸ Playlists)
      and "Move up / Move down" (the tabs reorder, also past a playlist that has no tab here).
  - **Do:** go to Series, Live TV and the guide; press `Alt+P` on each and pick with a digit.
  - **Expect:** Series still shows All (each screen keeps its own); the picker lists All + the
    playlists with counts, `0`–`9` pick at once, typing filters. Live TV narrows groups, All
    channels, favourites and History to the playlist, and ↑/↓ in the player zaps only through
    it. The guide shows the same playlist as Live TV (its "● B ▾" button sits before the
    collection button); with All, its collection picker (`f`) lists the groups under each
    playlist's name.
  - **Do:** at a tiled half (790 pt), with text size 16, look at the same screens.
  - **Expect:** the bar is a single "● B ▾" button that opens the picker; nothing overflows.
  - **Do:** pick B on Live TV, then disable B in Settings ▸ Playlists.
  - **Expect:** a toast "B is disabled — showing All playlists"; Live TV shows everything; with
    one playlist left the bar and "Alt+P playlist" in the status bar disappear.
- [ ] **Telling playlists apart and "Show in All playlists":**
  - **Do:** add two Xtream logins (or two URL playlists) without changing their colour.
  - **Expect:** the second gets a different colour from the first. If two playlists still look
    the same (same glyph and colour), Settings ▸ Playlists says "A and B look the same on
    cards and in lists" and offers "Give each its own colour"; nothing changes until it is
    pressed, then the second gets a new colour.
  - **Do:** add free channels (Settings ▸ Playlists ▸ Add free channels…), edit that playlist,
    turn off "Show in All playlists", Save.
  - **Expect:** "Saved …" without a refresh; Live TV ▸ All playlists no longer lists its groups
    or channels, but its own tab still shows them, and a channel of it in your favourites or
    History still shows there in All.
- [ ] **Across playlists in All:**
  - **Do:** on Live TV ▸ All playlists, open a provider group; then on Movies type a playlist's
    name in the category filter, open a category and press `Ctrl+PgDn`.
  - **Expect:** channel rows name their playlist at the right in its colour; the header reads
    "All playlists › Sports · B"; the filter finds that playlist's categories; after
    `Ctrl+PgDn` the filter is gone and the grid shows the new playlist's All movies.
  - **Do:** with Live TV set to B, search a channel of A and choose "Open in guide".
  - **Expect:** a toast "Showing all playlists" and the guide focused on that channel.
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
- [ ] **Explicit content (a playlist with adult categories, e.g. `XXX`, `18+`, `For Adults`,
  or Xtream categories flagged `is_adult`):**
  - **Do:** start with no PIN and *Show explicit content* off.
  - **Expect:** the adult categories and their channels and titles are missing everywhere:
    Live TV groups and *All channels*, the guide grid and its collection picker, Movies and
    Series categories and grids, Home rails (Continue watching, Favourites, Recently added),
    search (`/` and Ctrl+K, guide section too), the counts in group panes and on the playlist
    tabs, and ↑/↓ zapping. A playlist that only has adult movies has no Movies tab. *Adult Swim*
    and *Young Adult* categories still show. Settings ▸ Parental says "N categories are hidden
    as explicit" and names none.
  - **Do:** turn on Settings ▸ Parental ▸ *Show explicit content*.
  - **Expect:** a menu offers *Set a PIN first (recommended)*, which moves to the New PIN field,
    or *Show without a PIN*. After *Show without a PIN*, everything appears within a second with
    no flicker. Settings ▸ Parental lists each explicit category with the reason (its name, the
    playlist, you).
  - **Do:** right-click an ordinary group and choose *Mark as explicit*. Right-click an adult
    one and choose *Not explicit*. Turn *Show explicit content* off, then refresh the playlist.
  - **Expect:** the marked group disappears and the cleared one stays, before and after the
    refresh.
  - **Do:** set a PIN. Lock now. Run `tuner --play <ref of an adult channel>`.
  - **Expect:** it asks for the PIN and doesn't name the channel. The right PIN plays it, and
    the adult categories appear.
  - **Do:** with an adult channel playing (MPRIS widget and pop-out showing its title),
    Settings ▸ Parental ▸ *Lock now*, or wait out *Lock again after*.
  - **Expect:** in the same moment playback stops ("Playback stopped — parental control is
    on"), the categories are gone, and no toast, notification, MPRIS title, pop-out strip or
    jobs entry shows an adult title. `grep -i '<title>' /tmp/tuner-check/state/tuner.log` finds no
    INFO or higher line with it written while locked. A reminder set on an adult channel stays
    silent while locked.
  - **Do:** with a PIN set and the session locked, look at a group, channel and movie you
    locked with Ctrl+L, then enter the PIN.
  - **Expect:** while locked they stay listed, dimmed, with the lock glyph; after the PIN they
    paint normally (the lock glyph stays).
  - **Do:** watch half an adult movie, disable its playlist, lock. Open Home and Settings ▸
    Orphans (after a refresh dropped an adult favourite).
  - **Expect:** Continue watching doesn't show it; Orphans says "N more hidden as explicit"
    and names none.

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
- [ ] **No "application not responding" on a hidden workspace, without the snippet:**
  - **Do:** with the `render_unfocused` rule still commented out, play a live channel, switch
    to another workspace, open a browser there and use it for 2 minutes. Then check the main
    thread: `eu-stack -p $(pgrep -x tuner) | sed -n '/^TID/,/^$/p' | head -20`.
  - **Expect:**
    - Hyprland never shows its "… is not responding" dialog for Tuner.
    - The main thread waits in `epoll_wait`/`poll` (winit's event loop), not in
      `eglSwapBuffers` or `wl_display_dispatch_queue`.
    - Back on Tuner's workspace the UI answers at once.
- [ ] **CPU while visible:** **Do:** watch `top -p $(pgrep -x tuner)` with Tuner idle on
  Live TV (nothing playing), then playing a live channel fullscreen. **Expect:** idle ≈ 0 %
  (no busy loop); while playing the same level as before this version, and video motion as
  smooth.
- [ ] **Pop-out across workspaces:**
  - **Do:** play a live channel and press `o` in the player. Switch through workspaces 1–4,
    open a browser on one of them. Zap with ↑/↓, change the volume with `+`/`-`, pause with
    Space, hover the video.
  - **Expect:**
    - The window floats at 480 × 270 in the bottom-right corner (inside the bar, 10 px from
      the edges like tiled windows' gaps), with Omarchy's rounded `pop` look, and stays on
      top on every workspace.
    - Only the video shows; right after popping out, hovering or a key shows the strip
      (channel, programme, volume and "o · Esc back").
    - All the keys work; Esc first closes a side panel (`c`), then puts the window back.
    - `f` puts the window back and makes it fullscreen; `p` puts it back with the mini player;
      `q` stops and puts it back.
- [ ] **Back to tiled, remembered place:**
  - **Do:** pop out, drag the pop-out to the top-left and make it bigger (Super+drag), then
    press Esc. Pop out again. Then change Settings ▸ Player ▸ Pop-out corner and pop out again.
  - **Expect:**
    - Esc (also `o`, or a double-click) tiles the window again on the current workspace and
      shows the screen you popped out from (the player, or Live TV with the mini player
      after Ctrl+Shift+O there).
    - The second pop-out comes back at the dragged place and size; after changing the corner
      it goes to that corner at the preset size.
    - Popped in without moving it, the next pop-out on a different monitor still goes to that
      monitor's corner.
- [ ] **Movie ending while popped out:** **Do:** pop out a movie near its end and wait.
  **Expect:** the window stays popped out and shows "Nothing playing"; Esc tiles it and shows
  the movie's page (Esc there goes to the screen before it, not an empty player).
- [ ] **Super+O switches to video-only by itself:**
  - **Do:** with Tuner focused on Live TV, press Super+O. Then press Super+O again. Then press
    Super+O once more and resize the window bigger than 1300 × 900.
  - **Expect:**
    - The 1300 × 900 pinned window shows the video-only player on every workspace. With
      nothing playing it keeps the full interface (pinned).
    - The second Super+O brings the full interface back.
    - A pinned window bigger than 1300 × 900 shows the full interface; Esc/`o` in the player
      then unpins and tiles it.
- [ ] **Pop-out outside Hyprland:** **Do:** run `env -u HYPRLAND_INSTANCE_SIGNATURE tuner
  --profile-dir /tmp/tuner-nohypr` and press Ctrl+Shift+O. **Expect:** a toast says the
  pop-out needs Hyprland; nothing else changes.
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

- [ ] **Memory upkeep (big library):**
  - **Do:** with a large Xtream library (100k+ titles) and debug logging on, play a 4K channel,
    open and close search, refresh the playlist twice, hide the window for a minute, then stop.
    Record `RssAnon` and `VmSwap` from `/proc/$(pgrep -x tuner)/status` before and after.
  - **Expect:**
    - No frame hitch when a refresh lands (the old library is freed on the "tuner-mem" thread).
    - The log shows `memory: trimmed after …` lines for the library swap, "search closed",
      "video off" and "playback stopped", each taking well under 100 ms.
    - RSS returns to roughly the library plus what mpv needs for the stream; it doesn't climb
      with each refresh.

- [ ] **All-day watch through provider outages (field report 2026-10-11):**
  - **Do:** play a live channel of a restreaming Xtream provider for a working day (6 h+) at
    default settings, with the log at INFO. If no outage happens on its own, pull the network
    cable (or `nmcli networking off`) for 3 minutes, twice, an hour apart.
  - **Expect:**
    - Playback never ends in an error and the channel is never dimmed. During an outage the
      overlay reads "Reconnecting… attempt n" (no "/8") with a Stop button, and the picture
      comes back on its own within ~30 s of the network or provider returning.
    - The log has `player: … Playing → Reconnecting(…)` and `player: recovered after n attempts
      in t` for each outage, `health:` lines every 10 minutes, and no "Error reading HTTP
      response: End of file" lines at INFO. No INFO line contains a channel title or a stream
      address.
    - A channel whose provider answers "Cannot read …" on first play shows "The provider can't
      deliver this channel right now" with the provider's words after ~10 s.
- [ ] **Zap on a one-connection account:**
  - **Do:** with an Xtream account limited to 1 connection, play a channel for a minute, then
    zap to another channel of the same playlist; repeat ten times, including Stop followed by
    playing the same channel again at once.
  - **Expect:** every zap and restart ends up playing (a refused first attempt is retried every
    3 s, "Reconnecting… attempt n", for up to ~45 s). A channel the provider really refuses
    (expired account, a second device using the connection) shows "The provider refused the
    stream" after ~45 s instead of retrying forever.
- [ ] **Memory over a day:**
  - **Do:** during the all-day watch above (4K HEVC in software if available, a 100k+ title
    library), note the `health:` lines' `rss`, `anon` and `swap` every hour; refresh the
    playlist and open/close search a few times.
  - **Expect:** RSS + swap stays under ~0.7–0.8 GB while playing 4K and falls to ~0.4 GB with
    video off or stopped; it doesn't climb hour over hour (was ~1.1–1.3 GB after 6.5 h).
- [ ] **Workspace switch picture:**
  - **Do:** while a 4K HEVC channel plays (software decoding), switch to another workspace for
    5 s, come back; then for 30 s, come back. Repeat a few times.
  - **Expect:** after the 5 s visit the picture continues with no broken frames (video is only
    switched off after 10 s hidden). After the 30 s visit the picture returns within a couple
    of seconds; any blocky frames clear at the next keyframe.

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
- [ ] **Doubtful "4K?" claims (a provider that over-labels 4K):**
  - **Do:** in a playlist whose "4K" channels mostly play at HD (stream info, `I`, shows the
    size and a `Quality` line), play two of its "4K" channels and one other channel until each
    shows a picture. Restart Tuner (or refresh the playlist). Hover the badge of another, not
    yet played "4K" channel in the Live preview; open stream info on one you played.
  - **Expect:** the played ones show their real class (HD). The other "4K" channels of that
    playlist now read **4K?** everywhere a badge shows; the tooltip says "The provider says 4K;
    not verified…". A "4K" channel of a different playlist stays "4K". Stream info on the
    played "4K" channel says "Quality  named 4K · plays HD". Play one of the
    "4K?" channels: it shows its measured class at once. If two of the played "4K" channels
    later play at 4K (re-play them), the "?" goes after the next restart. The "4K?" never
    appears straight after playing — only after a restart or refresh (by design, §13.0.1).
    An "8K EXCLUSIVE" event slate shows no badge.
- [ ] **Setting:**
  - **Do:** Settings ▸ Interface ▸ Quality badges off, then on.
  - **Expect:** the badges disappear from every list at once and come back; the Live preview
    and the player OSD still show the stream's quality.

## 16. Smart names (real provider)

Use a provider with decorated names (e.g. "UK: ITV 4 4K ◉", "US ★ HGTV HD", "VIP: … ᴿᴬᵂ",
"IN-PREM: …", "|UK| …", "UK ❖ General" categories), smart names on (the default).

- [ ] **Existing library, no refresh:**
  - **Do:** start this build on a profile from the previous version (back it up first) and
    open Live TV without refreshing.
  - **Expect:** the first start may take about a second longer (the names are re-cleaned once);
    then channel rows read "ITV 4", "HGTV", "PRO TV" — no "US ★", "VIP:", "◉", "ⓧ", superscript
    or glued "4K" left — with the quality as a badge; categories read "General", not "UK ❖
    General". Event names keep their fields ("NEXT | Lyon vs Nice | 15:55"). A playlist with
    smart names off keeps the provider's names.
- [ ] **Look-alikes stay apart:** in a category with "FI: TV5" and "FI: TV5 ᴿᴬᵂ" (or the same
  channel under two country codes) the rows read "TV5" and "TV5 · RAW" ("… · PL" / "… · AF");
  categories "GENERAL ᴴᴰ/" and "GENERAL ʰᵉᵛᶜ" read "General · HD" and "General · HEVC";
  "UK ❖ SPORTS 4K" reads "Sports 4K". Stream info (`I`) on a cleaned channel shows a
  "Provider" line with the provider's own name.
- [ ] **Names that keep their symbols:** "E! News", "AT&T …", "Sky Sports+", "Channel 4 +1",
  "5 USA", "BBC One (Scotland)" read as written (search finds them by those words too).
- [ ] **Toggle per playlist:** Edit ▸ smart name cleanup off, refresh: the provider's raw names
  come back (badges stay); on again, refresh: cleaned again. Favourites, hidden channels and
  renames survive both.
- [ ] **Countries:** Settings ▸ Playlists ▸ (playlist) ▸ *Countries & languages…* lists "US ★ …",
  "UK ❖ …", "ALB ★ …" categories under their countries (United States, United Kingdom, Albania).
- [ ] **Filter typing:** in a type-to-filter list and in Search (`/`), type a word, AltGr+Space
  (no-break space) and another word, then Ctrl+Backspace twice: words go one at a time, no crash.

## 17. Mouse only (every screen)

Use the keyboard only to type text (and where an item says so). Use a mouse with back / forward
side buttons, at
**Wide**, **Tiled** and a **Compact** window (≈ 420 pt wide, e.g. a narrow float), text 12.

- [ ] **Side buttons walk the Esc ladder:**
  - **Do:** in Live TV click the channel filter and type a few letters, then press the mouse
    **back** button; press it again. Repeat in Movies (grid filter),
    with Help open (F1), in the player with the controls focused (Tab, then
    the mouse back button), and popped out. Press the **forward** button anywhere.
  - **Expect:** each back press does exactly what Esc does there: clears the filter first, then
    leaves the screen; closes Help; leaves the controls (banner stays), then hides the banner,
    then leaves the player; popped out, puts the window back. Forward does nothing.
- [ ] **Back buttons on every page:**
  - **Do:** open a movie from Home, a series from Search, a channel's EPG mapping from Settings ▸
    EPG, Logs from Settings, Settings ▸ Hidden items from Movies' empty state, the guide from Home
    (Ctrl+G on a tile, then put the keyboard away), and at Compact a Settings page, an EPG
    candidates pane and a Live group. Click each `‹ Back to …` button; click the lit sidebar tab
    on a movie page.
  - **Expect:** every page Esc can leave has the button, labelled with where it goes, and it goes
    exactly where Esc would (the movie page → Home, not Movies). A long category name is cut with
    "…". At Compact, `‹ Settings` above a page shows the page list; clicking a page opens it.
    In a Settings source editor, `‹ Playlists` closes it; a text field being edited keeps what
    was typed.
- [ ] **Dialogs close by mouse, edits are safe:**
  - **Do:** open Help, Jobs, Search, Resume (a half-watched movie), the tracks panel (click the
    Audio control in the player): close each with its ✕, then reopen and click outside. Open
    Rename (a favourites list) and Edit information (right-click a channel → Edit information):
    click outside untouched; then type a letter and click outside.
  - **Expect:** ✕ and an outside click close them. Untouched Rename / Edit information close; after
    typing, the outside click only flashes the card border (tooltip "Unsaved changes — Cancel or
    Esc to discard"), and Cancel or Esc closes.
- [ ] **Right-click everywhere:**
  - **Do:** right-click a channel row, a group, a poster, a Home tile, a guide cell, a guide
    channel, an episode, a detail page's header, a search hit, a Logs row, an EPG mapping channel,
    the player's video.
  - **Expect:** each opens the same menu as Shift+F10 there, hanging from the pointer (kept on the
    window at the right edge). A search hit's menu starts with "Play now", and choosing an item
    acts on that hit. The player menu lists Channels…, Now / next…, History… (live) through Stop
    and close.
- [ ] **Hover, pointer, double-click:**
  - **Do:** hover buttons, rows, posters, toggles, the sidebar; hover the Settings page list while
    a page has the keyboard cursor; move the mouse over onboarding's choices (Settings ▸
    Playlists ▸ Add). At Tiled and Compact, double-click a Movies category; at Compact,
    double-click a Settings page name.
  - **Expect:** a pointing hand over everything clickable, never over plain text or the scrim.
    Never two rows lit as the cursor at once. The double-click opens the category / page and
    nothing else (no movie page, no toggled setting).
- [ ] **Jobs, toasts, bulk hide, tracks:**
  - **Do:** start a playlist refresh, open Jobs, click a running task's ✕. Hover a toast and click
    its ✕. Mark two groups (right-click → Mark for hiding) and use the footer's `[Hide 2]` and
    `[Clear]`. In the tracks panel click AUDIO / SUBTITLES, the ‹ › around Delay and Size, and the
    value.
  - **Expect:** the task stops; the toast goes; the footer buttons do what their keys do. The
    open tab is underlined; ‹ › step the value (never backwards), a click on the value resets it.
- [ ] **Volume slider (full player):**
  - **Do:** at Wide play a channel; move the mouse to show the controls. Click the speaker; click
    the slider's middle; drag it; scroll over it one notch at a time; scroll over the speaker;
    mute, then click the knob. Repeat at Tiled (790 pt) and in a window under 700 pt.
  - **Expect:** Wide: speaker · slider (mark at 100 %) · "80 %" at the right end of the controls;
    the speaker glyph follows the level; click mutes ("Muted", greyed fill) and its tooltip says
    Mute / Unmute (m); click sets, drag scrubs smoothly (no jump back), one notch = ±5 exactly
    (also over the speaker — never ±10); clicking while muted unmutes. Tiled: a shorter slider,
    controls drop to glyphs before the volume shrinks. Under 700 pt: the speaker alone (wheel over
    it steps), the level in the top bar, previous / next still in the row.
- [ ] **Player by mouse:**
  - **Do:** click Play / pause, then press ↑. Click the top-left ←. Open Channels from the row,
    click two channels, click its ✕; open it again and click the video beside it. Rest the
    pointer on the bottom bar without moving it for 15 s. Stop the stream's network (or use a
    dead channel) and use Retry / Last channel. Let an episode end and click Play now / Cancel.
    Stop playback and click anywhere on "Nothing is playing".
  - **Expect:** the click pauses and ↑ still zaps (no controls focus). ← leaves to the mini player.
    The panel tunes each clicked channel and stays open; ✕ and the outside click close it. The
    banner stays while the pointer moves on a bar, then times out about 10 s after it stops, and
    the pointer hides over the video (never over a button). Retry reconnects; the next-episode
    buttons show their keys ("Play now · Enter"). The idle screen's button reads "‹ Back to
    Live TV" (or wherever it goes).
- [ ] **Pop-out strip and mini player:**
  - **Do:** pop the player out (Small 480 × 270 and Medium 640 × 360). Hover it; click each strip
    button; drag the video; double-click it. Stop playback while popped out and use the card's
    button. In the full window, click the mini player's pop-out button.
  - **Expect:** with the pointer over it the strip shows previous / play / next (skips for VOD),
    the speaker, a slider at Medium only, back to the full window and stop; at Small the title and
    position keep readable room (previous / next drop out below 420 pt). A slider change flashes
    the level. The drag moves the window; a click on a strip button acts without moving the window. The
    mini player's button pops the player out.

## 18. Several playlists: stacks in All (real catalogue)

Two Xtream playlists with overlapping movies and series (the real catalogue: ~300k movies), a
PIN set, English audio first. Every check in *All playlists* unless it says otherwise.

- [ ] **Stacks:**
  - **Do:** open Movies ▸ All movies, then a big category (EN Box Office), then Series. Sort by
    name, by recently added. Watch the scope bar's counts.
  - **Expect:** one card per title — card edges behind the poster, the copy count bottom-right,
    `TV + TV 2 · 2021` (or `TV +1` when narrow) under the title; the poster and title of the
    English copy where there is one; the list's sort holds (a stack sits where its first copy
    sorts). *All playlists* counts cards (fewer than the two playlists added up). Scrolling is
    as smooth as before; opening All movies takes no longer than before. No obviously different
    films share a card (spot-check 20 stacks; namesakes like *Leo* 2023 are the known case).
- [ ] **Fan-out:**
  - **Do:** on a stack card press Space, move onto a copy, press Space again; fan out two stacks
    and press Esc; click a count badge twice; type a filter and press Space.
  - **Expect:** the copies follow the card, each with `TV 2 · EN · 4K`, the head card names its
    own copy; Space on a copy folds its stack, the cursor stays on the stack; Esc folds
    everything first (then goes back); the badge does the same; while filtering Space types a
    space. The hint line shows `Space copies · Ctrl+O play from` on a stack card.
- [ ] **Copies on the page, Play from:**
  - **Do:** Enter on a stack card; look at the Copies row. Click a non-first copy; press Play.
    Ctrl+O and pick another copy. Open the same title from Search, from a category of TV 2 and
    from Favourites; press Play each time. Fan a stack out, Enter on TV 2's copy, Play.
  - **Expect:** one mini-card per copy with playlist chip, language, quality, progress and a
    status (*plays first*, *locked*, *failed 2 h ago* once a copy failed); "Matched by title and
    year (2021)" under them. The clicked copy plays and the Play button names its playlist.
    From Search / a category / Favourites, Play plays the same copy as Shift+Enter on the stack
    card (the pinned one if any). A copy opened from the fan plays itself.
- [ ] **Which copy plays, resume:**
  - **Do:** Shift+Enter on a stack with no progress; stop after a minute; Shift+Enter again;
    *Always play from* a copy in its menu, Shift+Enter; switch Settings ▸ Interface ▸ *Which
    copy plays first* to Best quality.
  - **Expect:** the English copy of the first playlist plays (never a CAM copy when a clean one
    exists, never an `IN-EN` Hindi dub before English); then "Resume 1:00" resumes that copy;
    the pinned copy plays first and its mini-card says *always plays first*; Best quality picks
    4K within the language.
- [ ] **Fallback:**
  - **Do:** pick a stack where one copy is dead (or block the first playlist's host in
    `/etc/hosts`) and Shift+Enter; then let a copy fail mid-film (pull the network after a
    minute); press Ctrl+O while a stacked movie plays normally.
  - **Expect:** before the first frame Tuner plays the next copy once and toasts "TV couldn't
    play Dune — playing from TV 2"; a second failure shows the error (no loop). Mid-film nothing
    switches by itself: after the reconnects the error card offers "Play from TV 2 at 41:20"
    (Ctrl+O too). During playback Ctrl+O lists the other copies and the one picked starts at
    the current position. A copy that failed shows "failed … ago" and plays last next time,
    also after a playlist refresh.
- [ ] **Not the same title, Undo:**
  - **Do:** on a page, a copy's menu ▸ Not the same title; click the toast's Undo. Do it again,
    open another page, then go to Settings ▸ Hidden ▸ Titles you separated or pinned and press
    Enter on it. Remove a stack from Favourites (Ctrl+F) and click Undo after opening a page.
  - **Expect:** the copy leaves the stack (the card count drops), Undo puts it back; the Hidden
    list shows the split and Enter combines it again; a refresh keeps a split. Each toast's Undo
    reverts its own action, from any screen.
- [ ] **Locked copies:**
  - **Do:** lock one copy of a stack (fan out, Ctrl+L on the copy), lock the session, look at
    the card and the page; lock every copy; Ctrl+L on the stack card; Shift+Enter.
  - **Expect:** a locked copy stays in the stack, dimmed with the lock, and asks for the PIN
    when picked; the card shows an unlocked copy. Every copy locked: the card is dimmed (edges
    too) with the lock and asks for the PIN. Ctrl+L on the stack locks / unlocks every copy;
    Shift+Enter never plays a locked copy while the session is locked.
- [ ] **Hide, rename, favourites on a stack:**
  - **Do:** Ctrl+H on a stack card; Undo. F2 on a stack whose one copy you renamed before;
    Enter, Enter. Ctrl+F on a stack, then Ctrl+F again; Ctrl+W; Delete in Continue watching.
  - **Expect:** Hide hides every copy ("Hid Dune in TV and TV 2"), Undo brings them back. Rename
    warns about the copy with its own name and renames all on the second Enter. Ctrl+F adds the
    copy that would play, the second Ctrl+F removes every copy; Delete forgets every copy's
    progress. Each with Undo.
- [ ] **Filters, hidden and explicit content:**
  - **Do:** with the category filter (UK + English) on one playlist, open All movies; hide a
    category holding one copy of a stack; mark a category explicit; lock the session; unlock.
  - **Expect:** filtered-out, hidden and explicit copies never join a stack (counts drop by
    them); explicit copies leave stacks the moment the session locks and come back after the
    PIN.
- [ ] **One playlist, Show each copy, Home and Search:**
  - **Do:** pick TV 2 in the scope bar; set *Same title in several playlists* to Show each copy;
    back to Combine; look at Home's rails and search a stacked title.
  - **Expect:** one playlist combines nothing (a title also in TV says `+1`, tooltip "Also in
    TV"); Show each copy makes every copy a card again at once (no rebuild); Home's rails and
    Search show one tile / row per stack with both playlists; Shift+Enter there plays the copy
    that plays first.

## 19. Use Tuner from Claude (MCP)

Default profile, Claude Code installed, `claude mcp add --scope user tuner -- tuner mcp` done
once (`claude mcp list` shows `tuner` connected).

- [ ] **Not running → status doesn't start it:** quit Tuner. Ask Claude "what's playing?".
  **Expect:** Claude says Tuner isn't running; Tuner does not open.

- [ ] **Not running → starts:** quit Tuner. Ask Claude "what's on now?".
  **Expect:** Tuner opens on the current workspace within a few seconds; Claude lists programmes
  with channels and times in your local time.
- [ ] **"Touring cars is on" (or any live sport / show in your guide):**
  **Expect:** Claude finds the live programme and Tuner plays the channel fullscreen and comes to
  the front. Backspace returns to the previous channel.
- [ ] **Upcoming → automatic switch:** ask for something starting in 2–3 minutes and accept the
  switch. **Expect:** Claude reports the start time; 10 s before, Tuner toasts "Switching to …
  in 10 s" with Cancel; at the start it switches by itself. Repeat and press Cancel: no switch.
  Settings ▸ Reminders lists the pending switch with its own label until then.
- [ ] **Mood pick:** "something light under 2 hours, I haven't seen". **Expect:** Claude names a
  film from a fitting category with its runtime; on "play it" Tuner starts it (resuming saved
  progress for a started film).
- [ ] **Catch-up:** ask for a programme that ended today on a channel with catch-up.
  **Expect:** it plays from the archive; on a channel without catch-up Claude says it can't.
- [ ] **Parental lock:** set a PIN, lock a channel, lock the session. Ask Claude to play that
  channel. **Expect:** Tuner shows its PIN prompt; Claude tells you to type the PIN in Tuner
  and does not ask for it. With explicit content present and hidden: ask Claude to search for an
  explicit channel or category by name — nothing found, no counts that give it away. Unlock in
  Tuner → it is found.
- [ ] **No addresses:** ask Claude for the stream URL of a channel. **Expect:** it can't give
  one (no tool result contains a URL).
- [ ] **Control:** "pause", "resume", "volume to 30", "stop". **Expect:** each happens, also
  while Tuner sits on another workspace.
- [ ] **Window hidden:** move Tuner to another workspace, ask "what's on BBC One at 9?".
  **Expect:** an answer without Tuner coming forward; "play it" brings it forward.
- [ ] **Outlives Claude:** with Tuner started by Claude (first check), quit Claude Code (and
  close its terminal). **Expect:** Tuner keeps running and playing.
- [ ] **Old instance:** with a Tuner from before this feature running (or simulate: keep an old
  build open), ask Claude anything. **Expect:** Claude says to restart Tuner; after a restart it
  works.

