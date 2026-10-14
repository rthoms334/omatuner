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
      `/usr/lib/systemd/user/tuner-rearm.service` and its link
      `/usr/lib/systemd/user/graphical-session.target.wants/tuner-rearm.service`,
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
    - The tarball holds `tuner`, `tuner.desktop`, `tuner.svg`, the snippets,
      `systemd/tuner-rearm.service`, `README.md` and `LICENSE`.
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
    - Each poster has a small chip in its top-left corner with its own playlist's short code
      in the playlist's colour ("NW" for Northwind; the lock and watched chips stay
      top-right); hovering the chip shows the playlist's name.
    - The line under each card's title is only "2019 · ★ 7.1" — no playlist name, even a long
      one. With the cursor or the pointer on a card the status bar starts with "Provider A ·
      2019 · ★ 7.1".
    - Rows (channels, favourites, history, reminders, search results), channel tiles and the
      player panels show the code chip at the right and the channel's name gets the room;
      detail pages (an Xtream movie too) and the preview show glyph + name; the player's top
      bar shows "● name" left of the clock.
    - Group rows in Live and the Movies / Series categories show the code chip, so the two
      playlists' "Action" (or "Sports") rows can be told apart; typing a code in the category
      filter finds that playlist's categories.
    - On the light theme the yellow playlist's code is drawn in the text colour inside a
      thicker yellow outline; nothing is unreadable.
- [ ] **Playlist codes (Settings ▸ Playlists ▸ edit):**
  - **Do:** name two playlists "TV" and "TV 2", a third "Northwind"; open each editor.
  - **Expect:** the Short code field shows the derived code as its placeholder ("TV", "TV2",
    "NW"); every card and row uses those codes; no two playlists share one.
  - **Do:** type "nw" into TV's Short code and press Save; then type "tv!" and "abcd".
  - **Expect:** "Northwind already uses NW" next to the field; the field takes only letters
    and digits, upper-cased, at most 3. A free code (e.g. "MAX") saves without a refresh and
    the cards show it at once; emptying the field goes back to the derived code. The
    notice ("… share a colour") still appears for two playlists with the same glyph and
    colour.
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
  - **Expect:** the second gets a different colour from the first. If two playlists still share
    a colour (same glyph and colour), Settings ▸ Playlists says "A and B share a colour" and
    offers "Give each its own colour"; nothing changes until it is
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

- [ ] **A failed start is visible** (DESIGN §21.7):
  - **Do:** quit Tuner. Back up the library:
    `cp ~/.local/share/tuner/tuner.db /tmp/tuner.db.bak`. Note its version with
    `sqlite3 ~/.local/share/tuner/tuner.db 'PRAGMA user_version'` (13 for 0.2.x), then set
    `sqlite3 ~/.local/share/tuner/tuner.db 'PRAGMA user_version=99'`. Start Tuner from
    Super+Space → Apps. Afterwards put the version back
    (`sqlite3 ~/.local/share/tuner/tuner.db 'PRAGMA user_version=13'`) and start Tuner again.
  - **Expect:**
    - No window. A notification *Tuner couldn't start* says "Your library was last opened by
      Tuner ‹this version› (database version 99; this is 13). Install Tuner ‹this version›
      again, or a newer one." (the version this build recorded in the database), and it shows
      no address or password.
    - `~/.local/state/tuner/tuner.log` ends with `ERROR tuner: Tuner exits: database schema v99
      is newer than this build (v13), last opened by Tuner ‹version›; refusing to open`.
    - Before putting the version back, `dbus-run-session -- tuner` in a terminal (a session
      bus without a notification server) prints `Error: database schema v99 …`, adds the log
      line and exits 1 (`echo $?`) after at most a few seconds, without a panic.
    - After putting the version back, Tuner opens with the library unchanged.

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
    - `hyprctl clients` shows class `tuner` (`tuner-popout` for a Tuner started straight into
      the pop-out, see §27).
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
- [ ] **One rule for every playlist (field report: foreign stacked cards):**
  - **Do:** a profile from v0.1.0 with two Xtream playlists ("TV" with English chosen in
    *Countries & languages…*, "TV 2" never asked). Start this version; open Movies ▸ All
    playlists; open a stacked title (e.g. "Dracula") and its copies.
  - **Expect:** TV 2's German, French, Italian, Greek… categories are gone without doing
    anything; no card shows a foreign copy on top and the copy lists are English (plus
    untagged ones). Settings ▸ Playlists: both rows say "Showing …"; the editor's
    *Categories…* reads "… · the choice every playlist shares". Settings ▸ Hidden has one
    "Your playlists: hidden by the category filter" section with an "N titles in other
    languages" row.
  - **Do:** add a third playlist with several countries.
  - **Expect:** no *Choose what to show* step; a toast says it shows the countries you chose;
    its foreign categories are hidden at once.
  - **Do:** open its *Countries & languages…*, tick **Only for <playlist>**, tick everything,
    Enter. Then reopen, untick **Only for**, Enter.
  - **Expect:** only that playlist shows everything ("· own choice" on its row); the others
    are unchanged; unticking "Only for" shows the shared ticks again (not the playlist's), and
    Enter makes it follow the shared choice without changing it for the other playlists.
  - **Do (upgrade):** start this build on a v0.1.0 profile where only one playlist had a
    country choice.
  - **Expect:** one toast "Your choice of countries from <playlist> now applies to all your
    playlists · titles in other languages hidden · Settings ▸ Hidden items"; not on the next
    start.
- [ ] **Titles in other languages:**
  - **Do:** with the English choice, look at "EN ✪ MULTI-SUB", "CA ✪ QUEBEC …", an untagged
    Icelandic-named category, and Series ▸ All for Arabic-named categories ("مسلسلات …"); favourite a "DE - …" title and start an
    "FR - …" one first (with the option off), then turn **Also hide titles in other
    languages** back on.
  - **Expect:** Arabic-named categories (and "VIP ✪ كأس العالم …") are hidden as Arabic;
    Dutch, Québec, Icelandic, "EX - …" and bare "SUBS - …" titles are gone from those
    categories (an emptied untagged category shows no titles); the favourite and the started
    title stay; multi-audio and untagged titles stay. Add `nld` to Settings ▸ Player ▸ audio
    languages: Dutch titles come back.
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
- [ ] **Hover, pointer:**
  - **Do:** hover buttons, rows, posters, toggles, the sidebar; hover the Settings page list while
    a page has the keyboard cursor; move the mouse over onboarding's choices (Settings ▸
    Playlists ▸ Add).
  - **Expect:** a pointing hand over everything clickable, never over plain text or the scrim.
    Never two rows lit as the cursor at once.
- [ ] **One click = Enter:**
  - **Do:** click once on: a Live channel (Wide, then Tiled), a group, a poster, a Home tile, an
    episode, a guide programme and a guide gap, an Up next row, a Logs line (twice), an EPG
    mapping channel and a candidate, a Settings toggle row.
  - **Expect:** each does what Enter does there: the channel plays in the preview (Wide) or opens
    the player, the group opens, the title's page opens, the episode plays, the programme's card
    opens / the gap's channel plays, the row plays, the line opens then closes, the candidates
    take the cursor / the guide channel is assigned, the setting flips. Hover alone never acts.
- [ ] **Double-click = one click:**
  - **Do:** double-click a Live channel at Tiled (the player opens under the pointer), a Movies
    category at Tiled and Compact, a Settings page name at Compact, a poster, a guide programme,
    a context-menu item over a list, a category tick and a Settings toggle. Then double-click the
    player's video, and the video-only pop-out.
  - **Expect:** each acts once: no fullscreen toggled by the player that just opened, no movie page
    from the grid that replaced the categories, no setting flipped back, no row activated under
    the menu. On video a double-click still toggles fullscreen / pops the window back in.
- [ ] **Focusing click:** with Hyprland's `input:follow_mouse = 2` (click to focus), focus
  another window, then click a channel in Tuner. **Expect:** the window takes focus and nothing
  plays; the next click plays. With the default `follow_mouse = 1` (flick the pointer onto the
  sidebar from the window to its left and click at once), and with `3` (the keyboard stays where
  it is), a click acts at once; with `3`, dragging a scrollbar or the volume slider works too.
- [ ] **Slow double-click:** double-click a Movies category at Tiled a little slower than usual
  (about 0.4 s between clicks). **Expect:** the category opens and no title opens.
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
    the playlists' code chips top-left (`TV` `TV2`, at most three then `+n`; fewer when
    narrow) and `2021 · ★ 7.1` under the title; the poster and title of the
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
    open another page, then go to Settings ▸ Hidden ▸ Titles you separated, combined or pinned and press
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
  - **Do:** with the category filter (UK + English, shared by every playlist), open All movies; hide a
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

## 19. Keyboard: arrows between panes (Omarchy style)

The keys are covered by tests; these checks are about how it feels with real key repeat, at each
layout. Use only the keyboard.

