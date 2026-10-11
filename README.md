# Tuner

[![CI](https://github.com/rthoms334/omatuner/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/rthoms334/omatuner/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rthoms334/omatuner?include_prereleases)](https://github.com/rthoms334/omatuner/releases)

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
  A provider's M3U (a `get.php?username=…` link or a downloaded file) switches to its Xtream
  login automatically, so movies, series and catch-up appear; providers that don't allow that
  still get their movies and series sorted out of the playlist. *Try free channels* adds a
  free iptv-org or Free-TV playlist in two keystrokes. Tuner refreshes playlists in the
  background and shows account information (expiry, connections, formats).
- **Live TV:** groups, channel rows with now/next and progress, and a preview pane.
  - Zap with ↑/↓, recall the last channel, and browse your history.
  - Keep several favourites lists.
  - Hide, rename and reorder channels and groups; mark several groups and hide them at once.
  - Keep only the countries and languages you want from a provider's categories
    ("UK | Sports", "[FR] Films", "AR: News"…), including categories the provider adds later.
  - Type a channel number to jump to it.
  - Quality badges (SD, HD, FHD, 4K) next to channel names, so the SD/HD/4K copies of a channel
    are told apart without playing each one; the best copy is listed first. A badge comes from
    the provider's name until the channel plays, then from the resolution it really plays at
    (a "4K" channel that plays at 1080p shows FHD). When the channels a playlist labels 4K keep
    playing at a lower quality (half of those you played, once you played at least three
    channels of it and two of them claimed 4K), its other "4K" channels show **4K?** (after
    the next restart or refresh) until they play: the provider says 4K, but it isn't
    verified. The same goes for FHD. Hover a
    badge in the preview for where it comes from; stream info (`I`) shows what the provider
    claims next to what plays. Settings ▸ Interface ▸ Quality badges turns them off.
  - Smart names: "UK: ITV 4 4K ◉", "US ★ HGTV HD", "VIP: Sky Sports F1 ᴿᴬᵂ" and
    "DE ┃ Das Erste ●" read "ITV 4", "HGTV", "Sky Sports F1" and "Das Erste". Country,
    language and region prefixes, service tags (VIP, SAT, RAW, PPV, 24/7), quality and codec
    words (also in superscript or glued to a symbol) and decorations at either end are dropped;
    symbols that belong to a name stay ("E! News", "AT&T", "Sky Sports+", "Channel 4 +1").
    Categories are cleaned the same way ("UK ❖ General" → "General", "UK ❖ SPORTS 4K" →
    "Sports 4K"). Channels or categories that would end up with the same name keep what tells
    them apart ("TV5" / "TV5 · RAW"), and stream info (`I`) shows the provider's own name.
    "8K" in a name claims nothing (providers use it for event slates). Each playlist can turn it
    off (Edit ▸ smart name cleanup). Libraries from earlier versions get the new names on the
    first start, without a refresh.
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
  - A pop-out player on Hyprland: a small video-only window that stays on screen on every
    workspace (`o` in the player, Ctrl+Shift+O anywhere; see [Pop-out player](#pop-out-player)).
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

The project and its Arch package are called **omatuner**; the app is *Tuner* and the command is
`tuner`.

### From a GitHub Release (recommended on Omarchy)

Each [release](https://github.com/rthoms334/omatuner/releases) has Arch packages for x86_64 and
aarch64 (Asahi), plain binary tarballs, and a `SHA256SUMS` file. Download the package for your
machine (`uname -m`) and install it with pacman, which also pulls in the dependencies (mpv,
fontconfig, JetBrains Mono Nerd Font, …):

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo pacman -U omatuner-0.1.0-1-x86_64.pkg.tar.zst     # or …-aarch64.pkg.tar.zst
```

The package installs:

- `/usr/bin/tuner`
- the desktop entry and icon
- the opt-in snippets under `/usr/share/tuner/`
- this README and the design notes under `/usr/share/doc/tuner/`, the licence under
  `/usr/share/licenses/omatuner/`

`omatuner-<version>-<arch>.tar.gz` holds the same binary, desktop entry, icon, snippets, README
and LICENSE for other setups. The binary links against the system libmpv, so install `mpv`
first. Copy `tuner` to a directory on your session PATH (e.g. `~/.local/bin`), then install the
desktop entry and icon as described under [With cargo](#with-cargo).

### Build the Arch package from source

```sh
git clone https://github.com/rthoms334/omatuner.git
cd omatuner/packaging
makepkg -si          # builds this checkout, smoke-tests the binary, installs the package
```

- `makepkg --nocheck -si` skips the smoke test. The test suite is `cargo test` (see Development).
- The PKGBUILD builds the checkout it sits in (no download of the app itself), so check out the
  tag you want first, e.g. `git checkout v0.1.0`.

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
2. **Type:** Xtream login, M3U URL, M3U file, or **Try free channels** (see
   [Free channels](#free-channels)).
3. **Details:** name, glyph, colour and credentials. For Xtream, the server field accepts a
   bare host. It adds `http://` and keeps the port.
4. **Test:** checks the account (status, expiry, connections).
   - A rejected or expired login sends you back to the field at fault.
   - An unreachable server offers *Retry* and *Add anyway*.
   - When an M3U playlist turns out to carry an Xtream login, the card says **Xtream login
     found**. *Continue* uses the login (movies, series, catch-up, account info);
     **Keep as M3U** keeps the plain playlist.
5. **Guide:** only for M3U playlists that don't name their own guide. Enter an XMLTV URL or
   file, or skip (you can add one later in Settings ▸ EPG).
6. **Importing:** **Open Live TV** becomes available as soon as the channels are in. Movies,
   series and the guide keep loading in the background (status bar, Ctrl+J).
   - **Choose what to show:** when the playlist's categories carry two or more countries or
     languages, the card asks which to keep (see
     [Countries and languages](#countries-and-languages)). Esc shows everything.
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
- **Drag and drop:** dropping an `.m3u`/`.m3u8` on the window opens the playlist card; dropping
  an XMLTV file (`.xml`, `.xml.gz`, `.xml.xz`) adds it as a guide. Whether drops arrive depends
  on the compositor (not yet confirmed under Wayland/Hyprland); the menu entries always work.
- **XMLTV guides:** **Settings ▸ EPG ▸ Add URL / Add a guide file…** accepts plain, `.gz` or
  `.xz`.
  - The order of the guides is their priority: drag the rows, or use Alt+↑/↓.
  - Each guide has a minutes offset for guides that are off by a time zone.

### Free channels

**Try free channels** in onboarding, or **Settings ▸ Playlists ▸ Add free channels…** later,
lists ready-made playlists of free, legal streams. Nothing is downloaded until you pick one, and
none is added for you. Playlists you already have read *Added*.

- **iptv-org, your country** (first): `https://iptv-org.github.io/iptv/countries/<cc>.m3u`. The
  country comes from the system time zone, then the locale (the UK is `uk`, about 300 channels).
- **iptv-org, all countries:** `https://iptv-org.github.io/iptv/index.m3u` (about 11 000).
- **Free-TV/IPTV:** about 2 000 hand-picked channels; its header names the epgshare01 guides,
  which are added with it.

Some channels are geo-blocked or offline; dead ones fail fast and get dimmed.

### Provider playlists and the Xtream login

Most providers hand out an M3U that is really an Xtream Codes account: a
`…/get.php?username=…&password=…` link, or a playlist (also a downloaded file) whose entries look
like `http://host[:port][/path]/live|movie|series/<user>/<pass>/<id>.<ext>`. Tuner recognises
these and signs in with the Xtream API (`player_api.php`) without asking:

- **New playlists:** the Test step shows *Xtream login found*. The source becomes an Xtream
  source with full movie and series metadata, catch-up and account info; it remembers the
  playlist URL or file so you can go back to it.
- **Playlists added before:** they switch on their next refresh, silently, with a toast
  ("Upgraded to Xtream login — movies & series now available"). Favourites, history, progress,
  renames and locks carry over. Settings ▸ Playlists shows them as *Xtream (from M3U)*.
- **If the login fails** the playlist simply stays M3U and Tuner tries again after a week (or
  at once with *Refresh now* in the playlist's editor). If the server couldn't answer at all
  (offline, a timeout, a server error), it tries again at the next refresh at least an hour
  later — normally the next scheduled one, as the playlist itself was refreshed meanwhile.
  If you quit Tuner just after a playlist switched, the toast is shown the next time it opens.
- **Keep as M3U:** choose it in the Test step, or turn off **Use the Xtream login** in the
  playlist's editor (it asks first, since movies and series that are not in the playlist go
  away). Turning it back on signs in right away and says so if the login is refused.

A playlist that mixes entries from other hosts is treated as a merged list and never switched.

**Movies and series from a plain M3U.** When a provider playlist stays M3U (login refused, or
*Keep as M3U*), its `/movie/` entries appear under Movies and its `/series/` entries under
Series, grouped by `group-title`. Episode titles such as `Show S01 E02`, `S01E02`, `1x02` or
`Season 1 Episode 2` are grouped into series, seasons and episodes; titles without one go to
season 1 in playlist order. Everything else stays a live channel. These pages only have what the
playlist carries (title, poster, group), so they show a short page with no plot or cast.
Ordinary playlists such as iptv-org or Free-TV are not affected.

### Countries and languages

Many Xtream panels (and M3U playlists) carry categories for dozens of countries: "UK | Sports",
"|DE| Filme", "[FR] Cinéma", "AR: News", "EX-YU Filmovi", "🇵🇹 Desporto". Tuner reads the country,
language or region at the start (or end) of each category name and lets you keep only the ones
you want. It is a **rule** stored per playlist, not a one-off hide: it is applied every time the
library is built, so categories the provider adds later for a country you didn't tick stay
hidden. One rule covers live groups, movie and series categories.

- **Where:** offered during onboarding right after the channels import (only when two or more
  tags are found), and later from **Settings ▸ Playlists**: the playlist's row menu
  (*Countries & languages…*) or its editor (*Categories…*). Also from the Live group or
  Movies/Series category menu (Ctrl+Enter) and **Settings ▸ Hidden ▸ Choose countries…**.
- **The list:** one row per tag with a code badge, its name and counts (live groups, channels,
  movie and series categories), plus **No country tag** for categories without one. A line shows
  the effect ("6 of 42 ticked · 3 210 of 48 900 channels"). Tags that appeared since the last
  save are marked *new*.
- **Defaults:** your country (from the time zone, then the locale) and its main language, the
  English-speaking countries (UK, US, Canada, Australia, Ireland, New Zealand), English, and
  untagged categories. Everything else starts unticked. These stay in the rule even when the
  playlist doesn't have them yet, so a later "NZ |" category still shows.
- **Keys:** Space or Insert ticks, type to filter, Ctrl+A ticks all / none of the rows shown,
  Ctrl+E goes back to "my country + English", Enter saves, Esc clears the filter, then cancels.
  Ticking nothing that the playlist has asks for a second Enter. Ticking everything removes the
  rule.
- **Recognised forms:** `UK | …`, `UK: …`, `UK - …`, `UK-…`, `|UK| …`, `[UK] …`, `(UK) …`,
  `EN ▎…`, `UK ✦ …`, `UK ★ …`, `UK ❖ …` (any symbol between the code and the name), a leading
  flag, full names (`ENGLISH`, `FRANCE`, `ARABIC`, `LATINO`,
  `EX-YU`, `AFRICA`) and trailing `… (UK)` / `… | UK`. UK, GB, ENGLAND, SCOTLAND and WALES are
  all the UK; EN, ENG and ENGLISH are the English *language*; AR is Arabic (Argentina is ARG or
  🇦🇷). Prefixes such as 4K, HD, FHD, ᴴᴰ, VIP, SAT, RAW, PPV, VOD, NEW, 24/7, XXX and ADULT are skipped, and
  unknown words never count: "4K | Sports", "BT Sport", "FA Cup" and "IT Crowd" are untagged
  and stay shown.
- **Exceptions:** Ctrl+H on a group always hides it. **Settings ▸ Hidden** lists the hidden
  countries: Enter shows a whole country again, → expands it so you can show single categories.
- **Playlists added before this version** need one refresh before most tags are found (the
  picker says so; Ctrl+R there refreshes).
- **Hiding several groups at once:** in Live groups and the Movies/Series category list, Space
  (or Insert) marks groups and Ctrl+H hides all marked ones (Esc clears the marks). While
  filtering, Ctrl+Shift+H hides every group whose name has words starting with what you typed
  ("ar" takes "AR | Movies" and "Arabic News", not "Cartoons"), after a confirm.

Group names carry a muted code badge ("UK") when the playlist has several tags, since smart
names turn "UK | Sports" and "US | Sports" into "Sports".

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
- **`?` or F1** shows every key, the screen you're on first. Type to filter it, like Omarchy's
  keybindings menu: "ctrl+h", "f5" or "mute" narrow the list as you type, Esc clears the filter
  and then closes. The status bar shows a short hint line for the same screen.
- **Esc** steps out one level: filter → panel → fullscreen → player → previous screen.
- **Alt+1…5** switch screens, and **Alt+0** or **Ctrl+,** opens Settings. **Ctrl+K** or **/**
  searches. **Ctrl+Q** quits.
- Super is left to Omarchy. Media keys arrive through MPRIS.

The full reference is generated from `src/keymap.rs`:

<!-- keymap:start -->
### Everywhere (`Global`)

| Keys | Action |
|---|---|
| `Ctrl+K` | Search everything |
| `Ctrl+Q` | Quit |
| `Alt+1` | Home |
| `Alt+2` | Live TV |
| `Alt+3` | TV guide |
| `Alt+4` | Movies |
| `Alt+5` | Series |
| `Ctrl+Comma` / `Alt+0` | Settings |
| `Alt+←` | Back |
| `F11` | Toggle window fullscreen |
| `Ctrl+P` | Open the player (from the mini player) |
| `Ctrl+Shift+P` | Play / pause |
| `Ctrl+Shift+S` | Stop playback |
| `Ctrl+Shift+O` | Pop the player out (small, on every workspace) / back in |
| `Ctrl+R` / `F5` | Refresh playlists & guide |
| `Ctrl+J` | Show background tasks |
| `F1` | Keyboard help |

### Lists (`List`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | First / last |
| `Enter` / `→` | Play here / open (playing → fullscreen) |
| `Shift+Enter` | Play fullscreen |
| `←` / `Backspace` | Back to the previous pane |
| `Tab` / `Shift+Tab` | Next / previous pane |
| `Esc` | Back |
| `/` | Search everything |
| `?` | Keyboard help |
| `Space` / `Insert` | Mark group (Ctrl+H then hides all marked) |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

### Lists while filtering (`ListFiltering`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Enter` | Play here / open (playing → fullscreen) |
| `Shift+Enter` | Play fullscreen |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` / `Esc` | Clear filter |
| `Tab` / `Shift+Tab` | Next / previous pane |
| `Insert` | Mark group (Ctrl+H then hides all marked) |
| `Ctrl+Shift+H` | Hide the groups with the words typed |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

### Tick lists (`Checklist`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | First / last |
| `Space` / `Insert` | Tick / untick |
| `Ctrl+A` | Tick all / none (of the rows shown) |
| `Ctrl+E` | Just my country + English |
| `Enter` | Save |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` | Clear filter |
| `Esc` | Clear the filter, then cancel |
| `a–z, 0–9 …` | Type to filter |

### Movies & series grid (`PosterGrid`)

| Keys | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | First / last |
| `Enter` | Open details (channels: play) |
| `Shift+Enter` | Play / resume fullscreen |
| `Backspace` | Back to categories |
| `Tab` / `Shift+Tab` | Next / previous pane |
| `Esc` | Back |
| `/` | Search everything |
| `?` | Keyboard help |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

### Movies & series grid while filtering (`PosterGridFiltering`)

| Keys | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Enter` | Open details (channels: play) |
| `Shift+Enter` | Play / resume fullscreen |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` / `Esc` | Clear filter |
| `Tab` / `Shift+Tab` | Next / previous pane |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Shift+F10` / `Ctrl+Enter` | Context menu |
| `a–z, 0–9 …` | Type to filter |

### Movie & series pages (`Detail`)

| Keys | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `[` / `]` | Previous / next season |
| `Tab` / `Shift+Tab` | Next / previous section (buttons, seasons, episodes) |
| `Enter` | Play / resume (or the focused button) |
| `Shift+Enter` | Play from the start |
| `Backspace` / `Esc` | Back |
| `/` | Search everything |
| `?` | Keyboard help |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Alt+↑` | Move up (reorder) |
| `Alt+↓` | Move down (reorder) |
| `Ctrl+S` | Sort & view options… |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Delete` | Remove (from list / cancel task / forget) |
| `Shift+F10` / `Ctrl+Enter` | Context menu |

### TV guide (`Guide`)

| Keys | Action |
|---|---|
| `↑` / `k` / `↓` / `j` | Previous / next channel |
| `←` / `h` / `→` / `l` | Previous / next programme |
| `PgUp` / `PgDn` | Page channels |
| `Shift+←` / `Shift+→` / `H` / `L` | Scroll time −/+ 3 h |
| `[` / `]` | Previous / next day |
| `n` / `Home` | Jump to now |
| `g` / `G` | First / last channel |
| `Enter` | Programme details (Watch / Catch up / Remind) |
| `w` / `Shift+Enter` | Watch channel |
| `c` | Catch up (past programme) |
| `r` | Remind me / cancel reminder |
| `f` | Collection: favourites lists, groups, all |
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Jump to channel number |
| `Ctrl+F` | Add channel to Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+I` | Edit channel information |
| `F2` | Rename channel |
| `Ctrl+H` | Hide channel |
| `Ctrl+L` | Lock channel with PIN / unlock |
| `Shift+F10` / `Ctrl+Enter` | Channel menu |
| `/` | Search everything |
| `?` | Keyboard help |
| `Esc` | Back |

### Player — live TV (`PlayerLive`)

| Keys | Action |
|---|---|
| `↑` / `k` / `↓` / `j` / `PgUp` / `PgDn` | Previous / next channel (in this list) |
| `Backspace` / `←` | Last channel |
| `→` / `h` | Channel history |
| `c` | Channel list |
| `g` | Mini guide (now / next) |
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Type a channel number |
| `Space` | Play / pause |
| `m` | Mute |
| `+` / `=` / `-` | Volume up / down |
| `Enter` / `i` / `Tab` | On-screen controls |
| `I` | Stream info (codec, resolution, bitrate) |
| `a` | Next audio track |
| `A` | Audio tracks… |
| `s` | Next subtitle track |
| `S` | Subtitles… |
| `z` / `x` | Subtitle delay −/+ 100 ms |
| `Z` | Reset subtitle delay |
| `{` / `}` | Subtitle size −/+ |
| `v` | Aspect ratio (auto, 16:9, 4:3, 21:9, fill, stretch) |
| `V` | Zoom |
| `f` | Toggle window fullscreen |
| `p` | Keep playing in the mini player |
| `o` | Pop out: small window on every workspace (Hyprland) |
| `q` | Stop and close |
| `r` | Retry (after an error) |
| `Ctrl+F` | Add to / remove from Favourites |
| `Esc` | Back (hide banner, then leave; keeps playing in the mini player) |
| `/` | Search everything |
| `?` | Keyboard help |

### Player — movies, episodes, catch-up (`PlayerVod`)

| Keys | Action |
|---|---|
| `←` / `h` / `→` / `l` | Skip back / forward (small step) |
| `Shift+←` / `Shift+→` / `↑` / `↓` | Skip back / forward (big step) |
| `L` | Jump to live (from catch-up) |
| `Home` | Go to start |
| `Space` | Play / pause |
| `m` | Mute |
| `+` / `=` / `-` | Volume up / down |
| `Enter` / `i` / `Tab` | On-screen controls |
| `I` | Stream info (codec, resolution, bitrate) |
| `a` | Next audio track |
| `A` | Audio tracks… |
| `s` | Next subtitle track |
| `S` | Subtitles… |
| `z` / `x` | Subtitle delay −/+ 100 ms |
| `Z` | Reset subtitle delay |
| `{` / `}` | Subtitle size −/+ |
| `v` | Aspect ratio (auto, 16:9, 4:3, 21:9, fill, stretch) |
| `V` | Zoom |
| `f` | Toggle window fullscreen |
| `p` | Keep playing in the mini player |
| `o` | Pop out: small window on every workspace (Hyprland) |
| `q` | Stop and close |
| `r` | Retry (after an error) |
| `Ctrl+F` | Add to / remove from Favourites |
| `Esc` | Back (hide banner, then leave; keeps playing in the mini player) |
| `/` | Search everything |
| `?` | Keyboard help |

### Player — on-screen controls (`PlayerOsd`)

| Keys | Action |
|---|---|
| `←` / `→` | Move between controls / scrub |
| `↑` / `↓` | Move between control rows |
| `Enter` | Activate control |
| `Tab` / `Shift+Tab` | Next / previous control |
| `Esc` | Leave the controls |

### Pop-out player (`PopOut`)

| Keys | Action |
|---|---|
| `Esc` / `o` | Back to the full window (also double-click) |
| `f` | Back to the full window, fullscreen |

### Channel number entry (`NumericEntry`)

| Keys | Action |
|---|---|
| `0` / `1` / `2` / `3` / `4` / `5` / `6` / `7` / `8` / `9` | Type the channel number |
| `Enter` | Go now (otherwise after 1.5 s) |
| `Backspace` | Delete digit |
| `Esc` | Cancel |

### EPG mapping (`EpgMapping`)

| Keys | Action |
|---|---|
| `Enter` | Assign the highlighted EPG channel |
| `Delete` | Clear the manual assignment |

### Search (`Search`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Move |
| `PgUp` / `PgDn` | Page up / down |
| `Tab` / `Shift+Tab` | Next / previous section (All, Live, Movies, Series, Guide) |
| `Enter` | Play fullscreen / open |
| `Shift+Enter` | Play fullscreen / open details |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` | Clear |
| `Esc` | Clear, then close |
| `Ctrl+F` | Add to / remove from Favourites |
| `Ctrl+Shift+F` | Favourites lists… (add, new list) |
| `Ctrl+H` | Hide / unhide |
| `F2` | Rename |
| `Ctrl+L` | Lock with PIN / unlock |
| `Ctrl+G` | Show in TV guide |
| `Ctrl+I` | Details / edit information |
| `Ctrl+E` | Assign EPG channel |
| `Ctrl+W` | Mark watched / unwatched |
| `Shift+F10` / `Ctrl+Enter` | List these shortcuts |

### Dialogs & menus (`Modal`)

| Keys | Action |
|---|---|
| `↑` / `↓` / `←` / `→` | Move |
| `Tab` / `Shift+Tab` | Next / previous field |
| `Enter` / `Space` | Confirm |
| `Esc` | Cancel |

### Text fields (`TextInput`)

| Keys | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous field |
| `Enter` | Confirm |
| `Esc` | Leave the field |

### Settings (`Panel`)

| Keys | Action |
|---|---|
| `↑` / `k` / `↓` / `j` | Move |
| `←` / `h` | Decrease / previous option |
| `→` / `l` | Increase / next option |
| `Alt+↑` / `K` / `Alt+↓` / `J` | Move up / down (reorder: EPG priority, playlists, lists) |
| `Tab` / `Shift+Tab` | Next / previous section |
| `Enter` / `Space` | Toggle / open |
| `x` / `Delete` | Delete / more actions |
| `Esc` | Back |
| `?` | Keyboard help |

### Keyboard help (`Help`)

| Keys | Action |
|---|---|
| `↑` / `↓` | Scroll |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | Top / bottom |
| `Backspace` | Delete character |
| `Ctrl+Backspace` | Delete word |
| `Ctrl+U` | Clear filter |
| `Esc` / `F1` | Clear the filter, then close |
| `?` | Close (types ? while filtering) |
| `Enter` | Close |
| `a–z, 0–9 …` | Type to filter |

<!-- keymap:end -->

## Mouse

Everything the keys do can be done with the mouse too:

- **Back:** the mouse's back (side) button is Esc — it clears a filter, closes a panel or
  dialog, leaves the player's controls, and only then goes back a screen. The forward button
  does nothing. Pages that Esc leaves have a **‹ Back to …** button at the top (movie and
  series pages, Logs, EPG mapping, Settings opened from another screen, a guide opened from
  Home or Search), and so do the one-pane views of a narrow window (`‹ Settings`,
  `‹ Channels`). Clicking the lit tab in the sidebar goes back to that tab.
- **Right-click** opens the same menu as the menu key (Shift+F10), at the pointer: on channels,
  groups, posters, Home tiles, guide cells, episodes, search results, the player's video.
  Detail pages also have a `⋯` button for it.
- **Dialogs** close with their ✕ or a click outside — except one with an unsaved edit (a typed
  name, changed fields or ticks), which stays open until you Save, Cancel or press Esc.
- **Player:** the ← at the top left leaves the player (it keeps playing in the mini player).
  The bottom bar ends with the volume: click the speaker to mute, click or drag the slider
  (0–130 %, a mark at 100 %), or scroll over it (±5). Scrolling anywhere over the video also
  sets the volume. A click on a control just does it — the arrow keys keep zapping. Side panels
  have a ✕ and tune on one click; a failed stream has Retry / Last channel buttons; the
  next-episode card has Play now / Cancel. Double-click toggles fullscreen.
- **Pop-out:** hover the small window for previous / play / next, the volume, back to the full
  window and stop; drag the video to move it; double-click to put it back. The corner mini
  player has a pop-out button next to its ✕.
- **Movies and series:** below full width, the "{category} ▾" button opens the categories and
  one click on a category opens it.

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
| `--refresh` | Refresh stale playlists and guides without a window, then exit (status 1 if a refresh failed). Does nothing while Tuner is running, because the open window refreshes on its own. While it runs, starting Tuner waits a few seconds and then says (also as a desktop notification) that a background refresh is running. A playlist it upgraded to its Xtream login is announced the next time Tuner opens. |
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
unless you tick *Include passwords*. Restoring offers Merge or Replace; the backup's settings
are applied too only with *Restore settings* (on for Replace, off for Merge), and a PIN you
already set is never replaced. After a restore, every playlist is refreshed and your data is
re-attached to the refreshed channels.

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

### Pop-out player

Press `o` in the player, or Ctrl+Shift+O anywhere, to pop the player out: Tuner floats its
window, makes it small (16:9), puts it in a corner and **pins** it, so it stays on screen on
every workspace you switch to. The window then shows only the video. Moving the mouse over it,
or pressing a key, shows a short strip with the channel, what's on and the volume (with the
pointer over the window, buttons for previous / play / next, the volume, back to the full
window and stop; drag the video to move the window). All player
keys keep working: ↑/↓ zap, Space pauses, `m` mutes, `+`/`-` set the volume, `c` lists the
channels. Press Esc or `o`, or double-click the video, to put the window back where it was
(tiled, on the workspace you are on now); `f` puts it back and goes fullscreen, `p` puts it
back with the mini player and `q` stops and puts it back. A movie that ends while popped out
leaves the pop-out where it is ("Nothing playing"); its page is waiting when you come back.

- Move it with Super+drag, resize it with Super+right-drag.
- Settings ▸ Player ▸ *Pop-out (Hyprland)* picks the size (Small 480 × 270 or Medium
  640 × 360) and the corner (bottom right by default). If you move or resize the pop-out, it
  comes back there next time; changing either setting resets that. An unmoved pop-out always
  goes to the corner (on whichever monitor the window is on).
- **Super+O** (Omarchy's *Pop window out*) works too: while something plays, a pinned Tuner
  window at Omarchy's pop size (1300 × 900) or smaller switches to the video-only player by
  itself, and Esc or `o` puts it back. A bigger pinned window, or nothing playing, keeps the
  full interface.
- The pop-out is tagged `pop` like Omarchy's own, so it gets the same look.
- It needs Hyprland. Elsewhere the key only says so.
- Install the packaged window rules (`hypr/tuner.lua`, above) for the pop-out too: without them
  Omarchy draws unfocused windows at 96 % opacity, so the page behind shows through the video.
  They also keep video rendering while a tiled Tuner sits on another workspace.

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
  (`render_unfocused`). Without it the audio keeps playing and video decoding pauses until you
  come back.
- **"Application not responding" for Tuner after switching workspaces:** fixed in this version.
  Older builds blocked waiting for the hidden window's next frame and missed Hyprland's pings,
  although playback continued. Tuner now never waits for a frame, with or without the
  snippet.
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
- **CI** (`.github/workflows/ci.yml`) runs on every pull request and push to `main`, natively on
  x86_64 and aarch64 in Arch Linux (ARM) containers: `cargo fmt --check`, clippy with
  `-D warnings`, the build, and `cargo test` with ffmpeg test media under a D-Bus session bus.
- **Releases** (`.github/workflows/release.yml`): pushing a tag `vX.Y.Z` (matching `Cargo.toml`
  and the PKGBUILD's `pkgver`) builds the PKGBUILD with makepkg on both architectures and
  publishes a GitHub Release with the packages, tarballs and `SHA256SUMS`. A tag with a suffix
  (`v0.1.1-rc1` for version 0.1.1; lowercase letters, digits and dots, starting with a letter)
  becomes a pre-release whose package version is `0.1.1rc1`, which pacman orders before `0.1.1`,
  so the final release upgrades it. *Run workflow* with `publish` off is a dry run that only
  uploads the files as workflow artifacts; pull requests that change `packaging/`, `Cargo.*` or
  the workflow get the same dry run.

## Licence

Tuner is licensed under the GNU General Public License, version 3 or later (see
[`LICENSE`](LICENSE)). Arch's libmpv is built with GPL components, which makes the combined
program GPL. MPL-2.0 dependencies (nucleo, mpris-server) are compatible.
