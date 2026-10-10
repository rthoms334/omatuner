# Tuner

Tuner is a keyboard-first IPTV player for [Omarchy](https://omarchy.org) (Hyprland). It plays
live TV, catch-up, movies and series from M3U playlists and Xtream Codes accounts, and has an
XMLTV programme guide. It looks like the rest of your Omarchy desktop: colours follow the
current theme live, corners are square, and the font is JetBrains Mono Nerd Font.

The video is rendered by an embedded libmpv inside a native Rust window (egui), so there is no
browser or Electron runtime and no separate player window.

- [Screenshots](#screenshots)
- [Features](#features)
- [Requirements](#requirements)
- [Build and install](#build-and-install)
- [First run](#first-run)
- [Adding sources](#adding-sources)
- [TV guide and guide coverage](#tv-guide-and-guide-coverage)
- [Keyboard](#keyboard)
- [Command line](#command-line)
- [Files and profiles](#files-and-profiles)
- [Theming](#theming)
- [Omarchy integration](#omarchy-integration)
- [Troubleshooting](#troubleshooting)
- [Privacy](#privacy)
- [Development](#development)
- [Licence](#licence)

## Screenshots

*No screenshots yet.* Planned: Live TV at full width, the TV guide, the player with its
on-screen controls, and a light theme (catppuccin-latte).

## Features

- **Sources:** M3U by URL or local file, and Xtream Codes (live, movies, series and the
  provider's guide). You can add several playlists, each with its own colour and glyph badge.
  A pasted `get.php?username=…` link is offered as an Xtream login. Tuner refreshes playlists
  in the background and shows account information (expiry, connections, formats).
- **Live TV:** groups, channel rows with now/next and progress, and a preview pane.
  - Zap with ↑/↓, recall the last channel, and browse your history.
  - Keep several favourites lists.
  - Hide, rename and reorder channels and groups.
  - Type a channel number to jump to it.
- **TV guide:** an EPG grid with programme details, reminders and catch-up.
  - XMLTV guides can come from a URL or a file (gz/xz).
  - Several guides can be ranked by priority, each with its own time offset.
  - Channels are matched to the guide automatically. The EPG mapping screen fixes the rest by
    hand.
- **Catch-up:** works with Xtream and with the M3U `catchup` types (`default`, `append`,
  `shift`, `flussonic`, `xc`, `vod`). You can set a catch-up offset per channel.
- **Movies and series:** posters, details, seasons and episodes, resume / start over,
  Continue watching, and watched markers.
- **Search:** global fuzzy search over channels, movies, series and programmes (`/` or Ctrl+K),
  plus type-to-filter in every list.
- **Player:**
  - Audio and subtitle tracks with language preferences, subtitle delay and size.
  - Aspect and zoom, stream info, and configurable skip steps.
  - Smart reconnects with clear error messages.
  - A corner mini-player, and window fullscreen.
- **Desktop:** MPRIS (media keys and the Omarchy bar's media widget), desktop notifications for
  reminders, and an idle inhibitor while video plays. It runs as a single instance and opens
  M3U files and links from the desktop.
- **Other:** parental PIN, backup export/import, an in-app log viewer with export, and a `?` help
  overlay generated from the real key bindings.

## Requirements

- Omarchy 4 / Hyprland (another Wayland compositor works, but the integration snippets are for
  Hyprland), with a GPU that supports OpenGL (Mesa).
- `mpv` with libmpv ≥ 0.38 (client API 2.5). On Apple Silicon under Asahi Linux, video is
  decoded in software. Expect about half a core per 1080p50 stream.
- `fontconfig`, and JetBrains Mono Nerd Font as the monospace font (Omarchy's default). Optional
  font packages: `noto-fonts-cjk` for CJK names, `noto-fonts` for Arabic, Hebrew and Indic names.
- `xdg-desktop-portal` with the GTK backend for the file pickers.
- To build: Rust ≥ 1.95 (`cargo`) and a C compiler (`base-devel`).
- Optional, for development only: `ffmpeg`. It generates the test media for the player tests,
  which are skipped without it.

## Build and install

### As an Arch package (recommended on Omarchy)

```sh
cd packaging
makepkg -si          # builds this checkout, runs the library tests, installs the package
```

- `makepkg --nocheck -si` skips the tests.
- The package installs:
  - `/usr/bin/tuner`
  - the desktop entry and icon
  - the opt-in snippets under `/usr/share/tuner/`
  - this README and the design notes under `/usr/share/doc/tuner/`

### With cargo

```sh
cargo build --release          # → target/release/tuner
cargo install --path .         # → ~/.cargo/bin/tuner
```

For the launcher entry and icon, copy two files. The desktop entry runs `Exec=tuner`, so make
sure `~/.cargo/bin` is on your session PATH, or edit `Exec=` to the full path:

- `packaging/tuner.desktop` to `~/.local/share/applications/`
- `packaging/icons/tuner.svg` to `~/.local/share/icons/hicolor/scalable/apps/`

Then follow [Omarchy integration](#omarchy-integration).

## First run

With no playlists, Tuner opens a single onboarding card with these steps:

1. **Welcome:** add a playlist or restore a backup.
2. **Type:** Xtream login, M3U URL or M3U file.
3. **Details:** name, glyph, colour and credentials. For Xtream, the server field accepts a
   bare host. It adds `http://` and keeps the port.
4. **Test:** checks the account (status, expiry, connections).
   - A rejected or expired login sends you back to the field at fault.
   - An unreachable server offers *Retry* and *Add anyway*.
5. **Guide:** only for M3U playlists that don't name their own guide. Enter an XMLTV URL or
   file, or skip (you can add one later in Settings ▸ EPG).
6. **Importing:** **Open Live TV** becomes available as soon as the channels are in. Movies,
   series and the guide keep loading in the background (status bar, Ctrl+J).
7. **Done:** counts per kind, and how much of the playlist the guide covers (see below).

Esc steps back. Everything works from the keyboard.

## Adding sources

Add more playlists in **Settings ▸ Playlists ▸ Add playlist…**. Opening an `.m3u`/`.m3u8` file with
Tuner (file manager, `tuner list.m3u`) does the same.

- **Xtream Codes:** server, username and password.
  - Live channels, movies, series and the provider's guide are imported together.
  - Choose TS or M3U8 per playlist (default: Auto).
- **M3U URL or file:** standard `#EXTM3U` playlists, with:
  - `tvg-id`, `tvg-name`, `tvg-logo`, `group-title`, `tvg-shift` and `catchup*` attributes
  - VLC options (`#EXTVLCOPT` User-Agent / Referer)
  - a guide in the header (`url-tvg` / `x-tvg-url`), which is added automatically
- **Per-playlist options (Edit):**
  - User-Agent
  - refresh interval
  - server time zone (for catch-up)
  - whether to use the provider guide
  - smart name cleanup
  - whether to import movies and series

  A User-Agent change applies without a restart: the next refresh, logo fetch and stream use it.
- **XMLTV guides:** **Settings ▸ EPG ▸ Add URL / Add a guide file…** accepts plain, `.gz` or
  `.xz`.
  - The order of the guides is their priority: drag the rows, or use Alt+↑/↓.
  - Each guide has a minutes offset for guides that are off by a time zone.

## TV guide and guide coverage

A guide only helps the channels it lists. Tuner matches playlist channels to guide channels in
this order:

1. `tvg-id`
2. the Xtream EPG id
3. iptv-org ids (with or without the `@Feed` suffix)
4. a unique name

Close names become suggestions in the EPG mapping screen.

**A guide can import correctly and still cover almost none of your channels.** The guide bundled
with the [iptv-org](https://github.com/iptv-org/iptv) playlist is the usual example: it has
11 249 channels, but its header guide lists only a couple of them. Tuner shows the coverage so
you don't mistake this for "no EPG loaded":

- **Settings ▸ EPG:** each guide says, for example, *Matches 3 of 11 249 channels · guide lists
  2 channels, 312 programmes*. When the guides cover less than half the channels, a note
  suggests what to do. Playlist rows show *N ch (M with guide)*.
- **Onboarding, Done step:** lists each guide's coverage, or says there is no guide, and offers
  *TV guide settings*.
- **Live preview, guide cells and guide header:** say why a channel has no programmes:
  - the guide is still loading (*Loading guide… 45 %*)
  - there is no guide at all
  - the guide hasn't loaded
  - the channel is *Not in your guide*, with the overall coverage
- **Ctrl+E on a channel without guide data:** opens EPG mapping for that channel, where you
  can pick the right guide channel. When there is nothing to map to, Ctrl+E opens
  Settings ▸ EPG to add a guide.

To get a fuller guide, add an XMLTV guide that lists your channels, ideally under the same ids
as the playlist's `tvg-id`. Some places to look:

- your provider's XMLTV link
- a guide generated with [iptv-org/epg](https://github.com/iptv-org/epg) for the sites your
  channels come from
- a community guide for a service (for example i.mjh.nz for Pluto TV)

Then rank it first in Settings ▸ EPG. Matching runs again after every guide import, and manual
mappings are never overwritten.

Tuner refreshes guides every 12 h, and sooner when the data runs out. The status bar warns when
a guide is stale or has ended.

## Keyboard

Tuner is built for the keyboard, and everything is also clickable. The basics:

- **In lists and grids, letters type into a filter** (launcher style). Vim keys work only in the
  guide, the player and Settings.
- **`?` or F1** shows every key for the screen you're on. The status bar shows a short hint line
  for the same screen.
- **Esc** steps out one level: filter → panel → fullscreen → player → previous screen.
- **Alt+1…5** switch screens, and **Alt+0** or **Ctrl+,** opens Settings. **Ctrl+K** or **/**
  searches. **Ctrl+Q** quits.
- Super is left to Omarchy. Media keys arrive through MPRIS.

The full reference is generated from `src/keymap.rs`:

<!-- keymap:start -->
*The key tables are generated at release time with `cargo run --example keymap_md`. Until then,
press `?` in the app or see [docs/DESIGN.md §15.2](docs/DESIGN.md).*
<!-- keymap:end -->

## Command line

```text
tuner [--view home|live|guide|movies|series|search|settings] [--play <ref>]
      [--debug] [--profile-dir <dir>] [--refresh] [PLAYLIST_OR_URL...]
```

| Flag | Meaning |
|---|---|
| `--view <screen>` | Open that screen. If Tuner is already running, it switches the running window. |
| `--play <ref>` | Play a channel by its stable reference `<source-id>:<item-key>` (e.g. `2:xt:live:1234`), a stream URL or a media file. |
| `--debug` | Debug logging for this session, whatever the setting says. |
| `--profile-dir <dir>` | Keep config, data, cache, logs and the socket under `<dir>`. Use it for testing, or for a second profile. |
| `--refresh` | Refresh stale playlists and guides without a window, then exit (status 1 if a refresh failed). Does nothing while Tuner is running, because the open window refreshes on its own. |
| `PLAYLIST_OR_URL…` | An `.m3u`/`.m3u8` file opens the add-playlist card. A stream URL or media file plays straight away. `file://` URIs are treated as paths. |
| `--help`, `--version` | Usage and version. |

No screen shows a channel's `--play` reference yet. With the `sqlite3` tool you can look it up:

```sh
sqlite3 -readonly ~/.local/share/tuner/tuner.db \
  "SELECT source_id || ':' || key, name FROM channel WHERE name LIKE '%News%'"
```

Only one Tuner runs per profile. A second `tuner …` hands its request to the running window over
`$XDG_RUNTIME_DIR/tuner/tuner.sock` (`<dir>/run/tuner.sock` with `--profile-dir`) and exits.
`omarchy-launch-or-focus` and the menu entries rely on this.

## Files and profiles

| What | Where | Notes |
|---|---|---|
| Settings | `~/.config/tuner/config.toml` | No secrets. Every key is optional. A broken file falls back to defaults and is left untouched. |
| Library database | `~/.local/share/tuner/tuner.db` | SQLite. Mode 0600 in a 0700 directory, because it holds the credentials. |
| Caches | `~/.cache/tuner/img`, `…/epg`, `…/playlists` | Logos and posters (up to 500 MB). Guide and playlist downloads are 0700 and deleted after import. |
| Log | `~/.local/state/tuner/tuner.log` (+ `.1`) | Rotated at 5 MB. Redacted. |
| Instance socket | `$XDG_RUNTIME_DIR/tuner/tuner.sock` | Removed on exit, including on SIGTERM/SIGINT. |

With `--profile-dir DIR`, the same files live under `DIR/config`, `DIR/data`, `DIR/cache`,
`DIR/state` and `DIR/run`. The Omarchy theme is still read from your real home.

Per-playlist and per-channel settings (renames, hidden items, favourites, progress, EPG
mappings) live in the database. **Settings ▸ Backup** exports them as JSON, without passwords
unless you tick *Include passwords*. Restoring offers Merge or Replace. After a restore, every
playlist is refreshed and your data is re-attached to the refreshed channels.

## Theming

Tuner follows the current Omarchy theme **live**: `omarchy-theme-set` recolours the running
window without a flash. Here is where each part of the look comes from:

- **Colours** come from `~/.local/state/omarchy/current/theme/colors.toml` and the theme's
  `shell.toml`, with your `~/.config/omarchy/shell.toml` overrides applied last.
- **Light themes** are detected and work: state tints are foreground-alpha, as in Omarchy.
- **Video overlays** use a fixed dark scrim, with the accent colour only when it is readable on
  it.
- **The font** is whatever `fc-match monospace` returns. Changes in `~/.config/fontconfig/`
  apply live.
- **Text size** follows Omarchy's base size.

Settings ▸ Interface has a *UI scale* on top of that, and *Follow the Omarchy theme*. Turn the
latter off for Omarchy's fallback palette.

## Omarchy integration

Nothing is written to your configuration. These snippets are opt-in. They are in `packaging/`
in the source tree and in `/usr/share/tuner/` when installed as a package.

**Hyprland window rules (recommended):** `hypr/tuner.lua` makes the video fully opaque, opting
out of Omarchy's default window transparency. It also sets **`render_unfocused`**, so playback
keeps rendering while Tuner is on another workspace. Without that rule, Hyprland stops drawing
the window; Tuner then pauses video decoding to save CPU, and the audio keeps playing.

```sh
cp /usr/share/tuner/hypr/tuner.lua ~/.config/hypr/tuner.lua   # or packaging/hypr/tuner.lua
```

Add this to `~/.config/hypr/hyprland.lua` **below** `require("default.hypr.toggles")`:

```lua
require("hypr.tuner")
```

The file also has an optional Super+Shift+T launch-or-focus binding, and an `idle_inhibit`
fallback rule. Both are commented out. Tuner already inhibits idle itself while video plays.

**Omarchy menu:** merge `omarchy/omarchy-menu.jsonc` into
`~/.config/omarchy/extensions/omarchy-menu.jsonc` for a *TV* submenu (Open, Live TV, TV Guide,
Search, Playlists & settings). Open it with `omarchy menu summon tv`.

**Media keys:** Tuner registers `org.mpris.MediaPlayer2.tuner`, so `playerctl -p tuner
play-pause` and Omarchy's media keys and bar widget work, also while the window is hidden.

## Troubleshooting

- **Logs:** open them with Settings ▸ About & logs ▸ *Open the log viewer*.
  - Type to filter. Ctrl+S sets the minimum level or exports the log (*Export…*).
  - The same page has *Export log…*. The file is `~/.local/state/tuner/tuner.log`.
  - Background job failures also appear in the Jobs panel (Ctrl+J) under *Recent problems*.
- **Debug logging:**
  - Turn on *Debug logging* in Settings ▸ About & logs. It takes effect immediately, without a
    restart.
  - `tuner --debug` turns it on for one session. `RUST_LOG` (e.g.
    `RUST_LOG=tuner::player=trace`) overrides both.
  - Messages from libraries (TLS, windowing) are hidden in normal use and appear as DEBUG lines
    with debug logging on.
- **"No guide data" everywhere:** see [guide coverage](#tv-guide-and-guide-coverage). Check
  Settings ▸ EPG for "Matches N of M channels".
- **Playback stops or stutters on another workspace:** install the Hyprland snippet
  (`render_unfocused`).
- **"The provider refused the stream":** the account has expired, or it is already using all
  its connections. Tuner plays one stream at a time and waits for the old one to close before
  starting the next. The account row in Settings ▸ Playlists shows the connection count.
- **A provider rejects Tuner:** set a User-Agent for that playlist (Edit), or a default one in
  Settings ▸ Player (default: VLC's).
- **"Video unavailable — libmpv failed to start":** check that `mpv` is installed and look in
  the log. Browsing still works.
- **Quitting:** Ctrl+Q, closing the window, SIGTERM and SIGINT all save your playback position
  and remove the socket.

## Privacy

- **Network:** Tuner talks only to the playlist, guide, stream and logo/poster addresses your
  playlists and guides contain. There is no telemetry and no account.
- **Credentials** are stored only in the database (0600). Credential-bearing downloads (Xtream
  `get.php` / `xmltv.php`) go to 0700 cache folders and are deleted after import. `config.toml`
  never holds secrets. libmpv is embedded, so stream URLs never appear on a command line.
- **Redaction:** every log line, error message, toast and stream-info panel is redacted.
  Passwords, usernames, tokens, URL credentials and Xtream path segments become `***`. The log
  still names your playlists and channels, so read an exported log before sharing it.
- **MPRIS** exposes the title, channel and logo URL of what is playing, and nothing else.
- **Backups** leave passwords out unless you tick *Include passwords*.
- **The parental PIN** is a speed bump for children, not security.

## Development

```sh
cargo run -- --profile-dir /tmp/tuner-dev     # isolated profile
cargo test                                    # no network, ffmpeg or Wayland needed (those tests skip)
cargo clippy --all-targets -- -D warnings     # lint gate (zero warnings)
cargo run --example keymap_md                 # regenerate the key tables (README, DESIGN §15.2)
scripts/gen-test-media.sh                     # ffmpeg test media for the player tests
scripts/gen-big-fixtures.py                   # 100k-channel M3U + 415k-programme XMLTV for benches
```

- The architecture, data model and decisions are in [`docs/DESIGN.md`](docs/DESIGN.md).
- The manual release checklist is [`docs/MANUAL_CHECKLIST.md`](docs/MANUAL_CHECKLIST.md).

## Licence

Tuner is licensed under the GNU General Public License, version 3 or later (see
[`LICENSE`](LICENSE)). Arch's libmpv is built with GPL components, which makes the combined
program GPL. MPL-2.0 dependencies (nucleo, mpris-server) are compatible.