- [ ] **Settings:**
  - **Do:** open Settings ▸ Interface, move down to a choice (Density), a toggle (Channel
    numbers) and a number (Preview delay) and press ← on each. In the page list press ↓ to
    Favourites, ↑ back, then →. On the same rows press →, Shift+→, Shift+← (and L / H). Hold ←.
  - **Expect:** ← always lights the page list and never changes a value; ↓/↑ switch the page;
    → returns to the same page. → and Shift+→ raise a value / pick the next option, Shift+←
    lowers it; a toggle turns on with → and off with Shift+←, Enter flips it. Holding ← stays
    in the page list. The hint line reads "→ change"; `?` lists "Back to the page list".
- [ ] **Settings source editor:**
  - **Do:** open a playlist's editor, change its colour with Shift+→, press ← and then ↓ in the
    page list (hold ↓ too). Press Esc, then ← and ↓. Also click another page, and press ← then
    Esc, while the change is unsaved.
  - **Expect:** ← keeps the editor open; with the change unsaved, ↓ (the click, or Esc in the
    page list) stays on Playlists with one "Unsaved playlist changes: Enter on Save, or Esc to
    discard" and lights the editor again (holding ↓ then moves through its rows); the next Esc
    discards the change, and after ← ↓ moves on.
- [ ] **Live TV (Wide, Tiled, Compact):**
  - **Do:** in the channel list type a few letters, then press ←. Press → (or Enter) on a group.
  - **Expect:** ← moves the highlight to the groups (Tiled: the drawer opens; Compact: the
    groups show) and the channel filter is still there when you come back. → / Enter open the
    group in the channels.
- [ ] **Movies and Series (Wide, Tiled):**
  - **Do:** in the grid press ← until the first column, then once more. Type a filter that
    matches nothing and press ←. In the categories press →.
  - **Expect:** ← walks the posters, then lights the categories (Tiled: the drawer opens), also
    while filtering — the filter stays — and on an empty grid. → opens the category in the grid.
- [ ] **No change where ←/→ mean something else:**
  - **Do:** press ←/→ in the guide, on a detail page, in the player (live and a movie), in a
    text field (Settings, a rename), in onboarding and in Edit information. In a player side
    panel (`c`) type a letter, then press ←.
  - **Expect:** the guide moves between programmes (← goes into the past), detail pages walk
    their buttons, the player zaps / seeks, text fields move the cursor, onboarding and Edit
    information change values. The filtered side panel closes with ← (no channel change).

## 20. Several playlists: copies everywhere (P3, real catalogue)

Same setup as §18: two Xtream playlists (TV, TV 2) with overlapping titles, a PIN set, copies
combined (Settings ▸ Interface). Undo toasts last 8 s.

- [ ] **A stacked title's page acts on every copy:**
  - **Do:** open the page of a movie in both playlists (from All). Add it to Favourites, then
    remove it; Undo. Mark watched, unmark; Undo. Ctrl+L (PIN), Ctrl+H; Undo. F2. Then fan the
    stack out in the grid, Enter on TV 2's copy and press Ctrl+F / F2 there.
  - **Expect:** ★ and ✓ show when any copy has them; adding favourites / marking watched writes
    the copy Play would use, removing / unmarking clears every copy, and Undo brings each row
    back; lock and hide cover every copy (the toast names both playlists); F2 says "Rename every
    copy". From the fanned-out copy everything acts on that copy only.
- [ ] **Series watched across copies:**
  - **Do:** watch an episode of a series in TV 2's copy (to the end); open TV's copy of the same
    series from All. Mark season 1 watched from its menu; click Undo. Pick a series whose TV 2
    copy you never opened (episodes not loaded yet) and mark a season watched in TV's copy.
  - **Expect:** TV's page shows that episode watched and opens at the next one; the season mark
    covers both copies (open TV 2's page to check) and Undo restores both. With the unloaded
    copy TV's episodes are marked at once ("Marked S01 watched in TV"), the page says "Loading
    the other copy's episodes to mark them too…", then TV 2's are marked (a second toast); with
    the network off a warning names TV 2. Stop TV 2's copy halfway through an episode, then on
    TV's page (pin TV: Always play from TV) the button says "Resume S01E0x mm:ss" and plays TV's
    copy from there.
- [ ] **Same title as…:**
  - **Do:** find a film that shows twice in All (different names, or a copy without a year).
    On one card ▸ menu ▸ *Same title as…*, type the other's name, Enter. Click Undo. Do it again,
    then Ctrl+R (refresh both playlists). Settings ▸ Hidden ▸ Titles you separated, combined or
    pinned: Enter on the joined row.
  - **Expect:** the picker lists one row per title (never the title itself) with year and
    playlists; after Enter the two cards are one stack, its page says "Combined by you"; Undo
    separates them; the join survives the refresh; the Hidden list names the other copy
    ("combined with … (TV 2)") and Enter separates them again.
- [ ] **Deleting a playlist moves your data (use a throwaway copy of a playlist):**
  - **Do:** add a second copy of TV (Settings ▸ Playlists ▸ add, same login, name "TV old"),
    favourite two films and watch one halfway in "TV old", plus an episode of a series. Delete
    "TV old": read the dialog, press ↑ three times and Enter (it ticks *Move favourites,
    progress and watched marks…*), press Delete. Repeat with the box unticked on another
    throwaway playlist.
  - **Expect:** the box is unticked at first and ↑ never wraps onto Delete; the subtitle says
    "This can't be undone."; the dialog lists "Title (year) → TV · ★ · 41:20"
    and what can't move (channel favourites, titles without a copy elsewhere); ticking it loads
    the other series copy's episodes first (a spinner, Delete waits). After Delete the toast
    says how much moved; TV's copies are favourites with the same progress, the episode too.
    Unticked, nothing moves.
- [ ] **Live TV "Also on":**
  - **Do:** in Live TV ▸ All, move onto a channel both playlists carry (BBC One). Click the
    "Also on" chip; press Ctrl+O and pick it from the menu; lock TV 2's copy and pick it again
    with the session locked; hide TV 2's copy.
  - **Expect:** the preview shows `Also on ● TV 2 (FHD)` (the quality when known), the hint line
    "Ctrl+O also on"; a click plays TV 2's copy in the preview while the cursor stays on TV's;
    the chip of the channel playing shows ▶ (no focus ring); Ctrl+O works in Compact too (no
    preview); the locked one shows the lock and asks for the PIN; a hidden one, a different channel with
    the same name but another tvg-id, and explicit channels with the session locked are never
    offered. Nothing ever switches channel by itself.

## 21. Use Tuner from an AI agent (MCP)

Default profile, Claude Code installed, `claude mcp add --scope user tuner -- tuner mcp` done
once (`claude mcp list` shows `tuner` connected). The checks below say "the agent": run them
with Claude Code, and *Second agent* with another one.

- [ ] **Not running → status doesn't start it:** quit Tuner. Ask the agent "what's playing?".
  **Expect:** the agent says Tuner isn't running; Tuner does not open.

- [ ] **Not running → starts:** quit Tuner. Ask the agent "what's on now?".
  **Expect:** Tuner opens on the current workspace within a few seconds; the agent lists programmes
  with channels and times in your local time.
- [ ] **"Touring cars is on" (or any live sport / show in your guide):** with Tuner's window on
  another workspace (or tiled behind the terminal made fullscreen with Super+F). **Expect:** the agent finds the live programme
  and it plays in the pop-out: pinned, bottom right, on the workspace you are on; the terminal keeps
  the keyboard (keep typing) and Tuner's window doesn't come forward. The agent says it's in the
  pop-out. Hover the pop-out → ↑ zaps; Backspace in it returns to the previous channel.
- [ ] **On the TV / full screen:** "put the news on the big screen" (or "full screen").
  **Expect:** it plays full screen in Tuner's window, brought forward; the agent says it's in
  Tuner's window. From the pop-out, the window pops back in and goes full screen.
- [ ] **Big screen for what plays:** with something in the pop-out, "put it on the big screen".
  **Expect:** the same programme or film carries on (no reconnect, no jump back) full screen in
  Tuner's window, in front.
- [ ] **Watching in the window:** Tuner fullscreen (Super+F) and playing, ask the agent to play
  another channel. **Expect:** it stays fullscreen in the window (no pop-out). Same with Tuner's
  player tiled beside the terminal while you type in the terminal (or on a second monitor), and
  with two pictures on (multi-view). "Move it to the pop-out" then pops it out.
- [ ] **Browsing in the window on screen:** Tuner tiled beside the terminal on Live TV with its
  preview pane playing (Esc out of the player), then on Home, ask the agent to play something
  each time. **Expect:** the player fills Tuner's window (not fullscreen, no pop-out, the preview
  pane doesn't just change channel); the terminal keeps the keyboard; the agent says it's in
  Tuner's window. Repeat a few times: same every time.
- [ ] **Watching on another workspace:** Tuner playing in its window on workspace 2, you on
  workspace 1 in the terminal: ask for another channel. **Expect:** it comes to you in the
  pop-out on workspace 1.
- [ ] **Setting:** Settings ▸ AI agents ▸ *Agents play in* = Tuner window, Tuner on another
  workspace, ask the agent to play something. **Expect:** Tuner's window comes forward and plays
  it. Set it back to Pop-out.
- [ ] **Tuner not running → the window:** quit Tuner, ask "play BBC One". **Expect:** Tuner
  starts like a plain `tuner` on this workspace (tiled, not pinned, not fullscreen, focused like
  any app launch) and BBC One plays in its full window's player; no pop-out appears.
- [ ] **Tuner not running → the pop-out, asked:** quit Tuner, ask "play BBC One in the pop-out".
  **Expect:** Tuner starts as the pop-out only, on this workspace, without the keyboard
  ("Nothing playing" for a second, then the channel); no full window jumps in front. Esc tiles it.
- [ ] **Not running, then it fails:** quit Tuner, ask for a programme that hasn't started (or Up
  next while it is empty) in the pop-out. **Expect:** the agent says why; Tuner doesn't stay as an empty pinned
  pop-out (it goes back into a normal window).
- [ ] **Not running, locked:** quit Tuner with a PIN set and a locked channel, ask the agent to
  play it. **Expect:** Tuner's window (not the pop-out) with the PIN prompt; typing the PIN works.
- [ ] **Up next follows the pop-out:** with Tuner on another workspace, queue two short films (or
  two episodes) from the agent and say "play Up next". **Expect:** the first plays in the pop-out
  (not the corner mini player in Tuner's window); when it ends, the second plays in the pop-out
  too. "Skip" keeps it there. With Tuner's window on screen (on Home), "play Up next" opens its
  player in the window instead.
  Lock the second film first: when it comes up, a notification asks for the PIN.
  With *Agents play in* = Tuner window, "play Up next" opens Tuner's player (not the mini player).
- [ ] **Multi-view from the pop-out:** with a channel in the pop-out, ask for "BBC One and BBC
  Two side by side". **Expect:** Tuner's window comes back in, in front, with the two pictures.
- [ ] **Locked item from the pop-out:** set a PIN, lock a channel, lock the session, ask the agent
  to play it. **Expect:** Tuner's window comes forward with its PIN prompt; after the PIN it plays
  there.
- [ ] **Off Hyprland (if available):** ask the agent to play something. **Expect:** Tuner's window
  plays it, as before; the agent says it's in Tuner's window.
- [ ] **Upcoming → automatic switch:** ask for something starting in 2–3 minutes and accept the
  switch. **Expect:** the agent reports the start time; 10 s before, Tuner toasts "Switching to …
  in 10 s" with Cancel; at the start it switches by itself. Repeat and press Cancel: no switch.
  Settings ▸ Reminders lists the pending switch with its own label until then.
- [ ] **Mood pick:** "something light under 2 hours, I haven't seen". **Expect:** the agent names
  a film from a fitting category with its runtime; on "play it" Tuner starts it (resuming saved
  progress for a started film).
- [ ] **Catch-up:** ask for a programme that ended today on a channel with catch-up.
  **Expect:** it plays from the archive; on a channel without catch-up the agent says it can't.
- [ ] **Parental lock:** set a PIN, lock a channel, lock the session. Ask the agent to play
  that channel. **Expect:** Tuner shows its PIN prompt; the agent tells you to type the PIN in Tuner
  and does not ask for it. With explicit content present and hidden: ask the agent to search for
  an explicit channel or category by name — nothing found, no counts that give it away. Unlock in
  Tuner → it is found.
- [ ] **No addresses:** ask the agent for the stream URL of a channel. **Expect:** it can't give
  one (no tool result contains a URL).
- [ ] **Control:** "pause", "resume", "volume to 30", "stop". **Expect:** each happens, also
  while Tuner sits on another workspace.
- [ ] **Window hidden:** move Tuner to another workspace, ask "what's on BBC One at 9?".
  **Expect:** an answer without Tuner coming forward; "play it" plays it in the pop-out on the
  workspace you are on.
- [ ] **Outlives the agent:** with Tuner started by the agent (first check), quit the agent (and
  close its terminal). **Expect:** Tuner keeps running and playing.
- [ ] **Relock while away:** set a PIN with a 1-minute relock, unlock, ask the agent to switch to
  an explicit channel in 3 minutes, then move Tuner to another workspace and wait. **Expect:** no
  toast or switch for it after the relock; "what's playing?" and the scheduled list don't name
  it.
- [ ] **Flags first:** `claude mcp add tuner-debug -- tuner --debug mcp`, then ask "what's
  playing?". **Expect:** an answer (not a Tuner window opening); remove it afterwards.
- [ ] **Old instance:** with a Tuner from before this feature running (or simulate: keep an old
  build open), ask the agent anything. **Expect:** the agent says to restart Tuner; after a
  restart it works.
- [ ] **Second agent:** connect Codex (`codex mcp add tuner -- tuner mcp`) or Gemini CLI
  (`gemini mcp add -s user tuner tuner mcp`) and ask it to follow a team. **Expect:** it shows
  the matches and asks first; the toast reads "Codex added … · N reminders" (or "Gemini added
  …") with Undo; Settings ▸ Following shows the rule "· by Codex" and its follow sheet "added by
  Codex". A rule Claude Code added still says "by Claude". A play, search and status work the
  same as with Claude Code.
- [ ] **Upgrade:** open a profile from Tuner 0.2.1 that has a Following rule, a spoiler rule and
  a guide-fix batch Claude made. **Expect:** they show "by Claude" / "added by Claude", and
  Recent changes in Guide fixes names Claude. Restoring a 0.2.1 backup with the two rules on a
  fresh profile shows them "by Claude" too (backups don't carry Recent changes).


## 22. Guide fixes (real catalogue)

- [ ] **The list:** open Guide fixes (`Ctrl+E`, or Settings ▸ EPG ▸ *Fix guide data…*) on the
  real catalogue. **Expect:** the dashboard's coverage per playlist matches what the guide
  shows; the issue chips have counts; rows draw at once in "All channels" (79k) and scrolling
  stays smooth. "Checking the guide data for new suggestions…" shows only while a check waits or
  runs (about 30 s after a guide refresh), never as a job for the whole wait.
- [ ] **Accept and Undo:** mark three "Suggested" rows, `Shift+Enter`. **Expect:** a card with
  the before/after now lines; Enter applies; the guide shows the programmes; the toast's Undo
  (or `Ctrl+Z`) puts all three back.
- [ ] **+1 channel:** on a "+1" channel without guide, accept "+1 h". **Expect:** the guide
  shows its base's programmes an hour later; a reminder set on it before moves by an hour.
  "Canal+ 1" or "Disney+ 1" (if your playlists have them) are not offered as +1 channels.
- [ ] **Rules:** turn on *+1/+2 follow their base* for one playlist. **Expect:** the confirm
  names how many channels change; after Enter those channels have guides.
- [ ] **Time shift keys:** `Ctrl+Shift+→` twice quickly, then once more after a second.
  **Expect:** +3 h, one undo entry per pause.
- [ ] **Catch-up offset:** on a channel with archive, set the offset to 0 while the playlist's
  offset is +2 min, press Check ▶. **Expect:** the last programme starts at its real start (the
  channel's 0 wins).
- [ ] **Backup:** export a backup after a shift and an "It's right", restore it on a fresh
  profile. **Expect:** the shift and the dismissal are back.
- [ ] **Agent:** ask your agent "why is the guide empty on half my channels?" and to fix them.
  **Expect:** a proposal; more than 50 channels is not applied from chat: Tuner comes forward
  on Guide fixes with "Accept <agent>'s N fixes?" (the agent's name, e.g. "Accept Codex's N
  fixes?") and Enter applies them (toast with Undo), Esc
  applies nothing; a smaller batch applies from chat and its toast/Undo works; a locked
  channel is refused while locked. Ask for a second big batch before opening the first card:
  **Expect:** a toast says the earlier fixes were replaced and the card shows the newer batch.

## 23. Up next

- [ ] **Queue and play:** add three episodes (`Ctrl+N`) and a film, `Alt+6`, Enter on the first.
  **Expect:** each item plays after the countdown card ("Next in 10 s"); the card works with
  keys and mouse, in the fullscreen player, the mini player and the pop-out; a queued film ending
  fullscreen stays in the player for the next item.
- [ ] **Step aside:** during an item, zap to a channel. **Expect:** "Up next paused · Alt+6 to
  carry on"; media keys Next/Previous zap (they don't step the queue); Carry on resumes the
  item.
- [ ] **Locked item:** queue a locked film with the session locked. **Expect:** the card waits
  for the PIN; with the window on another workspace a notification says Up next is waiting.
- [ ] **Explicit hidden:** queue an explicit item while unlocked, relock. **Expect:** it
  disappears from the list and counts; *Clear Up next* doesn't delete it (unlock: it's still
  there).
- [ ] **Next episode, empty queue:** watch an episode to the end with Up next empty. **Expect:**
  "Next episode in 10 s" with its label through the spoiler guard (a later episode's title
  hidden for a part-way series); it plays. Lock the series and repeat with the session locked:
  "Next episode is locked", nothing plays.
- [ ] **Restart:** quit during an item, start Tuner. **Expect:** nothing plays; Home shows
  "Resume Up next".
- [ ] **OSD buttons:** play a film with two items in Up next and show the OSD. **Expect:**
  `Up next` and `Next` before Info (tooltips "u" / "n"); a click on `Up next` opens the panel,
  `Next` plays the next item; with the controls focus (↓) the arrows reach both and Enter acts.
  With Up next empty there is no `Next`.
- [ ] **Resume prompt:** watch 5 minutes of a queued film, stop, `Alt+6`, Enter on it.
  **Expect:** "Resume from 5:00" / "Start over"; Start over plays it from 0 as Up next's item
  (the list stays; Shift+Enter opens the player); a row set to "Play from the start" doesn't
  ask.

## 24. Following and the news flash

- [ ] **Follow a team:** `R` on a match in the guide, Follow. **Expect:** the toast "Following:
  … · N in the guide" with Undo (Undo removes the rule and its reminders); reminders for the
  next games appear in Settings ▸ Reminders "from Following"; a game shown on two channels gets
  one reminder; two different games at the same time get one each.
- [ ] **No interrupt:** with a film playing, let a rule's switch come due. **Expect:** a
  notification with **Watch**, the film keeps playing. Your own one-off switch does interrupt,
  after the 10 s Cancel.
- [ ] **An agent's rule:** ask your agent to follow a show, then open the rule in Settings ▸
  Following. **Expect:** the sheet says "added by <agent>"; after an edit it still does.
- [ ] **Edit a rule:** change a rule from Notify to Switch. **Expect:** its coming reminders
  become switches after the next evaluation (within seconds).
- [ ] **Following off:** turn Following off in Settings. **Expect:** its reminders don't fire
  (on again: they do).
- [ ] **Rule menu and order:** Settings ▸ Following, `x` on a rule. **Expect:** Edit…, Pause,
  Test against the guide, Move up / down, Delete…. Test toasts "N in the next 14 days · next …"
  with the rule's name only; Pause shows "paused" and its reminders leave Settings ▸ Reminders
  within seconds (Resume brings them back); Delete… asks to cancel or keep them. With two
  rules, Alt+↑/↓ and the drag grip change the order, and it stays after a restart.
- [ ] **Summary Open:** let a new rule match (or wait for a guide refresh that adds matches).
  **Expect:** the "Following: N new" notification has **Open**; picking it raises Tuner on
  Settings ▸ Following. Without a notification server the toast has the same Open.
- [ ] **News flash (the user's addition):** on a news bulletin, *News flash this daily…*,
  weekdays, headlines 5 min. At the bulletin's time with a live channel playing. **Expect:** the
  countdown strip in the pop-out (pinned, every workspace, **keyboard focus stays where it
  was**), the bulletin plays, after 5 min the pop-out closes and the previous channel is back.
  With a film playing: a notification instead. With the bulletin's channel (or its copy)
  already on: nothing. In quiet hours (set them to cover now): nothing. With another app
  fullscreen (and "not while another app is fullscreen" on): nothing.
- [ ] **Silent flash:** set the rule's sound to Mute. **Expect:** the flash plays muted and the
  sound comes back after it.
- [ ] **Tuner closed:** quit Tuner before a flash. **Expect:** it still pops up (systemd timer)
  in the pop-out; in quiet hours nothing starts.

## 25. Spoiler guard

- [ ] **Part-way series:** with a series half watched, open its page and the guide. **Expect:**
  later episodes' titles are bars, plots say "hidden — … · Ctrl+B to show"; the next-up episode
  and earlier ones show. `Ctrl+B` on one reveals it until you move.
- [ ] **Guide airings of that series:** find its airings in the guide (some guides give no
  season/episode). **Expect:** an airing whose sub-title is an episode you've seen (or the
  next-up one) shows it; later episodes and first showings ("new") are bars; the status bar
  says "Ctrl+B show spoilers" on a hidden one. Watch one to the end (or Ctrl+W it): its repeats
  show their sub-title afterwards.
- [ ] **In the player:** play the channel airing a hidden episode. **Expect:** the banner reads
  "Show · ▆▆▆▆▆▆▆▆ 󰈉" with "Ctrl+B show spoilers"; Ctrl+B shows the episode title ("Ctrl+B
  hide spoilers"); a zap hides it again.
- [ ] **Series guarded before you start it:** guard a series you haven't watched (item menu ▸
  Spoiler guard), then find a repeat of a late episode whose guide entry has no season/episode.
  **Expect:** its sub-title is a bar; an old airing from before the series' guarded episodes
  first aired still shows.
- [ ] **Sport descriptions in the player:** with a topic that hides sport descriptions active,
  play a channel showing another match. **Expect:** the banner's line reads "Title ·
  ▆▆▆▆▆▆▆▆ 󰈉", never the other match's score.
- [ ] **Followed sport:** follow your team spoiler-safe; during and after their match, look at
  the guide and the news channels. **Expect:** "Arsenal v Chelsea 󰈉" where a score was (also
  "Arsenal v Chelsea 2-1"); news sub-titles mentioning the team are bars; unrelated sport and
  "Arsenal Women" / "Fan TV" are untouched. Watching the match (or its repeat the next day)
  lifts it.
- [ ] **Zap shield:** while the match's guard is active, zap with ↑/↓ onto a sports news
  channel talking about it. **Expect:** the card covers the picture **before any sound** (the
  channel isn't tuned; the previous picture pauses under it); Enter watches anyway, ↓ judges
  the next channel, Backspace resumes the previous one.
- [ ] **Zap shield in multi-view:** with two pictures, zap the focused one onto that channel.
  **Expect:** the card covers that picture only; the other keeps playing with sound; `w` to the
  other picture and Enter does nothing to the card; back on it, Enter watches anyway.
- [ ] **Notifications:** a reminder for the match's channel fires (Tuner open, then closed).
  **Expect:** no score in the notification either way.
- [ ] **Agent:** ask your agent "what was the Arsenal score?" and "what's on Sky Sports News?".
  **Expect:** no score or guarded text; asking it to remove your own spoiler rule is refused. Ask
  it to guard a show and to mark an episode seen: Settings ▸ Spoiler guard shows the rule "added
  by <agent>" and the mark "marked seen by <agent>".

## 26. Multi-view on the two one-connection accounts

Both test accounts allow one stream each (TV and TV 2).

- [ ] **Second picture from the other playlist:** play a channel from TV, `Ctrl+M` on a channel
  that exists on both. **Expect:** picture B plays TV 2's copy; A is untouched; the status shows
  one stream per playlist; focus and sound stay on A until `M`.
- [ ] **Only on the busy playlist:** `Ctrl+M` on a channel that's only on TV while A plays TV.
  **Expect:** "Move picture A to its TV 2 copy?"; Esc changes nothing; Enter moves A (one short
  gap) and B plays — never two streams on one playlist (no provider 403 / "too many
  connections").
- [ ] **Zap past the other playlist:** in B, zap with ↑/↓ through channels A's playlist holds.
  **Expect:** B plays copies on its own playlist or skips with "… skipped — TV is in use by
  picture A".
- [ ] **Layouts and keys:** `t` cycles side by side / big + small / picture in picture at Wide,
  Tiled and in the pop-out; `w`, Ctrl+arrows, `W`, `q`; Esc never closes a picture.
- [ ] **Automatic switches:** with two pictures, let a reminder switch and a news flash come
  due. **Expect:** they play in the silent picture; the picture you watch is untouched; after
  the flash the silent picture gets back what it showed.
- [ ] **Parental:** with the session locked, `Ctrl+M` on a locked channel. **Expect:** the PIN
  prompt before anything plays. Relock with a locked channel in A and a free one in B: only A
  stops.
- [ ] **Agent:** "put both semis side by side". **Expect:** two pictures on different
  playlists in one go. With two pictures already on, "now X and Y" replaces both in one go
  (no "Move picture …?" for a picture that is replaced); if one of them is already on screen
  it stays and only the other changes.
- [ ] **CPU:** two HD live channels for 10 minutes. **Expect:** smooth video, fans and battery
  as noted in `docs/design/multiview-spike.md`.

## 27. Reminders, scheduled playback and the sleep timer

- [ ] **Pop-out auto-launch without focus steal (Tuner closed), with the Hyprland snippet:**
  - **Do:** install the current `packaging/hypr/tuner.lua` (§ *Applying the Hyprland snippet*)
    and reload Hyprland. Set a switch for a programme starting in 3 minutes (Open in: Pop-out),
    quit Tuner, and keep typing in a terminal; keep the mouse still. When it has appeared, run
    `hyprctl clients -j | jq '.[] | select(.class | startswith("tuner")) | {class, floating,
    pinned, tags}'`. Then click the pop-out and press Esc.
  - **Expect:**
    - Tuner starts about 40 s before as a small window that is pinned on the current workspace
      from its first frame (never a big centred or tiled window first), then moves to the
      pop-out corner showing the countdown strip; it plays there when the programme starts.
    - **The terminal keeps the keyboard** the whole time: no keystroke goes to Tuner, the
      terminal's border stays the active one.
    - `hyprctl` lists class `tuner-popout`, floating, pinned, tag `pop`.
    - The video is fully opaque, like a normal Tuner window.
    - Esc tiles the window on the current workspace like any pop-in (not left floating or
      pinned); `o` pops it out again at the corner.
  - **Without the snippet** (comment out the `tuner-popout` rule, reload, repeat): Tuner may map
    tiled and take the keyboard, then pops out; nothing else breaks. Restore the rule afterwards.
- [ ] **Pop-out with Tuner open on another workspace:** same, Tuner running and tiled on another
  workspace, the mouse resting where the pop-out will land. **Expect:** the pop-out appears on
  the current workspace without taking focus; `hyprctl clients` still shows class `tuner`.
- [ ] **`tuner --play <ref> --popout`:** run it from a terminal, with Tuner closed and again with
  it running. **Expect:** plays in the pop-out without focus both times (the terminal keeps
  typing); the first run's window is class `tuner-popout`.
- [ ] **Open Tuner while it runs as `tuner-popout`:** with the auto-launched pop-out on screen,
  use the Omarchy menu's *Open Tuner* (and the Super+Shift+T binding if enabled). **Expect:**
  the pop-out gets the focus (it is found by its class), no second Tuner starts.
- [ ] **Esc cancels:** while "Switching to … in 8 s" shows over the player, press Esc.
  **Expect:** "Switch cancelled", the player stays.
- [ ] **Late switch:** suspend the laptop across a switch's start, resume 5 min later.
  **Expect:** the 10 s countdown still shows, then it plays from the beginning on catch-up
  ("Started 5 min ago — playing from the beginning") or live.
- [ ] **Re-login:** set a switch, log out and back in (lingering on), quit Tuner. **Expect:** the
  switch still opens the pop-out.
- [ ] **Reboot, Tuner never opened:**
  - **Do:** with the package installed, set a notification reminder for a programme starting in
    about 15 minutes and a switch for one in about 20 (Open in: Pop-out); quit Tuner and reboot.
    Log in and do **not** open Tuner.
  - **Expect:**
    - `systemctl --user list-timers 'tuner-reminder-*'` lists both "Tuner reminder" timers
      within a few seconds of the desktop appearing.
    - `systemctl --user status tuner-rearm` is `inactive (dead)` with a `status=0/SUCCESS` run
      at this login and a `reminders: 2 timer(s) armed` line; `systemctl --user is-enabled
      tuner-rearm` says `static`; `systemctl --user --failed` doesn't list it.
    - The notification shows at its time (Watch plays), and the switch opens the pop-out without
      taking the keyboard — so the timers fired with the new session's display.
- [ ] **Due during the reboot:** set a notification reminder for a programme starting in about
  3 minutes (lead 2 min); quit Tuner, reboot so that you log in after its time but while it
  airs; don't open Tuner. **Expect:** the note ("Started … ago") shows within seconds of
  logging in, once.
- [ ] **Log out and back in (no reboot):** same with a reminder 15 minutes ahead; log out, log in,
  don't open Tuner. **Expect:** `journalctl --user -u tuner-rearm -b` shows a run for each
  login; the timer is listed once and the reminder fires once.
- [ ] **Re-arm while Tuner runs, and with no profile:** with Tuner open, run
  `systemctl --user start tuner-rearm`. **Expect:** it succeeds, its log says Tuner is running,
  the timers are unchanged. Log in as an account that never started Tuner. **Expect:**
  `tuner-rearm` succeeded and `~/.local/share/tuner` and `~/.local/state/tuner` don't exist.
- [ ] **Masked:** `systemctl --user mask tuner-rearm`, reboot with a reminder pending, don't open
  Tuner. **Expect:** no timers until Tuner starts. `systemctl --user unmask tuner-rearm`
  afterwards.
- [ ] **`tuner --refresh` with a Following rule:** quit Tuner, run `tuner --refresh` after the
  guide changed. **Expect:** `systemctl --user list-timers` shows a "Tuner reminder" unit for
  the new match.
- [ ] **Sleep timer:** `Ctrl+Shift+T`, "after 2 items" on a series. **Expect:** the warning
  strip near the end of episode 2, then it stops — not earlier while episode 1's end card shows.
  "End of Up next" with three queued items stops after the third, not after the first.

## 28. Connect an AI agent (the card and Settings ▸ AI agents)

These checks change agents' settings. Unless an item says otherwise, run Tuner and the agent
CLIs in a **throwaway home**, so your real Claude Code, Codex and Gemini settings stay as they
are:

```sh
S=$(mktemp -d)
env HOME=$S XDG_CONFIG_HOME=$S/.config XDG_DATA_HOME=$S/.local/share XDG_CACHE_HOME=$S/.cache XDG_STATE_HOME=$S/.local/state CODEX_HOME=$S/.codex \
  target/release/tuner --profile-dir /tmp/tuner-check
```

Run every agent command of an item with the same `env HOME=$S …` prefix. The Omarchy theme
falls back to the default there; that's expected. Don't set `TUNER_NO_DESKTOP` (with it Tuner
never runs an agent's tool). To make Tuner treat the next start as the first after an upgrade,
quit it and remove the `checked_version` line under `[agents]` in
`/tmp/tuner-check/config/config.toml` (or set it to `"0.2.2"`).

- [ ] **pacman message:**
  - **Do:** `sudo pacman -U` the new package on a machine without omatuner; then remove it,
    install 0.2.2 and upgrade to the new package; then upgrade again to a rebuilt package with a
    higher `pkgrel`.
  - **Expect:** the two "To connect an AI agent … Tuner offers it when it next starts, or open
    Settings ▸ AI agents (Claude Code: tuner claude-setup)" lines print after the fresh install
    and after the upgrade from 0.2.2, not after the later upgrade. Nothing else runs
    (`cat /var/lib/pacman/local/omatuner-*/install` contains only `echo` lines).
- [ ] **The card offers only what isn't connected:**
  - **Do:** with Claude Code and Codex on `PATH`, connect Claude Code by hand
    (`claude plugin marketplace add /usr/share/tuner/claude`, `claude plugin install
    omatuner@omatuner --scope user`), finish onboarding (Try free channels), quit and start
    Tuner after clearing `checked_version`.
  - **Expect:** about 3 s after start, one card: *Connect Codex to Tuner?* (Claude Code is not
    offered). The hint line reads Enter Connect · Esc Not now. Holding Enter or clicking
    outside as it appears does nothing; Esc closes it; restarting shows no card. Clearing
    `checked_version` again shows it again. It doesn't open while Settings ▸ AI agents is on
    screen.
- [ ] **Never over the player:** clear `checked_version`, start Tuner with
  `--play <ref>` (full screen), then pop it out (`o`), then open multi-view. **Expect:** no card
  while any of them is on screen, nor during onboarding or with a dialog or a text field open;
  it appears as soon as you are back on a browse screen (a preview playing in a pane is fine).
- [ ] **Connect from the card (Claude Code):**
  - **Do:** remove the plugin (`claude plugin uninstall omatuner@omatuner --scope user`), clear
    `checked_version`, start Tuner and press Enter on the card.
  - **Expect:** the card shows "Claude Code — connecting…" with a busy bar, then closes; a toast
    says Claude Code is connected and to restart it. `claude plugin list --json` lists
    `omatuner@omatuner` with Tuner's version. (Optional, needs a Claude login in the throwaway
    home: a new `claude` session's `/mcp` lists `plugin:omatuner:tuner` and `/omatuner:watch`
    exists.)
- [ ] **Several agents:** with Codex, Gemini CLI and Claude Code all on `PATH` and none
  connected, clear `checked_version` and start. **Expect:** *Connect your AI agents to Tuner?*
  with Claude Code ticked and the others not; the button reads *Connect Claude Code*. Space on
  Codex makes it *Connect 2 agents*; unticking both disables it. Connect runs them one after the
  other with a toast each. Mouse: clicking the rows ticks them; the buttons work; a click outside
  the card is *Not now*.
- [ ] **Already connected:** with Claude Code connected and Codex found but not connected, clear
  `checked_version` and start. **Expect:** *Connect Codex to Tuner?* with a line "✓ Claude Code
  already connected" above *Don't ask again*; only Codex is offered. Tab, the arrows and clicks
  never land on that line. Same with Gemini CLI also found: the line sits above the Codex and
  Gemini CLI ticks.
- [ ] **Keyboard and help on the card:** **Expect:** `?` shows the dialog keys; ↑/↓/Tab move,
  Enter on a tick toggles it; during Connect, Enter or Esc hides the card and the toast still
  arrives.
- [ ] **Nothing found:** start with `PATH=/usr/bin` added to the `env` line and no agents in
  `~/.local/bin`, mise's shims, `~/.bun/bin`, `~/.npm-global/bin` or `~/.volta/bin` of the
  throwaway home. **Expect:** no card; the page shows one "Tuner didn't find …" note and no
  *Keep Claude Code's plugin up to date* switch.
- [ ] **Don't ask again:** clear `checked_version`, start, tick *Don't ask again*, press Esc.
  **Expect:** Settings ▸ AI agents shows *Offer to connect agents after updates* Off; clearing
  `checked_version` again shows no card. Turning the toggle back on and clearing
  `checked_version` brings the card back.
- [ ] **Settings ▸ AI agents, statuses:**
  - **Do:** open the page with Claude Code connected, Codex not connected, Copilot CLI not
    installed, opencode installed.
  - **Expect:** Claude Code "Connected · plugin <version>", Codex "Not connected", opencode
    "Not connected", "Not found on this computer: Copilot CLI." (and any other missing agent).
    The statuses match `claude plugin list --json`, `codex mcp get --json tuner`,
    `~/.gemini/settings.json` and `~/.config/opencode/opencode.json` in the throwaway home.
    *Check again* shows "Checking…" briefly and picks up a change made in a terminal. Wide,
    tiled, text 12/16: rows and the menu fit.
- [ ] **Connect and Disconnect from Settings:**
  - **Do:** on Codex, Enter ▸ *Connect*; then Enter ▸ *Disconnect…*, Esc on the confirm; then
    *Disconnect…* ▸ *Disconnect*. Repeat with Gemini CLI, Copilot CLI and Claude Code.
  - **Expect:** "Connecting…" on the row, a success toast, the row turns "Connected"
    (`codex mcp get --json tuner` finds it; Gemini: "tuner" under `mcpServers` in
    `~/.gemini/settings.json`; Copilot: `copilot mcp get --json tuner`). Esc on the confirm
    changes nothing. Disconnect removes it and toasts "Disconnected …"; Claude Code's plugin and
    its `omatuner` marketplace are both gone from `claude plugin list --json` and `claude plugin
    marketplace list --json`. Doing it all with the mouse works the same.
- [ ] **By hand: opencode and Cursor:** Connect opencode, then open its menu. **Expect:**
  "Connected"; no *Disconnect…*, but *Copy how to remove it* pastes the instruction with the
  file's path. With `~/.cursor/` present (or `cursor-agent` on `PATH`), Cursor's row is "Not
  connected · add it by hand"; *Copy the setup (JSON)* pastes
  `{"mcpServers":{"tuner":{"command":"tuner","args":["mcp"]}}}`. *Other agents* copies
  `tuner mcp`, the VS Code command and the README link; each pastes correctly.
- [ ] **Update after an upgrade:**
  - **Do:** in the throwaway home connect from GitHub (`claude plugin marketplace add
    rthoms334/omatuner`, `claude plugin install omatuner@omatuner --scope user`) while running a
    build whose version is newer than GitHub's, or upgrade the package from 0.2.2 with the
    plugin connected.
  - **Expect:** the row shows "Update available · plugin <old> → <new>" and *Update to <new>*
    works. With *Keep Claude Code's plugin up to date* on, the first start after the upgrade
    updates it by itself and toasts it (no card for it); with it off, the row just says
    *Update available*. When GitHub has nothing newer, *Update* toasts that the plugin is up to
    date and the row turns *Connected · plugin <old>*.
- [ ] **Started from the launcher:** with an agent CLI installed only through mise (or in
  `~/.local/bin`), start Tuner from the Omarchy launcher (default profile; only open the page,
  don't press Connect or Disconnect). **Expect:** the agent is found and its status is right.
- [ ] **A tool that hangs:** make a fake `codex` (`mkdir -p $S/bin; printf '#!/bin/sh\nsleep
  600\n' > $S/bin/codex; chmod +x $S/bin/codex`), start Tuner with `PATH=$S/bin:$PATH` added to
  the `env` line and connect Codex. **Expect:** the UI keeps working; after 2 minutes an error toast says it didn't answer;
  `pgrep -f 'sleep 600'` finds nothing. Quit Tuner during a second attempt: Tuner exits promptly
  and no `sleep` is left behind.
- [ ] **Last used by:** default profile (the agent's `tuner mcp` reaches it; this check changes
  no agent settings). Ask a connected agent "what's playing?", then open Settings ▸ AI agents
  within a minute. **Expect:** "Last used by Claude, just now" (or the agent's name) in the
  intro, and "used just now" on that agent's row.
- [ ] **Logs:** Settings ▸ About & logs ▸ open the log viewer after the checks above.
  **Expect:** each Connect / Update / Disconnect is logged with its command line and `ok` or
  `exit N` and a redacted first line. No raw `claude plugin list` output, no
  `~/.claude.json` content, no other MCP server's settings.
- [ ] **Backup restore:** with *Offer to connect* Off, restore a backup made with it On
  (*Restore settings* ticked). **Expect:** it stays Off: the AI agents settings belong to this
  computer.

## 29. Sidebar order, start screen and where Live TV opens

Use a profile with a playlist and at least one favourite channel.

- [ ] **Sidebar order:** Wide, tiled and the Compact glyph strip (a window under 480 pt).
  **Expect:** Search first at the top, then Home, Live TV, Guide, Movies, Series (Up next while
  it has items) and Settings last, Search and Settings set apart by a small gap. Clicks open
  each one; Alt+1…6 and Alt+0 still open Home … Up next and Settings.
- [ ] **Fresh start opens Home:** delete `/tmp/tuner-check/config/config.toml` (keep the
  database) and start. **Expect:** Home, also without favourites or history. With no playlists
  at all: onboarding, as before.
- [ ] **An old config:** set `start_screen = "auto"` under `[general]` and start. **Expect:**
  Home; Settings ▸ Interface ▸ *Start screen* reads Home (choices: Home, Live TV, Guide, Last
  used). Changing any setting saves `start_screen = "home"`.
- [ ] **Live opens on All channels:** start, then Alt+2. **Expect:** All channels open, the
  cursor on its first channel (on the playing channel when one plays); with one playlist picked
  in the scope bar, that playlist's All channels. The guide (Alt+3) also shows All channels.
  Open a favourites list, go Home, press Alt+2 again: the favourites list is still open.
- [ ] **Favourites instead:** Settings ▸ Interface ▸ *Live TV opens on* ▸ Favourites, quit and
  start, Alt+2. **Expect:** the first favourites list with channels opens; the guide shows it
  too. Remove every favourite channel, quit and start again: All channels.

## 30. Casting to the Apple TV (DESIGN §40; cast design §18)

Run these once against the real Apple TV ("Living Room", 192.168.1.5) on the release build, in
the **default profile** (pairing and the firewall rule belong to this computer). Casting ships
behind `cast::ENABLED` (`src/cast/mod.rs`); the build under test has it on.

- Start the **first session with `TUNER_CAST_TRACE=1`** and keep the transcript
  (`~/.local/state/tuner/cast-trace/`, redacted) as the fixture follow-up.
- Note the tvOS version shown in Settings ▸ Cast.
- **The Apple TV auto-updates.** When it reaches tvOS 27 (or a new 26.x), repeat items 1, 2 and 7.
- The Hisense repeats items 1, 2 and 7 (item 14).

- [ ] **1. Pairing:**
  - **Do:** pick Living Room in the player's cast picker (`C`).
  - **Expect:** a PIN appears on the TV (HKP 3). If it doesn't, *Show a new code* (HKP 5) works.
    Pairing completes without a 470. The pairing survives an Apple TV reboot. Note the Allow
    Access setting and the HomeKit room; with "Everyone in your Home" the hint is right.
- [ ] **2. Session:**
  - **Do:** cast live TV for **60 minutes** with ufw on and only the TCP rule.
  - **Expect:** events keep arriving (declared PTP, no NTP). From the trace: does SETUP return
    `skipRecord`, and what does RECORD answer? `playbackInfo` answers (under `info`, CMTimes):
    the log never says "playbackInfo goes unanswered".
- [ ] **3. Firewall button:**
  - **Do:** with ufw blocking, cast.
  - **Expect:** it fails within about 10 s with the Firewall message. *Allow on firewall* shows
    Omarchy's dialog (note its text); Cancel leaves everything as it was. Allow adds
    `allow tcp 47800 0.0.0.0/0 any 192.168.1.0/24 in_wlan0 comment=6f6d…` to
    `/etc/ufw/user.rules`, and Check again plays the test card. *Remove rule* removes it, with
    no second prompt within 5 minutes. *Show command* pasted into a terminal works.
- [ ] **4. Live HLS** (TS segments, H.264 + AAC) on a real channel: note the tune-in time with
  `min_segments = 2`, the delay behind live, and the behaviour on a long-GOP channel.
- [ ] **5. Formats:**
  - **Expect:** 1080i25 and 576i are deinterlaced (L3) with no combing, and ffmpeg's `speed`
    stays above 1.0× on this M2. MP2 is converted to AAC. HEVC (and 10-bit) plays in fMP4.
  - **Surround:** a film with AAC 5.1 (`xt:vod:346970`), a channel with AC-3 5.1 and a film with
    E-AC-3: the log says `… 5.1(side) 48 kHz + surround (copied)`, the TV's audio menu shows one
    track, an AVR (or the Apple TV's audio format) shows 5.1; on stereo speakers the sound is
    the stereo track. If the TV doesn't start it, the log says "again in stereo" and the cast
    goes on.
- [ ] **6. Segment hygiene on real channels:** `ffprobe -show_frames` on each segment's first
  frame: is it an IDR? A segment held back by the guard ("a segment outgrew the target
  duration: restarting with a longer one") happens at most once a film, and TARGETDURATION
  never changes within a playlist nor between the loads of one item.
- [ ] **7. Controls:**
  - **Expect:** pause (`setRate 0`) and resume work, including after more than 80 s paused.
    Seeking inside the produced part (playlist view) and beyond it (`-ss`) lands at the right
    time. Zap by replace shows the new channel with no stuck old picture. Stop really stops the
    TV. Stopping from the Apple TV remote gives "Living Room stopped playing". For VOD,
    `itemPlayedToEnd` arrives, Up next continues on the TV, and the progress shown matches the
    TV. Seeking a catch-up programme past what has loaded clamps with the toast "Catch-up can
    only seek as far as it has loaded".
- [ ] **8. Full-length films (cast design §8.5):**
  - **Do:** cast an MP4 film (most VOD) and an MKV film from the start.
  - **Expect:** the log says `… full length: <length> in <n> segments, target <t> s, from the MP4
    index` (or `the Matroska cues`); the TV shows the film's **real length** at once, not "LIVE"
    nor a minute.
  - **Do:** scrub with the **TV remote**: far forward (an hour), far back (to the start), and a
    few seconds; note the time to the picture after each.
  - **Expect:** each lands where asked; the log says "making segment … on demand" for the far
    ones; picture and sound start together (no lip-sync drift, no gap or click longer than a
    frame); no "segment outgrew the target" line ever.
  - **Do:** resume a film at **1:10:00** (Play on the TV from a film with progress).
  - **Expect:** the TV starts at 1:10:00 and its scrubber still reaches the start. If the log
    says "the TV doesn't start where the playlist says", it plays from 1:10:00 with a scrubber
    from there (note which, for the Apple TV and the Hisense).
  - **Do:** a **surround** film (the AAC 5.1 one, `xt:vod:346970`): scrub twice.
  - **Expect:** the AVR (or Settings ▸ Video and Audio ▸ Audio Format) shows 5.1 after each
    scrub; the TV's audio menu shows one track.
  - **Do:** pause the TV for 5 minutes on a full-length film, then play.
  - **Expect:** "pausing production … ahead of the TV", and it resumes cleanly.
  - **Do:** after scrubbing, *Stop casting* then play on the computer.
  - **Expect:** it resumes where the TV was, never later.
  - **Do:** a **catch-up** programme, from the start and from the middle.
  - **Expect:** note what the log says for your provider: `full length` (the TV scrubs anywhere)
    or `a growing playlist: …` (MPEG-TS catch-up: item 8b's behaviour).
- [ ] **8b. VOD EVENT playlist** (a film the log calls "a growing playlist": an `.ts` or `.avi`
  film): the TV starts before `ENDLIST`, at the resume point (not 10 s later); resume a film at
  1:10:00. The TV's scrubber shows about a minute ahead, not the whole film. Pause the TV for 5
  minutes: the log says "pausing production … ahead of the TV", and it resumes cleanly. A
  provider drop logs "resuming the provider's stream at byte …" and the TV doesn't jump;
  *Convert this channel* 10 minutes into a film resumes within seconds of where the TV was,
  never at the cast's start. The session's end is in the log with why.
- [ ] **9. Stray requests:** does the TV fetch anything besides the playlist and segments
  (`/favicon.ico`, `HEAD`, `Range`)? The 404s are harmless.
- [ ] **10. Volume:** confirm there is no working path (expected), so the "TV remote" copy is
  right.
- [ ] **11. Provider limit:** with a one-stream account, a relay reconnect inside 45 s of
  closing gets 403s that the race window absorbs.
- [ ] **12. Power:** with the logind inhibitor held, the laptop screen locks but casting
  continues for 30 minutes.
- [ ] **13. Workspaces (§3.8):**
  - **Do/Expect:** ask Claude to put a channel on the TV while Tuner is on another workspace:
    nothing pops out, raises or moves, and `play` answers `opened_in: "device"`. A locked
    channel raises Tuner with the PIN prompt. Block the firewall and cast, then switch
    workspace: a desktop notification appears, and its **Show** raises Tuner on the casting card
    with *Allow on firewall*. Start a cast while the pop-out is up: the pop-out goes back into
    the window without taking focus.
- [ ] **14. Hisense:** the picker and Settings ▸ Cast list "Living Room TV" (AirPlay TV · Pair
  first) beside the Apple TV, with no setting to turn on. Items 1, 2 and 7 through the
  third-party profile. If it can't play, fix the profile or stop listing third-party TVs (cast
  design §7.7).
- [ ] **15. Not covered by tests (found while building it):** the fMP4 keyframe check end to
  end, the 8 GiB film cap (a long film casts without filling the disk), a `CpuTooSlow` message
  on a channel the computer can't convert in time, and pkexec, polkit and ufw on a real system
  (the helper at `/usr/lib/tuner/tuner-firewall` exists after installing the package, and the
  dialog names the action "Allow your TV to connect to Tuner for casting").
- [ ] **16. Cast to… where things are chosen** (cast design §3.1):
  - **Do:** with only Living Room paired and nothing playing, right-click a Live TV channel ▸
    *Cast to Living Room*; then `Alt+C` on a Home tile, on a film card (with progress) and on a
    series card; `C` on an episode row of a series page, on a film's page, and in the guide on
    an airing programme, a past one with catch-up and an upcoming one. Pair the Hisense too and
    repeat one of them.
  - **Expect:** each goes straight to the TV with the toast "Casting … to Living Room"; the
    screen stays and the mini player shows the casting card; the local player never starts
    (no picture, no sound here, and on a one-stream account no 403). The film asks *Resume* /
    *Start over* first and the TV starts at the chosen point; a locked channel asks for the
    PIN first; the series card plays its next episode; the airing programme casts the
    channel, the past one its catch-up, the upcoming one only says "It hasn't started yet" (its
    menu has no *Cast to…*). With two TVs the panel "CAST “…” TO" asks which; an unpaired TV
    there pairs first and then casts that item. Typing `C` in a Live TV list still filters.
- [ ] **17. Play on** (cast design §3.9):
  - **Do:** with a channel playing here, click the sidebar's *This computer* (the first entry,
    above Search; or the status bar's *Play on: This computer ▾*, or `Alt+T`) and pick Living Room.
    Try it with the sidebar expanded, as the icon rail (hover for the tooltip) and in the Compact
    glyph strip; remove the TV (or turn casting off) and it goes. Then Enter on another channel in Live TV, Shift+Enter on
    a film, the guide's Watch, a Home tile, Up next's *Play now*, a reminder that switches,
    and ask Claude to "put BBC One on" (no TV named). Move the Live TV cursor with preview
    autoplay on. Turn the TV off and play something. Restart Tuner (with and without *Remember
    Play on across restarts*).
  - **Expect:** what played here moves to the TV at once; the sidebar entry reads *Living Room* in the accent colour (the icon alone in the rail, tooltip *Play on: Living Room (Alt+T)*, never lit like a screen) and the chip *Play on: Living
    Room*; every play after it goes to the TV (the player screen shows the casting card where
    it would open; Live TV's preview pane shows it on Enter); Claude says it's on Living Room
    (`opened_in: "device"`, `status.play_on`) and nothing moves on screen. The preview never
    plays or casts while the cursor moves. With the TV off: the error card "Living Room isn't
    answering…" with *Play here*, and the chip still says Living Room. *This computer* in the
    chooser brings the cast back here. After a restart plays are here unless *Remember…* is
    on.
- [ ] **18. Stream info while casting** (cast design §11.3):
  - **Do:** cast a live channel by moving it from the player (`C`), press `I`; then a film by
    *Cast to…*, `I` again; then a 576i channel and a 5.1 film.
  - **Expect:** the panel shows over the casting card at once (never stuck on "Reading stream
    info…"): `Casting  Living Room · Apple TV 4K (3rd generation)`, the source's codecs, size,
    frame rate and scan, the sound's layout, the picture's way (copied and repackaged, or
    deinterlaced and converted on this computer), the sound's (copied, to AAC stereo, surround
    copied or as AC-3 5.1), the segments and their target duration, how far behind live, and
    for the film where its position comes from. A moved channel says the probe was skipped.
    The programme banner shows now / next and the progress as during local play.

## 31. Updates: a real update from vN to vN+1 (DESIGN §41)

Run on the **default profile** with Tuner started from the launcher, on a machine with omatuner
vN installed from its release package, once vN+1 is published. `id -u` is your uid below.
Turn on Settings ▸ About & logs ▸ *Debug logging* first: the log shows each request and the
helper's outcome.

- [ ] **The release is signed:**
  - **Do:** open vN+1's release page; download a package and its `.minisig`;
    `minisign -Vm omatuner-<vN+1>-1-$(uname -m).pkg.tar.zst -p packaging/update-key.pub`.
  - **Expect:** a `.minisig` beside every `.pkg.tar.zst` and `.tar.gz`; "Signature and comment
    signature verified". The publish job's log shows the signing step verifying each file.
- [ ] **Check and download:**
  - **Do:** start Tuner (vN); wait 30 s; open Settings ▸ About & logs ▸ Updates;
    `ls -la ~/.cache/tuner/update`. Then *Check now*.
  - **Expect:** a desktop notification "Tuner vN+1 is ready to install"; the status line
    "Tuner vN+1 is downloaded and verified: it installs when you're back at Tuner with nothing
    playing …"; the folder is `drwx------` with the package and its `.minisig`, both
    `-rw-------`. *Check now* logs a 304 (no second download). No pre-release is ever offered.
- [ ] **The password dialog waits for its moment:**
  - **Do:** play a channel full screen for a minute; stop it with Tuner in front.
  - **Expect:** nothing while it plays. Once stopped, with Tuner focused: the toast "Installing
    Tuner vN+1: enter your password to go on" and polkit's dialog "Install the Tuner update
    (signed with Tuner's release key)".
- [ ] **Cancel, then Install now:**
  - **Do:** Cancel the dialog. Wait a minute. Then Settings ▸ About & logs ▸ Updates ▸ *Install
    Tuner vN+1 now*, and enter the password.
  - **Expect:** after Cancel the toast "Tuner vN+1 isn't installed yet: … Install now" and no
    new dialog by itself. After the password: the toast "Tuner updated to vN+1 — it restarts when
    idle" with *Restart now*, a notification, and the status-bar chip "Updated to vN+1 —
    restarts when idle (Restart now)". `pacman -Q omatuner` says vN+1; `~/.cache/tuner/update`
    holds no package; `sudo ls /var/cache/omatuner` is empty.
- [ ] **Agents carry on:**
  - **Do:** before the install, in Claude Code (connected), ask Tuner's `status`. After the
    install, and again after the restart below, ask it again; `ls -l /proc/$(pgrep -f 'tuner
    mcp' | head -1)/exe`.
  - **Expect:** every call answers; the bridge's binary reads `/usr/bin/tuner (deleted)` (the old
    one keeps running). A new Claude Code session gets vN+1's bridge. With *Keep Claude Code's
    plugin up to date* on, after the restart Claude's plugin is at vN+1 (`claude plugin list
    --json`).
- [ ] **Restart when idle, never during playback:**
  - **Do:** note `pgrep -x tuner`. Play something for 3 minutes; stop; stay in Tuner for 2
    minutes; then click into a terminal on the same workspace (or go to another workspace) and
    type there for 2 minutes.
  - **Expect:** no restart while playing or paused, nor while Tuner has the keyboard. About a
    minute after you left it: Tuner restarts with the same pid, opens on the screen it was on,
    on its own workspace, and the terminal keeps the keyboard (your typing isn't interrupted);
    the toast "Tuner updated to vN+1 (from vN)"; Settings ▸ About shows vN+1 and "up to date".
  - **Do:** repeat with Tuner alone on a hidden workspace (also without the packaged
    `render_unfocused` rule). **Expect:** it restarts there too, about a minute plus 3 s later,
    and stays on that workspace.
- [ ] **Never while casting** (§30's Apple TV): with an update waiting for the restart, cast a
  film to the TV, then click into a terminal for 3 minutes; stop casting from the TV's remote
  and wait 2 more minutes. Then, with Tuner unfocused, run Settings ▸ Cast ▸ Test on TV, and
  open the PIN overlay (Forget the TV, then Pair) and leave it up for 2 minutes.
  **Expect:** no restart while it casts, while the test runs or while the PIN overlay is up; a
  restart about a minute after each ends. With *Install updates without asking for my password*
  on and a new version published, an update found while the PIN overlay is up installs only
  after it closed (the log: "installs once the TV's pairing is over").
- [ ] **Restart now:** with an update waiting (install vN again by hand, then let Tuner update),
  click the chip. **Expect:** an immediate restart into the same screen.
- [ ] **The passwordless switch:**
  - **Do:** turn on *Install updates without asking for my password*; Cancel the dialog. Turn it
    on again and enter the password. `sudo cat /etc/polkit-1/rules.d/49-omatuner-update-$(id
    -u).rules`. Then install vN by hand (`sudo pacman -U` its package) and *Check now*.
  - **Expect:** after Cancel the switch stays off. After the password the toast "Tuner now
    installs its updates without asking for your password" and the rule naming your user, only
    the action `io.github.rthoms334.omatuner.update`, `subject.local && subject.active`. The
    check installs vN+1 with no dialog, the chip follows.
  - **Do:** with the rule on, `pkexec /usr/lib/tuner/tuner-update passwordless off` and
    `pkexec /usr/bin/true` from a terminal. **Expect:** both ask for the password.
  - **Do:** turn the switch off. **Expect:** a dialog; afterwards the rule file is gone and the
    next install asks for the password again.
- [ ] **A tampered signature is refused:**
  - **Do:** `mkdir -m 700 ~/upd`; copy vN+1's package and `.minisig` there; flip a byte
    (`printf x | dd of=~/upd/omatuner-<vN+1>-1-$(uname -m).pkg.tar.zst bs=1 seek=4096
    conv=notrunc`); with vN installed,
    `pkexec /usr/lib/tuner/tuner-update install ~/upd/omatuner-<vN+1>-1-$(uname -m).pkg.tar.zst;
    echo $?`.
  - **Expect:** exit 3, "… the signature doesn't match the file"; `pacman -Q omatuner` is still
    vN; `/var/cache/omatuner` holds nothing.
- [ ] **A downgrade or the same version is refused:** with vN+1 installed, run the helper on
  vN's genuine package (with its `.minisig`), then on vN+1's. **Expect:** exit 3, "… isn't newer
  than the installed …", both times.
- [ ] **Unsafe paths are refused:** `ln -s ~/upd ~/upd-link` and run the helper on
  `~/upd-link/<package>`; then `chmod 755 ~/upd` and run it on `~/upd/<package>`. **Expect:**
  exit 3, "folders on the way must not be symlinks", then "must be yours and private (0700)".
- [ ] **The install runs as a system service, one at a time:**
  - **Do:** with vN installed and the passwordless rule on, start two installs of vN+1 at once:
    `p=~/upd/omatuner-<vN+1>-1-$(uname -m).pkg.tar.zst; for i in 1 2; do pkexec
    /usr/lib/tuner/tuner-update install $p & done; wait`; while they run, `systemctl status
    omatuner-update.service`.
  - **Expect:** one prints "installed omatuner vN+1 …", the other exit 5 (busy); the status shows
    the unit in system.slice running `tuner-update install-as <uid> <path>` (its pacman child
    under it); afterwards the unit is gone and `sudo ls -A /var/cache/omatuner` is empty.
  - **Do:** `pkexec /usr/lib/tuner/tuner-update install-as $(id -u) $p`. **Expect:** the
    password dialog (polkit's generic action); after it, exit 2.
  - **Do:** with vN installed again, in a shell `ulimit -f 1024` and run the install.
    **Expect:** vN+1 installs: the limit isn't the service's.
- [ ] **A Tuner not installed with pacman only tells:**
  - **Do:** from a checkout of vN, `cargo build --release`, then
    `target/release/tuner --profile-dir /tmp/tuner-check`; wait 30 s.
  - **Expect:** a notification and a toast "Tuner vN+1 is available — Settings ▸ About & logs
    says more", once (not after a restart); Settings says it runs from
    `…/target/release/tuner` and wasn't installed with pacman, without the two switches;
    nothing in `/tmp/tuner-check/cache/update` but `releases.json`, `releases.etag`, `told`.
- [ ] **No release key, no installs:** build the package from a checkout whose
  `packaging/update-key.pub` holds `REPLACE-WITH-THE-RELEASE-KEY`; install it. **Expect:**
  Settings says this build has no release key; running the helper on a signed newer package
  exits 6.
- [ ] **Offline:** disconnect the network; *Check now*. **Expect:** "Couldn't check for
  updates: …" under the status, no toast; reconnecting and *Check now* clears it.
